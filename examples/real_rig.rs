//! Writes a lighting-only show made of manufacturers' own GDTF files, for trying OpnQ's programming
//! against OQVis: 53 fixtures of 15 real types hung on trusses with positions, and the design of
//! `capture_rig` (groups, presets of every family, effects of every shape, tracking, cue-only cues,
//! a block, feature timing, move in black, playback buttons on pages). An upstage truss holds the
//! types the design doesn't use (and a hazer) for programming by hand. No cues in the cue lists.
//!
//! `cargo run --example real_rig -- <gdtf dir> <out dir>` reads the GDTF files named in [`TYPES`]
//! from `<gdtf dir>` and writes `<out dir>/real-rig.opnq`, `<out dir>/real-rig.mvr` (the same rig
//! with the manufacturers' GDTF files, so OQVis draws their models), `<out dir>/real-rig-opnq.mvr`
//! (OpnQ's own export, as Patch → Export MVR… writes it: simple bodies) and a copy of each GDTF in
//! `<out dir>/profiles/`.

use opnq::light::dmx::{OutputConfig, OutputProtocol, UniverseRoute};
use opnq::light::effects::{AudioReact, Effect, EffectRun, Shape};
use opnq::light::model::{
    cell_id, Attribute as A, Feature, FeatureTime, Fixture, FocusPoint, FixtureId, FixtureType, Group, LightButton, LightCue, Lighting, Look, PlotItem, PlotNote, PlotShape,
    Preset, Sequence, ShapeKind, Value,
};
use opnq::model::Workspace;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// (GDTF file, mode patched). The first eight fill the roles of the design, the rest hang on the
/// upstage truss.
const TYPES: [(&str, &str); 15] = [
    ("Martin_Professional@ERA_800_Performance@Perastage.gdtf", "Basic"),
    ("SGM_Light@G-7_Spot@Rev_A.gdtf", "Standard"),
    ("Blizzard@TOURNado_WiMAX_COB_5@V1.2.gdtf", "11 Channel Mode"),
    ("Martin_Professional@MAC_Quantum_Wash@Perastage.gdtf", "Basic"),
    ("Martin_Professional@MAC_Viper_Profile@Perastage.gdtf", "Basic"),
    ("Clay_Paky@Sharpy_Plus@V.1.1_by_StefanoBigoloni.com_Standard_no_ch15_gobo_2_rotation_fine_it_is_in_progress.gdtf", "Standard"),
    ("GLP@impression_X4_Bar_20@Perastage.gdtf", "Dual Pixel Mode"),
    ("ELATION@Platinum_Beam_5R@initial.gdtf", "Default"),
    ("Robe Lighting@Robin Spiider.gdtf", "Mode 5 - Wash"),
    ("Robe_Lighting@Robin_MMX_Blade@2023-07-25__Beam_revision.gdtf", "Mode 1 - Standard"),
    ("Ayrton@Ghibli@V2.11_-_Soft_V201_-_3DReplaced.gdtf", "Standard_540"),
    ("JB-Lighting@P12_Spot_HP@V_1.15.gdtf", "Mode 1 (35 ch)"),
    ("Robe_Lighting@Robin_Viva_CMY@13042021.gdtf", "Mode 2 - Reduced 8 bit"),
    ("ACME@ACME_AE-610_BEAM@ACME_AE-610_BEAM.gdtf", "Mode 1 12 DMX"),
    ("MDG@theFAN@Perastage.gdtf", "Mode 2 Channel"),
];

// Presets.
const P_WARM: u32 = 1;
const P_BLUE: u32 = 2;
const P_RED: u32 = 3;
const P_AMBER: u32 = 4;
const P_MAGENTA: u32 = 5;
const P_GREEN: u32 = 6;
const P_WHITE: u32 = 7;
const P_CYAN: u32 = 8;
const P_CENTRE: u32 = 11;
const P_DSL: u32 = 12;
const P_DSR: u32 = 13;
const P_USC: u32 = 14;
const P_AUDIENCE: u32 = 15;
const P_CROSS: u32 = 16;
const P_FAN: u32 = 17;
const P_SKY: u32 = 18;
const P_GOBO: u32 = 21;
const P_PRISM: u32 = 22;
const P_TIGHT: u32 = 23;
const P_WIDE: u32 = 24;
const P_ALL_UP: u32 = 31;

// Effects.
const FX_CIRCLE: u32 = 1;
const FX_WAVE: u32 = 2;
const FX_CHASE: u32 = 3;
const FX_RAINBOW: u32 = 4;
const FX_STROBE: u32 = 5;
const FX_SWEEP: u32 = 6;
const FX_TILT: u32 = 7;
const FX_PULSE: u32 = 8;
const FX_TEMPO: u32 = 9;
const FX_BLUE: u32 = 10;

