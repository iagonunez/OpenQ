//! Import QLC+ fixture definitions (`.qxf`, XML; the QLC+ fixture library is Apache-2.0).
//!
//! * A channel's attribute comes from its `Preset` (QLC+ 4.12 and later) or from its `Group`,
//!   `Colour` and name (older files). Fine channels (`…Fine` presets, `Byte="1"`) become the
//!   fine byte of the coarse channel with the same attribute in the same unit.
//! * Capabilities become named ranges, with their colour (`Res1`/`Color`) and gobo image
//!   (`Res`/`Res1`) when given.
//! * `Head`s of a mode are cells; channels outside every head are master channels.

use super::import::{attribute_from_name, hex_colour, place, type_id, Imported};
use super::model::{Attribute, ChannelDef, ChannelRange, Feature, FixtureMode, FixtureType, Head, Physical};
use roxmltree::{Document, Node};
use std::collections::HashMap;

/// A channel definition: attribute, fine byte, default, ranges.
struct Def {
    attr: Option<Attribute>,
    fine: bool,
    default: Option<f32>,
    ranges: Vec<ChannelRange>,
}

fn text<'a>(n: Node<'a, '_>) -> &'a str {
    n.text().unwrap_or("").trim()
}

fn child<'a, 'i>(n: Node<'a, 'i>, tag: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(tag))
}

/// Attribute of a QLC+ channel preset name (None: no function); the bool is "fine byte".
fn from_preset(p: &str) -> Option<(Option<Attribute>, bool)> {
    use Attribute::*;
    let fine = p.ends_with("Fine");
    let base = p.trim_end_matches("Fine");
    let a = match base {
        "IntensityMasterDimmer" | "IntensityDimmer" => Some(Dimmer),
        "IntensityRed" => Some(Red),
        "IntensityGreen" => Some(Green),
        "IntensityBlue" => Some(Blue),
        "IntensityCyan" => Some(Cyan),
        "IntensityMagenta" => Some(Magenta),
        "IntensityYellow" => Some(Yellow),
        "IntensityAmber" => Some(Amber),
        "IntensityWhite" => Some(White),
        "IntensityUV" | "IntensityIndigo" => Some(Uv),
        "IntensityLime" => Some(Lime),
        "PositionPan" => Some(Pan),
        "PositionTilt" => Some(Tilt),
        p if p.starts_with("SpeedPan") || p.starts_with("SpeedTilt") => Some(PanTiltSpeed),
        "ColorMacro" | "ColorRGBMixer" => Some(ColorMacro),
        "ColorWheel" => Some(ColorWheel),
        "ColorCTOMixer" | "ColorCTCMixer" | "ColorCTBMixer" => Some(Cto),
        "GoboWheel" => Some(Gobo),
        "GoboIndex" => Some(GoboRotation),
        p if p.starts_with("ShutterStrobe") => Some(Shutter),
        p if p.starts_with("ShutterIris") => Some(Iris),
        p if p.starts_with("BeamFocus") => Some(Focus),
        p if p.starts_with("BeamZoom") => Some(Zoom),
        p if p.starts_with("PrismRotation") => Some(PrismRotation),
        "NoFunction" => None,
        // Hue/saturation/XY axes and the rest: not modelled.
        _ => return Some((None, fine)),
    };
    Some((a, fine))
}

