//! The lighting design: fixture types, the patch, groups and sequences of lighting cues.
//!
//! This is design data, edited in the Lighting workspace (Edit mode only) and saved in the show
//! file as `Workspace.lighting`. Playback never reads it directly: it is compiled into a flat
//! playback form first.
//!
//! Values are normalised `f32` in 0..=1 per [`Attribute`]; they become 8/16-bit DMX only at
//! output ([`ChannelDef::write`]).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// User-facing fixture number (unique in a show).
pub type FixtureId = u32;
/// User-facing group number.
pub type GroupId = u32;
/// User-facing sequence number.
pub type SequenceId = u32;
/// Stable id of a lighting cue (what OpenQ Light cues target).
pub type LightCueId = Uuid;

/// Highest universe number (Art-Net 15-bit port address, 1-based).
pub const MAX_UNIVERSE: u16 = 32768;
/// Slots in a DMX universe.
pub const UNIVERSE_SIZE: u16 = 512;

/// What a channel controls. Values are stored per attribute, so a look survives a mode change.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[allow(missing_docs)]
pub enum Attribute {
    Dimmer,
    Red,
    Green,
    Blue,
    White,
    Amber,
    Lime,
    Uv,
    Cyan,
    Magenta,
    Yellow,
    Cto,
    ColorWheel,
    Pan,
    Tilt,
    PanTiltSpeed,
    Zoom,
    Focus,
    Iris,
    Frost,
    Gobo,
    GoboRotation,
    Prism,
    Shutter,
    Control,
    Effect,
    EffectSpeed,
    Fog,
    Fan,
}

/// Attribute families, as on a desk's encoder bar.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[allow(missing_docs)]
pub enum Feature {
    Intensity,
    Colour,
    Position,
    Beam,
    Control,
}

impl Feature {
    /// Every feature, in encoder bar order.
    pub const ALL: [Feature; 5] = [Feature::Intensity, Feature::Colour, Feature::Position, Feature::Beam, Feature::Control];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Feature::Intensity => crate::i18n::t("Intensity"),
            Feature::Colour => crate::i18n::t("Colour"),
            Feature::Position => crate::i18n::t("Position"),
            Feature::Beam => crate::i18n::t("Beam"),
            Feature::Control => crate::i18n::t("Control"),
        }
    }
}

impl Attribute {
    /// Every attribute, grouped by feature.
    pub const ALL: [Attribute; 29] = [
        Attribute::Dimmer,
        Attribute::Red,
        Attribute::Green,
        Attribute::Blue,
        Attribute::White,
        Attribute::Amber,
        Attribute::Lime,
        Attribute::Uv,
        Attribute::Cyan,
        Attribute::Magenta,
        Attribute::Yellow,
        Attribute::Cto,
        Attribute::ColorWheel,
        Attribute::Pan,
        Attribute::Tilt,
        Attribute::PanTiltSpeed,
        Attribute::Zoom,
        Attribute::Focus,
        Attribute::Iris,
        Attribute::Frost,
        Attribute::Gobo,
        Attribute::GoboRotation,
        Attribute::Prism,
        Attribute::Shutter,
        Attribute::Control,
        Attribute::Effect,
        Attribute::EffectSpeed,
        Attribute::Fog,
        Attribute::Fan,
    ];

    /// The family this attribute belongs to.
    pub fn feature(self) -> Feature {
        use Attribute::*;
        match self {
            Dimmer => Feature::Intensity,
            Red | Green | Blue | White | Amber | Lime | Uv | Cyan | Magenta | Yellow | Cto | ColorWheel => Feature::Colour,
            Pan | Tilt | PanTiltSpeed => Feature::Position,
            Zoom | Focus | Iris | Frost | Gobo | GoboRotation | Prism | Shutter => Feature::Beam,
            Control | Effect | EffectSpeed | Fog | Fan => Feature::Control,
        }
    }

    /// Highest takes precedence (intensity); every other attribute is latest takes precedence.
    pub fn is_htp(self) -> bool {
        self == Attribute::Dimmer
    }

    /// Colour emitters scaled by the virtual dimmer of fixtures without a dimmer channel.
    pub fn is_emitter(self) -> bool {
        use Attribute::*;
        matches!(self, Red | Green | Blue | White | Amber | Lime | Uv)
    }

