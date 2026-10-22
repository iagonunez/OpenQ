//! Real-time audio output: a lock-free mixer running in the cpal callback.
//!
//! Each playing audio cue is a [`Voice`] fed by its own decoder thread through a ring buffer
//! (resampled to the device rate, still in the file's channel layout). The voice applies the
//! cue's EQ, effect chain ([`crate::audio_fx`]), routing matrix, level and pan in the callback,
//! so all of them can change live. Routing changes (crosspoint edits, object audio moving
//! through the room) ramp over [`MATRIX_RAMP_SECS`] instead of jumping, so they never click.
//! The control side talks to the callback only through a wait-free command queue, and removed
//! voices are shipped back for deallocation so the audio thread never frees memory.
//!
//! Timing: the mixer's frame counter is tied to wall-clock time by a [`ClockFilter`] (a
//! delay-locked loop) fed with each callback's timestamp, so scheduling jitter of the audio
//! thread doesn't move sample-accurate start times around.
//!
//! Failure handling: a panic in the callback silences the output instead of unwinding into
//! the audio driver, and the output (like one whose device went away) reports itself
//! [lost](AudioOutput::is_lost) so [`super::Media`] can reconnect. An [`Endpoint::Virtual`] output
//! runs the same mixer on a timer thread without hardware, for tests and CI machines.

use super::decoder::{self, DecodeParams};
use crate::audio_fx::FxChain;
use crate::dsp::{BiquadState, Coeffs, eq_coeffs};
use crate::effects::AudioEffect;
use crate::spatial::{AudioObject, ObjectAudio, Speaker};
use crate::model::{
    AudioCue, CueId, Eq, FadeCurve, MAX_EQ_BANDS, MAX_OUT_CHANNELS, MAX_SRC_CHANNELS, VideoCue, db_to_gain, out_level_gain, routing_gains,
};
use anyhow::{Context, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, OutputCallbackInfo, SampleFormat, SizedSample, StreamConfig};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const MAX_METER_CHANNELS: usize = MAX_OUT_CHANNELS;

/// Peak-hold slots written by the audio callback, one per reader.
type Meters = Arc<[[AtomicU32; MAX_METER_CHANNELS]; 3]>;

fn new_meters() -> Meters {
    Arc::new(std::array::from_fn(|_| std::array::from_fn(|_| AtomicU32::new(0))))
}

/// Who is reading the meters; each has its own peak-hold slot.
#[derive(Clone, Copy, Debug)]
pub enum MeterReader {
    /// The local UI.
    Local = 0,
    /// Remote replicas.
    Remote = 1,
    /// The light thread (audio-reactive effects), through a [`MeterTap`].
    Light = 2,
}

/// The light thread's view of an output's meters: the loudest peak since the last read.
#[derive(Clone)]
pub struct MeterTap {
    meters: Meters,
    channels: usize,
}

impl MeterTap {
    /// Peak over every output channel since the last call (linear 0..1). No allocation.
    pub fn take_peak(&self) -> f32 {
        let set = &self.meters[MeterReader::Light as usize];
        set[..self.channels].iter().map(|m| f32::from_bits(m.swap(0, Ordering::Relaxed))).fold(0.0, f32::max)
    }

    /// Identity of the output it reads (changes when the output is reopened).
    pub fn id(&self) -> usize {
        Arc::as_ptr(&self.meters) as *const u8 as usize
    }
}
/// Fade applied on stop to avoid clicks.
const DECLICK_SECS: f64 = 0.008;
/// Seconds of decoded audio buffered per voice.
const VOICE_BUFFER_SECS: f64 = 2.0;
/// Most voices (playing plus preloaded) the mixer holds. Its voice list is allocated once with
/// this capacity, so adding a voice never allocates on the audio thread.
pub const MAX_VOICES: usize = 256;
/// Commands held back while the mixer's queue is full (beyond this they are dropped).
const MAX_PENDING: usize = 1 << 16;

/// Time a voice takes to move to a new routing matrix.
pub const MATRIX_RAMP_SECS: f64 = 0.01;
/// How often a running object-audio fade sends new positions to the mixer.
const OBJECT_FADE_STEP: Duration = Duration::from_millis(10);

/// Linear routing gains, `[src * MAX_OUT_CHANNELS + out]`.
pub type Matrix = [f32; MAX_SRC_CHANNELS * MAX_OUT_CHANNELS];

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

    /// Anchor `frame` at `secs` after the clock's origin.
    fn store(&self, frame: u64, secs: f64) {
        // Seqlock writer (single writer: the audio callback).
        self.seq.fetch_add(1, Ordering::AcqRel);
        self.frame.store(frame, Ordering::Relaxed);
        self.nanos.store((secs.max(0.0) * 1e9) as u64, Ordering::Relaxed);
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
                return (f, self.origin + Duration::from_nanos(n));
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

/// Smooths the callback timestamps that anchor the mixer [`Clock`]: a second-order
/// delay-locked loop that tracks the device's real sample rate while averaging out the
/// scheduling jitter of the audio thread (often a millisecond or more). A single late callback
/// is ignored; several in a row (an xrun, a suspended machine) re-anchor at once.
#[derive(Clone, Debug)]
pub struct ClockFilter {
    nominal: f64,
    period: f64,
    frame: u64,
    t: f64,
    started: bool,
    outliers: u32,
}

impl ClockFilter {
    /// Loop gain for the time estimate (per callback).
    const GAIN: f64 = 0.01;
    /// Loop gain for the period (rate) estimate.
    const RATE_GAIN: f64 = Self::GAIN * Self::GAIN / 4.0;
    /// Errors beyond this are outliers (seconds).
    const OUTLIER: f64 = 0.004;
    /// Consecutive outliers that force a re-anchor.
    const MAX_OUTLIERS: u32 = 3;

    /// A filter for a device running at `rate` Hz.
    pub fn new(rate: u32) -> Self {
        let nominal = 1.0 / rate.max(1) as f64;
        Self { nominal, period: nominal, frame: 0, t: 0.0, started: false, outliers: 0 }
    }

    /// The callback that starts at `frame` ran at `measured` seconds; returns the smoothed
    /// time of `frame`.
    pub fn observe(&mut self, frame: u64, measured: f64) -> f64 {
        if !self.started || frame <= self.frame {
            return self.reset(frame, measured);
        }
        let n = (frame - self.frame) as f64;
        let predicted = self.t + self.period * n;
        let e = measured - predicted;
        if e.abs() > Self::OUTLIER {
            self.outliers += 1;
            if self.outliers >= Self::MAX_OUTLIERS {
                return self.reset(frame, measured);
            }
            self.t = predicted;
            self.frame = frame;
            return predicted;
        }
        self.outliers = 0;
        self.t = predicted + Self::GAIN * e;
        self.period = (self.period + Self::RATE_GAIN * e / n).clamp(self.nominal * 0.995, self.nominal * 1.005);
        self.frame = frame;
        self.t
    }

    fn reset(&mut self, frame: u64, measured: f64) -> f64 {
        self.started = true;
        self.outliers = 0;
        self.frame = frame;
        self.t = measured;
        measured
    }
}

/// Health of an output, shared with its audio thread.
#[derive(Default)]
struct Health {
    /// The device went away or the callback failed: the output must be reopened.
    lost: AtomicBool,
    /// Buffer underruns/overruns reported by the driver.
    xruns: AtomicU64,
    /// Callbacks run so far.
    callbacks: AtomicU64,
    /// Time spent rendering, in nanoseconds (see [`DspLoad`]).
    busy_ns: AtomicU64,
    /// Audio rendered, in nanoseconds of sound.
    audio_ns: AtomicU64,
    /// Highest busy/audio ratio of a single callback, in millionths.
    peak_ppm: AtomicU64,
    /// Longest buffer a callback filled, in nanoseconds of sound.
    block_ns: AtomicU64,
}

/// How much of the audio thread's time budget the mixer uses. Above 1.0 the device runs dry
/// (dropouts); a peak near it means one more voice or a busy machine could cause one.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DspLoad {
    /// Time spent rendering since the output opened.
    pub busy: Duration,
    /// Sound rendered in that time.
    pub audio: Duration,
    /// Worst single callback: render time / length of its buffer.
    pub peak: f64,
}

impl DspLoad {
    /// Average load (render time / audio time) between `earlier` and this reading.
    pub fn since(&self, earlier: &DspLoad) -> f64 {
        let audio = self.audio.saturating_sub(earlier.audio).as_secs_f64();
        if audio > 0.0 { self.busy.saturating_sub(earlier.busy).as_secs_f64() / audio } else { 0.0 }
    }
}

/// Where an [`AudioOutput`] sends its sound.
#[derive(Clone, Debug, PartialEq)]
pub enum Endpoint {
    /// A sound card (cpal device id; None = the system default).
    Device(Option<String>),
    /// No hardware: a timer thread pulls the mixer at this rate and channel count.
    Virtual {
        /// Sample rate.
        rate: u32,
        /// Output channels.
        channels: usize,
    },
}

/// Refuse a sample rate or channel count no sound card has (a broken driver can report 0 Hz,
/// which would make every period infinitely long).
fn check_format(rate: u32, channels: usize) -> anyhow::Result<()> {
    if !(1000..=1_000_000).contains(&rate) || channels == 0 {
        bail!("unusable audio format: {rate} Hz, {channels} channels");
    }
    Ok(())
}

