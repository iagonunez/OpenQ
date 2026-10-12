//! Timecode in and out: Timecode cues that send LTC (through the mixer) or MTC (to a MIDI port),
//! and the readers that follow incoming LTC or MTC for cue lists.
//!
//! Every MIDI connection and audio input lives on its own thread, which owns it until told to
//! stop, so none of them has to be `Send`.

use super::audio::{AudioOutput, MixParams};
use super::input::{InputStream, open_input};
use crate::model::{CueId, MIN_DB, TimecodeCue, TimecodeOutput, TimecodeRate, TimecodeSource, TimecodeSync};
use crate::timecode::{LtcDecoder, LtcEncoder, MtcDecoder, Timecode, mtc_full_frame, mtc_quarter_frame};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// MIDI output ports by name.
pub fn list_midi_outputs() -> Vec<String> {
    let Ok(m) = midir::MidiOutput::new("OpenQ") else { return Vec::new() };
    m.ports().iter().filter_map(|p| m.port_name(p).ok()).collect()
}

/// MIDI input ports by name.
pub fn list_midi_inputs() -> Vec<String> {
    let Ok(m) = midir::MidiInput::new("OpenQ") else { return Vec::new() };
    m.ports().iter().filter_map(|p| m.port_name(p).ok()).collect()
}

/// A running Timecode cue.
pub struct Generator {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    /// Frame count of the label being sent.
    frame: Arc<AtomicU64>,
    rate: TimecodeRate,
    /// Frame count it started from.
    start: u64,
    /// Sends LTC through the mixer (else MTC).
    ltc: bool,
}

