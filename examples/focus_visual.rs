//! Visual test of focus points as references, for OQVis.
//!
//! Rig: a FOH truss of six spots (7 m high, 8 m in front of the stage centre) and two spots on LX1
//! (6 m high, 2 m upstage), a focus point "Centre" on the floor at the stage centre and one cue
//! aiming every spot at it (`Value::Point`). Then the FOH truss really hangs 1 m further back
//! (towards the audience) than the patch says. Steps:
//!
//! 1. As designed: OQVis shows the patch, every beam on the centre.
//! 2. FOH truss 1 m back: OQVis shows the real rig, OpnQ still has the old patch; FOH beams miss.
//! 3. The operator puts FOH 1 on the real centre by hand: it keeps the point as reference and the
//!    move becomes its offset (`focus::move_from_point`, what the programmer does).
//! 4. "Correct the rig" with the group "FOH" picked, height and depth: the group's fixtures move in
//!    the patch (`focus::rig_offset` from the beams of its hand-aimed fixtures + `focus::shift`),
//!    the point stays; the cue (unchanged) aims every fixture at the real centre again.
//!
//! `cargo run --example focus_visual -- write <dir>` writes `patch.mvr` (the design) and `real.mvr`
//! (FOH truss 1 m back) and prints where every beam lands per step.
//! `cargo run --example focus_visual -- send <step>` sends that step's DMX by Art-Net to 127.0.0.1
//! (universe 1 = Art-Net port 0) until killed.

use opnq::light::focus::{self, Aim, Axes};
use opnq::light::model::{Attribute as A, Fixture, FixtureId, FocusPoint, Group, LightCue, Lighting, Look, PlotShape, Sequence, ShapeKind, Value};
use opnq::light::playback::{LightShow, Playback};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const FOH: [FixtureId; 6] = [1, 2, 3, 4, 5, 6];
const LX1: [FixtureId; 2] = [11, 12];
/// The fixture the operator aims by hand (off the centre line: one beam then tells height from depth).
const HAND: FixtureId = 1;
/// The group the operator picks to correct.
const FOH_GROUP: u32 = 1;
/// How far the FOH truss really hangs from where the patch has it (patch axes: +z = towards the audience).
const MOVED: [f32; 3] = [0.0, 0.0, 1.0];

fn rig() -> Lighting {
    let mut l = Lighting::default();
    let t = opnq::light::library::generics().into_iter().find(|t| t.id == "generic/spot").expect("generic spot");
    l.add_type(&t);
    let mut fixtures = Vec::new();
    for (i, id) in FOH.iter().enumerate() {
        let x = -5.0 + 2.0 * i as f32;
        fixtures.push(Fixture { id: *id, name: format!("FOH {}", i + 1), fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), universe: 1, address: 1 + 20 * i as u16, position: [x, 7.0, 8.0], ..Default::default() });
    }
    for (i, id) in LX1.iter().enumerate() {
        let x = -3.0 + 6.0 * i as f32;
        fixtures.push(Fixture { id: *id, name: format!("LX1 {}", i + 1), fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), universe: 1, address: 200 + 20 * i as u16, position: [x, 6.0, -2.0], ..Default::default() });
    }
    l.fixtures = fixtures.into();
    l.shapes = vec![
        PlotShape { id: 1, kind: ShapeKind::Bar, name: "FOH".into(), a: [-6.0, 8.0], b: [6.0, 8.0], height: 7.0, ..Default::default() },
        PlotShape { id: 2, kind: ShapeKind::Bar, name: "LX1".into(), a: [-4.0, -2.0], b: [4.0, -2.0], height: 6.0, ..Default::default() },
        // A small mark on the floor at the real stage centre.
        PlotShape { id: 3, kind: ShapeKind::Rect, name: "Centre mark".into(), a: [-0.1, -0.1], b: [0.1, 0.1], height: 0.02, ..Default::default() },
    ];
    l.groups.push(Group { id: FOH_GROUP, name: "FOH".into(), fixtures: FOH.to_vec() });
    l.groups.push(Group { id: 2, name: "LX1".into(), fixtures: LX1.to_vec() });
    l.points.push(FocusPoint { id: 1, name: "Centre".into(), position: [0.0; 3] });
    // Cue 1: every spot open, narrow, aimed at the centre point.
    let mut look = Look::new();
    for f in FOH.iter().chain(&LX1) {
        look.insert(*f, BTreeMap::from([(A::Dimmer, Value::Level(1.0)), (A::Shutter, Value::Level(1.0)), (A::Zoom, Value::Level(0.06)), (A::Pan, Value::point(1)), (A::Tilt, Value::point(1))]));
    }
    l.sequences.push(Sequence { id: 1, name: "Main".into(), cues: vec![LightCue { number: "1".into(), values: look.into(), ..Default::default() }].into(), ..Default::default() });
    l
}

/// The rig as it really hangs: FOH truss moved by [`MOVED`].
fn real(l: &Lighting) -> Lighting {
    let mut r = l.clone();
    focus::shift(&mut r, &FOH, MOVED);
    r
}

/// The pan and tilt that aim fixture `f` of `l` at `target`.
fn aim_at(l: &Lighting, f: FixtureId, target: [f32; 3]) -> (f32, f32) {
    let (fixture, t) = (l.fixtures.iter().find(|x| x.id == f).unwrap(), &l.fixture_types[0]);
    focus::aim(fixture, &t.physical, target).unwrap()
}