/// State shared between a voice, its decoder thread and the control side.
#[derive(Default)]
pub struct VoiceShared {
    /// Frames this voice has produced so far.
    pub frames_played: AtomicU64,
    /// Voice is gone from the mixer (ended, stopped or errored).
    pub finished: AtomicBool,
    /// Decoder produced its last sample.
    pub eof: AtomicBool,
    /// Ask the decoder thread to quit.
    pub stop: AtomicBool,
    /// Decoder filled the buffer once (or hit EOF): safe to start without underrun.
    pub ready: AtomicBool,
    /// Leave the current looping slice after this pass.
    pub devamp: AtomicBool,
    /// Decoder failure, if any.
    pub error: parking_lot::Mutex<Option<String>>,
    /// The mixer had no room for this voice (more than [`MAX_VOICES`]).
    pub rejected: AtomicBool,
    /// Frames the mixer wanted while playing but the decoder hadn't delivered (it fell behind or
    /// stalled, e.g. on a network share that stopped answering).
    pub starved: AtomicU64,
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

/// One playing sound inside the mixer (owned by the audio thread).
pub struct Voice {
    id: u64,
    rx: rtrb::Consumer<f32>,
    shared: Arc<VoiceShared>,
    state: VoiceState,
    src_channels: usize,
    gain: f32,
    pan: [f32; 2],
    matrix: Matrix,
    /// Matrix being left behind while `matrix_ramp` frames remain of a routing change.
    matrix_from: Matrix,
    matrix_ramp: u32,
    matrix_ramp_len: u32,
    eq: [Coeffs; MAX_EQ_BANDS],
    eq_bands: usize,
    eq_state: [[BiquadState; MAX_EQ_BANDS]; MAX_SRC_CHANNELS],
    /// Audio effects after the EQ (None = no effect enabled).
    fx: Option<Box<FxChain>>,
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
    Pan { id: u64, pan: [f32; 2] },
    Eq { id: u64, coeffs: [Coeffs; MAX_EQ_BANDS], bands: usize },
    Matrix { id: u64, matrix: Box<Matrix> },
    Fx { id: u64, chain: Option<Box<FxChain>> },
    /// New settings for the voice's effects, keeping their state (a Fade cue).
    FxParams { id: u64, params: [[f32; 6]; crate::effects::MAX_AUDIO_FX] },
    /// Freeze any running gain ramp at its current value.
    Hold(u64),
    Master(f32),
}

fn pan_gains(pan: f32) -> [f32; 2] {
    // Constant power, normalised so centre is unity on both sides.
    let theta = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    [theta.cos() * std::f32::consts::SQRT_2, theta.sin() * std::f32::consts::SQRT_2]
}

/// The live-adjustable mix settings of a voice (audio cue or video soundtrack).
#[derive(Clone, PartialEq, Debug, Default)]
pub struct MixParams {
    /// Level in dB.
    pub volume_db: f32,
    /// Stereo pan −1..1 (stereo outputs without a routing matrix).
    pub pan: f32,
    /// Per-voice equaliser.
    pub eq: Eq,
    /// Gain matrix `[source][output]`; None = default routing.
    pub routing: Option<Vec<Vec<f32>>>,
    /// Output fader levels in dB, folded into the matrix.
    pub out_levels: Vec<f32>,
    /// Effect chain after the EQ.
    pub audio_fx: Vec<AudioEffect>,
    /// Object audio (replaces `routing` when enabled).
    pub objects: ObjectAudio,
}

impl From<&AudioCue> for MixParams {
    fn from(a: &AudioCue) -> Self {
        Self {
            volume_db: a.volume_db,
            pan: a.pan,
            eq: a.eq.clone(),
            routing: a.routing.clone(),
            out_levels: a.out_levels.clone(),
            audio_fx: a.audio_fx.clone(),
            objects: a.objects.clone(),
        }
    }
}

impl From<&crate::model::MicCue> for MixParams {
    fn from(m: &crate::model::MicCue) -> Self {
        Self {
            volume_db: m.volume_db,
            pan: m.pan,
            eq: m.eq.clone(),
            routing: m.routing.clone(),
            out_levels: m.out_levels.clone(),
            audio_fx: m.audio_fx.clone(),
            objects: m.objects.clone(),
        }
    }
}

impl From<&VideoCue> for MixParams {
    fn from(v: &VideoCue) -> Self {
        Self {
            volume_db: v.volume_db,
            pan: 0.0,
            eq: Eq::default(),
            routing: v.routing.clone(),
            out_levels: v.out_levels.clone(),
            audio_fx: v.audio_fx.clone(),
            objects: v.objects.clone(),
        }
    }
}

/// The voice matrix for `params`: crosspoints (or object positions in `room`), times the output
/// faders.
fn build_matrix(params: &MixParams, src: usize, out: usize, room: &[Speaker]) -> Matrix {
    let mut m = [0.0; MAX_SRC_CHANNELS * MAX_OUT_CHANNELS];
    let gains = if params.objects.enabled {
        crate::spatial::object_matrix(&params.objects, room, src, out)
    } else {
        routing_gains(&params.routing, src, out)
    };
    for (s, row) in gains.iter().enumerate().take(MAX_SRC_CHANNELS) {
        for (o, g) in row.iter().enumerate().take(MAX_OUT_CHANNELS) {
            m[s * MAX_OUT_CHANNELS + o] = *g * out_level_gain(&params.out_levels, o);
        }
    }
    m
}

/// Things the audio thread hands back to be freed elsewhere.
enum Garbage {
    Voice(#[allow(dead_code)] Box<Voice>),
    Matrix(#[allow(dead_code)] Box<Matrix>),
    Fx(#[allow(dead_code)] Box<FxChain>),
}

/// Garbage kept on the audio thread while the queue is full (allocated up front).
const GARBAGE_OVERFLOW: usize = 1024;

struct Mixer {
    cmds: rtrb::Consumer<Cmd>,
    garbage: rtrb::Producer<Garbage>,
    /// Garbage the full queue didn't take, handed over on later callbacks: freeing it here
    /// could stall the callback (the engine thread empties the queue each tick, so it fills
    /// only while that thread is held up).
    overflow: Vec<Garbage>,
    // Boxed so voices move in/out of the RT thread via the queues without copying or freeing.
    #[allow(clippy::vec_box)]
    voices: Vec<Box<Voice>>,
    channels: usize,
    frame: u64,
    master: f32,
    clock: Arc<Clock>,
    filter: ClockFilter,
    meters: Meters,
    scratch: Vec<f32>,
    health: Arc<Health>,
}

impl Mixer {
    /// Hand `g` to the engine thread to free, or keep it for a later callback if the queue is full.
    fn retire(&mut self, g: Garbage) {
        if let Err(rtrb::PushError::Full(g)) = self.garbage.push(g) {
            if self.overflow.len() < self.overflow.capacity() {
                self.overflow.push(g);
            } else {
                // Both full: the engine thread has stopped emptying the queue for a long time.
                drop(g);
            }
        }
    }

    /// Move kept garbage into the queue while it has room.
    fn flush_overflow(&mut self) {
        while let Some(g) = self.overflow.pop() {
            if let Err(rtrb::PushError::Full(g)) = self.garbage.push(g) {
                self.overflow.push(g);
                break;
            }
        }
    }

    fn handle_commands(&mut self) {
        if !self.overflow.is_empty() {
            self.flush_overflow();
        }
        while let Ok(cmd) = self.cmds.pop() {
            match cmd {
                Cmd::Add(v) => {
                    if self.voices.len() < MAX_VOICES {
                        self.voices.push(v);
                    } else {
                        // No room (pushing would allocate): hand it straight back.
                        v.shared.rejected.store(true, Ordering::Release);
                        v.shared.finished.store(true, Ordering::Release);
                        v.shared.stop.store(true, Ordering::Release);
                        self.retire(Garbage::Voice(v));
                    }
                }
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
                                v.ramp = Some(Ramp {
                                    from: v.gain,
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
                        v.ramp = Some(Ramp { from: v.gain, to, start: at, total: frames.max(1), pos: 0, curve, started: false });
                        v.stop_after_ramp = stop_after;
                    }
                }
                Cmd::Hold(id) => {
                    if let Some(v) = self.voice(id)
                        && !v.stop_after_ramp
                    {
                        v.ramp = None;
                    }
                }
                Cmd::Pan { id, pan } => {
                    if let Some(v) = self.voice(id) {
                        v.pan = pan;
                    }
                }
                Cmd::Eq { id, coeffs, bands } => {
                    if let Some(v) = self.voice(id) {
                        if bands != v.eq_bands {
                            // Band layout changed: start the filters from rest.
                            v.eq_state = [[BiquadState::default(); MAX_EQ_BANDS]; MAX_SRC_CHANNELS];
                        }
                        v.eq = coeffs;
                        v.eq_bands = bands;
                    }
                }
                Cmd::Matrix { id, mut matrix } => {
                    let len = ((self.clock.rate as f64 * MATRIX_RAMP_SECS) as u32).max(1);
                    if let Some(v) = self.voice(id) {
                        // Ramp from wherever the voice is now (possibly mid-ramp) to the new one.
                        if v.matrix_ramp > 0 {
                            let t = 1.0 - v.matrix_ramp as f32 / v.matrix_ramp_len as f32;
                            for (a, b) in v.matrix_from.iter_mut().zip(v.matrix.iter()) {
                                *a += (b - *a) * t;
                            }
                        } else {
                            v.matrix_from = v.matrix;
                        }
                        std::mem::swap(&mut v.matrix, &mut *matrix);
                        if matches!(v.state, VoiceState::Playing) {
                            v.matrix_ramp = len;
                            v.matrix_ramp_len = len;
                        }
                    }
                    // The box (now holding the old matrix) is freed off the audio thread.
                    self.retire(Garbage::Matrix(matrix));
                }
                Cmd::FxParams { id, params } => {
                    if let Some(fx) = self.voice(id).and_then(|v| v.fx.as_mut()) {
                        fx.retune(&params);
                    }
                }
                Cmd::Fx { id, mut chain } => {
                    if let Some(v) = self.voice(id) {
                        if let (Some(new), Some(old)) = (chain.as_mut(), v.fx.as_mut()) {
                            new.adopt(old);
                        }
                        std::mem::swap(&mut v.fx, &mut chain);
                    }
                    // `chain` now holds the replaced one (or the unused new one).
                    if let Some(c) = chain {
                        self.retire(Garbage::Fx(c));
                    }
                }
            }
        }
    }

    /// One device callback: take commands, re-anchor the clock, render `data`.
    fn fill<T: FromSample<f32>>(&mut self, data: &mut [T]) {
        crate::recovery::fault_point(crate::recovery::Fault::AudioCallback);
        self.health.callbacks.fetch_add(1, Ordering::Relaxed);
        self.handle_commands();
        let measured = self.clock.origin.elapsed().as_secs_f64();
        let t = self.filter.observe(self.frame, measured);
        self.clock.store(self.frame, t);
        let ch = self.channels;
        let mut scratch = std::mem::take(&mut self.scratch);
        for chunk in data.chunks_mut(scratch.len() / ch * ch) {
            let buf = &mut scratch[..chunk.len()];
            self.render(buf);
            for (o, s) in chunk.iter_mut().zip(buf.iter()) {
                *o = T::from_sample_(*s);
            }
        }
        self.scratch = scratch;
    }

    /// [`Mixer::fill`], but a panic silences the output for good and marks it lost instead of
    /// unwinding into the driver.
    fn fill_guarded<T: FromSample<f32>>(&mut self, failed: &mut bool, data: &mut [T]) {
        let t0 = Instant::now();
        if !*failed && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.fill(data))).is_err() {
            *failed = true;
            self.health.lost.store(true, Ordering::Release);
        }
        if *failed {
            for s in data.iter_mut() {
                *s = T::from_sample_(0.0);
            }
        }
        // DSP load: two clock reads per callback (a vDSO call, no syscall), relaxed counters.
        let busy = t0.elapsed().as_nanos() as u64;
        let audio = (data.len() / self.channels.max(1)) as u64 * 1_000_000_000 / self.clock.rate.max(1) as u64;
        self.health.busy_ns.fetch_add(busy, Ordering::Relaxed);
        self.health.audio_ns.fetch_add(audio, Ordering::Relaxed);
        self.health.block_ns.fetch_max(audio, Ordering::Relaxed);
        if let Some(ppm) = (busy * 1_000_000).checked_div(audio) {
            self.health.peak_ppm.fetch_max(ppm, Ordering::Relaxed);
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
                self.retire(Garbage::Voice(v));
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
        for set in self.meters.iter() {
            for (c, p) in peaks.iter().enumerate().take(ch.min(MAX_METER_CHANNELS)) {
                let prev = f32::from_bits(set[c].load(Ordering::Relaxed));
                if *p > prev {
                    set[c].store(p.to_bits(), Ordering::Relaxed);
                }
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

    let src = v.src_channels;
    let outs = ch.min(MAX_OUT_CHANNELS);
    let available = v.rx.slots() / src;
    let n = (frames - i).min(available);
    if n > 0 {
        let Ok(chunk) = v.rx.read_chunk(n * src) else { return };
        let (a, b) = chunk.as_slices();
        let mut samples = a.iter().chain(b.iter());
        let mut x = [0f32; MAX_SRC_CHANNELS];
        // Frames actually heard: a stop ramp can end the voice inside the chunk.
        let mut played = n;
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
            for (s, xs) in x.iter_mut().enumerate().take(src) {
                let mut val = *samples.next().unwrap_or(&0.0);
                for b in 0..v.eq_bands {
                    val = v.eq_state[s][b].process(&v.eq[b], val);
                }
                *xs = val;
            }
            if let Some(fx) = &mut v.fx {
                fx.process(&mut x[..src]);
            }
            let mt = (v.matrix_ramp > 0).then(|| {
                v.matrix_ramp -= 1;
                1.0 - v.matrix_ramp as f32 / v.matrix_ramp_len as f32
            });
            let base = f * ch;
            for o in 0..outs {
                let mut acc = 0.0;
                match mt {
                    None => {
                        for (s, xs) in x.iter().enumerate().take(src) {
                            acc += xs * v.matrix[s * MAX_OUT_CHANNELS + o];
                        }
                    }
                    Some(t) => {
                        for (s, xs) in x.iter().enumerate().take(src) {
                            let k = s * MAX_OUT_CHANNELS + o;
                            acc += xs * (v.matrix_from[k] + (v.matrix[k] - v.matrix_from[k]) * t);
                        }
                    }
                }
                let p = if o < 2 && ch >= 2 { v.pan[o] } else { 1.0 };
                out[base + o] += acc * g * p;
            }
            if v.done {
                played = f + 1 - i;
                break;
            }
        }
        chunk.commit_all();
        v.shared.frames_played.fetch_add(played as u64, Ordering::Relaxed);
    }
    if !v.done && i + n < frames && v.rx.slots() < src {
        if v.shared.eof.load(Ordering::Acquire) {
            v.done = true;
        } else {
            v.shared.starved.fetch_add((frames - i - n) as u64, Ordering::Relaxed);
        }
    }
}

/// Seconds of silence from a decoder that fell behind before it is reported.
const STALL_REPORT_SECS: f64 = 0.5;

struct VoiceHandle {
    id: u64,
    shared: Arc<VoiceShared>,
    /// Decode parameters (None for externally fed voices, e.g. video soundtracks).
    params: Option<AudioCue>,
    mix: MixParams,
    label: String,
    src_channels: usize,
    error_reported: bool,
    /// The decoder stall was reported (once per voice).
    stall_reported: bool,
    /// A Fade cue moving the objects, and when positions were last sent.
    obj_fade: Option<ObjectFade>,
    obj_sent: Option<Instant>,
    /// The cue's own object positions once a fade has taken them over.
    obj_base: Option<Vec<AudioObject>>,
    /// A Fade cue fading the output faders or effect settings.
    mix_fade: Option<MixFade>,
    /// The cue's own output faders and effects once a fade has taken them over.
    mix_base: Option<(Vec<f32>, Vec<AudioEffect>)>,
}

/// A Fade cue on a voice's output faders and effect settings, stepped every
/// [`OBJECT_FADE_STEP`] (each step a matrix and/or effect settings for the mixer).
struct MixFade {
    levels: Option<(Vec<f32>, Vec<f32>)>,
    fx: Option<(Vec<AudioEffect>, Vec<AudioEffect>)>,
    start: Instant,
    secs: f64,
    curve: FadeCurve,
    sent: Option<Instant>,
}

impl MixFade {
    /// Progress (curve applied) at `now`, and whether it is over.
    fn at(&self, now: Instant) -> (f32, bool) {
        let t = if self.secs <= 0.0 { 1.0 } else { now.saturating_duration_since(self.start).as_secs_f64() / self.secs };
        (self.curve.progress(t.clamp(0.0, 1.0) as f32), t >= 1.0 && now >= self.start)
    }
}

/// Output fader levels `t` of the way from `a` to `b` (dB; missing entries are 0 dB).
fn lerp_levels(a: &[f32], b: &[f32], t: f32) -> Vec<f32> {
    (0..a.len().max(b.len())).map(|i| {
        let (x, y) = (a.get(i).copied().unwrap_or(0.0), b.get(i).copied().unwrap_or(0.0));
        x + (y - x) * t
    }).collect()
}

/// Effect settings `t` of the way from `a` to `b` (same kinds in the same places).
fn lerp_fx(a: &[AudioEffect], b: &[AudioEffect], t: f32) -> Vec<AudioEffect> {
    b.iter().zip(a).map(|(to, from)| {
        let mut e = *to;
        for (p, (x, y)) in e.params.iter_mut().zip(from.params.iter().zip(&to.params)) {
            *p = x + (y - x) * t;
        }
        e
    }).collect()
}

/// The settings array the mixer takes for `fx`.
fn fx_params(fx: &[AudioEffect]) -> [[f32; 6]; crate::effects::MAX_AUDIO_FX] {
    let mut out = [[0.0; 6]; crate::effects::MAX_AUDIO_FX];
    for (o, e) in out.iter_mut().zip(fx) {
        *o = e.params;
    }
    out
}

/// An object-audio fade in progress on one voice.
struct ObjectFade {
    from: Vec<AudioObject>,
    to: Vec<AudioObject>,
    start: Instant,
    secs: f64,
    curve: FadeCurve,
}

impl ObjectFade {
    fn at(&self, now: Instant) -> (Vec<AudioObject>, bool) {
        let t = if self.secs <= 0.0 { 1.0 } else { now.saturating_duration_since(self.start).as_secs_f64() / self.secs };
        let done = t >= 1.0 && now >= self.start;
        (crate::spatial::lerp_objects(&self.from, &self.to, self.curve.progress(t as f32)), done)
    }
}

/// An output device the user can pick.
#[derive(Clone, Debug)]
pub struct DeviceEntry {
    /// cpal device id.
    pub id: String,
    /// Human-readable device name.
    pub name: String,
}

/// Output devices of the default host.
pub fn list_output_devices() -> Vec<DeviceEntry> {
    let host = cpal::default_host();
    let Ok(devs) = host.output_devices() else { return Vec::new() };
    device_entries(devs, true)
}

/// The pickable entries among `devs` (`output`: for an output picker, else an input picker).
///
/// PipeWire also lists other programs' streams and a sink/output default pair; only the
/// default and real sinks (outputs) or sources (inputs) are kept. On ALSA every card shows up a
/// dozen times (`front:`, `surround51:`, `iec958:`, `dmix:`, `hw:`, `plughw:`, rate
/// converters…), all with the same description, so only the sound servers, one `sysdefault:`
/// per card, HDMI outputs and the `plughw:` devices nothing else covers are kept. Ids left out
/// still open (see [`find_device`]).
pub(crate) fn device_entries(devs: impl Iterator<Item = cpal::Device>, output: bool) -> Vec<DeviceEntry> {
    use cpal::DeviceDirection as Dir;
    let all: Vec<(DeviceEntry, Option<cpal::DeviceDescription>)> = devs
        .filter_map(|d| {
            let id = d.id().ok()?.to_string();
            let desc = d.description().ok();
            let name = desc.as_ref().map(|x| x.name().to_string()).unwrap_or_else(|| id.clone());
            Some((DeviceEntry { id, name }, desc))
        })
        .collect();
    let mut out: Vec<DeviceEntry> = if all.iter().any(|(d, _)| d.id.starts_with("pipewire:")) {
        all.into_iter()
            .filter_map(|(mut d, desc)| {
                let node = d.id.strip_prefix("pipewire:").unwrap_or(&d.id);
                let default = if output { "output_default" } else { "input_default" };
                if node == default {
                    d.name = if output { "Default output".into() } else { "Default input".into() };
                    return Some(d);
                }
                let desc = desc?;
                // Streams are client nodes; sinks show up as duplex (their monitor can be captured).
                let real = desc.driver().is_some_and(|f| f != "client-node") && !node.ends_with("_default");
                let dir_ok = if output { desc.direction() == Dir::Duplex } else { desc.direction() == Dir::Input };
                (real && dir_ok).then_some(d)
            })
            .collect()
    } else {
        alsa_entries(all.into_iter().map(|(d, _)| d).collect())
    };
    // Same description twice (e.g. two PCMs of one card): tell them apart by id.
    for i in 0..out.len() {
        if out.iter().filter(|o| o.name == out[i].name).count() > 1 {
            let pcm = alsa_pcm(&out[i].id).unwrap_or(&out[i].id).to_string();
            out[i].name = format!("{} ({pcm})", out[i].name);
        }
    }
    out
}

/// The ALSA part of [`device_entries`] (anything else is returned unchanged).
fn alsa_entries(all: Vec<DeviceEntry>) -> Vec<DeviceEntry> {
    let ids: Vec<&str> = all.iter().map(|d| alsa_pcm(&d.id).unwrap_or("")).collect();
    if !ids.iter().any(|p| !p.is_empty()) {
        return all;
    }
    let card_of = |pcm: &str| -> Option<String> {
        let rest = pcm.split_once(':')?.1;
        let card = rest.split(',').next()?.strip_prefix("CARD=")?;
        Some(card.to_string())
    };
    let has = |prefix: &str, card: &str| ids.iter().any(|p| p.starts_with(prefix) && card_of(p).as_deref() == Some(card));
    let mut out = Vec::new();
    for (d, pcm) in all.iter().zip(&ids) {
        let keep = match pcm.split_once(':') {
            None => matches!(*pcm, "default" | "pipewire" | "pulse" | "jack"),
            Some(("sysdefault", _)) | Some(("hdmi", _)) => card_of(pcm).is_some(),
            Some(("plughw", rest)) => {
                // plughw ids carry the card index; the other plugins carry its name.
                let mut parts = rest.split(',');
                let index = parts.next().and_then(|c| c.strip_prefix("CARD=")).unwrap_or("");
                let dev = parts.next().and_then(|c| c.strip_prefix("DEV=")).unwrap_or("0");
                let card = alsa_card_name(index).unwrap_or_else(|| index.to_string());
                !has("hdmi:", &card) && (!has("sysdefault:", &card) || dev != "0")
            }
            _ => false,
        };
        if keep {
            out.push(d.clone());
        }
    }
    out
}

/// The ALSA PCM name inside a cpal device id (`alsa:sysdefault:CARD=K1` → `sysdefault:CARD=K1`).
fn alsa_pcm(id: &str) -> Option<&str> {
    id.strip_prefix("alsa:")
}

/// The ALSA card id (as used in `CARD=` names) for card number `index`.
fn alsa_card_name(index: &str) -> Option<String> {
    if !cfg!(target_os = "linux") || index.parse::<u32>().is_err() {
        return None;
    }
    std::fs::read_to_string(format!("/proc/asound/card{index}/id")).ok().map(|s| s.trim().to_string())
}

/// Find the device with cpal id `id` (on the host the id names, e.g. an `alsa:` id saved before
/// PipeWire support). Compares ids exactly first: cpal's own `device_by_id` canonicalises ALSA
/// names (`sysdefault:CARD=K1` → `…,DEV=0`) and then misses devices listed without a `DEV`.
pub(crate) fn find_device(id: &str, output: bool) -> anyhow::Result<cpal::Device> {
    let parsed: cpal::DeviceId = id.parse().map_err(|e| anyhow!("bad device id {id}: {e:?}"))?;
    let default = cpal::default_host();
    let host = if default.id() == parsed.host() { default } else { cpal::host_from_id(parsed.host())? };
    let devs = if output { host.output_devices() } else { host.input_devices() };
    if let Ok(mut devs) = devs
        && let Some(d) = devs.find(|d| d.id().is_ok_and(|x| x.to_string() == id))
    {
        return Ok(d);
    }
    host.device_by_id(&parsed).with_context(|| format!("audio device not found: {id}"))
}

/// Control side of the audio output.
pub struct AudioOutput {
    cmd: rtrb::Producer<Cmd>,
    /// Commands that didn't fit in the queue yet (sent on [`AudioOutput::update`]).
    pending: VecDeque<Cmd>,
    garbage: rtrb::Consumer<Garbage>,
    clock: Arc<Clock>,
    meters: Meters,
    shutdown: Arc<AtomicBool>,
    /// The thread owning the stream (woken on drop so the stream closes at once).
    thread: std::thread::Thread,
    health: Arc<Health>,
    /// Output sample rate.
    pub rate: u32,
    /// Output channel count.
    pub channels: usize,
    /// Name of the opened device.
    pub device_name: String,
    /// The id this output was opened with (None = system default).
    pub device_id: Option<String>,
    /// What this output was opened on (to reopen it the same way).
    pub endpoint: Endpoint,
    /// Master level last set, in dB.
    pub master_db: f32,
    voices: HashMap<CueId, VoiceHandle>,
    preloaded: HashMap<CueId, VoiceHandle>,
    next_id: u64,
    /// The object-audio room (resolved: never empty while there are outputs).
    room: Vec<Speaker>,
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        for h in self.voices.values().chain(self.preloaded.values()) {
            h.shared.stop.store(true, Ordering::Release);
        }
        self.shutdown.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

impl AudioOutput {
    /// Open `device_id` (cpal id string) or the system default.
    pub fn open(device_id: Option<&str>) -> anyhow::Result<Self> {
        Self::open_endpoint(&Endpoint::Device(device_id.map(str::to_string)), None)
    }

    /// A hardware-free output at `rate` Hz with `channels` outputs. When `capture` is given,
    /// everything the mixer renders is also pushed into it (dropped when it is full).
    pub fn open_virtual(rate: u32, channels: usize, capture: Option<rtrb::Producer<f32>>) -> anyhow::Result<Self> {
        Self::open_endpoint(&Endpoint::Virtual { rate, channels }, capture)
    }

    /// Open `endpoint` (see [`AudioOutput::open`] and [`AudioOutput::open_virtual`]).
    pub fn open_endpoint(endpoint: &Endpoint, capture: Option<rtrb::Producer<f32>>) -> anyhow::Result<Self> {
        if let Endpoint::Virtual { rate, .. } = endpoint {
            check_format(*rate, 1)?;
        }
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::<Cmd>::new(2048);
        let (gb_tx, gb_rx) = rtrb::RingBuffer::<Garbage>::new(2048);
        let meters: Meters = new_meters();
        let shutdown = Arc::new(AtomicBool::new(false));
        let health = Arc::new(Health::default());
        type Setup = anyhow::Result<(u32, usize, String, Arc<Clock>)>;
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Setup>();
        let (meters2, shutdown2, health2) = (meters.clone(), shutdown.clone(), health.clone());
        let ep = endpoint.clone();
        let make_mixer = move |rate: u32, channels: usize| {
            let clock = Arc::new(Clock::new(rate));
            let mixer = Mixer {
                cmds: cmd_rx,
                garbage: gb_tx,
                overflow: Vec::with_capacity(GARBAGE_OVERFLOW),
                voices: Vec::with_capacity(MAX_VOICES),
                channels,
                frame: 0,
                master: 1.0,
                clock: clock.clone(),
                filter: ClockFilter::new(rate),
                meters: meters2,
                scratch: vec![0.0; 8192 * channels],
                health: health2,
            };
            (mixer, clock)
        };

        // The cpal stream isn't Send on every platform, so it lives on its own thread.
        let handle = std::thread::Builder::new().name("openq-audio-out".into()).spawn(move || match ep {
            Endpoint::Device(wanted) => {
                let res = (|| -> anyhow::Result<(cpal::Stream, u32, usize, String, Arc<Clock>)> {
                    let host = cpal::default_host();
                    let device = match &wanted {
                        Some(id) => find_device(id, true)?,
                        None => host.default_output_device().context("no default audio output device")?,
                    };
                    let name = device.description().map(|d| d.to_string()).unwrap_or_else(|_| "default".to_string());
                    let supported = device.default_output_config()?;
                    let format = supported.sample_format();
                    let config: StreamConfig = supported.into();
                    let rate = config.sample_rate;
                    let channels = config.channels as usize;
                    check_format(rate, channels)?;
                    let (mixer, clock) = make_mixer(rate, channels);
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
                        // Keep the stream alive until the output is dropped.
                        while !shutdown2.load(Ordering::Acquire) {
                            std::thread::park_timeout(Duration::from_millis(100));
                        }
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = setup_tx.send(Err(e));
                    }
                }
            }
            Endpoint::Virtual { rate, channels } => {
                let channels = channels.clamp(1, MAX_OUT_CHANNELS);
                let (mut mixer, clock) = make_mixer(rate, channels);
                let _ = setup_tx.send(Ok((rate, channels, "Virtual output".into(), clock)));
                run_virtual(&mut mixer, &shutdown2, capture);
            }
        })?;

        let (rate, channels, device_name, clock) = setup_rx.recv().context("audio thread died")??;
        log::info!("audio output: {device_name} @ {rate} Hz, {channels} ch");
        Ok(Self {
            cmd: cmd_tx,
            thread: handle.thread().clone(),
            pending: VecDeque::new(),
            garbage: gb_rx,
            clock,
            meters,
            shutdown,
            health,
            rate,
            channels,
            device_name,
            device_id: match endpoint {
                Endpoint::Device(id) => id.clone(),
                Endpoint::Virtual { .. } => None,
            },
            endpoint: endpoint.clone(),
            master_db: 0.0,
            voices: HashMap::new(),
            preloaded: HashMap::new(),
            next_id: 1,
            room: crate::spatial::default_room(channels),
        })
    }

    /// The device went away (unplugged, audio server restarted) or the mixer failed: this
    /// output plays nothing any more and should be replaced by a fresh one.
    pub fn is_lost(&self) -> bool {
        self.health.lost.load(Ordering::Acquire)
    }

    /// Buffer underruns/overruns the driver reported so far.
    pub fn xruns(&self) -> u64 {
        self.health.xruns.load(Ordering::Relaxed)
    }

    /// Device callbacks run so far.
    pub fn callbacks(&self) -> u64 {
        self.health.callbacks.load(Ordering::Relaxed)
    }

    /// Mixer load so far (see [`DspLoad::since`] for the load over an interval).
    pub fn dsp_load(&self) -> DspLoad {
        let h = &self.health;
        DspLoad {
            busy: Duration::from_nanos(h.busy_ns.load(Ordering::Relaxed)),
            audio: Duration::from_nanos(h.audio_ns.load(Ordering::Relaxed)),
            peak: h.peak_ppm.load(Ordering::Relaxed) as f64 / 1e6,
        }
    }

    /// Longest buffer the device has asked for at once. A start reaches the mixer only if it
    /// arrives before the callback that renders it, up to this long before its time.
    pub fn block(&self) -> Duration {
        Duration::from_nanos(self.health.block_ns.load(Ordering::Relaxed))
    }

    /// Voices the mixer holds or is about to (playing and preloaded).
    pub fn voice_count(&self) -> usize {
        self.voices.len() + self.preloaded.len()
    }

    fn send(&mut self, cmd: Cmd) {
        // Keep order: once something is waiting, everything queues behind it.
        if self.pending.is_empty() {
            match self.cmd.push(cmd) {
                Ok(()) => {}
                Err(rtrb::PushError::Full(c)) => self.pending.push_back(c),
            }
        } else if self.pending.len() < MAX_PENDING {
            self.pending.push_back(cmd);
        } else {
            log::error!("audio command queue full; command dropped");
        }
    }

    fn flush_pending(&mut self) {
        if self.is_lost() {
            // Nobody will read them.
            self.pending.clear();
            return;
        }
        while let Some(c) = self.pending.pop_front() {
            if let Err(rtrb::PushError::Full(c)) = self.cmd.push(c) {
                self.pending.push_front(c);
                break;
            }
        }
    }

    fn create_voice(&mut self, params: &AudioCue, src_channels: usize, skip: f64) -> VoiceHandle {
        let id = self.next_id;
        self.next_id += 1;
        let src = src_channels.clamp(1, MAX_SRC_CHANNELS);
        let cap = (self.rate as f64 * VOICE_BUFFER_SECS) as usize * src;
        let (tx, rx) = rtrb::RingBuffer::<f32>::new(cap);
        let shared = Arc::new(VoiceShared::default());
        decoder::spawn(
            params.path.clone(),
            DecodeParams {
                segments: params.segments(),
                rate: params.rate as f64,
                channels: src,
                fade_in: params.fade_in,
                fade_out: params.fade_out,
                fade_curve: params.fade_curve,
                skip,
            },
            self.rate,
            tx,
            shared.clone(),
        );
        let mix = MixParams::from(params);
        self.add_voice(id, rx, shared.clone(), &mix, src, VoiceState::Idle);
        VoiceHandle {
            id,
            shared,
            params: Some(params.clone()),
            mix,
            label: params.path.display().to_string(),
            src_channels: src,
            error_reported: false,
            stall_reported: false,
            obj_fade: None,
            obj_sent: None,
            obj_base: None,
            mix_fade: None,
            mix_base: None,
        }
    }

    fn add_voice(&mut self, id: u64, rx: rtrb::Consumer<f32>, shared: Arc<VoiceShared>, mix: &MixParams, src: usize, state: VoiceState) {
        let (eq, eq_bands) = eq_coeffs(&mix.eq, self.rate as f32);
        let voice = Box::new(Voice {
            id,
            rx,
            shared,
            state,
            src_channels: src,
            gain: db_to_gain(mix.volume_db),
            pan: pan_gains(mix.pan),
            matrix: build_matrix(mix, src, self.channels, &self.room),
            matrix_from: [0.0; MAX_SRC_CHANNELS * MAX_OUT_CHANNELS],
            matrix_ramp: 0,
            matrix_ramp_len: 1,
            eq,
            eq_bands,
            eq_state: [[BiquadState::default(); MAX_EQ_BANDS]; MAX_SRC_CHANNELS],
            fx: FxChain::new(&mix.audio_fx, src, self.rate as f32),
            ramp: None,
            stop_after_ramp: false,
            done: false,
        });
        self.send(Cmd::Add(voice));
    }

    /// A voice fed by someone else (a video's soundtrack, a live input). Samples pushed into the
    /// returned producer are interleaved `channels`-channel f32 at the device rate; set `eof` on
    /// the returned state when done. Plays as soon as data arrives. The buffer holds
    /// `buffer_secs` of audio.
    pub fn add_external(
        &mut self,
        cue: CueId,
        mix: &MixParams,
        channels: usize,
        label: &str,
        buffer_secs: f64,
    ) -> (rtrb::Producer<f32>, Arc<VoiceShared>) {
        self.stop(cue);
        let id = self.next_id;
        self.next_id += 1;
        let src = channels.clamp(1, MAX_SRC_CHANNELS);
        let (tx, rx) = rtrb::RingBuffer::<f32>::new(((self.rate as f64 * buffer_secs) as usize).max(1024) * src);
        let shared = Arc::new(VoiceShared::default());
        self.add_voice(id, rx, shared.clone(), mix, src, VoiceState::Playing);
        self.voices.insert(
            cue,
            VoiceHandle {
                id,
                shared: shared.clone(),
                params: None,
                mix: mix.clone(),
                label: label.to_string(),
                src_channels: src,
                error_reported: false,
                stall_reported: false,
                obj_fade: None,
                obj_sent: None,
                obj_base: None,
                mix_fade: None,
                mix_base: None,
            },
        );
        (tx, shared)
    }

    /// Leave the current looping slice of an audio cue after its current pass.
    pub fn devamp(&mut self, cue: CueId) -> bool {
        match self.voices.get(&cue) {
            Some(h) if h.params.is_some() => {
                h.shared.devamp.store(true, Ordering::Release);
                true
            }
            _ => false,
        }
    }

    /// Stop any fade in progress on this cue, keeping the current level and object positions.
    pub fn hold(&mut self, cue: CueId) {
        let Some(h) = self.voices.get_mut(&cue) else { return };
        let id = h.id;
        if let Some(f) = h.obj_fade.take() {
            let (now, _) = f.at(Instant::now());
            h.mix.objects.objects = now;
            let (mix, src) = (h.mix.clone(), h.src_channels);
            self.send_matrix(id, &mix, src);
        }
        if let Some(f) = self.voices.get_mut(&cue).and_then(|h| h.mix_fade.take()) {
            // Hold where it is: the values of this moment become the end of a fade that's over.
            let (t, _) = f.at(Instant::now());
            let levels = f.levels.map(|(a, b)| { let l = lerp_levels(&a, &b, t); (l.clone(), l) });
            let fx = f.fx.map(|(a, b)| { let x = lerp_fx(&a, &b, t); (x.clone(), x) });
            self.voices.get_mut(&cue).expect("voice").mix_fade = Some(MixFade { levels, fx, secs: 0.0, sent: None, ..f });
            self.step_mix_fade(cue, Instant::now(), true);
        }
        self.send(Cmd::Hold(id));
    }

    /// Fade a cue's output faders (dB per output) and/or effect settings to these over `secs`
    /// (a Fade cue). Effects whose kinds differ from the voice's are swapped in at once.
    pub fn fade_mix(&mut self, cue: CueId, out_levels: Option<&[f32]>, fx: Option<&[AudioEffect]>, secs: f64, curve: FadeCurve, at: Instant) {
        let rate = self.rate as f32;
        let Some(h) = self.voices.get_mut(&cue) else { return };
        let (id, src) = (h.id, h.src_channels);
        h.mix_base.get_or_insert_with(|| (h.mix.out_levels.clone(), h.mix.audio_fx.clone()));
        let levels = out_levels.map(|to| (h.mix.out_levels.clone(), to.to_vec()));
        let mut swap = None;
        let fx = fx.map(|to| {
            // Same kinds in the same places blend their settings; the rest switch now.
            let from = &h.mix.audio_fx;
            let start: Vec<AudioEffect> = to
                .iter()
                .enumerate()
                .map(|(i, e)| match from.get(i) {
                    Some(f) if f.kind == e.kind => AudioEffect { enabled: e.enabled, kind: e.kind, params: f.params },
                    _ => *e,
                })
                .collect();
            let shape = |v: &[AudioEffect]| v.iter().map(|e| (e.kind, e.enabled)).collect::<Vec<_>>();
            if shape(&start) != shape(from) {
                swap = Some(FxChain::new(&start, src, rate));
                h.mix.audio_fx = start.clone();
            }
            (start, to.to_vec())
        });
        h.mix_fade = Some(MixFade { levels, fx, start: at, secs: secs.max(0.0), curve, sent: None });
        if let Some(chain) = swap {
            self.send(Cmd::Fx { id, chain });
        }
    }

    /// One step of `cue`'s mix fade, if one is due at `now` (`force`: whatever the step time).
    fn step_mix_fade(&mut self, cue: CueId, now: Instant, force: bool) {
        let Some(h) = self.voices.get_mut(&cue) else { return };
        let Some(f) = &h.mix_fade else { return };
        if !force && (now < f.start || f.sent.is_some_and(|t| now.saturating_duration_since(t) < OBJECT_FADE_STEP)) {
            return;
        }
        let (t, done) = f.at(now);
        let mut matrix = false;
        let mut params = None;
        if let Some((a, b)) = &f.levels {
            let l = lerp_levels(a, b, t);
            matrix = l != h.mix.out_levels;
            h.mix.out_levels = l;
        }
        if let Some((a, b)) = &f.fx {
            let fx = lerp_fx(a, b, t);
            if fx != h.mix.audio_fx {
                params = Some(fx_params(&fx));
                h.mix.audio_fx = fx;
            }
        }
        if let Some(f) = &mut h.mix_fade {
            f.sent = Some(now);
        }
        if done {
            h.mix_fade = None;
        }
        let (id, mix, src) = (h.id, h.mix.clone(), h.src_channels);
        if matrix {
            self.send_matrix(id, &mix, src);
        }
        if let Some(params) = params {
            self.send(Cmd::FxParams { id, params });
        }
    }

    fn send_matrix(&mut self, id: u64, mix: &MixParams, src: usize) {
        let matrix = Box::new(build_matrix(mix, src, self.channels, &self.room));
        self.send(Cmd::Matrix { id, matrix });
    }

    /// Move a cue's objects to `to` over `secs` (a Fade cue on object audio).
    pub fn fade_objects(&mut self, cue: CueId, to: &[AudioObject], secs: f64, curve: FadeCurve, at: Instant) {
        let Some(h) = self.voices.get_mut(&cue) else { return };
        let from = match &h.obj_fade {
            Some(f) => f.at(Instant::now()).0,
            None => h.mix.objects.resolved(h.src_channels),
        };
        h.obj_base.get_or_insert_with(|| h.mix.objects.objects.clone());
        h.obj_fade = Some(ObjectFade { from, to: to.to_vec(), start: at, secs: secs.max(0.0), curve });
        h.obj_sent = None;
    }

    /// The object-audio room: speakers and their outputs (empty = the default layout for this
    /// device). Voices in object mode move to the new layout at once.
    pub fn set_room(&mut self, speakers: &[Speaker]) {
        let room = if speakers.is_empty() { crate::spatial::default_room(self.channels) } else { speakers.to_vec() };
        if room == self.room {
            return;
        }
        self.room = room;
        let moved: Vec<(u64, MixParams, usize)> = self
            .voices
            .values()
            .chain(self.preloaded.values())
            .filter(|h| h.mix.objects.enabled)
            .map(|h| (h.id, h.mix.clone(), h.src_channels))
            .collect();
        for (id, mix, src) in moved {
            self.send_matrix(id, &mix, src);
        }
    }

    /// Advance object-audio fades (new positions go to the mixer every 10 ms).
    pub fn advance_fades(&mut self, now: Instant) {
        let mut send = Vec::new();
        for h in self.voices.values_mut() {
            let Some(f) = &h.obj_fade else { continue };
            if now < f.start || h.obj_sent.is_some_and(|t| now.saturating_duration_since(t) < OBJECT_FADE_STEP) {
                continue;
            }
            let (objs, done) = f.at(now);
            h.mix.objects.objects = objs;
            h.obj_sent = Some(now);
            if done {
                h.obj_fade = None;
            }
            send.push((h.id, h.mix.clone(), h.src_channels));
        }
        for (id, mix, src) in send {
            self.send_matrix(id, &mix, src);
        }
        let fading: Vec<CueId> = self.voices.iter().filter(|(_, h)| h.mix_fade.is_some()).map(|(c, _)| *c).collect();
        for c in fading {
            self.step_mix_fade(c, now, false);
        }
    }

    /// Keep exactly these cues preloaded (buffered and ready to start instantly).
    /// Each entry is (cue, params, source channel count).
    pub fn set_preloaded(&mut self, cues: &[(CueId, &AudioCue, usize)]) {
        let stale: Vec<CueId> = self
            .preloaded
            .iter()
            .filter(|(id, h)| cues.iter().find(|c| c.0 == **id).is_none_or(|c| Some(c.1) != h.params.as_ref()))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(h) = self.preloaded.remove(&id) {
                self.send(Cmd::Stop(h.id));
            }
        }
        for (id, p, src) in cues {
            if !self.preloaded.contains_key(id) && self.voice_count() < MAX_VOICES {
                let h = self.create_voice(p, *src, 0.0);
                self.preloaded.insert(*id, h);
            }
        }
    }

    /// Start an audio cue so its first sample plays at `at` (sample-accurate).
    pub fn start(&mut self, cue: CueId, params: &AudioCue, src_channels: usize, at: Instant) -> Result<(), String> {
        self.start_from(cue, params, src_channels, at, 0.0)
    }

    /// [`AudioOutput::start`] `skip` seconds of file time into the cue's playback (a preloaded
    /// voice, which starts at the beginning, is not used then).
    pub fn start_from(&mut self, cue: CueId, params: &AudioCue, src_channels: usize, at: Instant, skip: f64) -> Result<(), String> {
        if params.path.as_os_str().is_empty() {
            return Err("no audio file".into());
        }
        // Whether the file exists is checked by the caller from the prober's cache (a stat here
        // would run on the engine thread); a missing file shows up as a decoder error.
        self.stop(cue);
        if !self.preloaded.contains_key(&cue) && self.voice_count() >= MAX_VOICES {
            return Err(format!("too many sounds at once (most is {MAX_VOICES})"));
        }
        let handle = match self.preloaded.remove(&cue) {
            Some(h) if skip <= 0.0 && h.params.as_ref() == Some(params) && !h.shared.finished.load(Ordering::Acquire) => h,
            Some(h) => {
                self.send(Cmd::Stop(h.id));
                self.create_voice(params, src_channels, skip)
            }
            None => self.create_voice(params, src_channels, skip),
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

    /// Stop a cue's voice at once.
    pub fn stop(&mut self, cue: CueId) {
        if let Some(h) = self.voices.remove(&cue) {
            self.send(Cmd::Stop(h.id));
        }
    }

    /// Stop every playing voice (preloaded ones stay ready).
    pub fn stop_all(&mut self) {
        let ids: Vec<u64> = self.voices.drain().map(|(_, h)| h.id).collect();
        for id in ids {
            self.send(Cmd::Stop(id));
        }
    }

    /// Pause a cue's voice.
    pub fn pause(&mut self, cue: CueId) {
        if let Some(id) = self.voices.get(&cue).map(|h| h.id) {
            self.send(Cmd::Pause(id));
        }
    }

    /// Resume a paused voice at `at`.
    pub fn resume(&mut self, cue: CueId, at: Instant) {
        if let Some(id) = self.voices.get(&cue).map(|h| h.id) {
            let at = self.clock.frame_at(at);
            self.send(Cmd::Resume { id, at });
        }
    }

    /// Ramp a voice to `to_db` over `secs`.
    pub fn fade(&mut self, cue: CueId, to_db: f32, secs: f64, curve: FadeCurve, at: Instant) {
        if let Some(id) = self.voices.get(&cue).map(|h| h.id) {
            let frames = (secs.max(0.0) * self.rate as f64) as u64;
            let at = self.clock.frame_at(at);
            self.send(Cmd::Ramp { id, to: db_to_gain(to_db), frames, curve, at, stop_after: false });
        }
    }

    /// Apply edits to a playing cue: level (short ramp), pan, EQ and routing take effect now.
    /// (Timing edits like start/end/rate apply the next time the cue starts.)
    pub fn update_live(&mut self, cue: CueId, params: &MixParams) {
        let Some(h) = self.voices.get_mut(&cue) else { return };
        let (id, src) = (h.id, h.src_channels);
        let mut params = params.clone();
        if let Some(base) = &h.obj_base {
            if params.objects.objects == *base {
                // A Fade cue moved the objects and this edit isn't about them: they stay put.
                params.objects.objects = h.mix.objects.objects.clone();
            } else {
                h.obj_fade = None;
                h.obj_base = None;
            }
        }
        if let Some((levels, fx)) = &h.mix_base {
            // A Fade cue took the faders/effects over: an edit of something else leaves them where
            // the fade put them; an edit of them takes them back.
            if params.out_levels == *levels && params.audio_fx == *fx {
                params.out_levels = h.mix.out_levels.clone();
                params.audio_fx = h.mix.audio_fx.clone();
            } else {
                h.mix_fade = None;
                h.mix_base = None;
            }
        }
        let params = &params;
        let old = std::mem::replace(&mut h.mix, params.clone());
        if old.volume_db != params.volume_db {
            let frames = (0.03 * self.rate as f64) as u64;
            self.send(Cmd::Ramp { id, to: db_to_gain(params.volume_db), frames, curve: FadeCurve::Linear, at: 0, stop_after: false });
        }
        if old.pan != params.pan {
            self.send(Cmd::Pan { id, pan: pan_gains(params.pan) });
        }
        if old.eq != params.eq {
            let (coeffs, bands) = eq_coeffs(&params.eq, self.rate as f32);
            self.send(Cmd::Eq { id, coeffs, bands });
        }
        if old.routing != params.routing || old.out_levels != params.out_levels || old.objects != params.objects {
            self.send_matrix(id, params, src);
        }
        if old.audio_fx != params.audio_fx {
            let chain = FxChain::new(&params.audio_fx, src, self.rate as f32);
            self.send(Cmd::Fx { id, chain });
        }
    }

    /// Set the master level in dB.
    pub fn set_master(&mut self, db: f32) {
        self.master_db = db;
        self.send(Cmd::Master(db_to_gain(db)));
    }

    /// True if the cue has a voice in the mixer.
    pub fn has(&self, cue: CueId) -> bool {
        self.voices.contains_key(&cue)
    }

    /// True if the cue isn't playing (finished, errored, or never started).
    pub fn is_finished(&self, cue: CueId) -> bool {
        self.voices.get(&cue).is_none_or(|h| h.shared.finished.load(Ordering::Acquire))
    }

    /// Wall-clock seconds of audio played for this cue.
    pub fn elapsed(&self, cue: CueId) -> Option<f64> {
        self.voices
            .get(&cue)
            .map(|h| h.shared.frames_played.load(Ordering::Relaxed) as f64 / self.rate as f64)
    }

    /// Errors from decoder threads not reported yet.
    pub fn drain_errors(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for h in self.voices.values_mut() {
            if h.error_reported {
                continue;
            }
            if let Some(e) = h.shared.error.lock().clone() {
                h.error_reported = true;
                out.push(format!("{}: {e}", h.label));
            } else if h.shared.rejected.load(Ordering::Acquire) {
                h.error_reported = true;
                out.push(format!("{}: too many sounds at once (most is {MAX_VOICES})", h.label));
            } else if h.params.is_some()
                && !h.stall_reported
                && h.shared.starved.load(Ordering::Relaxed) as f64 > STALL_REPORT_SECS * self.rate as f64
            {
                // Watchdog: the file stopped arriving without an error (a hung network share or
                // a drive spinning up). The voice stays and plays on if the data comes back.
                h.stall_reported = true;
                out.push(format!("{}: the file stopped loading in time (slow or disconnected drive?); the sound has gaps", h.label));
            }
        }
        out
    }

    /// Peak per output channel since the last call (linear 0..1). Each reader (the local
    /// meters, remote replicas) has its own set so they don't steal each other's peaks.
    pub fn take_meters(&self, reader: MeterReader) -> Vec<f32> {
        let set = &self.meters[reader as usize];
        (0..self.channels.min(MAX_METER_CHANNELS))
            .map(|c| f32::from_bits(set[c].swap(0, Ordering::Relaxed)))
            .collect()
    }

    /// A reader of this output's peaks for the light thread.
    pub fn meter_tap(&self) -> MeterTap {
        MeterTap { meters: self.meters.clone(), channels: self.channels.min(MAX_METER_CHANNELS) }
    }

    /// Housekeeping: free retired voices and matrices.
    pub fn update(&mut self) {
        self.flush_pending();
        while let Ok(g) = self.garbage.pop() {
            drop(g);
        }
    }
}

fn build<T>(device: &cpal::Device, config: StreamConfig, mut mixer: Mixer) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let health = mixer.health.clone();
    let mut failed = false;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &OutputCallbackInfo| mixer.fill_guarded(&mut failed, data),
        move |e| {
            use cpal::ErrorKind as K;
            match e.kind() {
                K::Xrun => {
                    health.xruns.fetch_add(1, Ordering::Relaxed);
                }
                K::DeviceNotAvailable | K::HostUnavailable | K::StreamInvalidated => {
                    log::error!("audio device lost: {e}");
                    health.lost.store(true, Ordering::Release);
                }
                _ => log::error!("audio stream error: {e}"),
            }
        },
        None,
    )?;
    Ok(stream)
}

/// Pull the mixer in 256-frame periods paced by the system clock, like a sound card would.
fn run_virtual(mixer: &mut Mixer, shutdown: &AtomicBool, mut capture: Option<rtrb::Producer<f32>>) {
    const PERIOD: usize = 256;
    let rate = mixer.clock.rate;
    let dt = Duration::from_secs_f64(PERIOD as f64 / rate as f64);
    let mut buf = vec![0f32; PERIOD * mixer.channels];
    let mut failed = false;
    let mut next = Instant::now();
    while !shutdown.load(Ordering::Acquire) {
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > Duration::from_millis(200) {
            next = now;
        }
        mixer.fill_guarded(&mut failed, &mut buf);
        if let Some(cap) = &mut capture {
            let n = buf.len().min(cap.slots());
            if let Ok(chunk) = cap.write_chunk_uninit(n) {
                chunk.fill_from_iter(buf[..n].iter().copied());
            }
        }
        next += dt;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Eq, EqBand, EqKind};

    /// The output reports the longest buffer its device asked for (the virtual one pulls 256
    /// frames at a time), which the engine adds to how early it hands starts over.
    #[test]
    fn output_reports_its_block() {
        let out = AudioOutput::open_virtual(48_000, 2, None).unwrap();
        let t = Instant::now();
        while out.block().is_zero() && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(out.block(), Duration::from_nanos(256 * 1_000_000_000 / 48_000));
    }

    /// When the garbage queue is full the audio thread keeps what it retires and hands it over
    /// once there is room, instead of freeing it in the callback.
    #[test]
    fn full_garbage_queue_keeps_garbage_for_later() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::<Cmd>::new(4);
        let (gb_tx, mut gb_rx) = rtrb::RingBuffer::<Garbage>::new(2);
        let mut m = Mixer {
            cmds: cmd_rx,
            garbage: gb_tx,
            overflow: Vec::with_capacity(GARBAGE_OVERFLOW),
            voices: Vec::new(),
            channels: 2,
            frame: 0,
            master: 1.0,
            clock: Arc::new(Clock::new(48_000)),
            filter: ClockFilter::new(48_000),
            meters: new_meters(),
            scratch: Vec::new(),
            health: Arc::new(Health::default()),
        };
        let cap = m.overflow.capacity();
        for _ in 0..5 {
            m.retire(Garbage::Matrix(Box::new([0.0; MAX_SRC_CHANNELS * MAX_OUT_CHANNELS])));
        }
        assert_eq!(m.overflow.len(), 3, "two in the queue, three kept");
        assert_eq!(m.overflow.capacity(), cap, "keeping never allocates");
        while gb_rx.pop().is_ok() {}
        m.handle_commands();
        assert_eq!(m.overflow.len(), 1);
        assert_eq!(gb_rx.slots(), 2, "the queue is full again");
        while gb_rx.pop().is_ok() {}
        m.handle_commands();
        assert!(m.overflow.is_empty());
    }

    #[test]
    fn alsa_list_keeps_one_entry_per_output() {
        let e = |id: &str| DeviceEntry { id: format!("alsa:{id}"), name: id.into() };
        let all = [
            "null", "default", "pipewire", "lavrate", "sysdefault", "sysdefault:CARD=K1", "front:CARD=K1,DEV=0",
            "surround51:CARD=K1,DEV=0", "iec958:CARD=K1,DEV=0", "usbstream:CARD=K1", "hdmi:CARD=NVidia,DEV=0",
            "hw:CARD=9,DEV=0", "plughw:CARD=9,DEV=0", "plughw:CARD=99,DEV=0",
        ]
        .map(e);
        let kept: Vec<String> = alsa_entries(all.to_vec()).into_iter().map(|d| d.id).collect();
        // Card 9 / 99 have no /proc/asound entry here, so their plughw devices stay (nothing covers them).
        assert_eq!(
            kept,
            ["default", "pipewire", "sysdefault:CARD=K1", "hdmi:CARD=NVidia,DEV=0", "plughw:CARD=9,DEV=0", "plughw:CARD=99,DEV=0"]
                .map(|s| format!("alsa:{s}"))
        );
    }

    #[test]
    fn clock_filter_removes_jitter_and_tracks_drift() {
        // A 48 kHz card that really runs 80 ppm fast, 256-frame callbacks woken up to 1.5 ms
        // late at random.
        let (rate, period) = (48_000u32, 256u64);
        let true_rate = rate as f64 * 1.00008;
        let mut rng = fastrand::Rng::with_seed(7);
        let mut f = ClockFilter::new(rate);
        let (mut raw, mut smooth) = (Vec::new(), Vec::new());
        for k in 0..20_000u64 {
            let frame = k * period;
            let truth = frame as f64 / true_rate;
            let measured = truth + rng.f64() * 0.0015;
            let t = f.observe(frame, measured);
            if k > 2_000 {
                raw.push(measured - truth);
                smooth.push(t - truth);
            }
        }
        // A constant offset is just latency; what matters is how much the error wanders.
        let spread = |v: &[f64]| {
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            v.iter().map(|e| (e - mean).abs()).fold(0.0, f64::max)
        };
        let (r, sm) = (spread(&raw), spread(&smooth));
        assert!(sm < 0.00015, "smoothed spread {sm}");
        assert!(sm < r / 4.0, "raw {r} smoothed {sm}");
    }

    #[test]
    fn clock_filter_ignores_one_late_callback_and_reanchors_after_a_gap() {
        let mut f = ClockFilter::new(48_000);
        for k in 0..100u64 {
            f.observe(k * 480, k as f64 * 0.01);
        }
        // One callback 20 ms late: ignored.
        let t = f.observe(100 * 480, 1.0 + 0.02);
        assert!((t - 1.0).abs() < 0.0005, "{t}");
        // The clock really jumped (e.g. after an xrun the card restarted): follow it.
        for k in 101..104u64 {
            f.observe(k * 480, k as f64 * 0.01 + 0.5);
        }
        let t = f.observe(104 * 480, 1.04 + 0.5);
        assert!((t - 1.54).abs() < 0.001, "{t}");
    }

    /// Push `input` (interleaved, `src` channels) into a voice and render it once.
    fn render_once(params: &AudioCue, src: usize, out_ch: usize, input: &[f32]) -> Vec<f32> {
        let (mut tx, rx) = rtrb::RingBuffer::<f32>::new(input.len().max(1));
        for s in input {
            tx.push(*s).unwrap();
        }
        let (eq, eq_bands) = eq_coeffs(&params.eq, 48000.0);
        let mut v = Voice {
            id: 1,
            rx,
            shared: Arc::new(VoiceShared::default()),
            state: VoiceState::Playing,
            src_channels: src,
            gain: db_to_gain(params.volume_db),
            pan: pan_gains(params.pan),
            matrix: build_matrix(&MixParams::from(params), src, out_ch, &crate::spatial::default_room(out_ch)),
            matrix_from: [0.0; MAX_SRC_CHANNELS * MAX_OUT_CHANNELS],
            matrix_ramp: 0,
            matrix_ramp_len: 1,
            eq,
            eq_bands,
            eq_state: [[BiquadState::default(); MAX_EQ_BANDS]; MAX_SRC_CHANNELS],
            fx: FxChain::new(&params.audio_fx, src, 48000.0),
            ramp: None,
            stop_after_ramp: false,
            done: false,
        };
        let frames = input.len() / src;
        let mut out = vec![0.0; frames * out_ch];
        render_voice(&mut v, &mut out, out_ch, frames, 0);
        out
    }

    #[test]
    fn mono_feeds_both_outputs_and_custom_routing_overrides() {
        let p = AudioCue::default();
        let out = render_once(&p, 1, 2, &[0.5; 4]);
        assert!(out.iter().all(|s| (s - 0.5).abs() < 1e-5), "{out:?}");

        // Stereo swapped L/R into a 4-channel device, -6 dB on the crosspoints.
        let p = AudioCue {
            routing: Some(vec![vec![-60.0, -6.0, -60.0, -60.0], vec![-6.0, -60.0, -60.0, -60.0]]),
            ..Default::default()
        };
        let out = render_once(&p, 2, 4, &[1.0, 0.0, 1.0, 0.0]);
        assert!(out[0].abs() < 1e-5);
        assert!((out[1] - 0.501).abs() < 0.01, "{out:?}");
        assert_eq!(out[2], 0.0);
    }

    #[test]
    fn output_faders_scale_their_output_only() {
        // Mono into stereo; output 2 down 6 dB, output 1 untouched (missing entries are 0 dB).
        let p = AudioCue { out_levels: vec![0.0, -6.0], ..Default::default() };
        let out = render_once(&p, 1, 2, &[1.0; 4]);
        assert!((out[0] - 1.0).abs() < 1e-5, "{out:?}");
        assert!((out[1] - 0.501).abs() < 0.01, "{out:?}");
        // At the bottom of the fader the output is silent.
        let p = AudioCue { out_levels: vec![crate::model::MIN_DB], ..Default::default() };
        let out = render_once(&p, 1, 2, &[1.0; 4]);
        assert_eq!(out[0], 0.0);
        assert!((out[1] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn object_audio_places_channels_by_position() {
        // Mono object hard left in a stereo room: nearly all of it on output 1.
        let objects = ObjectAudio { enabled: true, objects: vec![AudioObject { x: -0.8, y: -1.0, spread: 0.0 }] };
        let p = AudioCue { objects, ..Default::default() };
        let out = render_once(&p, 1, 2, &[1.0; 4]);
        assert!(out[0] > 0.99 && out[1] < 0.1, "{out:?}");
        // Output faders still apply on top.
        let p = AudioCue { out_levels: vec![-6.0], ..p };
        let out = render_once(&p, 1, 2, &[1.0; 4]);
        assert!((out[0] - 0.5).abs() < 0.02, "{out:?}");
    }

    #[test]
    fn effects_run_in_the_voice() {
        // A fully wet 1 ms delay: the first 48 frames are silent.
        let mut d = crate::effects::AudioEffect::new(crate::effects::AudioFxKind::Delay);
        d.params = [1.0, 0.0, 20000.0, 100.0, 0.0, 0.0];
        let p = AudioCue { audio_fx: vec![d], ..Default::default() };
        let out = render_once(&p, 1, 1, &[1.0; 200]);
        assert!(out[10].abs() < 1e-6 && (out[150] - 1.0).abs() < 0.01, "{} {}", out[10], out[150]);
    }

    #[test]
    fn a_new_matrix_ramps_in() {
        // A matrix change while playing glides over MATRIX_RAMP_SECS instead of jumping.
        let (mut tx, rx) = rtrb::RingBuffer::<f32>::new(4800);
        for _ in 0..4800 {
            tx.push(1.0).unwrap();
        }
        let p = AudioCue::default();
        let mut v = Voice {
            id: 1,
            rx,
            shared: Arc::new(VoiceShared::default()),
            state: VoiceState::Playing,
            src_channels: 1,
            gain: 1.0,
            pan: pan_gains(0.0),
            matrix: build_matrix(&MixParams::from(&p), 1, 1, &[]),
            matrix_from: [0.0; MAX_SRC_CHANNELS * MAX_OUT_CHANNELS],
            matrix_ramp: 0,
            matrix_ramp_len: 1,
            eq: [Coeffs::IDENTITY; MAX_EQ_BANDS],
            eq_bands: 0,
            eq_state: [[BiquadState::default(); MAX_EQ_BANDS]; MAX_SRC_CHANNELS],
            fx: None,
            ramp: None,
            stop_after_ramp: false,
            done: false,
        };
        // As Cmd::Matrix does: silence the output over 480 frames.
        v.matrix_from = v.matrix;
        v.matrix = [0.0; MAX_SRC_CHANNELS * MAX_OUT_CHANNELS];
        v.matrix_ramp = 480;
        v.matrix_ramp_len = 480;
        let mut out = vec![0.0; 960];
        render_voice(&mut v, &mut out, 1, 960, 0);
        assert!((out[0] - 1.0).abs() < 0.01 && (out[240] - 0.5).abs() < 0.01 && out[600] == 0.0, "{} {} {}", out[0], out[240], out[600]);
    }

    #[test]
    fn object_fades_move_objects_and_stick() {
        let mut a = AudioOutput::open_virtual(48000, 2, None).unwrap();
        let cue = crate::model::CueId::new_v4();
        let mix = MixParams { objects: ObjectAudio { enabled: true, objects: vec![AudioObject::default()] }, ..Default::default() };
        let _feed = a.add_external(cue, &mix, 1, "test", 0.1);
        let t0 = Instant::now();
        let to = [AudioObject { x: 1.0, y: 0.0, spread: 0.0 }];
        a.fade_objects(cue, &to, 1.0, FadeCurve::Linear, t0);
        a.advance_fades(t0 + Duration::from_millis(500));
        let x = a.voices[&cue].mix.objects.objects[0].x;
        assert!((x - 0.5).abs() < 0.01, "{x}");
        a.advance_fades(t0 + Duration::from_millis(1100));
        assert_eq!(a.voices[&cue].mix.objects.objects, to.to_vec());
        assert!(a.voices[&cue].obj_fade.is_none());
        // An edit that doesn't touch the objects keeps the faded positions; moving them wins.
        a.update_live(cue, &MixParams { volume_db: -3.0, ..mix.clone() });
        assert_eq!(a.voices[&cue].mix.objects.objects, to.to_vec());
        let moved = MixParams { objects: ObjectAudio { enabled: true, objects: vec![AudioObject { x: -1.0, ..Default::default() }] }, ..mix };
        a.update_live(cue, &moved);
        assert_eq!(a.voices[&cue].mix.objects.objects[0].x, -1.0);
    }

    /// A Fade cue glides output faders and effect settings, holds them when paused, and they stick
    /// through edits of other settings.
    #[test]
    fn output_fader_and_effect_fades() {
        use crate::effects::{AudioEffect, AudioFxKind};
        let mut a = AudioOutput::open_virtual(48000, 2, None).unwrap();
        let cue = crate::model::CueId::new_v4();
        let delay = AudioEffect::new(AudioFxKind::Delay);
        let mix = MixParams { out_levels: vec![0.0, 0.0], audio_fx: vec![delay], ..Default::default() };
        let _feed = a.add_external(cue, &mix, 1, "test", 0.1);
        // Started half a second ago: holding (which reads the clock) freezes it about half way.
        let t0 = Instant::now() - Duration::from_millis(500);
        let mut longer = delay;
        longer.params[0] = delay.params[0] + 100.0;
        a.fade_mix(cue, Some(&[-20.0, 0.0]), Some(std::slice::from_ref(&longer)), 1.0, FadeCurve::Linear, t0);
        a.advance_fades(t0 + Duration::from_millis(500));
        let h = &a.voices[&cue];
        assert!((h.mix.out_levels[0] + 10.0).abs() < 0.1, "{:?}", h.mix.out_levels);
        assert!((h.mix.audio_fx[0].params[0] - delay.params[0] - 50.0).abs() < 0.5);
        a.hold(cue);
        let held = a.voices[&cue].mix.out_levels[0];
        assert!((held + 10.0).abs() < 1.0, "{held}");
        a.advance_fades(t0 + Duration::from_millis(900));
        assert_eq!(a.voices[&cue].mix.out_levels[0], held, "held");
        a.fade_mix(cue, Some(&[-20.0, 0.0]), None, 0.0, FadeCurve::Linear, t0);
        a.advance_fades(t0 + Duration::from_millis(1000));
        assert_eq!(a.voices[&cue].mix.out_levels, vec![-20.0, 0.0]);
        a.update_live(cue, &MixParams { volume_db: -3.0, ..mix.clone() });
        assert_eq!(a.voices[&cue].mix.out_levels, vec![-20.0, 0.0], "stays where the fade put it");
        a.update_live(cue, &MixParams { out_levels: vec![-6.0, 0.0], ..mix });
        assert_eq!(a.voices[&cue].mix.out_levels, vec![-6.0, 0.0], "an edit of the faders wins");
    }

    #[test]
    fn eq_is_applied_in_the_voice() {
        // A -40 dB low shelf nearly removes DC.
        let eq = Eq {
            enabled: true,
            bands: vec![EqBand { enabled: true, kind: EqKind::LowShelf, freq: 1000.0, gain_db: -40.0, q: 0.707 }],
        };
        let p = AudioCue { eq, ..Default::default() };
        let out = render_once(&p, 1, 1, &[1.0; 4800]);
        assert!(out.last().unwrap().abs() < 0.02, "{}", out.last().unwrap());
    }
}
