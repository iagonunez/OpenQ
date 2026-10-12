//! Inspector: edit the selected cue's properties, organised in tabs.

use super::eq::{eq_bands, eq_graph};
use super::waveform::{Overlay, waveform_editor};
use super::widgets::{RED, db_edit, fmt_time, time_edit};
use super::{App, Deferred};
use crate::media::{MediaInfo, Prober, Waveforms, effective_duration, is_image_path};
use crate::model::{
    AudioCue, ContinueMode, Cue, CueColor, CueId, CueKind, FadeCurve, Geometry, GroupMode, MIN_DB, StageId, VideoCue,
    VideoFit, BlendMode, default_routing, gain_to_db,
};
use crate::show::Show;
use egui::{ComboBox, DragValue, RichText, Slider, TextEdit, Ui};
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) enum Tab {
    #[default]
    Basics,
    Triggers,
    Time,
    Io,
    Levels,
    Eq,
    Geometry,
    Animation,
    Settings,
}

fn tabs_for(kind: &CueKind) -> &'static [(Tab, &'static str)] {
    match kind {
        CueKind::Audio(_) => &[
            (Tab::Basics, "Basics"),
            (Tab::Triggers, "Triggers"),
            (Tab::Time, "Time & Loops"),
            (Tab::Io, "I/O"),
            (Tab::Levels, "Levels"),
            (Tab::Eq, "EQ"),
        ],
        CueKind::Video(_) => &[
            (Tab::Basics, "Basics"),
            (Tab::Triggers, "Triggers"),
            (Tab::Time, "Time & Loops"),
            (Tab::Geometry, "Geometry"),
            (Tab::Animation, "Animation"),
            (Tab::Io, "I/O"),
            (Tab::Levels, "Audio"),
        ],
        CueKind::Mic(_) => &[(Tab::Basics, "Basics"), (Tab::Triggers, "Triggers"), (Tab::Io, "I/O"), (Tab::Levels, "Levels"), (Tab::Eq, "EQ")],
        CueKind::Camera(_) => &[(Tab::Basics, "Basics"), (Tab::Triggers, "Triggers"), (Tab::Settings, "Camera"), (Tab::Geometry, "Geometry")],
        CueKind::Memo => &[(Tab::Basics, "Basics"), (Tab::Triggers, "Triggers")],
        CueKind::Text(_) => &[
            (Tab::Basics, "Basics"),
            (Tab::Triggers, "Triggers"),
            (Tab::Settings, "Text"),
            (Tab::Geometry, "Geometry"),
            (Tab::Animation, "Animation"),
        ],
        _ => &[(Tab::Basics, "Basics"), (Tab::Triggers, "Triggers"), (Tab::Settings, "Settings")],
    }
}

/// Everything the editors need that lives outside the cue being edited.
/// What a Fade cue can change on a target: its level (sound), its picture (opacity and
/// geometry), both (a movie, or a group holding both kinds), or nothing.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct FadeCaps {
    level: bool,
    picture: bool,
}

impl FadeCaps {
    fn of(cue: &Cue) -> Self {
        match &cue.kind {
            CueKind::Audio(_) | CueKind::Mic(_) => Self { level: true, picture: false },
            // A still has no soundtrack.
            CueKind::Video(v) => Self { level: !crate::media::is_image_path(&v.path), picture: true },
            CueKind::Text(_) | CueKind::Camera(_) => Self { level: false, picture: true },
            CueKind::Group { children, .. } => children.iter().map(Self::of).fold(Self::default(), |a, b| Self {
                level: a.level || b.level,
                picture: a.picture || b.picture,
            }),
            _ => Self::default(),
        }
    }
}

struct Ctx<'a> {
    options: &'a [(CueId, String)],
    geometries: &'a HashMap<CueId, Geometry>,
    /// What a Fade cue can change on each cue (Settings tab only).
    fade_caps: &'a HashMap<CueId, FadeCaps>,
    stages: &'a [(StageId, String)],
    out_channels: usize,
    /// Name of the audio output device (None = no audio).
    device: Option<String>,
    prober: &'a Prober,
    waveforms: &'a Waveforms,
    /// Wall-clock seconds this cue has been playing, if it is.
    elapsed: Option<f64>,
    /// Input devices, cameras and MIDI ports.
    devices: super::devices::Devices,
    /// Timecode settings of the list holding the cue (for its trigger time).
    list_tc: crate::model::TimecodeSync,
    /// What the cue's list receives right now, if it follows timecode.
    tc_in: Option<String>,
    /// Input level of a running Mic cue (linear peak).
    mic_level: Option<f32>,
}

impl App {
    pub(super) fn inspector(&mut self, ui: &mut Ui, show: &mut Show) {
        let Some(id) = self.primary() else {
            ui.centered_and_justified(|ui| ui.label(RichText::new("No cue selected").weak()));
            return;
        };
        let list = &show.ws.cue_list;
        // Target menus and target geometry: only the Settings tab uses them, and they cover every
        // cue, so they're built only for it (in one pass over the tree).
        let all = if self.inspector_tab == Tab::Settings { list.iter_all_with_list() } else { Vec::new() };
        let options: Vec<(CueId, String)> = all
            .iter()
            .filter(|(c, _)| c.id != id)
            .map(|(c, other_list)| {
                let label = format!("{} {}", c.number, c.display_name()).trim().to_string();
                // Cues in a background list say which list they're in.
                match other_list {
                    Some(l) => (c.id, format!("{label}  ({l})")),
                    None => (c.id, label),
                }
            })
            .collect();
        let geometries: HashMap<CueId, Geometry> = all
            .iter()
            .filter_map(|(c, _)| match &c.kind {
                CueKind::Video(v) => Some((c.id, v.geometry)),
                CueKind::Text(t) => Some((c.id, t.geometry)),
                CueKind::Camera(k) => Some((c.id, k.geometry)),
                _ => None,
            })
            .collect();
        let fade_caps: HashMap<CueId, FadeCaps> = all.iter().map(|(c, _)| (c.id, FadeCaps::of(c))).collect();
        let stages: Vec<(StageId, String)> = show.ws.stages.iter().map(|s| (s.id, s.name.clone())).collect();
        let elapsed = show.engine.instances.get(&id).and_then(|_| show.media.elapsed(id));
        let ctx = Ctx {
            options: &options,
            geometries: &geometries,
            fade_caps: &fade_caps,
            stages: &stages,
            out_channels: show.media.audio.as_ref().map_or(2, |a| a.channels),
            device: show.media.audio.as_ref().map(|a| a.device_name.clone()),
            prober: &self.prober,
            waveforms: &self.waveforms,
            elapsed,
            devices: self.inputs.lock().clone(),
            list_tc: show.ws.cue_list.timecode_of(id).clone(),
            mic_level: show.media.mic_peak(id),
            tc_in: {
                let sync = show.ws.cue_list.timecode_of(id);
                show.timecode.position(sync, std::time::Instant::now()).map(|r| crate::timecode::format_secs(r.secs, r.rate))
            },
        };
        if ctx.mic_level.is_some() || ctx.tc_in.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
        }
        let Some(cue) = show.ws.cue_list.find_mut(id) else { return };
        if matches!(cue.kind, CueKind::Mic(_) | CueKind::Camera(_)) {
            super::devices::ensure(&self.inputs, ui.ctx());
        }
        let before = cue.clone();
        let mut refresh_devices = false;
        let mut pick_media = false;

