//! Fuzz tests: damaged and hostile input from every place OpenQ reads it.
//!
//! Each test throws random and deliberately malicious data at one way in and checks that OpenQ
//! answers with an error (or a sane result) instead of crashing, hanging or running away with
//! memory:
//!
//! * `fuzz_show_files`: show files with valid JSON but hostile values (huge, negative and
//!   non-finite numbers, duplicate ids, cues aimed at the wrong things, paths to devices, other
//!   users' files and network shares, deep nesting), and broken bytes. What loads is saved again,
//!   vetted, then played: GO, stop, pause, playhead moves, light cues, on a virtual sound card.
//! * `fuzz_media_files`: damaged audio, video, image and SVG files through every reader (probe,
//!   waveform, decoder, posters, stills, thumbnails), plus special files (FIFOs, `/dev/zero`,
//!   directories, huge sparse files) and SVGs that reach for other files or blow up in size.
//! * `fuzz_lighting_files`: effect files, fixture files, zips, command lines, MVR and USITT ASCII files.
//! * `fuzz_config_files`: garbage in every file of the config folder (remote devices, language,
//!   MIDI port, OFL catalogue, autosaves, libraries) and file system errors when saving.
//! * `fuzz_remote_messages`: hostile handshakes, every message an Edit-role replica can send to the
//!   main device (over the real encrypted link), and every message a main device can send to a
//!   replica.
//! * `fuzz_network_packets`: Art-Net, sACN, OSC, MIDI and MTC packets.
//! * `fuzz_devices`: sound cards, inputs, MIDI ports, serial ports and network outputs that don't
//!   exist or aren't what they claim to be.
//!
//! A panic anywhere counts, also on a worker thread whose panic OpenQ catches and turns into an
//! error (`recovery::catch`): the panic hook records them all. The one exception is a panic inside
//! the SVG rasteriser (a bug in that library) that OpenQ caught: it is listed, not failed. A call that doesn't return in time
//! counts as a hang. The failing input is saved under `target/tmp/fuzz-failures/` and the message
//! says how to run it again; after a crash that ends the process (a stack overflow), the input
//! being tried is there as `<test>-last.<ext>`.
//!
//! These are `#[ignore]`d (slow); run them with `tools/fuzz.sh`, which amounts to:
//!
//! ```text
//! cargo test --release --test fuzz -- --ignored --test-threads=1
//! ```
//!
//! `OPENQ_FUZZ_SECS` sets the time per test (default 20), `OPENQ_FUZZ_SEED` the random seed (each
//! run prints its own), `OPENQ_FUZZ_REPLAY=<file>` runs one saved show or media file only.
//! `fuzz_show_files` also draws cues and lighting from the demo show (`tools/demo_show.sh`) when
//! it's there, which brings in every cue feature and real media.

use openq::control::{Command, CueAction, LightControl, apply};
use openq::light::model::{Fixture, LightCue, Sequence};
use openq::light::{Lighting, library};
use openq::media::Prober;
use openq::media::audio::AudioOutput;
use openq::model::{AudioCue, Cue, CueId, CueKind, KindTag, Workspace};
use openq::show::{SharedShow, Show};
use parking_lot::Mutex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ harness

static PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Panics inside the SVG rasteriser that OpenQ caught (`still::render_svg`): bugs in that library
/// we can't fix here. They don't fail a test as long as the call returned; they are listed.
static CONTAINED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn is_rasteriser_panic(p: &str) -> bool {
    ["/tiny-skia-", "/resvg-"].iter().any(|c| p.contains(c))
}

/// Record every panic, on any thread, even one that is caught afterwards.
fn watch_panics() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Warnings show why something was refused or a connection closed (RUST_LOG for more).
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).is_test(true).try_init();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current().name().unwrap_or("?").to_string();
            PANICS.lock().push(format!("thread {thread}: {info}"));
            previous(info);
        }));
        // No stage windows: video cues end with "video output unavailable", as on a machine
        // without a display.
        // SAFETY: set before any thread of this test reads the environment.
        unsafe { std::env::set_var("OPENQ_VIDEO_EXE", "/nonexistent/openq-fuzz-no-video") };
    });
}

