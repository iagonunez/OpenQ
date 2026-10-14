//! Lighting effects: a waveform applied to one attribute (or pan + tilt for circles) of a list
//! of fixtures, each fixture offset in phase so the effect runs across the rig.
//!
//! An [`Effect`] lives in the effects pool; a cue (or the programmer) runs it on fixtures with an
//! [`EffectRun`]. Running effects track through later cues of the sequence until a cue stops them.
//! Playback compiles them to [`CompiledEffect`]s (slots + phases) and evaluates them every frame
//! without allocating.

use super::model::{Attribute, FixtureId};
use serde::{Deserialize, Serialize};

/// User-facing effect number.
pub type EffectId = u32;

/// Waveform of an effect.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Shape {
    /// Smooth up and down.
    #[default]
    Sine,
    /// Linear up and down.
    Triangle,
    /// On for `duty` of the cycle, off for the rest (chases, strobes).
    Square,
    /// Ramp up, then snap down.
    SawUp,
    /// Snap up, then ramp down.
    SawDown,
    /// A new random level every cycle.
    Random,
    /// Pan and tilt around a circle (the effect's attribute must be Pan).
    Circle,
}

impl Shape {
    /// Every shape.
    pub const ALL: [Shape; 7] = [Shape::Sine, Shape::Triangle, Shape::Square, Shape::SawUp, Shape::SawDown, Shape::Random, Shape::Circle];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        use crate::i18n::t;
        match self {
            Shape::Sine => t("Sine"),
            Shape::Triangle => t("Triangle"),
            Shape::Square => t("Square"),
            Shape::SawUp => t("Ramp up"),
            Shape::SawDown => t("Ramp down"),
            Shape::Random => t("Random"),
            Shape::Circle => t("Circle"),
        }
    }

    /// Level (0..=1) at `x` cycles (any real number); `seed` decorrelates Random per fixture.
    pub fn eval(self, x: f64, duty: f32, seed: u32) -> f32 {
        let f = x.rem_euclid(1.0) as f32;
        match self {
            Shape::Sine | Shape::Circle => 0.5 - 0.5 * (std::f32::consts::TAU * f).cos(),
            Shape::Triangle => 1.0 - (2.0 * f - 1.0).abs(),
            Shape::Square => (f < duty.clamp(0.0, 1.0)) as u8 as f32,
            Shape::SawUp => f,
            Shape::SawDown => 1.0 - f,
            Shape::Random => {
                // Integer hash of (cycle, seed): stable for the whole cycle.
                let mut h = (x.floor() as i64 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (seed as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
                h ^= h >> 29;
                h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
                h ^= h >> 32;
                (h & 0xffff) as f32 / 65535.0
            }
        }
    }
}

/// An effect in the pool.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Effect {
    /// Effect number.
    pub id: EffectId,
    /// Name.
    pub name: String,
    /// Attribute it moves (Pan for a circle, which also moves Tilt).
    pub attribute: Attribute,
    /// Waveform.
    pub shape: Shape,
    /// Cycles per second, or per beat of the tempo when `sync` is on.
    pub speed: f32,
    /// Follow the tempo (BPM) instead of a fixed speed.
    pub sync: bool,
    /// Depth, 0..=1.
    pub size: f32,
    /// Absolute only: the lowest level (the effect runs from `base` to `base + size`).
    pub base: f32,
    /// On around the value the cue gives (true), or replacing it (false).
    pub relative: bool,
    /// Square: part of the cycle that is on.
    pub duty: f32,
    /// Phase offset across the fixtures, in degrees (0 = all together, 360 = one full wave).
    pub spread: f32,
    /// Fixtures per group: a group moves together.
    pub groups: u32,
    /// Wings: the phase pattern is mirrored this many times across the fixtures.
    pub wings: u32,
}

impl Default for Effect {
    fn default() -> Self {
        Self {
            id: 1,
            name: String::new(),
            attribute: Attribute::Dimmer,
            shape: Shape::Sine,
            speed: 0.5,
            sync: false,
            size: 1.0,
            base: 0.0,
            relative: false,
            duty: 0.5,
            spread: 360.0,
            groups: 1,
            wings: 1,
        }
    }
}

impl Effect {
    /// Phase (in cycles) of the `i`-th of `n` fixtures, from spread, groups and wings.
    pub fn phase(&self, i: usize, n: usize) -> f32 {
        let g = self.groups.max(1) as usize;
        let positions = n.div_ceil(g).max(1);
        let mut p = i / g;
        let wings = (self.wings.max(1) as usize).min(positions);
        let per_wing = positions.div_ceil(wings).max(1);
        if wings > 1 {
            let (w, k) = (p / per_wing, p % per_wing);
            // Odd wings run backwards: a mirrored pattern.
            p = if w % 2 == 1 { per_wing - 1 - k } else { k };
        }
        let steps = if wings > 1 { per_wing } else { positions };
        self.spread / 360.0 * p as f32 / steps as f32
    }

    /// Cycles per second at `bpm`.
    pub fn rate(&self, bpm: f32) -> f32 {
        if self.sync { self.speed * bpm / 60.0 } else { self.speed }
    }
}