        let tabs = tabs_for(&cue.kind);
        if !tabs.iter().any(|t| t.0 == self.inspector_tab) {
            self.inspector_tab = Tab::Basics;
        }
        ui.horizontal(|ui| {
            ui.label(super::theme::icon(super::theme::kind_icon(cue.kind.tag()), 18.0).color(super::theme::TEXT_DIM));
            ui.label(RichText::new(format!("{}  {}", cue.number, cue.display_name())).font(super::theme::semibold(14.0)));
            ui.separator();
            for (t, label) in tabs {
                let glyph = match t {
                    Tab::Basics => super::icons::INFO,
                    Tab::Time => super::icons::TIMER,
                    Tab::Io => super::icons::CABLE,
                    Tab::Levels => super::icons::TUNE,
                    Tab::Eq => super::icons::GRAPHIC_EQ,
                    Tab::Geometry => super::icons::OPEN_WITH,
                    Tab::Animation => super::icons::ANIMATION,
                    Tab::Settings => super::icons::SETTINGS,
                    Tab::Triggers => super::icons::KEYBOARD,
                };
                super::theme::tab(ui, &mut self.inspector_tab, *t, glyph, label);
            }
            if self.selection.len() > 1 {
                ui.label(RichText::new(format!("({} selected — editing the last)", self.selection.len())).weak());
            }
        });
        ui.separator();

        let tab = self.inspector_tab;
        egui::ScrollArea::vertical().id_salt("inspector").auto_shrink([false, false]).show(ui, |ui| match tab {
            Tab::Basics => basics(ui, cue, &ctx, &mut pick_media),
            Tab::Triggers => {
                if triggers(ui, cue, &ctx) {
                    self.show_list_timecode = true;
                }
            }
            Tab::Io => match &mut cue.kind {
                CueKind::Audio(a) => io_tab(ui, &a.path, &mut a.routing, None, &ctx, &mut pick_media),
                CueKind::Video(v) => io_tab(ui, &v.path, &mut v.routing, Some(v.stage), &ctx, &mut pick_media),
                CueKind::Mic(m) => mic_io(ui, m, &ctx, &mut refresh_devices),
                _ => {}
            },
            Tab::Settings => settings_tab(ui, cue, &ctx, &mut refresh_devices),
            Tab::Time => match &mut cue.kind {
                CueKind::Audio(a) => audio_time(ui, id, a, &ctx, &mut pick_media),
                CueKind::Video(v) => video_time(ui, v, &ctx, &mut pick_media),
                _ => {}
            },
            Tab::Levels => match &mut cue.kind {
                CueKind::Audio(a) => audio_levels(ui, a, ctx.out_channels.min(crate::model::MAX_OUT_CHANNELS)),
                CueKind::Video(v) => video_levels(ui, v, ctx.out_channels.min(crate::model::MAX_OUT_CHANNELS)),
                CueKind::Mic(m) => mic_levels(ui, m, ctx.out_channels.min(crate::model::MAX_OUT_CHANNELS)),
                _ => {}
            },
            Tab::Eq => {
                let eq = match &mut cue.kind {
                    CueKind::Audio(a) => Some(&mut a.eq),
                    CueKind::Mic(m) => Some(&mut m.eq),
                    _ => None,
                };
                if let Some(eq) = eq {
                    // Bands on the left, the response curve to their right (both edit the same EQ).
                    let specs = [
                        super::theme::Card::new(super::icons::TUNE, "Bands", 470.0),
                        super::theme::Card::new(super::icons::GRAPHIC_EQ, "Response", 320.0),
                    ];
                    let eq_id = ui.id().with(("eq", id));
                    super::theme::cards(ui, "eq", &specs, |ui, i| if i == 0 { eq_bands(ui, eq_id, eq) } else { eq_graph(ui, eq_id, eq) });
                }
            }
            Tab::Geometry => match &mut cue.kind {
                CueKind::Video(v) => {
                    let refs = LayerRefs {
                        stage: &mut v.stage,
                        fit: Some(&mut v.fit),
                        layer: &mut v.layer,
                        blend_mode: &mut v.blend_mode,
                        opacity: &mut v.opacity,
                        geometry: &mut v.geometry,
                    };
                    layer_geometry(ui, "vgeo", refs, &ctx, &mut self.show_preview);
                }
                CueKind::Camera(c) => {
                    let refs = LayerRefs {
                        stage: &mut c.stage,
                        fit: Some(&mut c.fit),
                        layer: &mut c.layer,
                        blend_mode: &mut c.blend_mode,
                        opacity: &mut c.opacity,
                        geometry: &mut c.geometry,
                    };
                    layer_geometry(ui, "cgeo", refs, &ctx, &mut self.show_preview);
                }
                CueKind::Text(t) => {
                    let refs = LayerRefs {
                        stage: &mut t.stage,
                        fit: None,
                        layer: &mut t.layer,
                        blend_mode: &mut t.blend_mode,
                        opacity: &mut t.opacity,
                        geometry: &mut t.geometry,
                    };
                    layer_geometry(ui, "tgeo", refs, &ctx, &mut self.show_preview);
                }
                _ => {}
            },
            Tab::Animation => match &mut cue.kind {
                CueKind::Video(v) => {
                    super::animation::animation_tab(ui, id, &mut v.animation, None, &mut self.anim_edit, &mut self.show_preview)
                }
                CueKind::Text(t) => {
                    let base = [t.bg_color, t.shadow_color, t.outline_color, t.color];
                    super::animation::animation_tab(ui, id, &mut t.animation, Some(base), &mut self.anim_edit, &mut self.show_preview)
                }
                _ => {}
            },
        });

        if pick_media {
            self.deferred.push(Deferred::PickMedia(id));
        }
        if refresh_devices {
            super::devices::refresh(&self.inputs, ui.ctx());
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

fn basics(ui: &mut Ui, cue: &mut Cue, ctx: &Ctx, pick: &mut bool) {
    use super::icons as ic;
    use super::theme::Card;
    let mut specs = vec![Card::new(ic::INFO, "Cue", 300.0), Card::new(ic::SCHEDULE, "Timing", 280.0)];
    let media = cue.kind.media_path().is_some();
    if media {
        specs.push(Card::new(ic::UPLOAD_FILE, "Media", 320.0));
    }
    specs.push(Card::new(ic::NOTES, "Notes", 280.0));
    let notes = specs.len() - 1;
    super::theme::cards(ui, "basics", &specs, |ui, i| match i {
        0 => {
            egui::Grid::new("basics-cue").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Number");
                ui.add(TextEdit::singleline(&mut cue.number).desired_width(80.0));
                ui.end_row();

                ui.label("Name");
                let hint = cue.display_name();
                ui.add(TextEdit::singleline(&mut cue.name).hint_text(hint).desired_width(f32::INFINITY));
                ui.end_row();

                ui.label("Color");
                ComboBox::from_id_salt("color").selected_text(cue.color.label()).show_ui(ui, |ui| {
                    for c in CueColor::ALL {
                        ui.selectable_value(&mut cue.color, c, c.label());
                    }
                });
                ui.end_row();

                ui.label("Armed");
                ui.checkbox(&mut cue.armed, "")
                    .on_hover_text("Disarmed cues skip their action but still run waits and continues");
                ui.end_row();
            });
        }
        1 => {
            egui::Grid::new("basics-timing").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Pre-wait");
                time_edit(ui, &mut cue.pre_wait).on_hover_text("Delay between GO and the cue's action");
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

            });
        }
        i if i == notes => {
            ui.add(TextEdit::multiline(&mut cue.notes).desired_rows(5).desired_width(f32::INFINITY).hint_text("Notes for the operator"));
        }
        _ => {
            if let Some(path) = cue.kind.media_path() {
                egui::Grid::new("basics-media").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    file_row(ui, path, ctx.prober, pick);
                });
            }
        }
    });
}

