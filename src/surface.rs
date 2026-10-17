//! Control surfaces: MIDI controllers (faders, buttons, pads) and OSC apps mapped onto
//! executors, lighting masters and the main GO/Stop.
//!
//! A [`Mapping`] ties one [`SurfaceInput`] (a MIDI controller change or note, or an OSC address)
//! to one [`SurfaceAction`]. Executors are numbered by their place in the sequence list (executor
//! 1 = the first sequence), like the Playback page. Mappings live in the show's settings
//! ([`SurfaceSettings`]); the MIDI input port is per computer ([`saved_midi_port`]), so a replica
//! uses its own controller (its commands go to the main device like the rest of its UI).
//!
//! Inputs carry a value 0..=1: a fader its position, a button 1 when pressed and 0 when let go.
//! Button actions fire when the value rises through one half, so a pad that sends note on/off,
//! a button sending CC 127/0 and an OSC button sending 1/0 all work; an OSC message without
//! arguments is a press. [`resolve`] turns an input into [`Command`]s; [`SurfaceHub`] holds what
//! the inputs share (learn mode, the last input, button states, commands waiting for a replica's
//! UI to route them) and [`MidiSurface`] reads a MIDI input port on its own thread, reconnecting
//! when the device comes back. The same port can carry MIDI clock: with
//! [`SurfaceSettings::midi_clock`] on, [`MidiClock`] turns it into the effects tempo.

use crate::control::{Command, LightControl};
use crate::light::Lighting;
use crate::light::engine::Masters;
use crate::light::playback::SequenceStatus;
use crate::show::SharedShow;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A physical control or OSC address.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub enum SurfaceInput {
    /// MIDI control change (`channel` 0..=15).
    MidiCc {
        /// MIDI channel, 0-based.
        channel: u8,
        /// Controller number.
        number: u8,
    },
    /// MIDI note on/off (`channel` 0..=15).
    MidiNote {
        /// MIDI channel, 0-based.
        channel: u8,
        /// Note number.
        number: u8,
    },
    /// An OSC address (its first argument is the value).
    Osc(String),
}

impl SurfaceInput {
    /// Short text for the UI ("CC 7 · ch 1", "Note 36 · ch 10", "/fader1").
    pub fn label(&self) -> String {
        match self {
            SurfaceInput::MidiCc { channel, number } => format!("CC {number} · ch {}", channel + 1),
            SurfaceInput::MidiNote { channel, number } => format!("Note {number} · ch {}", channel + 1),
            SurfaceInput::Osc(a) => a.clone(),
        }
    }
}

/// What a control does. Executor numbers count from 1.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SurfaceAction {
    /// Fader: the executor's master level.
    ExecutorLevel(u16),
    /// Button: GO the executor's next cue.
    ExecutorGo(u16),
    /// Button: release the executor.
    ExecutorRelease(u16),
    /// Button: the executor at full while held (GOes it first if it isn't running).
    ExecutorFlash(u16),
    /// Fader: grand master.
    GrandMaster,
    /// Button: blackout on/off.
    Blackout,
    /// Button: release every executor (the show's Stop-all fade time).
    ReleaseAll,
    /// Button: tap tempo for effects.
    TapTempo,
    /// Button: GO on the main cue list.
    Go,
    /// Button: Stop all.
    Stop,
}

impl SurfaceAction {
    /// One of each kind (executor 1), in menu order.
    pub const KINDS: [SurfaceAction; 10] = [
        SurfaceAction::ExecutorLevel(1),
        SurfaceAction::ExecutorGo(1),
        SurfaceAction::ExecutorRelease(1),
        SurfaceAction::ExecutorFlash(1),
        SurfaceAction::GrandMaster,
        SurfaceAction::Blackout,
        SurfaceAction::ReleaseAll,
        SurfaceAction::TapTempo,
        SurfaceAction::Go,
        SurfaceAction::Stop,
    ];