    /// Value of the attribute with nothing controlling it.
    pub fn home(self) -> f32 {
        use Attribute::*;
        match self {
            Pan | Tilt | Zoom | Focus => 0.5,
            Iris | Shutter => 1.0,
            _ => 0.0,
        }
    }

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        use crate::i18n::t;
        match self {
            Attribute::Dimmer => t("Dimmer"),
            Attribute::Red => t("Red"),
            Attribute::Green => t("Green"),
            Attribute::Blue => t("Blue"),
            Attribute::White => t("White"),
            Attribute::Amber => t("Amber"),
            Attribute::Lime => t("Lime"),
            Attribute::Uv => t("UV"),
            Attribute::Cyan => t("Cyan"),
            Attribute::Magenta => t("Magenta"),
            Attribute::Yellow => t("Yellow"),
            Attribute::Cto => t("CTO"),
            Attribute::ColorWheel => t("Colour wheel"),
            // "Pan" is translated for audio panning; desks keep the English word.
            Attribute::Pan => "Pan",
            Attribute::Tilt => t("Tilt"),
            Attribute::PanTiltSpeed => t("Pan/tilt speed"),
            Attribute::Zoom => t("Zoom"),
            Attribute::Focus => t("Focus"),
            Attribute::Iris => t("Iris"),
            Attribute::Frost => t("Frost"),
            Attribute::Gobo => t("Gobo"),
            Attribute::GoboRotation => t("Gobo rotation"),
            Attribute::Prism => t("Prism"),
            Attribute::Shutter => t("Shutter"),
            Attribute::Control => t("Control"),
            Attribute::Effect => t("Effect"),
            Attribute::EffectSpeed => t("Effect speed"),
            Attribute::Fog => t("Fog"),
            Attribute::Fan => t("Fan"),
        }
    }
}

/// One DMX channel (or a coarse/fine pair) of a fixture mode.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ChannelDef {
    /// What it controls.
    pub attribute: Attribute,
    /// Offset of the coarse byte from the fixture's start address (0-based).
    pub offset: u16,
    /// Offset of the fine byte (16-bit attributes).
    pub fine: Option<u16>,
    /// Value with nothing controlling it (`None` = [`Attribute::home`]).
    pub default: Option<f32>,
    /// Value while the fixture is highlighted (`None` = full for intensity/open for beam).
    pub highlight: Option<f32>,
}

impl Default for ChannelDef {
    fn default() -> Self {
        Self { attribute: Attribute::Dimmer, offset: 0, fine: None, default: None, highlight: None }
    }
}

impl ChannelDef {
    /// Value with nothing controlling the channel.
    pub fn default_value(&self) -> f32 {
        self.default.unwrap_or_else(|| self.attribute.home())
    }

    /// Write a normalised value into a universe starting at the fixture's address (0-based).
    /// Bytes past the end of the universe are dropped.
    pub fn write(&self, value: f32, start: usize, universe: &mut [u8]) {
        let v = value.clamp(0.0, 1.0);
        if let Some(fine) = self.fine {
            let v16 = (v * 65535.0).round() as u16;
            if let Some(b) = universe.get_mut(start + self.offset as usize) {
                *b = (v16 >> 8) as u8;
            }
            if let Some(b) = universe.get_mut(start + fine as usize) {
                *b = (v16 & 0xff) as u8;
            }
        } else if let Some(b) = universe.get_mut(start + self.offset as usize) {
            *b = (v * 255.0).round() as u8;
        }
    }
}

/// A DMX personality of a fixture type.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct FixtureMode {
    /// Mode name ("8 channel").
    pub name: String,
    /// Channels in this mode.
    pub channels: Vec<ChannelDef>,
}

impl FixtureMode {
    /// A mode from channels in DMX order; `(attribute, fine)` pairs, a fine byte right after its coarse one.
    pub fn sequential(name: &str, channels: &[(Attribute, bool)]) -> Self {
        let mut offset = 0;
        let channels = channels
            .iter()
            .map(|&(attribute, fine)| {
                let c = ChannelDef { attribute, offset, fine: fine.then_some(offset + 1), ..Default::default() };
                offset += 1 + fine as u16;
                c
            })
            .collect();
        Self { name: name.into(), channels }
    }

    /// DMX slots used.
    pub fn footprint(&self) -> u16 {
        self.channels.iter().map(|c| c.offset.max(c.fine.unwrap_or(0)) + 1).max().unwrap_or(0)
    }

