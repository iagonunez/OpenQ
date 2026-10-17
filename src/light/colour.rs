//! Colour mixing in CIE 1931 xy: the colours a fixture can make (its gamut, spanned by its
//! emitters) and the emitter levels for a chosen colour.
//!
//! Emitter colours come from the fixture type ([`super::model::Physical::emitters`], read from
//! GDTF files) when it has them, else from typical LED values ([`typical`]). CMY fixtures mix
//! subtractively from a white lamp: their gamut is approximated by the sRGB primaries.

use super::model::{Attribute, Emitter, FixtureType};

/// D65, the white of the colour field.
pub const WHITE_POINT: [f32; 2] = [0.3127, 0.3290];

/// CIE 1931 2° spectral locus (380–700 nm), the edge of the colours anyone can see.
pub const LOCUS: [[f32; 2]; 30] = [
    [0.1741, 0.0050],
    [0.1733, 0.0048],
    [0.1714, 0.0051],
    [0.1644, 0.0109],
    [0.1566, 0.0177],
    [0.1440, 0.0297],
    [0.1241, 0.0578],
    [0.1096, 0.0868],
    [0.0913, 0.1327],
    [0.0687, 0.2007],
    [0.0454, 0.2950],
    [0.0235, 0.4127],
    [0.0082, 0.5384],
    [0.0039, 0.6548],
    [0.0139, 0.7502],
    [0.0389, 0.8120],
    [0.0743, 0.8338],
    [0.1142, 0.8262],
    [0.1547, 0.8059],
    [0.2296, 0.7543],
    [0.3016, 0.6923],
    [0.3731, 0.6245],
    [0.4441, 0.5547],
    [0.5125, 0.4866],
    [0.5752, 0.4242],
    [0.6270, 0.3725],
    [0.6658, 0.3340],
    [0.6915, 0.3083],
    [0.7190, 0.2809],
    [0.7347, 0.2653],
];

/// A typical LED emitter for a colour attribute (xy, luminance at full relative to the green),
/// balanced so red + green + blue at full is close to white.
pub fn typical(a: Attribute) -> Option<Emitter> {
    let (x, y, luminance) = match a {
        Attribute::Red => (0.700, 0.299, 0.26),
        Attribute::Green => (0.170, 0.700, 0.70),
        Attribute::Blue => (0.135, 0.040, 0.05),
        Attribute::White => (0.3127, 0.3290, 1.0),
        Attribute::Amber => (0.570, 0.425, 0.40),
        Attribute::Lime => (0.410, 0.560, 0.80),
        _ => return None,
    };
    Some(Emitter { attribute: a, x, y, luminance })
}

/// sRGB primaries, the approximate gamut of CMY flags (as red, green, blue light let through).
const CMY_PRIMARIES: [(Attribute, f32, f32, f32); 3] =
    [(Attribute::Cyan, 0.640, 0.330, 0.2126), (Attribute::Magenta, 0.300, 0.600, 0.7152), (Attribute::Yellow, 0.150, 0.060, 0.0722)];

/// How a fixture mixes colour.
#[derive(Clone, Debug, PartialEq)]
pub struct Mixer {
    /// Emitters the colour picker drives (red, green, blue first). For CMY: the light each flag
    /// takes away (cyan = red …), set as `1 - level`.
    pub emitters: Vec<Emitter>,
    /// Other emitters (white) counted in the current colour but left alone.
    pub extra: Vec<Emitter>,
    /// CMY flags.
    pub subtractive: bool,
    /// Every emitter colour comes from the fixture type (not typical values).
    pub measured: bool,
}

