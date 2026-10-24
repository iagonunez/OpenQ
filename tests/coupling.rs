//! End to end: three computers of a coupling group in one process, over loopback.

use opnq::media::Prober;
use opnq::remote::client::{self, Auth, Link, LinkState};
use opnq::remote::coupling::{Duty, Coupling, Local, Member, Peer, Supervisor, Verdict};
use opnq::remote::server::Server;
use opnq::remote::RemoteConfig;
use opnq::show::Show;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let t0 = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t0.elapsed() < Duration::from_secs(15), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Everything these tests save goes to a folder of their own, never the user's config.
fn init() -> std::path::PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let dir = std::env::temp_dir().join(format!("opnq-coupling-test-{}", std::process::id()));
    ONCE.call_once(|| {
        // SAFETY: set before any test thread reads the environment (every test calls this first).
        unsafe { std::env::set_var("OPNQ_CONFIG_DIR", &dir) };
    });
    dir
}

struct Pc {
    name: &'static str,
    cfg: Arc<Mutex<RemoteConfig>>,
    port: u16,
    server: Option<Server>,
}

impl Pc {
    fn new(name: &'static str, port: u16) -> Self {
        let cfg = Arc::new(Mutex::new(RemoteConfig { port, instance_name: name.into(), ..Default::default() }));
        let show = Arc::new(Mutex::new(Show::new(Prober::new())));
        let server = Server::start(show, cfg.clone(), || {}).unwrap();
        Self { name, cfg, port, server: Some(server) }
    }

    fn id(&self) -> uuid::Uuid {
        self.cfg.lock().instance_id
    }

