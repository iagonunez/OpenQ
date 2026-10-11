//! Inspector: edit the selected cue's properties, organised in tabs.

use super::eq::eq_editor;
use super::waveform::waveform_editor;
use super::widgets::{RED, db_edit, fmt_time, time_edit};
use super::{App, Deferred};
use crate::media::{MediaInfo, Prober, Waveforms, effective_duration, is_image_path};
use crate::model::{
    AudioCue, ContinueMode, Cue, CueColor, CueId, CueKind, FadeCurve, Geometry, GroupMode, MIN_DB, StageId, VideoCue,
    VideoFit, default_routing, gain_to_db,
};
use crate::show::Show;
use egui::{ComboBox, DragValue, RichText, Slider, TextEdit, Ui};
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) enum Tab {
    #[default]
    Basics,
    Time,
    Levels,
    Eq,
    Geometry,
    Settings,
}

fn tabs_for(kind: &CueKind) -> &'static [(Tab, &'static str)] {
    match kind {
        CueKind::Audio(_) => &[
            (Tab::Basics, "Basics"),
            (Tab::Time, "Time & Loops"),
            (Tab::Levels, "Levels & Routing"),
            (Tab::Eq, "EQ"),
        ],
        CueKind::Video(_) => &[
            (Tab::Basics, "Basics"),
            (Tab::Time, "Time & Loops"),
            (Tab::Geometry, "Geometry"),
            (Tab::Levels, "Audio"),
        ],
        CueKind::Memo => &[(Tab::Basics, "Basics")],
        _ => &[(Tab::Basics, "Basics"), (Tab::Settings, "Settings")],
    }
}

/// Everything the editors need that lives outside the cue being edited.
struct Ctx<'a> {
    options: &'a [(CueId, String)],
    geometries: &'a HashMap<CueId, Geometry>,
    stages: &'a [(StageId, String)],
    out_channels: usize,
    prober: &'a Prober,
    waveforms: &'a Waveforms,
    /// Wall-clock seconds this cue has been playing, if it is.
    elapsed: Option<f64>,
}

impl App {
    pub(super) fn inspector(&mut self, ui: &mut Ui, show: &mut Show) {
        let Some(id) = self.primary() else {
            ui.centered_and_justified(|ui| ui.label(RichText::new("No cue selected").weak()));
            return;
        };
        let list = &show.ws.cue_list;
        let options: Vec<(CueId, String)> = list
            .iter_all()
            .iter()
            .filter(|c| c.id != id)
            .map(|c| (c.id, format!("{} {}", c.number, c.display_name()).trim().to_string()))
            .collect();
        let geometries: HashMap<CueId, Geometry> = list
            .iter_all()
            .iter()
            .filter_map(|c| match &c.kind {
                CueKind::Video(v) => Some((c.id, v.geometry)),
                _ => None,
            })
            .collect();
        let stages: Vec<(StageId, String)> = show.ws.stages.iter().map(|s| (s.id, s.name.clone())).collect();
        let elapsed = show.engine.instances.get(&id).and_then(|_| show.media.elapsed(id));
        let ctx = Ctx {
            options: &options,
            geometries: &geometries,
            stages: &stages,
            out_channels: show.media.audio.as_ref().map_or(2, |a| a.channels),
            prober: &self.prober,
            waveforms: &self.waveforms,
            elapsed,
        };
        let Some(cue) = show.ws.cue_list.find_mut(id) else { return };
        let before = cue.clone();
        let mut pick_media = false;

        let tabs = tabs_for(&cue.kind);
        if !tabs.iter().any(|t| t.0 == self.inspector_tab) {
            self.inspector_tab = Tab::Basics;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {} {}", cue.kind.tag().icon(), cue.number, cue.display_name())).strong());
            ui.separator();
            for (t, label) in tabs {
                ui.selectable_value(&mut self.inspector_tab, *t, *label);
            }
            if self.selection.len() > 1 {
                ui.label(RichText::new(format!("({} selected — editing the last)", self.selection.len())).weak());
            }
        });
        ui.separator();

