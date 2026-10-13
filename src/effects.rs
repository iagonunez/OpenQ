//! Audio and video effects as stored on cues: effect stacks.
//!
//! An effect is a kind plus up to [`MAX_PARAMS`] numbers whose meaning, range and default come
//! from the kind's [`ParamSpec`] table, so editors, fades and the file format treat every effect
//! the same way. Audio effects run in the mixer ([`crate::audio_fx`]) after the cue's EQ; video
//! effects run in the stage windows' layer shader (`ui::blend`), so neither touches the decoded
//! media and both can change live.

use serde::{Deserialize, Serialize};

/// Most numbers one effect has.
pub const MAX_PARAMS: usize = 6;
/// Most audio effects one cue runs (more are ignored).
pub const MAX_AUDIO_FX: usize = 8;
/// Most colour effects one layer runs in the shader (more are ignored).
pub const MAX_VIDEO_FX: usize = 8;

/// One adjustable number of an effect.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamSpec {
    /// Label in the editor.
    pub name: &'static str,
    /// Lowest value.
    pub min: f32,
    /// Highest value.
    pub max: f32,
    /// Value of a new effect.
    pub default: f32,
    /// Value at which the effect leaves the signal alone (where fades in and out start/end).
    pub neutral: f32,
    /// Unit shown after the number.
    pub unit: &'static str,
    /// Logarithmic slider (times, frequencies, ratios).
    pub log: bool,
}

const fn p(name: &'static str, min: f32, max: f32, default: f32, neutral: f32, unit: &'static str, log: bool) -> ParamSpec {
    ParamSpec { name, min, max, default, neutral, unit, log }
}

/// The type of an audio effect.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum AudioFxKind {
    /// Echo with feedback and a high cut in the loop.
    #[default]
    Delay,
    /// Room reverb (Freeverb-style comb and all-pass network).
    Reverb,
    /// Downward compressor with soft knee (channels linked).
    Compressor,
    /// Peak limiter (channels linked).
    Limiter,
    /// Noise gate with hold (channels linked).
    Gate,
    /// Soft-clipping overdrive with a tone filter.
    Distortion,
    /// Modulated short delay.
    Chorus,
    /// Lower bit depth and sample rate.
    Bitcrusher,
}

