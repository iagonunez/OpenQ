//! Focus points: aiming moving lights at places on stage, and finding those places (or the rig)
//! again in another venue.
//!
//! A [`FocusPoint`](super::model::FocusPoint) is a named place in the patch's space (metres, x
//! right, y up, z towards the audience). Cues, presets and the programmer refer to it with
//! [`Value::Point`](super::model::Value::Point); compiling turns that into pan and tilt for each
//! fixture from the fixture's position, rotation and pan/tilt ranges ([`aim`]), so moving a point
//! or a fixture re-aims every cue that uses it.
//!
//! Angles follow GDTF/MVR (what visualisers draw): with no rotation and pan/tilt in the middle of
//! their DMX range the beam points straight down; pan turns about the fixture's vertical axis, tilt
//! about the yoke's x axis, both right-handed, and the physical range ([`Physical::pan_range`],
//! [`Physical::tilt_range`]) is centred on the middle of the DMX range. Inverted pan/tilt
//! ([`Fixture::invert_pan`]) flips the DMX value.
//!
//! Calibration, from fixtures the operator aimed by hand at the real places ([`Aim`]):
//! * [`locate`] — where a point really is: where the beams cross (two beams or more), or where
//!   one beam meets the point's height.
//! * [`rig_offset`] — how far a group of fixtures (a truss) really is from where the patch has
//!   it, along the chosen [`Axes`]. One beam gives two equations, so height and depth come from
//!   one fixture aimed off the centre line; all three axes need two beams (another fixture of the
//!   group, or another point). [`shift`] then moves the fixtures (and the plot bar they hang on).

use super::model::{Fixture, FixtureId, Lighting, Physical, ShapeKind};

type V = [f64; 3];
type M = [[f64; 3]; 3];

fn v64(v: [f32; 3]) -> V {
    v.map(f64::from)
}

fn sub(a: V, b: V) -> V {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: V, b: V) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn norm(a: V) -> f64 {
    dot(a, a).sqrt()
}

fn mat_vec(m: &M, v: V) -> V {
    [dot(m[0], v), dot(m[1], v), dot(m[2], v)]
}

/// `mᵀ v`.
fn mat_t_vec(m: &M, v: V) -> V {
    [0, 1, 2].map(|j| (0..3).map(|i| m[i][j] * v[i]).sum())
}