    fn address(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn member(&self) -> Member {
        Member { id: self.id(), name: self.name.into(), address: self.address() }
    }

    fn say(&self, acting: bool, term: u64) {
        let orphan_since = (!acting).then(Instant::now);
        self.server.as_ref().unwrap().set_presence(Some(Local { acting, term, orphan_since }));
    }
}

#[test]
fn the_next_in_the_list_takes_over() {
    let dir = init();
    let base = 30000 + (std::process::id() % 20000) as u16 * 3 / 2;
    let (mut a, mut b, c) = (Pc::new("a", base), Pc::new("b", base + 1), Pc::new("c", base + 2));
    let all = [&a, &b, &c];
    // One group: `a` starts it, `b` and `c` join with codes.
    let first = a.member();
    a.cfg.lock().coupling.create(first);
    for joiner in [&b, &c] {
        let code = a.server.as_ref().unwrap().open_pairing();
        let grant = client::join(&joiner.cfg, &a.address(), &code).unwrap();
        joiner.cfg.lock().coupling.join(grant);
    }
    let list = a.cfg.lock().coupling.clone();
    assert_eq!(list.members.len(), 3);
    assert_eq!(list.members[0].address, a.address(), "the first computer's own address is filled in when someone joins");
    assert_eq!(list.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
    assert_eq!(list.members[2].address, c.address(), "a joiner is reached where it said");
    // `a` told the others only what it knew when they joined: b missed c, until the lists are compared.
    assert_eq!(b.cfg.lock().coupling.members.len(), 2);
    let mut b_list = b.cfg.lock().coupling.clone();
    assert_eq!(b_list.adopt(b.id(), list.roster()), opnq::remote::coupling::Adopted::Changed);
    b.cfg.lock().coupling = b_list;
    // Nobody is a paired device: joining stores no device keys.
    for p in all {
        let cfg = p.cfg.lock();
        assert!(cfg.devices.is_empty() && cfg.servers.is_empty());
    }
    let list = Coupling { silence: 1.0, ..list };
    for p in all {
        let mut cfg = p.cfg.lock();
        cfg.coupling.silence = 1.0;
        cfg.coupling.enabled = true;
    }
    a.say(true, 1);
    b.say(false, 1);
    c.say(false, 1);

    // What the others say about a device.
    let limit = Duration::from_secs(2);
    let (peer, roster) = client::probe(&c.cfg, a.id(), &a.address(), limit);
    assert_eq!(peer, Peer::Main { term: 1 });
    assert_eq!(roster.unwrap().members.len(), 3, "a member is told the list");
    assert!(matches!(client::probe(&c.cfg, b.id(), &b.address(), limit).0, Peer::Standby { term: 1, .. }));
    // Another computer answering at that address is not the member.
    assert!(matches!(client::probe(&c.cfg, a.id(), &b.address(), limit).0, Peer::NotPaired(_)));
    // Nobody there.
    assert!(matches!(client::probe(&c.cfg, a.id(), "127.0.0.1:9", limit).0, Peer::Unreachable(_)));
    // The probe left no trace as a connection.
    assert!(a.server.as_ref().unwrap().connected().is_empty());

    // A standby device serves nobody; the playing one serves.
    let link = Link::connect(&b.address(), Auth::Group, c.cfg.clone(), || {});
    let why = wait_for("standby refusal", || match link.state() {
        LinkState::Closed { reason, .. } => Some(reason),
        _ => None,
    });
    assert_eq!(why, client::STANDBY);
    let link = Link::connect(&a.address(), Auth::Group, c.cfg.clone(), || {});
    wait_for("connected to the main device", || matches!(link.state(), LinkState::Connected { .. }).then_some(()));

    // C has no main device yet: it finds A.
    let orphan = |term| Duty::Orphan { since: Instant::now(), grace: Duration::ZERO, term };
    let sup_c = Supervisor::start(c.cfg.clone(), || {});
    let sup_b = Supervisor::start(b.cfg.clone(), || {});
    sup_c.set(&list, orphan(0));
    wait_for("C follows A", || (sup_c.verdict() == Verdict::Follow(a.id())).then_some(()));

    // A stops. B (next in the list) takes over; C waits for it, since B is alive and above it.
    drop(link);
    a.server = None;
    // (B has just lost it too: it reports how long it has been without a main device.)
    b.say(false, 1);
    sup_c.set(&list, orphan(1));
    sup_b.set(&list, orphan(1));
    wait_for("B takes over", || (sup_b.verdict() == Verdict::TakeOver(2)).then_some(()));
    assert_ne!(sup_c.verdict(), Verdict::TakeOver(2));
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1200) {
        assert!(!matches!(sup_c.verdict(), Verdict::TakeOver(_)), "C must wait for B");
        std::thread::sleep(Duration::from_millis(50));
    }

    // B plays; C follows B.
    b.say(true, 2);
    sup_b.set(&list, Duty::Main { term: 2 });
    sup_c.set(&list, orphan(1));
    wait_for("C follows B", || (sup_c.verdict() == Verdict::Follow(b.id())).then_some(()));
    let link = Link::connect(&b.address(), Auth::Group, c.cfg.clone(), || {});
    wait_for("connected to B", || matches!(link.state(), LinkState::Connected { .. }).then_some(()));

    // B stops too: C is alone and plays.
    drop(link);
    b.server = None;
    sup_c.set(&list, orphan(2));
    wait_for("C takes over", || (sup_c.verdict() == Verdict::TakeOver(3)).then_some(()));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_standby_show_takes_over_what_it_mirrored() {
    init();
    use opnq::engine::InstanceSnapshot;
    use opnq::model::{Cue, CueKind, KindTag, Workspace};

    let mut ws = Workspace::default();
    for n in ["1", "2", "3"] {
        let mut c = Cue::new(CueKind::Wait { duration: 60.0 });
        c.number = n.into();
        ws.cue_list.cues.push(c);
    }
    ws.settings.audio_device = Some("the main device's card".into());
    let ids: Vec<_> = ws.cue_list.cues.iter().map(|c| c.id).collect();

    // A replica: it holds the main device's show and mirrors what runs there.
    let mut show = Show::new(Prober::new());
    show.replace_workspace(ws, None);
    show.replica = true;
    let snap = InstanceSnapshot {
        id: ids[0],
        kind: KindTag::Wait,
        pre_wait: 0.0,
        pre_wait_elapsed: 0.0,
        action_elapsed: 10.0,
        paused: false,
        started: true,
        action_done: false,
        action_duration: Some(60.0),
        continue_done: false,
        parent: None,
    };
    show.engine.mirror(Some(ids[1]), false, &[snap], Vec::new(), Instant::now());

    // Its own settings: no audio device choice of its own, a different OSC port.
    let mut mine = Workspace::default();
    mine.settings.osc_port = 54321;
    show.take_over(&mine);

    assert!(!show.replica);
    assert!(show.dirty, "the copy exists only here");
    assert_eq!(show.engine.playhead, Some(ids[1]));
    assert!(show.engine.is_running(ids[0]), "the running cue restarts");
    assert!(!show.engine.is_running(ids[1]));
    assert_eq!(show.ws.cue_list.cues.len(), 3);
    assert_eq!(show.ws.settings.osc_port, 54321);
    assert_eq!(show.ws.settings.audio_device.as_deref(), Some("the main device's card"));
    // The restarted wait ends 50 s from now (60 s long, 10 s played).
    let inst = &show.engine.instances[&ids[0]];
    let left = inst.action_duration.unwrap() - inst.action_elapsed(Instant::now());
    assert!((left - 50.0).abs() < 1.0, "{left}");
}

fn join_error(joiner: &Pc, target: &Pc, code: &str) -> String {
    format!("{:#}", client::join(&joiner.cfg, &target.address(), code).unwrap_err())
}

#[test]
fn joining_and_logging_in_are_guarded() {
    init();
    let base = 30000 + (std::process::id() % 20000) as u16 * 3 / 2 + 10;
    let (a, b, c, outsider) = (Pc::new("a", base), Pc::new("b", base + 1), Pc::new("c", base + 2), Pc::new("x", base + 3));
    let first = a.member();
    a.cfg.lock().coupling.create(first);
    a.say(true, 1);
    let server = a.server.as_ref().unwrap();
    let limit = Duration::from_secs(2);

    // A group needs the code, and the code must be open.
    assert!(join_error(&b, &a, "AAAA-AAAA").contains("isn't open"));
    let code = server.open_pairing();
    // A wrong code is refused and uses up a try; the right one still works.
    assert!(join_error(&b, &a, "2222-2222").to_lowercase().contains("wrong"));
    assert_eq!(server.pairing().unwrap().2, opnq::remote::server::PAIRING_ATTEMPTS - 1);
    assert!(b.cfg.lock().coupling.group.is_none(), "a failed join keeps nothing");
    let grant = client::join(&b.cfg, &a.address(), &code).unwrap();
    // The code works once.
    assert!(server.pairing().is_none());
    assert!(join_error(&c, &a, &code).contains("isn't open"));
    assert!(c.cfg.lock().coupling.group.is_none());
    b.cfg.lock().coupling.join(grant.clone());
    assert_eq!(grant.roster.members.len(), 2);
    assert_eq!(a.cfg.lock().coupling.members.len(), 2);

    // The grant is not a device login: nothing is stored as a paired device.
    assert!(a.cfg.lock().devices.is_empty());

    // A computer with no group can't be joined.
    let code = outsider.server.as_ref().unwrap().open_pairing();
    assert!(join_error(&c, &outsider, &code).contains("isn't in a coupling group"));

    // The key is what logs in: same group id with another key, or another group, is refused.
    let group = a.cfg.lock().coupling.group.clone().unwrap();
    for (name, g) in [
        ("wrong key", opnq::remote::coupling::Group { id: group.id, key: "0".repeat(64) }),
        ("other group", opnq::remote::coupling::Group::create()),
    ] {
        let mut cfg = outsider.cfg.lock();
        cfg.coupling.group = Some(g);
        cfg.coupling.enabled = true;
        drop(cfg);
        let (peer, roster) = client::probe(&outsider.cfg, a.id(), &a.address(), limit);
        assert!(matches!(peer, Peer::NotPaired(_)), "{name}: {peer:?}");
        assert!(roster.is_none(), "{name}: no list for a stranger");
        let link = Link::connect(&a.address(), Auth::Group, outsider.cfg.clone(), || {});
        wait_for("refusal", || matches!(link.state(), LinkState::Closed { .. }).then_some(()));
    }
    // A paired device (a tablet) has no way into the group: its key is not the group's.
    let code = server.open_pairing();
    let tablet = Link::connect(&a.address(), Auth::Pair(code), c.cfg.clone(), || {});
    wait_for("tablet paired", || matches!(tablet.state(), LinkState::Connected { .. }).then_some(()));
    assert!(c.cfg.lock().coupling.group.is_none(), "pairing a tablet gives no group key");
    assert_eq!(a.cfg.lock().devices.len(), 1);
    assert_eq!(a.cfg.lock().coupling.members.len(), 2, "and the tablet isn't a member");
    drop(tablet);

    // A member connects as Control, is not stored as a device, and can't edit.
    let link = Link::connect(&a.address(), Auth::Group, b.cfg.clone(), || {});
    wait_for("member connected", || matches!(link.state(), LinkState::Connected { role: opnq::remote::Role::Control, .. }).then_some(()));
    assert_eq!(a.cfg.lock().devices.len(), 1, "still only the tablet");
    link.send(opnq::remote::ClientMsg::Edit(Box::default()));
    wait_for("edit refused", || link.drain().into_iter().find_map(|m| matches!(m, opnq::remote::ServerMsg::Refused(_)).then_some(())));
    drop(link);

    // Coupling off on a computer: its group's key no longer opens it.
    a.cfg.lock().coupling.enabled = false;
    assert!(matches!(client::probe(&b.cfg, a.id(), &a.address(), limit).0, Peer::NotPaired(_)));
    a.cfg.lock().coupling.enabled = true;
    assert!(matches!(client::probe(&b.cfg, a.id(), &a.address(), limit).0, Peer::Main { .. }));

    // A full group takes nobody else.
    {
        let mut cfg = a.cfg.lock();
        while cfg.coupling.members.len() < opnq::remote::coupling::MAX_MEMBERS {
            let n = cfg.coupling.members.len() as u128;
            cfg.coupling.members.push(Member { id: uuid::Uuid::from_u128(1000 + n), name: format!("m{n}"), address: String::new() });
        }
    }
    let code = server.open_pairing();
    assert!(join_error(&c, &a, &code).contains("full"));
    std::fs::remove_dir_all(init()).ok();
}

#[test]
fn a_standby_computer_serves_nobody_and_drops_its_clients() {
    init();
    let base = 30000 + (std::process::id() % 20000) as u16 * 3 / 2 + 20;
    let (a, b) = (Pc::new("a", base), Pc::new("b", base + 1));
    let first = a.member();
    a.cfg.lock().coupling.create(first);
    let code = a.server.as_ref().unwrap().open_pairing();
    let grant = client::join(&b.cfg, &a.address(), &code).unwrap();
    b.cfg.lock().coupling.join(grant);
    a.say(true, 1);
    b.say(false, 1);
    let link = Link::connect(&a.address(), Auth::Group, b.cfg.clone(), || {});
    wait_for("connected", || matches!(link.state(), LinkState::Connected { .. }).then_some(()));
    // A steps down while B is connected: B is let go.
    a.say(false, 2);
    wait_for("dropped", || matches!(link.state(), LinkState::Closed { .. }).then_some(()));
    let link = Link::connect(&a.address(), Auth::Group, b.cfg.clone(), || {});
    let why = wait_for("refused", || match link.state() {
        LinkState::Closed { reason, .. } => Some(reason),
        _ => None,
    });
    assert_eq!(why, client::STANDBY);
}
