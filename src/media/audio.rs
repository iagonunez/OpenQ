//! Real-time audio output: a lock-free mixer running in the cpal callback.
//!
//! Each playing audio cue is a [`Voice`] fed by its own decoder thread through a ring buffer
//! (already resampled and channel-mapped to the device format). The control side talks to the
//! callback only through a wait-free command queue, and removed voices are shipped back for
//! deallocation so the audio thread never frees memory.

use super::decoder::{self, DecodeParams};
use crate::model::{AudioCue, CueId, FadeCurve, db_to_gain};
use anyhow::{Context, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, OutputCallbackInfo, SampleFormat, SizedSample, StreamConfig};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

const MAX_METER_CHANNELS: usize = 8;
/// Fade applied on stop to avoid clicks.
const DECLICK_SECS: f64 = 0.008;
/// Seconds of decoded audio buffered per voice.
const VOICE_BUFFER_SECS: f64 = 2.0;

/// Maps between wall-clock instants and the mixer's frame counter. Re-anchored every callback
/// so drift between the audio and system clocks never accumulates.
pub struct Clock {
    seq: AtomicU64,
    frame: AtomicU64,
    nanos: AtomicU64,
    origin: Instant,
    rate: u32,
}

impl Clock {
    fn new(rate: u32) -> Self {
        Self {
            seq: AtomicU64::new(0),
            frame: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
            origin: Instant::now(),
            rate,
        }
    }

    fn store(&self, frame: u64, now: Instant) {
        // Seqlock writer (single writer: the audio callback).
        self.seq.fetch_add(1, Ordering::AcqRel);
        self.frame.store(frame, Ordering::Relaxed);
        self.nanos.store((now - self.origin).as_nanos() as u64, Ordering::Relaxed);
        self.seq.fetch_add(1, Ordering::Release);
    }

    fn anchor(&self) -> (u64, Instant) {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let f = self.frame.load(Ordering::Relaxed);
            let n = self.nanos.load(Ordering::Relaxed);
            std::sync::atomic::fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return (f, self.origin + std::time::Duration::from_nanos(n));
            }
        }
    }

    /// Mixer frame corresponding to instant `t`.
    pub fn frame_at(&self, t: Instant) -> u64 {
        let (f, at) = self.anchor();
        let rate = self.rate as f64;
        if t >= at {
            f + ((t - at).as_secs_f64() * rate).round() as u64
        } else {
            f.saturating_sub(((at - t).as_secs_f64() * rate).round() as u64)
        }
    }
}

/// State shared between a voice, its decoder thread and the control side.
#[derive(Default)]
pub struct VoiceShared {
    pub frames_played: AtomicU64,
    /// Voice is gone from the mixer (ended, stopped or errored).
    pub finished: AtomicBool,
    /// Decoder produced its last sample.
    pub eof: AtomicBool,
    /// Ask the decoder thread to quit.
    pub stop: AtomicBool,
    /// Decoder filled the buffer once (or hit EOF): safe to start without underrun.
    pub ready: AtomicBool,
    pub error: parking_lot::Mutex<Option<String>>,
}

#[derive(Clone, Copy)]
enum VoiceState {
    /// Preloaded, waiting for a Start command.
    Idle,
    /// Starts when the mixer reaches this frame.
    Waiting(u64),
    Playing,
    Paused,
}

struct Ramp {
    from: f32,
    to: f32,
    start: u64,
    total: u64,
    pos: u64,
    curve: FadeCurve,
    started: bool,
}

pub struct Voice {
    id: u64,
    rx: rtrb::Consumer<f32>,
    shared: Arc<VoiceShared>,
    state: VoiceState,
    gain: f32,
    pan: [f32; 2],
    ramp: Option<Ramp>,
    stop_after_ramp: bool,
    done: bool,
}

enum Cmd {
    Add(Box<Voice>),
    Start { id: u64, at: u64 },
    Pause(u64),
    Resume { id: u64, at: u64 },
    Stop(u64),
    Ramp { id: u64, to: f32, frames: u64, curve: FadeCurve, at: u64, stop_after: bool },
    Master(f32),
}