fn mul(a: &M, b: &M) -> M {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

/// A fixture's rotation (degrees about x, y, z, applied as heading (y) · tilt (x) · roll (z), as
/// in the MVR export).
fn rotation(deg: [f32; 3]) -> M {
    let [x, y, z] = v64(deg).map(f64::to_radians);
    let rx = [[1.0, 0.0, 0.0], [0.0, x.cos(), -x.sin()], [0.0, x.sin(), x.cos()]];
    let ry = [[y.cos(), 0.0, y.sin()], [0.0, 1.0, 0.0], [-y.sin(), 0.0, y.cos()]];
    let rz = [[z.cos(), -z.sin(), 0.0], [z.sin(), z.cos(), 0.0], [0.0, 0.0, 1.0]];
    mul(&ry, &mul(&rx, &rz))
}

/// Stored value (0..=1) → physical angle, degrees.
fn to_angle(v: f32, range: f32, invert: bool) -> f64 {
    let d = if invert { 1.0 - v } else { v } as f64;
    (d - 0.5) * range as f64
}

/// Physical angle → stored value.
fn from_angle(a: f64, range: f32, invert: bool) -> f32 {
    let d = (a / range as f64 + 0.5).clamp(0.0, 1.0) as f32;
    if invert { 1.0 - d } else { d }
}

fn ranges(p: &Physical) -> Option<(f32, f32)> {
    let ok = |r: f32| r.is_finite() && r > 0.0;
    (ok(p.pan_range) && ok(p.tilt_range)).then_some((p.pan_range, p.tilt_range))
}

/// Pan and tilt (0..=1, as stored) that aim fixture `f` (of a type with physical data `p`) at
/// `target`. None if the type has no pan/tilt range or the target is where the fixture is. A
/// target out of reach gets the nearest aim within the ranges. Of the ways to reach a target
/// (the head turned over, pan a full turn round) the one with pan nearest the middle wins, then
/// the one with less tilt, so neighbouring points give neighbouring aims.
pub fn aim(f: &Fixture, p: &Physical, target: [f32; 3]) -> Option<(f32, f32)> {
    let (pr, tr) = ranges(p)?;
    let d = mat_t_vec(&rotation(f.rotation), sub(v64(target), v64(f.position)));
    let len = norm(d);
    if len.is_nan() || len <= 1e-6 {
        return None;
    }
    let d = d.map(|x| x / len);
    // Beam = (−sin T sin P, −cos T, −sin T cos P) in the fixture's axes.
    let t0 = (-d[1]).clamp(-1.0, 1.0).acos().to_degrees();
    let p0 = if d[0].hypot(d[2]) < 1e-9 { 0.0 } else { (-d[0]).atan2(-d[2]).to_degrees() };
    let (hp, ht) = (pr as f64 / 2.0, tr as f64 / 2.0);
    let mut best: Option<(f64, f64, f64)> = None;
    for (pan, tilt) in [(p0, t0), (p0 + 180.0, -t0)] {
        for k in -3..=3 {
            let pan = pan + 360.0 * k as f64;
            let excess = (pan.abs() - hp).max(0.0) + (tilt.abs() - ht).max(0.0);
            let score = excess * 1e6 + pan.abs() + tilt.abs() * 1e-3;
            if best.is_none_or(|b| score < b.0 - 1e-9) {
                best = Some((score, pan.clamp(-hp, hp), tilt.clamp(-ht, ht)));
            }
        }
    }
    let (_, pan, tilt) = best?;
    Some((from_angle(pan, pr, f.invert_pan), from_angle(tilt, tr, f.invert_tilt)))
}

/// The direction (unit vector, patch space) of fixture `f`'s beam with pan and tilt at `pan` and
/// `tilt` (0..=1, as stored). None if the type has no pan/tilt range.
pub fn beam(f: &Fixture, p: &Physical, pan: f32, tilt: f32) -> Option<[f64; 3]> {
    let (pr, tr) = ranges(p)?;
    let (pa, ta) = (to_angle(pan, pr, f.invert_pan).to_radians(), to_angle(tilt, tr, f.invert_tilt).to_radians());
    let local = [-ta.sin() * pa.sin(), -ta.cos(), -ta.sin() * pa.cos()];
    Some(mat_vec(&rotation(f.rotation), local))
}

/// A fixture aimed by hand at a known place: its beam (from where the patch has the fixture) and
/// the place it really lights.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aim {
    /// The fixture's position in the patch.
    pub from: [f64; 3],
    /// Its beam's direction (unit vector, [`beam`]).
    pub dir: [f64; 3],
    /// The place it was aimed at (the focus point's position, for [`rig_offset`]).
    pub target: [f64; 3],
}

impl Aim {
    /// Fixture `f` (type data `p`) aimed with pan/tilt `pan`, `tilt` at `target`.
    pub fn new(f: &Fixture, p: &Physical, pan: f32, tilt: f32, target: [f32; 3]) -> Option<Aim> {
        Some(Aim { from: v64(f.position), dir: beam(f, p, pan, tilt)?, target: v64(target) })
    }

    /// `v` with the part along the beam taken out (`(I − d dᵀ) v`).
    fn across(&self, v: V) -> V {
        let k = dot(self.dir, v);
        [v[0] - k * self.dir[0], v[1] - k * self.dir[1], v[2] - k * self.dir[2]]
    }
}

/// Why a calibration can't be worked out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusError {
    /// No fixture was aimed.
    NoAims,
    /// A beam points away from the place (up, or never reaching the point's height).
    NoReach,
    /// The beams don't tell the axes apart: aim another fixture (off the centre line), aim at
    /// another point, or solve fewer axes.
    Unclear,
}

/// A worked-out calibration: the result and how far the beams miss it (root mean square,
/// metres; 0 for one beam, which always fits).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit<T> {
    /// The point's position, or the offset of the fixtures.
    pub value: T,
    /// How far the beams pass from it, metres.
    pub miss: f32,
}