impl Drop for Generator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Generator {
    /// Start sending timecode for cue `id`. LTC needs the mixer.
    pub fn start(id: CueId, t: &TimecodeCue, audio: Option<&mut AudioOutput>) -> Result<Self, String> {
        let g = Self {
            stop: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(false)),
            frame: Arc::new(AtomicU64::new(Timecode::from_secs(t.start, t.rate).frame_count(t.rate))),
            rate: t.rate,
            start: Timecode::from_secs(t.start, t.rate).frame_count(t.rate),
            ltc: t.output == TimecodeOutput::Ltc,
        };
        match t.output {
            TimecodeOutput::Ltc => g.start_ltc(id, t, audio.ok_or("LTC needs an audio output")?),
            TimecodeOutput::Mtc => g.start_mtc(t)?,
        }
        Ok(g)
    }

    fn start_ltc(&self, id: CueId, t: &TimecodeCue, a: &mut AudioOutput) {
        // One source channel, sent only to the chosen output.
        let out = a.channels.min(crate::model::MAX_OUT_CHANNELS);
        let ch = (t.ltc_channel as usize).min(out.saturating_sub(1));
        let row = (0..out).map(|o| if o == ch { 0.0 } else { MIN_DB }).collect();
        let mix = MixParams { volume_db: t.volume_db, routing: Some(vec![row]), ..Default::default() };
        let (mut tx, shared) = a.add_external(id, &mix, 1, "LTC", 0.2);
        let mut enc = LtcEncoder::new(t.start, t.rate, a.rate, 1.0);
        let total = (t.duration > 0.0).then_some((t.duration * a.rate as f64) as u64);
        let (stop, done, frame) = (self.stop.clone(), self.done.clone(), self.frame.clone());
        let rate = t.rate;
        std::thread::Builder::new()
            .name("openq-ltc-out".into())
            .spawn(move || {
                let mut sent = 0u64;
                let mut buf = vec![0.0f32; 512];
                loop {
                    if stop.load(Ordering::Acquire) || shared.finished.load(Ordering::Acquire) {
                        break;
                    }
                    if total.is_some_and(|n| sent >= n) {
                        shared.eof.store(true, Ordering::Release);
                        break;
                    }
                    if tx.slots() >= buf.len() {
                        let n = total.map_or(buf.len(), |t| buf.len().min((t - sent) as usize));
                        enc.fill(&mut buf[..n]);
                        if let Ok(chunk) = tx.write_chunk_uninit(n) {
                            chunk.fill_from_iter(buf[..n].iter().copied());
                        }
                        sent += n as u64;
                        frame.store(enc.current().frame_count(rate), Ordering::Relaxed);
                    } else {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                // Wait for the mixer to play out what is buffered.
                while !stop.load(Ordering::Acquire) && !shared.finished.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                done.store(true, Ordering::Release);
            })
            .expect("spawn LTC thread");
    }

    fn start_mtc(&self, t: &TimecodeCue) -> Result<(), String> {
        let out = midir::MidiOutput::new("OpenQ").map_err(|e| e.to_string())?;
        let ports = out.ports();
        let port = match &t.mtc_port {
            Some(name) => ports.iter().find(|p| out.port_name(p).ok().as_deref() == Some(name.as_str())),
            None => ports.first(),
        }
        .ok_or_else(|| format!("MIDI output not found: {}", t.mtc_port.as_deref().unwrap_or("(none)")))?
        .clone();
        let mut conn = out.connect(&port, "openq-mtc").map_err(|e| e.to_string())?;
        let (stop, paused, done, frame) = (self.stop.clone(), self.paused.clone(), self.done.clone(), self.frame.clone());
        let (rate, duration) = (t.rate, t.duration);
        let start = frame.load(Ordering::Relaxed);
        std::thread::Builder::new()
            .name("openq-mtc-out".into())
            .spawn(move || {
                let quarter = Duration::from_secs_f64(1.0 / (4.0 * rate.fps()));
                let _ = conn.send(&mtc_full_frame(Timecode::from_frame_count(start, rate), rate));
                let mut q = 0u64;
                let mut t0 = Instant::now();
                let mut paused_at: Option<Instant> = None;
                while !stop.load(Ordering::Acquire) {
                    if paused.load(Ordering::Acquire) {
                        paused_at.get_or_insert_with(Instant::now);
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    if let Some(p) = paused_at.take() {
                        // Hold the time while paused, and relocate the receiver on resume.
                        t0 += p.elapsed();
                        let _ = conn.send(&mtc_full_frame(Timecode::from_frame_count(start + 2 * (q / 8), rate), rate));
                    }
                    let due = t0 + quarter * q as u32;
                    let now = Instant::now();
                    if now < due {
                        std::thread::sleep((due - now).min(Duration::from_millis(2)));
                        continue;
                    }
                    if duration > 0.0 && (now - t0).as_secs_f64() >= duration {
                        break;
                    }
                    // A group of 8 quarter frames spans 2 frames and describes the first of them.
                    let f = start + 2 * (q / 8);
                    let _ = conn.send(&mtc_quarter_frame(Timecode::from_frame_count(f, rate), rate, (q % 8) as u8));
                    frame.store(start + q / 4, Ordering::Relaxed);
                    q += 1;
                }
                done.store(true, Ordering::Release);
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// True once it has stopped sending.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Hold (MTC; LTC holds through its mixer voice).
    pub fn set_paused(&self, on: bool) {
        self.paused.store(on, Ordering::Release);
    }

    /// The label being sent. For LTC, `played` is the seconds the mixer has played of it: the
    /// encoder runs ahead of playback by the voice's buffer.
    pub fn current(&self, played: Option<f64>) -> Timecode {
        let frame = match played {
            Some(p) if self.ltc => self.start + (p * self.rate.fps()) as u64,
            _ => self.frame.load(Ordering::Relaxed),
        };
        Timecode::from_frame_count(frame, self.rate)
    }

    /// Its frame rate.
    pub fn rate(&self) -> TimecodeRate {
        self.rate
    }
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
        let mut r = Self { latest: latest.clone(), stop: stop.clone(), _input: None, error: None };
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
                            .map_err(|e| e.to_string())
                        })();
                        match res {
                            Ok(conn) => {
                                let _ = tx.send(Ok(()));
                                while !stop.load(Ordering::Acquire) {
                                    std::thread::park_timeout(Duration::from_millis(50));
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
                // Ends when the follower is dropped.
                while let Some(w) = w.upgrade() {
                    let want = w.lock().clone();
                    drop(w);
                    r.lock().retain(|(s, _)| want.contains(s));
                    let missing: Vec<TimecodeSync> =
                        want.into_iter().filter(|s| !r.lock().iter().any(|(k, _)| k == s)).collect();
                    for s in missing {
                        let reader = Arc::new(Reader::open(&s));
                        if let Some(e) = &reader.error {
                            log::warn!("{e}");
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
