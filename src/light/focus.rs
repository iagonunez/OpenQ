//! Focus points: aiming moving lights at places on stage, and finding those places (or the rig)
//! again in another venue.
//!
//! A [`FocusPoint`](super::model::FocusPoint) is a named place in the patch's space (metres, x
//! right, y up, z towards the audience). Cues, presets and the programmer refer to it with
//! [`Value::Point`]; compiling turns that into pan and tilt for each
//! fixture from the fixture's position, rotation and pan/tilt ranges ([`aim`]), so moving a point
//! or a fixture re-aims every cue that uses it.
//!
//! Heads move as their GDTF file says (and as visualisers draw it): a DMX value is an angle from
//! [`Physical::pan_span`]/[`Physical::tilt_span`] (the file's `PhysicalFrom` → `PhysicalTo`, so
//! "270 → −270" turns the other way and the middle needn't be 0; without a file, the range centred
//! on the DMX middle); pan turns the yoke geometry about its z axis and tilt the head geometry
//! about its x axis, right-handed, each where the file places it ([`Physical::parts`]), and the
//! beam leaves the lens along its −z. With no parts the axes and lens sit at the fixture's origin
//! and, at 0°, the beam points straight down. Inverted pan/tilt ([`Fixture::invert_pan`]) flips the
//! DMX value. [`aim`] solves the tilt for the beam's height in the yoke in closed form, the pan
//! from the rest, then repeats a few times for the lens sitting off the axes.
//!
//! Calibration, from fixtures the operator aimed by hand at the real places ([`Aim`]): points stay
//! where the operator put them (they only move when dragged on the plan), and the rig is corrected
//! instead.
//! * [`rig_offset`] — how far a group of fixtures (a truss) really is from where the patch has
//!   it, along the chosen [`Axes`]. One beam gives two equations, so height and depth come from
//!   one fixture aimed off the centre line; all three axes need two beams (another fixture of the
//!   group, or another point). [`shift`] then moves the fixtures (and the plot bar they hang on).
//!   The caller names the group: which fixtures share a truss is never guessed.
//! * [`locate`] — where beams meet (two beams or more cross, or one meets a height): where a new
//!   point goes when the operator makes one from aimed fixtures.
//!
//! [`Spread`] lays the aims of several fixtures out round one point (line, arc, circle) as offsets,
//! so a cue keeps the form and follows the point.

use super::model::{Attribute, Fixture, FixtureId, Lighting, Physical, PointId, ShapeKind, Value};

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

/// A placement in f64: rotation rows with the translation in the last column.
type P = [[f64; 4]; 3];

const UNTURNED: P = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]];

fn p64(p: &super::model::Placement) -> P {
    p.map(|r| r.map(f64::from))
}

/// `a` then `b` (`b` placed inside `a`).
fn then(a: &P, b: &P) -> P {
    let mut m = [[0.0; 4]; 3];
    for i in 0..3 {
        for j in 0..4 {
            m[i][j] = (0..3).map(|k| a[i][k] * b[k][j]).sum::<f64>() + if j == 3 { a[i][3] } else { 0.0 };
        }
    }
    m
}

fn at(p: &P, v: V) -> V {
    [0, 1, 2].map(|i| p[i][0] * v[0] + p[i][1] * v[1] + p[i][2] * v[2] + p[i][3])
}

fn turned(p: &P, v: V) -> V {
    [0, 1, 2].map(|i| p[i][0] * v[0] + p[i][1] * v[1] + p[i][2] * v[2])
}

/// Turn about z (pan), degrees.
fn rz(deg: f64) -> P {
    let (s, c) = deg.to_radians().sin_cos();
    [[c, -s, 0.0, 0.0], [s, c, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]]
}

/// Turn about x (tilt), degrees.
fn rx(deg: f64) -> P {
    let (s, c) = deg.to_radians().sin_cos();
    [[1.0, 0.0, 0.0, 0.0], [0.0, c, -s, 0.0], [0.0, s, c, 0.0]]
}

