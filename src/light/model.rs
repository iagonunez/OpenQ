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
/// Saved as its name (`"Gobo2"`, `"Extra3"`), so it can key JSON maps.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
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
    ColorMacro,
    Pan,
    Tilt,
    PanTiltSpeed,
    Zoom,
    Focus,
    Iris,
    Frost,
    Gobo,
    GoboRotation,
    Gobo2,
    Gobo2Rotation,
    Prism,
    PrismRotation,
    Shutter,
    Control,
    Effect,
    EffectSpeed,
    Fog,
    Fan,
    ColorWheel2,
    Frost2,
    Prism2,
    Control2,
    Control3,
    Effect2,
    Effect2Speed,
    /// A further channel of a unit whose attribute and its alternates are all taken (a third
    /// gobo wheel, a fourth control channel), numbered from 1 within the unit. Control family,
    /// latest takes precedence; [`ChannelDef::name`] says what it is.
    Extra(u8),
}

impl Attribute {
    /// Saved name.
    pub fn name(self) -> std::borrow::Cow<'static, str> {
        use Attribute::*;
        std::borrow::Cow::Borrowed(match self {
            Dimmer => "Dimmer",
            Red => "Red",
            Green => "Green",
            Blue => "Blue",
            White => "White",
            Amber => "Amber",
            Lime => "Lime",
            Uv => "Uv",
            Cyan => "Cyan",
            Magenta => "Magenta",
            Yellow => "Yellow",
            Cto => "Cto",
            ColorWheel => "ColorWheel",
            ColorMacro => "ColorMacro",
            Pan => "Pan",
            Tilt => "Tilt",
            PanTiltSpeed => "PanTiltSpeed",
            Zoom => "Zoom",
            Focus => "Focus",
            Iris => "Iris",
            Frost => "Frost",
            Gobo => "Gobo",
            GoboRotation => "GoboRotation",
            Gobo2 => "Gobo2",
            Gobo2Rotation => "Gobo2Rotation",
            Prism => "Prism",
            PrismRotation => "PrismRotation",
            Shutter => "Shutter",
            Control => "Control",
            Effect => "Effect",
            EffectSpeed => "EffectSpeed",
            Fog => "Fog",
            Fan => "Fan",
            ColorWheel2 => "ColorWheel2",
            Frost2 => "Frost2",
            Prism2 => "Prism2",
            Control2 => "Control2",
            Control3 => "Control3",
            Effect2 => "Effect2",
            Effect2Speed => "Effect2Speed",
            Extra(n) => return std::borrow::Cow::Owned(format!("Extra{n}")),
        })
    }

    /// The attribute a saved name stands for.
    pub fn from_name(s: &str) -> Option<Attribute> {
        use Attribute::*;
        Some(match s {
            "Dimmer" => Dimmer,
            "Red" => Red,
            "Green" => Green,
            "Blue" => Blue,
            "White" => White,
            "Amber" => Amber,
            "Lime" => Lime,
            "Uv" => Uv,
            "Cyan" => Cyan,
            "Magenta" => Magenta,
            "Yellow" => Yellow,
            "Cto" => Cto,
            "ColorWheel" => ColorWheel,
            "ColorMacro" => ColorMacro,
            "Pan" => Pan,
            "Tilt" => Tilt,
            "PanTiltSpeed" => PanTiltSpeed,
            "Zoom" => Zoom,
            "Focus" => Focus,
            "Iris" => Iris,
            "Frost" => Frost,
            "Gobo" => Gobo,
            "GoboRotation" => GoboRotation,
            "Gobo2" => Gobo2,
            "Gobo2Rotation" => Gobo2Rotation,
            "Prism" => Prism,
            "PrismRotation" => PrismRotation,
            "Shutter" => Shutter,
            "Control" => Control,
            "Effect" => Effect,
            "EffectSpeed" => EffectSpeed,
            "Fog" => Fog,
            "Fan" => Fan,
            "ColorWheel2" => ColorWheel2,
            "Frost2" => Frost2,
            "Prism2" => Prism2,
            "Control2" => Control2,
            "Control3" => Control3,
            "Effect2" => Effect2,
            "Effect2Speed" => Effect2Speed,
            _ => Extra(s.strip_prefix("Extra")?.parse().ok().filter(|n| *n > 0)?),
        })
    }
}

impl Serialize for Attribute {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.name())
    }
}

impl<'de> Deserialize<'de> for Attribute {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<str>::deserialize(d)?;
        Attribute::from_name(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown attribute {s:?}")))
    }
}

/// Attribute families, as on a desk's encoder bar.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
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
    pub const ALL: [Attribute; 40] = [
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
        Attribute::ColorWheel2,
        Attribute::ColorMacro,
        Attribute::Pan,
        Attribute::Tilt,
        Attribute::PanTiltSpeed,
        Attribute::Zoom,
        Attribute::Focus,
        Attribute::Iris,
        Attribute::Frost,
        Attribute::Frost2,
        Attribute::Gobo,
        Attribute::GoboRotation,
        Attribute::Gobo2,
        Attribute::Gobo2Rotation,
        Attribute::Prism,
        Attribute::PrismRotation,
        Attribute::Prism2,
        Attribute::Shutter,
        Attribute::Control,
        Attribute::Control2,
        Attribute::Control3,
        Attribute::Effect,
        Attribute::EffectSpeed,
        Attribute::Effect2,
        Attribute::Effect2Speed,
        Attribute::Fog,
        Attribute::Fan,
    ];

    /// The family this attribute belongs to.
    pub fn feature(self) -> Feature {
        use Attribute::*;
        match self {
            Dimmer => Feature::Intensity,
            Red | Green | Blue | White | Amber | Lime | Uv | Cyan | Magenta | Yellow | Cto | ColorWheel | ColorWheel2 | ColorMacro => Feature::Colour,
            Pan | Tilt | PanTiltSpeed => Feature::Position,
            Zoom | Focus | Iris | Frost | Frost2 | Gobo | GoboRotation | Gobo2 | Gobo2Rotation | Prism | PrismRotation | Prism2 | Shutter => Feature::Beam,
            Control | Control2 | Control3 | Effect | EffectSpeed | Effect2 | Effect2Speed | Fog | Fan | Extra(_) => Feature::Control,
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
            Attribute::ColorMacro => t("Colour macro"),
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
            Attribute::Gobo2 => t("Gobo 2"),
            Attribute::Gobo2Rotation => t("Gobo 2 rotation"),
            Attribute::Prism => t("Prism"),
            Attribute::PrismRotation => t("Prism rotation"),
            Attribute::Shutter => t("Shutter"),
            Attribute::Control => t("Control"),
            Attribute::Effect => t("Effect"),
            Attribute::EffectSpeed => t("Effect speed"),
            Attribute::Fog => t("Fog"),
            Attribute::Fan => t("Fan"),
            Attribute::ColorWheel2 => t("Colour wheel 2"),
            Attribute::Frost2 => t("Frost 2"),
            Attribute::Prism2 => t("Prism 2"),
            Attribute::Control2 => t("Control 2"),
            Attribute::Control3 => t("Control 3"),
            Attribute::Effect2 => t("Effect 2"),
            Attribute::Effect2Speed => t("Effect 2 speed"),
            Attribute::Extra(n) => extra_label(n),
        }
    }
}

/// "Extra 3" in the current language, kept for the rest of the run (at most 255 per language).
fn extra_label(n: u8) -> &'static str {
    static LABELS: parking_lot::Mutex<BTreeMap<(String, u8), &'static str>> = parking_lot::Mutex::new(BTreeMap::new());
    let text = crate::tr!("Extra {n}", n = n.to_string());
    LABELS.lock().entry((crate::i18n::t("Extra {n}").to_string(), n)).or_insert_with(|| Box::leak(text.into_boxed_str()))
}

