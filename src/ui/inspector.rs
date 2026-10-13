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
use std::path::Path;
use crate::i18n::t;

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
    AudioFx,
    VideoFx,
}

/// Every tab name (translation test).
#[cfg(test)]
pub(super) fn tab_names() -> Vec<&'static str> {
    crate::model::KindTag::ALL.iter().flat_map(|k| tabs_for(&k.default_kind()).iter().map(|t| t.1)).collect()
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
            (Tab::AudioFx, "Audio FX"),
        ],
        CueKind::Video(_) => &[
            (Tab::Basics, "Basics"),
            (Tab::Triggers, "Triggers"),
            (Tab::Time, "Time & Loops"),
            (Tab::Geometry, "Geometry"),
            (Tab::Animation, "Animation"),
            (Tab::VideoFx, "Video FX"),
            (Tab::Io, "I/O"),
            (Tab::Levels, "Audio"),
            (Tab::AudioFx, "Audio FX"),
        ],
        CueKind::Mic(_) => &[
            (Tab::Basics, "Basics"),
            (Tab::Triggers, "Triggers"),
            (Tab::Io, "I/O"),
            (Tab::Levels, "Levels"),
            (Tab::Eq, "EQ"),
            (Tab::AudioFx, "Audio FX"),
        ],
        CueKind::Camera(_) => &[
            (Tab::Basics, "Basics"),
            (Tab::Triggers, "Triggers"),
            (Tab::Settings, "Camera"),
            (Tab::Geometry, "Geometry"),
            (Tab::VideoFx, "Video FX"),
        ],
        CueKind::Memo => &[(Tab::Basics, "Basics"), (Tab::Triggers, "Triggers")],
        CueKind::Text(_) => &[
            (Tab::Basics, "Basics"),
            (Tab::Triggers, "Triggers"),
            (Tab::Settings, "Text"),
            (Tab::Geometry, "Geometry"),
            (Tab::Animation, "Animation"),
            (Tab::VideoFx, "Video FX"),
        ],
        _ => &[(Tab::Basics, "Basics"), (Tab::Triggers, "Triggers"), (Tab::Settings, "Settings")],
    }
}

/// Everything the editors need that lives outside the cue being edited.
/// What a Fade cue can change on a target: its level (sound), its picture (opacity, geometry
/// and video effects), its object-audio positions, a mix of them (a movie, or a group holding
/// several kinds), or nothing.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct FadeCaps {
    level: bool,
    picture: bool,
    objects: bool,
}

impl FadeCaps {
    fn of(cue: &Cue) -> Self {
        match &cue.kind {
            CueKind::Audio(a) => Self { level: true, picture: false, objects: a.objects.enabled },
            CueKind::Mic(m) => Self { level: true, picture: false, objects: m.objects.enabled },
            // A still has no soundtrack.
            CueKind::Video(v) => {
                let sound = !crate::media::is_image_path(&v.path);
                Self { level: sound, picture: true, objects: sound && v.objects.enabled }
            }
            CueKind::Text(_) | CueKind::Camera(_) => Self { level: false, picture: true, objects: false },
            CueKind::Group { children, .. } => children.iter().map(Self::of).fold(Self::default(), |a, b| Self {
                level: a.level || b.level,
                picture: a.picture || b.picture,
                objects: a.objects || b.objects,
            }),
            _ => Self::default(),
        }
    }
}

struct Ctx<'a> {
    /// Every cue, for targets (the cue being edited is a copy, so this is its saved state).
    list: &'a crate::model::CueList,
    /// The cue being edited.
    id: CueId,
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
    /// The object-audio room (the workspace's speakers or the device's default layout).
    room: Vec<crate::spatial::Speaker>,
}

impl Ctx<'_> {
    /// What a Fade cue can change on `target`.
    fn fade_caps(&self, target: CueId) -> FadeCaps {
        self.list.find(target).map(FadeCaps::of).unwrap_or_default()
    }

    /// Object positions of a sound cue in object mode (with its source channel count).
    fn objects(&self, target: CueId) -> Option<Vec<crate::spatial::AudioObject>> {
        let (o, path) = match &self.list.find(target)?.kind {
            CueKind::Audio(a) => (&a.objects, Some(&a.path)),
            CueKind::Video(v) => (&v.objects, Some(&v.path)),
            CueKind::Mic(m) => return Some(m.objects.resolved(m.channels.max(1) as usize)),
            _ => return None,
        };
        let src = path.and_then(|p| self.prober.get(p)).and_then(|r| r.ok()).map_or(2, |i| i.channels.max(1));
        Some(o.resolved(src.min(crate::model::MAX_SRC_CHANNELS)))
    }

    /// Video effects of a picture cue.
    fn video_fx(&self, target: CueId) -> Option<Vec<crate::effects::VideoEffect>> {
        match &self.list.find(target)?.kind {
            CueKind::Video(v) => Some(v.video_fx.clone()),
            CueKind::Text(t) => Some(t.video_fx.clone()),
            CueKind::Camera(k) => Some(k.video_fx.clone()),
            _ => None,
        }
    }

    /// Geometry of a picture cue.
    fn geometry(&self, target: CueId) -> Option<Geometry> {
        match &self.list.find(target)?.kind {
            CueKind::Video(v) => Some(v.geometry),
            CueKind::Text(t) => Some(t.geometry),
            CueKind::Camera(k) => Some(k.geometry),
            _ => None,
        }
    }
}

/// Name of the background list holding `id` (None in the current list).
fn background_list(list: &crate::model::CueList, id: CueId) -> Option<&str> {
    let mut top = id;
    while let Some(p) = list.parent_of(top) {
        if let Some(o) = list.others.iter().find(|o| o.info.id == p) {
            return Some(&o.info.name);
        }
        top = p;
    }
    None
}

/// "number name" of a cue, plus its list's name when it's in a background list.
fn target_label(cue: &Cue, other_list: Option<&str>) -> String {
    let label = format!("{} {}", cue.number, cue.display_name()).trim().to_string();
    match other_list {
        Some(l) => format!("{label}  ({l})"),
        None => label,
    }
}

