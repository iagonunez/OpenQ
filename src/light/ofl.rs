//! Import fixture definitions in the Open Fixture Library JSON format
//! (<https://open-fixture-library.org>, fixture data under the MIT licence).
//!
//! * A channel's attribute comes from its first capability that OpnQ models; every
//!   capability of a channel with more than one becomes a named range (wheel slots take their
//!   name and colour from the fixture's `wheels`).
//! * Fine channels come from `fineChannelAliases`; ranges are scaled to the coarse byte.
//! * Matrix fixtures: `templateChannels` are expanded for every pixel (and pixel group) key and
//!   `matrixChannels` insert blocks are unrolled. Channels of one pixel become a cell
//!   ([`Head`]), channels of the "all pixels" group or no pixel stay master channels.
//! * Channels whose function isn't known keep their slot (addresses stay right) and are noted.

use super::import::{place, try_place, type_id, Imported};
use super::model::{Attribute, ChannelDef, ChannelRange, Feature, FixtureMode, FixtureType, Head, Physical};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// The attribute of an OFL channel from its capability type, its colour and its name.
fn attribute(name: &str, cap: &Value, wheels: Option<&Map<String, Value>>) -> Option<Attribute> {
    let ty = cap.get("type").and_then(Value::as_str).unwrap_or("");
    let lname = name.to_lowercase();
    Some(match ty {
        "Intensity" => Attribute::Dimmer,
        "ColorIntensity" => match cap.get("color").and_then(Value::as_str).unwrap_or("") {
            "Red" => Attribute::Red,
            "Green" => Attribute::Green,
            "Blue" => Attribute::Blue,
            "Amber" => Attribute::Amber,
            "Lime" => Attribute::Lime,
            "UV" => Attribute::Uv,
            "Cyan" => Attribute::Cyan,
            "Magenta" => Attribute::Magenta,
            "Yellow" => Attribute::Yellow,
            _ => Attribute::White, // White, Warm White, Cold White, Indigo…
        },
        "ColorPreset" => Attribute::ColorMacro,
        "ColorTemperature" => Attribute::Cto,
        "Pan" | "PanContinuous" => Attribute::Pan,
        "Tilt" | "TiltContinuous" => Attribute::Tilt,
        "PanTiltSpeed" => Attribute::PanTiltSpeed,
        "Zoom" | "BeamAngle" => Attribute::Zoom,
        "Focus" => Attribute::Focus,
        "Iris" | "IrisEffect" => Attribute::Iris,
        "Frost" | "FrostEffect" => Attribute::Frost,
        "Prism" => Attribute::Prism,
        "PrismRotation" => Attribute::PrismRotation,
        "ShutterStrobe" | "StrobeSpeed" | "StrobeDuration" => Attribute::Shutter,
        "Fog" | "FogOutput" | "FogType" => Attribute::Fog,
        "EffectSpeed" | "Speed" => Attribute::EffectSpeed,
        "Effect" | "EffectDuration" | "EffectParameter" | "SoundSensitivity" => Attribute::Effect,
        "Maintenance" | "Generic" => Attribute::Control,
        "Rotation" => Attribute::Effect,
        "WheelSlot" | "WheelShake" | "WheelSlotRotation" | "WheelRotation" => {
            let wheel = cap.get("wheel").and_then(Value::as_str).unwrap_or(name);
            let kind = wheel_kind(wheels.and_then(|w| w.get(wheel)));
            let lwheel = wheel.to_lowercase();
            let rotation = ty == "WheelSlotRotation" || (ty == "WheelRotation" && lname.contains("rot"));
            let colour = kind == Some("Color") || lwheel.contains("color") || lwheel.contains("colour");
            let gobo = kind == Some("Gobo") || lwheel.contains("gobo");
            if colour {
                Attribute::ColorWheel
            } else if gobo && rotation {
                Attribute::GoboRotation
            } else if gobo {
                Attribute::Gobo
            } else if kind == Some("Prism") || lwheel.contains("prism") {
                Attribute::Prism
            } else {
                Attribute::Effect
            }
        }
        // NoFunction, blades, …: not modelled.
        _ => return None,
    })
}

/// What a wheel mostly holds: "Color", "Gobo", "Prism" (the first such slot type).
fn wheel_kind(wheel: Option<&Value>) -> Option<&str> {
    wheel?
        .get("slots")?
        .as_array()?
        .iter()
        .filter_map(|s| s.get("type").and_then(Value::as_str))
        .find(|t| matches!(*t, "Color" | "Gobo" | "Prism" | "AnimationGoboStart"))
        .map(|t| if t == "AnimationGoboStart" { "Gobo" } else { t })
}