/// One DMX channel (or a coarse/fine pair) of a fixture mode.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ChannelDef {
    /// What it controls.
    pub attribute: Attribute,
    /// The fixture file's own name for the channel (shown for [`Attribute::Extra`] channels).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// DMX break the channel is on (0 = the fixture's address; 1 = its second address...):
    /// fixtures that take several DMX inputs, like a moving light with a separate pixel input.
    #[serde(skip_serializing_if = "is_zero_u8")]
    pub dmx_break: u8,
    /// Offset of the coarse byte from the fixture's start address (0-based).
    pub offset: u16,
    /// Offset of the fine byte (16-bit attributes).
    pub fine: Option<u16>,
    /// Value with nothing controlling it (`None` = [`Attribute::home`]).
    pub default: Option<f32>,
    /// Value while the fixture is highlighted (`None` = full for intensity/open for beam).
    pub highlight: Option<f32>,
    /// Named DMX ranges (wheel slots, "open", "strobe slow → fast"), in DMX order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<ChannelRange>,
}

impl Default for ChannelDef {
    fn default() -> Self {
        Self { attribute: Attribute::Dimmer, name: String::new(), dmx_break: 0, offset: 0, fine: None, default: None, highlight: None, ranges: Vec::new() }
    }
}

/// A named part of a channel's DMX range: a wheel slot, a shutter state, a macro.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct ChannelRange {
    /// First DMX value (coarse byte), inclusive.
    pub from: u8,
    /// Last DMX value (coarse byte), inclusive.
    pub to: u8,
    /// Name shown in the programmer and accepted by the command line.
    pub name: String,
    /// Colour of a colour wheel slot or macro (sRGB).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 3]>,
    /// Image file name of a gobo slot (inside a GDTF/library folder; informational).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub image: String,
}

impl ChannelRange {
    /// The value that picks this range: its middle (a wheel slot centred, the middle speed).
    pub fn value(&self) -> f32 {
        let (a, b) = (self.from.min(self.to) as f32, self.from.max(self.to) as f32);
        (a + b) / 2.0 / 255.0
    }

    /// True if the coarse DMX byte `dmx` falls in this range.
    pub fn contains(&self, dmx: u8) -> bool {
        (self.from.min(self.to)..=self.from.max(self.to)).contains(&dmx)
    }

    /// A span whose place within it matters (a speed, an index angle, "strobe slow → fast"), not a
    /// slot or state that merely takes several DMX values (a gobo, "Open"). Judged on the name
    /// ("→", speed and rotation words), or a wide range with no picture or colour that isn't a state.
    pub fn is_span(&self) -> bool {
        const WORDS: [&str; 16] =
            ["→", "->", "slow", "fast", "speed", "rotat", "spin", "index", "angle", "strobe", "pulse", "shake", "ramp", "cw", "%", "dim"];
        let width = self.from.abs_diff(self.to);
        if width <= 3 {
            return false;
        }
        let name = self.name.to_lowercase();
        if WORDS.iter().any(|w| name.contains(w)) {
            return true;
        }
        let state = name.contains("no function") || name.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with("open") || w.starts_with("clos") || w == "off" || w == "on");
        width >= 63 && self.image.is_empty() && self.color.is_none() && !state
    }
}

/// DMX byte of a normalised value on an 8-bit channel (as [`ChannelDef::write`] outputs it).
pub fn coarse(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

impl ChannelDef {
    /// The named range a value falls in (judged on the coarse byte [`ChannelDef::write`] outputs).
    pub fn range_at(&self, value: f32) -> Option<&ChannelRange> {
        // A 16-bit channel's coarse byte is the high byte of the 16-bit value, not a rounding.
        let dmx = if self.fine.is_some() { ((value.clamp(0.0, 1.0) * 65535.0).round() as u16 >> 8) as u8 } else { coarse(value) };
        self.ranges.iter().find(|r| r.contains(dmx))
    }

    /// A range by name (case-insensitive; a prefix is enough) or by 1-based index.
    pub fn find_range(&self, name: &str) -> Option<&ChannelRange> {
        let key = name.trim().to_lowercase();
        if let Some(r) = self.ranges.iter().find(|r| r.name.to_lowercase() == key) {
            return Some(r);
        }
        if let Ok(i) = key.parse::<usize>() {
            return i.checked_sub(1).and_then(|i| self.ranges.get(i));
        }
        // A prefix: the shortest name it starts ("deep" → "Deep Red", not "Deep Red / Blue").
        self.ranges.iter().filter(|r| !key.is_empty() && r.name.to_lowercase().starts_with(&key)).min_by_key(|r| r.name.len())
    }

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

    /// The normalised value a universe holds for this channel, read from the fixture's address
    /// (0-based): the inverse of [`ChannelDef::write`]. None past the end of the universe.
    pub fn read(&self, start: usize, universe: &[u8]) -> Option<f32> {
        let hi = *universe.get(start + self.offset as usize)?;
        Some(match self.fine.and_then(|f| universe.get(start + f as usize)) {
            Some(lo) => u16::from_be_bytes([hi, *lo]) as f32 / 65535.0,
            None => hi as f32 / 255.0,
        })
    }
}

/// A DMX personality of a fixture type.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct FixtureMode {
    /// Mode name ("8 channel").
    pub name: String,
    /// Channels in this mode (for a multi-cell fixture, the master channels).
    pub channels: Vec<ChannelDef>,
    /// Cells of a multi-cell fixture (LED bar pixels, heads), each controlled as its own unit
    /// numbered `fixture.cell` (`101.1`, `101.2`…).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub heads: Vec<Head>,
    /// DMX slots the mode takes, counting channels OpenQ doesn't use (0 = up to the last
    /// channel). Imports set it so an unused last channel still gets its address.
    #[serde(skip_serializing_if = "is_zero")]
    pub slots: u16,
}

fn is_zero(v: &u16) -> bool {
    *v == 0
}

/// One cell of a multi-cell fixture mode.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Head {
    /// Name ("1", "Top", "Ring 2").
    pub name: String,
    /// Its channels; offsets count from the fixture's start address like the master channels.
    pub channels: Vec<ChannelDef>,
}

/// Attributes of a channel list: its channels plus a virtual dimmer when it has colour
/// emitters but no dimmer channel.
pub fn attributes_of(channels: &[ChannelDef]) -> Vec<Attribute> {
    let mut v: Vec<Attribute> = channels.iter().map(|c| c.attribute).collect();
    if !v.contains(&Attribute::Dimmer) && v.iter().any(|a| a.is_emitter()) {
        v.insert(0, Attribute::Dimmer);
    }
    v.sort();
    v.dedup();
    v
}

/// First fixture id used for cells of multi-cell fixtures; a cell's id is derived from its
/// fixture's number and its index, so cells need no numbers of their own.
pub const CELL_BASE: FixtureId = 1 << 31;
/// Most cells a fixture can have.
pub const MAX_CELLS: u32 = 1024;
/// Highest fixture number (cell ids must fit above [`CELL_BASE`]).
pub const MAX_FIXTURE_ID: FixtureId = (CELL_BASE / MAX_CELLS) - 1;

/// Id of cell `cell` (1-based) of fixture `fixture`.
pub fn cell_id(fixture: FixtureId, cell: u32) -> FixtureId {
    CELL_BASE + fixture.min(MAX_FIXTURE_ID) * MAX_CELLS + (cell.clamp(1, MAX_CELLS) - 1)
}

/// (fixture number, cell) of an id; cell is None for a fixture itself.
pub fn split_id(id: FixtureId) -> (FixtureId, Option<u32>) {
    if id >= CELL_BASE {
        let k = id - CELL_BASE;
        (k / MAX_CELLS, Some(k % MAX_CELLS + 1))
    } else {
        (id, None)
    }
}

/// How a fixture or cell is shown and typed: `101`, `101.3`.
pub fn fixture_label(id: FixtureId) -> String {
    match split_id(id) {
        (f, Some(c)) => format!("{f}.{c}"),
        (f, None) => f.to_string(),
    }
}

