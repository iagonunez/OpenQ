//! Main-device side: accept paired devices, keep them in sync and take their commands.

use super::secure::{self, HandshakeError, Hello, ServerInfo};
use super::{ClientMsg, LiveState, RemoteConfig, Role, ServerMsg, TrustedDevice, normalize_code, unix_now};
use crate::media::audio::MeterReader;
use crate::model::{CueKind, Workspace};
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
        let e = f.entry(ip).or_insert(Failures { count: 0, last: Instant::now() });
        e.count += 1;
        e.last = Instant::now();
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
                if shared.blocked(addr.ip()) || active.load(Ordering::Relaxed) as usize >= MAX_CONNECTIONS {
                    continue; // dropping the stream closes it
                }
                let (sh, act) = (shared.clone(), active.clone());
                act.fetch_add(1, Ordering::Relaxed);
                let _ = std::thread::Builder::new().name("openq-remote-conn".into()).spawn(move || {
                    if let Err(e) = serve(&sh, stream, addr) {
                        log::debug!("remote connection {addr}: {e:#}");
                    }
                    act.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                log::warn!("remote accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Media files the workspace refers to.
fn media_paths(ws: &Workspace) -> Vec<PathBuf> {
    ws.cue_list
        .iter_all()
        .iter()
        .filter_map(|c| match &c.kind {
            CueKind::Audio(a) => Some(a.path.clone()),
            CueKind::Video(v) => Some(v.path.clone()),
            _ => None,
        })
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

fn title_of(path: &Option<PathBuf>) -> String {
    path.as_ref().and_then(|p| p.file_stem()).map_or_else(|| "Untitled".into(), |s| s.to_string_lossy().into_owned())
}

fn serve(shared: &Arc<Shared>, stream: TcpStream, addr: SocketAddr) -> anyhow::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let info = {
        let c = shared.config.lock();
        ServerInfo { id: c.instance_id, name: c.instance_name.clone() }
    };

    // A pairing attempt is reserved up front so parallel connections can't exceed the limit.
    let reserved = std::cell::Cell::new(false);
    let secret_for = |hello: &Hello| -> Result<Vec<u8>, String> {
        if hello.pairing {
            let mut p = shared.pairing.lock();
            match p.as_mut() {
                Some(pr) if pr.expires > Instant::now() && pr.attempts_left > 0 => {
                    // Every completed attempt counts, right or wrong; success closes pairing.
                    pr.attempts_left -= 1;
                    reserved.set(true);
                    Ok(normalize_code(&pr.code).into_bytes())
                }
                _ => Err("Pairing isn't open on the main device. Open it there (Remote → Pair a device) and try again.".into()),
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

    let (mut tx, mut rx, hello) = match secure::server_handshake(stream, &info, secret_for) {
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
            // Gone before sending its proof: it learned nothing about the code, so the try
            // doesn't count.
            if reserved.get()
                && let Some(p) = shared.pairing.lock().as_mut()
            {
                p.attempts_left = (p.attempts_left + 1).min(PAIRING_ATTEMPTS);
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
        let role = dev.role;
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
    let writer = {
        let (sh, closed) = (shared.clone(), closed.clone());
        std::thread::Builder::new()
            .name("openq-remote-send".into())
            .spawn(move || {
                if let Err(e) = send_loop(&sh, &mut tx, hello.client_id, role, &closed, notice_rx) {
                    log::debug!("remote send: {e:#}");
                }
                closed.store(true, Ordering::Relaxed);
                tx.shutdown();
            })?
    };

    rx.set_timeout(Some(IDLE_TIMEOUT));
    let result = receive_loop(shared, &mut rx, hello.client_id, &closed, &notice_tx);
    closed.store(true, Ordering::Relaxed);
    let _ = writer.join();
    shared.clients.lock().retain(|c| c.conn != conn);
    shared.event(format!("{device_name} disconnected"));
    result
}

fn role_of(shared: &Shared, device: Uuid) -> Option<Role> {
    shared.config.lock().devices.iter().find(|d| d.id == device).map(|d| d.role)
}

fn receive_loop(
    shared: &Arc<Shared>,
    rx: &mut secure::Receiver,
    device: Uuid,
    closed: &AtomicBool,
    notices: &std::sync::mpsc::Sender<ServerMsg>,
) -> anyhow::Result<()> {
    while !closed.load(Ordering::Relaxed) && !shared.stop.load(Ordering::Relaxed) {
        let msg: ClientMsg = rx.recv()?;
        // Look the role up for every message, so changes and removals apply at once.
        let Some(role) = role_of(shared, device) else { return Ok(()) };
        match msg {
            ClientMsg::Ping => {}
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
            ClientMsg::Edit(ws) => {
                if role.can_edit() {
                    shared.show.lock().apply_remote_edit(*ws);
                    (shared.repaint)();
                } else {
                    let _ = notices.send(ServerMsg::Refused("This device can't edit the show".into()));
                }
            }
            ClientMsg::Waveform(path) => {
                // Only files the show uses: a replica must not be able to read anything else.
                let known = media_paths(&shared.show.lock().ws).contains(&path);
                if known {
                    let tx = notices.clone();
                    std::thread::spawn(move || {
                        let r = crate::media::decoder::waveform(&path).map_err(|e| format!("{e:#}"));
                        let _ = tx.send(ServerMsg::Waveform(path, r));
                    });
                } else {
                    let _ = notices.send(ServerMsg::Waveform(path, Err("not part of the show".into())));
                }
            }
        }
    }
    Ok(())
}

fn send_loop(
    shared: &Arc<Shared>,
    tx: &mut secure::Sender,
    device: Uuid,
    mut role: Role,
    closed: &AtomicBool,
    notices: std::sync::mpsc::Receiver<ServerMsg>,
) -> anyhow::Result<()> {
    let mut last_ws: Option<Workspace> = None;
    let mut last_state: Option<LiveState> = None;
    let mut last_sent = Instant::now();
    let mut media_sent: HashSet<PathBuf> = HashSet::new();
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

        let meters = shared.meters();
        let (ws, title, state, prober) = {
            let show = shared.show.lock();
            let ws = (last_ws.as_ref() != Some(&show.ws)).then(|| show.ws.clone());
            let now = Instant::now();
            let state = LiveState {
                playhead: show.engine.playhead,
                panicking: show.engine.is_panicking(),
                instances: show.engine.snapshot(now, |id| show.media.elapsed(id)),
                log: show.engine.log.clone(),
                meters,
                dirty: show.dirty,
            };
            (ws, title_of(&show.path), state, show.media.prober.clone())
        };
        if let Some(ws) = ws {
            tx.send(&ServerMsg::Workspace { ws: Box::new(ws.clone()), title })?;
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
