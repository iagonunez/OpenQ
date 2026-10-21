//! OSC remote control over UDP, with these addresses:
//!
//! | Address | Action |
//! |---|---|
//! | `/go` | GO |
//! | `/stop` | stop all (fade; send twice for immediate) |
//! | `/panic` | stop all immediately |
//! | `/pause`, `/resume` | pause / resume everything |
//! | `/playhead/next`, `/playhead/previous` | move the playhead |
//! | `/playhead/{number}` | put the playhead on a cue |
//! | `/cue/{number}/start` (or `/go`) | fire a cue without moving the playhead |
//! | `/cue/{number}/stop`, `/pause`, `/resume`, `/devamp` | act on one cue |
//! | `/light/executor/{n}/level f` | executor `n`'s level (0..1) |
//! | `/light/executor/{n}/go`, `/release`, `/flash` | executor buttons (1 = press, 0 = let go; no argument = press) |
//! | `/light/grandmaster f`, `/light/blackout`, `/light/release`, `/light/tap` | lighting masters |
//!
//! Any other address can be mapped in the show's control surface settings
//! ([`crate::surface`]); learn mode takes the next address received.
//!
//! OSC has no login, so everything received is checked before it reaches the show
//! ([`sanitize`]): plain addresses only (printable ASCII, no patterns), a few numeric or boolean
//! arguments, and only the addresses above or mapped in the show (anything else is refused).
//! Each sender gets [`MAX_RATE`] messages a second (one flooding host doesn't starve the real
//! surfaces) and all of them together [`MAX_TOTAL_RATE`]; the rest are dropped. Warnings about
//! refused messages are rate-limited too, so a flood can neither hold the show nor fill the error log.

use crate::remote::discovery::Budget;
use crate::show::SharedShow;
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the listener blocks on the socket between checks of its stop flag (bounds how long
/// dropping an [`OscServer`] waits).
const POLL: Duration = Duration::from_millis(50);
/// Longest address taken.
pub const MAX_ADDRESS: usize = 128;
/// Most arguments in one message.
pub const MAX_ARGS: usize = 4;
/// Messages handled per second from one sender at most (a fader sends tens); the rest are dropped.
pub const MAX_RATE: u32 = 500;
/// Messages handled per second from all senders together (bounds the time spent under the show lock).
pub const MAX_TOTAL_RATE: u32 = 2000;
/// Senders given their own budget; beyond that (forged addresses, say) new senders share one.
const MAX_SOURCES: usize = 64;
/// Messages taken from one packet (bundles included).
const MAX_PER_PACKET: usize = 32;
/// Warnings about refused messages written per minute at most.
const MAX_WARNINGS: u32 = 20;

/// A running OSC listener. Dropping it stops the thread and waits for it, so the port is free
/// again (to re-enable or move the listener) as soon as the drop returns.
pub struct OscServer {
    /// UDP port being listened on.
    pub port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for OscServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        log::info!("OSC stopped listening on UDP {}", self.port);
    }
}

impl OscServer {
    /// Listen on `port` on all interfaces; `repaint` is called after each handled message.
    pub fn start(port: u16, show: SharedShow, repaint: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        Self::start_with(port, show, crate::surface::SurfaceHub::new(), repaint)
    }