        let tab = self.inspector_tab;
        egui::ScrollArea::vertical().id_salt("inspector").auto_shrink([false, false]).show(ui, |ui| match tab {
            Tab::Basics => basics(ui, cue),
            Tab::Settings => settings_tab(ui, cue, &ctx),
            Tab::Time => match &mut cue.kind {
                CueKind::Audio(a) => audio_time(ui, id, a, &ctx, &mut pick_media),
                CueKind::Video(v) => video_time(ui, v, &ctx, &mut pick_media),
                _ => {}
            },
            Tab::Levels => match &mut cue.kind {
                CueKind::Audio(a) => audio_levels(ui, a, &ctx),
                CueKind::Video(v) => {
                    egui::Grid::new("vlevels").num_columns(2).show(ui, |ui| {
                        ui.label("Audio level");
                        db_edit(ui, &mut v.volume_db);
                        ui.end_row();
                    });
                    ui.add_space(8.0);
                    match ctx.prober.get(&v.path).and_then(|r| r.ok()) {
                        Some(i) if i.channels == 0 => {
                            ui.label(RichText::new("This file has no soundtrack.").weak());
                        }
                        info => {
                            let src = info.map_or(2, |i| i.channels).clamp(1, 2);
                            routing_editor(ui, &mut v.routing, src, ctx.out_channels.min(crate::model::MAX_OUT_CHANNELS));
                            ui.label(RichText::new("The soundtrack plays through the OpenQ mixer (stereo).").weak());
                        }
                    }
                }
                _ => {}
            },
            Tab::Eq => {
                if let CueKind::Audio(a) = &mut cue.kind {
                    eq_editor(ui, ui.id().with(("eq", id)), &mut a.eq);
                }
            }
            Tab::Geometry => {
                if let CueKind::Video(v) = &mut cue.kind {
                    video_geometry(ui, v, &ctx);
                }
            }
        });

        if pick_media {
            self.deferred.push(Deferred::PickMedia(id));
        }
        if *cue != before {
            let cue = cue.clone();
            if show.engine.instances.contains_key(&id) {
                show.media.update_live(&cue);
            }
            show.edited();
        }
    }
}

fn basics(ui: &mut Ui, cue: &mut Cue) {
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
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
            ComboBox::from_id_salt("continue").selected_text(cue.continue_mode.label()).show_ui(ui, |ui| {
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

            ui.label("Hotkey");
            ui.horizontal(|ui| {
                let mut text = cue.hotkey.clone().unwrap_or_default();
                let valid = text.is_empty() || egui::Key::from_name(&text).is_some();
                let r = ui.add(
                    TextEdit::singleline(&mut text)
                        .desired_width(60.0)
                        .hint_text("none")
                        .text_color_opt((!valid).then_some(super::widgets::RED)),
                );
                if r.changed() {
                    cue.hotkey = (!text.trim().is_empty()).then(|| text.trim().to_string());
                }
                r.on_hover_text("Key that fires this cue (e.g. F1, A, Num5). Not Space/P/R/Esc.");
            });
            ui.end_row();

            ui.label("Color");
            ComboBox::from_id_salt("color").selected_text(cue.color.label()).show_ui(ui, |ui| {
                for c in CueColor::ALL {
                    ui.selectable_value(&mut cue.color, c, c.label());
                }
            });
            ui.end_row();
        });
        let ui = &mut cols[1];
        ui.label("Notes");
        ui.add(TextEdit::multiline(&mut cue.notes).desired_rows(8).desired_width(f32::INFINITY));
    });
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

