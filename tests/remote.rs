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

    drop(server);
    let _ = std::fs::remove_dir_all(dir);
}