    /// The channel driving an attribute.
    pub fn channel(&self, attr: Attribute) -> Option<&ChannelDef> {
        self.channels.iter().find(|c| c.attribute == attr)
    }

    /// True when intensity is a real channel; otherwise the fixture gets a virtual dimmer
    /// that scales its colour emitters.
    pub fn has_dimmer(&self) -> bool {
        self.channel(Attribute::Dimmer).is_some()
    }

    /// Attributes a fixture in this mode can take (the real channels plus a virtual dimmer).
    pub fn attributes(&self) -> Vec<Attribute> {
        let mut v: Vec<Attribute> = self.channels.iter().map(|c| c.attribute).collect();
        if !self.has_dimmer() && v.iter().any(|a| a.is_emitter()) {
            v.insert(0, Attribute::Dimmer);
        }
        v.sort();
        v.dedup();
        v
    }
}

/// Physical properties used for units in the UI and by the visualiser.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Physical {
    /// Full pan range in degrees.
    pub pan_range: f32,
    /// Full tilt range in degrees.
    pub tilt_range: f32,
    /// Beam angle range in degrees (zoom 0 → min, 1 → max).
    pub beam_min: f32,
    /// See `beam_min`.
    pub beam_max: f32,
    /// Moving head (has a yoke), so the visualiser draws one.
    pub moving: bool,
}

impl Default for Physical {
    fn default() -> Self {
        Self { pan_range: 540.0, tilt_range: 270.0, beam_min: 25.0, beam_max: 25.0, moving: false }
    }
}

/// A fixture definition. Patched types are copied into the show so shows stay portable.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct FixtureType {
    /// Unique key (`generic/rgb`, or a library/import id).
    pub id: String,
    /// Manufacturer.
    pub manufacturer: String,
    /// Model name.
    pub name: String,
    /// DMX modes.
    pub modes: Vec<FixtureMode>,
    /// Physical properties.
    pub physical: Physical,
}

impl FixtureType {
    /// A mode by name (falls back to the first one).
    pub fn mode(&self, name: &str) -> Option<&FixtureMode> {
        self.modes.iter().find(|m| m.name == name).or(self.modes.first())
    }
}

/// A patched fixture.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Fixture {
    /// Fixture number.
    pub id: FixtureId,
    /// Name.
    pub name: String,
    /// [`FixtureType::id`].
    pub fixture_type: String,
    /// [`FixtureMode::name`].
    pub mode: String,
    /// Universe (1-based).
    pub universe: u16,
    /// Start address (1..=512), 0 = not patched (no output, still programmable).
    pub address: u16,
    /// Position in metres (x right, y up, z towards the audience).
    pub position: [f32; 3],
    /// Rotation in degrees (x, y, z).
    pub rotation: [f32; 3],
    /// Reverse pan.
    pub invert_pan: bool,
    /// Reverse tilt.
    pub invert_tilt: bool,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            id: 1,
            name: String::new(),
            fixture_type: String::new(),
            mode: String::new(),
            universe: 1,
            address: 0,
            position: [0.0; 3],
            rotation: [0.0; 3],
            invert_pan: false,
            invert_tilt: false,
        }
    }
}

/// A named selection of fixtures (in selection order).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Group {
    /// Group number.
    pub id: GroupId,
    /// Name.
    pub name: String,
    /// Fixtures, in order.
    pub fixtures: Vec<FixtureId>,
}

/// User-facing preset number.
pub type PresetId = u32;

/// A value in a cue or the programmer: a level, or a reference to a preset (so editing the
/// preset changes every cue that uses it). Saved as a bare number or `{"preset": n}`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(untagged)]
pub enum Value {
    /// Normalised level, 0..=1.
    Level(f32),
    /// The preset's value for this fixture and attribute.
    Preset {
        /// Preset number.
        preset: PresetId,
    },
}

impl From<f32> for Value {
    fn from(v: f32) -> Self {
        Value::Level(v)
    }
}

/// Attribute values per fixture.
pub type Look = BTreeMap<FixtureId, BTreeMap<Attribute, Value>>;

/// Stored values for one feature (or all), per fixture. Cues and the programmer refer to them.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Preset {
    /// Preset number (unique across features).
    pub id: PresetId,
    /// Name.
    pub name: String,
    /// The family it stores (None = any attribute).
    pub feature: Option<Feature>,
    /// Values per fixture. A fixture without its own values takes those of the first fixture
    /// of the same type in the preset, so one fixture's values work for the whole type.
    pub values: BTreeMap<FixtureId, BTreeMap<Attribute, f32>>,
}