/// The mixer of a unit with attributes `attrs` of type `ty`: red/green/blue (with amber and lime
/// when there), or cyan/magenta/yellow. None without either.
pub fn mixer(ty: Option<&FixtureType>, attrs: &[Attribute]) -> Option<Mixer> {
    let has = |a: &Attribute| attrs.contains(a);
    let profile = |a: Attribute| ty.and_then(|t| t.physical.emitters.iter().find(|e| e.attribute == a && e.is_valid()).copied());
    if [Attribute::Red, Attribute::Green, Attribute::Blue].iter().all(has) {
        let mut measured = true;
        let mut emitter = |a: Attribute| {
            profile(a).or_else(|| {
                measured = false;
                typical(a)
            })
        };
        let emitters: Vec<Emitter> = [Attribute::Red, Attribute::Green, Attribute::Blue, Attribute::Amber, Attribute::Lime]
            .into_iter()
            .filter(|a| has(a))
            .filter_map(&mut emitter)
            .collect();
        let extra = [Attribute::White].into_iter().filter(|a| has(a)).filter_map(|a| profile(a).or_else(|| typical(a))).collect();
        return Some(Mixer { emitters, extra, subtractive: false, measured });
    }
    if [Attribute::Cyan, Attribute::Magenta, Attribute::Yellow].iter().all(has) {
        let emitters = CMY_PRIMARIES.iter().map(|&(attribute, x, y, luminance)| Emitter { attribute, x, y, luminance }).collect();
        return Some(Mixer { emitters, extra: Vec::new(), subtractive: true, measured: false });
    }
    None
}

/// CIE XYZ of an emitter at full.
fn xyz(e: &Emitter) -> [f64; 3] {
    let (x, y, l) = (e.x as f64, e.y as f64, e.luminance as f64);
    [l * x / y, l, l * (1.0 - x - y) / y]
}