    /// Like [`OscServer::start`], sharing `hub` with the other control surfaces (learn mode,
    /// button states).
    pub fn start_with(port: u16, show: SharedShow, hub: crate::surface::SurfaceHub, repaint: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let sock = UdpSocket::bind(("0.0.0.0", port))?;
        sock.set_read_timeout(Some(POLL))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new().name("openq-osc".into()).spawn(move || {
            let mut buf = [0u8; rosc::decoder::MTU];
            let mut rate = Rates::default();
            let mut warnings = Warnings::default();
            while !stop2.load(Ordering::Acquire) {
                let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                match rosc::decoder::decode_udp(&buf[..n]) {
                    Ok((_, packet)) => {
                        let mut msgs = Vec::new();
                        flatten(packet, &mut msgs);
                        if msgs.len() > MAX_PER_PACKET {
                            warnings.warn(|| format!("OSC packet from {from} with {} messages refused", msgs.len()));
                            continue;
                        }
                        for m in msgs {
                            if let Err(e) = rate.take(from.ip()) {
                                warnings.warn(|| format!("OSC from {from}: {e}, dropping"));
                                continue;
                            }
                            // Address shown escaped: it comes from anyone on the network.
                            let shown = m.addr.chars().take(MAX_ADDRESS).collect::<String>().escape_debug().to_string();
                            if let Err(e) = sanitize(&m) {
                                warnings.warn(|| format!("OSC \"{shown}\" from {from} refused: {e}"));
                                continue;
                            }
                            hub.osc_from(from.ip());
                            // A failure handling one message must not end the listener.
                            let res = crate::recovery::catch(|| handle_message(&show, &hub, &m).map_err(anyhow::Error::msg))
                                .map_err(|e| format!("{e:#}"));
                            match res {
                                Ok(()) => log::info!("OSC {shown} from {from}"),
                                Err(e) => warnings.warn(|| format!("OSC {shown} from {from}: {e}")),
                            }
                        }
                        repaint();
                    }
                    Err(e) => warnings.warn(|| format!("bad OSC packet from {from}: {e:?}")),
                }
            }
        })?;
        log::info!("OSC listening on UDP {port}");
        Ok(Self { port, stop, thread: Some(thread) })
    }
}

fn flatten(p: rosc::OscPacket, out: &mut Vec<rosc::OscMessage>) {
    match p {
        rosc::OscPacket::Message(m) => out.push(m),
        rosc::OscPacket::Bundle(b) => b.content.into_iter().for_each(|p| flatten(p, out)),
    }
}

/// Rate budgets: one per sender (up to [`MAX_SOURCES`], then one shared by the rest) and one for all.
struct Rates {
    total: Budget,
    sources: std::collections::HashMap<std::net::IpAddr, Budget>,
    others: Budget,
}

impl Default for Rates {
    fn default() -> Self {
        Self { total: Budget::new(MAX_TOTAL_RATE), sources: Default::default(), others: Budget::new(MAX_RATE) }
    }
}

impl Rates {
    /// Use one message's worth for `from`; Err says which budget ran out.
    fn take(&mut self, from: std::net::IpAddr) -> Result<(), String> {
        if self.sources.len() >= MAX_SOURCES && !self.sources.contains_key(&from) {
            // Senders quiet for a while give their place up.
            self.sources.retain(|_, b| b.window.elapsed() < Duration::from_secs(10));
        }
        let own = if self.sources.len() < MAX_SOURCES || self.sources.contains_key(&from) {
            self.sources.entry(from).or_insert_with(|| Budget::new(MAX_RATE))
        } else {
            &mut self.others
        };
        if !own.take() {
            return Err(format!("more than {MAX_RATE} messages a second"));
        }
        if !self.total.take() {
            return Err(format!("more than {MAX_TOTAL_RATE} messages a second from all senders"));
        }
        Ok(())
    }
}

/// Warnings written at most [`MAX_WARNINGS`] a minute (they go to the error log file).
struct Warnings {
    window: Instant,
    count: u32,
}

impl Default for Warnings {
    fn default() -> Self {
        Self { window: Instant::now(), count: 0 }
    }
}

impl Warnings {
    fn warn(&mut self, msg: impl FnOnce() -> String) {
        if self.window.elapsed() >= Duration::from_secs(60) {
            if self.count > MAX_WARNINGS {
                log::warn!("OSC: {} more refused messages were not logged", self.count - MAX_WARNINGS);
            }
            self.window = Instant::now();
            self.count = 0;
        }
        self.count += 1;
        if self.count <= MAX_WARNINGS {
            log::warn!("{}", msg());
        } else if self.count == MAX_WARNINGS + 1 {
            log::warn!("OSC: too many refused messages, the rest of this minute's aren't logged");
        }
    }
}