    /// Name of the kind, for menus.
    pub fn label(self) -> &'static str {
        use crate::i18n::t;
        match self {
            SurfaceAction::ExecutorLevel(_) => t("Executor level"),
            SurfaceAction::ExecutorGo(_) => t("Executor GO"),
            SurfaceAction::ExecutorRelease(_) => t("Executor release"),
            SurfaceAction::ExecutorFlash(_) => t("Executor flash"),
            SurfaceAction::GrandMaster => t("Grand master"),
            SurfaceAction::Blackout => t("Blackout"),
            SurfaceAction::ReleaseAll => t("Release all"),
            SurfaceAction::TapTempo => t("Tap tempo"),
            SurfaceAction::Go => "GO",
            SurfaceAction::Stop => t("Stop all"),
        }
    }

    /// The executor it acts on.
    pub fn executor(self) -> Option<u16> {
        match self {
            SurfaceAction::ExecutorLevel(n) | SurfaceAction::ExecutorGo(n) | SurfaceAction::ExecutorRelease(n) | SurfaceAction::ExecutorFlash(n) => Some(n),
            _ => None,
        }
    }

    /// The same kind on executor `n` (unchanged for other kinds).
    pub fn with_executor(self, n: u16) -> Self {
        match self {
            SurfaceAction::ExecutorLevel(_) => SurfaceAction::ExecutorLevel(n),
            SurfaceAction::ExecutorGo(_) => SurfaceAction::ExecutorGo(n),
            SurfaceAction::ExecutorRelease(_) => SurfaceAction::ExecutorRelease(n),
            SurfaceAction::ExecutorFlash(_) => SurfaceAction::ExecutorFlash(n),
            a => a,
        }
    }

    /// Same kind, whatever the executor.
    pub fn same_kind(self, other: Self) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }

    /// Follows a continuous value (a fader) rather than presses.
    pub fn is_fader(self) -> bool {
        matches!(self, SurfaceAction::ExecutorLevel(_) | SurfaceAction::GrandMaster)
    }
}

/// One control mapped to one action.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Mapping {
    /// The control.
    pub input: SurfaceInput,
    /// What it does.
    pub action: SurfaceAction,
}

/// Control surface settings of a show.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct SurfaceSettings {
    /// Controls and what they do (OSC addresses come through the OSC listener).
    pub mappings: Vec<Mapping>,
    /// The effects tempo follows MIDI clock arriving on the surface's MIDI input.
    pub midi_clock: bool,
}

impl SurfaceSettings {
    /// Nothing set up (left out of the show file).
    pub fn is_empty(&self) -> bool {
        self.mappings.is_empty() && !self.midi_clock
    }

    /// Mappings of `n` faders as executor levels on CC `first`.. and `n` buttons as executor GOs
    /// on notes `first_note`.., all on `channel`: a starting point for a generic controller.
    pub fn executor_bank(n: u16, channel: u8, first_cc: u8, first_note: u8) -> Vec<Mapping> {
        let mut v = Vec::new();
        for i in 0..n {
            v.push(Mapping { input: SurfaceInput::MidiCc { channel, number: first_cc.saturating_add(i as u8) }, action: SurfaceAction::ExecutorLevel(i + 1) });
            v.push(Mapping { input: SurfaceInput::MidiNote { channel, number: first_note.saturating_add(i as u8) }, action: SurfaceAction::ExecutorGo(i + 1) });
        }
        v
    }
}

fn port_file() -> Option<std::path::PathBuf> {
    crate::remote::config_dir().map(|d| d.join("midi-surface"))
}

/// The MIDI input port chosen on this computer for control surfaces (read from the config
/// folder; call it once, not per frame).
pub fn saved_midi_port() -> Option<String> {
    let s = crate::recovery::read_text(&port_file()?, 4096).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Remember the MIDI input port for control surfaces on this computer (None = no MIDI surface).
pub fn save_midi_port(port: Option<&str>) -> anyhow::Result<()> {
    let path = port_file().ok_or_else(|| anyhow::anyhow!("no config folder"))?;
    match port {
        Some(p) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            crate::recovery::write_atomic(&path, p.as_bytes())
        }
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        },
    }
}

/// A MIDI message as a surface input and its value; None for anything else (clock, MTC, SysEx).
pub fn parse_midi(msg: &[u8]) -> Option<(SurfaceInput, f32)> {
    let (&status, data) = msg.split_first()?;
    let channel = status & 0x0f;
    let (number, value) = (*data.first()? & 0x7f, *data.get(1)? & 0x7f);
    match status & 0xf0 {
        0xb0 => Some((SurfaceInput::MidiCc { channel, number }, value as f32 / 127.0)),
        0x90 => Some((SurfaceInput::MidiNote { channel, number }, if value > 0 { 1.0 } else { 0.0 })),
        0x80 => Some((SurfaceInput::MidiNote { channel, number }, 0.0)),
        _ => None,
    }
}

