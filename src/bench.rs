//! Benchmarks for comparing builds: the engine tick, cue lookups, the audio mixer, start
//! accuracy, video conversion, text rendering, waveforms, saving/loading, remote patches, the
//! UI frame and lighting playback. Run them with `tools/bench.sh` (or `cargo run --release --example bench`), which
//! saves a JSON [`Report`], compares it with the previous run in the [`History`] (or a baseline),
//! adds it to the history and regenerates the results section of `docs/performance.html`
//! ([`render_history`], [`update_page`]) with a chart per metric.
//!
//! Every benchmark is headless and deterministic in its input (fixed seeds, generated media), so
//! two runs on the same machine differ only by noise. Lower is better for every value except
//! where [`Metric::higher_is_better`] says otherwise. Timing-sensitive runs (start accuracy,
//! mixer load) use a virtual audio output, so no sound is played and no device is needed.

use crate::engine::{Backend, Engine, FadeRequest};
use crate::media::Prober;
use crate::media::audio::AudioOutput;
use crate::model::{AudioCue, ContinueMode, Cue, CueId, CueKind, Eq, GroupMode, NetworkCue, TextCue, Workspace};
use crate::show::{SharedShow, Show};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// One measured value.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metric {
    /// Stable name, `group.what` (used to match runs).
    pub name: String,
    /// The value (a median unless the name says otherwise).
    pub value: f64,
    /// Unit of `value` ("us", "ms", "ns", "samples", "%", "MB", "count").
    pub unit: String,
    /// True when a larger value is the better one (throughput).
    #[serde(default)]
    pub higher_is_better: bool,
    /// Short explanation shown next to the value.
    #[serde(default)]
    pub note: String,
    /// A worst case or other single-sample value that moves a lot between runs: shown in
    /// comparisons but never counted as a regression.
    #[serde(default)]
    pub noisy: bool,
}

/// Everything one run measured, plus where it ran.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    /// OpenQ version (`Cargo.toml`) the run measured.
    pub version: String,
    /// Git revision (with `-dirty` for uncommitted changes), from the caller.
    pub revision: String,
    /// Free-form label (e.g. "before fade rewrite").
    pub label: String,
    /// Unix time of the run.
    pub unix_time: u64,
    /// CPU model and core count.
    pub machine: String,
    /// "release" or "debug" (debug numbers are not comparable with release ones).
    pub profile: String,
    /// Cargo features the library was built with.
    pub features: Vec<String>,
    /// Whether the run used `--quick` sizes (not comparable with full runs).
    pub quick: bool,
    /// Some groups were left out (`--only`, `--no-realtime`): not kept in the [`History`].
    pub partial: bool,
    /// The measurements.
    pub metrics: Vec<Metric>,
}

/// What to run.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Smaller sizes and fewer repetitions (a quick check, not for comparisons with full runs).
    pub quick: bool,
    /// Only benchmarks whose group name contains one of these (all when empty).
    pub filter: Vec<String>,
    /// Skip the real-time benchmarks (start accuracy, mixer load), which take ~15 s.
    pub no_realtime: bool,
}

/// The benchmark groups, in the order they run.
pub const GROUPS: &[&str] = &["engine", "lookup", "mixer", "timing", "video", "text", "waveform", "workspace", "remote", "ui", "light"];

