//! Main-device side: accept paired devices, keep them in sync and take their commands.

use super::secure::{self, HandshakeError, Hello, ServerInfo};
use super::netguard::{self, NetworkChange};
use super::{ClientMsg, LiveState, RemoteConfig, Role, ServerMsg, TrustedDevice, normalize_code, unix_now};
use crate::media::audio::MeterReader;
use crate::model::Workspace;
use crate::show::SharedShow;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a pairing code stays valid, and how many wrong tries close it.
pub const PAIRING_TTL: Duration = Duration::from_secs(300);
/// Wrong codes allowed before pairing closes.
pub const PAIRING_ATTEMPTS: u32 = 5;
/// Failed logins from one address before it is blocked, and for how long.
const MAX_FAILURES: u32 = 10;
const BLOCK_FOR: Duration = Duration::from_secs(300);
const MAX_CONNECTIONS: usize = 32;
/// Connections from one address at a time (a replica needs one; this leaves room for a few
/// instances on one computer while one host can't take every slot).
const MAX_PER_ADDRESS: usize = 8;
/// Handshakes in progress at a time, and from one address. A real handshake takes milliseconds,
/// so these only bite peers that connect and stall: they can't hold the slots authenticated
/// devices use, and one host can't keep the others from logging in.
const MAX_HANDSHAKES: usize = 16;
const MAX_HANDSHAKES_PER_ADDRESS: usize = 2;
/// Addresses remembered for failed logins (the oldest is forgotten first).
const MAX_FAILURE_ENTRIES: usize = 4096;
/// Largest message taken from a device that can't edit (commands, selections, pings).
const SMALL_MESSAGE: usize = 1 << 20;
/// Waveform requests one connection may have waiting.
const WAVEFORM_QUEUE: usize = 64;
/// Replicas get state at most this often, and at least once a second as a heartbeat.
const STATE_INTERVAL: Duration = Duration::from_millis(50);
const HEARTBEAT: Duration = Duration::from_secs(1);
/// A replica that sends nothing (not even a ping) for this long is dropped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// An open pairing window.
pub struct Pairing {
    /// Code to enter on the new device.
    pub code: String,
    /// When the code stops working.
    pub expires: Instant,
    /// Tries left.
    pub attempts_left: u32,
}

/// A device connected right now.
#[derive(Clone, Debug)]
pub struct Connected {
    /// Connection number.
    pub conn: u64,
    /// Device id.
    pub device: Uuid,
    /// Device name.
    pub name: String,
    /// Its address.
    pub addr: SocketAddr,
    /// Connected since.
    pub since: Instant,
}

struct Failures {
    count: u32,
    last: Instant,
}

struct Shared {
    show: SharedShow,
    config: Arc<Mutex<RemoteConfig>>,
    pairing: Mutex<Option<Pairing>>,
    clients: Mutex<Vec<Connected>>,
    failures: Mutex<HashMap<IpAddr, Failures>>,
    /// Open connections per address.
    per_address: Mutex<HashMap<IpAddr, usize>>,
    /// Handshakes in progress per address.
    handshaking: Mutex<HashMap<IpAddr, usize>>,
    /// The main device is in show mode: nobody edits, Edit-role devices act as Control.
    show_mode: AtomicBool,
    events: Mutex<VecDeque<String>>,
    stop: Arc<AtomicBool>,
    next_conn: AtomicU64,
    /// Meter peaks shared by all replicas (each read of the mixer resets it).
    meters: Mutex<(Instant, Vec<f32>)>,
    repaint: Box<dyn Fn() + Send + Sync>,
    port: u16,
}

impl Shared {
    fn event(&self, msg: String) {
        log::info!("remote: {msg}");
        let mut e = self.events.lock();
        let stamp = chrono_like_now();
        e.push_back(format!("{stamp}  {msg}"));
        while e.len() > 50 {
            e.pop_front();
        }
        drop(e);
        (self.repaint)();
    }