impl App {
    pub(super) fn inspector(&mut self, ui: &mut Ui, show: &mut Show) {
        let Some(id) = self.primary() else {
            ui.centered_and_justified(|ui| ui.label(RichText::new(t("No cue selected")).weak()));
            return;
        };
        let stages: Vec<(StageId, String)> = show.ws.stages.iter().map(|s| (s.id, s.name.clone())).collect();
        let elapsed = show.engine.instances.get(&id).and_then(|_| show.media.elapsed(id));
        // Edit a copy, so the tabs can look at the other cues (targets) while editing this one.
        let Some(mut cue) = show.ws.cue_list.find(id).cloned() else { return };
        let ctx = Ctx {
            list: &show.ws.cue_list,
            id,
            stages: &stages,
            out_channels: show.media.audio.as_ref().map_or(2, |a| a.channels),
            device: show.media.audio.as_ref().map(|a| a.device_name.clone()),
            prober: &self.prober,
            waveforms: &self.waveforms,
            elapsed,
            devices: self.inputs.lock().clone(),
            list_tc: show.ws.cue_list.timecode_of(id).clone(),
            mic_level: show.media.mic_peak(id),
            room: if show.ws.settings.speakers.is_empty() {
                crate::spatial::default_room(show.media.audio.as_ref().map_or(2, |a| a.channels))
            } else {
                show.ws.settings.speakers.clone()
            },
            tc_in: {
                let sync = show.ws.cue_list.timecode_of(id);
                show.timecode.position(sync, std::time::Instant::now()).map(|r| crate::timecode::format_secs(r.secs, r.rate))
            },
        };
        if ctx.mic_level.is_some() || ctx.tc_in.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
        }
        let cue = &mut cue;
        if matches!(cue.kind, CueKind::Mic(_) | CueKind::Camera(_)) {
            super::devices::ensure(&self.inputs, ui.ctx());
        }
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
                    Tab::AudioFx => super::icons::INSTANT_MIX,
                    Tab::VideoFx => super::icons::AUTO_FIX_HIGH,
                };
                super::theme::tab(ui, &mut self.inspector_tab, *t, glyph, crate::i18n::t(label));
            }
            if self.selection.len() > 1 {
                ui.label(RichText::new(tr!("({n} selected — editing the last)", n = self.selection.len())).weak());
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
            Tab::AudioFx => match &mut cue.kind {
                CueKind::Audio(a) => super::effects::audio_fx_tab(ui, &mut a.audio_fx),
                CueKind::Video(v) => super::effects::audio_fx_tab(ui, &mut v.audio_fx),
                CueKind::Mic(m) => super::effects::audio_fx_tab(ui, &mut m.audio_fx),
                _ => {}
            },
            Tab::VideoFx => match &mut cue.kind {
                CueKind::Video(v) => super::effects::video_fx_tab(ui, &mut v.video_fx),
                CueKind::Text(t) => super::effects::video_fx_tab(ui, &mut t.video_fx),
                CueKind::Camera(c) => super::effects::video_fx_tab(ui, &mut c.video_fx),
                _ => {}
            },
            Tab::Io => match &mut cue.kind {
                CueKind::Audio(a) => io_tab(ui, &a.path, &mut a.routing, &mut a.objects, None, &ctx, &mut pick_media),
                CueKind::Video(v) => io_tab(ui, &v.path, &mut v.routing, &mut v.objects, Some(v.stage), &ctx, &mut pick_media),
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
                        super::theme::Card::new(super::icons::TUNE, t("Bands"), 470.0),
                        super::theme::Card::new(super::icons::GRAPHIC_EQ, t("Response"), 320.0),
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
        if show.ws.cue_list.find(id) != Some(&*cue) {
            let cue = cue.clone();
            if let Some(c) = show.ws.cue_list.find_mut(id) {
                *c = cue.clone();
            }
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
    let mut specs = vec![Card::new(ic::INFO, t("Cue"), 300.0), Card::new(ic::SCHEDULE, t("Timing"), 280.0)];
    let media = cue.kind.media_path().is_some();
    if media {
        specs.push(Card::new(ic::UPLOAD_FILE, t("Media"), 320.0));
    }
    specs.push(Card::new(ic::NOTES, t("Notes"), 280.0));
    let notes = specs.len() - 1;
    super::theme::cards(ui, "basics", &specs, |ui, i| match i {
        0 => {
            egui::Grid::new("basics-cue").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Number"));
                ui.add(TextEdit::singleline(&mut cue.number).desired_width(80.0));
                ui.end_row();

                ui.label(t("Name"));
                let hint = cue.display_name();
                ui.add(TextEdit::singleline(&mut cue.name).hint_text(hint).desired_width(f32::INFINITY));
                ui.end_row();

                ui.label(t("Color"));
                ComboBox::from_id_salt("color").selected_text(cue.color.label()).show_ui(ui, |ui| {
                    for c in CueColor::ALL {
                        ui.selectable_value(&mut cue.color, c, c.label());
                    }
                });
                ui.end_row();

                ui.label(t("Armed"));
                ui.checkbox(&mut cue.armed, "")
                    .on_hover_text(t("Disarmed cues skip their action but still run waits and continues"));
                ui.end_row();
            });
        }
        1 => {
            egui::Grid::new("basics-timing").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Pre-wait"));
                time_edit(ui, &mut cue.pre_wait).on_hover_text(t("Delay between GO and the cue's action"));
                ui.end_row();

                ui.label(t("Continue"));
                ComboBox::from_id_salt("continue").selected_text(cue.continue_mode.label()).show_ui(ui, |ui| {
                    for m in ContinueMode::ALL {
                        ui.selectable_value(&mut cue.continue_mode, m, m.label());
                    }
                });
                ui.end_row();

                ui.label(t("Post-wait"));
                ui.add_enabled_ui(cue.continue_mode != ContinueMode::AutoFollow, |ui| {
                    time_edit(ui, &mut cue.post_wait)
                        .on_hover_text(t("Time after the action starts before the next cue auto-continues"));
                });
                ui.end_row();

            });
        }
        i if i == notes => {
            ui.add(TextEdit::multiline(&mut cue.notes).desired_rows(5).desired_width(f32::INFINITY).hint_text(t("Notes for the operator")));
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

/// Target picker. The menu lists every other cue; its labels are only made while it is open
/// (formatting thousands of labels every frame shows up in big shows).
fn target_combo(ui: &mut Ui, salt: &str, target: &mut Option<CueId>, ctx: &Ctx, none_label: &str) {
    let selected = match *target {
        None => none_label.into(),
        Some(t) => match ctx.list.find(t) {
            Some(c) => target_label(c, background_list(ctx.list, t)),
            None => super::theme::with_icon(super::icons::WARNING, crate::i18n::t("missing cue")),
        },
    };
    ComboBox::from_id_salt(salt).width(220.0).selected_text(selected).show_ui(ui, |ui| {
        ui.selectable_value(target, None, none_label);
        for (c, other_list) in ctx.list.iter_all_with_list() {
            if c.id != ctx.id {
                ui.selectable_value(target, Some(c.id), target_label(c, other_list));
            }
        }
    });
}

/// File picker row inside a grid; returns the probe result if known.
fn file_row(ui: &mut Ui, path: &Path, prober: &Prober, pick: &mut bool) -> Option<MediaInfo> {
    let mut info = None;
    ui.label(t("File"));
    ui.horizontal(|ui| {
        if ui.button(super::theme::with_icon(super::icons::FOLDER_OPEN, t("Browse…"))).clicked() {
            *pick = true;
        }
        if path.as_os_str().is_empty() {
            ui.label(RichText::new(t("none")).weak());
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
                        i.duration.map(fmt_time).unwrap_or_else(|| if i.still { t("still").into() } else { t("unknown length").into() }),
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
                ui.label(RichText::new(if prober.exists(path) { t("reading…") } else { t("file not found") }).weak());
            }
        }
        ui.end_row();
    }
    info
}

fn plays_row(ui: &mut Ui, play_count: &mut u32, infinite: &mut bool) {
    ui.label(t("Plays"));
    ui.horizontal(|ui| {
        ui.add_enabled(!*infinite, DragValue::new(play_count).range(1..=9999).suffix("×"));
        ui.checkbox(infinite, t("Loop forever"));
    });
    ui.end_row();
}

fn end_row(ui: &mut Ui, end: &mut Option<f64>, start: f64, file_dur: Option<f64>) {
    ui.label(t("End time"));
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
                ui.label(RichText::new(t("end of file")).weak());
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
        (Part::Wave, Card::new(ic::GRAPHIC_EQ, t("Waveform"), f32::INFINITY)),
        (Part::Trim, Card::new(ic::CROP, t("Trim"), 250.0)),
        (Part::Loops, Card::new(ic::REPLAY, t("Loops"), 250.0)),
        (Part::Playback, Card::new(ic::AV_TIMER, t("Playback"), 270.0)),
        (Part::Fades, Card::new(ic::TRENDING_DOWN, t("Fades"), 270.0)),
    ];
    if !a.slices.is_empty() {
        parts.push((Part::Slices, Card::new(ic::ALL_INCLUSIVE, t("Slices"), 460.0)));
    }
    let specs: Vec<Card> = parts.iter().map(|p| Card::new(p.1.glyph, p.1.title, p.1.width)).collect();
    super::theme::cards(ui, "atime", &specs, |ui, i| match parts[i].0 {
        Part::Wave => audio_waveform(ui, id, a, ctx, file_dur, pick),
        Part::Trim => {
            egui::Grid::new("atrim").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Start time"));
                time_edit(ui, &mut a.start_time);
                ui.end_row();
                end_row(ui, &mut a.end_time, a.start_time, file_dur);
                ui.label(t("Duration"));
                let probe = Cue::new(CueKind::Audio(a.clone()));
                ui.label(
                    RichText::new(effective_duration(&probe, file_dur).map(fmt_time).unwrap_or_else(|| t("∞ / unknown").into()))
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
            ui.label(RichText::new(t("Double-click the waveform to add slices that loop on their own.")).weak().small());
        }
        Part::Playback => {
            egui::Grid::new("arate").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Rate"));
                ui.horizontal(|ui| {
                    ui.add(DragValue::new(&mut a.rate).range(0.25..=4.0).speed(0.005).max_decimals(3).suffix("×"))
                        .on_hover_text(t("Playback speed; pitch follows (varispeed). Applies the next time the cue starts."));
                    if ui.small_button("1×").clicked() {
                        a.rate = 1.0;
                    }
                });
                ui.end_row();
                ui.label(t("Pitch"));
                let semis = 12.0 * (a.rate.max(1e-3)).log2();
                ui.label(RichText::new(tr!("{n} semitones", n = format!("{semis:+.2}"))).weak());
                ui.end_row();
            });
        }
        Part::Fades => {
            egui::Grid::new("afades").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Fade in"));
                time_edit(ui, &mut a.fade_in).on_hover_text(t("Fades up from silence when the cue starts"));
                ui.end_row();
                ui.label(t("Fade out"));
                time_edit(ui, &mut a.fade_out).on_hover_text(t("Fades down to silence at the end of the cue"));
                ui.end_row();
                ui.label(t("Curve"));
                ComboBox::from_id_salt("fade-curve").selected_text(a.fade_curve.label()).width(110.0).show_ui(ui, |ui| {
                    for c in FadeCurve::ALL {
                        ui.selectable_value(&mut a.fade_curve, c, c.label());
                    }
                });
                ui.end_row();
            });
            if a.fade_out > 0.0 && (a.infinite_loop || a.slices.iter().any(|s| s.plays == 0)) {
                ui.label(
                    RichText::new(t("The fade-out happens once the end is known: after a Devamp, never while looping forever."))
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
        ui.label(RichText::new(t("Reading the file…")).weak());
        return;
    }
    // Map wall-clock elapsed to a position in the file (rate + loops).
    let playhead = ctx.elapsed.map(|e| a.file_position(e * a.rate as f64, duration));
    if let Some(Err(e)) = &wf {
        ui.label(RichText::new(tr!("waveform: {e}", e)).color(RED));
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
                if ui.checkbox(&mut forever, t("loop until devamp")).changed() {
                    m.plays = if forever { 0 } else { 1 };
                }
            });
            if super::theme::icon_button(ui, super::icons::DELETE, t("Remove slice")).clicked() {
                remove = Some(i);
            }
            ui.end_row();
            prev = m.time;
        }
        ui.label(RichText::new(tr!("{time} – end", time = fmt_time(prev))).weak());
        ui.label(RichText::new(t("last slice uses Plays")).weak());
        ui.end_row();
    });
    if let Some(i) = remove {
        a.slices.remove(i);
    }
}

