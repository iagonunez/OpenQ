//! Benchmarks for comparing builds: the engine tick, cue lookups, the audio mixer, start
//! accuracy, video conversion, text rendering, waveforms, saving/loading, remote patches and the
//! UI frame. Run them with `tools/bench.sh` (or `cargo run --release --example bench`), which
//! saves a JSON [`Report`] and compares it with a baseline.
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
pub struct Report {
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
pub const GROUPS: &[&str] = &["engine", "lookup", "mixer", "timing", "video", "text", "waveform", "workspace", "remote", "ui"];

/// Run the benchmarks chosen by `opts`, printing progress to stderr.
pub fn run(opts: &Options) -> Report {
    let mut r = Report {
        unix_time: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        machine: machine(),
        profile: if cfg!(debug_assertions) { "debug" } else { "release" }.into(),
        features: features(),
        quick: opts.quick,
        ..Default::default()
    };
    let wanted = |g: &str| opts.filter.is_empty() || opts.filter.iter().any(|f| g.contains(f.as_str()));
    let q = opts.quick;
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
            _ => {}
        }
        eprintln!("bench: {g} done in {:.1} s", t.elapsed().as_secs_f64());
    }
    r
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
                push(m, &format!("video.convert_{name}"), ms(quantile(&mut v[2..], 0.5)), "ms", &format!("{w}x{h} decode-thread conversion to RGBA + publish{budget}"));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn report(values: &[(&str, f64, &str)]) -> Report {
        Report {
            metrics: values.iter().map(|(n, v, u)| Metric { name: (*n).into(), value: *v, unit: (*u).into(), higher_is_better: false, note: String::new(), noisy: false }).collect(),
            ..Default::default()
        }
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
