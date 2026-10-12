//! Keyframe animation for video, image and text layers.
//!
//! An [`Animation`] is a set of tracks, one per animated property, each a list of keyframes.
//! It is evaluated live, every time a stage window draws (a handful of interpolations per
//! layer); the GPU does the moving, scaling and colouring, so animating costs no decoding or
//! re-rendering. Values are *relative* to the cue's own settings: position, rotation and crop
//! are added, scale and opacity multiply, so Fade cues and the Geometry tab still move or fade
//! the whole animated layer. Colour tracks set a colour: `Tint` multiplies the picture (white
//! leaves it alone), the text tracks replace the Text cue's colours.
//!
//! The clock is the cue's own playing time, so an animation pauses and resumes with its cue.

use crate::model::Geometry;
use serde::{Deserialize, Serialize};

/// A property a track animates.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AnimProp {
    /// Added to the X position (fraction of the stage width).
    X,
    /// Added to the Y position (fraction of the stage height).
    Y,
    /// Multiplies the horizontal scale.
    ScaleX,
    /// Multiplies the vertical scale.
    ScaleY,
    /// Added to the rotation (degrees clockwise).
    Rotation,
    /// Added to the left crop (fraction of the picture).
    CropLeft,
    /// Added to the top crop.
    CropTop,
    /// Added to the right crop.
    CropRight,
    /// Added to the bottom crop.
    CropBottom,
    /// Multiplies the opacity.
    Opacity,
    /// Multiplies the picture's colour (white = unchanged).
    Tint,
    /// Text fill colour.
    TextColor,
    /// Text outline colour.
    OutlineColor,
    /// Text shadow colour.
    ShadowColor,
    /// Text background colour.
    Background,
}

impl AnimProp {
    /// Properties every layer can animate.
    pub const LAYER: [AnimProp; 11] = [
        AnimProp::X,
        AnimProp::Y,
        AnimProp::ScaleX,
        AnimProp::ScaleY,
        AnimProp::Rotation,
        AnimProp::CropLeft,
        AnimProp::CropTop,
        AnimProp::CropRight,
        AnimProp::CropBottom,
        AnimProp::Opacity,
        AnimProp::Tint,
    ];
    /// Extra properties of Text cues, in [`AnimState::text`] order.
    pub const TEXT: [AnimProp; 4] = [AnimProp::Background, AnimProp::ShadowColor, AnimProp::OutlineColor, AnimProp::TextColor];

    /// True for colour properties (values are sRGB RGBA 0..1; the others use `v[0]`).
    pub fn is_color(self) -> bool {
        matches!(self, AnimProp::Tint | AnimProp::TextColor | AnimProp::OutlineColor | AnimProp::ShadowColor | AnimProp::Background)
    }

    /// Name shown in the editor.
    pub fn label(self) -> &'static str {
        match self {
            AnimProp::X => "X",
            AnimProp::Y => "Y",
            AnimProp::ScaleX => "Scale X",
            AnimProp::ScaleY => "Scale Y",
            AnimProp::Rotation => "Rotation",
            AnimProp::CropLeft => "Crop left",
            AnimProp::CropTop => "Crop top",
            AnimProp::CropRight => "Crop right",
            AnimProp::CropBottom => "Crop bottom",
            AnimProp::Opacity => "Opacity",
            AnimProp::Tint => "Tint",
            AnimProp::TextColor => "Text colour",
            AnimProp::OutlineColor => "Outline colour",
            AnimProp::ShadowColor => "Shadow colour",
            AnimProp::Background => "Background",
        }
    }

    /// The value that changes nothing (used for a new track's first key).
    pub fn neutral(self) -> [f32; 4] {
        match self {
            AnimProp::ScaleX | AnimProp::ScaleY | AnimProp::Opacity => [1.0, 0.0, 0.0, 0.0],
            p if p.is_color() => [1.0; 4],
            _ => [0.0; 4],
        }
    }
}

/// How a keyframe moves on to the next one.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum Ease {
    /// Keep this value until the next key, then jump.
    Hold,
    /// Constant speed.
    #[default]
    Linear,
    /// Start slowly.
    In,
    /// End slowly.
    Out,
    /// Start and end slowly.
    InOut,
    /// CSS-style cubic Bézier `(x1, y1, x2, y2)`.
    Bezier([f32; 4]),
}

impl Ease {
    /// Shaped progress for `u` in 0..1.
    pub fn apply(self, u: f32) -> f32 {
        let u = u.clamp(0.0, 1.0);
        match self {
            Ease::Hold => 0.0,
            Ease::Linear => u,
            Ease::In => cubic_bezier(0.42, 0.0, 1.0, 1.0, u),
            Ease::Out => cubic_bezier(0.0, 0.0, 0.58, 1.0, u),
            Ease::InOut => cubic_bezier(0.42, 0.0, 0.58, 1.0, u),
            Ease::Bezier([x1, y1, x2, y2]) => cubic_bezier(x1.clamp(0.0, 1.0), y1, x2.clamp(0.0, 1.0), y2, u),
        }
    }