fn target_combo(ui: &mut Ui, salt: &str, target: &mut Option<CueId>, options: &[(CueId, String)], none_label: &str) {
    let selected = target
        .and_then(|t| options.iter().find(|o| o.0 == t))
        .map(|o| o.1.clone())
        .unwrap_or_else(|| if target.is_some() { super::theme::with_icon(super::icons::WARNING, "missing cue") } else { none_label.into() });
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
        if ui.button(super::theme::with_icon(super::icons::FOLDER_OPEN, "Browse…")).clicked() {
            *pick = true;
        }
        if path.as_os_str().is_empty() {
            ui.label(RichText::new("none").weak());
        } else {
            let name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let color = if prober.exists(path) { ui.visuals().text_color() } else { RED };
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
                ui.label(RichText::new(if prober.exists(path) { "reading…" } else { "file not found" }).weak());
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
    use super::icons as ic;
    use super::theme::Card;
    #[derive(Clone, Copy)]
    enum Part {
        Wave,
        Trim,
        Loops,
        Playback,
        Fades,
        Slices,
    }
    let file_dur = ctx.prober.get(&a.path).and_then(|r| r.ok()).and_then(|i| i.duration);
    let mut parts = vec![
        (Part::Wave, Card::new(ic::GRAPHIC_EQ, "Waveform", f32::INFINITY)),
        (Part::Trim, Card::new(ic::CROP, "Trim", 250.0)),
        (Part::Loops, Card::new(ic::REPLAY, "Loops", 250.0)),
        (Part::Playback, Card::new(ic::AV_TIMER, "Playback", 270.0)),
        (Part::Fades, Card::new(ic::TRENDING_DOWN, "Fades", 270.0)),
    ];
    if !a.slices.is_empty() {
        parts.push((Part::Slices, Card::new(ic::ALL_INCLUSIVE, "Slices", 460.0)));
    }
    let specs: Vec<Card> = parts.iter().map(|p| Card::new(p.1.glyph, p.1.title, p.1.width)).collect();
    super::theme::cards(ui, "atime", &specs, |ui, i| match parts[i].0 {
        Part::Wave => audio_waveform(ui, id, a, ctx, file_dur, pick),
        Part::Trim => {
            egui::Grid::new("atrim").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Start");
                time_edit(ui, &mut a.start_time);
                ui.end_row();
                end_row(ui, &mut a.end_time, a.start_time, file_dur);
                ui.label("Duration");
                let probe = Cue::new(CueKind::Audio(a.clone()));
                ui.label(
                    RichText::new(effective_duration(&probe, file_dur).map(fmt_time).unwrap_or_else(|| "∞ / unknown".into()))
                        .font(super::theme::mono(12.5)),
                );
                ui.end_row();
            });
        }
        Part::Loops => {
            egui::Grid::new("aloops").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                plays_row(ui, &mut a.play_count, &mut a.infinite_loop);
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Double-click the waveform to add slices that loop on their own.").weak().small());
        }
        Part::Playback => {
            egui::Grid::new("arate").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Rate");
                ui.horizontal(|ui| {
                    ui.add(DragValue::new(&mut a.rate).range(0.25..=4.0).speed(0.005).max_decimals(3).suffix("×"))
                        .on_hover_text("Playback speed; pitch follows (varispeed). Applies the next time the cue starts.");
                    if ui.small_button("1×").clicked() {
                        a.rate = 1.0;
                    }
                });
                ui.end_row();
                ui.label("Pitch");
                let semis = 12.0 * (a.rate.max(1e-3)).log2();
                ui.label(RichText::new(format!("{semis:+.2} semitones")).weak());
                ui.end_row();
            });
        }
        Part::Fades => {
            egui::Grid::new("afades").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Fade in");
                time_edit(ui, &mut a.fade_in).on_hover_text("Fades up from silence when the cue starts");
                ui.end_row();
                ui.label("Fade out");
                time_edit(ui, &mut a.fade_out).on_hover_text("Fades down to silence at the end of the cue");
                ui.end_row();
                ui.label("Curve");
                ComboBox::from_id_salt("fade-curve").selected_text(a.fade_curve.label()).width(110.0).show_ui(ui, |ui| {
                    for c in FadeCurve::ALL {
                        ui.selectable_value(&mut a.fade_curve, c, c.label());
                    }
                });
                ui.end_row();
            });
            if a.fade_out > 0.0 && (a.infinite_loop || a.slices.iter().any(|s| s.plays == 0)) {
                ui.label(
                    RichText::new("The fade-out happens once the end is known: after a Devamp, never while looping forever.")
                        .weak()
                        .small(),
                );
            }
        }
        Part::Slices => slice_table(ui, a),
    });
}

/// The waveform with trim/slice markers and the fades drawn over it (or a way to pick a file).
fn audio_waveform(ui: &mut Ui, id: CueId, a: &mut AudioCue, ctx: &Ctx, file_dur: Option<f64>, pick: &mut bool) {
    if a.path.as_os_str().is_empty() || !ctx.prober.exists(&a.path) {
        missing_file(ui, &a.path, pick);
        return;
    }
    let wf = ctx.waveforms.get(&a.path);
    let wf_ref = wf.as_ref().and_then(|r| r.as_ref().ok()).cloned();
    let duration = file_dur.or(wf_ref.as_ref().map(|w| w.duration)).unwrap_or(0.0);
    if duration <= 0.0 {
        ui.label(RichText::new("Reading the file…").weak());
        return;
    }
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
        Overlay {
            playhead,
            fade_in: a.fade_in * a.rate as f64,
            // Looping forever never reaches the end, so there is no fade-out to draw.
            fade_out: if a.infinite_loop { 0.0 } else { a.fade_out * a.rate as f64 },
            curve: a.fade_curve,
        },
    );
}

/// Slices: each marker ends a section that plays N times (∞ = until a Devamp cue).
fn slice_table(ui: &mut Ui, a: &mut AudioCue) {
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
            if super::theme::icon_button(ui, super::icons::DELETE, "Remove slice").clicked() {
                remove = Some(i);
            }
            ui.end_row();
            prev = m.time;
        }
        ui.label(RichText::new(format!("{} – end", fmt_time(prev))).weak());
        ui.label(RichText::new("last slice uses Plays").weak());
        ui.end_row();
    });
    if let Some(i) = remove {
        a.slices.remove(i);
    }
}

/// Shown where the media file is needed but missing: a way to pick it.
fn missing_file(ui: &mut Ui, path: &Path, pick: &mut bool) {
    ui.horizontal(|ui| {
        let msg = if path.as_os_str().is_empty() { "No file selected." } else { "The file can't be found." };
        ui.label(RichText::new(msg).color(if path.as_os_str().is_empty() { ui.visuals().weak_text_color() } else { RED }));
        if ui.button(super::theme::with_icon(super::icons::FOLDER_OPEN, "Choose file…")).clicked() {
            *pick = true;
        }
    });
    ui.add_space(4.0);
}