/// Run the benchmarks chosen by `opts`, printing progress to stderr.
pub fn run(opts: &Options) -> Report {
    let mut r = Report {
        unix_time: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        machine: machine(),
        profile: if cfg!(debug_assertions) { "debug" } else { "release" }.into(),
        features: features(),
        version: env!("CARGO_PKG_VERSION").into(),
        quick: opts.quick,
        partial: !opts.filter.is_empty() || opts.no_realtime,
        ..Default::default()
    };
    let wanted = |g: &str| opts.filter.is_empty() || opts.filter.iter().any(|f| g.contains(f.as_str()));
    let q = opts.quick;
    // Bring the CPU to its working clock first: without this the first group runs ~30 % slower,
    // so `--only` runs didn't match the same group inside a full run.
    let warm = Instant::now();
    let mut spin = 0u64;
    while warm.elapsed() < Duration::from_secs(1) {
        spin = std::hint::black_box(spin.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
    }
    for g in GROUPS {
        if !wanted(g) || (opts.no_realtime && matches!(*g, "mixer" | "timing")) {
            continue;
        }
        eprintln!("bench: {g}…");
        let t = Instant::now();
        let m = &mut r.metrics;
        match *g {
            "engine" => engine(m, q),
            "lookup" => lookup(m, q),
            "mixer" => mixer(m, q),
            "timing" => timing(m, q),
            "video" => video(m, q),
            "text" => text(m, q),
            "waveform" => waveform(m, q),
            "workspace" => workspace(m, q),
            "remote" => remote(m, q),
            "ui" => ui(m, q),
            "light" => light(m, q),
            _ => {}
        }
        eprintln!("bench: {g} done in {:.1} s", t.elapsed().as_secs_f64());
    }
    r
}

// ------------------------------------------------------------------ lighting

/// Lighting playback: a frame with every sequence fading, and compiling the design.
fn light(m: &mut Vec<Metric>, quick: bool) {
    use crate::light::playback::{LightShow, Playback};
    let (fixtures, cues) = if quick { (500, 20) } else { (2000, 100) };
    let design = crate::light::library::bench_rig(fixtures, 3, cues);
    let t = Instant::now();
    let show = Arc::new(LightShow::compile(&design));
    push(m, &format!("light.compile_{fixtures}"), t.elapsed().as_secs_f64() * 1e3, "ms", "compile the design (off the light thread)");
    let t0 = Instant::now();
    let mut p = Playback::new(show, t0);
    for s in 1..=3 {
        p.go(s, None, t0).expect("go");
    }
    // Every slot of every sequence fading over 10 s; time one frame per 22.7 ms step.
    let reps = if quick { 200 } else { 1000 };
    let mut v: Vec<Duration> = (0..reps)
        .map(|i| {
            let now = t0 + Duration::from_secs_f64(i as f64 * 0.0227 % 10.0);
            let t = Instant::now();
            std::hint::black_box(p.tick(now));
            t.elapsed()
        })
        .collect();
    let universes = p.frame().universes.len();
    let note = format!("{fixtures} fixtures, {universes} universes, 3 sequences fading");
    push(m, &format!("light.frame_{fixtures}"), quantile(&mut v, 0.5).as_secs_f64() * 1e6, "us", &note);
    push(m, &format!("light.frame_{fixtures}.p99"), quantile(&mut v, 0.99).as_secs_f64() * 1e6, "us", &note);
    light_effects(m, design.clone(), reps, &note);
    light_table(m, design, reps, &note);
    light_cells(m, quick, reps);
}

/// A pixel rig: LED bars of 32 RGB cells (plus a master dimmer each), every cell lit by a cue
/// and a chase running across all cells.
#[inline(never)]
fn light_cells(m: &mut Vec<Metric>, quick: bool, reps: usize) {
    use crate::light::effects::{Effect, EffectRun, Shape};
    use crate::light::model::{cell_id, Attribute, ChannelDef, Fixture, FixtureMode, FixtureType, Head, LightCue, Sequence};
    use crate::light::playback::{LightShow, Playback};
    let bars: u32 = if quick { 50 } else { 200 };
    let rgb = [Attribute::Red, Attribute::Green, Attribute::Blue];
    let heads = (0..32u16)
        .map(|i| Head { name: (i + 1).to_string(), channels: rgb.iter().enumerate().map(|(k, a)| ChannelDef { attribute: *a, offset: 1 + 3 * i + k as u16, ..Default::default() }).collect() })
        .collect();
    let mut mode = FixtureMode::sequential("97 channel", &[(Attribute::Dimmer, false)]);
    mode.heads = heads;
    let ty = FixtureType { id: "bench/bar32".into(), name: "Bar 32".into(), modes: vec![mode], ..Default::default() };
    let mut l = crate::light::Lighting::default();
    l.add_type(&ty);
    for i in 0..bars {
        // Five 97-slot bars per universe.
        l.fixtures.push(Fixture { id: i + 1, fixture_type: ty.id.clone(), mode: "97 channel".into(), universe: 1 + (i / 5) as u16, address: 1 + 97 * (i % 5) as u16, ..Default::default() });
    }
    let cells: Vec<u32> = (1..=bars).flat_map(|f| (1..=32).map(move |c| cell_id(f, c))).collect();
    let mut cue = LightCue { fade: 10.0, ..Default::default() };
    for (k, c) in cells.iter().enumerate() {
        let v = cue.values.entry(*c).or_default();
        for (j, a) in rgb.iter().enumerate() {
            v.insert(*a, (((k + 7 * j) % 100) as f32 / 100.0).into());
        }
    }
    for f in 1..=bars {
        cue.values.entry(f).or_default().insert(Attribute::Dimmer, 1.0.into());
    }
    l.effects.push(Effect { id: 1, shape: Shape::Square, ..Default::default() });
    cue.effects.push(EffectRun { effect: 1, fixtures: cells.clone(), stop: false });
    l.sequences.push(Sequence { id: 1, cues: vec![cue].into(), ..Default::default() });
    let t = Instant::now();
    let show = Arc::new(LightShow::compile(&l));
    let n = cells.len();
    push(m, &format!("light.compile_cells_{n}"), t.elapsed().as_secs_f64() * 1e3, "ms", &format!("{bars} bars × 32 RGB cells"));
    let t0 = Instant::now();
    let mut p = Playback::new(show, t0);
    p.go(1, None, t0).expect("go");
    let mut v: Vec<Duration> = (0..reps)
        .map(|i| {
            let now = t0 + Duration::from_secs_f64(i as f64 * 0.0227 % 10.0);
            let t = Instant::now();
            std::hint::black_box(p.tick(now));
            t.elapsed()
        })
        .collect();
    let universes = p.frame().universes.len();
    let note = format!("{bars} bars × 32 RGB cells, {universes} universes, fading + a chase on every cell");
    push(m, &format!("light.frame_cells_{n}"), quantile(&mut v, 0.5).as_secs_f64() * 1e6, "us", &note);
    push(m, &format!("light.frame_cells_{n}.p99"), quantile(&mut v, 0.99).as_secs_f64() * 1e6, "us", &note);
}

/// The frame with a generated colour table (what effect formulas make: red/green/blue, 64 steps,
/// 8 pattern rows) running on every fixture.
#[inline(never)]
fn light_table(m: &mut Vec<Metric>, mut design: crate::light::Lighting, reps: usize, note: &str) {
    use crate::light::effects::{Effect, EffectRun, EffectTable, Shape};
    use crate::light::playback::{LightShow, Playback};
    let fixtures = design.fixtures.len();
    let t0 = Instant::now();
    let attributes = vec![crate::light::Attribute::Red, crate::light::Attribute::Green, crate::light::Attribute::Blue];
    let values = (0..3 * 64 * 8).map(|i| (i % 11) as f32 / 11.0).collect();
    let table = EffectTable { attributes, steps: 64, width: 8, smooth: true, values };
    design.effects.push(Effect { id: 1, shape: Shape::Custom, table: Some(Arc::new(table)), ..Default::default() });
    let all: Vec<u32> = design.fixtures.iter().map(|f| f.id).collect();
    design.sequences[0].cues[0].effects.push(EffectRun { effect: 1, fixtures: all, stop: false });
    let mut p = Playback::new(Arc::new(LightShow::compile(&design)), t0);
    for s in 1..=3 {
        p.go(s, None, t0).expect("go");
    }
    let mut v: Vec<Duration> = (0..reps)
        .map(|i| {
            let now = t0 + Duration::from_secs_f64(i as f64 * 0.0227 % 10.0);
            let t = Instant::now();
            std::hint::black_box(p.tick(now));
            t.elapsed()
        })
        .collect();
    let note = format!("{note}, a colour table effect on every fixture");
    push(m, &format!("light.frame_table_{fixtures}"), quantile(&mut v, 0.5).as_secs_f64() * 1e6, "us", &note);
}

/// The same frame with two effects running on every fixture (a chase and a pan/tilt circle).
/// Separate from [`light`]: code added there moved its timings (layout), not just these.
#[inline(never)]
fn light_effects(m: &mut Vec<Metric>, mut design: crate::light::Lighting, reps: usize, note: &str) {
    use crate::light::effects::{Effect, EffectRun, Shape};
    use crate::light::playback::{LightShow, Playback};
    let fixtures = design.fixtures.len();
    let t0 = Instant::now();
    design.effects.push(Effect { id: 1, shape: Shape::Square, ..Default::default() });
    design.effects.push(Effect { id: 2, shape: Shape::Circle, attribute: crate::light::Attribute::Pan, relative: true, size: 0.2, ..Default::default() });
    let all: Vec<u32> = design.fixtures.iter().map(|f| f.id).collect();
    design.sequences[0].cues[0].effects.push(EffectRun { effect: 1, fixtures: all.clone(), stop: false });
    design.sequences[1].cues[0].effects.push(EffectRun { effect: 2, fixtures: all, stop: false });
    let mut p = Playback::new(Arc::new(LightShow::compile(&design)), t0);
    for s in 1..=3 {
        p.go(s, None, t0).expect("go");
    }
    let mut v: Vec<Duration> = (0..reps)
        .map(|i| {
            let now = t0 + Duration::from_secs_f64(i as f64 * 0.0227 % 10.0);
            let t = Instant::now();
            std::hint::black_box(p.tick(now));
            t.elapsed()
        })
        .collect();
    let note = format!("{note}, 2 effects on every fixture");
    push(m, &format!("light.frame_fx_{fixtures}"), quantile(&mut v, 0.5).as_secs_f64() * 1e6, "us", &note);
}

// ------------------------------------------------------------------ helpers

fn machine() -> String {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("model name").map(|v| v.trim_start_matches([' ', '\t', ':']).to_string())))
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
    format!("{cpu} ({cores} threads, {})", std::env::consts::OS)
}

fn features() -> Vec<String> {
    let mut f = Vec::new();
    if cfg!(feature = "video") {
        f.push("video".into());
    }
    if cfg!(feature = "pipewire") {
        f.push("pipewire".into());
    }
    if cfg!(feature = "fault-injection") {
        f.push("fault-injection".into());
    }
    f
}