    fn blocked(&self, ip: IpAddr) -> bool {
        let mut f = self.failures.lock();
        f.retain(|_, v| v.last.elapsed() < BLOCK_FOR);
        f.get(&ip).is_some_and(|v| v.count >= MAX_FAILURES)
    }

    fn record_failure(&self, ip: IpAddr) {
        let mut f = self.failures.lock();
        if f.len() >= MAX_FAILURE_ENTRIES && !f.contains_key(&ip) {
            f.retain(|_, v| v.last.elapsed() < BLOCK_FOR);
            if f.len() >= MAX_FAILURE_ENTRIES
                && let Some(oldest) = f.iter().min_by_key(|(_, v)| v.last).map(|(k, _)| *k)
            {
                f.remove(&oldest);
            }
        }
        let e = f.entry(ip).or_insert(Failures { count: 0, last: Instant::now() });
        e.count += 1;
        e.last = Instant::now();
    }

    /// `role` as it applies now: in show mode nobody edits.
    fn effective(&self, role: Role) -> Role {
        if role == Role::Edit && self.show_mode.load(Ordering::Acquire) { Role::Control } else { role }
    }

    fn meters(&self) -> Vec<f32> {
        let mut m = self.meters.lock();
        if m.0.elapsed() >= Duration::from_millis(40) {
            m.1 = self.show.lock().media.take_meters(MeterReader::Remote);
            m.0 = Instant::now();
        }
        m.1.clone()
    }
}