/// Parse `101` or `101.3` into a fixture or cell id.
pub fn parse_fixture(s: &str) -> Option<FixtureId> {
    let (a, b) = match s.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let f: FixtureId = a.parse().ok().filter(|f| *f <= MAX_FIXTURE_ID)?;
    match b {
        None => Some(f),
        Some(b) => {
            let c: u32 = b.parse().ok().filter(|c| (1..=MAX_CELLS).contains(c))?;
            Some(cell_id(f, c))
        }
    }
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
        Self { name: name.into(), channels, heads: Vec::new(), slots: 0 }
    }

    /// DMX slots used at the fixture's address (the first break).
    pub fn footprint(&self) -> u16 {
        self.footprint_of(0)
    }

    /// DMX slots used on break `brk` (`slots` counts on the first only).
    pub fn footprint_of(&self, brk: u8) -> u16 {
        let used = self.all_channels().filter(|c| c.dmx_break == brk).map(|c| c.offset.max(c.fine.unwrap_or(0)) + 1).max().unwrap_or(0);
        if brk == 0 { used.max(self.slots.min(UNIVERSE_SIZE)) } else { used }
    }

    /// Number of DMX breaks (addresses) a fixture in this mode takes; 1 for most.
    pub fn breaks(&self) -> u8 {
        self.all_channels().map(|c| c.dmx_break).max().map_or(1, |b| b.saturating_add(1))
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
        attributes_of(&self.channels)
    }

    /// Attributes of the master (cell None) or of a cell: its channels plus a virtual dimmer when
    /// it has colour emitters but no dimmer. A cell gets none when the master has a real dimmer:
    /// that dimmer already sets the cell's intensity.
    pub fn unit_attributes(&self, cell: Option<u32>) -> Vec<Attribute> {
        let Some(chans) = self.unit_channels(cell) else { return Vec::new() };
        let mut v = attributes_of(chans);
        if cell.is_some() && self.has_dimmer() && !chans.iter().any(|c| c.attribute == Attribute::Dimmer) {
            v.retain(|a| *a != Attribute::Dimmer);
        }
        v
    }

    /// Channels of the master (cell None) or of a cell (1-based).
    pub fn unit_channels(&self, cell: Option<u32>) -> Option<&[ChannelDef]> {
        match cell {
            None => Some(&self.channels),
            Some(c) => self.heads.get((c as usize).checked_sub(1)?).map(|h| h.channels.as_slice()),
        }
    }

    /// Every channel: master and cells.
    pub fn all_channels(&self) -> impl Iterator<Item = &ChannelDef> {
        self.channels.iter().chain(self.heads.iter().flat_map(|h| h.channels.iter()))
    }
}

/// Physical properties used for units in the UI and exported to visualisers.
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
    /// Moving head (has a yoke).
    pub moving: bool,
    /// Measured colours of the colour emitters (from GDTF files); empty = typical values
    /// ([`crate::light::colour`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub emitters: Vec<Emitter>,
}

impl Default for Physical {
    fn default() -> Self {
        Self { pan_range: 540.0, tilt_range: 270.0, beam_min: 25.0, beam_max: 25.0, moving: false, emitters: Vec::new() }
    }
}

/// The colour of an emitter (an LED colour) at full: CIE 1931 xy and its luminance (any unit,
/// relative to the type's other emitters).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Emitter {
    /// The attribute driving it (Red, Green, Blue, White, Amber, Lime).
    pub attribute: Attribute,
    /// Chromaticity x.
    pub x: f32,
    /// Chromaticity y.
    pub y: f32,
    /// Luminance at full.
    pub luminance: f32,
}

impl Emitter {
    /// Usable: finite, inside the xy triangle, not black.
    pub fn is_valid(&self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.luminance.is_finite() && self.x >= 0.0 && self.y > 0.0 && self.x + self.y <= 1.0 && self.luminance > 0.0
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
    /// Wheel slot pictures by [`ChannelRange::image`] name ([`super::gobo`] thumbnails).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub images: BTreeMap<String, String>,
}

impl FixtureType {
    /// A mode by name (falls back to the first one).
    pub fn mode(&self, name: &str) -> Option<&FixtureMode> {
        self.modes.iter().find(|m| m.name == name).or(self.modes.first())
    }
}

fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

/// Where one DMX break of a fixture is patched.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct DmxAddress {
    /// Universe (1-based).
    pub universe: u16,
    /// Start address (1..=512), 0 = not patched.
    pub address: u16,
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
    /// Rotation in degrees about x, y and z, applied as heading (y) · tilt (x) · roll (z).
    pub rotation: [f32; 3],
    /// Reverse pan.
    pub invert_pan: bool,
    /// Reverse tilt.
    pub invert_tilt: bool,
    /// Addresses of the second, third... DMX break of a multi-break mode
    /// ([`ChannelDef::dmx_break`]); a missing one isn't patched.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub breaks: Vec<DmxAddress>,
}

impl Fixture {
    /// Where break `brk` is patched (break 0 = `universe`/`address`).
    pub fn address_of(&self, brk: u8) -> DmxAddress {
        match brk {
            0 => DmxAddress { universe: self.universe, address: self.address },
            b => self.breaks.get(b as usize - 1).copied().unwrap_or_default(),
        }
    }
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
            breaks: Vec::new(),
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

/// Copy-on-write holder for the big parts of a design (fixtures, cue lists, cue and preset
/// values). Cloning a design
/// shares them and comparing two designs skips values that are still the same allocation, so
/// the UI can snapshot a large design every frame and keep undo steps without copying every
/// cue. Writing through `DerefMut` copies the value only while it is shared.
#[derive(Default)]
pub struct Shared<T>(std::sync::Arc<T>);

impl<T> Shared<T> {
    /// Both hold the same allocation (a clone nobody has written to since).
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        std::sync::Arc::ptr_eq(&a.0, &b.0)
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> std::ops::Deref for Shared<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Clone> std::ops::DerefMut for Shared<T> {
    fn deref_mut(&mut self) -> &mut T {
        std::sync::Arc::make_mut(&mut self.0)
    }
}

impl<T: PartialEq> PartialEq for Shared<T> {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0) || *self.0 == *other.0
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<'a, T> IntoIterator for &'a Shared<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        (&*self.0).into_iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut Shared<T>
where
    &'a mut T: IntoIterator,
{
    type Item = <&'a mut T as IntoIterator>::Item;
    type IntoIter = <&'a mut T as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        std::sync::Arc::make_mut(&mut self.0).into_iter()
    }
}

impl<T> FromIterator<T> for Shared<Vec<T>> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(std::sync::Arc::new(iter.into_iter().collect()))
    }
}

impl<T> From<T> for Shared<T> {
    fn from(v: T) -> Self {
        Self(std::sync::Arc::new(v))
    }
}

impl<T: Serialize> Serialize for Shared<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Shared<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::deserialize(d).map(Self::from)
    }
}

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
    pub values: Shared<BTreeMap<FixtureId, BTreeMap<Attribute, f32>>>,
}

/// Number of a playback button.
pub type ButtonId = u32;

/// Buttons play as hidden one-cue sequences numbered from here (`BUTTON_SEQUENCES + id`), so they
/// fade, mix (HTP/LTP), mirror to replicas and release like any sequence. User sequences never
/// get numbers this high.
pub const BUTTON_SEQUENCES: SequenceId = 0x4000_0000;