/// Shown where the media file is needed but missing: a way to pick it.
fn missing_file(ui: &mut Ui, path: &Path, pick: &mut bool) {
    ui.horizontal(|ui| {
        let msg = if path.as_os_str().is_empty() { t("No file selected.") } else { t("The file can't be found.") };
        ui.label(RichText::new(msg).color(if path.as_os_str().is_empty() { ui.visuals().weak_text_color() } else { RED }));
        if ui.button(super::theme::with_icon(super::icons::FOLDER_OPEN, t("Choose file…"))).clicked() {
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
    objects: &mut crate::spatial::ObjectAudio,
    stage: Option<Option<StageId>>,
    ctx: &Ctx,
    pick: &mut bool,
) {
    use super::icons as ic;
    use super::theme::Card;
    let info = ctx.prober.get(path).and_then(|r| r.ok());
    let specs = [
        Card::new(ic::UPLOAD_FILE, t("Input"), 400.0),
        Card::new(ic::SPEAKER, t("Output"), 360.0),
        Card::new(ic::GRID_VIEW, t("Routing"), routing_width(objects)),
    ];
    super::theme::cards(ui, "io", &specs, |ui, i| match i {
        0 => {
            egui::Grid::new("io-in").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                file_row(ui, path, ctx.prober, pick);
                if path.as_os_str().is_empty() {
                    return;
                }
                ui.label(t("Location"));
                ui.horizontal(|ui| {
                    if let Some(dir) = path.parent().filter(|_| ctx.prober.exists(path))
                        && super::theme::icon_button(ui, ic::FOLDER_OPEN, t("Show in folder")).clicked()
                    {
                        let _ = super::manual::open_with_default_app(dir);
                    }
                    ui.add(egui::Label::new(RichText::new(path.display().to_string()).font(super::theme::mono(11.5))).truncate())
                        .on_hover_text(path.display().to_string());
                });
                ui.end_row();
                if let Some(Some(len)) = ctx.prober.stat(path) {
                    ui.label(t("Size"));
                    let mb = len as f64 / 1_048_576.0;
                    ui.label(if mb >= 1.0 { format!("{mb:.1} MB") } else { format!("{:.0} KB", len as f64 / 1024.0) });
                    ui.end_row();
                }
                if let Some(i) = &info {
                    ui.label(t("Channels"));
                    ui.label(match i.channels {
                        0 => t("no audio").to_string(),
                        1 => t("1 (mono)").into(),
                        2 => t("2 (stereo)").into(),
                        n => n.to_string(),
                    });
                    ui.end_row();
                }
            });
        }
        1 => {
            egui::Grid::new("io-out").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                if let Some(stage) = stage {
                    ui.label(t("Picture"));
                    let name = stage
                        .and_then(|s| ctx.stages.iter().find(|x| x.0 == s))
                        .or(ctx.stages.first())
                        .map_or_else(|| t("no stage").to_string(), |s| tr!("stage \"{name}\"", name = s.1));
                    ui.label(name).on_hover_text(t("Set on the Geometry tab"));
                    ui.end_row();
                }
                ui.label(t("Device"));
                match &ctx.device {
                    Some(d) => ui.label(d.as_str()),
                    None => ui.label(RichText::new(t("no audio device")).color(RED)),
                }
                .on_hover_text(t("Settings → Audio"));
                ui.end_row();
                if ctx.device.is_some() {
                    ui.label(t("Outputs"));
                    ui.label(ctx.out_channels.to_string());
                    ui.end_row();
                }
            });
        }
        _ => match info.as_ref().map(|i| i.channels) {
            Some(0) => {
                ui.label(RichText::new(t("This file has no sound to route.")).weak());
            }
            ch => {
                let src = ch.unwrap_or(2).clamp(1, crate::model::MAX_SRC_CHANNELS);
                routing_card(ui, routing, objects, src, ctx);
            }
        },
    });
}