fn pan_gains(pan: f32) -> [f32; 2] {
    // Constant power, normalised so centre is unity on both sides.
    let theta = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    [theta.cos() * std::f32::consts::SQRT_2, theta.sin() * std::f32::consts::SQRT_2]
}

struct Mixer {
    cmds: rtrb::Consumer<Cmd>,
    garbage: rtrb::Producer<Box<Voice>>,
    // Boxed so voices move in/out of the RT thread via the queues without copying or freeing.
    #[allow(clippy::vec_box)]
    voices: Vec<Box<Voice>>,
    channels: usize,
    frame: u64,
    master: f32,
    clock: Arc<Clock>,
    meters: Arc<[AtomicU32; MAX_METER_CHANNELS]>,
    scratch: Vec<f32>,
}

impl Mixer {
    fn handle_commands(&mut self) {
        while let Ok(cmd) = self.cmds.pop() {
            match cmd {
                Cmd::Add(v) => self.voices.push(v),
                Cmd::Master(g) => self.master = g,
                Cmd::Start { id, at } => {
                    if let Some(v) = self.voice(id) {
                        v.state = VoiceState::Waiting(at);
                    }
                }
                Cmd::Pause(id) => {
                    if let Some(v) = self.voice(id) {
                        v.state = VoiceState::Paused;
                    }
                }
                Cmd::Resume { id, at } => {
                    if let Some(v) = self.voice(id)
                        && matches!(v.state, VoiceState::Paused)
                    {
                        v.state = VoiceState::Waiting(at);
                    }
                }
                Cmd::Stop(id) => {
                    let rate = self.clock.rate;
                    if let Some(v) = self.voice(id) {
                        match v.state {
                            VoiceState::Playing => {
                                let frames = (DECLICK_SECS * rate as f64) as u64;
                                let from = v.gain;
                                v.ramp = Some(Ramp {
                                    from,
                                    to: 0.0,
                                    start: 0,
                                    total: frames.max(1),
                                    pos: 0,
                                    curve: FadeCurve::Linear,
                                    started: false,
                                });
                                v.stop_after_ramp = true;
                            }
                            _ => v.done = true,
                        }
                    }
                }
                Cmd::Ramp { id, to, frames, curve, at, stop_after } => {
                    if let Some(v) = self.voice(id) {
                        if v.stop_after_ramp {
                            continue;
                        }
                        v.ramp = Some(Ramp {
                            from: v.gain,
                            to,
                            start: at,
                            total: frames.max(1),
                            pos: 0,
                            curve,
                            started: false,
                        });
                        v.stop_after_ramp = stop_after;
                    }
                }
            }
        }
    }

    fn voice(&mut self, id: u64) -> Option<&mut Voice> {
        self.voices.iter_mut().find(|v| v.id == id).map(|b| &mut **b)
    }

    fn render(&mut self, out: &mut [f32]) {
        let ch = self.channels;
        let frames = out.len() / ch;
        out.fill(0.0);
        let start_frame = self.frame;
        for v in self.voices.iter_mut() {
            render_voice(v, out, ch, frames, start_frame);
        }
        // Retire finished voices without freeing on this thread.
        let mut i = 0;
        while i < self.voices.len() {
            if self.voices[i].done {
                let v = self.voices.swap_remove(i);
                v.shared.finished.store(true, Ordering::Release);
                v.shared.stop.store(true, Ordering::Release);
                if let Err(rtrb::PushError::Full(v)) = self.garbage.push(v) {
                    drop(v);
                }
            } else {
                i += 1;
            }
        }
        // Master gain, clipping protection and metering.
        let mut peaks = [0f32; MAX_METER_CHANNELS];
        for f in out.chunks_exact_mut(ch) {
            for (c, s) in f.iter_mut().enumerate() {
                *s = (*s * self.master).clamp(-1.0, 1.0);
                if c < MAX_METER_CHANNELS {
                    peaks[c] = peaks[c].max(s.abs());
                }
            }
        }
        for (c, p) in peaks.iter().enumerate().take(ch.min(MAX_METER_CHANNELS)) {
            let prev = f32::from_bits(self.meters[c].load(Ordering::Relaxed));
            if *p > prev {
                self.meters[c].store(p.to_bits(), Ordering::Relaxed);
            }
        }
        self.frame += frames as u64;
    }
}

