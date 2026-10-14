//! The light thread: runs [`Playback`] at [`RATE`] and hands each frame to the DMX outputs.
//!
//! It shares no lock with the OpenQ engine, the UI or the audio callback: commands arrive on a
//! channel and are applied at the next frame (their timestamps keep fades exact), the latest
//! frame and sequence states are published with `try_lock`, so a reader never stalls it.
//! A panic inside a frame is caught: the last good frame keeps going out and it's reported once.

use super::dmx::{DmxFrame, OutputConfig, OutputHandle, OutputSender};
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
pub const THREAD: &str = "openq-light";

/// A command for the light thread.
pub enum LightCommand {
    /// A recompile started: hold GOs until its show arrives (keeps their order with the edit).
    Compiling(u64),
    /// Swap in a recompiled show.
    Show(u64, Arc<LightShow>),
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
}

/// Grand master, blackout and sequence levels as set through a [`LightEngine`].
#[derive(Clone, Debug, PartialEq)]
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
}

/// Owns the light thread. Dropping it stops the thread (outputs close).
pub struct LightEngine {
    tx: mpsc::Sender<LightCommand>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    compiles: u64,
    outputs: Vec<OutputConfig>,
    masters: Masters,
    handles: Vec<OutputHandle>,
}

impl LightEngine {
    /// Start the light thread with an empty show and no outputs.
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        let thread = std::thread::Builder::new().name(THREAD.into()).spawn(move || run(rx, s)).ok();
        Self { tx, shared, thread, compiles: 0, outputs: Vec::new(), handles: Vec::new(), masters: Masters::default() }
    }

    fn send(&self, cmd: LightCommand) {
        let _ = self.tx.send(cmd);
    }

    /// Compile a design (on a worker thread, so a big design never holds up the caller) and
    /// hand it to the light thread; restart the outputs if they changed. Commands sent after
    /// this wait for the new show.
    pub fn load(&mut self, lighting: &Lighting) {
        self.compiles += 1;
        let generation = self.compiles;
        self.send(LightCommand::Compiling(generation));
        let design = lighting.clone();
        let tx = self.tx.clone();
        let compile = move || {
            let show = Arc::new(LightShow::compile(&design));
            let _ = tx.send(LightCommand::Show(generation, show));
        };
        if std::thread::Builder::new().name("openq-light-compile".into()).spawn(compile).is_err() {
            log::error!("lighting: can't start the compile thread");
        }
        if lighting.outputs != self.outputs {
            self.set_outputs(lighting.outputs.clone());
        }
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
        self.send(LightCommand::Release { seq, fade, at });
    }

    /// Fade every sequence out.
    pub fn release_all(&self, fade: f64, at: Instant) {
        self.send(LightCommand::ReleaseAll { fade, at });
    }

    /// Sequence master level.
    pub fn set_level(&mut self, seq: SequenceId, level: f32) {
        let level = level.clamp(0.0, 1.0);
        self.masters.levels.insert(seq, level);
        self.send(LightCommand::Level(seq, level));
    }

    /// Grand master.
    pub fn set_grand_master(&mut self, level: f32) {
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

    /// Copy of the latest frame (DMX monitor, visualiser).
    pub fn frame(&self) -> DmxFrame {
        self.shared.frame.lock().clone()
    }

    /// Sequence states.
    pub fn status(&self) -> Vec<SequenceStatus> {
        self.shared.status.lock().clone()
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

impl Drop for LightEngine {
    fn drop(&mut self) {
        // Closing the channel ends the thread.
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
    let mut next = Instant::now();
    let mut n: u64 = 0;
    // Latest compile announced, newest show applied, commands held until they match.
    let (mut expected, mut applied) = (0u64, 0u64);
    let mut held: std::collections::VecDeque<LightCommand> = std::collections::VecDeque::new();
    let mut held_since: Option<Instant> = None;
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
                        let _ = std::thread::Builder::new().name("openq-dmx-close".into()).spawn(move || drop(old));
                    }
                }
                Ok(LightCommand::Compiling(g)) => expected = expected.max(g),
                Ok(LightCommand::Show(g, show)) => {
                    // Compiles can finish out of order: never go back to an older one.
                    if g > applied {
                        applied = g;
                        run_command(&mut playback, LightCommand::Show(g, show), &shared);
                    }
                    if applied >= expected {
                        held_since = None;
                        for cmd in held.drain(..) {
                            run_command(&mut playback, cmd, &shared);
                        }
                    }
                }
                Ok(cmd) if applied < expected && held_since.is_some_and(|t: Instant| t.elapsed() > HOLD_LIMIT) => {
                    // The compile never came back (thread couldn't start): don't hold GOs forever.
                    expected = applied;
                    for c in held.drain(..) {
                        run_command(&mut playback, c, &shared);
                    }
                    run_command(&mut playback, cmd, &shared);
                }
                Ok(cmd) if applied < expected => {
                    held_since.get_or_insert_with(Instant::now);
                    held.push_back(cmd);
                }
                Ok(cmd) => run_command(&mut playback, cmd, &shared),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    quit = true;
                    break;
                }
            }
        }
        if quit {
            break;
        }
        let now = Instant::now();
        let built = crate::recovery::catch(|| {
            crate::recovery::fault_point(crate::recovery::Fault::LightTick);
            last_good.universes.clone_from(&playback.tick(now).universes);
            Ok(())
        });
        if let Err(e) = built {
            // Keep sending the last good frame; report the first fault of a run only.
            if shared.faults.fetch_add(1, Ordering::Relaxed) == 0 {
                log::error!("lighting frame failed, holding the last frame: {e:#}");
                shared.errors.lock().push(format!("lighting: {e:#}"));
            }
        }
        for o in &outputs {
            o.publish(&last_good);
        }
        if let Some(mut f) = shared.frame.try_lock() {
            f.clone_from(&last_good);
        }
        n += 1;
        shared.frames.store(n, Ordering::Relaxed);
        // Sequence states for the UI, ~10 times a second.
        if n.is_multiple_of(4)
            && let Some(mut s) = shared.status.try_lock()
        {
            *s = playback.status();
        }
        next += period;
        let now = Instant::now();
        if now > next + Duration::from_millis(100) {
            next = now; // far behind (suspend, debugger): don't catch up
        }
    }
}

fn run_command(p: &mut Playback, cmd: LightCommand, shared: &Shared) {
    let r = crate::recovery::catch(|| apply(p, cmd).map_err(anyhow::Error::msg));
    if let Err(e) = r {
        shared.errors.lock().push(format!("lighting: {e:#}"));
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
        LightCommand::Outputs(_) | LightCommand::Compiling(_) => {}
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

    fn wait(mut f: impl FnMut() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(2) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
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
        l.sequences.push(Sequence { id: 1, cues: vec![c], ..Default::default() });

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
}