impl AudioFxKind {
    /// Every type, for menus.
    pub const ALL: [AudioFxKind; 8] = [
        AudioFxKind::Delay,
        AudioFxKind::Reverb,
        AudioFxKind::Compressor,
        AudioFxKind::Limiter,
        AudioFxKind::Gate,
        AudioFxKind::Distortion,
        AudioFxKind::Chorus,
        AudioFxKind::Bitcrusher,
    ];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            AudioFxKind::Delay => "Delay",
            AudioFxKind::Reverb => "Reverb",
            AudioFxKind::Compressor => "Compressor",
            AudioFxKind::Limiter => "Limiter",
            AudioFxKind::Gate => "Gate",
            AudioFxKind::Distortion => "Distortion",
            AudioFxKind::Chorus => "Chorus",
            AudioFxKind::Bitcrusher => "Bitcrusher",
        }
    }

    /// The numbers this effect takes, in `params` order.
    pub fn params(self) -> &'static [ParamSpec] {
        match self {
            AudioFxKind::Delay => const { &[
                p("Time", 1.0, 2000.0, 350.0, 350.0, "ms", true),
                p("Feedback", 0.0, 95.0, 35.0, 0.0, "%", false),
                p("High cut", 500.0, 20000.0, 8000.0, 20000.0, "Hz", true),
                p("Mix", 0.0, 100.0, 30.0, 0.0, "%", false),
            ] },
            AudioFxKind::Reverb => const { &[
                p("Size", 0.0, 100.0, 60.0, 60.0, "%", false),
                p("Damping", 0.0, 100.0, 40.0, 40.0, "%", false),
                p("Pre-delay", 0.0, 200.0, 10.0, 10.0, "ms", false),
                p("Width", 0.0, 100.0, 100.0, 100.0, "%", false),
                p("Mix", 0.0, 100.0, 25.0, 0.0, "%", false),
            ] },
            AudioFxKind::Compressor => const { &[
                p("Threshold", -60.0, 0.0, -18.0, 0.0, "dB", false),
                p("Ratio", 1.0, 20.0, 4.0, 1.0, ":1", true),
                p("Attack", 0.1, 200.0, 10.0, 10.0, "ms", true),
                p("Release", 5.0, 2000.0, 150.0, 150.0, "ms", true),
                p("Makeup", 0.0, 24.0, 0.0, 0.0, "dB", false),
            ] },
            AudioFxKind::Limiter => const { &[
                p("Ceiling", -24.0, 0.0, -1.0, 0.0, "dB", false),
                p("Release", 1.0, 1000.0, 50.0, 50.0, "ms", true),
            ] },
            AudioFxKind::Gate => const { &[
                p("Threshold", -80.0, 0.0, -40.0, -80.0, "dB", false),
                p("Range", 0.0, 80.0, 60.0, 0.0, "dB", false),
                p("Attack", 0.1, 50.0, 1.0, 1.0, "ms", true),
                p("Hold", 0.0, 500.0, 50.0, 50.0, "ms", false),
                p("Release", 5.0, 2000.0, 100.0, 100.0, "ms", true),
            ] },
            AudioFxKind::Distortion => const { &[
                p("Drive", 0.0, 48.0, 12.0, 0.0, "dB", false),
                p("Tone", 500.0, 20000.0, 6000.0, 20000.0, "Hz", true),
                p("Output", -24.0, 6.0, -6.0, 0.0, "dB", false),
                p("Mix", 0.0, 100.0, 100.0, 0.0, "%", false),
            ] },
            AudioFxKind::Chorus => const { &[
                p("Rate", 0.05, 5.0, 0.8, 0.8, "Hz", true),
                p("Depth", 0.0, 10.0, 3.0, 3.0, "ms", false),
                p("Delay", 1.0, 30.0, 12.0, 12.0, "ms", false),
                p("Feedback", 0.0, 90.0, 0.0, 0.0, "%", false),
                p("Mix", 0.0, 100.0, 50.0, 0.0, "%", false),
            ] },
            AudioFxKind::Bitcrusher => const { &[
                p("Bits", 1.0, 16.0, 8.0, 16.0, "bit", false),
                p("Downsample", 1.0, 32.0, 4.0, 1.0, "×", true),
                p("Mix", 0.0, 100.0, 100.0, 0.0, "%", false),
            ] },
        }
    }
}

/// One audio effect in a cue's chain.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct AudioEffect {
    /// Off = bypassed.
    pub enabled: bool,
    /// What it does.
    pub kind: AudioFxKind,
    /// Its numbers, as described by [`AudioFxKind::params`] (unused slots are 0).
    pub params: [f32; MAX_PARAMS],
}

impl Default for AudioEffect {
    fn default() -> Self {
        Self::new(AudioFxKind::default())
    }
}

impl AudioEffect {
    /// A new effect of `kind` with its default settings.
    pub fn new(kind: AudioFxKind) -> Self {
        Self { enabled: true, kind, params: defaults(kind.params()) }
    }

    /// Value `i` clamped to its range (default if the kind has no such number).
    pub fn get(&self, i: usize) -> f32 {
        let specs = self.kind.params();
        specs.get(i).map_or(0.0, |s| {
            let v = self.params[i];
            if v.is_finite() { v.clamp(s.min, s.max) } else { s.default }
        })
    }
}

/// The type of a video effect.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum VideoFxKind {
    /// Brightness, contrast and saturation.
    #[default]
    ColorControls,
    /// Exposure in stops and gamma.
    Exposure,
    /// Rotate hues around the colour wheel.
    Hue,
    /// Monochrome in a colour.
    Colorize,
    /// Old-photo brown tone.
    Sepia,
    /// Negative picture.
    Invert,
    /// Fewer colour levels.
    Posterize,
    /// Black and white at a brightness level.
    Threshold,
    /// Darkened edges.
    Vignette,
    /// Soft focus.
    Blur,
    /// Big square pixels.
    Pixellate,
    /// Crisper edges (unsharp mask).
    Sharpen,
    /// Make one colour transparent (green or blue screen).
    ChromaKey,
}

