//! Headless UI measurements for [`crate::bench`]: whole-window frames drawn on a plain egui
//! context (layout + tessellation, no GPU), as in the perf tests, but reported instead of
//! asserted.

use super::{App, inspector};
use crate::media::Prober;
use crate::model::{AudioCue, Cue, CueKind, FadeCue, TextCue};
use crate::show::{SharedShow, Show};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Frame times of one UI scenario.
pub(crate) struct UiTimes {
    /// Time per frame, one entry per measured frame.
    pub frames: Vec<Duration>,
    /// Longest the engine thread waited for the show lock while the frames ran.
    pub worst_lock_wait: Duration,
}

/// A show of `cues` cues (memos, audio cues with missing files, fades aimed at the last cue)
/// after a styled Text cue, which is selected.
fn show_with(cues: usize) -> (SharedShow, Arc<Prober>) {
    let prober = Prober::new();
    let mut show = Show::new(prober.clone());
    let t = TextCue { text: "Benchmark".into(), outline_width: 4.0, bg_color: [20, 30, 60, 220], ..Default::default() };
    let mut cue = Cue::new(CueKind::Text(t));
    cue.number = "1".into();
    show.ws.cue_list.cues.push(cue);
    let last = Cue::new(CueKind::Memo);
    for i in 0..cues {
        let kind = match i % 3 {
            0 => CueKind::Memo,
            1 => CueKind::Audio(AudioCue { path: format!("/nonexistent/openq-bench/{i}.wav").into(), ..Default::default() }),
            _ => CueKind::Fade(FadeCue { target: Some(last.id), ..Default::default() }),
        };
        let mut c = Cue::new(kind);
        c.number = format!("{}", i + 2);
        c.name = format!("Cue {i}");
        show.ws.cue_list.cues.push(c);
    }
    show.ws.cue_list.cues.push(last);
    (Arc::new(parking_lot::Mutex::new(show)), prober)
}

fn frame(ctx: &egui::Context, app: &mut App, shared: &SharedShow) -> Duration {
    shared.lock().tick(Instant::now());
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0))),
        ..Default::default()
    };
    let t0 = Instant::now();
    let out = ctx.run_ui(input, |ui| app.draw(ui, None));
    let _ = ctx.tessellate(out.shapes, out.pixels_per_point);
    t0.elapsed()
}

/// Draw `frames` frames of a `cues`-cue show with the inspector on `tab` (`settings` = Settings
/// tab, otherwise Basics) while a thread takes the show lock every millisecond like the engine.
pub(crate) fn frame_times(cues: usize, settings_tab: bool, frames: usize) -> UiTimes {
    crate::media::set_max_texture_side(2048);
    let (shared, prober) = show_with(cues);
    let mut app = App::new(shared.clone(), prober);
    let ctx = egui::Context::default();
    super::theme::install(&ctx);
    let first = shared.lock().ws.cue_list.cues[0].id;
    app.select(&mut shared.lock(), Some(first));
    app.inspector_tab = if settings_tab { inspector::Tab::Settings } else { inspector::Tab::Basics };
    // Warm up: fonts, layout caches, the prober's first answers.
    for _ in 0..10 {
        frame(&ctx, &mut app, &shared);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let waits = {
        let (shared, stop) = (shared.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut worst = Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                drop(shared.lock());
                worst = worst.max(t.elapsed());
                std::thread::sleep(Duration::from_millis(1));
            }
            worst
        })
    };
    let frames = (0..frames).map(|_| frame(&ctx, &mut app, &shared)).collect();
    stop.store(true, Ordering::Relaxed);
    let worst_lock_wait = waits.join().unwrap_or_default();
    shared.lock().shutdown();
    UiTimes { frames, worst_lock_wait }
}