/// File picker row inside a grid; returns the probe result if known.
fn file_row(ui: &mut Ui, path: &Path, prober: &Prober, pick: &mut bool) -> Option<MediaInfo> {
    let mut info = None;
    ui.label("File");
    ui.horizontal(|ui| {
        if ui.button("Browse…").clicked() {
            *pick = true;
        }
        if path.as_os_str().is_empty() {
            ui.label(RichText::new("none").weak());
        } else {
            let name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let color = if path.exists() { ui.visuals().text_color() } else { RED };
            ui.label(RichText::new(name).color(color)).on_hover_text(path.display().to_string());
        }
    });
    ui.end_row();
    if !path.as_os_str().is_empty() {
        ui.label("");
        match prober.get(path) {
            Some(Ok(i)) => {
                ui.label(
                    RichText::new(format!(
                        "{} · {}",
                        i.duration.map(fmt_time).unwrap_or_else(|| if i.still { "still".into() } else { "unknown length".into() }),
                        i.detail
                    ))
                    .weak(),
                );
                info = Some(i);
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(RED));
            }
            None => {
                ui.label(RichText::new(if path.exists() { "reading…" } else { "file not found" }).weak());
            }
        }
        ui.end_row();
    }
    info
}

fn plays_row(ui: &mut Ui, play_count: &mut u32, infinite: &mut bool) {
    ui.label("Plays");
    ui.horizontal(|ui| {
        ui.add_enabled(!*infinite, DragValue::new(play_count).range(1..=9999).suffix("×"));
        ui.checkbox(infinite, "Loop forever");
    });
    ui.end_row();
}

fn end_row(ui: &mut Ui, end: &mut Option<f64>, start: f64, file_dur: Option<f64>) {
    ui.label("End time");
    ui.horizontal(|ui| {
        let mut has_end = end.is_some();
        if ui.checkbox(&mut has_end, "").changed() {
            *end = if has_end { Some(file_dur.unwrap_or(start + 10.0)) } else { None };
        }
        match end {
            Some(e) => {
                time_edit(ui, e);
            }
            None => {
                ui.label(RichText::new("end of file").weak());
            }
        }
    });
    ui.end_row();
}

fn audio_time(ui: &mut Ui, id: CueId, a: &mut AudioCue, ctx: &Ctx, pick: &mut bool) {
    let mut file_dur = None;
    egui::Grid::new("afile").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        file_dur = file_row(ui, &a.path, ctx.prober, pick).and_then(|i| i.duration);
    });
    if !a.path.as_os_str().is_empty() && a.path.exists() {
        let wf = ctx.waveforms.get(&a.path);
        let wf_ref = wf.as_ref().and_then(|r| r.as_ref().ok()).cloned();
        let duration = file_dur.or(wf_ref.as_ref().map(|w| w.duration)).unwrap_or(0.0);
        if duration > 0.0 {
            // Map wall-clock elapsed to a position in the file (rate + loops).
            let playhead = ctx.elapsed.map(|e| a.file_position(e * a.rate as f64, duration));
            if let Some(Err(e)) = &wf {
                ui.label(RichText::new(format!("waveform: {e}")).color(RED));
            }
            waveform_editor(
                ui,
                ui.id().with(("wave", id)),
                wf_ref.as_deref(),
                duration,
                &mut a.start_time,
                &mut a.end_time,
                &mut a.slices,
                playhead,
            );
            ui.label(
                RichText::new(
                    "Drag green/red markers to trim · double-click to add a slice · drag the waveform to pan · Ctrl+scroll to zoom",
                )
                .weak()
                .small(),
            );
            slice_table(ui, a);
        }
    }
    ui.add_space(6.0);
    egui::Grid::new("atime").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        ui.label("Start time");
        time_edit(ui, &mut a.start_time);
        ui.end_row();
        end_row(ui, &mut a.end_time, a.start_time, file_dur);
        plays_row(ui, &mut a.play_count, &mut a.infinite_loop);
        ui.label("Rate");
        ui.horizontal(|ui| {
            ui.add(DragValue::new(&mut a.rate).range(0.25..=4.0).speed(0.005).max_decimals(3).suffix("×"))
                .on_hover_text("Playback speed; pitch follows (varispeed). Applies the next time the cue starts.");
            if ui.small_button("1×").clicked() {
                a.rate = 1.0;
            }
            let semis = 12.0 * (a.rate.max(1e-3)).log2();
            ui.label(RichText::new(format!("{semis:+.2} semitones")).weak());
        });
        ui.end_row();
        ui.label("Duration");
        let probe = Cue::new(CueKind::Audio(a.clone()));
        ui.label(effective_duration(&probe, file_dur).map(fmt_time).unwrap_or_else(|| "∞ / unknown".into()));
        ui.end_row();
    });
}