/// Local wall-clock time as HH:MM:SS (UTC offset ignored: good enough for an event list).
fn chrono_like_now() -> String {
    let s = unix_now();
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

/// A running remote-access server. Dropping it stops listening and disconnects everyone.
pub struct Server {
    shared: Arc<Shared>,
}

impl Server {
    /// Listen on the configured port and answer discovery queries.
    pub fn start(show: SharedShow, config: Arc<Mutex<RemoteConfig>>, repaint: impl Fn() + Send + Sync + 'static) -> anyhow::Result<Self> {
        let (port, id) = {
            let c = config.lock();
            (c.port, c.instance_id)
        };
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
            .map_err(|e| anyhow::anyhow!("cannot listen on port {port}: {e}"))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            show,
            config: config.clone(),
            pairing: Mutex::new(None),
            clients: Mutex::new(Vec::new()),
            failures: Mutex::new(HashMap::new()),
            per_address: Mutex::new(HashMap::new()),
            handshaking: Mutex::new(HashMap::new()),
            show_mode: AtomicBool::new(false),
            events: Mutex::new(VecDeque::new()),
            stop: stop.clone(),
            next_conn: AtomicU64::new(1),
            meters: Mutex::new((Instant::now(), Vec::new())),
            repaint: Box::new(repaint),
            port,
        });
        let cfg = config.clone();
        if let Err(e) = super::discovery::respond(id, move || cfg.lock().instance_name.clone(), port, stop.clone()) {
            log::warn!("remote discovery unavailable: {e}");
        }
        let sh = shared.clone();
        std::thread::Builder::new().name("openq-remote-accept".into()).spawn(move || accept_loop(sh, listener))?;
        shared.event(format!("Remote access on, port {port}"));
        Ok(Self { shared })
    }

    /// Port being listened on.
    pub fn port(&self) -> u16 {
        self.shared.port
    }

    /// Open pairing with a fresh code (replacing any open one).
    pub fn open_pairing(&self) -> String {
        let code = super::new_pairing_code();
        *self.shared.pairing.lock() =
            Some(Pairing { code: code.clone(), expires: Instant::now() + PAIRING_TTL, attempts_left: PAIRING_ATTEMPTS });
        self.shared.event("Pairing opened".into());
        code
    }

    /// Withdraw the pairing code.
    pub fn close_pairing(&self) {
        if self.shared.pairing.lock().take().is_some() {
            self.shared.event("Pairing closed".into());
        }
    }

    /// The open pairing code, with time and attempts left.
    pub fn pairing(&self) -> Option<(String, Duration, u32)> {
        let mut p = self.shared.pairing.lock();
        if p.as_ref().is_some_and(|p| p.expires <= Instant::now() || p.attempts_left == 0) {
            *p = None;
        }
        p.as_ref().map(|p| (p.code.clone(), p.expires.saturating_duration_since(Instant::now()), p.attempts_left))
    }

    /// Tell the server whether the main device is in show mode. While it is, no device may edit:
    /// Edit-role devices are told they are Control until it leaves show mode (their stored role
    /// stays Edit).
    pub fn set_show_mode(&self, on: bool) {
        if self.shared.show_mode.swap(on, Ordering::AcqRel) != on {
            self.shared.event(if on { "Show mode: editing from devices is off".into() } else { "Edit mode: devices with the edit role may edit".into() });
        }
    }

    /// Devices connected right now.
    pub fn connected(&self) -> Vec<Connected> {
        self.shared.clients.lock().clone()
    }

    /// Recent security/activity events, oldest first.
    pub fn events(&self) -> Vec<String> {
        self.shared.events.lock().iter().cloned().collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

fn accept_loop(shared: Arc<Shared>, listener: TcpListener) {
    let active = Arc::new(AtomicU64::new(0));
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                let ip = addr.ip();
                if shared.blocked(ip) || active.load(Ordering::Relaxed) as usize >= MAX_CONNECTIONS {
                    continue; // dropping the stream closes it
                }
                let Some(slot) = HandshakeSlot::take(&shared, ip) else { continue };
                {
                    let mut per = shared.per_address.lock();
                    let n = per.entry(ip).or_insert(0);
                    if *n >= MAX_PER_ADDRESS {
                        continue;
                    }
                    *n += 1;
                }
                let (sh, act) = (shared.clone(), active.clone());
                act.fetch_add(1, Ordering::Relaxed);
                let spawned = std::thread::Builder::new().name("openq-remote-conn".into()).spawn(move || {
                    // Even a connection that fails in an unexpected way gives its slot back,
                    // or the server would end up refusing everyone.
                    if let Err(e) = crate::recovery::catch(|| serve(&sh, stream, addr, slot)) {
                        log::debug!("remote connection {addr}: {e:#}");
                    }
                    act.fetch_sub(1, Ordering::Relaxed);
                    release_address(&sh, ip);
                });
                if spawned.is_err() {
                    active.fetch_sub(1, Ordering::Relaxed);
                    release_address(&shared, ip);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                log::warn!("remote accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// A handshake in progress ([`MAX_HANDSHAKES`], [`MAX_HANDSHAKES_PER_ADDRESS`]); given back when
/// released or dropped (also if the connection's thread fails).
struct HandshakeSlot {
    shared: Arc<Shared>,
    ip: IpAddr,
    held: bool,
}

impl HandshakeSlot {
    fn take(shared: &Arc<Shared>, ip: IpAddr) -> Option<Self> {
        let mut h = shared.handshaking.lock();
        if h.values().sum::<usize>() >= MAX_HANDSHAKES || h.get(&ip).is_some_and(|n| *n >= MAX_HANDSHAKES_PER_ADDRESS) {
            return None;
        }
        *h.entry(ip).or_insert(0) += 1;
        Some(Self { shared: shared.clone(), ip, held: true })
    }

    fn release(&mut self) {
        if std::mem::take(&mut self.held) {
            let mut h = self.shared.handshaking.lock();
            if let Some(n) = h.get_mut(&self.ip) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    h.remove(&self.ip);
                }
            }
        }
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        self.release();
    }
}

/// A connection from `ip` ended.
fn release_address(shared: &Shared, ip: IpAddr) {
    let mut per = shared.per_address.lock();
    if let Some(n) = per.get_mut(&ip) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            per.remove(&ip);
        }
    }
}

use super::vet::media_paths;

fn title_of(path: &Option<PathBuf>) -> String {
    path.as_ref().and_then(|p| p.file_stem()).map_or_else(|| "Untitled".into(), |s| s.to_string_lossy().into_owned())
}