/// An effect run by a cue or the programmer on some fixtures (in phase order).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct EffectRun {
    /// Effect from the pool.
    pub effect: EffectId,
    /// Fixtures, in phase order.
    pub fixtures: Vec<FixtureId>,
    /// Stop this effect instead of starting it.
    pub stop: bool,
}

/// One fixture of a compiled effect.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Member {
    /// Slot of the attribute.
    pub slot: u32,
    /// Tilt slot of a circle.
    pub slot2: Option<u32>,
    /// Phase in cycles.
    pub phase: f32,
    /// Seed for Random.
    pub seed: u32,
}

/// An effect ready for playback.
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledEffect {
    /// Effect number (matches runs across cues).
    pub id: EffectId,
    /// Parameters.
    pub effect: Effect,
    /// Fixtures that have the attribute.
    pub members: Vec<Member>,
}

impl CompiledEffect {
    /// Apply at `t` seconds, scaled by `scale` (fade in/out), onto resolved slot values.
    /// Intensity is dimmed (relative) or replaced; other attributes move around (relative) or
    /// replace the value.
    pub fn apply(&self, t: f64, bpm: f32, scale: f32, values: &mut [f32], htp: impl Fn(u32) -> bool) {
        if scale <= 0.0 {
            return;
        }
        let e = &self.effect;
        let x0 = t * e.rate(bpm) as f64;
        for m in &self.members {
            let x = x0 + m.phase as f64;
            let i = m.slot as usize;
            if e.shape == Shape::Circle {
                let a = std::f32::consts::TAU * x.rem_euclid(1.0) as f32;
                let (s, c) = a.sin_cos();
                let r = e.size * 0.5;
                if let Some(j) = m.slot2.map(|s| s as usize) {
                    let (pan, tilt) = if e.relative { (values[i] + r * s, values[j] + r * c) } else { (e.base + r * (1.0 + s), e.base + r * (1.0 + c)) };
                    values[i] += (pan.clamp(0.0, 1.0) - values[i]) * scale;
                    values[j] += (tilt.clamp(0.0, 1.0) - values[j]) * scale;
                }
                continue;
            }
            let v = e.shape.eval(x, e.duty, m.seed);
            let target = match (e.relative, htp(m.slot)) {
                (true, true) => values[i] * (1.0 - e.size * (1.0 - v)),
                (true, false) => values[i] + e.size * (v - 0.5),
                (false, _) => e.base + e.size * v,
            };
            values[i] += (target.clamp(0.0, 1.0) - values[i]) * scale;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_eq!(Shape::Sine.eval(0.0, 0.5, 0), 0.0);
        assert!((Shape::Sine.eval(0.5, 0.5, 0) - 1.0).abs() < 1e-6);
        assert_eq!(Shape::Triangle.eval(0.25, 0.5, 0), 0.5);
        assert_eq!(Shape::Square.eval(0.2, 0.25, 0), 1.0);
        assert_eq!(Shape::Square.eval(0.3, 0.25, 0), 0.0);
        assert_eq!(Shape::SawUp.eval(-0.25, 0.5, 0), 0.75, "negative time wraps");
        let r = Shape::Random.eval(3.1, 0.5, 7);
        assert_eq!(r, Shape::Random.eval(3.9, 0.5, 7), "held for the cycle");
        assert_ne!(r, Shape::Random.eval(4.1, 0.5, 7));
    }

    #[test]
    fn phases() {
        let e = Effect { spread: 360.0, ..Default::default() };
        let p: Vec<f32> = (0..4).map(|i| e.phase(i, 4)).collect();
        assert_eq!(p, [0.0, 0.25, 0.5, 0.75]);
        let g = Effect { groups: 2, ..e.clone() };
        assert_eq!((0..4).map(|i| g.phase(i, 4)).collect::<Vec<_>>(), [0.0, 0.0, 0.5, 0.5]);
        let w = Effect { wings: 2, ..e.clone() };
        assert_eq!((0..4).map(|i| w.phase(i, 4)).collect::<Vec<_>>(), [0.0, 0.5, 0.5, 0.0], "mirrored");
        let sync = Effect { sync: true, speed: 0.25, ..e };
        assert_eq!(sync.rate(120.0), 0.5);
    }

    #[test]
    fn apply_relative_and_absolute() {
        let members = vec![Member { slot: 0, slot2: None, phase: 0.0, seed: 0 }];
        let chase = CompiledEffect { id: 1, effect: Effect { shape: Shape::Square, relative: true, ..Default::default() }, members: members.clone() };
        let mut v = [0.8];
        chase.apply(0.1, 120.0, 1.0, &mut v, |_| true);
        assert_eq!(v[0], 0.8, "on: the cue's level");
        chase.apply(1.5, 120.0, 1.0, &mut v, |_| true);
        assert_eq!(v[0], 0.0, "off");
        let abs = CompiledEffect { id: 2, effect: Effect { shape: Shape::SawUp, base: 0.2, size: 0.5, speed: 1.0, ..Default::default() }, members };
        let mut v = [0.9];
        abs.apply(0.5, 120.0, 0.5, &mut v, |_| false);
        assert!((v[0] - (0.9 + (0.45 - 0.9) * 0.5)).abs() < 1e-6, "half faded in");
    }
}
