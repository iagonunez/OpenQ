//! Start accuracy on a real audio device: a chain of clicks 250 ms apart played through the
//! system's audio stack while another thread holds the show lock 0–12 ms at a time, like
//! `bench`'s `timing` but on a real output instead of the virtual one.
//!
//! ```text
//! cargo run --release --example clock_real -- --list          # output device ids
//! cargo run --release --example clock_real -- <device id>     # play the clicks
//! cargo run --release --example clock_real -- --analyse <capture.wav>
//! ```
//! The capture (32-bit float stereo 48 kHz WAV) is recorded outside while the clicks play. On
//! PipeWire, silently: `pactl load-module module-null-sink sink_name=opnq_clock_test`, play to
//! `pipewire:opnq_clock_test` and record it with `pw-record --target opnq_clock_test -P
//! '{ stream.capture.sink = true }' --rate 48000 --channels 2 --format f32 capture.wav`
//! (`parec` adds steps of its own). A sound card's own clock needs a loopback cable into an input.
//!
//! Measured 2026-10-21 (PipeWire 1.6, quantum 1024): with a 25 ms lead, 4–6 of 63 chained starts
//! were 1–6 ms late (the 21 ms block was rendered before they arrived); with the lead raised by
//! the device's block ([`opnq::engine::Backend::lookahead`]), all 63 on the sample in 3 runs.

use opnq::media::Prober;
use opnq::model::{AudioCue, ContinueMode, Cue, CueKind, Workspace};
use opnq::show::Show;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const COUNT: usize = 64;
const APART: usize = 12_000;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--list") {
        for d in opnq::media::audio::list_output_devices() {
            println!("{}\t{}", d.id, d.name);
        }
        return;
    }
    if args.first().map(String::as_str) == Some("--analyse") {
        analyse(&args[1]);
        return;
    }
    let device = args.first().cloned();
    let dir = std::env::temp_dir().join("opnq-clock-real");
    std::fs::create_dir_all(&dir).unwrap();
    let click = dir.join("click.wav");
    let mut s = vec![0.0f32; 4800];
    s[..8].fill(0.9);
    write_wav(&click, &s);

    let mut ws = Workspace::default();
    ws.settings.audio_device = device;
    ws.settings.double_go_protection = 0.0;
    for i in 0..COUNT {
        let mut c = Cue::new(CueKind::Audio(AudioCue { path: click.clone(), ..Default::default() }));
        c.number = i.to_string();
        c.continue_mode = ContinueMode::AutoContinue;
        c.post_wait = 0.25;
        ws.cue_list.cues.push(c);
    }
    let show = Arc::new(parking_lot::Mutex::new(Show::new(Prober::new())));
    show.lock().replace_workspace(ws, None);
    if let Some(e) = &show.lock().media.audio_error {
        panic!("audio: {e}");
    }
    opnq::show::spawn_engine_thread(show.clone(), || {});
    let prober = show.lock().media.prober.clone();
    let t = Instant::now();
    while prober.get(&click).is_none() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
    }
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
    std::thread::sleep(Duration::from_millis(500));
    let _ = opnq::control::apply(&mut show.lock(), opnq::control::Command::Go, Instant::now());
    std::thread::sleep(Duration::from_secs_f64(COUNT as f64 * 0.25 + 1.5));
    done.store(true, Ordering::Relaxed);
    let _ = load.join();
    let stats = show.lock().stats;
    println!("tick max gap {:?}, late ticks {}", stats.max_gap, stats.late);
    show.lock().shutdown();
}

fn analyse(path: &str) {
    let bytes = std::fs::read(path).unwrap();
    let data = bytes.windows(4).position(|w| w == b"data").expect("not a WAV file") + 8;
    let samples: Vec<f32> = bytes[data..].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let mut onsets = Vec::new();
    let mut quiet = 0usize;
    for (frame, l) in samples.as_chunks::<2>().0.iter().map(|c| c[0]).enumerate() {
        if l.abs() > 0.3 {
            if quiet > 1000 {
                onsets.push(frame);
            }
            quiet = 0;
        } else {
            quiet += 1;
        }
    }
    println!("{} clicks heard (expected {COUNT})", onsets.len());
    if onsets.len() < 3 {
        return;
    }
    // The first GO isn't scheduled; its followers are, each APART after the one before.
    let errs: Vec<i64> = onsets[1..].iter().enumerate().map(|(i, f)| (*f - onsets[1]) as i64 - (i * APART) as i64).collect();
    let worst = errs.iter().map(|e| e.abs()).max().unwrap();
    let mean = errs.iter().map(|e| e.abs()).sum::<i64>() as f64 / errs.len() as f64;
    let drift = *errs.last().unwrap();
    println!("chained start error: worst {worst} samples, mean {mean:.2}, last {drift} (48 = 1 ms)");
}

fn write_wav(path: &std::path::Path, mono: &[f32]) {
    let mut out = Vec::new();
    let data = (mono.len() * 2) as u32;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&48_000u32.to_le_bytes());
    out.extend_from_slice(&96_000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in mono {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, out).unwrap();
}