/// Slices: each marker ends a section that plays N times (∞ = until a Devamp cue).
fn slice_table(ui: &mut Ui, a: &mut AudioCue) {
    if a.slices.is_empty() {
        return;
    }
    ui.add_space(4.0);
    ui.label(RichText::new("Slices").strong());
    let mut remove = None;
    let mut prev = a.start_time;
    egui::Grid::new("slices").num_columns(4).spacing([10.0, 4.0]).show(ui, |ui| {
        for (i, m) in a.slices.iter_mut().enumerate() {
            ui.label(RichText::new(format!("{} – ", fmt_time(prev))).weak());
            time_edit(ui, &mut m.time);
            ui.horizontal(|ui| {
                let mut forever = m.plays == 0;
                ui.add_enabled(!forever, DragValue::new(&mut m.plays).range(1..=9999).suffix("×"));
                if ui.checkbox(&mut forever, "loop until devamp").changed() {
                    m.plays = if forever { 0 } else { 1 };
                }
            });
            if ui.small_button("🗑").clicked() {
                remove = Some(i);
            }
            ui.end_row();
            prev = m.time;
        }
        ui.label(RichText::new(format!("{} – end", fmt_time(prev))).weak());
        ui.label(RichText::new("last slice uses Plays below").weak());
        ui.end_row();
    });
    if let Some(i) = remove {
        a.slices.remove(i);
    }
}

fn audio_levels(ui: &mut Ui, a: &mut AudioCue, ctx: &Ctx) {
    egui::Grid::new("alevels").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        ui.label("Level");
        db_edit(ui, &mut a.volume_db);
        ui.end_row();
        ui.label("Pan");
        ui.add(Slider::new(&mut a.pan, -1.0..=1.0).custom_formatter(|v, _| {
            if v.abs() < 0.005 {
                "C".into()
            } else if v < 0.0 {
                format!("L{:.0}", -v * 100.0)
            } else {
                format!("R{:.0}", v * 100.0)
            }
        }));
        ui.end_row();
    });
    ui.add_space(8.0);
    let src = ctx.prober.get(&a.path).and_then(|r| r.ok()).map_or(2, |i| i.channels.max(1)).min(crate::model::MAX_SRC_CHANNELS);
    routing_editor(ui, &mut a.routing, src, ctx.out_channels.min(crate::model::MAX_OUT_CHANNELS));
}

fn routing_editor(ui: &mut Ui, routing: &mut Option<Vec<Vec<f32>>>, src: usize, outs: usize) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Routing matrix").strong());
        let mut custom = routing.is_some();
        if ui.checkbox(&mut custom, "Custom").changed() {
            *routing = custom.then(|| {
                default_routing(src, outs)
                    .into_iter()
                    .map(|row| row.into_iter().map(|g| if g > 0.0 { gain_to_db(g) } else { MIN_DB }).collect())
                    .collect()
            });
        }
        ui.label(RichText::new(format!("{src} in, {outs} out")).weak());
    });
    let Some(m) = routing else {
        ui.label(RichText::new("Automatic: mono feeds outputs 1+2, multichannel maps 1:1.").weak());
        return;
    };
    // Keep the matrix sized to the file and device.
    m.resize_with(src, || vec![MIN_DB; outs]);
    for row in m.iter_mut() {
        row.resize(outs, MIN_DB);
    }
    egui::ScrollArea::horizontal().id_salt("routing").show(ui, |ui| {
        egui::Grid::new("matrix").spacing([4.0, 4.0]).show(ui, |ui| {
            ui.label("");
            for o in 0..outs {
                ui.label(RichText::new(format!("Out {}", o + 1)).weak());
            }
            ui.end_row();
            for (s, row) in m.iter_mut().enumerate() {
                ui.label(RichText::new(format!("In {}", s + 1)).weak());
                for g in row.iter_mut() {
                    ui.add(
                        DragValue::new(g)
                            .range(MIN_DB..=12.0)
                            .speed(0.25)
                            .custom_formatter(|v, _| if v <= MIN_DB as f64 { "-∞".into() } else { format!("{v:.1}") }),
                    )
                    .on_hover_text("Crosspoint level in dB (drag to -60 for off)");
                }
                ui.end_row();
            }
        });
    });
    ui.horizontal(|ui| {
        if ui.small_button("All off").clicked() {
            m.iter_mut().flatten().for_each(|g| *g = MIN_DB);
        }
        if ui.small_button("Reset to default").clicked() {
            *m = default_routing(src, outs)
                .into_iter()
                .map(|row| row.into_iter().map(|g| if g > 0.0 { gain_to_db(g) } else { MIN_DB }).collect())
                .collect();
        }
    });
}

