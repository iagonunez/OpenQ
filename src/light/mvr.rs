//! MVR (My Virtual Rig, DIN SPEC 15801): the patch with positions, to and from visualisers and
//! other desks.
//!
//! [`export`]: a zip holding `GeneralSceneDescription.xml`: one layer with a `Fixture` per
//! patched fixture (name, stable UUID, transform, `GDTFSpec` + mode, fixture number, DMX
//! address), and a GDTF file per fixture type used ([`super::gdtf::export`]), named like the
//! GDTF Share file the type would have (`Manufacturer@Name.gdtf`).
//!
//! [`import`] reads fixtures anywhere in the scene (layers, group objects, nested transforms),
//! their addresses (absolute or `universe.address`) and types: a type already in the show or
//! the library with the same GDTF name, else the GDTF file in the MVR, else a generic dimmer.
//! Trusses and scene objects with a glTF geometry become plot bars and rectangles (their box:
//! the mesh's bounds); [`merge`] puts the result into a design, renumbering clashing fixtures.
//!
//! Coordinates: OpenQ uses metres with x right, y up, z towards the audience; MVR uses
//! millimetres with x right, y upstage, z up. Rotations are degrees about x, y and z, applied as
//! heading (y) · tilt (x) · roll (z).
//!
//! Plot shapes go in a second layer: a bar is a `Truss` (a 30 cm box along the bar, centred on
//! its height), a rectangle a `SceneObject` (a box from the floor up to its height, or a 2 cm
//! slab). Each has its own box mesh, `shape-<id>.glb`, in the file: glTF, so metres and y up,
//! which the importer turns into MVR's z up.

use super::model::{DmxAddress, Fixture, FixtureId, FixtureType, Lighting, PlotShape, ShapeKind};
use std::collections::HashMap;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// File extension.
pub const EXTENSION: &str = "mvr";

type Mat = [[f32; 3]; 3];

fn mul(a: &Mat, b: &Mat) -> Mat {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

/// Rotation of a fixture in OpenQ's axes: heading (y) · tilt (x) · roll (z).
fn rotation(deg: [f32; 3]) -> Mat {
    let [x, y, z] = deg.map(f32::to_radians);
    let rx = [[1.0, 0.0, 0.0], [0.0, x.cos(), -x.sin()], [0.0, x.sin(), x.cos()]];
    let ry = [[y.cos(), 0.0, y.sin()], [0.0, 1.0, 0.0], [-y.sin(), 0.0, y.cos()]];
    let rz = [[z.cos(), -z.sin(), 0.0], [z.sin(), z.cos(), 0.0], [0.0, 0.0, 1.0]];
    mul(&ry, &mul(&rx, &rz))
}

/// OpenQ axes → MVR axes: (x, y, z) → (x, −z, y).
const TO_MVR: Mat = [[1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]];

/// The fixture's MVR `Matrix` text: `{u}{v}{w}{o}`, u/v/w the rotated MVR axes, o in mm.
pub fn matrix(f: &Fixture) -> String {
    let t: Mat = [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, -1.0, 0.0]]; // transpose of TO_MVR
    let m = mul(&TO_MVR, &mul(&rotation(f.rotation), &t));
    let [x, y, z] = f.position;
    let o = [x * 1000.0, -z * 1000.0, y * 1000.0];
    let num = |v: f32| {
        let v = if v.abs() < 1e-6 { 0.0 } else { v };
        format!("{}", (v * 1e6).round() / 1e6)
    };
    // Rows are the images of the MVR x, y and z axes (columns of the rotation).
    let col = |j: usize| format!("{{{},{},{}}}", num(m[0][j]), num(m[1][j]), num(m[2][j]));
    format!("{}{}{}{{{},{},{}}}", col(0), col(1), col(2), num(o[0]), num(o[1]), num(o[2]))
}

/// A UUID that stays the same for the same show and fixture number, so a visualiser that
/// re-imports the file keeps its own settings per fixture.
fn stable_uuid(scope: &str, what: &str) -> Uuid {
    let h = Sha256::digest(format!("openq-mvr\0{scope}\0{what}").as_bytes());
    let mut b: [u8; 16] = h[..16].try_into().expect("32-byte digest");
    b[6] = (b[6] & 0x0f) | 0x50; // name-based
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    Uuid::from_bytes(b)
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => o.push(c),
        }
    }
    o
}

/// GDTF Share style file name of a fixture type: `Manufacturer@Name.gdtf` (characters that
/// can't be in a file name become `_`).
pub fn gdtf_name(manufacturer: &str, name: &str) -> String {
    let clean = |s: &str| s.trim().chars().map(|c| if c.is_alphanumeric() || " -_.()+".contains(c) { c } else { '_' }).collect::<String>().replace(' ', "_");
    let m = if manufacturer.trim().is_empty() { "Generic".into() } else { clean(manufacturer) };
    format!("{m}@{}.gdtf", clean(name))
}

/// The GDTF file name of each fixture type in the design (by type id): [`gdtf_name`], with a
/// number added when two types would get the same name.
pub fn spec_names(l: &Lighting) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    for t in &l.fixture_types {
        let base = gdtf_name(&t.manufacturer, &t.name);
        let mut name = base.clone();
        let mut n = 2;
        while !taken.insert(name.to_lowercase()) {
            name = format!("{}_{n}.gdtf", base.trim_end_matches(".gdtf"));
            n += 1;
        }
        out.insert(t.id.clone(), name);
    }
    out
}

/// Cross-section of a bar's box, metres.
const BAR_SIZE: f32 = 0.3;
/// Thickness of a rectangle with no height, metres.
const SLAB: f32 = 0.02;

