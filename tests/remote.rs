//! End to end: a main device and a replica in one process, over loopback.

use openq::control::Command;
use openq::media::Prober;
use openq::model::{Cue, CueKind, Workspace};
use openq::remote::client::{Auth, Link, LinkState};
use openq::remote::server::Server;
use openq::remote::{ClientMsg, RemoteConfig, Role, ServerMsg};
use openq::show::Show;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let t0 = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn closed(link: &Link) -> Option<(String, bool)> {
    match link.state() {
        LinkState::Closed { reason, needs_pairing } => Some((reason, needs_pairing)),
        _ => None,
    }
}

#[test]
fn pair_sync_control_and_revoke() {
    let dir = std::env::temp_dir().join(format!("openq-remote-test-{}", std::process::id()));
    // SAFETY: set before any other thread reads the environment.
    unsafe { std::env::set_var("OPENQ_CONFIG_DIR", &dir) };

    let mut ws = Workspace::default();
    for (n, secs) in [("1", 30.0), ("2", 30.0)] {
        let mut c = Cue::new(CueKind::Wait { duration: secs });
        c.number = n.into();
        ws.cue_list.cues.push(c);
    }
    let first = ws.cue_list.cues[0].id;
    let show = Arc::new(Mutex::new(Show::new(Prober::new())));
    show.lock().replace_workspace(ws, None);

    let port = 40000 + (std::process::id() % 20000) as u16;
    // Discovery's port is fixed: another OpenQ sharing on this computer may hold it.
    let discovery_free = std::net::UdpSocket::bind(("0.0.0.0", openq::remote::DISCOVERY_PORT)).is_ok();
    let server_cfg = Arc::new(Mutex::new(RemoteConfig { port, instance_name: "Main".into(), ..Default::default() }));
    let server = Server::start(show.clone(), server_cfg.clone(), || {}).unwrap();
    let client_cfg = Arc::new(Mutex::new(RemoteConfig { instance_name: "Tablet".into(), ..Default::default() }));
    let addr = format!("127.0.0.1:{port}");

    // Pairing closed: refused.
    let link = Link::connect(&addr, Auth::Pair("AAAA-AAAA".into()), client_cfg.clone(), || {});
    let (why, needs) = wait_for("refusal", || closed(&link));
    assert!(needs && why.contains("isn't open"), "{why}");

    // Wrong code: refused, and it uses up an attempt.
    let code = server.open_pairing();
    let link = Link::connect(&addr, Auth::Pair("2222-2222".into()), client_cfg.clone(), || {});
    let (why, _) = wait_for("wrong code", || closed(&link));
    assert_eq!(why, "Wrong pairing code");
    assert_eq!(server.pairing().unwrap().2, openq::remote::server::PAIRING_ATTEMPTS - 1);

    // Right code (typed loosely): paired, and pairing closes.
    let typed = code.to_lowercase().replace('-', " ");
    let link = Link::connect(&addr, Auth::Pair(typed), client_cfg.clone(), || {});
    wait_for("connected", || matches!(link.state(), LinkState::Connected { role: Role::Control, .. }).then_some(()));
    assert!(server.pairing().is_none());
    assert_eq!(client_cfg.lock().servers.len(), 1);
    assert_eq!(server_cfg.lock().devices[0].name, "Tablet");

    // The replica gets the workspace and live state.
    let mut got_ws = None;
    let mut playhead = None;
    wait_for("workspace and state", || {
        for m in link.drain() {
            match m {
                ServerMsg::Workspace { ws, .. } => got_ws = Some(ws),
                ServerMsg::State(s) => playhead = Some(s.playhead),
                _ => {}
            }
        }
        (got_ws.is_some() && playhead.is_some()).then_some(())
    });
    assert_eq!(got_ws.unwrap().cue_list.cues.len(), 2);
    assert_eq!(playhead.unwrap(), Some(first));

    // GO from the replica runs on the main device and comes back as state.
    link.send(ClientMsg::Command(Command::Go));
    wait_for("running cue", || {
        link.drain().into_iter().find_map(|m| match m {
            ServerMsg::State(s) if s.instances.iter().any(|i| i.id == first) => Some(()),
            _ => None,
        })
    });
    assert!(show.lock().engine.instances.contains_key(&first));

    // Edits need the Edit role.
    let mut edited = show.lock().ws.clone();
    edited.cue_list.cues[1].name = "From the tablet".into();
    link.send(ClientMsg::Edit(Box::new(edited.clone())));
    wait_for("refusal", || link.drain().into_iter().find_map(|m| matches!(m, ServerMsg::Refused(_)).then_some(())));
    server_cfg.lock().devices[0].role = Role::Edit;
    wait_for("role change", || link.drain().into_iter().find_map(|m| matches!(m, ServerMsg::Role(Role::Edit)).then_some(())));
    link.send(ClientMsg::Edit(Box::new(edited)));
    wait_for("edit applied", || (show.lock().ws.cue_list.cues[1].name == "From the tablet").then_some(()));
    // Its echo comes back as that one cue.
    wait_for("edit echoed", || {
        link.drain().into_iter().find_map(|m| match m {
            ServerMsg::Cues(c) if c.len() == 1 && c[0].name == "From the tablet" => Some(()),
            _ => None,
        })
    });

    // The main device's displays reach the replica; a replica's stage display choice waits
    // for the operator.
    let hdmi = openq::displays::Display { name: "HDMI-A-1".into(), size: [1920, 1080], position: [0, 0], refresh_hz: None };
    server.set_displays(std::slice::from_ref(&hdmi));
    wait_for("display list", || {
        link.drain().into_iter().find_map(|m| matches!(&m, ServerMsg::Displays(l) if l == std::slice::from_ref(&hdmi)).then_some(()))
    });
    let mut staged = show.lock().ws.clone();
    staged.stages.push(openq::model::Stage { output: Some("HDMI-A-1".into()), ..Default::default() });
    let stage = staged.stages.last().unwrap().id;
    link.send(ClientMsg::Edit(Box::new(staged)));
    wait_for("display request", || (!show.lock().network_requests.is_empty()).then_some(()));
    {
        let mut s = show.lock();
        assert_eq!(s.ws.stages.iter().find(|st| st.id == stage).unwrap().output, None, "nothing moves before the operator allows it");
        let id = s.network_requests[0].id;
        s.answer_network_request(id, true);
        assert_eq!(s.ws.stages.iter().find(|st| st.id == stage).unwrap().output.as_deref(), Some("HDMI-A-1"));
    }

    // Show mode on the main device: nobody edits, an Edit-role device acts as Control (its
    // stored role stays Edit) and gets it back in edit mode.
    server.set_show_mode(true);
    wait_for("show mode role", || link.drain().into_iter().find_map(|m| matches!(m, ServerMsg::Role(Role::Control)).then_some(())));
    let mut locked = show.lock().ws.clone();
    locked.cue_list.cues[1].name = "During the show".into();
    link.send(ClientMsg::Edit(Box::new(locked)));
    wait_for("show mode refusal", || {
        link.drain().into_iter().find_map(|m| matches!(m, ServerMsg::Refused(why) if why.contains("show mode")).then_some(()))
    });
    assert_eq!(show.lock().ws.cue_list.cues[1].name, "From the tablet");
    assert_eq!(server_cfg.lock().devices[0].role, Role::Edit);
    server.set_show_mode(false);
    wait_for("edit role back", || link.drain().into_iter().find_map(|m| matches!(m, ServerMsg::Role(Role::Edit)).then_some(())));

    // Lighting: design edits on the main device go as a lighting patch (not the whole show),
    // replica edits come back as one, a cue list reshape keeps the design out of the message,
    // and masters are mirrored in the live state.
    {
        use openq::light::model::Fixture;
        let mut s = show.lock();
        let dimmer = openq::light::library::generic("generic/dimmer").unwrap();
        s.ws.lighting.add_type(&dimmer);
        let mode = dimmer.modes[0].name.clone();
        s.ws.lighting.fixtures.push(Fixture { id: 7, name: "Spot".into(), fixture_type: dimmer.id, mode, address: 1, ..Default::default() });
        s.edited();
    }
    let lp = wait_for("lighting patch", || {
        link.drain().into_iter().find_map(|m| match m {
            ServerMsg::Lighting(p) => Some(p),
            ServerMsg::Workspace { .. } => panic!("the whole workspace was sent for a lighting edit"),
            _ => None,
        })
    });
    let mut replica = show.lock().ws.clone();
    replica.lighting = Default::default();
    assert!(openq::light::patch::apply(&mut replica.lighting, &lp));
    assert_eq!(replica.lighting.fixtures[0].name, "Spot");
    let mut mine = replica.lighting.clone();
    mine.groups.push(openq::light::model::Group { id: 1, name: "From the tablet".into(), fixtures: vec![7] });
    let lighting = openq::light::patch::diff(&replica.lighting, &mine).map(Box::new);
    link.send(ClientMsg::Patch { cues: Vec::new(), lighting });
    wait_for("lighting edit applied", || (show.lock().ws.lighting.groups.len() == 1).then_some(()));
    let mut reshaped = show.lock().ws.clone();
    reshaped.cue_list.cues.push(Cue::new(CueKind::Wait { duration: 1.0 }));
    reshaped.lighting = Default::default();
    link.send(ClientMsg::Reshape { ws: Box::new(reshaped), lighting: None });
    wait_for("reshape applied", || (show.lock().ws.cue_list.cues.len() == 3).then_some(()));
    let kept = wait_for("reshaped workspace", || {
        link.drain().into_iter().find_map(|m| match m {
            ServerMsg::Workspace { ws, keep_lighting, .. } => Some((ws, keep_lighting)),
            _ => None,
        })
    });
    assert!(kept.1 && kept.0.lighting.is_empty() && kept.0.cue_list.cues.len() == 3);
    assert_eq!(show.lock().ws.lighting.groups[0].name, "From the tablet", "the design survived the reshape");

    // Cues added, removed or moved go as operations both ways, and a replica's merge with what the
    // main device did meanwhile.
    let base = show.lock().ws.clone();
    let on_main = {
        let mut s = show.lock();
        let c = Cue::new(CueKind::Wait { duration: 2.0 });
        let id = c.id;
        s.ws.cue_list.cues.insert(0, c);
        s.edited_shape([id]);
        id
    };
    let ops = wait_for("operations", || {
        link.drain().into_iter().find_map(|m| match m {
            ServerMsg::Ops(o) => Some(o),
            ServerMsg::Workspace { .. } => panic!("the whole workspace was sent for an added cue"),
            _ => None,
        })
    });
    assert_eq!((ops.added.len(), ops.placed.len(), ops.removed.len()), (1, 1, 0));
    let mut mine = base.clone();
    let last = mine.cue_list.cues.pop().unwrap().id;
    let mut added = Cue::new(CueKind::Memo);
    added.name = "Added on the tablet".into();
    mine.cue_list.cues.push(added);
    let ops = openq::changes::Reshape::diff(&base, &mine).unwrap();
    link.send(ClientMsg::Ops { ops: Box::new(ops), lighting: None });
    wait_for("operations merged", || {
        let s = show.lock();
        let c = &s.ws.cue_list.cues;
        (c.last().unwrap().name == "Added on the tablet").then_some(())
    });
    {
        let s = show.lock();
        let c = &s.ws.cue_list.cues;
        assert_eq!(c.len(), 4);
        assert_eq!(c[0].id, on_main, "the main device's own edit kept");
        assert!(s.ws.cue_list.find(last).is_none());
    }
    link.send(ClientMsg::Command(Command::Light(openq::control::LightControl::GrandMaster(0.25))));
    wait_for("mirrored grand master", || {
        link.drain().into_iter().find_map(|m| match m {
            ServerMsg::State(s) if s.lighting.masters.grand_master == 0.25 => Some(()),
            _ => None,
        })
    });
    // A visualiser: asks for the DMX output and picks fixtures.
    link.send(ClientMsg::Subscribe { dmx: true });
    wait_for("DMX output", || {
        link.drain().into_iter().find_map(|m| match m {
            ServerMsg::Dmx(u) if u.iter().any(|(id, data)| *id == 1 && data.len() == 512) => Some(()),
            _ => None,
        })
    });
    link.send(ClientMsg::Select(vec![7]));
    wait_for("selection set", || (show.lock().light_selection == [7]).then_some(()));
    wait_for("selection mirrored", || {
        link.drain().into_iter().find_map(|m| match m {
            ServerMsg::State(s) if s.lighting.selection == [7] => Some(()),
            _ => None,
        })
    });
    link.send(ClientMsg::Subscribe { dmx: false });

    // Network settings from a replica wait for the operator; the rest of the edit goes through.
    {
        let mut s = show.lock();
        s.ws.cue_list.cues.push(Cue::new(CueKind::Network(openq::model::NetworkCue { host: "10.0.0.5".into(), ..Default::default() })));
        s.edited();
    }
    let mut net = show.lock().ws.clone();
    let n = net.cue_list.cues.len() - 1;
    net.cue_list.cues[n].name = "Console".into();
    if let CueKind::Network(c) = &mut net.cue_list.cues[n].kind {
        c.host = "203.0.113.9".into();
    }
    net.lighting.outputs.push(Default::default());
    link.send(ClientMsg::Edit(Box::new(net)));
    wait_for("network change held", || (show.lock().network_requests.len() == 1).then_some(()));
    {
        let s = show.lock();
        let cue = &s.ws.cue_list.cues[n];
        assert_eq!(cue.name, "Console", "the rest of the edit applied");
        assert!(matches!(&cue.kind, CueKind::Network(c) if c.host == "10.0.0.5"), "destination unchanged until allowed");
        assert!(s.ws.lighting.outputs.is_empty());
    }
    let id = show.lock().network_requests[0].id;
    show.lock().answer_network_request(id, true);
    assert!(matches!(&show.lock().ws.cue_list.cues[n].kind, CueKind::Network(c) if c.host == "203.0.113.9"));
    assert_eq!(show.lock().ws.lighting.outputs.len(), 1);

    // A replica can't bring in media files.
    let mut sneaky = show.lock().ws.clone();
    sneaky.cue_list.cues.push(Cue::new(CueKind::Audio(openq::model::AudioCue { path: "/etc/shadow".into(), ..Default::default() })));
    let cues_before = show.lock().ws.cue_list.cues.len();
    link.send(ClientMsg::Edit(Box::new(sneaky)));
    wait_for("media refused", || {
        link.drain().into_iter().find_map(|m| matches!(m, ServerMsg::Refused(why) if why.contains("media")).then_some(()))
    });
    assert_eq!(show.lock().ws.cue_list.cues.len(), cues_before);

    // An edit on the main device that keeps the cue tree's shape reaches replicas as the changed
    // cue only; a replica that asks for a resync gets the whole workspace again.
    {
        let mut s = show.lock();
        s.ws.cue_list.cues[0].name = "Renamed on the main device".into();
        s.edited();
    }
    let patch = wait_for("cue patch", || {
        link.drain().into_iter().find_map(|m| match m {
            ServerMsg::Cues(c) => Some(c),
            _ => None,
        })
    });
    assert_eq!(patch.len(), 1);
    assert_eq!(patch[0].name, "Renamed on the main device");
    link.send(ClientMsg::Resync);
    {
        // The resync is sent when there's something new to send.
        let mut s = show.lock();
        s.ws.cue_list.cues[0].notes = "after resync".into();
        s.edited();
    }
    let ws = wait_for("whole workspace after resync", || {
        link.drain().into_iter().find_map(|m| match m {
            ServerMsg::Workspace { ws, .. } => Some(ws),
            _ => None,
        })
    });
    assert_eq!(ws.cue_list.cues[0].notes, "after resync");
    drop(link);

    // Reconnect with the stored token, no code.
    let link = Link::connect(&addr, Auth::Token, client_cfg.clone(), || {});
    wait_for("token login", || matches!(link.state(), LinkState::Connected { role: Role::Edit, .. }).then_some(()));
    wait_for("listed as connected", || (server.connected().len() == 1).then_some(()));

    // Removing the device on the main side drops it, and its token stops working.
    server_cfg.lock().devices.clear();
    let (why, needs) = wait_for("revoked", || closed(&link));
    assert!(needs && why.contains("removed"), "{why}");
    let link = Link::connect(&addr, Auth::Token, client_cfg.clone(), || {});
    let (why, needs) = wait_for("token refused", || closed(&link));
    assert!(needs && why.contains("isn't paired"), "{why}");

    // A forged token for a known device id fails authentication.
    server_cfg.lock().devices.push(openq::remote::TrustedDevice {
        id: client_cfg.lock().instance_id,
        name: "Tablet".into(),
        token: openq::remote::new_token(),
        role: Role::Edit,
        paired_at: 0,
        last_seen: 0,
    });
    let link = Link::connect(&addr, Auth::Token, client_cfg.clone(), || {});
    let (why, needs) = wait_for("bad token", || closed(&link));
    assert!(needs && why.contains("Authentication failed"), "{why}");

    // Turning sharing off frees both ports at once, so it can be turned straight back on.
    drop(server);
    std::net::TcpListener::bind(("0.0.0.0", port)).expect("listening port still taken after the server stopped");
    if discovery_free {
        std::net::UdpSocket::bind(("0.0.0.0", openq::remote::DISCOVERY_PORT)).expect("discovery port still taken after the server stopped");
    }
    drop(Server::start(show.clone(), server_cfg.clone(), || {}).expect("sharing turned straight back on"));
    let _ = std::fs::remove_dir_all(dir);
}