/// Check a received message before anything acts on it: a plain address (`/` then printable
/// ASCII, at most [`MAX_ADDRESS`] long, no spaces, no empty parts, no OSC patterns such as `*`
/// or `{a,b}`), and at most [`MAX_ARGS`] arguments that are finite numbers, booleans or nil.
/// Whether the address means anything is up to [`handle_message`], which refuses unknown ones.
pub fn sanitize(m: &rosc::OscMessage) -> Result<(), &'static str> {
    use rosc::OscType as T;
    let a = &m.addr;
    if !a.starts_with('/') || a.len() < 2 || a.len() > MAX_ADDRESS {
        return Err("bad address length or start");
    }
    if !a.bytes().all(|b| b.is_ascii_graphic() && !b"#*,?[]{}\\".contains(&b)) {
        return Err("address has characters that aren't allowed");
    }
    if a.contains("//") || a.ends_with('/') {
        return Err("address has an empty part");
    }
    if m.args.len() > MAX_ARGS {
        return Err("too many arguments");
    }
    for arg in &m.args {
        match arg {
            T::Int(_) | T::Long(_) | T::Bool(_) | T::Nil => {}
            T::Float(f) if f.is_finite() => {}
            T::Double(d) if d.is_finite() => {}
            T::Float(_) | T::Double(_) => return Err("argument is not a finite number"),
            _ => return Err("only numbers and booleans are taken as arguments"),
        }
    }
    Ok(())
}

/// The first argument as a value (numbers and booleans; NaN and infinities are dropped).
fn value(m: &rosc::OscMessage) -> Option<f32> {
    use rosc::OscType as T;
    let v = match m.args.first()? {
        T::Float(f) => Some(*f),
        T::Double(d) => Some(*d as f32),
        T::Int(i) => Some(*i as f32),
        T::Long(l) => Some(*l as f32),
        T::Bool(b) => Some(*b as u8 as f32),
        _ => None,
    };
    v.filter(|v| v.is_finite())
}

/// Apply one OSC message: learn mode first, then the fixed addresses, then the built-in
/// lighting addresses and the surface mappings.
pub fn handle_message(show: &SharedShow, hub: &crate::surface::SurfaceHub, m: &rosc::OscMessage) -> Result<(), String> {
    use crate::surface::{SurfaceInput, osc_builtin};
    let input = SurfaceInput::Osc(m.addr.clone());
    if hub.learning() {
        hub.input(show, input, value(m), None);
        return Ok(());
    }
    match handle(show, &m.addr) {
        Err(e) if e == UNKNOWN => {
            if hub.input(show, input, value(m), osc_builtin(&m.addr)) {
                Ok(())
            } else {
                Err(e)
            }
        }
        r => r,
    }
}

const UNKNOWN: &str = "unknown address";

