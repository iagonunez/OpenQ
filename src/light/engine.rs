//! The light thread: runs [`Playback`] at [`RATE`] and hands each frame to the DMX outputs.
//!
//! It shares no lock with the OpnQ engine, the UI or the audio callback: commands arrive on a
//! channel and are applied at the next frame (their timestamps keep fades exact), the latest
//! frame and sequence states are published with `try_lock`, so a reader never stalls it.
//! A panic inside a frame is caught: the last good frame keeps going out and it's reported once.

use super::dmx::{DmxFrame, OutputConfig, OutputHandle, OutputSender};
use super::dmxin::{InputConfig, InputHandle, InputReceiver, MergeMode};
use super::model::{Attribute, FixtureId, LightCueId, Lighting, SequenceId};
use super::playback::{LightShow, Playback, SequenceStatus};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Frames per second (the DMX refresh rate of a full universe).
pub const RATE: f64 = 44.0;
/// Commands wait at most this long for a recompiled show.
const HOLD_LIMIT: Duration = Duration::from_secs(5);
/// Name of the light thread.
pub const THREAD: &str = "opnq-light";

/// A release fade as given by OSC, a surface or a replica, made safe: not negative, at most an
/// hour, and 0 for NaN.
fn fade_time(secs: f64) -> f64 {
    if secs.is_nan() { 0.0 } else { secs.clamp(0.0, 3600.0) }
}

/// A command for the light thread.
pub enum LightCommand {
    /// A recompile started: hold GOs until its show arrives (keeps their order with the edit).
    Compiling(u64),
    /// Swap in a recompiled show.
    Show(u64, Arc<LightShow>),
    /// A compile failed (panicked): keep the current show, stop holding commands for it.
    Failed(u64),
    /// GO a lighting cue wherever it is in the design.
    GoCue {
        /// Cue.
        cue: LightCueId,
        /// When.
        at: Instant,
    },
    /// GO a lighting cue (None = next in the sequence) at a moment (may be slightly ahead).
    Go {
        /// Sequence.
        seq: SequenceId,
        /// Cue.
        cue: Option<LightCueId>,
        /// When.
        at: Instant,
    },
    /// Fade a sequence out.
    Release {
        /// Sequence.
        seq: SequenceId,
        /// Seconds.
        fade: f64,
        /// When.
        at: Instant,
    },
    /// Fade every sequence out.
    ReleaseAll {
        /// Seconds.
        fade: f64,
        /// When.
        at: Instant,
    },
    /// Sequence master level.
    Level(SequenceId, f32),
    /// Grand master.
    GrandMaster(f32),
    /// Blackout on/off.
    Blackout(bool),
    /// Programmer values (replace).
    Programmer(Vec<(FixtureId, Attribute, f32)>),
    /// Programmer effects (replace).
    ProgrammerEffects(Vec<(crate::light::effects::Effect, Vec<FixtureId>)>),
    /// Tempo for effects that follow it, in BPM.
    Tempo(f32),
    /// Replace the outputs.
    Outputs(Vec<OutputSender>),
    /// Replace the inputs merged into the output.
    Inputs(Vec<InputHandle>),
}

/// Grand master, blackout and sequence levels as set through a [`LightEngine`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Masters {
    /// Grand master, 0..=1.
    pub grand_master: f32,
    /// Blackout on.
    pub blackout: bool,
    /// Sequence levels (missing = full).
    pub levels: std::collections::HashMap<SequenceId, f32>,
    /// Effects tempo, BPM.
    pub bpm: f32,
}

impl Default for Masters {
    fn default() -> Self {
        Self { grand_master: 1.0, blackout: false, levels: Default::default(), bpm: 120.0 }
    }
}

impl Masters {
    /// Level of a sequence.
    pub fn level(&self, seq: SequenceId) -> f32 {
        self.levels.get(&seq).copied().unwrap_or(1.0)
    }
}

