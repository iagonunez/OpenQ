//! Show data model: cues, cue tree operations and the workspace file format.

use crate::animation::Animation;
use crate::effects::{AudioEffect, VideoEffect};
use crate::spatial::{AudioObject, ObjectAudio, Speaker};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// Unique id of a cue (stable across saves).
pub type CueId = Uuid;

/// Lowest level (in dB) we treat as audible; anything at or below is silence.
pub const MIN_DB: f32 = -60.0;

/// Decibels to linear gain (`MIN_DB` and below = silence).
pub fn db_to_gain(db: f32) -> f32 {
    if db <= MIN_DB { 0.0 } else { 10f32.powf(db / 20.0) }
}

/// Linear gain to decibels (silence = `MIN_DB`).
pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.0 { MIN_DB } else { (20.0 * gain.log10()).max(MIN_DB) }
}

/// The kind of MIDI message a [`MidiTrigger`] waits for.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MidiTriggerKind {
    /// Note on (velocity above 0).
    #[default]
    Note,
    /// Program change.
    Program,
    /// Control change rising to 64 or more (a button sending 127/0).
    Control,
}

impl MidiTriggerKind {
    /// Every kind, for menus.
    pub const ALL: [MidiTriggerKind; 3] = [MidiTriggerKind::Note, MidiTriggerKind::Program, MidiTriggerKind::Control];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            MidiTriggerKind::Note => crate::i18n::t("Note on"),
            MidiTriggerKind::Program => crate::i18n::t("Program change"),
            MidiTriggerKind::Control => crate::i18n::t("Control change"),
        }
    }
}

/// A MIDI message that fires a cue. It arrives on this computer's control surface MIDI input
/// (Settings → Control surfaces), like the surface mappings.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct MidiTrigger {
    /// Message kind.
    pub kind: MidiTriggerKind,
    /// MIDI channel, 0-based (0..=15); None = any channel.
    pub channel: Option<u8>,
    /// Note, program or controller number (0..=127).
    pub number: u8,
}

impl MidiTrigger {
    /// Short text for the UI ("Note 36 · ch 10", "PC 5 · any ch").
    pub fn label(&self) -> String {
        let n = self.number;
        let what = match self.kind {
            MidiTriggerKind::Note => format!("Note {n}"),
            MidiTriggerKind::Program => format!("PC {n}"),
            MidiTriggerKind::Control => format!("CC {n}"),
        };
        match self.channel {
            Some(c) => format!("{what} · ch {}", c + 1),
            None => format!("{what} · {}", crate::i18n::t("any ch")),
        }
    }
}

/// A time of day that fires a cue, on this computer's clock and time zone.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct WallClockTrigger {
    /// Seconds after local midnight (0..86400).
    pub time: u32,
    /// Days it fires on: bit 0 = Monday ... bit 6 = Sunday; 0 = every day.
    pub days: u8,
}

impl WallClockTrigger {
    /// Whether it fires on this weekday (0 = Monday ... 6 = Sunday).
    pub fn on_day(&self, weekday: u8) -> bool {
        self.days & 0x7f == 0 || self.days & (1 << weekday.min(6)) != 0
    }

    /// Short text for the UI ("21:30:00", "21:30:00 · Mon Fri").
    pub fn label(&self) -> String {
        let t = self.time.min(86_399);
        let clock = format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60);
        if self.days & 0x7f == 0 || self.days & 0x7f == 0x7f {
            return clock;
        }
        let days: Vec<&str> = (0..7).filter(|d| self.days & (1 << d) != 0).map(|d| crate::wallclock::day_name(d)).collect();
        format!("{clock} · {}", days.join(" "))
    }
}

/// What happens after a cue fires.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ContinueMode {
    /// Wait for the next GO.
    #[default]
    DoNotContinue,
    /// Fire the next cue once this cue's post-wait has elapsed.
    AutoContinue,
    /// Fire the next cue when this cue's action completes.
    AutoFollow,
}

impl ContinueMode {
    /// Every mode, for menus.
    pub const ALL: [ContinueMode; 3] =
        [ContinueMode::DoNotContinue, ContinueMode::AutoContinue, ContinueMode::AutoFollow];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            ContinueMode::DoNotContinue => crate::i18n::t("Do not continue"),
            ContinueMode::AutoContinue => crate::i18n::t("Auto-continue"),
            ContinueMode::AutoFollow => crate::i18n::t("Auto-follow"),
        }
    }
}

/// What a trigger (hotkey, MIDI trigger, wall clock, timecode) does when it fires a cue that is
/// still running. GO, OSC and Start cues always restart it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SecondTrigger {
    /// Start it again from the top.
    #[default]
    Restart,
    /// Nothing: it carries on.
    Ignore,
    /// Stop it at once.
    Stop,
    /// Pause it, or resume it when paused.
    PauseResume,
    /// Fade it out over the show's Stop-all fade time, then stop it (a third press stops it at once).
    FadeOut,
}

impl SecondTrigger {
    /// Every action, for menus.
    pub const ALL: [SecondTrigger; 5] =
        [SecondTrigger::Restart, SecondTrigger::Ignore, SecondTrigger::Stop, SecondTrigger::PauseResume, SecondTrigger::FadeOut];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            SecondTrigger::Restart => crate::i18n::t("Restart it"),
            SecondTrigger::Ignore => crate::i18n::t("Do nothing"),
            SecondTrigger::Stop => crate::i18n::t("Stop it"),
            SecondTrigger::PauseResume => crate::i18n::t("Pause / resume it"),
            SecondTrigger::FadeOut => crate::i18n::t("Fade it out and stop"),
        }
    }

    fn is_default(&self) -> bool {
        *self == SecondTrigger::Restart
    }
}

/// How a Group cue plays its children.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum GroupMode {
    /// GO fires the first child and moves the playhead into the group.
    #[default]
    StartFirstAndEnter,
    /// Fire the first child (children chain via their own continue modes); playhead skips the group.
    StartFirst,
    /// Fire every child at once.
    StartAll,
    /// Fire one child at random.
    StartRandom,
    /// Start every child at its place on a timeline: a child's pre-wait is its start time from
    /// the group's start, and the children's continue modes are ignored.
    Timeline,
}

impl GroupMode {
    /// Every mode, for menus.
    pub const ALL: [GroupMode; 5] = [
        GroupMode::StartFirstAndEnter,
        GroupMode::StartFirst,
        GroupMode::StartAll,
        GroupMode::StartRandom,
        GroupMode::Timeline,
    ];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            GroupMode::StartFirstAndEnter => crate::i18n::t("Start first child and enter group"),
            GroupMode::StartFirst => crate::i18n::t("Start first child and go to next cue (playlist)"),
            GroupMode::StartAll => crate::i18n::t("Start all children simultaneously"),
            GroupMode::StartRandom => crate::i18n::t("Start random child and go to next cue"),
            GroupMode::Timeline => crate::i18n::t("Timeline: start children at their times"),
        }
    }
}

impl FadeCurve {
    /// Gain (0..1) of a fade-in at `t` (0..1) of the way through; a fade-out uses `1 - t`.
    pub fn gain(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            FadeCurve::Linear => t,
            FadeCurve::SCurve => t * t * (3.0 - 2.0 * t),
            FadeCurve::Logarithmic if t <= 0.0 => 0.0,
            FadeCurve::Logarithmic => db_to_gain(MIN_DB * (1.0 - t)),
            FadeCurve::Custom(h) => custom_curve(h, t),
        }
    }

    /// Progress (0..1) at `t` of the way through, for values without a level law (opacity,
    /// geometry): Logarithmic is linear here.
    pub fn progress(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            FadeCurve::Linear | FadeCurve::Logarithmic => t,
            FadeCurve::SCurve => t * t * (3.0 - 2.0 * t),
            FadeCurve::Custom(h) => custom_curve(h, t),
        }
    }

    /// The [`FadeCurve::Custom`] handle heights that draw (about) this curve, so editing a preset
    /// on the graph starts from what it showed. `falling` = the fade lowers the level, where the
    /// logarithmic law drops fast and then tails off.
    pub fn handles(self, falling: bool) -> [f32; 2] {
        match self {
            FadeCurve::Linear => [1.0 / 3.0, 2.0 / 3.0],
            FadeCurve::SCurve => [0.0, 1.0],
            FadeCurve::Logarithmic if falling => [1.0, 1.0],
            FadeCurve::Logarithmic => [0.0, 0.0],
            FadeCurve::Custom(h) => h,
        }
    }
}

/// A cubic Bézier from (0,0) to (1,1) whose inner control points sit at 1/3 and 2/3 of the time
/// with heights `h`: x(s) = s, so y is a plain polynomial in `t` (no solving, cheap enough for
/// the audio thread's per-sample ramps). `[1/3, 2/3]` is linear, `[0, 1]` the S-curve.
fn custom_curve(h: [f32; 2], t: f32) -> f32 {
    let m = 1.0 - t;
    3.0 * m * m * t * h[0] + 3.0 * m * t * t * h[1] + t * t * t
}

/// Shape of a fade over time.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum FadeCurve {
    /// Constant rate.
    Linear,
    /// Slow start and end.
    #[default]
    SCurve,
    /// Linear in dB (perceptually even).
    Logarithmic,
    /// Drawn on the fade's curve graph: the heights (0..1) of the two handles at 1/3 and 2/3 of
    /// the time (see [`FadeCurve::gain`]).
    Custom([f32; 2]),
}

impl FadeCurve {
    /// Every curve, for menus.
    pub const ALL: [FadeCurve; 3] = [FadeCurve::Linear, FadeCurve::SCurve, FadeCurve::Logarithmic];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            FadeCurve::Linear => crate::i18n::t("Linear"),
            FadeCurve::SCurve => crate::i18n::t("S-curve"),
            FadeCurve::Logarithmic => crate::i18n::t("Logarithmic"),
            FadeCurve::Custom(_) => crate::i18n::t("Custom"),
        }
    }

    /// Interpolate between two linear gains at progress `t` in [0, 1].
    pub fn interpolate(self, from: f32, to: f32, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            FadeCurve::Linear => from + (to - from) * t,
            FadeCurve::SCurve => {
                let s = t * t * (3.0 - 2.0 * t);
                from + (to - from) * s
            }
            FadeCurve::Logarithmic => {
                // Interpolate in dB, with silence mapped to a deep floor so fades to/from
                // silence still move smoothly.
                const FLOOR: f32 = -80.0;
                let db = |g: f32| if g <= 0.0 { FLOOR } else { (20.0 * g.log10()).max(FLOOR) };
                let (a, b) = (db(from), db(to));
                let v = a + (b - a) * t;
                if v <= FLOOR + 0.01 { 0.0 } else { 10f32.powf(v / 20.0) }
            }
            FadeCurve::Custom(h) => from + (to - from) * custom_curve(h, t),
        }
    }
}

/// Colour tag of a cue row.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CueColor {
    /// No colour.
    #[default]
    None,
    /// Red.
    Red,
    /// Orange.
    Orange,
    /// Yellow.
    Yellow,
    /// Green.
    Green,
    /// Blue.
    Blue,
    /// Purple.
    Purple,
}

impl CueColor {
    /// Every colour, for menus.
    pub const ALL: [CueColor; 7] = [
        CueColor::None,
        CueColor::Red,
        CueColor::Orange,
        CueColor::Yellow,
        CueColor::Green,
        CueColor::Blue,
        CueColor::Purple,
    ];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            CueColor::None => crate::i18n::t("None"),
            CueColor::Red => crate::i18n::t("Red"),
            CueColor::Orange => crate::i18n::t("Orange"),
            CueColor::Yellow => crate::i18n::t("Yellow"),
            CueColor::Green => crate::i18n::t("Green"),
            CueColor::Blue => crate::i18n::t("Blue"),
            CueColor::Purple => crate::i18n::t("Purple"),
        }
    }

    /// Display colour (None for no colour).
    pub fn rgb(self) -> Option<[u8; 3]> {
        match self {
            CueColor::None => None,
            CueColor::Red => Some([200, 60, 60]),
            CueColor::Orange => Some([220, 130, 40]),
            CueColor::Yellow => Some([210, 190, 50]),
            CueColor::Green => Some([70, 170, 80]),
            CueColor::Blue => Some([60, 120, 210]),
            CueColor::Purple => Some([150, 80, 200]),
        }
    }
}

/// How a picture fits its stage.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum VideoFit {
    /// Whole picture visible, letterboxed.
    #[default]
    Fit,
    /// Cover the stage, cropping the overflow.
    Fill,
    /// Fill the stage exactly, distorting if needed.
    Stretch,
    /// Pixel-for-pixel on the stage canvas (used by text cues).
    Native,
}

impl VideoFit {
    /// Every fit, for menus.
    pub const ALL: [VideoFit; 4] = [VideoFit::Fit, VideoFit::Fill, VideoFit::Stretch, VideoFit::Native];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            VideoFit::Fit => crate::i18n::t("Fit (letterbox)"),
            VideoFit::Fill => crate::i18n::t("Fill (crop)"),
            VideoFit::Stretch => crate::i18n::t("Stretch"),
            VideoFit::Native => crate::i18n::t("Native size"),
        }
    }
}

/// How a picture layer combines with the layers below it on its stage.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum BlendMode {
    /// Covers what is below (by its opacity).
    #[default]
    Normal,
    /// Adds its light: black disappears, bright parts glow.
    Add,
    /// Like Add, but never brighter than white: black disappears, highlights merge softly.
    Screen,
    /// Darkens what is below by its colour: white disappears (masks, vignettes, gobos).
    Multiply,
    /// Keeps the brighter of the two, channel by channel.
    Lighten,
    /// Keeps the darker of the two, channel by channel.
    Darken,
    /// Takes its light away from what is below.
    Subtract,
}

impl BlendMode {
    /// Every mode, for menus.
    pub const ALL: [BlendMode; 7] = [
        BlendMode::Normal,
        BlendMode::Add,
        BlendMode::Screen,
        BlendMode::Multiply,
        BlendMode::Lighten,
        BlendMode::Darken,
        BlendMode::Subtract,
    ];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            BlendMode::Normal => crate::i18n::t("Normal"),
            BlendMode::Add => crate::i18n::t("Add"),
            BlendMode::Screen => crate::i18n::t("Screen"),
            BlendMode::Multiply => crate::i18n::t("Multiply"),
            BlendMode::Lighten => crate::i18n::t("Lighten"),
            BlendMode::Darken => crate::i18n::t("Darken"),
            BlendMode::Subtract => crate::i18n::t("Subtract"),
        }
    }
}

/// Most source channels a cue's routing matrix handles (file, soundtrack or input channels).
pub const MAX_SRC_CHANNELS: usize = 64;
/// Most output channels a cue's routing matrix handles (a cue's outputs on its audio patch).
pub const MAX_OUT_CHANNELS: usize = 64;
/// Most EQ bands per cue.
pub const MAX_EQ_BANDS: usize = 8;

