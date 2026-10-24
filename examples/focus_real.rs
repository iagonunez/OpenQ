//! Visual test of focus points with manufacturers' GDTF files, for OQVis: every moving head in
//! a folder of GDTF files hangs on a FOH truss (7 m up, 8 m downstage) and stands on the floor
//! upstage (turned over), all aimed at a point on the floor at the stage centre by one cue
//! (`Value::Point`). OQVis draws the manufacturers' own files, so a type whose pan turns the
//! other way, whose head sits off the hanging point or is turned in the file, shows up as a beam
//! that misses the centre.
//!
//! `cargo run --example focus_real -- write <gdtf dir> <out dir>` writes `<out dir>/focus-real.mvr`
//! (the manufacturers' files inside) and prints where OpnQ's own maths puts each beam.
//! `cargo run --example focus_real -- send <gdtf dir>` sends the cue's DMX by Art-Net to
//! 127.0.0.1 (universe n = Art-Net port n − 1) until killed.

use opnq::light::focus;
use opnq::light::model::{Attribute as A, Fixture, FixtureType, FocusPoint, LightCue, Lighting, Look, PlotShape, Sequence, ShapeKind, Value};
use opnq::light::playback::{LightShow, Playback};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The moving heads among the GDTF files of `dir`, with the file each came from.
fn types(dir: &Path) -> Vec<(FixtureType, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir).expect("gdtf dir").filter_map(|e| Some(e.ok()?.path())).filter(|p| p.extension().is_some_and(|x| x == "gdtf")).collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|p| {
            let bytes = std::fs::read(&p).ok()?;
            let t = opnq::light::gdtf::import(&bytes).ok()?.fixture;
            t.physical.moving.then_some((t, bytes))
        })
        .collect()
}

fn rig(dir: &Path) -> (Lighting, HashMap<String, Vec<u8>>) {
    let mut l = Lighting::default();
    let mut originals = HashMap::new();
    let found = types(dir);
    let n = found.len() as f32;
    let (mut universe, mut address) = (1u16, 1u16);
    let mut id = 1;
    for (i, (t, bytes)) in found.iter().enumerate() {
        l.add_type(t);
        originals.insert(t.id.clone(), bytes.clone());
        let x = -7.0 + 14.0 * i as f32 / (n - 1.0).max(1.0);
        // On the FOH truss, and standing on the floor upstage, turned over.
        for (position, rotation) in [([x, 7.0, 8.0], [0.0; 3]), ([x * 0.8, 0.3, -5.0], [180.0, 0.0, 0.0])] {
            let footprint = t.modes[0].footprint();
            if address + footprint > 513 {
                (universe, address) = (universe + 1, 1);
            }
            l.fixtures.push(Fixture { id, name: format!("{} {}", t.name, id), fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), universe, address, position, rotation, ..Default::default() });
            address += footprint;
            id += 1;
        }
    }
    l.shapes = vec![
        PlotShape { id: 1, kind: ShapeKind::Bar, name: "FOH".into(), a: [-8.0, 8.0], b: [8.0, 8.0], height: 7.0, ..Default::default() },
        PlotShape { id: 2, kind: ShapeKind::Rect, name: "Centre mark".into(), a: [-0.1, -0.1], b: [0.1, 0.1], height: 0.02, ..Default::default() },
    ];
    l.points.push(FocusPoint { id: 1, name: "Centre".into(), position: [0.0; 3] });
    // Cue 1: every head open at full, aimed at the point.
    let mut look = Look::new();
    for f in l.fixtures.iter() {
        let t = l.fixture_type(&f.fixture_type).unwrap();
        let chans = &t.modes[0].channels;
        let mut v = BTreeMap::from([(A::Dimmer, Value::Level(1.0)), (A::Pan, Value::point(1)), (A::Tilt, Value::point(1))]);
        if let Some(open) = chans.iter().find(|c| c.attribute == A::Shutter).and_then(|c| c.find_range("open")) {
            v.insert(A::Shutter, Value::Level(open.value()));
        }
        look.insert(f.id, v);
    }
    l.sequences.push(Sequence { id: 1, name: "Main".into(), cues: vec![LightCue { number: "1".into(), values: look.into(), ..Default::default() }].into(), ..Default::default() });
    (l, originals)
}