#[derive(Default)]
struct Shared {
    frame: Mutex<DmxFrame>,
    status: Mutex<Vec<SequenceStatus>>,
    errors: Mutex<Vec<String>>,
    frames: AtomicU64,
    faults: AtomicU64,
    /// Set when the engine drops: the compile thread may still hold a sender, so the channel
    /// alone wouldn't end the light thread until a long compile finishes.
    quit: std::sync::atomic::AtomicBool,
    /// The audio output's meters, for audio-reactive effects.
    audio: Mutex<Option<crate::media::audio::MeterTap>>,
    /// Audio envelope of the last frame (`f32` bits), for the UI.
    audio_level: std::sync::atomic::AtomicU32,
}

/// Owns the light thread. Dropping it stops the thread (outputs close).
pub struct LightEngine {
    tx: mpsc::Sender<LightCommand>,
    /// Designs to compile, for the compile thread (started on the first load).
    compile_tx: Option<mpsc::Sender<(u64, Lighting)>>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    compiles: u64,
    outputs: Vec<OutputConfig>,
    masters: Masters,
    handles: Vec<OutputHandle>,
    inputs: Vec<InputConfig>,
    /// Running receivers (they stop when dropped).
    receivers: Vec<InputReceiver>,
    /// On a replica: the main device's sequence states (see [`LightEngine::mirror`]).
    mirrored: Option<Vec<SequenceStatus>>,
    /// On a replica: the main device's DMX output (see [`LightEngine::mirror_frame`]).
    mirrored_frame: Option<DmxFrame>,
}