/// Filter type of an EQ band.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum EqKind {
    /// Remove lows below the frequency.
    HighPass,
    /// Boost/cut everything below the frequency.
    LowShelf,
    /// Boost/cut around the frequency.
    Peak,
    /// Boost/cut everything above the frequency.
    HighShelf,
    /// Remove highs above the frequency.
    LowPass,
}

impl EqKind {
    /// Every type, for menus.
    pub const ALL: [EqKind; 5] = [EqKind::HighPass, EqKind::LowShelf, EqKind::Peak, EqKind::HighShelf, EqKind::LowPass];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            EqKind::HighPass => crate::i18n::t("High-pass"),
            EqKind::LowShelf => crate::i18n::t("Low shelf"),
            EqKind::Peak => crate::i18n::t("Peak"),
            EqKind::HighShelf => crate::i18n::t("High shelf"),
            EqKind::LowPass => crate::i18n::t("Low-pass"),
        }
    }

    /// True if the gain setting applies to this type.
    pub fn has_gain(self) -> bool {
        matches!(self, EqKind::LowShelf | EqKind::Peak | EqKind::HighShelf)
    }
}

/// One equaliser band.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct EqBand {
    /// Band is active.
    pub enabled: bool,
    /// Filter type.
    pub kind: EqKind,
    /// Frequency in Hz.
    pub freq: f32,
    /// Gain in dB (peak and shelf types).
    pub gain_db: f32,
    /// Bandwidth / resonance.
    pub q: f32,
}

/// A cue's equaliser.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Eq {
    /// EQ is active (off = bypassed).
    pub enabled: bool,
    /// Bands, applied in order.
    pub bands: Vec<EqBand>,
}

impl Default for Eq {
    fn default() -> Self {
        let b = |enabled, kind, freq, q| EqBand { enabled, kind, freq, gain_db: 0.0, q };
        Self {
            enabled: false,
            bands: vec![
                b(false, EqKind::HighPass, 80.0, 0.707),
                b(true, EqKind::LowShelf, 120.0, 0.707),
                b(true, EqKind::Peak, 500.0, 1.0),
                b(true, EqKind::Peak, 2000.0, 1.0),
                b(true, EqKind::HighShelf, 8000.0, 0.707),
                b(false, EqKind::LowPass, 16000.0, 0.707),
            ],
        }
    }
}

/// Default source→output mapping as linear gains `[src][out]`: mono feeds the first two
/// outputs, multichannel maps 1:1, everything folds down to a mono output.
pub fn default_routing(src: usize, out: usize) -> Vec<Vec<f32>> {
    let mut m = vec![vec![0.0; out]; src];
    for (s, row) in m.iter_mut().enumerate() {
        for (o, g) in row.iter_mut().enumerate() {
            *g = if out == 1 {
                1.0 / src as f32
            } else if src == 1 {
                if o < 2 { 1.0 } else { 0.0 }
            } else if s == o {
                1.0
            } else {
                0.0
            };
        }
    }
    m
}

/// Position/scale/rotation of a video layer on its stage.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct Geometry {
    /// Offset as a fraction of the stage width (0 = centred, 0.5 = half a stage to the right).
    pub x: f32,
    /// Offset as a fraction of the stage height (positive = down).
    pub y: f32,
    /// Horizontal scale (1 = fit size).
    pub scale_x: f32,
    /// Vertical scale (1 = fit size).
    pub scale_y: f32,
    /// Degrees clockwise.
    pub rotation: f32,
    /// Fraction of the image trimmed from each edge: left, top, right, bottom. The rest of the
    /// picture stays where it was.
    pub crop: [f32; 4],
}

impl Default for Geometry {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0, scale_x: 1.0, scale_y: 1.0, rotation: 0.0, crop: [0.0; 4] }
    }
}

impl Geometry {
    /// Interpolate towards `to` (t = 0..1), as fades do.
    pub fn lerp(&self, to: &Geometry, t: f32) -> Geometry {
        let l = |a: f32, b: f32| a + (b - a) * t;
        Geometry {
            x: l(self.x, to.x),
            y: l(self.y, to.y),
            scale_x: l(self.scale_x, to.scale_x),
            scale_y: l(self.scale_y, to.scale_y),
            rotation: l(self.rotation, to.rotation),
            crop: std::array::from_fn(|i| l(self.crop[i], to.crop[i])),
        }
    }
}

/// Unique id of a stage.
pub type StageId = Uuid;

/// A video output (projector/screen). Its canvas is warped onto the window with four corner
/// pins for keystone correction / projection mapping.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Stage {
    /// Stage id.
    pub id: StageId,
    /// Name shown in menus and the window title.
    pub name: String,
    /// Canvas width in pixels.
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
    /// Window-normalised positions of the canvas corners: top-left, top-right, bottom-right,
    /// bottom-left.
    pub corners: [[f32; 2]; 4],
    /// Show the content of another stage (a wider "surface") instead of its own cues. Used with
    /// `crop` to split one picture across several projectors.
    pub span_of: Option<StageId>,
    /// Part of the source canvas this output shows: x0, y0, x1, y1 (normalised).
    pub crop: [f32; 4],
    /// Soft-edge blend widths as a fraction of this output: left, top, right, bottom.
    pub blend: [f32; 4],
    /// Projector gamma used to shape the blend so overlapping light adds up evenly.
    pub blend_gamma: f32,
    /// Black level lift outside the blend zones (0..0.25): projectors never reach true black, so
    /// where two overlap the "black" is brighter; lifting the rest evens it out.
    pub black_lift: f32,
    /// System display (by name) the stage goes fullscreen on; `None` = an ordinary window.
    pub output: Option<String>,
    /// Close the output window when nothing has shown on it for a moment (it opens again with the
    /// next cue), instead of staying black. Not while its mapping is edited.
    pub close_when_idle: bool,
    /// Hides parts of this output (set pieces the projector must not light), over its whole
    /// picture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask: Option<Mask>,
}

/// A picture that hides parts of a layer or an output: white shows, black or transparent hides,
/// greys show partly (luminance × alpha).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Mask {
    /// The picture (stretched over the layer, or the output).
    pub path: PathBuf,
    /// Black shows and white hides.
    pub invert: bool,
}

impl Stage {
    /// Crop that shows the whole source canvas.
    pub const FULL_CROP: [f32; 4] = [0.0, 0.0, 1.0, 1.0];
}

impl Stage {
    /// Corner pins with no warping.
    pub const IDENTITY: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];

    /// A 1920×1080 stage with no mapping.
    pub fn new(name: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            width: 1920,
            height: 1080,
            corners: Self::IDENTITY,
            span_of: None,
            crop: Self::FULL_CROP,
            blend: [0.0; 4],
            blend_gamma: 2.2,
            black_lift: 0.0,
            output: None,
            close_when_idle: false,
            mask: None,
        }
    }
}

impl Default for Stage {
    fn default() -> Self {
        Self::new("Main")
    }
}

/// Split stage `surface` across `n` outputs overlapping by `overlap` of an output's width (see
/// [`crate::geometry::split`]): the stages already showing parts of it are reused in their order on
/// it, missing ones are added (named after the surface, sized to their part) up to `max` stages in
/// all. Returns how many outputs it has, or why nothing changed.
pub fn split_surface(stages: &mut Vec<Stage>, surface: StageId, n: usize, overlap: f32, down: bool, max: usize) -> Result<usize, String> {
    let Some(src) = stages.iter().find(|s| s.id == surface).cloned() else { return Err("no such stage".into()) };
    let mut parts: Vec<usize> = stages.iter().enumerate().filter(|(_, s)| s.span_of == Some(surface)).map(|(i, _)| i).collect();
    parts.sort_by(|a, b| {
        let key = |s: &Stage| if down { s.crop[1] } else { s.crop[0] };
        key(&stages[*a]).total_cmp(&key(&stages[*b]))
    });
    let missing = n.saturating_sub(parts.len());
    if stages.len() + missing > max {
        return Err(crate::tr!("A show holds at most {max} stages", max = max.to_string()));
    }
    for (k, (crop, blend)) in crate::geometry::split(n, overlap, down).into_iter().enumerate() {
        let i = match parts.get(k) {
            Some(&i) => i,
            None => {
                let mut st = Stage::new(&format!("{} {}", src.name, k + 1));
                st.span_of = Some(surface);
                st.width = ((crop[2] - crop[0]) * src.width as f32).round().max(16.0) as u32;
                st.height = ((crop[3] - crop[1]) * src.height as f32).round().max(16.0) as u32;
                stages.push(st);
                stages.len() - 1
            }
        };
        stages[i].crop = crop;
        stages[i].blend = blend;
    }
    Ok(n)
}

/// A slice marker: the slice *ending* at `time` plays `plays` times (0 = until devamped).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct SliceMarker {
    /// Position in the file, in seconds.
    pub time: f64,
    /// Times the slice plays (0 = until devamped).
    pub plays: u32,
}

/// One playable section of an audio file.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Segment {
    /// Start in the file, in seconds.
    pub start: f64,
    /// None = end of file.
    pub end: Option<f64>,
    /// None = loop until devamped / stopped.
    pub plays: Option<u32>,
}

/// Plays a sound file through the mixer.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct AudioCue {
    /// Media file.
    pub path: PathBuf,
    /// Level in dB.
    pub volume_db: f32,
    /// -1 (left) .. 1 (right).
    pub pan: f32,
    /// Seconds into the file to start from.
    pub start_time: f64,
    /// Seconds into the file to stop at (None = end of file).
    pub end_time: Option<f64>,
    /// Times the start–end region plays.
    pub play_count: u32,
    /// Loop the region until stopped.
    pub infinite_loop: bool,
    /// Playback speed (varispeed: pitch follows). 1 = normal.
    pub rate: f32,
    /// Equaliser.
    pub eq: Eq,
    /// Custom routing in dB, `[source channel][output channel]`; None = automatic.
    pub routing: Option<Vec<Vec<f32>>>,
    /// Slice markers between start and end; the last slice uses the cue's play count.
    pub slices: Vec<SliceMarker>,
    /// Integrated fade-in from silence at the start, in seconds of playback (0 = none).
    pub fade_in: f64,
    /// Integrated fade-out to silence at the end, in seconds of playback (0 = none). Applies when
    /// the end is known: not while the cue loops forever or a slice vamps.
    pub fade_out: f64,
    /// Shape of the integrated fades.
    pub fade_curve: FadeCurve,
    /// Output fader levels in dB, one per device output, applied after the
    /// routing matrix. Missing entries are 0 dB.
    pub out_levels: Vec<f32>,
    /// Audio effects, run in order after the EQ.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audio_fx: Vec<AudioEffect>,
    /// Object audio: place the channels in the room instead of using the routing matrix.
    #[serde(skip_serializing_if = "ObjectAudio::is_off")]
    pub objects: ObjectAudio,
    /// Gang letters of the crosspoints, `[source][output]` like `routing` (0 = none, 1 = A...):
    /// crosspoints with one letter move together when one is changed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gangs: Vec<Vec<u8>>,
    /// Audio patch it plays through: 0 = the main audio device, n = [`Settings::patches`]`[n - 1]`.
    #[serde(skip_serializing_if = "is_zero")]
    pub patch: u8,
}

impl AudioCue {
    /// Markers that fall strictly inside the start/end region, sorted.
    pub fn active_slices(&self) -> Vec<SliceMarker> {
        let end = self.end_time.unwrap_or(f64::INFINITY);
        let mut v: Vec<SliceMarker> =
            self.slices.iter().copied().filter(|m| m.time > self.start_time + 1e-3 && m.time < end - 1e-3).collect();
        v.sort_by(|a, b| a.time.total_cmp(&b.time));
        v
    }

    /// The sections the decoder plays, in order.
    pub fn segments(&self) -> Vec<Segment> {
        let mut out = Vec::new();
        let mut start = self.start_time.max(0.0);
        for m in self.active_slices() {
            out.push(Segment { start, end: Some(m.time), plays: (m.plays > 0).then_some(m.plays) });
            start = m.time;
        }
        out.push(Segment {
            start,
            end: self.end_time,
            plays: if self.infinite_loop { None } else { Some(self.play_count.max(1)) },
        });
        out
    }

    /// Total file-time played (before rate), None if anything loops forever.
    pub fn file_time_total(&self, file_duration: f64) -> Option<f64> {
        self.segments().iter().try_fold(0.0, |acc, s| {
            let end = s.end.unwrap_or(file_duration).min(file_duration);
            Some(acc + (end - s.start).max(0.0) * s.plays? as f64)
        })
    }

    /// Where in the file playback is after `played` seconds of file time (ignores devamps).
    pub fn file_position(&self, played: f64, file_duration: f64) -> f64 {
        let mut left = played.max(0.0);
        let segs = self.segments();
        for (i, s) in segs.iter().enumerate() {
            let end = s.end.unwrap_or(file_duration).min(file_duration);
            let len = (end - s.start).max(1e-6);
            match s.plays {
                None => return s.start + left % len,
                Some(n) => {
                    let total = len * n as f64;
                    if left < total || i + 1 == segs.len() {
                        return s.start + (left % len).min(len);
                    }
                    left -= total;
                }
            }
        }
        self.start_time
    }

}

/// Linear routing gains for `src` source and `out` output channels from an optional dB matrix
/// (None = automatic routing).
pub fn routing_gains(routing: &Option<Vec<Vec<f32>>>, src: usize, out: usize) -> Vec<Vec<f32>> {
    match routing {
        None => default_routing(src, out),
        Some(m) => (0..src)
            .map(|s| (0..out).map(|o| m.get(s).and_then(|r| r.get(o)).map_or(0.0, |db| db_to_gain(*db))).collect())
            .collect(),
    }
}

/// Linear gain of output `o` from per-output fader levels in dB (missing = 0 dB).
pub fn out_level_gain(out_levels: &[f32], o: usize) -> f32 {
    out_levels.get(o).map_or(1.0, |db| db_to_gain(*db))
}

impl Default for AudioCue {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            volume_db: 0.0,
            pan: 0.0,
            start_time: 0.0,
            end_time: None,
            play_count: 1,
            infinite_loop: false,
            rate: 1.0,
            eq: Eq::default(),
            routing: None,
            slices: Vec::new(),
            fade_in: 0.0,
            fade_out: 0.0,
            fade_curve: FadeCurve::SCurve,
            out_levels: Vec::new(),
            audio_fx: Vec::new(),
            objects: ObjectAudio::default(),
            gangs: Vec::new(),
            patch: 0,
        }
    }
}

