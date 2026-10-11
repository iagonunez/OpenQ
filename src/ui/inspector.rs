//! Inspector: edit the selected cue's properties.

use super::widgets::{db_edit, fmt_time, time_edit};
use super::{App, Deferred};
use crate::media::{Prober, effective_duration};
use crate::model::{ContinueMode, Cue, CueColor, CueId, CueKind, FadeCurve, GroupMode, MIN_DB, VideoFit};
use crate::show::Show;
use egui::{ComboBox, DragValue, RichText, Slider, TextEdit, Ui};
use std::time::Instant;

impl App {
    pub(super) fn inspector(&mut self, ui: &mut Ui, show: &mut Show) {
        let Some(id) = self.primary() else {
            ui.centered_and_justified(|ui| ui.label(RichText::new("No cue selected").weak()));
            return;
        };
        if self.selection.len() > 1 {
            ui.label(RichText::new(format!("{} cues selected — editing the last one", self.selection.len())).weak());
        }
        let options: Vec<(CueId, String)> = show
            .ws
            .cue_list
            .iter_all()
            .iter()
            .filter(|c| c.id != id)
            .map(|c| (c.id, format!("{} {}", c.number, c.display_name()).trim().to_string()))
            .collect();
        let prober = self.prober.clone();
        let Some(cue) = show.ws.cue_list.find_mut(id) else { return };
        let before = cue.clone();
        let mut pick_media = false;

        egui::ScrollArea::vertical().id_salt("inspector").auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(format!("{} {} cue", cue.kind.tag().icon(), cue.kind.tag().label()));
            });
            ui.separator();
            ui.columns(2, |cols| {
                basics(&mut cols[0], cue);
                kind_editor(&mut cols[1], cue, &options, &prober, &mut pick_media);
            });
        });

        if pick_media {
            self.deferred.push(Deferred::PickMedia(id));
        }
        if *cue != before {
            let cue = cue.clone();
            if let Some(p) = cue.kind.media_path() {
                prober.get(p);
            }
            apply_live_changes(show, &before, &cue);
            show.edited();
        }
    }
}

/// Push level/opacity edits to a cue that is currently playing.
fn apply_live_changes(show: &mut Show, before: &Cue, after: &Cue) {
    use crate::engine::{Backend, FadeRequest};
    if !show.engine.instances.contains_key(&after.id) {
        return;
    }
    let (vol, op) = match (&before.kind, &after.kind) {
        (CueKind::Audio(a), CueKind::Audio(b)) => ((a.volume_db != b.volume_db).then_some(b.volume_db), None),
        (CueKind::Video(a), CueKind::Video(b)) => (
            (a.volume_db != b.volume_db).then_some(b.volume_db),
            (a.opacity != b.opacity).then_some(b.opacity),
        ),
        _ => (None, None),
    };
    if vol.is_some() || op.is_some() {
        let req = FadeRequest { volume_db: vol, opacity: op, duration: 0.05, curve: FadeCurve::Linear };
        show.media.fade(after.id, &req, Instant::now());
    }
}

fn basics(ui: &mut Ui, cue: &mut Cue) {
    egui::Grid::new("basics").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        ui.label("Number");
        ui.add(TextEdit::singleline(&mut cue.number).desired_width(80.0));
        ui.end_row();

        ui.label("Name");
        let hint = cue.display_name();
        ui.add(TextEdit::singleline(&mut cue.name).hint_text(hint).desired_width(f32::INFINITY));
        ui.end_row();

        ui.label("Pre-wait");
        time_edit(ui, &mut cue.pre_wait);
        ui.end_row();

        ui.label("Continue");
        ComboBox::from_id_salt("continue")
            .selected_text(cue.continue_mode.label())
            .show_ui(ui, |ui| {
                for m in ContinueMode::ALL {
                    ui.selectable_value(&mut cue.continue_mode, m, m.label());
                }
            });
        ui.end_row();

        ui.label("Post-wait");
        ui.add_enabled_ui(cue.continue_mode != ContinueMode::AutoFollow, |ui| {
            time_edit(ui, &mut cue.post_wait)
                .on_hover_text("Time after the action starts before the next cue auto-continues");
        });
        ui.end_row();

        ui.label("Armed");
        ui.checkbox(&mut cue.armed, "")
            .on_hover_text("Disarmed cues skip their action but still run waits and continues");
        ui.end_row();

        ui.label("Color");
        ComboBox::from_id_salt("color").selected_text(cue.color.label()).show_ui(ui, |ui| {
            for c in CueColor::ALL {
                ui.selectable_value(&mut cue.color, c, c.label());
            }
        });
        ui.end_row();
    });
    ui.add_space(6.0);
    ui.label("Notes");
    ui.add(TextEdit::multiline(&mut cue.notes).desired_rows(3).desired_width(f32::INFINITY));
}