/// The inverse of a placement (None if its rotation part is singular).
fn inverse(p: &P) -> Option<P> {
    let r = [0, 1, 2].map(|i| [p[i][0], p[i][1], p[i][2]]);
    let cross = |a: V, b: V| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    // The adjugate's columns: row i · column j = det when i = j, else 0.
    let c = [cross(r[1], r[2]), cross(r[2], r[0]), cross(r[0], r[1])];
    let det = dot(r[0], c[0]);
    if !det.is_finite() || det.abs() < 1e-9 {
        return None;
    }
    let mut inv = [0, 1, 2].map(|i| [c[0][i] / det, c[1][i] / det, c[2][i] / det, 0.0]);
    let t = turned(&inv, [p[0][3], p[1][3], p[2][3]]);
    for (row, t) in inv.iter_mut().zip(t) {
        row[3] = -t;
    }
    Some(inv)
}

/// GDTF axes (x, y, z up) → the patch's axes (x, y up, z towards the audience).
fn to_patch(v: V) -> V {
    [v[0], v[2], -v[1]]
}

/// The patch's axes → GDTF axes.
fn from_patch(v: V) -> V {
    [v[0], -v[2], v[1]]
}

/// A moving head's parts ([`MovingParts`](super::model::MovingParts)) in f64.
struct Parts {
    yoke: P,
    head: P,
    beam: P,
}

fn parts(p: &Physical) -> Parts {
    match &p.parts {
        Some(m) => Parts { yoke: p64(&m.yoke), head: p64(&m.head), beam: p64(&m.beam) },
        None => Parts { yoke: UNTURNED, head: UNTURNED, beam: UNTURNED },
    }
}

/// Pan and tilt angle at the lowest and the highest DMX value (None if a span is empty or not a
/// number).
fn spans(p: &Physical) -> Option<([f64; 2], [f64; 2])> {
    let ok = |s: [f32; 2]| (s.iter().all(|x| x.is_finite()) && (s[1] - s[0]).abs() > 1e-3).then(|| s.map(f64::from));
    Some((ok(p.pan_span())?, ok(p.tilt_span())?))
}

/// Stored value (0..=1) → physical angle, degrees.
fn to_angle(v: f32, span: [f64; 2], invert: bool) -> f64 {
    let d = if invert { 1.0 - v } else { v } as f64;
    span[0] + d * (span[1] - span[0])
}

/// Physical angle → stored value.
fn from_angle(a: f64, span: [f64; 2], invert: bool) -> f32 {
    let d = ((a - span[0]) / (span[1] - span[0])).clamp(0.0, 1.0) as f32;
    if invert { 1.0 - d } else { d }
}

/// Where the beam leaves and which way it goes, in the fixture's GDTF axes, with pan and tilt at
/// physical angles `pan`, `tilt` (degrees).
fn local_beam(k: &Parts, pan: f64, tilt: f64) -> (V, V) {
    let m = then(&then(&then(&then(&k.yoke, &rz(pan)), &k.head), &rx(tilt)), &k.beam);
    let d = turned(&m, [0.0, 0.0, -1.0]);
    let len = norm(d);
    (at(&m, [0.0; 3]), d.map(|x| x / len))
}

/// The beam of fixture `f` (type data `p`) with pan and tilt at `pan`, `tilt` (0..=1, as stored):
/// where it leaves the fixture and its direction (unit vector), patch space. None if the type has
/// no pan/tilt angles.
pub fn ray(f: &Fixture, p: &Physical, pan: f32, tilt: f32) -> Option<([f64; 3], [f64; 3])> {
    let (ps, ts) = spans(p)?;
    let (o, d) = local_beam(&parts(p), to_angle(pan, ps, f.invert_pan), to_angle(tilt, ts, f.invert_tilt));
    let r = rotation(f.rotation);
    let o = mat_vec(&r, to_patch(o));
    let pos = v64(f.position);
    Some(([pos[0] + o[0], pos[1] + o[1], pos[2] + o[2]], mat_vec(&r, to_patch(d))))
}

