//! MIDI cue messages, sent off the engine thread.
//!
//! Opening a MIDI port can take a while (and a USB interface may have gone away), so the engine
//! thread only queues the message. A [`MidiOut`] worker keeps one connection per output port, opens
//! ports as messages for them arrive, and drops a connection that fails so the next message
//! reopens it. Errors go to the engine log through [`MidiOut::drain_errors`].

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

/// Messages queued for the worker at most.
const QUEUE: usize = 1024;
/// Errors kept until drained at most.
const ERRORS: usize = 64;
/// A port that failed to open is tried again after this long (messages meanwhile fail at once).
const RETRY: Duration = Duration::from_secs(2);

enum Msg {
    Send(Option<String>, Vec<u8>),
    Quit,
}

/// Sends MIDI cue messages from a worker thread (see the module docs).
pub struct MidiOut {
    tx: SyncSender<Msg>,
    errors: Arc<Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MidiOut {
    /// Start the worker.
    pub fn new() -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE);
        let errors = Arc::new(Mutex::new(Vec::new()));
        let e = errors.clone();
        let thread = std::thread::Builder::new().name("opnq-midi-out".into()).spawn(move || run(rx, e))?;
        Ok(Self { tx, errors, thread: Some(thread) })
    }

    /// Queue `bytes` for output `port` (None = the first output).
    pub fn send(&self, port: Option<&str>, bytes: Vec<u8>) -> Result<(), String> {
        match self.tx.try_send(Msg::Send(port.map(str::to_string), bytes)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("too many MIDI messages waiting".into()),
            Err(TrySendError::Disconnected(_)) => Err("MIDI output stopped".into()),
        }
    }

    /// Errors since the last call.
    pub fn drain_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.errors.lock())
    }
}

impl Drop for MidiOut {
    fn drop(&mut self) {
        // The worker ends once it reaches this (or the channel closes); never wait for it here:
        // it may be opening a slow port.
        let _ = self.tx.try_send(Msg::Quit);
        self.thread.take();
    }
}

/// Open output `port` (None = the first one): its connection and its name.
pub fn connect(port: Option<&str>, client: &str) -> Result<(midir::MidiOutputConnection, String), String> {
    let m = midir::MidiOutput::new("OpnQ").map_err(|e| e.to_string())?;
    let ports = m.ports();
    let p = match port {
        Some(name) => ports.iter().find(|p| m.port_name(p).ok().as_deref() == Some(name)).ok_or_else(|| format!("MIDI output not found: {name}"))?,
        None => ports.first().ok_or("no MIDI output on this computer")?,
    }
    .clone();
    let name = m.port_name(&p).unwrap_or_default();
    let c = m.connect(&p, client).map_err(|e| format!("{name}: {e}"))?;
    Ok((c, name))
}

fn run(rx: Receiver<Msg>, errors: Arc<Mutex<Vec<String>>>) {
    let mut conns: HashMap<Option<String>, midir::MidiOutputConnection> = HashMap::new();
    let mut failed: HashMap<Option<String>, (Instant, String)> = HashMap::new();
    let report = |e: String| {
        let mut errs = errors.lock();
        if errs.len() < ERRORS {
            errs.push(e);
        }
    };
    while let Ok(Msg::Send(port, bytes)) = rx.recv() {
        let r = crate::recovery::catch(|| {
            if !conns.contains_key(&port) {
                if let Some((t, e)) = failed.get(&port)
                    && t.elapsed() < RETRY
                {
                    return Err(anyhow::anyhow!("{e}"));
                }
                match connect(port.as_deref(), "opnq-midi-cue") {
                    Ok((c, _)) => {
                        failed.remove(&port);
                        conns.insert(port.clone(), c);
                    }
                    Err(e) => {
                        failed.insert(port.clone(), (Instant::now(), e.clone()));
                        return Err(anyhow::anyhow!("{e}"));
                    }
                }
            }
            let c = conns.get_mut(&port).expect("connected");
            if let Err(e) = c.send(&bytes) {
                // The port went away: reopen on the next message.
                conns.remove(&port);
                return Err(anyhow::anyhow!("MIDI send: {e}"));
            }
            Ok(())
        });
        if let Err(e) = r {
            report(format!("MIDI cue: {e:#}"));
        }
    }
}
