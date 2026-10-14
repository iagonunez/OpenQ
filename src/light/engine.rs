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
/// Name of the light thread.
pub const THREAD: &str = "openq-light";

/// A command for the light thread.
pub enum LightCommand {
    /// Swap in a recompiled show.
    Show(Arc<LightShow>),
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
    /// Replace the outputs.
    Outputs(Vec<OutputSender>),
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
    show: Arc<LightShow>,
    outputs: Vec<OutputConfig>,
    handles: Vec<OutputHandle>,
}

impl LightEngine {
    /// Start the light thread with an empty show and no outputs.
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        let thread = std::thread::Builder::new().name(THREAD.into()).spawn(move || run(rx, s)).ok();
        Self { tx, shared, thread, show: Arc::new(LightShow::default()), outputs: Vec::new(), handles: Vec::new() }
    }

    fn send(&self, cmd: LightCommand) {
        let _ = self.tx.send(cmd);
    }

    /// Compile a design and send it to the light thread; restart the outputs if they changed.
    pub fn load(&mut self, lighting: &Lighting) {
        self.show = Arc::new(LightShow::compile(lighting));
        self.send(LightCommand::Show(self.show.clone()));
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

    /// The compiled show last loaded.
    pub fn show(&self) -> &Arc<LightShow> {
        &self.show
    }

    /// GO a lighting cue (`cue` None = next in the sequence).
    pub fn go(&self, seq: SequenceId, cue: Option<LightCueId>, at: Instant) {
        self.send(LightCommand::Go { seq, cue, at });
    }

    /// GO a lighting cue by its id alone. Err when the cue isn't in the design.
    pub fn go_cue(&self, cue: LightCueId, at: Instant) -> Result<(), String> {
        let (si, _) = self.show.find_cue(cue).ok_or("lighting cue not found")?;
        self.go(self.show.sequences[si].id, Some(cue), at);
        Ok(())
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
    pub fn set_level(&self, seq: SequenceId, level: f32) {
        self.send(LightCommand::Level(seq, level));
    }

    /// Grand master.
    pub fn set_grand_master(&self, level: f32) {
        self.send(LightCommand::GrandMaster(level));
    }

    /// Blackout.
    pub fn set_blackout(&self, on: bool) {
        self.send(LightCommand::Blackout(on));
    }

    /// Replace the programmer.
    pub fn set_programmer(&self, values: Vec<(FixtureId, Attribute, f32)>) {
        self.send(LightCommand::Programmer(values));
    }

    /// Seconds from GO until a lighting cue's fades are complete.
    pub fn cue_duration(&self, cue: LightCueId) -> Option<f64> {
        let (si, ci) = self.show.find_cue(cue)?;
        Some(self.show.sequences[si].cues[ci].duration())
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
                Ok(cmd) => {
                    let r = crate::recovery::catch(|| apply(&mut playback, cmd).map_err(anyhow::Error::msg));
                    if let Err(e) = r {
                        shared.errors.lock().push(format!("{e:#}"));
                    }
                }
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

fn apply(p: &mut Playback, cmd: LightCommand) -> Result<(), String> {
    match cmd {
        LightCommand::Show(s) => p.set_show(s),
        LightCommand::Go { seq, cue, at } => {
            p.go(seq, cue, at)?;
        }
        LightCommand::Release { seq, fade, at } => p.release(seq, fade, at),
        LightCommand::ReleaseAll { fade, at } => p.release_all(fade, at),
        LightCommand::Level(seq, l) => p.set_level(seq, l),
        LightCommand::GrandMaster(l) => p.grand_master = l,
        LightCommand::Blackout(b) => p.blackout = b,
        LightCommand::Programmer(v) => p.set_programmer(&v),
        LightCommand::Outputs(_) => {}
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
        c.values.entry(1).or_default().insert(Attribute::Dimmer, 1.0);
        let id = c.id;
        l.sequences.push(Sequence { id: 1, cues: vec![c], ..Default::default() });

        let mut e = LightEngine::start();
        e.load(&l);
        let seen = Arc::new(Mutex::new(0u8));
        let s2 = seen.clone();
        let cfg = OutputConfig { routes: vec![UniverseRoute { universe: 1, wire: 0 }], ..Default::default() };
        e.set_senders(vec![OutputSender::with_opener(cfg, Box::new(move || Ok(Box::new(Probe(s2.clone())) as Box<dyn DmxOutput>)))]);
        assert_eq!(e.cue_duration(id), Some(0.0));
        e.go_cue(id, Instant::now()).unwrap();
        assert!(wait(|| *seen.lock() == 255), "GO reached the device");
        assert!(wait(|| e.status().first().is_some_and(|s| s.cue == Some(id))));
        assert_eq!(e.frame().get(1).unwrap().data[0], 255);
        e.set_blackout(true);
        assert!(wait(|| *seen.lock() == 0));
        assert!(e.go_cue(uuid::Uuid::new_v4(), Instant::now()).is_err());
        e.go(9, None, Instant::now());
        assert!(wait(|| !e.drain_errors().is_empty()), "unknown sequence reported");
        assert!(e.frames() > 0);
    }
}