/// Tap tempo: add a tap and return the tempo of the recent taps (a pause over 2 s starts again).
pub fn tap(taps: &mut Vec<Instant>, now: Instant) -> Option<f32> {
    if taps.last().is_some_and(|l| now.duration_since(*l).as_secs_f32() > 2.0) {
        taps.clear();
    }
    taps.push(now);
    if taps.len() > 5 {
        taps.remove(0);
    }
    let n = taps.len();
    (n >= 2).then(|| 60.0 * (n - 1) as f32 / taps[n - 1].duration_since(taps[0]).as_secs_f32())
}

/// MIDI clock ticks in one beat.
const CLOCK_PPQN: usize = 24;
/// Ticks the tempo is measured over (two beats: steady, yet follows a change within a bar).
const CLOCK_WINDOW: usize = 2 * CLOCK_PPQN;

/// Tempo from MIDI clock (24 ticks per beat). Feed it every message; it answers with a tempo
/// once per beat when the tempo moved, measured over the last two beats of ticks so the
/// jitter of single ticks averages out. Start/Continue/Stop and a pause over a second start the
/// measurement again (the last tempo stays).
#[derive(Default)]
pub struct MidiClock {
    /// Tick times, µs, oldest first (at most `CLOCK_WINDOW + 1`).
    ticks: std::collections::VecDeque<u64>,
    count: usize,
    sent: Option<f32>,
}

impl MidiClock {
    /// Handle one MIDI message received at `us` (microseconds on any steady clock). Returns the
    /// tempo in BPM when there is a new one to set.
    pub fn message(&mut self, msg: &[u8], us: u64) -> Option<f32> {
        match msg.first()? {
            0xf8 => {}
            0xfa..=0xfc => {
                self.reset();
                return None;
            }
            _ => return None,
        }
        if self.ticks.back().is_some_and(|&t| us <= t || us - t > 1_000_000) {
            self.reset();
        }
        self.ticks.push_back(us);
        if self.ticks.len() > CLOCK_WINDOW + 1 {
            self.ticks.pop_front();
        }
        self.count += 1;
        let n = self.ticks.len() - 1;
        if !self.count.is_multiple_of(CLOCK_PPQN) || n < CLOCK_PPQN {
            return None;
        }
        let span = (self.ticks[n] - self.ticks[0]) as f64 / 1e6;
        let bpm = (60.0 * n as f64 / CLOCK_PPQN as f64 / span) as f32;
        let bpm = (bpm * 10.0).round() / 10.0;
        if !(20.0..=300.0).contains(&bpm) || self.sent.is_some_and(|s| (s - bpm).abs() < 0.2) {
            return None;
        }
        self.sent = Some(bpm);
        Some(bpm)
    }

    fn reset(&mut self) {
        self.ticks.clear();
        self.count = 0;
    }
}

/// What button handling remembers between inputs.
#[derive(Default)]
pub struct Feel {
    /// Last value of each input (presses are rising edges).
    values: HashMap<SurfaceInput, f32>,
    /// Executor levels to go back to when a flash button is let go.
    flashed: HashMap<u16, f32>,
    taps: Vec<Instant>,
}

/// What the show looks like to [`resolve`].
pub struct View<'a> {
    /// The lighting design (executor number → sequence).
    pub lighting: &'a Lighting,
    /// Master levels as last set.
    pub masters: &'a Masters,
    /// Sequence states.
    pub status: &'a [SequenceStatus],
    /// Fade time of Release all.
    pub release_fade: f32,
}