fn serve(shared: &Arc<Shared>, stream: TcpStream, addr: SocketAddr, mut slot: HandshakeSlot) -> anyhow::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let info = {
        let c = shared.config.lock();
        ServerInfo { id: c.instance_id, name: c.instance_name.clone() }
    };

    const NOT_OPEN: &str = "Pairing isn't open on the main device. Open it there (Remote → Pair a device) and try again.";
    let open = |p: &Pairing| p.expires > Instant::now() && p.attempts_left > 0;
    let secret_for = |hello: &Hello| -> Result<Vec<u8>, String> {
        if hello.pairing {
            match shared.pairing.lock().as_ref() {
                Some(pr) if open(pr) => Ok(normalize_code(&pr.code).into_bytes()),
                _ => Err(NOT_OPEN.into()),
            }
        } else {
            shared
                .config
                .lock()
                .devices
                .iter()
                .find(|d| d.id == hello.client_id)
                .map(|d| d.token.clone().into_bytes())
                .ok_or_else(|| "This device isn't paired with the main device (or was removed). Pair it with a code.".into())
        }
    };

    // A pairing attempt is used up when the proof arrives, under the lock, so parallel
    // connections can't test more codes than allowed, and peers that stall before proving
    // anything hold no attempts.
    let may_check = |hello: &Hello| -> Result<(), String> {
        if hello.pairing {
            let mut p = shared.pairing.lock();
            match p.as_mut() {
                // Every checked attempt counts, right or wrong; success closes pairing.
                Some(pr) if open(pr) => pr.attempts_left -= 1,
                _ => return Err(NOT_OPEN.into()),
            }
        }
        Ok(())
    };

    let handshake = secure::server_handshake(stream, &info, secret_for, may_check);
    slot.release();
    let (mut tx, mut rx, hello) = match handshake {
        Ok(ok) => ok,
        Err(HandshakeError::Auth(why)) => {
            shared.record_failure(addr.ip());
            let mut p = shared.pairing.lock();
            if p.as_ref().is_some_and(|p| p.attempts_left == 0) {
                *p = None;
                drop(p);
                shared.event("Pairing closed after too many wrong codes".into());
            }
            shared.event(format!("Rejected {addr}: {why}"));
            // Slow down guessing.
            std::thread::sleep(Duration::from_secs(1));
            return Ok(());
        }
        Err(HandshakeError::Refused(why)) => {
            shared.event(format!("Refused {addr}: {why}"));
            return Ok(());
        }
        Err(HandshakeError::Io(e)) => {
            // Holding a connection open without finishing the handshake counts as a failure
            // (a host that keeps doing it would hold the connection slots).
            if secure::is_timeout(&e) {
                shared.record_failure(addr.ip());
                shared.event(format!("Dropped {addr}: the handshake took too long"));
            }
            return Err(e);
        }
    };

    // Authenticated. A pairing connection becomes a trusted device with its own token.
    let device_name: String = hello.client_name.chars().filter(|c| !c.is_control()).take(64).collect();
    let (role, token) = {
        let mut c = shared.config.lock();
        let now = unix_now();
        let token = if hello.pairing {
            let token = super::new_token();
            let role = c.default_role;
            c.devices.retain(|d| d.id != hello.client_id);
            c.devices.push(TrustedDevice {
                id: hello.client_id,
                name: device_name.clone(),
                token: token.clone(),
                role,
                paired_at: now,
                last_seen: now,
            });
            Some(token)
        } else {
            None
        };
        let dev = c.devices.iter_mut().find(|d| d.id == hello.client_id).expect("device exists after auth");
        dev.last_seen = now;
        dev.name = device_name.clone();
        let role = shared.effective(dev.role);
        if let Err(e) = c.save() {
            log::warn!("cannot save remote config: {e:#}");
        }
        (role, token)
    };
    if hello.pairing {
        *shared.pairing.lock() = None;
        shared.event(format!("Paired {device_name} ({addr}) as {}", role.label()));
    }
    shared.failures.lock().remove(&addr.ip());
    tx.send(&ServerMsg::Welcome { role, token })?;

    let conn = shared.next_conn.fetch_add(1, Ordering::Relaxed);
    shared.clients.lock().push(Connected { conn, device: hello.client_id, name: device_name.clone(), addr, since: Instant::now() });
    shared.event(format!("{device_name} connected from {addr}"));

    let closed = Arc::new(AtomicBool::new(false));
    let (notice_tx, notice_rx) = std::sync::mpsc::channel::<ServerMsg>();
    let flags = Arc::new(Flags::default());
    let writer = {
        let (sh, closed, flags) = (shared.clone(), closed.clone(), flags.clone());
        std::thread::Builder::new()
            .name("openq-remote-send".into())
            .spawn(move || {
                if let Err(e) = send_loop(&sh, &mut tx, hello.client_id, role, &closed, &flags, notice_rx) {
                    log::debug!("remote send: {e:#}");
                }
                closed.store(true, Ordering::Relaxed);
                tx.shutdown();
            })?
    };

    rx.set_timeout(Some(IDLE_TIMEOUT));
    let result = receive_loop(shared, &mut rx, hello.client_id, &device_name, &closed, &flags, &notice_tx);
    closed.store(true, Ordering::Relaxed);
    let _ = writer.join();
    shared.clients.lock().retain(|c| c.conn != conn);
    shared.event(format!("{device_name} disconnected"));
    result
}