/// Pan and tilt (0..=1, as stored) that aim fixture `f` (of a type with physical data `p`) at
/// `target`, the way its GDTF file moves it ([`Physical::pan_angles`], [`Physical::parts`]).
/// None if the type has no pan/tilt angles or the target is where the beam starts. A target out
/// of reach gets the nearest aim within the ranges.
///
/// A head reaches most places two ways: tilted one way, or panned half a turn and tilted the
/// other way (and pan may go a full turn round). The one with pan nearest the middle of its range
/// wins, then the one with tilt nearest the middle; `flip` takes the best of the other way
/// instead (the head turned over).
pub fn aim(f: &Fixture, p: &Physical, target: [f32; 3]) -> Option<(f32, f32)> {
    aim_with(f, p, target, false)
}

/// [`aim`], or with `flip` the head turned over (the other way to reach the target).
pub fn aim_with(f: &Fixture, p: &Physical, target: [f32; 3], flip: bool) -> Option<(f32, f32)> {
    let (ps, ts) = spans(p)?;
    let k = parts(p);
    // The target in the yoke's own axes (where pan turns about z).
    let local = from_patch(mat_t_vec(&rotation(f.rotation), sub(v64(target), v64(f.position))));
    if !local.iter().all(|x| x.is_finite()) {
        return None;
    }
    let q = at(&inverse(&k.yoke)?, local);
    // After tilt the beam's height part (z, in the yoke) doesn't depend on pan:
    // z(T) = C + A cos T + B sin T with the head's z row h and the beam b0 in the head.
    let h = [k.head[2][0], k.head[2][1], k.head[2][2]];
    let b0 = turned(&k.beam, [0.0, 0.0, -1.0]);
    let b0 = b0.map(|x| x / norm(b0));
    let (c0, a0, s0) = (h[0] * b0[0], h[1] * b0[1] + h[2] * b0[2], h[2] * b0[1] - h[1] * b0[2]);
    let r = a0.hypot(s0);
    if r < 1e-9 {
        return None; // tilt can't raise or lower the beam
    }
    let phi = s0.atan2(a0);
    let wrap = |a: f64| (a + 180.0).rem_euclid(360.0) - 180.0;
    let mid = |s: [f64; 2]| (s[0] + s[1]) / 2.0;
    let bounds = |s: [f64; 2]| (s[0].min(s[1]), s[0].max(s[1]));
    // The head and lens in the yoke, before pan.
    let inner = Parts { yoke: UNTURNED, head: k.head, beam: k.beam };
    let mut candidates: Vec<(f64, usize, f64, f64)> = Vec::new();
    for way in 0..2 {
        // The lens sits off the axes: aim from where it ends up, a few times over.
        let (mut pan, mut tilt) = (mid(ps), 0.0);
        let mut ok = true;
        for _ in 0..12 {
            let (o, _) = local_beam(&inner, pan, tilt);
            let d = sub(q, o);
            let len = norm(d);
            if len.is_nan() || len <= 1e-6 {
                ok = false;
                break;
            }
            let d = d.map(|x| x / len);
            let c = ((d[2] - c0) / r).clamp(-1.0, 1.0);
            let t = if way == 0 { phi + c.acos() } else { phi - c.acos() };
            let u = turned(&then(&k.head, &rx(t.to_degrees())), b0);
            let (dxy, uxy) = (d[0].hypot(d[1]), u[0].hypot(u[1]));
            let next_pan = if dxy < 1e-9 || uxy < 1e-9 { pan } else { (d[1].atan2(d[0]) - u[1].atan2(u[0])).to_degrees() };
            let (np, nt) = (wrap(next_pan), wrap(t.to_degrees()));
            let done = (np - wrap(pan)).abs() < 1e-7 && (nt - tilt).abs() < 1e-7;
            (pan, tilt) = (np, nt);
            if done {
                break;
            }
        }
        if !ok {
            continue;
        }
        // Every pan and tilt a full turn round, near the ranges.
        let (pl, ph) = bounds(ps);
        let (tl, th) = bounds(ts);
        for kp in -3..=3 {
            for kt in -1..=1 {
                let (pa, ta) = (pan + 360.0 * kp as f64, tilt + 360.0 * kt as f64);
                let excess = (pl - pa).max(0.0) + (pa - ph).max(0.0) + (tl - ta).max(0.0) + (ta - th).max(0.0);
                let score = excess * 1e6 + (pa - mid(ps)).abs() + (ta - mid(ts)).abs() * 1e-3;
                candidates.push((score, way, pa.clamp(pl, ph), ta.clamp(tl, th)));
            }
        }
    }
    let best = |way: Option<usize>| candidates.iter().filter(|c| way.is_none_or(|w| c.1 == w)).min_by(|a, b| a.0.total_cmp(&b.0)).copied();
    let first = best(None)?;
    let (_, _, pan, tilt) = if flip { best(Some(1 - first.1)).unwrap_or(first) } else { first };
    Some((from_angle(pan, ps, f.invert_pan), from_angle(tilt, ts, f.invert_tilt)))
}