/// A shape's box: MVR placement (`Matrix` text) and the glTF box, min and max corners in the
/// glTF frame (metres, y up, z = −MVR y).
fn shape_box(s: &PlotShape) -> (String, [f32; 3], [f32; 3]) {
    let num = |v: f32| {
        let v = if v.abs() < 1e-6 { 0.0 } else { v };
        format!("{}", (v * 1e6).round() / 1e6)
    };
    let mid = [(s.a[0] + s.b[0]) / 2.0, (s.a[1] + s.b[1]) / 2.0];
    match s.kind {
        ShapeKind::Bar => {
            // Along MVR (dx, dy) = (Δx, −Δz), turned about MVR z.
            let (dx, dy) = (s.b[0] - s.a[0], -(s.b[1] - s.a[1]));
            let len = dx.hypot(dy).max(0.01);
            let (c, sn) = if dx.hypot(dy) > 0.0 { (dx / len, dy / len) } else { (1.0, 0.0) };
            let m = format!(
                "{{{},{},0}}{{{},{},0}}{{0,0,1}}{{{},{},{}}}",
                num(c),
                num(sn),
                num(-sn),
                num(c),
                num(mid[0] * 1000.0),
                num(-mid[1] * 1000.0),
                num(s.height * 1000.0)
            );
            let h = BAR_SIZE / 2.0;
            (m, [-len / 2.0, -h, -h], [len / 2.0, h, h])
        }
        ShapeKind::Rect => {
            let (w, d) = ((s.b[0] - s.a[0]).abs().max(0.01), (s.b[1] - s.a[1]).abs().max(0.01));
            let m = format!("{{1,0,0}}{{0,1,0}}{{0,0,1}}{{{},{},0}}", num(mid[0] * 1000.0), num(-mid[1] * 1000.0));
            let top = if s.height > 0.0 { s.height } else { SLAB };
            (m, [-w / 2.0, 0.0, -d / 2.0], [w / 2.0, top, d / 2.0])
        }
    }
}

