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
    pub port: u16,
    stop: Arc<AtomicBool>,
}

impl Drop for OscServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl OscServer {
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
    use crate::show::Show;
    let mut guard = show.lock();
    let now = Instant::now();
    let parts: Vec<&str> = addr.trim_matches('/').split('/').collect();
    let find = |s: &Show, number: &str| {
        s.ws.cue_list
            .iter_all()
            .iter()
            .find(|c| c.number == number)
            .map(|c| c.id)
            .ok_or_else(|| format!("no cue numbered {number}"))
    };
    let target = match parts.as_slice() {
        ["playhead", n] if *n != "next" && *n != "previous" => Some(find(&guard, n)?),
        ["cue", n, _] => Some(find(&guard, n)?),
        _ => None,
    };
    let Show { ws, engine, media, .. } = &mut *guard;
    match parts.as_slice() {
        ["go"] => engine.go(ws, media, now),
        ["stop"] => engine.stop_all(media, now, ws.settings.panic_fade),
        ["panic"] => engine.stop_all(media, now, 0.0),
        ["pause"] => engine.pause_all(media, now),
        ["resume"] => engine.resume_all(media, now),
        ["playhead", dir @ ("next" | "previous")] => {
            let rows = ws.cue_list.visible_rows(&|_| false);
            let i = engine.playhead.and_then(|p| rows.iter().position(|r| r.1 == p));
            let next = match (*dir, i) {
                ("next", Some(i)) => rows.get(i + 1),
                ("previous", Some(i)) if i > 0 => rows.get(i - 1),
                (_, Some(_)) => None,
                _ => rows.first(),
            };
            if let Some(r) = next {
                engine.set_playhead(Some(r.1));
            }
        }
        ["playhead", _] => engine.set_playhead(target),
        ["cue", _, action] => {
            let id = target.expect("resolved above");
            match *action {
                "start" | "go" => engine.start_cue(ws, media, id, now),
                "stop" => engine.stop_cue(ws, media, id),
                "pause" => engine.pause_cue(ws, media, id, now),
                "resume" => engine.resume_cue(ws, media, id, now),
                "devamp" => {
                    use crate::engine::Backend;
                    if !media.devamp(id) {
                        return Err("cue is not playing".into());
                    }
                }
                other => return Err(format!("unknown cue action {other}")),
            }
        }
        _ => return Err("unknown address".into()),
    }
    Ok(())
}