/// A channel's capabilities (the single `capability` or the list).
fn capabilities(ch: &Value) -> Vec<&Value> {
    match ch.get("capability") {
        Some(c) => vec![c],
        None => ch.get("capabilities").and_then(Value::as_array).map(|v| v.iter().collect()).unwrap_or_default(),
    }
}

/// "50%" or a DMX number (in `max` steps) → 0..=1.
fn level(v: &Value, max: f64) -> Option<f32> {
    match v {
        Value::Number(n) => Some((n.as_f64()? / max).clamp(0.0, 1.0) as f32),
        Value::String(s) => s.trim_end_matches('%').parse::<f32>().ok().map(|p| (p / 100.0).clamp(0.0, 1.0)),
        _ => None,
    }
}

/// A wheel slot (1-based `slot`) of a wheel.
fn wheel_slot<'a>(wheels: Option<&'a Map<String, Value>>, wheel: &str, slot: usize) -> Option<&'a Value> {
    wheels?.get(wheel)?.get("slots")?.as_array()?.get(slot.checked_sub(1)?)
}

/// The name a slot's `resource` goes by: the string itself, or an embedded resource's key.
fn resource_key(r: &Value) -> Option<String> {
    r.as_str().or_else(|| r.get("key")?.as_str()).or_else(|| r.get("name")?.as_str()).map(str::to_string)
}

/// Thumbnails of the resources embedded in the wheels (`"resource": {"key", "image": {"mimeType",
/// "data", "encoding": "utf8"|"base64"}}`, as the OFL website's own export writes them).
fn embedded_images(wheels: Option<&Map<String, Value>>, notes: &mut Vec<String>) -> std::collections::BTreeMap<String, String> {
    use base64::Engine;
    let mut out = std::collections::BTreeMap::new();
    let slots = wheels.into_iter().flat_map(|w| w.values()).filter_map(|w| w.get("slots")?.as_array()).flatten();
    for r in slots.filter_map(|s| s.get("resource")).filter(|r| r.is_object()) {
        let (Some(key), Some(img)) = (resource_key(r), r.get("image")) else { continue };
        if out.contains_key(&key) || out.len() >= super::gobo::MAX_IMAGES {
            continue;
        }
        let Some(data) = img.get("data").and_then(Value::as_str) else { continue };
        let bytes = match img.get("encoding").and_then(Value::as_str) {
            Some("base64") => base64::engine::general_purpose::STANDARD.decode(data.trim()).ok(),
            _ => Some(data.as_bytes().to_vec()),
        };
        match bytes.map(|b| super::gobo::thumbnail(&b)) {
            Some(Ok(t)) => {
                out.insert(key, t);
            }
            Some(Err(e)) => notes.push(format!("picture \"{key}\": {e:#}")),
            None => notes.push(format!("picture \"{key}\": bad base64")),
        }
    }
    out
}

/// Name (and colour) of a capability for its range.
fn range_label(cap: &Value, channel: &str, wheels: Option<&Map<String, Value>>) -> (String, Option<[u8; 3]>, String) {
    let s = |k: &str| cap.get(k).and_then(Value::as_str).map(str::to_string);
    let ty = s("type").unwrap_or_default();
    let mut colour = cap.get("colors").and_then(Value::as_array).and_then(|c| c.first()).and_then(Value::as_str).and_then(super::import::hex_colour);
    let mut image = String::new();
    let mut name = s("comment").filter(|c| !c.is_empty());
    if name.is_none() && (ty.starts_with("WheelSlot") || ty == "WheelShake") {
        let wheel = s("wheel").unwrap_or_else(|| channel.to_string());
        let start = cap.get("slotNumberStart").and_then(Value::as_f64);
        let end = cap.get("slotNumberEnd").and_then(Value::as_f64);
        if let (Some(a), Some(b), None) = (start, end, cap.get("slotNumber")) {
            let label = |k: f64| {
                let v = wheel_slot(wheels, &wheel, k as usize);
                let t = v.and_then(|v| v.get("type")).and_then(Value::as_str).unwrap_or("");
                v.and_then(|v| v.get("name")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| match t {
                    "" => k.to_string(),
                    "Color" | "Gobo" => format!("{t} {k}"),
                    t => t.to_string(),
                })
            };
            name = Some(format!("{} → {}", label(a), label(b)));
        }
        if let Some(n) = cap.get("slotNumber").and_then(Value::as_f64) {
            let slot_name = |k: usize| {
                let v = wheel_slot(wheels, &wheel, k)?;
                let t = v.get("type").and_then(Value::as_str).unwrap_or("");
                Some((v.get("name").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| if t == "Gobo" || t == "Color" { format!("{t} {k}") } else { t.to_string() }), v))
            };
            if n.fract() == 0.0 {
                if let Some((sn, v)) = slot_name(n as usize) {
                    colour = colour.or_else(|| v.get("colors").and_then(Value::as_array).and_then(|c| c.first()).and_then(Value::as_str).and_then(super::import::hex_colour));
                    image = v.get("resource").and_then(resource_key).unwrap_or_default();
                    name = Some(sn);
                } else {
                    name = Some(format!("Slot {n}"));
                }
            } else {
                let (a, b) = (n.floor() as usize, n.ceil() as usize);
                let label = |k| slot_name(k).map_or_else(|| k.to_string(), |x| x.0);
                name = Some(format!("{} / {}", label(a), label(b)));
            }
            if ty == "WheelShake" {
                name = name.map(|n| format!("{n} shake"));
            }
        }
    }
    let base = name
        .or_else(|| s("effectName"))
        .or_else(|| s("effectPreset"))
        .or_else(|| s("shutterEffect"))
        .unwrap_or_else(|| words(&ty));
    let speed = match (s("speedStart").or_else(|| s("speed")), s("speedEnd")) {
        (Some(a), Some(b)) if a != b => format!(" {a} → {b}"),
        (Some(a), _) => format!(" {a}"),
        _ => String::new(),
    };
    (format!("{base}{speed}"), colour, image)
}