/// Smallest eigenvalue of a symmetric n × n matrix (n ≤ 3; Jacobi rotations).
#[allow(clippy::needless_range_loop)] // matrix maths reads best with indices
fn min_eigen(m: &M, n: usize) -> f64 {
    let mut a = *m;
    for _ in 0..32 {
        let (mut p, mut q, mut big) = (0, 1, 0.0);
        for i in 0..n {
            for j in i + 1..n {
                if a[i][j].abs() > big {
                    (p, q, big) = (i, j, a[i][j].abs());
                }
            }
        }
        if big < 1e-15 {
            break;
        }
        let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
        let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
        let t = if theta == 0.0 { 1.0 } else { t };
        let (c, s) = (1.0 / (t * t + 1.0).sqrt(), t / (t * t + 1.0).sqrt());
        let mut b = a;
        for k in 0..n {
            b[k][p] = c * a[k][p] - s * a[k][q];
            b[k][q] = s * a[k][p] + c * a[k][q];
        }
        a = b;
        for k in 0..n {
            b[p][k] = c * a[p][k] - s * a[q][k];
            b[q][k] = s * a[p][k] + c * a[q][k];
        }
        a = b;
    }
    (0..n).map(|i| a[i][i]).fold(f64::INFINITY, f64::min)
}

/// Solve `m x = r` (n × n, n ≤ 3; Gaussian elimination with pivoting).
#[allow(clippy::needless_range_loop)]
fn solve(m: &M, r: V, n: usize) -> Option<V> {
    let mut a = *m;
    let mut b = r;
    for c in 0..n {
        let p = (c..n).max_by(|i, j| a[*i][c].abs().total_cmp(&a[*j][c].abs()))?;
        if a[p][c].abs() < 1e-12 {
            return None;
        }
        a.swap(c, p);
        b.swap(c, p);
        for i in c + 1..n {
            let k = a[i][c] / a[c][c];
            for j in c..n {
                a[i][j] -= k * a[c][j];
            }
            b[i] -= k * b[c];
        }
    }
    let mut x = [0.0; 3];
    for i in (0..n).rev() {
        x[i] = (b[i] - (i + 1..n).map(|j| a[i][j] * x[j]).sum::<f64>()) / a[i][i];
    }
    Some(x)
}

/// Below this the beams are too close to parallel to cross usefully (1 − cos 4°).
const CROSSING: f64 = 0.0024;
/// Below this (smallest eigenvalue of the normal equations) an axis of [`rig_offset`] isn't
/// pinned down: one beam needs ~6° across the axes it solves.
const PINNED: f64 = 0.01;

/// Where the beams of `aims` meet (their `target`s are ignored): the point nearest every beam
/// for two beams or more that cross at an angle, else where the beams reach `height` (y, the
/// point's height in the patch: a mark on the floor stays on the floor), averaged.
#[allow(clippy::needless_range_loop)]
pub fn locate(aims: &[Aim], height: f32) -> Result<Fit<[f32; 3]>, FocusError> {
    if aims.is_empty() {
        return Err(FocusError::NoAims);
    }
    let mut a = [[0.0; 3]; 3];
    let mut r = [0.0; 3];
    for aim in aims {
        for (j, e) in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]].into_iter().enumerate() {
            let col = aim.across(e);
            for i in 0..3 {
                a[i][j] += col[i];
            }
        }
        let c = aim.across(aim.from);
        for i in 0..3 {
            r[i] += c[i];
        }
    }
    let crossing = aims.len() >= 2 && min_eigen(&a, 3) >= CROSSING;
    let p = match crossing.then(|| solve(&a, r, 3)).flatten() {
        Some(p) => p,
        None => {
            let mut sum = [0.0; 3];
            for aim in aims {
                let t = (height as f64 - aim.from[1]) / aim.dir[1];
                if !t.is_finite() || t <= 0.0 || aim.dir[1].abs() < 1e-3 {
                    return Err(FocusError::NoReach);
                }
                for i in 0..3 {
                    sum[i] += aim.from[i] + t * aim.dir[i];
                }
            }
            sum.map(|x| x / aims.len() as f64)
        }
    };
    if aims.iter().any(|aim| dot(sub(p, aim.from), aim.dir) <= 0.0) {
        return Err(FocusError::NoReach);
    }
    let miss = rms(aims.iter().map(|aim| norm(aim.across(sub(p, aim.from)))));
    Ok(Fit { value: p.map(|x| x as f32), miss })
}