/// OpnQ's design at a step, and its programmer.
fn step(n: u32) -> (Lighting, Look) {
    let design = rig();
    let truth = real(&design);
    // The operator moves a fixture by hand until it lights the real centre: the pan and tilt
    // that reach it from where the fixture really hangs.
    let hand = |l: &Lighting, f: FixtureId| {
        let (pan, tilt) = aim_at(&truth, f, [0.0; 3]);
        let mut values = l.sequences[0].cues[0].values[&f].clone();
        values.retain(|a, _| matches!(a, A::Pan | A::Tilt));
        focus::move_from_point(l, f, &mut values, A::Pan, pan);
        focus::move_from_point(l, f, &mut values, A::Tilt, tilt);
        (f, values)
    };
    // The calibration beam of a fixture in the programmer.
    let beam = |l: &Lighting, programmer: &Look, f: FixtureId| {
        let v = &programmer[&f];
        let get = |a| l.resolve(f, a, v[&a]).unwrap();
        let fixture = l.fixtures.iter().find(|x| x.id == f).unwrap();
        Aim::new(fixture, &l.fixture_types[0].physical, get(A::Pan), get(A::Tilt), l.points[0].position).unwrap()
    };
    match n {
        1 | 2 => (design, Look::new()),
        3 => {
            let p = Look::from([hand(&design, HAND)]);
            (design, p)
        }
        4 => {
            // As the programmer does it: the picked group moves, only its hand-aimed fixtures count.
            let mut l = design;
            let p = Look::from([hand(&l, HAND)]);
            let group = l.groups.iter().find(|g| g.id == FOH_GROUP).unwrap().fixtures.clone();
            let beams: Vec<Aim> = p.keys().filter(|f| group.contains(f)).map(|f| beam(&l, &p, *f)).collect();
            let fit = focus::rig_offset(&beams, Axes::HeightDepth).expect("rig offset");
            println!("  (rig correction: FOH moved by x {:+.2} y {:+.2} z {:+.2}, beams within {:.0} cm)", fit.value[0], fit.value[1], fit.value[2], fit.miss * 100.0);
            focus::shift(&mut l, &group, fit.value);
            (l, Look::new())
        }
        _ => panic!("steps 1 to 4"),
    }
}

/// The DMX OpnQ sends at a step: cue 1 playing, the programmer on top.
fn dmx(l: &Lighting, programmer: &Look) -> Vec<u8> {
    let t0 = Instant::now();
    let mut p = Playback::new(Arc::new(LightShow::compile(l)), t0);
    p.go(1, None, t0).unwrap();
    let values: Vec<(FixtureId, A, f32)> = programmer.iter().flat_map(|(f, v)| v.iter().filter_map(move |(a, x)| Some((*f, *a, l.resolve(*f, *a, *x)?)))).collect();
    p.set_programmer(&values);
    p.tick(t0 + Duration::from_secs(60)).get(1).unwrap().data.to_vec()
}

/// Where each beam lands on the floor of the real rig (from the pan/tilt OpnQ sends).
fn report(n: u32) {
    let (l, programmer) = step(n);
    let truth = if n == 1 { rig() } else { real(&rig()) };
    let t = &l.fixture_types[0];
    let ph = l.points[0].position;
    println!("step {n}: point at x {:.2} y {:.2} z {:.2} (patch axes)", ph[0], ph[1], ph[2]);
    for f in FOH.iter().chain(&LX1) {
        let v = programmer.get(f).cloned().unwrap_or_else(|| l.sequences[0].cues[0].values[f].clone());
        let (pan, tilt) = (l.resolve(*f, A::Pan, v[&A::Pan]).unwrap(), l.resolve(*f, A::Tilt, v[&A::Tilt]).unwrap());
        let fixture = truth.fixtures.iter().find(|x| x.id == *f).unwrap();
        let hit = focus::reach(fixture, &t.physical, pan, tilt, 0.0).unwrap();
        let name = &fixture.name;
        let note = match (n, v[&A::Pan]) {
            (3, Value::Point { offset, .. }) if *f == HAND => format!("  (by hand, offset {:.2} {:.2} {:.2})", offset[0], offset[1], offset[2]),
            _ => String::new(),
        };
        println!("  {name:6} lands at x {:+.2} z {:+.2}: {:.0} cm from the real centre{note}", hit[0], hit[2], hit[0].hypot(hit[2]) * 100.0);
    }
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
    match args.first().map(String::as_str) {
        Some("write") => {
            let dir = std::path::PathBuf::from(args.get(1).map_or("target/focus-visual", String::as_str));
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("patch.mvr"), opnq::light::mvr::export(&rig(), "OpnQ")?)?;
            std::fs::write(dir.join("real.mvr"), opnq::light::mvr::export(&real(&rig()), "OpnQ")?)?;
            for n in 1..=4 {
                report(n);
            }
        }
        Some("send") => {
            let n: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
            let (l, programmer) = step(n);
            let data = dmx(&l, &programmer);
            let sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
            for seq in (1..=255u8).cycle() {
                sock.send_to(&artnet(0, seq, &data), "127.0.0.1:6454")?;
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        _ => eprintln!("focus_visual write <dir> | send <step>"),
    }
    Ok(())
}