/// Attribute from an old-style `Group` (+ `Colour` and the channel name).
fn from_group(group: &str, colour: &str, name: &str) -> Option<Attribute> {
    use Attribute::*;
    let n = name.to_lowercase();
    match group {
        "Intensity" => Some(match colour {
            "Red" => Red,
            "Green" => Green,
            "Blue" => Blue,
            "Cyan" => Cyan,
            "Magenta" => Magenta,
            "Yellow" => Yellow,
            "White" => White,
            "Amber" => Amber,
            "UV" | "Indigo" => Uv,
            "Lime" => Lime,
            _ => Dimmer,
        }),
        "Colour" => Some(if n.contains("macro") {
            ColorMacro
        } else if n.contains("cto") || n.contains("ctc") || n.contains("temp") {
            Cto
        } else {
            ColorWheel
        }),
        "Gobo" => Some(if n.contains("rot") || n.contains("index") { GoboRotation } else { Gobo }),
        "Pan" => Some(Pan),
        "Tilt" => Some(Tilt),
        "Prism" => Some(if n.contains("rot") { PrismRotation } else { Prism }),
        "Shutter" => Some(if n.contains("iris") { Iris } else { Shutter }),
        "Beam" => attribute_from_name(name).or(Some(Zoom)),
        "Speed" => Some(if n.contains("pan") || n.contains("tilt") { PanTiltSpeed } else { EffectSpeed }),
        // The name says more than these groups ("Frost" filed under Effect).
        "Effect" => attribute_from_name(name).or(Some(Effect)),
        "Maintenance" => attribute_from_name(name).filter(|a| a.feature() == Feature::Control).or(Some(Control)),
        "Nothing" => None,
        _ => attribute_from_name(name),
    }
}

fn channel_def(c: Node) -> Def {
    let name = c.attribute("Name").unwrap_or("");
    let group = child(c, "Group");
    let byte_fine = group.and_then(|g| g.attribute("Byte")) == Some("1");
    let (attr, fine) = match c.attribute("Preset").and_then(from_preset) {
        Some((a, f)) => (a, f),
        None => (
            from_group(group.map(text).unwrap_or(""), child(c, "Colour").map(text).unwrap_or(""), name),
            byte_fine || name.to_lowercase().ends_with("fine"),
        ),
    };
    let mut ranges = Vec::new();
    let caps: Vec<Node> = c.children().filter(|x| x.has_tag_name("Capability")).collect();
    if caps.len() > 1 {
        for k in caps {
            let num = |a: &str| k.attribute(a).and_then(|v| v.trim().parse::<u32>().ok()).map(|v| v.min(255) as u8);
            let (Some(from), Some(to)) = (num("Min"), num("Max")) else { continue };
            let res1 = k.attribute("Res1").unwrap_or("");
            let color = k.attribute("Color").or(Some(res1)).and_then(hex_colour);
            let image = k.attribute("Res").filter(|r| !r.is_empty()).or((color.is_none() && !res1.is_empty()).then_some(res1)).unwrap_or("").to_string();
            ranges.push(ChannelRange { from, to, name: text(k).to_string(), color, image });
        }
    }
    let default = c.attribute("Default").and_then(|v| v.trim().parse::<f32>().ok()).filter(|v| *v > 0.0).map(|v| (v / 255.0).clamp(0.0, 1.0));
    Def { attr, fine, default, ranges }
}

/// Pan/tilt range and beam angles from a `Physical` element.
fn physical(p: Option<Node>, into: &mut Physical) {
    let Some(p) = p else { return };
    let num = |n: Option<Node>, a: &str| n.and_then(|n| n.attribute(a)).and_then(|v| v.parse::<f32>().ok());
    let focus = child(p, "Focus");
    let lens = child(p, "Lens");
    if let Some(v) = num(focus, "PanMax").filter(|v| *v > 0.0) {
        into.pan_range = v;
    }
    if let Some(v) = num(focus, "TiltMax").filter(|v| *v > 0.0) {
        into.tilt_range = v;
    }
    if let Some(v) = num(lens, "DegreesMin").filter(|v| *v > 0.0) {
        into.beam_min = v;
        into.beam_max = into.beam_max.max(v);
    }
    if let Some(v) = num(lens, "DegreesMax").filter(|v| *v > 0.0) {
        into.beam_max = v;
    }
    if focus.and_then(|f| f.attribute("Type")) == Some("Head") {
        into.moving = true;
    }
}

