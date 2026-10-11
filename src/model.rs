//! Show data model: cues, cue tree operations and the workspace file format.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub type CueId = Uuid;

/// Lowest level (in dB) we treat as audible; anything at or below is silence.
pub const MIN_DB: f32 = -60.0;

pub fn db_to_gain(db: f32) -> f32 {
    if db <= MIN_DB { 0.0 } else { 10f32.powf(db / 20.0) }
}

pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.0 { MIN_DB } else { (20.0 * gain.log10()).max(MIN_DB) }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ContinueMode {
    #[default]
    DoNotContinue,
    /// Fire the next cue once this cue's post-wait has elapsed.
    AutoContinue,
    /// Fire the next cue when this cue's action completes.
    AutoFollow,
}

impl ContinueMode {
    pub const ALL: [ContinueMode; 3] =
        [ContinueMode::DoNotContinue, ContinueMode::AutoContinue, ContinueMode::AutoFollow];

    pub fn label(self) -> &'static str {
        match self {
            ContinueMode::DoNotContinue => "Do not continue",
            ContinueMode::AutoContinue => "Auto-continue",
            ContinueMode::AutoFollow => "Auto-follow",
        }
    }
}

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
}

impl GroupMode {
    pub const ALL: [GroupMode; 4] = [
        GroupMode::StartFirstAndEnter,
        GroupMode::StartFirst,
        GroupMode::StartAll,
        GroupMode::StartRandom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            GroupMode::StartFirstAndEnter => "Start first child and enter group",
            GroupMode::StartFirst => "Start first child and go to next cue (playlist)",
            GroupMode::StartAll => "Start all children simultaneously",
            GroupMode::StartRandom => "Start random child and go to next cue",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FadeCurve {
    Linear,
    #[default]
    SCurve,
    /// Linear in dB (perceptually even).
    Logarithmic,
}

impl FadeCurve {
    pub const ALL: [FadeCurve; 3] = [FadeCurve::Linear, FadeCurve::SCurve, FadeCurve::Logarithmic];

    pub fn label(self) -> &'static str {
        match self {
            FadeCurve::Linear => "Linear",
            FadeCurve::SCurve => "S-curve",
            FadeCurve::Logarithmic => "Logarithmic",
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
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CueColor {
    #[default]
    None,
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl CueColor {
    pub const ALL: [CueColor; 7] = [
        CueColor::None,
        CueColor::Red,
        CueColor::Orange,
        CueColor::Yellow,
        CueColor::Green,
        CueColor::Blue,
        CueColor::Purple,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CueColor::None => "None",
            CueColor::Red => "Red",
            CueColor::Orange => "Orange",
            CueColor::Yellow => "Yellow",
            CueColor::Green => "Green",
            CueColor::Blue => "Blue",
            CueColor::Purple => "Purple",
        }
    }

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

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum VideoFit {
    #[default]
    Fit,
    Fill,
    Stretch,
}

impl VideoFit {
    pub const ALL: [VideoFit; 3] = [VideoFit::Fit, VideoFit::Fill, VideoFit::Stretch];

    pub fn label(self) -> &'static str {
        match self {
            VideoFit::Fit => "Fit (letterbox)",
            VideoFit::Fill => "Fill (crop)",
            VideoFit::Stretch => "Stretch",
        }
    }
}

/// Most source channels a cue's routing matrix handles.
pub const MAX_SRC_CHANNELS: usize = 8;
/// Most output channels a cue's routing matrix handles.
pub const MAX_OUT_CHANNELS: usize = 16;
pub const MAX_EQ_BANDS: usize = 8;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum EqKind {
    HighPass,
    LowShelf,
    Peak,
    HighShelf,
    LowPass,
}

impl EqKind {
    pub const ALL: [EqKind; 5] = [EqKind::HighPass, EqKind::LowShelf, EqKind::Peak, EqKind::HighShelf, EqKind::LowPass];

    pub fn label(self) -> &'static str {
        match self {
            EqKind::HighPass => "High-pass",
            EqKind::LowShelf => "Low shelf",
            EqKind::Peak => "Peak",
            EqKind::HighShelf => "High shelf",
            EqKind::LowPass => "Low-pass",
        }
    }

    pub fn has_gain(self) -> bool {
        matches!(self, EqKind::LowShelf | EqKind::Peak | EqKind::HighShelf)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct EqBand {
    pub enabled: bool,
    pub kind: EqKind,
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Eq {
    pub enabled: bool,
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
    pub scale_x: f32,
    pub scale_y: f32,
    /// Degrees clockwise.
    pub rotation: f32,
}

impl Default for Geometry {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0, scale_x: 1.0, scale_y: 1.0, rotation: 0.0 }
    }
}

impl Geometry {
    pub fn lerp(&self, to: &Geometry, t: f32) -> Geometry {
        let l = |a: f32, b: f32| a + (b - a) * t;
        Geometry {
            x: l(self.x, to.x),
            y: l(self.y, to.y),
            scale_x: l(self.scale_x, to.scale_x),
            scale_y: l(self.scale_y, to.scale_y),
            rotation: l(self.rotation, to.rotation),
        }
    }
}

pub type StageId = Uuid;

/// A video output (projector/screen). Its canvas is warped onto the window with four corner
/// pins for keystone correction / projection mapping.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Stage {
    pub id: StageId,
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Window-normalised positions of the canvas corners: top-left, top-right, bottom-right,
    /// bottom-left.
    pub corners: [[f32; 2]; 4],
}

impl Stage {
    pub const IDENTITY: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];

    pub fn new(name: &str) -> Self {
        Self { id: Uuid::new_v4(), name: name.into(), width: 1920, height: 1080, corners: Self::IDENTITY }
    }
}

impl Default for Stage {
    fn default() -> Self {
        Self::new("Main")
    }
}

/// A slice marker: the slice *ending* at `time` plays `plays` times (0 = until devamped).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct SliceMarker {
    pub time: f64,
    pub plays: u32,
}

/// One playable section of an audio file.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Segment {
    pub start: f64,
    /// None = end of file.
    pub end: Option<f64>,
    /// None = loop until devamped / stopped.
    pub plays: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct AudioCue {
    pub path: PathBuf,
    pub volume_db: f32,
    /// -1 (left) .. 1 (right).
    pub pan: f32,
    /// Seconds into the file to start from.
    pub start_time: f64,
    /// Seconds into the file to stop at (None = end of file).
    pub end_time: Option<f64>,
    pub play_count: u32,
    pub infinite_loop: bool,
    /// Playback speed (varispeed: pitch follows). 1 = normal.
    pub rate: f32,
    pub eq: Eq,
    /// Custom routing in dB, `[source channel][output channel]`; None = automatic.
    pub routing: Option<Vec<Vec<f32>>>,
    /// Slice markers between start and end; the last slice uses the cue's play count.
    pub slices: Vec<SliceMarker>,
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
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct VideoCue {
    pub path: PathBuf,
    pub volume_db: f32,
    /// 0..1
    pub opacity: f32,
    /// Higher layers draw on top.
    pub layer: i32,
    pub fit: VideoFit,
    pub start_time: f64,
    /// Seconds into the file to stop at (None = end of file).
    pub end_time: Option<f64>,
    pub play_count: u32,
    pub infinite_loop: bool,
    /// Keep showing the last frame when playback ends instead of clearing.
    pub hold_last_frame: bool,
    /// Output stage (None = the first stage).
    pub stage: Option<StageId>,
    pub geometry: Geometry,
    /// For still images: seconds to show (0 = until stopped).
    pub still_duration: f64,
    /// Soundtrack routing in dB `[source channel][output channel]`; None = automatic.
    pub routing: Option<Vec<Vec<f32>>>,
}

impl Default for VideoCue {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            volume_db: 0.0,
            opacity: 1.0,
            layer: 0,
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
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct FadeCue {
    pub target: Option<CueId>,
    pub duration: f64,
    pub curve: FadeCurve,
    /// Fade audio level to this value (None = leave level alone).
    pub volume_db: Option<f32>,
    /// Fade video opacity to this value (None = leave opacity alone).
    pub opacity: Option<f32>,
    /// Fade video geometry to this value (None = leave geometry alone).
    pub geometry: Option<Geometry>,
    pub stop_target_when_done: bool,
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
        }
    }
}

/// Sends one OSC message over UDP.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct NetworkCue {
    pub host: String,
    pub port: u16,
    pub address: String,
    /// Space-separated arguments; integers and decimals are sent as numbers, the rest as strings
    /// ("quoted text" keeps spaces).
    pub args: String,
}