fn cross(o: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Convex hull, counter-clockwise (x right, y up).
pub fn hull(points: &[[f32; 2]]) -> Vec<[f32; 2]> {
    let mut p: Vec<[f32; 2]> = points.to_vec();
    p.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    p.dedup();
    if p.len() < 3 {
        return p;
    }
    let mut out: Vec<[f32; 2]> = Vec::new();
    for pass in 0..2 {
        let start = out.len();
        let pts: Box<dyn Iterator<Item = &[f32; 2]>> = if pass == 0 { Box::new(p.iter()) } else { Box::new(p.iter().rev()) };
        for &q in pts {
            while out.len() >= start + 2 && cross(out[out.len() - 2], out[out.len() - 1], q) <= 0.0 {
                out.pop();
            }
            out.push(q);
        }
        out.pop();
    }
    out
}

/// `p` if it's inside the convex polygon `poly` (counter-clockwise), else the nearest point on
/// its edge.
pub fn clamp_into(poly: &[[f32; 2]], p: [f32; 2]) -> [f32; 2] {
    let n = poly.len();
    if n == 0 || (n >= 3 && (0..n).all(|i| cross(poly[i], poly[(i + 1) % n], p) >= 0.0)) {
        return p;
    }
    let mut best = (f32::MAX, p);
    for i in 0..n {
        let (a, b) = (poly[i], poly[(i + 1) % n]);
        let ab = [b[0] - a[0], b[1] - a[1]];
        let len = ab[0] * ab[0] + ab[1] * ab[1];
        let k = if len > 0.0 { (((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1]) / len).clamp(0.0, 1.0) } else { 0.0 };
        let q = [a[0] + ab[0] * k, a[1] + ab[1] * k];
        let d = (q[0] - p[0]).powi(2) + (q[1] - p[1]).powi(2);
        if d < best.0 {
            best = (d, q);
        }
    }
    best.1
}

/// Solve `m · w = t` (columns of `m` are the vectors); None when singular.
fn solve(m: [[f64; 3]; 3], t: [f64; 3]) -> Option<[f64; 3]> {
    let det = |c: [[f64; 3]; 3]| {
        c[0][0] * (c[1][1] * c[2][2] - c[2][1] * c[1][2]) - c[1][0] * (c[0][1] * c[2][2] - c[2][1] * c[0][2]) + c[2][0] * (c[0][1] * c[1][2] - c[1][1] * c[0][2])
    };
    let d = det(m);
    if d.abs() < 1e-12 {
        return None;
    }
    let mut w = [0.0; 3];
    for (i, wi) in w.iter_mut().enumerate() {
        let mut c = m;
        c[i] = t;
        *wi = det(c) / d;
    }
    Some(w)
}

impl Mixer {
    /// The colours it can make: the hull of its emitters (xy, counter-clockwise).
    pub fn gamut(&self) -> Vec<[f32; 2]> {
        hull(&self.emitters.iter().map(|e| [e.x, e.y]).collect::<Vec<_>>())
    }

    /// Light of emitter `i` at `level` (CMY flags let through `1 - level`).
    fn output(&self, level: f32) -> f32 {
        if self.subtractive { 1.0 - level } else { level }
    }

    /// The colour (xy) made by the levels `level` gives (missing = 0); None when dark.
    pub fn chromaticity(&self, level: impl Fn(Attribute) -> Option<f32>) -> Option<[f32; 2]> {
        let mut sum = [0.0f64; 3];
        let drive = self.emitters.iter().map(|e| (e, self.output(level(e.attribute).unwrap_or(0.0).clamp(0.0, 1.0))));
        for (e, k) in drive.chain(self.extra.iter().map(|e| (e, level(e.attribute).unwrap_or(0.0).clamp(0.0, 1.0)))) {
            let c = xyz(e);
            for i in 0..3 {
                sum[i] += c[i] * k as f64;
            }
        }
        let total = sum[0] + sum[1] + sum[2];
        (total > 1e-9).then(|| [(sum[0] / total) as f32, (sum[1] / total) as f32])
    }

    /// Levels of the driven emitters for colour `xy` (brought into the gamut first), the
    /// brightest at full. Mixes from three emitters: red/green/blue when they can make it.
    pub fn levels(&self, xy: [f32; 2]) -> Vec<(Attribute, f32)> {
        let n = self.emitters.len();
        let p = clamp_into(&self.gamut(), xy);
        let target = xyz(&Emitter { attribute: Attribute::Dimmer, x: p[0], y: p[1].max(1e-4), luminance: 1.0 });
        let cols: Vec<[f64; 3]> = self.emitters.iter().map(xyz).collect();
        let mut w = vec![0.0f64; n];
        let mut found = false;
        'search: for a in 0..n {
            for b in a + 1..n {
                for c in b + 1..n {
                    if let Some(s) = solve([cols[a], cols[b], cols[c]], target)
                        && s.iter().all(|x| *x >= -1e-6)
                    {
                        w[a] = s[0].max(0.0);
                        w[b] = s[1].max(0.0);
                        w[c] = s[2].max(0.0);
                        found = true;
                        break 'search;
                    }
                }
            }
        }
        if !found {
            // Rounding at the gamut edge: the nearest emitter.
            let near = (0..n).min_by(|i, j| {
                let d = |k: usize| (self.emitters[k].x - p[0]).powi(2) + (self.emitters[k].y - p[1]).powi(2);
                d(*i).total_cmp(&d(*j))
            });
            if let Some(i) = near {
                w[i] = 1.0;
            }
        }
        let max = w.iter().cloned().fold(0.0, f64::max);
        self.emitters
            .iter()
            .zip(w)
            .map(|(e, k)| {
                let k = if max > 0.0 { (k / max) as f32 } else { 0.0 };
                (e.attribute, self.output(k.clamp(0.0, 1.0)))
            })
            .collect()
    }
}

/// Display colour (sRGB, 0..1, brightest channel full) of chromaticity `xy`; colours outside
/// sRGB are clipped.
pub fn display(xy: [f32; 2]) -> [f32; 3] {
    let (x, y) = (xy[0] as f64, (xy[1] as f64).max(1e-4));
    let (xx, zz) = (x / y, (1.0 - x - y) / y);
    let lin = [3.2406 * xx - 1.5372 - 0.4986 * zz, -0.9689 * xx + 1.8758 + 0.0415 * zz, 0.0557 * xx - 0.2040 + 1.0570 * zz].map(|c| c.max(0.0));
    let m = lin.iter().cloned().fold(0.0, f64::max).max(1e-9);
    lin.map(|c| {
        let c = c / m;
        (if c <= 0.0031308 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }) as f32
    })
}