/// A box from `min` to `max` as a binary glTF (GLB) file: 24 vertices with normals, 12
/// triangles, one mesh in one node.
pub fn box_glb(min: [f32; 3], max: [f32; 3]) -> Vec<u8> {
    // Faces: axis, side (−1/+1); each a quad with outward winding.
    let mut pos: Vec<[f32; 3]> = Vec::new();
    let mut nrm: Vec<[f32; 3]> = Vec::new();
    let mut idx: Vec<u16> = Vec::new();
    for axis in 0..3 {
        for side in [-1.0f32, 1.0] {
            let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
            let base = pos.len() as u16;
            for (a, b) in [(0, 0), (1, 0), (1, 1), (0, 1)] {
                let mut p = [0.0; 3];
                p[axis] = if side > 0.0 { max[axis] } else { min[axis] };
                p[u] = if a == 1 { max[u] } else { min[u] };
                p[v] = if b == 1 { max[v] } else { min[v] };
                pos.push(p);
                let mut n = [0.0; 3];
                n[axis] = side;
                nrm.push(n);
            }
            // (u, v, axis) is right-handed, so 0-1-2 faces +axis: reverse for the −axis face.
            let quad = if side > 0.0 { [0, 1, 2, 0, 2, 3] } else { [0, 2, 1, 0, 3, 2] };
            idx.extend(quad.iter().map(|k| base + k));
        }
    }
    let mut bin: Vec<u8> = Vec::new();
    for p in pos.iter().chain(&nrm) {
        for c in p {
            bin.extend_from_slice(&c.to_le_bytes());
        }
    }
    let idx_at = bin.len();
    for i in &idx {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let vbytes = pos.len() * 12;
    let f = |v: f32| format!("{v}");
    let json = format!(
        concat!(
            r#"{{"asset":{{"version":"2.0","generator":"OpenQ"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0}}],"#,
            r#""meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1}},"indices":2}}]}}],"#,
            r#""accessors":[{{"bufferView":0,"componentType":5126,"count":{n},"type":"VEC3","min":[{x0},{y0},{z0}],"max":[{x1},{y1},{z1}]}},"#,
            r#"{{"bufferView":1,"componentType":5126,"count":{n},"type":"VEC3"}},"#,
            r#"{{"bufferView":2,"componentType":5123,"count":{ni},"type":"SCALAR"}}],"#,
            r#""bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":{vb},"target":34962}},{{"buffer":0,"byteOffset":{vb},"byteLength":{vb},"target":34962}},"#,
            r#"{{"buffer":0,"byteOffset":{io},"byteLength":{ib},"target":34963}}],"buffers":[{{"byteLength":{bl}}}]}}"#
        ),
        n = pos.len(),
        ni = idx.len(),
        x0 = f(min[0]),
        y0 = f(min[1]),
        z0 = f(min[2]),
        x1 = f(max[0]),
        y1 = f(max[1]),
        z1 = f(max[2]),
        vb = vbytes,
        io = idx_at,
        ib = idx.len() * 2,
        bl = bin.len()
    );
    let mut json = json.into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

fn shape_file(s: &PlotShape) -> String {
    format!("shape-{}.glb", s.id)
}

/// `GeneralSceneDescription.xml` for a design. `scope` (the show's name) seeds the UUIDs.
pub fn scene(l: &Lighting, scope: &str) -> String {
    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\" ?>\n");
    x.push_str(&format!("<GeneralSceneDescription verMajor=\"1\" verMinor=\"6\" provider=\"OpenQ\" providerVersion=\"{}\">\n", env!("CARGO_PKG_VERSION")));
    x.push_str("  <UserData/>\n  <Scene>\n    <Layers>\n");
    x.push_str(&format!("      <Layer name=\"{}\" uuid=\"{}\">\n        <ChildList>\n", esc(if scope.is_empty() { "OpenQ" } else { scope }), stable_uuid(scope, "layer")));
    let specs = spec_names(l);
    for f in &l.fixtures {
        let ty = l.fixture_type(&f.fixture_type);
        let (manufacturer, type_name) = ty.map_or(("", f.fixture_type.as_str()), |t| (t.manufacturer.as_str(), t.name.as_str()));
        let spec = specs.get(&f.fixture_type).cloned().unwrap_or_else(|| gdtf_name(manufacturer, type_name));
        let name = if f.name.trim().is_empty() { format!("{type_name} {}", f.id) } else { f.name.clone() };
        x.push_str(&format!("          <Fixture name=\"{}\" uuid=\"{}\">\n", esc(&name), stable_uuid(scope, &format!("fixture {}", f.id))));
        x.push_str(&format!("            <Matrix>{}</Matrix>\n", matrix(f)));
        x.push_str(&format!("            <GDTFSpec>{}</GDTFSpec>\n", esc(&spec)));
        // The mode as the exported GDTF names it ("Standard [Lamp]" → "Standard (Lamp)").
        let mode = ty.and_then(|t| Some(super::gdtf::mode_names(t).swap_remove(t.modes.iter().position(|m| m.name == f.mode)?))).unwrap_or_else(|| f.mode.clone());
        x.push_str(&format!("            <GDTFMode>{}</GDTFMode>\n", esc(&mode)));
        x.push_str(&format!("            <FixtureID>{}</FixtureID>\n            <FixtureIDNumeric>{}</FixtureIDNumeric>\n", f.id, f.id));
        x.push_str("            <UnitNumber>0</UnitNumber>\n");
        // One address per patched DMX break.
        let breaks = l.mode_of(f).map_or(1, |m| m.breaks());
        let addresses: Vec<String> = (0..breaks)
            .map(|b| (b, f.address_of(b)))
            .filter(|(_, at)| at.address > 0 && at.universe > 0)
            .map(|(b, at)| format!("              <Address break=\"{b}\">{}</Address>\n", (at.universe as u32 - 1) * 512 + at.address as u32))
            .collect();
        if !addresses.is_empty() {
            x.push_str(&format!("            <Addresses>\n{}            </Addresses>\n", addresses.concat()));
        }
        x.push_str("          </Fixture>\n");
    }
    x.push_str("        </ChildList>\n      </Layer>\n");
    if !l.shapes.is_empty() {
        x.push_str(&format!("      <Layer name=\"Plot\" uuid=\"{}\">\n        <ChildList>\n", stable_uuid(scope, "plot layer")));
        for s in &l.shapes {
            let (tag, what) = match s.kind {
                ShapeKind::Bar => ("Truss", "Bar"),
                ShapeKind::Rect => ("SceneObject", "Rectangle"),
            };
            let name = if s.name.trim().is_empty() { format!("{what} {}", s.id) } else { s.name.clone() };
            let (m, _, _) = shape_box(s);
            x.push_str(&format!("          <{tag} name=\"{}\" uuid=\"{}\">\n", esc(&name), stable_uuid(scope, &format!("shape {}", s.id))));
            x.push_str(&format!("            <Matrix>{m}</Matrix>\n"));
            x.push_str(&format!("            <Geometries>\n              <Geometry3D fileName=\"{}\"/>\n            </Geometries>\n", shape_file(s)));
            // MVR 1.6 requires a FixtureID on every truss and scene object (empty: not addressed by one).
            x.push_str(&format!("            <FixtureID></FixtureID>\n          </{tag}>\n"));
        }
        x.push_str("        </ChildList>\n      </Layer>\n");
    }
    x.push_str("    </Layers>\n  </Scene>\n</GeneralSceneDescription>\n");
    x
}

/// The MVR file of a design, with the GDTF files of the types its fixtures use.
pub fn export(l: &Lighting, scope: &str) -> anyhow::Result<Vec<u8>> {
    let scene = scene(l, scope);
    let specs = spec_names(l);
    let used: std::collections::HashSet<&str> = l.fixtures.iter().map(|f| f.fixture_type.as_str()).collect();
    let mut gdtfs: Vec<(String, Vec<u8>)> = Vec::new();
    for t in l.fixture_types.iter().filter(|t| used.contains(t.id.as_str())) {
        gdtfs.push((specs[&t.id].clone(), super::gdtf::export(t)?));
    }
    let meshes: Vec<(String, Vec<u8>)> = l
        .shapes
        .iter()
        .map(|s| {
            let (_, min, max) = shape_box(s);
            (shape_file(s), box_glb(min, max))
        })
        .collect();
    let mut entries: Vec<(&str, &[u8])> = vec![("GeneralSceneDescription.xml", scene.as_bytes())];
    entries.extend(meshes.iter().map(|(n, d)| (n.as_str(), d.as_slice())));
    entries.extend(gdtfs.iter().map(|(n, d)| (n.as_str(), d.as_slice())));
    super::zipread::write(&entries)
}

/// Largest MVR file read.
pub const MAX_FILE: u64 = 1 << 30;
/// Largest `GeneralSceneDescription.xml` read.
const MAX_SCENE: usize = 64 << 20;
/// Largest glTF geometry read for a truss or scene object.
const MAX_MESH: usize = 64 << 20;
/// Most fixtures read from one file.
const MAX_FIXTURES: usize = 100_000;

/// What an MVR file holds, ready for [`merge`].
#[derive(Debug, Default)]
pub struct Imported {
    /// Fixture types not in the show yet (from the library, built-ins or the file's GDTFs).
    pub types: Vec<FixtureType>,
    /// Fixtures, numbered as in the file (0 = no number given).
    pub fixtures: Vec<Fixture>,
    /// Trusses and scene objects as plot shapes (ids from 1).
    pub shapes: Vec<PlotShape>,
    /// Things left out or guessed.
    pub notes: Vec<String>,
}

/// A transform: rotation (columns = the local axes in parent coordinates) and offset (mm).
#[derive(Clone, Copy)]
struct Transform {
    r: Mat,
    o: [f32; 3],
}

const IDENTITY: Transform = Transform { r: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], o: [0.0; 3] };

