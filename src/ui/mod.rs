//! egui front end.

mod cue_list;
mod eq;
mod inspector;
#[cfg(feature = "video")]
mod stage;
mod waveform;
mod widgets;

use crate::media::audio::DeviceEntry;
use crate::media::{self, Prober, Waveforms};
use crate::model::{AudioCue, Cue, CueId, CueKind, GroupMode, KindTag, Stage, StageId, VideoCue, Workspace};
use crate::show::{SharedShow, Show};
use egui::{Color32, Key, KeyboardShortcut, Modifiers, RichText};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use widgets::{AMBER, GREEN, RED, fmt_time};

/// Work that needs a native dialog; done after the show lock is released so the engine keeps
/// running while a dialog is open.
enum Deferred {
    New,
    Open,
    Save,
    SaveAs,
    PickMedia(CueId),
    AddMediaCues,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    General,
    Audio,
    Stages,
}

pub struct App {
    show: SharedShow,
    prober: Arc<Prober>,
    /// Selected cues; the last one is the primary (shown in the inspector).
    selection: Vec<CueId>,
    collapsed: HashSet<CueId>,
    /// Cues being dragged in the cue list.
    drag: Option<Vec<CueId>>,
    last_playhead: Option<CueId>,
    deferred: Vec<Deferred>,
    /// Stage windows currently open / fullscreen / in mapping-edit mode.
    stage_open: HashSet<StageId>,
    stage_fullscreen: HashSet<StageId>,
    mapping_edit: HashSet<StageId>,
    waveforms: Arc<Waveforms>,
    inspector_tab: inspector::Tab,
    settings_tab: SettingsTab,
    /// Cached audio device list for the settings window.
    devices: Option<Vec<DeviceEntry>>,
    #[cfg(feature = "video")]
    textures: std::collections::HashMap<CueId, (egui::TextureHandle, u64)>,
    meters: Vec<f32>,
    status: Option<(String, Instant)>,
    show_settings: bool,
    show_log: bool,
    scroll_to: Option<CueId>,
    title: String,
    allow_close: bool,
    frame_count: u64,
    debug_t0: Option<Instant>,
    debug_gos: u64,
}

impl App {
    pub fn new(show: SharedShow, prober: Arc<Prober>) -> Self {
        Self {
            show,
            prober,
            selection: Vec::new(),
            collapsed: HashSet::new(),
            drag: None,
            last_playhead: None,
            deferred: Vec::new(),
            stage_open: HashSet::new(),
            stage_fullscreen: HashSet::new(),
            mapping_edit: HashSet::new(),
            waveforms: Waveforms::new(),
            inspector_tab: Default::default(),
            settings_tab: SettingsTab::General,
            devices: None,
            #[cfg(feature = "video")]
            textures: Default::default(),
            meters: Vec::new(),
            status: None,
            show_settings: false,
            show_log: false,
            scroll_to: None,
            title: String::new(),
            allow_close: false,
            frame_count: 0,
            debug_t0: None,
            debug_gos: 0,
        }
    }

