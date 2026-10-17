//! Import GDTF fixture types (General Device Type Format, DIN SPEC 15800): a zip whose
//! `description.xml` describes attributes, wheels, geometry and DMX modes.
//!
//! * A DMX channel's attribute comes from its first channel function with a known GDTF
//!   attribute (`ColorAdd_R` → Red, `Gobo2` → Gobo 2…, see [`attribute`]).
//! * Channel sets (and channel functions without sets) become named ranges; sets that pick a
//!   wheel slot take the slot's name and colour (CIE xyY converted to sRGB).
//! * Multi-instance fixtures: channels on a geometry used by `GeometryReference`s are repeated
//!   for every reference (offset by its DMX break offset), one cell each. Channels on geometries
//!   that repeat another geometry's attributes (pixels without references) are cells too.
//! * Multi-break modes: a channel on a later DMX break keeps it ([`ChannelDef::dmx_break`]; a
//!   reference's offset for that break places its cells), and each break is patched at an
//!   address of its own ([`super::model::Fixture::breaks`]).
//! * Wheel slot pictures (`wheels/*.png|svg`) become thumbnails ([`super::gobo`]).
//! * Not read: 3D models, virtual channels, macros.
//!
//! [`export`] writes a fixture type back as a GDTF file (MVR exports carry them).

use super::import::{attribute_from_name, place, type_id, Imported};
use super::model::{Attribute, ChannelDef, ChannelRange, Emitter, FixtureMode, FixtureType, Head, Physical};
use roxmltree::{Document, Node};
use std::collections::HashMap;

/// Largest `description.xml` read.
const MAX_XML: usize = 32 << 20;

/// Most DMX breaks a mode may use.
const MAX_BREAKS: u8 = 8;

/// A GDTF `DMXBreak` as OpenQ's 0-based [`ChannelDef::dmx_break`] ("Overwrite" = the first).
fn dmx_break(s: &str) -> Option<u8> {
    match s.trim() {
        "Overwrite" => Some(0),
        n => n.parse::<u8>().ok().filter(|b| (1..=MAX_BREAKS).contains(b)).map(|b| b - 1),
    }
}