/// Plays a movie or still image on a stage (soundtrack through the mixer).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct VideoCue {
    /// Media file.
    pub path: PathBuf,
    /// Soundtrack level in dB.
    pub volume_db: f32,
    /// 0..1
    pub opacity: f32,
    /// Higher layers draw on top.
    pub layer: i32,
    /// How the picture combines with the layers below.
    pub blend_mode: BlendMode,
    /// How the picture fits the stage.
    pub fit: VideoFit,
    /// Start point in the file, in seconds.
    pub start_time: f64,
    /// Seconds into the file to stop at (None = end of file).
    pub end_time: Option<f64>,
    /// Times the start–end region plays.
    pub play_count: u32,
    /// Loop until stopped.
    pub infinite_loop: bool,
    /// Keep showing the last frame when playback ends instead of clearing.
    pub hold_last_frame: bool,
    /// Output stage (None = the first stage).
    pub stage: Option<StageId>,
    /// Position, scale, rotation and crop on the stage.
    pub geometry: Geometry,
    /// For still images: seconds to show (0 = until stopped).
    pub still_duration: f64,
    /// Soundtrack routing in dB `[source channel][output channel]`; None = automatic.
    pub routing: Option<Vec<Vec<f32>>>,
    /// Soundtrack output fader levels in dB, one per device output; missing entries are 0 dB.
    pub out_levels: Vec<f32>,
    /// Keyframe animation of the picture (relative to the settings above).
    #[serde(skip_serializing_if = "Animation::is_empty")]
    pub animation: Animation,
    /// Video effects on the picture, in order (blur, pixellate and sharpen act first).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub video_fx: Vec<VideoEffect>,
    /// Hides parts of the picture (moves, scales and crops with it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask: Option<Mask>,
    /// Audio effects on the soundtrack.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audio_fx: Vec<AudioEffect>,
    /// Object audio for the soundtrack.
    #[serde(skip_serializing_if = "ObjectAudio::is_off")]
    pub objects: ObjectAudio,
    /// Gang letters of the crosspoints, `[source][output]` like `routing` (0 = none, 1 = A...):
    /// crosspoints with one letter move together when one is changed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gangs: Vec<Vec<u8>>,
    /// Audio patch it plays through: 0 = the main audio device, n = [`Settings::patches`]`[n - 1]`.
    #[serde(skip_serializing_if = "is_zero")]
    pub patch: u8,
}

impl Default for VideoCue {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            volume_db: 0.0,
            opacity: 1.0,
            layer: 0,
            blend_mode: BlendMode::Normal,
            fit: VideoFit::Fit,
            start_time: 0.0,
            end_time: None,
            play_count: 1,
            infinite_loop: false,
            hold_last_frame: false,
            stage: None,
            geometry: Geometry::default(),
            still_duration: 0.0,
            routing: None,
            out_levels: Vec::new(),
            animation: Animation::default(),
            video_fx: Vec::new(),
            mask: None,
            audio_fx: Vec::new(),
            objects: ObjectAudio::default(),
            gangs: Vec::new(),
            patch: 0,
        }
    }
}

/// Fades a running audio/video/text cue's level, opacity or geometry.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct FadeCue {
    /// Cue to fade (None = not set).
    pub target: Option<CueId>,
    /// Fade length in seconds.
    pub duration: f64,
    /// Shape of the fade.
    pub curve: FadeCurve,
    /// Fade audio level to this value (None = leave level alone).
    pub volume_db: Option<f32>,
    /// Fade video opacity to this value (None = leave opacity alone).
    pub opacity: Option<f32>,
    /// Fade video geometry to this value (None = leave geometry alone).
    pub geometry: Option<Geometry>,
    /// Stop the target when the fade ends (fade-outs).
    pub stop_target_when_done: bool,
    /// Move object-audio objects to these positions (None = leave them alone).
    pub objects: Option<Vec<AudioObject>>,
    /// Fade the target's video effects to these settings (None = leave them alone).
    pub video_fx: Option<Vec<VideoEffect>>,
    /// Fade the target's output faders to these levels, dB per output (None = leave them alone).
    pub out_levels: Option<Vec<f32>>,
    /// Fade the target's audio effects to these settings (None = leave them alone).
    pub audio_fx: Option<Vec<AudioEffect>>,
}

impl Default for FadeCue {
    fn default() -> Self {
        Self {
            target: None,
            duration: 5.0,
            curve: FadeCurve::SCurve,
            volume_db: Some(MIN_DB),
            opacity: None,
            geometry: None,
            stop_target_when_done: true,
            objects: None,
            video_fx: None,
            out_levels: None,
            audio_fx: None,
        }
    }
}

/// Horizontal text alignment.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TextAlign {
    /// Left-aligned.
    Left,
    /// Centred.
    #[default]
    Center,
    /// Right-aligned.
    Right,
}

/// Text shown on a stage like a video layer.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct TextCue {
    /// The text (may span lines).
    pub text: String,
    /// Glyph size in stage-canvas pixels.
    pub size: f32,
    /// RGBA colour.
    pub color: [u8; 4],
    /// Bold weight (used when `weight` is 0, i.e. files from before weights existed).
    pub bold: bool,
    /// Font family as installed on the system; empty = the built-in Inter.
    pub font: String,
    /// Weight 100 (thin) to 900 (black); 0 = from `bold`.
    pub weight: u16,
    /// Italic / oblique style.
    pub italic: bool,
    /// Extra space between letters, in em (may be negative).
    pub letter_spacing: f32,
    /// Line height as a multiple of the size.
    pub line_height: f32,
    /// Outline width in stage pixels (0 = none).
    pub outline_width: f32,
    /// Outline RGBA colour.
    pub outline_color: [u8; 4],
    /// Drop shadow RGBA colour (alpha 0 = no shadow).
    pub shadow_color: [u8; 4],
    /// Shadow offset in stage pixels (right, down).
    pub shadow_offset: [f32; 2],
    /// Shadow softness (blur radius) in stage pixels.
    pub shadow_blur: f32,
    /// Line alignment.
    pub align: TextAlign,
    /// Stage it shows on (None = first stage).
    pub stage: Option<StageId>,
    /// Position, scale, rotation and crop on the stage.
    pub geometry: Geometry,
    /// Stacking layer: higher draws on top.
    pub layer: i32,
    /// How the text combines with the layers below.
    pub blend_mode: BlendMode,
    /// Opacity 0..1.
    pub opacity: f32,
    /// Seconds to show (0 = until stopped).
    pub duration: f64,
    /// Box colour behind the text, RGBA (alpha 0 = none).
    pub bg_color: [u8; 4],
    /// Space between the text and the edge of its background box, in stage pixels.
    pub bg_padding: f32,
    /// Corner radius of the background box, in stage pixels.
    pub bg_radius: f32,
    /// Keyframe animation (placement, opacity and colours; relative to the settings above).
    #[serde(skip_serializing_if = "Animation::is_empty")]
    pub animation: Animation,
    /// Video effects on the text layer.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub video_fx: Vec<VideoEffect>,
    /// Hides parts of the picture (moves, scales and crops with it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask: Option<Mask>,
}

impl TextCue {
    /// The weight to render with (100–900).
    pub fn effective_weight(&self) -> u16 {
        match self.weight {
            0 if self.bold => 700,
            0 => 400,
            w => w.clamp(100, 900),
        }
    }
}

impl Default for TextCue {
    fn default() -> Self {
        Self {
            text: "Text".into(),
            size: 96.0,
            color: [255, 255, 255, 255],
            bold: true,
            font: String::new(),
            weight: 0,
            italic: false,
            letter_spacing: 0.0,
            line_height: 1.2,
            outline_width: 0.0,
            outline_color: [0, 0, 0, 255],
            shadow_color: [0, 0, 0, 0],
            shadow_offset: [4.0, 4.0],
            shadow_blur: 6.0,
            align: TextAlign::Center,
            stage: None,
            geometry: Geometry::default(),
            layer: 10,
            blend_mode: BlendMode::Normal,
            opacity: 1.0,
            duration: 0.0,
            bg_color: [0, 0, 0, 0],
            bg_padding: 24.0,
            bg_radius: 0.0,
            animation: Animation::default(),
            video_fx: Vec::new(),
            mask: None,
        }
    }
}

/// What a Network cue sends.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum NetFormat {
    /// One OSC message (address + arguments).
    #[default]
    Osc,
    /// Plain text with escapes (`\r`, `\n`, `\t`, `\0`, `\xHH`, `\\`).
    Text,
}

impl NetFormat {
    /// Both, for menus.
    pub const ALL: [NetFormat; 2] = [NetFormat::Osc, NetFormat::Text];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            NetFormat::Osc => crate::i18n::t("OSC message"),
            NetFormat::Text => crate::i18n::t("Text"),
        }
    }
}

/// How a Network cue's message travels.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum NetTransport {
    /// One UDP datagram.
    #[default]
    Udp,
    /// A TCP connection kept open per destination (OSC is SLIP-framed, as OSC 1.1 says).
    Tcp,
}

impl NetTransport {
    /// Both, for menus.
    pub const ALL: [NetTransport; 2] = [NetTransport::Udp, NetTransport::Tcp];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            NetTransport::Udp => "UDP",
            NetTransport::Tcp => "TCP",
        }
    }
}

/// The escapes [`parse_net_text`] reads, for messages and hints.
pub const NET_ESCAPES: &str = r"\r \n \t \0 \xHH \\";

/// Longest text a Network cue sends (bytes after escapes; a UDP datagram holds 65 507).
pub const MAX_NET_TEXT: usize = 65_000;

/// Sends one OSC message or a piece of text over UDP or TCP.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct NetworkCue {
    /// Destination host name or IP.
    pub host: String,
    /// Destination port.
    pub port: u16,
    /// OSC address, e.g. `/cue/1/go`.
    pub address: String,
    /// Space-separated arguments; integers and decimals are sent as numbers, the rest as strings
    /// ("quoted text" keeps spaces).
    pub args: String,
    /// OSC or plain text.
    pub format: NetFormat,
    /// UDP or TCP.
    pub transport: NetTransport,
    /// The text sent when `format` is [`NetFormat::Text`] (escapes as [`parse_net_text`] reads them).
    pub text: String,
}

impl Default for NetworkCue {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 53000,
            address: "/go".into(),
            args: String::new(),
            format: NetFormat::Osc,
            transport: NetTransport::Udp,
            text: String::new(),
        }
    }
}

impl NetworkCue {
    /// The bytes it sends (OSC over TCP is SLIP-framed here), or why it can't.
    pub fn payload(&self) -> Result<Vec<u8>, String> {
        match self.format {
            NetFormat::Osc => {
                if !self.address.starts_with('/') {
                    return Err(crate::i18n::t("The OSC address must start with /").into());
                }
                let msg = rosc::OscPacket::Message(rosc::OscMessage {
                    addr: self.address.clone(),
                    args: crate::media::parse_osc_args(&self.args),
                });
                let bytes = rosc::encoder::encode(&msg).map_err(|e| e.to_string())?;
                Ok(match self.transport {
                    NetTransport::Udp => bytes,
                    NetTransport::Tcp => slip(&bytes),
                })
            }
            NetFormat::Text => {
                let bytes = parse_net_text(&self.text)?;
                if bytes.is_empty() {
                    return Err(crate::i18n::t("No text to send").into());
                }
                Ok(bytes)
            }
        }
    }
}

/// SLIP framing (RFC 1055, double END) used for OSC over TCP.
pub fn slip(bytes: &[u8]) -> Vec<u8> {
    const END: u8 = 0xc0;
    const ESC: u8 = 0xdb;
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(END);
    for &b in bytes {
        match b {
            END => out.extend([ESC, 0xdc]),
            ESC => out.extend([ESC, 0xdd]),
            b => out.push(b),
        }
    }
    out.push(END);
    out
}

/// Network cue text → bytes: UTF-8, with `\r`, `\n`, `\t`, `\0`, `\xHH` and `\\` escapes.
pub fn parse_net_text(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('0') => out.push(0),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                let b = (hex.len() == 2).then(|| u8::from_str_radix(&hex, 16).ok()).flatten();
                out.push(b.ok_or_else(|| crate::i18n::t("A hex escape needs two hex digits").to_string())?);
            }
            _ => return Err(crate::tr!("Unknown escape in the text (use {escapes})", escapes = NET_ESCAPES)),
        }
    }
    if out.len() > MAX_NET_TEXT {
        return Err(crate::i18n::t("Text too long for one message").into());
    }
    Ok(out)
}

/// The kind of message a MIDI cue sends.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MidiMessageKind {
    /// Note on (number = note, value = velocity).
    #[default]
    NoteOn,
    /// Note off (number = note, value = release velocity).
    NoteOff,
    /// Control change (number = controller, value).
    Control,
    /// Program change (number = program).
    Program,
    /// Pitch bend (value 0..=16383, 8192 = centre).
    PitchBend,
    /// MIDI Show Control command.
    Msc,
    /// Any System Exclusive message, typed as hex bytes.
    SysEx,
}

impl MidiMessageKind {
    /// Every kind, for menus.
    pub const ALL: [MidiMessageKind; 7] = [
        MidiMessageKind::NoteOn,
        MidiMessageKind::NoteOff,
        MidiMessageKind::Control,
        MidiMessageKind::Program,
        MidiMessageKind::PitchBend,
        MidiMessageKind::Msc,
        MidiMessageKind::SysEx,
    ];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            MidiMessageKind::NoteOn => crate::i18n::t("Note on"),
            MidiMessageKind::NoteOff => crate::i18n::t("Note off"),
            MidiMessageKind::Control => crate::i18n::t("Control change"),
            MidiMessageKind::Program => crate::i18n::t("Program change"),
            MidiMessageKind::PitchBend => crate::i18n::t("Pitch bend"),
            MidiMessageKind::Msc => crate::i18n::t("MIDI Show Control"),
            MidiMessageKind::SysEx => crate::i18n::t("SysEx"),
        }
    }
}

/// Sends one MIDI message out of a MIDI port.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct MidiCue {
    /// MIDI output port name on the computer playing the show (None = the first output).
    pub port: Option<String>,
    /// What it sends.
    pub message: MidiMessageKind,
    /// MIDI channel, 0-based (0..=15).
    pub channel: u8,
    /// Note, controller or program number (0..=127).
    pub number: u8,
    /// Velocity or controller value (0..=127); pitch bend 0..=16383.
    pub value: u16,
    /// MSC: device ID it is sent to (0..=127, 127 = all-call).
    pub msc_device: u8,
    /// MSC: command format ([`crate::msc::FORMATS`]).
    pub msc_format: u8,
    /// MSC: command code ([`crate::msc::SEND_COMMANDS`]).
    pub msc_command: u8,
    /// MSC: cue number (may be empty).
    pub msc_cue: String,
    /// MSC: cue list (may be empty).
    pub msc_list: String,
    /// SysEx bytes as hex ("F0 7E 7F 06 01 F7").
    pub sysex: String,
}

impl Default for MidiCue {
    fn default() -> Self {
        Self {
            port: None,
            message: MidiMessageKind::NoteOn,
            channel: 0,
            number: 60,
            value: 100,
            msc_device: 0x7f,
            msc_format: 0x01,
            msc_command: 0x01,
            msc_cue: String::new(),
            msc_list: String::new(),
            sysex: "F0 7E 7F 06 01 F7".into(),
        }
    }
}