fn rms(v: impl Iterator<Item = f64>) -> f32 {
    let (mut sum, mut n) = (0.0, 0usize);
    for x in v {
        sum += x * x;
        n += 1;
    }
    if n == 0 { 0.0 } else { (sum / n as f64).sqrt() as f32 }
}

/// Which axes [`rig_offset`] may move the fixtures along.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Axes {
    /// Height only (the trim changed).
    Height,
    /// Height and depth (up/down stage): what a truss usually changes between venues.
    #[default]
    HeightDepth,
    /// Height, depth and across the stage.
    All,
}

impl Axes {
    /// Every choice, in the order the UI lists them.
    pub const ALL: [Axes; 3] = [Axes::Height, Axes::HeightDepth, Axes::All];

    /// Shown name.
    pub fn label(self) -> &'static str {
        match self {
            Axes::Height => crate::i18n::t("Height"),
            Axes::HeightDepth => crate::i18n::t("Height and depth"),
            Axes::All => crate::i18n::t("Height, depth and side"),
        }
    }

    fn basis(self) -> &'static [V] {
        match self {
            Axes::Height => &[[0.0, 1.0, 0.0]],
            Axes::HeightDepth => &[[0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            Axes::All => &[[0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]],
        }
    }
}

/// How far to move a group of fixtures (all by the same amount) so that each aimed beam, from its
/// fixture's moved position, passes through its target: least squares along `axes`.
pub fn rig_offset(aims: &[Aim], axes: Axes) -> Result<Fit<[f32; 3]>, FocusError> {
    if aims.is_empty() {
        return Err(FocusError::NoAims);
    }
    let basis = axes.basis();
    let n = basis.len();
    let mut a = [[0.0; 3]; 3];
    let mut r = [0.0; 3];
    for aim in aims {
        // The target must sit on the beam: (I − d dᵀ)(target − from − B c) = 0.
        let cols: Vec<V> = basis.iter().map(|e| aim.across(*e)).collect();
        let rest = aim.across(sub(aim.target, aim.from));
        for i in 0..n {
            for j in 0..n {
                a[i][j] += dot(cols[i], cols[j]);
            }
            r[i] += dot(cols[i], rest);
        }
    }
    if min_eigen(&a, n) < PINNED {
        return Err(FocusError::Unclear);
    }
    let c = solve(&a, r, n).ok_or(FocusError::Unclear)?;
    let mut d = [0.0; 3];
    for (k, e) in basis.iter().enumerate() {
        for i in 0..3 {
            d[i] += c[k] * e[i];
        }
    }
    // The targets must be in front of the moved fixtures.
    if aims.iter().any(|aim| dot(sub(aim.target, [aim.from[0] + d[0], aim.from[1] + d[1], aim.from[2] + d[2]]), aim.dir) <= 0.0) {
        return Err(FocusError::NoReach);
    }
    let miss = rms(aims.iter().map(|aim| norm(aim.across(sub(sub(aim.target, aim.from), d)))));
    Ok(Fit { value: d.map(|x| x as f32), miss })
}

/// How close a fixture must be to a plot bar to hang on it, metres.
const ON_BAR: f32 = 0.05;