/// OpenQ attribute of a GDTF attribute name.
pub fn attribute(name: &str) -> Option<Attribute> {
    use Attribute::*;
    // Wheel/instance numbers at the end: "Gobo2Pos" → ("Gobo", 2, "Pos").
    let lead: String = name.chars().take_while(|c| c.is_ascii_alphabetic() || *c == '_').collect();
    let rest = &name[lead.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: u32 = digits.parse().unwrap_or(1);
    let tail = &rest[digits.len()..];
    Some(match (lead.as_str(), tail) {
        ("Dimmer", _) => Dimmer,
        ("PanRotate" | "TiltRotate" | "PanMode" | "TiltMode", _) => return None,
        ("Pan", "") => Pan,
        ("Tilt", "") => Tilt,
        ("PositionMSpeed" | "PositionMTime", _) | ("PanTiltSpeed", _) => PanTiltSpeed,
        ("ColorAdd_R" | "ColorRGB_Red", _) => Red,
        ("ColorAdd_G" | "ColorRGB_Green", _) => Green,
        ("ColorAdd_B" | "ColorRGB_Blue", _) => Blue,
        ("ColorAdd_W" | "ColorAdd_WW" | "ColorAdd_CW" | "ColorRGB_White" | "ColorRGB_WarmWhite" | "ColorRGB_CoolWhite", _) => White,
        ("ColorAdd_A" | "ColorAdd_RY" | "ColorRGB_Amber", _) => Amber,
        ("ColorAdd_GY" | "ColorRGB_Lime", _) => Lime,
        ("ColorAdd_UV" | "ColorRGB_UV", _) => Uv,
        ("ColorAdd_C" | "ColorSub_C" | "ColorRGB_Cyan", _) => Cyan,
        ("ColorAdd_M" | "ColorSub_M" | "ColorRGB_Magenta", _) => Magenta,
        ("ColorAdd_Y" | "ColorSub_Y" | "ColorRGB_Yellow", _) => Yellow,
        ("CTO" | "CTC" | "CTB", _) => Cto,
        ("Color", "" | "WheelIndex" | "WheelSpin" | "WheelAudio" | "Spin") if n == 1 => ColorWheel,
        ("Color", "" | "WheelIndex" | "WheelSpin") if n == 2 => ColorWheel2,
        ("Color", "" | "WheelIndex" | "WheelSpin") => ColorMacro,
        ("ColorMacro" | "ColorMixMSpeed", _) => ColorMacro,
        ("Gobo", "" | "SelectSpin" | "SelectShake" | "SelectEffects" | "WheelIndex" | "WheelSpin" | "WheelShake" | "WheelRandom" | "WheelAudio") => {
            if n >= 2 { Gobo2 } else { Gobo }
        }
        ("Gobo", t) if t.starts_with("Pos") => {
            if n >= 2 { Gobo2Rotation } else { GoboRotation }
        }
        ("Prism", "") => {
            if n >= 2 { Prism2 } else { Prism }
        }
        ("Prism", t) if t.starts_with("Pos") => PrismRotation,
        ("Zoom" | "ZoomModeSpot" | "ZoomModeBeam", _) => Zoom,
        ("Focus", _) => Focus,
        ("Iris", _) => Iris,
        ("Frost", _) => {
            if n >= 2 { Frost2 } else { Frost }
        }
        ("Shutter" | "StrobeDuration" | "StrobeRate" | "StrobeFrequency", _) => Shutter,
        ("Fog" | "Haze" | "FogOutput", _) => Fog,
        ("Fans" | "Fan", _) => Fan,
        ("Effects", "Rate" | "Fade" | "Adjust") | ("EffectsRate", _) => {
            if n >= 2 { Effect2Speed } else { EffectSpeed }
        }
        ("Effects" | "AnimationWheel" | "AnimationSystem", _) => {
            if n >= 2 { Effect2 } else { Effect }
        }
        ("Control" | "Function" | "Reset" | "LampControl" | "LampPowerMode" | "DisplayIntensity" | "DMXInput" | "NoFeature" | "Blower" | "Pump", _) => {
            if lead == "NoFeature" {
                return None;
            }
            Control
        }
        _ => return attribute_from_name(name),
    })
}

/// "32768/2" → 32768 out of 65535; a bare number is 8-bit.
fn dmx_value(s: &str) -> Option<f64> {
    let (v, bytes) = match s.split_once('/') {
        Some((v, b)) => (v.trim().parse::<f64>().ok()?, b.trim().parse::<u32>().ok()?.clamp(1, 4)),
        None => (s.trim().parse::<f64>().ok()?, 1),
    };
    let max = (1u64 << (8 * bytes)) as f64 - 1.0;
    Some((v / max).clamp(0.0, 1.0))
}

/// Coarse byte of a GDTF DMX value.
fn dmx_byte(s: &str) -> Option<u8> {
    dmx_value(s).map(|v| (v * 255.0).floor() as u8)
}

/// CIE xyY ("0.64,0.33,21.26") → sRGB, scaled so the brightest channel is full.
pub fn cie_to_srgb(s: &str) -> Option<[u8; 3]> {
    let v: Vec<f64> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    let (x, y) = (*v.first()?, *v.get(1)?);
    if y <= 0.0 {
        return None;
    }
    let (xx, zz) = (x / y, (1.0 - x - y) / y);
    let lin = [
        3.2406 * xx - 1.5372 - 0.4986 * zz,
        -0.9689 * xx + 1.8758 + 0.0415 * zz,
        0.0557 * xx - 0.2040 + 1.0570 * zz,
    ];
    let lin = lin.map(|c| c.max(0.0));
    let m = lin.iter().cloned().fold(0.0, f64::max);
    if m <= 0.0 {
        return Some([0, 0, 0]);
    }
    let g = |c: f64| {
        let c = c / m;
        let c = if c <= 0.0031308 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
        (c * 255.0).round().clamp(0.0, 255.0) as u8
    };
    Some([g(lin[0]), g(lin[1]), g(lin[2])])
}

/// Colours of the colour emitters (`<Emitter Color="x,y,Y">`), tied to attributes by the channel
/// functions that drive them (`Emitter="…"`), else by their name.
fn emitters(ft: Node) -> Vec<Emitter> {
    let mut driven: HashMap<&str, Attribute> = HashMap::new();
    for f in ft.descendants().filter(|n| n.has_tag_name("ChannelFunction")) {
        if let (Some(e), Some(a)) = (attr(f, "Emitter"), attr(f, "Attribute").and_then(attribute))
            && a.is_emitter()
        {
            driven.entry(e).or_insert(a);
        }
    }
    let mut out: Vec<Emitter> = Vec::new();
    for e in ft.descendants().filter(|n| n.has_tag_name("Emitter")) {
        let name = attr(e, "Name").unwrap_or("");
        let by_name = || {
            let n = name.to_lowercase();
            [("red", Attribute::Red), ("green", Attribute::Green), ("blue", Attribute::Blue), ("white", Attribute::White), ("amber", Attribute::Amber), ("lime", Attribute::Lime)]
                .into_iter()
                .find(|(k, _)| n.contains(k))
                .map(|x| x.1)
        };
        let Some(a) = driven.get(name).copied().or_else(by_name) else { continue };
        let v: Vec<f32> = attr(e, "Color").unwrap_or("").split(',').filter_map(|x| x.trim().parse().ok()).collect();
        let (Some(&x), Some(&y)) = (v.first(), v.get(1)) else { continue };
        let em = Emitter { attribute: a, x, y, luminance: v.get(2).copied().filter(|l| *l > 0.0).unwrap_or(1.0) };
        if em.is_valid() && !out.iter().any(|o| o.attribute == a) {
            out.push(em);
        }
    }
    out
}

fn attr<'a>(n: Node<'a, '_>, k: &str) -> Option<&'a str> {
    n.attribute(k)
}

fn children<'a, 'i>(n: Node<'a, 'i>, tag: &'a str) -> impl Iterator<Item = Node<'a, 'i>> + 'a {
    n.children().filter(move |c| c.has_tag_name(tag))
}

/// A wheel slot: (name, colour, image).
type Slot = (String, Option<[u8; 3]>, String);

/// Every geometry node (anything named under `Geometries`) by name.
fn geometries<'a, 'i>(root: Node<'a, 'i>) -> HashMap<&'a str, Node<'a, 'i>> {
    root.descendants()
        .filter(|n| n.has_tag_name("Geometries"))
        .flat_map(|g| g.descendants().skip(1))
        .filter(|n| n.is_element())
        .filter_map(|n| Some((attr(n, "Name")?, n)))
        .collect()
}

/// Read a GDTF file (the zip's bytes).
pub fn import(zip: &[u8]) -> anyhow::Result<Imported> {
    let xml = super::zipread::read(zip, "description.xml", MAX_XML)?;
    let text = std::str::from_utf8(&xml).map_err(|_| anyhow::anyhow!("description.xml is not UTF-8"))?;
    let mut r = import_xml(text)?;
    // Wheel slot pictures: `wheels/<MediaFileName>.png` (or .svg, or the name as it is).
    let names: std::collections::BTreeSet<String> =
        r.fixture.modes.iter().flat_map(|m| m.all_channels()).flat_map(|c| c.ranges.iter()).map(|r| r.image.clone()).filter(|i| !i.is_empty()).collect();
    for name in names.into_iter().take(super::gobo::MAX_IMAGES) {
        let data = ["png", "svg"].iter().map(|e| format!("wheels/{name}.{e}")).chain([format!("wheels/{name}")]).find_map(|p| super::zipread::read(zip, &p, MAX_IMAGE).ok());
        match data.map(|d| super::gobo::thumbnail(&d)) {
            Some(Ok(t)) => {
                r.fixture.images.insert(name, t);
            }
            Some(Err(e)) => r.notes.push(format!("picture \"{name}\": {e:#}")),
            None => {}
        }
    }
    Ok(r)
}

/// Largest wheel picture read from a GDTF file.
const MAX_IMAGE: usize = 8 << 20;

/// Read a GDTF `description.xml`.
pub fn import_xml(text: &str) -> anyhow::Result<Imported> {
    let text = super::import::strip_doctype(text)?;
    let doc = Document::parse_with_options(&text, roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: 4_000_000, ..Default::default() })
        .map_err(|e| anyhow::anyhow!("description.xml: {e}"))?;
    let ft = doc.descendants().find(|n| n.has_tag_name("FixtureType")).ok_or_else(|| anyhow::anyhow!("not a GDTF file (no FixtureType)"))?;
    let name = attr(ft, "LongName").filter(|s| !s.is_empty()).or(attr(ft, "Name")).unwrap_or("Fixture").to_string();
    let manufacturer = attr(ft, "Manufacturer").unwrap_or("").to_string();
    let mut notes = Vec::new();
    // Wheels.
    let mut wheels: HashMap<&str, Vec<Slot>> = HashMap::new();
    for w in ft.descendants().filter(|n| n.has_tag_name("Wheel")) {
        let slots = children(w, "Slot")
            .map(|s| {
                (
                    attr(s, "Name").unwrap_or("").to_string(),
                    attr(s, "Color").and_then(cie_to_srgb),
                    attr(s, "MediaFileName").unwrap_or("").to_string(),
                )
            })
            .collect();
        wheels.insert(attr(w, "Name").unwrap_or(""), slots);
    }
    let geoms = geometries(ft);
    // Geometry references: template geometry → [(reference name, DMX offset per break)].
    type Refs<'a> = HashMap<&'a str, Vec<(String, HashMap<u8, i64>)>>;
    let mut refs: Refs = HashMap::new();
    for r in ft.descendants().filter(|n| n.has_tag_name("GeometryReference")) {
        let Some(g) = attr(r, "Geometry") else { continue };
        let offs: HashMap<u8, i64> =
            children(r, "Break").filter_map(|b| Some((dmx_break(attr(b, "DMXBreak").unwrap_or("1"))?, attr(b, "DMXOffset")?.parse::<i64>().ok()?))).collect();
        let list = refs.entry(g).or_default();
        if list.len() < super::model::MAX_CELLS as usize {
            list.push((attr(r, "Name").unwrap_or(g).to_string(), offs));
        }
    }
    // A geometry's template: itself, or the referenced geometry it lies inside.
    let template_of = |g: &str| -> Option<String> {
        if refs.contains_key(g) {
            return Some(g.to_string());
        }
        let n = geoms.get(g)?;
        n.ancestors().take_while(|a| !a.has_tag_name("Geometries")).filter_map(|a| attr(a, "Name")).find(|a| refs.contains_key(a)).map(str::to_string)
    };
    let mut pan_range = None;
    let mut tilt_range = None;
    let mut modes = Vec::new();
    for m in ft.descendants().filter(|n| n.has_tag_name("DMXMode")) {
        let mname = attr(m, "Name").unwrap_or("Mode").to_string();
        // (geometry, offset, def) before splitting into master and cells.
        struct Ch {
            geometry: String,
            def: ChannelDef,
            label: String,
        }
        let mut list: Vec<Ch> = Vec::new();
        // Slots of every channel on the first break, used or not.
        let mut slots = 0u16;
        for c in m.descendants().filter(|n| n.has_tag_name("DMXChannel")) {
            let geometry = attr(c, "Geometry").unwrap_or("").to_string();
            let offsets: Vec<u16> = attr(c, "Offset").unwrap_or("").split(',').filter_map(|x| x.trim().parse::<u16>().ok()).filter(|x| (1..=512).contains(x)).collect();
            let Some(&coarse) = offsets.first() else { continue }; // virtual channel
            let brk_text = attr(c, "DMXBreak").unwrap_or("1");
            let functions: Vec<Node> = c.descendants().filter(|n| n.has_tag_name("ChannelFunction")).collect();
            fn gattr<'a>(f: &Node<'a, '_>) -> &'a str {
                f.attribute("Attribute").unwrap_or("")
            }
            let label = functions.first().map(gattr).or(c.children().find(|x| x.has_tag_name("LogicalChannel")).and_then(|l| attr(l, "Attribute"))).unwrap_or("?").to_string();
            let Some(brk) = dmx_break(brk_text) else {
                notes.push(format!("mode \"{mname}\": channel {label} on DMX break {brk_text}, skipped"));
                continue;
            };
            if brk == 0 && template_of(&geometry).is_none() {
                slots = slots.max(offsets.iter().copied().max().unwrap_or(0));
            }
            // Unknown functions become Extra channels (set by hand, named after the GDTF attribute).
            let a = functions
                .iter()
                .find_map(|f| attribute(gattr(f)))
                .or_else(|| c.children().filter(|x| x.has_tag_name("LogicalChannel")).find_map(|l| attribute(attr(l, "Attribute")?)))
                .unwrap_or(super::import::ANY_EXTRA);
            // Ranges: channel sets, or the functions themselves when there are several.
            let end_of = |i: usize, list: &[(u8, String, Option<[u8; 3]>, String)]| list.get(i + 1).map_or(255, |n| n.0.saturating_sub(1).max(list[i].0));
            let mut starts: Vec<(u8, String, Option<[u8; 3]>, String)> = Vec::new();
            for f in &functions {
                let ffrom = attr(*f, "DMXFrom").and_then(dmx_byte).unwrap_or(0);
                let fname = attr(*f, "Name").unwrap_or(gattr(f)).to_string();
                let wheel = attr(*f, "Wheel").and_then(|w| wheels.get(w));
                let sets: Vec<Node> = children(*f, "ChannelSet").collect();
                if sets.is_empty() {
                    starts.push((ffrom, fname.clone(), None, String::new()));
                }
                for s in sets {
                    let from = attr(s, "DMXFrom").and_then(dmx_byte).unwrap_or(ffrom);
                    let slot = attr(s, "WheelSlotIndex").and_then(|i| i.parse::<usize>().ok()).and_then(|i| wheel?.get(i.checked_sub(1)?));
                    let sname = attr(s, "Name").filter(|n| !n.is_empty()).map(str::to_string).or_else(|| slot.map(|x| x.0.clone()).filter(|n| !n.is_empty())).unwrap_or_else(|| fname.clone());
                    starts.push((from, sname, slot.and_then(|x| x.1), slot.map(|x| x.2.clone()).unwrap_or_default()));
                }
            }
            starts.sort_by_key(|s| s.0);
            starts.dedup_by_key(|s| s.0);
            let ranges: Vec<ChannelRange> = if starts.len() > 1 {
                (0..starts.len())
                    .map(|i| ChannelRange { from: starts[i].0, to: end_of(i, &starts), name: starts[i].1.clone(), color: starts[i].2, image: starts[i].3.clone() })
                    .collect()
            } else {
                Vec::new()
            };
            let default = functions
                .iter()
                .find_map(|f| attr(*f, "Default"))
                .or(attr(c, "Default"))
                .and_then(dmx_value)
                .map(|v| v as f32);
            let highlight = attr(c, "Highlight").filter(|h| *h != "None").and_then(dmx_value).map(|v| v as f32);
            // Pan/tilt range from the physical values of the first function.
            if let Some(f) = functions.iter().find(|f| attribute(gattr(f)) == Some(a))
                && let (Some(from), Some(to)) = (attr(*f, "PhysicalFrom").and_then(|x| x.parse::<f32>().ok()), attr(*f, "PhysicalTo").and_then(|x| x.parse::<f32>().ok()))
            {
                match a {
                    Attribute::Pan => pan_range = pan_range.or(Some((to - from).abs())),
                    Attribute::Tilt => tilt_range = tilt_range.or(Some((to - from).abs())),
                    _ => {}
                }
            }
            // An unknown function keeps the name the file gives it ("Gobo 3", "Lamp control").
            let name = match functions.first().and_then(|f| attr(*f, "Name")) {
                Some(n) if a == super::import::ANY_EXTRA && !n.is_empty() => n.to_string(),
                _ => label.clone(),
            };
            let def = ChannelDef {
                attribute: a,
                name,
                dmx_break: brk,
                offset: coarse - 1,
                fine: offsets.get(1).map(|f| f - 1),
                default,
                highlight,
                ranges,
            };
            list.push(Ch { geometry, def, label });
        }
        // Cells: referenced templates, else geometries whose attributes repeat another's.
        let mut master: Vec<ChannelDef> = Vec::new();
        let mut heads: Vec<Head> = Vec::new();
        let mut attrs_of: HashMap<&str, Vec<Attribute>> = HashMap::new();
        for c in &list {
            attrs_of.entry(c.geometry.as_str()).or_default().push(c.def.attribute);
        }
        for v in attrs_of.values_mut() {
            v.sort();
        }
        let repeated = |g: &str| {
            let a = &attrs_of[g];
            a.iter().any(|x| x.is_emitter() || *x == Attribute::Dimmer) && attrs_of.iter().any(|(k, v)| *k != g && v == a)
        };
        let mut head_index: HashMap<String, usize> = HashMap::new();
        let mut add_head = |heads: &mut Vec<Head>, name: String| -> usize {
            *head_index.entry(name.clone()).or_insert_with(|| {
                heads.push(Head { name, channels: Vec::new() });
                heads.len() - 1
            })
        };
        for c in &list {
            let what = format!("mode \"{mname}\", channel {}", c.label);
            if let Some(t) = template_of(&c.geometry) {
                for (rname, offs) in &refs[t.as_str()] {
                    let off = offs.get(&c.def.dmx_break).copied().unwrap_or(1);
                    let shift = |o: u16| (o as i64 + off - 1).clamp(0, 511) as u16;
                    let def = ChannelDef { offset: shift(c.def.offset), fine: c.def.fine.map(shift), ..c.def.clone() };
                    let i = add_head(&mut heads, rname.clone());
                    place(&mut heads[i].channels, def, &what, &mut notes);
                }
            } else if repeated(&c.geometry) {
                let i = add_head(&mut heads, c.geometry.clone());
                place(&mut heads[i].channels, c.def.clone(), &what, &mut notes);
            } else {
                place(&mut master, c.def.clone(), &what, &mut notes);
            }
        }
        heads.truncate(super::model::MAX_CELLS as usize);
        if master.is_empty() && heads.is_empty() {
            notes.push(format!("mode \"{mname}\": nothing usable, skipped"));
            continue;
        }
        modes.push(FixtureMode { name: mname, channels: master, heads, slots });
    }
    if modes.is_empty() {
        anyhow::bail!("no usable DMX modes");
    }
    let beam: Vec<f32> = ft.descendants().filter(|n| n.has_tag_name("Beam")).filter_map(|b| attr(b, "BeamAngle")?.parse().ok()).collect();
    let d = Physical::default();
    let emitters = emitters(ft);
    let moving = modes.iter().any(|m| m.channel(Attribute::Pan).is_some() && m.channel(Attribute::Tilt).is_some());
    let physical = Physical {
        pan_range: pan_range.unwrap_or(d.pan_range),
        tilt_range: tilt_range.unwrap_or(d.tilt_range),
        beam_min: if beam.is_empty() { d.beam_min } else { beam.iter().cloned().fold(f32::MAX, f32::min) },
        beam_max: if beam.is_empty() { d.beam_max } else { beam.iter().cloned().fold(0.0, f32::max) },
        moving,
        emitters,
    };
    let fixture = FixtureType { id: type_id("gdtf", &manufacturer, &name), manufacturer, name, modes, physical, images: Default::default() };
    Ok(Imported { fixture: super::fixlib::sanitize(fixture)?, notes })
}