/// A lighting cue: the values it changes (tracking) and its timing.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct LightCue {
    /// Stable id; OpenQ Light cues refer to it.
    pub id: LightCueId,
    /// Number shown ("1", "1.5").
    pub number: String,
    /// Name.
    pub name: String,
    /// Fade time in seconds (intensity up and every other attribute).
    pub fade: f64,
    /// Fade time for intensity going down (`None` = `fade`).
    pub down_fade: Option<f64>,
    /// Delay before the fade starts, seconds.
    pub delay: f64,
    /// Values this cue sets; everything else tracks from earlier cues.
    pub values: Look,
}

impl LightCue {
    /// Seconds from GO until every fade of this cue is complete.
    pub fn duration(&self) -> f64 {
        self.delay.max(0.0) + self.fade.max(self.down_fade.unwrap_or(self.fade)).max(0.0)
    }
}

impl Default for LightCue {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            number: String::new(),
            name: String::new(),
            fade: 2.0,
            down_fade: None,
            delay: 0.0,
            values: Look::new(),
        }
    }
}

/// A list of lighting cues played in order (a cue stack), with tracking.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Sequence {
    /// Sequence number.
    pub id: SequenceId,
    /// Name.
    pub name: String,
    /// Cues in order.
    pub cues: Vec<LightCue>,
}

impl Sequence {
    /// Full state at cue `index`: every value set by it or tracked from an earlier cue.
    pub fn tracked(&self, index: usize) -> Look {
        let mut look = Look::new();
        for cue in self.cues.iter().take(index + 1) {
            for (fixture, values) in &cue.values {
                let state = look.entry(*fixture).or_default();
                state.extend(values.iter().map(|(a, v)| (*a, *v)));
            }
        }
        look
    }

    /// Index of a cue.
    pub fn index_of(&self, cue: LightCueId) -> Option<usize> {
        self.cues.iter().position(|c| c.id == cue)
    }
}

/// Replace references to preset `id` in `look` by the levels they stand for (dropped if none).
pub fn unlink_preset(l: &Lighting, id: PresetId, look: &mut Look) {
    for (f, values) in look.iter_mut() {
        values.retain(|a, v| {
            if *v != (Value::Preset { preset: id }) {
                return true;
            }
            match l.resolve(*f, *a, *v) {
                Some(x) => {
                    *v = Value::Level(x);
                    true
                }
                None => false,
            }
        });
    }
}

/// A patch problem shown in the patch window.
#[derive(Clone, PartialEq, Debug)]
pub enum PatchIssue {
    /// Two fixtures use the same fixture number.
    DuplicateId(FixtureId),
    /// The fixture's type or mode is missing from the show.
    UnknownType(FixtureId),
    /// The fixture runs past slot 512 or uses an invalid universe/address.
    OutOfRange(FixtureId),
    /// Two fixtures share DMX slots.
    Overlap(FixtureId, FixtureId),
}

/// The lighting design of a show.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Lighting {
    /// Fixture types used by the patch (copied from the library when patched).
    pub fixture_types: Vec<FixtureType>,
    /// Patched fixtures.
    pub fixtures: Vec<Fixture>,
    /// Groups.
    pub groups: Vec<Group>,
    /// Cue stacks.
    pub sequences: Vec<Sequence>,
    /// DMX outputs.
    pub outputs: Vec<super::dmx::OutputConfig>,
    /// Presets (palettes).
    pub presets: Vec<Preset>,
}

impl Lighting {
    /// Nothing designed yet (not written to the show file).
    pub fn is_empty(&self) -> bool {
        self.fixtures.is_empty() && self.fixture_types.is_empty() && self.groups.is_empty() && self.sequences.is_empty()
            && self.outputs.is_empty()
            && self.presets.is_empty()
    }

    /// A fixture by number.
    pub fn fixture(&self, id: FixtureId) -> Option<&Fixture> {
        self.fixtures.iter().find(|f| f.id == id)
    }

    /// A fixture type by id.
    pub fn fixture_type(&self, id: &str) -> Option<&FixtureType> {
        self.fixture_types.iter().find(|t| t.id == id)
    }

    /// The mode a fixture is patched in.
    pub fn mode_of(&self, f: &Fixture) -> Option<&FixtureMode> {
        self.fixture_type(&f.fixture_type)?.mode(&f.mode)
    }