impl VideoFxKind {
    /// Every type, for menus.
    pub const ALL: [VideoFxKind; 13] = [
        VideoFxKind::ColorControls,
        VideoFxKind::Exposure,
        VideoFxKind::Hue,
        VideoFxKind::Colorize,
        VideoFxKind::Sepia,
        VideoFxKind::Invert,
        VideoFxKind::Posterize,
        VideoFxKind::Threshold,
        VideoFxKind::Vignette,
        VideoFxKind::Blur,
        VideoFxKind::Pixellate,
        VideoFxKind::Sharpen,
        VideoFxKind::ChromaKey,
    ];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            VideoFxKind::ColorControls => "Colour controls",
            VideoFxKind::Exposure => "Exposure",
            VideoFxKind::Hue => "Hue",
            VideoFxKind::Colorize => "Colourise",
            VideoFxKind::Sepia => "Sepia",
            VideoFxKind::Invert => "Invert",
            VideoFxKind::Posterize => "Posterise",
            VideoFxKind::Threshold => "Threshold",
            VideoFxKind::Vignette => "Vignette",
            VideoFxKind::Blur => "Blur",
            VideoFxKind::Pixellate => "Pixellate",
            VideoFxKind::Sharpen => "Sharpen",
            VideoFxKind::ChromaKey => "Chroma key",
        }
    }

    /// The numbers this effect takes, in `params` order.
    pub fn params(self) -> &'static [ParamSpec] {
        match self {
            VideoFxKind::ColorControls => const { &[
                p("Brightness", -1.0, 1.0, 0.0, 0.0, "", false),
                p("Contrast", 0.0, 4.0, 1.0, 1.0, "×", false),
                p("Saturation", 0.0, 4.0, 1.0, 1.0, "×", false),
            ] },
            VideoFxKind::Exposure => const { &[p("Exposure", -4.0, 4.0, 0.0, 0.0, "stops", false), p("Gamma", 0.2, 5.0, 1.0, 1.0, "", true)] },
            VideoFxKind::Hue => const { &[p("Angle", -180.0, 180.0, 60.0, 0.0, "°", false)] },
            VideoFxKind::Colorize => const { &[p("Amount", 0.0, 100.0, 100.0, 0.0, "%", false)] },
            VideoFxKind::Sepia | VideoFxKind::Invert => const { &[p("Amount", 0.0, 100.0, 100.0, 0.0, "%", false)] },
            VideoFxKind::Posterize => const { &[p("Levels", 2.0, 256.0, 6.0, 256.0, "", true)] },
            VideoFxKind::Threshold => const { &[p("Level", 0.0, 1.0, 0.5, 0.5, "", false), p("Amount", 0.0, 100.0, 100.0, 0.0, "%", false)] },
            VideoFxKind::Vignette => const { &[
                p("Radius", 0.0, 1.5, 0.75, 0.75, "", false),
                p("Softness", 0.01, 1.0, 0.45, 0.45, "", false),
                p("Amount", 0.0, 100.0, 80.0, 0.0, "%", false),
            ] },
            VideoFxKind::Blur => const { &[p("Radius", 0.0, 64.0, 8.0, 0.0, "px", false)] },
            VideoFxKind::Pixellate => const { &[p("Size", 1.0, 128.0, 16.0, 1.0, "px", true)] },
            VideoFxKind::Sharpen => const { &[p("Amount", 0.0, 400.0, 100.0, 0.0, "%", false), p("Radius", 0.5, 8.0, 1.5, 1.5, "px", false)] },
            VideoFxKind::ChromaKey => const { &[
                p("Threshold", 0.0, 100.0, 25.0, 0.0, "%", false),
                p("Softness", 0.0, 100.0, 10.0, 0.0, "%", false),
                p("Spill", 0.0, 100.0, 50.0, 0.0, "%", false),
            ] },
        }
    }

    /// True if the effect has a colour setting.
    pub fn has_color(self) -> bool {
        matches!(self, VideoFxKind::Colorize | VideoFxKind::ChromaKey)
    }

    /// True for effects that change where the picture is sampled (they run before the colour
    /// effects, whatever their place in the list).
    pub fn is_spatial(self) -> bool {
        matches!(self, VideoFxKind::Blur | VideoFxKind::Pixellate | VideoFxKind::Sharpen)
    }

    /// Colour setting of a new effect.
    fn default_color(self) -> [u8; 3] {
        match self {
            VideoFxKind::ChromaKey => [0, 255, 0],
            _ => [255, 170, 60],
        }
    }
}