/// GDTF attribute name and feature (`Group.Feature`) of an OpenQ attribute.
fn gdtf_attribute(a: Attribute) -> (&'static str, &'static str) {
    use Attribute::*;
    match a {
        Dimmer => ("Dimmer", "Dimmer.Dimmer"),
        Red => ("ColorAdd_R", "Color.RGB"),
        Green => ("ColorAdd_G", "Color.RGB"),
        Blue => ("ColorAdd_B", "Color.RGB"),
        White => ("ColorAdd_W", "Color.RGB"),
        Amber => ("ColorAdd_A", "Color.RGB"),
        Lime => ("ColorAdd_GY", "Color.RGB"),
        Uv => ("ColorAdd_UV", "Color.RGB"),
        Cyan => ("ColorSub_C", "Color.RGB"),
        Magenta => ("ColorSub_M", "Color.RGB"),
        Yellow => ("ColorSub_Y", "Color.RGB"),
        Cto => ("CTO", "Color.Color"),
        ColorWheel => ("Color1", "Color.Color"),
        ColorWheel2 => ("Color2", "Color.Color"),
        ColorMacro => ("ColorMacro1", "Color.Color"),
        Pan => ("Pan", "Position.PanTilt"),
        Tilt => ("Tilt", "Position.PanTilt"),
        PanTiltSpeed => ("PositionMSpeed", "Control.Control"),
        Zoom => ("Zoom", "Beam.Beam"),
        Focus => ("Focus1", "Focus.Focus"),
        Iris => ("Iris", "Beam.Beam"),
        Frost => ("Frost1", "Beam.Beam"),
        Frost2 => ("Frost2", "Beam.Beam"),
        Gobo => ("Gobo1", "Gobo.Gobo"),
        GoboRotation => ("Gobo1Pos", "Gobo.Gobo"),
        Gobo2 => ("Gobo2", "Gobo.Gobo"),
        Gobo2Rotation => ("Gobo2Pos", "Gobo.Gobo"),
        Prism => ("Prism1", "Beam.Beam"),
        PrismRotation => ("Prism1Pos", "Beam.Beam"),
        Prism2 => ("Prism2", "Beam.Beam"),
        Shutter => ("Shutter1", "Beam.Beam"),
        Control => ("Control1", "Control.Control"),
        Control2 => ("Control2", "Control.Control"),
        Control3 => ("Control3", "Control.Control"),
        Effect => ("Effects1", "Control.Control"),
        EffectSpeed => ("Effects1Rate", "Control.Control"),
        Effect2 => ("Effects2", "Control.Control"),
        Effect2Speed => ("Effects2Rate", "Control.Control"),
        Fog => ("Fog", "Control.Control"),
        Fan => ("Fans", "Control.Control"),
        Extra(_) => ("NoFeature", "Control.Control"),
    }
}