/// Apply one OSC address to the show.
pub fn handle(show: &SharedShow, addr: &str) -> Result<(), String> {
    use crate::control::{Command, CueAction};
    let mut guard = show.lock();
    let parts: Vec<&str> = addr.trim_matches('/').split('/').collect();
    // Numbers are looked up in a map built once per edit: messages can come 500 times a second.
    let numbers = guard.lookups();
    let find = |number: &str| numbers.numbers.get(number).copied().ok_or_else(|| format!("no cue numbered {number}"));
    let cmd = match parts.as_slice() {
        ["go"] => Command::Go,
        ["stop"] => Command::Stop,
        ["panic"] => Command::Panic,
        ["pause"] => Command::PauseAll,
        ["resume"] => Command::ResumeAll,
        ["playhead", "next"] => Command::PlayheadNext,
        ["playhead", "previous"] => Command::PlayheadPrevious,
        ["playhead", n] => Command::SetPlayhead(Some(find(n)?)),
        ["cue", n, action] => {
            let action = match *action {
                "start" | "go" => CueAction::Start,
                "stop" => CueAction::Stop,
                "pause" => CueAction::Pause,
                "resume" => CueAction::Resume,
                "devamp" => CueAction::Devamp,
                other => return Err(format!("unknown cue action {other}")),
            };
            Command::Cue(find(n)?, action)
        }
        _ => return Err(UNKNOWN.into()),
    };
    crate::control::apply(&mut guard, cmd, Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only plain addresses with a few numeric arguments get through.
    #[test]
    fn messages_are_sanitized() {
        use rosc::OscType as T;
        let msg = |addr: &str, args: Vec<T>| rosc::OscMessage { addr: addr.into(), args };
        for ok in [msg("/go", vec![]), msg("/cue/12.5/start", vec![]), msg("/light/executor/3/level", vec![T::Float(0.5)])] {
            assert_eq!(sanitize(&ok), Ok(()), "{}", ok.addr);
        }
        for bad in [
            msg("go", vec![]),
            msg("/", vec![]),
            msg("/cue/*/start", vec![]),
            msg("/cue/{1,2}/start", vec![]),
            msg("/go\n/stop", vec![]),
            msg("/go now", vec![]),
            msg("/cue//start", vec![]),
            msg(&format!("/{}", "a".repeat(MAX_ADDRESS)), vec![]),
            msg("/light/grandmaster", vec![T::Float(f32::NAN)]),
            msg("/light/grandmaster", vec![T::String("1".into())]),
            msg("/light/grandmaster", vec![T::Blob(vec![0; 100])]),
            msg("/go", vec![T::Int(1); MAX_ARGS + 1]),
        ] {
            assert!(sanitize(&bad).is_err(), "{:?}", bad.addr);
        }
    }

    /// Built-in lighting addresses work without mappings; unknown ones are refused.
    #[test]
    fn lighting_addresses() {
        let show: SharedShow = Arc::new(parking_lot::Mutex::new(crate::show::Show::new(crate::media::Prober::new())));
        let hub = crate::surface::SurfaceHub::new();
        let msg = |addr: &str, args| rosc::OscMessage { addr: addr.into(), args };
        handle_message(&show, &hub, &msg("/light/grandmaster", vec![rosc::OscType::Float(0.25)])).unwrap();
        assert_eq!(show.lock().media.light.masters().grand_master, 0.25);
        handle_message(&show, &hub, &msg("/light/blackout", vec![])).unwrap();
        assert!(show.lock().media.light.masters().blackout);
        assert!(handle_message(&show, &hub, &msg("/nothing/here", vec![])).is_err());
    }

    /// A flooding sender uses up only its own budget: another sender still gets through.
    #[test]
    fn each_sender_has_its_own_budget() {
        let mut rates = Rates::default();
        let (flood, surface) = ("10.0.0.9".parse().unwrap(), "10.0.0.2".parse().unwrap());
        let taken = (0..MAX_RATE * 3).filter(|_| rates.take(flood).is_ok()).count();
        assert_eq!(taken, MAX_RATE as usize);
        assert!(rates.take(surface).is_ok());
        // Forged senders past MAX_SOURCES share one budget, and all together stop at the total.
        let mut ok = 0;
        for i in 0..10_000u32 {
            let ip = std::net::IpAddr::from(std::net::Ipv4Addr::from(0x0b00_0000 + i));
            ok += rates.take(ip).is_ok() as u32;
        }
        assert!(ok <= MAX_TOTAL_RATE, "{ok}");
        assert!(rates.sources.len() <= MAX_SOURCES);
    }

    /// Turning OSC off must really close the port: it can be bound again right away (re-enabling
    /// the listener, or another program taking the port).
    #[test]
    fn dropping_the_server_frees_the_port() {
        let show: SharedShow = Arc::new(parking_lot::Mutex::new(crate::show::Show::new(crate::media::Prober::new())));
        let port = UdpSocket::bind(("0.0.0.0", 0)).unwrap().local_addr().unwrap().port();
        let server = OscServer::start(port, show.clone(), || {}).unwrap();
        assert!(UdpSocket::bind(("0.0.0.0", port)).is_err(), "the listener holds the port");
        drop(server);
        UdpSocket::bind(("0.0.0.0", port)).expect("port still taken after the listener was dropped");
        drop(OscServer::start(port, show, || {}).expect("re-enabling on the same port"));
    }
}
