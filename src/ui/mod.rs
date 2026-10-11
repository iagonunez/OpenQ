//! egui front end.

mod about;
mod cue_list;
mod eq;
mod icons;
mod inspector;
mod outputs;
#[cfg(feature = "video")]
mod preview;
mod remote;
#[cfg(feature = "video")]
mod stage;
mod theme;
mod waveform;
mod widgets;

use crate::media::audio::DeviceEntry;
use crate::media::{self, Prober, Waveforms};
use crate::model::{
    AudioCue, Cue, CueId, CueKind, GroupMode, KindTag, Stage, StageId, VideoCue, Workspace,
};
use crate::control::{Command, CueAction};
use crate::show::{SharedShow, Show};
use egui::{Color32, Key, KeyboardShortcut, Modifiers, RichText};
use parking_lot::Mutex;
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

/// Edit mode allows changing the show; show mode locks it down for running a performance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Edit,
    Show,
}

/// What undo/redo restores.
#[derive(Clone, PartialEq)]
struct UndoState {
    cues: crate::model::CueList,
    stages: Vec<Stage>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    General,
    Audio,
    Stages,
}

/// The egui application: owns all UI state and draws the main window, stage windows and dialogs.
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
    /// Displays a stage can be sent to, and the monitor index last applied to each stage window.
    displays: Arc<Mutex<outputs::DisplayList>>,
    #[cfg(feature = "video")]
    stage_monitor: std::collections::HashMap<StageId, Option<usize>>,
    show_preview: bool,
    show_about: bool,
    remote: remote::RemoteUi,
    #[cfg(feature = "video")]
    posters: Arc<media::Posters>,
    #[cfg(feature = "video")]
    preview_tex: Option<(String, egui::TextureHandle, [f32; 2])>,
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
    mode: Mode,
    inline: Option<cue_list::InlineEdit>,
    undo: Vec<UndoState>,
    redo: Vec<UndoState>,
    undo_base: Option<UndoState>,
    undo_seen_gen: u64,
    undo_pending: Option<Instant>,
    osc: Option<crate::osc::OscServer>,
    osc_error: Option<String>,
    osc_failed_for: Option<u16>,
    debug_t0: Option<Instant>,
    debug_gos: u64,
}