    /// Name shown in the editor.
    pub fn label(self) -> &'static str {
        match self {
            Ease::Hold => "Hold",
            Ease::Linear => "Linear",
            Ease::In => "Ease in",
            Ease::Out => "Ease out",
            Ease::InOut => "Ease in-out",
            Ease::Bezier(_) => "Custom curve",
        }
    }
}

/// y of a cubic Bézier from (0,0) to (1,1) with control points (x1,y1), (x2,y2) at x = `x`.
fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    let bez = |a: f32, b: f32, s: f32| {
        let m = 1.0 - s;
        3.0 * m * m * s * a + 3.0 * m * s * s * b + s * s * s
    };
    let slope = |a: f32, b: f32, s: f32| {
        let m = 1.0 - s;
        3.0 * m * m * a + 6.0 * m * s * (b - a) + 3.0 * s * s * (1.0 - b)
    };
    // Newton from s = x, falling back to bisection where the curve is flat.
    let mut s = x;
    for _ in 0..8 {
        let d = slope(x1, x2, s);
        if d.abs() < 1e-6 {
            break;
        }
        let e = bez(x1, x2, s) - x;
        if e.abs() < 1e-6 {
            return bez(y1, y2, s);
        }
        s = (s - e / d).clamp(0.0, 1.0);
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    s = x;
    for _ in 0..30 {
        let e = bez(x1, x2, s) - x;
        if e.abs() < 1e-6 {
            break;
        }
        if e > 0.0 { hi = s } else { lo = s }
        s = (lo + hi) / 2.0;
    }
    bez(y1, y2, s)
}

/// A value at a point in time.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Key {
    /// Seconds from the start of the cue.
    pub t: f64,
    /// The value (`v[0]` for numbers; sRGB RGBA 0..1 for colours).
    pub v: [f32; 4],
    /// How the value moves on to the next key.
    pub ease: Ease,
}

/// The keyframes of one property, sorted by time.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Track {
    /// What the track animates.
    pub prop: AnimProp,
    /// Keyframes, in time order.
    pub keys: Vec<Key>,
}

impl Default for Track {
    fn default() -> Self {
        Self { prop: AnimProp::Opacity, keys: Vec::new() }
    }
}

impl Track {
    /// The value at `t` seconds (held before the first and after the last key).
    pub fn sample(&self, t: f64) -> [f32; 4] {
        let Some(first) = self.keys.first() else { return self.prop.neutral() };
        if t <= first.t {
            return first.v;
        }
        let i = self.keys.partition_point(|k| k.t <= t);
        let Some(b) = self.keys.get(i) else { return self.keys[i - 1].v };
        let a = &self.keys[i - 1];
        let span = b.t - a.t;
        let u = if span > 0.0 { a.ease.apply(((t - a.t) / span) as f32) } else { 1.0 };
        if self.prop.is_color() { lerp_color(a.v, b.v, u) } else { std::array::from_fn(|k| a.v[k] + (b.v[k] - a.v[k]) * u) }
    }

    /// Put keys back in time order (after an edit moved one).
    pub fn sort(&mut self) {
        self.keys.sort_by(|a, b| a.t.total_cmp(&b.t));
    }
}

/// Colour interpolation in linear light, so a red-to-green change doesn't pass through a muddy
/// dark brown.
fn lerp_color(a: [f32; 4], b: [f32; 4], u: f32) -> [f32; 4] {
    let (la, lb) = (a.map(to_linear), b.map(to_linear));
    let mut out: [f32; 4] = std::array::from_fn(|k| to_srgb(la[k] + (lb[k] - la[k]) * u));
    out[3] = a[3] + (b[3] - a[3]) * u;
    out
}

fn to_linear(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

/// What happens after the last key.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Repeat {
    /// Play once and hold the last values.
    #[default]
    Once,
    /// Start again from the beginning.
    Loop,
    /// Play forwards, then backwards, and so on.
    PingPong,
}

/// Keyframe tracks of a layer (empty = not animated).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Animation {
    /// Length of one pass in seconds (0 = up to the last key). Loops repeat this length.
    pub duration: f64,
    /// What happens after one pass.
    pub repeat: Repeat,
    /// One track per animated property.
    pub tracks: Vec<Track>,
}

