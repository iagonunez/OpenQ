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

use super::model::{Fixture, Lighting};
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
        if f.address > 0 && f.universe > 0 {
            let absolute = (f.universe as u32 - 1) * 512 + f.address as u32;
            x.push_str(&format!("            <Addresses>\n              <Address break=\"0\">{absolute}</Address>\n            </Addresses>\n"));
        }
        x.push_str("          </Fixture>\n");
    }
    x.push_str("        </ChildList>\n      </Layer>\n    </Layers>\n  </Scene>\n</GeneralSceneDescription>\n");
    x
}

/// The MVR file of a design.
pub fn export(l: &Lighting, scope: &str) -> anyhow::Result<Vec<u8>> {
    super::zipread::write(&[("GeneralSceneDescription.xml", scene(l, scope).as_bytes())])
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
    }
}