fn render_voice(v: &mut Voice, out: &mut [f32], ch: usize, frames: usize, start_frame: u64) {
    if v.done {
        return;
    }
    let mut i = 0usize;
    match v.state {
        VoiceState::Idle | VoiceState::Paused => return,
        VoiceState::Waiting(at) => {
            if at >= start_frame + frames as u64 {
                return;
            }
            i = at.saturating_sub(start_frame) as usize;
            v.state = VoiceState::Playing;
        }
        VoiceState::Playing => {}
    }

    let available = v.rx.slots() / ch;
    let n = (frames - i).min(available);
    if n > 0 {
        let Ok(chunk) = v.rx.read_chunk(n * ch) else { return };
        let (a, b) = chunk.as_slices();
        let mut samples = a.iter().chain(b.iter());
        for f in i..i + n {
            let gf = start_frame + f as u64;
            let g = match &mut v.ramp {
                Some(r) if r.started || gf >= r.start => {
                    if !r.started {
                        r.started = true;
                        r.from = v.gain;
                    }
                    r.pos += 1;
                    let t = r.pos as f32 / r.total as f32;
                    let g = r.curve.interpolate(r.from, r.to, t);
                    v.gain = g;
                    if r.pos >= r.total {
                        v.gain = r.to;
                        v.ramp = None;
                        if v.stop_after_ramp {
                            v.done = true;
                        }
                    }
                    g
                }
                _ => v.gain,
            };
            let base = f * ch;
            for c in 0..ch {
                let s = *samples.next().unwrap_or(&0.0);
                let p = if c < 2 && ch >= 2 { v.pan[c] } else { 1.0 };
                out[base + c] += s * g * p;
            }
            if v.done {
                break;
            }
        }
        chunk.commit_all();
        v.shared.frames_played.fetch_add(n as u64, Ordering::Relaxed);
    }
    if !v.done && i + n < frames && v.rx.slots() < ch && v.shared.eof.load(Ordering::Acquire) {
        v.done = true;
    }
}

struct VoiceHandle {
    id: u64,
    shared: Arc<VoiceShared>,
    params: AudioCue,
    error_reported: bool,
}

/// Control side of the audio output.
pub struct AudioOutput {
    cmd: rtrb::Producer<Cmd>,
    garbage: rtrb::Consumer<Box<Voice>>,
    clock: Arc<Clock>,
    meters: Arc<[AtomicU32; MAX_METER_CHANNELS]>,
    pub rate: u32,
    pub channels: usize,
    pub device_name: String,
    voices: HashMap<CueId, VoiceHandle>,
    preloaded: HashMap<CueId, VoiceHandle>,
    next_id: u64,
}