/// A playback button (Playback window, touch screens): a preset put on chosen fixtures, and/or
/// stored values and effects, switched on and off (or held, `flash`) without the programmer, so it
/// works in Show mode. Typical uses: fog or haze on, a strobe hit, a special.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct LightButton {
    /// Button number.
    pub id: ButtonId,
    /// Name on the button.
    pub name: String,
    /// Colour of the button.
    pub color: [u8; 3],
    /// Preset put on `fixtures` (every attribute it has for them).
    pub preset: Option<PresetId>,
    /// Fixtures (and cells) the preset is applied to.
    pub fixtures: Vec<FixtureId>,
    /// Stored values (from the programmer); win over the preset.
    pub values: Shared<Look>,
    /// Effects the button runs.
    pub effects: Vec<super::effects::EffectRun>,
    /// Fade in and out, seconds.
    pub fade: f64,
    /// On only while held down (otherwise each press toggles it).
    pub flash: bool,
    /// Page of the Playback page it is on (index into [`Lighting::button_pages`]).
    #[serde(skip_serializing_if = "is_zero")]
    pub page: u16,
}

impl Default for LightButton {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            color: [0x4a, 0x7c, 0xf0],
            preset: None,
            fixtures: Vec::new(),
            values: Default::default(),
            effects: Vec::new(),
            fade: 0.0,
            flash: false,
            page: 0,
        }
    }
}

impl LightButton {
    /// The sequence number the button plays as.
    pub fn sequence_id(&self) -> SequenceId {
        BUTTON_SEQUENCES + self.id
    }

    /// Id of the one cue of its sequence (derived from the button number, so a recompile, e.g.
    /// an edit while the button is on, keeps it on).
    pub fn cue_id(&self) -> LightCueId {
        Uuid::from_u64_pair(0x6f70_656e_7162_746e, self.id as u64)
    }

    /// What the button puts on stage: the preset on its fixtures, then its own values.
    pub fn look(&self, l: &Lighting) -> Look {
        let mut look = Look::new();
        if let Some(p) = self.preset {
            for f in &self.fixtures {
                for a in l.preset_attributes(p, *f) {
                    look.entry(*f).or_default().insert(a, Value::Preset { preset: p });
                }
            }
        }
        for (f, v) in self.values.iter() {
            look.entry(*f).or_default().extend(v.iter().map(|(a, x)| (*a, *x)));
        }
        look
    }

    /// The one-cue sequence it plays as.
    pub fn sequence(&self, l: &Lighting) -> Sequence {
        let cue = LightCue {
            id: self.cue_id(),
            number: "1".into(),
            name: self.name.clone(),
            fade: self.fade,
            values: self.look(l).into(),
            effects: self.effects.clone(),
            ..Default::default()
        };
        Sequence { id: self.sequence_id(), name: self.name.clone(), cues: vec![cue].into(), ..Default::default() }
    }
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
    pub values: Shared<Look>,
    /// Own fade/delay for some attribute families (the others use the cue's).
    pub feature_times: BTreeMap<Feature, FeatureTime>,
    /// The values this cue sets don't track on: the next cue goes back to what was there before.
    pub cue_only: bool,
    /// A block: every value tracked into it was stored in it ([`Sequence::block`]), so edits to
    /// earlier cues don't change it. Shown on the cue; [`Sequence::unblock`] undoes it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub block: bool,
    /// Effects this cue starts or stops (running effects track on).
    pub effects: Vec<super::effects::EffectRun>,
}

/// Fade and delay for one attribute family of a cue.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(default)]
pub struct FeatureTime {
    /// Fade, seconds.
    pub fade: f64,
    /// Delay, seconds.
    pub delay: f64,
}

impl LightCue {
    /// Seconds from GO until every fade of this cue is complete.
    pub fn duration(&self) -> f64 {
        let base = self.delay.max(0.0) + self.fade.max(self.down_fade.unwrap_or(self.fade)).max(0.0);
        self.feature_times.values().map(|t| t.delay.max(0.0) + t.fade.max(0.0)).fold(base, f64::max)
    }

    /// (delay, fade up, fade down) for an attribute.
    pub fn timing(&self, attr: Attribute) -> (f64, f64, f64) {
        self.feature_timing(attr.feature())
    }

    /// (delay, fade up, fade down) for a feature's attributes.
    pub fn feature_timing(&self, feature: Feature) -> (f64, f64, f64) {
        match self.feature_times.get(&feature) {
            Some(t) => (t.delay.max(0.0), t.fade.max(0.0), t.fade.max(0.0)),
            None => (self.delay.max(0.0), self.fade.max(0.0), self.down_fade.unwrap_or(self.fade).max(0.0)),
        }
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
            values: Default::default(),
            feature_times: BTreeMap::new(),
            cue_only: false,
            block: false,
            effects: Vec::new(),
        }
    }
}

/// A list of lighting cues played in order (a cue stack), with tracking.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Sequence {
    /// Sequence number.
    pub id: SequenceId,
    /// Name.
    pub name: String,
    /// Cues in order.
    pub cues: Shared<Vec<LightCue>>,
    /// Move in black: fixtures that are dark move to their next cue's position, colour and beam
    /// while still dark, so they don't move live.
    pub move_in_black: bool,
    /// Move in black: wait this long after the cue's fades before moving.
    pub mib_delay: f64,
    /// Move in black: move time.
    pub mib_fade: f64,
}

impl Default for Sequence {
    fn default() -> Self {
        Self { id: 1, name: String::new(), cues: Default::default(), move_in_black: false, mib_delay: 0.5, mib_fade: 2.0 }
    }
}

impl Sequence {
    /// Store every tracked value in cue `index` itself (a block): later edits to earlier cues
    /// no longer change it.
    pub fn block(&mut self, index: usize) {
        if index < self.cues.len() {
            let t = self.tracked(index);
            let cue = &mut self.cues[index];
            cue.values = t.into();
            cue.block = true;
        }
    }

    /// Undo a block: values of cue `index` that are the same as what tracks into it are taken
    /// out (they track again).
    pub fn unblock(&mut self, index: usize) {
        if index >= self.cues.len() {
            return;
        }
        let before = self.state_before(index);
        let cue = &mut self.cues[index];
        cue.block = false;
        let mut values = (*cue.values).clone();
        for (f, vals) in values.iter_mut() {
            if let Some(prev) = before.get(f) {
                vals.retain(|a, v| prev.get(a) != Some(v));
            }
        }
        values.retain(|_, v| !v.is_empty());
        if values != *cue.values {
            cue.values = values.into();
        }
    }

    /// What tracks into cue `index` (the state before it runs).
    fn state_before(&self, index: usize) -> Look {
        let mut look = Look::new();
        for cue in self.cues.iter().take(index) {
            let before = cue.cue_only.then(|| look.clone());
            for (fixture, values) in &cue.values {
                look.entry(*fixture).or_default().extend(values.iter().map(|(a, v)| (*a, *v)));
            }
            if let Some(b) = before {
                look = b;
            }
        }
        look
    }