/// The MVR of the rig with each type's GDTF replaced by the manufacturer's file.
fn mvr_with_originals(l: &Lighting, originals: &HashMap<String, Vec<u8>>) -> Vec<u8> {
    use opnq::light::{gdtf, mvr, zipread};
    let zip = mvr::export(l, "Focus real").expect("mvr");
    let specs = mvr::spec_names(l);
    let by_file: HashMap<&str, &Vec<u8>> = specs.iter().filter_map(|(id, file)| Some((file.as_str(), originals.get(id)?))).collect();
    let mut scene = String::from_utf8(zipread::read(&zip, "GeneralSceneDescription.xml", 64 << 20).expect("scene")).expect("utf-8");
    for t in &l.fixture_types {
        for (clean, m) in gdtf::mode_names(t).into_iter().zip(&t.modes) {
            if clean != m.name {
                scene = scene.replace(&format!("<GDTFMode>{clean}</GDTFMode>"), &format!("<GDTFMode>{}</GDTFMode>", m.name));
            }
        }
    }
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for name in zipread::names(&zip).expect("names") {
        let data = match by_file.get(name.as_str()) {
            Some(original) => (*original).clone(),
            None if name == "GeneralSceneDescription.xml" => scene.clone().into_bytes(),
            None => zipread::read(&zip, &name, 64 << 20).expect("entry"),
        };
        entries.push((name, data));
    }
    let refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, d)| (n.as_str(), d.as_slice())).collect();
    zipread::write(&refs).expect("zip")
}

fn artnet(port: u16, seq: u8, data: &[u8]) -> Vec<u8> {
    let mut p = b"Art-Net\0".to_vec();
    p.extend_from_slice(&0x5000u16.to_le_bytes());
    p.extend_from_slice(&[0, 14, seq, 0, (port & 0xff) as u8, (port >> 8) as u8]);
    p.extend_from_slice(&(data.len() as u16).to_be_bytes());
    p.extend_from_slice(data);
    p
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = Path::new(args.get(1).map_or("target/real-rig/profiles", String::as_str));
    let (l, originals) = rig(dir);
    match args.first().map(String::as_str) {
        Some("write") => {
            let out = Path::new(args.get(2).map_or("target/focus-real", String::as_str));
            std::fs::create_dir_all(out)?;
            std::fs::write(out.join("focus-real.mvr"), mvr_with_originals(&l, &originals))?;
            for f in l.fixtures.iter() {
                let t = l.fixture_type(&f.fixture_type).unwrap();
                let p = &t.physical;
                let (pan, tilt) = (l.resolve(f.id, A::Pan, Value::point(1)).unwrap(), l.resolve(f.id, A::Tilt, Value::point(1)).unwrap());
                let (from, dir) = focus::ray(f, p, pan, tilt).unwrap();
                let s = -from[1] / dir[1];
                let hit = [from[0] + s * dir[0], from[2] + s * dir[2]];
                println!(
                    "{:<34} pan {:>6.1}…{:<6.1} tilt {:>6.1}…{:<6.1} parts {:<3}  DMX pan {pan:.5} tilt {tilt:.5}  lands {:.0} cm from the centre",
                    f.name,
                    p.pan_span()[0],
                    p.pan_span()[1],
                    p.tilt_span()[0],
                    p.tilt_span()[1],
                    if p.parts.is_some() { "yes" } else { "no" },
                    hit[0].hypot(hit[1]) * 100.0
                );
            }
        }
        Some("send") => {
            let t0 = Instant::now();
            let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
            p.go(1, None, t0).unwrap();
            let frame = p.tick(t0 + Duration::from_secs(60)).clone();
            let sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
            for seq in (1..=255u8).cycle() {
                for u in &frame.universes {
                    sock.send_to(&artnet(u.id - 1, seq, &u.data), "127.0.0.1:6454")?;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        _ => eprintln!("focus_real write <gdtf dir> <out dir> | send <gdtf dir>"),
    }
    Ok(())
}