/// I/O: the input file (with its details) and where the sound goes.
fn io_tab(
    ui: &mut Ui,
    path: &Path,
    routing: &mut Option<Vec<Vec<f32>>>,
    stage: Option<Option<StageId>>,
    ctx: &Ctx,
    pick: &mut bool,
) {
    use super::icons as ic;
    use super::theme::Card;
    let info = ctx.prober.get(path).and_then(|r| r.ok());
    let specs = [
        Card::new(ic::UPLOAD_FILE, "Input", 400.0),
        Card::new(ic::SPEAKER, "Output", 360.0),
        Card::new(ic::GRID_VIEW, "Routing", f32::INFINITY),
    ];
    super::theme::cards(ui, "io", &specs, |ui, i| match i {
        0 => {
            egui::Grid::new("io-in").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                file_row(ui, path, ctx.prober, pick);
                if path.as_os_str().is_empty() {
                    return;
                }
                ui.label("Location");
                ui.horizontal(|ui| {
                    if let Some(dir) = path.parent().filter(|_| ctx.prober.exists(path))
                        && super::theme::icon_button(ui, ic::FOLDER_OPEN, "Show in folder").clicked()
                    {
                        let _ = super::manual::open_with_default_app(dir);
                    }
                    ui.add(egui::Label::new(RichText::new(path.display().to_string()).font(super::theme::mono(11.5))).truncate())
                        .on_hover_text(path.display().to_string());
                });
                ui.end_row();
                if let Some(Some(len)) = ctx.prober.stat(path) {
                    ui.label("Size");
                    let mb = len as f64 / 1_048_576.0;
                    ui.label(if mb >= 1.0 { format!("{mb:.1} MB") } else { format!("{:.0} KB", len as f64 / 1024.0) });
                    ui.end_row();
                }
                if let Some(i) = &info {
                    ui.label("Channels");
                    ui.label(match i.channels {
                        0 => "no audio".to_string(),
                        1 => "1 (mono)".into(),
                        2 => "2 (stereo)".into(),
                        n => n.to_string(),
                    });
                    ui.end_row();
                }
            });
        }
        1 => {
            egui::Grid::new("io-out").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                if let Some(stage) = stage {
                    ui.label("Picture");
                    let name = stage
                        .and_then(|s| ctx.stages.iter().find(|x| x.0 == s))
                        .or(ctx.stages.first())
                        .map_or_else(|| "no stage".to_string(), |s| format!("stage \"{}\"", s.1));
                    ui.label(name).on_hover_text("Set on the Geometry tab");
                    ui.end_row();
                }
                ui.label("Device");
                match &ctx.device {
                    Some(d) => ui.label(d.as_str()),
                    None => ui.label(RichText::new("no audio device").color(RED)),
                }
                .on_hover_text("Settings → Audio");
                ui.end_row();
                if ctx.device.is_some() {
                    ui.label("Outputs");
                    ui.label(ctx.out_channels.to_string());
                    ui.end_row();
                }
            });
        }
        _ => match info.as_ref().map(|i| i.channels) {
            Some(0) => {
                ui.label(RichText::new("This file has no sound to route.").weak());
            }
            ch => {
                let src = ch.unwrap_or(2).clamp(1, crate::model::MAX_SRC_CHANNELS);
                routing_editor(ui, routing, src, ctx.out_channels.min(crate::model::MAX_OUT_CHANNELS));
            }
        },
    });
}

/// Height of the faders' travel on the Levels tab.
const FADER_TRACK: f32 = 150.0;

/// Space a [`fader_bank`] needs around its faders: dB scale, the gap after the main fader and
/// the button column on the right.
const BANK_EXTRA: f32 = 32.0 + 14.0 + 128.0;

/// Width a [`fader_bank`] needs for `outs` output faders at their narrowest (plus card margins).
fn fader_bank_width(outs: usize) -> f32 {
    BANK_EXTRA + (outs as f32 + 1.0) * (super::widgets::FADER_MIN_W + 6.0) + 26.0
}

/// Level faders: the cue's main level, then one fader per device output, with the
/// "Set all silent" / "Default levels" buttons beside them. The faders narrow to fit the card, so
/// every output shows without scrolling.
fn fader_bank(ui: &mut Ui, main: &mut f32, out_levels: &mut Vec<f32>, outs: usize) {
    use super::widgets::{FADER_W, fader_scale, vfader};
    let mut levels: Vec<f32> = (0..outs).map(|o| out_levels.get(o).copied().unwrap_or(0.0)).collect();
    let mut changed = false;
    let fader_w = ((ui.available_width() - BANK_EXTRA) / (outs as f32 + 1.0) - 6.0).min(FADER_W);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
        fader_scale(ui, FADER_TRACK);
        vfader(ui, main, "Main", FADER_TRACK, true, fader_w).on_hover_text("The cue's level");
        ui.add_space(14.0);
        for (o, l) in levels.iter_mut().enumerate() {
            changed |= vfader(ui, l, &(o + 1).to_string(), FADER_TRACK, false, fader_w)
                .on_hover_text(format!("Output {} level, after the routing matrix", o + 1))
                .changed();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            ui.vertical(|ui| {
                ui.set_width(120.0);
                ui.add_space(20.0);
                let silent = *main <= MIN_DB && levels.iter().all(|l| *l <= MIN_DB);
                if ui.add_enabled(!silent, egui::Button::new("Set all silent").min_size(egui::vec2(120.0, 0.0))).clicked() {
                    *main = MIN_DB;
                    levels.iter_mut().for_each(|l| *l = MIN_DB);
                    changed = true;
                }
                let flat = *main == 0.0 && levels.iter().all(|l| *l == 0.0);
                if ui
                    .add_enabled(!flat, egui::Button::new("Default levels").min_size(egui::vec2(120.0, 0.0)))
                    .on_hover_text("Main and every output to 0 dB")
                    .clicked()
                {
                    *main = 0.0;
                    levels.iter_mut().for_each(|l| *l = 0.0);
                    changed = true;
                }
                ui.add_space(6.0);
                ui.label(RichText::new("Crosspoints are on the I/O tab.").weak().small());
            });
        });
    });
    if changed {
        *out_levels = if levels.iter().all(|l| *l == 0.0) { Vec::new() } else { levels };
    }
}

fn audio_levels(ui: &mut Ui, a: &mut AudioCue, outs: usize) {
    use super::icons as ic;
    use super::theme::Card;
    // Pan in a narrow card on the left, the fader bank takes the rest of the row.
    let specs = [Card::new(ic::CONTROL_CAMERA, "Pan", PAN_W), Card::new(ic::TUNE, "Levels", fader_bank_width(outs))];
    super::theme::cards(ui, "alevels", &specs, |ui, i| match i {
        0 => pan_card(ui, &mut a.pan),
        _ => fader_bank(ui, &mut a.volume_db, &mut a.out_levels, outs),
    });
}

fn video_levels(ui: &mut Ui, v: &mut VideoCue, outs: usize) {
    let specs = [super::theme::Card::new(super::icons::TUNE, "Soundtrack levels", f32::INFINITY)];
    super::theme::cards(ui, "vlevels", &specs, |ui, _| fader_bank(ui, &mut v.volume_db, &mut v.out_levels, outs));
}

fn pan_label(v: f32) -> String {
    if v.abs() < 0.005 {
        "C".into()
    } else if v < 0.0 {
        format!("L{:.0}", -v * 100.0)
    } else {
        format!("R{:.0}", v * 100.0)
    }
}

fn routing_editor(ui: &mut Ui, routing: &mut Option<Vec<Vec<f32>>>, src: usize, outs: usize) {
    ui.horizontal(|ui| {
        let mut custom = routing.is_some();
        if ui.checkbox(&mut custom, "Custom crosspoints").changed() {
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
    use super::icons as ic;
    use super::theme::Card;
    if v.path.as_os_str().is_empty() || !ctx.prober.exists(&v.path) {
        missing_file(ui, &v.path, pick);
    }
    let file_dur = ctx.prober.get(&v.path).and_then(|r| r.ok()).and_then(|i| i.duration);
    let still = is_image_path(&v.path);
    let specs = if still {
        vec![Card::new(ic::IMAGE, "Still image", 300.0), Card::new(ic::REPLAY, "At end", 300.0)]
    } else {
        vec![Card::new(ic::CROP, "Trim", 280.0), Card::new(ic::REPLAY, "Loops", 300.0)]
    };
    super::theme::cards(ui, "vtime", &specs, |ui, i| match (i, still) {
        (0, true) => {
            egui::Grid::new("vstill").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Show for");
                ui.horizontal(|ui| {
                    time_edit(ui, &mut v.still_duration);
                    ui.label(RichText::new(if v.still_duration <= 0.0 { "until stopped" } else { "" }).weak());
                });
                ui.end_row();
            });
        }
        (0, false) => {
            egui::Grid::new("vtrim").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Start");
                time_edit(ui, &mut v.start_time);
                ui.end_row();
                end_row(ui, &mut v.end_time, v.start_time, file_dur);
                ui.label("Duration");
                let probe = Cue::new(CueKind::Video(v.clone()));
                ui.label(
                    RichText::new(effective_duration(&probe, file_dur).map(fmt_time).unwrap_or_else(|| "∞ / until stopped".into()))
                        .font(super::theme::mono(12.5)),
                );
                ui.end_row();
            });
        }
        _ => {
            egui::Grid::new("vloops").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                if !still {
                    plays_row(ui, &mut v.play_count, &mut v.infinite_loop);
                }
                ui.label("At end");
                ui.checkbox(&mut v.hold_last_frame, "Hold last frame");
                ui.end_row();
            });
        }
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

