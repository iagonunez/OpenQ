//! Scripted runs and frame timing, for finding UI slowdowns in the real application.
//!
//! `OPENQ_DEBUG_SCRIPT="1:go,4:stop,5:close,9:quit"` performs actions at those seconds after
//! start-up: `go`, `stop`, `panic`, `close` (close every stage window), `select:<cue number>`,
//! `tab:<inspector tab>`, `preview` (toggle the geometry preview), `scrub` / `noscrub` (move the
//! Animation tab playhead every frame), `drag` / `nodrag` (edit the selected cue's first keyframe
//! every frame, like dragging it on the timeline), `cmd:<lighting command line>` (`cmd:go 1`),
//! `gm:<0..1>`, `blackout` / `noblackout`, `quit`.
//! `OPENQ_DEBUG_FRAMES=<file>` writes one CSV line per frame (seconds since start, time spent in
//! `App::ui` in ms, time since the previous frame in ms, the previous frame's whole CPU time in ms
//! including tessellation and painting) when the script quits.

use super::{App, inspector};
use crate::control::Command;
use crate::show::Show;
use std::time::Instant;

/// State of a scripted run.
#[derive(Default)]
pub(super) struct DebugRun {
    t0: Option<Instant>,
    steps: Vec<(f64, String)>,
    next: usize,
    parsed: bool,
    last_frame: Option<Instant>,
    scrub: bool,
    drag: bool,
    /// (seconds since start, ui ms, interval ms, previous frame's total CPU ms incl. painting)
    frames: Vec<(f64, f64, f64, f64)>,
}

impl App {
    /// Run due script steps (call once per frame, with the show locked).
    pub(super) fn debug_script(&mut self, ctx: &egui::Context, show: &mut Show) {
        let run = &mut self.debug_run;
        if !run.parsed {
            run.parsed = true;
            if let Ok(s) = std::env::var("OPENQ_DEBUG_SCRIPT") {
                run.steps = s
                    .split(',')
                    .filter_map(|st| {
                        let (t, a) = st.trim().split_once(':')?;
                        Some((t.parse().ok()?, a.to_string()))
                    })
                    .collect();
            }
        }
        if run.steps.is_empty() {
            return;
        }
        let t0 = *run.t0.get_or_insert_with(Instant::now);
        let now = t0.elapsed().as_secs_f64();
        ctx.request_repaint();
        while let Some((t, action)) = self.debug_run.steps.get(self.debug_run.next).cloned()
            && t <= now
        {
            self.debug_run.next += 1;
            let (verb, arg) = action.split_once(':').unwrap_or((action.as_str(), ""));
            match verb {
                "go" => self.control(show, Command::Go),
                "stop" => self.control(show, Command::Stop),
                "panic" => self.control(show, Command::Panic),
                "close" => self.stage_open.clear(),
                "scrub" | "noscrub" => self.debug_run.scrub = verb == "scrub",
                "drag" | "nodrag" => self.debug_run.drag = verb == "drag",
                "preview" => self.show_preview = !self.show_preview,
                "select" => {
                    let id = show.ws.cue_list.iter_all().iter().find(|c| c.number == arg).map(|c| c.id);
                    self.select(show, id);
                }
                "tab" => {
                    self.inspector_tab = match arg {
                        "geometry" => inspector::Tab::Geometry,
                        "animation" => inspector::Tab::Animation,
                        "settings" | "text" => inspector::Tab::Settings,
                        "audiofx" => inspector::Tab::AudioFx,
                        "videofx" => inspector::Tab::VideoFx,
                        _ => inspector::Tab::Basics,
                    }
                }
                // Masters, as the header's controls send them.
                "gm" => {
                    if let Ok(v) = arg.parse::<f32>() {
                        self.control(show, Command::Light(crate::control::LightControl::GrandMaster(v)));
                    }
                }
                "blackout" | "noblackout" => self.control(show, Command::Light(crate::control::LightControl::Blackout(verb == "blackout"))),
                // A lighting command line ("go 1", "1 thru 10 at 50").
                "cmd" => match crate::light::command::parse(arg).and_then(|c| self.run_commands(show, &c)) {
                    Ok(m) => log::info!("debug script: {arg}: {m}"),
                    Err(e) => log::warn!("debug script: {arg}: {e}"),
                },
                "quit" => {
                    self.write_frames();
                    std::process::exit(0);
                }
                other => log::warn!("debug script: unknown action {other}"),
            }
        }
        if self.debug_run.scrub {
            self.anim_edit.time = (self.anim_edit.time + 0.017) % 4.0;
        }
        if self.debug_run.drag
            && let Some(id) = self.primary()
            && let Some(cue) = show.ws.cue_list.find_mut(id)
        {
            let anim = match &mut cue.kind {
                crate::model::CueKind::Text(t) => &mut t.animation,
                crate::model::CueKind::Video(v) => &mut v.animation,
                _ => return,
            };
            if let Some(k) = anim.tracks.first_mut().and_then(|t| t.keys.first_mut()) {
                k.t = (k.t + 0.01) % 0.5;
            }
            let cue = cue.clone();
            if show.engine.instances.contains_key(&id) {
                show.media.update_live(&cue);
            }
            show.edited();
        }
    }

    /// Record how long this frame's `App::ui` took.
    pub(super) fn debug_frame_done(&mut self, start: Instant, cpu: Option<f32>) {
        let run = &mut self.debug_run;
        let Some(t0) = run.t0 else { return };
        let interval = run.last_frame.map_or(0.0, |l| (start - l).as_secs_f64() * 1e3);
        run.last_frame = Some(start);
        let cpu = cpu.map_or(0.0, |c| c as f64 * 1e3);
        run.frames.push(((start - t0).as_secs_f64(), start.elapsed().as_secs_f64() * 1e3, interval, cpu));
    }

    fn write_frames(&self) {
        let Ok(path) = std::env::var("OPENQ_DEBUG_FRAMES") else { return };
        let csv: String =
            self.debug_run.frames.iter().map(|(t, ui, gap, cpu)| format!("{t:.4},{ui:.3},{gap:.3},{cpu:.3}\n")).collect();
        if let Err(e) = std::fs::write(&path, csv) {
            log::error!("cannot write {path}: {e}");
        }
    }
}