impl AudioOutput {
    pub fn open() -> anyhow::Result<Self> {
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::<Cmd>::new(4096);
        let (gb_tx, gb_rx) = rtrb::RingBuffer::<Box<Voice>>::new(1024);
        let meters: Arc<[AtomicU32; MAX_METER_CHANNELS]> = Arc::new(Default::default());
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<anyhow::Result<(u32, usize, String, Arc<Clock>)>>();
        let meters2 = meters.clone();

        // The cpal stream isn't Send on every platform, so it lives on its own thread.
        std::thread::Builder::new().name("openq-audio-out".into()).spawn(move || {
            let res = (|| -> anyhow::Result<(cpal::Stream, u32, usize, String, Arc<Clock>)> {
                let host = cpal::default_host();
                let device = host.default_output_device().context("no default audio output device")?;
                let name = device
                    .description()
                    .map(|d| d.to_string())
                    .unwrap_or_else(|_| "default".to_string());
                let supported = device.default_output_config()?;
                let format = supported.sample_format();
                let config: StreamConfig = supported.into();
                let rate = config.sample_rate;
                let channels = config.channels as usize;
                let clock = Arc::new(Clock::new(rate));
                let mixer = Mixer {
                    cmds: cmd_rx,
                    garbage: gb_tx,
                    voices: Vec::with_capacity(256),
                    channels,
                    frame: 0,
                    master: 1.0,
                    clock: clock.clone(),
                    meters: meters2,
                    scratch: vec![0.0; 8192 * channels],
                };
                let stream = match format {
                    SampleFormat::F32 => build::<f32>(&device, config, mixer)?,
                    SampleFormat::I16 => build::<i16>(&device, config, mixer)?,
                    SampleFormat::I32 => build::<i32>(&device, config, mixer)?,
                    SampleFormat::U16 => build::<u16>(&device, config, mixer)?,
                    SampleFormat::F64 => build::<f64>(&device, config, mixer)?,
                    f => return Err(anyhow!("unsupported sample format {f}")),
                };
                stream.play()?;
                Ok((stream, rate, channels, name, clock))
            })();
            match res {
                Ok((stream, rate, ch, name, clock)) => {
                    let _ = setup_tx.send(Ok((rate, ch, name, clock)));
                    // Keep the stream alive for the life of the process.
                    loop {
                        std::thread::park();
                        let _ = &stream;
                    }
                }
                Err(e) => {
                    let _ = setup_tx.send(Err(e));
                }
            }
        })?;

        let (rate, channels, device_name, clock) = setup_rx.recv().context("audio thread died")??;
        log::info!("audio output: {device_name} @ {rate} Hz, {channels} ch");
        Ok(Self {
            cmd: cmd_tx,
            garbage: gb_rx,
            clock,
            meters,
            rate,
            channels,
            device_name,
            voices: HashMap::new(),
            preloaded: HashMap::new(),
            next_id: 1,
        })
    }

    fn send(&mut self, cmd: Cmd) {
        if self.cmd.push(cmd).is_err() {
            log::error!("audio command queue full");
        }
    }

    fn create_voice(&mut self, params: &AudioCue) -> VoiceHandle {
        let id = self.next_id;
        self.next_id += 1;
        let cap = (self.rate as f64 * VOICE_BUFFER_SECS) as usize * self.channels;
        let (tx, rx) = rtrb::RingBuffer::<f32>::new(cap);
        let shared = Arc::new(VoiceShared::default());
        decoder::spawn(
            params.path.clone(),
            DecodeParams {
                start: params.start_time.max(0.0),
                end: params.end_time,
                plays: if params.infinite_loop { None } else { Some(params.play_count.max(1)) },
            },
            self.rate,
            self.channels,
            tx,
            shared.clone(),
        );
        let voice = Box::new(Voice {
            id,
            rx,
            shared: shared.clone(),
            state: VoiceState::Idle,
            gain: db_to_gain(params.volume_db),
            pan: pan_gains(params.pan),
            ramp: None,
            stop_after_ramp: false,
            done: false,
        });
        self.send(Cmd::Add(voice));
        VoiceHandle { id, shared, params: params.clone(), error_reported: false }
    }

