//! Timecode input: each cue list can follow incoming LTC (on an audio input) or
//! MTC (on a MIDI input), and cues fire at their timecode trigger as it runs past.
//!
//! Every MIDI connection and audio input lives on its own thread, which owns it until told to
//! stop, so none of them has to be `Send`.

use super::input::{InputStream, open_input};
use crate::model::{TimecodeRate, TimecodeSource, TimecodeSync};
use crate::timecode::{LtcDecoder, MtcDecoder};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// MIDI input ports by name.
pub fn list_midi_inputs() -> Vec<String> {
    let Ok(m) = midir::MidiInput::new("OpenQ") else { return Vec::new() };
    m.ports().iter().filter_map(|p| m.port_name(p).ok()).collect()
}

/// The last label a reader decoded.
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    /// Label-seconds.
    pub secs: f64,
    /// Rate reported by the signal (MTC) or configured (LTC).
    pub rate: TimecodeRate,
    /// When it arrived.
    pub at: Instant,
}

/// Follows LTC on an audio input or MTC on a MIDI input.
pub struct Reader {
    latest: Arc<Mutex<Option<Reading>>>,
    stop: Arc<AtomicBool>,
    _input: Option<InputStream>,
    /// Why it couldn't open.
    pub error: Option<String>,
    opened: Instant,
    /// The MIDI port went away (MTC); the input is reopened when it's back.
    midi_lost: Arc<AtomicBool>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// How long without a new frame before timecode counts as stopped.
const DROPOUT: Duration = Duration::from_millis(250);

impl Reader {
    /// Open the input `sync` describes (its source must not be Off).
    pub fn open(sync: &TimecodeSync) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let mut r = Self { latest: latest.clone(), stop: stop.clone(), _input: None, error: None, opened: Instant::now(), midi_lost: Arc::default() };
        let rate = sync.rate;
        match sync.source {
            TimecodeSource::Off => {}
            TimecodeSource::Ltc => {
                let ch = sync.ltc_channel as usize;
                let mut dec: Option<LtcDecoder> = None;
                let sr = Arc::new(std::sync::atomic::AtomicU32::new(0));
                let sr2 = sr.clone();
                let mut mono = Vec::new();
                match open_input(sync.ltc_device.as_deref(), move |data, in_ch| {
                    let rate_in = sr2.load(Ordering::Acquire);
                    if rate_in == 0 {
                        return;
                    }
                    let d = dec.get_or_insert_with(|| LtcDecoder::new(rate_in));
                    mono.clear();
                    mono.extend(data.chunks_exact(in_ch.max(1)).map(|f| f.get(ch).copied().unwrap_or(0.0)));
                    let frames = mono.len();
                    let now = Instant::now();
                    if let Some((tc, _, i)) = d.push(&mono).pop() {
                        // The frame's label applies from the end of its sync word, `frames - i` samples ago;
                        // it was sent during the previous frame period, hence one frame on.
                        let ago = Duration::from_secs_f64((frames - i) as f64 / rate_in as f64);
                        *latest.lock() = Some(Reading { secs: tc.next(rate).secs(rate), rate, at: now - ago });
                    }
                }) {
                    Ok(s) => {
                        sr.store(s.rate, Ordering::Release);
                        r._input = Some(s);
                    }
                    Err(e) => r.error = Some(format!("LTC input: {e:#}")),
                }
            }
            TimecodeSource::Mtc => {
                let port_name = sync.mtc_port.clone();
                let lost = r.midi_lost.clone();
                let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
                std::thread::Builder::new()
                    .name("openq-mtc-in".into())
                    .spawn(move || {
                        let res = (|| {
                            let mut m = midir::MidiInput::new("OpenQ").map_err(|e| e.to_string())?;
                            m.ignore(midir::Ignore::None);
                            let ports = m.ports();
                            let port = match &port_name {
                                Some(n) => ports.iter().find(|p| m.port_name(p).ok().as_deref() == Some(n.as_str())),
                                None => ports.first(),
                            }
                            .ok_or_else(|| format!("MIDI input not found: {}", port_name.as_deref().unwrap_or("(none)")))?
                            .clone();
                            let connected = m.port_name(&port).unwrap_or_default();
                            let mut dec = MtcDecoder::default();
                            m.connect(
                                &port,
                                "openq-mtc-in",
                                move |_, msg, _| {
                                    if let Some((tc, rate)) = dec.push(msg) {
                                        *latest.lock() = Some(Reading { secs: tc.secs(rate), rate, at: Instant::now() });
                                    }
                                },
                                (),
                            )
                            .map(|c| (c, connected))
                            .map_err(|e| e.to_string())
                        })();
                        match res {
                            Ok((conn, connected)) => {
                                let _ = tx.send(Ok(()));
                                // midir doesn't report an unplugged port: look for it now and then.
                                let mut checked = Instant::now();
                                while !stop.load(Ordering::Acquire) {
                                    std::thread::park_timeout(Duration::from_millis(50));
                                    if checked.elapsed() >= super::AUDIO_RETRY {
                                        checked = Instant::now();
                                        if !list_midi_inputs().contains(&connected) {
                                            log::warn!("MTC input lost ({connected}); reconnecting…");
                                            lost.store(true, Ordering::Release);
                                            break;
                                        }
                                    }
                                }
                                drop(conn);
                            }
                            Err(e) => {
                                let _ = tx.send(Err(e));
                            }
                        }
                    })
                    .ok();
                if let Ok(Err(e)) = rx.recv_timeout(Duration::from_secs(2)) {
                    r.error = Some(format!("MTC input: {e}"));
                }
            }
        }
        r
    }