    /// Full state at cue `index`: every value set by it or tracked from an earlier cue
    /// (values of earlier cue-only cues don't track).
    pub fn tracked(&self, index: usize) -> Look {
        let mut look = Look::new();
        for (i, cue) in self.cues.iter().take(index + 1).enumerate() {
            let before = (cue.cue_only && i < index).then(|| look.clone());
            for (fixture, values) in &cue.values {
                let state = look.entry(*fixture).or_default();
                state.extend(values.iter().map(|(a, v)| (*a, *v)));
            }
            if let Some(b) = before {
                look = b;
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

/// Move the entries whose key `map` changes.
fn rekey<V>(m: &mut BTreeMap<FixtureId, V>, map: impl Fn(FixtureId) -> Option<FixtureId>) {
    let moved: Vec<FixtureId> = m.keys().copied().filter(|k| map(*k).is_some()).collect();
    for k in moved {
        let v = m.remove(&k).expect("key");
        m.insert(map(k).expect("mapped"), v);
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

/// What a [`PlotShape`] is.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ShapeKind {
    /// A lighting bar: a pipe, a truss or a boom, from `a` to `b`.
    #[default]
    Bar,
    /// A rectangle with corners `a` and `b`: the stage, a riser, a set piece.
    Rect,
}

/// A drawn element of the light plot (not a fixture): bars to hang fixtures on, rectangles for
/// the stage and the set. Only for placement and plots; playback ignores them.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct PlotShape {
    /// Shape number.
    pub id: u32,
    /// Bar or rectangle.
    pub kind: ShapeKind,
    /// Label ("LX1", "Stage").
    pub name: String,
    /// A bar's first end, a rectangle's corner: (x, z) in metres, as fixture positions.
    pub a: [f32; 2],
    /// A bar's other end, a rectangle's opposite corner.
    pub b: [f32; 2],
    /// Height (y) in metres: a bar's trim, the top of a riser.
    pub height: f32,
    /// Where the user moved the label to, metres (x, z) from its usual place.
    #[serde(skip_serializing_if = "is_origin")]
    pub label: [f32; 2],
}

fn is_origin(v: &[f32; 2]) -> bool {
    *v == [0.0, 0.0]
}

impl PlotShape {
    /// Positions spreading `n` fixtures evenly along a bar at its height, half a gap in from each
    /// end (one fixture hangs in the middle).
    pub fn hang(&self, n: usize) -> Vec<[f32; 3]> {
        let at = |k: f32| [self.a[0] + (self.b[0] - self.a[0]) * k, self.height, self.a[1] + (self.b[1] - self.a[1]) * k];
        (0..n).map(|i| at((i as f32 + 0.5) / n as f32)).collect()
    }

    /// Length of a bar (diagonal of a rectangle), metres.
    pub fn length(&self) -> f32 {
        (self.b[0] - self.a[0]).hypot(self.b[1] - self.a[1])
    }
}

/// Something on the light plot that the plot editor can move, re-word or hide.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug, Default)]
#[serde(tag = "item", content = "id", rename_all = "snake_case")]
pub enum PlotItem {
    /// The title (the show's name).
    #[default]
    Title,
    /// The line at the top right (scale, fixture count).
    Subtitle,
    /// "AUDIENCE" at the bottom of the grid.
    Audience,
    /// The legend as a block (moves only).
    Legend,
    /// One legend row, by fixture type name (text, hiding and the type's symbol; it moves with the legend).
    LegendRow(String),
    /// A fixture's symbol, with its number as text; its labels move with it.
    Fixture(FixtureId),
    /// The patch label above a fixture.
    Patch(FixtureId),
    /// The distance label below a fixture.
    Distance(FixtureId),
    /// A bar's or rectangle's name (moved through [`PlotShape::label`], like in the Plan view).
    Shape(u32),
    /// An added text ([`PlotNote`]: it keeps its own place and text, so it has no [`PlotEdit`]).
    Note(u32),
}

impl PlotItem {
    /// Moved in page units (points on the PDF, pixels in the SVG) rather than metres.
    pub fn on_page(&self) -> bool {
        matches!(self, PlotItem::Title | PlotItem::Subtitle | PlotItem::Legend | PlotItem::LegendRow(_))
    }
}

/// A fixture's symbol on the light plot (drawn facing its heading, the lens towards the front).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PlotSymbol {
    /// A circle with a heading mark: moving heads.
    Circle,
    /// A rectangle with a lens line: the generic lantern.
    Rectangle,
    /// A long body and a tapered lens barrel: profile spots (ellipsoidals).
    Profile,
    /// A short body with a lens lip at the front: fresnels and PCs.
    Fresnel,
    /// A round-ended can: PARs.
    Par,
    /// A box widening to the front: floods and cyc lights.
    Flood,
    /// A wide, flat box with a flash line: strobes and blinders.
    Strobe,
    /// A long thin bar across the heading: LED battens and pixel bars.
    Batten,
    /// A triangle: smoke, haze and other effects.
    Effect,
}

impl PlotSymbol {
    /// Every symbol, in the order the plot editor offers them.
    pub const ALL: [PlotSymbol; 9] = [
        PlotSymbol::Circle,
        PlotSymbol::Rectangle,
        PlotSymbol::Profile,
        PlotSymbol::Fresnel,
        PlotSymbol::Par,
        PlotSymbol::Flood,
        PlotSymbol::Strobe,
        PlotSymbol::Batten,
        PlotSymbol::Effect,
    ];

    /// The symbol a fixture type gets unless the user picks one: a circle for moving heads, else
    /// a guess from its name (profile, fresnel, PAR, flood/cyc, strobe/blinder, batten/bar, fog/haze),
    /// else the rectangle.
    pub fn guess(name: &str, moving: bool) -> Self {
        if moving {
            return PlotSymbol::Circle;
        }
        let n = name.to_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
        if has(&["profile", "ellipsoidal", "leko", "source four", "zoom spot"]) {
            PlotSymbol::Profile
        } else if has(&["fresnel", "pebble", " pc", "plano"]) {
            PlotSymbol::Fresnel
        } else if has(&["par"]) && !has(&["parameter"]) {
            PlotSymbol::Par
        } else if has(&["flood", "cyc", "wash light", "asymmetric"]) {
            PlotSymbol::Flood
        } else if has(&["strobe", "blinder", "molefay", "audience light"]) {
            PlotSymbol::Strobe
        } else if has(&["batten", "bar", "strip", "pixel line", "tube"]) {
            PlotSymbol::Batten
        } else if has(&["fog", "haze", "smoke", "hazer", "fogger", "effect"]) {
            PlotSymbol::Effect
        } else {
            PlotSymbol::Rectangle
        }
    }
}

/// The plot editor's change to one [`PlotItem`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct PlotEdit {
    /// Which item.
    pub item: PlotItem,
    /// Moved from its usual place: metres (x, z) on the grid, page units for
    /// [`PlotItem::on_page`] items. A moved label is no longer moved out of the others' way.
    pub offset: [f32; 2],
    /// Text drawn instead of the usual one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Left off the plot.
    pub hidden: bool,
    /// Text size instead of the usual one: points at the SVG scale for items on the grid (smaller
    /// on a small-scale PDF, like the labels), page units for [`PlotItem::on_page`] items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    /// Text colour (sRGB) instead of the usual grey.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colour: Option<[u8; 3]>,
    /// Symbol of a [`PlotItem::Fixture`], or of every fixture of a type for its
    /// [`PlotItem::LegendRow`] (a fixture's own choice wins); None = [`PlotSymbol::guess`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<PlotSymbol>,
}

impl PlotEdit {
    /// Changes nothing.
    pub fn is_noop(&self) -> bool {
        self.offset == [0.0, 0.0] && self.text.is_none() && !self.hidden && self.size.is_none() && self.colour.is_none() && self.symbol.is_none()
    }
}

/// A free text added to the plot.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct PlotNote {
    /// Note number.
    pub id: u32,
    /// Baseline start of the first line, metres (x, z) as fixture positions.
    pub at: [f32; 2],
    /// Text (several lines allowed).
    pub text: String,
    /// Text size at the SVG scale (smaller on a small-scale PDF, like the labels).
    pub size: f32,
    /// Bold.
    pub bold: bool,
    /// Text colour (sRGB).
    pub colour: [u8; 3],
}

impl Default for PlotNote {
    fn default() -> Self {
        Self { id: 0, at: [0.0, 0.0], text: String::new(), size: 14.0, bold: false, colour: [0, 0, 0] }
    }
}

/// How the plot editor changed the light plot (`light::plot` draws it with these).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct PlotLayout {
    /// One per changed item.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<PlotEdit>,
    /// Added texts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<PlotNote>,
}

