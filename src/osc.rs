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

use crate::show::SharedShow;
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A running OSC listener; stops when dropped.
pub struct OscServer {
    /// UDP port being listened on.
    pub port: u16,
    stop: Arc<AtomicBool>,
}

impl Drop for OscServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl OscServer {
    /// Listen on `port` on all interfaces; `repaint` is called after each handled message.
    pub fn start(port: u16, show: SharedShow, repaint: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let sock = UdpSocket::bind(("0.0.0.0", port))?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::Builder::new().name("openq-osc".into()).spawn(move || {
            let mut buf = [0u8; rosc::decoder::MTU];
            while !stop2.load(Ordering::Acquire) {
                let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                match rosc::decoder::decode_udp(&buf[..n]) {
                    Ok((_, packet)) => {
                        let mut msgs = Vec::new();
                        flatten(packet, &mut msgs);
                        for m in msgs {
                            let res = handle(&show, &m.addr);
                            match res {
                                Ok(()) => log::info!("OSC {} from {from}", m.addr),
                                Err(e) => log::warn!("OSC {} from {from}: {e}", m.addr),
                            }
                        }
                        repaint();
                    }
                    Err(e) => log::warn!("bad OSC packet from {from}: {e:?}"),
                }
            }
        })?;
        log::info!("OSC listening on UDP {port}");
        Ok(Self { port, stop })
    }
}

fn flatten(p: rosc::OscPacket, out: &mut Vec<rosc::OscMessage>) {
    match p {
        rosc::OscPacket::Message(m) => out.push(m),
        rosc::OscPacket::Bundle(b) => b.content.into_iter().for_each(|p| flatten(p, out)),
    }
}

/// Apply one OSC address to the show.
pub fn handle(show: &SharedShow, addr: &str) -> Result<(), String> {
    use crate::control::{Command, CueAction};
    let mut guard = show.lock();
    let parts: Vec<&str> = addr.trim_matches('/').split('/').collect();
    let find = |number: &str| {
        guard
            .ws
            .cue_list
            .iter_all()
            .iter()
            .find(|c| c.number == number)
            .map(|c| c.id)
            .ok_or_else(|| format!("no cue numbered {number}"))
    };
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
        _ => return Err("unknown address".into()),
    };
    crate::control::apply(&mut guard, cmd, Instant::now())
}