/// Animated values at one moment, ready to combine with a layer's own settings.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct AnimState {
    /// Added to x.
    pub dx: f32,
    /// Added to y.
    pub dy: f32,
    /// Multiplies scale_x.
    pub sx: f32,
    /// Multiplies scale_y.
    pub sy: f32,
    /// Added to the rotation.
    pub rotation: f32,
    /// Added to the crop.
    pub crop: [f32; 4],
    /// Multiplies the opacity.
    pub opacity: f32,
    /// Multiplies the colour (sRGB RGBA 0..1).
    pub tint: [f32; 4],
    /// Text colours that replace the cue's own, in [`AnimProp::TEXT`] order.
    pub text: [Option<[f32; 4]>; 4],
}

impl Default for AnimState {
    fn default() -> Self {
        Self { dx: 0.0, dy: 0.0, sx: 1.0, sy: 1.0, rotation: 0.0, crop: [0.0; 4], opacity: 1.0, tint: [1.0; 4], text: [None; 4] }
    }
}

impl AnimState {
    /// A layer's geometry with the animation applied.
    pub fn geometry(&self, g: &Geometry) -> Geometry {
        Geometry {
            x: g.x + self.dx,
            y: g.y + self.dy,
            scale_x: g.scale_x * self.sx,
            scale_y: g.scale_y * self.sy,
            rotation: g.rotation + self.rotation,
            crop: std::array::from_fn(|i| (g.crop[i] + self.crop[i]).clamp(0.0, 1.0)),
        }
    }
}

impl Animation {
    /// True when there is nothing to animate.
    pub fn is_empty(&self) -> bool {
        self.tracks.iter().all(|t| t.keys.is_empty())
    }

    /// The track of `prop`, if any.
    pub fn track(&self, prop: AnimProp) -> Option<&Track> {
        self.tracks.iter().find(|t| t.prop == prop && !t.keys.is_empty())
    }

    /// True if `prop` has keys.
    pub fn has(&self, prop: AnimProp) -> bool {
        self.track(prop).is_some()
    }

    /// Length of one pass, in seconds.
    pub fn length(&self) -> f64 {
        if self.duration > 0.0 {
            return self.duration;
        }
        self.tracks.iter().filter_map(|t| t.keys.last()).map(|k| k.t).fold(0.0, f64::max)
    }

    /// Animation time for `t` seconds of cue time (after looping).
    pub fn local_time(&self, t: f64) -> f64 {
        let len = self.length();
        if len <= 0.0 || t <= 0.0 {
            return t.max(0.0);
        }
        match self.repeat {
            Repeat::Once => t.min(len),
            Repeat::Loop => t % len,
            Repeat::PingPong => {
                let p = t % (2.0 * len);
                if p <= len { p } else { 2.0 * len - p }
            }
        }
    }

    /// Values at `t` seconds of cue time.
    pub fn sample(&self, t: f64) -> AnimState {
        let mut s = AnimState::default();
        if self.is_empty() {
            return s;
        }
        let t = self.local_time(t);
        for tr in self.tracks.iter().filter(|t| !t.keys.is_empty()) {
            let v = tr.sample(t);
            match tr.prop {
                AnimProp::X => s.dx = v[0],
                AnimProp::Y => s.dy = v[0],
                AnimProp::ScaleX => s.sx = v[0].max(0.0),
                AnimProp::ScaleY => s.sy = v[0].max(0.0),
                AnimProp::Rotation => s.rotation = v[0],
                AnimProp::CropLeft => s.crop[0] = v[0],
                AnimProp::CropTop => s.crop[1] = v[0],
                AnimProp::CropRight => s.crop[2] = v[0],
                AnimProp::CropBottom => s.crop[3] = v[0],
                AnimProp::Opacity => s.opacity = v[0].clamp(0.0, 1.0),
                AnimProp::Tint => s.tint = v,
                p => {
                    if let Some(i) = AnimProp::TEXT.iter().position(|q| *q == p) {
                        s.text[i] = Some(v);
                    }
                }
            }
        }
        s
    }

    /// Largest scale factor the animation reaches (≥ 1), so stills can be rasterised sharp
    /// enough for it.
    pub fn max_scale(&self) -> f32 {
        [AnimProp::ScaleX, AnimProp::ScaleY]
            .iter()
            .filter_map(|p| self.track(*p))
            .flat_map(|t| t.keys.iter().map(|k| k.v[0]))
            .fold(1.0, f32::max)
    }

