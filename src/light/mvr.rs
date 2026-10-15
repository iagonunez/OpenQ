//! MVR export (My Virtual Rig, DIN SPEC 15801): the patch with positions for visualisers and
//! other desks.
//!
//! The file is a zip holding `GeneralSceneDescription.xml`: one layer with a `Fixture` per
//! patched fixture (name, stable UUID, transform, `GDTFSpec` + mode, fixture number, DMX
//! address). OpenQ keeps no GDTF files, so `GDTFSpec` names the GDTF Share file the type would
//! have (`Manufacturer@Name.gdtf`); visualisers match it with their own library.
//!
//! Coordinates: OpenQ uses metres with x right, y up, z towards the audience; MVR uses
//! millimetres with x right, y upstage, z up. Rotations are degrees about x, y and z, applied as
//! heading (y) · tilt (x) · roll (z).
//!
//! Plot shapes go in a second layer: a bar is a `Truss` (a 30 cm box along the bar, centred on
//! its height), a rectangle a `SceneObject` (a box from the floor up to its height, or a 2 cm
//! slab). Each has its own box mesh, `shape-<id>.glb`, in the file: glTF, so metres and y up,
//! which the importer turns into MVR's z up.

use super::model::{Fixture, Lighting, PlotShape, ShapeKind};
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
    for f in &l.fixtures {
        let ty = l.fixture_type(&f.fixture_type);
        let (manufacturer, type_name) = ty.map_or(("", f.fixture_type.as_str()), |t| (t.manufacturer.as_str(), t.name.as_str()));
        let name = if f.name.trim().is_empty() { format!("{type_name} {}", f.id) } else { f.name.clone() };
        x.push_str(&format!("          <Fixture name=\"{}\" uuid=\"{}\">\n", esc(&name), stable_uuid(scope, &format!("fixture {}", f.id))));
        x.push_str(&format!("            <Matrix>{}</Matrix>\n", matrix(f)));
        x.push_str(&format!("            <GDTFSpec>{}</GDTFSpec>\n", esc(&gdtf_name(manufacturer, type_name))));
        x.push_str(&format!("            <GDTFMode>{}</GDTFMode>\n", esc(&f.mode)));
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
            x.push_str(&format!("          </{tag}>\n"));
        }
        x.push_str("        </ChildList>\n      </Layer>\n");
    }
    x.push_str("    </Layers>\n  </Scene>\n</GeneralSceneDescription>\n");
    x
}

/// The MVR file of a design.
pub fn export(l: &Lighting, scope: &str) -> anyhow::Result<Vec<u8>> {
    let scene = scene(l, scope);
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
    super::zipread::write(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::library;

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
    fn shapes_become_truss_and_scene_objects() {
        let mut l = library::bench_rig(1, 1, 1);
        // A bar 4 m long across the stage at 6 m, 2 m upstage (OpenQ z = −2); a 2 × 1 m riser.
        l.shapes.push(PlotShape { id: 1, kind: ShapeKind::Bar, name: "LX1".into(), a: [-2.0, -2.0], b: [2.0, -2.0], height: 6.0 });
        l.shapes.push(PlotShape { id: 2, kind: ShapeKind::Rect, a: [0.0, 0.0], b: [2.0, 1.0], height: 0.4, ..Default::default() });
        let x = scene(&l, "Show");
        assert!(x.contains("<Truss name=\"LX1\""), "{x}");
        assert!(x.contains("<Matrix>{1,0,0}{0,1,0}{0,0,1}{0,2000,6000}</Matrix>"), "{x}");
        assert!(x.contains("<SceneObject name=\"Rectangle 2\""));
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
}