fn target_combo(ui: &mut Ui, salt: &str, target: &mut Option<CueId>, options: &[(CueId, String)], none_label: &str) {
    let selected = target
        .and_then(|t| options.iter().find(|o| o.0 == t))
        .map(|o| o.1.clone())
        .unwrap_or_else(|| if target.is_some() { "⚠ missing cue".into() } else { none_label.into() });
    ComboBox::from_id_salt(salt).width(220.0).selected_text(selected).show_ui(ui, |ui| {
        ui.selectable_value(target, None, none_label);
        for (id, label) in options {
            ui.selectable_value(target, Some(*id), label);
        }
    });
}

fn file_row(ui: &mut Ui, path: &std::path::Path, prober: &Prober, pick: &mut bool) -> Option<f64> {
    let mut dur = None;
    ui.label("File");
    ui.horizontal(|ui| {
        if ui.button("Browse…").clicked() {
            *pick = true;
        }
        if path.as_os_str().is_empty() {
            ui.label(RichText::new("none").weak());
        } else {
            let name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let color = if path.exists() { ui.visuals().text_color() } else { super::widgets::RED };
            ui.label(RichText::new(name).color(color)).on_hover_text(path.display().to_string());
        }
    });
    ui.end_row();
    if !path.as_os_str().is_empty() {
        ui.label("");
        match prober.get(path) {
            Some(Ok(i)) => {
                dur = i.duration;
                ui.label(RichText::new(format!(
                    "{} · {}",
                    i.duration.map(fmt_time).unwrap_or_else(|| "unknown length".into()),
                    i.detail
                ))
                .weak());
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(super::widgets::RED));
            }
            None => {
                ui.label(RichText::new(if path.exists() { "reading…" } else { "file not found" }).weak());
            }
        }
        ui.end_row();
    }
    dur
}

fn plays_row(ui: &mut Ui, play_count: &mut u32, infinite: &mut bool) {
    ui.label("Plays");
    ui.horizontal(|ui| {
        ui.add_enabled(!*infinite, DragValue::new(play_count).range(1..=9999).suffix("×"));
        ui.checkbox(infinite, "Loop forever");
    });
    ui.end_row();
}