impl MidiCue {
    /// The bytes it sends, or why it can't.
    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        let ch = self.channel & 0x0f;
        let (n, v) = (self.number & 0x7f, self.value.min(127) as u8);
        Ok(match self.message {
            MidiMessageKind::NoteOn => vec![0x90 | ch, n, v],
            MidiMessageKind::NoteOff => vec![0x80 | ch, n, v],
            MidiMessageKind::Control => vec![0xb0 | ch, n, v],
            MidiMessageKind::Program => vec![0xc0 | ch, n],
            MidiMessageKind::PitchBend => {
                let b = self.value.min(16383);
                vec![0xe0 | ch, (b & 0x7f) as u8, (b >> 7) as u8]
            }
            MidiMessageKind::Msc => crate::msc::encode(self.msc_device, self.msc_format, self.msc_command, &self.msc_cue, &self.msc_list),
            MidiMessageKind::SysEx => parse_sysex(&self.sysex)?,
        })
    }

    /// Short text for the cue list ("Note on 60 · ch 1", "MSC GO 12").
    pub fn label(&self) -> String {
        let ch = self.channel + 1;
        match self.message {
            MidiMessageKind::NoteOn | MidiMessageKind::NoteOff | MidiMessageKind::Control => format!("{} {} = {} · ch {ch}", self.message.label(), self.number, self.value.min(127)),
            MidiMessageKind::Program => format!("{} {} · ch {ch}", self.message.label(), self.number),
            MidiMessageKind::PitchBend => format!("{} {} · ch {ch}", self.message.label(), self.value.min(16383)),
            MidiMessageKind::Msc => {
                let cmd = crate::msc::SEND_COMMANDS.iter().find(|c| c.0 == self.msc_command).map_or_else(|| format!("{:02X}", self.msc_command), |c| c.1.to_string());
                format!("MSC {cmd} {}", self.msc_cue).trim_end().to_string()
            }
            MidiMessageKind::SysEx => format!("SysEx {}", self.sysex.trim()),
        }
    }
}

/// Longest SysEx message a MIDI cue sends.
pub const MAX_SYSEX: usize = 1024;

/// SysEx bytes from hex text ("F0 7E 7F 06 01 F7"; spaces optional): starts with F0, ends with F7,
/// data bytes below 80.
pub fn parse_sysex(text: &str) -> Result<Vec<u8>, String> {
    let hex: String = text.chars().filter(|c| !c.is_whitespace() && *c != ',').collect();
    if !hex.len().is_multiple_of(2) || hex.len() / 2 > MAX_SYSEX {
        return Err(crate::i18n::t("SysEx: type whole hex bytes (at most 1024)").into());
    }
    let bytes = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16)).collect::<Result<Vec<u8>, _>>().map_err(|_| crate::i18n::t("SysEx: not hex bytes").to_string())?;
    match bytes.as_slice() {
        [0xf0, data @ .., 0xf7] if data.iter().all(|b| *b < 0x80) => Ok(bytes),
        _ => Err(crate::i18n::t("SysEx starts with F0, ends with F7, and its data bytes are below 80").into()),
    }
}

/// Where a Timecode cue sends its code.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TimecodeOutput {
    /// LTC audio on an output channel of the audio device.
    #[default]
    Ltc,
    /// MIDI Time Code out of a MIDI port.
    Mtc,
}

impl TimecodeOutput {
    /// Both, for menus.
    pub const ALL: [TimecodeOutput; 2] = [TimecodeOutput::Ltc, TimecodeOutput::Mtc];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            TimecodeOutput::Ltc => crate::i18n::t("LTC (audio)"),
            TimecodeOutput::Mtc => crate::i18n::t("MTC (MIDI)"),
        }
    }
}

/// Plays a Standard MIDI File out of a MIDI port.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct MidiFileCue {
    /// The `.mid` file.
    pub path: PathBuf,
    /// MIDI output port name on the computer playing the show (None = the first output).
    pub port: Option<String>,
    /// Speed (1 = the file's own tempo).
    pub rate: f32,
    /// Start again at the end, until stopped.
    pub infinite_loop: bool,
}

impl Default for MidiFileCue {
    fn default() -> Self {
        Self { path: PathBuf::new(), port: None, rate: 1.0, infinite_loop: false }
    }
}

/// Generates timecode from a start time, as long as it runs.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct TimecodeCue {
    /// LTC or MTC.
    pub output: TimecodeOutput,
    /// Frame rate.
    pub rate: TimecodeRate,
    /// First label sent, in label-seconds.
    pub start: f64,
    /// Seconds it runs (0 = until stopped).
    pub duration: f64,
    /// LTC level in dB.
    pub volume_db: f32,
    /// LTC: audio output channel, 0-based.
    pub channel: u16,
    /// MTC: MIDI output port on the computer playing the show (None = the first output).
    pub midi_port: Option<String>,
}

impl Default for TimecodeCue {
    fn default() -> Self {
        Self { output: TimecodeOutput::Ltc, rate: TimecodeRate::Fps25, start: 3600.0, duration: 0.0, volume_db: -12.0, channel: 0, midi_port: None }
    }
}

impl TimecodeCue {
    /// The LTC voice's routing: its one source channel to [`TimecodeCue::channel`] only (dB).
    pub fn ltc_routing(&self) -> Vec<Vec<f32>> {
        let ch = self.channel as usize;
        vec![(0..=ch).map(|o| if o == ch { 0.0 } else { MIN_DB }).collect()]
    }
}

/// What a Light cue does with the lighting design.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LightAction {
    /// GO a designed lighting cue.
    #[default]
    Go,
    /// Fade a sequence out (None = every sequence).
    Release,
}

impl LightAction {
    /// Every action.
    pub const ALL: [LightAction; 2] = [LightAction::Go, LightAction::Release];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            LightAction::Go => crate::i18n::t("GO lighting cue"),
            LightAction::Release => crate::i18n::t("Release sequence"),
        }
    }
}

/// Plays a cue of the lighting design (the design itself lives in `Workspace.lighting`).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct LightingCue {
    /// Go or release.
    pub action: LightAction,
    /// Lighting cue fired by [`LightAction::Go`].
    pub cue: Option<crate::light::model::LightCueId>,
    /// Sequence released by [`LightAction::Release`] (None = all).
    pub sequence: Option<crate::light::model::SequenceId>,
    /// Release fade in seconds.
    pub fade: f64,
}

/// Plays a live audio input (microphones, line inputs) through the mixer.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct MicCue {
    /// cpal input device id (None = the system's default input).
    pub device: Option<String>,
    /// First input channel used, from 0.
    pub first_channel: u16,
    /// Number of input channels, from `first_channel` on (1..=`MAX_SRC_CHANNELS`).
    pub channels: u16,
    /// Level in dB.
    pub volume_db: f32,
    /// -1 (left) .. 1 (right).
    pub pan: f32,
    /// Equaliser.
    pub eq: Eq,
    /// Custom routing in dB, `[input channel][output channel]`; None = automatic.
    pub routing: Option<Vec<Vec<f32>>>,
    /// Output fader levels in dB, one per device output.
    pub out_levels: Vec<f32>,
    /// Seconds the input stays open (0 = until stopped).
    pub duration: f64,
    /// Fade up from silence when the cue starts, in seconds.
    pub fade_in: f64,
    /// Audio effects, run in order after the EQ.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audio_fx: Vec<AudioEffect>,
    /// Object audio: place the input channels in the room.
    #[serde(skip_serializing_if = "ObjectAudio::is_off")]
    pub objects: ObjectAudio,
    /// Gang letters of the crosspoints, `[source][output]` like `routing` (0 = none, 1 = A...):
    /// crosspoints with one letter move together when one is changed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gangs: Vec<Vec<u8>>,
    /// Audio patch it plays through: 0 = the main audio device, n = [`Settings::patches`]`[n - 1]`.
    #[serde(skip_serializing_if = "is_zero")]
    pub patch: u8,
}

impl Default for MicCue {
    fn default() -> Self {
        Self {
            device: None,
            first_channel: 0,
            channels: 1,
            volume_db: 0.0,
            pan: 0.0,
            eq: Eq::default(),
            routing: None,
            out_levels: Vec::new(),
            duration: 0.0,
            fade_in: 0.0,
            audio_fx: Vec::new(),
            objects: ObjectAudio::default(),
            gangs: Vec::new(),
            patch: 0,
        }
    }
}

/// Shows a live camera (webcam, capture card) as a layer on a stage.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct CameraCue {
    /// Capture device as FFmpeg names it (None = the first camera found): `/dev/video0` (Linux),
    /// `video=Name` (Windows), a device index or name (macOS).
    pub device: Option<String>,
    /// FFmpeg input format to open it with (None = this system's camera driver: v4l2, dshow or
    /// avfoundation). `lavfi` with a device like `testsrc` gives a test pattern.
    pub format: Option<String>,
    /// Requested capture size in pixels (None = the camera's default).
    pub size: Option<[u32; 2]>,
    /// 0..1
    pub opacity: f32,
    /// Higher layers draw on top.
    pub layer: i32,
    /// How the picture combines with the layers below.
    pub blend_mode: BlendMode,
    /// How the picture fits the stage.
    pub fit: VideoFit,
    /// Output stage (None = the first stage).
    pub stage: Option<StageId>,
    /// Position, scale, rotation and crop on the stage.
    pub geometry: Geometry,
    /// Mirror the picture left to right (as a selfie view).
    pub mirror: bool,
    /// Seconds the camera shows (0 = until stopped).
    pub duration: f64,
    /// Video effects on the picture (chroma key for a green screen, colour, blur…).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub video_fx: Vec<VideoEffect>,
    /// Hides parts of the picture (moves, scales and crops with it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask: Option<Mask>,
    /// Keyframe animation of the picture (relative to the settings above).
    #[serde(skip_serializing_if = "Animation::is_empty")]
    pub animation: Animation,
}

impl Default for CameraCue {
    fn default() -> Self {
        Self {
            device: None,
            format: None,
            size: None,
            opacity: 1.0,
            layer: 0,
            blend_mode: BlendMode::Normal,
            fit: VideoFit::Fit,
            stage: None,
            geometry: Geometry::default(),
            mirror: false,
            duration: 0.0,
            animation: Animation::default(),
            video_fx: Vec::new(),
            mask: None,
        }
    }
}

/// What a cue does.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum CueKind {
    /// Play a sound file.
    Audio(AudioCue),
    /// Play a movie or image.
    Video(VideoCue),
    /// Contain other cues.
    Group {
        /// How the children play.
        mode: GroupMode,
        /// The contained cues.
        children: Vec<Cue>,
    },
    /// Wait for a time (e.g. to time an auto-follow).
    Wait {
        /// Seconds.
        duration: f64,
    },
    /// Do nothing (notes / markers).
    Memo,
    /// Fade another cue.
    Fade(FadeCue),
    /// Start the target cue.
    /// Fire another cue.
    Start {
        /// Cue acted on.
        target: Option<CueId>,
    },
    /// Stop the target cue (None = stop everything else).
    /// Stop another cue.
    Stop {
        /// Cue acted on.
        target: Option<CueId>,
    },
    /// Pause the target cue (None = pause everything else).
    /// Pause another cue.
    Pause {
        /// Cue acted on.
        target: Option<CueId>,
    },
    /// Move the playhead to the target cue.
    /// Move the playhead to another cue.
    Goto {
        /// Cue acted on.
        target: Option<CueId>,
    },
    /// Make the target audio cue leave its looping slice and carry on.
    /// Release a vamping slice in another cue.
    Devamp {
        /// Cue acted on.
        target: Option<CueId>,
    },
    /// Send an OSC message.
    Network(NetworkCue),
    /// Show text on a stage.
    Text(TextCue),
    /// Play a live audio input.
    Mic(MicCue),
    /// Show a live camera on a stage.
    Camera(CameraCue),
    /// Play a cue of the lighting design.
    Light(LightingCue),
    /// Arm the target cue (GO and triggers fire it again).
    Arm {
        /// Cue acted on.
        target: Option<CueId>,
    },
    /// Disarm the target cue (skipped by GO and triggers until armed).
    Disarm {
        /// Cue acted on.
        target: Option<CueId>,
    },
    /// Stop the target cue at once and undo what Arm, Disarm, Target and Load cues did to it.
    Reset {
        /// Cue acted on.
        target: Option<CueId>,
    },
    /// Get the target cue ready to start part way through: its next start begins `time` seconds in.
    Load {
        /// Cue acted on.
        target: Option<CueId>,
        /// Seconds into the target (pre-wait included) its next start begins at.
        #[serde(default)]
        time: f64,
    },
    /// Send a MIDI message.
    Midi(MidiCue),
    /// Generate timecode (LTC or MTC).
    Timecode(TimecodeCue),
    /// Play a MIDI file.
    MidiFile(MidiFileCue),
    /// Point the target cue (a Fade, Start, Stop... cue) at another cue.
    Target {
        /// Cue whose target changes.
        target: Option<CueId>,
        /// Its new target (None = no target).
        #[serde(default)]
        new_target: Option<CueId>,
    },
}

/// The type of a cue without its settings (for menus, icons and display).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum KindTag {
    /// Audio cue.
    Audio,
    /// Video cue.
    Video,
    /// Group cue.
    Group,
    /// Wait cue.
    Wait,
    /// Memo cue.
    Memo,
    /// Fade cue.
    Fade,
    /// Start cue.
    Start,
    /// Stop cue.
    Stop,
    /// Pause cue.
    Pause,
    /// Goto cue.
    Goto,
    /// Devamp cue.
    Devamp,
    /// Network cue.
    Network,
    /// Text cue.
    Text,
    /// Mic cue.
    Mic,
    /// Camera cue.
    Camera,
    /// Light cue.
    Light,
    /// Arm cue.
    Arm,
    /// Disarm cue.
    Disarm,
    /// Reset cue.
    Reset,
    /// Load cue.
    Load,
    /// Target cue.
    Target,
    /// MIDI cue.
    Midi,
    /// Timecode cue.
    Timecode,
    /// MIDI File cue.
    MidiFile,
}

impl KindTag {
    /// Cues whose action is media playback handled by the media backend.
    pub fn is_media(self) -> bool {
        matches!(self, KindTag::Audio | KindTag::Video | KindTag::Text | KindTag::Mic | KindTag::Camera | KindTag::Timecode | KindTag::MidiFile)
    }

    /// Every type, in toolbox order (the first ten match Ctrl+1…0).
    pub const ALL: [KindTag; 24] = [
        KindTag::Audio,
        KindTag::Video,
        KindTag::Group,
        KindTag::Wait,
        KindTag::Memo,
        KindTag::Fade,
        KindTag::Start,
        KindTag::Stop,
        KindTag::Pause,
        KindTag::Goto,
        KindTag::Devamp,
        KindTag::Network,
        KindTag::Text,
        KindTag::Mic,
        KindTag::Camera,
        KindTag::Light,
        KindTag::Arm,
        KindTag::Disarm,
        KindTag::Reset,
        KindTag::Load,
        KindTag::Target,
        KindTag::Midi,
        KindTag::Timecode,
        KindTag::MidiFile,
    ];