/// The commands `action` gives for `value` (after `prev`, the input's previous value).
fn act(action: SurfaceAction, value: f32, prev: f32, view: &View, feel: &mut Feel, now: Instant) -> Vec<Command> {
    let value = value.clamp(0.0, 1.0);
    let pressed = value >= 0.5 && prev < 0.5;
    let released = value < 0.5 && prev >= 0.5;
    let seq = |n: u16| view.lighting.sequences.get((n as usize).checked_sub(1)?).map(|s| s.id);
    let light = |c: LightControl| Command::Light(c);
    match action {
        SurfaceAction::ExecutorLevel(n) => seq(n).map(|s| light(LightControl::Level(s, value))).into_iter().collect(),
        SurfaceAction::GrandMaster => vec![light(LightControl::GrandMaster(value))],
        SurfaceAction::ExecutorGo(n) if pressed => seq(n).map(|s| light(LightControl::Go(s, None))).into_iter().collect(),
        SurfaceAction::ExecutorRelease(n) if pressed => seq(n).map(|s| light(LightControl::Release(Some(s), 1.0))).into_iter().collect(),
        SurfaceAction::ExecutorFlash(n) => {
            let Some(s) = seq(n) else { return Vec::new() };
            if pressed {
                feel.flashed.insert(n, view.masters.level(s));
                let mut v = vec![light(LightControl::Level(s, 1.0))];
                if !view.status.iter().any(|x| x.id == s && x.active) {
                    v.push(light(LightControl::Go(s, None)));
                }
                v
            } else if released {
                feel.flashed.remove(&n).map(|l| light(LightControl::Level(s, l))).into_iter().collect()
            } else {
                Vec::new()
            }
        }
        SurfaceAction::Blackout if pressed => vec![light(LightControl::Blackout(!view.masters.blackout))],
        SurfaceAction::ReleaseAll if pressed => vec![light(LightControl::Release(None, view.release_fade))],
        SurfaceAction::TapTempo if pressed => tap(&mut feel.taps, now).map(|b| light(LightControl::Tempo(b))).into_iter().collect(),
        SurfaceAction::Go if pressed => vec![Command::Go],
        SurfaceAction::Stop if pressed => vec![Command::Stop],
        _ => Vec::new(),
    }
}

/// The commands for one input event: every mapping on `input`, plus `builtin` (an OSC address
/// OpenQ knows without a mapping). `value` None = a press with no value (an OSC message
/// without arguments).
pub fn resolve(
    settings: &SurfaceSettings,
    builtin: Option<SurfaceAction>,
    view: &View,
    feel: &mut Feel,
    input: &SurfaceInput,
    value: Option<f32>,
    now: Instant,
) -> Vec<Command> {
    let actions = settings.mappings.iter().filter(|m| &m.input == input).map(|m| m.action).chain(builtin);
    // A value-less message is a press then a release.
    let (value, after) = match value {
        Some(v) => (v, v),
        None => (1.0, 0.0),
    };
    let prev = feel.values.get(input).copied().unwrap_or(0.0);
    let mut out = Vec::new();
    for a in actions {
        out.extend(act(a, value, prev, view, feel, now));
    }
    feel.values.insert(input.clone(), after);
    out
}

/// The lighting action of a built-in OSC address: `/light/executor/{n}/level|go|release|flash`,
/// `/light/grandmaster`, `/light/blackout`, `/light/release`, `/light/tap`.
pub fn osc_builtin(addr: &str) -> Option<SurfaceAction> {
    let parts: Vec<&str> = addr.trim_matches('/').split('/').collect();
    Some(match parts.as_slice() {
        ["light", "executor", n, what] => {
            let n: u16 = n.parse().ok().filter(|n| *n > 0)?;
            match *what {
                "level" => SurfaceAction::ExecutorLevel(n),
                "go" => SurfaceAction::ExecutorGo(n),
                "release" => SurfaceAction::ExecutorRelease(n),
                "flash" => SurfaceAction::ExecutorFlash(n),
                _ => return None,
            }
        }
        ["light", "grandmaster"] => SurfaceAction::GrandMaster,
        ["light", "blackout"] => SurfaceAction::Blackout,
        ["light", "release"] => SurfaceAction::ReleaseAll,
        ["light", "tap"] => SurfaceAction::TapTempo,
        _ => return None,
    })
}

/// The last input seen, for the settings page.
#[derive(Clone, Debug)]
pub struct LastInput {
    /// The control.
    pub input: SurfaceInput,
    /// Its value (None = an OSC message without arguments).
    pub value: Option<f32>,
    /// When.
    pub at: Instant,
}

#[derive(Default)]
struct HubState {
    feel: Feel,
    last: Option<LastInput>,
    /// Learn mode: the next input is kept here instead of acting.
    learning: bool,
    learned: Option<SurfaceInput>,
    /// Commands a replica's UI routes to the main device.
    pending: Vec<Command>,
    /// Last tempo measured from MIDI clock, and when.
    clock: Option<(f32, Instant)>,
}

/// What every surface input shares. Cheap to clone (one shared state).
#[derive(Clone, Default)]
pub struct SurfaceHub {
    state: Arc<Mutex<HubState>>,
}