impl LightEngine {
    /// Start the light thread with an empty show and no outputs.
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        let thread = std::thread::Builder::new().name(THREAD.into()).spawn(move || run(rx, s)).ok();
        Self {
            tx,
            compile_tx: None,
            shared,
            thread,
            compiles: 0,
            outputs: Vec::new(),
            handles: Vec::new(),
            masters: Masters::default(),
            inputs: Vec::new(),
            receivers: Vec::new(),
            mirrored: None,
            mirrored_frame: None,
        }
    }

    fn send(&self, cmd: LightCommand) {
        let _ = self.tx.send(cmd);
    }

    /// Compile a design (on the compile thread, so a big design never holds up the caller) and
    /// hand it to the light thread; restart the outputs if they changed. Commands sent after
    /// this wait for the new show. Designs that arrive while one compiles are coalesced: only
    /// the newest is compiled next, so a stream of edits (a drag) never piles up compiles.
    pub fn load(&mut self, lighting: &Lighting) {
        self.compiles += 1;
        let generation = self.compiles;
        self.send(LightCommand::Compiling(generation));
        if self.compile_tx.is_none() {
            self.compile_tx = start_compiler(self.tx.clone());
        }
        // Cheap: the big parts of a design are shared (`model::Shared`).
        let sent = self.compile_tx.as_ref().is_some_and(|c| c.send((generation, lighting.clone())).is_ok());
        if !sent {
            log::error!("lighting: the compile thread isn't running");
            self.compile_tx = None;
            self.send(LightCommand::Failed(generation));
        }
        self.load_io(lighting);
    }

    /// Restart the outputs and inputs if they changed (no recompile: see [`LightShow::same_input`]).
    pub fn load_io(&mut self, lighting: &Lighting) {
        if lighting.outputs != self.outputs {
            self.set_outputs(lighting.outputs.clone());
        }
        if lighting.inputs != self.inputs {
            self.set_inputs(lighting.inputs.clone());
        }
    }

    /// Replace the inputs (disabled ones don't listen).
    pub fn set_inputs(&mut self, inputs: Vec<InputConfig>) {
        let receivers: Vec<InputReceiver> = inputs.iter().filter(|i| i.enabled).cloned().map(InputReceiver::start).collect();
        self.set_receivers(receivers);
        self.inputs = inputs;
    }

    /// Replace the inputs with ready receivers (tests).
    pub fn set_receivers(&mut self, receivers: Vec<InputReceiver>) {
        self.send(LightCommand::Inputs(receivers.iter().map(|r| r.handle()).collect()));
        // Dropping a receiver waits for its thread to notice (a fraction of a second): not here.
        let old = std::mem::replace(&mut self.receivers, receivers);
        if !old.is_empty() {
            let _ = std::thread::Builder::new().name("opnq-dmx-close".into()).spawn(move || drop(old));
        }
    }

    /// Inputs and their status.
    pub fn inputs(&self) -> Vec<InputHandle> {
        self.receivers.iter().map(|r| r.handle()).collect()
    }

    /// Everything the inputs receive, merged highest-wins (whatever their merge mode): what
    /// "Capture input" reads.
    pub fn input_frame(&self) -> DmxFrame {
        let mut f = DmxFrame::default();
        for r in &self.receivers {
            super::dmxin::merge(&mut f, &r.handle().frame(), MergeMode::Htp);
        }
        f
    }

    /// Replace the outputs (disabled ones aren't opened).
    pub fn set_outputs(&mut self, outputs: Vec<OutputConfig>) {
        let senders: Vec<OutputSender> = outputs.iter().filter(|o| o.enabled).cloned().map(OutputSender::start).collect();
        self.set_senders(senders);
        self.outputs = outputs;
    }

    /// Replace the outputs with ready senders (tests, virtual outputs).
    pub fn set_senders(&mut self, senders: Vec<OutputSender>) {
        self.handles = senders.iter().map(|s| s.handle()).collect();
        self.send(LightCommand::Outputs(senders));
    }

    /// GO a lighting cue (`cue` None = next in the sequence).
    pub fn go(&self, seq: SequenceId, cue: Option<LightCueId>, at: Instant) {
        self.send(LightCommand::Go { seq, cue, at });
    }

    /// GO a lighting cue wherever it is (a missing cue is reported through
    /// [`LightEngine::drain_errors`]).
    pub fn go_cue(&self, cue: LightCueId, at: Instant) {
        self.send(LightCommand::GoCue { cue, at });
    }

    /// Fade a sequence out.
    pub fn release(&self, seq: SequenceId, fade: f64, at: Instant) {
        self.send(LightCommand::Release { seq, fade: fade_time(fade), at });
    }

    /// Fade every sequence out.
    pub fn release_all(&self, fade: f64, at: Instant) {
        self.send(LightCommand::ReleaseAll { fade: fade_time(fade), at });
    }

    /// Sequence master level.
    pub fn set_level(&mut self, seq: SequenceId, level: f32) {
        if !level.is_finite() {
            return;
        }
        let level = level.clamp(0.0, 1.0);
        self.masters.levels.insert(seq, level);
        self.send(LightCommand::Level(seq, level));
    }

    /// Grand master.
    pub fn set_grand_master(&mut self, level: f32) {
        if !level.is_finite() {
            return;
        }
        self.masters.grand_master = level.clamp(0.0, 1.0);
        self.send(LightCommand::GrandMaster(self.masters.grand_master));
    }

    /// Blackout.
    pub fn set_blackout(&mut self, on: bool) {
        self.masters.blackout = on;
        self.send(LightCommand::Blackout(on));
    }

    /// Replace the programmer's effects.
    pub fn set_programmer_effects(&self, effects: Vec<(crate::light::effects::Effect, Vec<FixtureId>)>) {
        self.send(LightCommand::ProgrammerEffects(effects));
    }

    /// Tempo for effects that follow it.
    pub fn set_tempo(&mut self, bpm: f32) {
        if !bpm.is_finite() {
            return;
        }
        self.masters.bpm = bpm.clamp(1.0, 999.0);
        self.send(LightCommand::Tempo(self.masters.bpm));
    }

    /// Master levels as last set.
    pub fn masters(&self) -> &Masters {
        &self.masters
    }

    /// Replace the programmer.
    pub fn set_programmer(&self, values: Vec<(FixtureId, Attribute, f32)>) {
        self.send(LightCommand::Programmer(values));
    }

    /// Copy of the latest frame (DMX monitor, visualiser); the main device's on a replica that
    /// receives it.
    pub fn frame(&self) -> DmxFrame {
        match &self.mirrored_frame {
            Some(f) => f.clone(),
            None => self.shared.frame.lock().clone(),
        }
    }

    /// Take universes of another device's output (a replica's DMX monitor). An empty list
    /// stops mirroring. Slices shorter than a universe are zero-filled.
    pub fn mirror_frame(&mut self, universes: &[(u16, Vec<u8>)]) {
        if universes.is_empty() {
            self.mirrored_frame = None;
            return;
        }
        let f = self.mirrored_frame.get_or_insert_with(DmxFrame::default);
        for (id, data) in universes {
            let u = f.get_mut(*id);
            let n = data.len().min(u.data.len());
            u.data[..n].copy_from_slice(&data[..n]);
            u.data[n..].fill(0);
        }
    }

    /// Give the light thread the audio output's meters (None: no output) for audio-reactive
    /// effects. [`crate::media::Media`] calls it whenever the output opens or reopens.
    pub fn set_audio(&self, tap: Option<crate::media::audio::MeterTap>) {
        *self.shared.audio.lock() = tap;
    }

    /// The audio envelope audio-reactive effects follow (0..1).
    pub fn audio_level(&self) -> f32 {
        f32::from_bits(self.shared.audio_level.load(Ordering::Relaxed))
    }

    /// Sequence states (the main device's on a replica).
    pub fn status(&self) -> Vec<SequenceStatus> {
        match &self.mirrored {
            Some(m) => m.clone(),
            None => self.shared.status.lock().clone(),
        }
    }

    /// Show another device's masters and sequence states instead of this engine's (a replica:
    /// its executors are the main device's). Nothing is played here.
    pub fn mirror(&mut self, masters: Masters, sequences: Vec<SequenceStatus>) {
        self.masters = masters;
        self.mirrored = Some(sequences);
    }

    /// Stop mirroring (back to this engine's own state).
    pub fn clear_mirror(&mut self) {
        self.mirrored_frame = None;
        if self.mirrored.take().is_some() {
            self.masters = Masters::default();
            self.send(LightCommand::GrandMaster(1.0));
            self.send(LightCommand::Blackout(false));
            self.send(LightCommand::Tempo(self.masters.bpm));
        }
    }

    /// Outputs and their status.
    pub fn outputs(&self) -> &[OutputHandle] {
        &self.handles
    }

    /// Frames built so far.
    pub fn frames(&self) -> u64 {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// Errors (GO on a missing cue, faults) since the last call.
    pub fn drain_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.shared.errors.lock())
    }
}