    /// Whether this build offers the type (Light cues need the `lighting` feature).
    pub fn enabled(self) -> bool {
        self != KindTag::Light || cfg!(feature = "lighting")
    }

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            KindTag::Audio => crate::i18n::t("Audio"),
            KindTag::Video => crate::i18n::t("Video"),
            KindTag::Group => crate::i18n::t("Group"),
            KindTag::Wait => crate::i18n::t("Wait"),
            KindTag::Memo => crate::i18n::t("Memo"),
            KindTag::Fade => crate::i18n::t("Fade"),
            KindTag::Start => crate::i18n::t("Start"),
            KindTag::Stop => crate::i18n::t("Stop"),
            KindTag::Pause => crate::i18n::t("Pause"),
            KindTag::Goto => crate::i18n::t("Go To"),
            KindTag::Devamp => crate::i18n::t("Devamp"),
            KindTag::Network => crate::i18n::t("Network"),
            KindTag::Text => crate::i18n::t("Text"),
            KindTag::Mic => crate::i18n::t("Mic"),
            KindTag::Camera => crate::i18n::t("Camera"),
            KindTag::Light => crate::i18n::t("Light"),
            KindTag::Arm => crate::i18n::t("Arm"),
            KindTag::Disarm => crate::i18n::t("Disarm"),
            KindTag::Reset => crate::i18n::t("Reset"),
            KindTag::Load => crate::i18n::t("Load"),
            KindTag::Target => crate::i18n::t("Target"),
            KindTag::Midi => crate::i18n::t("MIDI"),
            KindTag::Timecode => crate::i18n::t("Timecode"),
            KindTag::MidiFile => crate::i18n::t("MIDI File"),
        }
    }

    /// A new cue of this type with default settings.
    pub fn default_kind(self) -> CueKind {
        match self {
            KindTag::Audio => CueKind::Audio(AudioCue::default()),
            KindTag::Video => CueKind::Video(VideoCue::default()),
            KindTag::Group => CueKind::Group { mode: GroupMode::default(), children: Vec::new() },
            KindTag::Wait => CueKind::Wait { duration: 5.0 },
            KindTag::Memo => CueKind::Memo,
            KindTag::Fade => CueKind::Fade(FadeCue::default()),
            KindTag::Start => CueKind::Start { target: None },
            KindTag::Stop => CueKind::Stop { target: None },
            KindTag::Pause => CueKind::Pause { target: None },
            KindTag::Goto => CueKind::Goto { target: None },
            KindTag::Devamp => CueKind::Devamp { target: None },
            KindTag::Network => CueKind::Network(NetworkCue::default()),
            KindTag::Text => CueKind::Text(TextCue::default()),
            KindTag::Mic => CueKind::Mic(MicCue::default()),
            KindTag::Camera => CueKind::Camera(CameraCue::default()),
            KindTag::Light => CueKind::Light(LightingCue::default()),
            KindTag::Arm => CueKind::Arm { target: None },
            KindTag::Disarm => CueKind::Disarm { target: None },
            KindTag::Reset => CueKind::Reset { target: None },
            KindTag::Load => CueKind::Load { target: None, time: 0.0 },
            KindTag::Target => CueKind::Target { target: None, new_target: None },
            KindTag::Midi => CueKind::Midi(MidiCue::default()),
            KindTag::Timecode => CueKind::Timecode(TimecodeCue::default()),
            KindTag::MidiFile => CueKind::MidiFile(MidiFileCue::default()),
        }
    }
}

impl CueKind {
    /// The type of this cue.
    pub fn tag(&self) -> KindTag {
        match self {
            CueKind::Audio(_) => KindTag::Audio,
            CueKind::Video(_) => KindTag::Video,
            CueKind::Group { .. } => KindTag::Group,
            CueKind::Wait { .. } => KindTag::Wait,
            CueKind::Memo => KindTag::Memo,
            CueKind::Fade(_) => KindTag::Fade,
            CueKind::Start { .. } => KindTag::Start,
            CueKind::Stop { .. } => KindTag::Stop,
            CueKind::Pause { .. } => KindTag::Pause,
            CueKind::Goto { .. } => KindTag::Goto,
            CueKind::Devamp { .. } => KindTag::Devamp,
            CueKind::Network(_) => KindTag::Network,
            CueKind::Text(_) => KindTag::Text,
            CueKind::Mic(_) => KindTag::Mic,
            CueKind::Camera(_) => KindTag::Camera,
            CueKind::Light(_) => KindTag::Light,
            CueKind::Arm { .. } => KindTag::Arm,
            CueKind::Disarm { .. } => KindTag::Disarm,
            CueKind::Reset { .. } => KindTag::Reset,
            CueKind::Load { .. } => KindTag::Load,
            CueKind::Target { .. } => KindTag::Target,
            CueKind::Midi(_) => KindTag::Midi,
            CueKind::Timecode(_) => KindTag::Timecode,
            CueKind::MidiFile(_) => KindTag::MidiFile,
        }
    }

    /// The cue this cue acts on, if any.
    pub fn target(&self) -> Option<CueId> {
        match self {
            CueKind::Fade(f) => f.target,
            CueKind::Start { target }
            | CueKind::Stop { target }
            | CueKind::Pause { target }
            | CueKind::Goto { target }
            | CueKind::Devamp { target }
            | CueKind::Arm { target }
            | CueKind::Disarm { target }
            | CueKind::Reset { target }
            | CueKind::Load { target, .. }
            | CueKind::Target { target, .. } => *target,
            _ => None,
        }
    }

    /// Whether this kind of cue acts on a target cue (has [`CueKind::target_mut`]).
    pub fn takes_target(&self) -> bool {
        matches!(
            self,
            CueKind::Fade(_)
                | CueKind::Start { .. }
                | CueKind::Stop { .. }
                | CueKind::Pause { .. }
                | CueKind::Goto { .. }
                | CueKind::Devamp { .. }
                | CueKind::Arm { .. }
                | CueKind::Disarm { .. }
                | CueKind::Reset { .. }
                | CueKind::Load { .. }
                | CueKind::Target { .. }
        )
    }

    /// The target of Fade/Start/Stop/Pause/Goto/Devamp/Arm/Disarm/Reset/Load/Target cues.
    pub fn target_mut(&mut self) -> Option<&mut Option<CueId>> {
        match self {
            CueKind::Fade(f) => Some(&mut f.target),
            CueKind::Start { target }
            | CueKind::Stop { target }
            | CueKind::Pause { target }
            | CueKind::Goto { target }
            | CueKind::Devamp { target }
            | CueKind::Arm { target }
            | CueKind::Disarm { target }
            | CueKind::Reset { target }
            | CueKind::Load { target, .. }
            | CueKind::Target { target, .. } => Some(target),
            _ => None,
        }
    }

    /// The mask of a Video, Text or Camera cue.
    pub fn mask(&self) -> Option<&Mask> {
        match self {
            CueKind::Video(v) => v.mask.as_ref(),
            CueKind::Text(t) => t.mask.as_ref(),
            CueKind::Camera(c) => c.mask.as_ref(),
            _ => None,
        }
    }

    /// The mask of a Video, Text or Camera cue, to change.
    pub fn mask_mut(&mut self) -> Option<&mut Mask> {
        match self {
            CueKind::Video(v) => v.mask.as_mut(),
            CueKind::Text(t) => t.mask.as_mut(),
            CueKind::Camera(c) => c.mask.as_mut(),
            _ => None,
        }
    }

    /// The media file of Audio/Video/MIDI File cues.
    pub fn media_path(&self) -> Option<&Path> {
        match self {
            CueKind::Audio(a) => Some(&a.path),
            CueKind::Video(v) => Some(&v.path),
            CueKind::MidiFile(m) => Some(&m.path),
            _ => None,
        }
    }
}

/// One entry in the cue list.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Cue {
    /// Unique id.
    pub id: CueId,
    /// Cue number as typed (e.g. "1", "4.2"); may be empty.
    #[serde(default)]
    pub number: String,
    /// Name (empty = derived from the media file or type).
    #[serde(default)]
    pub name: String,
    /// Free notes, shown in the standby card.
    #[serde(default)]
    pub notes: String,
    /// Row colour.
    #[serde(default)]
    pub color: CueColor,
    /// Disarmed cues are skipped by GO.
    #[serde(default = "default_true")]
    pub armed: bool,
    /// Seconds between GO and the action.
    #[serde(default)]
    pub pre_wait: f64,
    /// Seconds after the action starts before an auto-continue fires.
    #[serde(default)]
    pub post_wait: f64,
    /// What happens next.
    #[serde(default)]
    pub continue_mode: ContinueMode,
    /// Keyboard key that fires this cue in show mode (e.g. "F1", "A").
    #[serde(default)]
    pub hotkey: Option<String>,
    /// Fire when the list's incoming timecode reaches this time, in seconds of timecode.
    #[serde(default)]
    pub timecode_trigger: Option<f64>,
    /// Fire when this MIDI message arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midi_trigger: Option<MidiTrigger>,
    /// Fire at this time of day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallclock_trigger: Option<WallClockTrigger>,
    /// What a trigger does when it fires this cue while it still runs.
    #[serde(default, skip_serializing_if = "SecondTrigger::is_default")]
    pub second_trigger: SecondTrigger,
    /// Type and type-specific settings.
    pub kind: CueKind,
}

fn default_true() -> bool {
    true
}

fn is_zero(v: &u8) -> bool {
    *v == 0
}

/// An extra audio output: another device that cues can play through (their I/O tab), besides the
/// main one (Settings → Audio).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct AudioPatch {
    /// Name shown when picking it.
    pub name: String,
    /// cpal output device id on the computer playing the show (None = not chosen yet).
    pub device: Option<String>,
}

/// Most audio patches besides the main output.
pub const MAX_PATCHES: usize = 8;

/// Gang letter `g` (1 = A) for the UI.
pub fn gang_letter(g: u8) -> char {
    (b'A' + g.saturating_sub(1).min(25)) as char
}

/// Apply a crosspoint change of `delta` dB to every crosspoint in gang `gang` except
/// `(src, out)`, which the user already changed.
pub fn move_gang(routing: &mut [Vec<f32>], gangs: &[Vec<u8>], gang: u8, (src, out): (usize, usize), delta: f32) {
    if gang == 0 || delta == 0.0 {
        return;
    }
    for (s, row) in routing.iter_mut().enumerate() {
        for (o, g) in row.iter_mut().enumerate() {
            if (s, o) != (src, out) && gangs.get(s).and_then(|r| r.get(o)) == Some(&gang) {
                *g = (*g + delta).clamp(MIN_DB, crate::limits::MAX_DB);
            }
        }
    }
}

impl Cue {
    /// A copy of the cue without a group's children (they're separate cues to compare/send).
    pub fn shallow_clone(&self) -> Cue {
        Cue {
            id: self.id,
            number: self.number.clone(),
            name: self.name.clone(),
            notes: self.notes.clone(),
            color: self.color,
            armed: self.armed,
            pre_wait: self.pre_wait,
            post_wait: self.post_wait,
            continue_mode: self.continue_mode,
            hotkey: self.hotkey.clone(),
            timecode_trigger: self.timecode_trigger,
            midi_trigger: self.midi_trigger,
            wallclock_trigger: self.wallclock_trigger,
            second_trigger: self.second_trigger,
            kind: match &self.kind {
                CueKind::Group { mode, .. } => CueKind::Group { mode: *mode, children: Vec::new() },
                k => k.clone(),
            },
        }
    }

    /// Equal apart from a group's children.
    pub fn shallow_eq(&self, other: &Cue) -> bool {
        // Naming every field: a new one fails to compile here until it is compared too.
        let Cue { id: _, number: _, name: _, notes: _, color: _, armed: _, pre_wait: _, post_wait: _, continue_mode: _, hotkey: _, timecode_trigger: _, midi_trigger: _, wallclock_trigger: _, second_trigger: _, kind: _ } = self;
        let (a, b) = (self, other);
        a.id == b.id
            && a.number == b.number
            && a.name == b.name
            && a.notes == b.notes
            && a.color == b.color
            && a.armed == b.armed
            && a.pre_wait == b.pre_wait
            && a.post_wait == b.post_wait
            && a.continue_mode == b.continue_mode
            && a.hotkey == b.hotkey
            && a.timecode_trigger == b.timecode_trigger
            && a.midi_trigger == b.midi_trigger
            && a.wallclock_trigger == b.wallclock_trigger
            && a.second_trigger == b.second_trigger
            && match (&self.kind, &other.kind) {
                (CueKind::Group { mode: a, .. }, CueKind::Group { mode: b, .. }) => a == b,
                (CueKind::Group { .. }, _) | (_, CueKind::Group { .. }) => false,
                (a, b) => a == b,
            }
    }

    /// Take `from`'s own fields (see [`Cue::shallow_clone`]), keeping this group's children.
    pub fn assign_shallow(&mut self, from: Cue) {
        let children = self.children_mut().map(std::mem::take);
        *self = from;
        if let (Some(kept), Some(ch)) = (children, self.children_mut()) {
            *ch = kept;
        }
    }

    /// A new cue with a fresh id and default timing.
    pub fn new(kind: CueKind) -> Self {
        Self {
            id: Uuid::new_v4(),
            number: String::new(),
            name: String::new(),
            notes: String::new(),
            color: CueColor::None,
            armed: true,
            pre_wait: 0.0,
            post_wait: 0.0,
            continue_mode: ContinueMode::DoNotContinue,
            hotkey: None,
            timecode_trigger: None,
            midi_trigger: None,
            wallclock_trigger: None,
            second_trigger: SecondTrigger::Restart,
            kind,
        }
    }

    /// True for Group cues.
    pub fn is_group(&self) -> bool {
        matches!(self.kind, CueKind::Group { .. })
    }

    /// Children of a Group (empty otherwise).
    pub fn children(&self) -> &[Cue] {
        match &self.kind {
            CueKind::Group { children, .. } => children,
            _ => &[],
        }
    }

    /// Mutable children of a Group.
    pub fn children_mut(&mut self) -> Option<&mut Vec<Cue>> {
        match &mut self.kind {
            CueKind::Group { children, .. } => Some(children),
            _ => None,
        }
    }

    /// Name shown in the cue list: explicit name, else file name, else kind.
    pub fn display_name(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        if let Some(p) = self.kind.media_path()
            && let Some(f) = p.file_name()
        {
            return f.to_string_lossy().into_owned();
        }
        self.kind.tag().label().to_string()
    }

    /// Deep clone with fresh ids (for duplicate).
    pub fn duplicate(&self) -> Cue {
        let mut c = self.clone();
        fn refresh(c: &mut Cue) {
            c.id = Uuid::new_v4();
            if let Some(ch) = c.children_mut() {
                ch.iter_mut().for_each(refresh);
            }
        }
        refresh(&mut c);
        c
    }