impl Transform {
    fn then(&self, local: &Transform) -> Transform {
        Transform { r: mul(&self.r, &local.r), o: add(self.o, apply(&self.r, local.o)) }
    }

    fn point(&self, p: [f32; 3]) -> [f32; 3] {
        add(self.o, apply(&self.r, p))
    }
}

fn apply(m: &Mat, v: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

/// An MVR `Matrix` text (`{u}{v}{w}{o}`); None if it isn't one.
fn parse_matrix(s: &str) -> Option<Transform> {
    let nums: Vec<f32> = s.split(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))).filter(|t| !t.is_empty()).map(|t| t.parse::<f32>().ok()).collect::<Option<Vec<_>>>()?;
    if nums.len() != 12 || nums.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let mut r = [[0.0; 3]; 3];
    for col in 0..3 {
        for row in 0..3 {
            r[row][col] = nums[col * 3 + row];
        }
    }
    Some(Transform { r, o: [nums[9], nums[10], nums[11]] })
}

/// OpenQ rotation (degrees, heading · tilt · roll) of an MVR rotation; scale is taken out.
fn angles(m: &Mat) -> [f32; 3] {
    let mut m = *m;
    for col in 0..3 {
        let n = (0..3).map(|r| m[r][col] * m[r][col]).sum::<f32>().sqrt();
        if n > 1e-9 {
            for row in m.iter_mut() {
                row[col] /= n;
            }
        }
    }
    let t: Mat = [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, -1.0, 0.0]];
    let r = mul(&t, &mul(&m, &TO_MVR));
    let x = (-r[1][2]).clamp(-1.0, 1.0).asin();
    let (y, z) = if x.cos().abs() > 1e-5 { (r[0][2].atan2(r[2][2]), r[1][0].atan2(r[1][1])) } else { ((-r[2][0]).atan2(r[0][0]), 0.0) };
    let deg = |v: f32| {
        let d = v.to_degrees();
        let d = if d.abs() < 1e-4 { 0.0 } else { d };
        (d * 1e4).round() / 1e4
    };
    [deg(x), deg(y), deg(z)]
}

/// OpenQ position (metres) of an MVR point (mm).
fn position(p: [f32; 3]) -> [f32; 3] {
    let r = |v: f32| (v * 1e3).round() / 1e6;
    [r(p[0]), r(p[2]), r(-p[1])]
}