/// Read a `.qxf` file.
pub fn import(text_: &str) -> anyhow::Result<Imported> {
    let text_ = super::import::strip_doctype(text_)?;
    let doc = Document::parse_with_options(&text_, roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: 2_000_000, ..Default::default() })
        .map_err(|e| anyhow::anyhow!("not a QLC+ fixture: {e}"))?;
    let root = doc.root_element();
    if !root.has_tag_name("FixtureDefinition") {
        anyhow::bail!("not a QLC+ fixture definition");
    }
    let manufacturer = child(root, "Manufacturer").map(text).unwrap_or("").to_string();
    let name = child(root, "Model").map(text).filter(|s| !s.is_empty()).unwrap_or("Fixture").to_string();
    let mut notes = Vec::new();
    let defs: HashMap<&str, Def> = root.children().filter(|c| c.has_tag_name("Channel")).filter_map(|c| Some((c.attribute("Name")?, channel_def(c)))).collect();
    let mut phys = Physical::default();
    physical(child(root, "Physical"), &mut phys);
    let mut modes = Vec::new();
    for m in root.children().filter(|c| c.has_tag_name("Mode")) {
        let mname = m.attribute("Name").unwrap_or("Mode").to_string();
        physical(child(m, "Physical"), &mut phys);
        // Channel number → name.
        let chans: Vec<(u16, &str)> = m
            .children()
            .filter(|c| c.has_tag_name("Channel"))
            .filter_map(|c| Some((c.attribute("Number")?.trim().parse::<u16>().ok().filter(|n| *n < 512)?, text(c))))
            .collect();
        // Channel number → head index.
        let mut head_of: HashMap<u16, usize> = HashMap::new();
        let head_nodes: Vec<Node> = m.children().filter(|c| c.has_tag_name("Head")).collect();
        for (i, h) in head_nodes.iter().enumerate() {
            for c in h.children().filter(|c| c.has_tag_name("Channel")) {
                if let Ok(n) = text(c).parse::<u16>() {
                    head_of.insert(n, i);
                }
            }
        }
        let mut master: Vec<ChannelDef> = Vec::new();
        let mut heads: Vec<Head> = (1..=head_nodes.len()).map(|i| Head { name: i.to_string(), channels: Vec::new() }).collect();
        // Coarse channels first, then fine bytes onto them.
        let mut fines = Vec::new();
        for (num, cname) in &chans {
            let Some(d) = defs.get(cname) else {
                notes.push(format!("mode \"{mname}\": unknown channel \"{cname}\""));
                continue;
            };
            // Unknown functions become Extra channels (set by hand, named after the channel); a
            // fine byte of one is left out.
            let a = match d.attr {
                Some(a) => a,
                None if !d.fine && !cname.is_empty() => super::import::ANY_EXTRA,
                None => {
                    if !cname.is_empty() {
                        notes.push(format!("channel \"{cname}\": function not supported, left unused"));
                    }
                    continue;
                }
            };
            if d.fine {
                fines.push((*num, a, *cname));
                continue;
            }
            let def = ChannelDef { attribute: a, name: cname.to_string(), dmx_break: 0, offset: *num, fine: None, default: d.default, highlight: None, ranges: d.ranges.clone() };
            let what = format!("mode \"{mname}\", channel \"{cname}\"");
            match head_of.get(num) {
                Some(i) => place(&mut heads[*i].channels, def, &what, &mut notes),
                None => place(&mut master, def, &what, &mut notes),
            };
        }
        for (num, a, cname) in fines {
            let unit = match head_of.get(&num) {
                Some(i) => &mut heads[*i].channels,
                None => &mut master,
            };
            match unit.iter_mut().find(|c| c.attribute == a && c.fine.is_none()) {
                Some(c) => c.fine = Some(num),
                None => notes.push(format!("mode \"{mname}\": fine channel \"{cname}\" has no coarse channel, left unused")),
            }
        }
        heads.retain(|h| !h.channels.is_empty());
        heads.truncate(super::model::MAX_CELLS as usize);
        if master.is_empty() && heads.is_empty() {
            notes.push(format!("mode \"{mname}\": nothing usable, skipped"));
            continue;
        }
        let slots = chans.iter().map(|c| c.0 + 1).max().unwrap_or(0);
        modes.push(FixtureMode { name: mname, channels: master, heads, slots });
    }
    if modes.is_empty() {
        anyhow::bail!("no usable modes");
    }
    phys.moving |= modes.iter().any(|m| m.channel(Attribute::Pan).is_some() && m.channel(Attribute::Tilt).is_some());
    // Gobo pictures are files of QLC+'s own gobo folder: only their names come across.
    let fixture = FixtureType { id: type_id("qlc", &manufacturer, &name), manufacturer, name, modes, physical: phys, images: Default::default() };
    Ok(Imported { fixture: super::fixlib::sanitize(fixture)?, notes })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAR: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE FixtureDefinition>