impl SurfaceHub {
    /// A new hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle one input: in learn mode keep it; otherwise run its mappings (and `builtin`) on
    /// the show, or queue them for the UI when this device is a replica (they must go to the
    /// main device). Returns true if it was used (learned, or something mapped to it).
    pub fn input(&self, show: &SharedShow, input: SurfaceInput, value: Option<f32>, builtin: Option<SurfaceAction>) -> bool {
        let now = Instant::now();
        // Lock order: show, then the hub (the UI reads the hub while holding the show).
        let mut show = show.lock();
        let mut st = self.state.lock();
        st.last = Some(LastInput { input: input.clone(), value, at: now });
        if st.learning {
            st.learning = false;
            st.learned = Some(input);
            return true;
        }
        let masters = show.media.light.masters().clone();
        let status = show.media.light.status();
        let view = View { lighting: &show.ws.lighting, masters: &masters, status: &status, release_fade: show.ws.settings.panic_fade as f32 };
        let cmds = resolve(&show.ws.settings.surfaces, builtin, &view, &mut st.feel, &input, value, now);
        let used = !cmds.is_empty() || builtin.is_some() || show.ws.settings.surfaces.mappings.iter().any(|m| m.input == input);
        if show.replica {
            st.pending.extend(cmds);
            return used;
        }
        drop(st);
        for c in cmds {
            if let Err(e) = crate::control::apply(&mut show, c, now) {
                log::warn!("control surface {}: {e}", input.label());
            }
        }
        used
    }

    /// A tempo measured from MIDI clock: sets the effects tempo when the show follows MIDI clock
    /// ([`SurfaceSettings::midi_clock`]), else is only kept for [`SurfaceHub::clock`].
    pub fn clock_tempo(&self, show: &SharedShow, bpm: f32) {
        let now = Instant::now();
        let mut show = show.lock();
        let mut st = self.state.lock();
        st.clock = Some((bpm, now));
        if !show.ws.settings.surfaces.midi_clock {
            return;
        }
        let cmd = Command::Light(LightControl::Tempo(bpm));
        if show.replica {
            st.pending.push(cmd);
            return;
        }
        drop(st);
        if let Err(e) = crate::control::apply(&mut show, cmd, now) {
            log::warn!("MIDI clock: {e}");
        }
    }

    /// The tempo MIDI clock gave in the last two seconds.
    pub fn clock(&self) -> Option<f32> {
        self.state.lock().clock.filter(|(_, at)| at.elapsed() < Duration::from_secs(2)).map(|(b, _)| b)
    }

    /// Start or stop learn mode (the next input is kept for [`SurfaceHub::take_learned`]).
    pub fn set_learning(&self, on: bool) {
        let mut st = self.state.lock();
        st.learning = on;
        st.learned = None;
    }

    /// Learn mode is waiting for an input.
    pub fn learning(&self) -> bool {
        self.state.lock().learning
    }

    /// The input learned, once.
    pub fn take_learned(&self) -> Option<SurfaceInput> {
        self.state.lock().learned.take()
    }

    /// The last input seen.
    pub fn last(&self) -> Option<LastInput> {
        self.state.lock().last.clone()
    }

    /// Commands waiting to be routed by a replica's UI.
    pub fn take_pending(&self) -> Vec<Command> {
        std::mem::take(&mut self.state.lock().pending)
    }
}

/// How a MIDI surface is doing.
#[derive(Clone, Debug, Default)]
pub struct MidiStatus {
    /// Connected port.
    pub connected: Option<String>,
    /// Why it isn't connected.
    pub error: Option<String>,
}