/// One video effect in a layer's stack.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct VideoEffect {
    /// Off = bypassed.
    pub enabled: bool,
    /// What it does.
    pub kind: VideoFxKind,
    /// Its numbers, as described by [`VideoFxKind::params`] (unused slots are 0).
    pub params: [f32; MAX_PARAMS],
    /// Colour setting (Colourise, Chroma key), sRGB.
    pub color: [u8; 3],
}

impl Default for VideoEffect {
    fn default() -> Self {
        Self::new(VideoFxKind::default())
    }
}

fn defaults(specs: &[ParamSpec]) -> [f32; MAX_PARAMS] {
    std::array::from_fn(|i| specs.get(i).map_or(0.0, |s| s.default))
}

impl VideoEffect {
    /// A new effect of `kind` with its default settings.
    pub fn new(kind: VideoFxKind) -> Self {
        Self { enabled: true, kind, params: defaults(kind.params()), color: kind.default_color() }
    }

    /// Value `i` clamped to its range.
    pub fn get(&self, i: usize) -> f32 {
        let specs = self.kind.params();
        specs.get(i).map_or(0.0, |s| {
            let v = self.params[i];
            if v.is_finite() { v.clamp(s.min, s.max) } else { s.default }
        })
    }

    /// The same effect set so it changes nothing (its colour stays).
    pub fn neutral(&self) -> Self {
        let specs = self.kind.params();
        Self { enabled: true, params: std::array::from_fn(|i| specs.get(i).map_or(0.0, |s| s.neutral)), ..*self }
    }

    /// What this effect does as an enabled one: itself, or its neutral form when bypassed.
    fn active(&self) -> Self {
        if self.enabled { *self } else { self.neutral() }
    }

    /// Interpolate towards `to` (same kind) at progress `t`.
    fn lerp(&self, to: &Self, t: f32) -> Self {
        let l = |a: f32, b: f32| a + (b - a) * t;
        Self {
            enabled: true,
            kind: self.kind,
            params: std::array::from_fn(|i| l(self.params[i], to.params[i])),
            color: std::array::from_fn(|i| l(self.color[i] as f32, to.color[i] as f32).round().clamp(0.0, 255.0) as u8),
        }
    }
}

/// A video effect stack part-way through a fade from `from` to `to` (progress `t` in 0..1).
/// Effects of the same kind at the same place blend their settings; any other effect fades
/// out of (or into) its neutral setting. At `t = 1` the result is `to`.
pub fn lerp_video_fx(from: &[VideoEffect], to: &[VideoEffect], t: f32) -> Vec<VideoEffect> {
    if t >= 1.0 {
        return to.to_vec();
    }
    let t = t.max(0.0);
    let mut out = Vec::new();
    for i in 0..from.len().max(to.len()) {
        match (from.get(i), to.get(i)) {
            (Some(a), Some(b)) if a.kind == b.kind => out.push(a.active().lerp(&b.active(), t)),
            (a, b) => {
                if let Some(a) = a {
                    out.push(a.active().lerp(&a.neutral(), t));
                }
                if let Some(b) = b {
                    out.push(b.neutral().lerp(&b.active(), t));
                }
            }
        }
    }
    out
}

/// Floats in the shader's effect uniform (see [`video_fx_uniform`]).
pub const VIDEO_FX_FLOATS: usize = 8 + MAX_VIDEO_FX * 8;

/// Shader code of a colour effect (0 = empty slot).
fn shader_code(kind: VideoFxKind) -> f32 {
    match kind {
        VideoFxKind::ColorControls => 1.0,
        VideoFxKind::Exposure => 2.0,
        VideoFxKind::Hue => 3.0,
        VideoFxKind::Colorize => 4.0,
        VideoFxKind::Sepia => 5.0,
        VideoFxKind::Invert => 6.0,
        VideoFxKind::Posterize => 7.0,
        VideoFxKind::Threshold => 8.0,
        VideoFxKind::Vignette => 9.0,
        VideoFxKind::ChromaKey => 10.0,
        VideoFxKind::Blur | VideoFxKind::Pixellate | VideoFxKind::Sharpen => 0.0,
    }
}