/// The compile thread: compiles the newest queued design, reports a panic as `Failed`. It ends
/// when the engine drops its sender (not joined: a long compile must not hold up the drop).
fn start_compiler(tx: mpsc::Sender<LightCommand>) -> Option<mpsc::Sender<(u64, Lighting)>> {
    let (ctx, crx) = mpsc::channel::<(u64, Lighting)>();
    let spawned = std::thread::Builder::new().name("opnq-light-compile".into()).spawn(move || {
        // The last design compiled and its show: unchanged sequences are kept from it.
        let mut prev: Option<(Lighting, Arc<LightShow>)> = None;
        while let Ok(mut job) = crx.recv() {
            while let Ok(newer) = crx.try_recv() {
                job = newer;
            }
            let (generation, design) = job;
            let from = prev.as_ref().map(|(l, s)| (l, &**s));
            let cmd = match crate::recovery::catch(|| Ok(Arc::new(LightShow::compile_from(&design, from)))) {
                Ok(show) => {
                    prev = Some((design, show.clone()));
                    LightCommand::Show(generation, show)
                }
                Err(e) => {
                    log::error!("lighting: compiling the design failed: {e:#}");
                    prev = None;
                    LightCommand::Failed(generation)
                }
            };
            if tx.send(cmd).is_err() {
                return;
            }
        }
    });
    match spawned {
        Ok(_) => Some(ctx),
        Err(e) => {
            log::error!("lighting: can't start the compile thread: {e}");
            None
        }
    }
}