/// "WheelRotation" → "Wheel rotation".
fn words(camel: &str) -> String {
    let mut out = String::new();
    for (i, c) in camel.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Pixel keys of a matrix in "XYZ" (reading) order, and group keys.
fn pixel_keys(matrix: Option<&Value>) -> (Vec<String>, Vec<(String, Value)>) {
    let Some(m) = matrix else { return (Vec::new(), Vec::new()) };
    let mut keys = Vec::new();
    if let Some(z_layers) = m.get("pixelKeys").and_then(Value::as_array) {
        // pixelKeys[z][y][x]
        for layer in z_layers.iter().filter_map(Value::as_array) {
            for row in layer.iter().filter_map(Value::as_array) {
                keys.extend(row.iter().filter_map(Value::as_str).map(str::to_string));
            }
        }
    } else if let Some(c) = m.get("pixelCount").and_then(Value::as_array) {
        let n = |i: usize| c.get(i).and_then(Value::as_u64).unwrap_or(1).clamp(1, 1024) as usize;
        let (x, y, z) = (n(0), n(1), n(2));
        let max = super::model::MAX_CELLS as usize;
        'all: for zi in 1..=z {
            for yi in 1..=y {
                for xi in 1..=x {
                    if keys.len() >= max {
                        break 'all;
                    }
                    keys.push(match (y, z) {
                        (1, 1) => xi.to_string(),
                        (_, 1) => format!("({xi}, {yi})"),
                        _ => format!("({xi}, {yi}, {zi})"),
                    });
                }
            }
        }
    }
    keys.truncate(super::model::MAX_CELLS as usize);
    let groups = m.get("pixelGroups").and_then(Value::as_object).map(|g| g.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
    (keys, groups)
}

/// Where a channel goes: the master channels or a cell (pixel key).
#[derive(Clone)]
struct ChannelRef<'a> {
    def: &'a Value,
    /// Pixel key (None: master).
    pixel: Option<String>,
}