pub(super) fn preview_toggle(ui: &mut Ui, open: &mut bool) {
    if cfg!(feature = "video") {
        ui.toggle_value(open, super::theme::with_icon(super::icons::PREVIEW, "Preview"))
            .on_hover_text("Show where this layer lands on its stage (View → Geometry preview)");
    }
}

/// Geometry tab of a video or text cue: where the layer goes, then how it's placed.
fn layer_geometry(ui: &mut Ui, salt: &str, layer: LayerRefs, ctx: &Ctx, preview: &mut bool) {
    use super::icons as ic;
    use super::theme::Card;
    let LayerRefs { stage, mut fit, layer, blend_mode, opacity, geometry } = layer;
    let specs = [Card::new(ic::DESKTOP_WINDOWS, "Layer", 320.0), Card::new(ic::OPEN_WITH, "Geometry", 520.0)];
    super::theme::cards(ui, salt, &specs, |ui, i| match i {
        0 => {
            egui::Grid::new((salt, "layer")).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Stage");
                stage_combo(ui, &format!("{salt}-stage"), stage, ctx.stages);
                ui.end_row();
                if let Some(fit) = fit.as_deref_mut() {
                    ui.label("Fit");
                    ComboBox::from_id_salt((salt, "fit")).selected_text(fit.label()).show_ui(ui, |ui| {
                        for f in VideoFit::ALL {
                            ui.selectable_value(fit, f, f.label());
                        }
                    });
                    ui.end_row();
                }
                ui.label("Layer");
                ui.add(DragValue::new(layer).range(-100..=100)).on_hover_text("Higher layers draw on top");
                ui.end_row();
                ui.label("Opacity");
                ui.add(Slider::new(opacity, 0.0..=1.0));
                ui.end_row();
                ui.label("Blend");
                ComboBox::from_id_salt((salt, "blend")).selected_text(blend_mode.label()).show_ui(ui, |ui| {
                    for m in BlendMode::ALL {
                        ui.selectable_value(blend_mode, m, m.label());
                    }
                })
                .response
                .on_hover_text("How this layer combines with the layers below it");
                ui.end_row();
            });
            ui.add_space(4.0);
            preview_toggle(ui, preview);
        }
        _ => geometry_editor(ui, &format!("{salt}-geom"), geometry),
    });
}

/// The layer fields of a video or text cue, for [`layer_geometry`].
struct LayerRefs<'a> {
    stage: &'a mut Option<StageId>,
    /// None for text, which has no fit mode.
    fit: Option<&'a mut VideoFit>,
    layer: &'a mut i32,
    blend_mode: &'a mut BlendMode,
    opacity: &'a mut f32,
    geometry: &'a mut Geometry,
}

/// Position / scale / rotation editor.
pub(super) fn geometry_editor(ui: &mut Ui, salt: &str, g: &mut Geometry) {
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
            let glyph = if lock { super::icons::LOCK } else { super::icons::LOCK_OPEN };
            ui.checkbox(&mut lock, super::theme::with_icon(glyph, "proportional"));
        });
        ui.end_row();
        ui.label("Rotation");
        ui.add(Slider::new(&mut g.rotation, -180.0..=180.0).suffix("°"));
        ui.end_row();
        ui.label("Crop");
        ui.horizontal(|ui| {
            for (i, side) in ["L", "T", "R", "B"].into_iter().enumerate() {
                ui.label(side);
                // Opposite edges can't cross.
                let max = (1.0 - g.crop[(i + 2) % 4]).max(0.0);
                ui.add(
                    DragValue::new(&mut g.crop[i]).speed(0.002).range(0.0..=max).custom_formatter(pct).custom_parser(parse_pct),
                );
            }
            if g.crop != [0.0; 4] && ui.small_button("Clear").clicked() {
                g.crop = [0.0; 4];
            }
        });
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

fn settings_tab(ui: &mut Ui, cue: &mut Cue, ctx: &Ctx, refresh: &mut bool) {
    use super::icons as ic;
    use super::theme::{Card, cards};
    let options = ctx.options;
    // Cues that only point at another cue: one card with the target.
    let target_card = |ui: &mut Ui, label: &str, salt: &str, target: &mut Option<CueId>, none: &str, note: Option<&str>| {
        let specs = [Card::new(ic::ARROW_RIGHT, "Target", 420.0)];
        cards(ui, salt, &specs, |ui, _| {
            egui::Grid::new(salt).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(label);
                target_combo(ui, salt, target, options, none);
                ui.end_row();
            });
            if let Some(note) = note {
                ui.add_space(4.0);
                ui.label(RichText::new(note).weak().small());
            }
        });
    };
    match &mut cue.kind {
        CueKind::Group { mode, children } => {
            let specs = [Card::new(ic::FOLDER, "Group", 460.0)];
            cards(ui, "group", &specs, |ui, _| {
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
                ui.add_space(4.0);
                ui.label(RichText::new("Drag cues onto the middle of a group row to put them inside it.").weak().small());
            });
        }
        CueKind::Wait { duration } => {
            let specs = [Card::new(ic::HOURGLASS_EMPTY, "Wait", 300.0)];
            cards(ui, "wait", &specs, |ui, _| {
                egui::Grid::new("wait").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Duration");
                    time_edit(ui, duration);
                    ui.end_row();
                });
            });
        }
        CueKind::Fade(f) => {
            // Only what the target can do: no geometry for a sound cue, no level for a text cue.
            // With no target yet, everything is offered.
            let caps = f.target.map_or(FadeCaps { level: true, picture: true }, |t| ctx.fade_caps.get(&t).copied().unwrap_or_default());
            let mut specs = vec![Card::new(ic::TUNE, "Changes", 300.0), Card::new(ic::TRENDING_DOWN, "Fade", 420.0)];
            if f.geometry.is_some() && caps.picture {
                specs.push(Card::new(ic::OPEN_WITH, "Target geometry", f32::INFINITY));
            }
            // Fading toward silence / transparency draws the curve falling.
            let falling = match (f.volume_db.filter(|_| caps.level), f.opacity.filter(|_| caps.picture)) {
                (Some(db), _) => db < 0.0,
                (None, Some(o)) => o < 0.5,
                _ => false,
            };
            cards(ui, "fade", &specs, |ui, i| match i {
                0 => {
                    if !caps.level && !caps.picture {
                        ui.label(RichText::new("The target has nothing to fade: pick a sound, video, text, camera or group cue.").weak());
                        return;
                    }
                    egui::Grid::new("fade-changes").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                        if caps.level {
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
                        }
                        if caps.picture {
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
                                f.geometry =
                                    on.then(|| f.target.and_then(|t| ctx.geometries.get(&t).copied()).unwrap_or_default());
                            }
                            ui.end_row();
                        }
                    });
                }
                1 => {
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
                            if let FadeCurve::Custom(_) = f.curve {
                                let _ = ui.selectable_label(true, "Custom");
                            }
                        });
                        ui.end_row();
                        ui.label("When done");
                        ui.checkbox(&mut f.stop_target_when_done, "Stop target");
                        ui.end_row();
                    });
                    ui.add_space(6.0);
                    super::widgets::curve_graph(ui, "fade-curve", &mut f.curve, f.duration, falling, caps.level);
                }
                _ => {
                    if let Some(g) = &mut f.geometry {
                        geometry_editor(ui, "fgeom", g);
                    }
                }
            });
        }
        CueKind::Start { target } => target_card(ui, "Start", "start_target", target, "— none —", None),
        CueKind::Stop { target } => target_card(ui, "Stop", "stop_target", target, "All cues", None),
        CueKind::Pause { target } => target_card(ui, "Pause", "pause_target", target, "All cues", None),
        CueKind::Goto { target } => target_card(ui, "Move playhead to", "goto_target", target, "— none —", None),
        CueKind::Devamp { target } => target_card(
            ui,
            "Devamp",
            "devamp_target",
            target,
            "— none —",
            Some("Lets the target's looping slice finish its current pass, then play on to the next slice."),
        ),
        CueKind::Text(t) => text_settings(ui, t),
        CueKind::Camera(c) => camera_settings(ui, c, ctx, refresh),
        CueKind::Network(n) => {
            let specs = [Card::new(ic::SETTINGS_REMOTE, "OSC message", 480.0)];
            cards(ui, "net", &specs, |ui, _| {
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
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Sends one OSC message over UDP. Numbers are sent as int/float, other words as strings.")
                        .weak()
                        .small(),
                );
            });
        }
        _ => {}
    }
}