#[derive(Default)]
struct Rig {
    l: Lighting,
    lustr: Vec<FixtureId>,
    t1: Vec<FixtureId>,
    blinders: Vec<FixtureId>,
    auras: Vec<FixtureId>,
    vipers: Vec<FixtureId>,
    sharpies: Vec<FixtureId>,
    bars: Vec<FixtureId>,
    cells: Vec<FixtureId>,
    atomics: Vec<FixtureId>,
    /// Moving heads on the upstage truss.
    extras: Vec<FixtureId>,
    hazer: Vec<FixtureId>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(src), Some(dir)) = (args.next().map(PathBuf::from), args.next().map(PathBuf::from)) else {
        panic!("usage: real_rig <gdtf dir> <out dir>")
    };
    let profiles = dir.join("profiles");
    std::fs::create_dir_all(&profiles).expect("output folder");
    let mut types = Vec::new();
    let mut originals: HashMap<String, Vec<u8>> = HashMap::new();
    for (file, mode) in TYPES {
        let bytes = std::fs::read(src.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        let imp = opnq::light::gdtf::import(&bytes).unwrap_or_else(|e| panic!("{file}: {e:#}"));
        let t = imp.fixture;
        assert!(t.mode(mode).is_some(), "{} has no mode {mode}", t.name);
        let p = &t.physical;
        println!("{} {} · pan {}° tilt {}° beam {}–{}° moving {}", t.manufacturer, t.name, p.pan_range, p.tilt_range, p.beam_min, p.beam_max, p.moving);
        std::fs::write(profiles.join(file), &bytes).expect("copy gdtf");
        originals.insert(t.id.clone(), bytes);
        types.push((t, mode));
    }
    let rig = Rig::build(&types);
    let ws = Workspace { lighting: rig.l, ..Default::default() };
    let issues = ws.lighting.validate();
    assert!(issues.is_empty(), "patch problems: {issues:?}");
    let show = dir.join("real-rig.opnq");
    ws.save(&show).expect("save show");
    std::fs::write(dir.join("real-rig.mvr"), mvr_with_originals(&ws.lighting, &originals)).expect("write mvr");
    // What Patch → Export MVR… writes (OpnQ's own GDTF files: no models).
    std::fs::write(dir.join("real-rig-opnq.mvr"), opnq::light::mvr::export(&ws.lighting, "Real rig").expect("mvr")).expect("write mvr");
    let cues: usize = ws.lighting.sequences.iter().map(|s| s.cues.len()).sum();
    println!(
        "wrote {} — {} fixtures, {} groups, {} presets, {} effects, {cues} cues in {} sequences, {} buttons",
        show.display(),
        ws.lighting.fixtures.len(),
        ws.lighting.groups.len(),
        ws.lighting.presets.len(),
        ws.lighting.effects.len(),
        ws.lighting.sequences.len(),
        ws.lighting.buttons.len()
    );
    for f in ws.lighting.fixtures.iter() {
        println!("  {:>3} {:<22} U{} @{:<3} {}", f.id, f.name, f.universe, f.address, f.mode);
    }
    check(&ws.lighting);
}