/// Requests a replica makes of its connection's sender.
#[derive(Default)]
struct Flags {
    /// Set by the replica's Resync: the next workspace goes whole.
    resync: AtomicBool,
    /// The replica wants the DMX output ([`ServerMsg::Dmx`]).
    dmx: AtomicBool,
}

/// The role `device` acts with now (its own role, but no editing in show mode); None if removed.
fn role_of(shared: &Shared, device: Uuid) -> Option<Role> {
    let role = shared.config.lock().devices.iter().find(|d| d.id == device).map(|d| d.role)?;
    Some(shared.effective(role))
}

/// Why an edit from a device that may not edit now is refused.
fn edit_refusal(shared: &Shared) -> String {
    if shared.show_mode.load(Ordering::Acquire) {
        "The main device is in show mode: editing is off until it goes back to edit mode".into()
    } else {
        "This device can't edit the show".into()
    }
}

fn receive_loop(
    shared: &Arc<Shared>,
    rx: &mut secure::Receiver,
    device: Uuid,
    device_name: &str,
    closed: &AtomicBool,
    flags: &Flags,
    notices: &std::sync::mpsc::Sender<ServerMsg>,
) -> anyhow::Result<()> {
    let mut waveforms = Waveforms::new(notices.clone());
    while !closed.load(Ordering::Relaxed) && !shared.stop.load(Ordering::Relaxed) {
        // Only an editor may send big messages (a whole show): the others can't make the main
        // device hold one in memory.
        let editor = role_of(shared, device).is_some_and(Role::can_edit);
        rx.set_limit(if editor { secure::MAX_MESSAGE } else { SMALL_MESSAGE });
        let msg: ClientMsg = rx.recv()?;
        // Look the role up for every message, so changes and removals apply at once.
        let Some(role) = role_of(shared, device) else { return Ok(()) };
        match msg {
            ClientMsg::Ping => {}
            ClientMsg::Resync => flags.resync.store(true, Ordering::Release),
            ClientMsg::Subscribe { dmx } => flags.dmx.store(dmx, Ordering::Release),
            ClientMsg::Select(fixtures) => {
                if role.can_control() {
                    let mut show = shared.show.lock();
                    show.light_selection = fixtures.into_iter().take(crate::light::model::MAX_FIXTURE_ID as usize).collect();
                    show.light_selection_gen += 1;
                    drop(show);
                    (shared.repaint)();
                } else {
                    let _ = notices.send(ServerMsg::Refused("This device is view-only".into()));
                }
            }
            ClientMsg::Command(cmd) => {
                if role.can_control() {
                    let r = crate::control::apply(&mut shared.show.lock(), cmd, Instant::now());
                    if let Err(e) = r {
                        let _ = notices.send(ServerMsg::Refused(e));
                    }
                } else {
                    let _ = notices.send(ServerMsg::Refused("This device is view-only".into()));
                }
            }
            ClientMsg::Patch { mut cues, mut lighting } => {
                if !role.can_edit() {
                    refuse_edit(shared, flags, notices, edit_refusal(shared));
                    continue;
                }
                let mut show = shared.show.lock();
                if let Err(why) = super::vet::cues(&show.ws, &cues).and_then(|_| super::vet::numbers(&lighting)) {
                    drop(show);
                    refuse_edit(shared, flags, notices, why);
                    continue;
                }
                let mut held = NetworkChange::default();
                netguard::hold_cues(&show.ws, &mut cues, &mut held);
                if let Some(p) = lighting.as_deref_mut() {
                    netguard::hold_lighting(&show.ws, p, &mut held);
                }
                if show.apply_remote_patch(&cues, lighting.as_deref()) {
                    hold_for_operator(shared, &mut show, flags, notices, device_name, held);
                    drop(show);
                    (shared.repaint)();
                } else {
                    drop(show);
                    // The replica's copy has drifted: its edit is lost, it gets the whole show.
                    flags.resync.store(true, Ordering::Release);
                    let _ = notices.send(ServerMsg::Refused("An edit didn't fit the show on the main device; resyncing".into()));
                }
            }
            ClientMsg::Reshape { mut ws, mut lighting } => {
                if !role.can_edit() {
                    refuse_edit(shared, flags, notices, edit_refusal(shared));
                    continue;
                }
                let mut show = shared.show.lock();
                if let Err(why) = super::vet::workspace(&show.ws, &ws).and_then(|_| super::vet::numbers(&lighting)) {
                    drop(show);
                    refuse_edit(shared, flags, notices, why);
                    continue;
                }
                let mut held = NetworkChange::default();
                netguard::hold_workspace(&show.ws, &mut ws, &mut held);
                if let Some(p) = lighting.as_deref_mut() {
                    netguard::hold_lighting(&show.ws, p, &mut held);
                }
                ws.lighting = show.ws.lighting.clone();
                if lighting.as_ref().is_some_and(|p| !crate::light::patch::apply(&mut ws.lighting, p)) {
                    drop(show);
                    flags.resync.store(true, Ordering::Release);
                    let _ = notices.send(ServerMsg::Refused("An edit didn't fit the show on the main device; resyncing".into()));
                    continue;
                }
                show.apply_remote_edit(*ws);
                hold_for_operator(shared, &mut show, flags, notices, device_name, held);
                drop(show);
                (shared.repaint)();
            }
            ClientMsg::Edit(mut ws) => {
                if role.can_edit() {
                    let mut show = shared.show.lock();
                    if let Err(why) = super::vet::workspace(&show.ws, &ws) {
                        drop(show);
                        refuse_edit(shared, flags, notices, why);
                        continue;
                    }
                    let mut held = NetworkChange::default();
                    netguard::hold_workspace(&show.ws, &mut ws, &mut held);
                    show.apply_remote_edit(*ws);
                    hold_for_operator(shared, &mut show, flags, notices, device_name, held);
                    drop(show);
                    (shared.repaint)();
                } else {
                    refuse_edit(shared, flags, notices, edit_refusal(shared));
                }
            }
            ClientMsg::Waveform(path) => {
                // Only files the show uses: a replica must not be able to read anything else.
                let known = media_paths(&shared.show.lock().ws).contains(&path);
                if known {
                    waveforms.request(path);
                } else {
                    let _ = notices.send(ServerMsg::Waveform(path, Err("not part of the show".into())));
                }
            }
        }
    }
    Ok(())
}