/// The direction (unit vector, patch space) of fixture `f`'s beam with pan and tilt at `pan` and
/// `tilt` (0..=1, as stored). None if the type has no pan/tilt angles.
pub fn beam(f: &Fixture, p: &Physical, pan: f32, tilt: f32) -> Option<[f64; 3]> {
    ray(f, p, pan, tilt).map(|r| r.1)
}

/// Where fixture `f`'s beam (pan and tilt at `pan`, `tilt`, as stored) meets the level plane at
/// `height` (y, patch space). None if the type has no pan/tilt angles, or the beam runs (nearly)
/// level, away from the plane or further than [`MAX_DISTANCE`](crate::limits::MAX_DISTANCE).
pub fn reach(f: &Fixture, p: &Physical, pan: f32, tilt: f32, height: f32) -> Option<[f32; 3]> {
    let (from, dir) = ray(f, p, pan, tilt)?;
    let t = (height as f64 - from[1]) / dir[1];
    if !t.is_finite() || t <= 0.0 || dir[1].abs() < 1e-3 || t > crate::limits::MAX_DISTANCE as f64 {
        return None;
    }
    Some([0, 1, 2].map(|i| (from[i] + t * dir[i]) as f32))
}

/// The focus point and offset of a unit's pan/tilt (pan first), if they refer to one.
pub fn reference(values: &std::collections::BTreeMap<Attribute, Value>) -> Option<(PointId, [f32; 3])> {
    [Attribute::Pan, Attribute::Tilt].iter().find_map(|a| match values.get(a)? {
        Value::Point { point, offset, .. } => Some((*point, *offset)),
        _ => None,
    })
}

/// Pan or tilt (`attr`) of unit `f` set to `value` by hand while it aims from a focus point: the
/// reference stays and the offset becomes where the beam now meets the height it aimed at, with
/// `flip` set when the head is the other way round from [`aim`]'s choice (so the fixture stays
/// where the operator put it). A beam that meets no such plane (aimed up or level) drops the
/// reference (pan and tilt become levels). False if `values` don't refer to a point or `attr`
/// isn't pan/tilt (nothing changed).
pub fn move_from_point(l: &Lighting, f: FixtureId, values: &mut std::collections::BTreeMap<Attribute, Value>, attr: Attribute, value: f32) -> bool {
    if !matches!(attr, Attribute::Pan | Attribute::Tilt) {
        return false;
    }
    let Some((point, offset)) = reference(values) else { return false };
    let resolve = |a: Attribute| values.get(&a).and_then(|v| l.resolve(f, a, *v));
    let (mut pan, mut tilt) = (resolve(Attribute::Pan), resolve(Attribute::Tilt));
    if attr == Attribute::Pan {
        pan = Some(value);
    } else {
        tilt = Some(value);
    }
    let base = l.point(point).map(|p| p.position);
    let hit = (|| {
        let (fixture, _) = l.unit(f)?;
        let physical = &l.fixture_type(&fixture.fixture_type)?.physical;
        let base = base?;
        let (pan, tilt) = (pan?, tilt?);
        let m = crate::limits::MAX_DISTANCE;
        let hit = reach(fixture, physical, pan, tilt, base[1] + offset[1])?;
        let offset = [0, 1, 2].map(|i| (((hit[i] - base[i]) * 1000.0).round() / 1000.0).clamp(-m, m));
        // The way round that keeps the head where it was put.
        let target = [0, 1, 2].map(|i| base[i] + offset[i]);
        let gap = |w: Option<(f32, f32)>| w.map_or(f32::INFINITY, |(p, t)| (p - pan).abs() + (t - tilt).abs());
        let flip = gap(aim_with(fixture, physical, target, true)) < gap(aim_with(fixture, physical, target, false));
        Some((offset, flip))
    })();
    for (a, v) in [(Attribute::Pan, pan), (Attribute::Tilt, tilt)] {
        if values.contains_key(&a)
            && let Some(v) = v
        {
            values.insert(a, hit.map_or(Value::Level(v), |(offset, flip)| Value::Point { point, offset, flip }));
        }
    }
    if hit.is_none() {
        values.insert(attr, Value::Level(value));
    }
    true
}