/// Width of the Routing card: the matrix gets a row of its own; the room fits beside the other
/// cards on a wide inspector.
fn routing_width(objects: &crate::spatial::ObjectAudio) -> f32 {
    if objects.enabled { 640.0 } else { f32::INFINITY }
}

/// The Routing card: crosspoints, or object audio placing each channel in the room.
fn routing_card(ui: &mut Ui, routing: &mut Option<Vec<Vec<f32>>>, objects: &mut crate::spatial::ObjectAudio, src: usize, ctx: &Ctx) {
    ui.horizontal(|ui| {
        ui.selectable_value(&mut objects.enabled, false, super::theme::with_icon(super::icons::GRID_VIEW, t("Crosspoints")));
        ui.selectable_value(&mut objects.enabled, true, super::theme::with_icon(super::icons::SURROUND_SOUND, t("Object audio")))
            .on_hover_text(t("Place each channel in the room (Settings → Audio); the speakers it is near play it"));
    });
    ui.add_space(4.0);
    if objects.enabled {
        super::room::object_editor(ui, objects, src, &ctx.room);
    } else {
        routing_editor(ui, routing, src, ctx.out_channels.min(crate::model::MAX_OUT_CHANNELS));
    }
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
        vfader(ui, main, t("Main"), FADER_TRACK, true, fader_w).on_hover_text(t("The cue's level"));
        ui.add_space(14.0);
        for (o, l) in levels.iter_mut().enumerate() {
            changed |= vfader(ui, l, &(o + 1).to_string(), FADER_TRACK, false, fader_w)
                .on_hover_text(tr!("Output {n} level, after the routing matrix", n = o + 1))
                .changed();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            ui.vertical(|ui| {
                ui.set_width(120.0);
                ui.add_space(20.0);
                let silent = *main <= MIN_DB && levels.iter().all(|l| *l <= MIN_DB);
                if ui.add_enabled(!silent, egui::Button::new(t("Set all silent")).min_size(egui::vec2(120.0, 0.0))).clicked() {
                    *main = MIN_DB;
                    levels.iter_mut().for_each(|l| *l = MIN_DB);
                    changed = true;
                }
                let flat = *main == 0.0 && levels.iter().all(|l| *l == 0.0);
                if ui
                    .add_enabled(!flat, egui::Button::new(t("Default levels")).min_size(egui::vec2(120.0, 0.0)))
                    .on_hover_text(t("Main and every output to 0 dB"))
                    .clicked()
                {
                    *main = 0.0;
                    levels.iter_mut().for_each(|l| *l = 0.0);
                    changed = true;
                }
                ui.add_space(6.0);
                ui.label(RichText::new(t("Crosspoints are on the I/O tab.")).weak().small());
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
    let specs = [Card::new(ic::CONTROL_CAMERA, t("Pan"), PAN_W), Card::new(ic::TUNE, t("Levels"), fader_bank_width(outs))];
    super::theme::cards(ui, "alevels", &specs, |ui, i| match i {
        0 => pan_card(ui, &mut a.pan),
        _ => fader_bank(ui, &mut a.volume_db, &mut a.out_levels, outs),
    });
}

fn video_levels(ui: &mut Ui, v: &mut VideoCue, outs: usize) {
    let specs = [super::theme::Card::new(super::icons::TUNE, t("Soundtrack levels"), f32::INFINITY)];
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
        if ui.checkbox(&mut custom, t("Custom crosspoints")).changed() {
            *routing = custom.then(|| {
                default_routing(src, outs)
                    .into_iter()
                    .map(|row| row.into_iter().map(|g| if g > 0.0 { gain_to_db(g) } else { MIN_DB }).collect())
                    .collect()
            });
        }
        ui.label(RichText::new(tr!("{src} in, {outs} out", src, outs)).weak());
    });
    let Some(m) = routing else {
        ui.label(RichText::new(t("Automatic: mono feeds outputs 1+2, multichannel maps 1:1.")).weak());
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
                ui.label(RichText::new(tr!("Out {n}", n = o + 1)).weak());
            }
            ui.end_row();
            for (s, row) in m.iter_mut().enumerate() {
                ui.label(RichText::new(tr!("In {n}", n = s + 1)).weak());
                for g in row.iter_mut() {
                    ui.add(
                        DragValue::new(g)
                            .range(MIN_DB..=12.0)
                            .speed(0.25)
                            .custom_formatter(|v, _| if v <= MIN_DB as f64 { "-∞".into() } else { format!("{v:.1}") }),
                    )
                    .on_hover_text(t("Crosspoint level in dB (drag to -60 for off)"));
                }
                ui.end_row();
            }
        });
    });
    ui.horizontal(|ui| {
        if ui.small_button(t("All off")).clicked() {
            m.iter_mut().flatten().for_each(|g| *g = MIN_DB);
        }
        if ui.small_button(t("Reset to default")).clicked() {
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
        vec![Card::new(ic::IMAGE, t("Still image"), 300.0), Card::new(ic::REPLAY, t("At end"), 300.0)]
    } else {
        vec![Card::new(ic::CROP, t("Trim"), 280.0), Card::new(ic::REPLAY, t("Loops"), 300.0)]
    };
    super::theme::cards(ui, "vtime", &specs, |ui, i| match (i, still) {
        (0, true) => {
            egui::Grid::new("vstill").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Show for"));
                ui.horizontal(|ui| {
                    time_edit(ui, &mut v.still_duration);
                    ui.label(RichText::new(if v.still_duration <= 0.0 { crate::i18n::t("until stopped") } else { "" }).weak());
                });
                ui.end_row();
            });
        }
        (0, false) => {
            egui::Grid::new("vtrim").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Start time"));
                time_edit(ui, &mut v.start_time);
                ui.end_row();
                end_row(ui, &mut v.end_time, v.start_time, file_dur);
                ui.label(t("Duration"));
                let probe = Cue::new(CueKind::Video(v.clone()));
                ui.label(
                    RichText::new(effective_duration(&probe, file_dur).map(fmt_time).unwrap_or_else(|| t("∞ / until stopped").into()))
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
                ui.label(t("At end"));
                ui.checkbox(&mut v.hold_last_frame, t("Hold last frame"));
                ui.end_row();
            });
        }
    });
}

fn stage_combo(ui: &mut Ui, salt: &str, stage: &mut Option<StageId>, stages: &[(StageId, String)]) {
    let label = stage
        .and_then(|s| stages.iter().find(|x| x.0 == s))
        .map(|s| s.1.clone())
        .unwrap_or_else(|| stages.first().map(|s| tr!("{name} (default)", name = s.1)).unwrap_or_default());
    ComboBox::from_id_salt(salt).selected_text(label).show_ui(ui, |ui| {
        ui.selectable_value(stage, None, t("Default (first stage)"));
        for (id, name) in stages {
            ui.selectable_value(stage, Some(*id), name);
        }
    });
}

pub(super) fn preview_toggle(ui: &mut Ui, open: &mut bool) {
    if cfg!(feature = "video") {
        ui.toggle_value(open, super::theme::with_icon(super::icons::PREVIEW, t("Preview")))
            .on_hover_text(t("Show where this layer lands on its stage (View → Geometry preview)"));
    }
}