/// Reads one MIDI input port for a [`SurfaceHub`] on its own thread. The port is looked for by
/// name and reconnected when it comes back (midir doesn't report an unplugged port, so the
/// thread checks the port list every [`crate::media::AUDIO_RETRY`]). Dropping it stops the thread.
pub struct MidiSurface {
    /// Port name.
    pub port: String,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<MidiStatus>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MidiSurface {
    /// Connect to the input port `port`; `repaint` is called after each handled message.
    pub fn start(port: String, show: SharedShow, hub: SurfaceHub, repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(MidiStatus::default()));
        let (stop2, status2, name) = (stop.clone(), status.clone(), port.clone());
        let repaint = Arc::new(repaint);
        let thread = std::thread::Builder::new()
            .name("openq-midi-surface".into())
            .spawn(move || {
                while !stop2.load(Ordering::Acquire) {
                    let (show, hub, repaint) = (show.clone(), hub.clone(), repaint.clone());
                    let conn = (|| {
                        let mut m = midir::MidiInput::new("OpenQ").map_err(|e| e.to_string())?;
                        // Timing messages too: MIDI clock (cheap, 24 per beat).
                        m.ignore(midir::Ignore::SysexAndActiveSense);
                        let ports = m.ports();
                        let p = ports.iter().find(|p| m.port_name(p).ok().as_deref() == Some(name.as_str())).ok_or_else(|| format!("MIDI input not found: {name}"))?.clone();
                        let (epoch, mut clock) = (Instant::now(), MidiClock::default());
                        m.connect(
                            &p,
                            "openq-surface",
                            move |_, msg, _| {
                                if msg.first().is_some_and(|b| *b >= 0xf8) {
                                    if let Some(bpm) = clock.message(msg, epoch.elapsed().as_micros() as u64) {
                                        let r = crate::recovery::catch(|| {
                                            hub.clock_tempo(&show, bpm);
                                            Ok(())
                                        });
                                        if let Err(e) = r {
                                            log::warn!("MIDI clock: {e:#}");
                                        }
                                        repaint();
                                    }
                                } else if let Some((input, value)) = parse_midi(msg) {
                                    // A failure on one message must not end the connection.
                                    let r = crate::recovery::catch(|| Ok(hub.input(&show, input, Some(value), None)));
                                    if let Err(e) = r {
                                        log::warn!("control surface: {e:#}");
                                    }
                                    repaint();
                                }
                            },
                            (),
                        )
                        .map_err(|e| e.to_string())
                    })();
                    match conn {
                        Ok(c) => {
                            log::info!("control surface: MIDI input {name}");
                            *status2.lock() = MidiStatus { connected: Some(name.clone()), error: None };
                            let mut checked = Instant::now();
                            while !stop2.load(Ordering::Acquire) {
                                std::thread::park_timeout(Duration::from_millis(50));
                                if checked.elapsed() >= crate::media::AUDIO_RETRY {
                                    checked = Instant::now();
                                    if !crate::media::tcio::list_midi_inputs().contains(&name) {
                                        log::warn!("control surface: MIDI input lost ({name}); reconnecting…");
                                        break;
                                    }
                                }
                            }
                            drop(c);
                            if !stop2.load(Ordering::Acquire) {
                                *status2.lock() = MidiStatus { connected: None, error: Some(format!("MIDI input lost: {name}")) };
                            }
                        }
                        Err(e) => {
                            let mut st = status2.lock();
                            if st.error.as_deref() != Some(e.as_str()) {
                                log::warn!("control surface: {e}");
                            }
                            *st = MidiStatus { connected: None, error: Some(e) };
                        }
                    }
                    let until = Instant::now() + crate::media::AUDIO_RETRY;
                    while !stop2.load(Ordering::Acquire) && Instant::now() < until {
                        std::thread::park_timeout(Duration::from_millis(50));
                    }
                }
            })
            .ok();
        Self { port, stop, status, thread }
    }

    /// Current state.
    pub fn status(&self) -> MidiStatus {
        self.status.lock().clone()
    }
}

impl Drop for MidiSurface {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            // Closing a MIDI connection can take a moment on some systems: don't wait long.
            let deadline = Instant::now() + Duration::from_millis(200);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::model::Sequence;

    fn design() -> Lighting {
        let mut l = Lighting::default();
        for id in [7, 9] {
            l.sequences.push(Sequence { id, ..Default::default() });
        }
        l
    }

    fn run(s: &SurfaceSettings, feel: &mut Feel, input: &SurfaceInput, v: Option<f32>, masters: &Masters, status: &[SequenceStatus]) -> Vec<Command> {
        let l = design();
        let view = View { lighting: &l, masters, status, release_fade: 2.0 };
        resolve(s, None, &view, feel, input, v, Instant::now())
    }