fn kind_editor(ui: &mut Ui, cue: &mut Cue, options: &[(CueId, String)], prober: &Prober, pick: &mut bool) {
    let snapshot = cue.clone();
    match &mut cue.kind {
        CueKind::Audio(a) => {
            egui::Grid::new("audio").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                let file_dur = file_row(ui, &a.path, prober, pick);
                ui.label("Level");
                db_edit(ui, &mut a.volume_db);
                ui.end_row();
                ui.label("Pan");
                ui.add(Slider::new(&mut a.pan, -1.0..=1.0).custom_formatter(|v, _| {
                    if v.abs() < 0.005 { "C".into() } else if v < 0.0 { format!("L{:.0}", -v * 100.0) } else { format!("R{:.0}", v * 100.0) }
                }));
                ui.end_row();
                ui.label("Start time");
                time_edit(ui, &mut a.start_time);
                ui.end_row();
                ui.label("End time");
                ui.horizontal(|ui| {
                    let mut has_end = a.end_time.is_some();
                    if ui.checkbox(&mut has_end, "").changed() {
                        a.end_time = if has_end { Some(file_dur.unwrap_or(a.start_time + 10.0)) } else { None };
                    }
                    match &mut a.end_time {
                        Some(e) => {
                            time_edit(ui, e);
                        }
                        None => {
                            ui.label(RichText::new("end of file").weak());
                        }
                    }
                });
                ui.end_row();
                plays_row(ui, &mut a.play_count, &mut a.infinite_loop);
                ui.label("Duration");
                ui.label(effective_duration(&snapshot, file_dur).map(fmt_time).unwrap_or_else(|| "∞ / unknown".into()));
                ui.end_row();
            });
        }
        CueKind::Video(v) => {
            egui::Grid::new("video").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                let file_dur = file_row(ui, &v.path, prober, pick);
                ui.label("Audio level");
                db_edit(ui, &mut v.volume_db);
                ui.end_row();
                ui.label("Opacity");
                ui.add(Slider::new(&mut v.opacity, 0.0..=1.0));
                ui.end_row();
                ui.label("Layer");
                ui.add(DragValue::new(&mut v.layer).range(-100..=100)).on_hover_text("Higher layers draw on top");
                ui.end_row();
                ui.label("Fit");
                ComboBox::from_id_salt("fit").selected_text(v.fit.label()).show_ui(ui, |ui| {
                    for f in VideoFit::ALL {
                        ui.selectable_value(&mut v.fit, f, f.label());
                    }
                });
                ui.end_row();
                ui.label("Start time");
                time_edit(ui, &mut v.start_time);
                ui.end_row();
                plays_row(ui, &mut v.play_count, &mut v.infinite_loop);
                ui.label("At end");
                ui.checkbox(&mut v.hold_last_frame, "Hold last frame until stopped");
                ui.end_row();
                ui.label("Duration");
                ui.label(effective_duration(&snapshot, file_dur).map(fmt_time).unwrap_or_else(|| "∞ / unknown".into()));
                ui.end_row();
            });
        }
        CueKind::Group { mode, children } => {
            egui::Grid::new("group").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Mode");
                ComboBox::from_id_salt("gmode").width(300.0).selected_text(mode.label()).show_ui(ui, |ui| {
                    for m in GroupMode::ALL {
                        ui.selectable_value(mode, m, m.label());
                    }
                });
                ui.end_row();
                ui.label("Children");
                ui.label(format!("{}", children.len()));
                ui.end_row();
            });
            ui.label(RichText::new("Drag cues onto the middle of a group row to put them inside it.").weak());
        }
        CueKind::Wait { duration } => {
            egui::Grid::new("wait").num_columns(2).show(ui, |ui| {
                ui.label("Duration");
                time_edit(ui, duration);
                ui.end_row();
            });
        }
        CueKind::Memo => {
            ui.label(RichText::new("Memo cues do nothing on their own — use them for notes and as chain points.").weak());
        }
        CueKind::Fade(f) => {
            egui::Grid::new("fade").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Target");
                target_combo(ui, "fade_target", &mut f.target, options, "— none —");
                ui.end_row();
                ui.label("Duration");
                time_edit(ui, &mut f.duration);
                ui.end_row();
                ui.label("Curve");
                ComboBox::from_id_salt("curve").selected_text(f.curve.label()).show_ui(ui, |ui| {
                    for c in FadeCurve::ALL {
                        ui.selectable_value(&mut f.curve, c, c.label());
                    }
                });
                ui.end_row();
                ui.label("Level");
                ui.horizontal(|ui| {
                    let mut on = f.volume_db.is_some();
                    if ui.checkbox(&mut on, "").changed() {
                        f.volume_db = on.then_some(MIN_DB);
                    }
                    if let Some(db) = &mut f.volume_db {
                        db_edit(ui, db);
                    } else {
                        ui.label(RichText::new("unchanged").weak());
                    }
                });
                ui.end_row();
                ui.label("Opacity");
                ui.horizontal(|ui| {
                    let mut on = f.opacity.is_some();
                    if ui.checkbox(&mut on, "").changed() {
                        f.opacity = on.then_some(0.0);
                    }
                    if let Some(o) = &mut f.opacity {
                        ui.add(Slider::new(o, 0.0..=1.0));
                    } else {
                        ui.label(RichText::new("unchanged").weak());
                    }
                });
                ui.end_row();
                ui.label("When done");
                ui.checkbox(&mut f.stop_target_when_done, "Stop target");
                ui.end_row();
            });
        }
        CueKind::Start { target } => {
            egui::Grid::new("tgt").num_columns(2).show(ui, |ui| {
                ui.label("Start");
                target_combo(ui, "start_target", target, options, "— none —");
                ui.end_row();
            });
        }
        CueKind::Stop { target } => {
            egui::Grid::new("tgt").num_columns(2).show(ui, |ui| {
                ui.label("Stop");
                target_combo(ui, "stop_target", target, options, "All cues");
                ui.end_row();
            });
        }
        CueKind::Pause { target } => {
            egui::Grid::new("tgt").num_columns(2).show(ui, |ui| {
                ui.label("Pause");
                target_combo(ui, "pause_target", target, options, "All cues");
                ui.end_row();
            });
        }
        CueKind::Goto { target } => {
            egui::Grid::new("tgt").num_columns(2).show(ui, |ui| {
                ui.label("Move playhead to");
                target_combo(ui, "goto_target", target, options, "— none —");
                ui.end_row();
            });
        }
    }
}