/// Move fixtures `ids` by `by` metres, and the plot bars only they hang on (within 5 cm of the
/// bar's line and height), so the plan and the plot stay right.
pub fn shift(l: &mut Lighting, ids: &[FixtureId], by: [f32; 3]) {
    let on = |s: &super::model::PlotShape, f: &Fixture| {
        let p = [f.position[0], f.position[2]];
        let (ab, ap) = ([s.b[0] - s.a[0], s.b[1] - s.a[1]], [p[0] - s.a[0], p[1] - s.a[1]]);
        let len = ab[0] * ab[0] + ab[1] * ab[1];
        let k = if len > 0.0 { ((ap[0] * ab[0] + ap[1] * ab[1]) / len).clamp(0.0, 1.0) } else { 0.0 };
        let gap = (p[0] - s.a[0] - k * ab[0]).hypot(p[1] - s.a[1] - k * ab[1]);
        (f.position[1] - s.height).abs() <= ON_BAR && gap <= ON_BAR
    };
    let moved: std::collections::HashSet<FixtureId> = ids.iter().copied().collect();
    for s in l.shapes.iter_mut().filter(|s| s.kind == ShapeKind::Bar) {
        let hung: Vec<&Fixture> = l.fixtures.iter().filter(|f| on(s, f)).collect();
        if !hung.is_empty() && hung.iter().all(|f| moved.contains(&f.id)) {
            s.a = [s.a[0] + by[0], s.a[1] + by[2]];
            s.b = [s.b[0] + by[0], s.b[1] + by[2]];
            s.height += by[1];
        }
    }
    for f in l.fixtures.iter_mut().filter(|f| moved.contains(&f.id)) {
        for (p, d) in f.position.iter_mut().zip(by) {
            *p += d;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::model::{FocusPoint, PlotShape};

    fn mover(position: [f32; 3], rotation: [f32; 3]) -> Fixture {
        Fixture { position, rotation, ..Default::default() }
    }

    fn physical() -> Physical {
        Physical { pan_range: 540.0, tilt_range: 270.0, moving: true, ..Default::default() }
    }

    /// The beam of an aimed fixture passes through the target.
    fn hits(f: &Fixture, p: &Physical, target: [f32; 3]) -> f64 {
        let (pan, tilt) = aim(f, p, target).unwrap();
        let a = Aim::new(f, p, pan, tilt, target).unwrap();
        norm(a.across(sub(a.target, a.from)))
    }

    #[test]
    fn middle_of_the_ranges_points_down() {
        let f = mover([1.0, 6.0, -2.0], [0.0; 3]);
        let d = beam(&f, &physical(), 0.5, 0.5).unwrap();
        assert!(norm(sub(d, [0.0, -1.0, 0.0])) < 1e-9, "{d:?}");
        assert_eq!(aim(&f, &physical(), [1.0, 0.0, -2.0]), Some((0.5, 0.5)));
    }

    #[test]
    fn aims_reach_their_targets() {
        // Tilt all the way round, so every target is in reach.
        let p = Physical { tilt_range: 360.0, ..physical() };
        let rigs = [
            mover([0.0, 6.0, -3.0], [0.0; 3]),
            mover([-4.0, 7.5, 2.0], [0.0, 30.0, 0.0]),
            mover([3.0, 0.5, 4.0], [180.0, 0.0, 0.0]), // standing on the floor
            mover([5.0, 4.0, 0.0], [0.0, 90.0, 90.0]), // on a boom, sideways
            Fixture { invert_pan: true, invert_tilt: true, ..mover([2.0, 8.0, -6.0], [10.0, -20.0, 5.0]) },
        ];
        let targets = [[0.0, 0.0, 0.0], [2.0, 0.0, 3.0], [-3.0, 1.0, -2.0], [0.0, 3.0, 8.0], [6.0, 0.0, 0.0]];
        for f in &rigs {
            for t in targets {
                assert!(hits(f, &p, t) < 1e-4, "{f:?} at {t:?}: misses by {}", hits(f, &p, t));
            }
        }
    }

    #[test]
    fn pan_nearest_the_middle() {
        // Hanging over the stage: a target downstage is reached with pan in the middle.
        let f = mover([0.0, 6.0, 0.0], [0.0; 3]);
        let (pan, tilt) = aim(&f, &physical(), [0.0, 0.0, 5.0]).unwrap();
        assert!((pan - 0.5).abs() < 1e-6 && tilt < 0.5, "{pan} {tilt}");
        // Upstage: still pan in the middle, tilted the other way.
        let (pan, tilt) = aim(&f, &physical(), [0.0, 0.0, -5.0]).unwrap();
        assert!((pan - 0.5).abs() < 1e-6 && tilt > 0.5, "{pan} {tilt}");
    }

    #[test]
    fn out_of_reach_is_clamped() {
        // 90° of tilt either way can't reach the ceiling above a hanging fixture.
        let p = Physical { pan_range: 360.0, tilt_range: 180.0, ..physical() };
        let (_, tilt) = aim(&mover([0.0, 6.0, 0.0], [0.0; 3]), &p, [0.0, 9.0, 0.1]).unwrap();
        assert!(tilt == 0.0 || tilt == 1.0, "{tilt}");
        assert_eq!(aim(&mover([0.0; 3], [0.0; 3]), &p, [0.0; 3]), None, "the target is the fixture");
        assert_eq!(aim(&mover([0.0, 6.0, 0.0], [0.0; 3]), &Physical { pan_range: 0.0, ..p }, [0.0; 3]), None);
    }

    /// The beam fixture `f` would have if it were where `real` is, aimed at `target`.
    fn aimed_from(f: &Fixture, real: [f32; 3], target: [f32; 3]) -> Aim {
        let p = physical();
        let moved = Fixture { position: real, ..f.clone() };
        let (pan, tilt) = aim(&moved, &p, target).unwrap();
        Aim::new(f, &p, pan, tilt, target).unwrap()
    }

    #[test]
    fn a_point_from_one_beam_stays_at_its_height() {
        let f = mover([2.0, 6.0, -2.0], [0.0; 3]);
        let real = [0.5, 0.0, 1.2];
        let fit = locate(&[aimed_from(&f, f.position, real)], 0.0).unwrap();
        assert!(norm(sub(v64(fit.value), v64(real))) < 1e-4, "{fit:?}");
        assert_eq!(locate(&[], 0.0), Err(FocusError::NoAims));
        // Pointing up never reaches the floor.
        let up = Aim { from: [0.0, 6.0, 0.0], dir: [0.0, 1.0, 0.0], target: [0.0; 3] };
        assert_eq!(locate(&[up], 0.0), Err(FocusError::NoReach));
    }

    #[test]
    fn a_point_from_crossing_beams_finds_its_height_too() {
        let real = [1.0, 1.5, 2.0]; // a riser, not the floor
        let aims = [aimed_from(&mover([-4.0, 6.0, -2.0], [0.0; 3]), [-4.0, 6.0, -2.0], real), aimed_from(&mover([5.0, 7.0, 0.0], [0.0; 3]), [5.0, 7.0, 0.0], real)];
        let fit = locate(&aims, 0.0).unwrap();
        assert!(norm(sub(v64(fit.value), v64(real))) < 1e-4 && fit.miss < 1e-4, "{fit:?}");
    }

    #[test]
    fn a_truss_found_lower_and_further_upstage() {
        // The patch has the truss at 7 m, 2 m upstage; in this venue it hangs at 6.2 m, 2.5 m upstage.
        let (patch, real) = ([3.0, 7.0, -2.0], [3.0, 6.2, -2.5]);
        let f = mover(patch, [0.0; 3]);
        let centre = [0.0, 0.0, 0.0];
        let fit = rig_offset(&[aimed_from(&f, real, centre)], Axes::HeightDepth).unwrap();
        assert!(norm(sub(v64(fit.value), [0.0, -0.8, -0.5])) < 1e-3, "{fit:?}");
        // Height alone: the best height for that beam (it can't fit exactly).
        let h = rig_offset(&[aimed_from(&f, real, centre)], Axes::Height).unwrap();
        assert!(h.value[0] == 0.0 && h.value[2] == 0.0 && h.miss > 0.0, "{h:?}");
        // Every axis needs a second beam.
        assert_eq!(rig_offset(&[aimed_from(&f, real, centre)], Axes::All), Err(FocusError::Unclear));
        let g = mover([-3.0, 7.0, -2.0], [0.0; 3]);
        let both = [aimed_from(&f, real, centre), aimed_from(&g, [-3.0, 6.2, -2.5], [1.0, 0.0, 2.0])];
        let fit = rig_offset(&both, Axes::All).unwrap();
        assert!(norm(sub(v64(fit.value), [0.0, -0.8, -0.5])) < 1e-3 && fit.miss < 1e-3, "{fit:?}");
    }

    #[test]
    fn a_fixture_on_the_centre_line_cant_tell_height_from_depth() {
        let f = mover([0.0, 7.0, -2.0], [0.0; 3]);
        assert_eq!(rig_offset(&[aimed_from(&f, [0.0, 6.0, -2.5], [0.0; 3])], Axes::HeightDepth), Err(FocusError::Unclear));
        assert!(rig_offset(&[aimed_from(&f, [0.0, 6.0, -2.5], [0.0; 3])], Axes::Height).is_ok());
    }

    #[test]
    fn shift_moves_fixtures_and_their_bar() {
        let mut l = Lighting::default();
        let bar = |id, z| PlotShape { id, kind: ShapeKind::Bar, a: [-4.0, z], b: [4.0, z], height: 7.0, ..Default::default() };
        l.shapes = vec![bar(1, -2.0), bar(2, 2.0)];
        l.fixtures = vec![
            Fixture { id: 1, position: [-2.0, 7.0, -2.0], ..Default::default() },
            Fixture { id: 2, position: [2.0, 7.0, -2.0], ..Default::default() },
            Fixture { id: 3, position: [0.0, 7.0, 2.0], ..Default::default() },
        ]
        .into();
        shift(&mut l, &[1, 2], [0.0, -1.0, -0.5]);
        assert_eq!(l.fixtures[0].position, [-2.0, 6.0, -2.5]);
        assert_eq!((l.shapes[0].height, l.shapes[0].a[1]), (6.0, -2.5));
        assert_eq!((l.shapes[1].height, l.shapes[1].a[1], l.fixtures[2].position), (7.0, 2.0, [0.0, 7.0, 2.0]), "the other truss stays");
        // A bar shared with a fixture that doesn't move stays too.
        shift(&mut l, &[1], [0.0, 1.0, 0.0]);
        assert_eq!(l.shapes[0].height, 6.0);
    }

    #[test]
    fn cues_follow_points_and_fixtures() {
        use crate::light::model::{Attribute, Value};
        let mut l = Lighting::default();
        let t = crate::light::library::generics().into_iter().find(|t| t.physical.moving).expect("a generic mover");
        let mode = t.modes[0].name.clone();
        l.fixture_types.push(t.clone());
        l.fixtures = vec![Fixture { id: 1, fixture_type: t.id.clone(), mode, position: [3.0, 6.0, -2.0], ..Default::default() }].into();
        l.points.push(FocusPoint { id: 1, name: "Centre".into(), position: [0.0; 3] });
        let v = Value::Point { point: 1 };
        let at = |l: &Lighting| (l.resolve(1, Attribute::Pan, v).unwrap(), l.resolve(1, Attribute::Tilt, v).unwrap());
        let before = at(&l);
        assert_eq!(before, aim(&l.fixtures[0], &t.physical, [0.0; 3]).unwrap());
        assert_eq!(l.resolve(1, Attribute::Dimmer, v), None);
        l.points[0].position = [1.0, 0.0, 1.0];
        assert_ne!(at(&l), before);
        let moved = at(&l);
        l.fixtures[0].position[1] = 5.0;
        assert_ne!(at(&l), moved);
        // Deleting the point keeps what it gave the cue.
        let mut s = crate::light::model::Sequence::default();
        let values = [(1, [(Attribute::Pan, v), (Attribute::Tilt, v)].into_iter().collect())].into_iter().collect::<crate::light::model::Look>().into();
        let cue = crate::light::model::LightCue { values, ..Default::default() };
        s.cues = vec![cue].into();
        l.sequences.push(s);
        let kept = at(&l);
        l.delete_point(1);
        let vals = &l.sequences[0].cues[0].values[&1];
        assert_eq!((vals[&Attribute::Pan], vals[&Attribute::Tilt]), (Value::Level(kept.0), Value::Level(kept.1)));
    }
}
