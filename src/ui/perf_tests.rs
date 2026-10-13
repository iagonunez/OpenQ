//! Headless frame-time tests of the whole window: they run [`App::draw`] on a plain egui context
//! (no window, no GPU) through the editing and playback flows that once made the UI lag, and
//! check that nothing heavy happens per frame. Timing checks compare phases of the same run
//! (so they hold in debug builds and on slow machines); render counts are exact.

use super::{App, inspector};
use crate::animation::{AnimProp, Animation};
use crate::control::Command;
use crate::media::Prober;
use crate::model::{AudioCue, Cue, CueKind, FadeCue, TextCue};
use crate::show::{SharedShow, Show};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One headless frame (layout + tessellation, like a real frame minus the GPU), with the engine
/// ticked first as its thread would. Returns how long the frame took.
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

/// A context with OpenQ's fonts and theme, like the real window. Headless egui allows 2048 px
/// textures, smaller than most GPUs: pictures must be made to fit (egui panics otherwise).
fn context() -> egui::Context {
    crate::media::set_max_texture_side(2048);
    let ctx = egui::Context::default();
    super::theme::install(&ctx);
    ctx
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// A show with an animated text cue (first) plus `extra` cues: memos, audio cues whose files
/// are missing (their status is looked up per row) and fades targeting the last cue (a target
/// lookup per row).
fn show_with(text: &str, extra: usize) -> (SharedShow, Arc<Prober>) {
    let prober = Prober::new();
    let mut show = Show::new(prober.clone());
    let mut anim = Animation::default();
    anim.set_key(AnimProp::X, 0.0, [-0.5, 0.0, 0.0, 0.0]);
    anim.set_key(AnimProp::X, 2.0, [0.0; 4]);
    anim.set_key(AnimProp::TextColor, 0.0, [1.0; 4]);
    anim.set_key(AnimProp::TextColor, 2.0, [1.0, 0.7, 0.1, 1.0]);
    let t = TextCue {
        text: text.into(),
        outline_width: 4.0,
        shadow_color: [0, 0, 0, 200],
        bg_color: [20, 30, 60, 220],
        animation: anim,
        ..Default::default()
    };
    let mut cue = Cue::new(CueKind::Text(t));
    cue.number = "1".into();
    show.ws.cue_list.cues.push(cue);
    let last = Cue::new(CueKind::Memo);
    for i in 0..extra {
        let kind = match i % 3 {
            0 => CueKind::Memo,
            1 => CueKind::Audio(AudioCue { path: format!("/nonexistent/openq-perf/{i}.wav").into(), ..Default::default() }),
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

/// Edit the text cue as the inspector does (the cue changes, the show is marked edited and a
/// playing cue is updated live).
fn edit_text(shared: &SharedShow, f: impl FnOnce(&mut TextCue)) {
    let mut s = shared.lock();
    let cue = &mut s.ws.cue_list.cues[0];
    if let CueKind::Text(t) = &mut cue.kind {
        f(t);
    }
    let (id, cue) = (cue.id, cue.clone());
    if s.engine.instances.contains_key(&id) {
        s.media.update_live(&cue);
    }
    s.edited();
}

/// The reported lag: editing an animation with the geometry preview open re-rendered the text
/// on every frame (scrubbing, dragging keys, changing colours). Each phase must cost about what
/// an idle frame does, and nothing may be rasterised after the first render.
#[test]
fn animation_editing_and_playback_stay_cheap() {
    let text = "headless perf: animated title";
    let (shared, prober) = show_with(text, 20);
    let mut app = App::new(shared.clone(), prober);
    let ctx = context();
    let first = shared.lock().ws.cue_list.cues[0].id;
    {
        let mut s = shared.lock();
        app.select(&mut s, Some(first));
    }
    app.inspector_tab = inspector::Tab::Animation;
    app.show_preview = true;

    // Warm up: fonts, the first text render (preview and preload), textures.
    let t0 = Instant::now();
    while crate::media::text::render_count(text) < 2 || t0.elapsed() < Duration::from_millis(300) {
        frame(&ctx, &mut app, &shared);
        assert!(t0.elapsed() < Duration::from_secs(30), "text never rendered");
        std::thread::sleep(Duration::from_millis(5));
    }
    for _ in 0..10 {
        frame(&ctx, &mut app, &shared);
    }
    let renders = crate::media::text::render_count(text);
    let idle = median((0..30).map(|_| frame(&ctx, &mut app, &shared)).collect());

    // Scrub the Animation tab playhead (the preview follows it, colours included).
    let scrub = median(
        (0..30)
            .map(|i| {
                app.anim_edit.time = i as f64 * 0.07;
                frame(&ctx, &mut app, &shared)
            })
            .collect(),
    );
    // Drag a keyframe and a colour: the cue changes every frame.
    let drag = median(
        (0..30)
            .map(|i| {
                edit_text(&shared, |t| {
                    t.animation.tracks[0].keys[0].t = i as f64 * 0.01;
                    t.color = [255, (i * 8) as u8, 0, 255];
                    t.geometry.y = i as f32 * 0.01;
                });
                frame(&ctx, &mut app, &shared)
            })
            .collect(),
    );
    assert_eq!(crate::media::text::render_count(text), renders, "scrubbing or dragging re-rendered the text");

    // Play it (with live edits), then stop.
    {
        let mut s = shared.lock();
        app.control(&mut s, Command::Go);
    }
    let playing = median(
        (0..30)
            .map(|i| {
                edit_text(&shared, |t| t.color = [255, 255, (i * 8) as u8, 255]);
                frame(&ctx, &mut app, &shared)
            })
            .collect(),
    );
    let shown = shared.lock().media.stage_layers().iter().find_map(|l| l.frame.lock().planes.as_ref().map(|p| (p.width, p.height)));
    let (w, h) = shown.expect("text never showed");
    assert!(w.max(h) <= 2048, "{w}×{h} text doesn't fit the GPU's textures");
    {
        let mut s = shared.lock();
        app.control(&mut s, Command::Panic);
    }
    let stopped = median((0..30).map(|_| frame(&ctx, &mut app, &shared)).collect());
    // GO used the preloaded render: nothing new was rasterised for playback or live edits.
    assert_eq!(crate::media::text::render_count(text), renders, "playback or live edits re-rendered the text");

    // Rendering this text takes several milliseconds (much more in debug builds), so a phase
    // that re-renders per frame blows far past these bounds.
    let bound = idle * 3 + Duration::from_millis(4);
    for (name, d) in [("scrub", scrub), ("drag", drag), ("playing", playing), ("stopped", stopped)] {
        assert!(d <= bound, "{name} frames take {d:?}; idle {idle:?}");
    }
    // A changed look renders once, in the background, and the frame doesn't wait for it.
    edit_text(&shared, |t| t.size = 60.0);
    let d = frame(&ctx, &mut app, &shared);
    assert!(d <= bound, "a look change blocked the frame for {d:?}");
}

/// Per-frame UI work must not grow badly with the size of the show: lists, menus and the
/// inspector run every frame, so anything that touches every cue shows up here (a lookup by id
/// per row once made a 3000-cue show take 30 ms per frame).
#[test]
fn frame_time_scales_with_cue_count() {
    let run = |extra: usize, tab: inspector::Tab| {
        let (shared, prober) = show_with(&format!("scaling test {extra} {tab:?}"), extra);
        let mut app = App::new(shared.clone(), prober);
        let ctx = context();
        let first = shared.lock().ws.cue_list.cues[0].id;
        {
            let mut s = shared.lock();
            app.select(&mut s, Some(first));
        }
        app.inspector_tab = tab;
        for _ in 0..10 {
            frame(&ctx, &mut app, &shared);
        }
        median((0..20).map(|_| frame(&ctx, &mut app, &shared)).collect())
    };
    for tab in [inspector::Tab::Basics, inspector::Tab::Settings, inspector::Tab::Animation] {
        let small = run(20, tab);
        let large = run(3000, tab);
        eprintln!("{tab:?}: 20 cues {small:?}, 3000 cues {large:?} per frame");
        assert!(large <= small * 3 + Duration::from_millis(3), "{tab:?}: 3000 cues take {large:?} per frame; 20 cues {small:?}");
    }
}

/// The engine thread needs the show lock every millisecond. The UI frame lets it in between
/// panels, so the engine waits for one panel at most, never a whole frame.
#[test]
fn engine_waits_for_one_panel_not_a_frame() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (shared, prober) = show_with("lock wait test", 3000);
    let mut app = App::new(shared.clone(), prober);
    let ctx = context();
    for _ in 0..5 {
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
    let frames: Vec<Duration> = (0..20).map(|_| frame(&ctx, &mut app, &shared)).collect();
    stop.store(true, Ordering::Relaxed);
    let worst_wait = waits.join().unwrap();
    let frame_time = median(frames);
    eprintln!("3000 cues: frame {frame_time:?}, engine waited at most {worst_wait:?}");
    assert!(worst_wait < frame_time.mul_f32(0.75), "engine waited {worst_wait:?}; a frame takes {frame_time:?}");
}