/// Geometry tab of a video or text cue: where the layer goes, then how it's placed.
fn layer_geometry(ui: &mut Ui, salt: &str, layer: LayerRefs, ctx: &Ctx, preview: &mut bool) {
    use super::icons as ic;
    use super::theme::Card;
    let LayerRefs { stage, mut fit, layer, blend_mode, opacity, geometry } = layer;
    let specs = [Card::new(ic::DESKTOP_WINDOWS, t("Layer"), 320.0), Card::new(ic::OPEN_WITH, t("Geometry"), 520.0)];
    super::theme::cards(ui, salt, &specs, |ui, i| match i {
        0 => {
            egui::Grid::new((salt, "layer")).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Stage"));
                stage_combo(ui, &format!("{salt}-stage"), stage, ctx.stages);
                ui.end_row();
                if let Some(fit) = fit.as_deref_mut() {
                    ui.label(t("Fit"));
                    ComboBox::from_id_salt((salt, "fit")).selected_text(fit.label()).show_ui(ui, |ui| {
                        for f in VideoFit::ALL {
                            ui.selectable_value(fit, f, f.label());
                        }
                    });
                    ui.end_row();
                }
                ui.label(t("Layer"));
                ui.add(DragValue::new(layer).range(-100..=100)).on_hover_text(t("Higher layers draw on top"));
                ui.end_row();
                ui.label(t("Opacity"));
                ui.add(Slider::new(opacity, 0.0..=1.0));
                ui.end_row();
                ui.label(t("Blend"));
                ComboBox::from_id_salt((salt, "blend")).selected_text(blend_mode.label()).show_ui(ui, |ui| {
                    for m in BlendMode::ALL {
                        ui.selectable_value(blend_mode, m, m.label());
                    }
                })
                .response
                .on_hover_text(t("How this layer combines with the layers below it"));
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
        ui.label(t("Position"));
        ui.horizontal(|ui| {
            ui.label("X");
            ui.add(DragValue::new(&mut g.x).speed(0.002).range(-2.0..=2.0).custom_formatter(pct).custom_parser(parse_pct));
            ui.label("Y");
            ui.add(DragValue::new(&mut g.y).speed(0.002).range(-2.0..=2.0).custom_formatter(pct).custom_parser(parse_pct));
        });
        ui.end_row();
        ui.label(t("Scale"));
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
            ui.checkbox(&mut lock, super::theme::with_icon(glyph, t("proportional")));
        });
        ui.end_row();
        ui.label(t("Rotation"));
        ui.add(Slider::new(&mut g.rotation, -180.0..=180.0).suffix("°"));
        ui.end_row();
        ui.label(t("Crop"));
        ui.horizontal(|ui| {
            for (i, side) in ["L", "T", "R", "B"].into_iter().enumerate() {
                ui.label(side);
                // Opposite edges can't cross.
                let max = (1.0 - g.crop[(i + 2) % 4]).max(0.0);
                ui.add(
                    DragValue::new(&mut g.crop[i]).speed(0.002).range(0.0..=max).custom_formatter(pct).custom_parser(parse_pct),
                );
            }
            if g.crop != [0.0; 4] && ui.small_button(t("Clear")).clicked() {
                g.crop = [0.0; 4];
            }
        });
        ui.end_row();
        ui.label("");
        if ui.button(t("Reset geometry")).clicked() {
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
    // Cues that only point at another cue: one card with the target.
    let target_card = |ui: &mut Ui, label: &str, salt: &str, target: &mut Option<CueId>, none: &str, note: Option<&str>| {
        let specs = [Card::new(ic::ARROW_RIGHT, t("Target"), 420.0)];
        cards(ui, salt, &specs, |ui, _| {
            egui::Grid::new(salt).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(label);
                target_combo(ui, salt, target, ctx, none);
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
            let specs = [Card::new(ic::FOLDER, t("Group"), 460.0)];
            cards(ui, "group", &specs, |ui, _| {
                egui::Grid::new("group").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label(t("Mode"));
                    ComboBox::from_id_salt("gmode").width(300.0).selected_text(mode.label()).show_ui(ui, |ui| {
                        for m in GroupMode::ALL {
                            ui.selectable_value(mode, m, m.label());
                        }
                    });
                    ui.end_row();
                    ui.label(t("Children"));
                    ui.label(format!("{}", children.len()));
                    ui.end_row();
                });
                ui.add_space(4.0);
                ui.label(RichText::new(t("Drag cues onto the middle of a group row to put them inside it.")).weak().small());
            });
        }
        CueKind::Wait { duration } => {
            let specs = [Card::new(ic::HOURGLASS_EMPTY, t("Wait"), 300.0)];
            cards(ui, "wait", &specs, |ui, _| {
                egui::Grid::new("wait").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label(t("Duration"));
                    time_edit(ui, duration);
                    ui.end_row();
                });
            });
        }
        CueKind::Fade(f) => {
            // Only what the target can do: no geometry for a sound cue, no level for a text cue.
            // With no target yet, everything is offered.
            let caps = f.target.map_or(FadeCaps { level: true, picture: true, objects: false }, |t| ctx.fade_caps(t));
            let mut specs = vec![Card::new(ic::TUNE, t("Changes"), 300.0), Card::new(ic::TRENDING_DOWN, t("Fade"), 420.0)];
            // The extra cards, in order: geometry, object positions, video effects.
            let mut extra = Vec::new();
            if f.geometry.is_some() && caps.picture {
                specs.push(Card::new(ic::OPEN_WITH, t("Target geometry"), f32::INFINITY));
                extra.push(0);
            }
            if f.objects.is_some() && caps.objects {
                specs.push(Card::new(ic::SURROUND_SOUND, t("Target object positions"), f32::INFINITY));
                extra.push(1);
            }
            if f.video_fx.is_some() && caps.picture {
                specs.push(Card::new(ic::AUTO_FIX_HIGH, t("Target video effects"), f32::INFINITY));
                extra.push(2);
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
                        ui.label(RichText::new(t("The target has nothing to fade: pick a sound, video, text, camera or group cue.")).weak());
                        return;
                    }
                    egui::Grid::new("fade-changes").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                        if caps.level {
                            ui.label(t("Level"));
                            ui.horizontal(|ui| {
                                let mut on = f.volume_db.is_some();
                                if ui.checkbox(&mut on, "").changed() {
                                    f.volume_db = on.then_some(MIN_DB);
                                }
                                if let Some(db) = &mut f.volume_db {
                                    db_edit(ui, db);
                                } else {
                                    ui.label(RichText::new(t("unchanged")).weak());
                                }
                            });
                            ui.end_row();
                        }
                        if caps.picture {
                            ui.label(t("Opacity"));
                            ui.horizontal(|ui| {
                                let mut on = f.opacity.is_some();
                                if ui.checkbox(&mut on, "").changed() {
                                    f.opacity = on.then_some(0.0);
                                }
                                if let Some(o) = &mut f.opacity {
                                    ui.add(Slider::new(o, 0.0..=1.0));
                                } else {
                                    ui.label(RichText::new(t("unchanged")).weak());
                                }
                            });
                            ui.end_row();
                            ui.label(t("Geometry"));
                            let mut on = f.geometry.is_some();
                            if ui.checkbox(&mut on, t("Animate position / scale / rotation")).changed() {
                                // Start from the target's own geometry so only what you change moves.
                                f.geometry =
                                    on.then(|| f.target.and_then(|t| ctx.geometry(t)).unwrap_or_default());
                            }
                            ui.end_row();
                            ui.label(t("Video effects"));
                            let mut on = f.video_fx.is_some();
                            if ui.checkbox(&mut on, t("Fade effect settings")).changed() {
                                f.video_fx = on.then(|| f.target.and_then(|t| ctx.video_fx(t)).unwrap_or_default());
                            }
                            ui.end_row();
                        }
                        if caps.objects {
                            ui.label(t("Objects"));
                            let mut on = f.objects.is_some();
                            if ui.checkbox(&mut on, t("Move object audio")).changed() {
                                f.objects = on.then(|| f.target.and_then(|t| ctx.objects(t)).unwrap_or_default());
                            }
                            ui.end_row();
                        }
                    });
                }
                1 => {
                    egui::Grid::new("fade").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                        ui.label(t("Target"));
                        target_combo(ui, "fade_target", &mut f.target, ctx, t("— none —"));
                        ui.end_row();
                        ui.label(t("Duration"));
                        time_edit(ui, &mut f.duration);
                        ui.end_row();
                        ui.label(t("Curve"));
                        ComboBox::from_id_salt("curve").selected_text(f.curve.label()).show_ui(ui, |ui| {
                            for c in FadeCurve::ALL {
                                ui.selectable_value(&mut f.curve, c, c.label());
                            }
                            if let FadeCurve::Custom(_) = f.curve {
                                let _ = ui.selectable_label(true, t("Custom"));
                            }
                        });
                        ui.end_row();
                        ui.label(t("When done"));
                        ui.checkbox(&mut f.stop_target_when_done, t("Stop target"));
                        ui.end_row();
                    });
                    ui.add_space(6.0);
                    super::widgets::curve_graph(ui, "fade-curve", &mut f.curve, f.duration, falling, caps.level);
                }
                i => match extra.get(i - 2) {
                    Some(0) => {
                        if let Some(g) = &mut f.geometry {
                            geometry_editor(ui, "fgeom", g);
                        }
                    }
                    Some(1) => {
                        if let Some(objs) = &mut f.objects {
                            let mut o = crate::spatial::ObjectAudio { enabled: true, objects: std::mem::take(objs) };
                            let src = o.objects.len().max(1);
                            super::room::object_editor(ui, &mut o, src, &ctx.room);
                            *objs = o.objects;
                        }
                    }
                    _ => fade_video_fx(ui, f, ctx),
                },
            });
        }
        CueKind::Start { target } => target_card(ui, t("Start"), "start_target", target, t("— none —"), None),
        CueKind::Stop { target } => target_card(ui, t("Stop"), "stop_target", target, t("All cues"), None),
        CueKind::Pause { target } => target_card(ui, t("Pause"), "pause_target", target, t("All cues"), None),
        CueKind::Goto { target } => target_card(ui, t("Move playhead to"), "goto_target", target, t("— none —"), None),
        CueKind::Devamp { target } => target_card(
            ui,
            t("Devamp"),
            "devamp_target",
            target,
            t("— none —"),
            Some(t("Lets the target's looping slice finish its current pass, then play on to the next slice.")),
        ),
        CueKind::Text(t) => text_settings(ui, t),
        CueKind::Camera(c) => camera_settings(ui, c, ctx, refresh),
        CueKind::Network(n) => {
            let specs = [Card::new(ic::SETTINGS_REMOTE, t("OSC message"), 480.0)];
            cards(ui, "net", &specs, |ui, _| {
                egui::Grid::new("net").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label(t("Destination"));
                    ui.horizontal(|ui| {
                        ui.add(TextEdit::singleline(&mut n.host).desired_width(160.0));
                        ui.label(":");
                        ui.add(DragValue::new(&mut n.port).range(1..=65535));
                    });
                    ui.end_row();
                    ui.label(t("OSC address"));
                    ui.add(TextEdit::singleline(&mut n.address).desired_width(260.0));
                    ui.end_row();
                    ui.label(t("Arguments"));
                    ui.add(TextEdit::singleline(&mut n.args).desired_width(260.0).hint_text(r#"1 0.5 "text""#));
                    ui.end_row();
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new(t("Sends one OSC message over UDP. Numbers are sent as int/float, other words as strings."))
                        .weak()
                        .small(),
                );
            });
        }
        _ => {}
    }
}