    /// Set the value of `prop` at `t`: replaces a key within 1 ms, else inserts one.
    pub fn set_key(&mut self, prop: AnimProp, t: f64, v: [f32; 4]) {
        let i = match self.tracks.iter().position(|tr| tr.prop == prop) {
            Some(i) => i,
            None => {
                self.tracks.push(Track { prop, keys: Vec::new() });
                self.tracks.len() - 1
            }
        };
        let tr = &mut self.tracks[i];
        match tr.keys.iter_mut().find(|k| (k.t - t).abs() < 1e-3) {
            Some(k) => k.v = v,
            None => {
                tr.keys.push(Key { t: t.max(0.0), v, ease: Ease::default() });
                tr.sort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anim(prop: AnimProp, keys: &[(f64, f32)]) -> Animation {
        let keys = keys.iter().map(|&(t, v)| Key { t, v: [v, 0.0, 0.0, 0.0], ease: Ease::Linear }).collect();
        Animation { tracks: vec![Track { prop, keys }], ..Default::default() }
    }

    #[test]
    fn samples_hold_ends_and_interpolate() {
        let a = anim(AnimProp::X, &[(1.0, 0.0), (3.0, 1.0)]);
        assert_eq!(a.sample(0.0).dx, 0.0);
        assert!((a.sample(2.0).dx - 0.5).abs() < 1e-6);
        assert_eq!(a.sample(10.0).dx, 1.0);
        assert_eq!(a.length(), 3.0);
    }

    #[test]
    fn relative_geometry() {
        let mut a = anim(AnimProp::ScaleX, &[(0.0, 2.0)]);
        a.set_key(AnimProp::X, 0.0, [0.25, 0.0, 0.0, 0.0]);
        a.set_key(AnimProp::CropLeft, 0.0, [0.9, 0.0, 0.0, 0.0]);
        let base = Geometry { x: 0.1, scale_x: 0.5, crop: [0.3, 0.0, 0.0, 0.0], ..Default::default() };
        let g = a.sample(0.0).geometry(&base);
        assert!((g.x - 0.35).abs() < 1e-6);
        assert_eq!(g.scale_x, 1.0);
        assert_eq!(g.crop[0], 1.0);
        assert_eq!(a.max_scale(), 2.0);
    }

    #[test]
    fn loops_and_ping_pong() {
        let mut a = anim(AnimProp::Opacity, &[(0.0, 0.0), (2.0, 1.0)]);
        a.repeat = Repeat::Loop;
        assert!((a.sample(2.5).opacity - 0.25).abs() < 1e-6);
        a.repeat = Repeat::PingPong;
        assert!((a.sample(2.5).opacity - 0.75).abs() < 1e-6);
        a.repeat = Repeat::Once;
        a.duration = 4.0;
        assert_eq!(a.sample(3.0).opacity, 1.0);
    }

    #[test]
    fn hold_and_easing() {
        let mut a = anim(AnimProp::Y, &[(0.0, 0.0), (1.0, 1.0)]);
        a.tracks[0].keys[0].ease = Ease::Hold;
        assert_eq!(a.sample(0.99).dy, 0.0);
        assert_eq!(a.sample(1.0).dy, 1.0);
        for e in [Ease::In, Ease::Out, Ease::InOut, Ease::Bezier([0.2, 0.8, 0.3, 1.0])] {
            assert!(e.apply(0.0).abs() < 1e-4 && (e.apply(1.0) - 1.0).abs() < 1e-4, "{e:?}");
        }
        assert!(Ease::In.apply(0.5) < 0.5 && Ease::Out.apply(0.5) > 0.5);
        assert!((Ease::InOut.apply(0.5) - 0.5).abs() < 1e-3);
    }

    #[test]
    fn colours_mix_in_linear_light() {
        let mut a = Animation::default();
        a.set_key(AnimProp::TextColor, 0.0, [1.0, 0.0, 0.0, 1.0]);
        a.set_key(AnimProp::TextColor, 1.0, [0.0, 1.0, 0.0, 1.0]);
        let mid = a.sample(0.5).text[3].unwrap();
        // sRGB halfway would be 0.5; linear-light halfway is brighter.
        assert!(mid[0] > 0.7 && mid[1] > 0.7, "{mid:?}");
        assert_eq!(a.sample(0.5).text[0], None);
    }

    #[test]
    fn set_key_replaces_nearby_and_sorts() {
        let mut a = Animation::default();
        a.set_key(AnimProp::X, 2.0, [1.0; 4]);
        a.set_key(AnimProp::X, 1.0, [0.5; 4]);
        a.set_key(AnimProp::X, 2.0004, [2.0; 4]);
        let k = &a.tracks[0].keys;
        assert_eq!(k.len(), 2);
        assert_eq!((k[0].t, k[1].v[0]), (1.0, 2.0));
    }

    #[test]
    fn roundtrips_and_old_files_load() {
        let mut a = anim(AnimProp::Rotation, &[(0.0, 0.0), (1.5, 90.0)]);
        a.tracks[0].keys[0].ease = Ease::Bezier([0.1, 0.2, 0.3, 0.4]);
        let back: Animation = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(a, back);
        let v: crate::model::VideoCue = serde_json::from_str("{}").unwrap();
        assert!(v.animation.is_empty());
    }
}