<!-- comment -->
<FixtureDefinition xmlns="http://www.qlcplus.org/FixtureDefinition">
 <Manufacturer>Acme</Manufacturer>
 <Model>Bar 4</Model>
 <Type>LED Bar (Pixels)</Type>
 <Channel Name="Dimmer" Preset="IntensityMasterDimmer"/>
 <Channel Name="Dimmer fine" Preset="IntensityMasterDimmerFine"/>
 <Channel Name="Strobe">
  <Group Byte="0">Shutter</Group>
  <Capability Min="0" Max="9">Open</Capability>
  <Capability Min="10" Max="255">Strobe slow to fast</Capability>
 </Channel>
 <Channel Name="Macro" Preset="ColorMacro">
  <Capability Min="0" Max="127" Preset="ColorMacro" Res1="#ff0000">Red</Capability>
  <Capability Min="128" Max="255" Preset="ColorMacro" Res1="#0000ff">Blue</Capability>
 </Channel>
 <Channel Name="Red"><Group Byte="0">Intensity</Group><Colour>Red</Colour></Channel>
 <Channel Name="Green" Preset="IntensityGreen"/>
 <Channel Name="Blue" Preset="IntensityBlue"/>
 <Channel Name="Hue" Preset="IntensityHue"/>
 <Mode Name="Pixel">
  <Physical><Lens DegreesMin="20" DegreesMax="20"/></Physical>
  <Channel Number="0">Dimmer</Channel>
  <Channel Number="1">Dimmer fine</Channel>
  <Channel Number="2">Strobe</Channel>
  <Channel Number="3">Macro</Channel>
  <Channel Number="4">Red</Channel>
  <Channel Number="5">Green</Channel>
  <Channel Number="6">Blue</Channel>
  <Channel Number="7">Red</Channel>
  <Channel Number="8">Green</Channel>
  <Channel Number="9">Blue</Channel>
  <Channel Number="10">Hue</Channel>
  <Head><Channel>4</Channel><Channel>5</Channel><Channel>6</Channel></Head>
  <Head><Channel>7</Channel><Channel>8</Channel><Channel>9</Channel></Head>
 </Mode>
</FixtureDefinition>"##;

    #[test]
    fn bar_with_heads() {
        let r = import(BAR).unwrap();
        let f = &r.fixture;
        assert_eq!((f.id.as_str(), f.name.as_str()), ("qlc/acme/bar-4", "Bar 4"));
        let m = &f.modes[0];
        let dim = m.channel(Attribute::Dimmer).unwrap();
        assert_eq!((dim.offset, dim.fine), (0, Some(1)));
        let strobe = m.channel(Attribute::Shutter).unwrap();
        assert_eq!(strobe.ranges.len(), 2);
        assert_eq!(strobe.ranges[1].name, "Strobe slow to fast");
        let mac = m.channel(Attribute::ColorMacro).unwrap();
        assert_eq!(mac.ranges[1].color, Some([0, 0, 255]));
        assert_eq!(m.heads.len(), 2);
        assert_eq!(m.heads[1].channels.iter().map(|c| c.offset).collect::<Vec<_>>(), [7, 8, 9]);
        assert_eq!(m.heads[0].channels[0].attribute, Attribute::Red);
        assert_eq!(f.physical.beam_max, 20.0);
        // The Hue channel (no OpnQ attribute) becomes an extra channel under its own name.
        let hue = m.channel(Attribute::Extra(1)).unwrap();
        assert_eq!((hue.offset, hue.name.as_str()), (10, "Hue"));
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        assert_eq!(m.footprint(), 11);
    }

    #[test]
    fn rejects_other_files() {
        assert!(import("<Other/>").is_err());
        assert!(import("not xml").is_err());
    }
}