/// A Fade cue's target video effect settings: the target's stack (taken when the option was
/// switched on), with values to fade to.
fn fade_video_fx(ui: &mut Ui, f: &mut crate::model::FadeCue, ctx: &Ctx) {
    let current = f.target.and_then(|t| ctx.video_fx(t));
    let Some(fx) = &mut f.video_fx else { return };
    let kinds = |v: &[crate::effects::VideoEffect]| v.iter().map(|e| e.kind).collect::<Vec<_>>();
    ui.horizontal(|ui| {
        if let Some(cur) = &current
            && kinds(cur) != kinds(fx)
        {
            ui.label(RichText::new(t("The target's effects changed since these were taken.")).color(super::widgets::AMBER));
        }
        if ui.small_button(t("Take the target's effects")).on_hover_text(t("Start again from the target's current effects")).clicked() {
            *fx = current.clone().unwrap_or_default();
        }
    });
    if fx.is_empty() {
        ui.label(RichText::new(t("The target has no video effects: add them on its Video FX tab.")).weak());
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for (i, e) in fx.iter_mut().enumerate() {
            super::theme::card_frame().show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_width(280.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{}. {}", i + 1, e.kind.label())).strong());
                        ui.checkbox(&mut e.enabled, t("On")).on_hover_text(t("Off: fade this effect out"));
                    });
                    super::effects::video_effect_settings(ui, 100 + i, e);
                });
            });
        }
    });
    ui.label(RichText::new(t("Effects off here fade out; effects the target doesn't have fade in from nothing.")).weak().small());
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
        Card::new(ic::TEXT_FIELDS, crate::i18n::t("Text"), 360.0),
        Card::new(ic::NOTES, crate::i18n::t("Font"), 520.0),
        Card::new(ic::PALETTE, crate::i18n::t("Look"), 520.0),
        Card::new(ic::PREVIEW, crate::i18n::t("Sample"), 360.0),
    ];
    super::theme::cards(ui, "text", &specs, |ui, i| match i {
        0 => {
            ui.add(TextEdit::multiline(&mut t.text).desired_rows(3).desired_width(f32::INFINITY).hint_text(crate::i18n::t("Text to show")));
            ui.add_space(4.0);
            egui::Grid::new("text-time").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(crate::i18n::t("Show for"));
                ui.horizontal(|ui| {
                    time_edit(ui, &mut t.duration);
                    ui.label(RichText::new(if t.duration <= 0.0 { crate::i18n::t("until stopped") } else { "" }).weak());
                });
                ui.end_row();
            });
        }
        1 => {
            egui::Grid::new("text-font").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(crate::i18n::t("Font"));
                ui.horizontal(|ui| {
                    font_picker(ui, &mut t.font);
                    if text::families_ready() && !text::is_installed(&t.font) {
                        ui.label(RichText::new(crate::i18n::t("not installed here: a fallback font is used")).color(super::widgets::AMBER))
                            .on_hover_text(crate::i18n::t("Install this font on every computer that plays the show"));
                    }
                });
                ui.end_row();

                ui.label(crate::i18n::t("Weight"));
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
                    .on_hover_text(crate::i18n::t("Weights the font doesn't include are greyed out; the nearest one is used"));
                    if w != t.effective_weight() {
                        t.weight = w;
                        t.bold = w >= 600;
                    }
                    ui.checkbox(&mut t.italic, crate::i18n::t("Italic"));
                });
                ui.end_row();

                ui.label(crate::i18n::t("Size"));
                ui.horizontal(|ui| {
                    ui.add(DragValue::new(&mut t.size).range(4.0..=1000.0).speed(1.0).suffix(" px"));
                    ui.label(crate::i18n::t("Line height"));
                    ui.add(DragValue::new(&mut t.line_height).range(0.5..=4.0).speed(0.01).fixed_decimals(2).suffix("×"));
                    ui.label(crate::i18n::t("Spacing"));
                    ui.add(DragValue::new(&mut t.letter_spacing).range(-0.5..=2.0).speed(0.002).fixed_decimals(3).suffix(" em"))
                        .on_hover_text(crate::i18n::t("Extra space between letters"));
                });
                ui.end_row();

            });
        }
        2 => {
            egui::Grid::new("text-look").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(crate::i18n::t("Colour"));
                ui.horizontal(|ui| {
                    color_button(ui, &mut t.color);
                    use crate::model::TextAlign;
                    for (a, label) in [(TextAlign::Left, "Left"), (TextAlign::Center, "Centre"), (TextAlign::Right, "Right")] {
                        ui.selectable_value(&mut t.align, a, crate::i18n::t(label));
                    }
                });
                ui.end_row();

                ui.label(crate::i18n::t("Outline"));
                ui.horizontal(|ui| {
                    ui.add(DragValue::new(&mut t.outline_width).range(0.0..=100.0).speed(0.1).fixed_decimals(1).suffix(" px"));
                    color_button(ui, &mut t.outline_color);
                    ui.label(RichText::new(if t.outline_width <= 0.0 { crate::i18n::t("off") } else { "" }).weak());
                });
                ui.end_row();

                ui.label(crate::i18n::t("Shadow"));
                ui.horizontal(|ui| {
                    let mut on = t.shadow_color[3] > 0;
                    if ui.checkbox(&mut on, "").changed() {
                        t.shadow_color[3] = if on { 200 } else { 0 };
                    }
                    ui.add_enabled_ui(on, |ui| {
                        color_button(ui, &mut t.shadow_color);
                        ui.label(crate::i18n::t("offset"));
                        ui.add(DragValue::new(&mut t.shadow_offset[0]).range(-200.0..=200.0).speed(0.2).suffix(" px"));
                        ui.add(DragValue::new(&mut t.shadow_offset[1]).range(-200.0..=200.0).speed(0.2).suffix(" px"));
                        ui.label(crate::i18n::t("blur"));
                        ui.add(DragValue::new(&mut t.shadow_blur).range(0.0..=100.0).speed(0.2).suffix(" px"));
                    });
                });
                ui.end_row();

                ui.label(crate::i18n::t("Background"));
                ui.horizontal(|ui| {
                    let mut on = t.bg_color[3] > 0;
                    if ui.checkbox(&mut on, "").on_hover_text(crate::i18n::t("A box behind the text")).changed() {
                        t.bg_color[3] = if on { 255 } else { 0 };
                    }
                    ui.add_enabled_ui(on, |ui| {
                        color_button(ui, &mut t.bg_color);
                        ui.label(crate::i18n::t("padding"));
                        ui.add(DragValue::new(&mut t.bg_padding).range(0.0..=500.0).speed(0.5).suffix(" px"));
                        ui.label(crate::i18n::t("radius"));
                        ui.add(DragValue::new(&mut t.bg_radius).range(0.0..=500.0).speed(0.5).suffix(" px"))
                            .on_hover_text(crate::i18n::t("Rounded corners"));
                    });
                });
                ui.end_row();

            });
        }
        _ => {
            text_sample(ui, t);
            ui.add_space(4.0);
            ui.label(
                RichText::new(crate::i18n::t("Text appears on the stage like a video layer: use the Geometry and Animation tabs and Fade cues to place, animate and fade it."))
                    .weak()
                    .small(),
            );
        }
    });
}