fn push(m: &mut Vec<Metric>, name: &str, value: f64, unit: &str, note: &str) {
    // Worst cases depend on what else the machine did at that moment.
    let noisy = [".max", ".p99", "peak_", "lock_wait", "max_gap", "go_to_sound", "tick_late"].iter().any(|k| name.contains(k));
    m.push(Metric { name: name.into(), value, unit: unit.into(), higher_is_better: false, note: note.into(), noisy });
}

/// Value at quantile `p` (0..=1) of `v` (sorted here).
fn quantile(v: &mut [Duration], p: f64) -> Duration {
    if v.is_empty() {
        return Duration::ZERO;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Median over `reps` runs of `f`, in the time per call (`f` returns how many calls it made).
fn per_call(reps: usize, mut f: impl FnMut() -> usize) -> Duration {
    let mut v: Vec<Duration> = (0..reps)
        .map(|_| {
            let t = Instant::now();
            let n = f().max(1);
            t.elapsed() / n as u32
        })
        .collect();
    quantile(&mut v, 0.5)
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("openq-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::create_dir_all(&d);
    d
}

/// 16-bit PCM WAV.
fn write_wav(path: &Path, rate: u32, channels: u16, samples: &[f32]) -> std::io::Result<()> {
    let data: Vec<u8> = samples.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
    let mut b = Vec::with_capacity(44 + data.len());
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    b.extend_from_slice(&(channels * 2).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(&data);
    std::fs::write(path, b)
}

/// A stereo 48 kHz tone of `secs` seconds.
fn tone(path: &Path, secs: f64) -> std::io::Result<()> {
    let n = (48_000.0 * secs) as usize;
    let s: Vec<f32> = (0..n)
        .flat_map(|i| {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.2;
            [v, v * 0.5]
        })
        .collect();
    write_wav(path, 48_000, 2, &s)
}

/// A show whose audio goes to a virtual 48 kHz stereo output (optionally recorded).
fn virtual_show(capture: Option<rtrb::Producer<f32>>) -> Option<SharedShow> {
    let mut show = Show::new(Prober::new());
    show.media.audio = Some(AudioOutput::open_virtual(48_000, 2, capture).ok()?);
    show.media.audio_error = None;
    Some(Arc::new(Mutex::new(show)))
}

/// Install `ws` keeping the virtual output (the workspace asks for the default device, which is
/// what the virtual output was opened as).
fn install(show: &SharedShow, mut ws: Workspace) {
    ws.settings.double_go_protection = 0.0;
    show.lock().replace_workspace(ws, None);
}

/// Ticks `show` every millisecond like the engine thread, until dropped.
struct Ticker {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Ticker {
    fn start(show: &SharedShow) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (show, stop2) = (show.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut next = Instant::now();
            while !stop2.load(Ordering::Relaxed) {
                {
                    let mut s = show.lock();
                    let now = Instant::now();
                    s.guarded_tick(now);
                    let st = &mut s.stats;
                    st.ticks += 1;
                    st.max_tick = st.max_tick.max(now.elapsed());
                }
                next += Duration::from_millis(1);
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    let gap = now - next + Duration::from_millis(1);
                    let mut s = show.lock();
                    s.stats.max_gap = s.stats.max_gap.max(gap);
                    if gap > crate::show::LATE_TICK {
                        s.stats.late += 1;
                    }
                    next = now;
                }
            }
        });
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Backend that plays nothing: media cues last as long as their end time says.
#[derive(Default)]
struct NullBackend {
    now: Option<Instant>,
    playing: HashMap<CueId, (Instant, Option<f64>)>,
}

impl Backend for NullBackend {
    fn set_preloaded(&mut self, _cues: &[&Cue]) {}
    fn start(&mut self, cue: &Cue, at: Instant) -> Result<(), String> {
        let d = self.duration(cue);
        self.playing.insert(cue.id, (at, d));
        Ok(())
    }
    fn stop(&mut self, id: CueId) {
        self.playing.remove(&id);
    }
    fn pause(&mut self, _id: CueId) {}
    fn resume(&mut self, _id: CueId, _at: Instant) {}
    fn fade(&mut self, _id: CueId, _f: &FadeRequest, _at: Instant) {}
    fn is_finished(&self, id: CueId) -> bool {
        match (self.playing.get(&id), self.now) {
            (None, _) => true,
            (Some((at, Some(d))), Some(now)) => now >= *at + Duration::from_secs_f64(*d),
            _ => false,
        }
    }
    fn duration(&self, cue: &Cue) -> Option<f64> {
        match &cue.kind {
            CueKind::Audio(a) => a.end_time,
            _ => None,
        }
    }
    fn update(&mut self, now: Instant) {
        self.now = Some(now);
    }
    fn devamp(&mut self, id: CueId) -> bool {
        self.playing.contains_key(&id)
    }
    fn send_network(&mut self, _n: &NetworkCue) -> Result<(), String> {
        Ok(())
    }
}

/// A list of `n` cues of mixed kinds (memos, waits, audio, fades, groups of 4) with names.
fn mixed_workspace(n: usize) -> Workspace {
    let mut rng = fastrand::Rng::with_seed(7);
    let mut ws = Workspace::default();
    let mut ids = Vec::new();
    for i in 0..n {
        let kind = match i % 5 {
            0 => CueKind::Memo,
            1 => CueKind::Wait { duration: rng.f64() * 5.0 },
            2 => CueKind::Audio(AudioCue { path: format!("/media/show/sound-{i}.wav").into(), volume_db: -6.0, ..Default::default() }),
            3 => CueKind::Fade(crate::model::FadeCue { target: ids.last().copied(), duration: 2.0, volume_db: Some(-20.0), ..Default::default() }),
            _ => CueKind::Group {
                mode: GroupMode::StartAll,
                children: (0..4).map(|_| Cue::new(CueKind::Wait { duration: 1.0 })).collect(),
            },
        };
        let mut c = Cue::new(kind);
        c.number = i.to_string();
        c.name = format!("Cue number {i} with a name of usual length");
        ids.push(c.id);
        ws.cue_list.cues.push(c);
    }
    ws
}

// ------------------------------------------------------------------ engine

fn engine(m: &mut Vec<Metric>, quick: bool) {
    // A long list with a few cues running: the common case during a show.
    let n = if quick { 2_000 } else { 10_000 };
    let mut ws = Workspace::default();
    for i in 0..n {
        let mut c = Cue::new(CueKind::Wait { duration: 600.0 });
        c.number = i.to_string();
        ws.cue_list.cues.push(c);
    }
    let mut e = Engine::new();
    let mut be = NullBackend::default();
    let t0 = Instant::now();
    for c in &ws.cue_list.cues[n - 4..] {
        e.start_cue(&ws, &mut be, c.id, t0);
    }
    let mut now = t0;
    let mut ticks: Vec<Duration> = (0..2000)
        .map(|_| {
            now += Duration::from_millis(1);
            let t = Instant::now();
            e.tick(&ws, &mut be, now);
            t.elapsed()
        })
        .collect();
    push(m, "engine.tick_long_list.p50", us(quantile(&mut ticks, 0.5)), "us", &format!("{n} cues, 4 running (budget: 1000 us per tick)"));
    push(m, "engine.tick_long_list.p99", us(quantile(&mut ticks, 0.99)), "us", "same, 99th percentile");

    // Thousands running at once: a chain of short auto-follow waits plus a StartAll group of
    // waits and media.
    let n = if quick { 500 } else { 2_000 };
    let mut rng = fastrand::Rng::with_seed(1);
    let mut ws = Workspace::default();
    let mut chain: Vec<Cue> = (0..2 * n)
        .map(|_| {
            let mut c = Cue::new(CueKind::Wait { duration: 0.001 });
            c.continue_mode = ContinueMode::AutoFollow;
            c
        })
        .collect();
    if let Some(c) = chain.last_mut() {
        c.continue_mode = ContinueMode::DoNotContinue;
    }
    let children = (0..n)
        .map(|i| {
            Cue::new(if i % 2 == 0 {
                CueKind::Wait { duration: rng.f64() * 2.0 }
            } else {
                CueKind::Audio(AudioCue { end_time: Some(rng.f64() * 2.0), ..Default::default() })
            })
        })
        .collect();
    let group = Cue::new(CueKind::Group { mode: GroupMode::StartAll, children });
    let (first, gid) = (chain[0].id, group.id);
    ws.cue_list.cues = chain;
    ws.cue_list.cues.push(group);
    let mut e = Engine::new();
    let mut be = NullBackend::default();
    let t0 = Instant::now();
    let mut now = t0;
    e.start_cue(&ws, &mut be, first, now);
    e.start_cue(&ws, &mut be, gid, now);
    let mut ticks = Vec::new();
    while e.is_active() && now - t0 < Duration::from_secs(60) {
        now += Duration::from_millis(1);
        let t = Instant::now();
        e.tick(&ws, &mut be, now);
        ticks.push(t.elapsed());
    }
    push(m, "engine.tick_many_running.p50", us(quantile(&mut ticks, 0.5)), "us", &format!("{n} cues running at once"));
    push(m, "engine.tick_many_running.p99", us(quantile(&mut ticks, 0.99)), "us", "same, 99th percentile");
    push(m, "engine.tick_many_running.max", us(quantile(&mut ticks, 1.0)), "us", "same, worst tick");

    // GO through a mixed list (fires, preloads, completes), one tick after each GO.
    let n = if quick { 1_000 } else { 5_000 };
    let ws = mixed_workspace(n);
    let mut e = Engine::new();
    let mut be = NullBackend::default();
    e.set_playhead(ws.cue_list.first());
    let mut now = Instant::now();
    let mut gos: Vec<Duration> = (0..500.min(n))
        .map(|_| {
            now += Duration::from_millis(50);
            let t = Instant::now();
            e.go(&ws, &mut be, now);
            e.tick(&ws, &mut be, now);
            t.elapsed()
        })
        .collect();
    push(m, "engine.go.p50", us(quantile(&mut gos, 0.5)), "us", &format!("GO + tick in a {n}-cue mixed list"));
    push(m, "engine.go.p99", us(quantile(&mut gos, 0.99)), "us", "same, 99th percentile");
}

// ------------------------------------------------------------------ lookups

fn lookup(m: &mut Vec<Metric>, quick: bool) {
    let n = if quick { 2_000 } else { 10_000 };
    let ws = mixed_workspace(n);
    let ids: Vec<CueId> = ws.cue_list.iter_all().iter().map(|c| c.id).collect();
    let mut rng = fastrand::Rng::with_seed(3);
    let probe: Vec<CueId> = (0..10_000).map(|_| ids[rng.usize(0..ids.len())]).collect();
    ws.cue_list.find(ids[0]); // build the index
    let t = per_call(5, || {
        for id in &probe {
            std::hint::black_box(ws.cue_list.find(*id));
        }
        probe.len()
    });
    push(m, "lookup.find", t.as_nanos() as f64, "ns", &format!("cue by id in {} cues (cached path)", ids.len()));
    let t = per_call(5, || {
        for id in &probe[..2000] {
            std::hint::black_box(ws.cue_list.next_sibling(*id));
        }
        2000
    });
    push(m, "lookup.next_sibling", t.as_nanos() as f64, "ns", "next cue after one, same list");
    let missing = CueId::new_v4();
    let t = per_call(5, || {
        std::hint::black_box(ws.cue_list.find(missing));
        1
    });
    push(m, "lookup.find_missing", us(t), "us", "id not in the list (full walk)");
    let t = per_call(5, || {
        std::hint::black_box(ws.cue_list.iter_all());
        1
    });
    push(m, "lookup.iter_all", us(t), "us", "flatten every cue (UI menus, remote)");
}

// ------------------------------------------------------------------ mixer

fn mixer(m: &mut Vec<Metric>, quick: bool) {
    let dir = tmpdir("mixer");
    let file = dir.join("tone.wav");
    if tone(&file, 10.0).is_err() {
        return;
    }
    for (voices, eq) in [(16usize, false), (64, false), (64, true), (128, true)] {
        if quick && voices > 64 {
            continue;
        }
        let Some(show) = virtual_show(None) else { return };
        let mut ws = Workspace::default();
        let mut eq_on = Eq { enabled: eq, ..Default::default() };
        for (i, b) in eq_on.bands.iter_mut().enumerate() {
            b.enabled = eq;
            b.gain_db = if i % 2 == 0 { 3.0 } else { -3.0 };
        }
        let children = (0..voices)
            .map(|i| {
                Cue::new(CueKind::Audio(AudioCue {
                    path: file.clone(),
                    volume_db: -30.0,
                    pan: (i as f32 / voices as f32) * 2.0 - 1.0,
                    infinite_loop: true,
                    eq: eq_on.clone(),
                    ..Default::default()
                }))
            })
            .collect();
        ws.cue_list.cues.push(Cue::new(CueKind::Group { mode: GroupMode::StartAll, children }));
        install(&show, ws);
        // Probe first (a GO on an unprobed file waits for the probe).
        let prober = show.lock().media.prober.clone();
        let t = Instant::now();
        while prober.get(&file).is_none() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let ticker = Ticker::start(&show);
        let _ = crate::control::apply(&mut show.lock(), crate::control::Command::Go, Instant::now());
        std::thread::sleep(Duration::from_millis(800));
        let load = |s: &SharedShow| s.lock().media.audio.as_ref().map(|a| a.dsp_load()).unwrap_or_default();
        let a = load(&show);
        std::thread::sleep(Duration::from_secs(if quick { 1 } else { 3 }));
        let b = load(&show);
        drop(ticker);
        show.lock().shutdown();
        let tag = format!("{voices}{}", if eq { "_eq" } else { "" });
        let note = format!("{voices} looping stereo voices{}, 48 kHz, 256-frame buffers", if eq { " with 5-band EQ" } else { "" });
        push(m, &format!("mixer.load_{tag}"), b.since(&a) * 100.0, "%", &format!("{note}: average audio-thread load"));
        push(m, &format!("mixer.peak_{tag}"), b.peak * 100.0, "%", "worst single callback (100% = dropout)");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ timing

/// Start accuracy: a chain of clicks 250 ms apart into the virtual output while another thread
/// grabs the show lock for 0–12 ms at a time (like a busy UI). Ideal: exactly 12 000 samples apart.
fn timing(m: &mut Vec<Metric>, quick: bool) {
    let dir = tmpdir("timing");
    let click = dir.join("click.wav");
    let mut s = vec![0.0f32; 4800];
    s[..8].fill(0.9);
    if write_wav(&click, 48_000, 1, &s).is_err() {
        return;
    }
    let count = if quick { 12 } else { 32 };
    let mut ws = Workspace::default();
    for i in 0..count {
        let mut c = Cue::new(CueKind::Audio(AudioCue { path: click.clone(), ..Default::default() }));
        c.number = i.to_string();
        c.continue_mode = ContinueMode::AutoContinue;
        c.post_wait = 0.25;
        ws.cue_list.cues.push(c);
    }
    let secs = count as f64 * 0.25 + 1.5;
    let (tx, mut rx) = rtrb::RingBuffer::<f32>::new((48_000.0 * 2.0 * (secs + 3.0)) as usize);
    let Some(show) = virtual_show(Some(tx)) else { return };
    install(&show, ws);
    let prober = show.lock().media.prober.clone();
    let t = Instant::now();
    while prober.get(&click).is_none() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let ticker = Ticker::start(&show);
    let done = Arc::new(AtomicBool::new(false));
    let load = {
        let (show, done) = (show.clone(), done.clone());
        std::thread::spawn(move || {
            let mut rng = fastrand::Rng::with_seed(3);
            while !done.load(Ordering::Relaxed) {
                let g = show.lock();
                let until = Instant::now() + Duration::from_micros(rng.u64(0..12_000));
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
                drop(g);
                std::thread::sleep(Duration::from_millis(4));
            }
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let _ = crate::control::apply(&mut show.lock(), crate::control::Command::Go, Instant::now());
    std::thread::sleep(Duration::from_secs_f64(secs));
    done.store(true, Ordering::Relaxed);
    let _ = load.join();
    drop(ticker);
    let stats = show.lock().stats;
    show.lock().shutdown();
    let mut onsets = Vec::new();
    let (mut quiet, mut frame) = (0usize, 0usize);
    while let Ok(l) = rx.pop() {
        let _ = rx.pop();
        if l.abs() > 0.3 {
            if quiet > 1000 {
                onsets.push(frame);
            }
            quiet = 0;
        } else {
            quiet += 1;
        }
        frame += 1;
    }
    let _ = std::fs::remove_dir_all(&dir);
    if onsets.len() < 3 {
        push(m, "timing.clicks_heard", onsets.len() as f64, "count", &format!("expected {count}: the run failed"));
        return;
    }
    let errs: Vec<i64> = onsets[1..].iter().enumerate().map(|(i, f)| (*f - onsets[1]) as i64 - i as i64 * 12_000).collect();
    let worst = errs.iter().map(|e| e.abs()).max().unwrap_or(0);
    let mean = errs.iter().map(|e| e.abs()).sum::<i64>() as f64 / errs.len() as f64;
    push(m, "timing.start_error_worst", worst as f64, "samples", &format!("{count} chained starts, show lock held 0–12 ms by another thread (48 = 1 ms)"));
    push(m, "timing.start_error_mean", mean, "samples", "same, mean absolute error");
    push(m, "timing.go_to_sound", ((onsets[1] - onsets[0]) as i64 - 12_000).abs() as f64, "samples", "first (unscheduled) GO vs its follower");
    push(m, "timing.tick_max_gap", ms(stats.max_gap), "ms", "longest engine-tick gap under that lock load");
    push(m, "timing.tick_late", stats.late as f64, "count", "ticks later than 5 ms");
}

// ------------------------------------------------------------------ video

#[cfg(feature = "video")]
fn video(m: &mut Vec<Metric>, quick: bool) {
    use ffmpeg_next::format::Pixel;
    let n = if quick { 20 } else { 60 };
    for (name, w, h, fmt) in [("1080p_yuv420", 1920, 1080, Pixel::YUV420P), ("4k_yuv420", 3840, 2160, Pixel::YUV420P), ("1080p_yuva420", 1920, 1080, Pixel::YUVA420P)] {
        match crate::media::video::conversion_times(w, h, fmt, n) {
            Ok(mut v) => {
                let budget = if h > 1080 { "" } else { " (budget at 60 fps: 16.7 ms)" };
                push(m, &format!("video.convert_{name}"), ms(quantile(&mut v[2..], 0.5)), "ms", &format!("{w}x{h} decode-thread frame preparation + publish (YUV goes to the GPU as planes, no conversion){budget}"));
            }
            Err(e) => eprintln!("bench: video conversion unavailable: {e:#}"),
        }
    }
}

#[cfg(not(feature = "video"))]
fn video(_m: &mut Vec<Metric>, _quick: bool) {}

// ------------------------------------------------------------------ text

fn text(m: &mut Vec<Metric>, quick: bool) {
    crate::media::text::preload();
    let reps = if quick { 5 } else { 15 };
    let cue = |s: &str, size: f32| TextCue {
        text: s.into(),
        size,
        outline_width: 4.0,
        shadow_color: [0, 0, 0, 200],
        bg_color: [20, 30, 60, 220],
        ..Default::default()
    };
    let short = cue("Act One", 96.0);
    let long = cue(&"The quick brown fox jumps over the lazy dog. ".repeat(6), 48.0);
    let _ = crate::media::text::render_planes(&short); // fonts loaded
    let t = per_call(reps, || {
        std::hint::black_box(crate::media::text::render_planes(&short));
        1
    });
    push(m, "text.render_title", ms(t), "ms", "96 px title with outline, shadow and box (text worker)");
    let t = per_call(reps, || {
        std::hint::black_box(crate::media::text::render_planes(&long));
        1
    });
    push(m, "text.render_paragraph", ms(t), "ms", "six-line paragraph, same style");
}

// ------------------------------------------------------------------ waveform

fn waveform(m: &mut Vec<Metric>, quick: bool) {
    let dir = tmpdir("waveform");
    let file = dir.join("long.wav");
    let secs = if quick { 60.0 } else { 300.0 };
    if tone(&file, secs).is_err() {
        return;
    }
    let t = per_call(if quick { 1 } else { 3 }, || {
        std::hint::black_box(crate::media::decoder::probe(&file).ok());
        1
    });
    push(m, "waveform.probe", ms(t), "ms", "read a WAV's header (Prober worker)");
    let t0 = Instant::now();
    let w = crate::media::decoder::waveform(&file);
    let build = t0.elapsed();
    push(m, "waveform.build", ms(build), "ms", &format!("decode {secs:.0} s of stereo audio into peaks (waveform worker)"));
    if let Ok(w) = w {
        // One redraw of an 800 px waveform view of the whole file.
        let _ = w.range(0.0, 1.0);
        let t = per_call(20, || {
            for x in 0..800 {
                let a = secs * x as f64 / 800.0;
                std::hint::black_box(w.range(a, a + secs / 800.0));
            }
            1
        });
        push(m, "waveform.view_800px", us(t), "us", "min/max for 800 columns over the whole file (UI frame)");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ workspace

fn workspace(m: &mut Vec<Metric>, quick: bool) {
    let n = if quick { 1_000 } else { 5_000 };
    let ws = mixed_workspace(n);
    let dir = tmpdir("workspace");
    let path = dir.join("bench.openq");
    let t = per_call(3, || {
        let _ = ws.save(&path);
        1
    });
    push(m, "workspace.save", ms(t), "ms", &format!("{n} cues to JSON, fsync, rename (Save, autosave)"));
    let size = std::fs::metadata(&path).map_or(0, |md| md.len());
    push(m, "workspace.file_size", size as f64 / 1e6, "MB", "same, file size");
    let t = per_call(3, || {
        std::hint::black_box(Workspace::load(&path).ok());
        1
    });
    push(m, "workspace.load", ms(t), "ms", "read it back");
    let t = per_call(10, || {
        std::hint::black_box(ws.clone());
        1
    });
    push(m, "workspace.clone", ms(t), "ms", "copy the workspace (one undo step, remote snapshot)");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ remote

fn remote(m: &mut Vec<Metric>, quick: bool) {
    let n = if quick { 1_000 } else { 5_000 };
    let old = mixed_workspace(n);
    let mut new = old.clone();
    new.cue_list.cues[n / 2].name = "Edited".into();
    let t = per_call(10, || {
        std::hint::black_box(crate::remote::cue_patch(&old, &new));
        1
    });
    push(m, "remote.cue_patch", ms(t), "ms", &format!("find the one edited cue among {n} (sent to replicas per edit)"));
    let t = per_call(5, || {
        std::hint::black_box(serde_json::to_vec(&new).map(|v| v.len()).unwrap_or(0));
        1
    });
    push(m, "remote.full_snapshot", ms(t), "ms", "serialise the whole workspace (first sync, resync)");
}

// ------------------------------------------------------------------ ui

fn ui(m: &mut Vec<Metric>, quick: bool) {
    let frames = if quick { 15 } else { 40 };
    for (cues, settings) in [(20usize, false), (3000, false), (3000, true)] {
        let mut r = crate::ui::bench::frame_times(cues, settings, frames);
        let tab = if settings { "settings" } else { "basics" };
        let note = format!("headless frame (layout + tessellation, no GPU), {cues} cues, {tab} tab");
        push(m, &format!("ui.frame_{cues}_{tab}"), ms(quantile(&mut r.frames, 0.5)), "ms", &note);
        push(m, &format!("ui.lock_wait_{cues}_{tab}"), ms(r.worst_lock_wait), "ms", "longest the engine waited for the show lock meanwhile");
    }
}

// ------------------------------------------------------------------ comparison

/// One metric of a comparison.
#[derive(Clone, Debug)]
pub struct Change {
    /// Metric name.
    pub name: String,
    /// Baseline value (None: new metric).
    pub old: Option<f64>,
    /// This run's value (None: metric gone).
    pub new: Option<f64>,
    /// Unit.
    pub unit: String,
    /// Relative change, positive = worse.
    pub worse_by: f64,
    /// Beyond the tolerance in the bad direction.
    pub regression: bool,
    /// Beyond the tolerance in the good direction.
    pub improvement: bool,
    /// A noisy metric ([`Metric::noisy`]): `regression` is then only a hint.
    pub noisy: bool,
}

/// Compare `new` with `old`. A metric regresses when it is worse by more than `tolerance`
/// (0.15 = 15 %) and by more than a small absolute floor for its unit (so 2 → 3 ns isn't
/// reported as +50 %).
pub fn compare(old: &Report, new: &Report, tolerance: f64) -> Vec<Change> {
    let floor = |unit: &str| match unit {
        "ns" => 5.0,
        "us" => 2.0,
        "ms" => 0.05,
        "samples" => 2.0,
        "%" => 0.5,
        "count" => 1.0,
        _ => 0.0,
    };
    let mut out = Vec::new();
    for n in &new.metrics {
        let o = old.metrics.iter().find(|o| o.name == n.name);
        let (worse_by, regression, improvement) = match o {
            Some(o) => {
                let diff = if n.higher_is_better { o.value - n.value } else { n.value - o.value };
                let rel = if o.value.abs() > f64::EPSILON { diff / o.value.abs() } else if diff > 0.0 { 1.0 } else { 0.0 };
                let big = diff.abs() > floor(&n.unit);
                (rel, big && rel > tolerance, big && rel < -tolerance)
            }
            None => (0.0, false, false),
        };
        out.push(Change {
            name: n.name.clone(),
            old: o.map(|o| o.value),
            new: Some(n.value),
            unit: n.unit.clone(),
            worse_by,
            regression,
            improvement,
            noisy: n.noisy,
        });
    }
    for o in &old.metrics {
        if !new.metrics.iter().any(|n| n.name == o.name) {
            out.push(Change {
                name: o.name.clone(),
                old: Some(o.value),
                new: None,
                unit: o.unit.clone(),
                worse_by: 0.0,
                regression: false,
                improvement: false,
                noisy: o.noisy,
            });
        }
    }
    out
}

/// Warnings about runs that shouldn't be compared (different machine, profile or sizes).
pub fn comparability(old: &Report, new: &Report) -> Vec<String> {
    let mut w = Vec::new();
    if old.machine != new.machine {
        w.push(format!("different machines: {} vs {}", old.machine, new.machine));
    }
    if old.profile != new.profile {
        w.push(format!("different build profiles: {} vs {}", old.profile, new.profile));
    }
    if old.quick != new.quick {
        w.push("one run used --quick sizes".into());
    }
    if old.features != new.features {
        w.push(format!("different features: {:?} vs {:?}", old.features, new.features));
    }
    w
}

// ------------------------------------------------------------------ history and the page

/// Runs kept to compare versions over time (`bench/history.json`). Only complete runs
/// (not `--quick`, no groups left out) are kept; a run of a revision already in the history
/// replaces it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct History {
    /// Oldest first.
    pub runs: Vec<Report>,
}

/// Runs kept in a [`History`] (the oldest are dropped).
pub const MAX_HISTORY: usize = 100;

impl History {
    /// Read a history file; a missing file is an empty history.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(serde_json::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write the history, one run per line (small, readable diffs).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let runs = self.runs.iter().map(serde_json::to_string).collect::<Result<Vec<_>, _>>()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::recovery::write_atomic(path, format!("{{\"runs\": [\n{}\n]}}\n", runs.join(",\n")).as_bytes())
    }

    /// Whether `r` can go into the history (and be compared with its runs).
    pub fn accepts(r: &Report) -> bool {
        !r.quick && !r.partial
    }

    /// Add a run (if [`Self::accepts`] it). Returns whether it was added.
    pub fn add(&mut self, r: Report) -> bool {
        if !Self::accepts(&r) {
            return false;
        }
        self.runs.retain(|o| !(o.revision == r.revision && same_setup(o, &r)) || r.revision.is_empty());
        self.runs.push(r);
        self.runs.sort_by_key(|r| r.unix_time);
        let extra = self.runs.len().saturating_sub(MAX_HISTORY);
        self.runs.drain(..extra);
        true
    }

    /// The runs comparable with `r` (same machine, profile and features), oldest first, `r`'s
    /// own revision excluded.
    pub fn comparable(&self, r: &Report) -> Vec<&Report> {
        self.runs.iter().filter(|o| same_setup(o, r) && (o.revision != r.revision || o.revision.is_empty()) && o.unix_time <= r.unix_time).collect()
    }

    /// The latest comparable run before `r`.
    pub fn previous(&self, r: &Report) -> Option<&Report> {
        self.comparable(r).pop()
    }

    /// The latest comparable run of an earlier OpenQ version than `r`'s.
    pub fn previous_version(&self, r: &Report) -> Option<&Report> {
        self.comparable(r).into_iter().rfind(|o| !o.version.is_empty() && version_key(&o.version) < version_key(&r.version))
    }
}

/// `"1.10.2"` → `[1, 10, 2]`, for ordering versions.
fn version_key(v: &str) -> Vec<u64> {
    v.split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect()
}

fn same_setup(a: &Report, b: &Report) -> bool {
    a.machine == b.machine && a.profile == b.profile && a.features == b.features && a.quick == b.quick
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A value with sensible precision.
pub fn fmt_value(v: f64) -> String {
    if v == 0.0 || v.abs() >= 100.0 {
        format!("{v:.0}")
    } else if v.abs() >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.3}")
    }
}

fn run_label(r: &Report) -> String {
    let rev = if r.revision.is_empty() { "?" } else { &r.revision };
    if r.version.is_empty() { rev.to_string() } else { format!("v{} {rev}", r.version) }
}

fn run_date(r: &Report) -> String {
    let t = crate::errlog::timestamp(std::time::UNIX_EPOCH + Duration::from_secs(r.unix_time));
    t[..16].to_string()
}

/// Round `v` up to 1, 2 or 5 × a power of ten (chart axis top).
fn nice_ceil(v: f64) -> f64 {
    if v <= 0.0 || !v.is_finite() {
        return 1.0;
    }
    let p = 10f64.powf(v.log10().floor());
    [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * p).find(|x| *x >= v * 0.999).unwrap_or(10.0 * p)
}

/// One small line chart (inline SVG) of a metric over `runs`.
fn chart(name: &str, runs: &[&Report]) -> String {
    let points: Vec<(usize, &Report, &Metric)> =
        runs.iter().enumerate().filter_map(|(i, r)| r.metrics.iter().find(|m| m.name == name).map(|m| (i, *r, m))).collect();
    let Some(&(_, _, last)) = points.last() else { return String::new() };
    let (w, h, left, right, top, bottom) = (240.0, 112.0, 38.0, 8.0, 8.0, 88.0);
    let ymax = nice_ceil(points.iter().map(|p| p.2.value).fold(0.0, f64::max));
    let n = runs.len().max(1);
    let x = |i: usize| if n == 1 { (left + w - right) / 2.0 } else { left + (w - left - right) * i as f64 / (n - 1) as f64 };
    let y = |v: f64| bottom - (bottom - top) * (v / ymax).clamp(0.0, 1.0);
    let mut svg = format!(r#"<svg viewBox="0 0 {w} {h}" role="img" aria-label="{} over {} runs">"#, esc(name), points.len());
    for (v, cls) in [(0.0, "base"), (ymax / 2.0, "grid"), (ymax, "grid")] {
        svg += &format!(r#"<line class="{cls}" x1="{left}" x2="{}" y1="{y:.1}" y2="{y:.1}"/><text x="{}" y="{:.1}" text-anchor="end">{}</text>"#, w - right, left - 4.0, y(v) + 3.5, (v * 1e6).round() / 1e6, y = y(v));
    }
    if points.len() > 1 {
        let path: Vec<String> = points.iter().map(|(i, _, m)| format!("{:.1},{:.1}", x(*i), y(m.value))).collect();
        svg += &format!(r#"<polyline class="ln" points="{}"/>"#, path.join(" "));
    }
    for (i, r, m) in &points {
        let tip = format!("{} · {} · {} {}", run_label(r), run_date(r), fmt_value(m.value), m.unit);
        let (cx, cy) = (x(*i), y(m.value));
        svg += &format!(r#"<g><title>{}</title><circle class="hit" cx="{cx:.1}" cy="{cy:.1}" r="9"/><circle class="pt" cx="{cx:.1}" cy="{cy:.1}" r="3.5"/></g>"#, esc(&tip));
    }
    let (first, latest) = (runs[0], runs[n - 1]);
    svg += &format!(r#"<text x="{left}" y="{}" text-anchor="start">{}</text>"#, h - 6.0, esc(&run_label(first)));
    if n > 1 {
        svg += &format!(r#"<text x="{}" y="{}" text-anchor="end">{}</text>"#, w - right, h - 6.0, esc(&run_label(latest)));
    }
    svg += "</svg>";
    let short = name.split_once('.').map_or(name, |(_, rest)| rest);
    format!(
        r#"<div class="bchart"><div class="bhead"><code title="{} — {}">{}</code><b>{} {}</b></div>{svg}</div>"#,
        esc(name),
        esc(&last.note),
        esc(short),
        fmt_value(last.value),
        esc(&last.unit)
    )
}

/// One "vs" cell: the relative change and whether it's beyond `tolerance`.
fn change_cell(old: Option<&Report>, m: &Metric, tolerance: f64) -> String {
    let Some(old) = old else { return "<td></td>".into() };
    let one = Report { metrics: vec![m.clone()], ..Default::default() };
    let Some(c) = compare(old, &one, tolerance).into_iter().find(|c| c.name == m.name) else { return "<td></td>".into() };
    let Some(o) = c.old else { return r#"<td class="n">new</td>"#.into() };
    let pct = format!("{:+.0}%", c.worse_by * 100.0);
    let tag = if c.regression && c.noisy {
        r#" <span class="tag warn">noisy</span>"#
    } else if c.regression {
        r#" <span class="tag bad">slower</span>"#
    } else if c.improvement {
        r#" <span class="tag good">faster</span>"#
    } else {
        ""
    };
    format!(r#"<td class="n" title="was {} {}">{pct}{tag}</td>"#, fmt_value(o), esc(&m.unit))
}

/// The generated part of `docs/performance.html`: the latest run, a comparison with the
/// previous run and the previous version, and one chart per metric over the history. Uses the
/// runs comparable with the latest one.
pub fn render_history(h: &History, tolerance: f64) -> String {
    let Some(latest) = h.runs.iter().rev().find(|r| History::accepts(r)) else {
        return "<p>No benchmark runs yet: run <code>tools/bench.sh</code>.</p>".into();
    };
    let mut runs = h.comparable(latest);
    runs.push(latest);
    let skip = runs.len().saturating_sub(30);
    let runs = &runs[skip..];
    let prev = h.previous(latest);
    let prev_ver = h.previous_version(latest);
    let mut out = String::from(CHART_CSS);
    out += &format!(
        "<p>Latest run: <b>{}</b> · {} UTC · {} · {} {}. {} comparable run{} in <code>bench/history.json</code>{}.</p>\n",
        esc(&run_label(latest)),
        run_date(latest),
        esc(&latest.machine),
        esc(&latest.profile),
        esc(&latest.features.join("+")),
        runs.len(),
        if runs.len() == 1 { "" } else { "s" },
        if skip > 0 { " (charts show the last 30)" } else { "" }
    );
    let head = |r: Option<&Report>, what: &str| r.map_or(format!("vs {what} (none yet)"), |r| format!("vs {}", esc(&run_label(r))));
    out += &format!(
        "<div class=\"tbl\"><table>\n<tr><th>Metric</th><th>Latest</th><th>{}</th><th>{}</th></tr>\n",
        head(prev, "previous run"),
        head(prev_ver, "previous version")
    );
    for m in &latest.metrics {
        out += &format!(
            "<tr><td><code>{}</code></td><td class=\"n\" title=\"{}\">{} {}</td>{}{}</tr>\n",
            esc(&m.name),
            esc(&m.note),
            fmt_value(m.value),
            esc(&m.unit),
            change_cell(prev, m, tolerance),
            change_cell(prev_ver, m, tolerance)
        );
    }
    out += "</table></div>\n<p class=\"loc\">Positive = slower/worse. Tags follow the regression rule above \
            (tolerance and per-unit floor); <em>noisy</em> = a worst-case metric that moved, never a failure. \
            Hover a chart point for its run.</p>\n";
    let mut groups: Vec<&str> = Vec::new();
    for m in &latest.metrics {
        let g = m.name.split('.').next().unwrap_or("");
        if !groups.contains(&g) {
            groups.push(g);
        }
    }
    for g in groups {
        out += &format!("<h3>{}</h3>\n<div class=\"bcharts\">\n", esc(g));
        for m in latest.metrics.iter().filter(|m| m.name.split('.').next() == Some(g)) {
            out += &chart(&m.name, runs);
            out += "\n";
        }
        out += "</div>\n";
    }
    out
}

const CHART_CSS: &str = "<style>
.bcharts { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 10px; margin: 10px 0 18px; }
.bchart { background: var(--surface); border: 1px solid var(--line); border-radius: 10px; padding: 8px 10px 4px; min-width: 0; }
.bhead { display: flex; justify-content: space-between; gap: 8px; align-items: baseline; font-size: 13px; }
.bhead code { background: none; padding: 0; color: var(--muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.bhead b { font-variant-numeric: tabular-nums; white-space: nowrap; }
.bchart svg { display: block; width: 100%; height: auto; }
.bchart text { fill: var(--muted); font: 9.5px system-ui, sans-serif; font-variant-numeric: tabular-nums; }
.bchart .grid { stroke: var(--line); stroke-width: 1; stroke-dasharray: 2 3; }
.bchart .base { stroke: var(--line); stroke-width: 1; }
.bchart .ln { fill: none; stroke: var(--accent); stroke-width: 2; stroke-linejoin: round; stroke-linecap: round; }
.bchart .pt { fill: var(--accent); stroke: var(--surface); stroke-width: 2; }
.bchart .hit { fill: transparent; }
.bchart g:hover .pt { r: 5; }
</style>
";

/// Replace the text between `<!--bench:TAG-->` and `<!--/bench:TAG-->` in `page`.
pub fn replace_marked(page: &str, tag: &str, content: &str) -> anyhow::Result<String> {
    let (open, close) = (format!("<!--bench:{tag}-->"), format!("<!--/bench:{tag}-->"));
    let start = page.find(&open).ok_or_else(|| anyhow::anyhow!("marker {open} not found"))? + open.len();
    let end = start + page[start..].find(&close).ok_or_else(|| anyhow::anyhow!("marker {close} not found"))?;
    Ok(format!("{}{content}{}", &page[..start], &page[end..]))
}

/// Regenerate the benchmark parts of the performance page at `path` from `h`: the results
/// section (`results` markers) and the "latest numbers" line under the title (`latest`).
pub fn update_page(path: &Path, h: &History, tolerance: f64) -> anyhow::Result<()> {
    let page = std::fs::read_to_string(path)?;
    let mut page = replace_marked(&page, "results", &format!("\n{}", render_history(h, tolerance)))?;
    if let Some(r) = h.runs.iter().rev().find(|r| History::accepts(r)) {
        let line = format!("latest benchmark run: {} on {} UTC", esc(&run_label(r)), run_date(r));
        page = replace_marked(&page, "latest", &line)?;
    }
    crate::recovery::write_atomic(path, page.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(values: &[(&str, f64, &str)]) -> Report {
        Report {
            metrics: values.iter().map(|(n, v, u)| Metric { name: (*n).into(), value: *v, unit: (*u).into(), higher_is_better: false, note: String::new(), noisy: false }).collect(),
            ..Default::default()
        }
    }

    fn hrun(version: &str, rev: &str, time: u64, value: f64) -> Report {
        Report { version: version.into(), revision: rev.into(), unix_time: time, machine: "m".into(), ..report(&[("engine.go", value, "us")]) }
    }

    #[test]
    fn history_keeps_one_run_per_revision_and_finds_previous_versions() {
        let mut h = History::default();
        assert!(h.add(hrun("0.1.0", "a", 1, 10.0)));
        assert!(h.add(hrun("0.1.0", "b", 2, 11.0)));
        assert!(h.add(hrun("0.2.0", "c", 3, 30.0)));
        assert!(h.add(hrun("0.2.0", "c", 4, 12.0)), "a re-run replaces the revision's run");
        assert!(!h.add(Report { quick: true, ..hrun("0.2.0", "d", 5, 1.0) }));
        assert!(h.add(Report { machine: "other".into(), ..hrun("0.2.0", "e", 6, 1.0) }));
        assert_eq!(h.runs.len(), 4);
        let latest = h.runs.iter().find(|r| r.revision == "c").unwrap().clone();
        assert_eq!(latest.metrics[0].value, 12.0);
        assert_eq!(h.previous(&latest).unwrap().revision, "b");
        assert_eq!(h.previous_version(&latest).unwrap().revision, "b");
        assert!(h.previous_version(&hrun("0.1.0", "x", 9, 1.0)).is_none());

        // The page uses the latest run's machine: "e" is alone there.
        assert!(render_history(&h, 0.15).contains("1 comparable run in"));
        h.runs.retain(|r| r.machine == "m");
        let html = render_history(&h, 0.15);
        assert!(html.contains("3 comparable runs") && html.contains("vs v0.1.0 b") && html.contains("+9%"), "{html}");
        assert_eq!(html.matches("<circle class=\"pt\"").count(), 3);

        let page = "<p><!--bench:latest-->old<!--/bench:latest--></p>x<!--bench:results-->old<!--/bench:results-->";
        let page = replace_marked(page, "results", "NEW").unwrap();
        assert_eq!(page, "<p><!--bench:latest-->old<!--/bench:latest--></p>x<!--bench:results-->NEW<!--/bench:results-->");
        assert!(replace_marked("nothing", "results", "x").is_err());

        let path = std::env::temp_dir().join(format!("openq-bench-history-{}.json", std::process::id()));
        h.save(&path).unwrap();
        assert_eq!(History::load(&path).unwrap().runs.len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn compare_flags_only_real_changes() {
        let old = report(&[("a", 100.0, "us"), ("b", 2.0, "ns"), ("c", 10.0, "ms"), ("gone", 1.0, "ms")]);
        let new = report(&[("a", 130.0, "us"), ("b", 3.0, "ns"), ("c", 5.0, "ms"), ("fresh", 1.0, "ms")]);
        let c = compare(&old, &new, 0.15);
        let get = |n: &str| c.iter().find(|x| x.name == n).unwrap();
        assert!(get("a").regression, "30 % slower and 30 us");
        assert!(!get("b").regression, "+1 ns is under the floor");
        assert!(get("c").improvement);
        assert!(get("fresh").old.is_none() && get("gone").new.is_none());
    }

    #[test]
    fn quick_engine_and_lookup_benchmarks_run() {
        let r = run(&Options { quick: true, filter: vec!["engine".into(), "lookup".into()], no_realtime: true });
        assert!(r.metrics.iter().any(|m| m.name == "engine.tick_long_list.p50" && m.value > 0.0));
        assert!(r.metrics.iter().any(|m| m.name == "lookup.find"));
    }
}