    #[test]
    fn midi_messages() {
        assert_eq!(parse_midi(&[0xb2, 7, 127]), Some((SurfaceInput::MidiCc { channel: 2, number: 7 }, 1.0)));
        assert_eq!(parse_midi(&[0x99, 36, 100]), Some((SurfaceInput::MidiNote { channel: 9, number: 36 }, 1.0)));
        assert_eq!(parse_midi(&[0x99, 36, 0]), Some((SurfaceInput::MidiNote { channel: 9, number: 36 }, 0.0)), "note on, velocity 0 = off");
        assert_eq!(parse_midi(&[0x80, 36, 64]).unwrap().1, 0.0);
        assert_eq!(parse_midi(&[0xf8]), None, "clock");
        assert_eq!(parse_midi(&[0xf1, 0x12]), None, "MTC quarter frame");
        assert_eq!(parse_midi(&[0xb0, 7]), None, "short");
        assert_eq!(parse_midi(&[]), None);
    }

    #[test]
    fn faders_and_buttons() {
        let cc = |n| SurfaceInput::MidiCc { channel: 0, number: n };
        let note = |n| SurfaceInput::MidiNote { channel: 0, number: n };
        let mut s = SurfaceSettings { mappings: SurfaceSettings::executor_bank(3, 0, 1, 36), ..Default::default() };
        s.mappings.push(Mapping { input: note(50), action: SurfaceAction::Blackout });
        s.mappings.push(Mapping { input: cc(20), action: SurfaceAction::Go });
        let (mut feel, m) = (Feel::default(), Masters::default());
        let level = |c: &[Command]| match c {
            [Command::Light(LightControl::Level(s, v))] => (*s, *v),
            other => panic!("{other:?}"),
        };
        assert_eq!(level(&run(&s, &mut feel, &cc(2), Some(0.5), &m, &[])), (9, 0.5), "fader 2 = second sequence");
        assert!(run(&s, &mut feel, &cc(3), Some(0.5), &m, &[]).is_empty(), "no third sequence");
        // A button fires once per press.
        assert!(matches!(run(&s, &mut feel, &note(36), Some(1.0), &m, &[])[..], [Command::Light(LightControl::Go(7, None))]));
        assert!(run(&s, &mut feel, &note(36), Some(1.0), &m, &[]).is_empty(), "held");
        assert!(run(&s, &mut feel, &note(36), Some(0.0), &m, &[]).is_empty(), "let go");
        assert_eq!(run(&s, &mut feel, &note(36), Some(1.0), &m, &[]).len(), 1, "pressed again");
        // CC buttons: 127 then 0.
        assert!(matches!(run(&s, &mut feel, &cc(20), Some(1.0), &m, &[])[..], [Command::Go]));
        assert!(run(&s, &mut feel, &cc(20), Some(0.8), &m, &[]).is_empty());
        // Blackout toggles from the masters' state.
        let bo = Masters { blackout: true, ..Default::default() };
        assert!(matches!(run(&s, &mut feel, &note(50), Some(1.0), &bo, &[])[..], [Command::Light(LightControl::Blackout(false))]));
        // Unmapped inputs do nothing.
        assert!(run(&s, &mut feel, &cc(99), Some(1.0), &m, &[]).is_empty());
    }

    #[test]
    fn flash_goes_and_restores() {
        let b = SurfaceInput::Osc("/flash".into());
        let s = SurfaceSettings { mappings: vec![Mapping { input: b.clone(), action: SurfaceAction::ExecutorFlash(1) }], ..Default::default() };
        let mut feel = Feel::default();
        let mut m = Masters::default();
        m.levels.insert(7, 0.3);
        let down = run(&s, &mut feel, &b, Some(1.0), &m, &[]);
        assert!(matches!(down[..], [Command::Light(LightControl::Level(7, 1.0)), Command::Light(LightControl::Go(7, None))]), "{down:?}");
        let up = run(&s, &mut feel, &b, Some(0.0), &m, &[]);
        assert!(matches!(up[..], [Command::Light(LightControl::Level(7, l))] if l == 0.3));
        // Already running: no GO.
        let running = [SequenceStatus { id: 7, cue: None, active: true, level: 0.3 }];
        assert_eq!(run(&s, &mut feel, &b, Some(1.0), &m, &running).len(), 1);
    }