/// The layer shader's uniform for an effect stack (`None` when nothing is enabled). Layout, in
/// vec4s: spatial (pixel size, blur radius, sharpen amount, sharpen radius; all in source
/// pixels), meta (colour effect count), then two vec4s per colour effect in list order:
/// (code, value 0, value 1, value 2) and (red, green, blue, 0) with percentages as fractions.
pub fn video_fx_uniform(fx: &[VideoEffect]) -> Option<[f32; VIDEO_FX_FLOATS]> {
    let mut u = [0f32; VIDEO_FX_FLOATS];
    let (mut pixel, mut blur2, mut sharpen, mut sharpen_r) = (1f32, 0f32, 0f32, 0f32);
    let mut n = 0;
    let mut any = false;
    for e in fx.iter().filter(|e| e.enabled) {
        let specs = e.kind.params();
        // Percent values go to the shader as 0..1.
        let v = |i: usize| if specs.get(i).is_some_and(|s| s.unit == "%") { e.get(i) / 100.0 } else { e.get(i) };
        match e.kind {
            VideoFxKind::Pixellate => pixel = pixel.max(v(0)),
            VideoFxKind::Blur => blur2 += v(0) * v(0),
            VideoFxKind::Sharpen => {
                sharpen += v(0);
                sharpen_r = sharpen_r.max(v(1));
            }
            kind if n < MAX_VIDEO_FX => {
                let s = 8 + n * 8;
                u[s..s + 4].copy_from_slice(&[shader_code(kind), v(0), v(1), v(2)]);
                u[s + 4..s + 7].copy_from_slice(&e.color.map(|c| c as f32 / 255.0));
                n += 1;
            }
            _ => continue,
        }
        any = true;
    }
    if !any {
        return None;
    }
    u[..4].copy_from_slice(&[pixel, blur2.sqrt(), sharpen, sharpen_r.max(0.5)]);
    u[4] = n as f32;
    Some(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fit_their_ranges() {
        for k in AudioFxKind::ALL {
            assert!(k.params().len() <= MAX_PARAMS);
            for s in k.params() {
                assert!(s.min <= s.default && s.default <= s.max, "{k:?} {}", s.name);
            }
        }
        for k in VideoFxKind::ALL {
            assert!(k.params().len() <= MAX_PARAMS);
            for s in k.params() {
                assert!(s.min <= s.default && s.default <= s.max, "{k:?} {}", s.name);
                assert!(s.min <= s.neutral && s.neutral <= s.max, "{k:?} {}", s.name);
            }
        }
    }

    #[test]
    fn video_fades_blend_matching_effects_and_fade_others_in() {
        let blur = VideoEffect::new(VideoFxKind::Blur);
        let mut sharper = blur;
        sharper.params[0] = 20.0;
        let mid = lerp_video_fx(&[blur], &[sharper], 0.5);
        assert_eq!(mid.len(), 1);
        assert!((mid[0].params[0] - 14.0).abs() < 1e-4);
        // A new effect starts neutral: half way through, half its amount.
        let sepia = VideoEffect::new(VideoFxKind::Sepia);
        let mid = lerp_video_fx(&[], &[sepia], 0.5);
        assert!((mid[0].params[0] - 50.0).abs() < 1e-4);
        // A disabled effect in the target fades to neutral.
        let off = VideoEffect { enabled: false, ..sepia };
        let mid = lerp_video_fx(&[sepia], &[off], 0.25);
        assert!((mid[0].params[0] - 75.0).abs() < 1e-4);
        assert_eq!(lerp_video_fx(&[sepia], &[off], 1.0), vec![off]);
    }

    #[test]
    fn shader_uniform_packs_spatial_and_colour_effects() {
        assert!(video_fx_uniform(&[]).is_none());
        assert!(video_fx_uniform(&[VideoEffect { enabled: false, ..Default::default() }]).is_none());
        let u = video_fx_uniform(&[
            VideoEffect::new(VideoFxKind::Blur),
            VideoEffect::new(VideoFxKind::Sepia),
            VideoEffect::new(VideoFxKind::ChromaKey),
        ])
        .unwrap();
        assert_eq!(u[1], 8.0); // blur radius
        assert_eq!(u[4], 2.0); // two colour effects
        assert_eq!(&u[8..10], &[5.0, 1.0]); // sepia at 100 %
        assert_eq!(u[16], 10.0);
        assert_eq!(&u[20..23], &[0.0, 1.0, 0.0]); // key colour
    }
}