impl Default for NetworkCue {
    fn default() -> Self {
        Self { host: "127.0.0.1".into(), port: 53000, address: "/go".into(), args: String::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum CueKind {
    Audio(AudioCue),
    Video(VideoCue),
    Group { mode: GroupMode, children: Vec<Cue> },
    Wait { duration: f64 },
    Memo,
    Fade(FadeCue),
    /// Start the target cue.
    Start { target: Option<CueId> },
    /// Stop the target cue (None = stop everything else).
    Stop { target: Option<CueId> },
    /// Pause the target cue (None = pause everything else).
    Pause { target: Option<CueId> },
    /// Move the playhead to the target cue.
    Goto { target: Option<CueId> },
    /// Make the target audio cue leave its looping slice and carry on.
    Devamp { target: Option<CueId> },
    Network(NetworkCue),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KindTag {
    Audio,
    Video,
    Group,
    Wait,
    Memo,
    Fade,
    Start,
    Stop,
    Pause,
    Goto,
    Devamp,
    Network,
}

impl KindTag {
    pub const ALL: [KindTag; 12] = [
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
    ];

    pub fn label(self) -> &'static str {
        match self {
            KindTag::Audio => "Audio",
            KindTag::Video => "Video",
            KindTag::Group => "Group",
            KindTag::Wait => "Wait",
            KindTag::Memo => "Memo",
            KindTag::Fade => "Fade",
            KindTag::Start => "Start",
            KindTag::Stop => "Stop",
            KindTag::Pause => "Pause",
            KindTag::Goto => "Go To",
            KindTag::Devamp => "Devamp",
            KindTag::Network => "Network",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            KindTag::Audio => "🔊",
            KindTag::Video => "🎬",
            KindTag::Group => "📁",
            KindTag::Wait => "⏳",
            KindTag::Memo => "📝",
            KindTag::Fade => "📉",
            KindTag::Start => "▶",
            KindTag::Stop => "⏹",
            KindTag::Pause => "⏸",
            KindTag::Goto => "➡",
            KindTag::Devamp => "⏭",
            KindTag::Network => "🖧",
        }
    }

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
        }
    }
}

impl CueKind {
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
            | CueKind::Devamp { target } => *target,
            _ => None,
        }
    }

    pub fn target_mut(&mut self) -> Option<&mut Option<CueId>> {
        match self {
            CueKind::Fade(f) => Some(&mut f.target),
            CueKind::Start { target }
            | CueKind::Stop { target }
            | CueKind::Pause { target }
            | CueKind::Goto { target }
            | CueKind::Devamp { target } => Some(target),
            _ => None,
        }
    }

    pub fn media_path(&self) -> Option<&Path> {
        match self {
            CueKind::Audio(a) => Some(&a.path),
            CueKind::Video(v) => Some(&v.path),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Cue {
    pub id: CueId,
    #[serde(default)]
    pub number: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub color: CueColor,
    #[serde(default = "default_true")]
    pub armed: bool,
    #[serde(default)]
    pub pre_wait: f64,
    #[serde(default)]
    pub post_wait: f64,
    #[serde(default)]
    pub continue_mode: ContinueMode,
    /// Keyboard key that fires this cue in show mode (e.g. "F1", "A").
    #[serde(default)]
    pub hotkey: Option<String>,
    pub kind: CueKind,
}

fn default_true() -> bool {
    true
}

impl Cue {
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
            kind,
        }
    }

    pub fn is_group(&self) -> bool {
        matches!(self.kind, CueKind::Group { .. })
    }

    pub fn children(&self) -> &[Cue] {
        match &self.kind {
            CueKind::Group { children, .. } => children,
            _ => &[],
        }
    }

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

    fn visit<'a>(&'a self, f: &mut impl FnMut(&'a Cue)) {
        f(self);
        for c in self.children() {
            c.visit(f);
        }
    }
}