    /// Call `f` on this cue and every cue inside it, depth first.
    pub fn visit<'a>(&'a self, f: &mut impl FnMut(&'a Cue)) {
        f(self);
        for c in self.children() {
            c.visit(f);
        }
    }
}

/// Where a cue lives in the tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Location {
    /// Containing group (None = top level).
    pub parent: Option<CueId>,
    /// Position among its siblings.
    pub index: usize,
}

/// Timecode frame rate.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TimecodeRate {
    /// 24 fps (film).
    Fps24,
    /// 25 fps (PAL).
    #[default]
    Fps25,
    /// 29.97 fps drop-frame (NTSC).
    Fps2997Df,
    /// 30 fps non-drop.
    Fps30,
}

impl TimecodeRate {
    /// Every rate, for menus.
    pub const ALL: [TimecodeRate; 4] = [TimecodeRate::Fps24, TimecodeRate::Fps25, TimecodeRate::Fps2997Df, TimecodeRate::Fps30];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            TimecodeRate::Fps24 => crate::i18n::t("24 fps"),
            TimecodeRate::Fps25 => crate::i18n::t("25 fps"),
            TimecodeRate::Fps2997Df => crate::i18n::t("29.97 drop"),
            TimecodeRate::Fps30 => crate::i18n::t("30 fps"),
        }
    }

    /// Frames counted per second (30 for 29.97 drop-frame, which drops frame numbers instead).
    pub fn nominal(self) -> u32 {
        match self {
            TimecodeRate::Fps24 => 24,
            TimecodeRate::Fps25 => 25,
            TimecodeRate::Fps2997Df | TimecodeRate::Fps30 => 30,
        }
    }

    /// Real frames per second.
    pub fn fps(self) -> f64 {
        match self {
            TimecodeRate::Fps2997Df => 30_000.0 / 1001.0,
            r => r.nominal() as f64,
        }
    }

    /// True for drop-frame counting.
    pub fn drop_frame(self) -> bool {
        self == TimecodeRate::Fps2997Df
    }
}

/// Where a cue list's timecode comes from.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TimecodeSource {
    /// The list ignores timecode.
    #[default]
    Off,
    /// LTC (SMPTE audio) on an audio input channel.
    Ltc,
    /// MIDI Time Code from a MIDI input port.
    Mtc,
}

/// A cue list's timecode input: cues with a timecode trigger fire when it passes their time.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct TimecodeSync {
    /// Off, LTC or MTC.
    pub source: TimecodeSource,
    /// Expected frame rate (LTC also reports its own).
    pub rate: TimecodeRate,
    /// LTC: audio input device name (None = system default input).
    pub ltc_device: Option<String>,
    /// LTC: input channel, from 0.
    pub ltc_channel: u16,
    /// MTC: MIDI input port name (None = the first port).
    pub mtc_port: Option<String>,
}

/// Name and per-list state of one cue list.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ListInfo {
    /// Shown on the list's tab.
    pub name: String,
    /// Stands in for "the top level of this list" as a parent id while the list isn't the current
    /// one. Not saved: a fresh one is made on load.
    #[serde(skip, default = "Uuid::new_v4")]
    pub id: CueId,
    /// Where the playhead was when another list was brought forward.
    pub playhead: Option<CueId>,
    /// Timecode this list follows.
    pub timecode: TimecodeSync,
    /// Shown as a grid of buttons, each firing its own cue (None: a normal list with a playhead).
    pub cart: Option<Cart>,
}

// The id is per-session scaffolding, not part of the list.
impl PartialEq for ListInfo {
    fn eq(&self, o: &Self) -> bool {
        self.name == o.name && self.playhead == o.playhead && self.timecode == o.timecode && self.cart == o.cart
    }
}

impl Default for ListInfo {
    fn default() -> Self {
        Self { name: "Main Cue List".into(), id: Uuid::new_v4(), playhead: None, timecode: TimecodeSync::default(), cart: None }
    }
}

/// Most columns or rows a cart grid is set to.
pub const MAX_CART_SIDE: u16 = 16;
/// Highest cell a cue may be pinned to (the grid grows past its rows for more cues).
pub const MAX_CART_CELL: u32 = 50_000;

/// A cart list's grid: its size and the cells cues were put in. The list's top-level cues are its
/// buttons; one without a cell (new, or the cell was taken) goes in the first free one.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Cart {
    /// Buttons across.
    pub columns: u16,
    /// Rows shown at least (more appear when there are more cues).
    pub rows: u16,
    /// Cell (row × columns + column) of cues put somewhere.
    pub cells: BTreeMap<CueId, u32>,
}

impl Default for Cart {
    fn default() -> Self {
        Self { columns: 4, rows: 4, cells: BTreeMap::new() }
    }
}

impl Cart {
    /// The grid: a cue id or nothing per cell, row by row; at least `columns × rows` cells, whole
    /// rows, as many as the cues need.
    pub fn layout(&self, cues: &[Cue]) -> Vec<Option<CueId>> {
        let cols = self.columns.clamp(1, MAX_CART_SIDE) as usize;
        let min = cols * self.rows.clamp(1, MAX_CART_SIDE) as usize;
        let mut grid: Vec<Option<CueId>> = vec![None; min];
        let mut loose = Vec::new();
        for c in cues {
            match self.cells.get(&c.id).map(|&i| i as usize) {
                Some(i) if i <= MAX_CART_CELL as usize && grid.get(i).is_none_or(|g| g.is_none()) => {
                    if i >= grid.len() {
                        grid.resize(i + 1, None);
                    }
                    grid[i] = Some(c.id);
                }
                _ => loose.push(c.id),
            }
        }
        let mut free = 0;
        for id in loose {
            while grid.get(free).is_some_and(|g| g.is_some()) {
                free += 1;
            }
            if free >= grid.len() {
                grid.resize(free + 1, None);
            }
            grid[free] = Some(id);
        }
        grid.resize(grid.len().div_ceil(cols) * cols, None);
        grid
    }

    /// Put cue `id` in `cell`; a cue already there takes `id`'s old cell. Every cue is pinned to
    /// where it shows, so nothing else moves, and cells of cues no longer in `cues` are dropped.
    pub fn put(&mut self, cues: &[Cue], id: CueId, cell: u32) {
        let grid = self.layout(cues);
        self.cells = grid.iter().enumerate().filter_map(|(i, c)| Some(((*c)?, i as u32))).collect();
        let cell = cell.min(MAX_CART_CELL);
        let Some(&from) = self.cells.get(&id) else { return };
        if let Some(other) = grid.get(cell as usize).copied().flatten() {
            self.cells.insert(other, from);
        }
        self.cells.insert(id, cell);
    }
}

/// A cue list that is not the current one. Its cues keep running and can be targeted.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct OtherList {
    /// Name and state.
    pub info: ListInfo,
    /// Top-level cues.
    pub cues: Vec<Cue>,
}

/// The cue lists of a workspace: the current one, shown and driven by GO, in
/// `cues`, and the others in `others`. Lookups (`find`, `location`, `next_sibling`, `iter_all`, …)
/// span every list so cues in background lists keep running and can be targeted; display and
/// navigation (`visible_rows`, `first`, …) use the current list. The top level of a background list
/// has its `ListInfo::id` as parent.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct CueList {
    /// Top-level cues of the current list in order (groups hold their own children).
    pub cues: Vec<Cue>,
    /// Name and state of the current list.
    #[serde(default)]
    pub info: ListInfo,
    /// The other lists, in tab order without the current one.
    #[serde(default)]
    pub others: Vec<OtherList>,
    /// Tab position of the current list among all lists.
    #[serde(default)]
    pub pos: usize,
    /// Where each cue sits, so lookups don't walk the tree (the engine looks cues up every tick).
    #[serde(skip)]
    pub(crate) index: CueIndex,
}

/// A cue's place in a [`CueList`]: which list (0 = current, k = `others[k - 1]`) and the
/// indices down the groups.
type CuePath = (usize, Vec<u32>);

/// Lookup cache for [`CueList`]: cue id → path. Fields of the list are edited directly all over
/// the app, so it is never trusted: a hit is checked against the cue it leads to, and a stale
/// entry falls back to walking the tree and rebuilds the cache. Copies start empty.
#[derive(Default)]
pub(crate) struct CueIndex(parking_lot::Mutex<HashMap<CueId, CuePath>>);

impl Clone for CueIndex {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for CueIndex {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for CueIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CueIndex")
    }
}

impl CueList {
    /// The cue at `path`, if the tree still has one there.
    fn lists_root(&self, root: usize) -> Option<&[Cue]> {
        match root {
            0 => Some(&self.cues),
            k => Some(&self.others.get(k - 1)?.cues),
        }
    }

    fn at_path(&self, (root, steps): &CuePath) -> Option<&Cue> {
        let mut list = self.lists_root(*root)?;
        let (last, groups) = steps.split_last()?;
        for &i in groups {
            list = list.get(i as usize)?.children();
        }
        list.get(*last as usize)
    }

    /// `f` of the cached path of `id`, if the cache knows it and the path still leads to it.
    fn with_cached<'a, R>(&'a self, id: CueId, f: impl FnOnce(&CuePath, &'a Cue) -> Option<R>) -> Option<R> {
        let map = self.index.0.lock();
        let path = map.get(&id)?;
        let cue = self.at_path(path).filter(|c| c.id == id)?;
        f(path, cue)
    }

    /// Rebuild the lookup cache from the tree.
    fn reindex(&self) {
        fn rec(list: &[Cue], root: usize, prefix: &mut Vec<u32>, out: &mut HashMap<CueId, CuePath>) {
            for (i, c) in list.iter().enumerate() {
                prefix.push(i as u32);
                out.insert(c.id, (root, prefix.clone()));
                rec(c.children(), root, prefix, out);
                prefix.pop();
            }
        }
        let mut map = HashMap::new();
        for (root, (_, cues)) in self.roots().enumerate() {
            rec(cues, root, &mut Vec::new(), &mut map);
        }
        *self.index.0.lock() = map;
    }

    /// [`CueList::with_cached`], rebuilding the cache first if it doesn't know `id` but the tree
    /// has it (the tree changed). Ids not in the tree cost a walk, as before the cache.
    fn with_path<'a, R>(&'a self, id: CueId, f: impl Fn(&CuePath, &'a Cue) -> Option<R>) -> Option<R> {
        if let Some(r) = self.with_cached(id, &f) {
            return Some(r);
        }
        if !self.contains_slow(id) {
            return None;
        }
        self.reindex();
        self.with_cached(id, f)
    }

    fn contains_slow(&self, id: CueId) -> bool {
        fn rec(list: &[Cue], id: CueId) -> bool {
            list.iter().any(|c| c.id == id || rec(c.children(), id))
        }
        self.roots().any(|(_, l)| rec(l, id))
    }

    /// Every list's top level with its parent id (None for the current list).
    fn roots(&self) -> impl Iterator<Item = (Option<CueId>, &[Cue])> {
        std::iter::once((None, self.cues.as_slice())).chain(self.others.iter().map(|o| (Some(o.info.id), o.cues.as_slice())))
    }

    /// Every cue of every list (depth first, current list first) with the name of its list when
    /// that isn't the current one. One pass: for per-frame UI such as target menus.
    pub fn iter_all_with_list(&self) -> Vec<(&Cue, Option<&str>)> {
        let mut out = Vec::new();
        let lists = std::iter::once((None, self.cues.as_slice())).chain(self.others.iter().map(|o| (Some(o.info.name.as_str()), o.cues.as_slice())));
        for (name, cues) in lists {
            for c in cues {
                c.visit(&mut |c| out.push((c, name)));
            }
        }
        out
    }

    /// Every list's state and top-level cues, the current one first.
    pub fn lists(&self) -> Vec<(&ListInfo, &[Cue])> {
        std::iter::once((&self.info, self.cues.as_slice())).chain(self.others.iter().map(|o| (&o.info, o.cues.as_slice()))).collect()
    }

    /// Cues with a timecode trigger in `(from, to]` (label-seconds) among `cues`, at any depth.
    pub fn timecode_due(cues: &[Cue], from: f64, to: f64) -> Vec<CueId> {
        let mut out = Vec::new();
        for c in cues {
            c.visit(&mut |c| {
                if c.armed && c.timecode_trigger.is_some_and(|t| t > from && t <= to) {
                    out.push(c.id);
                }
            });
        }
        out
    }

    /// The timecode settings of the list holding `id`.
    pub fn timecode_of(&self, id: CueId) -> &TimecodeSync {
        let mut top = id;
        while let Some(p) = self.parent_of(top) {
            if let Some(o) = self.others.iter().find(|o| o.info.id == p) {
                return &o.info.timecode;
            }
            top = p;
        }
        &self.info.timecode
    }

    /// Number of cue lists.
    pub fn list_count(&self) -> usize {
        self.others.len() + 1
    }

    /// Names of all lists in tab order, and the tab of the current one.
    pub fn list_names(&self) -> (Vec<String>, usize) {
        let mut names: Vec<String> = self.others.iter().map(|o| o.info.name.clone()).collect();
        let pos = self.pos.min(names.len());
        names.insert(pos, self.info.name.clone());
        (names, pos)
    }

    /// Every list as an [`OtherList`] in tab order (the current one included), consuming `self`'s lists.
    fn take_all(&mut self) -> Vec<OtherList> {
        let mut all = std::mem::take(&mut self.others);
        let cur = OtherList { info: std::mem::take(&mut self.info), cues: std::mem::take(&mut self.cues) };
        all.insert(self.pos.min(all.len()), cur);
        all
    }

    /// Make the list at tab `tab` current from a tab-ordered set of lists.
    fn put_all(&mut self, mut all: Vec<OtherList>, tab: usize) {
        let tab = tab.min(all.len() - 1);
        let cur = all.remove(tab);
        self.cues = cur.cues;
        self.info = cur.info;
        self.others = all;
        self.pos = tab;
    }

    /// Bring list `tab` forward. `playhead` is the outgoing list's playhead, kept with it; returns
    /// the incoming list's saved playhead.
    pub fn switch_to(&mut self, tab: usize, playhead: Option<CueId>) -> Option<CueId> {
        if tab >= self.list_count() || tab == self.pos.min(self.others.len()) {
            return playhead;
        }
        self.info.playhead = playhead;
        let all = self.take_all();
        self.put_all(all, tab);
        // A saved playhead may point at a cue that was since moved or deleted.
        self.info.playhead.filter(|p| self.in_current(*p))
    }

    /// True if `id` is in the current list (at any depth).
    pub fn in_current(&self, id: CueId) -> bool {
        let mut cur = id;
        loop {
            match self.location(cur) {
                None => return false,
                Some(Location { parent: None, .. }) => return true,
                Some(Location { parent: Some(p), .. }) => cur = p,
            }
        }
    }