/// Queue network settings changes held back from a replica's edit for the operator; the replica
/// is told and gets the show as it now is (without them).
fn hold_for_operator(
    shared: &Shared,
    show: &mut crate::show::Show,
    flags: &Flags,
    notices: &std::sync::mpsc::Sender<ServerMsg>,
    device_name: &str,
    held: NetworkChange,
) {
    if held.is_empty() {
        return;
    }
    shared.event(format!("{device_name} asks to change network settings: {}", held.describe().join("; ")));
    show.request_network_change(device_name, held);
    flags.resync.store(true, Ordering::Release);
    let _ = notices.send(ServerMsg::Refused("Network settings changes wait for approval on the main device".into()));
}

/// Refuse a replica's edit: it gets the reason and, as its copy now differs, the whole show.
fn refuse_edit(shared: &Shared, flags: &Flags, notices: &std::sync::mpsc::Sender<ServerMsg>, why: String) {
    shared.event(format!("Refused an edit: {why}"));
    flags.resync.store(true, Ordering::Release);
    let _ = notices.send(ServerMsg::Refused(why));
}

/// Waveforms a connection asked for, made one at a time on its own worker thread (a replica
/// can't start any number of decodes at once).
struct Waveforms {
    notices: std::sync::mpsc::Sender<ServerMsg>,
    queue: Option<std::sync::mpsc::SyncSender<PathBuf>>,
}