impl PlotLayout {
    /// Nothing changed.
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty() && self.notes.is_empty()
    }

    /// The change to `item`, if any.
    pub fn edit(&self, item: &PlotItem) -> Option<&PlotEdit> {
        self.edits.iter().find(|e| &e.item == item)
    }

    /// The change to `item`, added when missing (call [`PlotLayout::tidy`] afterwards).
    pub fn edit_mut(&mut self, item: &PlotItem) -> &mut PlotEdit {
        match self.edits.iter().position(|e| &e.item == item) {
            Some(i) => &mut self.edits[i],
            None => {
                self.edits.push(PlotEdit { item: item.clone(), ..Default::default() });
                self.edits.last_mut().expect("just pushed")
            }
        }
    }

    /// Drop changes that change nothing.
    pub fn tidy(&mut self) {
        self.edits.retain(|e| !e.is_noop());
    }

    /// A note by number.
    pub fn note_mut(&mut self, id: u32) -> Option<&mut PlotNote> {
        self.notes.iter_mut().find(|n| n.id == id)
    }
}

/// The lighting design of a show.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Lighting {
    /// Fixture types used by the patch (copied from the library when patched).
    pub fixture_types: Vec<FixtureType>,
    /// Patched fixtures.
    pub fixtures: Shared<Vec<Fixture>>,
    /// Groups.
    pub groups: Vec<Group>,
    /// Cue stacks.
    pub sequences: Vec<Sequence>,
    /// DMX outputs.
    pub outputs: Vec<super::dmx::OutputConfig>,
    /// DMX inputs (Art-Net/sACN from another desk: merged into the output or captured).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<super::dmxin::InputConfig>,
    /// Bars and rectangles drawn on the plot.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shapes: Vec<PlotShape>,
    /// Plot editor changes (moved, re-worded or hidden items, added texts).
    #[serde(skip_serializing_if = "PlotLayout::is_empty")]
    pub plot: PlotLayout,
    /// Presets (palettes).
    pub presets: Vec<Preset>,
    /// Effects pool.
    pub effects: Vec<super::effects::Effect>,
    /// Playback buttons (Playback window).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub buttons: Vec<LightButton>,
    /// Names of the playback buttons' pages (empty: one page). Buttons on other pages keep
    /// playing; a page only chooses which buttons are shown (and which a control surface reaches).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub button_pages: Vec<String>,
    /// Size of the playback buttons' cards on the Playback page and window (1 = normal).
    pub button_scale: f32,
    /// OpenQ's Stop all (and Stop cues without a target) also fade the lighting out.
    pub stop_all_releases: bool,
    /// Fade used when a Stop cue releases a sequence, seconds.
    pub release_fade: f64,
    /// Fixture number → position in `fixtures` (not saved).
    #[serde(skip)]
    index: FixtureIndex,
}

/// Private lookup cache for [`Lighting::fixture`]: the UI looks fixtures up per selected unit
/// many times a frame. Every hit is checked against `fixtures` (which is edited through public
/// fields), and the map is rebuilt when it is stale, so it can never return a wrong fixture.
#[derive(Default)]
struct FixtureIndex(parking_lot::Mutex<std::collections::HashMap<FixtureId, usize>>);

impl Clone for FixtureIndex {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for FixtureIndex {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for FixtureIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FixtureIndex")
    }
}

impl Default for Lighting {
    fn default() -> Self {
        Self {
            fixture_types: Vec::new(),
            fixtures: Default::default(),
            groups: Vec::new(),
            sequences: Vec::new(),
            outputs: Vec::new(),
            inputs: Vec::new(),
            shapes: Vec::new(),
            plot: PlotLayout::default(),
            presets: Vec::new(),
            effects: Vec::new(),
            buttons: Vec::new(),
            button_pages: Vec::new(),
            button_scale: 1.0,
            stop_all_releases: false,
            release_fade: 1.0,
            index: FixtureIndex::default(),
        }
    }
}

impl Lighting {
    /// Nothing designed yet (not written to the show file).
    pub fn is_empty(&self) -> bool {
        self.fixtures.is_empty() && self.fixture_types.is_empty() && self.groups.is_empty() && self.sequences.is_empty()
            && self.outputs.is_empty()
            && self.inputs.is_empty()
            && self.shapes.is_empty()
            && self.plot.is_empty()
            && self.presets.is_empty()
            && self.effects.is_empty()
            && self.buttons.is_empty()
    }

    /// A fixture by number.
    pub fn fixture(&self, id: FixtureId) -> Option<&Fixture> {
        let mut index = self.index.0.lock();
        if let Some(&i) = index.get(&id)
            && self.fixtures.get(i).is_some_and(|f| f.id == id)
        {
            return self.fixtures.get(i);
        }
        // Missing or stale: find it the slow way; rebuild the map if it exists (an unknown
        // number then costs one scan, as without the map).
        let i = self.fixtures.iter().position(|f| f.id == id)?;
        index.clear();
        for (k, f) in self.fixtures.iter().enumerate().rev() {
            index.insert(f.id, k); // the first fixture wins over a duplicate number, as before
        }
        drop(index);
        self.fixtures.get(i)
    }

    /// A fixture type by id.
    pub fn fixture_type(&self, id: &str) -> Option<&FixtureType> {
        self.fixture_types.iter().find(|t| t.id == id)
    }

    /// The mode a fixture is patched in.
    pub fn mode_of(&self, f: &Fixture) -> Option<&FixtureMode> {
        self.fixture_type(&f.fixture_type)?.mode(&f.mode)
    }

    /// A fixture or cell: its fixture and cell number (None for the fixture itself). None if
    /// the fixture doesn't exist or its mode has no such cell.
    pub fn unit(&self, id: FixtureId) -> Option<(&Fixture, Option<u32>)> {
        let (num, cell) = split_id(id);
        let f = self.fixture(num)?;
        if let Some(c) = cell {
            self.mode_of(f)?.unit_channels(Some(c))?;
        }
        Some((f, cell))
    }

    /// Channels of a fixture (its master channels) or a cell.
    pub fn unit_channels(&self, id: FixtureId) -> Option<&[ChannelDef]> {
        let (f, cell) = self.unit(id)?;
        self.mode_of(f)?.unit_channels(cell)
    }

    /// Attributes of a fixture or cell (plus a virtual dimmer, see [`FixtureMode::unit_attributes`]).
    pub fn unit_attributes(&self, id: FixtureId) -> Vec<Attribute> {
        self.unit(id).and_then(|(f, cell)| Some(self.mode_of(f)?.unit_attributes(cell))).unwrap_or_default()
    }

    /// What to call an attribute of a unit: an [`Attribute::Extra`] channel goes by the fixture
    /// file's name for it ("Gobo 3", "Lamp control"), everything else by [`Attribute::label`].
    pub fn attribute_label(&self, id: FixtureId, attr: Attribute) -> String {
        let named = matches!(attr, Attribute::Extra(_))
            .then(|| self.unit_channels(id)?.iter().find(|c| c.attribute == attr).filter(|c| !c.name.is_empty()).map(|c| c.name.clone()))
            .flatten();
        named.unwrap_or_else(|| attr.label().to_string())
    }

    /// Number of cells of a fixture (0 for an ordinary one).
    pub fn cell_count(&self, f: &Fixture) -> u32 {
        self.mode_of(f).map_or(0, |m| m.heads.len().min(MAX_CELLS as usize) as u32)
    }

    /// Every fixture and cell id in patch order (a fixture, then its cells).
    pub fn units(&self) -> Vec<FixtureId> {
        let mut v = Vec::with_capacity(self.fixtures.len());
        for f in &self.fixtures {
            v.push(f.id);
            v.extend((1..=self.cell_count(f)).map(|c| cell_id(f.id, c)));
        }
        v
    }