    /// Add a fixture type unless one with the same id is there.
    pub fn add_type(&mut self, t: &FixtureType) {
        if self.fixture_type(&t.id).is_none() {
            self.fixture_types.push(t.clone());
        }
    }

    /// A sequence by number.
    pub fn sequence(&self, id: SequenceId) -> Option<&Sequence> {
        self.sequences.iter().find(|s| s.id == id)
    }

    /// Find a lighting cue anywhere: (sequence, index).
    pub fn find_cue(&self, cue: LightCueId) -> Option<(&Sequence, usize)> {
        self.sequences.iter().find_map(|s| s.index_of(cue).map(|i| (s, i)))
    }

    /// A preset by number.
    pub fn preset(&self, id: PresetId) -> Option<&Preset> {
        self.presets.iter().find(|p| p.id == id)
    }

    /// The level a value stands for on a fixture attribute (None: a preset that doesn't exist
    /// or has nothing for this fixture/attribute).
    pub fn resolve(&self, fixture: FixtureId, attr: Attribute, value: Value) -> Option<f32> {
        match value {
            Value::Level(v) => Some(v),
            Value::Preset { preset } => {
                let p = self.preset(preset)?;
                if let Some(v) = p.values.get(&fixture).and_then(|v| v.get(&attr)) {
                    return Some(*v);
                }
                let ty = &self.fixture(fixture)?.fixture_type;
                p.values
                    .iter()
                    .find(|(f, _)| self.fixture(**f).is_some_and(|x| &x.fixture_type == ty))
                    .and_then(|(_, v)| v.get(&attr).copied())
            }
        }
    }

    /// Delete a preset; cues that referred to it keep its values as plain levels.
    pub fn delete_preset(&mut self, id: PresetId) {
        let mut cues: Vec<LightCue> = self.sequences.iter().flat_map(|s| s.cues.iter().cloned()).collect();
        for c in &mut cues {
            unlink_preset(self, id, &mut c.values);
        }
        let mut it = cues.into_iter();
        for c in self.sequences.iter_mut().flat_map(|s| s.cues.iter_mut()) {
            *c = it.next().expect("same cues");
        }
        self.presets.retain(|p| p.id != id);
    }

    /// Attributes a preset holds for a fixture (its own or its type's).
    pub fn preset_attributes(&self, preset: PresetId, fixture: FixtureId) -> Vec<Attribute> {
        let Some(p) = self.preset(preset) else { return Vec::new() };
        if let Some(v) = p.values.get(&fixture) {
            return v.keys().copied().collect();
        }
        let Some(ty) = self.fixture(fixture).map(|f| &f.fixture_type) else { return Vec::new() };
        p.values
            .iter()
            .find(|(f, _)| self.fixture(**f).is_some_and(|x| &x.fixture_type == ty))
            .map(|(_, v)| v.keys().copied().collect())
            .unwrap_or_default()
    }

    /// "1/5 Name": sequence number / cue number and name, for menus and the cue list.
    pub fn cue_label(&self, cue: LightCueId) -> Option<String> {
        let (s, i) = self.find_cue(cue)?;
        let c = &s.cues[i];
        Some(format!("{}/{} {}", s.id, c.number, c.name).trim_end().to_string())
    }

    /// "1 Name" for a sequence.
    pub fn sequence_label(&self, id: SequenceId) -> Option<String> {
        self.sequence(id).map(|s| format!("{} {}", s.id, s.name).trim_end().to_string())
    }

    /// Give a fixture a new number, updating groups and cue values. False if `new` is taken.
    pub fn renumber_fixture(&mut self, old: FixtureId, new: FixtureId) -> bool {
        if old == new || self.fixture(new).is_some() || new == 0 {
            return false;
        }
        let Some(f) = self.fixtures.iter_mut().find(|f| f.id == old) else { return false };
        f.id = new;
        for g in &mut self.groups {
            g.fixtures.iter_mut().filter(|x| **x == old).for_each(|x| *x = new);
        }
        for c in self.sequences.iter_mut().flat_map(|s| s.cues.iter_mut()) {
            if let Some(v) = c.values.remove(&old) {
                c.values.insert(new, v);
            }
        }
        true
    }