/// OpnQ's MVR of the design with each type's GDTF replaced by the manufacturer's file (and the
/// modes named as that file names them).
fn mvr_with_originals(l: &Lighting, originals: &HashMap<String, Vec<u8>>) -> Vec<u8> {
    use opnq::light::{gdtf, mvr, zipread};
    let zip = mvr::export(l, "Real rig").expect("mvr");
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

/// Plays the Concert sequence and prints the DMX of one fixture of each type after some cues.
fn check(l: &Lighting) {
    use opnq::light::playback::{LightShow, Playback};
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let mut p = Playback::new(std::sync::Arc::new(LightShow::compile(l)), t0);
    let mut now = t0;
    for (i, c) in l.sequences[0].cues.iter().enumerate() {
        p.go(1, None, now).expect("go");
        now += Duration::from_secs(30);
        let frame = p.tick(now);
        if !matches!(i, 0 | 4 | 9 | 10) {
            continue;
        }
        println!("after cue {} \"{}\"", c.number, c.name);
        for id in [101, 111, 121, 201, 301, 401, 501, 601] {
            let f = l.fixture(id).expect("fixture");
            let n = l.mode_of(f).expect("mode").footprint() as usize;
            let u = &frame.get(f.universe).expect("universe").data;
            let a = f.address as usize - 1;
            println!("  {:<10} {:?}", f.name, &u[a..(a + n).min(a + 24)]);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Aiming: pan/tilt values that point a fixture at a spot, worked out from its position and
// rotation the way OpnQ aims at focus points.

type V3 = [f32; 3];

/// Pan and tilt aiming fixture `f` at `target`, as OpnQ aims cues at focus points (the type's
/// GDTF pan/tilt angles and parts).
fn aim(f: &Fixture, t: &FixtureType, target: V3) -> (f32, f32) {
    opnq::light::focus::aim(f, &t.physical, target).unwrap_or((0.5, 0.5))
}

// ---------------------------------------------------------------------------------------------

/// Colour as each kind of fixture makes it.
#[derive(Clone, Copy)]
struct Colour {
    rgb: [f32; 3],
    /// Sharpy colour wheel slot.
    wheel: &'static str,
}

const COLOURS: [(u32, &str, Colour); 8] = [
    (P_WARM, "Warm white", Colour { rgb: [1.0, 0.78, 0.5], wheel: "Open" }),
    (P_BLUE, "Deep blue", Colour { rgb: [0.0, 0.15, 1.0], wheel: "Blue" }),
    (P_RED, "Red", Colour { rgb: [1.0, 0.0, 0.0], wheel: "Red" }),
    (P_AMBER, "Amber", Colour { rgb: [1.0, 0.45, 0.0], wheel: "Orange" }),
    (P_MAGENTA, "Magenta", Colour { rgb: [1.0, 0.0, 0.8], wheel: "Magenta" }),
    (P_GREEN, "Green", Colour { rgb: [0.0, 1.0, 0.1], wheel: "Green" }),
    (P_WHITE, "Open white", Colour { rgb: [1.0, 1.0, 1.0], wheel: "Open" }),
    (P_CYAN, "Cyan", Colour { rgb: [0.0, 0.8, 1.0], wheel: "Cyan" }),
];

impl Rig {
    fn build(types: &[(FixtureType, &str)]) -> Rig {
        let mut r = Rig::default();
        r.patch(types);
        r.groups();
        r.presets();
        r.effects();
        r.sequences();
        r.buttons();
        r.l.outputs = vec![
            OutputConfig {
                name: "Art-Net broadcast (visualiser)".into(),
                enabled: true,
                protocol: OutputProtocol::ArtNet { target: String::new(), sync: false },
                routes: (1..=4).map(|u| UniverseRoute { universe: u, wire: u - 1 }).collect(),
            },
            OutputConfig {
                name: "sACN multicast (visualiser)".into(),
                enabled: false,
                protocol: OutputProtocol::Sacn { target: String::new(), priority: 100 },
                routes: (1..=4).map(|u| UniverseRoute { universe: u, wire: u }).collect(),
            },
        ];
        r.l.release_fade = 2.0;
        r
    }

    fn ty(&self, f: FixtureId) -> &FixtureType {
        let fx = self.l.fixture(f).expect("fixture");
        self.l.fixture_type(&fx.fixture_type).expect("type")
    }

    fn has(&self, unit: FixtureId, a: A) -> bool {
        self.l.unit_attributes(unit).contains(&a)
    }

    /// The value picking the named range of a unit's channel.
    fn range(&self, unit: FixtureId, a: A, name: &str) -> Option<f32> {
        self.l.unit_channels(unit)?.iter().find(|c| c.attribute == a)?.find_range(name).map(|r| r.value())
    }

    #[allow(clippy::too_many_arguments)]
    fn add(&mut self, id: FixtureId, name: String, t: &FixtureType, mode: &str, universe: u16, position: V3, rotation: V3) -> FixtureId {
        self.l.add_type(t);
        let m = t.mode(mode).expect("mode");
        let address = self.l.next_free_address(universe, m.footprint()).expect("universe full");
        self.l.fixtures.push(Fixture { id, name, fixture_type: t.id.clone(), mode: mode.into(), universe, address, position, rotation, ..Default::default() });
        id
    }

    fn patch(&mut self, types: &[(FixtureType, &str)]) {
        let bar = |id, name: &str, a: [f32; 2], b: [f32; 2], h| PlotShape { id, kind: ShapeKind::Bar, name: name.into(), a, b, height: h, label: [0.0, 0.0] };
        self.l.shapes = vec![
            PlotShape { id: 1, kind: ShapeKind::Rect, name: "Stage".into(), a: [-7.0, -4.5], b: [7.0, 4.5], height: 1.0, label: [0.0, 0.0] },
            PlotShape { id: 2, kind: ShapeKind::Rect, name: "Drum riser".into(), a: [-1.5, -3.5], b: [1.5, -1.5], height: 1.5, label: [0.0, 0.0] },
            bar(3, "FOH truss", [-6.0, 10.0], [6.0, 10.0], 8.0),
            bar(4, "LX1", [-6.0, 3.0], [6.0, 3.0], 8.0),
            bar(5, "LX2", [-5.0, 0.0], [5.0, 0.0], 8.5),
            bar(6, "LX3", [-6.0, -3.0], [6.0, -3.0], 8.0),
            bar(9, "Upstage truss", [-6.5, -5.0], [6.5, -5.0], 9.0),
        ];
        // Focus points on the stage top (the deck is 1 m high, the riser 1.5 m): the spots the position presets aim at.
        let point = |id, name: &str, position| FocusPoint { id, name: name.into(), position };
        self.l.points = vec![
            point(1, "Centre stage", [0.0, 1.0, 0.0]),
            point(2, "DSL", [-3.5, 1.0, 2.5]),
            point(3, "DSR", [3.5, 1.0, 2.5]),
            point(4, "Drum riser", [0.0, 1.5, -2.5]),
        ];
        self.l.plot.notes.push(PlotNote { id: 1, at: [-6.0, 13.0], text: "Real fixtures rig\n53 fixtures · U1–U4".into(), bold: true, ..Default::default() });
        self.l.plot.edit_mut(&PlotItem::Audience).text = Some("AUDIENCE".into());
        let shape = |s: &[PlotShape], name: &str| s.iter().find(|x| x.name == name).expect("shape").clone();
        // Lines the floor fixtures stand along (not drawn: a truss there would bury them in the stage), just above
        // the stage top: models are centred on their hang point, so a base reaches a few centimetres below it.
        let mut shapes = self.l.shapes.clone();
        shapes.push(bar(7, "Upstage floor", [-6.0, -4.2], [6.0, -4.2], 1.06));
        shapes.push(bar(8, "Downstage edge", [-5.0, 4.3], [5.0, 4.3], 1.02));
        let t = |i: usize| (&types[i].0, types[i].1);
        // Hung fixtures point down (rotation 0); floor fixtures stand up (180° about x).
        let hung = [0.0, 0.0, 0.0];
        let floor = [180.0, 0.0, 0.0];

        // Universe 1: front light, specials, blinders.
        let (lustr, mode) = t(0);
        for (i, p) in shape(&shapes, "FOH truss").hang(6).into_iter().enumerate() {
            // Tilted 50° towards the stage.
            let id = self.add(101 + i as u32, format!("FOH {}", i + 1), lustr, mode, 1, p, [50.0, (p[0] * -3.0).clamp(-15.0, 15.0), 0.0]);
            self.lustr.push(id);
        }
        let (t1, mode) = t(1);
        for (i, x) in [-7.0f32, 7.0].into_iter().enumerate() {
            let id = self.add(111 + i as u32, format!("Special {}", if i == 0 { "SL" } else { "SR" }), t1, mode, 1, [x, 8.0, 10.0], hung);
            self.t1.push(id);
        }
        let (blinder, mode) = t(2);
        for (i, x) in [-3.0f32, 3.0].into_iter().enumerate() {
            // On the upstage floor, facing the audience.
            let id = self.add(121 + i as u32, format!("COB {}", i + 1), blinder, mode, 1, [x, 1.2, -4.2], [-100.0, 0.0, 0.0]);
            self.blinders.push(id);
        }

        // Universe 2: washes and spots on LX1 and LX3, alternating.
        let (aura, aura_mode) = t(3);
        let (viper, viper_mode) = t(4);
        for (k, name) in ["LX1", "LX3"].into_iter().enumerate() {
            for (i, p) in shape(&shapes, name).hang(8).into_iter().enumerate() {
                let n = (k * 4 + i / 2) as u32;
                if i % 2 == 0 {
                    let id = self.add(201 + n, format!("Quantum {}", n + 1), aura, aura_mode, 2, p, hung);
                    self.auras.push(id);
                } else {
                    let id = self.add(301 + n, format!("Viper {}", n + 1), viper, viper_mode, 2, p, hung);
                    self.vipers.push(id);
                }
            }
        }

        // Universe 3: beams and pixel bars on the floor, strobes on LX2.
        let (sharpy, mode) = t(5);
        for (i, p) in shape(&shapes, "Upstage floor").hang(6).into_iter().enumerate() {
            let id = self.add(401 + i as u32, format!("Sharpy {}", i + 1), sharpy, mode, 3, p, floor);
            self.sharpies.push(id);
        }
        let (x4, mode) = t(6);
        for (i, p) in shape(&shapes, "Downstage edge").hang(4).into_iter().enumerate() {
            let id = self.add(501 + i as u32, format!("X4 bar {}", i + 1), x4, mode, 3, p, floor);
            self.bars.push(id);
            let cells = self.l.cell_count(self.l.fixture(id).expect("bar"));
            self.cells.extend((1..=cells).map(|c| cell_id(id, c)));
        }
        let (atomic, mode) = t(7);
        for (i, p) in shape(&shapes, "LX2").hang(4).into_iter().enumerate() {
            let id = self.add(601 + i as u32, format!("Platinum {}", i + 1), atomic, mode, 3, p, hung);
            self.atomics.push(id);
        }
        assert_eq!(self.l.fixtures.len(), 40);

        // Universe 4: the other types in pairs on the upstage truss, the hazer on the floor SR.
        let spots = shape(&shapes, "Upstage truss").hang(12);
        for (k, short) in ["Spiider", "MMX Blade", "Ghibli", "P12", "Viva", "AE-610"].into_iter().enumerate() {
            let (ty, mode) = t(8 + k);
            for side in 0..2 {
                let n = k * 2 + side;
                let id = self.add(701 + n as u32, format!("{short} {}", side + 1), ty, mode, 4, spots[if side == 0 { k } else { 11 - k }], hung);
                self.extras.push(id);
            }
        }
        let (fan, mode) = t(14);
        let id = self.add(801, "Hazer".into(), fan, mode, 4, [6.5, 0.13, -3.5], floor);
        self.hazer.push(id);
    }

    fn groups(&mut self) {
        let g = |id, name: &str, fixtures: &[FixtureId]| Group { id, name: name.into(), fixtures: fixtures.to_vec() };
        let all: Vec<FixtureId> = self.l.fixtures.iter().map(|f| f.id).collect();
        let movers: Vec<FixtureId> = self.auras.iter().chain(&self.vipers).chain(&self.sharpies).chain(&self.t1).copied().collect();
        let side = |l: &Lighting, ids: &[FixtureId], left: bool| ids.iter().copied().filter(|f| (l.fixture(*f).expect("fixture").position[0] < 0.0) == left).collect::<Vec<_>>();
        let odd = |ids: &[FixtureId]| ids.iter().copied().step_by(2).collect::<Vec<_>>();
        self.l.groups = vec![
            g(1, "All", &all),
            g(2, "FOH", &self.lustr),
            g(3, "Specials", &self.t1),
            g(4, "Blinders", &self.blinders),
            g(5, "Quantums", &self.auras),
            g(6, "Vipers", &self.vipers),
            g(7, "Sharpies", &self.sharpies),
            g(8, "X4 bars", &self.bars),
            g(9, "X4 cells", &self.cells),
            g(10, "Platinum beams", &self.atomics),
            g(11, "Movers", &movers),
            g(12, "Movers SL", &side(&self.l, &movers, true)),
            g(13, "Movers SR", &side(&self.l, &movers, false)),
            g(14, "Quantums (odd)", &odd(&self.auras)),
            g(15, "Sharpies (odd)", &odd(&self.sharpies)),
            g(16, "X4 cells (odd)", &odd(&self.cells)),
            g(17, "Upstage truss", &self.extras),
            g(18, "Hazer", &self.hazer),
        ];
    }

    /// Colour values of one unit for a colour.
    fn colour_of(&self, unit: FixtureId, c: Colour) -> Vec<(A, f32)> {
        let [r, g, b] = c.rgb;
        let mut v = Vec::new();
        let white = r.min(g).min(b);
        let mut put = |a: A, x: f32| {
            if self.has(unit, a) {
                v.push((a, x.clamp(0.0, 1.0)));
            }
        };
        if self.has(unit, A::Lime) {
            // ETC Lustr: seven emitters.
            put(A::Red, r);
            put(A::Lime, (g * 0.6).min(r * 0.8));
            put(A::Amber, (r * 0.7).min(g * 1.5));
            put(A::Green, g);
            put(A::Cyan, g.min(b));
            put(A::Blue, b);
            put(A::White, white);
        } else if self.has(unit, A::Red) {
            put(A::Red, r - white);
            put(A::Green, g - white);
            put(A::Blue, b - white);
            put(A::White, white);
        } else if self.has(unit, A::Cyan) {
            put(A::Cyan, 1.0 - r);
            put(A::Magenta, 1.0 - g);
            put(A::Yellow, 1.0 - b);
            put(A::Cto, 0.0);
            if let Some(open) = self.range(unit, A::ColorWheel, "Open") {
                put(A::ColorWheel, open);
            }
        } else if let Some(w) = self.range(unit, A::ColorWheel, c.wheel).or_else(|| self.range(unit, A::ColorWheel, "Open")) {
            put(A::ColorWheel, w);
        }
        v
    }

    fn presets(&mut self) {
        let mk = |id, name: &str, feature, v: Vec<(FixtureId, Vec<(A, f32)>)>| Preset {
            id,
            name: name.into(),
            feature,
            values: v
                .into_iter()
                .fold(BTreeMap::<FixtureId, BTreeMap<A, f32>>::new(), |mut m, (f, vals)| {
                    m.entry(f).or_default().extend(vals);
                    m
                })
                .into(),
        };
        let colour_units: Vec<FixtureId> = self.lustr.iter().chain(&self.t1).chain(&self.auras).chain(&self.vipers).chain(&self.sharpies).chain(&self.cells).chain(&self.extras).copied().collect();
        let mut presets = Vec::new();
        for (id, name, c) in COLOURS {
            let v = colour_units.iter().map(|u| (*u, self.colour_of(*u, c))).filter(|(_, v)| !v.is_empty()).collect();
            presets.push(mk(id, name, Some(Feature::Colour), v));
        }
        // Positions: every moving head aimed by its own position (no "first fixture of the type").
        let movers: Vec<FixtureId> = self.auras.iter().chain(&self.vipers).chain(&self.sharpies).chain(&self.t1).chain(&self.extras).copied().collect();
        let pos = |rig: &Rig, target: &dyn Fn(&Fixture, usize) -> V3| -> Vec<(FixtureId, Vec<(A, f32)>)> {
            movers
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let f = rig.l.fixture(*id).expect("fixture");
                    let (p, t) = aim(f, rig.ty(*id), target(f, i));
                    (*id, vec![(A::Pan, p), (A::Tilt, t)])
                })
                .collect()
        };
        let mut fan_x: Vec<(FixtureId, f32)> = Vec::new();
        for list in [&self.auras, &self.vipers, &self.sharpies, &self.extras] {
            let n = list.len() as f32;
            fan_x.extend(list.iter().enumerate().map(|(i, f)| (*f, -9.0 + 18.0 * i as f32 / (n - 1.0))));
        }
        let fan_of = |f: &Fixture| fan_x.iter().find(|(id, _)| *id == f.id).map_or(f.position[0] * 1.5, |(_, x)| *x);
        presets.push(mk(P_CENTRE, "Centre stage", Some(Feature::Position), pos(self, &|_, _| [0.0, 1.0, 0.0])));
        presets.push(mk(P_DSL, "DSL", Some(Feature::Position), pos(self, &|_, _| [-3.5, 1.0, 2.5])));
        presets.push(mk(P_DSR, "DSR", Some(Feature::Position), pos(self, &|_, _| [3.5, 1.0, 2.5])));
        presets.push(mk(P_USC, "Drum riser", Some(Feature::Position), pos(self, &|_, _| [0.0, 1.5, -2.5])));
        presets.push(mk(P_AUDIENCE, "Audience", Some(Feature::Position), pos(self, &|f, _| [f.position[0] * 1.6, 1.5, 14.0])));
        presets.push(mk(P_CROSS, "Cross", Some(Feature::Position), pos(self, &|f, _| [-f.position[0] * 0.8, 1.0, 0.5])));
        presets.push(mk(P_FAN, "Fan", Some(Feature::Position), pos(self, &|f, _| [fan_of(f), 1.0, 1.0])));
        presets.push(mk(P_SKY, "Sky", Some(Feature::Position), pos(self, &|f, _| if f.position[1] < 3.0 { [fan_of(f) * 0.8, 20.0, -8.0] } else { [f.position[0] * 1.3, 1.0, 4.0] })));
        // Beam presets: gobo, prism, zoom.
        let beam = |rig: &Rig, gobo: Option<&str>, prism: bool, zoom: f32| -> Vec<(FixtureId, Vec<(A, f32)>)> {
            let mut v = Vec::new();
            for id in rig.vipers.iter().chain(&rig.sharpies).chain(&rig.t1).chain(&rig.auras).chain(&rig.extras) {
                let mut vals = Vec::new();
                if rig.has(*id, A::Gobo) {
                    let g = gobo.and_then(|g| rig.range(*id, A::Gobo, g)).or_else(|| rig.range(*id, A::Gobo, "Open")).unwrap_or(0.0);
                    vals.push((A::Gobo, g));
                }
                if rig.has(*id, A::Prism) {
                    let p = if prism { rig.l.unit_channels(*id).and_then(|c| c.iter().find(|c| c.attribute == A::Prism)?.ranges.get(1).map(|r| r.value())).unwrap_or(0.5) } else { 0.0 };
                    vals.push((A::Prism, p));
                }
                if rig.has(*id, A::Zoom) {
                    vals.push((A::Zoom, zoom));
                }
                if rig.has(*id, A::Iris) {
                    vals.push((A::Iris, if zoom < 0.2 { 0.6 } else { 0.0 }));
                }
                v.push((*id, vals));
            }
            v
        };
        presets.push(mk(P_GOBO, "Gobo 3", Some(Feature::Beam), beam(self, Some("3"), false, 0.4)));
        presets.push(mk(P_PRISM, "Prism", Some(Feature::Beam), beam(self, Some("2"), true, 0.5)));
        presets.push(mk(P_TIGHT, "Tight", Some(Feature::Beam), beam(self, None, false, 0.0)));
        presets.push(mk(P_WIDE, "Wide", Some(Feature::Beam), beam(self, None, false, 1.0)));
        // An all-features preset: everything up in open white at centre.
        let mut all = Vec::new();
        for u in &colour_units {
            all.push((*u, self.colour_of(*u, COLOURS[6].2)));
        }
        for (id, v) in pos(self, &|_, _| [0.0, 1.0, 0.0]) {
            all.push((id, v));
        }
        for f in self.l.fixtures.iter() {
            all.push((f.id, vec![(A::Dimmer, 1.0)]));
        }
        presets.push(mk(P_ALL_UP, "All up (all features)", None, all));
        self.l.presets = presets;
    }

    fn effects(&mut self) {
        let e = |id, name: &str, attribute, shape| Effect { id, name: name.into(), attribute, shape, ..Default::default() };
        let mut circle = e(FX_CIRCLE, "Circle", A::Pan, Shape::Circle);
        circle.speed = 0.25;
        circle.size = 0.08;
        circle.relative = true;
        circle.spread = 90.0;
        let mut wave = e(FX_WAVE, "Dimmer wave", A::Dimmer, Shape::Sine);
        wave.speed = 0.4;
        wave.size = 0.8;
        wave.spread = 360.0;
        let mut chase = e(FX_CHASE, "Pixel chase", A::White, Shape::Square);
        chase.speed = 1.0;
        chase.duty = 0.2;
        chase.spread = 360.0;
        chase.wings = 2;
        let mut strobe = e(FX_STROBE, "Random strobe", A::Dimmer, Shape::Random);
        strobe.speed = 8.0;
        strobe.size = 1.0;
        strobe.spread = 0.0;
        let mut sweep = e(FX_SWEEP, "Pan sweep (wings)", A::Pan, Shape::Sine);
        sweep.speed = 0.2;
        sweep.size = 0.12;
        sweep.relative = true;
        sweep.spread = 180.0;
        sweep.wings = 2;
        let mut tilt = e(FX_TILT, "Tilt saw", A::Tilt, Shape::SawUp);
        tilt.speed = 0.3;
        tilt.size = 0.3;
        tilt.relative = true;
        tilt.spread = 90.0;
        let mut pulse = e(FX_PULSE, "Pulse with the music", A::Dimmer, Shape::Sine);
        pulse.speed = 1.0;
        pulse.size = 0.6;
        pulse.relative = true;
        pulse.audio = Some(AudioReact { gain: 3.0, floor: 0.2 });
        let mut tempo = e(FX_TEMPO, "Tempo chase (tap)", A::Dimmer, Shape::Square);
        tempo.sync = true;
        tempo.speed = 0.5;
        tempo.duty = 0.25;
        tempo.groups = 2;
        tempo.wings = 2;
        let mut blue = e(FX_BLUE, "Blue breathing", A::Blue, Shape::Triangle);
        blue.speed = 0.1;
        blue.size = 0.5;
        blue.base = 0.3;
        blue.spread = 120.0;
        self.l.effects = vec![circle, wave, chase, rainbow(), strobe, sweep, tilt, pulse, tempo, blue];
    }

    fn set(&self, c: &mut LightCue, unit: FixtureId, a: A, v: f32) {
        if self.has(unit, a) {
            c.values.entry(unit).or_default().insert(a, Value::Level(v.clamp(0.0, 1.0)));
        }
    }

    /// A preset's values for every attribute it has for the unit.
    fn preset(&self, c: &mut LightCue, unit: FixtureId, preset: u32) {
        for a in self.l.preset_attributes(preset, unit) {
            c.values.entry(unit).or_default().insert(a, Value::Preset { preset });
        }
    }

    fn dim(&self, c: &mut LightCue, units: &[FixtureId], v: f32) {
        for u in units {
            self.set(c, *u, A::Dimmer, v);
        }
    }

    fn presets_on(&self, c: &mut LightCue, units: &[FixtureId], presets: &[u32]) {
        for u in units {
            for p in presets {
                self.preset(c, *u, *p);
            }
        }
    }

    /// Shutters open, master dimmers of pixel bars up, strobe rates parked: the base every
    /// look builds on (tracked from cue 1).
    fn base(&self, c: &mut LightCue) {
        for f in self.l.fixtures.iter() {
            if let Some(open) = self.range(f.id, A::Shutter, "Open") {
                self.set(c, f.id, A::Shutter, open);
            }
        }
        // Atomic: flash duration half, rate "single flash", the Effects channel (a second
        // shutter, so a cell of its own) at plain strobe.
        for a in &self.atomics {
            self.set(c, *a, A::Shutter, 0.5);
            self.set(c, *a, A::Effect, 0.0);
            self.set(c, cell_id(*a, 1), A::Shutter, 0.0);
        }
        for b in &self.bars {
            self.set(c, *b, A::Tilt, 0.5);
            self.set(c, *b, A::Zoom, 0.5);
        }
    }

    fn sequences(&mut self) {
        let lc = |num: &str, name: &str, fade: f64| LightCue { number: num.into(), name: name.into(), fade, ..Default::default() };
        let all: Vec<FixtureId> = self.l.fixtures.iter().map(|f| f.id).collect();
        let movers: Vec<FixtureId> = self.auras.iter().chain(&self.vipers).chain(&self.sharpies).copied().collect();
        let (lustr, t1, blinders, auras, vipers, sharpies, bars, cells, atomics) =
            (self.lustr.clone(), self.t1.clone(), self.blinders.clone(), self.auras.clone(), self.vipers.clone(), self.sharpies.clone(), self.bars.clone(), self.cells.clone(), self.atomics.clone());
        let run = |effect, fixtures: &[FixtureId]| EffectRun { effect, fixtures: fixtures.to_vec(), stop: false };
        let stop = |effect, fixtures: &[FixtureId]| EffectRun { effect, fixtures: fixtures.to_vec(), stop: true };

        let mut main = Sequence { id: 1, name: "Concert".into(), move_in_black: true, mib_delay: 1.0, mib_fade: 2.0, ..Default::default() };
        let mut q = Vec::new();

        let mut c = lc("1", "Walk-in: warm front, blue washes", 5.0);
        self.base(&mut c);
        self.dim(&mut c, &all, 0.0);
        self.dim(&mut c, &lustr, 0.5);
        self.presets_on(&mut c, &lustr, &[P_WARM]);
        self.dim(&mut c, &auras, 0.35);
        self.presets_on(&mut c, &auras, &[P_BLUE, P_WIDE, P_CENTRE]);
        self.presets_on(&mut c, &vipers, &[P_BLUE, P_GOBO, P_USC]);
        self.presets_on(&mut c, &sharpies, &[P_BLUE, P_SKY]);
        self.presets_on(&mut c, &t1, &[P_WARM, P_TIGHT, P_CENTRE]);
        self.presets_on(&mut c, &cells, &[P_BLUE]);
        self.dim(&mut c, &bars, 0.4);
        c.effects.push(run(FX_BLUE, &auras));
        q.push(c);

        let mut c = lc("2", "Blackout (band walks on)", 3.0);
        self.dim(&mut c, &all, 0.0);
        c.effects.push(stop(FX_BLUE, &auras));
        q.push(c);

        let mut c = lc("3", "Intro: sharpy fan into the sky", 0.0);
        self.dim(&mut c, &sharpies, 1.0);
        self.presets_on(&mut c, &sharpies, &[P_WHITE, P_FAN]);
        c.feature_times.insert(Feature::Position, FeatureTime { fade: 4.0, delay: 0.5 });
        q.push(c);

        let mut c = lc("4", "Song 1 verse: amber washes, front up", 3.0);
        self.dim(&mut c, &lustr, 0.7);
        self.dim(&mut c, &auras, 0.8);
        self.presets_on(&mut c, &auras, &[P_AMBER, P_CENTRE, P_WIDE]);
        self.dim(&mut c, &sharpies, 0.6);
        self.presets_on(&mut c, &sharpies, &[P_AMBER, P_SKY]);
        c.effects.push(run(FX_SWEEP, &sharpies));
        c.down_fade = Some(5.0);
        q.push(c);

        let mut c = lc("5", "Song 1 chorus: spots with gobos, pixel rainbow", 1.5);
        self.dim(&mut c, &vipers, 1.0);
        self.presets_on(&mut c, &vipers, &[P_MAGENTA, P_GOBO, P_CROSS]);
        self.dim(&mut c, &bars, 1.0);
        c.effects.push(run(FX_RAINBOW, &cells));
        c.effects.push(run(FX_CIRCLE, &vipers));
        c.feature_times.insert(Feature::Colour, FeatureTime { fade: 0.5, delay: 0.0 });
        q.push(c);

        let mut c = lc("6", "Song 1 bridge: specials only (cue only)", 2.0);
        c.cue_only = true;
        self.dim(&mut c, &movers, 0.0);
        self.dim(&mut c, &bars, 0.0);
        self.dim(&mut c, &t1, 1.0);
        self.presets_on(&mut c, &t1, &[P_DSL]);
        self.preset(&mut c, t1[1], P_DSR);
        q.push(c);

        let mut c = lc("7", "Song 1 last chorus: back to the chorus", 1.0);
        self.dim(&mut c, &t1, 0.0);
        q.push(c);

        let mut c = lc("8", "Song 1 end: hit and blackout", 0.0);
        c.down_fade = Some(0.0);
        c.effects.push(stop(FX_RAINBOW, &cells));
        c.effects.push(stop(FX_CIRCLE, &vipers));
        c.effects.push(stop(FX_SWEEP, &sharpies));
        self.dim(&mut c, &all, 0.0);
        q.push(c);

        let mut c = lc("9", "Song 2 (block): red, tempo chase on the spots", 0.5);
        self.base(&mut c);
        self.dim(&mut c, &vipers, 1.0);
        self.presets_on(&mut c, &vipers, &[P_RED, P_PRISM, P_FAN]);
        self.dim(&mut c, &auras, 0.9);
        self.presets_on(&mut c, &auras, &[P_RED, P_TIGHT, P_AUDIENCE]);
        self.dim(&mut c, &lustr, 0.4);
        self.presets_on(&mut c, &lustr, &[P_RED]);
        self.dim(&mut c, &bars, 1.0);
        self.presets_on(&mut c, &cells, &[P_RED]);
        c.effects.push(run(FX_TEMPO, &vipers));
        c.effects.push(run(FX_CHASE, &cells));
        q.push(c);

        let mut c = lc("10", "Song 2 build: strobes and tilt saw", 4.0);
        self.dim(&mut c, &atomics, 0.8);
        for a in &atomics {
            self.set(&mut c, *a, A::Effect, 0.6);
        }
        c.effects.push(run(FX_STROBE, &atomics));
        self.dim(&mut c, &sharpies, 1.0);
        self.presets_on(&mut c, &sharpies, &[P_RED, P_SKY]);
        c.effects.push(run(FX_TILT, &sharpies));
        q.push(c);

        let mut c = lc("11", "Song 2 drop: white, blinders, everything", 0.0);
        c.effects.push(stop(FX_STROBE, &atomics));
        c.effects.push(stop(FX_TILT, &sharpies));
        c.effects.push(stop(FX_TEMPO, &vipers));
        self.dim(&mut c, &atomics, 0.0);
        self.dim(&mut c, &blinders, 1.0);
        for b in &blinders {
            for cell in 1..=self.l.cell_count(self.l.fixture(*b).expect("blinder")) {
                self.set(&mut c, cell_id(*b, cell), A::Dimmer, 1.0);
                self.set(&mut c, cell_id(*b, cell), A::White, 1.0);
            }
        }
        for u in auras.iter().chain(&vipers).chain(&sharpies).chain(&lustr).chain(&cells) {
            self.preset(&mut c, *u, P_WHITE);
        }
        self.presets_on(&mut c, &movers, &[P_AUDIENCE]);
        c.down_fade = Some(2.0);
        q.push(c);

        let mut c = lc("11.5", "Blinders out", 2.0);
        self.dim(&mut c, &blinders, 0.0);
        self.presets_on(&mut c, &movers, &[P_CENTRE]);
        q.push(c);

        let mut c = lc("12", "Ballad: lavender washes, cyan spots, music pulse", 6.0);
        c.delay = 1.0;
        self.dim(&mut c, &sharpies, 0.0);
        self.dim(&mut c, &bars, 0.3);
        self.presets_on(&mut c, &cells, &[P_MAGENTA]);
        self.dim(&mut c, &auras, 0.6);
        self.presets_on(&mut c, &auras, &[P_MAGENTA, P_WIDE]);
        self.dim(&mut c, &vipers, 0.7);
        self.presets_on(&mut c, &vipers, &[P_CYAN, P_GOBO, P_DSL]);
        self.dim(&mut c, &lustr, 0.6);
        self.presets_on(&mut c, &lustr, &[P_WARM]);
        c.effects.push(stop(FX_CHASE, &cells));
        c.effects.push(run(FX_PULSE, &lustr));
        c.effects.push(run(FX_WAVE, &auras));
        c.feature_times.insert(Feature::Position, FeatureTime { fade: 8.0, delay: 2.0 });
        c.feature_times.insert(Feature::Colour, FeatureTime { fade: 10.0, delay: 0.0 });
        q.push(c);

        let mut c = lc("13", "Encore: green and all effects", 2.0);
        self.dim(&mut c, &movers, 1.0);
        self.dim(&mut c, &bars, 1.0);
        for u in movers.iter().chain(&cells) {
            self.preset(&mut c, *u, P_GREEN);
        }
        self.presets_on(&mut c, &movers, &[P_FAN, P_PRISM]);
        c.effects.push(stop(FX_PULSE, &lustr));
        c.effects.push(stop(FX_WAVE, &auras));
        c.effects.push(run(FX_CIRCLE, &vipers));
        c.effects.push(run(FX_SWEEP, &sharpies));
        c.effects.push(run(FX_RAINBOW, &cells));
        q.push(c);

        let mut c = lc("14", "House look: everything out but the front", 8.0);
        for f in &self.l.effects {
            c.effects.push(stop(f.id, &all));
        }
        self.dim(&mut c, &all, 0.0);
        self.dim(&mut c, &lustr, 0.5);
        self.presets_on(&mut c, &lustr, &[P_WARM]);
        q.push(c);
        main.cues = q.into();
        let song2 = main.cues.iter().position(|c| c.number == "9").expect("song 2");
        main.block(song2);

        // Blinder and strobe hits for the GO button of an executor: cue-only flashes.
        let mut hits = Sequence { id: 2, name: "Hits".into(), ..Default::default() };
        let mut v = Vec::new();
        for (i, (name, units)) in [("Blinders", blinders.clone()), ("Platinum beams", atomics.clone()), ("Blinders + Platinums", blinders.iter().chain(&atomics).copied().collect())].into_iter().enumerate() {
            let mut c = lc(&(i + 1).to_string(), name, 0.0);
            c.down_fade = Some(1.5);
            c.cue_only = true;
            self.dim(&mut c, &units, 1.0);
            for a in units.iter().filter(|u| atomics.contains(u)) {
                self.set(&mut c, *a, A::Effect, 0.8);
            }
            for b in &units {
                for cell in 1..=self.l.cell_count(self.l.fixture(*b).expect("fixture")) {
                    self.set(&mut c, cell_id(*b, cell), A::Dimmer, 1.0);
                    self.set(&mut c, cell_id(*b, cell), A::White, 1.0);
                }
                if let Some(open) = self.range(*b, A::Shutter, "Open") {
                    self.set(&mut c, *b, A::Shutter, open);
                }
            }
            v.push(c);
        }
        hits.cues = v.into();

        // One cue per colour on the washes and pixel bars (a colour busk stack).
        let mut busk = Sequence { id: 3, name: "Colour busk".into(), ..Default::default() };
        let mut v = Vec::new();
        for (i, (preset, name, _)) in COLOURS.iter().enumerate() {
            let mut c = lc(&(i + 1).to_string(), name, 1.0);
            self.base(&mut c);
            self.dim(&mut c, &auras, 1.0);
            self.dim(&mut c, &bars, 1.0);
            for u in auras.iter().chain(&cells) {
                self.preset(&mut c, *u, *preset);
            }
            v.push(c);
        }
        busk.cues = v.into();

        self.l.sequences = vec![main, hits, busk];
    }

    /// Playback buttons on three pages.
    fn buttons(&mut self) {
        let values = |rig: &Rig, units: &[FixtureId], v: &[(A, f32)]| {
            let mut look = Look::new();
            for u in units {
                for (a, x) in v {
                    if rig.has(*u, *a) {
                        look.entry(*u).or_default().insert(*a, (*x).into());
                    }
                }
                if let Some(open) = rig.range(*u, A::Shutter, "Open") {
                    look.entry(*u).or_default().insert(A::Shutter, open.into());
                }
            }
            look.into()
        };
        let blinder_units: Vec<FixtureId> = self.blinders.iter().flat_map(|b| std::iter::once(*b).chain((1..=2).map(move |c| cell_id(*b, c)))).collect();
        self.l.buttons = vec![
            LightButton { id: 1, name: "Blinders".into(), color: [0xf0, 0xc0, 0x60], values: values(self, &blinder_units, &[(A::Dimmer, 1.0), (A::White, 1.0)]), flash: true, ..Default::default() },
            LightButton {
                id: 2,
                name: "Strobe".into(),
                color: [0xf0, 0xf0, 0xf0],
                values: values(self, &self.atomics, &[(A::Dimmer, 1.0), (A::Effect, 0.8)]),
                effects: vec![EffectRun { effect: FX_STROBE, fixtures: self.atomics.clone(), stop: false }],
                flash: true,
                ..Default::default()
            },
            LightButton { id: 3, name: "Specials DS".into(), color: [0xf0, 0xa0, 0x50], preset: Some(P_DSL), fixtures: self.t1.clone(), fade: 1.0, ..Default::default() },
            LightButton { id: 4, name: "Sharpy fan".into(), color: [0x60, 0xa0, 0xf0], preset: Some(P_FAN), fixtures: self.sharpies.clone(), fade: 0.5, page: 1, ..Default::default() },
            LightButton {
                id: 5,
                name: "Rainbow bars".into(),
                color: [0xb0, 0x60, 0xf0],
                values: values(self, &self.bars, &[(A::Dimmer, 1.0)]),
                effects: vec![EffectRun { effect: FX_RAINBOW, fixtures: self.cells.clone(), stop: false }],
                fade: 1.0,
                page: 1,
                ..Default::default()
            },
            LightButton {
                id: 6,
                name: "Viper circles".into(),
                color: [0x50, 0xd0, 0x90],
                effects: vec![EffectRun { effect: FX_CIRCLE, fixtures: self.vipers.clone(), stop: false }],
                page: 1,
                ..Default::default()
            },
            LightButton { id: 7, name: "Washes red".into(), color: [0xe0, 0x30, 0x30], preset: Some(P_RED), fixtures: self.auras.clone(), fade: 0.5, page: 2, ..Default::default() },
            LightButton { id: 8, name: "Washes blue".into(), color: [0x30, 0x50, 0xe0], preset: Some(P_BLUE), fixtures: self.auras.clone(), fade: 0.5, page: 2, ..Default::default() },
            LightButton { id: 9, name: "Washes amber".into(), color: [0xf0, 0x90, 0x20], preset: Some(P_AMBER), fixtures: self.auras.clone(), fade: 0.5, page: 2, ..Default::default() },
        ];
        self.l.button_pages = vec!["Hits".into(), "Effects".into(), "Colours".into()];
    }
}

/// A rainbow along the pixel bars, as the Effects page would work it out from the formula.
fn rainbow() -> Effect {
    const FORMULA: &str = r#"# A rainbow running along the X4 bars: each cell a step further round the colour wheel.
param sat 1 0..1
effect "Rainbow" steps 64 width 10 smooth
red, green, blue = hsv(t + row / width, sat)
"#;
    let out = opnq::light::formula::evaluate(FORMULA, &[]).expect("rainbow formula");
    Effect {
        id: FX_RAINBOW,
        name: "Rainbow (formula)".into(),
        attribute: A::Red,
        shape: Shape::Custom,
        speed: 0.25,
        table: Some(std::sync::Arc::new(out.effects[0].1.clone())),
        params: out.params,
        formula: FORMULA.into(),
        ..Default::default()
    }
}