    #[test]
    fn osc_without_arguments_is_a_press() {
        let b = SurfaceInput::Osc("/x".into());
        let s = SurfaceSettings { mappings: vec![Mapping { input: b.clone(), action: SurfaceAction::ExecutorGo(2) }], ..Default::default() };
        let mut feel = Feel::default();
        let m = Masters::default();
        for _ in 0..3 {
            assert_eq!(run(&s, &mut feel, &b, None, &m, &[]).len(), 1, "every message GOes");
        }
        assert_eq!(osc_builtin("/light/executor/3/level"), Some(SurfaceAction::ExecutorLevel(3)));
        assert_eq!(osc_builtin("/light/executor/0/go"), None);
        assert_eq!(osc_builtin("/light/grandmaster"), Some(SurfaceAction::GrandMaster));
        assert_eq!(osc_builtin("/go"), None);
    }

    /// Learn mode keeps the next input instead of acting on it; replicas queue commands.
    #[test]
    fn hub_learns_and_queues_for_replicas() {
        let show: SharedShow = Arc::new(Mutex::new(crate::show::Show::new(crate::media::Prober::new())));
        show.lock().ws.lighting = design();
        show.lock().ws.settings.surfaces.mappings = SurfaceSettings::executor_bank(1, 0, 1, 36);
        let hub = SurfaceHub::new();
        hub.set_learning(true);
        let cc = SurfaceInput::MidiCc { channel: 0, number: 1 };
        assert!(hub.input(&show, cc.clone(), Some(0.4), None));
        assert_eq!(hub.take_learned(), Some(cc.clone()));
        assert!(!hub.learning());
        assert_eq!(show.lock().media.light.masters().level(7), 1.0, "learning doesn't act");
        assert!(hub.input(&show, cc.clone(), Some(0.4), None));
        assert_eq!(show.lock().media.light.masters().level(7), 0.4);
        show.lock().replica = true;
        hub.input(&show, cc.clone(), Some(0.6), None);
        assert_eq!(show.lock().media.light.masters().level(7), 0.4, "a replica doesn't apply locally");
        assert!(matches!(hub.take_pending()[..], [Command::Light(LightControl::Level(7, v))] if v == 0.6));
        assert_eq!(hub.last().unwrap().input, cc);
    }

    #[test]
    fn midi_clock_tempo() {
        let mut c = MidiClock::default();
        // 120 BPM = a tick every 20 833 µs; ±1 ms jitter on every other tick.
        let mut t = 1_000_000u64;
        let mut got = Vec::new();
        for i in 0..24 * 8 {
            let jitter = if i % 2 == 0 { 1000 } else { 0 };
            if let Some(b) = c.message(&[0xf8], t + jitter) {
                got.push(b);
            }
            t += 20_833;
        }
        assert_eq!(got.len(), 1, "one tempo, then only changes: {got:?}");
        assert!((got[0] - 120.0).abs() <= 0.1, "{got:?}");
        assert_eq!(c.message(&[0x90, 1, 1], t), None, "not clock");
        // A new tempo after Stop/Start: 90 BPM.
        c.message(&[0xfc], t);
        c.message(&[0xfa], t);
        let mut last = None;
        for _ in 0..24 * 3 {
            t += 27_778;
            last = c.message(&[0xf8], t).or(last);
        }
        assert!(last.is_some_and(|b| (b - 90.0).abs() <= 0.1), "{last:?}");
        // A pause over a second starts again rather than reading a very slow tempo.
        t += 5_000_000;
        assert_eq!(c.message(&[0xf8], t), None);
    }

    #[test]
    fn hub_follows_clock_only_when_on() {
        let show: SharedShow = Arc::new(Mutex::new(crate::show::Show::new(crate::media::Prober::new())));
        let hub = SurfaceHub::new();
        let bpm = || show.lock().media.light.masters().bpm;
        let before = bpm();
        hub.clock_tempo(&show, 133.0);
        assert_eq!(bpm(), before, "off by default");
        assert_eq!(hub.clock(), Some(133.0), "still shown");
        show.lock().ws.settings.surfaces.midi_clock = true;
        hub.clock_tempo(&show, 133.0);
        assert_eq!(bpm(), 133.0);
    }

    #[test]
    fn tap_tempo() {
        let t0 = Instant::now();
        let mut taps = Vec::new();
        assert_eq!(tap(&mut taps, t0), None);
        for i in 1..4 {
            let bpm = tap(&mut taps, t0 + Duration::from_millis(500 * i)).unwrap();
            assert!((bpm - 120.0).abs() < 0.01);
        }
        assert_eq!(tap(&mut taps, t0 + Duration::from_secs(10)), None, "a pause starts again");
    }
}
