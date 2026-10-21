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
//!
//! Cue triggers: a cue's [`MidiTrigger`] fires it ([`triggered`]) when its message arrives on the
//! same MIDI input; a program change is a press each time (so a "GO on program change" is just
//! a mapping to [`SurfaceAction::Go`]).
//!
//! Pages: with [`SurfaceSettings::page_size`] set, the surface's page (Page next/previous
//! buttons; per computer, kept in the hub) shifts its executors by that many sequences, and
//! picks the Playback page's button page its playback buttons reach.
//! Soft takeover ([`SurfaceSettings::soft_takeover`]): a fader that doesn't match the level it
//! controls (moved elsewhere, or a new page) does nothing until it reaches or crosses that level.
//! Feedback: [`feedback`] is what each mapped control should show (fader levels for motor
//! faders, button LEDs for running executors, playback buttons that are on, blackout, pages);
//! [`SurfaceFeedback`] sends the changes to a MIDI output and to the OSC app that last sent
//! (at [`SurfaceSettings::osc_feedback_port`]).

use crate::control::{Command, CueAction, LightControl};
use crate::model::{CueId, CueList, MidiTrigger, MidiTriggerKind};
use crate::light::Lighting;
use crate::light::model::{ButtonId, LightButton, SequenceId};
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
    /// MIDI program change (`channel` 0..=15): a press each time it arrives.
    MidiProgram {
        /// MIDI channel, 0-based.
        channel: u8,
        /// Program number.
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
            SurfaceInput::MidiProgram { channel, number } => format!("PC {number} · ch {}", channel + 1),
            SurfaceInput::Osc(a) => a.clone(),
        }
    }

    /// The cue trigger that waits for this input (any channel taken as this one's), if it is MIDI.
    pub fn midi_trigger(&self) -> Option<MidiTrigger> {
        let (kind, channel, number) = match *self {
            SurfaceInput::MidiCc { channel, number } => (MidiTriggerKind::Control, channel, number),
            SurfaceInput::MidiNote { channel, number } => (MidiTriggerKind::Note, channel, number),
            SurfaceInput::MidiProgram { channel, number } => (MidiTriggerKind::Program, channel, number),
            SurfaceInput::Osc(_) => return None,
        };
        Some(MidiTrigger { kind, channel: Some(channel), number })
    }
}

/// The armed cues (any list, any depth) whose MIDI trigger `input` fires. `pressed` is whether
/// the input was just pressed (a note on, a CC rising through 64, any program change).
pub fn triggered(cues: &CueList, input: &SurfaceInput, pressed: bool) -> Vec<CueId> {
    let Some(got) = input.midi_trigger().filter(|_| pressed) else { return Vec::new() };
    let fires = |t: &MidiTrigger| t.kind == got.kind && t.number == got.number && t.channel.is_none_or(|c| Some(c) == got.channel);
    cues.iter_all().into_iter().filter(|c| c.armed && c.midi_trigger.as_ref().is_some_and(fires)).map(|c| c.id).collect()
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
    /// Button: playback button `n` (from 1) of the surface's page on the Playback page; toggles
    /// it, or holds it on while pressed (a flash button).
    PlaybackButton(u16),
    /// Button: the surface's next page.
    PageNext,
    /// Button: the surface's previous page.
    PagePrevious,
}

