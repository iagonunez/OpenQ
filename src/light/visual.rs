//! What the 3D visualiser draws for each fixture, read back from a DMX frame: where it is,
//! where its beam points, how wide, what colour and how bright. Also the inverse for
//! "click to focus": the pan/tilt that aims a moving head at a point.
//!
//! Space: metres, x to the right (stage left), y up, z towards the audience. A fixture with no
//! rotation hangs pointing straight down. Its rotation (degrees) is applied as yaw (y), then
//! pitch (x), then roll (z). Pan turns the head about the fixture's own vertical axis, tilt
//! swings it from straight down towards its own +z (the audience, with pan at centre).

use super::dmx::DmxFrame;
use super::model::{Attribute, ChannelDef, Fixture, FixtureId, Lighting};

/// A point or direction.
pub type V3 = [f32; 3];

/// Vector helpers (tiny on purpose: no maths crate needed for a few dozen operations).
pub mod v3 {
    use super::V3;
    /// a + b
    pub fn add(a: V3, b: V3) -> V3 {
        [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
    }
    /// a - b
    pub fn sub(a: V3, b: V3) -> V3 {
        [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
    }
    /// a * k
    pub fn scale(a: V3, k: f32) -> V3 {
        [a[0] * k, a[1] * k, a[2] * k]
    }
    /// Dot product.
    pub fn dot(a: V3, b: V3) -> f32 {
        a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
    }
    /// Cross product.
    pub fn cross(a: V3, b: V3) -> V3 {
        [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
    }
    /// Length.
    pub fn len(a: V3) -> f32 {
        dot(a, a).sqrt()
    }
    /// Unit vector (zero stays zero).
    pub fn norm(a: V3) -> V3 {
        let l = len(a);
        if l > 1e-9 { scale(a, 1.0 / l) } else { a }
    }
}

/// A 3×3 rotation, rows.
pub type M3 = [V3; 3];

/// m · v
pub fn mul(m: &M3, v: V3) -> V3 {
    [v3::dot(m[0], v), v3::dot(m[1], v), v3::dot(m[2], v)]
}

/// m1 · m2
pub fn mul_m(a: &M3, b: &M3) -> M3 {
    let col = |j: usize| [b[0][j], b[1][j], b[2][j]];
    let r = |i: usize| [v3::dot(a[i], col(0)), v3::dot(a[i], col(1)), v3::dot(a[i], col(2))];
    [r(0), r(1), r(2)]
}

/// Transpose (the inverse of a rotation).
pub fn transpose(m: &M3) -> M3 {
    [[m[0][0], m[1][0], m[2][0]], [m[0][1], m[1][1], m[2][1]], [m[0][2], m[1][2], m[2][2]]]
}

/// Rotation about x by `a` radians (y towards z).
pub fn rot_x(a: f32) -> M3 {
    let (s, c) = a.sin_cos();
    [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
}

/// Rotation about y by `a` radians (z towards x).
pub fn rot_y(a: f32) -> M3 {
    let (s, c) = a.sin_cos();
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}

/// Rotation about z by `a` radians (x towards y).
pub fn rot_z(a: f32) -> M3 {
    let (s, c) = a.sin_cos();
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}

/// A fixture's mounting rotation (yaw, then pitch, then roll).
pub fn mounting(f: &Fixture) -> M3 {
    let r = f.rotation.map(f32::to_radians);
    mul_m(&mul_m(&rot_y(r[1]), &rot_x(r[0])), &rot_z(r[2]))
}

/// Rotation of a moving head relative to its base: pan about y, then tilt about x.
pub fn head(pan_deg: f32, tilt_deg: f32) -> M3 {
    mul_m(&rot_y(pan_deg.to_radians()), &rot_x(-tilt_deg.to_radians()))
}

/// Straight down: the beam of an unrotated fixture.
pub const DOWN: V3 = [0.0, -1.0, 0.0];

/// A fixture as the visualiser shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureLook {
    /// Fixture number.
    pub id: FixtureId,
    /// Position.
    pub position: V3,
    /// Mounting rotation.
    pub mount: M3,
    /// Has a moving head.
    pub moving: bool,
    /// Head rotation relative to the mount (identity for fixed fixtures).
    pub head: M3,
    /// Beam direction (unit).
    pub dir: V3,
    /// Full beam angle, degrees.
    pub angle: f32,
    /// Linear colour of the light (0..=1 each).
    pub color: V3,
    /// Brightness, 0..=1.
    pub intensity: f32,
}

/// Read one channel back as 0..=1.
fn read(c: &ChannelDef, start: usize, data: &[u8]) -> f32 {
    let byte = |o: u16| data.get(start + o as usize).copied().unwrap_or(0) as f32;
    match c.fine {
        Some(f) => (byte(c.offset) * 256.0 + byte(f)) / 65535.0,
        None => byte(c.offset) / 255.0,
    }
}

/// Every patched fixture as it looks in `frame`. Fixtures without output (unpatched, unknown
/// type) are still placed, unlit.
pub fn looks(l: &Lighting, frame: &DmxFrame) -> Vec<FixtureLook> {
    l.fixtures
        .iter()
        .filter_map(|f| {
            let ty = l.fixture_type(&f.fixture_type)?;
            let mode = ty.mode(&f.mode)?;
            let data = (f.address > 0).then(|| frame.get(f.universe)).flatten().map(|u| &u.data[..]);
            let start = f.address.max(1) as usize - 1;
            let get = |a: Attribute| -> Option<f32> {
                let c = mode.channel(a)?;
                let mut v = read(c, start, data?);
                if (a == Attribute::Pan && f.invert_pan) || (a == Attribute::Tilt && f.invert_tilt) {
                    v = 1.0 - v;
                }
                Some(v)
            };
            let has = |a: Attribute| mode.channel(a).is_some();
            // Colour: RGB(+W/A/L/UV) mixing, CMY subtractive, or a white source.
            let mut color = [0.0f32; 3];
            let mut emitters = false;
            for (a, rgb) in [
                (Attribute::Red, [1.0, 0.0, 0.0]),
                (Attribute::Green, [0.0, 1.0, 0.0]),
                (Attribute::Blue, [0.0, 0.0, 1.0]),
                (Attribute::White, [1.0, 1.0, 1.0]),
                (Attribute::Amber, [1.0, 0.55, 0.0]),
                (Attribute::Lime, [0.6, 1.0, 0.0]),
                (Attribute::Uv, [0.25, 0.0, 0.5]),
            ] {
                if has(a) {
                    emitters = true;
                    color = v3::add(color, v3::scale(rgb, get(a).unwrap_or(0.0)));
                }
            }
            if !emitters {
                color = if has(Attribute::Cyan) || has(Attribute::Magenta) || has(Attribute::Yellow) {
                    [1.0 - get(Attribute::Cyan).unwrap_or(0.0), 1.0 - get(Attribute::Magenta).unwrap_or(0.0), 1.0 - get(Attribute::Yellow).unwrap_or(0.0)]
                } else {
                    [1.0, 0.92, 0.8]
                };
            }
            let peak = color.iter().copied().fold(0.0, f32::max);
            let mut intensity = match get(Attribute::Dimmer) {
                Some(d) => d * peak.min(1.0),
                // No dimmer channel: the emitters carry the level.
                None if emitters => peak.min(1.0),
                None => 0.0,
            };
            if peak > 0.0 {
                color = v3::scale(color, 1.0 / peak);
            }
            if get(Attribute::Shutter).is_some_and(|s| s < 0.02) {
                intensity = 0.0;
            }
            let p = &ty.physical;
            let zoom = get(Attribute::Zoom).unwrap_or(0.5);
            let mut angle = p.beam_min + (p.beam_max - p.beam_min) * zoom;
            if let Some(iris) = get(Attribute::Iris) {
                angle *= 0.15 + 0.85 * iris;
            }
            let mount = mounting(f);
            let moving = p.moving && has(Attribute::Pan) && has(Attribute::Tilt);
            let head_m = if moving {
                let pan = (get(Attribute::Pan).unwrap_or(0.5) - 0.5) * p.pan_range;
                let tilt = (get(Attribute::Tilt).unwrap_or(0.5) - 0.5) * p.tilt_range;
                head(pan, tilt)
            } else {
                [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
            };
            let dir = v3::norm(mul(&mount, mul(&head_m, DOWN)));
            Some(FixtureLook { id: f.id, position: f.position, mount, moving, head: head_m, dir, angle: angle.clamp(1.0, 120.0), color, intensity })
        })
        .collect()
}

/// Pan and tilt (normalised 0..=1) that aim a moving head at `target`, or None for fixtures
/// that can't move (or a target out of their range).
pub fn aim(l: &Lighting, fixture: FixtureId, target: V3) -> Option<(f32, f32)> {
    let f = l.fixture(fixture)?;
    let p = &l.fixture_type(&f.fixture_type)?.physical;
    if !p.moving {
        return None;
    }
    // Direction in the fixture's own frame: d = Ry(pan) · Rx(-tilt) · down
    //   = (sin(tilt)·sin(pan), -cos(tilt), sin(tilt)·cos(pan)).
    let d = v3::norm(mul(&transpose(&mounting(f)), v3::sub(target, f.position)));
    let tilt = (-d[1]).clamp(-1.0, 1.0).acos();
    let pan = if tilt.abs() < 1e-4 { 0.0 } else { d[0].atan2(d[2]) };
    let pv = 0.5 + pan.to_degrees() / p.pan_range;
    let tv = 0.5 + tilt.to_degrees() / p.tilt_range;
    let mut v = (pv, tv);
    if f.invert_pan {
        v.0 = 1.0 - v.0;
    }
    if f.invert_tilt {
        v.1 = 1.0 - v.1;
    }
    ((0.0..=1.0).contains(&v.0) && (0.0..=1.0).contains(&v.1)).then_some(v)
}

/// Where a ray from `origin` along `dir` meets the floor (y = 0), if it goes down to it.
pub fn floor_hit(origin: V3, dir: V3) -> Option<V3> {
    (dir[1] < -1e-6).then(|| v3::add(origin, v3::scale(dir, -origin[1] / dir[1])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::library;
    use crate::light::model::Fixture;
    use crate::light::playback::{LightShow, Playback};
    use std::sync::Arc;
    use std::time::Instant;

    fn rig() -> Lighting {
        let mut l = Lighting::default();
        for (id, ty, addr, pos) in [(1, "generic/rgb", 1, [0.0, 5.0, 0.0]), (2, "generic/spot", 10, [2.0, 6.0, -1.0])] {
            let t = library::generic(ty).unwrap();
            l.add_type(&t);
            l.fixtures.push(Fixture { id, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), address: addr, position: pos, ..Default::default() });
        }
        l
    }

    fn frame(l: &Lighting, prog: &[(FixtureId, Attribute, f32)]) -> DmxFrame {
        let mut p = Playback::new(Arc::new(LightShow::compile(l)), Instant::now());
        p.set_programmer(prog);
        p.tick(Instant::now()).clone()
    }

    #[test]
    fn colour_intensity_and_direction() {
        let l = rig();
        let f = frame(&l, &[(1, Attribute::Dimmer, 0.5), (1, Attribute::Red, 1.0), (1, Attribute::Blue, 0.5)]);
        let looks = looks(&l, &f);
        let a = &looks[0];
        assert!((a.intensity - 0.5).abs() < 0.01, "virtual dimmer");
        assert!((a.color[0] - 1.0).abs() < 0.01 && (a.color[2] - 0.5).abs() < 0.01);
        assert_eq!(a.dir, DOWN);
        assert!(!a.moving);
        let b = &looks[1];
        assert!(b.moving);
        assert_eq!(b.intensity, 0.0, "dimmer at 0");
        assert!((b.dir[1] + 1.0).abs() < 1e-3, "pan/tilt home: straight down");
    }

    #[test]
    fn aim_round_trips() {
        let l = rig();
        for target in [[5.0, 0.0, 2.0], [-3.0, 0.0, 4.0], [2.0, 0.0, -6.0], [2.0, 0.0, -1.0]] {
            let (pan, tilt) = aim(&l, 2, target).expect("in range");
            let f = frame(&l, &[(2, Attribute::Pan, pan), (2, Attribute::Tilt, tilt), (2, Attribute::Dimmer, 1.0)]);
            let s = &looks(&l, &f)[1];
            let hit = floor_hit(s.position, s.dir).unwrap();
            assert!(v3::len(v3::sub(hit, target)) < 0.05, "{target:?} → {hit:?}");
        }
        assert_eq!(aim(&l, 1, [0.0, 0.0, 0.0]), None, "not a moving head");
    }
}