/// A fixture aimed by hand at a known place: its beam (from where the patch has the fixture) and
/// the place it really lights.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aim {
    /// Where the beam leaves the fixture, as the patch has it.
    pub from: [f64; 3],
    /// Its beam's direction (unit vector, [`beam`]).
    pub dir: [f64; 3],
    /// The place it was aimed at (the focus point's position, for [`rig_offset`]).
    pub target: [f64; 3],
}

impl Aim {
    /// Fixture `f` (type data `p`) aimed with pan/tilt `pan`, `tilt` at `target`.
    pub fn new(f: &Fixture, p: &Physical, pan: f32, tilt: f32, target: [f32; 3]) -> Option<Aim> {
        let (from, dir) = ray(f, p, pan, tilt)?;
        Some(Aim { from, dir, target: v64(target) })
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

/// The form [`Spread`] lays fixtures' aims out in round a focus point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SpreadShape {
    /// A straight line through the point, the point in its middle.
    #[default]
    Line,
    /// Part of a circle round the point, its middle in the `turn` direction.
    Arc,
    /// A whole circle round the point, evenly spaced.
    Circle,
}

impl SpreadShape {
    /// Every form, in the order the UI lists them.
    pub const ALL: [SpreadShape; 3] = [SpreadShape::Line, SpreadShape::Arc, SpreadShape::Circle];

    /// Shown name.
    pub fn label(self) -> &'static str {
        match self {
            SpreadShape::Line => crate::i18n::t("Line"),
            SpreadShape::Arc => crate::i18n::t("Arc"),
            SpreadShape::Circle => crate::i18n::t("Circle"),
        }
    }
}

/// Where a group of fixtures aimed at one focus point aim round it: offsets on the point's
/// height plane (the plan view), so the cue keeps the form and follows the point when it moves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spread {
    /// Line, arc or circle.
    pub shape: SpreadShape,
    /// Line: length from the first fixture to the last; arc and circle: radius. Metres.
    pub size: f32,
    /// Arc only: degrees from the first fixture to the last.
    pub span: f32,
    /// Direction of the form, degrees about the vertical (as [`super::place::Layout::Arc`]: 0 =
    /// towards the audience, 90 = stage left, +x). At 0 a line runs across the stage, an arc
    /// bulges downstage of the point and a circle starts downstage of it.
    pub turn: f32,
}

impl Default for Spread {
    fn default() -> Self {
        Self { shape: SpreadShape::Line, size: 4.0, span: 180.0, turn: 0.0 }
    }
}