fn video_time(ui: &mut Ui, v: &mut VideoCue, ctx: &Ctx, pick: &mut bool) {
    egui::Grid::new("vtime").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        let info = file_row(ui, &v.path, ctx.prober, pick);
        let file_dur = info.as_ref().and_then(|i| i.duration);
        if is_image_path(&v.path) {
            ui.label("Show for");
            ui.horizontal(|ui| {
                time_edit(ui, &mut v.still_duration);
                ui.label(RichText::new(if v.still_duration <= 0.0 { "until stopped" } else { "" }).weak());
            });
            ui.end_row();
        } else {
            ui.label("Start time");
            time_edit(ui, &mut v.start_time);
            ui.end_row();
            end_row(ui, &mut v.end_time, v.start_time, file_dur);
            plays_row(ui, &mut v.play_count, &mut v.infinite_loop);
        }
        ui.label("At end");
        ui.checkbox(&mut v.hold_last_frame, "Hold last frame until stopped");
        ui.end_row();
        ui.label("Duration");
        let probe = Cue::new(CueKind::Video(v.clone()));
        ui.label(effective_duration(&probe, file_dur).map(fmt_time).unwrap_or_else(|| "∞ / until stopped".into()));
        ui.end_row();
    });
}

fn stage_combo(ui: &mut Ui, salt: &str, stage: &mut Option<StageId>, stages: &[(StageId, String)]) {
    let label = stage
        .and_then(|s| stages.iter().find(|x| x.0 == s))
        .map(|s| s.1.clone())
        .unwrap_or_else(|| stages.first().map(|s| format!("{} (default)", s.1)).unwrap_or_default());
    ComboBox::from_id_salt(salt).selected_text(label).show_ui(ui, |ui| {
        ui.selectable_value(stage, None, "Default (first stage)");
        for (id, name) in stages {
            ui.selectable_value(stage, Some(*id), name);
        }
    });
}

fn video_geometry(ui: &mut Ui, v: &mut VideoCue, ctx: &Ctx) {
    egui::Grid::new("vgeo").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        ui.label("Stage");
        stage_combo(ui, "vstage", &mut v.stage, ctx.stages);
        ui.end_row();
        ui.label("Fit");
        ComboBox::from_id_salt("fit").selected_text(v.fit.label()).show_ui(ui, |ui| {
            for f in VideoFit::ALL {
                ui.selectable_value(&mut v.fit, f, f.label());
            }
        });
        ui.end_row();
        ui.label("Layer");
        ui.add(DragValue::new(&mut v.layer).range(-100..=100)).on_hover_text("Higher layers draw on top");
        ui.end_row();
        ui.label("Opacity");
        ui.add(Slider::new(&mut v.opacity, 0.0..=1.0));
        ui.end_row();
    });
    ui.add_space(6.0);
    geometry_editor(ui, "vgeom", &mut v.geometry);
}