impl SurfaceAction {
    /// One of each kind (executor 1), in menu order.
    pub const KINDS: [SurfaceAction; 13] = [
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
        SurfaceAction::PlaybackButton(1),
        SurfaceAction::PageNext,
        SurfaceAction::PagePrevious,
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
            SurfaceAction::PlaybackButton(_) => t("Playback button"),
            SurfaceAction::PageNext => t("Next page"),
            SurfaceAction::PagePrevious => t("Previous page"),
        }
    }

    /// The executor it acts on.
    pub fn executor(self) -> Option<u16> {
        match self {
            SurfaceAction::ExecutorLevel(n) | SurfaceAction::ExecutorGo(n) | SurfaceAction::ExecutorRelease(n) | SurfaceAction::ExecutorFlash(n) => Some(n),
            _ => None,
        }
    }

    /// The executor or playback button number it acts on.
    pub fn number(self) -> Option<u16> {
        match self {
            SurfaceAction::PlaybackButton(n) => Some(n),
            a => a.executor(),
        }
    }

    /// The same kind on executor or playback button `n` (unchanged for other kinds).
    pub fn with_number(self, n: u16) -> Self {
        match self {
            SurfaceAction::ExecutorLevel(_) => SurfaceAction::ExecutorLevel(n),
            SurfaceAction::ExecutorGo(_) => SurfaceAction::ExecutorGo(n),
            SurfaceAction::ExecutorRelease(_) => SurfaceAction::ExecutorRelease(n),
            SurfaceAction::ExecutorFlash(_) => SurfaceAction::ExecutorFlash(n),
            SurfaceAction::PlaybackButton(_) => SurfaceAction::PlaybackButton(n),
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
    /// Executors per surface page: page 2 shifts executor faders and buttons by this many
    /// sequences (0 = no pages for executors).
    pub page_size: u16,
    /// Faders pick up the level they control before they change it.
    pub soft_takeover: bool,
    /// OSC feedback goes to the address the last OSC control came from, at this port (0 = no
    /// OSC feedback).
    pub osc_feedback_port: u16,
}

impl SurfaceSettings {
    /// Nothing set up (left out of the show file).
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
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

fn port_file(output: bool) -> Option<std::path::PathBuf> {
    crate::remote::config_dir().map(|d| d.join(if output { "midi-surface-feedback" } else { "midi-surface" }))
}

/// The MIDI input port chosen on this computer for control surfaces (read from the config
/// folder; call it once, not per frame).
pub fn saved_midi_port() -> Option<String> {
    saved_port(false)
}

/// The MIDI output port chosen on this computer for surface feedback (LEDs, motor faders).
pub fn saved_feedback_port() -> Option<String> {
    saved_port(true)
}

fn saved_port(output: bool) -> Option<String> {
    let s = crate::recovery::read_text(&port_file(output)?, 4096).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Remember the MIDI input port for control surfaces on this computer (None = no MIDI surface).
pub fn save_midi_port(port: Option<&str>) -> anyhow::Result<()> {
    save_port(false, port)
}

/// Remember the MIDI output port for surface feedback on this computer (None = no feedback).
pub fn save_feedback_port(port: Option<&str>) -> anyhow::Result<()> {
    save_port(true, port)
}

fn save_port(output: bool, port: Option<&str>) -> anyhow::Result<()> {
    let path = port_file(output).ok_or_else(|| anyhow::anyhow!("no config folder"))?;
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

/// MIDI output ports by name.
pub fn list_midi_outputs() -> Vec<String> {
    let Ok(m) = midir::MidiOutput::new("OpenQ") else { return Vec::new() };
    m.ports().iter().filter_map(|p| m.port_name(p).ok()).collect()
}

/// The MIDI message that shows `value` on a control: the same controller (value 0..=127) or
/// note (on at 127 / off) it sends. None for OSC.
pub fn midi_message(input: &SurfaceInput, value: f32) -> Option<[u8; 3]> {
    let v = (value.clamp(0.0, 1.0) * 127.0).round() as u8;
    match *input {
        SurfaceInput::MidiCc { channel, number } => Some([0xb0 | (channel & 0x0f), number & 0x7f, v]),
        SurfaceInput::MidiNote { channel, number } => Some([0x90 | (channel & 0x0f), number & 0x7f, if value >= 0.5 { 127 } else { 0 }]),
        // A program change has nothing to show.
        SurfaceInput::MidiProgram { .. } | SurfaceInput::Osc(_) => None,
    }
}

/// A MIDI message as a surface input and its value; None for anything else (clock, MTC, SysEx).
pub fn parse_midi(msg: &[u8]) -> Option<(SurfaceInput, f32)> {
    let (&status, data) = msg.split_first()?;
    let channel = status & 0x0f;
    if status & 0xf0 == 0xc0 {
        return Some((SurfaceInput::MidiProgram { channel, number: *data.first()? & 0x7f }, 1.0));
    }
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

/// What a fader controls (soft takeover follows it).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Target {
    Sequence(SequenceId),
    GrandMaster,
}

/// A fader within this of its level picks it up.
const PICKUP: f32 = 0.03;
/// A fader that picked up a level keeps it this long after its last move even if the level
/// reads differently (a replica's mirrored levels lag behind its own moves).
const HOLD: Duration = Duration::from_millis(500);

/// What button handling remembers between inputs.
#[derive(Default)]
pub struct Feel {
    /// Last value of each input (presses are rising edges).
    values: HashMap<SurfaceInput, f32>,
    /// Executor levels to go back to when a flash button is let go, by sequence.
    flashed: HashMap<SequenceId, f32>,
    /// Flash playback buttons held down: the input and the button it pressed (the page may
    /// change while it is held).
    held: HashMap<SurfaceInput, ButtonId>,
    /// Faders that picked up their level: what, the value last sent and when.
    caught: HashMap<SurfaceInput, (Target, f32, Instant)>,
    taps: Vec<Instant>,
    /// The surface's page, from 0.
    pub page: u16,
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
    /// Executors per page ([`SurfaceSettings::page_size`]).
    pub page_size: u16,
    /// Soft takeover on ([`SurfaceSettings::soft_takeover`]).
    pub soft_takeover: bool,
}

impl View<'_> {
    /// Number of surface pages: enough for every sequence and every button page.
    pub fn pages(&self) -> u16 {
        let seqs = match self.page_size {
            0 => 1,
            n => self.lighting.sequences.len().div_ceil(n as usize).max(1),
        };
        (seqs.max(self.lighting.button_page_count() as usize)).min(u16::MAX as usize) as u16
    }

    /// Sequence of executor `n` on page `page`.
    pub fn executor(&self, n: u16, page: u16) -> Option<SequenceId> {
        let i = (n as usize).checked_sub(1)? + page as usize * self.page_size as usize;
        self.lighting.sequences.get(i).map(|s| s.id)
    }

    /// Playback button `n` on page `page`.
    pub fn button(&self, n: u16, page: u16) -> Option<&LightButton> {
        let page = page.min(self.lighting.button_page_count() - 1);
        self.lighting.page_buttons(page).nth((n as usize).checked_sub(1)?)
    }

    /// A sequence is running (a cue is up).
    fn on(&self, seq: SequenceId) -> bool {
        self.status.iter().any(|x| x.id == seq && x.cue.is_some())
    }
}

/// The commands `action` gives for `value` on `input` (after `prev`, the input's previous value,
/// None the first time).
#[allow(clippy::too_many_arguments)]
fn act(action: SurfaceAction, input: &SurfaceInput, value: f32, prev: Option<f32>, view: &View, feel: &mut Feel, now: Instant) -> Vec<Command> {
    let value = value.clamp(0.0, 1.0);
    let was = prev.unwrap_or(0.0);
    let pressed = value >= 0.5 && was < 0.5;
    let released = value < 0.5 && was >= 0.5;
    let page = feel.page.min(view.pages() - 1);
    let seq = |n: u16| view.executor(n, page);
    let light = |c: LightControl| Command::Light(c);
    // Soft takeover: a fader acts once it has met its level.
    let mut fader = |target: Target, level: f32| -> bool {
        if !view.soft_takeover {
            return true;
        }
        let held = feel.caught.get(input).is_some_and(|(t, sent, at)| *t == target && ((level - sent).abs() < PICKUP || now.duration_since(*at) < HOLD));
        let meets = (value - level).abs() <= PICKUP || prev.is_some_and(|p| (p - level) * (value - level) <= 0.0);
        if held || meets {
            feel.caught.insert(input.clone(), (target, value, now));
            true
        } else {
            feel.caught.remove(input);
            false
        }
    };
    match action {
        SurfaceAction::ExecutorLevel(n) => match seq(n) {
            Some(s) if fader(Target::Sequence(s), view.masters.level(s)) => vec![light(LightControl::Level(s, value))],
            _ => Vec::new(),
        },
        SurfaceAction::GrandMaster if fader(Target::GrandMaster, view.masters.grand_master) => vec![light(LightControl::GrandMaster(value))],
        SurfaceAction::ExecutorGo(n) if pressed => seq(n).map(|s| light(LightControl::Go(s, None))).into_iter().collect(),
        SurfaceAction::ExecutorRelease(n) if pressed => seq(n).map(|s| light(LightControl::Release(Some(s), 1.0))).into_iter().collect(),
        SurfaceAction::ExecutorFlash(n) => {
            let Some(s) = seq(n) else { return Vec::new() };
            if pressed {
                feel.flashed.insert(s, view.masters.level(s));
                let mut v = vec![light(LightControl::Level(s, 1.0))];
                if !view.status.iter().any(|x| x.id == s && x.active) {
                    v.push(light(LightControl::Go(s, None)));
                }
                v
            } else if released {
                feel.flashed.remove(&s).map(|l| light(LightControl::Level(s, l))).into_iter().collect()
            } else {
                Vec::new()
            }
        }
        SurfaceAction::PlaybackButton(n) => {
            if released && let Some(id) = feel.held.remove(input) {
                return view.lighting.button(id).map(|b| light(LightControl::Release(Some(b.sequence_id()), b.fade as f32))).into_iter().collect();
            }
            let Some(b) = view.button(n, page).filter(|_| pressed) else { return Vec::new() };
            let (seq, go) = (b.sequence_id(), light(LightControl::Go(b.sequence_id(), Some(b.cue_id()))));
            if b.flash {
                feel.held.insert(input.clone(), b.id);
                vec![go]
            } else if view.on(seq) {
                vec![light(LightControl::Release(Some(seq), b.fade as f32))]
            } else {
                vec![go]
            }
        }
        SurfaceAction::PageNext | SurfaceAction::PagePrevious if pressed => {
            let next = if action == SurfaceAction::PageNext { page.saturating_add(1).min(view.pages() - 1) } else { page.saturating_sub(1) };
            if next != page {
                feel.page = next;
                // Faders must pick up the new page's levels.
                feel.caught.clear();
            }
            Vec::new()
        }
        SurfaceAction::Blackout if pressed => vec![light(LightControl::Blackout(!view.masters.blackout))],
        SurfaceAction::ReleaseAll if pressed => vec![light(LightControl::Release(None, view.release_fade))],
        SurfaceAction::TapTempo if pressed => tap(&mut feel.taps, now).map(|b| light(LightControl::Tempo(b))).into_iter().collect(),
        SurfaceAction::Go if pressed => vec![Command::Go],
        SurfaceAction::Stop if pressed => vec![Command::Stop],
        _ => Vec::new(),
    }
}

/// What each mapped control should show, for feedback: fader levels (motor faders), 1/0 for
/// buttons (LEDs: a running executor, a playback button that is on, blackout on, a page to go
/// to). Controls of actions with no state (GO, Stop, tap...) are left out; a control mapped
/// twice shows its first mapping.
pub fn feedback(settings: &SurfaceSettings, view: &View, page: u16) -> Vec<(SurfaceInput, f32)> {
    let page = page.min(view.pages() - 1);
    let flag = |b: bool| if b { 1.0 } else { 0.0 };
    let mut out: Vec<(SurfaceInput, f32)> = Vec::new();
    for m in &settings.mappings {
        if out.iter().any(|(i, _)| *i == m.input) {
            continue;
        }
        let active = |n: u16| view.executor(n, page).is_some_and(|s| view.status.iter().any(|x| x.id == s && x.active));
        let v = match m.action {
            SurfaceAction::ExecutorLevel(n) => view.executor(n, page).map_or(0.0, |s| view.masters.level(s)),
            SurfaceAction::GrandMaster => view.masters.grand_master,
            SurfaceAction::ExecutorGo(n) | SurfaceAction::ExecutorRelease(n) | SurfaceAction::ExecutorFlash(n) => flag(active(n)),
            SurfaceAction::PlaybackButton(n) => flag(view.button(n, page).is_some_and(|b| view.on(b.sequence_id()))),
            SurfaceAction::Blackout => flag(view.masters.blackout),
            SurfaceAction::PageNext => flag(page + 1 < view.pages()),
            SurfaceAction::PagePrevious => flag(page > 0),
            SurfaceAction::ReleaseAll | SurfaceAction::TapTempo | SurfaceAction::Go | SurfaceAction::Stop => continue,
        };
        out.push((m.input.clone(), v.clamp(0.0, 1.0)));
    }
    out
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
    // A value-less message (or a program change, which has no release) is a press then a release.
    let value = if matches!(input, SurfaceInput::MidiProgram { .. }) { None } else { value };
    let (value, after) = match value {
        Some(v) => (v, v),
        None => (1.0, 0.0),
    };
    let prev = feel.values.get(input).copied();
    let mut out = Vec::new();
    for a in actions {
        out.extend(act(a, input, value, prev, view, feel, now));
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

/// The [`View`] of a show.
fn view<'a>(show: &'a crate::show::Show, masters: &'a Masters, status: &'a [SequenceStatus]) -> View<'a> {
    let s = &show.ws.settings.surfaces;
    View { lighting: &show.ws.lighting, masters, status, release_fade: show.ws.settings.panic_fade as f32, page_size: s.page_size, soft_takeover: s.soft_takeover }
}

#[derive(Default)]
struct HubState {
    feel: Feel,
    last: Option<LastInput>,
    /// When each control last moved (feedback leaves a fader alone while it is being moved).
    touched: HashMap<SurfaceInput, Instant>,
    /// Where the last OSC control message came from (OSC feedback goes there).
    osc_peer: Option<std::net::IpAddr>,
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
        let view = view(&show, &masters, &status);
        st.touched.insert(input.clone(), now);
        let was = st.feel.values.get(&input).copied().unwrap_or(0.0);
        let pressed = matches!(input, SurfaceInput::MidiProgram { .. }) || value.is_none_or(|v| v >= 0.5 && was < 0.5);
        let fired = triggered(&show.ws.cue_list, &input, pressed);
        let mut cmds = resolve(&show.ws.settings.surfaces, builtin, &view, &mut st.feel, &input, value, now);
        let used = !cmds.is_empty() || !fired.is_empty() || builtin.is_some() || show.ws.settings.surfaces.mappings.iter().any(|m| m.input == input);
        cmds.extend(fired.into_iter().map(|id| Command::Cue(id, CueAction::Start)));
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

    /// The surface's page (from 0).
    pub fn page(&self) -> u16 {
        self.state.lock().feel.page
    }

    /// Go to a page (from 0; kept within the show's pages when used).
    pub fn set_page(&self, page: u16) {
        let mut st = self.state.lock();
        if st.feel.page != page {
            st.feel.page = page;
            st.feel.caught.clear();
        }
    }

    /// An OSC message came from `ip` (OSC feedback goes to the latest sender).
    pub fn osc_from(&self, ip: std::net::IpAddr) {
        self.state.lock().osc_peer = Some(ip);
    }

    /// Feedback for the show's mapped controls ([`feedback`]) on the current page, the OSC peer,
    /// and the controls moved within `recent` (left alone: a motor fader must not fight the hand).
    pub fn feedback(&self, show: &crate::show::Show, recent: Duration) -> (Vec<(SurfaceInput, f32)>, Option<std::net::IpAddr>) {
        let masters = show.media.light.masters().clone();
        let status = show.media.light.status();
        let v = view(show, &masters, &status);
        let st = self.state.lock();
        let now = Instant::now();
        let mut out = feedback(&show.ws.settings.surfaces, &v, st.feel.page);
        out.retain(|(i, _)| st.touched.get(i).is_none_or(|at| now.duration_since(*at) >= recent));
        (out, st.osc_peer)
    }
}

/// How often feedback is worked out and sent.
const FEEDBACK_EVERY: Duration = Duration::from_millis(40);
/// Feedback leaves a control alone this long after it moved.
const TOUCH_HOLD: Duration = Duration::from_millis(300);

/// Sends [`feedback`] to the controls: MIDI to an output port (looked for by name, reconnected
/// when it comes back), OSC to the last OSC sender at [`SurfaceSettings::osc_feedback_port`].
/// Only changes are sent (all again after a reconnect or a new OSC peer). Dropping it stops the
/// thread.
pub struct SurfaceFeedback {
    /// MIDI output port.
    pub port: Option<String>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<MidiStatus>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Values sent per control (MIDI controls by their 7-bit value, OSC by value).
#[derive(Default)]
pub struct FeedbackSent {
    midi: HashMap<SurfaceInput, [u8; 3]>,
    osc: HashMap<SurfaceInput, f32>,
}

impl FeedbackSent {
    /// The MIDI messages and OSC values to send for `wanted` (what changed since the last call).
    pub fn changes(&mut self, wanted: &[(SurfaceInput, f32)]) -> (Vec<[u8; 3]>, Vec<(String, f32)>) {
        let (mut midi, mut osc) = (Vec::new(), Vec::new());
        for (input, v) in wanted {
            match input {
                SurfaceInput::Osc(addr) => {
                    if self.osc.get(input).is_none_or(|x| (x - v).abs() > 1e-3) {
                        self.osc.insert(input.clone(), *v);
                        osc.push((addr.clone(), *v));
                    }
                }
                _ => {
                    let Some(m) = midi_message(input, *v) else { continue };
                    if self.midi.get(input) != Some(&m) {
                        self.midi.insert(input.clone(), m);
                        midi.push(m);
                    }
                }
            }
        }
        (midi, osc)
    }

    /// Send everything again (MIDI: a reconnect; OSC: a new peer).
    pub fn forget(&mut self, midi: bool, osc: bool) {
        if midi {
            self.midi.clear();
        }
        if osc {
            self.osc.clear();
        }
    }
}

impl SurfaceFeedback {
    /// Start sending feedback (MIDI to `port` if given; OSC when the show sets a port).
    pub fn start(port: Option<String>, show: SharedShow, hub: SurfaceHub) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(MidiStatus::default()));
        let (stop2, status2, name) = (stop.clone(), status.clone(), port.clone());
        let thread = std::thread::Builder::new()
            .name("openq-surface-feedback".into())
            .spawn(move || {
                let mut sent = FeedbackSent::default();
                let mut conn: Option<midir::MidiOutputConnection> = None;
                let mut tried: Option<Instant> = None;
                let mut checked = Instant::now();
                let sock = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok();
                let mut peer: Option<std::net::SocketAddr> = None;
                while !stop2.load(Ordering::Acquire) {
                    // MIDI output: connect, and notice a port that went away.
                    if let Some(name) = &name {
                        if conn.is_some() && checked.elapsed() >= crate::media::AUDIO_RETRY {
                            checked = Instant::now();
                            if !list_midi_outputs().contains(name) {
                                log::warn!("surface feedback: MIDI output lost ({name}); reconnecting…");
                                conn = None;
                                *status2.lock() = MidiStatus { connected: None, error: Some(format!("MIDI output lost: {name}")) };
                            }
                        }
                        if conn.is_none() && tried.is_none_or(|t| t.elapsed() >= crate::media::AUDIO_RETRY) {
                            tried = Some(Instant::now());
                            let r = (|| {
                                let m = midir::MidiOutput::new("OpenQ").map_err(|e| e.to_string())?;
                                let ports = m.ports();
                                let p = ports.iter().find(|p| m.port_name(p).ok().as_deref() == Some(name.as_str())).ok_or_else(|| format!("MIDI output not found: {name}"))?.clone();
                                m.connect(&p, "openq-feedback").map_err(|e| e.to_string())
                            })();
                            match r {
                                Ok(c) => {
                                    log::info!("surface feedback: MIDI output {name}");
                                    conn = Some(c);
                                    checked = Instant::now();
                                    sent.forget(true, false);
                                    *status2.lock() = MidiStatus { connected: Some(name.clone()), error: None };
                                }
                                Err(e) => {
                                    let mut st = status2.lock();
                                    if st.error.as_deref() != Some(e.as_str()) {
                                        log::warn!("surface feedback: {e}");
                                    }
                                    *st = MidiStatus { connected: None, error: Some(e) };
                                }
                            }
                        }
                    }
                    let r = crate::recovery::catch(|| {
                        let (wanted, ip, port) = {
                            let show = show.lock();
                            if show.ws.settings.surfaces.mappings.is_empty() {
                                return Ok(());
                            }
                            let (w, ip) = hub.feedback(&show, TOUCH_HOLD);
                            (w, ip, show.ws.settings.surfaces.osc_feedback_port)
                        };
                        let to = ip.filter(|_| port > 0).map(|ip| std::net::SocketAddr::new(ip, port));
                        if to != peer {
                            peer = to;
                            sent.forget(false, true);
                        }
                        let (midi, osc) = sent.changes(&wanted);
                        if let Some(c) = &mut conn {
                            for m in midi {
                                if let Err(e) = c.send(&m) {
                                    log::warn!("surface feedback: {e}");
                                    break;
                                }
                            }
                        }
                        if let (Some(sock), Some(to)) = (&sock, peer) {
                            for (addr, v) in osc {
                                let msg = rosc::OscPacket::Message(rosc::OscMessage { addr, args: vec![rosc::OscType::Float(v)] });
                                if let Ok(bytes) = rosc::encoder::encode(&msg) {
                                    let _ = sock.send_to(&bytes, to);
                                }
                            }
                        }
                        Ok(())
                    });
                    if let Err(e) = r {
                        log::warn!("surface feedback: {e:#}");
                    }
                    std::thread::park_timeout(FEEDBACK_EVERY);
                }
            })
            .ok();
        Self { port, stop, status, thread }
    }

    /// State of the MIDI output.
    pub fn status(&self) -> MidiStatus {
        self.status.lock().clone()
    }
}

impl Drop for SurfaceFeedback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
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
        let view = View { lighting: &l, masters, status, release_fade: 2.0, page_size: s.page_size, soft_takeover: s.soft_takeover };
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

    #[test]
    fn program_changes_parse() {
        assert_eq!(parse_midi(&[0xc3, 5]), Some((SurfaceInput::MidiProgram { channel: 3, number: 5 }, 1.0)));
        assert_eq!(parse_midi(&[0xc0]), None, "short");
        assert_eq!(midi_message(&SurfaceInput::MidiProgram { channel: 0, number: 5 }, 1.0), None, "no feedback");
        // Each program change is a press, mapped GO fires every time.
        let pc = SurfaceInput::MidiProgram { channel: 0, number: 5 };
        let s = SurfaceSettings { mappings: vec![Mapping { input: pc.clone(), action: SurfaceAction::Go }], ..Default::default() };
        let (mut feel, m) = (Feel::default(), Masters::default());
        for _ in 0..2 {
            assert!(matches!(run(&s, &mut feel, &pc, Some(1.0), &m, &[])[..], [Command::Go]));
        }
    }

    /// Cues fire on their MIDI trigger: kind, number and channel (or any channel) must match,
    /// once per press, armed cues only, in any list and group.
    #[test]
    fn midi_triggers_fire_cues() {
        use crate::model::{Cue, CueKind};
        let show: SharedShow = Arc::new(Mutex::new(crate::show::Show::new(crate::media::Prober::new())));
        let wait = |trigger| Cue { midi_trigger: Some(trigger), ..Cue::new(CueKind::Wait { duration: 60.0 }) };
        let note36 = wait(MidiTrigger { kind: MidiTriggerKind::Note, channel: Some(9), number: 36 });
        let pc5 = wait(MidiTrigger { kind: MidiTriggerKind::Program, channel: None, number: 5 });
        let cc20 = wait(MidiTrigger { kind: MidiTriggerKind::Control, channel: Some(0), number: 20 });
        let mut off = wait(MidiTrigger { kind: MidiTriggerKind::Note, channel: None, number: 36 });
        off.armed = false;
        let ids = (note36.id, pc5.id, cc20.id);
        {
            let mut s = show.lock();
            let group = Cue::new(CueKind::Group { mode: Default::default(), children: vec![cc20] });
            s.ws.cue_list.cues = vec![note36, pc5, group, off];
            // Replicas queue the commands: the test reads them there.
            s.replica = true;
        }
        let hub = SurfaceHub::new();
        let fired = |input: SurfaceInput, v: f32| {
            hub.input(&show, input, Some(v), None);
            hub.take_pending().into_iter().map(|c| match c {
                Command::Cue(id, CueAction::Start) => id,
                other => panic!("{other:?}"),
            }).collect::<Vec<_>>()
        };
        let note = |channel| SurfaceInput::MidiNote { channel, number: 36 };
        assert_eq!(fired(note(9), 1.0), vec![ids.0]);
        assert!(fired(note(9), 1.0).is_empty(), "held");
        assert!(fired(note(9), 0.0).is_empty(), "note off");
        assert_eq!(fired(note(9), 1.0), vec![ids.0], "again");
        assert!(fired(note(0), 1.0).is_empty(), "other channel");
        let pc = |channel| SurfaceInput::MidiProgram { channel, number: 5 };
        assert_eq!(fired(pc(2), 1.0), vec![ids.1], "any channel");
        assert_eq!(fired(pc(7), 1.0), vec![ids.1], "every program change");
        let cc = SurfaceInput::MidiCc { channel: 0, number: 20 };
        assert_eq!(fired(cc.clone(), 1.0), vec![ids.2], "inside a group");
        assert!(fired(cc.clone(), 0.9).is_empty());
        assert!(fired(cc.clone(), 0.0).is_empty());
        assert!(fired(SurfaceInput::Osc("/x".into()), 1.0).is_empty());
        assert_eq!(cc.midi_trigger(), Some(MidiTrigger { kind: MidiTriggerKind::Control, channel: Some(0), number: 20 }));
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

    /// A design with sequences 1..=6 and buttons 1..=4 (1, 2 on page 0; 3 a flash on page 1; 4 on page 1).
    fn paged() -> Lighting {
        let mut l = Lighting::default();
        for id in 1..=6 {
            l.sequences.push(Sequence { id, ..Default::default() });
        }
        for id in 1..=4 {
            l.buttons.push(LightButton { id, flash: id == 3, page: (id > 2) as u16, ..Default::default() });
        }
        l.button_pages = vec!["A".into(), "B".into()];
        l
    }

    #[test]
    fn pages_shift_executors_and_buttons() {
        let l = paged();
        let s = SurfaceSettings {
            page_size: 4,
            mappings: vec![
                Mapping { input: SurfaceInput::MidiCc { channel: 0, number: 1 }, action: SurfaceAction::ExecutorLevel(2) },
                Mapping { input: SurfaceInput::MidiNote { channel: 0, number: 1 }, action: SurfaceAction::PlaybackButton(1) },
                Mapping { input: SurfaceInput::MidiNote { channel: 0, number: 2 }, action: SurfaceAction::PageNext },
                Mapping { input: SurfaceInput::MidiNote { channel: 0, number: 3 }, action: SurfaceAction::PagePrevious },
            ],
            ..Default::default()
        };
        let (m, mut feel) = (Masters::default(), Feel::default());
        let mut status = Vec::new();
        let go = |feel: &mut Feel, input: SurfaceInput, v: f32, status: &[SequenceStatus]| {
            let view = View { lighting: &l, masters: &m, status, release_fade: 1.0, page_size: s.page_size, soft_takeover: false };
            resolve(&s, None, &view, feel, &input, Some(v), Instant::now())
        };
        let (fader, button, next, prev) = (SurfaceInput::MidiCc { channel: 0, number: 1 }, SurfaceInput::MidiNote { channel: 0, number: 1 }, SurfaceInput::MidiNote { channel: 0, number: 2 }, SurfaceInput::MidiNote { channel: 0, number: 3 });
        assert!(matches!(go(&mut feel, fader.clone(), 0.5, &status)[..], [Command::Light(LightControl::Level(2, _))]));
        // Playback button 1 on page 1 = button 1: a toggle; on → off.
        let b1 = l.button(1).unwrap();
        assert!(matches!(go(&mut feel, button.clone(), 1.0, &status)[..], [Command::Light(LightControl::Go(s, Some(_)))] if s == b1.sequence_id()));
        go(&mut feel, button.clone(), 0.0, &status);
        status.push(SequenceStatus { id: b1.sequence_id(), cue: Some(b1.cue_id()), active: true, level: 1.0 });
        assert!(matches!(go(&mut feel, button.clone(), 1.0, &status)[..], [Command::Light(LightControl::Release(Some(_), _))]));
        go(&mut feel, button.clone(), 0.0, &status);
        // Next page: executor 2 → sequence 6, button 1 → button 3 (a flash: GO, then release on let go).
        go(&mut feel, next.clone(), 1.0, &status);
        go(&mut feel, next.clone(), 0.0, &status);
        assert_eq!(feel.page, 1);
        assert!(matches!(go(&mut feel, fader.clone(), 0.4, &status)[..], [Command::Light(LightControl::Level(6, _))]));
        let b3 = l.button(3).unwrap();
        assert!(matches!(go(&mut feel, button.clone(), 1.0, &status)[..], [Command::Light(LightControl::Go(s, _))] if s == b3.sequence_id()));
        // Back to page 1 while held: the release still goes to button 3.
        go(&mut feel, prev.clone(), 1.0, &status);
        assert_eq!(feel.page, 0);
        assert!(matches!(go(&mut feel, button.clone(), 0.0, &status)[..], [Command::Light(LightControl::Release(Some(s), _))] if s == b3.sequence_id()));
        // Pages stop at the ends: two pages (6 sequences / 4, and two button pages).
        for _ in 0..3 {
            go(&mut feel, next.clone(), 1.0, &status);
            go(&mut feel, next.clone(), 0.0, &status);
        }
        assert_eq!(feel.page, 1);
    }

    #[test]
    fn soft_takeover_waits_for_the_level() {
        let l = design();
        let s = SurfaceSettings { mappings: SurfaceSettings::executor_bank(1, 0, 1, 36), soft_takeover: true, ..Default::default() };
        let mut m = Masters::default();
        m.levels.insert(7, 0.6);
        let mut feel = Feel::default();
        let cc = SurfaceInput::MidiCc { channel: 0, number: 1 };
        let move_to = |feel: &mut Feel, m: &Masters, v: f32| {
            let view = View { lighting: &l, masters: m, status: &[], release_fade: 1.0, page_size: 0, soft_takeover: true };
            resolve(&s, None, &view, feel, &cc, Some(v), Instant::now())
        };
        assert!(move_to(&mut feel, &m, 0.0).is_empty(), "far from 60 %");
        assert!(move_to(&mut feel, &m, 0.3).is_empty());
        // Crossing the level picks it up.
        assert_eq!(move_to(&mut feel, &m, 0.7).len(), 1);
        m.levels.insert(7, 0.7);
        assert_eq!(move_to(&mut feel, &m, 0.5).len(), 1, "caught: follows");
        // The level changes elsewhere (the Playback page): the fader lets go until it meets it.
        m.levels.insert(7, 0.1);
        feel.caught.values_mut().for_each(|c| c.2 -= HOLD);
        assert!(move_to(&mut feel, &m, 0.45).is_empty());
        assert_eq!(move_to(&mut feel, &m, 0.12).len(), 1, "close enough");
    }

    #[test]
    fn feedback_shows_levels_and_states() {
        let l = paged();
        let mut s = SurfaceSettings { page_size: 4, ..Default::default() };
        let cc = |n| SurfaceInput::MidiCc { channel: 1, number: n };
        let note = |n| SurfaceInput::MidiNote { channel: 1, number: n };
        for (i, a) in [SurfaceAction::ExecutorLevel(1), SurfaceAction::GrandMaster].into_iter().enumerate() {
            s.mappings.push(Mapping { input: cc(i as u8), action: a });
        }
        for (i, a) in [SurfaceAction::ExecutorGo(1), SurfaceAction::PlaybackButton(2), SurfaceAction::Blackout, SurfaceAction::PageNext, SurfaceAction::PagePrevious, SurfaceAction::Go].into_iter().enumerate() {
            s.mappings.push(Mapping { input: note(i as u8), action: a });
        }
        s.mappings.push(Mapping { input: note(0), action: SurfaceAction::Stop });
        let mut m = Masters { grand_master: 0.5, blackout: true, ..Default::default() };
        m.levels.insert(1, 0.25);
        let b2 = l.button(2).unwrap();
        let status = [SequenceStatus { id: 1, cue: None, active: true, level: 0.25 }, SequenceStatus { id: b2.sequence_id(), cue: Some(b2.cue_id()), active: true, level: 1.0 }];
        let view = View { lighting: &l, masters: &m, status: &status, release_fade: 1.0, page_size: 4, soft_takeover: false };
        let fb = feedback(&s, &view, 0);
        let get = |i: SurfaceInput| fb.iter().find(|(x, _)| *x == i).map(|(_, v)| *v);
        assert_eq!(get(cc(0)), Some(0.25));
        assert_eq!(get(cc(1)), Some(0.5));
        assert_eq!(get(note(0)), Some(1.0), "executor 1 running; its second mapping ignored");
        assert_eq!(get(note(1)), Some(1.0), "button 2 on");
        assert_eq!(get(note(2)), Some(1.0), "blackout");
        assert_eq!((get(note(3)), get(note(4))), (Some(1.0), Some(0.0)), "a next page, no previous");
        assert_eq!(get(note(5)), None, "GO has no state");
        let fb2 = feedback(&s, &view, 1);
        assert_eq!(fb2.iter().find(|(x, _)| *x == cc(0)).unwrap().1, 1.0, "page 2: sequence 5 at full");
        // Only changes are sent, as MIDI the controls understand.
        let mut sent = FeedbackSent::default();
        let (midi, osc) = sent.changes(&fb);
        assert_eq!(midi.len(), 7);
        assert!(osc.is_empty());
        assert!(midi.contains(&[0xb1, 0, 32]) && midi.contains(&[0x91, 1, 127]) && midi.contains(&[0x91, 4, 0]));
        assert!(sent.changes(&fb).0.is_empty());
        assert_eq!(sent.changes(&fb2).0.len(), 5, "fader, executor LED, button LED, both page LEDs");
        sent.forget(true, false);
        assert_eq!(sent.changes(&fb2).0.len(), 7);
        let (_, osc) = sent.changes(&[(SurfaceInput::Osc("/fader".into()), 0.5)]);
        assert_eq!(osc, [("/fader".to_string(), 0.5)]);
        assert_eq!(midi_message(&SurfaceInput::Osc("/x".into()), 1.0), None);
    }

}