    fn primary(&self) -> Option<CueId> {
        self.selection.last().copied()
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// Select one cue and put the playhead on it (selection drives standby).
    fn select(&mut self, show: &mut Show, id: Option<CueId>) {
        self.selection = id.into_iter().collect();
        show.engine.set_playhead(id);
        self.last_playhead = id;
        self.scroll_to = id;
        // Reveal the cue if it's inside collapsed groups.
        if let Some(id) = id {
            let mut p = show.ws.cue_list.parent_of(id);
            while let Some(g) = p {
                self.collapsed.remove(&g);
                p = show.ws.cue_list.parent_of(g);
            }
        }
    }

    /// Follow the playhead when the engine moves it (GO, Go To cues).
    fn sync_playhead(&mut self, show: &Show) {
        if show.engine.playhead != self.last_playhead {
            self.last_playhead = show.engine.playhead;
            self.selection = show.engine.playhead.into_iter().collect();
            self.scroll_to = show.engine.playhead;
        }
        self.selection.retain(|id| show.ws.cue_list.find(*id).is_some());
    }

    // ------------------------------------------------------------------ editing

    fn add_cue(&mut self, show: &mut Show, tag: KindTag) -> CueId {
        let mut cue = Cue::new(tag.default_kind());
        cue.number = show.ws.cue_list.next_number();
        // Control cues target the currently selected cue.
        let sel = self.primary();
        if let (Some(t), Some(sel)) = (cue.kind.target_mut(), sel) {
            *t = Some(sel);
        }
        if let (CueKind::Fade(_), Some(sel)) = (&cue.kind, sel)
            && let Some(target) = show.ws.cue_list.find(sel)
        {
            cue.name = format!("Fade out {}", target.display_name());
        }
        let id = cue.id;
        show.ws.cue_list.insert_after(sel, cue);
        show.edited();
        self.select(show, Some(id));
        id
    }

    fn add_media_cue(&mut self, show: &mut Show, path: PathBuf) -> Option<CueId> {
        let kind = if media::is_video_file(&path) {
            CueKind::Video(VideoCue { path: path.clone(), ..Default::default() })
        } else if media::is_audio_path(&path) {
            CueKind::Audio(AudioCue { path: path.clone(), ..Default::default() })
        } else {
            self.notify(format!("Unsupported file: {}", path.display()));
            return None;
        };
        let mut cue = Cue::new(kind);
        cue.number = show.ws.cue_list.next_number();
        let id = cue.id;
        show.ws.cue_list.insert_after(self.primary(), cue);
        self.prober.get(&path);
        show.edited();
        self.select(show, Some(id));
        Some(id)
    }

    fn delete_selected(&mut self, show: &mut Show) {
        if self.selection.is_empty() {
            return;
        }
        let rows = show.ws.cue_list.visible_rows(&|id| self.collapsed.contains(&id));
        let first_row = rows.iter().position(|(_, id)| self.selection.contains(id));
        for id in self.selection.clone() {
            let Show { ws, engine, media, .. } = show;
            engine.stop_cue(ws, media, id);
            show.ws.cue_list.remove(id);
        }
        show.edited();
        let rows = show.ws.cue_list.visible_rows(&|id| self.collapsed.contains(&id));
        let next = first_row.and_then(|i| rows.get(i.min(rows.len().saturating_sub(1)))).map(|r| r.1);
        self.select(show, next);
    }

    fn duplicate_selected(&mut self, show: &mut Show) {
        let mut last = None;
        for id in self.selection.clone() {
            if let Some(c) = show.ws.cue_list.find(id) {
                let mut d = c.duplicate();
                d.number = show.ws.cue_list.next_number();
                last = Some(d.id);
                show.ws.cue_list.insert_after(Some(id), d);
            }
        }
        show.edited();
        if last.is_some() {
            self.select(show, last);
        }
    }

    /// Wrap the selected cues (those sharing the primary's parent) in a new group.
    fn group_selected(&mut self, show: &mut Show) {
        let Some(primary) = self.primary() else { return };
        let list = &show.ws.cue_list;
        let Some(ploc) = list.location(primary) else { return };
        let mut members: Vec<(usize, CueId)> = self
            .selection
            .iter()
            .filter_map(|id| list.location(*id).filter(|l| l.parent == ploc.parent).map(|l| (l.index, *id)))
            .collect();
        members.sort();
        let at = members[0].0;
        let mut group = Cue::new(CueKind::Group { mode: GroupMode::StartAll, children: Vec::new() });
        group.number = String::new();
        group.name = "Group".into();
        let gid = group.id;
        let children: Vec<Cue> = members.iter().filter_map(|(_, id)| show.ws.cue_list.remove(*id)).collect();
        if let Some(ch) = group.children_mut() {
            *ch = children;
        }
        show.ws.cue_list.insert(ploc.parent, at, group);
        show.edited();
        self.select(show, Some(gid));
    }

    fn renumber_all(&mut self, show: &mut Show) {
        fn rec(list: &mut [Cue], n: &mut u32) {
            for c in list {
                if !c.is_group() || !c.number.is_empty() {
                    *n += 1;
                    c.number = n.to_string();
                }
                if let Some(ch) = c.children_mut() {
                    rec(ch, n);
                }
            }
        }
        let mut n = 0;
        rec(&mut show.ws.cue_list.cues, &mut n);
        show.edited();
    }

    // ------------------------------------------------------------------ input

    fn handle_keys(&mut self, ctx: &egui::Context, show: &mut Show) {
        let typing = ctx.egui_wants_keyboard_input();
        let now = Instant::now();
        let sc = |m: Modifiers, k: Key| KeyboardShortcut::new(m, k);
        let cmd = Modifiers::COMMAND;

        // Panic always works, even while typing.
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            let fade = show.ws.settings.panic_fade;
            let Show { engine, media, .. } = show;
            engine.stop_all(media, now, fade);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::S))) {
            self.deferred.push(Deferred::Save);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd | Modifiers::SHIFT, Key::S))) {
            self.deferred.push(Deferred::SaveAs);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::O))) {
            self.deferred.push(Deferred::Open);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::N))) {
            self.deferred.push(Deferred::New);
        }
        if typing {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Space)) {
            show.go();
        }
        let keys = [
            (Key::Num1, KindTag::Audio),
            (Key::Num2, KindTag::Video),
            (Key::Num3, KindTag::Group),
            (Key::Num4, KindTag::Wait),
            (Key::Num5, KindTag::Memo),
            (Key::Num6, KindTag::Fade),
            (Key::Num7, KindTag::Start),
            (Key::Num8, KindTag::Stop),
            (Key::Num9, KindTag::Pause),
            (Key::Num0, KindTag::Goto),
        ];
        for (k, tag) in keys {
            if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, k))) {
                self.add_cue(show, tag);
            }
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::D))) {
            self.duplicate_selected(show);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::G))) {
            self.group_selected(show);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::NONE, Key::Backspace)) {
            self.delete_selected(show);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::P)) {
            let Show { engine, media, .. } = show;
            engine.pause_all(media, now);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::R)) {
            let Show { engine, media, .. } = show;
            engine.resume_all(media, now);
        }
        let collapsed = self.collapsed.clone();
        let is_collapsed = |id: CueId| collapsed.contains(&id);
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
            let next = match self.primary() {
                Some(p) => show.ws.cue_list.next_in_display(p, &is_collapsed),
                None => show.ws.cue_list.first(),
            };
            if next.is_some() {
                self.select(show, next);
            }
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
            let prev = self.primary().and_then(|p| show.ws.cue_list.prev_in_display(p, &is_collapsed));
            if prev.is_some() {
                self.select(show, prev);
            }
        }
        if let Some(p) = self.primary()
            && show.ws.cue_list.find(p).is_some_and(|c| c.is_group())
        {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowLeft)) {
                self.collapsed.insert(p);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowRight)) {
                self.collapsed.remove(&p);
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context, show: &mut Show) {
        let files: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for f in files {
            self.add_media_cue(show, f);
        }
    }

    // ------------------------------------------------------------------ panels

    fn menu_bar(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New").clicked() {
                    self.deferred.push(Deferred::New);
                }
                if ui.button("Open…").clicked() {
                    self.deferred.push(Deferred::Open);
                }
                if ui.button("Save").clicked() {
                    self.deferred.push(Deferred::Save);
                }
                if ui.button("Save As…").clicked() {
                    self.deferred.push(Deferred::SaveAs);
                }
                ui.separator();
                if ui.button("Import media files…").clicked() {
                    self.deferred.push(Deferred::AddMediaCues);
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui.button("Duplicate  (Ctrl+D)").clicked() {
                    self.duplicate_selected(show);
                }
                if ui.button("Delete  (Del)").clicked() {
                    self.delete_selected(show);
                }
                if ui.button("Group selected  (Ctrl+G)").clicked() {
                    self.group_selected(show);
                }
                ui.separator();
                if ui.button("Renumber all cues").clicked() {
                    self.renumber_all(show);
                }
                if ui.button("Settings…").clicked() {
                    self.show_settings = true;
                }
            });
            ui.menu_button("Cue", |ui| {
                for (i, tag) in KindTag::ALL.iter().enumerate() {
                    let key = (i + 1) % 10;
                    if ui.button(format!("{} New {} cue  (Ctrl+{key})", tag.icon(), tag.label())).clicked() {
                        self.add_cue(show, *tag);
                    }
                }
            });
            ui.menu_button("Show", |ui| {
                let now = Instant::now();
                if ui.button("GO  (Space)").clicked() {
                    show.go();
                }
                if ui.button("Stop all  (Esc)").clicked() {
                    let fade = show.ws.settings.panic_fade;
                    let Show { engine, media, .. } = show;
                    engine.stop_all(media, now, fade);
                }
                if ui.button("Pause all  (P)").clicked() {
                    let Show { engine, media, .. } = show;
                    engine.pause_all(media, now);
                }
                if ui.button("Resume all  (R)").clicked() {
                    let Show { engine, media, .. } = show;
                    engine.resume_all(media, now);
                }
            });
            ui.menu_button("Window", |ui| {
                for st in &show.ws.stages {
                    let mut open = self.stage_open.contains(&st.id);
                    if ui.checkbox(&mut open, format!("Stage: {}", st.name)).changed() {
                        if open {
                            self.stage_open.insert(st.id);
                        } else {
                            self.stage_open.remove(&st.id);
                        }
                    }
                }
                if ui.button("Stages & mapping…").clicked() {
                    self.show_settings = true;
                    self.settings_tab = SettingsTab::Stages;
                    ui.close();
                }
                ui.checkbox(&mut self.show_log, "Engine log");
            });
        });
    }

    fn transport(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        let now = Instant::now();
        ui.horizontal(|ui| {
            let go = egui::Button::new(RichText::new("GO").size(34.0).strong().color(Color32::WHITE))
                .fill(Color32::from_rgb(30, 130, 50))
                .min_size(egui::vec2(130.0, 70.0));
            if ui.add_enabled(show.engine.playhead.is_some(), go).on_hover_text("Space").clicked() {
                show.go();
            }
            ui.add_space(8.0);

            ui.vertical(|ui| {
                ui.set_width((ui.available_width() - 330.0).max(200.0));
                match show.engine.playhead.and_then(|id| show.ws.cue_list.find(id)) {
                    Some(c) => {
                        ui.label(RichText::new("STANDBY").small().color(GREEN));
                        ui.label(RichText::new(format!("{}  {}", c.number, c.display_name())).size(22.0).strong());
                        if !c.notes.is_empty() {
                            ui.label(RichText::new(&c.notes).italics());
                        }
                    }
                    None => {
                        ui.label(RichText::new("STANDBY").small().color(AMBER));
                        ui.label(RichText::new("— end of cue list —").size(22.0).weak());
                    }
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.meter(ui);
                ui.vertical(|ui| {
                    let panicking = show.engine.is_panicking();
                    let stop = egui::Button::new(RichText::new(if panicking { "STOP NOW" } else { "Stop all" }).strong())
                        .fill(if panicking { RED } else { Color32::from_rgb(120, 40, 40) })
                        .min_size(egui::vec2(110.0, 30.0));
                    if ui.add(stop).on_hover_text("Esc — fades out; press again to stop instantly").clicked() {
                        let fade = show.ws.settings.panic_fade;
                        let Show { engine, media, .. } = show;
                        engine.stop_all(media, now, fade);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("⏸ Pause").on_hover_text("P").clicked() {
                            let Show { engine, media, .. } = show;
                            engine.pause_all(media, now);
                        }
                        if ui.button("▶ Resume").on_hover_text("R").clicked() {
                            let Show { engine, media, .. } = show;
                            engine.resume_all(media, now);
                        }
                    });
                });
            });
        });
    }

    fn meter(&mut self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0 * self.meters.len().max(2) as f32 + 4.0, 70.0), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
        for (i, m) in self.meters.iter().enumerate() {
            let db = crate::model::gain_to_db(*m);
            let frac = ((db - crate::model::MIN_DB) / -crate::model::MIN_DB).clamp(0.0, 1.0);
            let x = rect.left() + 2.0 + i as f32 * 10.0;
            let bar = egui::Rect::from_min_max(
                egui::pos2(x, rect.bottom() - 2.0 - (rect.height() - 4.0) * frac),
                egui::pos2(x + 8.0, rect.bottom() - 2.0),
            );
            let color = if db > -3.0 { RED } else if db > -12.0 { AMBER } else { GREEN };
            p.rect_filled(bar, 1.0, color);
        }
    }

    fn active_panel(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        let now = Instant::now();
        ui.heading("Active cues");
        ui.separator();
        let mut list: Vec<(Instant, CueId)> =
            show.engine.instances.iter().map(|(id, i)| (i.fired_at, *id)).collect();
        list.sort();
        if list.is_empty() {
            ui.label(RichText::new("Nothing running").weak());
        }
        egui::ScrollArea::vertical().id_salt("active").max_height(ui.available_height() - if self.show_log { 160.0 } else { 0.0 }).show(ui, |ui| {
            for (_, id) in list {
                let Some(cue) = show.ws.cue_list.find(id) else { continue };
                let Some(inst) = show.engine.instances.get(&id) else { continue };
                let paused = inst.is_paused();
                let (label, frac) = if inst.in_pre_wait(now) {
                    let e = inst.pre_wait_elapsed(now);
                    (format!("pre-wait {}", fmt_time(inst.pre_wait - e)), (e / inst.pre_wait.max(1e-6)) as f32)
                } else {
                    let e = match cue.kind {
                        CueKind::Audio(_) | CueKind::Video(_) => show.media.elapsed(id).unwrap_or(inst.action_elapsed(now)),
                        _ => inst.action_elapsed(now),
                    };
                    match inst.action_duration {
                        Some(d) if d > 0.0 => (format!("{} / -{}", fmt_time(e), fmt_time((d - e).max(0.0))), (e / d) as f32),
                        _ => (fmt_time(e), 1.0),
                    }
                };
                ui.horizontal(|ui| {
                    ui.label(cue.kind.tag().icon());
                    ui.label(RichText::new(format!("{} {}", cue.number, cue.display_name())).strong());
                });
                ui.horizontal(|ui| {
                    if ui.small_button(if paused { "▶" } else { "⏸" }).clicked() {
                        let Show { ws, engine, media, .. } = show;
                        if paused {
                            engine.resume_cue(ws, media, id, now);
                        } else {
                            engine.pause_cue(ws, media, id, now);
                        }
                    }
                    if ui.small_button("⏹").clicked() {
                        let Show { ws, engine, media, .. } = show;
                        engine.stop_cue(ws, media, id);
                    }
                    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width().max(40.0), 14.0), egui::Sense::hover());
                    widgets::paint_progress(ui, r, frac, if paused { AMBER } else { GREEN });
                    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, &label, egui::FontId::monospace(10.0), ui.visuals().strong_text_color());
                });
                ui.add_space(4.0);
            }
        });
        if self.show_log {
            ui.separator();
            ui.label(RichText::new("Engine log").strong());
            egui::ScrollArea::vertical().id_salt("log").stick_to_bottom(true).show(ui, |ui| {
                for l in &show.engine.log {
                    ui.label(RichText::new(l).small().color(AMBER));
                }
            });
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, show: &Show) {
        ui.horizontal(|ui| {
            let file = show.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "Untitled".into());
            ui.label(format!("{file}{}", if show.dirty { " •" } else { "" }));
            ui.separator();
            match (&show.media.audio, &show.media.audio_error) {
                (Some(a), _) => ui.label(format!("🔊 {} · {} Hz · {} ch", a.device_name, a.rate, a.channels)),
                (None, e) => ui.label(RichText::new(format!("🔇 audio: {}", e.as_deref().unwrap_or("off"))).color(RED)),
            };
            ui.separator();
            match &show.media.video_error {
                None => ui.label("🎬 video ready"),
                Some(e) => ui.label(RichText::new(format!("video: {e}")).color(AMBER)),
            };
            ui.separator();
            ui.label(format!("{} cues", show.ws.cue_list.iter_all().len()));
            if let Some((msg, t)) = &self.status {
                if t.elapsed() < Duration::from_secs(6) {
                    ui.separator();
                    ui.label(RichText::new(msg).color(AMBER));
                } else {
                    self.status = None;
                }
            }
            if let Some(l) = show.engine.log.last() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.link(RichText::new(format!("⚠ {l}")).color(AMBER).small()).clicked() {
                        self.show_log = true;
                    }
                });
            }
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        let mut open = self.show_settings;
        egui::Window::new("Settings").open(&mut open).default_width(560.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.settings_tab, SettingsTab::General, "General");
                ui.selectable_value(&mut self.settings_tab, SettingsTab::Audio, "Audio");
                ui.selectable_value(&mut self.settings_tab, SettingsTab::Stages, "Video stages");
            });
            ui.separator();
            match self.settings_tab {
                SettingsTab::General => self.settings_general(ui, show),
                SettingsTab::Audio => self.settings_audio(ui, show),
                SettingsTab::Stages => self.settings_stages(ui, show),
            }
        });
        self.show_settings = open;
    }

    fn settings_general(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        let before = show.ws.settings.clone();
        egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
            ui.label("Stop-all fade time");
            widgets::time_edit(ui, &mut show.ws.settings.panic_fade);
            ui.end_row();
            ui.label("Double-GO protection");
            widgets::time_edit(ui, &mut show.ws.settings.double_go_protection);
            ui.end_row();
            ui.label("Master volume");
            widgets::db_edit(ui, &mut show.ws.settings.master_volume_db);
            ui.end_row();
            ui.label("Video decoding");
            ui.checkbox(&mut show.ws.settings.hardware_video_decoding, "Use GPU decoders")
                .on_hover_text("Faster for big files, but some drivers (e.g. NVIDIA NVDEC) can hang. Applies to cues loaded from now on.");
            ui.end_row();
        });
        if show.ws.settings.hardware_video_decoding != before.hardware_video_decoding {
            show.media.set_hardware_video_decoding(show.ws.settings.hardware_video_decoding);
        }
        if show.ws.settings != before {
            if let Some(a) = &mut show.media.audio {
                a.set_master(show.ws.settings.master_volume_db);
            }
            show.edited();
        }
    }

    fn settings_audio(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        if self.devices.is_none() {
            self.devices = Some(media::audio::list_output_devices());
        }
        let devices = self.devices.clone().unwrap_or_default();
        let current = show.ws.settings.audio_device.clone();
        let label = |id: &Option<String>| match id {
            None => "System default".to_string(),
            Some(id) => devices.iter().find(|d| &d.id == id).map(|d| d.name.clone()).unwrap_or_else(|| format!("{id} (missing)")),
        };
        let mut chosen = current.clone();
        egui::Grid::new("audio_settings").num_columns(2).show(ui, |ui| {
            ui.label("Output device");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("device").width(320.0).selected_text(label(&chosen)).show_ui(ui, |ui| {
                    ui.selectable_value(&mut chosen, None, "System default");
                    for d in &devices {
                        ui.selectable_value(&mut chosen, Some(d.id.clone()), &d.name).on_hover_text(&d.id);
                    }
                });
                if ui.button("⟳").on_hover_text("Rescan devices").clicked() {
                    self.devices = None;
                }
            });
            ui.end_row();
            ui.label("Active");
            match &show.media.audio {
                Some(a) => ui.label(format!("{} · {} Hz · {} outputs", a.device_name, a.rate, a.channels)),
                None => ui.label(RichText::new(show.media.audio_error.clone().unwrap_or_default()).color(RED)),
            };
            ui.end_row();
        });
        ui.label(RichText::new("Changing the device stops all playback. Cue routing matrices address the device's outputs.").weak());
        if chosen != current {
            show.ws.settings.audio_device = chosen;
            let now = Instant::now();
            let Show { engine, media, ws, .. } = show;
            engine.stop_all(media, now, 0.0);
            media.reopen_audio(ws.settings.audio_device.as_deref(), ws.settings.master_volume_db);
            engine.workspace_changed();
            show.edited();
            if let Some(e) = &show.media.audio_error {
                self.notify(format!("Audio device failed: {e}"));
            }
        }
    }

    fn settings_stages(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        let before = show.ws.stages.clone();
        let mut remove = None;
        let n = show.ws.stages.len();
        for (i, st) in show.ws.stages.iter_mut().enumerate() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut st.name).desired_width(140.0));
                    ui.add(egui::DragValue::new(&mut st.width).range(16..=16384).suffix(" px"));
                    ui.label("×");
                    ui.add(egui::DragValue::new(&mut st.height).range(16..=16384).suffix(" px"));
                    if n > 1 && ui.small_button("🗑").on_hover_text("Delete stage").clicked() {
                        remove = Some(i);
                    }
                });
                ui.horizontal(|ui| {
                    let mut open = self.stage_open.contains(&st.id);
                    if ui.toggle_value(&mut open, "🖵 Window").changed() {
                        if open {
                            self.stage_open.insert(st.id);
                        } else {
                            self.stage_open.remove(&st.id);
                        }
                    }
                    if ui.button("⛶ Fullscreen").on_hover_text("Also: double-click or F11 in the stage window").clicked() {
                        self.stage_open.insert(st.id);
                        let fs = !self.stage_fullscreen.remove(&st.id);
                        if fs {
                            self.stage_fullscreen.insert(st.id);
                        }
                        ui.ctx().send_viewport_cmd_to(stage::viewport_id(st.id), egui::ViewportCommand::Fullscreen(fs));
                    }
                    let mut editing = self.mapping_edit.contains(&st.id);
                    if ui.toggle_value(&mut editing, "✥ Edit mapping").on_hover_text("Shows a test pattern and corner pins in the stage window").changed() {
                        if editing {
                            self.mapping_edit.insert(st.id);
                            self.stage_open.insert(st.id);
                        } else {
                            self.mapping_edit.remove(&st.id);
                        }
                    }
                    if ui.button("Reset mapping").clicked() {
                        st.corners = Stage::IDENTITY;
                    }
                });
                if st.corners != Stage::IDENTITY {
                    ui.label(
                        RichText::new(format!(
                            "corners: {}",
                            st.corners.iter().map(|c| format!("({:.3}, {:.3})", c[0], c[1])).collect::<Vec<_>>().join("  ")
                        ))
                        .weak()
                        .small(),
                    );
                }
            });
        }
        if let Some(i) = remove {
            let id = show.ws.stages.remove(i).id;
            self.stage_open.remove(&id);
            self.mapping_edit.remove(&id);
        }
        if ui.button("➕ Add stage").clicked() {
            let st = Stage::new(&format!("Stage {}", show.ws.stages.len() + 1));
            self.stage_open.insert(st.id);
            show.ws.stages.push(st);
        }
        if show.ws.stages != before {
            show.edited();
        }
    }

    // ------------------------------------------------------------------ files

    fn confirm_discard(&self) -> bool {
        if !self.show.lock().dirty {
            return true;
        }
        matches!(
            rfd::MessageDialog::new()
                .set_title("Unsaved changes")
                .set_description("Discard unsaved changes to this workspace?")
                .set_buttons(rfd::MessageButtons::YesNo)
                .show(),
            rfd::MessageDialogResult::Yes
        )
    }

    fn save_to(&mut self, path: Option<PathBuf>) {
        let path = match path.or_else(|| self.show.lock().path.clone()) {
            Some(p) => p,
            None => match rfd::FileDialog::new()
                .add_filter("OpenQ workspace", &[Workspace::EXTENSION])
                .set_file_name("show.openq")
                .save_file()
            {
                Some(p) => p,
                None => return,
            },
        };
        let path = if path.extension().is_none() { path.with_extension(Workspace::EXTENSION) } else { path };
        let ws = self.show.lock().ws.clone();
        match ws.save(&path) {
            Ok(()) => {
                let mut s = self.show.lock();
                s.path = Some(path.clone());
                s.dirty = false;
                drop(s);
                self.notify(format!("Saved {}", path.display()));
            }
            Err(e) => self.notify(format!("Save failed: {e:#}")),
        }
    }

    pub fn open_path(&mut self, path: PathBuf) {
        match Workspace::load(&path) {
            Ok(ws) => {
                let mut s = self.show.lock();
                s.replace_workspace(ws, Some(path.clone()));
                self.selection.clear();
                self.collapsed.clear();
                self.last_playhead = None;
                drop(s);
                self.notify(format!("Opened {}", path.display()));
            }
            Err(e) => self.notify(format!("Open failed: {e:#}")),
        }
    }

    fn run_deferred(&mut self) {
        for d in std::mem::take(&mut self.deferred) {
            match d {
                Deferred::New => {
                    if self.confirm_discard() {
                        self.show.lock().replace_workspace(Workspace::default(), None);
                        self.selection.clear();
                    }
                }
                Deferred::Open => {
                    if self.confirm_discard()
                        && let Some(p) = rfd::FileDialog::new()
                            .add_filter("OpenQ workspace", &[Workspace::EXTENSION])
                            .pick_file()
                    {
                        self.open_path(p);
                    }
                }
                Deferred::Save => self.save_to(None),
                Deferred::SaveAs => {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("OpenQ workspace", &[Workspace::EXTENSION])
                        .set_file_name("show.openq")
                        .save_file()
                    {
                        self.save_to(Some(p));
                    }
                }
                Deferred::PickMedia(id) => {
                    let video = matches!(self.show.lock().ws.cue_list.find(id).map(|c| c.kind.tag()), Some(KindTag::Video));
                    let mut dlg = rfd::FileDialog::new();
                    dlg = if video {
                        dlg.add_filter("Video", &["mp4", "mov", "mkv", "avi", "webm", "m4v", "mpg", "mpeg", "ogv", "mxf"])
                    } else {
                        dlg.add_filter("Audio", &["wav", "aif", "aiff", "flac", "mp3", "ogg", "opus", "m4a", "aac", "caf"])
                    };
                    if let Some(p) = dlg.add_filter("All files", &["*"]).pick_file() {
                        let mut s = self.show.lock();
                        if let Some(c) = s.ws.cue_list.find_mut(id) {
                            match &mut c.kind {
                                CueKind::Audio(a) => a.path = p.clone(),
                                CueKind::Video(v) => v.path = p.clone(),
                                _ => {}
                            }
                        }
                        s.edited();
                        self.prober.get(&p);
                    }
                }
                Deferred::AddMediaCues => {
                    if let Some(files) = rfd::FileDialog::new()
                        .add_filter(
                            "Media",
                            &["wav", "aif", "aiff", "flac", "mp3", "ogg", "opus", "m4a", "aac", "caf", "mp4", "mov", "mkv", "avi", "webm", "m4v"],
                        )
                        .pick_files()
                    {
                        let show = self.show.clone();
                        let mut s = show.lock();
                        for f in files {
                            self.add_media_cue(&mut s, f);
                        }
                    }
                }
            }
        }
    }
}