/// Searchable list of installed font families.
fn font_picker(ui: &mut Ui, font: &mut String) {
    use crate::media::text;
    let label = if font.is_empty() { tr!("{name} (built-in)", name = text::BUILT_IN) } else { font.clone() };
    let search_id = ui.id().with("font-search");
    ComboBox::from_id_salt("font").selected_text(label).width(240.0).height(360.0).show_ui(ui, |ui| {
        if !text::families_ready() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(t("Loading installed fonts…"));
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            return;
        }
        let mut q = ui.data(|d| d.get_temp::<String>(search_id)).unwrap_or_default();
        let r = ui.add(TextEdit::singleline(&mut q).hint_text(t("Search fonts")).desired_width(220.0));
        if q.is_empty() && !r.has_focus() {
            r.request_focus();
        }
        ui.data_mut(|d| d.insert_temp(search_id, q.clone()));
        let q = q.to_lowercase();
        ui.selectable_value(font, String::new(), tr!("{name} (built-in)", name = text::BUILT_IN));
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
    let first = t.text.lines().find(|l| !l.trim().is_empty()).unwrap_or(crate::i18n::t("Sample")).chars().take(60).collect::<String>();
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
    .on_hover_text(tr!("Timecode at {rate} (type hh:mm:ss:ff)", rate = rate.label()))
}

/// Combo of choices by (value, label), with a first "default" entry for None.
fn device_combo(ui: &mut Ui, salt: &str, value: &mut Option<String>, choices: &[(String, String)], default: &str) {
    let label = match value {
        None => default.to_string(),
        Some(v) => choices.iter().find(|c| &c.0 == v).map_or_else(|| tr!("{v} (not found)", v), |c| c.1.clone()),
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
    } else if super::theme::icon_button(ui, super::icons::REFRESH, t("Look for devices again")).clicked() {
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
        Card::new(ic::MIC, t("Input"), 420.0),
        Card::new(ic::SCHEDULE, t("Timing"), 260.0),
        Card::new(ic::GRID_VIEW, t("Routing"), routing_width(&m.objects)),
    ];
    let inputs: Vec<(String, String)> = ctx.devices.audio_in.iter().map(|d| (d.id.clone(), d.name.clone())).collect();
    super::theme::cards(ui, "mic", &specs, |ui, i| match i {
        0 => {
            egui::Grid::new("mic-in").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Device"));
                ui.horizontal(|ui| {
                    device_combo(ui, "mic-dev", &mut m.device, &inputs, t("System default input"));
                    refresh_button(ui, ctx, refresh);
                });
                ui.end_row();
                ui.label(t("Channels"));
                ui.horizontal(|ui| {
                    let mut first = m.first_channel + 1;
                    ui.add(DragValue::new(&mut first).range(1..=64).prefix(t("from ")));
                    m.first_channel = first - 1;
                    ui.add(DragValue::new(&mut m.channels).range(1..=crate::model::MAX_SRC_CHANNELS as u16).suffix(" ch"));
                    ui.label(RichText::new(match m.channels {
                        1 => tr!("input {n}", n = m.first_channel + 1),
                        n => tr!("inputs {first}–{last}", first = m.first_channel + 1, last = m.first_channel + n),
                    }).weak());
                });
                ui.end_row();
                ui.label(t("Level"));
                ui.horizontal(|ui| {
                    level_bar(ui, ctx.mic_level);
                    if ctx.mic_level.is_none() {
                        ui.label(RichText::new(t("shown while the cue runs")).weak().small());
                    }
                });
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(RichText::new(t("Live input is delayed by about 30 ms on its way through the mixer.")).weak().small());
        }
        1 => {
            egui::Grid::new("mic-time").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Duration"));
                ui.horizontal(|ui| {
                    time_edit(ui, &mut m.duration);
                    ui.label(RichText::new(if m.duration <= 0.0 { crate::i18n::t("until stopped") } else { "" }).weak());
                });
                ui.end_row();
                ui.label(t("Fade in"));
                time_edit(ui, &mut m.fade_in).on_hover_text(t("Fades up from silence when the cue starts"));
                ui.end_row();
            });
        }
        _ => routing_card(ui, &mut m.routing, &mut m.objects, (m.channels as usize).clamp(1, crate::model::MAX_SRC_CHANNELS), ctx),
    });
}

