//! Stress, soak, fuzz and crash-recovery tests.
//!
//! These are slow (minutes) and lean on the machine, so every test is `#[ignore]`d: plain
//! `cargo test` skips them. Run them before a release and after big features with
//! `tools/stress.sh`, which amounts to:
//!
//! ```text
//! cargo test --release --features fault-injection --test stress -- --ignored --test-threads=1
//! ```
//!
//! One at a time (`--test-threads=1`), because some of them measure timing, thread counts and
//! memory of the whole process. Nothing here needs a sound card: audio goes to a virtual output
//! (`AudioOutput::open_virtual`) that can also record what it plays, so start times are checked
//! to the sample. `OPENQ_STRESS_SCALE=<n>` multiplies the amount of work (default 1).
//! Tests marked `fault-injection` need that feature; they make real code paths panic.

use openq::control::{Command, CueAction, apply};
use openq::engine::{Backend, Engine, FadeRequest};
use openq::media::Prober;
use openq::media::audio::AudioOutput;
use openq::model::{AudioCue, ContinueMode, Cue, CueId, CueKind, FadeCue, GroupMode, KindTag, NetworkCue, Workspace};
use openq::show::{SharedShow, Show};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ helpers

fn scale() -> usize {
    std::env::var("OPENQ_STRESS_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1).max(1)
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("openq-stress-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 16-bit PCM WAV.
fn write_wav(path: &Path, rate: u32, channels: u16, samples: &[f32]) {
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
    std::fs::write(path, b).unwrap();
}

/// A stereo tone of `secs` seconds.
fn tone(path: &Path, secs: f64, hz: f32) {
    let n = (48_000.0 * secs) as usize;
    let s: Vec<f32> = (0..n).flat_map(|i| {
        let v = (i as f32 * hz * std::f32::consts::TAU / 48_000.0).sin() * 0.2;
        [v, v]
    }).collect();
    write_wav(path, 48_000, 2, &s);
}

fn audio_cue(number: &str, path: &Path) -> Cue {
    let mut c = Cue::new(CueKind::Audio(AudioCue { path: path.to_path_buf(), ..Default::default() }));
    c.number = number.into();
    c
}

/// A show whose audio goes to a virtual 48 kHz stereo output (optionally recorded).
fn virtual_show(capture: Option<rtrb::Producer<f32>>) -> SharedShow {
    let mut show = Show::new(Prober::new());
    show.media.audio = Some(AudioOutput::open_virtual(48_000, 2, capture).expect("virtual output"));
    show.media.audio_error = None;
    show.ws.settings.double_go_protection = 0.0;
    Arc::new(Mutex::new(show))
}

fn install(show: &SharedShow, mut ws: Workspace) {
    ws.settings.double_go_protection = 0.0;
    // The virtual output stays: the workspace asks for the default device and the output was
    // opened without a device id.
    show.lock().replace_workspace(ws, None);
    assert_eq!(show.lock().media.audio.as_ref().unwrap().device_name, "Virtual output");
}

fn wait_for(what: &str, secs: f64, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed().as_secs_f64() < secs, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Resident memory of this process in bytes (Linux only).
fn rss() -> Option<usize> {
    let s = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: usize = s.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

/// Threads of this process (Linux only).
fn threads() -> Option<usize> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines().find_map(|l| l.strip_prefix("Threads:")).and_then(|v| v.trim().parse().ok())
}

// ------------------------------------------------------------------ mock backend

/// Records what the engine asks for and checks it never leaks a started media cue.
#[derive(Default)]
struct Mock {
    now: Option<Instant>,
    playing: HashMap<CueId, (Instant, Option<f64>)>,
    paused: HashSet<CueId>,
    starts: u64,
    stops: u64,
    max_preload: usize,
}

impl Backend for Mock {
    fn set_preloaded(&mut self, cues: &[&Cue]) {
        self.max_preload = self.max_preload.max(cues.len());
    }
    fn start(&mut self, cue: &Cue, at: Instant) -> Result<(), String> {
        self.starts += 1;
        let d = self.duration(cue);
        self.playing.insert(cue.id, (at, d));
        Ok(())
    }
    fn stop(&mut self, id: CueId) {
        self.stops += 1;
        self.playing.remove(&id);
        self.paused.remove(&id);
    }
    fn pause(&mut self, id: CueId) {
        self.paused.insert(id);
    }
    fn resume(&mut self, id: CueId, _at: Instant) {
        self.paused.remove(&id);
    }
    fn fade(&mut self, _id: CueId, _f: &FadeRequest, _at: Instant) {}
    fn is_finished(&self, id: CueId) -> bool {
        match (self.playing.get(&id), self.now) {
            (None, _) => true,
            (Some((at, Some(d))), Some(now)) => !self.paused.contains(&id) && now >= *at + Duration::from_secs_f64(*d),
            _ => false,
        }
    }
    fn duration(&self, cue: &Cue) -> Option<f64> {
        match &cue.kind {
            // The mock reads a media cue's length from its end time (None = loops forever).
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

// ------------------------------------------------------------------ random workspaces

fn random_cue(rng: &mut fastrand::Rng, depth: usize, n: &mut usize) -> Cue {
    *n += 1;
    let kind = match rng.u32(0..100) {
        0..30 => CueKind::Audio(AudioCue {
            path: PathBuf::from(format!("{}/nonexistent/{}.wav", if cfg!(windows) { "C:" } else { "" }, rng.u32(0..20))),
            end_time: if rng.u8(0..10) == 0 { None } else { Some(rng.f64() * 2.0) },
            ..Default::default()
        }),
        30..45 => CueKind::Wait { duration: rng.f64() * 1.5 },
        45..50 => CueKind::Memo,
        50..58 => CueKind::Fade(FadeCue { duration: rng.f64(), volume_db: Some(-20.0), stop_target_when_done: rng.bool(), ..Default::default() }),
        58..63 => CueKind::Start { target: None },
        63..67 => CueKind::Stop { target: None },
        67..70 => CueKind::Pause { target: None },
        70..73 => CueKind::Goto { target: None },
        73..76 => CueKind::Devamp { target: None },
        76 => CueKind::Network(NetworkCue { host: "127.0.0.1".into(), port: 9, address: "/x".into(), ..Default::default() }),
        77 => CueKind::Network(NetworkCue {
            host: "127.0.0.1".into(),
            port: 9,
            format: openq::model::NetFormat::Text,
            transport: openq::model::NetTransport::Tcp,
            text: r"GO\r\n".into(),
            ..Default::default()
        }),
        78 => CueKind::Arm { target: None },
        79 => CueKind::Disarm { target: None },
        80 => CueKind::Reset { target: None },
        81 => CueKind::Load { target: None, time: rng.f64() },
        82 => CueKind::Target { target: None, new_target: None },
        _ if depth < 3 => {
            let mode = [GroupMode::StartFirstAndEnter, GroupMode::StartFirst, GroupMode::StartAll, GroupMode::StartRandom][rng.usize(0..4)];
            let children = (0..rng.usize(0..6)).map(|_| random_cue(rng, depth + 1, n)).collect();
            CueKind::Group { mode, children }
        }
        _ => CueKind::Memo,
    };
    let mut c = Cue::new(kind);
    c.number = n.to_string();
    c.pre_wait = if rng.u8(0..4) == 0 { rng.f64() * 0.5 } else { 0.0 };
    c.post_wait = rng.f64() * 0.5;
    c.continue_mode = [ContinueMode::DoNotContinue, ContinueMode::AutoContinue, ContinueMode::AutoFollow][rng.usize(0..3)];
    c.armed = rng.u8(0..12) != 0;
    c
}

/// A workspace of `count` top-level cues (plus group children) in one or two lists, with
/// control cues aimed at random cues, sometimes at ones that don't exist.
fn random_workspace(rng: &mut fastrand::Rng, count: usize) -> Workspace {
    let mut ws = Workspace::default();
    ws.settings.double_go_protection = 0.0;
    let mut n = 0;
    ws.cue_list.cues = (0..count).map(|_| random_cue(rng, 0, &mut n)).collect();
    if rng.bool() {
        let tab = ws.cue_list.add_list("Second", None);
        ws.cue_list.switch_to(tab, None);
        ws.cue_list.cues = (0..count / 2).map(|_| random_cue(rng, 0, &mut n)).collect();
        ws.cue_list.switch_to(0, None);
    }
    let ids: Vec<CueId> = ws.cue_list.iter_all().iter().map(|c| c.id).collect();
    let pick = |rng: &mut fastrand::Rng| if rng.u8(0..15) == 0 { Some(CueId::new_v4()) } else { Some(ids[rng.usize(0..ids.len())]) };
    fn visit(cues: &mut [Cue], f: &mut dyn FnMut(&mut Cue)) {
        for c in cues {
            f(c);
            if let Some(ch) = c.children_mut() {
                visit(ch, f);
            }
        }
    }
    let roots: Vec<&mut Vec<Cue>> = ws.cue_list.all_roots_mut().collect();
    for r in roots {
        visit(r, &mut |c| match &mut c.kind {
            CueKind::Fade(f) => f.target = pick(rng),
            CueKind::Start { target } | CueKind::Goto { target } | CueKind::Devamp { target } => *target = pick(rng),
            CueKind::Stop { target } | CueKind::Pause { target } => *target = if rng.bool() { pick(rng) } else { None },
            CueKind::Target { target, new_target } => {
                *target = pick(rng);
                *new_target = pick(rng);
            }
            k => {
                if let Some(t) = k.target_mut() {
                    *t = pick(rng);
                }
            }
        });
    }
    ws
}

fn check_invariants(e: &Engine, ws: &Workspace, m: &Mock, ctx: &str) {
    for id in m.playing.keys() {
        assert!(e.instances.contains_key(id), "{ctx}: media {id} still playing without an instance (leak)");
    }
    for id in e.instances.keys() {
        assert!(ws.cue_list.find(*id).is_some(), "{ctx}: instance of a deleted cue survived a tick");
    }
    assert!(e.log.len() <= 200, "{ctx}: engine log grew to {}", e.log.len());
    assert!(m.max_preload <= 16, "{ctx}: {} cues preloaded", m.max_preload);
}

// ------------------------------------------------------------------ engine

/// Thousands of random shows driven by random transport and edit operations: no panics, no
/// leaked media, no instances of deleted cues, bounded logs, and everything stops on Panic.
#[test]
#[ignore = "stress"]
fn stress_engine_random_operations() {
    let seeds = 400 * scale();
    let mut worst_tick = Duration::ZERO;
    let mut worst_at = String::new();
    for seed in 0..seeds as u64 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let count = rng.usize(5..60);
        let mut ws = random_workspace(&mut rng, count);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let mut now = Instant::now();
        e.set_playhead(ws.cue_list.first());
        for step in 0..400 {
            let ctx = format!("seed {seed} step {step}");
            let ids: Vec<CueId> = ws.cue_list.iter_all().iter().map(|c| c.id).collect();
            let any = |rng: &mut fastrand::Rng| ids.get(rng.usize(0..ids.len().max(1))).copied();
            match rng.u32(0..100) {
                0..35 => e.go(&ws, &mut m, now),
                35..40 => e.stop_all(&mut m, now, if rng.bool() { 0.0 } else { rng.f64() }),
                40..44 => e.pause_all(&mut m, now),
                44..48 => e.resume_all(&mut m, now),
                48..55 => {
                    if let Some(id) = any(&mut rng) {
                        e.start_cue(&ws, &mut m, id, now)
                    }
                }
                55..60 => {
                    if let Some(id) = any(&mut rng) {
                        e.stop_cue(&ws, &mut m, id)
                    }
                }
                60..63 => {
                    if let Some(id) = any(&mut rng) {
                        e.pause_cue(&ws, &mut m, id, now)
                    }
                }
                63..66 => {
                    if let Some(id) = any(&mut rng) {
                        e.resume_cue(&ws, &mut m, id, now)
                    }
                }
                66..70 => e.set_playhead(any(&mut rng)),
                70..74 => {
                    // Edit: delete a cue (maybe a running one).
                    if let Some(id) = any(&mut rng) {
                        ws.cue_list.remove(id);
                        e.workspace_changed();
                    }
                }
                74..76 if ws.cue_list.list_count() > 1 => {
                    let tab = rng.usize(0..ws.cue_list.list_count());
                    let ph = ws.cue_list.switch_to(tab, e.playhead);
                    e.set_playhead(ph);
                }
                76..78 => {
                    // A finished media file.
                    if let Some(id) = m.playing.keys().next().copied() {
                        m.playing.remove(&id);
                    }
                }
                _ => {}
            }
            now += Duration::from_millis(rng.u64(0..80));
            let t = Instant::now();
            e.tick(&ws, &mut m, now);
            if t.elapsed() > worst_tick {
                worst_tick = t.elapsed();
                worst_at = ctx.clone();
            }
            check_invariants(&e, &ws, &m, &ctx);
        }
        // Let everything run out, then Panic: nothing may be left.
        for _ in 0..50 {
            now += Duration::from_millis(500);
            e.tick(&ws, &mut m, now);
            check_invariants(&e, &ws, &m, &format!("seed {seed} drain"));
        }
        e.stop_all(&mut m, now, 0.0);
        e.tick(&ws, &mut m, now);
        assert!(e.instances.is_empty(), "seed {seed}: instances after Panic: {}", e.instances.len());
        assert!(m.playing.is_empty(), "seed {seed}: media still playing after Panic");
    }
    eprintln!("{seeds} random shows, worst tick {worst_tick:?} ({worst_at})");
    // Loops with no time in them (a Start cue aimed at itself...) must not spin the engine.
    // Shared macOS runners stall a thread for a few ms now and then (CI saw 6.9 ms), so they get a wider bound.
    let tick_limit = if cfg!(target_os = "macos") { 25 } else { 5 };
    assert!(worst_tick < Duration::from_millis(tick_limit), "worst tick {worst_tick:?} at {worst_at}");
}

/// Big shows: a 4000-cue auto-follow chain and a group firing 2000 cues at once must run
/// to the end, and no tick may take long enough to disturb timing.
#[test]
#[ignore = "stress"]
fn stress_engine_scale() {
    let n = 2000 * scale();
    let mut rng = fastrand::Rng::with_seed(1);
    let mut ws = Workspace::default();
    ws.settings.double_go_protection = 0.0;
    let mut chain = Vec::new();
    for i in 0..2 * n {
        let mut c = Cue::new(CueKind::Wait { duration: 0.001 });
        c.number = format!("c{i}");
        c.continue_mode = ContinueMode::AutoFollow;
        chain.push(c);
    }
    let children: Vec<Cue> = (0..n)
        .map(|i| {
            let mut c = if i % 2 == 0 {
                Cue::new(CueKind::Wait { duration: rng.f64() * 2.0 })
            } else {
                Cue::new(CueKind::Audio(AudioCue { end_time: Some(rng.f64() * 2.0), ..Default::default() }))
            };
            c.number = format!("g{i}");
            c
        })
        .collect();
    let mut group = Cue::new(CueKind::Group { mode: GroupMode::StartAll, children });
    group.number = "G".into();
    // The chain ends there (it must not run on into the group).
    chain.last_mut().unwrap().continue_mode = ContinueMode::DoNotContinue;
    let chain_first = chain[0].id;
    let group_id = group.id;
    ws.cue_list.cues = chain;
    ws.cue_list.cues.push(group);

    let mut e = Engine::new();
    let mut m = Mock::default();
    let t0 = Instant::now();
    let mut now = t0;
    e.start_cue(&ws, &mut m, chain_first, now);
    e.start_cue(&ws, &mut m, group_id, now);
    let mut ticks = Vec::new();
    while e.is_active() {
        now += Duration::from_millis(1);
        let t = Instant::now();
        e.tick(&ws, &mut m, now);
        ticks.push(t.elapsed());
        assert!(now - t0 < Duration::from_secs(60), "show didn't finish");
    }
    ticks.sort();
    let p99 = ticks[ticks.len() * 99 / 100];
    let max = *ticks.last().unwrap();
    eprintln!("{} ticks, p99 {p99:?}, max {max:?}, starts {}", ticks.len(), m.starts);
    assert_eq!(m.starts as usize, n / 2, "every audio child started once");
    // The engine thread ticks every millisecond; one tick must stay well under that.
    assert!(p99 < Duration::from_micros(1500), "p99 tick {p99:?}");
    assert!(max < Duration::from_millis(100), "max tick {max:?}");
}

// ------------------------------------------------------------------ timing

/// Sample accuracy under load: a chain of clicks 250 ms apart, fired by the real engine thread
/// into the virtual output while other threads grab the show lock for up to 12 ms at a time
/// and every core is busy. The recorded clicks must be 12 000 samples apart, give or take a
/// fraction of a millisecond.
#[test]
#[ignore = "stress"]
fn stress_timing_sample_accuracy() {
    let dir = tmpdir("timing");
    let click = dir.join("click.wav");
    let mut s = vec![0.0f32; 4800];
    s[..8].fill(0.9);
    write_wav(&click, 48_000, 1, &s);

    let count = 40 * scale();
    let mut ws = Workspace::default();
    for i in 0..count {
        let mut c = audio_cue(&i.to_string(), &click);
        c.continue_mode = ContinueMode::AutoContinue;
        c.post_wait = 0.25;
        ws.cue_list.cues.push(c);
    }
    let secs = count as f64 * 0.25 + 2.0;
    let (tx, mut rx) = rtrb::RingBuffer::<f32>::new((48_000.0 * 2.0 * (secs + 2.0)) as usize);
    let show = virtual_show(Some(tx));
    install(&show, ws);
    openq::show::spawn_engine_thread(show.clone(), || {});

    let done = Arc::new(AtomicBool::new(false));
    let mut load = Vec::new();
    // Something like a busy UI: holds the show lock 0–12 ms, often.
    {
        let (show, done) = (show.clone(), done.clone());
        load.push(std::thread::spawn(move || {
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
        }));
    }
    // Every core busy.
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    for _ in 0..cores {
        let done = done.clone();
        load.push(std::thread::spawn(move || {
            let mut x = 1.0f64;
            while !done.load(Ordering::Relaxed) {
                x = (x * 1.000001).sin() + 1.5;
            }
            std::hint::black_box(x);
        }));
    }
    std::thread::sleep(Duration::from_millis(500));
    apply(&mut show.lock(), Command::Go, Instant::now()).unwrap();
    std::thread::sleep(Duration::from_secs_f64(secs));
    done.store(true, Ordering::Relaxed);
    for t in load {
        t.join().unwrap();
    }
    let stats = show.lock().stats;
    show.lock().shutdown();

    let mut onsets = Vec::new();
    let mut quiet = 0usize;
    let mut frame = 0usize;
    while let Ok(l) = rx.pop() {
        let _r = rx.pop();
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
    assert_eq!(onsets.len(), count, "clicks heard");
    // The first click starts on GO, at the next audio period; the chain after it is scheduled
    // ahead and must land exactly 12 000 samples apart.
    let errs: Vec<i64> = onsets[1..].iter().enumerate().map(|(i, f)| (*f - onsets[1]) as i64 - i as i64 * 12_000).collect();
    let worst = errs.iter().map(|e| e.abs()).max().unwrap();
    let mean = errs.iter().map(|e| e.abs()).sum::<i64>() as f64 / errs.len() as f64;
    let go_late = (onsets[1] - onsets[0]) as i64 - 12_000;
    eprintln!(
        "{count} clicks: worst {worst} samples ({:.3} ms), mean {mean:.2}; first click (on GO) {go_late} samples off; \
         engine ticks {}, late {}, longest gap {:?}",
        worst as f64 / 48.0,
        stats.ticks,
        stats.late,
        stats.max_gap
    );
    // Windows runners are shared VMs whose scheduler stalls threads for tens of ms (CI saw an 82 ms engine gap, 10 ms
    // worst start): allow a quarter of a second of mixer periods there, keep the strict 0.5 ms elsewhere.
    // macOS runners do the same (CI saw 468 samples, ~10 ms).
    let limit = if cfg!(windows) || cfg!(target_os = "macos") { 1200 } else { 24 };
    assert!(worst <= limit, "worst start error {worst} samples (> {limit})");
    // GO itself waits for the next audio period at most (256 frames on the virtual output).
    assert!(go_late.abs() <= 512, "first click {go_late} samples off");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ memory and threads

/// Soak: random transport on a 64-cue audio show for a while. Afterwards no voices,
/// decoder threads or memory may be left over.
#[test]
#[ignore = "stress"]
fn stress_audio_soak_releases_everything() {
    let dir = tmpdir("soak");
    let files: Vec<PathBuf> = (0..6)
        .map(|i| {
            let p = dir.join(format!("t{i}.wav"));
            tone(&p, if i == 0 { 10.0 } else { 0.2 + i as f64 * 0.1 }, 220.0 + i as f32 * 110.0);
            p
        })
        .collect();
    let mut rng = fastrand::Rng::with_seed(5);
    let mut ws = Workspace::default();
    for i in 0..64 {
        let mut c = audio_cue(&i.to_string(), &files[rng.usize(0..files.len())]);
        c.continue_mode = [ContinueMode::DoNotContinue, ContinueMode::AutoFollow, ContinueMode::AutoContinue][rng.usize(0..3)];
        c.post_wait = rng.f64() * 0.3;
        ws.cue_list.cues.push(c);
    }
    let ids: Vec<CueId> = ws.cue_list.cues.iter().map(|c| c.id).collect();
    let show = virtual_show(None);
    install(&show, ws);
    openq::show::spawn_engine_thread(show.clone(), || {});

    let round = |rng: &mut fastrand::Rng, ops: usize| {
        for _ in 0..ops {
            let id = ids[rng.usize(0..ids.len())];
            let cmd = match rng.u32(0..100) {
                0..40 => Command::Go,
                40..60 => Command::Cue(id, CueAction::Start),
                60..72 => Command::Cue(id, CueAction::Stop),
                72..78 => Command::Cue(id, CueAction::Pause),
                78..84 => Command::Cue(id, CueAction::Resume),
                84..90 => Command::SetPlayhead(Some(id)),
                90..95 => Command::Stop,
                _ => Command::Panic,
            };
            let _ = apply(&mut show.lock(), cmd, Instant::now());
            std::thread::sleep(Duration::from_millis(rng.u64(0..15)));
        }
        apply(&mut show.lock(), Command::Panic, Instant::now()).unwrap();
        std::thread::sleep(Duration::from_millis(600));
    };
    // Warm up so caches, thread pools and allocator arenas exist before measuring.
    round(&mut rng, 300);
    let (rss0, th0) = (rss(), threads());
    let rounds = 6 * scale();
    for _ in 0..rounds {
        round(&mut rng, 500);
    }
    let s = show.lock();
    assert!(s.engine.instances.is_empty());
    let a = s.media.audio.as_ref().unwrap();
    assert!(a.voice_count() <= 16, "voices left: {}", a.voice_count());
    assert!(!a.is_lost());
    drop(s);
    if let (Some(r0), Some(r1), Some(t0), Some(t1)) = (rss0, rss(), th0, threads()) {
        eprintln!("rss {} → {} MB, threads {t0} → {t1}", r0 >> 20, r1 >> 20);
        assert!(r1 < r0 + (48 << 20), "memory grew {} MB", (r1 - r0) >> 20);
        assert!(t1 <= t0 + 4, "threads grew {t0} → {t1}");
    }
    show.lock().shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// More sounds than the mixer has room for: the extra ones are refused with an error, nothing
/// allocates on the audio thread, and the slots come back once they stop.
#[test]
#[ignore = "stress"]
fn stress_voice_limit() {
    let dir = tmpdir("voices");
    let p = dir.join("long.wav");
    tone(&p, 5.0, 330.0);
    let mut a = AudioOutput::open_virtual(48_000, 2, None).unwrap();
    let cue = AudioCue { path: p.clone(), ..Default::default() };
    let max = openq::media::audio::MAX_VOICES;
    let ids: Vec<CueId> = (0..max + 40).map(|_| CueId::new_v4()).collect();
    let mut refused = 0;
    for id in &ids {
        if let Err(e) = a.start(*id, &cue, 2, Instant::now()) {
            assert!(e.contains("too many"), "{e}");
            refused += 1;
        }
    }
    assert_eq!(refused, 40);
    std::thread::sleep(Duration::from_millis(200));
    assert!(a.drain_errors().is_empty(), "the mixer itself had to refuse voices");
    for id in &ids {
        a.stop(*id);
    }
    std::thread::sleep(Duration::from_millis(100));
    a.update();
    assert_eq!(a.voice_count(), 0);
    a.start(ids[0], &cue, 2, Instant::now()).expect("room again");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ hostile input

/// Random and damaged show files: saving and loading round-trips; damaged files are refused
/// with an error, never a crash; big shows load quickly.
#[test]
#[ignore = "stress"]
fn stress_workspace_files() {
    let dir = tmpdir("files");
    let path = dir.join("show.openq");
    for seed in 0..(100 * scale()) as u64 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let count = rng.usize(1..80);
        let ws = random_workspace(&mut rng, count);
        ws.save(&path).unwrap();
        let back = Workspace::load(&path).unwrap();
        if back != ws {
            let (a, b) = (serde_json::to_string_pretty(&ws).unwrap(), serde_json::to_string_pretty(&back).unwrap());
            let diff: Vec<String> = a.lines().zip(b.lines()).filter(|(x, y)| x != y).take(6).map(|(x, y)| format!("{x}  →  {y}")).collect();
            panic!("seed {seed}: round trip changed the show:\n{}", diff.join("\n"));
        }

        let good = std::fs::read(&path).unwrap();
        for k in 0..30 {
            let mut b = good.clone();
            match k % 5 {
                0 => b.truncate(rng.usize(0..b.len())),
                1 => {
                    for _ in 0..rng.usize(1..20) {
                        let i = rng.usize(0..b.len());
                        b[i] = rng.u8(..);
                    }
                }
                2 => {
                    let i = rng.usize(0..b.len());
                    b.splice(i..i, std::iter::repeat_n(b'[', rng.usize(1..5000)));
                }
                3 => {
                    let s = String::from_utf8_lossy(&b).replace("0.0", "1e999").replace("false", "null");
                    b = s.into_bytes();
                }
                _ => b = (0..rng.usize(0..4096)).map(|_| rng.u8(..)).collect(),
            }
            std::fs::write(&path, &b).unwrap();
            let r = std::panic::catch_unwind(|| Workspace::load(&path).map(|_| ()));
            assert!(r.is_ok(), "seed {seed} mutation {k}: loading a damaged file panicked");
        }
    }
    // A big show.
    let mut ws = Workspace::default();
    // (At most as many as a show may hold.)
    for i in 0..(20_000 * scale()).min(openq::limits::MAX_CUES) {
        let mut c = Cue::new(KindTag::Wait.default_kind());
        c.number = i.to_string();
        ws.cue_list.cues.push(c);
    }
    let t = Instant::now();
    ws.save(&path).unwrap();
    let back = Workspace::load(&path).unwrap();
    eprintln!("{} cues saved and loaded in {:?}", back.cue_list.cues.len(), t.elapsed());
    assert!(t.elapsed() < Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Garbage media files and timecode signals: the workers answer with errors and keep serving;
/// decoders recover a real signal after noise.
#[test]
#[ignore = "stress"]
fn stress_garbage_media_and_timecode() {
    let dir = tmpdir("garbage");
    let mut rng = fastrand::Rng::with_seed(9);
    let mut files = Vec::new();
    let good = dir.join("ok.wav");
    tone(&good, 0.3, 440.0);
    let header = std::fs::read(&good).unwrap()[..44].to_vec();
    for i in 0..60 {
        let ext = ["wav", "mp3", "flac", "ogg", "aiff", "m4a"][i % 6];
        let p = dir.join(format!("bad{i}.{ext}"));
        let mut b: Vec<u8> = if i % 3 == 0 { header.clone() } else { Vec::new() };
        b.extend((0..rng.usize(0..20_000)).map(|_| rng.u8(..)));
        std::fs::write(&p, b).unwrap();
        files.push(p);
    }
    let prober = Prober::new();
    for p in &files {
        prober.get(p);
    }
    wait_for("all probes answered", 60.0, || files.iter().all(|p| prober.get(p).is_some()));
    wait_for("prober still alive", 10.0, || matches!(prober.get(&good), Some(Ok(_))));

    // Playing them: each cue ends (with an error), none hangs.
    let mut a = AudioOutput::open_virtual(48_000, 2, None).unwrap();
    let ids: Vec<CueId> = files
        .iter()
        .map(|p| {
            let id = CueId::new_v4();
            let _ = a.start(id, &AudioCue { path: p.clone(), ..Default::default() }, 2, Instant::now());
            id
        })
        .collect();
    wait_for("garbage cues to end", 30.0, || ids.iter().all(|id| a.is_finished(*id)));

    // Timecode decoders on noise, then a real signal.
    use openq::model::TimecodeRate;
    use openq::timecode::{LtcDecoder, LtcEncoder, MtcDecoder};
    let mut ltc = LtcDecoder::new(48_000);
    let noise: Vec<f32> = (0..48_000 * 10).map(|i| if i % 997 < 400 { rng.f32() * 2.0 - 1.0 } else { ((i / rng.usize(1..50)) % 2) as f32 - 0.5 }).collect();
    for chunk in noise.chunks(rng.usize(64..2048)) {
        ltc.push(chunk);
    }
    let mut enc = LtcEncoder::new(3600.0, TimecodeRate::Fps25, 48_000, 0.5);
    let mut sig = vec![0.0; 48_000];
    enc.fill(&mut sig);
    assert!(!ltc.push(&sig).is_empty(), "LTC decoder didn't recover after noise");
    let mut mtc = MtcDecoder::default();
    for _ in 0..100_000 {
        let len = rng.usize(0..12);
        let msg: Vec<u8> = (0..len).map(|_| rng.u8(..)).collect();
        mtc.push(&msg);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// An OSC flood of valid, invalid and random packets: the listener survives and still obeys.
#[test]
#[ignore = "stress"]
fn stress_osc_flood() {
    let mut ws = Workspace::default();
    for i in 1..=5 {
        let mut c = Cue::new(CueKind::Wait { duration: 0.05 });
        c.number = i.to_string();
        ws.cue_list.cues.push(c);
    }
    let three = ws.cue_list.cues[2].id;
    let show = virtual_show(None);
    install(&show, ws);
    openq::show::spawn_engine_thread(show.clone(), || {});
    let port = 41000 + (std::process::id() % 10000) as u16;
    let _osc = openq::osc::OscServer::start(port, show.clone(), || {}).unwrap();
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let to = ("127.0.0.1", port);
    let msg = |addr: &str| {
        rosc::encoder::encode(&rosc::OscPacket::Message(rosc::OscMessage { addr: addr.into(), args: vec![] })).unwrap()
    };
    let mut rng = fastrand::Rng::with_seed(11);
    let addrs = ["/go", "/stop", "/panic", "/pause", "/resume", "/cue/2/start", "/cue/9/stop", "/playhead/next", "/nope", "/cue//", "////"];
    for i in 0..30_000 * scale() {
        let b = if i % 3 == 0 {
            (0..rng.usize(0..600)).map(|_| rng.u8(..)).collect()
        } else {
            msg(addrs[rng.usize(0..addrs.len())])
        };
        let _ = sock.send_to(&b, to);
        if i % 500 == 0 {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    wait_for("OSC still obeys", 10.0, || {
        let _ = sock.send_to(&msg("/playhead/3"), to);
        std::thread::sleep(Duration::from_millis(50));
        show.lock().engine.playhead == Some(three)
    });
    show.lock().shutdown();
}

/// Junk and idle connections to the remote server, more than it accepts at once: afterwards it
/// still pairs a real device.
#[test]
#[ignore = "stress"]
fn stress_remote_junk_connections() {
    use openq::remote::client::{Auth, Link, LinkState};
    use openq::remote::server::Server;
    use openq::remote::{RemoteConfig, Role};
    use std::io::Write;
    let dir = tmpdir("remote");
    // SAFETY: set before other threads of this test read the environment.
    unsafe { std::env::set_var("OPENQ_CONFIG_DIR", &dir) };
    let show = virtual_show(None);
    let port = 42000 + (std::process::id() % 10000) as u16;
    let cfg = Arc::new(Mutex::new(RemoteConfig { port, instance_name: "Main".into(), ..Default::default() }));
    let server = Server::start(show.clone(), cfg, || {}).unwrap();
    let addr = format!("127.0.0.1:{port}");
    let mut rng = fastrand::Rng::with_seed(13);
    // Idle connections filling every slot, then junk.
    let idle: Vec<_> = (0..40).filter_map(|_| std::net::TcpStream::connect(&addr).ok()).collect();
    for _ in 0..300 {
        if let Ok(mut s) = std::net::TcpStream::connect(&addr) {
            let junk: Vec<u8> = match rng.u8(0..3) {
                0 => (0..rng.usize(0..5000)).map(|_| rng.u8(..)).collect(),
                // A frame claiming to be huge.
                1 => u32::MAX.to_be_bytes().to_vec(),
                _ => {
                    let mut v = 100u32.to_be_bytes().to_vec();
                    v.extend(b"{\"not\":\"a hello\"}");
                    v
                }
            };
            let _ = s.write_all(&junk);
        }
    }
    drop(idle);
    std::thread::sleep(Duration::from_millis(500));
    let code = server.open_pairing();
    let client = Arc::new(Mutex::new(RemoteConfig { instance_name: "Tablet".into(), ..Default::default() }));
    let link = Link::connect(&addr, Auth::Pair(code), client, || {});
    wait_for("pairing after junk", 30.0, || matches!(link.state(), LinkState::Connected { role: Role::Control, .. }));
    show.lock().shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ crash and recovery

/// The process dies (abort, no clean-up) with unsaved changes: the next start finds the
/// autosaved copy, and a crash report was written for a panic before that.
#[test]
#[ignore = "stress"]
fn stress_crash_leaves_recoverable_autosave() {
    use openq::recovery::{Autosave, find_orphans_in, install_panic_hook};
    if let Ok(dir) = std::env::var("OPENQ_STRESS_CHILD") {
        // Child: edit a show, autosave, panic on a worker, then die hard.
        let dir = PathBuf::from(dir);
        unsafe { std::env::set_var("OPENQ_CONFIG_DIR", &dir) };
        let show: SharedShow = Arc::new(Mutex::new(Show::new(Prober::new())));
        {
            let mut s = show.lock();
            s.path = Some(PathBuf::from("/shows/Crashy.openq"));
            s.ws.settings.osc_port = 5151;
            s.edited();
        }
        let a = Arc::new(Mutex::new(Autosave::new_in(&dir.join("recovery")).unwrap()));
        install_panic_hook(&show, a.clone());
        let _ = std::thread::Builder::new().name("openq-doomed".into()).spawn(|| panic!("worker went down")).unwrap().join();
        a.lock().sync(&show).unwrap();
        std::process::abort();
    }
    let dir = tmpdir("crash");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "stress_crash_leaves_recoverable_autosave", "--ignored", "--nocapture"])
        .env("OPENQ_STRESS_CHILD", &dir)
        .status()
        .unwrap();
    assert!(!status.success(), "child should have crashed");
    let found = find_orphans_in(&dir.join("recovery"), None);
    assert_eq!(found.len(), 1, "autosave left behind");
    assert_eq!(found[0].name(), "Crashy");
    assert_eq!(found[0].load().unwrap().settings.osc_port, 5151);
    let logs: Vec<_> = std::fs::read_dir(dir.join("crash")).unwrap().flatten().collect();
    assert!(!logs.is_empty(), "crash report written");
    let report = std::fs::read_to_string(logs[0].path()).unwrap();
    assert!(report.contains("worker went down") && report.contains("openq-doomed"), "{report}");
    found[0].discard();
    assert!(find_orphans_in(&dir.join("recovery"), None).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "fault-injection")]
mod faults {
    use super::*;
    use openq::recovery::{Fault, arm};

    /// The engine panics mid-show: the thread survives, everything stops, the log says why, and
    /// the next GO works.
    #[test]
    #[ignore = "stress"]
    fn stress_engine_panic_is_recovered() {
        let mut ws = Workspace::default();
        for i in 0..3 {
            let mut c = Cue::new(CueKind::Wait { duration: 30.0 });
            c.number = i.to_string();
            ws.cue_list.cues.push(c);
        }
        let show = virtual_show(None);
        install(&show, ws);
        openq::show::spawn_engine_thread(show.clone(), || {});
        apply(&mut show.lock(), Command::Go, Instant::now()).unwrap();
        assert!(show.lock().engine.is_active());
        arm(Fault::EngineTick);
        wait_for("recovery", 5.0, || show.lock().stats.panics == 1);
        let mut s = show.lock();
        assert!(s.engine.instances.is_empty());
        assert!(s.recovered.take().is_some());
        assert!(s.engine.log.iter().any(|l| l.contains("Internal error")));
        apply(&mut s, Command::Go, Instant::now()).unwrap();
        assert!(s.engine.is_active(), "GO after recovery");
        drop(s);
        std::thread::sleep(Duration::from_millis(50));
        assert!(show.lock().engine.is_active(), "engine thread still ticking");
        show.lock().shutdown();
    }

    /// The audio callback panics: silence instead of a crash, the output counts as lost, and
    /// it is reopened automatically.
    #[test]
    #[ignore = "stress"]
    fn stress_audio_callback_panic_reconnects() {
        let dir = tmpdir("cbpanic");
        let p = dir.join("t.wav");
        tone(&p, 3.0, 220.0);
        let mut ws = Workspace::default();
        ws.cue_list.cues.push(audio_cue("1", &p));
        ws.cue_list.cues.push(audio_cue("2", &p));
        let show = virtual_show(None);
        install(&show, ws);
        openq::show::spawn_engine_thread(show.clone(), || {});
        apply(&mut show.lock(), Command::Go, Instant::now()).unwrap();
        arm(Fault::AudioCallback);
        // Reconnecting to a virtual output takes milliseconds, so watch the log, not the state.
        wait_for("audio back", 10.0, || {
            let s = show.lock();
            s.engine.log.iter().any(|l| l.contains("Audio output back")) && s.media.audio.as_ref().is_some_and(|a| !a.is_lost() && a.callbacks() > 0)
        });
        let mut s = show.lock();
        assert!(s.engine.log.iter().any(|l| l.contains("Audio output lost")), "{:?}", s.engine.log);
        assert!(s.engine.log.iter().any(|l| l.contains("Audio output back")), "{:?}", s.engine.log);
        // The cue that was playing ended with the old output.
        apply(&mut s, Command::Go, Instant::now()).unwrap();
        assert!(s.media.audio.as_ref().unwrap().voice_count() > 0, "plays on the new output");
        drop(s);
        show.lock().shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A decoder thread panics: its cue ends with an error instead of hanging, and the next
    /// one plays.
    #[test]
    #[ignore = "stress"]
    fn stress_decoder_and_prober_panics_are_contained() {
        let dir = tmpdir("decpanic");
        let p = dir.join("t.wav");
        tone(&p, 1.0, 220.0);
        let mut a = AudioOutput::open_virtual(48_000, 2, None).unwrap();
        let cue = AudioCue { path: p.clone(), ..Default::default() };
        arm(Fault::Decoder);
        let id = CueId::new_v4();
        let started = a.start(id, &cue, 2, Instant::now());
        if started.is_ok() {
            wait_for("broken cue ends", 5.0, || a.is_finished(id));
            let errs = a.drain_errors();
            assert!(errs.iter().any(|e| e.contains("internal error")), "{errs:?}");
        }
        let id2 = CueId::new_v4();
        a.start(id2, &cue, 2, Instant::now()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!a.is_finished(id2), "next cue plays");

        let prober = Prober::new();
        arm(Fault::Prober);
        let other = dir.join("u.wav");
        tone(&other, 0.5, 330.0);
        wait_for("failed probe answered", 5.0, || matches!(prober.get(&p), Some(Err(_))));
        wait_for("prober still serves", 5.0, || matches!(prober.get(&other), Some(Ok(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A lighting frame panics: the light thread keeps sending the last good frame, reports it
    /// once and goes on with the next GO.
    #[test]
    #[ignore = "stress"]
    fn stress_light_panic_is_recovered() {
        use openq::light::{engine::LightEngine, library, model::*};
        let mut l = Lighting::default();
        let t = library::generic("generic/dimmer").unwrap();
        l.add_type(&t);
        l.fixtures.push(Fixture { id: 1, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), address: 1, ..Default::default() });
        let cues: Vec<LightCue> = [1.0, 0.5]
            .iter()
            .map(|v| {
                let mut c = LightCue { fade: 0.0, ..Default::default() };
                c.values.entry(1).or_default().insert(Attribute::Dimmer, (*v).into());
                c
            })
            .collect();
        let ids: Vec<_> = cues.iter().map(|c| c.id).collect();
        l.sequences.push(Sequence { id: 1, cues: cues.into(), ..Default::default() });
        let mut e = LightEngine::start();
        e.load(&l);
        e.go_cue(ids[0], Instant::now());
        wait_for("first look", 2.0, || e.frame().get(1).is_some_and(|u| u.data[0] == 255));
        // Light threads of shows left by earlier tests can take the fault: arm until ours does.
        wait_for("fault reported", 5.0, || {
            arm(Fault::LightTick);
            std::thread::sleep(Duration::from_millis(60));
            e.drain_errors().iter().any(|m| m.contains("injected fault"))
        });
        assert_eq!(e.frame().get(1).unwrap().data[0], 255, "last good frame held");
        e.go_cue(ids[1], Instant::now());
        wait_for("next GO after the fault", 2.0, || e.frame().get(1).is_some_and(|u| u.data[0] == 128));
    }
}

/// A long cue list with only a few cues running (the usual show): the tick must not walk the
/// whole tree for every running cue.
#[test]
#[ignore]
fn stress_long_list_few_running() {
    let n = 10_000 * scale();
    let mut ws = Workspace::default();
    for i in 0..n {
        let mut c = Cue::new(CueKind::Wait { duration: 30.0 });
        c.number = format!("{i}");
        ws.cue_list.cues.push(c);
    }
    // The last cues of the list are the worst case for a tree walk.
    let running: Vec<CueId> = ws.cue_list.cues[n - 4..].iter().map(|c| c.id).collect();
    let mut e = Engine::new();
    let mut m = Mock::default();
    let t0 = Instant::now();
    for id in &running {
        e.start_cue(&ws, &mut m, *id, t0);
    }
    let mut now = t0;
    let mut ticks = Vec::new();
    for _ in 0..2000 {
        now += Duration::from_millis(1);
        let t = Instant::now();
        e.tick(&ws, &mut m, now);
        ticks.push(t.elapsed());
    }
    ticks.sort();
    let p50 = ticks[ticks.len() / 2];
    let p99 = ticks[ticks.len() * 99 / 100];
    eprintln!("{n} cues, 4 running: p50 {p50:?}, p99 {p99:?}");
    assert!(p50 < Duration::from_micros(500), "p50 tick {p50:?}");
}

// ------------------------------------------------------------------ lighting

mod lighting {
    use super::{rss, scale, tmpdir, wait_for};
    use openq::light::engine::LightEngine;
    use openq::light::effects::{Effect, EffectRun, Shape};
    use openq::light::fixlib::{self, Library};
    use openq::light::import::{Format, import};
    use openq::light::library;
    use openq::light::model::*;
    use openq::light::playback::{LightShow, Playback};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const OFL: &str = r##"{
      "name": "Fuzz Bar",
      "matrix": { "pixelCount": [4, 1, 1], "pixelGroups": { "Master": "all" } },
      "wheels": { "Color Wheel": { "slots": [ { "type": "Open" }, { "type": "Color", "name": "Red", "colors": ["#ff0000"] } ] } },
      "availableChannels": {
        "Pan": { "fineChannelAliases": ["Pan fine"], "capability": { "type": "Pan" } },
        "Color Wheel": { "capabilities": [ { "dmxRange": [0, 9], "type": "WheelSlot", "slotNumber": 1 }, { "dmxRange": [10, 255], "type": "WheelSlot", "slotNumber": 2 } ] },
        "Shutter": { "capabilities": [ { "dmxRange": [0, 9], "type": "ShutterStrobe", "shutterEffect": "Closed", "switchChannels": { "Sw": "Pan" } }, { "dmxRange": [10, 255], "type": "ShutterStrobe", "shutterEffect": "Open" } ] }
      },
      "templateChannels": {
        "Dimmer $pixelKey": { "capability": { "type": "Intensity" } },
        "Red $pixelKey": { "fineChannelAliases": ["Red $pixelKey fine"], "capability": { "type": "ColorIntensity", "color": "Red" } }
      },
      "modes": [
        { "name": "A", "channels": ["Dimmer Master", "Pan", "Pan fine", "Color Wheel", "Shutter", "Sw", null,
          { "insert": "matrixChannels", "repeatFor": "eachPixelXYZ", "channelOrder": "perPixel", "templateChannels": ["Red $pixelKey", "Red $pixelKey fine"] } ] },
        { "name": "B", "channels": [ { "insert": "matrixChannels", "repeatFor": ["2", "1"], "channelOrder": "perChannel", "templateChannels": ["Red $pixelKey", "Dimmer $pixelKey"] } ] }
      ]
    }"##;

    const QXF: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE FixtureDefinition>
<FixtureDefinition xmlns="http://www.qlcplus.org/FixtureDefinition">
 <Manufacturer>Fuzz</Manufacturer><Model>Bar</Model>
 <Channel Name="Dimmer" Preset="IntensityMasterDimmer"/>
 <Channel Name="Dimmer fine" Preset="IntensityMasterDimmerFine"/>
 <Channel Name="Strobe"><Group Byte="0">Shutter</Group>
  <Capability Min="0" Max="9">Open</Capability><Capability Min="10" Max="255" Res1="#ff0000">Strobe</Capability></Channel>
 <Channel Name="Red"><Group Byte="0">Intensity</Group><Colour>Red</Colour></Channel>
 <Mode Name="M"><Physical><Lens DegreesMin="20" DegreesMax="40"/><Focus Type="Head" PanMax="540" TiltMax="270"/></Physical>
  <Channel Number="0">Dimmer</Channel><Channel Number="1">Dimmer fine</Channel><Channel Number="2">Strobe</Channel>
  <Channel Number="3">Red</Channel><Channel Number="4">Red</Channel>
  <Head><Channel>3</Channel></Head><Head><Channel>4</Channel></Head>
 </Mode>
</FixtureDefinition>"##;

    const GDTF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<GDTF DataVersion="1.2"><FixtureType Name="Bar" LongName="Fuzz Bar" Manufacturer="Fuzz">
 <Wheels><Wheel Name="Gobo1"><Slot Name="Open"/><Slot Name="Star" MediaFileName="star"/></Wheel>
  <Wheel Name="Color1"><Slot Name="Open" Color="0.3127,0.3290,100"/><Slot Name="Red" Color="0.64,0.33,21"/></Wheel></Wheels>
 <Geometries><Geometry Name="Body">
   <GeometryReference Name="P1" Geometry="Pixel"><Break DMXBreak="1" DMXOffset="4"/></GeometryReference>
   <GeometryReference Name="P2" Geometry="Pixel"><Break DMXBreak="1" DMXOffset="5"/></GeometryReference>
   <Axis Name="Yoke"><Axis Name="Head"><Beam Name="B" BeamAngle="20"/></Axis></Axis></Geometry>
  <Geometry Name="Pixel"/></Geometries>
 <DMXModes><DMXMode Name="M" Geometry="Body"><DMXChannels>
  <DMXChannel DMXBreak="1" Offset="1,2" Geometry="Yoke"><LogicalChannel Attribute="Pan"><ChannelFunction Attribute="Pan" DMXFrom="0/2" Default="32768/2" PhysicalFrom="-270" PhysicalTo="270"/></LogicalChannel></DMXChannel>
  <DMXChannel DMXBreak="1" Offset="3" Geometry="Head"><LogicalChannel Attribute="Gobo1"><ChannelFunction Attribute="Gobo1" DMXFrom="0/1" Wheel="Gobo1">
   <ChannelSet DMXFrom="0/1" WheelSlotIndex="1"/><ChannelSet DMXFrom="20/1" WheelSlotIndex="2"/></ChannelFunction>
   <ChannelFunction Attribute="Gobo1WheelSpin" DMXFrom="128/1"/></LogicalChannel></DMXChannel>
  <DMXChannel DMXBreak="1" Offset="1" Geometry="Pixel"><LogicalChannel Attribute="ColorAdd_R"><ChannelFunction Attribute="ColorAdd_R"/></LogicalChannel></DMXChannel>
  <DMXChannel DMXBreak="2" Offset="1" Geometry="Head"><LogicalChannel Attribute="Zoom"><ChannelFunction Attribute="Zoom"/></LogicalChannel></DMXChannel>
 </DMXChannels></DMXMode></DMXModes></FixtureType></GDTF>"#;

    /// A zip of the given entries; `deflate` compresses them (else stored).
    fn zip(files: &[(&str, &[u8])], deflate: bool) -> Vec<u8> {
        use std::io::Write;
        let (mut out, mut central) = (Vec::new(), Vec::new());
        for (name, data) in files {
            let body = if deflate {
                let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
                e.write_all(data).unwrap();
                e.finish().unwrap()
            } else {
                data.to_vec()
            };
            let method: u16 = if deflate { 8 } else { 0 };
            let local = out.len() as u32;
            let header = |v: &mut Vec<u8>, sig: u32, central: bool| {
                v.extend_from_slice(&sig.to_le_bytes());
                if central {
                    v.extend_from_slice(&20u16.to_le_bytes());
                }
                v.extend_from_slice(&[20, 0, 0, 0]);
                v.extend_from_slice(&method.to_le_bytes());
                v.extend_from_slice(&[0; 8]); // time, date, crc (not checked)
                v.extend_from_slice(&(body.len() as u32).to_le_bytes());
                v.extend_from_slice(&(data.len() as u32).to_le_bytes());
                v.extend_from_slice(&(name.len() as u16).to_le_bytes());
                v.extend_from_slice(&0u16.to_le_bytes());
                if central {
                    v.extend_from_slice(&[0; 10]); // comment length, disk, attributes
                    v.extend_from_slice(&local.to_le_bytes());
                }
                v.extend_from_slice(name.as_bytes());
            };
            header(&mut out, 0x0403_4b50, false);
            out.extend_from_slice(&body);
            header(&mut central, 0x0201_4b50, true);
        }
        let at = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Damage a file: flip bytes, cut, repeat or drop a span.
    fn mutate(rng: &mut fastrand::Rng, src: &[u8]) -> Vec<u8> {
        let mut v = src.to_vec();
        for _ in 0..rng.usize(1..6) {
            if v.is_empty() {
                break;
            }
            let i = rng.usize(..v.len());
            match rng.u8(..5) {
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
                _ => {
                    // A digit into a number: offsets, DMX values, counts.
                    let d = b"0951"[rng.usize(..4)];
                    if v[i].is_ascii_digit() {
                        v.insert(i, d);
                    }
                }
            }
        }
        v
    }

    /// Change values inside valid JSON (numbers huge/negative/fractional, strings, arrays
    /// emptied or repeated), so the importer gets past the parser.
    fn mutate_json(rng: &mut fastrand::Rng, v: &mut serde_json::Value) {
        use serde_json::Value;
        match v {
            Value::Number(_) => {
                if rng.u8(..4) == 0 {
                    *v = [serde_json::json!(-1), serde_json::json!(1e12), serde_json::json!(65535), serde_json::json!(0.5), serde_json::json!(511), serde_json::json!("x")][rng.usize(..6)].clone();
                }
            }
            Value::String(s) => {
                if rng.u8(..8) == 0 {
                    *s = ["", "Pan", "Red $pixelKey", "eachPixelABC", "1", "#zzzzzz", "Open"][rng.usize(..7)].to_string();
                }
            }
            Value::Array(a) => {
                if rng.u8(..10) == 0 && !a.is_empty() {
                    let i = rng.usize(..a.len());
                    let x = a[i].clone();
                    a.extend(std::iter::repeat_n(x, rng.usize(1..40)));
                } else if rng.u8(..15) == 0 {
                    a.clear();
                }
                for x in a.iter_mut() {
                    mutate_json(rng, x);
                }
            }
            Value::Object(o) => {
                if rng.u8(..12) == 0 && !o.is_empty() {
                    let k = o.keys().nth(rng.usize(..o.len())).cloned().expect("key");
                    o.remove(&k);
                }
                for x in o.values_mut() {
                    mutate_json(rng, x);
                }
            }
            _ => {}
        }
    }

    /// What an import may hand to the show: within the limits, and it compiles and plays.
    fn check_type(t: &FixtureType, what: &str) {
        assert!(!t.modes.is_empty() && t.modes.len() <= fixlib::MAX_MODES, "{what}: modes");
        for m in &t.modes {
            assert!(m.footprint() <= 512, "{what}: footprint {}", m.footprint());
            assert!(m.heads.len() <= MAX_CELLS as usize, "{what}: cells");
            for c in m.all_channels() {
                assert!(c.offset < 512 && c.fine.is_none_or(|f| f < 512), "{what}: offset");
                assert!(c.default.is_none_or(|v| (0.0..=1.0).contains(&v)), "{what}: default");
            }
        }
        let mut l = Lighting::default();
        l.add_type(t);
        for (i, m) in t.modes.iter().enumerate() {
            l.fixtures.push(Fixture { id: i as u32 + 1, fixture_type: t.id.clone(), mode: m.name.clone(), universe: i as u16 + 1, address: 1, ..Default::default() });
        }
        let units = l.units();
        let show = Arc::new(LightShow::compile(&l));
        let t0 = Instant::now();
        let mut p = Playback::new(show, t0);
        let values: Vec<(FixtureId, Attribute, f32)> = units.iter().flat_map(|u| l.unit_attributes(*u).into_iter().map(move |a| (*u, a, 0.7))).collect();
        p.set_programmer(&values);
        p.tick(t0);
    }

    /// Damaged and hostile fixture files (OFL, QLC+, GDTF, OpenQ): never a panic, a hang or a
    /// memory blow-up; whatever imports stays within the limits and plays.
    #[test]
    #[ignore = "stress"]
    fn stress_fixture_import_fuzz() {
        let mut rng = fastrand::Rng::with_seed(0x6f71_6669);
        let gdtf = zip(&[("description.xml", GDTF.as_bytes())], true);
        let oq = fixlib::write(&import(OFL.as_bytes(), Format::Ofl, "fuzz").unwrap().fixture);
        let samples: [(&str, Format, Vec<u8>); 4] =
            [("ofl", Format::Ofl, OFL.into()), ("qxf", Format::Qxf, QXF.into()), ("gdtf", Format::Gdtf, gdtf), ("oqfixture", Format::OpenQ, oq.into_bytes())];
        for (name, f, bytes) in &samples {
            check_type(&import(bytes, *f, "fuzz").unwrap_or_else(|e| panic!("{name}: {e:#}")).fixture, name);
        }
        let n = 4000 * scale();
        let (mut ok, mut err) = (0, 0);
        let t = Instant::now();
        for i in 0..n {
            let (name, f, src) = &samples[i % samples.len()];
            // GDTF: damage the XML inside a valid zip too, not only the zip.
            let bytes = if *f == Format::Gdtf && rng.bool() {
                zip(&[("description.xml", &mutate(&mut rng, GDTF.as_bytes()))], rng.bool())
            } else if matches!(f, Format::Ofl | Format::OpenQ) && rng.u8(..3) > 0 {
                let mut v: serde_json::Value = serde_json::from_slice(src).expect("sample");
                mutate_json(&mut rng, &mut v);
                serde_json::to_vec(&v).expect("json")
            } else {
                mutate(&mut rng, src)
            };
            let start = Instant::now();
            let r = std::panic::catch_unwind(|| import(&bytes, *f, "fuzz")).unwrap_or_else(|_| panic!("{name} #{i} panicked"));
            assert!(start.elapsed() < Duration::from_secs(2), "{name} #{i} took {:?}", start.elapsed());
            match r {
                Ok(imp) => {
                    check_type(&imp.fixture, &format!("{name} #{i}"));
                    ok += 1;
                }
                Err(_) => err += 1,
            }
        }
        eprintln!("{n} damaged files in {:.1} s: {ok} imported, {err} refused", t.elapsed().as_secs_f64());

        // Hostile on purpose.
        let rss0 = rss();
        let bomb = zip(&[("description.xml", &vec![b' '; 300 << 20])], true);
        eprintln!("zip bomb: {} KB → 300 MB", bomb.len() >> 10);
        assert!(import(&bomb, Format::Gdtf, "x").is_err(), "zip bomb refused");
        let laughs = r#"<?xml version="1.0"?><!DOCTYPE x [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;"><!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;">]><FixtureDefinition><Model>&c;</Model></FixtureDefinition>"#;
        assert!(import(laughs.as_bytes(), Format::Qxf, "x").is_err(), "entities refused");
        assert!(import(&zip(&[("description.xml", laughs.as_bytes())], false), Format::Gdtf, "x").is_err());
        let deep = format!("{}{}", "<a>".repeat(200_000), "</a>".repeat(200_000));
        let _ = import(deep.as_bytes(), Format::Qxf, "x");
        let _ = import(&zip(&[("description.xml", deep.as_bytes())], true), Format::Gdtf, "x");
        let nested = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(import(nested.as_bytes(), Format::Ofl, "x").is_err(), "deep JSON refused");
        let t = Instant::now();
        let huge = OFL.replace("\"pixelCount\": [4, 1, 1]", "\"pixelCount\": [100000, 100000, 100000]");
        let r = import(huge.as_bytes(), Format::Ofl, "x").expect("huge matrix imports, capped");
        check_type(&r.fixture, "huge matrix");
        assert!(r.fixture.modes.iter().all(|m| m.heads.len() <= MAX_CELLS as usize));
        assert!(t.elapsed() < Duration::from_secs(2), "huge matrix took {:?}", t.elapsed());
        let wide = OFL.replace("\"repeatFor\": [\"2\", \"1\"]", &format!("\"repeatFor\": [{}]", (0..100_000).map(|i| format!("\"{i}\"")).collect::<Vec<_>>().join(",")));
        check_type(&import(wide.as_bytes(), Format::Ofl, "x").expect("long repeat list, cut at 512").fixture, "wide mode");
        let mut far = import(OFL.as_bytes(), Format::Ofl, "x").unwrap().fixture;
        far.modes[0].channels[0].offset = 4000;
        assert!(import(fixlib::write(&far).as_bytes(), Format::OpenQ, "x").is_err(), "offsets past the universe refused");
        if let (Some(a), Some(b)) = (rss0, rss()) {
            eprintln!("RSS {} → {} MB", a >> 20, b >> 20);
            assert!(b < a + (200 << 20), "memory grew by {} MB", (b - a) >> 20);
        }
    }

    /// A type with `cells` RGB cells and a master dimmer.
    fn bar(cells: u16) -> FixtureType {
        let heads = (0..cells)
            .map(|i| Head {
                name: (i + 1).to_string(),
                channels: [Attribute::Red, Attribute::Green, Attribute::Blue]
                    .iter()
                    .enumerate()
                    .map(|(k, a)| ChannelDef { attribute: *a, offset: 1 + 3 * i + k as u16, ..Default::default() })
                    .collect(),
            })
            .collect();
        let mut m = FixtureMode::sequential("bar", &[(Attribute::Dimmer, false)]);
        m.heads = heads;
        FixtureType { id: format!("stress/bar{cells}"), name: "Bar".into(), modes: vec![m], ..Default::default() }
    }

    /// A random design: generic types and a multi-cell bar, sequences with random values
    /// (also on units that don't exist), effects on random units.
    fn random_design(rng: &mut fastrand::Rng, fixtures: u32) -> Lighting {
        let mut types: Vec<FixtureType> = ["generic/dimmer", "generic/drgbw", "generic/spot", "generic/wash"].iter().filter_map(|i| library::generic(i)).collect();
        types.push(bar(16));
        let mut l = Lighting::default();
        let (mut u, mut a) = (1u16, 1u16);
        for i in 0..fixtures {
            let t = &types[rng.usize(..types.len())];
            l.add_type(t);
            let n = t.modes[0].footprint();
            if a + n - 1 > 512 {
                u += 1;
                a = 1;
            }
            l.fixtures.push(Fixture { id: i + 1, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), universe: u, address: if rng.u8(..20) == 0 { 0 } else { a }, ..Default::default() });
            a += n;
        }
        let units = l.units();
        let pick = |rng: &mut fastrand::Rng| if rng.u8(..30) == 0 { cell_id(rng.u32(1..fixtures + 5), rng.u32(1..40)) } else { units[rng.usize(..units.len())] };
        for e in 1..=4 {
            let shape = [Shape::Sine, Shape::Square, Shape::Circle, Shape::Random][rng.usize(..4)];
            l.effects.push(Effect { id: e, shape, attribute: Attribute::ALL[rng.usize(..Attribute::ALL.len())], ..Default::default() });
        }
        for s in 1..=3 {
            let cues = (0..rng.usize(1..12))
                .map(|c| {
                    let mut cue = LightCue { number: (c + 1).to_string(), fade: rng.f64() * 3.0, delay: rng.f64(), cue_only: rng.u8(..8) == 0, ..Default::default() };
                    for _ in 0..rng.usize(..200) {
                        let f = pick(rng);
                        let a = Attribute::ALL[rng.usize(..Attribute::ALL.len())];
                        cue.values.entry(f).or_default().insert(a, rng.f32().into());
                    }
                    if rng.bool() {
                        let fx: Vec<FixtureId> = (0..rng.usize(1..64)).map(|_| pick(rng)).collect();
                        cue.effects.push(EffectRun { effect: rng.u32(1..6), fixtures: fx, stop: rng.u8(..5) == 0 });
                    }
                    cue
                })
                .collect();
            l.sequences.push(Sequence { id: s, cues, move_in_black: rng.bool(), ..Default::default() });
        }
        l
    }

    /// Random designs, edits (renumber, remove, re-patch) and transport on the light thread at
    /// once: no internal errors, frames keep coming, the design stays consistent.
    #[test]
    #[ignore = "stress"]
    fn stress_light_engine_random_operations() {
        let mut rng = fastrand::Rng::with_seed(0x6c69_6768);
        let mut e = LightEngine::start();
        let mut l = random_design(&mut rng, 150);
        e.load(&l);
        let steps = 3000 * scale();
        let t = Instant::now();
        for step in 0..steps {
            let now = Instant::now();
            match rng.u8(..12) {
                0 => {
                    let n = rng.u32(1..300);
                    l = random_design(&mut rng, n);
                    e.load(&l);
                }
                1 if !l.fixtures.is_empty() => {
                    let old = l.fixtures[rng.usize(..l.fixtures.len())].id;
                    let new = rng.u32(1..400);
                    l.renumber_fixture(old, new);
                    e.load(&l);
                }
                2 if !l.fixtures.is_empty() => {
                    let gone = (0..rng.usize(1..10)).map(|_| l.fixtures[rng.usize(..l.fixtures.len())].id).collect();
                    l.remove_fixtures(&gone);
                    e.load(&l);
                }
                3 => {
                    let units = l.units();
                    let values = (0..rng.usize(..500))
                        .filter_map(|_| units.get(rng.usize(..units.len().max(1))).map(|u| (*u, Attribute::ALL[rng.usize(..Attribute::ALL.len())], rng.f32() * 1.4 - 0.2)))
                        .collect();
                    e.set_programmer(values);
                }
                4 => {
                    let fx = (1..=rng.u32(..4)).filter_map(|i| l.effect(i).cloned()).map(|f| (f, l.units())).collect();
                    e.set_programmer_effects(fx);
                }
                5 => e.release(rng.u32(1..5), rng.f64(), now),
                6 => e.set_level(rng.u32(1..4), rng.f32()),
                7 => e.set_grand_master(rng.f32()),
                8 => e.set_blackout(rng.u8(..10) == 0),
                9 => e.set_tempo(rng.f32() * 300.0),
                10 => {
                    if let Some(c) = l.sequences.iter().flat_map(|s| s.cues.iter()).nth(rng.usize(..20)) {
                        e.go_cue(c.id, now);
                    }
                }
                _ => e.go(rng.u32(1..5), None, now),
            }
            if step % 50 == 0 {
                std::thread::sleep(Duration::from_millis(2));
            }
            for err in e.drain_errors() {
                assert!(!err.contains("internal error") && !err.contains("panic"), "step {step}: {err}");
            }
            assert!(l.validate().iter().all(|i| !matches!(i, PatchIssue::DuplicateId(_))), "step {step}: duplicate numbers");
        }
        let before = e.frames();
        std::thread::sleep(Duration::from_millis(300));
        assert!(e.frames() > before + 5, "light thread still running");
        eprintln!("{steps} operations in {:.1} s, {} frames", t.elapsed().as_secs_f64(), e.frames());
    }

    /// A big pixel rig (bars of 32 RGB cells, every cell in a cue and a chase across all of
    /// them) on the real light thread: it keeps its 44 Hz and a frame stays well in budget.
    #[test]
    #[ignore = "stress"]
    fn stress_light_pixel_rig() {
        let bars = 300 * scale() as u32;
        let ty = bar(32);
        let mut l = Lighting::default();
        l.add_type(&ty);
        for i in 0..bars {
            l.fixtures.push(Fixture { id: i + 1, fixture_type: ty.id.clone(), mode: "bar".into(), universe: 1 + (i / 5) as u16, address: 1 + 97 * (i % 5) as u16, ..Default::default() });
        }
        let cells: Vec<FixtureId> = (1..=bars).flat_map(|f| (1..=32).map(move |c| cell_id(f, c))).collect();
        l.effects.push(Effect { id: 1, shape: Shape::Square, ..Default::default() });
        l.effects.push(Effect { id: 2, shape: Shape::Sine, attribute: Attribute::Blue, ..Default::default() });
        let cues: Vec<LightCue> = (0..4)
            .map(|k| {
                let mut c = LightCue { fade: 2.0, ..Default::default() };
                for (i, id) in cells.iter().enumerate() {
                    let v = c.values.entry(*id).or_default();
                    v.insert(Attribute::Red, (((i + k) % 10) as f32 / 10.0).into());
                    v.insert(Attribute::Green, (((i * 3 + k) % 10) as f32 / 10.0).into());
                }
                for f in 1..=bars {
                    c.values.entry(f).or_default().insert(Attribute::Dimmer, 1.0.into());
                }
                c.effects.push(EffectRun { effect: 1 + (k as u32 % 2), fixtures: cells.clone(), stop: false });
                c
            })
            .collect();
        let ids: Vec<_> = cues.iter().map(|c| c.id).collect();
        l.sequences.push(Sequence { id: 1, cues: cues.into(), ..Default::default() });
        eprintln!("{bars} bars, {} cells, {} universes", cells.len(), bars.div_ceil(5));
        // Compile and frame cost, measured directly.
        let t = Instant::now();
        let show = Arc::new(LightShow::compile(&l));
        let compile = t.elapsed();
        let t0 = Instant::now();
        let mut p = Playback::new(show, t0);
        p.go(1, None, t0).unwrap();
        let mut v: Vec<Duration> = (0..500)
            .map(|i| {
                let t = Instant::now();
                std::hint::black_box(p.tick(t0 + Duration::from_secs_f64(i as f64 * 0.0227)));
                t.elapsed()
            })
            .collect();
        v.sort();
        let (p50, p99) = (v[v.len() / 2], v[v.len() * 99 / 100]);
        eprintln!("compile {compile:?}, frame p50 {p50:?}, p99 {p99:?}");
        assert!(p99 < Duration::from_millis(8), "frame p99 {p99:?} (budget 22.7 ms)");
        // The light thread under it, with GOs while it runs.
        let mut e = LightEngine::start();
        e.load(&l);
        wait_for("rig loaded", 30.0, || e.status().len() == 1);
        let f0 = e.frames();
        let t = Instant::now();
        for (k, id) in ids.iter().cycle().take(12).enumerate() {
            e.go_cue(*id, Instant::now());
            std::thread::sleep(Duration::from_millis(250));
            assert!(e.drain_errors().is_empty(), "GO {k}");
        }
        let rate = (e.frames() - f0) as f64 / t.elapsed().as_secs_f64();
        eprintln!("light thread: {rate:.1} frames/s");
        // Shared macOS runners deliver ~37 frames/s here (CI), so they get a lower floor.
        let floor = if cfg!(target_os = "macos") { 25.0 } else { 40.0 };
        assert!(rate > floor, "{rate:.1} frames/s");
        assert_eq!(e.frame().universes.len(), bars.div_ceil(5) as usize);
    }

    /// The fixture library under concurrent saves, removals and reloads from several threads,
    /// with junk files in the folder: no panic, every save lands, junk is reported, not loaded.
    #[test]
    #[ignore = "stress"]
    fn stress_fixture_library_concurrent() {
        let dir = tmpdir("fixlib");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("junk.oqfixture"), b"\x00\xffnot json").unwrap();
        std::fs::write(dir.join("other.txt"), b"ignored").unwrap();
        let lib = Library::new(Some(dir.clone()));
        let per = 40 * scale();
        let workers: Vec<_> = (0..8)
            .map(|w| {
                let lib = lib.clone();
                std::thread::spawn(move || {
                    for i in 0..per {
                        let mut t = bar(1 + (i % 32) as u16);
                        t.id = format!("stress/w{w}-{i}");
                        t.name = format!("Bar {w}-{i}");
                        lib.save(vec![t.clone()], |r| r.expect("save"));
                        if i % 4 == 3 {
                            lib.remove(t, |r| r.expect("remove"));
                        }
                        if i % 7 == 0 {
                            lib.reload();
                        }
                        let _ = lib.snapshot();
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().expect("worker");
        }
        lib.wait();
        assert!(!lib.snapshot().loading);
        let s = lib.snapshot();
        let expect = 8 * (per - per / 4);
        assert_eq!(s.items.len(), expect, "saved minus removed");
        assert_eq!(s.errors.len(), 1, "the junk file: {:?}", s.errors);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