impl Waveforms {
    fn new(notices: std::sync::mpsc::Sender<ServerMsg>) -> Self {
        Self { notices, queue: None }
    }

    fn request(&mut self, path: PathBuf) {
        if self.queue.is_none() {
            let (tx, rx) = std::sync::mpsc::sync_channel::<PathBuf>(WAVEFORM_QUEUE);
            let notices = self.notices.clone();
            // The worker ends when the connection drops its queue.
            let spawned = std::thread::Builder::new().name("openq-remote-waveform".into()).spawn(move || {
                for path in rx {
                    let r = crate::recovery::catch(|| crate::media::decoder::waveform(&path)).map_err(|e| format!("{e:#}"));
                    if notices.send(ServerMsg::Waveform(path, r)).is_err() {
                        break;
                    }
                }
            });
            if spawned.is_ok() {
                self.queue = Some(tx);
            }
        }
        let refused = match &self.queue {
            Some(q) => match q.try_send(path) {
                Ok(()) => None,
                Err(std::sync::mpsc::TrySendError::Full(p) | std::sync::mpsc::TrySendError::Disconnected(p)) => Some(p),
            },
            None => Some(path),
        };
        if let Some(path) = refused {
            let _ = self.notices.send(ServerMsg::Waveform(path, Err("too many waveform requests".into())));
        }
    }
}