    /// Unpatch fixtures and drop them from groups and cues.
    pub fn remove_fixtures(&mut self, ids: &std::collections::HashSet<FixtureId>) {
        self.fixtures.retain(|f| !ids.contains(&f.id));
        for g in &mut self.groups {
            g.fixtures.retain(|f| !ids.contains(f));
        }
        for c in self.sequences.iter_mut().flat_map(|s| s.cues.iter_mut()) {
            c.values.retain(|f, _| !ids.contains(f));
        }
        let used: std::collections::HashSet<&str> = self.fixtures.iter().map(|f| f.fixture_type.as_str()).collect();
        self.fixture_types.retain(|t| used.contains(t.id.as_str()));
    }

    /// Lowest unused fixture number at or above `from`.
    pub fn next_fixture_id(&self, from: FixtureId) -> FixtureId {
        let mut id = from.max(1);
        while self.fixture(id).is_some() {
            id += 1;
        }
        id
    }

    /// First start address in `universe` where `footprint` slots are free, if any.
    pub fn next_free_address(&self, universe: u16, footprint: u16) -> Option<u16> {
        let mut used = [false; UNIVERSE_SIZE as usize];
        for f in self.fixtures.iter().filter(|f| f.universe == universe && f.address > 0) {
            let n = self.mode_of(f).map_or(1, |m| m.footprint());
            for s in f.address..f.address + n {
                if let Some(u) = used.get_mut(s as usize - 1) {
                    *u = true;
                }
            }
        }
        let need = footprint.max(1) as usize;
        let mut run = 0;
        for (i, u) in used.iter().enumerate() {
            run = if *u { 0 } else { run + 1 };
            if run == need {
                return Some((i + 2 - need) as u16);
            }
        }
        None
    }