/// Bounds of a glTF binary's meshes (the POSITION accessors' min/max), in its own frame.
fn glb_bounds(glb: &[u8]) -> Option<([f32; 3], [f32; 3])> {
    if glb.get(..4)? != b"glTF" {
        return None;
    }
    let len = u32::from_le_bytes(glb.get(12..16)?.try_into().ok()?) as usize;
    if glb.get(16..20)? != b"JSON" {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(glb.get(20..20usize.checked_add(len)?)?).ok()?;
    let accessors = json.get("accessors")?.as_array()?;
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    let mut any = false;
    for mesh in json.get("meshes")?.as_array()? {
        for prim in mesh.get("primitives").and_then(|p| p.as_array()).into_iter().flatten() {
            let Some(a) = prim.pointer("/attributes/POSITION").and_then(|v| v.as_u64()).and_then(|i| accessors.get(i as usize)) else { continue };
            let v3 = |k: &str| -> Option<[f32; 3]> {
                let v = a.get(k)?.as_array()?;
                let f = |i: usize| v.get(i)?.as_f64().map(|x| x as f32).filter(|x| x.is_finite());
                Some([f(0)?, f(1)?, f(2)?])
            };
            if let (Some(mn), Some(mx)) = (v3("min"), v3("max")) {
                for i in 0..3 {
                    lo[i] = lo[i].min(mn[i]);
                    hi[i] = hi[i].max(mx[i]);
                }
                any = true;
            }
        }
    }
    any.then_some((lo, hi))
}

/// The plot shape of a truss (`bar`) or scene object whose glTF box is `lo`..`hi`, placed by `t`.
fn shape_of(t: &Transform, lo: [f32; 3], hi: [f32; 3], bar: bool) -> PlotShape {
    // glTF metres, y up → MVR local mm, z up.
    let to_mvr = |p: [f32; 3]| [p[0] * 1000.0, -p[2] * 1000.0, p[1] * 1000.0];
    let corners: Vec<[f32; 3]> = (0..8).map(|k| position(t.point(to_mvr([if k & 1 == 0 { lo[0] } else { hi[0] }, if k & 2 == 0 { lo[1] } else { hi[1] }, if k & 4 == 0 { lo[2] } else { hi[2] }])))).collect();
    let r2 = |v: f32| (v * 1000.0).round() / 1000.0;
    if bar {
        let centre = position(t.point(to_mvr([(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0])));
        // The box's longest side (glTF x, y or z) is the bar.
        let size = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
        let axis = (0..3).max_by(|a, b| size[*a].total_cmp(&size[*b])).unwrap_or(0);
        let mut e = [0.0; 3];
        e[axis] = size[axis] / 2.0;
        let end = |sign: f32| {
            let mut d = to_mvr(e.map(|v| v * sign));
            d = apply(&t.r, d);
            let p = position(d);
            [r2(centre[0] + p[0]), r2(centre[2] + p[2])]
        };
        PlotShape { kind: ShapeKind::Bar, a: end(-1.0), b: end(1.0), height: r2(centre[1]), ..Default::default() }
    } else {
        let (mut x0, mut x1, mut z0, mut z1, mut top) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN, f32::MIN);
        for c in &corners {
            x0 = x0.min(c[0]);
            x1 = x1.max(c[0]);
            z0 = z0.min(c[2]);
            z1 = z1.max(c[2]);
            top = top.max(c[1]);
        }
        let height = if top <= SLAB * 1.5 { 0.0 } else { r2(top) };
        PlotShape { kind: ShapeKind::Rect, a: [r2(x0), r2(z0)], b: [r2(x1), r2(z1)], height, ..Default::default() }
    }
}

/// Read an MVR file. `known` are the types to match by GDTF name before reading the file's own
/// GDTFs (the show's first, then the library and the built-ins).
pub fn import(zip: &[u8], known: &[&FixtureType]) -> anyhow::Result<Imported> {
    use roxmltree::{Document, Node};
    let xml = super::zipread::read(zip, "GeneralSceneDescription.xml", MAX_SCENE)?;
    let text = std::str::from_utf8(&xml).map_err(|_| anyhow::anyhow!("GeneralSceneDescription.xml is not UTF-8"))?;
    let text = super::import::strip_doctype(text)?;
    let doc = Document::parse_with_options(&text, roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: 8_000_000, ..Default::default() })
        .map_err(|e| anyhow::anyhow!("GeneralSceneDescription.xml: {e}"))?;
    let scene = doc.descendants().find(|n| n.has_tag_name("Scene")).ok_or_else(|| anyhow::anyhow!("not an MVR file (no Scene)"))?;
    let child_text = |n: Node, tag: &str| n.children().find(|c| c.has_tag_name(tag)).and_then(|c| c.text()).map(|t| t.trim().to_string());
    // World transform: every ancestor's Matrix, outermost first.
    let world = |n: Node| {
        let mut chain: Vec<Transform> = n.ancestors().filter_map(|a| child_text(a, "Matrix").and_then(|m| parse_matrix(&m))).collect();
        chain.reverse();
        chain.iter().fold(IDENTITY, |acc, t| acc.then(t))
    };
    let mut out = Imported::default();
    let mut types: HashMap<String, Option<FixtureType>> = HashMap::new();
    let names = super::zipread::names(zip)?;
    let (mut generic, mut no_mode, mut no_geometry) = (0usize, 0usize, 0usize);
    let dimmer = super::library::generics().into_iter().find(|t| t.id == "generic/dimmer").expect("built-in dimmer");
    for n in scene.descendants().filter(|n| n.has_tag_name("Fixture")) {
        if out.fixtures.len() >= MAX_FIXTURES {
            out.notes.push(format!("more than {MAX_FIXTURES} fixtures: the rest left out"));
            break;
        }
        let spec = child_text(n, "GDTFSpec").unwrap_or_default();
        let key = spec.trim_end_matches(".gdtf").to_lowercase();
        if !types.contains_key(&key) {
            let found = known.iter().find(|t| gdtf_name(&t.manufacturer, &t.name).trim_end_matches(".gdtf").to_lowercase() == key).map(|t| (*t).clone());
            let found = found.or_else(|| {
                let file = names.iter().find(|f| f.trim_end_matches(".gdtf").eq_ignore_ascii_case(spec.trim_end_matches(".gdtf")) && !spec.is_empty())?;
                let bytes = super::zipread::read(zip, file, super::import::MAX_FILE as usize).ok()?;
                match crate::recovery::catch(|| super::gdtf::import(&bytes)) {
                    Ok(r) => {
                        out.notes.extend(r.notes.iter().map(|x| format!("{spec}: {x}")));
                        Some(r.fixture)
                    }
                    Err(e) => {
                        out.notes.push(format!("{spec}: {e:#}"));
                        None
                    }
                }
            });
            types.insert(key.clone(), found);
        }
        let ty = match &types[&key] {
            Some(t) => t,
            None => {
                generic += 1;
                &dimmer
            }
        };
        let want = child_text(n, "GDTFMode").unwrap_or_default();
        // The type's own mode name, or the one an OpenQ export gave it (a library type's "Standard [Lamp]"
        // is "Standard (Lamp)" in the file).
        let exported = super::gdtf::mode_names(ty);
        let found = ty.modes.iter().find(|m| m.name == want).or_else(|| ty.modes.iter().zip(&exported).find(|(_, e)| **e == want).map(|(m, _)| m));
        let mode = match found {
            Some(m) => m.name.clone(),
            None => {
                no_mode += (ty.id != dimmer.id) as usize;
                ty.modes.first().map(|m| m.name.clone()).unwrap_or_default()
            }
        };
        if !out.types.iter().any(|t| t.id == ty.id) {
            out.types.push(ty.clone());
        }
        let id = child_text(n, "FixtureIDNumeric").and_then(|v| v.parse::<FixtureId>().ok()).filter(|v| *v > 0).or_else(|| child_text(n, "FixtureID").and_then(|v| v.parse().ok())).unwrap_or(0);
        let t = world(n);
        let mut f = Fixture {
            id: id.min(super::model::MAX_FIXTURE_ID),
            name: n.attribute("name").unwrap_or("").to_string(),
            fixture_type: ty.id.clone(),
            mode,
            position: position(t.o),
            rotation: angles(&t.r),
            ..Default::default()
        };
        for a in n.descendants().filter(|c| c.has_tag_name("Address")) {
            let brk: usize = a.attribute("break").and_then(|b| b.parse().ok()).unwrap_or(0);
            let v = a.text().unwrap_or("").trim();
            let at = match v.split_once('.') {
                Some((u, d)) => u.parse::<u32>().ok().zip(d.parse::<u32>().ok()).filter(|(u, d)| *u >= 1 && (1..=512).contains(d)),
                None => v.parse::<u32>().ok().filter(|x| *x >= 1).map(|x| ((x - 1) / 512 + 1, (x - 1) % 512 + 1)),
            };
            let Some((u, d)) = at.filter(|(u, _)| *u <= super::model::MAX_UNIVERSE as u32) else { continue };
            let at = DmxAddress { universe: u as u16, address: d as u16 };
            match brk {
                0 => (f.universe, f.address) = (at.universe, at.address),
                b if b <= 8 => {
                    if f.breaks.len() < b {
                        f.breaks.resize(b, DmxAddress::default());
                    }
                    f.breaks[b - 1] = at;
                }
                _ => {}
            }
        }
        out.fixtures.push(f);
    }
    // Trusses and scene objects with a glTF box.
    for n in scene.descendants().filter(|n| n.has_tag_name("Truss") || n.has_tag_name("SceneObject") || n.has_tag_name("Support")) {
        let files: Vec<&str> = n.descendants().filter(|g| g.has_tag_name("Geometry3D")).filter_map(|g| g.attribute("fileName")).collect();
        let bounds = files.iter().filter(|f| f.to_lowercase().ends_with(".glb")).find_map(|f| glb_bounds(&super::zipread::read(zip, f, MAX_MESH).ok()?));
        let Some((lo, hi)) = bounds else {
            no_geometry += 1;
            continue;
        };
        let mut s = shape_of(&world(n), lo, hi, !n.has_tag_name("SceneObject"));
        s.id = out.shapes.len() as u32 + 1;
        s.name = n.attribute("name").unwrap_or("").to_string();
        out.shapes.push(s);
    }
    if out.fixtures.is_empty() && out.shapes.is_empty() {
        anyhow::bail!("no fixtures in the file");
    }
    if generic > 0 {
        out.notes.push(format!("{generic} fixtures have a type that is neither in the file nor in the library: patched as dimmers"));
    }
    if no_mode > 0 {
        out.notes.push(format!("{no_mode} fixtures have a mode their type doesn't know: the first mode was used"));
    }
    if no_geometry > 0 {
        out.notes.push(format!("{no_geometry} trusses or objects have no glTF geometry: left off the plot"));
    }
    Ok(out)
}