fn color_button(ui: &mut Ui, rgba: &mut [u8; 4]) -> bool {
    let mut c = egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
    let changed = egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::OnlyBlend).changed();
    if changed {
        *rgba = c.to_srgba_unmultiplied();
    }
    changed
}

const WEIGHTS: [(u16, &str); 9] = [
    (100, "Thin"),
    (200, "Extra Light"),
    (300, "Light"),
    (400, "Regular"),
    (500, "Medium"),
    (600, "Semibold"),
    (700, "Bold"),
    (800, "Extra Bold"),
    (900, "Black"),
];

/// Text cue settings: text, font and style, outline and shadow, with a rendered sample.
fn text_settings(ui: &mut Ui, t: &mut crate::model::TextCue) {
    use super::icons as ic;
    use super::theme::Card;
    use crate::media::text;
    let specs = [
        Card::new(ic::TEXT_FIELDS, "Text", 360.0),
        Card::new(ic::NOTES, "Font", 520.0),
        Card::new(ic::PALETTE, "Look", 520.0),
        Card::new(ic::PREVIEW, "Sample", 360.0),
    ];
    super::theme::cards(ui, "text", &specs, |ui, i| match i {
        0 => {
            ui.add(TextEdit::multiline(&mut t.text).desired_rows(3).desired_width(f32::INFINITY).hint_text("Text to show"));
            ui.add_space(4.0);
            egui::Grid::new("text-time").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Show for");
                ui.horizontal(|ui| {
                    time_edit(ui, &mut t.duration);
                    ui.label(RichText::new(if t.duration <= 0.0 { "until stopped" } else { "" }).weak());
                });
                ui.end_row();
            });
        }
        1 => {
            egui::Grid::new("text-font").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Font");
                ui.horizontal(|ui| {
                    font_picker(ui, &mut t.font);
                    if text::families_ready() && !text::is_installed(&t.font) {
                        ui.label(RichText::new("not installed here: a fallback font is used").color(super::widgets::AMBER))
                            .on_hover_text("Install this font on every computer that plays the show");
                    }
                });
                ui.end_row();

                ui.label("Weight");
                ui.horizontal(|ui| {
                    let styles = if text::families_ready() { text::styles_of(&t.font) } else { Vec::new() };
                    let has = |w: u16, italic: bool| styles.iter().any(|s| s.0 == w && s.1 == italic);
                    let mut w = t.effective_weight();
                    let name = |w: u16| WEIGHTS.iter().find(|x| x.0 == w).map_or_else(|| w.to_string(), |x| format!("{} {}", x.0, x.1));
                    ComboBox::from_id_salt("weight").selected_text(name(w)).show_ui(ui, |ui| {
                        for (v, _) in WEIGHTS {
                            let label = if styles.is_empty() || has(v, t.italic) { RichText::new(name(v)) } else { RichText::new(name(v)).weak() };
                            ui.selectable_value(&mut w, v, label);
                        }
                    })
                    .response
                    .on_hover_text("Weights the font doesn't include are greyed out; the nearest one is used");
                    if w != t.effective_weight() {
                        t.weight = w;
                        t.bold = w >= 600;
                    }
                    ui.checkbox(&mut t.italic, "Italic");
                });
                ui.end_row();

                ui.label("Size");
                ui.horizontal(|ui| {
                    ui.add(DragValue::new(&mut t.size).range(4.0..=1000.0).speed(1.0).suffix(" px"));
                    ui.label("Line height");
                    ui.add(DragValue::new(&mut t.line_height).range(0.5..=4.0).speed(0.01).fixed_decimals(2).suffix("×"));
                    ui.label("Spacing");
                    ui.add(DragValue::new(&mut t.letter_spacing).range(-0.5..=2.0).speed(0.002).fixed_decimals(3).suffix(" em"))
                        .on_hover_text("Extra space between letters");
                });
                ui.end_row();

            });
        }
        2 => {
            egui::Grid::new("text-look").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Colour");
                ui.horizontal(|ui| {
                    color_button(ui, &mut t.color);
                    for a in [crate::model::TextAlign::Left, crate::model::TextAlign::Center, crate::model::TextAlign::Right] {
                        ui.selectable_value(&mut t.align, a, format!("{a:?}"));
                    }
                });
                ui.end_row();

                ui.label("Outline");
                ui.horizontal(|ui| {
                    ui.add(DragValue::new(&mut t.outline_width).range(0.0..=100.0).speed(0.1).fixed_decimals(1).suffix(" px"));
                    color_button(ui, &mut t.outline_color);
                    ui.label(RichText::new(if t.outline_width <= 0.0 { "off" } else { "" }).weak());
                });
                ui.end_row();

                ui.label("Shadow");
                ui.horizontal(|ui| {
                    let mut on = t.shadow_color[3] > 0;
                    if ui.checkbox(&mut on, "").changed() {
                        t.shadow_color[3] = if on { 200 } else { 0 };
                    }
                    ui.add_enabled_ui(on, |ui| {
                        color_button(ui, &mut t.shadow_color);
                        ui.label("offset");
                        ui.add(DragValue::new(&mut t.shadow_offset[0]).range(-200.0..=200.0).speed(0.2).suffix(" px"));
                        ui.add(DragValue::new(&mut t.shadow_offset[1]).range(-200.0..=200.0).speed(0.2).suffix(" px"));
                        ui.label("blur");
                        ui.add(DragValue::new(&mut t.shadow_blur).range(0.0..=100.0).speed(0.2).suffix(" px"));
                    });
                });
                ui.end_row();

                ui.label("Background");
                ui.horizontal(|ui| {
                    let mut on = t.bg_color[3] > 0;
                    if ui.checkbox(&mut on, "").on_hover_text("A box behind the text").changed() {
                        t.bg_color[3] = if on { 255 } else { 0 };
                    }
                    ui.add_enabled_ui(on, |ui| {
                        color_button(ui, &mut t.bg_color);
                        ui.label("padding");
                        ui.add(DragValue::new(&mut t.bg_padding).range(0.0..=500.0).speed(0.5).suffix(" px"));
                        ui.label("radius");
                        ui.add(DragValue::new(&mut t.bg_radius).range(0.0..=500.0).speed(0.5).suffix(" px"))
                            .on_hover_text("Rounded corners");
                    });
                });
                ui.end_row();

            });
        }
        _ => {
            text_sample(ui, t);
            ui.add_space(4.0);
            ui.label(
                RichText::new("Text appears on the stage like a video layer: use the Geometry and Animation tabs and Fade cues to place, animate and fade it.")
                    .weak()
                    .small(),
            );
        }
    });
}