    /// It failed to open, or its audio device went away: try a fresh one (after
    /// [`super::AUDIO_RETRY`], so a missing device isn't hammered).
    fn needs_reopen(&self) -> bool {
        let broken = self.error.is_some()
            || self._input.as_ref().is_some_and(|i| i.is_lost())
            || self.midi_lost.load(Ordering::Acquire);
        broken && self.opened.elapsed() >= super::AUDIO_RETRY
    }

    /// Where the timecode is now (label-seconds, running on from the last frame), or None if
    /// it has stopped.
    pub fn position(&self, now: Instant) -> Option<Reading> {
        let r = (*self.latest.lock())?;
        let age = now.saturating_duration_since(r.at);
        (age < DROPOUT).then_some(Reading { secs: r.secs + age.as_secs_f64(), ..r })
    }
}

/// Open readers, each with the settings it was opened for.
type Readers = Arc<Mutex<Vec<(TimecodeSync, Arc<Reader>)>>>;

/// Keeps one [`Reader`] open for each distinct timecode input the cue lists use. Opening an input
/// can take a moment, so a background thread does it; the engine only asks for positions.
pub struct Follower {
    wanted: Arc<Mutex<Vec<TimecodeSync>>>,
    readers: Readers,
}

impl Default for Follower {
    fn default() -> Self {
        Self::new()
    }
}

impl Follower {
    /// Start the background thread.
    pub fn new() -> Self {
        let wanted: Arc<Mutex<Vec<TimecodeSync>>> = Arc::default();
        let readers: Readers = Arc::default();
        let (w, r) = (Arc::downgrade(&wanted), readers.clone());
        std::thread::Builder::new()
            .name("openq-tc-follow".into())
            .spawn(move || {
                // Errors already logged, so retries don't repeat them.
                let mut logged: Vec<(TimecodeSync, String)> = Vec::new();
                // Ends when the follower is dropped.
                while let Some(w) = w.upgrade() {
                    let want = w.lock().clone();
                    drop(w);
                    // Inputs that failed or were unplugged are reopened (the device may be back).
                    r.lock().retain(|(s, reader)| want.contains(s) && !reader.needs_reopen());
                    logged.retain(|(s, _)| want.contains(s));
                    let missing: Vec<TimecodeSync> =
                        want.into_iter().filter(|s| !r.lock().iter().any(|(k, _)| k == s)).collect();
                    for s in missing {
                        let reader = Arc::new(Reader::open(&s));
                        match &reader.error {
                            Some(e) if !logged.iter().any(|(k, old)| *k == s && old == e) => {
                                log::warn!("{e}");
                                logged.retain(|(k, _)| *k != s);
                                logged.push((s.clone(), e.clone()));
                            }
                            Some(_) => {}
                            None => logged.retain(|(k, _)| *k != s),
                        }
                        r.lock().push((s, reader));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
            .expect("spawn timecode thread");
        Self { wanted, readers }
    }

    /// The inputs to keep open.
    pub fn set_wanted(&self, syncs: Vec<TimecodeSync>) {
        let mut w = self.wanted.lock();
        if *w != syncs {
            *w = syncs;
        }
    }

    fn reader(&self, sync: &TimecodeSync) -> Option<Arc<Reader>> {
        self.readers.lock().iter().find(|(k, _)| k == sync).map(|(_, r)| r.clone())
    }

    /// Current position of the input `sync` describes (None: stopped, or not open yet).
    pub fn position(&self, sync: &TimecodeSync, now: Instant) -> Option<Reading> {
        self.reader(sync)?.position(now)
    }

    /// Why the input `sync` describes couldn't open.
    pub fn error(&self, sync: &TimecodeSync) -> Option<String> {
        self.reader(sync)?.error.clone()
    }
}