/// Parse an OFL fixture file. `manufacturer` is used when the file doesn't name one (OFL keeps
/// it in the folder name).
pub fn import(json: &str, manufacturer: &str) -> anyhow::Result<Imported> {
    let v: Value = serde_json::from_str(json)?;
    let name = v.get("name").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("not an Open Fixture Library fixture (no name)"))?;
    let channels = v.get("availableChannels").and_then(Value::as_object);
    let templates = v.get("templateChannels").and_then(Value::as_object);
    if channels.is_none() && templates.is_none() {
        anyhow::bail!("no availableChannels");
    }
    let wheels = v.get("wheels").and_then(Value::as_object);
    let manufacturer = v.get("manufacturer").and_then(Value::as_str).unwrap_or(manufacturer).to_string();
    let mut notes = Vec::new();
    let (pixels, groups) = pixel_keys(v.get("matrix"));
    // Every channel by key (templates expanded), and fine alias → (coarse key, byte index).
    let mut all: HashMap<String, ChannelRef> = HashMap::new();
    let mut fine_of: HashMap<String, (String, usize)> = HashMap::new();
    for (k, ch) in channels.into_iter().flatten() {
        all.insert(k.clone(), ChannelRef { def: ch, pixel: None });
        for (i, a) in ch.get("fineChannelAliases").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).enumerate() {
            fine_of.insert(a.to_string(), (k.clone(), i + 1));
        }
    }
    // A pixel group of every pixel acts on the whole fixture: its channels are master channels.
    let whole = |g: &str| groups.iter().any(|(k, v)| k == g && v.as_str() == Some("all"));
    for (t, ch) in templates.into_iter().flatten() {
        for key in pixels.iter().chain(groups.iter().map(|g| &g.0)) {
            let k = t.replace("$pixelKey", key);
            let pixel = (!whole(key)).then(|| key.clone());
            all.insert(k.clone(), ChannelRef { def: ch, pixel });
            for (i, a) in ch.get("fineChannelAliases").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).enumerate() {
                fine_of.insert(a.replace("$pixelKey", key), (k.clone(), i + 1));
            }
        }
    }
    // Switching channels: an alias stands for the channel it switches to first (with the
    // defaults that's what the fixture does).
    let mut switch: HashMap<String, String> = HashMap::new();
    for ch in channels.into_iter().flat_map(|c| c.values()) {
        for cap in capabilities(ch) {
            for (alias, target) in cap.get("switchChannels").and_then(Value::as_object).into_iter().flatten() {
                if let Some(t) = target.as_str() {
                    switch.entry(alias.clone()).or_insert_with(|| t.to_string());
                }
            }
        }
    }
    let mut modes = Vec::new();
    for m in v.get("modes").and_then(Value::as_array).into_iter().flatten() {
        let mname = m.get("name").and_then(Value::as_str).unwrap_or("Mode").to_string();
        // Unroll matrix insert blocks.
        let mut names: Vec<Option<String>> = Vec::new();
        for c in m.get("channels").and_then(Value::as_array).into_iter().flatten() {
            match c {
                Value::String(s) => names.push(Some(switch.get(s).cloned().unwrap_or_else(|| s.clone()))),
                Value::Object(o) if o.get("insert").and_then(Value::as_str) == Some("matrixChannels") => {
                    let keys: Vec<String> = match o.get("repeatFor") {
                        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
                        Some(Value::String(s)) if s == "eachPixelGroup" => groups.iter().map(|g| g.0.clone()).collect(),
                        Some(Value::String(s)) if s == "eachPixelABC" => {
                            let mut k = pixels.clone();
                            k.sort();
                            k
                        }
                        _ => pixels.clone(), // eachPixelXYZ and the other orders: reading order
                    };
                    let tmpl: Vec<&str> = o.get("templateChannels").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    let per_channel = o.get("channelOrder").and_then(Value::as_str) == Some("perChannel");
                    // A mode can't have more than 512 slots: don't unroll more than that.
                    let room = 513usize.saturating_sub(names.len());
                    let pairs: Box<dyn Iterator<Item = (&str, &String)>> = if per_channel {
                        Box::new(tmpl.iter().flat_map(|t| keys.iter().map(move |k| (*t, k))))
                    } else {
                        Box::new(keys.iter().flat_map(|k| tmpl.iter().map(move |t| (*t, k))))
                    };
                    names.extend(pairs.take(room).map(|(t, k)| Some(t.replace("$pixelKey", k))));
                }
                _ => names.push(None),
            }
            if names.len() > 512 {
                break;
            }
        }
        if names.len() > 512 {
            notes.push(format!("mode \"{mname}\": more than 512 channels, cut at 512"));
            names.truncate(512);
        }
        let mut master: Vec<ChannelDef> = Vec::new();
        let mut heads: Vec<Head> = Vec::new();
        for (offset, n) in names.iter().enumerate() {
            let Some(n) = n else { continue };
            if fine_of.contains_key(n) {
                continue; // placed with its coarse channel below
            }
            let Some(r) = all.get(n) else {
                notes.push(format!("mode \"{mname}\": unknown channel \"{n}\""));
                continue;
            };
            let caps = capabilities(r.def);
            // Unknown functions become Extra channels (set by hand, named after the channel).
            let attr = caps.iter().find_map(|c| attribute(n, c, wheels)).unwrap_or(super::import::ANY_EXTRA);
            // Its fine bytes in this mode (16-bit only; a 24-bit byte is ignored).
            let fines: Vec<(usize, usize)> = names
                .iter()
                .enumerate()
                .filter_map(|(p, x)| x.as_ref().and_then(|x| fine_of.get(x)).filter(|(c, _)| c == n).map(|(_, i)| (p, *i)))
                .collect();
            let fine = fines.iter().find(|(_, i)| *i == 1).map(|(p, _)| *p as u16);
            // Values are given at the channel's full resolution unless it says otherwise.
            let declared = r.def.get("fineChannelAliases").and_then(Value::as_array).map_or(0, |a| a.len());
            let bits = match r.def.get("dmxValueResolution").and_then(Value::as_str) {
                Some("8bit") => 8,
                Some("16bit") => 16,
                Some("24bit") => 24,
                // Three bytes at most: more aliases would overflow the shift below.
                _ => 8 * (1 + declared.min(2) as u32),
            };
            let max = ((1u64 << bits) - 1) as f64;
            let shift = bits - 8;
            let mut ranges = Vec::new();
            if caps.len() > 1 {
                for c in &caps {
                    if c.get("menuClick").and_then(Value::as_str) == Some("hidden") {
                        continue;
                    }
                    let Some(d) = c.get("dmxRange").and_then(Value::as_array) else { continue };
                    let b = |i: usize| d.get(i).and_then(Value::as_u64).map(|x| (x >> shift).min(255) as u8);
                    let (Some(from), Some(to)) = (b(0), b(1)) else { continue };
                    let (name, color, image) = range_label(c, n, wheels);
                    ranges.push(ChannelRange { from, to, name, color, image });
                }
            }
            let def = ChannelDef {
                attribute: attr,
                name: n.clone(),
                dmx_break: 0,
                offset: offset as u16,
                fine,
                default: r.def.get("defaultValue").and_then(|x| level(x, max)),
                highlight: r.def.get("highlightValue").and_then(|x| level(x, max)),
                ranges,
            };
            let what = format!("mode \"{mname}\", channel \"{n}\"");
            match &r.pixel {
                None => {
                    // A repeated attribute (a second light source: MAC Aura's backlight) goes
                    // into a cell named after the channel's first word ("Aura").
                    if let Err(def) = try_place(&mut master, def) {
                        let source = matches!(def.attribute.feature(), Feature::Intensity | Feature::Colour) || def.attribute == Attribute::Shutter;
                        if !source {
                            place(&mut master, def, &what, &mut notes);
                            continue;
                        }
                        let cell = n.split_whitespace().next().unwrap_or(n).to_string();
                        let i = match heads.iter().position(|h| h.name == cell) {
                            Some(i) => i,
                            None => {
                                heads.push(Head { name: cell, channels: Vec::new() });
                                heads.len() - 1
                            }
                        };
                        place(&mut heads[i].channels, def, &what, &mut notes);
                    }
                }
                Some(px) => {
                    let i = match heads.iter().position(|h| &h.name == px) {
                        Some(i) => i,
                        None => {
                            heads.push(Head { name: px.clone(), channels: Vec::new() });
                            heads.len() - 1
                        }
                    };
                    place(&mut heads[i].channels, def, &what, &mut notes);
                }
            }
        }
        heads.retain(|h| !h.channels.is_empty());
        if master.is_empty() && heads.is_empty() {
            notes.push(format!("mode \"{mname}\": nothing usable, skipped"));
            continue;
        }
        modes.push(FixtureMode { name: mname, channels: master, heads, slots: names.len().min(512) as u16 });
    }
    if modes.is_empty() {
        anyhow::bail!("no usable modes");
    }
    let phys = v.get("physical");
    let lens = phys.and_then(|p| p.get("lens")).and_then(|l| l.get("degreesMinMax")).and_then(Value::as_array);
    let deg = |i: usize| lens.and_then(|l| l.get(i)).and_then(Value::as_f64).map(|x| x as f32);
    let focus = phys.and_then(|p| p.get("focus"));
    let moving = focus.and_then(|f| f.get("type")).and_then(Value::as_str) == Some("Head");
    let num = |k: &str| focus.and_then(|f| f.get(k)).and_then(Value::as_f64).map(|x| x as f32);
    let d = Physical::default();
    let physical = Physical {
        pan_range: num("panMax").or_else(|| capability_angle(&v, "Pan").map(f32::abs)).unwrap_or(d.pan_range),
        tilt_range: num("tiltMax").or_else(|| capability_angle(&v, "Tilt").map(f32::abs)).unwrap_or(d.tilt_range),
        beam_min: deg(0).unwrap_or(d.beam_min),
        beam_max: deg(1).or(deg(0)).unwrap_or(d.beam_max),
        moving: moving || modes.iter().any(|m| m.channel(Attribute::Pan).is_some() && m.channel(Attribute::Tilt).is_some()),
        emitters: Vec::new(),
        // OFL has no geometry, and its pan/tilt angles have no zero: centred on the DMX middle,
        // turning the other way when the capability's angles fall as the value rises.
        pan_angles: reversed(&v, "Pan"),
        tilt_angles: reversed(&v, "Tilt"),
        zoom_angles: None,
        parts: None,
    };
    let id = type_id("ofl", &manufacturer, name);
    let images = embedded_images(wheels, &mut notes);
    Ok(Imported { fixture: super::fixlib::sanitize(FixtureType { id, manufacturer, name: name.to_string(), modes, physical, images })?, notes })
}