/// Searchable list of installed font families.
fn font_picker(ui: &mut Ui, font: &mut String) {
    use crate::media::text;
    let label = if font.is_empty() { format!("{} (built-in)", text::BUILT_IN) } else { font.clone() };
    let search_id = ui.id().with("font-search");
    ComboBox::from_id_salt("font").selected_text(label).width(240.0).height(360.0).show_ui(ui, |ui| {
        if !text::families_ready() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading installed fonts…");
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            return;
        }
        let mut q = ui.data(|d| d.get_temp::<String>(search_id)).unwrap_or_default();
        let r = ui.add(TextEdit::singleline(&mut q).hint_text("Search fonts").desired_width(220.0));
        if q.is_empty() && !r.has_focus() {
            r.request_focus();
        }
        ui.data_mut(|d| d.insert_temp(search_id, q.clone()));
        let q = q.to_lowercase();
        ui.selectable_value(font, String::new(), format!("{} (built-in)", text::BUILT_IN));
        for name in text::families().iter().filter(|n| q.is_empty() || n.to_lowercase().contains(&q)) {
            if ui.selectable_label(font == name, name).clicked() {
                *font = name.clone();
                ui.close();
            }
        }
    });
}

/// A small rendering of the cue's first line in its font and style.
fn text_sample(ui: &mut Ui, t: &crate::model::TextCue) {
    use crate::media::text;
    if !text::families_ready() {
        return;
    }
    let first = t.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("Sample").chars().take(60).collect::<String>();
    // Placement, timing and animation don't change the sample, so editing them renders nothing.
    let sample = crate::model::TextCue {
        text: first,
        size: 40.0,
        align: crate::model::TextAlign::Left,
        animation: Default::default(),
        geometry: Default::default(),
        stage: None,
        layer: 0,
        opacity: 1.0,
        duration: 0.0,
        ..t.clone()
    };
    // One cached texture, replaced when the look changes.
    let slot = egui::Id::new("text-sample");
    let key = sample.clone();
    let cached = ui.data(|d| d.get_temp::<(crate::model::TextCue, egui::TextureHandle)>(slot)).filter(|(k, _)| *k == key);
    let tex = cached.map(|(_, h)| h).unwrap_or_else(|| {
        let img = text::render(&sample);
        let ci = egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba);
        let opts = egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR };
        let h = ui.ctx().load_texture("text-sample", ci, opts);
        ui.data_mut(|d| d.insert_temp(slot, (key, h.clone())));
        h
    });
    let size = tex.size_vec2() / 2.0;
    // Small enough that the Text tab fits the inspector without scrolling.
    let max_w = ui.available_width().min(560.0);
    let size = size * (max_w / size.x).min(64.0 / size.y).min(1.0);
    egui::Frame::new().fill(super::theme::BG_INPUT).corner_radius(6).inner_margin(8).show(ui, |ui| {
        ui.add(egui::Image::new((tex.id(), size)));
    });
}

/// Edit label-seconds as "hh:mm:ss:ff" at `rate`; drag to step by frames.
pub(super) fn timecode_edit(ui: &mut Ui, secs: &mut f64, rate: crate::model::TimecodeRate) -> egui::Response {
    use crate::timecode::{format_secs, parse_secs};
    let frame = 1.0 / rate.nominal() as f64;
    ui.add(
        DragValue::new(secs)
            .range(0.0..=86_400.0 - frame)
            .speed(frame)
            .custom_formatter(move |v, _| format_secs(v, rate))
            .custom_parser(move |t| parse_secs(t, rate)),
    )
    .on_hover_text(format!("Timecode at {} (type hh:mm:ss:ff)", rate.label()))
}

/// Combo of choices by (value, label), with a first "default" entry for None.
fn device_combo(ui: &mut Ui, salt: &str, value: &mut Option<String>, choices: &[(String, String)], default: &str) {
    let label = match value {
        None => default.to_string(),
        Some(v) => choices.iter().find(|c| &c.0 == v).map_or_else(|| format!("{v} (not found)"), |c| c.1.clone()),
    };
    ComboBox::from_id_salt(salt).width(260.0).selected_text(label).show_ui(ui, |ui| {
        ui.selectable_value(value, None, default);
        for (v, l) in choices {
            ui.selectable_value(value, Some(v.clone()), l);
        }
    });
}

fn refresh_button(ui: &mut Ui, ctx: &Ctx, refresh: &mut bool) {
    if ctx.devices.loading {
        ui.spinner();
    } else if super::theme::icon_button(ui, super::icons::REFRESH, "Look for devices again").clicked() {
        *refresh = true;
    }
}

/// A horizontal input level bar (linear peak).
fn level_bar(ui: &mut Ui, peak: Option<f32>) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(160.0, 8.0), egui::Sense::hover());
    let db = peak.map_or(MIN_DB, gain_to_db);
    let frac = ((db - MIN_DB) / -MIN_DB).clamp(0.0, 1.0);
    let color = if db > -3.0 { RED } else if db > -12.0 { super::widgets::AMBER } else { super::widgets::GREEN };
    super::widgets::paint_progress(ui, rect, frac, color);
}

fn mic_io(ui: &mut Ui, m: &mut crate::model::MicCue, ctx: &Ctx, refresh: &mut bool) {
    use super::icons as ic;
    use super::theme::Card;
    let specs = [
        Card::new(ic::MIC, "Input", 420.0),
        Card::new(ic::SCHEDULE, "Timing", 260.0),
        Card::new(ic::GRID_VIEW, "Routing", f32::INFINITY),
    ];
    let inputs: Vec<(String, String)> = ctx.devices.audio_in.iter().map(|d| (d.id.clone(), d.name.clone())).collect();
    super::theme::cards(ui, "mic", &specs, |ui, i| match i {
        0 => {
            egui::Grid::new("mic-in").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Device");
                ui.horizontal(|ui| {
                    device_combo(ui, "mic-dev", &mut m.device, &inputs, "System default input");
                    refresh_button(ui, ctx, refresh);
                });
                ui.end_row();
                ui.label("Channels");
                ui.horizontal(|ui| {
                    let mut first = m.first_channel + 1;
                    ui.add(DragValue::new(&mut first).range(1..=64).prefix("from "));
                    m.first_channel = first - 1;
                    ui.add(DragValue::new(&mut m.channels).range(1..=crate::model::MAX_SRC_CHANNELS as u16).suffix(" ch"));
                    ui.label(RichText::new(match m.channels {
                        1 => format!("input {}", m.first_channel + 1),
                        n => format!("inputs {}–{}", m.first_channel + 1, m.first_channel + n),
                    }).weak());
                });
                ui.end_row();
                ui.label("Level");
                ui.horizontal(|ui| {
                    level_bar(ui, ctx.mic_level);
                    if ctx.mic_level.is_none() {
                        ui.label(RichText::new("shown while the cue runs").weak().small());
                    }
                });
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Live input is delayed by about 30 ms on its way through the mixer.").weak().small());
        }
        1 => {
            egui::Grid::new("mic-time").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Duration");
                ui.horizontal(|ui| {
                    time_edit(ui, &mut m.duration);
                    ui.label(RichText::new(if m.duration <= 0.0 { "until stopped" } else { "" }).weak());
                });
                ui.end_row();
                ui.label("Fade in");
                time_edit(ui, &mut m.fade_in).on_hover_text("Fades up from silence when the cue starts");
                ui.end_row();
            });
        }
        _ => routing_editor(ui, &mut m.routing, m.channels as usize, ctx.out_channels.min(crate::model::MAX_OUT_CHANNELS)),
    });
}