/// Put an import into a design: types it lacks, the fixtures (a number already taken, or none,
/// gets the next free one) and the shapes (new ids). Returns notes.
pub fn merge(l: &mut Lighting, imp: Imported) -> Vec<String> {
    let mut notes = imp.notes;
    for t in &imp.types {
        l.add_type(t);
    }
    let mut renumbered = 0;
    for mut f in imp.fixtures {
        if f.id == 0 || l.fixture(f.id).is_some() {
            renumbered += (f.id != 0) as usize;
            f.id = l.next_fixture_id(if f.id == 0 { 1 } else { f.id });
        }
        l.fixtures.push(f);
    }
    if renumbered > 0 {
        notes.push(format!("{renumbered} fixture numbers were taken: renumbered"));
    }
    let mut next = l.shapes.iter().map(|s| s.id).max().unwrap_or(0);
    for mut s in imp.shapes {
        next += 1;
        s.id = next;
        l.shapes.push(s);
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::library;

    #[test]
    fn stage_axes_are_mvr_axes() {
        // What the UI shows (model::to_stage) is the MVR position, in metres.
        let f = Fixture { position: [1.5, 6.0, -2.0], ..Default::default() };
        assert!(matrix(&f).ends_with("{1500,2000,6000}"), "{}", matrix(&f));
        assert_eq!(crate::light::model::to_stage(f.position), [1.5, 2.0, 6.0]);
        assert_eq!(crate::light::model::from_stage([1.5, 2.0, 6.0]), f.position);
        assert!(crate::light::model::to_stage([0.0; 3])[1].is_sign_positive(), "no -0 shown");
    }

    #[test]
    fn transform_axes() {
        let mut f = Fixture { position: [1.0, 6.0, -2.0], ..Default::default() };
        assert_eq!(matrix(&f), "{1,0,0}{0,1,0}{0,0,1}{1000,2000,6000}");
        // Heading 90°: the fixture's axis towards the audience (OpenQ +z) turns to OpenQ +x,
        // which is MVR +x.
        f.rotation = [0.0, 90.0, 0.0];
        let m = mul(&TO_MVR, &rotation(f.rotation));
        let fwd = [m[0][2], m[1][2], m[2][2]];
        assert!((fwd[0] - 1.0).abs() < 1e-6 && fwd[1].abs() < 1e-6 && fwd[2].abs() < 1e-6, "{fwd:?}");
        // Rotation matrices stay orthonormal.
        f.rotation = [30.0, 45.0, 10.0];
        let r = rotation(f.rotation);
        for col in 0..3 {
            let n: f32 = r.iter().map(|row| row[col] * row[col]).sum();
            assert!((n - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn scene_lists_fixtures() {
        let mut l = library::bench_rig(3, 1, 1);
        l.fixtures[0].name = "Spot <1> & \"co\"".into();
        l.fixtures[2].address = 0;
        let x = scene(&l, "Show");
        assert_eq!(x.matches("<Fixture ").count(), 3);
        assert!(x.contains("Spot &lt;1&gt; &amp; &quot;co&quot;"));
        assert_eq!(x.matches("<Address ").count(), 2, "unpatched fixture has no address");
        assert_eq!(scene(&l, "Show"), x, "stable UUIDs");
        assert_ne!(scene(&l, "Other"), x);
        let zip = export(&l, "Show").unwrap();
        assert_eq!(crate::light::zipread::read(&zip, "GeneralSceneDescription.xml", 1 << 20).unwrap(), x.as_bytes());
        assert_eq!(gdtf_name("Robe Lighting", "Robin 600E/Spot"), "Robe_Lighting@Robin_600E_Spot.gdtf");
        assert!(!x.contains("<Truss"), "no plot layer without shapes");
    }

    #[test]
    fn modes_named_like_the_exported_gdtf() {
        let mut l = library::bench_rig(1, 1, 1);
        let ty = l.fixtures[0].fixture_type.clone();
        let mode = l.fixtures[0].mode.clone();
        let t = l.fixture_types.iter_mut().find(|t| t.id == ty).unwrap();
        t.modes.iter_mut().find(|m| m.name == mode).unwrap().name = "Standard [Lamp, 16.bit]".into();
        for f in l.fixtures.iter_mut().filter(|f| f.fixture_type == ty) {
            f.mode = "Standard [Lamp, 16.bit]".into();
        }
        let x = scene(&l, "Show");
        assert!(x.contains("<GDTFMode>Standard (Lamp_ 16_bit)</GDTFMode>"), "{x}");
        // Read back with the show's own type, its mode is found by the exported name.
        let t = l.fixture_type(&ty).unwrap().clone();
        let back = import(&export(&l, "Show").unwrap(), &[&t]).unwrap();
        let f = back.fixtures.iter().find(|f| f.fixture_type == ty).unwrap();
        assert_eq!(f.mode, "Standard [Lamp, 16.bit]");
    }

    #[test]
    fn shapes_become_truss_and_scene_objects() {
        let mut l = library::bench_rig(1, 1, 1);
        // A bar 4 m long across the stage at 6 m, 2 m upstage (OpenQ z = −2); a 2 × 1 m riser.
        l.shapes.push(PlotShape { id: 1, kind: ShapeKind::Bar, name: "LX1".into(), a: [-2.0, -2.0], b: [2.0, -2.0], height: 6.0, label: [0.0, 0.0] });
        l.shapes.push(PlotShape { id: 2, kind: ShapeKind::Rect, a: [0.0, 0.0], b: [2.0, 1.0], height: 0.4, ..Default::default() });
        let x = scene(&l, "Show");
        assert!(x.contains("<Truss name=\"LX1\""), "{x}");
        assert!(x.contains("<Matrix>{1,0,0}{0,1,0}{0,0,1}{0,2000,6000}</Matrix>"), "{x}");
        assert!(x.contains("<SceneObject name=\"Rectangle 2\""));
        assert_eq!(x.matches("<FixtureID></FixtureID>").count(), 2, "the schema wants one per shape");
        assert!(x.contains("{1000,-500,0}"), "riser centre");
        assert!(x.contains("fileName=\"shape-2.glb\""));
        // A bar along OpenQ z turns 90° about MVR z.
        let (m, min, max) = shape_box(&PlotShape { kind: ShapeKind::Bar, a: [0.0, 0.0], b: [0.0, -3.0], ..Default::default() });
        assert!(m.starts_with("{0,1,0}{-1,0,0}"), "{m}");
        assert_eq!((min[0], max[0]), (-1.5, 1.5));
        // Every mesh is in the zip and is a well-formed GLB.
        let zip = export(&l, "Show").unwrap();
        for id in [1, 2] {
            let glb = crate::light::zipread::read(&zip, &format!("shape-{id}.glb"), 1 << 20).unwrap();
            assert_eq!(&glb[..4], b"glTF");
            assert_eq!(u32::from_le_bytes(glb[8..12].try_into().unwrap()) as usize, glb.len());
            let jlen = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
            let json: serde_json::Value = serde_json::from_slice(&glb[20..20 + jlen]).unwrap();
            let blen = json["buffers"][0]["byteLength"].as_u64().unwrap() as usize;
            assert_eq!(u32::from_le_bytes(glb[20 + jlen..24 + jlen].try_into().unwrap()) as usize, blen);
            assert_eq!(&glb[24 + jlen..28 + jlen], b"BIN\0");
            assert_eq!(json["accessors"][2]["count"], 36);
        }
        let riser = box_glb([-1.0, 0.0, -0.5], [1.0, 0.4, 0.5]);
        assert_eq!(riser.len() % 4, 0);
    }

    /// An exported file reads back as the same patch: positions, rotations, addresses, types
    /// (from the GDTFs in the file, or matched in the library), modes and shapes.
    #[test]
    fn export_imports_back() {
        let mut l = library::bench_rig(9, 1, 1);
        for (i, f) in l.fixtures.iter_mut().enumerate() {
            f.position = [i as f32 - 4.0, 6.5, -2.0 + i as f32 * 0.25];
            f.rotation = [[0.0, 0.0, 0.0], [30.0, 0.0, 0.0], [10.0, 45.0, 5.0], [0.0, -120.0, 0.0], [-60.0, 170.0, 20.0]][i % 5];
            f.name = format!("F{i}");
        }
        l.fixtures[3].universe = 3;
        l.fixtures[4].address = 0;
        l.shapes.push(PlotShape { id: 4, kind: ShapeKind::Bar, name: "LX1".into(), a: [-2.0, -2.0], b: [2.0, -1.0], height: 6.0, label: [0.0, 0.0] });
        l.shapes.push(PlotShape { id: 9, kind: ShapeKind::Rect, name: "Riser".into(), a: [0.0, 0.0], b: [2.0, 1.0], height: 0.4, ..Default::default() });
        let zip = export(&l, "Show").unwrap();
        let names = crate::light::zipread::names(&zip).unwrap();
        assert!(names.iter().any(|n| n == "Generic@Dimmer.gdtf"), "{names:?}");
        // Nothing known: the types come from the file's GDTFs.
        let imp = import(&zip, &[]).unwrap();
        assert!(imp.notes.is_empty(), "{:?}", imp.notes);
        assert_eq!(imp.types.len(), 3);
        assert!(imp.types.iter().all(|t| t.id.starts_with("gdtf/")));
        assert_eq!(imp.fixtures.len(), 9);
        for (a, b) in l.fixtures.iter().zip(&imp.fixtures) {
            assert_eq!((a.id, a.name.as_str(), a.mode.as_str(), a.universe, a.address), (b.id, b.name.as_str(), b.mode.as_str(), if a.address == 0 { 1 } else { b.universe }, b.address));
            for k in 0..3 {
                assert!((a.position[k] - b.position[k]).abs() < 1e-3, "{:?} {:?}", a.position, b.position);
            }
            // The same orientation (angles may differ by a full turn).
            let (ra, rb) = (rotation(a.rotation), rotation(b.rotation));
            for i in 0..3 {
                for j in 0..3 {
                    assert!((ra[i][j] - rb[i][j]).abs() < 1e-3, "{:?} vs {:?}", a.rotation, b.rotation);
                }
            }
        }
        assert_eq!(imp.shapes.len(), 2);
        let bar = &imp.shapes[0];
        assert_eq!((bar.kind, bar.name.as_str(), bar.height), (ShapeKind::Bar, "LX1", 6.0));
        let ends = |s: &PlotShape| {
            let mut v = [s.a, s.b];
            v.sort_by(|x, y| x[0].total_cmp(&y[0]));
            v
        };
        assert_eq!(ends(bar), [[-2.0, -2.0], [2.0, -1.0]]);
        let riser = &imp.shapes[1];
        assert_eq!((riser.kind, riser.a, riser.b, riser.height), (ShapeKind::Rect, [0.0, 0.0], [2.0, 1.0], 0.4));
        // Types the library knows are used instead of the file's.
        let generics = library::generics();
        let known: Vec<&FixtureType> = generics.iter().collect();
        let imp = import(&zip, &known).unwrap();
        assert!(imp.types.iter().all(|t| t.id.starts_with("generic/")), "{:?}", imp.types.iter().map(|t| &t.id).collect::<Vec<_>>());
        // Into the same design: clashing numbers move on, shapes get new ids.
        let mut again = l.clone();
        let notes = merge(&mut again, imp);
        assert_eq!(again.fixtures.len(), 18);
        assert!(notes.iter().any(|n| n.contains("renumbered")));
        assert_eq!(again.shapes.iter().map(|s| s.id).collect::<Vec<_>>(), [4, 9, 10, 11]);
        assert!(again.validate().iter().all(|i| !matches!(i, crate::light::model::PatchIssue::DuplicateId(_))));
    }

    #[test]
    fn foreign_files() {
        // Nested group transforms, "universe.address", an unknown type, a 3ds truss.
        let xml = r#"<?xml version="1.0"?><GeneralSceneDescription verMajor="1" verMinor="6"><Scene><Layers>
            <Layer name="L"><Matrix>{1,0,0}{0,1,0}{0,0,1}{0,0,1000}</Matrix><ChildList>
              <GroupObject name="G"><Matrix>{1,0,0}{0,1,0}{0,0,1}{500,0,0}</Matrix><ChildList>
                <Fixture name="Spot 1" uuid="x"><Matrix>{1,0,0}{0,1,0}{0,0,1}{0,-2000,5000}</Matrix>
                  <GDTFSpec>Nobody@Nothing.gdtf</GDTFSpec><GDTFMode>Mode 1</GDTFMode><FixtureID>101</FixtureID>
                  <Addresses><Address break="0">2.10</Address></Addresses></Fixture>
              </ChildList></GroupObject>
              <Truss name="T"><Geometries><Geometry3D fileName="truss.3ds"/></Geometries></Truss>
            </ChildList></Layer></Layers></Scene></GeneralSceneDescription>"#;
        let zip = crate::light::zipread::write(&[("GeneralSceneDescription.xml", xml.as_bytes())]).unwrap();
        let imp = import(&zip, &[]).unwrap();
        let f = &imp.fixtures[0];
        assert_eq!((f.id, f.universe, f.address, f.fixture_type.as_str()), (101, 2, 10, "generic/dimmer"));
        assert_eq!(f.position, [0.5, 6.0, 2.0]);
        assert_eq!(imp.notes.len(), 2, "{:?}", imp.notes);
        assert!(import(&crate::light::zipread::write(&[("GeneralSceneDescription.xml", b"<GeneralSceneDescription><Scene/></GeneralSceneDescription>")]).unwrap(), &[]).is_err());
        assert!(import(b"junk", &[]).is_err());
        assert!(parse_matrix("{1,0,0}{0,1,0}{0,0,1}").is_none());
    }
}