    /// Add an empty list after the current one and make it current; returns its tab.
    pub fn add_list(&mut self, name: &str, playhead: Option<CueId>) -> usize {
        self.info.playhead = playhead;
        let mut all = self.take_all();
        let tab = self.pos.min(all.len() - 1) + 1;
        all.insert(tab, OtherList { info: ListInfo { name: name.into(), ..ListInfo::default() }, cues: Vec::new() });
        self.put_all(all, tab);
        tab
    }

    /// Delete the current list (not the last one) and bring the neighbouring list forward.
    /// Returns the deleted cues and the new list's saved playhead.
    pub fn remove_current_list(&mut self) -> Option<(Vec<Cue>, Option<CueId>)> {
        if self.others.is_empty() {
            return None;
        }
        let mut all = self.take_all();
        let tab = self.pos.min(all.len() - 1);
        let gone = all.remove(tab);
        self.put_all(all, tab.saturating_sub(1));
        Some((gone.cues, self.info.playhead))
    }

    /// Move the current list's tab by `delta` places.
    pub fn move_current_list(&mut self, delta: isize) {
        let n = self.list_count() as isize;
        self.pos = (self.pos as isize + delta).clamp(0, n - 1) as usize;
    }

    /// Name of the list holding `id` (any list).
    pub fn list_name_of(&self, id: CueId) -> Option<&str> {
        let mut top = id;
        while let Some(p) = self.parent_of(top) {
            if let Some(o) = self.others.iter().find(|o| o.info.id == p) {
                return Some(&o.info.name);
            }
            top = p;
        }
        self.find(id).map(|_| self.info.name.as_str())
    }

    /// Find a cue anywhere in the tree.
    pub fn find(&self, id: CueId) -> Option<&Cue> {
        self.with_path(id, |_, c| Some(c))
    }

    /// Find a cue anywhere in the tree, mutably.
    pub fn find_mut(&mut self, id: CueId) -> Option<&mut Cue> {
        fn rec(list: &mut [Cue], id: CueId) -> Option<&mut Cue> {
            for c in list {
                if c.id == id {
                    return Some(c);
                }
                if let Some(ch) = c.children_mut()
                    && let Some(f) = rec(ch, id)
                {
                    return Some(f);
                }
            }
            None
        }
        if let Some(c) = rec(&mut self.cues, id) {
            return Some(c);
        }
        self.others.iter_mut().find_map(|o| rec(&mut o.cues, id))
    }

    /// Where a cue sits in the tree.
    pub fn location(&self, id: CueId) -> Option<Location> {
        self.with_path(id, |(root, steps), _| {
            let (&index, groups) = steps.split_last()?;
            let parent = match (groups.split_last(), root) {
                (None, 0) => None,
                (None, k) => Some(self.others[k - 1].info.id),
                (Some((&g, outer)), _) => {
                    // The enclosing group: walk down to it (no allocation).
                    let mut list = self.lists_root(*root)?;
                    for &i in outer {
                        list = list.get(i as usize)?.children();
                    }
                    Some(list.get(g as usize)?.id)
                }
            };
            Some(Location { parent, index: index as usize })
        })
    }

    /// Group containing a cue.
    pub fn parent_of(&self, id: CueId) -> Option<CueId> {
        self.location(id).and_then(|l| l.parent)
    }

    /// The list of cues inside `parent` (None = top level).
    pub fn list(&self, parent: Option<CueId>) -> &[Cue] {
        match parent {
            None => &self.cues,
            Some(p) => match self.others.iter().find(|o| o.info.id == p) {
                Some(o) => &o.cues,
                None => self.find(p).map(|c| c.children()).unwrap_or(&[]),
            },
        }
    }

    fn list_mut(&mut self, parent: Option<CueId>) -> Option<&mut Vec<Cue>> {
        match parent {
            None => Some(&mut self.cues),
            Some(p) => match self.others.iter().position(|o| o.info.id == p) {
                Some(i) => Some(&mut self.others[i].cues),
                None => self.find_mut(p).and_then(|c| c.children_mut()),
            },
        }
    }

    /// The cue after this one at the same level.
    pub fn next_sibling(&self, id: CueId) -> Option<CueId> {
        let loc = self.location(id)?;
        self.list(loc.parent).get(loc.index + 1).map(|c| c.id)
    }

    /// True if `id` is `ancestor` or nested anywhere inside it.
    pub fn is_descendant_or_self(&self, id: CueId, ancestor: CueId) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = self.parent_of(c);
        }
        false
    }

    /// Take a cue out of the tree.
    pub fn remove(&mut self, id: CueId) -> Option<Cue> {
        let loc = self.location(id)?;
        let list = self.list_mut(loc.parent)?;
        Some(list.remove(loc.index))
    }

    /// Insert a cue into `parent` at `index` (clamped). Falls back to the top level if the
    /// parent isn't a group.
    pub fn insert(&mut self, parent: Option<CueId>, index: usize, cue: Cue) {
        let list = match self.list_mut(parent) {
            Some(l) => l,
            None => &mut self.cues,
        };
        let index = index.min(list.len());
        list.insert(index, cue);
    }

    /// Insert directly after `after` in the same list, or at the end of the top level.
    pub fn insert_after(&mut self, after: Option<CueId>, cue: Cue) {
        match after.and_then(|a| self.location(a)) {
            Some(loc) => self.insert(loc.parent, loc.index + 1, cue),
            None => self.cues.push(cue),
        }
    }

    #[allow(dead_code)]
    /// Move a cue to a new place. Refuses to move a group inside itself.
    /// `index` refers to the destination list *before* the cue is removed.
    pub fn move_cue(&mut self, id: CueId, parent: Option<CueId>, index: usize) -> bool {
        if let Some(p) = parent
            && self.is_descendant_or_self(p, id)
        {
            return false;
        }
        let Some(loc) = self.location(id) else { return false };
        let mut index = index;
        if loc.parent == parent && loc.index < index {
            index -= 1;
        }
        let Some(cue) = self.remove(id) else { return false };
        self.insert(parent, index, cue);
        true
    }

    /// All cues of every list in depth-first display order, the current list first.
    pub fn iter_all(&self) -> Vec<&Cue> {
        let mut out = Vec::new();
        for (_, l) in self.roots() {
            for c in l {
                c.visit(&mut |c| out.push(c));
            }
        }
        out
    }

    /// Top-level cues of every list (for walking the whole tree).
    pub fn all_roots_mut(&mut self) -> impl Iterator<Item = &mut Vec<Cue>> {
        std::iter::once(&mut self.cues).chain(self.others.iter_mut().map(|o| &mut o.cues))
    }

    /// All ids nested inside `id` (not including itself).
    pub fn descendants(&self, id: CueId) -> Vec<CueId> {
        let mut out = Vec::new();
        if let Some(c) = self.find(id) {
            for ch in c.children() {
                ch.visit(&mut |c| out.push(c.id));
            }
        }
        out
    }

    /// Suggest a cue number one greater than the largest integer-numbered cue.
    pub fn next_number(&self) -> String {
        let max = self
            .iter_all()
            .iter()
            .filter_map(|c| c.number.trim().parse::<f64>().ok())
            .fold(0.0_f64, f64::max);
        format!("{}", max.floor() as i64 + 1)
    }

    /// First cue in the list.
    pub fn first(&self) -> Option<CueId> {
        self.cues.first().map(|c| c.id)
    }

    /// Next cue in display order (descending into groups), used for arrow-key navigation.
    pub fn next_in_display(&self, id: CueId, collapsed: &dyn Fn(CueId) -> bool) -> Option<CueId> {
        let rows = self.visible_rows(collapsed);
        let i = rows.iter().position(|(_, c)| *c == id)?;
        rows.get(i + 1).map(|r| r.1)
    }

    /// The row above `id` as displayed, skipping collapsed groups' children.
    pub fn prev_in_display(&self, id: CueId, collapsed: &dyn Fn(CueId) -> bool) -> Option<CueId> {
        let rows = self.visible_rows(collapsed);
        let i = rows.iter().position(|(_, c)| *c == id)?;
        if i == 0 { None } else { Some(rows[i - 1].1) }
    }

    /// (depth, id) of every row visible in the cue list.
    pub fn visible_rows(&self, collapsed: &dyn Fn(CueId) -> bool) -> Vec<(usize, CueId)> {
        self.visible_cues(collapsed).into_iter().map(|(d, c)| (d, c.id)).collect()
    }

    /// Like [`Self::visible_rows`], with the cues themselves (one pass; no lookups by id).
    pub fn visible_cues(&self, collapsed: &dyn Fn(CueId) -> bool) -> Vec<(usize, &Cue)> {
        fn rec<'a>(list: &'a [Cue], depth: usize, collapsed: &dyn Fn(CueId) -> bool, out: &mut Vec<(usize, &'a Cue)>) {
            for c in list {
                out.push((depth, c));
                if c.is_group() && !collapsed(c.id) {
                    rec(c.children(), depth + 1, collapsed, out);
                }
            }
        }
        let mut out = Vec::new();
        rec(&self.cues, 0, collapsed, &mut out);
        out
    }
}

/// Workspace-wide settings.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Fade time used by "Stop all" (panic). Pressing it twice stops immediately.
    pub panic_fade: f64,
    /// Master output level in dB.
    pub master_volume_db: f32,
    /// Ignore a second GO within this many seconds (double-GO protection).
    pub double_go_protection: f64,
    /// cpal output device id (None = system default).
    pub audio_device: Option<String>,
    /// Listen for OSC remote control on UDP `osc_port`.
    pub osc_enabled: bool,
    /// UDP port for OSC input.
    pub osc_port: u16,
    /// The room for object audio: speaker positions and outputs (empty = a usual layout for the
    /// device's output count, see [`crate::spatial::default_room`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub speakers: Vec<Speaker>,
    /// MIDI/OSC control surfaces mapped onto executors, masters and GO.
    #[serde(skip_serializing_if = "crate::surface::SurfaceSettings::is_empty")]
    pub surfaces: crate::surface::SurfaceSettings,
    /// Loudness Audio cues are levelled to, LUFS ([`crate::loudness`]).
    pub loudness_target: f64,
    /// Extra audio outputs cues can play through (the main one is `audio_device`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patches: Vec<AudioPatch>,
    /// Decode movies on the graphics card's video engine where it takes them (this computer's
    /// choice: a replica's edit doesn't change it). Off: always on the CPU.
    pub hardware_video_decoding: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { panic_fade: 1.0, master_volume_db: 0.0, double_go_protection: 0.25, audio_device: None,
            osc_enabled: false,
            osc_port: 53000,
            speakers: Vec::new(),
            surfaces: Default::default(),
            loudness_target: crate::loudness::DEFAULT_TARGET,
            patches: Vec::new(),
            hardware_video_decoding: true,
        }
    }
}

/// A show file: cue list, settings and stages.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Workspace {
    /// File format version.
    pub format_version: u32,
    /// The cues.
    pub cue_list: CueList,
    /// Settings.
    pub settings: Settings,
    /// Video outputs.
    pub stages: Vec<Stage>,
    /// Lighting design (patch, groups, lighting cue sequences).
    #[serde(skip_serializing_if = "crate::light::Lighting::is_empty")]
    pub lighting: crate::light::Lighting,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            cue_list: CueList::default(),
            settings: Settings::default(),
            stages: vec![Stage::default()],
            lighting: Default::default(),
        }
    }
}

impl Workspace {
    /// The stage a video cue plays on (falls back to the first stage).
    pub fn stage_for(&self, stage: Option<StageId>) -> Option<&Stage> {
        stage.and_then(|id| self.stages.iter().find(|s| s.id == id)).or(self.stages.first())
    }

    /// Current file format version.
    pub const FORMAT_VERSION: u32 = 1;
    /// File extension of workspaces.
    pub const EXTENSION: &'static str = "openq";

    /// Largest show file read (a big rig's lighting design takes a few hundred MB).
    pub const MAX_FILE: u64 = 1 << 30;

    /// Read a workspace, resolving relative media paths against the file's folder. Only a regular
    /// file of at most [`Workspace::MAX_FILE`] bytes (see [`crate::recovery::read_file`]) and at
    /// most [`crate::limits::MAX_CUES`] cues; values are brought into their plausible ranges
    /// ([`crate::limits`]).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = crate::recovery::read_file(path, Self::MAX_FILE)?;
        let mut ws: Workspace = serde_json::from_slice(&s)?;
        crate::limits::check_counts(&ws).map_err(anyhow::Error::msg)?;
        ws.fix_duplicate_ids();
        crate::limits::workspace(&mut ws);
        if ws.stages.is_empty() {
            ws.stages.push(Stage::default());
        }
        // Resolve media paths stored relative to the workspace file.
        if let Some(dir) = path.parent() {
            ws.for_each_file_mut(&mut |p| {
                if p.is_relative() && !p.as_os_str().is_empty() {
                    *p = dir.join(&*p);
                }
            });
        }
        Ok(ws)
    }

    /// Give a fresh id to every cue whose id an earlier cue already has (a damaged or hand-edited
    /// file): the cue tree and the engine rely on ids being unique (a group holding a cue with its
    /// own id would be its own parent). Returns how many were changed.
    pub fn fix_duplicate_ids(&mut self) -> usize {
        fn walk(cues: &mut [Cue], seen: &mut std::collections::HashSet<CueId>, fixed: &mut usize) {
            for c in cues {
                if !seen.insert(c.id) {
                    c.id = CueId::new_v4();
                    seen.insert(c.id);
                    *fixed += 1;
                }
                if let Some(ch) = c.children_mut() {
                    walk(ch, seen, fixed);
                }
            }
        }
        let (mut seen, mut fixed) = (std::collections::HashSet::new(), 0);
        for list in self.cue_list.all_roots_mut() {
            walk(list, &mut seen, &mut fixed);
        }
        if fixed > 0 {
            log::warn!("{fixed} cues had the id of another cue; they got new ones");
        }
        fixed
    }

    /// Write a workspace, storing media paths relative to the file's folder.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut ws = self.clone();
        ws.format_version = Self::FORMAT_VERSION;
        // Store media next to (or below) the workspace as relative paths so shows are portable.
        if let Some(dir) = path.parent() {
            ws.for_each_file_mut(&mut |p| {
                if let Ok(rel) = p.strip_prefix(dir) {
                    *p = rel.to_path_buf();
                }
            });
        }
        // Written to a temporary file, flushed to disk, then renamed over the old one: a crash
        // or power cut mid-save leaves the previous version intact.
        crate::recovery::write_atomic(path, serde_json::to_string_pretty(&ws)?.as_bytes())
    }
}

fn for_each_media_path(list: &mut [Cue], f: &mut impl FnMut(&mut PathBuf)) {
    for c in list {
        if let Some(m) = c.kind.mask_mut() {
            f(&mut m.path);
        }
        match &mut c.kind {
            CueKind::Audio(a) => f(&mut a.path),
            CueKind::Video(v) => f(&mut v.path),
            CueKind::MidiFile(m) => f(&mut m.path),
            CueKind::Group { children, .. } => for_each_media_path(children, f),
            _ => {}
        }
    }
}