/// The angle a `Pan`/`Tilt` capability covers (`angleStart`/`angleEnd` in degrees), for files
/// that give it there instead of in `physical.focus`. The widest one, if several.
/// Pan or tilt angles of a type whose capability turns the other way (`angleStart` > `angleEnd`):
/// centred on the DMX middle, falling. None otherwise.
fn reversed(v: &Value, kind: &str) -> Option<[f32; 2]> {
    let a = capability_angle(v, kind).filter(|a| *a < 0.0)?;
    Some([-a / 2.0, a / 2.0])
}

fn capability_angle(v: &Value, kind: &str) -> Option<f32> {
    let deg = |c: &Value, k: &str| c.get(k)?.as_str()?.trim().strip_suffix("deg")?.trim().parse::<f32>().ok();
    ["availableChannels", "templateChannels"]
        .iter()
        .filter_map(|k| v.get(*k)?.as_object())
        .flat_map(|m| m.values())
        .flat_map(|ch| ch.get("capability").into_iter().chain(ch.get("capabilities").and_then(Value::as_array).into_iter().flatten()))
        .filter(|c| c.get("type").and_then(Value::as_str) == Some(kind))
        .filter_map(|c| Some(deg(c, "angleEnd")? - deg(c, "angleStart")?))
        .filter(|a| a.is_finite() && *a != 0.0)
        .reduce(|a, b| if b.abs() > a.abs() { b } else { a })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pan_running_backwards_turns_the_other_way() {
        let cap = |start: &str, end: &str| serde_json::json!({"availableChannels": {"Pan": {"capability": {"type": "Pan", "angleStart": start, "angleEnd": end}}}});
        assert_eq!(reversed(&cap("540deg", "0deg"), "Pan"), Some([270.0, -270.0]));
        assert_eq!(reversed(&cap("0deg", "540deg"), "Pan"), None, "the usual way: centred, rising");
        assert_eq!(capability_angle(&cap("540deg", "0deg"), "Pan").map(f32::abs), Some(540.0));
    }

    const SPOT: &str = r#"{
      "name": "Test Spot 250",
      "physical": { "lens": { "degreesMinMax": [14, 30] }, "focus": { "type": "Head", "panMax": 540, "tiltMax": 265 } },
      "availableChannels": {
        "Pan": { "fineChannelAliases": ["Pan fine"], "capability": { "type": "Pan", "angleStart": "0deg", "angleEnd": "540deg" } },
        "Tilt": { "fineChannelAliases": ["Tilt fine"], "capability": { "type": "Tilt" } },
        "Dimmer": { "capability": { "type": "Intensity" } },
        "Shutter": { "defaultValue": 255, "capabilities": [ { "dmxRange": [0, 7], "type": "ShutterStrobe", "shutterEffect": "Closed" } ] },
        "Color Wheel": { "capabilities": [ { "dmxRange": [0, 9], "type": "WheelSlot", "slotNumber": 1 } ] },
        "Gobo Wheel Rotation": { "capabilities": [ { "dmxRange": [0, 255], "type": "WheelSlotRotation", "wheel": "Gobo Wheel" } ] },
        "Red": { "capability": { "type": "ColorIntensity", "color": "Red" } },
        "Reset": { "capability": { "type": "Maintenance" } }
      },
      "modes": [
        { "name": "Extended", "channels": ["Pan", "Pan fine", "Tilt", "Tilt fine", "Dimmer", "Shutter", "Color Wheel", "Gobo Wheel Rotation", "Reset", null, "Red"] },
        { "name": "Basic", "channels": ["Pan", "Tilt", "Dimmer"] },
        { "name": "Matrix", "channels": [ { "insert": "matrixChannels" } ] }
      ]
    }"#;

    #[test]
    fn spot() {
        let r = import(SPOT, "Acme").unwrap();
        let f = &r.fixture;
        assert_eq!(f.id, "ofl/acme/test-spot-250");
        assert_eq!(f.modes.len(), 2, "the matrix mode inserts nothing (no matrix): skipped");
        let m = &f.modes[0];
        let pan = m.channel(Attribute::Pan).unwrap();
        assert_eq!((pan.offset, pan.fine), (0, Some(1)));
        assert_eq!(m.channel(Attribute::Tilt).unwrap().fine, Some(3));
        assert_eq!(m.channel(Attribute::Shutter).unwrap().default, Some(1.0));
        assert_eq!(m.channel(Attribute::ColorWheel).unwrap().offset, 6);
        assert_eq!(m.channel(Attribute::GoboRotation).unwrap().offset, 7);
        assert_eq!(m.channel(Attribute::Red).unwrap().offset, 10, "unused and unknown channels keep their slots");
        assert_eq!(m.footprint(), 11);
        assert_eq!(f.modes[1].channel(Attribute::Pan).unwrap().fine, None, "no fine byte in this mode");
        assert!(f.physical.moving);
        assert_eq!((f.physical.tilt_range, f.physical.beam_min, f.physical.beam_max), (265.0, 14.0, 30.0));
        assert_eq!(m.channel(Attribute::Control).unwrap().offset, 8, "maintenance channels are Control");
        // Without physical.focus the ranges come from the Pan/Tilt capabilities' angles.
        let bare = SPOT.replace(r#", "focus": { "type": "Head", "panMax": 540, "tiltMax": 265 }"#, "").replace(r#""type": "Pan", "angleStart": "0deg", "angleEnd": "540deg""#, r#""type": "Pan", "angleStart": "-270deg", "angleEnd": "180deg""#);
        assert_ne!(bare, SPOT);
        let p = import(&bare, "Acme").unwrap().fixture.physical;
        assert_eq!(p.pan_range, 450.0);
        assert_eq!(p.tilt_range, Physical::default().tilt_range, "no tilt angle in the file");
    }

    const WHEELS: &str = r##"{
      "name": "Wheel Spot",
      "wheels": {
        "Color Wheel": { "slots": [ { "type": "Open" }, { "type": "Color", "name": "Deep Red", "colors": ["#ff0000"] }, { "type": "Color", "colors": ["#0000ff"] } ] },
        "Gobo Wheel": { "slots": [ { "type": "Open" }, { "type": "Gobo", "name": "Star", "resource": "gobos/star" } ] },
        "Gobo Wheel 2": { "slots": [ { "type": "Open" }, { "type": "Gobo" } ] }
      },
      "availableChannels": {
        "Color Wheel": { "capabilities": [
          { "dmxRange": [0, 9], "type": "WheelSlot", "slotNumber": 1 },
          { "dmxRange": [10, 19], "type": "WheelSlot", "slotNumber": 2 },
          { "dmxRange": [20, 29], "type": "WheelSlot", "slotNumber": 2.5 },
          { "dmxRange": [30, 39], "type": "WheelSlot", "slotNumber": 3 },
          { "dmxRange": [40, 255], "type": "WheelRotation", "speedStart": "slow CW", "speedEnd": "fast CW" }
        ] },
        "Gobo Wheel": { "capabilities": [
          { "dmxRange": [0, 63], "type": "WheelSlot", "slotNumber": 1 },
          { "dmxRange": [64, 127], "type": "WheelSlot", "slotNumber": 2 },
          { "dmxRange": [128, 255], "type": "WheelShake", "slotNumber": 2, "menuClick": "hidden" }
        ] },
        "Gobo Wheel 2": { "capabilities": [
          { "dmxRange": [0, 127], "type": "WheelSlot", "slotNumber": 1 },
          { "dmxRange": [128, 255], "type": "WheelSlot", "slotNumber": 2 }
        ] },
        "Dimmer": { "fineChannelAliases": ["Dimmer fine"], "defaultValue": 32768, "capabilities": [
          { "dmxRange": [0, 65279], "type": "Intensity" },
          { "dmxRange": [65280, 65535], "type": "Intensity", "comment": "Full" }
        ] }
      },
      "modes": [ { "name": "Std", "channels": ["Color Wheel", "Gobo Wheel", "Gobo Wheel 2", "Dimmer", "Dimmer fine", null] } ]
    }"##;

    #[test]
    fn ranges_and_wheels() {
        let f = import(WHEELS, "Acme").unwrap().fixture;
        let m = &f.modes[0];
        let cw = m.channel(Attribute::ColorWheel).unwrap();
        let names: Vec<&str> = cw.ranges.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Open", "Deep Red", "Deep Red / Color 3", "Color 3", "Wheel rotation slow CW → fast CW"]);
        assert_eq!(cw.ranges[1].color, Some([255, 0, 0]));
        assert_eq!(cw.ranges[3].color, Some([0, 0, 255]));
        assert_eq!(cw.range_at(0.06).unwrap().name, "Deep Red", "0.06 → DMX 15");
        assert_eq!(cw.find_range("deep").unwrap().from, 10, "prefix: the shortest name that fits");
        assert_eq!(cw.find_range("2").unwrap().name, "Deep Red", "1-based index");
        let gobo = m.channel(Attribute::Gobo).unwrap();
        assert_eq!(gobo.ranges.len(), 2, "hidden capability left out");
        assert_eq!(gobo.ranges[1].image, "gobos/star");
        assert!(f.images.is_empty(), "a resource name only: no picture");
        // The website's export embeds the resource with its picture.
        let svg = r#"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='4'><rect width='4' height='4' fill='white'/></svg>"#;
        let embedded = WHEELS.replace(
            r#""resource": "gobos/star""#,
            &format!(r#""resource": {{"key": "gobos/star", "name": "Star", "image": {{"mimeType": "image/svg+xml", "data": "{svg}", "encoding": "utf8"}}}}"#),
        );
        let f2 = import(&embedded, "Acme").unwrap().fixture;
        assert_eq!(f2.modes[0].channel(Attribute::Gobo).unwrap().ranges[1].image, "gobos/star");
        let (w, _, px) = crate::light::gobo::decode(&f2.images["gobos/star"]).unwrap();
        assert_eq!((w, px[((32 * w + 32) * 4) as usize]), (crate::light::gobo::THUMB, 255));
        let g2 = m.channel(Attribute::Gobo2).unwrap();
        assert_eq!((g2.offset, g2.ranges[1].name.as_str()), (2, "Gobo 2"));
        let dim = m.channel(Attribute::Dimmer).unwrap();
        assert_eq!(dim.fine, Some(4));
        assert_eq!((dim.ranges[1].from, dim.ranges[1].name.as_str()), (255, "Full"), "16-bit ranges scaled to the coarse byte");
        assert!((dim.default.unwrap() - 0.5).abs() < 0.001);
        assert_eq!(m.footprint(), 6, "the unused last slot counts");
    }

    const BAR: &str = r#"{
      "name": "Pixel Bar",
      "matrix": { "pixelCount": [3, 1, 1], "pixelGroups": { "Master": "all" } },
      "availableChannels": { "Strobe": { "capability": { "type": "ShutterStrobe", "shutterEffect": "Strobe" } } },
      "templateChannels": {
        "Dimmer $pixelKey": { "capability": { "type": "Intensity" } },
        "Red $pixelKey": { "fineChannelAliases": ["Red $pixelKey fine"], "capability": { "type": "ColorIntensity", "color": "Red" } },
        "Green $pixelKey": { "capability": { "type": "ColorIntensity", "color": "Green" } }
      },
      "modes": [
        { "name": "Per pixel", "channels": ["Dimmer Master", "Strobe",
          { "insert": "matrixChannels", "repeatFor": "eachPixelXYZ", "channelOrder": "perPixel", "templateChannels": ["Red $pixelKey", "Red $pixelKey fine", "Green $pixelKey"] } ] },
        { "name": "Per channel", "channels": [
          { "insert": "matrixChannels", "repeatFor": ["3", "1"], "channelOrder": "perChannel", "templateChannels": ["Red $pixelKey", "Green $pixelKey"] } ] }
      ]
    }"#;

    #[test]
    fn matrix_modes_become_cells() {
        let r = import(BAR, "Acme").unwrap();
        let f = &r.fixture;
        assert_eq!(f.modes.len(), 2, "{:?}", r.notes);
        let m = &f.modes[0];
        assert_eq!(m.channels.iter().map(|c| c.attribute).collect::<Vec<_>>(), [Attribute::Dimmer, Attribute::Shutter]);
        assert_eq!(m.heads.len(), 3);
        assert_eq!(m.heads[1].name, "2");
        let red = m.heads[1].channels.iter().find(|c| c.attribute == Attribute::Red).unwrap();
        assert_eq!((red.offset, red.fine), (5, Some(6)));
        assert_eq!(m.footprint(), 11);
        let m = &f.modes[1];
        assert_eq!(m.heads.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["3", "1"]);
        let offs = |h: &Head| h.channels.iter().map(|c| c.offset).collect::<Vec<_>>();
        assert_eq!(offs(&m.heads[0]), [0, 2], "per channel: reds first, then greens");
        assert_eq!(offs(&m.heads[1]), [1, 3]);
    }

    #[test]
    fn rejects_other_json() {
        assert!(import("{}", "x").is_err());
        assert!(import("not json", "x").is_err());
    }
}