impl App {
    /// Create the UI for a shared show; `prober` is the media-info cache shared with the engine.
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
            displays: Default::default(),
            #[cfg(feature = "video")]
            stage_monitor: Default::default(),
            show_preview: false,
            show_about: false,
            remote: Default::default(),
            #[cfg(feature = "video")]
            posters: media::Posters::new(),
            #[cfg(feature = "video")]
            preview_tex: None,
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
            mode: Mode::Edit,
            inline: None,
            undo: Vec::new(),
            redo: Vec::new(),
            undo_base: None,
            undo_seen_gen: 0,
            undo_pending: None,
            osc: None,
            osc_error: None,
            osc_failed_for: None,
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
        self.selection
            .retain(|id| show.ws.cue_list.find(*id).is_some());
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
            CueKind::Video(VideoCue {
                path: path.clone(),
                ..Default::default()
            })
        } else if media::is_audio_path(&path) {
            CueKind::Audio(AudioCue {
                path: path.clone(),
                ..Default::default()
            })
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
        let rows = show
            .ws
            .cue_list
            .visible_rows(&|id| self.collapsed.contains(&id));
        let first_row = rows.iter().position(|(_, id)| self.selection.contains(id));
        for id in self.selection.clone() {
            let Show {
                ws, engine, media, ..
            } = show;
            engine.stop_cue(ws, media, id);
            show.ws.cue_list.remove(id);
        }
        show.edited();
        let rows = show
            .ws
            .cue_list
            .visible_rows(&|id| self.collapsed.contains(&id));
        let next = first_row
            .and_then(|i| rows.get(i.min(rows.len().saturating_sub(1))))
            .map(|r| r.1);
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
        let Some(primary) = self.primary() else {
            return;
        };
        let list = &show.ws.cue_list;
        let Some(ploc) = list.location(primary) else {
            return;
        };
        let mut members: Vec<(usize, CueId)> = self
            .selection
            .iter()
            .filter_map(|id| {
                list.location(*id)
                    .filter(|l| l.parent == ploc.parent)
                    .map(|l| (l.index, *id))
            })
            .collect();
        members.sort();
        let at = members[0].0;
        let mut group = Cue::new(CueKind::Group {
            mode: GroupMode::StartAll,
            children: Vec::new(),
        });
        group.number = String::new();
        group.name = "Group".into();
        let gid = group.id;
        let children: Vec<Cue> = members
            .iter()
            .filter_map(|(_, id)| show.ws.cue_list.remove(*id))
            .collect();
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
        let sc = |m: Modifiers, k: Key| KeyboardShortcut::new(m, k);
        let cmd = Modifiers::COMMAND;
        let editing = self.mode == Mode::Edit;

        // Panic always works, even while typing (Esc only cancels an in-place edit).
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
            && self.inline.take().is_none()
        {
            self.control(show, Command::Stop);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::E))) {
            self.toggle_mode();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::S))) {
            self.deferred.push(Deferred::Save);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd | Modifiers::SHIFT, Key::S))) {
            self.deferred.push(Deferred::SaveAs);
        }
        if editing && ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::O))) {
            self.deferred.push(Deferred::Open);
        }
        if editing && ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::N))) {
            self.deferred.push(Deferred::New);
        }
        if typing {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Space)) {
            self.control(show, Command::Go);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::P)) {
            self.control(show, Command::PauseAll);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::R)) {
            self.control(show, Command::ResumeAll);
        }

        // Per-cue hotkeys (plain key presses, no modifiers).
        let hotkeys: Vec<(Key, CueId)> = show
            .ws
            .cue_list
            .iter_all()
            .iter()
            .filter_map(|c| Some((Key::from_name(c.hotkey.as_deref()?)?, c.id)))
            .collect();
        for (k, id) in hotkeys {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k)) {
                self.control(show, Command::Cue(id, CueAction::Start));
            }
        }

        if editing {
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
            if ctx.input_mut(|i| {
                i.consume_shortcut(&sc(cmd | Modifiers::SHIFT, Key::Z))
                    || i.consume_shortcut(&sc(cmd, Key::Y))
            }) {
                self.redo(show);
            }
            if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::Z))) {
                self.undo(show);
            }
            if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::D))) {
                self.duplicate_selected(show);
            }
            if ctx.input_mut(|i| i.consume_shortcut(&sc(cmd, Key::G))) {
                self.group_selected(show);
            }
            if ctx.input_mut(|i| {
                i.consume_key(Modifiers::NONE, Key::Delete)
                    || i.consume_key(Modifiers::NONE, Key::Backspace)
            }) {
                self.delete_selected(show);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                // Enter edits the selected cue's name in place.
                if let Some(c) = self.primary().and_then(|p| show.ws.cue_list.find(p)) {
                    self.inline = Some(cue_list::InlineEdit::new(
                        c.id,
                        cue_list::InlineField::Name,
                        c.display_name(),
                    ));
                }
            }
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
            let prev = self
                .primary()
                .and_then(|p| show.ws.cue_list.prev_in_display(p, &is_collapsed));
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

    fn toggle_mode(&mut self) {
        if self.mode == Mode::Show && !self.can_edit_show() {
            self.notify("This device can only run the show: editing needs the Control & edit role");
            return;
        }
        self.mode = match self.mode {
            Mode::Edit => Mode::Show,
            Mode::Show => Mode::Edit,
        };
        self.inline = None;
        self.drag = None;
    }

    // ------------------------------------------------------------------ undo

    /// Record undo points: an edit becomes one step once things settle (no button held,
    /// 400 ms since the last change), so a slider drag or typing burst undoes in one go.
    fn track_undo(&mut self, ctx: &egui::Context, show: &Show) {
        let snap = || UndoState {
            cues: show.ws.cue_list.clone(),
            stages: show.ws.stages.clone(),
        };
        if self.undo_base.is_none() {
            self.undo_base = Some(snap());
            self.undo_seen_gen = show.edit_gen;
            return;
        }
        if show.edit_gen != self.undo_seen_gen {
            self.undo_seen_gen = show.edit_gen;
            self.undo_pending = Some(Instant::now());
        }
        let settled = self
            .undo_pending
            .is_some_and(|t| t.elapsed() > Duration::from_millis(400))
            && !ctx.input(|i| i.pointer.any_down());
        if settled {
            self.commit_undo(show);
        } else if self.undo_pending.is_some() {
            ctx.request_repaint_after(Duration::from_millis(450));
        }
    }

    fn commit_undo(&mut self, show: &Show) {
        self.undo_pending = None;
        let now = UndoState {
            cues: show.ws.cue_list.clone(),
            stages: show.ws.stages.clone(),
        };
        if self.undo_base.as_ref() != Some(&now)
            && let Some(prev) = self.undo_base.replace(now)
        {
            self.undo.push(prev);
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
    }

    fn apply_undo_state(&mut self, show: &mut Show, st: UndoState) {
        show.ws.cue_list = st.cues.clone();
        show.ws.stages = st.stages.clone();
        show.dirty = true;
        show.engine.workspace_changed();
        self.undo_base = Some(st);
        self.undo_seen_gen = show.edit_gen;
        self.inline = None;
    }

    fn undo(&mut self, show: &mut Show) {
        self.commit_undo(show);
        if let Some(prev) = self.undo.pop() {
            self.redo.push(UndoState {
                cues: show.ws.cue_list.clone(),
                stages: show.ws.stages.clone(),
            });
            self.apply_undo_state(show, prev);
            self.notify("Undo");
        }
    }

    fn redo(&mut self, show: &mut Show) {
        self.commit_undo(show);
        if let Some(next) = self.redo.pop() {
            self.undo.push(UndoState {
                cues: show.ws.cue_list.clone(),
                stages: show.ws.stages.clone(),
            });
            self.apply_undo_state(show, next);
            self.notify("Redo");
        }
    }

    fn reset_undo(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.undo_base = None;
        self.undo_pending = None;
    }

    /// Start/stop/restart the OSC listener to match the settings.
    fn sync_osc(&mut self, ctx: &egui::Context, show: &Show) {
        // A replica's OSC would drive the main device's settings here: leave it to the main one.
        let want = (show.ws.settings.osc_enabled && !show.replica).then_some(show.ws.settings.osc_port);
        if self.osc.as_ref().map(|o| o.port) == want && self.osc_error.is_none() {
            return;
        }
        if want == self.osc_failed_for {
            return;
        }
        self.osc = None;
        self.osc_error = None;
        self.osc_failed_for = None;
        if let Some(port) = want {
            let c = ctx.clone();
            match crate::osc::OscServer::start(port, self.show.clone(), move || c.request_repaint())
            {
                Ok(s) => self.osc = Some(s),
                Err(e) => {
                    self.osc_error = Some(format!("OSC port {port}: {e}"));
                    self.osc_failed_for = want;
                }
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context, show: &mut Show) {
        let files: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if self.mode == Mode::Show && !files.is_empty() {
            self.notify("Switch to edit mode (Ctrl+E) to add cues");
            return;
        }
        if self.remote.is_replica() && !files.is_empty() {
            self.notify("Media files live on the main device: add them there");
            return;
        }
        for f in files {
            self.add_media_cue(show, f);
        }
    }

    // ------------------------------------------------------------------ panels

    fn menu_bar(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        use icons as I;
        use theme::with_icon as wi;
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button(wi(I::NOTE_ADD, "New")).clicked() {
                    self.deferred.push(Deferred::New);
                }
                if ui.button(wi(I::FOLDER_OPEN, "Open…")).clicked() {
                    self.deferred.push(Deferred::Open);
                }
                if ui.button(wi(I::SAVE, "Save")).clicked() {
                    self.deferred.push(Deferred::Save);
                }
                if ui.button(wi(I::SAVE, "Save As…")).clicked() {
                    self.deferred.push(Deferred::SaveAs);
                }
                ui.separator();
                if ui.button(wi(I::ADD, "Import media files…")).clicked() {
                    self.deferred.push(Deferred::AddMediaCues);
                }
                ui.separator();
                if ui.button(wi(I::CLOSE, "Quit")).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if self.mode == Mode::Show {
                    ui.disable();
                }
                let can_undo = !self.undo.is_empty() || self.undo_pending.is_some();
                if ui
                    .add_enabled(
                        can_undo,
                        egui::Button::new(wi(I::UNDO, "Undo")).shortcut_text("Ctrl+Z"),
                    )
                    .clicked()
                {
                    self.undo(show);
                }
                if ui
                    .add_enabled(
                        !self.redo.is_empty(),
                        egui::Button::new(wi(I::REDO, "Redo")).shortcut_text("Ctrl+Shift+Z"),
                    )
                    .clicked()
                {
                    self.redo(show);
                }
                ui.separator();
                if ui
                    .add(
                        egui::Button::new(wi(I::CONTENT_COPY, "Duplicate")).shortcut_text("Ctrl+D"),
                    )
                    .clicked()
                {
                    self.duplicate_selected(show);
                }
                if ui
                    .add(egui::Button::new(wi(I::DELETE, "Delete")).shortcut_text("Del"))
                    .clicked()
                {
                    self.delete_selected(show);
                }
                if ui
                    .add(
                        egui::Button::new(wi(I::CREATE_NEW_FOLDER, "Group selected"))
                            .shortcut_text("Ctrl+G"),
                    )
                    .clicked()
                {
                    self.group_selected(show);
                }
                ui.separator();
                if ui.button(wi(I::REFRESH, "Renumber all cues")).clicked() {
                    self.renumber_all(show);
                }
                if ui.button(wi(I::SETTINGS, "Settings…")).clicked() {
                    self.show_settings = true;
                }
            });
            ui.menu_button("Cue", |ui| {
                if self.mode == Mode::Show {
                    ui.disable();
                }
                for (i, tag) in KindTag::ALL.iter().enumerate() {
                    let mut b = egui::Button::new(wi(
                        theme::kind_icon(*tag),
                        &format!("New {} cue", tag.label()),
                    ));
                    if i < 10 {
                        b = b.shortcut_text(format!("Ctrl+{}", (i + 1) % 10));
                    }
                    if ui.add(b).clicked() {
                        self.add_cue(show, *tag);
                    }
                }
            });
            ui.menu_button("Show", |ui| {
                if ui
                    .add(egui::Button::new(wi(I::PLAY_ARROW, "GO")).shortcut_text("Space"))
                    .clicked()
                {
                    self.control(show, Command::Go);
                }
                if ui
                    .add(egui::Button::new(wi(I::STOP, "Stop all")).shortcut_text("Esc"))
                    .clicked()
                {
                    self.control(show, Command::Stop);
                }
                if ui
                    .add(egui::Button::new(wi(I::PAUSE, "Pause all")).shortcut_text("P"))
                    .clicked()
                {
                    self.control(show, Command::PauseAll);
                }
                if ui
                    .add(egui::Button::new(wi(I::PLAY_ARROW, "Resume all")).shortcut_text("R"))
                    .clicked()
                {
                    self.control(show, Command::ResumeAll);
                }
                ui.separator();
                let label = if self.mode == Mode::Edit {
                    wi(I::THEATER_COMEDY, "Enter show mode")
                } else {
                    wi(I::EDIT, "Back to edit mode")
                };
                if ui
                    .add(egui::Button::new(label).shortcut_text("Ctrl+E"))
                    .clicked()
                {
                    self.toggle_mode();
                }
            });
            ui.menu_button("Video", |ui| {
                let mut edited = false;
                for st in show.ws.stages.iter_mut() {
                    let open = self.stage_open.contains(&st.id);
                    let glyph = if open { I::DESKTOP_WINDOWS } else { I::MONITOR };
                    ui.menu_button(wi(glyph, &format!("Stage: {}", st.name)), |ui| {
                        let mut open = open;
                        if ui.checkbox(&mut open, "Show stage window").changed() {
                            if open {
                                self.stage_open.insert(st.id);
                            } else {
                                self.stage_open.remove(&st.id);
                            }
                        }
                        let mut editing = self.mapping_edit.contains(&st.id);
                        if ui.checkbox(&mut editing, "Edit mapping").changed() {
                            if editing {
                                self.mapping_edit.insert(st.id);
                                self.stage_open.insert(st.id);
                            } else {
                                self.mapping_edit.remove(&st.id);
                            }
                        }
                        ui.separator();
                        ui.label(theme::caption("Output"));
                        edited |= self.output_choices(ui, st);
                    });
                }
                if edited {
                    show.edited();
                }
                ui.separator();
                if ui.button(wi(I::CROP_FREE, "Stages & mapping…")).clicked() {
                    self.show_settings = true;
                    self.settings_tab = SettingsTab::Stages;
                    ui.close();
                }
            });
            ui.menu_button("Remote", |ui| {
                if ui.button(wi(I::LAN, "Share this OpenQ…")).clicked() {
                    self.remote.window = true;
                    self.remote.tab = remote::RemoteTab::Share;
                }
                if ui.button(wi(I::CABLE, "Connect to another OpenQ…")).clicked() {
                    self.remote.window = true;
                    self.remote.tab = remote::RemoteTab::Connect;
                }
                if self.remote.is_replica() && ui.button(wi(I::CLOSE, "Disconnect")).clicked() {
                    self.disconnect(show);
                }
            });
            ui.menu_button("View", |ui| {
                #[cfg(feature = "video")]
                ui.checkbox(&mut self.show_preview, wi(I::PREVIEW, "Geometry preview"));
                ui.checkbox(&mut self.show_log, wi(I::RECEIPT_LONG, "Engine log"));
            });
            ui.menu_button("Help", |ui| {
                if ui.button(wi(I::INFO, "About OpenQ and licences…")).clicked() {
                    self.show_about = true;
                }
            });
        });
    }

    /// The top of the workspace: GO, standby, transport, mode and meters.
    fn header(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        use egui::{Align2, CornerRadius, Margin, Sense, Stroke, vec2};
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            // GO.
            let enabled = show.engine.playhead.is_some();
            let (rect, resp) = ui.allocate_exact_size(vec2(150.0, 92.0), Sense::click());
            let fill = if !enabled {
                theme::BG_RAISED
            } else if resp.is_pointer_button_down_on() {
                Color32::from_rgb(0x22, 0x8f, 0x42)
            } else if resp.hovered() {
                Color32::from_rgb(0x33, 0xbd, 0x5a)
            } else {
                theme::GO
            };
            let p = ui.painter();
            p.rect_filled(rect, CornerRadius::same(14), fill);
            p.rect_stroke(
                rect,
                CornerRadius::same(14),
                Stroke::new(1.0, Color32::from_white_alpha(28)),
                egui::StrokeKind::Inside,
            );
            let text = if enabled {
                Color32::WHITE
            } else {
                theme::TEXT_FAINT
            };
            p.text(
                rect.center() - vec2(0.0, 6.0),
                Align2::CENTER_CENTER,
                "GO",
                theme::semibold(40.0),
                text,
            );
            p.text(
                rect.center_bottom() - vec2(0.0, 12.0),
                Align2::CENTER_CENTER,
                "SPACE",
                theme::semibold(9.5),
                text.gamma_multiply(0.7),
            );
            if resp
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
                && enabled
            {
                self.control(show, Command::Go);
            }

            // Standby card.
            let right_w = 250.0;
            let card_w = (ui.available_width() - right_w).max(240.0);
            egui::Frame::new()
                .fill(theme::BG_RAISED)
                .corner_radius(CornerRadius::same(12))
                .stroke(Stroke::new(1.0, theme::BORDER))
                .inner_margin(Margin::symmetric(16, 10))
                .show(ui, |ui| {
                    ui.set_width(card_w - 32.0);
                    ui.set_height(72.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.horizontal(|ui| {
                            ui.label(theme::caption("Standby"));
                            if self.mode == Mode::Show {
                                ui.label(
                                    RichText::new("SHOW MODE")
                                        .font(theme::semibold(10.0))
                                        .color(theme::RED),
                                );
                            }
                        });
                        match show
                            .engine
                            .playhead
                            .and_then(|id| show.ws.cue_list.find(id))
                        {
                            Some(c) => {
                                ui.horizontal(|ui| {
                                    if !c.number.is_empty() {
                                        egui::Frame::new()
                                            .fill(theme::accent_bg())
                                            .corner_radius(CornerRadius::same(6))
                                            .inner_margin(Margin::symmetric(8, 2))
                                            .show(ui, |ui| {
                                                ui.label(
                                                    RichText::new(&c.number)
                                                        .font(theme::semibold(20.0))
                                                        .color(Color32::WHITE),
                                                );
                                            });
                                    }
                                    ui.label(
                                        theme::icon(theme::kind_icon(c.kind.tag()), 22.0)
                                            .color(theme::TEXT_DIM),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(c.display_name())
                                                .font(theme::semibold(24.0))
                                                .color(theme::TEXT),
                                        )
                                        .truncate(),
                                    );
                                });
                                if !c.notes.is_empty() {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(c.notes.replace('\n', "  ·  "))
                                                .color(theme::TEXT_DIM),
                                        )
                                        .truncate(),
                                    );
                                }
                            }
                            None => {
                                ui.label(
                                    RichText::new("End of cue list")
                                        .font(theme::semibold(22.0))
                                        .color(theme::TEXT_FAINT),
                                );
                            }
                        }
                    });
                });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.meter(ui, show);
                ui.add_space(4.0);
                // Transport.
                let panicking = show.engine.is_panicking();
                if round_button(
                    ui,
                    icons::STOP,
                    if panicking {
                        theme::RED
                    } else {
                        Color32::from_rgb(0x8f, 0x2f, 0x33)
                    },
                    48.0,
                )
                .on_hover_text(if panicking {
                    "Stop now"
                } else {
                    "Stop all (Esc) — fades out; press again to stop instantly"
                })
                .clicked()
                {
                    self.control(show, Command::Stop);
                }
                if round_button(ui, icons::PLAY_ARROW, theme::BG_HOVER, 40.0)
                    .on_hover_text("Resume all (R)")
                    .clicked()
                {
                    self.control(show, Command::ResumeAll);
                }
                if round_button(ui, icons::PAUSE, theme::BG_HOVER, 40.0)
                    .on_hover_text("Pause all (P)")
                    .clicked()
                {
                    self.control(show, Command::PauseAll);
                }
            });
        });
    }

    /// Edit / Show segmented control: two halves side by side that fill `rect` exactly.
    fn mode_toggle(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        use egui::{CornerRadius, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(8), theme::BG_INPUT);
        p.rect_stroke(
            rect,
            CornerRadius::same(8),
            Stroke::new(1.0, theme::BORDER),
            StrokeKind::Inside,
        );
        let inner = rect.shrink(3.0);
        let gap = 3.0;
        let w = (inner.width() - gap) / 2.0;
        let halves = [
            (
                Mode::Edit,
                icons::EDIT,
                "Edit",
                theme::ACCENT,
                Rect::from_min_size(inner.min, vec2(w, inner.height())),
            ),
            (
                Mode::Show,
                icons::THEATER_COMEDY,
                "Show",
                theme::RED,
                Rect::from_min_size(
                    pos2(inner.left() + w + gap, inner.top()),
                    vec2(w, inner.height()),
                ),
            ),
        ];
        for (mode, glyph, label, color, r) in halves {
            let resp = ui
                .interact(r, ui.id().with(("mode", label)), Sense::click())
                .on_hover_text("Ctrl+E");
            let on = self.mode == mode;
            let fill = if on {
                color
            } else if resp.hovered() {
                theme::BG_HOVER
            } else {
                Color32::TRANSPARENT
            };
            let p = ui.painter();
            p.rect_filled(r, CornerRadius::same(6), fill);
            let fg = if on || resp.hovered() {
                Color32::WHITE
            } else {
                theme::TEXT_DIM
            };
            // Icon and label laid out as one centred group.
            let icon_g = p.layout_no_wrap(glyph.to_string(), theme::icon_font(16.0), fg);
            let text_g = p.layout_no_wrap(label.to_string(), theme::semibold(12.5), fg);
            let total = icon_g.size().x + 6.0 + text_g.size().x;
            let x0 = r.center().x - total / 2.0;
            p.galley(
                pos2(x0, r.center().y - icon_g.size().y / 2.0),
                icon_g.clone(),
                fg,
            );
            p.galley(
                pos2(
                    x0 + icon_g.size().x + 6.0,
                    r.center().y - text_g.size().y / 2.0,
                ),
                text_g,
                fg,
            );
            if resp
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
                && !on
            {
                self.toggle_mode();
            }
        }
    }

    /// Branding strip above the menu bar.
    fn title_bar(&mut self, ui: &mut egui::Ui, show: &Show) {
        use egui::text::{LayoutJob, TextFormat};
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
        let mut job = LayoutJob::default();
        job.append("Open", 0.0, TextFormat::simple(theme::title(26.0), theme::TEXT));
        job.append("Q", 0.0, TextFormat::simple(theme::title(26.0), theme::GREEN));
        let logo = ui.painter().layout_job(job);
        ui.painter().galley(egui::pos2(rect.left(), rect.center().y - logo.size().y / 2.0), logo, theme::TEXT);
        // Workspace name, centred in the window.
        let name = show
            .path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        let (name, dirty) = self.remote.title().unwrap_or((name, show.dirty));
        let label = format!("{name}{}", if dirty { "  •" } else { "" });
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(14.0), theme::TEXT_DIM);
        let toggle = egui::Rect::from_min_size(
            egui::pos2(rect.right() - 168.0, rect.center().y - 13.0),
            egui::vec2(168.0, 26.0),
        );
        self.mode_toggle(ui, toggle);
    }

    /// Output level meter with a dB scale (one bar per device channel, up to 8).
    fn meter(&mut self, ui: &mut egui::Ui, show: &Show) {
        use egui::{Align2, CornerRadius, Rect, Sense, pos2, vec2};
        let n = self.meters.len().clamp(1, 8);
        let bar = 7.0;
        let gap = 3.0;
        let scale_w = 20.0;
        let w = scale_w + n as f32 * (bar + gap) + 6.0;
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 92.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(8), theme::BG_INPUT);
        let inner = Rect::from_min_max(
            pos2(rect.left() + scale_w, rect.top() + 8.0),
            pos2(rect.right() - 6.0, rect.bottom() - 16.0),
        );
        let y_of = |db: f32| {
            inner.bottom()
                - ((db - crate::model::MIN_DB) / -crate::model::MIN_DB).clamp(0.0, 1.0)
                    * inner.height()
        };
        for db in [0.0, -12.0, -24.0, -48.0] {
            let y = y_of(db);
            p.text(
                pos2(rect.left() + scale_w - 4.0, y),
                Align2::RIGHT_CENTER,
                format!("{}", db as i32),
                theme::mono(8.5),
                theme::TEXT_FAINT,
            );
            p.line_segment(
                [pos2(inner.left(), y), pos2(inner.right(), y)],
                egui::Stroke::new(0.5, theme::BORDER),
            );
        }
        let audio_ok = show.media.audio.is_some();
        for i in 0..n {
            let x = inner.left() + i as f32 * (bar + gap);
            let col = Rect::from_min_max(pos2(x, inner.top()), pos2(x + bar, inner.bottom()));
            p.rect_filled(
                col,
                CornerRadius::same(2),
                Color32::from_rgb(0x1f, 0x21, 0x26),
            );
            let level = self.meters.get(i).copied().unwrap_or(0.0);
            let db = crate::model::gain_to_db(level);
            if db > crate::model::MIN_DB {
                // Green up to -12 dB, amber to -3 dB, red above.
                for (lo, hi, c) in [
                    (crate::model::MIN_DB, -12.0, theme::GREEN),
                    (-12.0, -3.0, theme::AMBER),
                    (-3.0, 0.0, theme::RED),
                ] {
                    if db > lo {
                        let top = y_of(db.min(hi));
                        let seg = Rect::from_min_max(pos2(x, top), pos2(x + bar, y_of(lo)));
                        p.rect_filled(seg, CornerRadius::ZERO, c);
                    }
                }
            }
            let label = if n == 2 {
                ["L", "R"][i].to_string()
            } else {
                format!("{}", i + 1)
            };
            p.text(
                pos2(x + bar / 2.0, rect.bottom() - 8.0),
                Align2::CENTER_CENTER,
                label,
                theme::semibold(8.5),
                theme::TEXT_FAINT,
            );
        }
        if !audio_ok {
            p.text(
                inner.center(),
                Align2::CENTER_CENTER,
                icons::ERROR,
                theme::icon_font(16.0),
                theme::RED,
            );
        }
        resp.on_hover_text(match &show.media.audio {
            Some(a) => format!(
                "Output: {} · {} Hz · {} ch",
                a.device_name, a.rate, a.channels
            ),
            None => "No audio output".into(),
        });
    }

    fn active_panel(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        use egui::{Align2, CornerRadius, Margin, Sense, Stroke, vec2};
        let now = Instant::now();
        let mut list: Vec<(Instant, CueId)> = show
            .engine
            .instances
            .iter()
            .map(|(id, i)| (i.fired_at, *id))
            .collect();
        list.sort();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(theme::caption("Active cues"));
            if !list.is_empty() {
                ui.label(
                    RichText::new(format!("{}", list.len()))
                        .font(theme::semibold(10.5))
                        .color(theme::GREEN),
                );
            }
        });
        ui.add_space(2.0);
        if list.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(theme::icon(icons::CHECK_CIRCLE, 30.0).color(theme::TEXT_FAINT));
                ui.label(RichText::new("Nothing running").color(theme::TEXT_FAINT));
            });
        }
        let log_h = if self.show_log { 170.0 } else { 0.0 };
        egui::ScrollArea::vertical()
            .id_salt("active")
            .max_height(ui.available_height() - log_h)
            .show(ui, |ui| {
                for (_, id) in list {
                    let Some(cue) = show.ws.cue_list.find(id) else {
                        continue;
                    };
                    let (tag, number, name) =
                        (cue.kind.tag(), cue.number.clone(), cue.display_name());
                    let is_media = matches!(cue.kind, CueKind::Audio(_) | CueKind::Video(_));
                    let Some(inst) = show.engine.instances.get(&id) else {
                        continue;
                    };
                    let paused = inst.is_paused();
                    let prewait = inst.in_pre_wait(now);
                    let (label, frac, color) = if prewait {
                        let e = inst.pre_wait_elapsed(now);
                        (
                            format!("pre-wait −{}", fmt_time(inst.pre_wait - e)),
                            (e / inst.pre_wait.max(1e-6)) as f32,
                            AMBER,
                        )
                    } else {
                        let e = if is_media {
                            show.media.elapsed(id).unwrap_or(inst.action_elapsed(now))
                        } else {
                            inst.action_elapsed(now)
                        };
                        let c = if paused { AMBER } else { GREEN };
                        match inst.action_duration {
                            Some(d) if d > 0.0 => (
                                format!("{}  −{}", fmt_time(e), fmt_time((d - e).max(0.0))),
                                (e / d) as f32,
                                c,
                            ),
                            _ => (fmt_time(e), 1.0, theme::BLUE),
                        }
                    };
                    egui::Frame::new()
                        .fill(theme::BG_RAISED)
                        .corner_radius(CornerRadius::same(8))
                        .stroke(Stroke::new(
                            1.0,
                            if paused {
                                AMBER.gamma_multiply(0.5)
                            } else {
                                theme::BORDER
                            },
                        ))
                        .inner_margin(Margin::same(8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(theme::icon(theme::kind_icon(tag), 16.0).color(color));
                                ui.label(RichText::new(&number).font(theme::semibold(13.0)));
                                ui.add(
                                    egui::Label::new(RichText::new(&name).color(theme::TEXT))
                                        .truncate(),
                                );
                            });
                            let (r, _) = ui.allocate_exact_size(
                                vec2(ui.available_width(), 6.0),
                                Sense::hover(),
                            );
                            widgets::paint_progress(ui, r, frac, color);
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(&label)
                                        .font(theme::mono(11.5))
                                        .color(theme::TEXT_DIM),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if theme::icon_button(ui, icons::STOP, "Stop this cue")
                                            .clicked()
                                        {
                                            self.control(show, Command::Cue(id, CueAction::Stop));
                                        }
                                        let (g, tip) = if paused {
                                            (icons::PLAY_ARROW, "Resume")
                                        } else {
                                            (icons::PAUSE, "Pause")
                                        };
                                        if theme::icon_button(ui, g, tip).clicked() {
                                            self.control(show, Command::Cue(id, if paused { CueAction::Resume } else { CueAction::Pause }));
                                        }
                                    },
                                );
                            });
                        });
                    ui.add_space(2.0);
                    let _ = Align2::CENTER_CENTER;
                }
            });
        if self.show_log {
            ui.separator();
            ui.label(theme::caption("Engine log"));
            egui::ScrollArea::vertical()
                .id_salt("log")
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for l in &show.engine.log {
                        ui.label(
                            RichText::new(theme::with_icon(icons::WARNING, l))
                                .small()
                                .color(AMBER),
                        );
                    }
                });
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, show: &Show) {
        let item = |ui: &mut egui::Ui, glyph: &str, text: String, color: Color32| {
            ui.label(theme::icon(glyph, 15.0).color(color));
            ui.label(RichText::new(text).small().color(theme::TEXT_DIM));
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            let file = if let Some((t, _)) = self.remote.title() {
                t
            } else {
                show
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".into())
            };
            let replica = self.remote.is_replica();
            item(ui, icons::FOLDER, file, theme::TEXT_DIM);
            if show.dirty && !replica {
                ui.label(RichText::new("●").small().color(AMBER))
                    .on_hover_text("Unsaved changes");
            }
            ui.add_space(10.0);
            match (&show.media.audio, &show.media.audio_error) {
                // A replica plays nothing itself.
                _ if replica => {}
                (Some(a), _) => item(
                    ui,
                    icons::SPEAKER,
                    format!(
                        "{} · {} kHz · {} ch",
                        a.device_name,
                        a.rate as f32 / 1000.0,
                        a.channels
                    ),
                    theme::TEXT_DIM,
                ),
                (None, e) => item(
                    ui,
                    icons::SPEAKER,
                    format!("no audio: {}", e.as_deref().unwrap_or("off")),
                    RED,
                ),
            }
            ui.add_space(10.0);
            match &show.media.video_error {
                _ if replica => {}
                None => item(ui, icons::VIDEOCAM, "video ready".into(), theme::TEXT_DIM),
                Some(e) => item(ui, icons::VIDEOCAM, format!("video: {e}"), AMBER),
            }
            ui.add_space(10.0);
            match (&self.osc, &self.osc_error) {
                (Some(o), _) => item(
                    ui,
                    icons::SETTINGS_REMOTE,
                    format!("OSC :{}", o.port),
                    theme::GREEN,
                ),
                (None, Some(_)) => item(ui, icons::SETTINGS_REMOTE, "OSC error".into(), RED),
                _ => {}
            }
            if let Some((text, replica)) = self.remote.status() {
                ui.add_space(10.0);
                let glyph = if replica { icons::CABLE } else { icons::LAN };
                item(ui, glyph, text, if replica { theme::BLUE } else { theme::GREEN });
            }
            ui.add_space(10.0);
            item(
                ui,
                icons::NOTES,
                format!("{} cues", show.ws.cue_list.iter_all().len()),
                theme::TEXT_DIM,
            );
            if let Some((msg, t)) = &self.status {
                if t.elapsed() < Duration::from_secs(6) {
                    ui.add_space(10.0);
                    ui.label(RichText::new(msg).small().color(AMBER));
                } else {
                    self.status = None;
                }
            }
            if let Some(l) = show.engine.log.last() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .link(
                            RichText::new(theme::with_icon(icons::WARNING, l))
                                .color(AMBER)
                                .small(),
                        )
                        .clicked()
                    {
                        self.show_log = true;
                    }
                });
            }
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        let mut open = self.show_settings;
        egui::Window::new("Settings")
            .open(&mut open)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.settings_tab,
                        SettingsTab::General,
                        theme::with_icon(icons::SETTINGS, "General"),
                    );
                    ui.selectable_value(
                        &mut self.settings_tab,
                        SettingsTab::Audio,
                        theme::with_icon(icons::SPEAKER, "Audio"),
                    );
                    ui.selectable_value(
                        &mut self.settings_tab,
                        SettingsTab::Stages,
                        theme::with_icon(icons::DESKTOP_WINDOWS, "Video stages"),
                    );
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
            ui.label("OSC remote control");
            ui.horizontal(|ui| {
                ui.checkbox(&mut show.ws.settings.osc_enabled, "Listen on UDP port");
                ui.add(egui::DragValue::new(&mut show.ws.settings.osc_port).range(1024..=65535));
            });
            ui.end_row();
            ui.label("");
            match (&self.osc, &self.osc_error) {
                (Some(o), _) => ui.label(
                    RichText::new(format!(
                        "listening on {} · /go /stop /pause /resume /cue/<n>/start …",
                        o.port
                    ))
                    .weak(),
                ),
                (None, Some(e)) => ui.label(RichText::new(e).color(RED)),
                _ => ui.label(RichText::new("off").weak()),
            };
            ui.end_row();
        });
        if show.ws.settings != before {
            if let Some(a) = &mut show.media.audio {
                a.set_master(show.ws.settings.master_volume_db);
            }
            show.edited();
        }
    }

    fn settings_audio(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        if self.remote.is_replica() {
            ui.label(RichText::new("Audio output is set up on the main device.").weak());
            return;
        }
        if self.devices.is_none() {
            self.devices = Some(media::audio::list_output_devices());
        }
        let devices = self.devices.clone().unwrap_or_default();
        let current = show.ws.settings.audio_device.clone();
        let label = |id: &Option<String>| match id {
            None => "System default".to_string(),
            Some(id) => devices
                .iter()
                .find(|d| &d.id == id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| format!("{id} (missing)")),
        };
        let mut chosen = current.clone();
        egui::Grid::new("audio_settings")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Output device");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("device")
                        .width(320.0)
                        .selected_text(label(&chosen))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut chosen, None, "System default");
                            for d in &devices {
                                ui.selectable_value(&mut chosen, Some(d.id.clone()), &d.name)
                                    .on_hover_text(&d.id);
                            }
                        });
                    if theme::icon_button(ui, icons::REFRESH, "Rescan devices").clicked() {
                        self.devices = None;
                    }
                });
                ui.end_row();
                ui.label("Active");
                match &show.media.audio {
                    Some(a) => ui.label(format!(
                        "{} · {} Hz · {} outputs",
                        a.device_name, a.rate, a.channels
                    )),
                    None => ui.label(
                        RichText::new(show.media.audio_error.clone().unwrap_or_default())
                            .color(RED),
                    ),
                };
                ui.end_row();
            });
        ui.label(RichText::new("Changing the device stops all playback. Cue routing matrices address the device's outputs.").weak());
        if chosen != current {
            show.ws.settings.audio_device = chosen;
            let now = Instant::now();
            let Show {
                engine, media, ws, ..
            } = show;
            engine.stop_all(media, now, 0.0);
            media.reopen_audio(
                ws.settings.audio_device.as_deref(),
                ws.settings.master_volume_db,
            );
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
                    ui.add(
                        egui::DragValue::new(&mut st.width)
                            .range(16..=16384)
                            .suffix(" px"),
                    );
                    ui.label("×");
                    ui.add(
                        egui::DragValue::new(&mut st.height)
                            .range(16..=16384)
                            .suffix(" px"),
                    );
                    if n > 1 && theme::icon_button(ui, icons::DELETE, "Delete stage").clicked() {
                        remove = Some(i);
                    }
                });
                ui.horizontal(|ui| {
                    let mut open = self.stage_open.contains(&st.id);
                    if ui
                        .toggle_value(
                            &mut open,
                            theme::with_icon(icons::DESKTOP_WINDOWS, "Window"),
                        )
                        .changed()
                    {
                        if open {
                            self.stage_open.insert(st.id);
                        } else {
                            self.stage_open.remove(&st.id);
                        }
                    }
                    if ui
                        .button(theme::with_icon(icons::FULLSCREEN, "Fullscreen"))
                        .on_hover_text("Also: double-click or F11 in the stage window")
                        .clicked()
                    {
                        self.stage_open.insert(st.id);
                        let fs = !self.stage_fullscreen.remove(&st.id);
                        if fs {
                            self.stage_fullscreen.insert(st.id);
                        }
                        #[cfg(feature = "video")]
                        ui.ctx().send_viewport_cmd_to(
                            stage::viewport_id(st.id),
                            egui::ViewportCommand::Fullscreen(fs),
                        );
                    }
                    let mut editing = self.mapping_edit.contains(&st.id);
                    if ui
                        .toggle_value(
                            &mut editing,
                            theme::with_icon(icons::CROP_FREE, "Edit mapping"),
                        )
                        .on_hover_text("Shows a test pattern and corner pins in the stage window")
                        .changed()
                    {
                        if editing {
                            self.mapping_edit.insert(st.id);
                            self.stage_open.insert(st.id);
                        } else {
                            self.mapping_edit.remove(&st.id);
                        }
                    }
                    if ui
                        .button(theme::with_icon(icons::REPLAY, "Reset mapping"))
                        .clicked()
                    {
                        st.corners = Stage::IDENTITY;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Output");
                    let label = st.output.clone().unwrap_or_else(|| "Window".into());
                    egui::ComboBox::from_id_salt(("stage-output", st.id))
                        .selected_text(label)
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            self.output_choices(ui, st);
                        });
                });
                if st.corners != Stage::IDENTITY {
                    ui.label(
                        RichText::new(format!(
                            "corners: {}",
                            st.corners
                                .iter()
                                .map(|c| format!("({:.3}, {:.3})", c[0], c[1]))
                                .collect::<Vec<_>>()
                                .join("  ")
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
        if ui
            .button(theme::with_icon(icons::ADD, "Add stage"))
            .clicked()
        {
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
        let path = if path.extension().is_none() {
            path.with_extension(Workspace::EXTENSION)
        } else {
            path
        };
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

    /// Load a workspace file, replacing the current show.
    pub fn open_path(&mut self, path: PathBuf) {
        match Workspace::load(&path) {
            Ok(ws) => {
                self.show.lock().replace_workspace(ws, Some(path.clone()));
                self.reset_undo();
                let s = self.show.lock();
                self.selection.clear();
                self.collapsed.clear();
                self.last_playhead = None;
                // Stages assigned to a display come up on it straight away.
                self.stage_open.extend(s.ws.stages.iter().filter(|st| st.output.is_some()).map(|st| st.id));
                drop(s);
                self.notify(format!("Opened {}", path.display()));
            }
            Err(e) => self.notify(format!("Open failed: {e:#}")),
        }
    }

    fn run_deferred(&mut self) {
        for d in std::mem::take(&mut self.deferred) {
            if self.remote.is_replica() {
                let why = match d {
                    Deferred::New | Deferred::Open => Some("Disconnect from the main device first (Remote → Disconnect)"),
                    Deferred::PickMedia(_) | Deferred::AddMediaCues => Some("Media files live on the main device: add them there"),
                    _ => None,
                };
                if let Some(why) = why {
                    self.notify(why);
                    continue;
                }
            }
            match d {
                Deferred::New => {
                    if self.confirm_discard() {
                        self.show
                            .lock()
                            .replace_workspace(Workspace::default(), None);
                        self.reset_undo();
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
                    let video = matches!(
                        self.show.lock().ws.cue_list.find(id).map(|c| c.kind.tag()),
                        Some(KindTag::Video)
                    );
                    let mut dlg = rfd::FileDialog::new();
                    dlg = if video {
                        dlg.add_filter(
                            "Video",
                            &[
                                "mp4", "mov", "mkv", "avi", "webm", "m4v", "mpg", "mpeg", "ogv",
                                "mxf",
                            ],
                        )
                    } else {
                        dlg.add_filter(
                            "Audio",
                            &[
                                "wav", "aif", "aiff", "flac", "mp3", "ogg", "opus", "m4a", "aac",
                                "caf",
                            ],
                        )
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
                            &[
                                "wav", "aif", "aiff", "flac", "mp3", "ogg", "opus", "m4a", "aac",
                                "caf", "mp4", "mov", "mkv", "avi", "webm", "m4v",
                            ],
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
    /// `OPENQ_SCREENSHOT_MODE=show` starts in show mode; `OPENQ_SCREENSHOT_INLINE=1` opens an
    /// in-place pre-wait editor on the selected cue.
    /// `OPENQ_SCREENSHOT_STAGE=1` embeds stage windows in the main window for the capture
    /// (`OPENQ_SCREENSHOT_MAPPING=1` turns on the first stage's mapping test pattern).
    fn debug_screenshot(&mut self, ctx: &egui::Context, show: &mut Show) {
        let Some(path) = std::env::var_os("OPENQ_SCREENSHOT") else {
            return;
        };
        self.frame_count += 1;
        if self.frame_count == 1 {
            // Optional: select cue N and an inspector tab (time|levels|eq|geometry|settings).
            if let Ok(n) = std::env::var("OPENQ_SCREENSHOT_CUE") {
                let id = show
                    .ws
                    .cue_list
                    .iter_all()
                    .iter()
                    .find(|c| c.number == n)
                    .map(|c| c.id);
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
            if std::env::var("OPENQ_SCREENSHOT_MODE").as_deref() == Ok("show") {
                self.mode = Mode::Show;
            }
            if std::env::var_os("OPENQ_SCREENSHOT_INLINE").is_some()
                && let Some(id) = self.primary()
            {
                self.inline = Some(cue_list::InlineEdit::new(
                    id,
                    cue_list::InlineField::PreWait,
                    "1:30.5".into(),
                ));
            }
            if std::env::var_os("OPENQ_SCREENSHOT_MAPPING").is_some()
                && let Some(st) = show.ws.stages.first()
            {
                self.mapping_edit.insert(st.id);
                self.stage_open.insert(st.id);
            }
            if let Ok(addr) = std::env::var("OPENQ_SCREENSHOT_CONNECT") {
                self.connect(ctx, show, addr, crate::remote::client::Auth::Token);
            }
            if std::env::var_os("OPENQ_SCREENSHOT_PAIR").is_some() {
                self.remote_debug_pair();
            }
            if let Ok(t) = std::env::var("OPENQ_SCREENSHOT_REMOTE") {
                self.remote.window = true;
                self.remote.tab = if t == "connect" { remote::RemoteTab::Connect } else { remote::RemoteTab::Share };
            }
            if std::env::var_os("OPENQ_SCREENSHOT_ABOUT").is_some() {
                self.show_about = true;
            }
            if std::env::var_os("OPENQ_SCREENSHOT_PREVIEW").is_some() {
                self.show_preview = true;
            }
            if let Ok(t) = std::env::var("OPENQ_SCREENSHOT_SETTINGS") {
                self.show_settings = true;
                self.settings_tab = if t == "audio" {
                    SettingsTab::Audio
                } else {
                    SettingsTab::Stages
                };
            }
        }
        ctx.request_repaint();
        let gos: u64 = std::env::var("OPENQ_SCREENSHOT_GOS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // GOs 0.4 s apart (past double-GO protection), then shoot 2 s after the last one.
        let t0 = *self.debug_t0.get_or_insert_with(Instant::now);
        let elapsed = t0.elapsed().as_secs_f64();
        let gos_due = ((elapsed / 0.4) as u64).min(gos);
        while self.debug_gos < gos_due {
            self.control(show, Command::Go);
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

/// A filled circular button with a filled icon (transport controls).
fn round_button(ui: &mut egui::Ui, glyph: &str, fill: Color32, size: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let fill = if resp.hovered() {
        fill.gamma_multiply(1.25)
    } else {
        fill
    };
    let p = ui.painter();
    p.circle_filled(rect.center(), size / 2.0, fill);
    p.circle_stroke(
        rect.center(),
        size / 2.0,
        egui::Stroke::new(1.0, Color32::from_white_alpha(20)),
    );
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        theme::icon_font_filled(size * 0.5),
        Color32::WHITE,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Fonts and visuals; call once at startup.
pub fn install_theme(ctx: &egui::Context) {
    theme::install(ctx);
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
            self.remote_tick(&ctx, show);
            self.sync_playhead(show);
            self.handle_keys(&ctx, show);
            self.handle_dropped_files(&ctx, show);
            self.debug_screenshot(&ctx, show);

            let fresh = match self.remote.meters() {
                Some(m) => m.to_vec(),
                None => show.media.take_meters(media::audio::MeterReader::Local),
            };
            if self.meters.len() != fresh.len() {
                self.meters = fresh.clone();
            }
            for (m, f) in self.meters.iter_mut().zip(fresh) {
                *m = f.max(*m * 0.85);
            }

            let bar = |fill| {
                egui::Frame::new()
                    .fill(fill)
                    .inner_margin(egui::Margin::symmetric(10, 4))
            };
            egui::Panel::top("titlebar")
                .frame(
                    egui::Frame::new()
                        .fill(Color32::from_rgb(0x0e, 0x0f, 0x12))
                        .inner_margin(egui::Margin::symmetric(14, 6)),
                )
                .show_separator_line(false)
                .show(ui, |ui| self.title_bar(ui, show));
            egui::Panel::top("menu")
                .frame(bar(theme::BG_APP))
                .show(ui, |ui| self.menu_bar(ui, show));
            egui::Panel::top("header")
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG_PANEL)
                        .inner_margin(egui::Margin::symmetric(14, 12)),
                )
                .show(ui, |ui| self.header(ui, show));
            egui::Panel::bottom("status")
                .frame(bar(theme::BG_APP))
                .show(ui, |ui| self.status_bar(ui, show));
            if self.mode == Mode::Edit {
                egui::Panel::bottom("inspector")
                    .resizable(true)
                    .default_size(340.0)
                    .min_size(120.0)
                    .show(ui, |ui| self.inspector(ui, show));
            }
            egui::Panel::right("active")
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG_PANEL)
                        .inner_margin(egui::Margin::same(10)),
                )
                .resizable(true)
                .default_size(280.0)
                .show(ui, |ui| self.active_panel(ui, show));
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG_APP)
                        .inner_margin(egui::Margin::symmetric(10, 8)),
                )
                .show(ui, |ui| {
                    if self.mode == Mode::Edit {
                        self.toolbar(ui, show);
                        ui.separator();
                    }
                    self.cue_list(ui, show);
                });

            if self.show_settings {
                self.settings_window(&ctx, show);
            }
            #[cfg(feature = "video")]
            self.geometry_preview(&ctx, show);
            self.remote_window(&ctx, show);
            self.about_window(&ctx);
            self.track_undo(&ctx, show);
            self.sync_osc(&ctx, show);

            #[cfg(feature = "video")]
            let video_active = self.stages(&ctx, show);
            #[cfg(not(feature = "video"))]
            let video_active = false;

            let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
            if hovering {
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("drop"),
                ));
                let rect = ctx.content_rect();
                painter.rect_filled(rect, 0.0, Color32::from_black_alpha(160));
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Drop audio / video files to create cues",
                    egui::FontId::proportional(26.0),
                    Color32::WHITE,
                );
            }

            let name = show
                .path
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".into());
            let title = format!("{name}{} — OpenQ", if show.dirty { " •" } else { "" });
            if title != self.title {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
                self.title = title;
            }

            if show.engine.is_active() {
                ctx.request_repaint_after(if video_active {
                    Duration::from_millis(16)
                } else {
                    Duration::from_millis(33)
                });
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