fn mic_levels(ui: &mut Ui, m: &mut crate::model::MicCue, outs: usize) {
    use super::icons as ic;
    use super::theme::Card;
    let specs = [Card::new(ic::CONTROL_CAMERA, "Pan", PAN_W), Card::new(ic::TUNE, "Levels", fader_bank_width(outs))];
    super::theme::cards(ui, "miclevels", &specs, |ui, i| match i {
        0 => pan_card(ui, &mut m.pan),
        _ => fader_bank(ui, &mut m.volume_db, &mut m.out_levels, outs),
    });
}

/// Width of the Pan card on the Levels tab.
const PAN_W: f32 = 190.0;

fn pan_card(ui: &mut Ui, pan: &mut f32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = (ui.available_width() - 44.0).max(60.0);
        ui.add(Slider::new(pan, -1.0..=1.0).show_value(false));
        ui.label(RichText::new(pan_label(*pan)).font(super::theme::mono(12.5)));
    });
    if ui.add_enabled(*pan != 0.0, egui::Button::new("Centre").small()).clicked() {
        *pan = 0.0;
    }
    ui.add_space(4.0);
    ui.label(RichText::new("Pans between outputs 1 and 2 when the routing is automatic.").weak().small());
}

/// Capture sizes offered for cameras.
const CAMERA_SIZES: [[u32; 2]; 4] = [[640, 480], [1280, 720], [1920, 1080], [3840, 2160]];

fn camera_settings(ui: &mut Ui, c: &mut crate::model::CameraCue, ctx: &Ctx, refresh: &mut bool) {
    use super::icons as ic;
    use super::theme::Card;
    let specs = [Card::new(ic::PHOTO_CAMERA, "Camera", 460.0), Card::new(ic::SCHEDULE, "Timing", 300.0)];
    super::theme::cards(ui, "camera", &specs, |ui, i| match i {
        0 => {
            egui::Grid::new("cam").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Device");
                ui.horizontal(|ui| {
                    if c.format.is_some() {
                        // A source typed by hand (e.g. a test pattern): show it as text.
                        let mut text = c.device.clone().unwrap_or_default();
                        if ui.add(TextEdit::singleline(&mut text).desired_width(260.0)).changed() {
                            c.device = (!text.is_empty()).then_some(text);
                        }
                    } else {
                        device_combo(ui, "cam-dev", &mut c.device, &ctx.devices.cameras, "First camera found");
                    }
                    refresh_button(ui, ctx, refresh);
                });
                ui.end_row();
                if ctx.devices.loaded && ctx.devices.cameras.is_empty() && c.format.is_none() {
                    ui.label("");
                    ui.label(RichText::new("No cameras found.").color(super::widgets::AMBER));
                    ui.end_row();
                }
                ui.label("Size");
                let label = c.size.map_or_else(|| "Camera default".to_string(), |[w, h]| format!("{w} × {h}"));
                ComboBox::from_id_salt("cam-size").selected_text(label).show_ui(ui, |ui| {
                    ui.selectable_value(&mut c.size, None, "Camera default");
                    for s in CAMERA_SIZES {
                        ui.selectable_value(&mut c.size, Some(s), format!("{} × {}", s[0], s[1]));
                    }
                });
                ui.end_row();
                ui.label("Mirror");
                ui.checkbox(&mut c.mirror, "Flip left to right");
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Device and size changes apply the next time the cue starts.").weak().small());
        }
        _ => {
            egui::Grid::new("cam-time").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Show for");
                ui.horizontal(|ui| {
                    time_edit(ui, &mut c.duration);
                    ui.label(RichText::new(if c.duration <= 0.0 { "until stopped" } else { "" }).weak());
                });
                ui.end_row();
            });
        }
    });
}

/// Triggers: ways to fire the cue besides GO. Returns true when the user asks to set
/// up the list's timecode.
fn triggers(ui: &mut Ui, cue: &mut Cue, ctx: &Ctx) -> bool {
    use super::icons as ic;
    use super::theme::Card;
    use crate::model::TimecodeSource;
    let mut setup = false;
    let specs = [Card::new(ic::KEYBOARD, "Hotkey", 340.0), Card::new(ic::TIMER_PLAY, "Timecode", 460.0)];
    super::theme::cards(ui, "triggers", &specs, |ui, i| match i {
        0 => {
            egui::Grid::new("trig-key").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Key");
                let mut text = cue.hotkey.clone().unwrap_or_default();
                let valid = text.is_empty() || egui::Key::from_name(&text).is_some();
                let r = ui.add(
                    TextEdit::singleline(&mut text)
                        .desired_width(80.0)
                        .hint_text("none")
                        .text_color_opt((!valid).then_some(RED)),
                );
                if r.changed() {
                    cue.hotkey = (!text.trim().is_empty()).then(|| text.trim().to_string());
                }
                r.on_hover_text("Key that fires this cue (e.g. F1, A, Num5). Not Space/P/R/Esc.");
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Fires the cue directly, whatever the playhead is on.").weak().small());
        }
        _ => {
            let sync = &ctx.list_tc;
            egui::Grid::new("trig-tc").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Fire at");
                ui.horizontal(|ui| {
                    let mut on = cue.timecode_trigger.is_some();
                    if ui.checkbox(&mut on, "").on_hover_text("Fire when the cue list's timecode reaches this time").changed() {
                        cue.timecode_trigger = on.then_some(3600.0);
                    }
                    match &mut cue.timecode_trigger {
                        Some(t) => {
                            timecode_edit(ui, t, sync.rate);
                        }
                        None => {
                            ui.label(RichText::new("off").weak());
                        }
                    }
                });
                ui.end_row();
                ui.label("Cue list");
                ui.horizontal(|ui| {
                    let text = match sync.source {
                        TimecodeSource::Off => "doesn't follow timecode".to_string(),
                        TimecodeSource::Ltc => format!("follows LTC at {}", sync.rate.label()),
                        TimecodeSource::Mtc => format!("follows MTC at {}", sync.rate.label()),
                    };
                    let color = if sync.source == TimecodeSource::Off && cue.timecode_trigger.is_some() {
                        super::widgets::AMBER
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    ui.label(RichText::new(text).color(color));
                    if ui.small_button("Set up…").on_hover_text("The cue list's timecode settings").clicked() {
                        setup = true;
                    }
                });
                ui.end_row();
                if sync.source != TimecodeSource::Off {
                    ui.label("Receiving");
                    match &ctx.tc_in {
                        Some(t) => ui.label(RichText::new(t).font(super::theme::mono(13.0)).color(super::theme::GREEN)),
                        None => ui.label(RichText::new("no timecode").color(super::widgets::AMBER)),
                    };
                    ui.end_row();
                }
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new("The cue fires as running timecode passes this time. Jumps and dropouts don't fire cues.")
                    .weak()
                    .small(),
            );
        }
    });
    setup
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::KindTag;

    /// The Fade cue shows only what its target can change.
    #[test]
    fn fade_caps_follow_the_target_kind() {
        let caps = |k: KindTag| FadeCaps::of(&Cue::new(k.default_kind()));
        assert_eq!(caps(KindTag::Audio), FadeCaps { level: true, picture: false });
        assert_eq!(caps(KindTag::Text), FadeCaps { level: false, picture: true });
        assert_eq!(caps(KindTag::Video), FadeCaps { level: true, picture: true });
        assert_eq!(caps(KindTag::Wait), FadeCaps::default());
        let still = crate::model::VideoCue { path: "logo.svg".into(), ..Default::default() };
        assert_eq!(FadeCaps::of(&Cue::new(CueKind::Video(still))), FadeCaps { level: false, picture: true });
        let group = CueKind::Group {
            mode: GroupMode::StartAll,
            children: vec![Cue::new(KindTag::Audio.default_kind()), Cue::new(KindTag::Camera.default_kind())],
        };
        assert_eq!(FadeCaps::of(&Cue::new(group)), FadeCaps { level: true, picture: true });
    }
}