/// Where a cue lives in the tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Location {
    pub parent: Option<CueId>,
    pub index: usize,
}

/// The cue tree. Top-level list plus nested groups.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct CueList {
    pub cues: Vec<Cue>,
}

impl CueList {
    pub fn find(&self, id: CueId) -> Option<&Cue> {
        fn rec(list: &[Cue], id: CueId) -> Option<&Cue> {
            for c in list {
                if c.id == id {
                    return Some(c);
                }
                if let Some(f) = rec(c.children(), id) {
                    return Some(f);
                }
            }
            None
        }
        rec(&self.cues, id)
    }

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
        rec(&mut self.cues, id)
    }

    pub fn location(&self, id: CueId) -> Option<Location> {
        fn rec(list: &[Cue], parent: Option<CueId>, id: CueId) -> Option<Location> {
            for (i, c) in list.iter().enumerate() {
                if c.id == id {
                    return Some(Location { parent, index: i });
                }
                if let Some(l) = rec(c.children(), Some(c.id), id) {
                    return Some(l);
                }
            }
            None
        }
        rec(&self.cues, None, id)
    }

    pub fn parent_of(&self, id: CueId) -> Option<CueId> {
        self.location(id).and_then(|l| l.parent)
    }

    /// The list of cues inside `parent` (None = top level).
    pub fn list(&self, parent: Option<CueId>) -> &[Cue] {
        match parent {
            None => &self.cues,
            Some(p) => self.find(p).map(|c| c.children()).unwrap_or(&[]),
        }
    }

    fn list_mut(&mut self, parent: Option<CueId>) -> Option<&mut Vec<Cue>> {
        match parent {
            None => Some(&mut self.cues),
            Some(p) => self.find_mut(p).and_then(|c| c.children_mut()),
        }
    }

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

    /// All cues in depth-first display order.
    pub fn iter_all(&self) -> Vec<&Cue> {
        let mut out = Vec::new();
        for c in &self.cues {
            c.visit(&mut |c| out.push(c));
        }
        out
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

    pub fn first(&self) -> Option<CueId> {
        self.cues.first().map(|c| c.id)
    }

    /// Next cue in display order (descending into groups), used for arrow-key navigation.
    pub fn next_in_display(&self, id: CueId, collapsed: &dyn Fn(CueId) -> bool) -> Option<CueId> {
        let rows = self.visible_rows(collapsed);
        let i = rows.iter().position(|(_, c)| *c == id)?;
        rows.get(i + 1).map(|r| r.1)
    }

    pub fn prev_in_display(&self, id: CueId, collapsed: &dyn Fn(CueId) -> bool) -> Option<CueId> {
        let rows = self.visible_rows(collapsed);
        let i = rows.iter().position(|(_, c)| *c == id)?;
        if i == 0 { None } else { Some(rows[i - 1].1) }
    }

    /// (depth, id) of every row visible in the cue list.
    pub fn visible_rows(&self, collapsed: &dyn Fn(CueId) -> bool) -> Vec<(usize, CueId)> {
        fn rec(list: &[Cue], depth: usize, collapsed: &dyn Fn(CueId) -> bool, out: &mut Vec<(usize, CueId)>) {
            for c in list {
                out.push((depth, c.id));
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

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Fade time used by "Stop all" (panic). Pressing it twice stops immediately.
    pub panic_fade: f64,
    pub master_volume_db: f32,
    /// Ignore a second GO within this many seconds (double-GO protection).
    pub double_go_protection: f64,
    /// cpal output device id (None = system default).
    pub audio_device: Option<String>,
    /// Listen for OSC remote control on UDP `osc_port`.
    pub osc_enabled: bool,
    pub osc_port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self { panic_fade: 1.0, master_volume_db: 0.0, double_go_protection: 0.25, audio_device: None,
            osc_enabled: false,
            osc_port: 53000,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Workspace {
    pub format_version: u32,
    pub cue_list: CueList,
    pub settings: Settings,
    pub stages: Vec<Stage>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            cue_list: CueList::default(),
            settings: Settings::default(),
            stages: vec![Stage::default()],
        }
    }
}

impl Workspace {
    /// The stage a video cue plays on (falls back to the first stage).
    pub fn stage_for(&self, stage: Option<StageId>) -> Option<&Stage> {
        stage.and_then(|id| self.stages.iter().find(|s| s.id == id)).or(self.stages.first())
    }

    pub const FORMAT_VERSION: u32 = 1;
    pub const EXTENSION: &'static str = "openq";

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let mut ws: Workspace = serde_json::from_str(&s)?;
        if ws.stages.is_empty() {
            ws.stages.push(Stage::default());
        }
        // Resolve media paths stored relative to the workspace file.
        if let Some(dir) = path.parent() {
            for_each_media_path(&mut ws.cue_list.cues, &mut |p| {
                if p.is_relative() && !p.as_os_str().is_empty() {
                    *p = dir.join(&*p);
                }
            });
        }
        Ok(ws)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut ws = self.clone();
        ws.format_version = Self::FORMAT_VERSION;
        // Store media next to (or below) the workspace as relative paths so shows are portable.
        if let Some(dir) = path.parent() {
            for_each_media_path(&mut ws.cue_list.cues, &mut |p| {
                if let Ok(rel) = p.strip_prefix(dir) {
                    *p = rel.to_path_buf();
                }
            });
        }
        let s = serde_json::to_string_pretty(&ws)?;
        let tmp = path.with_extension("openq.tmp");
        std::fs::write(&tmp, s)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn for_each_media_path(list: &mut [Cue], f: &mut impl FnMut(&mut PathBuf)) {
    for c in list {
        match &mut c.kind {
            CueKind::Audio(a) => f(&mut a.path),
            CueKind::Video(v) => f(&mut v.path),
            CueKind::Group { children, .. } => for_each_media_path(children, f),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
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
        let mut l = CueList { cues: vec![a, g, c] };

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
        let l = CueList { cues: vec![a, b] };
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
}