    /// Keep exactly these cues preloaded (buffered and ready to start instantly).
    pub fn set_preloaded(&mut self, cues: &[(CueId, &AudioCue)]) {
        let keep: Vec<CueId> = cues.iter().map(|c| c.0).collect();
        let stale: Vec<CueId> = self
            .preloaded
            .iter()
            .filter(|(id, h)| {
                !keep.contains(id) || cues.iter().find(|c| c.0 == **id).is_none_or(|c| *c.1 != h.params)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(h) = self.preloaded.remove(&id) {
                self.send(Cmd::Stop(h.id));
            }
        }
        for (id, p) in cues {
            if !self.preloaded.contains_key(id) && !p.path.as_os_str().is_empty() {
                let h = self.create_voice(p);
                self.preloaded.insert(*id, h);
            }
        }
    }

    pub fn start(&mut self, cue: CueId, params: &AudioCue, at: Instant) -> Result<(), String> {
        if params.path.as_os_str().is_empty() {
            return Err("no audio file".into());
        }
        if !params.path.exists() {
            return Err(format!("file not found: {}", params.path.display()));
        }
        self.stop(cue);
        let handle = match self.preloaded.remove(&cue) {
            Some(h) if h.params == *params && !h.shared.finished.load(Ordering::Acquire) => h,
            Some(h) => {
                self.send(Cmd::Stop(h.id));
                self.create_voice(params)
            }
            None => self.create_voice(params),
        };
        if let Some(err) = handle.shared.error.lock().clone() {
            self.send(Cmd::Stop(handle.id));
            return Err(err);
        }
        let frame = self.clock.frame_at(at);
        self.send(Cmd::Start { id: handle.id, at: frame });
        self.voices.insert(cue, handle);
        Ok(())
    }

    pub fn stop(&mut self, cue: CueId) {
        if let Some(h) = self.voices.remove(&cue) {
            self.send(Cmd::Stop(h.id));
        }
    }

    pub fn pause(&mut self, cue: CueId) {
        if let Some(id) = self.voices.get(&cue).map(|h| h.id) {
            self.send(Cmd::Pause(id));
        }
    }

    pub fn resume(&mut self, cue: CueId, at: Instant) {
        if let Some(id) = self.voices.get(&cue).map(|h| h.id) {
            let at = self.clock.frame_at(at);
            self.send(Cmd::Resume { id, at });
        }
    }

    pub fn fade(&mut self, cue: CueId, to_db: f32, secs: f64, curve: FadeCurve, at: Instant) {
        if let Some(id) = self.voices.get(&cue).map(|h| h.id) {
            let frames = (secs.max(0.0) * self.rate as f64) as u64;
            let at = self.clock.frame_at(at);
            self.send(Cmd::Ramp { id, to: db_to_gain(to_db), frames, curve, at, stop_after: false });
        }
    }

    pub fn set_master(&mut self, db: f32) {
        self.send(Cmd::Master(db_to_gain(db)));
    }

    pub fn has(&self, cue: CueId) -> bool {
        self.voices.contains_key(&cue)
    }

    /// True if the cue isn't playing (finished, errored, or never started).
    pub fn is_finished(&self, cue: CueId) -> bool {
        self.voices.get(&cue).is_none_or(|h| h.shared.finished.load(Ordering::Acquire))
    }

    pub fn elapsed(&self, cue: CueId) -> Option<f64> {
        self.voices
            .get(&cue)
            .map(|h| h.shared.frames_played.load(Ordering::Relaxed) as f64 / self.rate as f64)
    }

    /// Errors from decoder threads not reported yet.
    pub fn drain_errors(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for h in self.voices.values_mut() {
            if !h.error_reported
                && let Some(e) = h.shared.error.lock().clone()
            {
                h.error_reported = true;
                out.push(format!("{}: {e}", h.params.path.display()));
            }
        }
        out
    }

    /// Peak per output channel since the last call (linear 0..1).
    pub fn take_meters(&self) -> Vec<f32> {
        (0..self.channels.min(MAX_METER_CHANNELS))
            .map(|c| f32::from_bits(self.meters[c].swap(0, Ordering::Relaxed)))
            .collect()
    }

    /// Housekeeping: free retired voices.
    pub fn update(&mut self) {
        while let Ok(v) = self.garbage.pop() {
            drop(v);
        }
    }
}

fn build<T>(device: &cpal::Device, config: StreamConfig, mut mixer: Mixer) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = mixer.channels;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &OutputCallbackInfo| {
            mixer.handle_commands();
            mixer.clock.store(mixer.frame, Instant::now());
            let mut scratch = std::mem::take(&mut mixer.scratch);
            for chunk in data.chunks_mut(scratch.len() / ch * ch) {
                let buf = &mut scratch[..chunk.len()];
                mixer.render(buf);
                for (o, s) in chunk.iter_mut().zip(buf.iter()) {
                    *o = T::from_sample(*s);
                }
            }
            mixer.scratch = scratch;
        },
        |e| log::error!("audio stream error: {e}"),
        None,
    )?;
    Ok(stream)
}