impl Spread {
    /// Offsets (patch axes, y = 0) for `n` fixtures, first to last: a line or an arc goes from
    /// stage right to stage left at `turn` 0, a circle turns the same way. One fixture aims at
    /// the middle of a line or arc. Millimetres; within the distance limit.
    pub fn offsets(&self, n: usize) -> Vec<[f32; 3]> {
        let m = crate::limits::MAX_DISTANCE;
        let size = if self.size.is_finite() { self.size.clamp(0.0, m) } else { 0.0 };
        let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
        let (span, turn) = (finite(self.span).clamp(0.0, 360.0), finite(self.turn));
        // Position 0…1 along the form (the middle for one fixture).
        let k = |i: usize| if n <= 1 { 0.5 } else { i as f32 / (n - 1) as f32 };
        let at = |deg: f32, r: f32| {
            let a = deg.to_radians();
            [r * a.sin(), 0.0, r * a.cos()]
        };
        (0..n)
            .map(|i| {
                let p = match self.shape {
                    // Along the direction 90° on from `turn`, centred on the point.
                    SpreadShape::Line => at(turn + 90.0, size * (k(i) - 0.5)),
                    SpreadShape::Arc => at(turn - span / 2.0 + span * k(i), size),
                    SpreadShape::Circle => at(turn + 360.0 * i as f32 / n.max(1) as f32, size),
                };
                p.map(|v| ((v * 1000.0).round() / 1000.0).clamp(-m, m) + 0.0)
            })
            .collect()
    }
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
    let m = crate::limits::MAX_DISTANCE;
    let within = |v: f32| if v.is_finite() { v.clamp(-m, m) } else { 0.0 };
    for s in l.shapes.iter_mut().filter(|s| s.kind == ShapeKind::Bar) {
        let hung: Vec<&Fixture> = l.fixtures.iter().filter(|f| on(s, f)).collect();
        if !hung.is_empty() && hung.iter().all(|f| moved.contains(&f.id)) {
            s.a = [s.a[0] + by[0], s.a[1] + by[2]].map(within);
            s.b = [s.b[0] + by[0], s.b[1] + by[2]].map(within);
            s.height = within(s.height + by[1]);
        }
    }
    for f in l.fixtures.iter_mut().filter(|f| moved.contains(&f.id)) {
        for (p, d) in f.position.iter_mut().zip(by) {
            *p = within(*p + d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::model::{FocusPoint, PlotShape};

    #[test]
    fn spread_round_a_point() {
        let s = |shape, size, span, turn| Spread { shape, size, span, turn };
        let xs = |v: Vec<[f32; 3]>| v.iter().map(|p| [p[0], p[2]]).collect::<Vec<_>>();
        // A line across the stage, stage right to stage left, centred on the point.
        assert_eq!(xs(s(SpreadShape::Line, 4.0, 0.0, 0.0).offsets(5)), [[-2.0, 0.0], [-1.0, 0.0], [0.0, 0.0], [1.0, 0.0], [2.0, 0.0]]);
        assert_eq!(s(SpreadShape::Line, 4.0, 0.0, 0.0).offsets(1), [[0.0; 3]], "one fixture: on the point");
        // Turned a quarter: up and down stage.
        assert_eq!(xs(s(SpreadShape::Line, 2.0, 0.0, 90.0).offsets(2)), [[0.0, 1.0], [0.0, -1.0]]);
        // A half circle bulging downstage, ends on the line's ends.
        assert_eq!(xs(s(SpreadShape::Arc, 2.0, 180.0, 0.0).offsets(3)), [[-2.0, 0.0], [0.0, 2.0], [2.0, 0.0]]);
        assert_eq!(xs(s(SpreadShape::Arc, 2.0, 90.0, 0.0).offsets(1)), [[0.0, 2.0]], "one fixture: the arc's middle");
        // A whole circle, evenly spaced (no two fixtures on the same place).
        assert_eq!(xs(s(SpreadShape::Circle, 3.0, 0.0, 0.0).offsets(4)), [[0.0, 3.0], [3.0, 0.0], [0.0, -3.0], [-3.0, 0.0]]);
        assert!(s(SpreadShape::Circle, 1.0, 0.0, 0.0).offsets(4).iter().all(|p| p[1] == 0.0), "on the point's height");
        // Wild settings stay finite and in range.
        let wild = s(SpreadShape::Arc, f32::INFINITY, f32::NAN, 1e30).offsets(3);
        assert!(wild.iter().flatten().all(|v| v.is_finite() && v.abs() <= crate::limits::MAX_DISTANCE));
        assert!(s(SpreadShape::Line, 4.0, 0.0, 0.0).offsets(0).is_empty());
    }

    fn mover(position: [f32; 3], rotation: [f32; 3]) -> Fixture {
        Fixture { position, rotation, ..Default::default() }
    }

    fn physical() -> Physical {
        Physical { pan_range: 540.0, tilt_range: 270.0, moving: true, ..Default::default() }
    }

    /// The beam of an aimed fixture passes through the target.
    fn hits(f: &Fixture, p: &Physical, target: [f32; 3]) -> f64 {
        hits_with(f, p, target, false)
    }

    /// How far the beam aimed at `target` (either way round) passes from it (infinite when it
    /// points away).
    fn hits_with(f: &Fixture, p: &Physical, target: [f32; 3], flip: bool) -> f64 {
        let (pan, tilt) = aim_with(f, p, target, flip).unwrap();
        let a = Aim::new(f, p, pan, tilt, target).unwrap();
        if dot(sub(a.target, a.from), a.dir) <= 0.0 {
            return f64::INFINITY;
        }
        norm(a.across(sub(a.target, a.from)))
    }

    /// A head like manufacturers' files have it: pan the other way round, tilt off-centre, the
    /// parts below the hanging point and the head turned a quarter round.
    fn real_head() -> Physical {
        let at = |r: [[f32; 3]; 3], t: [f32; 3]| [0, 1, 2].map(|i| [r[i][0], r[i][1], r[i][2], t[i]]);
        let i3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let quarter = [[0.0, 1.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
        Physical {
            pan_angles: Some([270.0, -270.0]),
            tilt_angles: Some([-100.0, 140.0]),
            parts: Some(super::super::model::MovingParts { yoke: at(i3, [0.0, 0.0, -0.1]), head: at(quarter, [0.02, 0.0, -0.3]), beam: at(i3, [0.0, 0.0, -0.2]) }),
            ..Physical { tilt_range: 360.0, ..physical() }
        }
    }

    #[test]
    fn heads_as_their_files_have_them_reach_their_targets_both_ways() {
        let p = Physical { tilt_angles: Some([-180.0, 180.0]), ..real_head() };
        let rigs = [
            mover([0.0, 6.0, -3.0], [0.0; 3]),
            mover([-4.0, 7.5, 8.0], [0.0, 30.0, 0.0]),
            mover([3.0, 0.5, 4.0], [180.0, 0.0, 0.0]), // standing on the floor
            mover([5.0, 4.0, 0.0], [0.0, 90.0, 90.0]), // on a boom, sideways
            Fixture { invert_pan: true, invert_tilt: true, ..mover([2.0, 8.0, -6.0], [10.0, -20.0, 5.0]) },
        ];
        let targets = [[0.0, 0.0, 0.0], [2.0, 0.0, 3.0], [-3.0, 1.0, -2.0], [0.0, 3.0, 8.0], [6.0, 0.0, 0.0]];
        for f in &rigs {
            for t in targets {
                for flip in [false, true] {
                    let miss = hits_with(f, &p, t, flip);
                    assert!(miss < 1e-4, "{f:?} at {t:?} (flip {flip}): misses by {miss}");
                }
                let (a, b) = (aim_with(f, &p, t, false).unwrap(), aim_with(f, &p, t, true).unwrap());
                assert!((a.0 - b.0).abs() > 0.2, "flipped: pan half a turn round ({a:?} {b:?})");
            }
        }
    }

    #[test]
    fn reversed_angles_reverse_the_dmx() {
        let f = mover([1.0, 6.0, 5.0], [0.0; 3]);
        let base = Physical { pan_angles: Some([-270.0, 270.0]), ..physical() };
        let reversed = Physical { pan_angles: Some([270.0, -270.0]), ..physical() };
        let (a, b) = (aim(&f, &base, [0.0; 3]).unwrap(), aim(&f, &reversed, [0.0; 3]).unwrap());
        assert!((a.0 - (1.0 - b.0)).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6, "{a:?} {b:?}");
        // An off-centre span: the middle of the DMX range isn't straight down.
        let offset = Physical { tilt_angles: Some([0.0, 270.0]), ..physical() };
        let (_, tilt) = aim(&f, &offset, [1.0, 0.0, 5.0]).unwrap();
        assert!(tilt.abs() < 1e-6, "straight down = tilt 0° = DMX 0: {tilt}");
    }

    #[test]
    fn the_lens_is_where_the_beam_starts() {
        // A head hanging 0.5 m below the fixture's point, aimed level: the beam is 0.5 m lower.
        let at = |z: f32| [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, z]];
        let p = Physical { parts: Some(super::super::model::MovingParts { yoke: at(-0.2), head: at(-0.3), beam: at(0.0) }), ..physical() };
        let f = mover([0.0, 6.0, 0.0], [0.0; 3]);
        let (from, _) = ray(&f, &p, 0.5, 0.5).unwrap();
        assert!(norm(sub(from, [0.0, 5.5, 0.0])) < 1e-6, "{from:?}");
        assert!(hits(&f, &p, [10.0, 5.5, 0.0]) < 1e-4, "stored values are f32");
        // Without the parts the same pan/tilt would light 0.5 m higher at that distance.
        let (pan, tilt) = aim(&f, &p, [10.0, 5.5, 0.0]).unwrap();
        let plain = Aim::new(&f, &physical(), pan, tilt, [10.0, 5.5, 0.0]).unwrap();
        assert!(norm(plain.across(sub(plain.target, plain.from))) > 0.4);
    }

    #[test]
    fn wild_input_gives_no_aim() {
        let f = mover([0.0, 6.0, 0.0], [0.0; 3]);
        assert_eq!(aim(&f, &physical(), [f32::INFINITY, 0.0, 0.0]), None);
        assert_eq!(aim(&f, &physical(), [f32::NAN, 0.0, 0.0]), None);
        assert_eq!(aim(&f, &Physical { pan_angles: Some([10.0, 10.0]), ..physical() }, [0.0; 3]), None, "empty span");
        let flat = |z: f32| [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, z, 0.0]];
        let p = Physical { parts: Some(super::super::model::MovingParts { yoke: flat(0.0), ..Default::default() }), ..physical() };
        assert_eq!(aim(&f, &p, [0.0; 3]), None, "a yoke that can't be undone");
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
        let v = Value::point(1);
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

    #[test]
    fn offsets_follow_their_point() {
        use crate::light::model::{Attribute, Value};
        let mut l = Lighting::default();
        let t = crate::light::library::generics().into_iter().find(|t| t.physical.moving).expect("a generic mover");
        let mode = t.modes[0].name.clone();
        l.fixture_types.push(t.clone());
        l.fixtures = vec![Fixture { id: 1, fixture_type: t.id.clone(), mode, position: [3.0, 6.0, -2.0], ..Default::default() }].into();
        l.points.push(FocusPoint { id: 1, name: "Centre".into(), position: [0.0; 3] });
        let v = Value::Point { point: 1, offset: [1.0, 0.0, 0.5], flip: false };
        let at = |l: &Lighting| (l.resolve(1, Attribute::Pan, v).unwrap(), l.resolve(1, Attribute::Tilt, v).unwrap());
        assert_eq!(at(&l), aim(&l.fixtures[0], &t.physical, [1.0, 0.0, 0.5]).unwrap());
        l.points[0].position = [-1.0, 0.0, 1.0];
        assert_eq!(at(&l), aim(&l.fixtures[0], &t.physical, [0.0, 0.0, 1.5]).unwrap(), "the offset moves with the point");
        // Where the beam lands at the point's height gives the offset back.
        let (pan, tilt) = at(&l);
        let hit = reach(&l.fixtures[0], &t.physical, pan, tilt, 0.0).unwrap();
        assert!(hit.iter().zip([0.0, 0.0, 1.5]).all(|(a, b)| (a - b).abs() < 1e-3), "{hit:?}");
        // A beam pointing up reaches no floor.
        let up = aim(&l.fixtures[0], &t.physical, [3.0, 9.0, -2.0]).unwrap();
        assert_eq!(reach(&l.fixtures[0], &t.physical, up.0, up.1, 0.0), None);
        // Saved without the offset when there is none.
        assert_eq!(serde_json::to_string(&Value::point(2)).unwrap(), r#"{"point":2}"#);
        let back: Value = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert_eq!(back, v);
    }
}
