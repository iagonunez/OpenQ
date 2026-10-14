//! Import fixture definitions in the Open Fixture Library JSON format
//! (<https://open-fixture-library.org>, fixture data under the MIT licence).
//!
//! Each channel's first capability type picks the [`Attribute`]; fine channels come from
//! `fineChannelAliases`. Matrix (template) channels and modes that use them are skipped, as
//! are channels whose function isn't known (they keep their slot, so addresses stay right).

use super::model::{Attribute, ChannelDef, FixtureMode, FixtureType, Physical};
use serde_json::Value;

/// What an import produced, with notes about what was left out.
#[derive(Debug)]
pub struct Imported {
    /// The fixture type.
    pub fixture: FixtureType,
    /// Things that didn't come across (shown to the user).
    pub notes: Vec<String>,
}

/// The attribute of an OFL channel from its capability type, its colour and its name.
fn attribute(name: &str, cap: &Value) -> Option<Attribute> {
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
        "ColorTemperature" => Attribute::Cto,
        "Pan" | "PanContinuous" => Attribute::Pan,
        "Tilt" | "TiltContinuous" => Attribute::Tilt,
        "PanTiltSpeed" => Attribute::PanTiltSpeed,
        "Zoom" => Attribute::Zoom,
        "Focus" => Attribute::Focus,
        "Iris" | "IrisEffect" => Attribute::Iris,
        "Frost" | "FrostEffect" => Attribute::Frost,
        "Prism" | "PrismRotation" => Attribute::Prism,
        "ShutterStrobe" | "StrobeSpeed" | "StrobeDuration" => Attribute::Shutter,
        "Fog" | "FogOutput" => Attribute::Fog,
        "EffectSpeed" | "Speed" => Attribute::EffectSpeed,
        "Effect" | "EffectDuration" | "EffectParameter" | "SoundSensitivity" => Attribute::Effect,
        "WheelSlot" | "WheelShake" | "WheelSlotRotation" | "WheelRotation" => {
            let wheel = cap.get("wheel").and_then(Value::as_str).unwrap_or(name).to_lowercase();
            let rotation = ty.contains("Rotation") || lname.contains("rot");
            if wheel.contains("color") || wheel.contains("colour") {
                Attribute::ColorWheel
            } else if wheel.contains("gobo") && rotation {
                Attribute::GoboRotation
            } else if wheel.contains("gobo") {
                Attribute::Gobo
            } else if wheel.contains("prism") {
                Attribute::Prism
            } else {
                Attribute::Effect
            }
        }
        // NoFunction, Generic, Maintenance, ColorPreset, blades, …: not modelled yet.
        _ => return None,
    })
}

/// A channel's first capability (single `capability` or the first of `capabilities`).
fn first_capability(ch: &Value) -> Option<&Value> {
    ch.get("capability").or_else(|| ch.get("capabilities").and_then(|c| c.get(0)))
}

/// "50%" or a DMX number → 0..=1.
fn level(v: &Value) -> Option<f32> {
    match v {
        Value::Number(n) => Some((n.as_f64()? / 255.0).clamp(0.0, 1.0) as f32),
        Value::String(s) => s.trim_end_matches('%').parse::<f32>().ok().map(|p| (p / 100.0).clamp(0.0, 1.0)),
        _ => None,
    }
}

/// Parse an OFL fixture file. `manufacturer` is used when the file doesn't name one (OFL keeps
/// it in the folder name).
pub fn import(json: &str, manufacturer: &str) -> anyhow::Result<Imported> {
    let v: Value = serde_json::from_str(json)?;
    let name = v.get("name").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("not an Open Fixture Library fixture (no name)"))?;
    let channels = v.get("availableChannels").and_then(Value::as_object).ok_or_else(|| anyhow::anyhow!("no availableChannels"))?;
    let manufacturer = v.get("manufacturer").and_then(Value::as_str).unwrap_or(manufacturer).to_string();
    let mut notes = Vec::new();
    // Fine aliases: alias → coarse channel.
    let mut fine_of = std::collections::HashMap::new();
    for (k, ch) in channels {
        for a in ch.get("fineChannelAliases").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            fine_of.insert(a.to_string(), k.clone());
        }
    }
    let mut modes = Vec::new();
    for m in v.get("modes").and_then(Value::as_array).into_iter().flatten() {
        let mname = m.get("name").and_then(Value::as_str).unwrap_or("Mode").to_string();
        let list = m.get("channels").and_then(Value::as_array).cloned().unwrap_or_default();
        if list.iter().any(|c| c.is_object()) {
            notes.push(format!("mode \"{mname}\": matrix channels are not supported, skipped"));
            continue;
        }
        let names: Vec<Option<&str>> = list.iter().map(Value::as_str).collect();
        let mut defs: Vec<ChannelDef> = Vec::new();
        for (offset, n) in names.iter().enumerate() {
            let Some(n) = n else { continue };
            if fine_of.contains_key(*n) {
                continue; // placed with its coarse channel below
            }
            let Some(ch) = channels.get(*n) else {
                notes.push(format!("mode \"{mname}\": unknown channel \"{n}\""));
                continue;
            };
            let Some(attr) = first_capability(ch).and_then(|c| attribute(n, c)) else {
                notes.push(format!("channel \"{n}\": function not supported, left unused"));
                continue;
            };
            if defs.iter().any(|d| d.attribute == attr) {
                notes.push(format!("mode \"{mname}\": \"{n}\" repeats {attr:?}, left unused"));
                continue;
            }
            // Its fine byte, if this mode has it.
            let fine = names.iter().position(|x| x.is_some_and(|x| fine_of.get(x).is_some_and(|c| c == n))).map(|p| p as u16);
            defs.push(ChannelDef {
                attribute: attr,
                offset: offset as u16,
                fine,
                default: ch.get("defaultValue").and_then(level),
                highlight: ch.get("highlightValue").and_then(level),
            });
        }
        if defs.is_empty() {
            notes.push(format!("mode \"{mname}\": nothing usable, skipped"));
            continue;
        }
        modes.push(FixtureMode { name: mname, channels: defs });
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
        pan_range: num("panMax").unwrap_or(d.pan_range),
        tilt_range: num("tiltMax").unwrap_or(d.tilt_range),
        beam_min: deg(0).unwrap_or(d.beam_min),
        beam_max: deg(1).or(deg(0)).unwrap_or(d.beam_max),
        moving: moving || modes.iter().any(|m| m.channel(Attribute::Pan).is_some() && m.channel(Attribute::Tilt).is_some()),
    };
    let key = |s: &str| s.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>();
    let id = format!("ofl/{}/{}", key(&manufacturer), key(name));
    Ok(Imported { fixture: FixtureType { id, manufacturer, name: name.to_string(), modes, physical }, notes })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(f.modes.len(), 2);
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
        assert!(r.notes.iter().any(|n| n.contains("Matrix")));
        assert!(r.notes.iter().any(|n| n.contains("Reset")));
    }

    #[test]
    fn rejects_other_json() {
        assert!(import("{}", "x").is_err());
        assert!(import("not json", "x").is_err());
    }
}