impl Drop for LightEngine {
    fn drop(&mut self) {
        // Closing the channels ends the threads.
        self.shared.quit.store(true, Ordering::Relaxed);
        self.compile_tx = None;
        let (tx, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.tx, tx));
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

fn run(rx: mpsc::Receiver<LightCommand>, shared: Arc<Shared>) {
    let period = Duration::from_secs_f64(1.0 / RATE);
    let mut playback = Playback::new(Arc::new(LightShow::default()), Instant::now());
    let mut outputs: Vec<OutputSender> = Vec::new();
    let mut last_good = DmxFrame::default();
    // Inputs, the copy of each one's universes and the output with them merged.
    let mut inputs: Vec<(InputHandle, DmxFrame)> = Vec::new();
    let mut merged = DmxFrame::default();
    let mut next = Instant::now();
    let mut n: u64 = 0;
    // Latest compile announced, newest show applied. A command that needs the design (GO,
    // release, programmer) waits for the compile announced before it, not for later ones, so a
    // stream of edits delays it by one compile at most; masters never wait.
    let (mut expected, mut applied) = (0u64, 0u64);
    let mut held: std::collections::VecDeque<(u64, LightCommand)> = std::collections::VecDeque::new();
    let mut held_since: Option<Instant> = None;
    // Sequence levels as last set: a level for a sequence the current show doesn't have yet
    // (it's being compiled) applies when the show arrives.
    let mut levels: std::collections::HashMap<SequenceId, f32> = std::collections::HashMap::new();
    loop {
        // Commands, until the frame is due.
        let mut quit = false;
        loop {
            let now = Instant::now();
            let cmd = if now >= next {
                rx.try_recv()
            } else {
                rx.recv_timeout(next - now).map_err(|e| match e {
                    mpsc::RecvTimeoutError::Timeout => mpsc::TryRecvError::Empty,
                    mpsc::RecvTimeoutError::Disconnected => mpsc::TryRecvError::Disconnected,
                })
            };
            match cmd {
                Ok(LightCommand::Outputs(o)) => {
                    let old = std::mem::replace(&mut outputs, o);
                    // A stuck device could make dropping slow: not on this thread.
                    if !old.is_empty() {
                        let _ = std::thread::Builder::new().name("opnq-dmx-close".into()).spawn(move || drop(old));
                    }
                }
                Ok(LightCommand::Inputs(i)) => inputs = i.into_iter().filter(|h| h.merge != MergeMode::Capture).map(|h| (h, DmxFrame::default())).collect(),
                Ok(LightCommand::Compiling(g)) => expected = expected.max(g),
                Ok(LightCommand::Show(g, show)) => {
                    // Compiles can finish out of order: never go back to an older one.
                    if g > applied {
                        applied = g;
                        run_command(&mut playback, LightCommand::Show(g, show), &shared);
                        for (seq, l) in &levels {
                            playback.set_level(*seq, *l);
                        }
                    }
                    release_held(&mut playback, &mut held, &mut held_since, applied, &shared);
                }
                Ok(LightCommand::Failed(g)) => {
                    push_error(&shared, "lighting: the design could not be compiled (see the error log)".into());
                    applied = applied.max(g);
                    release_held(&mut playback, &mut held, &mut held_since, applied, &shared);
                }
                Ok(LightCommand::Level(seq, l)) => {
                    levels.insert(seq, l);
                    playback.set_level(seq, l);
                }
                Ok(cmd @ (LightCommand::GrandMaster(_) | LightCommand::Blackout(_) | LightCommand::Tempo(_))) => run_command(&mut playback, cmd, &shared),
                Ok(cmd) if applied < expected || !held.is_empty() => {
                    held_since.get_or_insert_with(Instant::now);
                    // Only the newest programmer matters: don't queue one per UI frame. The
                    // replaced entry waits for the newer compile (entries behind it wait too,
                    // so the order is kept).
                    let replaces = |a: &LightCommand, b: &LightCommand| {
                        matches!((a, b), (LightCommand::Programmer(_), LightCommand::Programmer(_)) | (LightCommand::ProgrammerEffects(_), LightCommand::ProgrammerEffects(_)))
                    };
                    match held.iter_mut().rev().find(|h| replaces(&h.1, &cmd)) {
                        Some(h) => *h = (expected, cmd),
                        None => held.push_back((expected, cmd)),
                    }
                }
                Ok(cmd) => run_command(&mut playback, cmd, &shared),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    quit = true;
                    break;
                }
            }
        }
        if quit || shared.quit.load(Ordering::Relaxed) {
            break;
        }
        if held_since.is_some_and(|t| t.elapsed() > HOLD_LIMIT) {
            // The compile is taking far too long: don't hold GOs any more.
            expected = applied;
            release_held(&mut playback, &mut held, &mut held_since, u64::MAX, &shared);
        }
        let now = Instant::now();
        if let Some(a) = shared.audio.try_lock()
            && let Some(tap) = a.as_ref()
        {
            playback.set_audio(tap.take_peak(), now);
            shared.audio_level.store(playback.audio_level().to_bits(), Ordering::Relaxed);
        }
        let built = crate::recovery::catch(|| {
            crate::recovery::fault_point(crate::recovery::Fault::LightTick);
            last_good.universes.clone_from(&playback.tick(now).universes);
            Ok(())
        });
        if let Err(e) = built {
            // Keep sending the last good frame; report the first fault of a run only.
            if shared.faults.fetch_add(1, Ordering::Relaxed) == 0 {
                log::error!("lighting frame failed, holding the last frame: {e:#}");
                push_error(&shared, format!("lighting: {e:#}"));
            }
        }
        let out = if inputs.is_empty() {
            &last_good
        } else {
            merged.universes.clone_from(&last_good.universes);
            for (h, copy) in &mut inputs {
                h.read_into(copy);
                super::dmxin::merge(&mut merged, copy, h.merge);
            }
            &merged
        };
        if crate::overrides::allowed(crate::overrides::Override::LightOut) {
            for o in &outputs {
                o.publish(out);
            }
        }
        if let Some(mut f) = shared.frame.try_lock() {
            f.clone_from(out);
        }
        n += 1;
        shared.frames.store(n, Ordering::Relaxed);
        // Sequence states for the UI, ~10 times a second.
        if n.is_multiple_of(4)
            && let Some(mut s) = shared.status.try_lock()
        {
            playback.status_into(&mut s);
        }
        next += period;
        let now = Instant::now();
        if now > next + Duration::from_millis(100) {
            next = now; // far behind (suspend, debugger): don't catch up
        }
    }
}