fn send_loop(
    shared: &Arc<Shared>,
    tx: &mut secure::Sender,
    device: Uuid,
    mut role: Role,
    closed: &AtomicBool,
    flags: &Flags,
    notices: std::sync::mpsc::Receiver<ServerMsg>,
) -> anyhow::Result<()> {
    let mut last_ws: Option<Workspace> = None;
    // Comparing whole workspaces under the show lock every 50 ms (per replica) costs real time on
    // big shows, so it's done when the edit generation moves, or once per heartbeat for changes
    // that don't bump it (loading another show).
    let mut last_gen = None;
    let mut last_compare = Instant::now();
    let mut last_state: Option<LiveState> = None;
    let mut last_sent = Instant::now();
    let mut media_sent: HashSet<PathBuf> = HashSet::new();
    // DMX output as last sent to a replica that asked for it (None: send everything next).
    let mut dmx_sent: Option<crate::light::dmx::DmxFrame> = None;
    while !closed.load(Ordering::Relaxed) && !shared.stop.load(Ordering::Relaxed) {
        match role_of(shared, device) {
            None => {
                tx.send(&ServerMsg::Refused("This device was removed on the main device".into()))?;
                return Ok(());
            }
            Some(r) if r != role => {
                role = r;
                tx.send(&ServerMsg::Role(r))?;
            }
            _ => {}
        }
        while let Ok(n) = notices.try_recv() {
            tx.send(&n)?;
        }

        if flags.resync.swap(false, Ordering::AcqRel) {
            last_ws = None;
            last_gen = None;
        }
        let meters = shared.meters();
        let want_dmx = flags.dmx.load(Ordering::Acquire);
        if !want_dmx {
            dmx_sent = None;
        }
        let (ws, title, state, prober, dmx) = {
            let show = shared.show.lock();
            let now = Instant::now();
            let compare = last_gen != Some(show.edit_gen) || now.duration_since(last_compare) >= HEARTBEAT;
            let ws = if compare {
                last_gen = Some(show.edit_gen);
                last_compare = now;
                (last_ws.as_ref() != Some(&show.ws)).then(|| show.ws.clone())
            } else {
                None
            };
            let state = LiveState {
                playhead: show.engine.playhead,
                panicking: show.engine.is_panicking(),
                instances: show.engine.snapshot(now, |id| show.media.elapsed(id)),
                log: show.engine.log.clone(),
                meters,
                dirty: show.dirty,
                lighting: super::LightLive {
                    masters: show.media.light.masters().clone(),
                    sequences: show.media.light.status(),
                    selection: show.light_selection.clone(),
                },
            };
            let dmx = want_dmx.then(|| show.media.light.frame());
            (ws, title_of(&show.path), state, show.media.prober.clone(), dmx)
        };
        if let Some(frame) = dmx {
            let changed: Vec<(u16, Vec<u8>)> = frame
                .universes
                .iter()
                .filter(|u| dmx_sent.as_ref().and_then(|f| f.get(u.id)) != Some(u))
                .map(|u| (u.id, u.data.to_vec()))
                .collect();
            if !changed.is_empty() {
                tx.send(&ServerMsg::Dmx(changed))?;
            }
            dmx_sent = Some(frame);
        }
        if let Some(ws) = ws {
            // Small edits (a slider being dragged) go as the changed cues and lighting design
            // changes, not the whole show.
            match last_ws.as_ref().and_then(|old| super::workspace_patch(old, &ws)) {
                Some((cues, lighting)) => {
                    if !cues.is_empty() || lighting.is_none() {
                        tx.send(&ServerMsg::Cues(cues))?;
                    }
                    if let Some(l) = lighting {
                        tx.send(&ServerMsg::Lighting(Box::new(l)))?;
                    }
                }
                None => match &last_ws {
                    // The replica has a design: keep it, send only its changes.
                    Some(old) => {
                        let mut light = ws.clone();
                        light.lighting = Default::default();
                        tx.send(&ServerMsg::Workspace { ws: Box::new(light), title, keep_lighting: true })?;
                        if let Some(l) = crate::light::patch::diff(&old.lighting, &ws.lighting) {
                            tx.send(&ServerMsg::Lighting(Box::new(l)))?;
                        }
                    }
                    None => tx.send(&ServerMsg::Workspace { ws: Box::new(ws.clone()), title, keep_lighting: false })?,
                },
            }
            last_ws = Some(ws);
        }
        if let Some(ws) = &last_ws {
            let fresh: Vec<_> = media_paths(ws)
                .into_iter()
                .filter(|p| !media_sent.contains(p))
                .filter_map(|p| prober.get(&p).map(|r| (p, r)))
                .collect();
            if !fresh.is_empty() {
                media_sent.extend(fresh.iter().map(|f| f.0.clone()));
                tx.send(&ServerMsg::MediaInfo(fresh))?;
            }
        }
        if last_state.as_ref() != Some(&state) || last_sent.elapsed() >= HEARTBEAT {
            tx.send(&ServerMsg::State(Box::new(state.clone())))?;
            last_state = Some(state);
            last_sent = Instant::now();
        }
        std::thread::sleep(STATE_INTERVAL);
    }
    Ok(())
}