fn mic_levels(ui: &mut Ui, m: &mut crate::model::MicCue, outs: usize) {
    use super::icons as ic;
    use super::theme::Card;
    let specs = [Card::new(ic::CONTROL_CAMERA, t("Pan"), PAN_W), Card::new(ic::TUNE, t("Levels"), fader_bank_width(outs))];
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
    if ui.add_enabled(*pan != 0.0, egui::Button::new(t("Centre")).small()).clicked() {
        *pan = 0.0;
    }
    ui.add_space(4.0);
    ui.label(RichText::new(t("Pans between outputs 1 and 2 when the routing is automatic.")).weak().small());
}

/// Capture sizes offered for cameras.
const CAMERA_SIZES: [[u32; 2]; 4] = [[640, 480], [1280, 720], [1920, 1080], [3840, 2160]];

fn camera_settings(ui: &mut Ui, c: &mut crate::model::CameraCue, ctx: &Ctx, refresh: &mut bool) {
    use super::icons as ic;
    use super::theme::Card;
    let specs = [Card::new(ic::PHOTO_CAMERA, t("Camera"), 460.0), Card::new(ic::SCHEDULE, t("Timing"), 300.0)];
    super::theme::cards(ui, "camera", &specs, |ui, i| match i {
        0 => {
            egui::Grid::new("cam").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Device"));
                ui.horizontal(|ui| {
                    if c.format.is_some() {
                        // A source typed by hand (e.g. a test pattern): show it as text.
                        let mut text = c.device.clone().unwrap_or_default();
                        if ui.add(TextEdit::singleline(&mut text).desired_width(260.0)).changed() {
                            c.device = (!text.is_empty()).then_some(text);
                        }
                    } else {
                        device_combo(ui, "cam-dev", &mut c.device, &ctx.devices.cameras, t("First camera found"));
                    }
                    refresh_button(ui, ctx, refresh);
                });
                ui.end_row();
                if ctx.devices.loaded && ctx.devices.cameras.is_empty() && c.format.is_none() {
                    ui.label("");
                    ui.label(RichText::new(t("No cameras found.")).color(super::widgets::AMBER));
                    ui.end_row();
                }
                ui.label(t("Size"));
                let label = c.size.map_or_else(|| t("Camera default").to_string(), |[w, h]| format!("{w} × {h}"));
                ComboBox::from_id_salt("cam-size").selected_text(label).show_ui(ui, |ui| {
                    ui.selectable_value(&mut c.size, None, t("Camera default"));
                    for s in CAMERA_SIZES {
                        ui.selectable_value(&mut c.size, Some(s), format!("{} × {}", s[0], s[1]));
                    }
                });
                ui.end_row();
                ui.label(t("Mirror"));
                ui.checkbox(&mut c.mirror, t("Flip left to right"));
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(RichText::new(t("Device and size changes apply the next time the cue starts.")).weak().small());
        }
        _ => {
            egui::Grid::new("cam-time").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Show for"));
                ui.horizontal(|ui| {
                    time_edit(ui, &mut c.duration);
                    ui.label(RichText::new(if c.duration <= 0.0 { crate::i18n::t("until stopped") } else { "" }).weak());
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
    let specs = [Card::new(ic::KEYBOARD, t("Hotkey"), 340.0), Card::new(ic::TIMER_PLAY, t("Timecode"), 460.0)];
    super::theme::cards(ui, "triggers", &specs, |ui, i| match i {
        0 => {
            egui::Grid::new("trig-key").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Key"));
                let mut text = cue.hotkey.clone().unwrap_or_default();
                let valid = text.is_empty() || egui::Key::from_name(&text).is_some();
                let r = ui.add(
                    TextEdit::singleline(&mut text)
                        .desired_width(80.0)
                        .hint_text(t("no key"))
                        .text_color_opt((!valid).then_some(RED)),
                );
                if r.changed() {
                    cue.hotkey = (!text.trim().is_empty()).then(|| text.trim().to_string());
                }
                r.on_hover_text(t("Key that fires this cue (e.g. F1, A, Num5). Not Space/P/R/Esc."));
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(RichText::new(t("Fires the cue directly, whatever the playhead is on.")).weak().small());
        }
        _ => {
            let sync = &ctx.list_tc;
            egui::Grid::new("trig-tc").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t("Fire at"));
                ui.horizontal(|ui| {
                    let mut on = cue.timecode_trigger.is_some();
                    if ui.checkbox(&mut on, "").on_hover_text(t("Fire when the cue list's timecode reaches this time")).changed() {
                        cue.timecode_trigger = on.then_some(3600.0);
                    }
                    match &mut cue.timecode_trigger {
                        Some(t) => {
                            timecode_edit(ui, t, sync.rate);
                        }
                        None => {
                            ui.label(RichText::new(t("off")).weak());
                        }
                    }
                });
                ui.end_row();
                ui.label(t("Cue list"));
                ui.horizontal(|ui| {
                    let text = match sync.source {
                        TimecodeSource::Off => t("doesn't follow timecode").to_string(),
                        TimecodeSource::Ltc => tr!("follows LTC at {rate}", rate = sync.rate.label()),
                        TimecodeSource::Mtc => tr!("follows MTC at {rate}", rate = sync.rate.label()),
                    };
                    let color = if sync.source == TimecodeSource::Off && cue.timecode_trigger.is_some() {
                        super::widgets::AMBER
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    ui.label(RichText::new(text).color(color));
                    if ui.small_button(t("Set up…")).on_hover_text(t("The cue list's timecode settings")).clicked() {
                        setup = true;
                    }
                });
                ui.end_row();
                if sync.source != TimecodeSource::Off {
                    ui.label(t("Receiving"));
                    match &ctx.tc_in {
                        Some(t) => ui.label(RichText::new(t).font(super::theme::mono(13.0)).color(super::theme::GREEN)),
                        None => ui.label(RichText::new(t("no timecode")).color(super::widgets::AMBER)),
                    };
                    ui.end_row();
                }
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(t("The cue fires as running timecode passes this time. Jumps and dropouts don't fire cues."))
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
        assert_eq!(caps(KindTag::Audio), FadeCaps { level: true, picture: false, objects: false });
        assert_eq!(caps(KindTag::Text), FadeCaps { level: false, picture: true, objects: false });
        assert_eq!(caps(KindTag::Video), FadeCaps { level: true, picture: true, objects: false });
        assert_eq!(caps(KindTag::Wait), FadeCaps::default());
        let still = crate::model::VideoCue { path: "logo.svg".into(), ..Default::default() };
        assert_eq!(FadeCaps::of(&Cue::new(CueKind::Video(still))), FadeCaps { level: false, picture: true, objects: false });
        let group = CueKind::Group {
            mode: GroupMode::StartAll,
            children: vec![Cue::new(KindTag::Audio.default_kind()), Cue::new(KindTag::Camera.default_kind())],
        };
        assert_eq!(FadeCaps::of(&Cue::new(group)), FadeCaps { level: true, picture: true, objects: false });
    }
}
