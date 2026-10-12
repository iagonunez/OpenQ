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

/// How long the listener blocks on the socket between checks of its stop flag (bounds how long
/// dropping an [`OscServer`] waits).
const POLL: Duration = Duration::from_millis(50);

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
        let sock = UdpSocket::bind(("0.0.0.0", port))?;
        sock.set_read_timeout(Some(POLL))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new().name("openq-osc".into()).spawn(move || {
            let mut buf = [0u8; rosc::decoder::MTU];
            while !stop2.load(Ordering::Acquire) {
                let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                match rosc::decoder::decode_udp(&buf[..n]) {
                    Ok((_, packet)) => {
                        let mut msgs = Vec::new();
                        flatten(packet, &mut msgs);
                        for m in msgs {
                            // A failure handling one message must not end the listener.
                            let res = crate::recovery::catch(|| handle(&show, &m.addr).map_err(anyhow::Error::msg))
                                .map_err(|e| format!("{e:#}"));
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
        Ok(Self { port, stop, thread: Some(thread) })
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

#[cfg(test)]
mod tests {
    use super::*;

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