impl App {
    /// Dev hook for automated visual checks: with `OPENQ_SCREENSHOT=out.ppm` the app presses GO
    /// `OPENQ_SCREENSHOT_GOS` times (default 0), captures its window a couple of frames later and exits.
    /// `OPENQ_SCREENSHOT_CUE` / `OPENQ_SCREENSHOT_TAB` pick the cue and inspector tab shown;
    /// `OPENQ_SCREENSHOT_STAGE=1` embeds stage windows in the main window for the capture
    /// (`OPENQ_SCREENSHOT_MAPPING=1` turns on the first stage's mapping test pattern).
    fn debug_screenshot(&mut self, ctx: &egui::Context, show: &mut Show) {
        let Some(path) = std::env::var_os("OPENQ_SCREENSHOT") else { return };
        self.frame_count += 1;
        if self.frame_count == 1 {
            // Optional: select cue N and an inspector tab (time|levels|eq|geometry|settings).
            if let Ok(n) = std::env::var("OPENQ_SCREENSHOT_CUE") {
                let id = show.ws.cue_list.iter_all().iter().find(|c| c.number == n).map(|c| c.id);
                self.select(show, id);
            }
            self.inspector_tab = match std::env::var("OPENQ_SCREENSHOT_TAB").as_deref() {
                Ok("time") => inspector::Tab::Time,
                Ok("levels") => inspector::Tab::Levels,
                Ok("eq") => inspector::Tab::Eq,
                Ok("geometry") => inspector::Tab::Geometry,
                Ok("settings") => inspector::Tab::Settings,
                _ => inspector::Tab::Basics,
            };
            if std::env::var_os("OPENQ_SCREENSHOT_MAPPING").is_some()
                && let Some(st) = show.ws.stages.first()
            {
                self.mapping_edit.insert(st.id);
                self.stage_open.insert(st.id);
            }
            if let Ok(t) = std::env::var("OPENQ_SCREENSHOT_SETTINGS") {
                self.show_settings = true;
                self.settings_tab = if t == "audio" { SettingsTab::Audio } else { SettingsTab::Stages };
            }
        }
        ctx.request_repaint();
        let gos: u64 = std::env::var("OPENQ_SCREENSHOT_GOS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        // GOs 0.4 s apart (past double-GO protection), then shoot 2 s after the last one.
        let t0 = *self.debug_t0.get_or_insert_with(Instant::now);
        let elapsed = t0.elapsed().as_secs_f64();
        let gos_due = ((elapsed / 0.4) as u64).min(gos);
        while self.debug_gos < gos_due {
            show.go();
            self.debug_gos += 1;
        }
        if std::env::var_os("OPENQ_SCREENSHOT_STAGE").is_some() {
            // Render stage windows embedded in the main window so one capture shows them.
            ctx.set_embed_viewports(true);
        }
        if self.frame_count > 2 && elapsed > gos as f64 * 0.4 + 2.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let mut out = format!("P6 {} {} 255\n", img.size[0], img.size[1]).into_bytes();
            for p in &img.pixels {
                out.extend_from_slice(&[p.r(), p.g(), p.b()]);
            }
            let _ = std::fs::write(&path, out);
            show.shutdown();
            std::process::exit(0);
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        self.show.lock().shutdown();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let show_arc = self.show.clone();
        {
            let mut guard = show_arc.lock();

            let show: &mut Show = &mut guard;
            self.sync_playhead(show);
            self.handle_keys(&ctx, show);
            self.handle_dropped_files(&ctx, show);
            self.debug_screenshot(&ctx, show);

            let fresh = show.media.take_meters();
            if self.meters.len() != fresh.len() {
                self.meters = fresh.clone();
            }
            for (m, f) in self.meters.iter_mut().zip(fresh) {
                *m = f.max(*m * 0.85);
            }

            egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui, show));
            egui::Panel::top("transport").show(ui, |ui| {
                ui.add_space(6.0);
                self.transport(ui, show);
                ui.add_space(6.0);
            });
            egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui, show));
            egui::Panel::bottom("inspector")
                .resizable(true)
                .default_size(340.0)
                .min_size(120.0)
                .show(ui, |ui| self.inspector(ui, show));
            egui::Panel::right("active")
                .resizable(true)
                .default_size(260.0)
                .show(ui, |ui| self.active_panel(ui, show));
            egui::CentralPanel::default().show(ui, |ui| {
                self.toolbar(ui, show);
                ui.separator();
                self.cue_list(ui, show);
            });

            if self.show_settings {
                self.settings_window(&ctx, show);
            }

            #[cfg(feature = "video")]
            let video_active = self.stages(&ctx, show);
            #[cfg(not(feature = "video"))]
            let video_active = false;

            let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
            if hovering {
                let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop")));
                let rect = ctx.content_rect();
                painter.rect_filled(rect, 0.0, Color32::from_black_alpha(160));
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Drop audio / video files to create cues", egui::FontId::proportional(26.0), Color32::WHITE);
            }

            let name = show.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Untitled".into());
            let title = format!("{name}{} — OpenQ", if show.dirty { " •" } else { "" });
            if title != self.title {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
                self.title = title;
            }

            if show.engine.is_active() {
                ctx.request_repaint_after(if video_active { Duration::from_millis(16) } else { Duration::from_millis(33) });
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }

            if ctx.input(|i| i.viewport().close_requested()) && show.dirty && !self.allow_close {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                drop(guard);
                if self.confirm_discard() {
                    self.allow_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                return;
            }
        }
        self.run_deferred();
    }
}