    /// Patch problems, in fixture order.
    pub fn validate(&self) -> Vec<PatchIssue> {
        let mut issues = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // (universe, first slot, last slot, fixture)
        let mut spans = Vec::new();
        for f in &self.fixtures {
            if !seen.insert(f.id) {
                issues.push(PatchIssue::DuplicateId(f.id));
            }
            let Some(mode) = self.mode_of(f) else {
                issues.push(PatchIssue::UnknownType(f.id));
                continue;
            };
            if f.address == 0 {
                continue;
            }
            let last = f.address as u32 + mode.footprint().max(1) as u32 - 1;
            if f.universe == 0 || f.universe > MAX_UNIVERSE || last > UNIVERSE_SIZE as u32 {
                issues.push(PatchIssue::OutOfRange(f.id));
                continue;
            }
            spans.push((f.universe, f.address as u32, last, f.id));
        }
        spans.sort();
        for w in spans.windows(2) {
            if w[0].0 == w[1].0 && w[1].1 <= w[0].2 {
                issues.push(PatchIssue::Overlap(w[0].3, w[1].3));
            }
        }
        issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::library;

    fn rig() -> Lighting {
        let mut l = Lighting::default();
        let rgb = library::generic("generic/rgb").unwrap();
        l.add_type(&rgb);
        for i in 0..4 {
            l.fixtures.push(Fixture {
                id: 101 + i,
                fixture_type: rgb.id.clone(),
                mode: rgb.modes[0].name.clone(),
                address: 1 + 3 * i as u16,
                ..Default::default()
            });
        }
        l
    }

    #[test]
    fn free_address_and_overlap() {
        let mut l = rig();
        assert!(l.validate().is_empty());
        assert_eq!(l.next_free_address(1, 3), Some(13));
        assert_eq!(l.next_free_address(2, 512), Some(1));
        assert_eq!(l.next_fixture_id(101), 105);
        l.fixtures[1].address = 2;
        assert_eq!(l.validate(), vec![PatchIssue::Overlap(101, 102)]);
        l.fixtures[1].address = 511;
        assert_eq!(l.validate(), vec![PatchIssue::OutOfRange(102)]);
        l.fixtures[1].id = 101;
        assert!(l.validate().contains(&PatchIssue::DuplicateId(101)));
    }

    #[test]
    fn renumber_and_remove() {
        let mut l = rig();
        l.groups.push(Group { id: 1, name: String::new(), fixtures: vec![101, 102] });
        let mut c = LightCue::default();
        c.values.entry(101).or_default().insert(Attribute::Red, 1.0.into());
        l.sequences.push(Sequence { id: 1, name: String::new(), cues: vec![c] });
        assert!(!l.renumber_fixture(101, 102), "taken");
        assert!(l.renumber_fixture(101, 201));
        assert_eq!(l.groups[0].fixtures, [201, 102]);
        assert!(l.sequences[0].cues[0].values.contains_key(&201));
        l.remove_fixtures(&[201].into_iter().collect());
        assert_eq!(l.groups[0].fixtures, [102]);
        assert!(l.sequences[0].cues[0].values.is_empty());
        l.remove_fixtures(&l.fixtures.iter().map(|f| f.id).collect());
        assert!(l.fixture_types.is_empty(), "unused types dropped");
    }

    #[test]
    fn tracking_folds_earlier_cues() {
        let mut s = Sequence::default();
        for (fix, attr, v) in [(1, Attribute::Dimmer, 1.0), (2, Attribute::Red, 0.5), (1, Attribute::Dimmer, 0.2)] {
            let mut c = LightCue::default();
            c.values.entry(fix).or_default().insert(attr, v.into());
            s.cues.push(c);
        }
        assert_eq!(s.tracked(0)[&1][&Attribute::Dimmer], 1.0.into());
        let t = s.tracked(2);
        assert_eq!(t[&1][&Attribute::Dimmer], 0.2.into());
        assert_eq!(t[&2][&Attribute::Red], 0.5.into());
    }

    #[test]
    fn dmx_write_coarse_fine() {
        let mut u = [0u8; 512];
        let c = ChannelDef { attribute: Attribute::Pan, offset: 0, fine: Some(1), ..Default::default() };
        c.write(0.5, 10, &mut u);
        assert_eq!((u[10], u[11]), (0x80, 0x00));
        let d = ChannelDef { attribute: Attribute::Dimmer, offset: 2, ..Default::default() };
        d.write(1.0, 510, &mut u); // past the end: dropped, no panic
        d.write(1.0, 0, &mut u);
        assert_eq!(u[2], 255);
    }

    #[test]
    fn virtual_dimmer_and_footprint() {
        let rgb = library::generic("generic/rgb").unwrap();
        let m = &rgb.modes[0];
        assert_eq!(m.footprint(), 3);
        assert!(!m.has_dimmer());
        assert_eq!(m.attributes()[0], Attribute::Dimmer);
        let spot = library::generic("generic/spot").unwrap();
        assert!(spot.modes[0].footprint() > 10);
    }

    #[test]
    fn presets_resolve_per_fixture_or_type() {
        let mut l = rig();
        let mut values = BTreeMap::new();
        values.insert(101, [(Attribute::Red, 0.7)].into_iter().collect());
        l.presets.push(Preset { id: 1, name: "Red".into(), feature: Some(Feature::Colour), values });
        let p = Value::Preset { preset: 1 };
        assert_eq!(l.resolve(101, Attribute::Red, p), Some(0.7));
        assert_eq!(l.resolve(103, Attribute::Red, p), Some(0.7), "same type");
        assert_eq!(l.resolve(103, Attribute::Green, p), None);
        assert_eq!(l.resolve(103, Attribute::Red, Value::Preset { preset: 9 }), None);
        assert_eq!(l.preset_attributes(1, 104), [Attribute::Red]);
        let mut cue = LightCue::default();
        cue.values.entry(103).or_default().insert(Attribute::Red, p);
        l.sequences.push(Sequence { id: 1, name: String::new(), cues: vec![cue] });
        let mut d = l.clone();
        d.delete_preset(1);
        assert_eq!(d.sequences[0].cues[0].values[&103][&Attribute::Red], Value::Level(0.7), "unlinked, not lost");
        // Saved as a bare number or {"preset": n}.
        let mut c = LightCue::default();
        c.values.entry(101).or_default().insert(Attribute::Red, p);
        c.values.entry(101).or_default().insert(Attribute::Dimmer, 0.5.into());
        let json = serde_json::to_string(&c.values).unwrap();
        assert_eq!(json, r#"{"101":{"Dimmer":0.5,"Red":{"preset":1}}}"#);
        assert_eq!(serde_json::from_str::<Look>(&json).unwrap(), c.values);
    }

    #[test]
    fn roundtrip_json() {
        let mut l = rig();
        let mut c = LightCue::default();
        c.values.entry(101).or_default().insert(Attribute::Red, 0.25.into());
        l.sequences.push(Sequence { id: 1, name: "Main".into(), cues: vec![c] });
        let s = serde_json::to_string(&l).unwrap();
        assert_eq!(serde_json::from_str::<Lighting>(&s).unwrap(), l);
    }
}