/// Chromaticity of an sRGB colour (0..1); None for black.
pub fn srgb_to_xy(rgb: [f32; 3]) -> Option<[f32; 2]> {
    let lin = rgb.map(|c| {
        let c = c.clamp(0.0, 1.0) as f64;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    });
    let x = 0.4124 * lin[0] + 0.3576 * lin[1] + 0.1805 * lin[2];
    let y = 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
    let z = 0.0193 * lin[0] + 0.1192 * lin[1] + 0.9505 * lin[2];
    let total = x + y + z;
    (total > 1e-9).then(|| [(x / total) as f32, (y / total) as f32])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb() -> Mixer {
        mixer(None, &[Attribute::Dimmer, Attribute::Red, Attribute::Green, Attribute::Blue]).unwrap()
    }

    #[test]
    fn typical_rgb_full_is_near_white() {
        let m = rgb();
        assert!(!m.measured && !m.subtractive);
        let w = m.chromaticity(|_| Some(1.0)).unwrap();
        assert!((w[0] - WHITE_POINT[0]).abs() < 0.03 && (w[1] - WHITE_POINT[1]).abs() < 0.03, "{w:?}");
    }

    #[test]
    fn levels_make_the_colour_asked_for() {
        let m = rgb();
        for xy in [[0.3127, 0.329], [0.5, 0.35], [0.25, 0.5], [0.2, 0.15]] {
            let lv = m.levels(xy);
            assert!(lv.iter().any(|(_, v)| (*v - 1.0).abs() < 1e-5), "brightest at full: {lv:?}");
            let back = m.chromaticity(|a| lv.iter().find(|x| x.0 == a).map(|x| x.1)).unwrap();
            assert!((back[0] - xy[0]).abs() < 1e-3 && (back[1] - xy[1]).abs() < 1e-3, "{xy:?} → {back:?}");
        }
        // Outside the gamut: the nearest colour it can make (on the red–green edge, no blue).
        let lv = m.levels([0.6, 0.5]);
        assert!(lv.iter().find(|x| x.0 == Attribute::Blue).unwrap().1 < 1e-4, "{lv:?}");
    }

    #[test]
    fn amber_widens_the_gamut_and_mixes() {
        let m = mixer(None, &[Attribute::Red, Attribute::Green, Attribute::Blue, Attribute::Amber]).unwrap();
        assert_eq!(m.gamut().len(), 4);
        let amber = [0.57, 0.425];
        let lv = m.levels(amber);
        assert!(lv.iter().find(|x| x.0 == Attribute::Amber).unwrap().1 > 0.99, "{lv:?}");
    }

    #[test]
    fn cmy_and_profile_emitters() {
        let m = mixer(None, &[Attribute::Cyan, Attribute::Magenta, Attribute::Yellow]).unwrap();
        assert!(m.subtractive);
        // Open white: no flags in.
        let lv = m.levels(WHITE_POINT);
        assert!(lv.iter().all(|(_, v)| *v < 0.1), "{lv:?}");
        let mut ty = FixtureType::default();
        ty.physical.emitters = vec![
            Emitter { attribute: Attribute::Red, x: 0.69, y: 0.30, luminance: 0.3 },
            Emitter { attribute: Attribute::Green, x: 0.20, y: 0.72, luminance: 0.7 },
            Emitter { attribute: Attribute::Blue, x: 0.14, y: 0.05, luminance: 0.06 },
        ];
        let m = mixer(Some(&ty), &[Attribute::Red, Attribute::Green, Attribute::Blue]).unwrap();
        assert!(m.measured);
        assert_eq!(m.emitters[1].x, 0.20);
        assert!(mixer(None, &[Attribute::Red, Attribute::Green]).is_none());
    }

    #[test]
    fn hull_and_clamp() {
        let h = hull(&[[0.0, 0.0], [1.0, 0.0], [0.5, 0.2], [0.0, 1.0]]);
        assert_eq!(h.len(), 3);
        assert_eq!(clamp_into(&h, [0.2, 0.2]), [0.2, 0.2]);
        let c = clamp_into(&h, [1.0, 1.0]);
        assert!((c[0] - 0.5).abs() < 1e-5 && (c[1] - 0.5).abs() < 1e-5);
        let xy = srgb_to_xy([1.0, 1.0, 1.0]).unwrap();
        assert!((xy[0] - 0.3127).abs() < 1e-3);
        assert!(srgb_to_xy([0.0; 3]).is_none());
    }
}