fn budget() -> Duration {
    Duration::from_secs_f64(std::env::var("OPENQ_FUZZ_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(20.0))
}

fn base_seed() -> u64 {
    std::env::var("OPENQ_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        t ^ (std::process::id() as u64) << 32
    })
}

fn replay() -> Option<PathBuf> {
    std::env::var_os("OPENQ_FUZZ_REPLAY").map(PathBuf::from)
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("openq-fuzz-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One fuzz run: its seed, clock and the bookkeeping for reports.
struct Fuzz {
    name: &'static str,
    seed: u64,
    t0: Instant,
    rng: fastrand::Rng,
    iteration: u64,
}

impl Fuzz {
    fn new(name: &'static str) -> Self {
        watch_panics();
        PANICS.lock().clear();
        let seed = base_seed();
        eprintln!("{name}: OPENQ_FUZZ_SEED={seed}, {:.0} s", budget().as_secs_f64());
        Self { name, seed, t0: Instant::now(), rng: fastrand::Rng::with_seed(seed), iteration: 0 }
    }

    /// Another iteration while there is time left.
    fn next(&mut self) -> bool {
        self.check_panics(&[]);
        self.iteration += 1;
        self.t0.elapsed() < budget()
    }

    /// Fail the test: save `input` and say how to run it again.
    fn fail(&self, input: &[u8], ext: &str, why: &str) -> ! {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fuzz-failures");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join(format!("{}-{}-{}.{ext}", self.name, self.seed, self.iteration));
        let saved = std::fs::write(&file, input).map(|_| format!("input saved to {}", file.display())).unwrap_or_else(|e| format!("input not saved: {e}"));
        panic!(
            "{}: iteration {}: {why}\n{saved}\nrun again: OPENQ_FUZZ_SEED={} tools/fuzz.sh {}{}",
            self.name,
            self.iteration,
            self.seed,
            self.name,
            if input.is_empty() { String::new() } else { format!("  (or OPENQ_FUZZ_REPLAY={} for this input)", file.display()) }
        )
    }

    /// Fail if anything panicked since the last check (except rasteriser panics OpenQ caught).
    fn check_panics(&self, input: &[u8]) {
        let (contained, p): (Vec<String>, Vec<String>) = std::mem::take(&mut *PANICS.lock()).into_iter().partition(|p| is_rasteriser_panic(p));
        CONTAINED.lock().extend(contained);
        if !p.is_empty() {
            self.fail(input, "bin", &format!("panicked:\n{}", p.join("\n")));
        }
    }

    /// Run `f` on its own thread: a panic or no answer within `limit` fails the test.
    fn guard<T: Send + 'static>(&self, what: &str, limit: Duration, input: &[u8], ext: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
        // A stack overflow aborts the whole process before anything can be saved: the input
        // being tried is always on disk.
        if !input.is_empty() {
            let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fuzz-failures");
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(format!("{}-last.{ext}", self.name)), input);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::Builder::new()
            .name("fuzz-case".into())
            .spawn(move || {
                let _ = tx.send(f());
            })
            .unwrap();
        match rx.recv_timeout(limit) {
            Ok(v) => {
                let _ = h.join();
                self.check_panics(input);
                v
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => self.fail(input, ext, &format!("{what}: no answer after {limit:?} (hang)")),
            Err(_) => {
                let p = std::mem::take(&mut *PANICS.lock());
                self.fail(input, ext, &format!("{what} panicked:\n{}", p.join("\n")))
            }
        }
    }

    fn done(&self, what: impl std::fmt::Display) {
        self.check_panics(&[]);
        for c in std::mem::take(&mut *CONTAINED.lock()) {
            eprintln!("{}: contained (not a failure): {}", self.name, c.lines().take(2).collect::<Vec<_>>().join(" "));
        }
        eprintln!("{}: {} iterations in {:.1} s: {what}", self.name, self.iteration, self.t0.elapsed().as_secs_f64());
    }
}

fn rss() -> Option<usize> {
    let s = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: usize = s.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

// ------------------------------------------------------------------ mutators

/// Numbers that tend to break things: limits of every integer type, values past what `f32`
/// holds (they become infinity), tiny, negative and zero values.
const NUMBERS: &[&str] = &[
    "0", "-0", "1", "-1", "2", "0.5", "-0.5", "1e-300", "-1e-300", "1e12", "1e13", "-1e12", "1e300", "-1e300", "3.4e38", "3.5e38", "-3.5e38", "255", "256",
    "511", "512", "513", "32767", "65535", "65536", "-65536", "2147483647", "4294967295", "4294967296", "9007199254740993", "9223372036854775807",
    "18446744073709551615", "-9223372036854775808", "44100", "1000000",
];

/// Strings that tend to break things or reach where they shouldn't.
fn hostile_string(rng: &mut fastrand::Rng) -> String {
    const S: &[&str] = &[
        "",
        " ",
        "../../../../../../../../etc/passwd",
        "/etc/shadow",
        "/dev/zero",
        "/dev/urandom",
        "/dev/stdin",
        "/proc/self/mem",
        "/proc/self/environ",
        "\\\\attacker\\share\\a.wav",
        "//attacker/share/a.wav",
        "\\\\?\\UNC\\attacker\\share\\a.wav",
        "\\\\.\\pipe\\x",
        "C:\\Windows\\win.ini",
        "file:///etc/passwd",
        "http://127.0.0.1:9/a.wav",
        "concat:/etc/passwd|/etc/passwd",
        "subfile,,start,0,end,0,,:/etc/passwd",
        "movie=/etc/passwd",
        "testsrc2,movie=/etc/passwd",
        "amovie=/etc/passwd[out0]",
        "color=red:size=99999x99999",
        "testsrc2=size=65535x65535",
        "x11grab",
        "lavfi",
        "v4l2",
        ":0.0",
        "%s%s%s%n%n",
        "\u{202e}gnp.exe",
        "\0",
        "a\0b",
        "$(touch /tmp/openq-pwned)",
        "`id`",
        "'; DROP TABLE cues; --",
        "<script>alert(1)</script>",
        "{{7*7}}",
        "00000000-0000-0000-0000-000000000000",
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
        "1.2.3",
        "101.3",
        "Pan",
        "Extra4294967295",
        "#zzzzzz",
        "255.255.255.255",
        "0.0.0.0:0",
        "[::1]:6454",
        "-1",
        "NaN",
        "inf",
    ];
    match rng.u8(..10) {
        0 => "A".repeat(rng.usize(1000..200_000)),
        1 => "😀\u{301}".repeat(rng.usize(1..5000)),
        2 => (0..rng.usize(1..64)).map(|_| char::from_u32(rng.u32(1..0x10FFFF)).unwrap_or('?')).collect(),
        _ => S[rng.usize(..S.len())].to_string(),
    }
}

/// Every value in `v`, for grafting pieces of a document into other places.
fn collect(v: &Value, out: &mut Vec<Value>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    match v {
        Value::Array(a) => a.iter().for_each(|x| collect(x, out, limit)),
        Value::Object(o) => o.values().for_each(|x| collect(x, out, limit)),
        _ => {}
    }
    if !matches!(v, Value::Object(o) if o.len() > 40) {
        out.push(v.clone());
    }
}

fn count_nodes(v: &Value) -> usize {
    1 + match v {
        Value::Array(a) => a.iter().map(count_nodes).sum(),
        Value::Object(o) => o.values().map(count_nodes).sum(),
        _ => 0,
    }
}

/// Strings that look like ids (cue ids, stage ids) in `v`.
fn ids(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if s.len() == 36 && s.as_bytes()[8] == b'-' => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| ids(x, out)),
        Value::Object(o) => o.values().for_each(|x| ids(x, out)),
        _ => {}
    }
}

/// Valid JSON with hostile contents: a few nodes picked at random are replaced, removed,
/// repeated, swapped with other parts of the document or with ids of other things.
fn mutate_json(rng: &mut fastrand::Rng, v: &mut Value, pool: &[Value]) {
    let mut id_list = Vec::new();
    ids(v, &mut id_list);
    let n = count_nodes(v);
    let picks: Vec<usize> = (0..rng.usize(1..5)).map(|_| rng.usize(..n)).collect();
    let mut counter = 0;
    walk(rng, v, &picks, &mut counter, pool, &id_list);
}

fn walk(rng: &mut fastrand::Rng, v: &mut Value, picks: &[usize], counter: &mut usize, pool: &[Value], id_list: &[String]) {
    let me = *counter;
    *counter += 1;
    if picks.contains(&me) {
        mutate_node(rng, v, pool, id_list);
        return;
    }
    match v {
        Value::Array(a) => a.iter_mut().for_each(|x| walk(rng, x, picks, counter, pool, id_list)),
        Value::Object(o) => o.values_mut().for_each(|x| walk(rng, x, picks, counter, pool, id_list)),
        _ => {}
    }
}

fn mutate_node(rng: &mut fastrand::Rng, v: &mut Value, pool: &[Value], id_list: &[String]) {
    let number = |rng: &mut fastrand::Rng| serde_json::from_str::<Value>(NUMBERS[rng.usize(..NUMBERS.len())]).unwrap();
    match v {
        Value::Number(x) => {
            *v = match rng.u8(..4) {
                0 => serde_json::json!(-x.as_f64().unwrap_or(0.0)),
                1 => serde_json::json!(x.as_f64().unwrap_or(0.0) * 1e6),
                _ => number(rng),
            }
        }
        Value::String(s) if s.len() == 36 && !id_list.is_empty() && rng.bool() => {
            // Aim at something else: another cue, a stage, itself.
            *s = id_list[rng.usize(..id_list.len())].clone();
        }
        Value::String(_) => *v = Value::String(hostile_string(rng)),
        Value::Array(a) if !a.is_empty() && rng.u8(..3) > 0 => {
            let i = rng.usize(..a.len());
            match rng.u8(..5) {
                // Repeated elements: duplicate ids, the same fixture twice...
                0 => {
                    let x = a[i].clone();
                    let n = if rng.u8(..8) == 0 { rng.usize(100..3000) } else { rng.usize(1..4) };
                    a.extend(std::iter::repeat_n(x, n));
                }
                1 => {
                    a.remove(i);
                }
                2 => rng.shuffle(a),
                3 if !pool.is_empty() => a.push(pool[rng.usize(..pool.len())].clone()),
                _ => a.truncate(i),
            }
        }
        Value::Object(o) if !o.is_empty() && rng.u8(..3) > 0 => {
            let k = o.keys().nth(rng.usize(..o.len())).cloned().unwrap();
            match rng.u8(..4) {
                0 => {
                    o.remove(&k);
                }
                1 if !pool.is_empty() => {
                    o.insert(k, pool[rng.usize(..pool.len())].clone());
                }
                2 => {
                    // Deep nesting (serde's recursion limit is 128).
                    let mut deep = Value::Null;
                    for _ in 0..rng.usize(10..200) {
                        deep = if rng.bool() { Value::Array(vec![deep]) } else { serde_json::json!({ "children": [deep] }) };
                    }
                    o.insert(k, deep);
                }
                _ => {
                    o.insert(k, Value::Null);
                }
            }
        }
        _ => {
            *v = match rng.u8(..6) {
                0 => Value::Null,
                1 => Value::Bool(rng.bool()),
                2 => number(rng),
                3 => Value::String(hostile_string(rng)),
                4 if !pool.is_empty() => pool[rng.usize(..pool.len())].clone(),
                _ => Value::Array(vec![]),
            }
        }
    }
}

/// Damaged bytes: changed, cut, repeated and removed spans, digits made longer.
fn mutate_bytes(rng: &mut fastrand::Rng, src: &[u8]) -> Vec<u8> {
    let mut v = src.to_vec();
    for _ in 0..rng.usize(1..12) {
        if v.is_empty() {
            break;
        }
        let i = rng.usize(..v.len());
        match rng.u8(..7) {
            0 => v[i] = rng.u8(..),
            1 => v.truncate(i),
            2 => {
                let j = (i + rng.usize(..64)).min(v.len());
                let span = v[i..j].to_vec();
                v.splice(i..i, span.iter().cycle().take(span.len() * rng.usize(1..20)).copied());
            }
            3 => {
                let j = (i + rng.usize(..32)).min(v.len());
                v.drain(i..j);
            }
            4 => {
                // A length or count field set to something huge.
                let j = (i + 4).min(v.len());
                v[i..j].fill(0xff);
            }
            5 => {
                let j = (i + 4).min(v.len());
                v[i..j].fill(0);
            }
            _ => {
                if v[i].is_ascii_digit() {
                    v.insert(i, b"095"[rng.usize(..3)]);
                }
            }
        }
    }
    v
}

// ------------------------------------------------------------------ show files

/// 16-bit PCM WAV.
fn write_wav(path: &Path, rate: u32, channels: u16, secs: f64) {
    let frames = (rate as f64 * secs) as usize;
    let n = frames * channels as usize;
    let mut b = Vec::with_capacity(44 + n * 2);
    b.extend(b"RIFF");
    b.extend(((36 + n * 2) as u32).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(channels.to_le_bytes());
    b.extend(rate.to_le_bytes());
    b.extend((rate * channels as u32 * 2).to_le_bytes());
    b.extend((channels * 2).to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(((n * 2) as u32).to_le_bytes());
    for i in 0..n {
        let s = ((i / channels as usize) as f32 * 0.06).sin() * 0.3;
        b.extend(((s * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// A small lighting design: generic fixtures, a sequence that sets them, effects.
fn seed_lighting() -> Lighting {
    let mut l = Lighting::default();
    let types: Vec<_> = ["generic/dimmer", "generic/drgbw", "generic/spot", "generic/wash"].iter().filter_map(|i| library::generic(i)).collect();
    let mut address = 1;
    for (i, t) in types.iter().cycle().take(12).enumerate() {
        l.add_type(t);
        l.fixtures.push(Fixture { id: i as u32 + 1, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), universe: 1, address, ..Default::default() });
        address += t.modes[0].footprint();
    }
    let units = l.units();
    let attrs = openq::light::model::Attribute::ALL;
    let cues = (0..4)
        .map(|c| {
            let mut cue = LightCue { number: (c + 1).to_string(), fade: 0.5, ..Default::default() };
            for (k, u) in units.iter().enumerate() {
                cue.values.entry(*u).or_default().insert(attrs[(k + c) % attrs.len()], (0.25 * c as f32).into());
            }
            cue
        })
        .collect();
    l.sequences.push(Sequence { id: 1, cues, ..Default::default() });
    l.effects.push(openq::light::effects::Effect { id: 1, ..Default::default() });
    l
}

/// A show with one cue of every kind, a group, a second list and a lighting design.
fn seed_workspace(media: &Path) -> Workspace {
    let mut ws = Workspace::default();
    let mut cues: Vec<Cue> = KindTag::ALL.iter().map(|k| Cue::new(k.default_kind())).collect();
    let ids: Vec<CueId> = cues.iter().map(|c| c.id).collect();
    for (i, c) in cues.iter_mut().enumerate() {
        c.number = (i + 1).to_string();
        c.timecode_trigger = (i % 4 == 0).then_some(i as f64);
        let target = Some(ids[(i * 7) % ids.len()]);
        match &mut c.kind {
            CueKind::Audio(a) => {
                a.path = media.to_path_buf();
                a.end_time = Some(0.5);
            }
            CueKind::Fade(f) => {
                f.target = target;
                f.volume_db = Some(-12.0);
                f.duration = 0.3;
            }
            CueKind::Start { target: t } | CueKind::Stop { target: t } | CueKind::Pause { target: t } | CueKind::Goto { target: t } | CueKind::Devamp { target: t } => *t = target,
            CueKind::Network(n) => {
                n.host = "127.0.0.1".into();
                n.port = 9;
            }
            CueKind::Light(l) => l.sequence = Some(1),
            // Never out of this computer's real MIDI ports.
            CueKind::Midi(m) => m.port = Some("openq-fuzz-no-port".into()),
            CueKind::Timecode(t) => t.midi_port = Some("openq-fuzz-no-port".into()),
            CueKind::Target { target: t, new_target } => {
                *t = target;
                *new_target = Some(ids[(i * 3) % ids.len()]);
            }
            CueKind::Load { target: t, time } => {
                *t = target;
                *time = 0.2;
            }
            k => {
                if let Some(t) = k.target_mut() {
                    *t = target;
                }
            }
        }
    }
    let group = Cue::new(CueKind::Group { mode: openq::model::GroupMode::StartAll, children: cues[..4].iter().map(|c| Cue { id: CueId::new_v4(), ..c.clone() }).collect() });
    cues.push(group);
    ws.cue_list.cues = cues;
    let tab = ws.cue_list.add_list("Second", None);
    ws.cue_list.switch_to(tab, None);
    let mut wait = Cue::new(CueKind::Wait { duration: 0.2 });
    wait.number = "w".into();
    ws.cue_list.cues = vec![wait];
    ws.cue_list.switch_to(0, None);
    ws.lighting = seed_lighting();
    ws.settings.double_go_protection = 0.0;
    ws
}

/// Pieces to build shows from: the seed show, plus cues and lighting from the demo show when
/// it's there.
struct Corpus {
    base: Value,
    cues: Vec<Value>,
    lighting: Option<Value>,
    pool: Vec<Value>,
}

impl Corpus {
    fn new(media: &Path) -> Self {
        let base = serde_json::to_value(seed_workspace(media)).unwrap();
        let mut cues = Vec::new();
        let mut lighting = None;
        let demo = std::env::var_os("OPENQ_FUZZ_SHOW").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/demo-show/demo.openq"));
        match Workspace::load(&demo) {
            Ok(ws) => {
                for c in ws.cue_list.iter_all().into_iter().take(3000) {
                    cues.push(serde_json::to_value(c).unwrap());
                }
                let mut l = ws.lighting.clone();
                l.fixtures.truncate(80);
                l.sequences.truncate(6);
                for s in &mut l.sequences {
                    s.cues.truncate(6);
                }
                lighting = Some(serde_json::to_value(&l).unwrap());
                eprintln!("demo show: {} cues to draw from", cues.len());
            }
            Err(e) => eprintln!("no demo show ({}: {e:#}); using the built-in seed only (tools/demo_show.sh --no-run makes it)", demo.display()),
        }
        let mut pool = Vec::new();
        collect(&base, &mut pool, 4000);
        for c in cues.iter().step_by(17) {
            collect(c, &mut pool, 8000);
        }
        Self { base, cues, lighting, pool }
    }

    fn show(&self, rng: &mut fastrand::Rng) -> Value {
        let mut v = self.base.clone();
        if !self.cues.is_empty() && rng.u8(..3) > 0 {
            let picked: Vec<Value> = (0..rng.usize(1..40)).map(|_| self.cues[rng.usize(..self.cues.len())].clone()).collect();
            v["cue_list"]["cues"] = Value::Array(picked);
        }
        if let Some(l) = &self.lighting
            && rng.bool()
        {
            v["lighting"] = l.clone();
        }
        v
    }
}

/// Keep a fuzzed show off the network and the real sound card: Network cues and DMX outputs go
/// to loopback, DMX inputs and the Enttec port are off, audio stays on the virtual output.
fn keep_local(ws: &mut Workspace) {
    use openq::light::dmx::OutputProtocol;
    ws.settings.audio_device = None;
    for list in ws.cue_list.all_roots_mut() {
        visit(list, &mut |c| {
            if let CueKind::Network(n) = &mut c.kind {
                n.host = "127.0.0.1".into();
                n.port = 9;
            }
        });
    }
    for o in &mut ws.lighting.outputs {
        match &mut o.protocol {
            OutputProtocol::ArtNet { target, .. } | OutputProtocol::Sacn { target, .. } => *target = "127.0.0.1".into(),
            OutputProtocol::EnttecPro { .. } => o.enabled = false,
        }
    }
    for i in &mut ws.lighting.inputs {
        i.enabled = false;
    }
}

fn visit(cues: &mut [Cue], f: &mut dyn FnMut(&mut Cue)) {
    for c in cues {
        f(c);
        if let Some(ch) = c.children_mut() {
            visit(ch, f);
        }
    }
}

fn random_command(rng: &mut fastrand::Rng, ids: &[CueId]) -> Command {
    let id = |rng: &mut fastrand::Rng| if ids.is_empty() || rng.u8(..10) == 0 { CueId::new_v4() } else { ids[rng.usize(..ids.len())] };
    match rng.u8(..14) {
        0..4 => Command::Go,
        4 => Command::Stop,
        5 => Command::Panic,
        6 => Command::PauseAll,
        7 => Command::ResumeAll,
        8 => Command::PlayheadNext,
        9 => Command::PlayheadPrevious,
        10 => Command::SetPlayhead(Some(id(rng))),
        11 => Command::Cue(id(rng), [CueAction::Start, CueAction::Stop, CueAction::Pause, CueAction::Resume, CueAction::Devamp][rng.usize(..5)]),
        12 => Command::SwitchList(rng.usize(..4)),
        _ => Command::Light(if rng.bool() { LightControl::Go(rng.u32(..4), None) } else { LightControl::Release(Some(rng.u32(..4)), rng.f32() * 2.0) }),
    }
}

/// A show whose audio goes to a virtual output.
fn virtual_show() -> SharedShow {
    let mut show = Show::new(Prober::new());
    show.media.audio = Some(AudioOutput::open_virtual(48_000, 2, None).expect("virtual output"));
    show.media.audio_error = None;
    Arc::new(Mutex::new(show))
}

/// Load a show file; if it loads, save it again, vet it as a replica's edit would be, and play
/// it for a few simulated seconds with random commands.
fn exercise_show(f: &Fuzz, show: &SharedShow, path: &Path, bytes: &[u8], rng_seed: u64) -> bool {
    let p = path.to_path_buf();
    let loaded = f.guard("loading", Duration::from_secs(5), bytes, "openq", move || Workspace::load(&p).map_err(|e| format!("{e:#}")));
    let Ok(mut ws) = loaded else { return false };
    // Every value came in within its plausible range (`limits`): clamping again changes nothing.
    let mut again = ws.clone();
    openq::limits::workspace(&mut again);
    if again != ws {
        f.fail(bytes, "openq", "a loaded show still holds values out of range");
    }
    keep_local(&mut ws);
    let (show, path) = (show.clone(), path.to_path_buf());
    f.guard("saving and playing", Duration::from_secs(30), bytes, "openq", move || {
        let mut rng = fastrand::Rng::with_seed(rng_seed);
        let _ = ws.save(&path.with_extension("again.openq"));
        let _ = openq::remote::vet::workspace(&openq::remote::vet::media_paths(&ws).into_iter().collect(), &ws);
        let _ = ws.lighting.validate();
        let _ = openq::light::patch::diff(&Lighting::default(), &ws.lighting).map(|p| openq::light::patch::apply(&mut Lighting::default(), &p));
        let _ = openq::light::plot::svg(&ws.lighting, "fuzz");
        let ids: Vec<CueId> = ws.cue_list.iter_all().iter().map(|c| c.id).collect();
        let mut s = show.lock();
        s.replace_workspace(ws, None);
        let t0 = Instant::now();
        for i in 0..300u32 {
            let now = t0 + Duration::from_millis(20 * i as u64);
            if rng.u8(..8) == 0 {
                let _ = apply(&mut s, random_command(&mut rng, &ids), now);
            }
            if rng.u8(..40) == 0 {
                // An edit while playing, as the inspector does.
                if let Some(c) = s.ws.cue_list.cues.first_mut() {
                    c.pre_wait = rng.f64();
                }
                s.edited();
            }
            s.tick(now);
        }
        let _ = apply(&mut s, Command::Panic, t0 + Duration::from_secs(7));
        s.tick(t0 + Duration::from_secs(8));
        let errors = s.media.light.drain_errors();
        assert!(s.recovered.is_none(), "the engine recovered from a panic: {:?}", s.recovered);
        assert!(errors.iter().all(|e| !e.contains("internal error") && !e.contains("panic")), "light engine: {errors:?}");
    });
    true
}

/// Damaged and hostile show files: no panic, no hang; what loads plays.
#[test]
#[ignore = "fuzz"]
fn fuzz_show_files() {
    let mut f = Fuzz::new("fuzz_show_files");
    let dir = tmpdir("show");
    let media = dir.join("tone.wav");
    write_wav(&media, 48_000, 2, 0.5);
    let show = virtual_show();
    let path = dir.join("fuzz.openq");
    if let Some(r) = replay() {
        let bytes = std::fs::read(&r).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let loaded = exercise_show(&f, &show, &path, &bytes, f.seed);
        f.done(format!("replayed {} (loaded: {loaded})", r.display()));
        return;
    }
    let corpus = Corpus::new(&media);
    let (mut loaded, mut refused) = (0, 0);
    let rss0 = rss();
    while f.next() {
        let mut v = corpus.show(&mut f.rng);
        // Mostly few changes, so most files still parse and get played.
        for _ in 0..if f.rng.u8(..3) == 0 { f.rng.usize(2..5) } else { 1 } {
            mutate_json(&mut f.rng, &mut v, &corpus.pool);
        }
        let mut bytes = serde_json::to_vec(&v).unwrap();
        if f.rng.u8(..6) == 0 {
            bytes = mutate_bytes(&mut f.rng, &bytes);
        }
        std::fs::write(&path, &bytes).unwrap();
        let seed = f.rng.u64(..);
        if exercise_show(&f, &show, &path, &bytes, seed) {
            loaded += 1;
        } else {
            refused += 1;
        }
    }
    // Hostile on purpose: a huge file, deep nesting, a show that is a device or a pipe.
    let deep = format!("{{\"cue_list\":{{\"cues\":[{}{}]}}}}", "{\"kind\":{\"Group\":{\"children\":[".repeat(5000), "]}}}".repeat(5000));
    for (name, bytes) in [("deep", deep.into_bytes()), ("nested", "[".repeat(1_000_000).into_bytes()), ("big string", format!("{{\"stages\":[{{\"name\":\"{}\"}}]}}", "x".repeat(50 << 20)).into_bytes())] {
        std::fs::write(&path, &bytes).unwrap();
        let p = path.clone();
        f.guard(name, Duration::from_secs(10), &[], "openq", move || {
            let _ = Workspace::load(&p);
        });
    }
    for special in special_files(&dir) {
        let p = special.clone();
        let r = f.guard(&format!("loading {}", special.display()), Duration::from_secs(5), &[], "openq", move || Workspace::load(&p).is_ok());
        assert!(!r, "{} loaded as a show", special.display());
    }
    if let (Some(a), Some(b)) = (rss0, rss()) {
        eprintln!("RSS {} → {} MB", a >> 20, b >> 20);
    }
    show.lock().shutdown();
    f.done(format!("{loaded} loaded and played, {refused} refused"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Files that aren't regular files, or are absurdly big: a FIFO nobody writes to (opening it
/// blocks), endless devices, a directory, a sparse 64 GB file, a symlink loop.
fn special_files(dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/dev/zero"), PathBuf::from("/dev/urandom"), dir.to_path_buf()];
    let fifo = dir.join("pipe.wav");
    if std::process::Command::new("mkfifo").arg(&fifo).status().is_ok_and(|s| s.success()) {
        v.push(fifo);
    }
    let sparse = dir.join("huge.wav");
    if std::fs::File::create(&sparse).and_then(|f| f.set_len(64 << 30)).is_ok() {
        v.push(sparse);
    }
    #[cfg(unix)]
    {
        let a = dir.join("loop-a.png");
        let _ = std::os::unix::fs::symlink(dir.join("loop-b.png"), &a);
        let _ = std::os::unix::fs::symlink(&a, dir.join("loop-b.png"));
        v.push(a);
    }
    v
}

// ------------------------------------------------------------------ media files

/// Small valid files of many formats to damage: WAV and images always, more audio and video
/// when the `ffmpeg` program is there, and small files from the demo show.
fn media_seeds(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut seeds = Vec::new();
    let wav = dir.join("seed.wav");
    write_wav(&wav, 44_100, 2, 0.2);
    seeds.push(("wav".into(), std::fs::read(&wav).unwrap()));
    let rgba = image::RgbaImage::from_fn(24, 16, |x, y| image::Rgba([(x * 10) as u8, (y * 15) as u8, 128, 200]));
    for ext in ["png", "bmp", "gif", "tiff", "webp", "jpg"] {
        let p = dir.join(format!("seed.{ext}"));
        let img = if ext == "jpg" { image::DynamicImage::ImageRgb8(image::DynamicImage::ImageRgba8(rgba.clone()).to_rgb8()) } else { image::DynamicImage::ImageRgba8(rgba.clone()) };
        if img.save(&p).is_ok() {
            seeds.push((ext.into(), std::fs::read(&p).unwrap()));
        }
    }
    seeds.push(("svg".into(), br##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32" viewBox="0 0 64 32"><defs><linearGradient id="g"><stop offset="0" stop-color="#f00"/><stop offset="1" stop-color="#00f"/></linearGradient></defs><rect width="64" height="32" fill="url(#g)"/><circle cx="16" cy="16" r="10" stroke="#fff" stroke-width="2"/><text x="30" y="20" font-size="10">Hi</text></svg>"##.to_vec()));
    let ffmpeg: &[(&str, &[&str])] = &[
        ("mp4", &["-f", "lavfi", "-i", "testsrc2=size=64x36:rate=10:duration=0.4", "-f", "lavfi", "-i", "sine=duration=0.4", "-c:v", "mpeg4", "-c:a", "aac", "-shortest"]),
        ("mov", &["-f", "lavfi", "-i", "testsrc2=size=64x36:rate=10:duration=0.4", "-c:v", "mjpeg", "-pix_fmt", "yuvj420p"]),
        ("mkv", &["-f", "lavfi", "-i", "testsrc2=size=64x36:rate=10:duration=0.4", "-f", "lavfi", "-i", "sine=duration=0.4", "-c:v", "ffv1", "-c:a", "flac", "-shortest"]),
        ("webm", &["-f", "lavfi", "-i", "testsrc2=size=64x36:rate=10:duration=0.4", "-c:v", "libvpx", "-b:v", "50k"]),
        ("avi", &["-f", "lavfi", "-i", "testsrc2=size=64x36:rate=10:duration=0.4", "-c:v", "mjpeg", "-pix_fmt", "yuvj422p"]),
        ("ts", &["-f", "lavfi", "-i", "testsrc2=size=64x36:rate=25:duration=0.4", "-c:v", "mpeg2video"]),
        ("flac", &["-f", "lavfi", "-i", "sine=duration=0.3"]),
        ("mp3", &["-f", "lavfi", "-i", "sine=duration=0.3"]),
        ("ogg", &["-f", "lavfi", "-i", "sine=duration=0.3", "-c:a", "libvorbis"]),
        ("opus", &["-f", "lavfi", "-i", "sine=duration=0.3"]),
        ("m4a", &["-f", "lavfi", "-i", "sine=duration=0.3", "-c:a", "aac"]),
        ("aiff", &["-f", "lavfi", "-i", "sine=duration=0.3"]),
        ("wav", &["-f", "lavfi", "-i", "sine=duration=0.3", "-c:a", "pcm_f32le", "-ac", "6"]),
        ("caf", &["-f", "lavfi", "-i", "sine=duration=0.3", "-c:a", "alac"]),
    ];
    for (ext, args) in ffmpeg {
        let p = dir.join(format!("ff.{ext}"));
        let ok = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error"])
            .args(*args)
            .arg(&p)
            .stdin(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok && let Ok(b) = std::fs::read(&p) {
            seeds.push((ext.to_string(), b));
        }
    }
    let demo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/demo-show/media");
    let mut demo_files = Vec::new();
    for sub in ["sfx", "music", "video", "images"] {
        for e in std::fs::read_dir(demo.join(sub)).into_iter().flatten().flatten() {
            if e.metadata().is_ok_and(|m| m.is_file() && m.len() < 400_000) {
                demo_files.push(e.path());
            }
        }
    }
    demo_files.sort();
    let mut seen = std::collections::HashSet::new();
    for p in demo_files {
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if seen.insert(ext.clone())
            && let Ok(b) = std::fs::read(&p)
        {
            seeds.push((ext, b));
        }
    }
    seeds
}

/// SVGs that try to read other files, reach the network or blow up.
fn hostile_svgs() -> Vec<String> {
    let mut v = vec![
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="1000000000" height="1000000000"><rect width="10" height="10"/></svg>"#.to_string(),
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="1e30" height="-5" viewBox="0 0 0 0"/>"#.into(),
        r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;"><!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;"><!ENTITY d "&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;">]><svg xmlns="http://www.w3.org/2000/svg"><text>&d;&d;&d;&d;&d;</text></svg>"#.into(),
        r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY x SYSTEM "file:///etc/passwd">]><svg xmlns="http://www.w3.org/2000/svg"><text>&x;</text></svg>"#.into(),
    ];
    for href in ["file:///etc/passwd", "/etc/passwd", "/dev/zero", "/dev/urandom", "\\\\attacker\\share\\x.png", "//attacker/share/x.png", "http://127.0.0.1:9/x.png", "self.svg", "../../../../etc/shadow"] {
        v.push(format!(r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="32" height="32"><image href="{href}" xlink:href="{href}" width="32" height="32"/></svg>"#));
        v.push(format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><style>@import url("{href}");</style><rect width="9" height="9" style="fill:url({href})"/></svg>"#));
    }
    // Billion laughs with <use>: each level references the previous ten times.
    let mut defs = String::from(r#"<g id="l0"><rect width="1" height="1"/></g>"#);
    for i in 1..12 {
        defs.push_str(&format!(r#"<g id="l{i}">"#));
        for _ in 0..10 {
            defs.push_str(&format!(r##"<use href="#l{}"/>"##, i - 1));
        }
        defs.push_str("</g>");
    }
    v.push(format!(r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><defs>{defs}</defs><use href="#l11"/></svg>"##));
    v.push(format!(r#"<svg xmlns="http://www.w3.org/2000/svg">{}{}</svg>"#, "<g>".repeat(100_000), "</g>".repeat(100_000)));
    v.push(r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><filter id="f"><feGaussianBlur stdDeviation="1e9"/><feMorphology radius="1e9"/></filter><rect width="64" height="64" filter="url(#f)"/></svg>"#.into());
    v.push(r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><pattern id="p" width="0.0000001" height="0.0000001" patternUnits="userSpaceOnUse"><rect width="1" height="1"/></pattern><rect width="64" height="64" fill="url(#p)"/></svg>"##.into());
    v
}

/// Run every media reader on `p`. Errors are fine; a panic or a hang isn't.
fn read_media(p: &Path, prober: &Prober, audio: &Mutex<AudioOutput>) {
    let _ = prober.get_blocking(p);
    let _ = openq::media::decoder::probe(p);
    if let Ok(w) = openq::media::decoder::waveform(p) {
        let _ = w.range(0.0, w.duration);
        let _ = w.range(-1.0, 1e300);
    }
    let _ = openq::media::still::dimensions(p);
    let _ = openq::media::still::load(p, 1.0);
    let _ = openq::media::still::load(p, 64.0);
    let _ = openq::media::still::thumbnail(p, 96);
    let id = CueId::new_v4();
    let mut a = audio.lock();
    if a.start(id, &AudioCue { path: p.to_path_buf(), ..Default::default() }, 2, Instant::now()).is_ok() {
        drop(a);
        std::thread::sleep(Duration::from_millis(30));
        audio.lock().stop(id);
    }
}

/// Damaged media files of every format and hostile special files: every reader answers in time,
/// with an error or a result, and keeps serving.
#[test]
#[ignore = "fuzz"]
fn fuzz_media_files() {
    let mut f = Fuzz::new("fuzz_media_files");
    let dir = tmpdir("media");
    let prober = Prober::new();
    let posters = openq::media::Posters::new();
    let waveforms = openq::media::Waveforms::new();
    let audio = Arc::new(Mutex::new(AudioOutput::open_virtual(48_000, 2, None).unwrap()));
    if let Some(r) = replay() {
        let bytes = std::fs::read(&r).unwrap_or_default();
        let (prober, audio) = (prober.clone(), audio.clone());
        f.guard("replay", Duration::from_secs(20), &[], "bin", move || read_media(&r, &prober, &audio));
        f.done("replayed");
        drop(bytes);
        return;
    }
    let seeds = media_seeds(&dir);
    eprintln!("media seeds: {}", seeds.iter().map(|(e, b)| format!("{e} ({} B)", b.len())).collect::<Vec<_>>().join(", "));

    // Special files and hostile SVGs first: each reader must give up on its own.
    let mut specials = special_files(&dir);
    for (i, svg) in hostile_svgs().iter().enumerate() {
        let p = dir.join(format!("hostile{i}.svg"));
        std::fs::write(&p, svg).unwrap();
        specials.push(p);
    }
    let marker = Path::new("/tmp/openq-pwned");
    for p in specials {
        eprintln!("  special: {}", p.display());
        let (pr, au, q) = (prober.clone(), audio.clone(), p.clone());
        let input = openq::recovery::read_file(&p, 1 << 20).unwrap_or_default();
        f.guard(&format!("reading {}", p.display()), Duration::from_secs(15), &input, "svg", move || read_media(&q, &pr, &au));
        let _ = posters.get(&p, 0.0);
        let _ = waveforms.get(&p);
    }
    assert!(!marker.exists(), "something ran a shell command from a file name");

    let (mut n, rss0) = (0u64, rss());
    while f.next() {
        let (ext, src) = &seeds[f.rng.usize(..seeds.len())];
        let bytes = match f.rng.u8(..8) {
            // Another format's bytes under this extension.
            0 => seeds[f.rng.usize(..seeds.len())].1.clone(),
            // A valid header followed by garbage.
            1 => {
                let keep = f.rng.usize(..src.len().min(512) + 1);
                let mut b = src[..keep].to_vec();
                b.extend((0..f.rng.usize(..20_000)).map(|_| f.rng.u8(..)));
                b
            }
            _ => mutate_bytes(&mut f.rng, src),
        };
        // A new name every time: the readers cache by path.
        let p = dir.join(format!("m{}.{ext}", f.iteration));
        std::fs::write(&p, &bytes).unwrap();
        let (pr, au, q) = (prober.clone(), audio.clone(), p.clone());
        f.guard(&format!("reading a damaged .{ext}"), Duration::from_secs(15), &bytes, ext, move || read_media(&q, &pr, &au));
        // Posters and waveforms on their worker pools.
        let _ = posters.get(&p, f.rng.f64() * 2.0);
        let _ = waveforms.get(&p);
        if f.iteration.is_multiple_of(50) {
            let t = Instant::now();
            while posters.get(&p, 0.0).is_none() || waveforms.get(&p).is_none() {
                if t.elapsed() > Duration::from_secs(15) {
                    f.fail(&bytes, ext, "poster/waveform workers stopped answering");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                if e.file_name().to_string_lossy().starts_with('m') {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        n += 1;
    }
    // Still serving after all that.
    let good = dir.join("seed.wav");
    let t = Instant::now();
    while !matches!(prober.get(&good), Some(Ok(_))) {
        assert!(t.elapsed() < Duration::from_secs(10), "prober stopped serving");
        std::thread::sleep(Duration::from_millis(5));
    }
    if let (Some(a), Some(b)) = (rss0, rss()) {
        eprintln!("RSS {} → {} MB", a >> 20, b >> 20);
        assert!(b < a + (1 << 30), "memory grew by {} MB", (b - a) >> 20);
    }
    f.done(format!("{n} damaged files"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ lighting files

/// Effect files, fixture files, zips, command lines and gobo images: errors, never a panic.
#[test]
#[ignore = "fuzz"]
fn fuzz_lighting_files() {
    use openq::light::{command, fixlib, fxfile, gobo, mvr, usitt, zipread};
    let mut f = Fuzz::new("fuzz_lighting_files");
    let dir = tmpdir("lighting");
    let l = seed_lighting();
    let effect = openq::light::effects::Effect { id: 3, name: "Chase".into(), ..Default::default() };
    let fx = fxfile::write(&effect, "fuzz", "a chase");
    let types: Vec<String> = l.fixture_types.iter().map(fixlib::write).collect();
    let zip = zipread::write(&[("description.xml", b"<GDTF/>"), ("wheels/gobo1.png", b"\x89PNG\r\n\x1a\n"), ("a/b/c.txt", &[7; 3000])]).unwrap();
    // An MVR of the seed design (scene, GDTFs, meshes) and its scene text; a USITT ASCII file.
    let mvr_zip = mvr::export(&l, "fuzz").unwrap();
    let scene = mvr::scene(&l, "fuzz");
    let ascii = usitt::write(&l, "fuzz").0;
    let words = ["fixture", "group", "thru", "+", "-", "at", "full", "out", "@", "record", "cue", "sequence", "preset", "effect", "gobo", "wheel", "star", "101.3", "1", "0", "99999999999", "-5", ".", "..", "/", "time", "delay", "go", "release", "clear", "update", "delete", "copy", "move", "label", "\"x\""];
    // Gobo images as fixture files carry them (PNG, and SVG in GDTF wheels).
    let mut gobos = vec![br#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><circle cx="32" cy="32" r="20"/></svg>"#.to_vec()];
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(image::GrayImage::from_fn(32, 32, |x, y| image::Luma([((x ^ y) * 8) as u8]))).write_to(&mut png, image::ImageFormat::Png).unwrap();
    gobos.push(png.into_inner());
    let mut pool = Vec::new();
    for t in &types {
        collect(&serde_json::from_str(t).unwrap(), &mut pool, 3000);
    }
    let (mut ok, mut err) = (0, 0);
    while f.next() {
        let r = f.rng.u8(..7);
        let (input, ext): (Vec<u8>, &str) = match r {
            0 | 1 => {
                let src = if r == 0 { fx.clone() } else { types[f.rng.usize(..types.len())].clone() };
                let mut v: Value = serde_json::from_str(&src).unwrap();
                mutate_json(&mut f.rng, &mut v, &pool);
                let mut b = serde_json::to_vec(&v).unwrap();
                if f.rng.u8(..4) == 0 {
                    b = mutate_bytes(&mut f.rng, &b);
                }
                (b, if r == 0 { "oqfx" } else { "oqfixture" })
            }
            2 => (mutate_bytes(&mut f.rng, &zip), "zip"),
            3 => {
                let line: Vec<String> = (0..f.rng.usize(..12)).map(|_| if f.rng.u8(..8) == 0 { hostile_string(&mut f.rng) } else { words[f.rng.usize(..words.len())].to_string() }).collect();
                (line.join(if f.rng.bool() { " " } else { "" }).into_bytes(), "txt")
            }
            4 => {
                let src = &gobos[f.rng.usize(..gobos.len())];
                (if f.rng.u8(..8) == 0 { src.clone() } else { mutate_bytes(&mut f.rng, src) }, "png")
            }
            5 => {
                // The whole zip, or a damaged scene in a sound zip (with the seed's other files).
                if f.rng.bool() {
                    (mutate_bytes(&mut f.rng, &mvr_zip), "mvr")
                } else {
                    let bad = mutate_bytes(&mut f.rng, scene.as_bytes());
                    let mut entries: Vec<(String, Vec<u8>)> = vec![("GeneralSceneDescription.xml".into(), bad)];
                    for n in zipread::names(&mvr_zip).unwrap().into_iter().filter(|n| n != "GeneralSceneDescription.xml") {
                        let data = zipread::read(&mvr_zip, &n, 1 << 24).unwrap();
                        entries.push((n, data));
                    }
                    let refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, d)| (n.as_str(), d.as_slice())).collect();
                    (zipread::write(&refs).unwrap(), "mvr")
                }
            }
            _ => {
                let mut b = mutate_bytes(&mut f.rng, ascii.as_bytes());
                if f.rng.u8(..4) == 0 {
                    b.extend(hostile_string(&mut f.rng).into_bytes());
                }
                (b, "asc")
            }
        };
        let i = input.clone();
        let fine = f.guard("reading", Duration::from_secs(5), &input, ext, move || {
            let text = String::from_utf8_lossy(&i);
            match r {
                0 => fxfile::read(&text).and_then(|e| fxfile::sanitize(e.effect)).is_ok(),
                1 => fixlib::read(&text).is_ok(),
                2 => zipread::names(&i).map(|names| names.iter().all(|n| zipread::read(&i, n, 1 << 20).is_ok())).is_ok(),
                3 => command::parse(&text).is_ok(),
                4 => {
                    let thumb = gobo::thumbnail(&i);
                    let _ = gobo::decode(&text);
                    thumb.is_ok_and(|t| gobo::decode(&t).is_some())
                }
                // Read, merged into a design, brought into range: a design that still checks.
                5 => mvr::import(&i, &[]).is_ok_and(|imp| {
                    let mut l = Lighting::default();
                    mvr::merge(&mut l, imp);
                    openq::limits::lighting(&mut l);
                    let _ = l.validate();
                    true
                }),
                _ => usitt::read(&text).is_ok_and(|file| {
                    let mut l = Lighting::default();
                    usitt::merge(&mut l, &file, "fuzz");
                    openq::limits::lighting(&mut l);
                    let _ = l.validate();
                    true
                }),
            }
        });
        if fine { ok += 1 } else { err += 1 }
        // The libraries read whole folders of such files.
        if f.iteration.is_multiple_of(25) && r < 2 {
            let sub = dir.join(if r == 0 { "effects" } else { "fixtures" });
            let _ = std::fs::create_dir_all(&sub);
            std::fs::write(sub.join(format!("{}.{ext}", f.iteration)), &input).unwrap();
            let s = sub.clone();
            f.guard("loading a library folder", Duration::from_secs(10), &input, ext, move || {
                let _ = fixlib::load_dir(&s);
            });
        }
    }
    f.done(format!("{ok} accepted, {err} refused"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ config files

/// Garbage in every file of the config folder, and file system errors when saving: OpenQ starts
/// with defaults or ignores the file.
#[test]
#[ignore = "fuzz"]
fn fuzz_config_files() {
    use openq::recovery::{Autosave, find_orphans_in};
    let mut f = Fuzz::new("fuzz_config_files");
    let dir = tmpdir("config");
    // SAFETY: this test reads the variable only on its own threads, after setting it.
    unsafe { std::env::set_var("OPENQ_CONFIG_DIR", &dir) };
    let remote = serde_json::to_vec(&openq::remote::RemoteConfig { instance_name: "Main".into(), ..Default::default() }).unwrap();
    let ws = serde_json::to_vec(&seed_workspace(Path::new("/nonexistent.wav"))).unwrap();
    let samples: Vec<(&str, Vec<u8>)> = vec![
        ("remote.json", remote),
        ("language", b"es".to_vec()),
        ("midi-surface", b"Midi Through:Midi Through Port-0 14:0".to_vec()),
        ("ofl-catalogue.json", br#"{"fetched":1700000000,"entries":[{"maker":"generic","key":"dimmer","name":"Dimmer","categories":["Dimmer"]}]}"#.to_vec()),
        ("recovery/session-1-1.openq", ws.clone()),
        ("recovery/session-1-1.json", br#"{"path":"/shows/a.openq","saved":1700000000}"#.to_vec()),
        ("recovery/session-1-1.lock", Vec::new()),
    ];
    let mut n = 0;
    while f.next() {
        let (name, src) = &samples[f.rng.usize(..samples.len())];
        let bytes = match f.rng.u8(..4) {
            0 => (0..f.rng.usize(..4096)).map(|_| f.rng.u8(..)).collect(),
            1 if serde_json::from_slice::<Value>(src).is_ok() => {
                let mut v: Value = serde_json::from_slice(src).unwrap();
                mutate_json(&mut f.rng, &mut v, &[]);
                serde_json::to_vec(&v).unwrap()
            }
            _ => mutate_bytes(&mut f.rng, src),
        };
        let p = dir.join(name);
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        // Sometimes the file is a directory or a pipe instead.
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir_all(&p);
        match f.rng.u8(..20) {
            0 => std::fs::create_dir_all(&p).unwrap(),
            1 => {
                let _ = std::process::Command::new("mkfifo").arg(&p).status();
            }
            _ => std::fs::write(&p, &bytes).unwrap(),
        }
        let d = dir.clone();
        f.guard(&format!("reading {name}"), Duration::from_secs(5), &bytes, "bin", move || {
            let cfg = openq::remote::RemoteConfig::load();
            let _ = cfg.save();
            let _ = openq::i18n::init();
            let _ = openq::surface::saved_midi_port();
            let _ = openq::light::oflsite::load_cache();
            for o in find_orphans_in(&d.join("recovery"), None) {
                let _ = o.name();
                let _ = o.load();
            }
        });
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir_all(&p);
        n += 1;
    }
    // Saving where it can't: missing folder, a folder in the way, a read-only folder, a pipe.
    let ws = seed_workspace(Path::new("/nonexistent.wav"));
    let ro = dir.join("read-only");
    std::fs::create_dir_all(&ro).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
    }
    let blocked = dir.join("a-folder.openq");
    std::fs::create_dir_all(&blocked).unwrap();
    for p in [dir.join("missing/dir/show.openq"), blocked.clone(), ro.join("show.openq"), PathBuf::from("/proc/version"), PathBuf::from("")] {
        let (w, q) = (ws.clone(), p.clone());
        f.guard(&format!("saving to {}", p.display()), Duration::from_secs(5), &[], "bin", move || {
            let _ = w.save(&q);
        });
    }
    let (r1, r2) = (ro.join("recovery"), blocked.clone());
    f.guard("autosave where it can't write", Duration::from_secs(5), &[], "bin", move || {
        if let Ok(mut a) = Autosave::new_in(&r1) {
            let _ = a.write(&Workspace::default(), None);
        }
        if let Ok(mut a) = Autosave::new_in(&r2.join("x")) {
            let _ = a.write(&Workspace::default(), None);
        }
        let _ = openq::errlog::ErrorLog::open_in(&r2.join("show.openq"));
    });
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700));
    }
    f.done(format!("{n} damaged config files"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ remote

fn wait_for(what: &str, secs: f64, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed().as_secs_f64() < secs, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What a replica does with each message from the main device (the same calls as the remote
/// panel in the UI).
fn apply_as_replica(show: &mut Show, synced: &mut Option<Workspace>, waveforms: &openq::media::Waveforms, msg: openq::remote::ServerMsg) {
    use openq::remote::ServerMsg;
    let now = Instant::now();
    match msg {
        ServerMsg::Workspace { mut ws, keep_lighting, .. } => {
            if keep_lighting && let Some(s) = synced {
                ws.lighting = s.lighting.clone();
            }
            show.ws = (*ws).clone();
            *synced = Some(*ws);
            show.engine.workspace_changed();
            show.sync_lighting();
        }
        ServerMsg::Cues(patch) => {
            if synced.as_mut().is_some_and(|s| openq::remote::apply_cue_patch(s, &patch)) {
                openq::remote::apply_cue_patch(&mut show.ws, &patch);
                show.engine.workspace_changed();
            }
        }
        ServerMsg::Lighting(patch) => {
            if synced.as_mut().is_some_and(|s| openq::light::patch::apply(&mut s.lighting, &patch)) {
                if !openq::light::patch::apply(&mut show.ws.lighting, &patch) {
                    show.ws.lighting = synced.as_ref().unwrap().lighting.clone();
                }
                show.sync_lighting();
            }
        }
        ServerMsg::Dmx(universes) => show.media.light.mirror_frame(&universes),
        ServerMsg::State(s) => {
            show.media.light.mirror(s.lighting.masters.clone(), s.lighting.sequences.clone());
            show.engine.mirror(s.playhead, s.panicking, &s.instances, s.log, now);
        }
        ServerMsg::MediaInfo(list) => {
            for (p, info) in list {
                show.media.prober.insert(p, info);
            }
        }
        ServerMsg::Waveform(p, w) => {
            if let Ok(w) = &w {
                let _ = w.range(0.0, w.duration);
                let _ = w.range(w.duration * 0.3, w.duration * 0.6);
            }
            waveforms.insert(p, w.map(Arc::new));
        }
        ServerMsg::Displays(list) => {
            let list: Vec<_> = list.into_iter().take(openq::displays::MAX_REMOTE).map(openq::displays::Display::cleaned).collect();
            assert!(list.iter().all(|d| d.name.chars().count() <= 64));
        }
        ServerMsg::Welcome { .. } | ServerMsg::Role(_) | ServerMsg::Refused(_) => {}
    }
    show.tick(now);
    let _ = show.engine.snapshot(now, |_| None);
    let _ = show.media.light.frame();
    let _ = show.media.light.status();
}

/// Hostile handshakes, then every message an editing replica can send (over the real encrypted
/// link), then every message a main device can send to a replica: the main device stays up and
/// keeps obeying, the replica doesn't crash.
#[test]
#[ignore = "fuzz"]
fn fuzz_remote_messages() {
    use openq::remote::client::{Auth, Link, LinkState};
    use openq::remote::secure::Hello;
    use openq::remote::server::Server;
    use openq::remote::{ClientMsg, LightLive, LiveState, RemoteConfig, Role, ServerMsg};
    use std::io::{Read, Write};
    let mut f = Fuzz::new("fuzz_remote_messages");
    let dir = tmpdir("remote");
    // SAFETY: set before the server's threads start.
    unsafe { std::env::set_var("OPENQ_CONFIG_DIR", &dir) };
    let media = dir.join("tone.wav");
    write_wav(&media, 48_000, 2, 0.5);
    let mut ws = seed_workspace(&media);
    keep_local(&mut ws);
    let show = virtual_show();
    show.lock().replace_workspace(ws.clone(), None);
    openq::show::spawn_engine_thread(show.clone(), || {});
    let port = 43000 + (std::process::id() % 10000) as u16;
    let cfg = Arc::new(Mutex::new(RemoteConfig { port, instance_name: "Main".into(), ..Default::default() }));
    let server = Server::start(show.clone(), cfg.clone(), || {}).unwrap();
    let addr = format!("127.0.0.1:{port}");
    let code = server.open_pairing();
    let client = Arc::new(Mutex::new(RemoteConfig { instance_name: "Fuzzer".into(), ..Default::default() }));
    let link = Link::connect(&addr, Auth::Pair(code), client, || {});
    wait_for("pairing", 30.0, || matches!(link.state(), LinkState::Connected { .. }));
    cfg.lock().devices[0].role = Role::Edit;
    wait_for("edit role", 10.0, || link.drain().iter().any(|m| matches!(m, ServerMsg::Role(Role::Edit))));

    // 1. Handshakes: valid hellos with hostile fields, damaged hellos, junk after the hello.
    let hello = serde_json::to_value(Hello { protocol: openq::remote::PROTOCOL, client_id: uuid::Uuid::new_v4(), client_name: "x".into(), pairing: true }).unwrap();
    let handshakes = (budget() / 6).max(Duration::from_secs(2));
    let t = Instant::now();
    let mut shakes = 0;
    while t.elapsed() < handshakes {
        let mut v = hello.clone();
        mutate_json(&mut f.rng, &mut v, &[]);
        let mut body = serde_json::to_vec(&v).unwrap();
        if f.rng.u8(..4) == 0 {
            body = mutate_bytes(&mut f.rng, &body);
        }
        let Ok(mut s) = std::net::TcpStream::connect(&addr) else { continue };
        let _ = s.set_read_timeout(Some(Duration::from_millis(300)));
        let len = if f.rng.u8(..10) == 0 { f.rng.u32(..) } else { body.len() as u32 };
        let _ = s.write_all(&len.to_be_bytes());
        let _ = s.write_all(&body);
        let mut reply = [0u8; 256];
        let _ = s.read(&mut reply);
        for _ in 0..f.rng.usize(..4) {
            let junk: Vec<u8> = (0..f.rng.usize(..300)).map(|_| f.rng.u8(..)).collect();
            let _ = s.write_all(&(junk.len() as u32).to_be_bytes());
            let _ = s.write_all(&junk);
        }
        shakes += 1;
        f.check_panics(&body);
    }
    eprintln!("{shakes} hostile handshakes");

    // 2. Everything an editor can send.
    let others = {
        let mut o = ws.clone();
        o.cue_list.cues.truncate(5);
        o
    };
    let ids: Vec<CueId> = ws.cue_list.iter_all().iter().map(|c| c.id).collect();
    let lp = openq::light::patch::diff(&Lighting::default(), &ws.lighting).map(Box::new);
    let seeds: Vec<Value> = [
        ClientMsg::Edit(Box::new(ws.clone())),
        ClientMsg::Reshape { ws: Box::new(others.clone()), lighting: lp.clone() },
        ClientMsg::Patch { cues: ws.cue_list.cues.iter().take(6).map(|c| c.shallow_clone()).collect(), lighting: lp.clone() },
        ClientMsg::Command(Command::Go),
        ClientMsg::Command(Command::Cue(ids[0], CueAction::Start)),
        ClientMsg::Command(Command::Light(LightControl::Go(1, None))),
        ClientMsg::Select(vec![1, 2, 3]),
        ClientMsg::Waveform(media.clone()),
        ClientMsg::Subscribe { dmx: true },
        ClientMsg::Resync,
    ]
    .iter()
    .map(|m| serde_json::to_value(m).unwrap())
    .collect();
    let mut pool = Vec::new();
    for s in &seeds {
        collect(s, &mut pool, 6000);
    }
    let t = Instant::now();
    let mut sent = 0;
    while t.elapsed() < budget() / 2 && f.next() {
        let mut v = seeds[f.rng.usize(..seeds.len())].clone();
        for _ in 0..f.rng.usize(1..4) {
            mutate_json(&mut f.rng, &mut v, &pool);
        }
        let Ok(msg) = serde_json::from_value::<ClientMsg>(v.clone()) else { continue };
        // Only what survives the wire: a huge literal in an f32 field becomes infinity, which
        // JSON writes as null; the main device rightly drops a link that sends such a message.
        let Ok(msg) = serde_json::to_vec(&msg).and_then(|b| serde_json::from_slice::<ClientMsg>(&b)) else { continue };
        link.send(msg);
        sent += 1;
        if sent % 20 == 0 {
            // Let the main device catch up (it answers a big edit with the whole show).
            std::thread::sleep(Duration::from_millis(20));
            let _ = link.drain();
            if !matches!(link.state(), LinkState::Connected { .. }) {
                f.fail(&serde_json::to_vec(&v).unwrap(), "json", &format!("the main device dropped the link: {:?}", link.state()));
            }
        }
        f.check_panics(&serde_json::to_vec(&v).unwrap());
    }
    // Still connected and obeying.
    let target = show.lock().ws.cue_list.first();
    link.send(ClientMsg::Command(Command::Panic));
    link.send(ClientMsg::Command(Command::SetPlayhead(target)));
    wait_for("main device obeys after the fuzz", 10.0, || show.lock().engine.playhead == target);
    assert!(show.lock().recovered.is_none(), "the main device's engine recovered from a panic");
    {
        // Whatever the replica sent, the show holds only plausible values.
        let s = show.lock();
        let mut again = s.ws.clone();
        openq::limits::workspace(&mut again);
        assert!(again == s.ws, "values out of range got into the main device's show");
    }
    eprintln!("{sent} hostile messages from an editing replica");

    // 3. Everything a main device can send to a replica.
    let replica_show = {
        let mut s = Show::new(Prober::new());
        s.replica = true;
        s
    };
    let replica = Arc::new(Mutex::new(replica_show));
    let waveforms = openq::media::Waveforms::new();
    let info = show.lock().media.prober.get_blocking(&media);
    let state = {
        let s = show.lock();
        let now = Instant::now();
        LiveState {
            playhead: s.engine.playhead,
            panicking: false,
            instances: s.engine.snapshot(now, |_| Some(0.1)),
            log: vec!["x".into()],
            meters: vec![0.5; 2],
            dirty: true,
            lighting: LightLive { masters: s.media.light.masters().clone(), sequences: s.media.light.status(), selection: vec![1] },
            overrides_off: 0b101,
        }
    };
    let seeds: Vec<Value> = [
        ServerMsg::Workspace { ws: Box::new(ws.clone()), title: "Show".into(), keep_lighting: false },
        ServerMsg::Cues(ws.cue_list.cues.iter().take(5).map(|c| c.shallow_clone()).collect()),
        ServerMsg::Dmx(vec![(1, vec![255; 512]), (2, vec![0; 512])]),
        ServerMsg::State(Box::new(state)),
        ServerMsg::MediaInfo(vec![(media.clone(), info)]),
        ServerMsg::Waveform(media.clone(), Ok(openq::media::decoder::waveform(&media).unwrap())),
        ServerMsg::Role(Role::Control),
        ServerMsg::Displays(vec![openq::displays::Display {
            name: "HDMI-A-1".into(),
            size: [1920, 1080],
            position: [0, 0],
            refresh_hz: Some(60.0),
        }]),
    ]
    .into_iter()
    .chain(lp.map(ServerMsg::Lighting))
    .map(|m| serde_json::to_value(&m).unwrap())
    .collect();
    let mut pool = Vec::new();
    for s in &seeds {
        collect(s, &mut pool, 6000);
    }
    let synced = Arc::new(Mutex::new(None));
    let wf = Arc::new(waveforms);
    let mut received = 0;
    while f.next() {
        let mut v = seeds[f.rng.usize(..seeds.len())].clone();
        for _ in 0..f.rng.usize(1..4) {
            mutate_json(&mut f.rng, &mut v, &pool);
        }
        let Ok(msg) = serde_json::from_value::<ServerMsg>(v.clone()) else { continue };
        let input = serde_json::to_vec(&v).unwrap();
        let (r, s, w) = (replica.clone(), synced.clone(), wf.clone());
        f.guard("a replica handling a message", Duration::from_secs(10), &input, "json", move || apply_as_replica(&mut r.lock(), &mut s.lock(), &w, msg));
        received += 1;
    }
    f.done(format!("{shakes} handshakes, {sent} messages to the main device, {received} to a replica"));
    show.lock().shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ network packets

/// Art-Net, sACN, OSC, MIDI and MTC packets, valid, damaged and random, to the parsers and to
/// real listeners.
#[test]
#[ignore = "fuzz"]
fn fuzz_network_packets() {
    use openq::light::dmx::{Universe, UniverseRoute};
    use openq::light::dmxin::{InputConfig, InputProtocol, InputReceiver};
    use openq::light::{artnet, sacn};
    let mut f = Fuzz::new("fuzz_network_packets");
    let mut universe = Universe::new(3);
    universe.data[0] = 255;
    let mut art = Vec::new();
    artnet::dmx_packet(1, &universe, &mut art);
    let mut sacn_packet = [0u8; sacn::PACKET_LEN];
    sacn::data_packet(&[7; 16], "fuzz", 100, 1, &universe, &mut sacn_packet);
    let replies = artnet::poll_replies(std::net::Ipv4Addr::LOCALHOST, "fuzz", &[1, 2, 3]);
    let osc = |addr: &str, args: Vec<rosc::OscType>| rosc::encoder::encode(&rosc::OscPacket::Message(rosc::OscMessage { addr: addr.into(), args })).unwrap();
    let osc_seeds = [
        osc("/go", vec![]),
        osc("/cue/1/start", vec![]),
        osc("/playhead/2", vec![]),
        osc("/light/executor/1/level", vec![rosc::OscType::Float(0.5)]),
        osc("/light/gm", vec![rosc::OscType::Double(1e300)]),
        osc("/light/tap", vec![rosc::OscType::Int(i32::MIN), rosc::OscType::String("x".repeat(1000)), rosc::OscType::Blob(vec![0; 2000])]),
    ];
    let midi_seeds: [&[u8]; 8] = [
        &[0xb0, 1, 127],
        &[0x90, 60, 100],
        &[0xf8],
        &[0xfa],
        &[0xf1, 0x23],
        &[0xf0, 0x7f, 0x7f, 0x01, 0x01, 1, 2, 3, 4, 0xf7],
        // MSC GO 1.5 in list 1, TIMED_GO.
        &[0xf0, 0x7f, 0x01, 0x02, 0x10, 0x01, b'1', b'.', b'5', 0, b'1', 0xf7],
        &[0xf0, 0x7f, 0x7f, 0x02, 0x7f, 0x04, 0x61, 0, 2, 0, 0, b'2', 0xf7],
    ];

    // Real listeners: a DMX input on its own ports, OSC on a show.
    let base = 44000 + (std::process::id() % 5000) as u16 * 2;
    let art_in = InputReceiver::start_on(InputConfig { enabled: true, protocol: InputProtocol::ArtNet, routes: vec![UniverseRoute { universe: 1, wire: 0 }], ..Default::default() }, base);
    let sacn_in = InputReceiver::start_on(InputConfig { enabled: true, protocol: InputProtocol::Sacn, routes: vec![UniverseRoute { universe: 1, wire: 3 }], ..Default::default() }, base + 1);
    let ws = Workspace { lighting: seed_lighting(), ..Default::default() };
    let show = virtual_show();
    show.lock().replace_workspace(ws, None);
    show.lock().ws.settings.surfaces.msc = openq::msc::MscSettings { enabled: true, device: 1, ..Default::default() };
    let hub = openq::surface::SurfaceHub::new();
    let osc_port = base + 2;
    let _osc = openq::osc::OscServer::start_with(osc_port, show.clone(), hub.clone(), || {}).unwrap();
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut mtc = openq::timecode::MtcDecoder::default();
    let mut clock = openq::surface::MidiClock::default();
    let mut ltc = openq::timecode::LtcDecoder::new(48_000);
    let mut n = 0u64;
    while f.next() {
        let kind = f.rng.u8(..6);
        let src: Vec<u8> = match kind {
            0 => art.clone(),
            1 => sacn_packet.to_vec(),
            2 => replies[f.rng.usize(..replies.len())].clone(),
            3 => osc_seeds[f.rng.usize(..osc_seeds.len())].clone(),
            4 => midi_seeds[f.rng.usize(..midi_seeds.len())].to_vec(),
            _ => (0..f.rng.usize(..700)).map(|_| f.rng.u8(..)).collect(),
        };
        let p = if f.rng.u8(..5) == 0 { src } else { mutate_bytes(&mut f.rng, &src) };
        // Parsers, as the receive threads call them.
        let _ = artnet::parse_dmx(&p);
        let _ = artnet::is_poll(&p);
        let _ = artnet::parse_poll_reply(&p);
        let _ = sacn::parse(&p);
        let _ = sacn::parse_data(&p);
        let _ = openq::surface::parse_midi(&p);
        if let Some(m) = openq::msc::parse(&p) {
            hub.msc(&show, &m);
        }
        let _ = clock.message(&p, n * 20_833);
        mtc.push(&p);
        let samples: Vec<f32> = p.iter().map(|b| *b as f32 / 127.5 - 1.0).collect();
        let _ = ltc.push(&samples);
        if let Ok((_, rosc::OscPacket::Message(m))) = rosc::decoder::decode_udp(&p)
            && openq::osc::sanitize(&m).is_ok()
        {
            let _ = openq::osc::handle_message(&show, &hub, &m);
        }
        // And over the wire.
        let port = match kind {
            0 | 2 => base,
            1 => base + 1,
            3 => osc_port,
            _ => [base, base + 1, osc_port][f.rng.usize(..3)],
        };
        let _ = sock.send_to(&p, ("127.0.0.1", port));
        if n.is_multiple_of(200) {
            std::thread::sleep(Duration::from_millis(2));
            let _ = art_in.handle().frame();
            let _ = sacn_in.handle().frame();
            show.lock().tick(Instant::now());
        }
        f.check_panics(&p);
        n += 1;
    }
    // The listeners still work.
    let mut good = Universe::new(0);
    good.data[0] = 77;
    let mut pkt = Vec::new();
    artnet::dmx_packet(9, &good, &mut pkt);
    wait_for("Art-Net input still listening", 10.0, || {
        let _ = sock.send_to(&pkt, ("127.0.0.1", base));
        std::thread::sleep(Duration::from_millis(20));
        art_in.handle().frame().get(1).is_some_and(|u| u.data[0] == 77)
    });
    let first = show.lock().ws.cue_list.first();
    show.lock().engine.set_playhead(None);
    wait_for("OSC still listening", 10.0, || {
        let _ = sock.send_to(&osc("/playhead/next", vec![]), ("127.0.0.1", osc_port));
        std::thread::sleep(Duration::from_millis(20));
        show.lock().engine.playhead.is_some() || first.is_none()
    });
    f.done(format!("{n} packets"));
    show.lock().shutdown();
}

// ------------------------------------------------------------------ devices

/// Devices that don't exist or aren't what they claim: every open fails with an error (or
/// falls back), nothing panics or hangs.
#[test]
#[ignore = "fuzz"]
fn fuzz_devices() {
    use openq::light::{artnet::ArtNetOutput, enttec::EnttecPro, sacn::SacnOutput};
    use openq::media::audio::Endpoint;
    use openq::model::{TimecodeRate, TimecodeSource, TimecodeSync};
    let mut f = Fuzz::new("fuzz_devices");
    let mut names: Vec<String> = vec![
        "".into(),
        "nonexistent".into(),
        "hw:99,99".into(),
        "plughw:CARD=Nope,DEV=0".into(),
        "sysdefault:CARD=".into(),
        "/dev/null".into(),
        "/dev/zero".into(),
        "/etc/passwd".into(),
        "/dev/ttyUSB99".into(),
        "/dev/ttyS0".into(),
        "COM999".into(),
        "\\\\.\\COM1".into(),
        "999.999.999.999".into(),
        "256.1.1.1:99999".into(),
        "[::1]".into(),
        "localhost:".into(),
        ":".into(),
        "127.0.0.1:0".into(),
        "attacker.invalid".into(),
    ];
    // Each name costs a few seconds (opening looks through every sound and MIDI device).
    for _ in 0..8 {
        names.push(hostile_string(&mut f.rng).chars().take(300).collect());
    }
    for name in names {
        f.iteration += 1;
        let n = name.clone();
        f.guard(&format!("opening {name:?}"), Duration::from_secs(20), name.as_bytes(), "txt", move || {
            let _ = AudioOutput::open(Some(&n));
            let _ = openq::media::input::open_input(Some(&n), |_, _| {});
            let _ = EnttecPro::open(&n);
            let _ = ArtNetOutput::open(&n, true);
            let _ = SacnOutput::open(&n, 200);
            for source in [TimecodeSource::Ltc, TimecodeSource::Mtc] {
                let sync = TimecodeSync { source, rate: TimecodeRate::Fps25, ltc_device: Some(n.clone()), ltc_channel: 9999, mtc_port: Some(n.clone()) };
                let r = openq::media::tcio::Reader::open(&sync);
                let _ = r.position(Instant::now());
            }
        });
    }
    // Odd virtual outputs.
    for (rate, channels) in [(0, 2), (1, 1), (48_000, 0), (48_000, 4096), (u32::MAX, 2), (8_000, 64)] {
        f.iteration += 1;
        f.guard(&format!("virtual output {rate} Hz × {channels}"), Duration::from_secs(10), &[], "txt", move || {
            if let Ok(mut a) = AudioOutput::open_endpoint(&Endpoint::Virtual { rate, channels }, None) {
                let id = CueId::new_v4();
                let _ = a.start(id, &AudioCue { path: "/nonexistent.wav".into(), ..Default::default() }, 2, Instant::now());
                std::thread::sleep(Duration::from_millis(50));
                a.stop(id);
            }
        });
    }
    // A MIDI surface on a port that isn't there; an OSC listener on a port that is taken.
    let show = virtual_show();
    let midi = openq::surface::MidiSurface::start("Nope:Nope 99:0".into(), show.clone(), openq::surface::SurfaceHub::new(), || {});
    std::thread::sleep(Duration::from_millis(300));
    let _ = midi.status();
    let taken = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    assert!(openq::osc::OscServer::start(port, show.clone(), || {}).is_err(), "OSC on a taken port");
    // Cues that need devices: a Mic on a missing input, a Camera on a missing device, a Network
    // cue to a host that doesn't resolve (an mDNS name takes seconds to fail: the lookup must not
    // hold up the engine).
    let mut ws = Workspace::default();
    for k in KindTag::ALL {
        let mut c = Cue::new(k.default_kind());
        match &mut c.kind {
            CueKind::Mic(m) => m.device = Some("nonexistent".into()),
            CueKind::Camera(cam) => cam.device = Some("/dev/video99".into()),
            CueKind::Network(n) => n.host = "openq-fuzz-missing-desk.local".into(),
            _ => {}
        }
        ws.cue_list.cues.push(c);
    }
    let s = show.clone();
    f.guard("cues on missing devices", Duration::from_secs(3), &[], "txt", move || {
        let mut show = s.lock();
        let ids: Vec<CueId> = ws.cue_list.cues.iter().map(|c| c.id).collect();
        show.replace_workspace(ws, None);
        let t0 = Instant::now();
        // Each cue started directly (GO stops at the Goto cue, which has no target), then GOs.
        for id in ids {
            let _ = apply(&mut show, Command::Cue(id, CueAction::Start), t0);
        }
        for i in 0..100 {
            let now = t0 + Duration::from_millis(20 * i);
            if i % 5 == 0 {
                let _ = apply(&mut show, Command::Go, now);
            }
            show.tick(now);
        }
        let _ = apply(&mut show, Command::Panic, Instant::now());
    });
    show.lock().shutdown();
    f.done("devices");
}