impl Workspace {
    /// Every file the show uses (media, MIDI files, masks of cues and stages), to rewrite paths.
    pub fn for_each_file_mut(&mut self, f: &mut impl FnMut(&mut PathBuf)) {
        for list in self.cue_list.all_roots_mut() {
            for_each_media_path(list, f);
        }
        for m in self.stages.iter_mut().filter_map(|s| s.mask.as_mut()) {
            f(&mut m.path);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn split_surface_reuses_and_adds_outputs() {
        use super::{Stage, split_surface};
        let surface = Stage { width: 3840, ..Stage::new("Cyc") };
        let mut right = Stage { span_of: Some(surface.id), crop: [0.5, 0.0, 1.0, 1.0], ..Stage::new("R") };
        right.width = 1400;
        let mut stages = vec![surface.clone(), right.clone()];
        assert_eq!(split_surface(&mut stages, surface.id, 3, 0.1, false, 18), Ok(3));
        assert_eq!(stages.len(), 4);
        // The existing output takes the first part (in its order on the surface), keeping its size.
        assert_eq!(stages[1].crop[0], 0.0);
        assert_eq!(stages[1].width, 1400);
        assert_eq!(stages[2].name, "Cyc 2");
        assert_eq!(stages[3].span_of, Some(surface.id));
        assert!((stages[3].crop[2] - 1.0).abs() < 1e-6);
        assert!(split_surface(&mut stages, surface.id, 8, 0.1, false, 6).is_err(), "over the stage count");
        assert_eq!(stages.len(), 4, "nothing changed");
    }

    #[test]
    fn cart_layout_keeps_put_cues_and_fills_gaps() {
        use super::{Cart, Cue, CueKind};
        let cues: Vec<Cue> = (0..6).map(|_| Cue::new(CueKind::Memo)).collect();
        let id = |i: usize| Some(cues[i].id);
        let mut cart = Cart { columns: 3, rows: 1, ..Default::default() };
        assert_eq!(cart.layout(&cues), [id(0), id(1), id(2), id(3), id(4), id(5)], "grows by whole rows");
        cart.put(&cues, cues[5].id, 7);
        assert_eq!(cart.layout(&cues), [id(0), id(1), id(2), id(3), id(4), None, None, id(5), None]);
        // Onto a taken cell: the two swap, nothing else moves.
        cart.put(&cues, cues[0].id, 4);
        assert_eq!(cart.layout(&cues), [id(4), id(1), id(2), id(3), id(0), None, None, id(5), None]);
        // A new cue takes the first free cell; a deleted one leaves its cell empty.
        let mut more = cues.clone();
        more.remove(1);
        more.push(Cue::new(CueKind::Memo));
        let grid = cart.layout(&more);
        assert_eq!(grid[1], Some(more[5].id));
        assert_eq!(grid[5], None);
        // Two cues claiming one cell (an edited file): the second goes to a free one.
        cart.cells.insert(cues[2].id, 3);
        let grid = cart.layout(&cues);
        assert_eq!(grid.iter().flatten().count(), 6);
        assert_eq!(grid[3], id(2));
        // A wild cell number doesn't make a huge grid.
        cart.cells.insert(cues[2].id, u32::MAX);
        assert!(cart.layout(&cues).len() < 20);
    }

    #[test]
    fn network_text_escapes_and_osc_over_tcp() {
        use super::{NetFormat, NetTransport, NetworkCue, parse_net_text, slip};
        assert_eq!(parse_net_text(r"GO 1\r\n").unwrap(), b"GO 1\r\n");
        assert_eq!(parse_net_text(r"a\x00\xff\\\t\0é").unwrap(), [b'a', 0, 0xff, b'\\', b'\t', 0, 0xc3, 0xa9]);
        assert!(parse_net_text(r"\x4").is_err() && parse_net_text(r"\q").is_err() && parse_net_text(r"\").is_err());
        assert_eq!(slip(&[1, 0xc0, 0xdb]), [0xc0, 1, 0xdb, 0xdc, 0xdb, 0xdd, 0xc0]);
        let mut n = NetworkCue::default();
        let udp = n.payload().unwrap();
        n.transport = NetTransport::Tcp;
        assert_eq!(n.payload().unwrap(), slip(&udp));
        n.format = NetFormat::Text;
        assert!(n.payload().is_err(), "nothing to send");
        n.text = "x".repeat(super::MAX_NET_TEXT + 1);
        assert!(n.payload().is_err());
    }

    /// A file with a cue id twice (here a group holding a cue with its own id) loads with fresh
    /// ids, and moving the playhead past it ends.
    #[test]
    fn duplicate_cue_ids_are_fixed_on_load() {
        use super::*;
        let mut ws = Workspace::default();
        let mut child = Cue::new(CueKind::Memo);
        let mut group = Cue::new(CueKind::Group { mode: GroupMode::StartFirstAndEnter, children: vec![] });
        child.id = group.id;
        group.children_mut().unwrap().push(child);
        ws.cue_list.cues = vec![group.clone(), group];
        let dir = std::env::temp_dir().join(format!("openq-dup-ids-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("dup.openq");
        std::fs::write(&p, serde_json::to_vec(&ws).unwrap()).unwrap();
        let back = Workspace::load(&p).unwrap();
        let all = back.cue_list.iter_all();
        let ids: std::collections::HashSet<CueId> = all.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), all.len());
        assert_eq!(all.len(), 4);
        assert!(crate::engine::after_chain(&back, all[1].id).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Crosspoints in a gang move by the change made to one of them, within the level range.
    #[test]
    fn gangs_move_together() {
        use super::*;
        let mut r = vec![vec![0.0, -10.0], vec![-6.0, MIN_DB]];
        let g = vec![vec![1, 1], vec![2, 1]];
        r[0][0] = -3.0;
        move_gang(&mut r, &g, 1, (0, 0), -3.0);
        assert_eq!(r, vec![vec![-3.0, -13.0], vec![-6.0, MIN_DB]]);
        assert_eq!(gang_letter(1), 'A');
    }

    /// The drawn curve's presets match the built-in ones, so turning a preset into a Custom curve
    /// (by dragging a handle) doesn't jump; and it keeps a fade's end points.
    #[test]
    fn custom_fade_curve_matches_presets() {
        for t in [0.0, 0.1, 0.25, 0.5, 0.8, 1.0] {
            for c in [FadeCurve::Linear, FadeCurve::SCurve] {
                let custom = FadeCurve::Custom(c.handles(false));
                assert!((custom.gain(t) - c.gain(t)).abs() < 1e-5, "{c:?} at {t}");
                assert!((custom.progress(t) - c.progress(t)).abs() < 1e-5, "{c:?} at {t}");
            }
        }
        let c = FadeCurve::Custom([0.9, 0.2]);
        assert_eq!((c.interpolate(1.0, 0.0, 0.0), c.interpolate(1.0, 0.0, 1.0)), (1.0, 0.0));
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<FadeCurve>(&json).unwrap(), c);
        assert_eq!(serde_json::from_str::<FadeCurve>("\"SCurve\"").unwrap(), FadeCurve::SCurve);
    }

    use super::*;

    fn group(children: Vec<Cue>) -> Cue {
        Cue::new(CueKind::Group { mode: GroupMode::StartAll, children })
    }

    #[test]
    fn tree_ops() {
        let a = Cue::new(CueKind::Memo);
        let b = Cue::new(CueKind::Memo);
        let c = Cue::new(CueKind::Memo);
        let (ai, bi, ci) = (a.id, b.id, c.id);
        let g = group(vec![b]);
        let gi = g.id;
        let mut l = CueList { cues: vec![a, g, c], ..Default::default() };

        assert_eq!(l.parent_of(bi), Some(gi));
        assert_eq!(l.next_sibling(ai), Some(gi));
        assert_eq!(l.next_sibling(bi), None);

        // move c into the group before b
        assert!(l.move_cue(ci, Some(gi), 0));
        assert_eq!(l.list(Some(gi)).iter().map(|c| c.id).collect::<Vec<_>>(), vec![ci, bi]);

        // a group can't be moved into itself
        assert!(!l.move_cue(gi, Some(gi), 0));

        // moving forward in the same list accounts for removal
        assert!(l.move_cue(ai, None, 2));
        assert_eq!(l.cues.iter().map(|c| c.id).collect::<Vec<_>>(), vec![gi, ai]);

        assert_eq!(l.descendants(gi).len(), 2);
        let rows = l.visible_rows(&|_| false);
        assert_eq!(rows.len(), 4);
        let rows = l.visible_rows(&|id| id == gi);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn slices_segment_and_map_positions() {
        let a = AudioCue {
            start_time: 1.0,
            end_time: Some(9.0),
            slices: vec![SliceMarker { time: 5.0, plays: 0 }, SliceMarker { time: 3.0, plays: 2 }, SliceMarker { time: 20.0, plays: 1 }],
            ..Default::default()
        };
        let segs = a.segments();
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0], Segment { start: 1.0, end: Some(3.0), plays: Some(2) });
        assert_eq!(segs[1], Segment { start: 3.0, end: Some(5.0), plays: None });
        assert_eq!(segs[2], Segment { start: 5.0, end: Some(9.0), plays: Some(1) });
        assert_eq!(a.file_time_total(10.0), None);
        // 2 passes of [1,3) then looping [3,5).
        assert!((a.file_position(1.0, 10.0) - 2.0).abs() < 1e-9);
        assert!((a.file_position(4.5, 10.0) - 3.5).abs() < 1e-9);
        assert!((a.file_position(6.5, 10.0) - 3.5).abs() < 1e-9);
        let b = AudioCue { slices: vec![SliceMarker { time: 2.0, plays: 3 }], ..Default::default() };
        assert_eq!(b.file_time_total(5.0), Some(2.0 * 3.0 + 3.0));
    }

    #[test]
    fn numbering() {
        let mut a = Cue::new(CueKind::Memo);
        a.number = "3.5".into();
        let mut b = Cue::new(CueKind::Memo);
        b.number = "x".into();
        let l = CueList { cues: vec![a, b], ..Default::default() };
        assert_eq!(l.next_number(), "4");
    }

    #[test]
    fn curves_hit_endpoints() {
        for c in FadeCurve::ALL {
            assert!((c.interpolate(1.0, 0.0, 0.0) - 1.0).abs() < 1e-4);
            assert!(c.interpolate(1.0, 0.0, 1.0).abs() < 1e-4);
            assert!((c.interpolate(0.0, 0.5, 1.0) - 0.5).abs() < 1e-4);
        }
    }

    #[test]
    fn workspace_roundtrip_relative_paths() {
        let dir = std::env::temp_dir().join(format!("openq-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ws = Workspace::default();
        let mut cue = Cue::new(KindTag::Audio.default_kind());
        if let CueKind::Audio(a) = &mut cue.kind {
            a.path = dir.join("sound.wav");
        }
        ws.cue_list.cues.push(cue);
        let file = dir.join("show.openq");
        ws.save(&file).unwrap();
        let raw = std::fs::read_to_string(&file).unwrap();
        assert!(raw.contains("\"sound.wav\""));
        let loaded = Workspace::load(&file).unwrap();
        assert_eq!(loaded.cue_list, ws.cue_list);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn several_lists_share_lookups_but_not_display() {
        let a = Cue::new(CueKind::Memo);
        let (b, c) = (Cue::new(CueKind::Memo), Cue::new(CueKind::Memo));
        let (ai, bi, ci) = (a.id, b.id, c.id);
        let mut l = CueList { cues: vec![a], ..Default::default() };
        assert_eq!(l.add_list("FX", Some(ai)), 1);
        assert!(l.cues.is_empty());
        l.cues.push(b);
        l.cues.push(c);
        // The first list is in the background now: still found, targetable and walkable.
        assert!(l.find(ai).is_some());
        assert!(!l.in_current(ai) && l.in_current(bi));
        assert_eq!(l.next_sibling(bi), Some(ci));
        assert_eq!(l.next_sibling(ai), None);
        assert_eq!(l.iter_all().len(), 3);
        assert_eq!(l.list_name_of(ai), Some("Main Cue List"));
        assert_eq!(l.visible_rows(&|_| false).len(), 2);
        // Switching back restores its playhead and tab order stays put.
        assert_eq!(l.switch_to(0, Some(ci)), Some(ai));
        assert_eq!(l.list_names(), (vec!["Main Cue List".to_string(), "FX".to_string()], 0));
        assert_eq!(l.switch_to(1, None), Some(ci));
        // Removing a background cue works through its list.
        assert!(l.remove(ai).is_some());
        let (gone, _) = l.remove_current_list().unwrap();
        assert_eq!(gone.len(), 2);
        assert_eq!(l.list_count(), 1);
        assert!(l.remove_current_list().is_none());
    }

    /// The lookup cache never answers from a stale tree: cues moved, deleted, regrouped or
    /// moved between lists through the public fields are still found in the right place.
    #[test]
    fn lookups_follow_direct_edits() {
        use super::*;
        let memo = || Cue::new(CueKind::Memo);
        let (a, b, inner) = (memo(), memo(), memo());
        let mut g = Cue::new(CueKind::Group { mode: GroupMode::default(), children: vec![inner.clone()] });
        g.number = "g".into();
        let mut l = CueList { cues: vec![a.clone(), g.clone(), b.clone()], ..Default::default() };
        l.others.push(OtherList { info: ListInfo::default(), cues: vec![memo()] });
        let other = l.others[0].cues[0].id;
        let bg = l.others[0].info.id;
        assert_eq!(l.location(inner.id), Some(Location { parent: Some(g.id), index: 0 }));
        assert_eq!(l.location(other), Some(Location { parent: Some(bg), index: 0 }));
        assert_eq!(l.next_sibling(a.id), Some(g.id));

        // Reorder and delete straight through the field.
        l.cues.swap(0, 2);
        assert_eq!(l.location(a.id), Some(Location { parent: None, index: 2 }));
        assert_eq!(l.next_sibling(b.id), Some(g.id));
        l.cues.remove(1);
        assert!(l.find(g.id).is_none());
        assert!(l.find(inner.id).is_none(), "children go with their group");
        // Undo puts it back somewhere else.
        l.cues.insert(0, g.clone());
        assert_eq!(l.find(inner.id).map(|c| c.id), Some(inner.id));
        assert_eq!(l.parent_of(inner.id), Some(g.id));
        // Moved to the background list.
        let moved = l.cues.pop().unwrap();
        l.others[0].cues.insert(0, moved.clone());
        assert_eq!(l.location(moved.id), Some(Location { parent: Some(bg), index: 0 }));
        assert_eq!(l.location(other), Some(Location { parent: Some(bg), index: 1 }));
        // A copy has its own (empty) cache and still works.
        let copy = l.clone();
        assert_eq!(copy.parent_of(inner.id), Some(g.id));
    }
}