    /// (fixture type, cell) of a unit: presets share values between units of the same kind.
    fn unit_kind(&self, id: FixtureId) -> Option<(&str, Option<u32>)> {
        let (num, cell) = split_id(id);
        Some((self.fixture(num)?.fixture_type.as_str(), cell))
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

    /// An effect by number.
    pub fn effect(&self, id: super::effects::EffectId) -> Option<&super::effects::Effect> {
        self.effects.iter().find(|e| e.id == id)
    }

    /// Every unit with an intensity (its own dimmer or the virtual one) at zero: the values of a
    /// blackout cue.
    pub fn blackout_look(&self) -> Look {
        self.units()
            .into_iter()
            .filter(|u| self.unit_attributes(*u).contains(&Attribute::Dimmer))
            .map(|u| (u, [(Attribute::Dimmer, Value::Level(0.0))].into_iter().collect()))
            .collect()
    }

    /// A playback button by number.
    pub fn button(&self, id: ButtonId) -> Option<&LightButton> {
        self.buttons.iter().find(|b| b.id == id)
    }

    /// Number of playback button pages (at least 1).
    pub fn button_page_count(&self) -> u16 {
        self.button_pages.len().clamp(1, u16::MAX as usize) as u16
    }

    /// The playback buttons on page `page`, in order. With one page, every button (whatever
    /// its stored page: a page deleted while buttons were on it).
    pub fn page_buttons(&self, page: u16) -> impl Iterator<Item = &LightButton> {
        let all = self.button_pages.len() <= 1;
        self.buttons.iter().filter(move |b| all || b.page.min(self.button_page_count() - 1) == page)
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
                let kind = self.unit_kind(fixture)?;
                p.values
                    .iter()
                    .find(|(f, _)| self.unit_kind(**f) == Some(kind))
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
        // Buttons keep what the preset put on their fixtures as their own values.
        let looks: Vec<Option<Look>> = self
            .buttons
            .iter()
            .map(|b| {
                let refers = b.preset == Some(id) || b.values.values().any(|v| v.values().any(|x| *x == Value::Preset { preset: id }));
                refers.then(|| {
                    let mut look = b.look(self);
                    unlink_preset(self, id, &mut look);
                    look
                })
            })
            .collect();
        for (b, look) in self.buttons.iter_mut().zip(looks) {
            if let Some(look) = look {
                if b.preset == Some(id) {
                    b.preset = None;
                    b.fixtures.clear();
                }
                b.values = look.into();
            }
        }
        self.presets.retain(|p| p.id != id);
    }

    /// Attributes a preset holds for a fixture (its own or its type's).
    pub fn preset_attributes(&self, preset: PresetId, fixture: FixtureId) -> Vec<Attribute> {
        let Some(p) = self.preset(preset) else { return Vec::new() };
        if let Some(v) = p.values.get(&fixture) {
            return v.keys().copied().collect();
        }
        let Some(kind) = self.unit_kind(fixture) else { return Vec::new() };
        p.values
            .iter()
            .find(|(f, _)| self.unit_kind(**f) == Some(kind))
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
        // Above MAX_FIXTURE_ID, cell ids would clamp onto another fixture's.
        if old == new || self.fixture(new).is_some() || new == 0 || new > MAX_FIXTURE_ID {
            return false;
        }
        let Some(f) = self.fixtures.iter_mut().find(|f| f.id == old) else { return false };
        f.id = new;
        // The fixture and its cells.
        let map = |id: FixtureId| match split_id(id) {
            (f, None) if f == old => Some(new),
            (f, Some(c)) if f == old => Some(cell_id(new, c)),
            _ => None,
        };
        for g in &mut self.groups {
            g.fixtures.iter_mut().for_each(|x| *x = map(*x).unwrap_or(*x));
        }
        for c in self.sequences.iter_mut().flat_map(|s| s.cues.iter_mut()) {
            rekey(&mut c.values, map);
            for r in &mut c.effects {
                r.fixtures.iter_mut().for_each(|x| *x = map(*x).unwrap_or(*x));
            }
        }
        for p in &mut self.presets {
            rekey(&mut p.values, map);
        }
        for b in &mut self.buttons {
            b.fixtures.iter_mut().for_each(|x| *x = map(*x).unwrap_or(*x));
            rekey(&mut b.values, map);
            for r in &mut b.effects {
                r.fixtures.iter_mut().for_each(|x| *x = map(*x).unwrap_or(*x));
            }
        }
        true
    }

    /// Unpatch fixtures and drop them from groups and cues.
    pub fn remove_fixtures(&mut self, ids: &std::collections::HashSet<FixtureId>) {
        self.fixtures.retain(|f| !ids.contains(&f.id));
        // Cells go with their fixture.
        let gone = |f: &FixtureId| ids.contains(&split_id(*f).0);
        for g in &mut self.groups {
            g.fixtures.retain(|f| !gone(f));
        }
        for c in self.sequences.iter_mut().flat_map(|s| s.cues.iter_mut()) {
            c.values.retain(|f, _| !gone(f));
            for r in &mut c.effects {
                r.fixtures.retain(|f| !gone(f));
            }
        }
        for p in &mut self.presets {
            p.values.retain(|f, _| !gone(f));
        }
        for b in &mut self.buttons {
            b.fixtures.retain(|f| !gone(f));
            b.values.retain(|f, _| !gone(f));
            for r in &mut b.effects {
                r.fixtures.retain(|f| !gone(f));
            }
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

    /// The DMX spans a fixture's patched breaks take: (universe, first, last slot).
    pub fn spans(&self, f: &Fixture) -> Vec<(u16, u32, u32)> {
        let Some(mode) = self.mode_of(f) else { return Vec::new() };
        (0..mode.breaks())
            .filter_map(|b| {
                let at = f.address_of(b);
                (at.address > 0).then(|| (at.universe, at.address as u32, at.address as u32 + mode.footprint_of(b).max(1) as u32 - 1))
            })
            .collect()
    }

    /// First start address in `universe` where `footprint` slots are free, if any.
    pub fn next_free_address(&self, universe: u16, footprint: u16) -> Option<u16> {
        let mut used = [false; UNIVERSE_SIZE as usize];
        for f in self.fixtures.iter() {
            for (u, first, last) in self.spans(f) {
                if u != universe {
                    continue;
                }
                for s in first..=last {
                    if let Some(u) = used.get_mut(s as usize - 1) {
                        *u = true;
                    }
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
            if f.id > MAX_FIXTURE_ID {
                issues.push(PatchIssue::OutOfRange(f.id));
                continue;
            }
            let _ = mode;
            let all = self.spans(f);
            if all.iter().any(|(u, _, last)| *u == 0 || *u > MAX_UNIVERSE || *last > UNIVERSE_SIZE as u32) {
                issues.push(PatchIssue::OutOfRange(f.id));
                continue;
            }
            spans.extend(all.into_iter().map(|(u, first, last)| (u, first, last, f.id)));
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

    #[test]
    fn spans_are_speeds_not_slots() {
        let r = |from, to, name: &str| ChannelRange { from, to, name: name.into(), ..Default::default() };
        assert!(r(128, 255, "Strobe slow → fast").is_span());
        assert!(r(0, 127, "Index").is_span());
        assert!(r(190, 220, "Rotate CCW slow -> fast").is_span());
        assert!(!r(10, 19, "Window").is_span(), "a gobo slot");
        assert!(!r(32, 95, "Open").is_span(), "a state");
        assert!(!r(250, 255, "Stop").is_span());
        assert!(!r(0, 3, "Strobe").is_span(), "too narrow");
        assert!(r(64, 191, "Macro 1").is_span(), "wide and plain");
        let gobo = ChannelRange { image: "gobo3.png".into(), ..r(64, 191, "Breakup") };
        assert!(!gobo.is_span());
    }

    #[test]
    fn clones_share_fixtures_and_cues() {
        // `Show::sync_lighting` compares the design on every edit: with fixtures and cue lists
        // shared, an untouched design compares by pointer.
        let a = library::bench_rig(20, 2, 5);
        let mut b = a.clone();
        assert!(Shared::ptr_eq(&a.fixtures, &b.fixtures));
        assert!(a.sequences.iter().zip(&b.sequences).all(|(x, y)| Shared::ptr_eq(&x.cues, &y.cues)));
        assert_eq!(a, b);
        b.fixtures[3].name = "Spot".into();
        assert!(!Shared::ptr_eq(&a.fixtures, &b.fixtures));
        assert_ne!(a, b);
        assert_eq!(a.fixtures[3].name, "");
    }

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
        l.sequences.push(Sequence { id: 1, cues: vec![c].into(), ..Default::default() });
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
    fn cell_ids_and_labels() {
        assert_eq!(split_id(cell_id(101, 3)), (101, Some(3)));
        assert_eq!(fixture_label(cell_id(101, 12)), "101.12");
        assert_eq!(fixture_label(7), "7");
        assert_eq!(parse_fixture("101.3"), Some(cell_id(101, 3)));
        assert_eq!(parse_fixture("101"), Some(101));
        assert_eq!(parse_fixture("101.0"), None);
        assert_eq!(parse_fixture("x"), None);
        assert!(cell_id(101, 1) < cell_id(101, 2) && cell_id(101, 1024) < cell_id(102, 1), "ranges stay in order");
        assert_eq!(split_id(cell_id(MAX_FIXTURE_ID, MAX_CELLS)), (MAX_FIXTURE_ID, Some(MAX_CELLS)));
    }

    #[test]
    fn presets_share_values_per_cell() {
        let mut l = Lighting::default();
        let mut t = library::generic("generic/dimmer").unwrap();
        t.modes[0].heads = vec![Head { name: "1".into(), channels: vec![ChannelDef { attribute: Attribute::Red, offset: 1, ..Default::default() }] }];
        l.add_type(&t);
        for id in [1, 2] {
            l.fixtures.push(Fixture { id, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), ..Default::default() });
        }
        let mut p = Preset { id: 1, ..Default::default() };
        p.values.entry(cell_id(1, 1)).or_default().insert(Attribute::Red, 0.7);
        p.values.entry(1).or_default().insert(Attribute::Dimmer, 0.3);
        l.presets.push(p);
        let v = Value::Preset { preset: 1 };
        assert_eq!(l.resolve(cell_id(2, 1), Attribute::Red, v), Some(0.7), "cell 1 of the same type");
        assert_eq!(l.resolve(2, Attribute::Dimmer, v), Some(0.3));
        assert_eq!(l.resolve(2, Attribute::Red, v), None, "the fixture's master has no red");
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
    fn cue_only_and_block() {
        let mut s = Sequence::default();
        for (v, only) in [(1.0, false), (0.5, true), (0.2, false)] {
            let mut c = LightCue { cue_only: only, ..Default::default() };
            c.values.entry(1).or_default().insert(Attribute::Dimmer, Value::Level(v));
            if v == 0.2 {
                c.values.clear();
                c.values.entry(2).or_default().insert(Attribute::Red, Value::Level(v));
            }
            s.cues.push(c);
        }
        assert_eq!(s.tracked(1)[&1][&Attribute::Dimmer], Value::Level(0.5));
        assert_eq!(s.tracked(2)[&1][&Attribute::Dimmer], Value::Level(1.0), "cue only doesn't track");
        s.block(2);
        assert_eq!(s.cues[2].values.len(), 2);
        assert!(s.cues[2].block);
        s.cues[0].values.get_mut(&1).unwrap().insert(Attribute::Dimmer, Value::Level(0.3));
        assert_eq!(s.tracked(2)[&1][&Attribute::Dimmer], Value::Level(1.0), "blocked");
        // Unblocked: the values equal to what tracks in go (fixture 1's dimmer), the rest stays.
        s.cues[0].values.get_mut(&1).unwrap().insert(Attribute::Dimmer, Value::Level(1.0));
        s.unblock(2);
        assert!(!s.cues[2].block);
        assert_eq!(s.cues[2].values.keys().copied().collect::<Vec<_>>(), [2]);
        assert_eq!(s.tracked(2)[&1][&Attribute::Dimmer], Value::Level(1.0), "tracks again");
    }

    #[test]
    fn hang_on_a_bar() {
        let bar = PlotShape { a: [-3.0, -2.0], b: [3.0, -2.0], height: 6.5, ..Default::default() };
        assert_eq!(bar.hang(1), [[0.0, 6.5, -2.0]]);
        assert_eq!(bar.hang(3), [[-2.0, 6.5, -2.0], [0.0, 6.5, -2.0], [2.0, 6.5, -2.0]]);
        assert_eq!(bar.length(), 6.0);
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

    /// Cloning shares values; writing copies only the written one; comparing still compares values.
    #[test]
    fn shared_values_copy_on_write() {
        let mut a: Shared<Look> = Look::new().into();
        a.entry(1).or_default().insert(Attribute::Dimmer, 0.5.into());
        let mut b = a.clone();
        assert!(std::sync::Arc::ptr_eq(&a.0, &b.0) && a == b);
        b.entry(1).or_default().insert(Attribute::Dimmer, 1.0.into());
        assert!(!std::sync::Arc::ptr_eq(&a.0, &b.0) && a != b);
        assert_eq!(a[&1][&Attribute::Dimmer], 0.5.into(), "the original is untouched");
        let c: Shared<Look> = (*b).clone().into();
        assert_eq!(b, c, "different allocations with the same values are equal");
        assert_eq!(serde_json::to_string(&c).unwrap(), serde_json::to_string(&*c).unwrap(), "saved like the plain map");
    }

    /// The fixture lookup cache follows edits made through the public fields.
    #[test]
    fn fixture_lookup_follows_edits() {
        let mut l = rig();
        let first = l.fixtures[0].id;
        assert_eq!(l.fixture(first).map(|f| f.id), Some(first));
        l.fixtures.reverse();
        assert_eq!(l.fixture(first).map(|f| f.id), Some(first), "moved");
        l.fixtures.retain(|f| f.id != first);
        assert!(l.fixture(first).is_none(), "removed");
        let mut f = l.fixtures[0].clone();
        f.id = 4242;
        l.fixtures.push(f);
        assert_eq!(l.fixture(4242).map(|f| f.id), Some(4242), "added");
        assert!(l.clone().fixture(4242).is_some(), "a clone starts with an empty cache");
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
        l.presets.push(Preset { id: 1, name: "Red".into(), feature: Some(Feature::Colour), values: values.into() });
        let p = Value::Preset { preset: 1 };
        assert_eq!(l.resolve(101, Attribute::Red, p), Some(0.7));
        assert_eq!(l.resolve(103, Attribute::Red, p), Some(0.7), "same type");
        assert_eq!(l.resolve(103, Attribute::Green, p), None);
        assert_eq!(l.resolve(103, Attribute::Red, Value::Preset { preset: 9 }), None);
        assert_eq!(l.preset_attributes(1, 104), [Attribute::Red]);
        let mut cue = LightCue::default();
        cue.values.entry(103).or_default().insert(Attribute::Red, p);
        l.sequences.push(Sequence { id: 1, cues: vec![cue].into(), ..Default::default() });
        let mut d = l.clone();
        d.delete_preset(1);
        assert_eq!(d.sequences[0].cues[0].values[&103][&Attribute::Red], Value::Level(0.7), "unlinked, not lost");
        // Saved as a bare number or {"preset": n}.
        let mut c = LightCue::default();
        c.values.entry(101).or_default().insert(Attribute::Red, p);
        c.values.entry(101).or_default().insert(Attribute::Dimmer, 0.5.into());
        let json = serde_json::to_string(&c.values).unwrap();
        assert_eq!(json, r#"{"101":{"Dimmer":0.5,"Red":{"preset":1}}}"#);
        assert_eq!(serde_json::from_str::<Look>(&json).unwrap(), *c.values);
    }

    #[test]
    fn roundtrip_json() {
        let mut l = rig();
        let mut c = LightCue::default();
        c.values.entry(101).or_default().insert(Attribute::Red, 0.25.into());
        l.sequences.push(Sequence { id: 1, name: "Main".into(), cues: vec![c].into(), ..Default::default() });
        let s = serde_json::to_string(&l).unwrap();
        assert_eq!(serde_json::from_str::<Lighting>(&s).unwrap(), l);
    }
}