/// Run the held commands whose compile has been applied (in order: one still waiting holds up
/// those behind it).
fn release_held(p: &mut Playback, held: &mut std::collections::VecDeque<(u64, LightCommand)>, since: &mut Option<Instant>, applied: u64, shared: &Shared) {
    let before = held.len();
    while held.front().is_some_and(|h| h.0 <= applied) {
        let (_, cmd) = held.pop_front().expect("checked");
        run_command(p, cmd, shared);
    }
    if held.is_empty() {
        *since = None;
    } else if held.len() < before {
        *since = Some(Instant::now());
    }
}

/// Errors kept until drained (a replica never drains them: its engine doesn't tick).
const MAX_ERRORS: usize = 100;

fn run_command(p: &mut Playback, cmd: LightCommand, shared: &Shared) {
    let r = crate::recovery::catch(|| apply(p, cmd).map_err(anyhow::Error::msg));
    if let Err(e) = r {
        push_error(shared, format!("lighting: {e:#}"));
    }
}

fn push_error(shared: &Shared, e: String) {
    let mut errors = shared.errors.lock();
    if errors.len() < MAX_ERRORS {
        errors.push(e);
    }
}

fn apply(p: &mut Playback, cmd: LightCommand) -> Result<(), String> {
    match cmd {
        LightCommand::Show(_, s) => p.set_show(s),
        LightCommand::Go { seq, cue, at } => match p.go(seq, cue, at) {
            // GO past the last cue does nothing; not worth a warning.
            Err(e) if cue.is_none() && e == crate::light::playback::END_OF_SEQUENCE => {}
            r => {
                r?;
            }
        },
        LightCommand::GoCue { cue, at } => {
            let (si, _) = p.show().find_cue(cue).ok_or("lighting cue not found")?;
            let seq = p.show().sequences[si].id;
            p.go(seq, Some(cue), at)?;
        }
        LightCommand::Release { seq, fade, at } => p.release(seq, fade, at),
        LightCommand::ReleaseAll { fade, at } => p.release_all(fade, at),
        LightCommand::Level(seq, l) => p.set_level(seq, l),
        LightCommand::GrandMaster(l) => p.grand_master = l,
        LightCommand::Blackout(b) => p.blackout = b,
        LightCommand::Programmer(v) => p.set_programmer(&v),
        LightCommand::ProgrammerEffects(v) => p.set_programmer_effects(&v),
        LightCommand::Tempo(bpm) => p.bpm = bpm.clamp(1.0, 999.0),
        LightCommand::Outputs(_) | LightCommand::Inputs(_) | LightCommand::Compiling(_) | LightCommand::Failed(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::dmx::{DmxOutput, Universe, UniverseRoute};
    use crate::light::library;
    use crate::light::model::{Fixture, LightCue, Sequence};

    struct Probe(Arc<Mutex<u8>>);
    impl DmxOutput for Probe {
        fn send(&mut self, u: &Universe) -> anyhow::Result<()> {
            *self.0.lock() = u.data[0];
            Ok(())
        }
    }

    fn wait(f: impl FnMut() -> bool) -> bool {
        wait_for(Duration::from_secs(2), f)
    }

    fn wait_for(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < limit {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// A replica shows the main device's masters, sequence states and output until it stops.
    #[test]
    fn mirror_and_clear() {
        let mut e = LightEngine::start();
        let st = SequenceStatus { id: 4, cue: None, active: true, level: 0.5 };
        e.mirror(Masters { grand_master: 0.3, ..Default::default() }, vec![st.clone()]);
        e.mirror_frame(&[(2, vec![9; 3])]);
        assert_eq!(e.status(), [st]);
        assert_eq!(e.masters().grand_master, 0.3);
        let f = e.frame();
        assert_eq!(f.get(2).unwrap().data[..4], [9, 9, 9, 0]);
        e.clear_mirror();
        assert!(e.status().is_empty() && e.frame().universes.is_empty());
        assert_eq!(e.masters().grand_master, 1.0);
    }

    /// An Art-Net input merged HTP reaches the output; in Capture mode it doesn't.
    #[test]
    fn input_merges_into_the_output() {
        use crate::light::dmxin::{InputConfig, InputReceiver, MergeMode};
        let mut e = LightEngine::start();
        let seen = Arc::new(Mutex::new(0u8));
        let s2 = seen.clone();
        let cfg = OutputConfig { routes: vec![UniverseRoute { universe: 2, wire: 0 }], ..Default::default() };
        e.set_senders(vec![OutputSender::with_opener(cfg, Box::new(move || Ok(Box::new(Probe(s2.clone())) as Box<dyn DmxOutput>)))]);
        // Sender first, so the probed port can't be handed to it as well (macOS reuses them quickly).
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = loop {
            let p = std::net::UdpSocket::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port();
            if p != tx.local_addr().unwrap().port() {
                break p;
            }
        };
        let send = |v: u8| {
            let mut u = Universe::new(4);
            u.data[0] = v;
            let mut p = Vec::new();
            crate::light::artnet::dmx_packet(1, &u, &mut p);
            tx.send_to(&p, ("127.0.0.1", port)).unwrap();
        };
        for (merge, expect) in [(MergeMode::Capture, 0), (MergeMode::Htp, 77)] {
            // The previous receiver still holds the port (macOS refuses a second bind); closing is retried below.
            e.set_receivers(Vec::new());
            let input = InputConfig { routes: vec![UniverseRoute { universe: 2, wire: 4 }], merge, ..Default::default() };
            let r = InputReceiver::start_on(input, port);
            let h = r.handle();
            // A bind that lost a race for the port retries after RETRY (2 s), longer than `wait` allows.
            assert!(wait_for(Duration::from_secs(6), || h.status().open), "input never opened: {:?}", h.status().error);
            e.set_receivers(vec![r]);
            assert!(wait(|| {
                send(77);
                e.input_frame().get(2).is_some_and(|u| u.data[0] == 77)
            }));
            std::thread::sleep(Duration::from_millis(100));
            assert!(wait(|| *seen.lock() == expect), "{merge:?}");
        }
    }

    #[test]
    fn go_reaches_the_output() {
        let mut l = Lighting::default();
        let t = library::generic("generic/dimmer").unwrap();
        l.add_type(&t);
        l.fixtures.push(Fixture { id: 1, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), address: 1, ..Default::default() });
        let mut c = LightCue { fade: 0.0, ..Default::default() };
        c.values.entry(1).or_default().insert(Attribute::Dimmer, 1.0.into());
        let id = c.id;
        l.sequences.push(Sequence { id: 1, cues: vec![c].into(), ..Default::default() });

        let mut e = LightEngine::start();
        e.load(&l);
        let seen = Arc::new(Mutex::new(0u8));
        let s2 = seen.clone();
        let cfg = OutputConfig { routes: vec![UniverseRoute { universe: 1, wire: 0 }], ..Default::default() };
        e.set_senders(vec![OutputSender::with_opener(cfg, Box::new(move || Ok(Box::new(Probe(s2.clone())) as Box<dyn DmxOutput>)))]);
        e.go_cue(id, Instant::now());
        assert!(wait(|| *seen.lock() == 255), "GO reached the device");
        assert!(wait(|| e.status().first().is_some_and(|s| s.cue == Some(id))));
        assert_eq!(e.frame().get(1).unwrap().data[0], 255);
        e.set_blackout(true);
        assert!(wait(|| *seen.lock() == 0));
        e.go_cue(uuid::Uuid::new_v4(), Instant::now());
        assert!(wait(|| e.drain_errors().iter().any(|m| m.contains("not found"))), "unknown cue reported");
        e.go(9, None, Instant::now());
        assert!(wait(|| !e.drain_errors().is_empty()), "unknown sequence reported");
        assert!(e.frames() > 0);
    }

    /// Blackout never waits for a compile, and a GO waits only for the compile announced before
    /// it, not for edits that keep coming after it.
    #[test]
    fn masters_never_wait_and_gos_wait_one_compile() {
        let t = library::generic("generic/dimmer").unwrap();
        let mut l = Lighting::default();
        l.add_type(&t);
        l.fixtures.push(Fixture { id: 1, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), address: 1, ..Default::default() });
        let mut c = LightCue { fade: 0.0, ..Default::default() };
        let id = c.id;
        c.values.entry(1).or_default().insert(Attribute::Dimmer, 1.0.into());
        l.sequences.push(Sequence { id: 1, cues: vec![c].into(), ..Default::default() });
        let e = LightEngine::start();
        let seen = Arc::new(Mutex::new(0u8));
        let s2 = seen.clone();
        let cfg = OutputConfig { routes: vec![UniverseRoute { universe: 1, wire: 0 }], ..Default::default() };
        let mut e = e;
        e.set_senders(vec![OutputSender::with_opener(cfg, Box::new(move || Ok(Box::new(Probe(s2.clone())) as Box<dyn DmxOutput>)))]);
        // Compile 100 is announced but never arrives in time; the GO waits for it.
        e.send(LightCommand::Compiling(100));
        e.go_cue(id, Instant::now());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(*seen.lock(), 0, "held");
        // More edits after the GO, then compile 100 lands: the GO plays without waiting for 101.
        e.send(LightCommand::Compiling(101));
        e.send(LightCommand::Show(100, Arc::new(LightShow::compile(&l))));
        assert!(wait(|| *seen.lock() == 255), "GO played after its compile");
        // Blackout goes through while compile 101 is still pending.
        e.set_blackout(true);
        let t0 = Instant::now();
        assert!(wait(|| *seen.lock() == 0));
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    /// A burst of edits (a drag) is coalesced into few compiles, and a GO sent after it still
    /// plays the newest design.
    #[test]
    fn edit_bursts_coalesce() {
        let t = library::generic("generic/dimmer").unwrap();
        let mut l = Lighting::default();
        l.add_type(&t);
        l.fixtures.push(Fixture { id: 1, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), address: 1, ..Default::default() });
        let mut c = LightCue { fade: 0.0, ..Default::default() };
        let id = c.id;
        c.values.entry(1).or_default().insert(Attribute::Dimmer, 0.0.into());
        l.sequences.push(Sequence { id: 1, cues: vec![c].into(), ..Default::default() });
        let mut e = LightEngine::start();
        let seen = Arc::new(Mutex::new(0u8));
        let s2 = seen.clone();
        let cfg = OutputConfig { routes: vec![UniverseRoute { universe: 1, wire: 0 }], ..Default::default() };
        e.set_senders(vec![OutputSender::with_opener(cfg, Box::new(move || Ok(Box::new(Probe(s2.clone())) as Box<dyn DmxOutput>)))]);
        for i in 1..=200u32 {
            l.sequences[0].cues[0].values.entry(1).or_default().insert(Attribute::Dimmer, (i as f32 / 200.0).into());
            e.load(&l);
        }
        e.go_cue(id, Instant::now());
        assert!(wait(|| *seen.lock() == 255), "the GO played the last design");
        assert!(e.drain_errors().is_empty());
    }
}