/// Position / scale / rotation editor.
fn geometry_editor(ui: &mut Ui, salt: &str, g: &mut Geometry) {
    let lock_id = ui.id().with((salt, "lock"));
    let mut lock = ui.data(|d| d.get_temp::<bool>(lock_id)).unwrap_or(true);
    let pct = |v: f64, _| format!("{:.1}%", v * 100.0);
    egui::Grid::new(salt).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        ui.label("Position");
        ui.horizontal(|ui| {
            ui.label("X");
            ui.add(DragValue::new(&mut g.x).speed(0.002).range(-2.0..=2.0).custom_formatter(pct).custom_parser(parse_pct));
            ui.label("Y");
            ui.add(DragValue::new(&mut g.y).speed(0.002).range(-2.0..=2.0).custom_formatter(pct).custom_parser(parse_pct));
        });
        ui.end_row();
        ui.label("Scale");
        ui.horizontal(|ui| {
            if lock {
                let mut s = g.scale_x;
                if ui.add(DragValue::new(&mut s).speed(0.005).range(0.0..=10.0).custom_formatter(pct).custom_parser(parse_pct)).changed() {
                    let ratio = if g.scale_x.abs() > 1e-6 { g.scale_y / g.scale_x } else { 1.0 };
                    g.scale_x = s;
                    g.scale_y = s * ratio;
                }
            } else {
                ui.label("X");
                ui.add(DragValue::new(&mut g.scale_x).speed(0.005).range(0.0..=10.0).custom_formatter(pct).custom_parser(parse_pct));
                ui.label("Y");
                ui.add(DragValue::new(&mut g.scale_y).speed(0.005).range(0.0..=10.0).custom_formatter(pct).custom_parser(parse_pct));
            }
            ui.checkbox(&mut lock, "🔒 proportional");
        });
        ui.end_row();
        ui.label("Rotation");
        ui.add(Slider::new(&mut g.rotation, -180.0..=180.0).suffix("°"));
        ui.end_row();
        ui.label("");
        if ui.button("Reset geometry").clicked() {
            *g = Geometry::default();
        }
        ui.end_row();
    });
    ui.data_mut(|d| d.insert_temp(lock_id, lock));
}

fn parse_pct(s: &str) -> Option<f64> {
    s.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|v| v / 100.0)
}

fn settings_tab(ui: &mut Ui, cue: &mut Cue, ctx: &Ctx) {
    let options = ctx.options;
    match &mut cue.kind {
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
                ui.label("Geometry");
                let mut on = f.geometry.is_some();
                if ui.checkbox(&mut on, "Animate position / scale / rotation").changed() {
                    // Start from the target's own geometry so only what you change moves.
                    f.geometry = on.then(|| f.target.and_then(|t| ctx.geometries.get(&t).copied()).unwrap_or_default());
                }
                ui.end_row();
                ui.label("When done");
                ui.checkbox(&mut f.stop_target_when_done, "Stop target");
                ui.end_row();
            });
            if let Some(g) = &mut f.geometry {
                ui.add_space(4.0);
                ui.label(RichText::new("Target geometry").strong());
                geometry_editor(ui, "fgeom", g);
            }
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
        CueKind::Devamp { target } => {
            egui::Grid::new("tgt").num_columns(2).show(ui, |ui| {
                ui.label("Devamp");
                target_combo(ui, "devamp_target", target, options, "— none —");
                ui.end_row();
            });
            ui.label(
                RichText::new("Lets the target's looping slice finish its current pass, then play on to the next slice.").weak(),
            );
        }
        CueKind::Network(n) => {
            egui::Grid::new("net").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Destination");
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut n.host).desired_width(160.0));
                    ui.label(":");
                    ui.add(DragValue::new(&mut n.port).range(1..=65535));
                });
                ui.end_row();
                ui.label("OSC address");
                ui.add(TextEdit::singleline(&mut n.address).desired_width(260.0));
                ui.end_row();
                ui.label("Arguments");
                ui.add(TextEdit::singleline(&mut n.args).desired_width(260.0).hint_text(r#"1 0.5 "text""#));
                ui.end_row();
            });
            ui.label(RichText::new("Sends one OSC message over UDP. Numbers are sent as int/float, other words as strings.").weak());
        }
        _ => {}
    }
}