/// sRGB → CIE xyY text ("x,y,Y", Y in percent), the form GDTF wheel slots use.
fn srgb_to_cie(c: [u8; 3]) -> String {
    let lin = c.map(|v| {
        let v = v as f64 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    });
    let x = 0.4124 * lin[0] + 0.3576 * lin[1] + 0.1805 * lin[2];
    let y = 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
    let z = 0.0193 * lin[0] + 0.1192 * lin[1] + 0.9505 * lin[2];
    let sum = x + y + z;
    if sum <= 0.0 {
        return "0.3127,0.3290,0".into();
    }
    format!("{:.4},{:.4},{:.2}", x / sum, y / sum, y * 100.0)
}

fn xml_esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => o.push(c),
        }
    }
    o
}

/// A DMX value in the channel's resolution ("128/1", "32768/2").
fn dmx_text(v: f32, fine: bool) -> String {
    let v = v.clamp(0.0, 1.0);
    if fine { format!("{}/2", (v * 65535.0).round() as u32) } else { format!("{}/1", (v * 255.0).round() as u32) }
}

/// A fixture type as a GDTF file (the zip), for MVR exports: attributes, modes with their
/// channels (fine channels, breaks, defaults, highlights), named ranges as channel sets, wheels
/// for ranges with a colour or a picture (pictures from [`FixtureType::images`] in `wheels/`),
/// emitters, pan/tilt ranges and the beam angle. Cells become geometries of their own, which
/// [`import`] reads back as cells when they repeat each other's channels. No 3D models.
pub fn export(t: &FixtureType) -> anyhow::Result<Vec<u8>> {
    let e = xml_esc;
    let uuid = {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(format!("openq-gdtf\0{}", t.id).as_bytes());
        let mut b: [u8; 16] = h[..16].try_into().expect("32-byte digest");
        b[6] = (b[6] & 0x0f) | 0x50;
        b[8] = (b[8] & 0x3f) | 0x80;
        uuid::Uuid::from_bytes(b).to_string().to_uppercase()
    };
    let short: String = t.name.chars().take(16).collect();
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\" ?>\n<GDTF DataVersion=\"1.2\">\n");
    x.push_str(&format!(
        "  <FixtureType Name=\"{}\" ShortName=\"{}\" LongName=\"{}\" Manufacturer=\"{}\" Description=\"Exported by OpenQ\" FixtureTypeID=\"{uuid}\" Thumbnail=\"\" RefFT=\"\">\n",
        e(&t.name),
        e(&short),
        e(&t.name),
        e(if t.manufacturer.trim().is_empty() { "Generic" } else { &t.manufacturer })
    ));
    // Attributes used, in a stable order.
    let used: std::collections::BTreeSet<Attribute> = t.modes.iter().flat_map(|m| m.all_channels()).map(|c| c.attribute).collect();
    let mut names: Vec<(&str, &str)> = used.iter().map(|a| gdtf_attribute(*a)).collect();
    names.sort();
    names.dedup();
    x.push_str("    <AttributeDefinitions>\n      <ActivationGroups/>\n      <FeatureGroups>\n");
    for (group, features) in [("Dimmer", &["Dimmer"][..]), ("Position", &["PanTilt"]), ("Gobo", &["Gobo"]), ("Color", &["Color", "RGB"]), ("Beam", &["Beam"]), ("Focus", &["Focus"]), ("Control", &["Control"])] {
        x.push_str(&format!("        <FeatureGroup Name=\"{group}\" Pretty=\"{group}\">\n"));
        for f in features {
            x.push_str(&format!("          <Feature Name=\"{f}\"/>\n"));
        }
        x.push_str("        </FeatureGroup>\n");
    }
    x.push_str("      </FeatureGroups>\n      <Attributes>\n");
    for (name, feature) in &names {
        x.push_str(&format!("        <Attribute Name=\"{name}\" Pretty=\"{name}\" Feature=\"{feature}\" PhysicalUnit=\"None\"/>\n"));
    }
    x.push_str("      </Attributes>\n    </AttributeDefinitions>\n");
    // Wheels: one per channel with coloured or pictured ranges.
    let wheel_of = |mi: usize, unit: usize, c: &ChannelDef| format!("{}_{mi}_{unit}_{}_{}", gdtf_attribute(c.attribute).0, c.dmx_break, c.offset);
    let has_wheel = |c: &ChannelDef| c.ranges.len() > 1 && c.ranges.iter().any(|r| r.color.is_some() || !r.image.is_empty());
    let mut wheels = String::new();
    let mut pictures: Vec<(String, Vec<u8>)> = Vec::new();
    for (mi, m) in t.modes.iter().enumerate() {
        let units = std::iter::once(&m.channels).chain(m.heads.iter().map(|h| &h.channels));
        for (ui, chans) in units.enumerate() {
            for c in chans.iter().filter(|c| has_wheel(c)) {
                wheels.push_str(&format!("      <Wheel Name=\"{}\">\n", wheel_of(mi, ui, c)));
                for r in &c.ranges {
                    let mut slot = format!("        <Slot Name=\"{}\"", e(&r.name));
                    if let Some(col) = r.color {
                        slot.push_str(&format!(" Color=\"{}\"", srgb_to_cie(col)));
                    }
                    if !r.image.is_empty()
                        && let Some(img) = t.images.get(&r.image)
                    {
                        use base64::Engine;
                        if let Ok(png) = base64::engine::general_purpose::STANDARD.decode(img) {
                            slot.push_str(&format!(" MediaFileName=\"{}\"", e(&r.image)));
                            if !pictures.iter().any(|(n, _)| *n == r.image) {
                                pictures.push((r.image.clone(), png));
                            }
                        }
                    }
                    slot.push_str("/>\n");
                    wheels.push_str(&slot);
                }
                wheels.push_str("      </Wheel>\n");
            }
        }
    }
    if !wheels.is_empty() {
        x.push_str(&format!("    <Wheels>\n{wheels}    </Wheels>\n"));
    }
    let p = &t.physical;
    if !p.emitters.is_empty() {
        x.push_str("    <PhysicalDescriptions>\n      <Emitters>\n");
        for em in &p.emitters {
            x.push_str(&format!("        <Emitter Name=\"{}\" Color=\"{:.4},{:.4},{}\"/>\n", em.attribute.name(), em.x, em.y, em.luminance));
        }
        x.push_str("      </Emitters>\n    </PhysicalDescriptions>\n");
    }
    x.push_str("    <Models/>\n");
    // Geometry: body, yoke and head for moving fixtures, one beam per cell.
    let mut cell_names: Vec<String> = Vec::new();
    for h in t.modes.iter().flat_map(|m| &m.heads) {
        let base = if h.name.trim().is_empty() { "Cell".to_string() } else { h.name.trim().to_string() };
        if !cell_names.contains(&base) && !["Body", "Yoke", "Head", "Beam"].contains(&base.as_str()) {
            cell_names.push(base);
        }
    }
    let beams = |indent: &str| {
        let mut s = format!("{indent}<Beam Name=\"Beam\" BeamAngle=\"{}\" FieldAngle=\"{}\"/>\n", p.beam_max, p.beam_max);
        for c in &cell_names {
            s.push_str(&format!("{indent}<Beam Name=\"{}\" BeamAngle=\"{}\"/>\n", e(c), p.beam_max));
        }
        s
    };
    x.push_str("    <Geometries>\n      <Geometry Name=\"Body\">\n");
    if p.moving {
        x.push_str(&format!("        <Axis Name=\"Yoke\">\n          <Axis Name=\"Head\">\n{}          </Axis>\n        </Axis>\n", beams("            ")));
    } else {
        x.push_str(&beams("        "));
    }
    x.push_str("      </Geometry>\n    </Geometries>\n    <DMXModes>\n");
    let main_geometry = |a: Attribute| match a {
        Attribute::Pan if p.moving => "Yoke",
        _ if p.moving => "Head",
        _ => "Body",
    };
    for (mi, m) in t.modes.iter().enumerate() {
        x.push_str(&format!("      <DMXMode Name=\"{}\" Geometry=\"Body\">\n        <DMXChannels>\n", e(&m.name)));
        let units = std::iter::once((None, &m.channels)).chain(m.heads.iter().map(|h| (Some(h), &h.channels)));
        for (ui, (head, chans)) in units.enumerate() {
            let geometry = match head {
                Some(h) => {
                    let n = h.name.trim();
                    if n.is_empty() { "Cell".to_string() } else { n.to_string() }
                }
                None => String::new(),
            };
            for c in chans {
                let (gname, _) = gdtf_attribute(c.attribute);
                let geo = if geometry.is_empty() || !cell_names.contains(&geometry) { main_geometry(c.attribute).to_string() } else { geometry.clone() };
                let offset = match c.fine {
                    Some(f) => format!("{},{}", c.offset + 1, f + 1),
                    None => format!("{}", c.offset + 1),
                };
                let fine = c.fine.is_some();
                let highlight = c.highlight.map_or("None".to_string(), |h| dmx_text(h, fine));
                x.push_str(&format!("          <DMXChannel DMXBreak=\"{}\" Offset=\"{offset}\" Highlight=\"{highlight}\" Geometry=\"{}\">\n", c.dmx_break + 1, e(&geo)));
                x.push_str(&format!("            <LogicalChannel Attribute=\"{gname}\" Snap=\"No\" Master=\"None\" MibFade=\"0\" DMXChangeTimeLimit=\"0\">\n"));
                let fname = if c.name.trim().is_empty() { c.attribute.name().to_string() } else { c.name.trim().to_string() };
                let mut f = format!(
                    "              <ChannelFunction Name=\"{}\" Attribute=\"{gname}\" OriginalAttribute=\"\" DMXFrom=\"{}\" Default=\"{}\"",
                    e(&fname),
                    dmx_text(0.0, fine),
                    dmx_text(c.default_value(), fine)
                );
                let range = match c.attribute {
                    Attribute::Pan => Some(p.pan_range),
                    Attribute::Tilt => Some(p.tilt_range),
                    _ => None,
                };
                if let Some(r) = range {
                    f.push_str(&format!(" PhysicalFrom=\"{}\" PhysicalTo=\"{}\"", -r / 2.0, r / 2.0));
                }
                if c.attribute.is_emitter() && p.emitters.iter().any(|em| em.attribute == c.attribute) {
                    f.push_str(&format!(" Emitter=\"{}\"", c.attribute.name()));
                }
                if has_wheel(c) {
                    f.push_str(&format!(" Wheel=\"{}\"", wheel_of(mi, ui, c)));
                }
                if c.ranges.len() > 1 {
                    x.push_str(&f);
                    x.push_str(">\n");
                    for (i, r) in c.ranges.iter().enumerate() {
                        let from = if fine { format!("{}/2", r.from as u32 * 256) } else { format!("{}/1", r.from) };
                        let slot = if has_wheel(c) { format!(" WheelSlotIndex=\"{}\"", i + 1) } else { String::new() };
                        x.push_str(&format!("                <ChannelSet Name=\"{}\" DMXFrom=\"{from}\"{slot}/>\n", e(&r.name)));
                    }
                    x.push_str("              </ChannelFunction>\n");
                } else {
                    x.push_str(&f);
                    x.push_str("/>\n");
                }
                x.push_str("            </LogicalChannel>\n          </DMXChannel>\n");
            }
        }
        x.push_str("        </DMXChannels>\n        <Relations/>\n        <FTMacros/>\n      </DMXMode>\n");
    }
    x.push_str("    </DMXModes>\n  </FixtureType>\n</GDTF>\n");
    let files: Vec<(String, Vec<u8>)> = pictures.into_iter().map(|(n, d)| (format!("wheels/{n}.png"), d)).collect();
    let mut entries: Vec<(&str, &[u8])> = vec![("description.xml", x.as_bytes())];
    entries.extend(files.iter().map(|(n, d)| (n.as_str(), d.as_slice())));
    super::zipread::write(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPOT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<GDTF DataVersion="1.2">
  <FixtureType Name="Spot" LongName="Test Spot 300" Manufacturer="Acme">
    <Wheels>
      <Wheel Name="Color1">
        <Slot Name="Open" Color="0.3127,0.3290,100.0"/>
        <Slot Name="Deep Red" Color="0.6400,0.3300,21.26"/>
      </Wheel>
      <Wheel Name="Gobo1"><Slot Name="Open"/><Slot Name="Star" MediaFileName="star"/></Wheel>
      <Wheel Name="Gobo2"><Slot Name="Open"/><Slot Name="Dots"/></Wheel>
    </Wheels>
    <Geometries>
      <Geometry Name="Body">
        <Axis Name="Yoke"><Axis Name="Head"><Beam Name="Beam" BeamAngle="18"/></Axis></Axis>
      </Geometry>
    </Geometries>
    <DMXModes>
      <DMXMode Name="Standard" Geometry="Body">
        <DMXChannels>
          <DMXChannel DMXBreak="1" Offset="1,2" Geometry="Yoke">
            <LogicalChannel Attribute="Pan">
              <ChannelFunction Name="Pan" Attribute="Pan" DMXFrom="0/2" Default="32768/2" PhysicalFrom="-270" PhysicalTo="270"/>
            </LogicalChannel>
          </DMXChannel>
          <DMXChannel DMXBreak="1" Offset="3" Geometry="Head">
            <LogicalChannel Attribute="Color1">
              <ChannelFunction Name="Color1" Attribute="Color1" DMXFrom="0/1" Wheel="Color1">
                <ChannelSet Name="" DMXFrom="0/1" WheelSlotIndex="1"/>
                <ChannelSet Name="" DMXFrom="10/1" WheelSlotIndex="2"/>
              </ChannelFunction>
              <ChannelFunction Name="Spin" Attribute="Color1WheelSpin" DMXFrom="128/1"/>
            </LogicalChannel>
          </DMXChannel>
          <DMXChannel DMXBreak="1" Offset="4" Geometry="Head">
            <LogicalChannel Attribute="Gobo1"><ChannelFunction Attribute="Gobo1" DMXFrom="0/1" Wheel="Gobo1">
              <ChannelSet DMXFrom="0/1" WheelSlotIndex="1"/><ChannelSet DMXFrom="20/1" WheelSlotIndex="2"/>
            </ChannelFunction></LogicalChannel>
          </DMXChannel>
          <DMXChannel DMXBreak="1" Offset="5" Geometry="Head">
            <LogicalChannel Attribute="Gobo2"><ChannelFunction Attribute="Gobo2" DMXFrom="0/1" Wheel="Gobo2"/></LogicalChannel>
          </DMXChannel>
          <DMXChannel DMXBreak="1" Offset="6" Geometry="Head" Highlight="255/1">
            <LogicalChannel Attribute="Dimmer"><ChannelFunction Attribute="Dimmer" DMXFrom="0/1"/></LogicalChannel>
          </DMXChannel>
          <DMXChannel DMXBreak="2" Offset="1" Geometry="Head">
            <LogicalChannel Attribute="Zoom"><ChannelFunction Attribute="Zoom" DMXFrom="0/1"/></LogicalChannel>
          </DMXChannel>
        </DMXChannels>
      </DMXMode>
    </DMXModes>
  </FixtureType>
</GDTF>"#;

    const BAR: &str = r#"<GDTF DataVersion="1.2"><FixtureType Name="Bar" Manufacturer="Acme">
    <Geometries>
      <Geometry Name="Body">
        <GeometryReference Name="Pixel 1" Geometry="Pixel"><Break DMXBreak="1" DMXOffset="2"/></GeometryReference>
        <GeometryReference Name="Pixel 2" Geometry="Pixel"><Break DMXBreak="1" DMXOffset="5"/></GeometryReference>
      </Geometry>
      <Geometry Name="Pixel"><Beam Name="PixelBeam" BeamAngle="40"/></Geometry>
    </Geometries>
    <DMXModes><DMXMode Name="7ch" Geometry="Body"><DMXChannels>
      <DMXChannel DMXBreak="1" Offset="1" Geometry="Body"><LogicalChannel Attribute="Dimmer"><ChannelFunction Attribute="Dimmer"/></LogicalChannel></DMXChannel>
      <DMXChannel DMXBreak="1" Offset="1" Geometry="Pixel"><LogicalChannel Attribute="ColorAdd_R"><ChannelFunction Attribute="ColorAdd_R"/></LogicalChannel></DMXChannel>
      <DMXChannel DMXBreak="1" Offset="2" Geometry="Pixel"><LogicalChannel Attribute="ColorAdd_G"><ChannelFunction Attribute="ColorAdd_G"/></LogicalChannel></DMXChannel>
      <DMXChannel DMXBreak="1" Offset="3" Geometry="Pixel"><LogicalChannel Attribute="ColorAdd_B"><ChannelFunction Attribute="ColorAdd_B"/></LogicalChannel></DMXChannel>
    </DMXChannels></DMXMode></DMXModes>
    </FixtureType></GDTF>"#;

    #[test]
    fn spot() {
        // The star gobo's picture is in the zip; a missing one is simply left out.
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 255, 255, 255])))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let zip = super::super::zipread::tests::make(&[("description.xml", SPOT.as_bytes()), ("wheels/star.png", &png)]);
        let r = import(&zip).unwrap();
        let f = &r.fixture;
        assert_eq!(f.images.keys().collect::<Vec<_>>(), ["star"]);
        assert!(crate::light::gobo::decode(&f.images["star"]).is_some());
        assert_eq!((f.id.as_str(), f.manufacturer.as_str(), f.name.as_str()), ("gdtf/acme/test-spot-300", "Acme", "Test Spot 300"));
        let m = &f.modes[0];
        let pan = m.channel(Attribute::Pan).unwrap();
        assert_eq!((pan.offset, pan.fine), (0, Some(1)));
        assert!((pan.default.unwrap() - 0.5).abs() < 0.001);
        assert_eq!(f.physical.pan_range, 540.0);
        assert_eq!(f.physical.beam_max, 18.0);
        let cw = m.channel(Attribute::ColorWheel).unwrap();
        assert_eq!(cw.ranges.len(), 3);
        assert_eq!((cw.ranges[1].from, cw.ranges[1].to, cw.ranges[1].name.as_str()), (10, 127, "Deep Red"));
        let red = cw.ranges[1].color.unwrap();
        assert!(red[0] == 255 && red[1] < 40 && red[2] < 40, "{red:?}");
        assert_eq!(cw.ranges[0].color, Some([255, 255, 255]));
        assert_eq!(cw.ranges[2].name, "Spin");
        let gobo = m.channel(Attribute::Gobo).unwrap();
        assert_eq!(gobo.ranges[1].name, "Star");
        assert_eq!(gobo.ranges[1].image, "star");
        assert_eq!(m.channel(Attribute::Gobo2).unwrap().offset, 4);
        assert_eq!(m.channel(Attribute::Dimmer).unwrap().highlight, Some(1.0));
        // Zoom is on the second DMX break, at its first slot.
        let zoom = m.channel(Attribute::Zoom).unwrap();
        assert_eq!((zoom.dmx_break, zoom.offset), (1, 0));
        assert_eq!(m.breaks(), 2);
        assert_eq!(m.footprint_of(1), 1);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        assert!(!f.physical.moving, "no tilt channel");
    }

    #[test]
    fn geometry_references_become_cells() {
        let r = import_xml(BAR).unwrap();
        let m = &r.fixture.modes[0];
        assert_eq!(m.channels.len(), 1, "master dimmer");
        assert_eq!(m.heads.len(), 2);
        assert_eq!(m.heads[0].name, "Pixel 1");
        let offs = |h: &Head| h.channels.iter().map(|c| c.offset).collect::<Vec<_>>();
        assert_eq!(offs(&m.heads[0]), [1, 2, 3]);
        assert_eq!(offs(&m.heads[1]), [4, 5, 6]);
        assert_eq!(m.footprint(), 7);
    }

    #[test]
    fn emitter_colours() {
        let xml = BAR
            .replace("<Geometries>", r#"<PhysicalDescriptions><Emitters>
              <Emitter Name="R" Color="0.69,0.30,30.5"/><Emitter Name="Deep Blue" Color="0.14,0.05,6"/><Emitter Name="G" Color="0.2,0.7,0"/>
              <Emitter Name="Bad" Color="nan,0.3,1"/></Emitters></PhysicalDescriptions><Geometries>"#)
            .replace(r#"<ChannelFunction Attribute="ColorAdd_R"/>"#, r#"<ChannelFunction Attribute="ColorAdd_R" Emitter="R"/>"#)
            .replace(r#"<ChannelFunction Attribute="ColorAdd_G"/>"#, r#"<ChannelFunction Attribute="ColorAdd_G" Emitter="G"/>"#);
        let e = import_xml(&xml).unwrap().fixture.physical.emitters;
        assert_eq!(e.len(), 3, "{e:?}");
        assert_eq!((e[0].attribute, e[0].x, e[0].luminance), (Attribute::Red, 0.69, 30.5));
        assert_eq!(e[1].attribute, Attribute::Blue, "by name");
        assert_eq!((e[2].attribute, e[2].luminance), (Attribute::Green, 1.0), "no luminance: 1");
    }

    #[test]
    fn multi_break_cells() {
        // A moving light whose pixels take a second DMX input: two pixels at 1 and 4 of break 2.
        let xml = BAR
            .replace(r#"<Break DMXBreak="1" DMXOffset="2"/>"#, r#"<Break DMXBreak="1" DMXOffset="1"/><Break DMXBreak="2" DMXOffset="1"/>"#)
            .replace(r#"<Break DMXBreak="1" DMXOffset="5"/>"#, r#"<Break DMXBreak="1" DMXOffset="1"/><Break DMXBreak="2" DMXOffset="4"/>"#)
            .replace(r#"DMXBreak="1" Offset="1" Geometry="Pixel""#, r#"DMXBreak="2" Offset="1" Geometry="Pixel""#)
            .replace(r#"DMXBreak="1" Offset="2" Geometry="Pixel""#, r#"DMXBreak="2" Offset="2" Geometry="Pixel""#)
            .replace(r#"DMXBreak="1" Offset="3" Geometry="Pixel""#, r#"DMXBreak="2" Offset="3" Geometry="Pixel""#);
        let r = import_xml(&xml).unwrap();
        let m = &r.fixture.modes[0];
        assert_eq!((m.breaks(), m.footprint(), m.footprint_of(1)), (2, 1, 6));
        let offs = |h: &Head| h.channels.iter().map(|c| (c.dmx_break, c.offset)).collect::<Vec<_>>();
        assert_eq!(offs(&m.heads[1]), [(1, 3), (1, 4), (1, 5)]);
        assert_eq!(dmx_break("9"), None);
        assert_eq!(dmx_break("Overwrite"), Some(0));
    }

    #[test]
    fn names_values_and_bad_input() {
        assert_eq!(attribute("Gobo2Pos"), Some(Attribute::Gobo2Rotation));
        assert_eq!(attribute("Gobo1PosRotate"), Some(Attribute::GoboRotation));
        assert_eq!(attribute("Color2"), Some(Attribute::ColorWheel2));
        assert_eq!(attribute("Color3"), Some(Attribute::ColorMacro));
        assert_eq!(attribute("Effects2Rate"), Some(Attribute::Effect2Speed));
        assert_eq!(attribute("ColorAdd_WW"), Some(Attribute::White));
        assert_eq!(attribute("Shutter1Strobe"), Some(Attribute::Shutter));
        assert_eq!(attribute("NoFeature"), None);
        assert_eq!(dmx_value("65535/2"), Some(1.0));
        assert_eq!(dmx_byte("128/1"), Some(128));
        assert!(import(b"not a zip").is_err());
        assert!(import_xml("<a/>").is_err());
        assert!(import_xml("<!DOCTYPE x [<!ENTITY a \"b\">]><GDTF/>").is_err(), "no DTDs");
    }

    /// A type written as GDTF reads back with the same channels, ranges, cells and pictures.
    #[test]
    fn export_reads_back() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 255, 255, 255])))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let zip = super::super::zipread::tests::make(&[("description.xml", SPOT.as_bytes()), ("wheels/star.png", &png)]);
        let mut types = vec![import(&zip).unwrap().fixture, import_xml(BAR).unwrap().fixture];
        types.extend(crate::light::library::generics());
        for t in &types {
            let back = import(&export(t).unwrap()).unwrap();
            let b = &back.fixture;
            assert!(back.notes.is_empty(), "{}: {:?}", t.name, back.notes);
            assert_eq!((b.name.as_str(), b.manufacturer.as_str()), (t.name.as_str(), t.manufacturer.as_str()));
            assert_eq!(b.modes.len(), t.modes.len(), "{}", t.name);
            for (m, n) in t.modes.iter().zip(&b.modes) {
                assert_eq!(m.name, n.name);
                let key = |c: &ChannelDef| (c.attribute, c.dmx_break, c.offset, c.fine, c.ranges.iter().map(|r| (r.from, r.name.clone(), r.color.is_some(), r.image.clone())).collect::<Vec<_>>());
                let mut a: Vec<_> = m.channels.iter().map(key).collect();
                let mut z: Vec<_> = n.channels.iter().map(key).collect();
                a.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
                z.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
                assert_eq!(a, z, "{} / {}", t.name, m.name);
                assert_eq!(m.heads.len(), n.heads.len(), "{} cells", t.name);
                for (h, k) in m.heads.iter().zip(&n.heads) {
                    assert_eq!(h.channels.iter().map(key).collect::<Vec<_>>(), k.channels.iter().map(key).collect::<Vec<_>>());
                }
            }
            assert_eq!(b.physical.moving, t.physical.moving, "{}", t.name);
            assert_eq!(b.images.keys().collect::<Vec<_>>(), t.images.keys().collect::<Vec<_>>());
            if t.physical.moving {
                assert_eq!((b.physical.pan_range, b.physical.tilt_range), (t.physical.pan_range, t.physical.tilt_range));
            }
        }
        // Colours survive the trip through CIE xy closely.
        let spot = &types[0];
        let cw = |t: &FixtureType| t.modes[0].channel(Attribute::ColorWheel).unwrap().ranges[1].color.unwrap();
        let back = import(&export(spot).unwrap()).unwrap().fixture;
        let (a, b) = (cw(spot), cw(&back));
        assert!(a.iter().zip(b).all(|(x, y)| (*x as i32 - y as i32).abs() <= 2), "{a:?} {b:?}");
    }

}
