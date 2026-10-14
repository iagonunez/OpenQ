//! Built-in generic fixture types (dimmer, RGB family, moving heads, strobe, fog).
//!
//! Real fixtures will come from importers (Open Fixture Library, GDTF); these cover most
//! rigs until then and keep tests independent of files.

use super::model::{Attribute as A, FixtureMode, FixtureType, Physical};

fn simple(id: &str, name: &str, modes: Vec<FixtureMode>, physical: Physical) -> FixtureType {
    FixtureType { id: id.into(), manufacturer: "Generic".into(), name: name.into(), modes, physical }
}

fn mode(name: &str, ch: &[(A, bool)]) -> FixtureMode {
    FixtureMode::sequential(name, ch)
}

/// Every built-in type.
pub fn generics() -> Vec<FixtureType> {
    let fixed = Physical { moving: false, ..Default::default() };
    let moving = Physical { moving: true, beam_min: 12.0, beam_max: 40.0, ..Default::default() };
    let f = false;
    let t = true;
    vec![
        simple("generic/dimmer", "Dimmer", vec![mode("1 channel", &[(A::Dimmer, f)]), mode("16 bit", &[(A::Dimmer, t)])], fixed.clone()),
        simple("generic/rgb", "RGB", vec![mode("3 channel", &[(A::Red, f), (A::Green, f), (A::Blue, f)])], fixed.clone()),
        simple(
            "generic/drgb",
            "Dimmer + RGB",
            vec![mode("4 channel", &[(A::Dimmer, f), (A::Red, f), (A::Green, f), (A::Blue, f)])],
            fixed.clone(),
        ),
        simple("generic/rgbw", "RGBW", vec![mode("4 channel", &[(A::Red, f), (A::Green, f), (A::Blue, f), (A::White, f)])], fixed.clone()),
        simple(
            "generic/drgbw",
            "Dimmer + RGBW",
            vec![mode("5 channel", &[(A::Dimmer, f), (A::Red, f), (A::Green, f), (A::Blue, f), (A::White, f)])],
            fixed.clone(),
        ),
        simple(
            "generic/rgbaw-uv",
            "RGBAW+UV",
            vec![mode("6 channel", &[(A::Red, f), (A::Green, f), (A::Blue, f), (A::Amber, f), (A::White, f), (A::Uv, f)])],
            fixed.clone(),
        ),
        simple(
            "generic/cmy",
            "CMY",
            vec![mode("4 channel", &[(A::Dimmer, f), (A::Cyan, f), (A::Magenta, f), (A::Yellow, f)])],
            fixed.clone(),
        ),
        simple(
            "generic/spot",
            "Moving head spot",
            vec![mode(
                "16 channel",
                &[
                    (A::Pan, t),
                    (A::Tilt, t),
                    (A::PanTiltSpeed, f),
                    (A::Dimmer, f),
                    (A::Shutter, f),
                    (A::ColorWheel, f),
                    (A::Gobo, f),
                    (A::GoboRotation, f),
                    (A::Prism, f),
                    (A::Focus, f),
                    (A::Zoom, f),
                    (A::Iris, f),
                    (A::Control, f),
                ],
            )],
            moving.clone(),
        ),
        simple(
            "generic/wash",
            "Moving head wash",
            vec![mode(
                "14 channel",
                &[
                    (A::Pan, t),
                    (A::Tilt, t),
                    (A::PanTiltSpeed, f),
                    (A::Dimmer, f),
                    (A::Shutter, f),
                    (A::Red, f),
                    (A::Green, f),
                    (A::Blue, f),
                    (A::White, f),
                    (A::Zoom, f),
                    (A::Control, f),
                    (A::Effect, f),
                ],
            )],
            moving,
        ),
        simple("generic/strobe", "Strobe", vec![mode("2 channel", &[(A::Dimmer, f), (A::Shutter, f)])], fixed.clone()),
        simple("generic/fog", "Fog / haze", vec![mode("2 channel", &[(A::Fog, f), (A::Fan, f)])], fixed),
    ]
}

/// A built-in type by id.
pub fn generic(id: &str) -> Option<FixtureType> {
    generics().into_iter().find(|t| t.id == id)
}

/// A test/benchmark rig: `fixtures` fixtures (a mix of dimmers, RGBW washes and spots) patched
/// from universe 1 on, and `sequences` sequences of `cues` cues that each set every fixture.
pub fn bench_rig(fixtures: u32, sequences: u32, cues: u32) -> super::Lighting {
    use super::model::{Fixture, LightCue, Sequence};
    let mut l = super::Lighting::default();
    let types: Vec<FixtureType> = ["generic/dimmer", "generic/drgbw", "generic/spot"].iter().filter_map(|id| generic(id)).collect();
    for t in &types {
        l.add_type(t);
    }
    let (mut universe, mut address) = (1u16, 1u16);
    for i in 0..fixtures {
        let t = &types[i as usize % types.len()];
        let n = t.modes[0].footprint();
        if address + n - 1 > 512 {
            universe += 1;
            address = 1;
        }
        l.fixtures.push(Fixture { id: i + 1, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), universe, address, ..Default::default() });
        address += n;
    }
    for s in 1..=sequences {
        let cues = (0..cues)
            .map(|c| {
                let mut cue = LightCue { number: (c + 1).to_string(), fade: 10.0, ..Default::default() };
                for f in &l.fixtures {
                    let attrs = l.mode_of(f).map(|m| m.attributes()).unwrap_or_default();
                    let v = cue.values.entry(f.id).or_default();
                    for (k, a) in attrs.into_iter().enumerate() {
                        v.insert(a, (((f.id + c * 7 + s * 13 + k as u32) % 100) as f32 / 100.0).into());
                    }
                }
                cue
            })
            .collect();
        l.sequences.push(Sequence { id: s, name: String::new(), cues });
    }
    l
}

#[cfg(test)]
mod tests {
    #[test]
    fn generics_are_consistent() {
        let all = super::generics();
        let mut ids: Vec<_> = all.iter().map(|t| t.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), all.len(), "duplicate ids");
        for t in &all {
            for m in &t.modes {
                // No two channels share a slot.
                let mut slots: Vec<u16> = m.channels.iter().flat_map(|c| std::iter::once(c.offset).chain(c.fine)).collect();
                let n = slots.len();
                slots.sort();
                slots.dedup();
                assert_eq!(slots.len(), n, "{} {}", t.id, m.name);
                assert_eq!(m.footprint() as usize, n, "{} {}", t.id, m.name);
            }
        }
    }
}
