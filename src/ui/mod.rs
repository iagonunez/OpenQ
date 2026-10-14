//! egui front end.

mod about;
mod animation;
pub(crate) mod bench;
#[cfg(feature = "video")]
mod blend;
mod debug;
mod cue_list;
mod devices;
mod eq;
mod effects;
mod fixture_types;
mod icons;
mod inspector;
mod language;
mod lighting;
mod manual;
#[cfg(target_os = "macos")]
mod native_menu;
mod outputs;
#[cfg(all(test, feature = "video"))]
mod perf_tests;
#[cfg(feature = "video")]
mod preview;
mod recovery;
mod remote;
mod room;
#[cfg(feature = "video")]
mod stage;
mod surfaces;
mod theme;
mod waveform;
mod widgets;

use crate::media::audio::DeviceEntry;
use crate::media::{self, Prober, Waveforms};
use crate::model::{
    AudioCue, Cue, CueId, CueKind, GroupMode, KindTag, Stage, StageId, VideoCue, Workspace,
};
use crate::control::{Command, CueAction};
use crate::errlog;
use crate::show::{SharedShow, Show};
use egui::{Color32, Key, KeyboardShortcut, Modifiers, RichText};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use widgets::{AMBER, GREEN, RED, fmt_time};
use crate::i18n::t;

/// Work that needs a native dialog; done after the show lock is released so the engine keeps
/// running while a dialog is open.
enum Deferred {
    New,
    Open,
    Save,
    SaveAs,
    PickMedia(CueId),
    AddMediaCues,
    /// Open a show autosaved before a crash (may ask to discard the current one first).
    Recover(crate::recovery::Orphan),
    /// Import fixture types (OFL, GDTF, QLC+, OpenQ files) into the design and the library.
    ImportFixture,
    /// Save a fixture type as an `.oqfixture` file.
    ExportFixture(Box<crate::light::FixtureType>),
    /// Import effect files (`.oqfx`) into the effects pool.
    ImportEffects,
    /// Save an effect as an effect file.
    ExportEffect(crate::light::effects::EffectId),
    /// Save the patch as an MVR file (false) or the plan as an SVG plot (true).
    ExportPatch(bool),
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
    lighting: crate::light::Lighting,
    /// Cues in `cues` plus the lighting design's weight in cue-sized units (the history's memory
    /// budget).
    size: usize,
}

impl UndoState {
    fn of(show: &Show) -> Self {
        let cues = show.ws.cue_list.clone();
        let l = &show.ws.lighting;
        // Every step copies the patch, types and cue headers; cue and preset values are shared
        // between steps (`light::model::Shared`) until edited, so they aren't counted.
        let channels: usize = l.fixture_types.iter().flat_map(|t| &t.modes).map(|m| m.all_channels().count()).sum();
        let light_cues: usize = l.sequences.iter().map(|s| s.cues.len()).sum();
        let lighting = l.fixtures.len() + channels / 4 + light_cues / 4 + l.effects.len();
        Self { size: cues.iter_all().len() + lighting, cues, stages: show.ws.stages.clone(), lighting: l.clone() }
    }
}

/// Drop the oldest steps of an undo (or redo) stack beyond [`UNDO_STEPS`] or the cue budget.
fn trim_undo(stack: &mut Vec<UndoState>) {
    let mut total: usize = stack.iter().map(|s| s.size).sum();
    let mut drop = 0;
    while stack.len() - drop > UNDO_STEPS || (stack.len() - drop > UNDO_MIN_STEPS && total > UNDO_CUE_BUDGET) {
        total -= stack[drop].size;
        drop += 1;
    }
    stack.drain(..drop);
}

/// Undo steps kept at most.
const UNDO_STEPS: usize = 200;
/// Cues kept across all undo steps. Every step is a full copy of the cue lists (~0.5 KB per
/// cue) and of the lighting patch, so 200 steps of a 5000-cue show would hold ~500 MB; big
/// shows keep fewer steps.
const UNDO_CUE_BUDGET: usize = 100_000;
/// Steps kept whatever the budget says.
const UNDO_MIN_STEPS: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    General,
    Audio,
    Stages,
    Surfaces,
}

/// Default height of the inspector panel: room for the tallest tab (the waveform plus a row of
/// cards, or the level faders) without scrolling.
const INSPECTOR_HEIGHT: f32 = 420.0;
/// How long status bar messages (notifications, the latest engine log entry) stay visible.
const STATUS_TIMEOUT: Duration = Duration::from_secs(6);

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
    /// The menu bar in the macOS system bar (installed on the first frame).
    #[cfg(target_os = "macos")]
    native_menu: Option<native_menu::NativeMenu>,
    /// Stage windows currently open / fullscreen / in mapping-edit mode.
    stage_open: HashSet<StageId>,
    stage_fullscreen: HashSet<StageId>,
    mapping_edit: HashSet<StageId>,
    /// Displays a stage can be sent to, and the monitor index last applied to each stage window.
    displays: Arc<Mutex<outputs::DisplayList>>,
    #[cfg(feature = "video")]
    stage_monitor: std::collections::HashMap<StageId, Option<usize>>,
    show_preview: bool,
    /// The user has dragged the inspector's edge; until then it keeps its default height.
    inspector_resized: bool,
    /// Text of the current cue list's name while it is being renamed.
    list_rename: Option<String>,
    /// Audio inputs, cameras and MIDI ports, loaded in the background.
    inputs: devices::SharedDevices,
    /// The current list's Timecode window is open.
    show_list_timecode: bool,
    show_about: bool,
    remote: remote::RemoteUi,
    #[cfg(feature = "video")]
    posters: Arc<media::Posters>,
    #[cfg(feature = "video")]
    preview_tex: Option<(String, egui::TextureHandle, [f32; 2])>,
    #[cfg(feature = "video")]
    preview_text: preview::PreviewText,
    waveforms: Arc<Waveforms>,
    inspector_tab: inspector::Tab,
    /// Animation tab playhead and selection.
    anim_edit: animation::AnimEdit,
    settings_tab: SettingsTab,
    /// Cached audio device list for the settings window.
    devices: Option<Vec<DeviceEntry>>,
    #[cfg(feature = "video")]
    textures: stage::LayerTextures,
    /// The wgpu renderer, for stage layers with a blend mode (None when headless).
    #[cfg(feature = "video")]
    gpu: Option<eframe::egui_wgpu::RenderState>,
    meters: Vec<f32>,
    status: Option<(String, Instant)>,
    /// The engine log entry shown in the status bar and when it appeared (hidden after
    /// [`STATUS_TIMEOUT`]; View → Engine log keeps them all).
    log_tail: (usize, String, Instant),
    show_settings: bool,
    /// The first-run language picker is open ([`App::ask_language`]).
    ask_language: bool,
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
    /// What the OSC listener and the MIDI surface share (learn mode, button states).
    surfaces: crate::surface::SurfaceHub,
    /// This computer's MIDI input for control surfaces, and its connection.
    midi_port: Option<String>,
    midi_surface: Option<crate::surface::MidiSurface>,
    /// Where a control being learned goes.
    learn_into: Option<surfaces::LearnInto>,
    debug_t0: Option<Instant>,
    /// `OPENQ_DEBUG_SCRIPT` run and frame timings.
    debug_run: debug::DebugRun,
    debug_gos: u64,
    /// Autosave session and shows to recover after a crash.
    recovery: recovery::Recovery,
    /// Audio was reconnecting at the last frame (to announce changes).
    audio_reconnecting: bool,
    /// Cue lists or the Lighting workspace.
    view: lighting::View,
    /// Lighting workspace state.
    light: lighting::LightUi,
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
            #[cfg(target_os = "macos")]
            native_menu: None,
            stage_open: HashSet::new(),
            stage_fullscreen: HashSet::new(),
            mapping_edit: HashSet::new(),
            displays: Default::default(),
            #[cfg(feature = "video")]
            stage_monitor: Default::default(),
            show_preview: false,
            inspector_resized: false,
            list_rename: None,
            inputs: Default::default(),
            show_list_timecode: false,
            show_about: false,
            remote: Default::default(),
            #[cfg(feature = "video")]
            posters: media::Posters::new(),
            #[cfg(feature = "video")]
            preview_tex: None,
            #[cfg(feature = "video")]
            preview_text: Default::default(),
            waveforms: Waveforms::new(),
            inspector_tab: Default::default(),
            anim_edit: Default::default(),
            settings_tab: SettingsTab::General,
            devices: None,
            #[cfg(feature = "video")]
            textures: Default::default(),
            #[cfg(feature = "video")]
            gpu: None,
            meters: Vec::new(),
            status: None,
            log_tail: (0, String::new(), Instant::now()),
            show_settings: false,
            ask_language: false,
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
            surfaces: Default::default(),
            midi_port: crate::surface::saved_midi_port(),
            midi_surface: None,
            learn_into: None,
            debug_t0: None,
            debug_run: Default::default(),
            debug_gos: 0,
            recovery: Default::default(),
            audio_reconnecting: false,
            view: Default::default(),
            light: Default::default(),
        }
    }

    /// Hand the app this instance's autosave session (`now` asks its thread to write at once)
    /// and the shows earlier crashed instances left behind, which it offers to recover.
    pub fn set_recovery(
        &mut self,
        autosave: Option<Arc<Mutex<crate::recovery::Autosave>>>,
        now: Arc<std::sync::atomic::AtomicBool>,
        orphans: Vec<crate::recovery::Orphan>,
    ) {
        self.recovery = recovery::Recovery { autosave, now, orphans };
    }

    fn primary(&self) -> Option<CueId> {
        self.selection.last().copied()
    }

    /// Open the bundled user manual in the default browser.
    fn open_manual(&mut self) {
        match manual::open() {
            Ok(path) => self.notify(tr!("Opened the user manual in your browser ({path})", path = path.display())),
            Err(e) => self.notify_error(format!("{e:#}")),
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// Like [`Self::notify`], for failures: also written to the error log.
    fn notify_error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::warn!(target: "openq::ui", "{msg}");
        self.notify(msg);
    }

    /// Open the error log (warnings, errors, crashes) with the system's text viewer.
    fn open_error_log(&mut self) {
        let Some(path) = errlog::log_path() else {
            return self.notify(t("No config folder: the error log is off"));
        };
        if let Err(e) = manual::open_with_default_app(&path) {
            self.notify(tr!("Cannot open {path}: {e}", path = path.display(), e));
        }
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
            cue.name = tr!("Fade out {name}", name = target.display_name());
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
            self.notify(tr!("Unsupported file: {path}", path = path.display()));
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
        group.name = t("Group").into();
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
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F1)) {
            self.open_manual();
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
            // Delete and Enter act on the cue list only while it is showing (the Lighting view
            // uses Delete for its own things).
            let cues = self.view == lighting::View::Cues;
            if cues
                && ctx.input_mut(|i| {
                    i.consume_key(Modifiers::NONE, Key::Delete)
                        || i.consume_key(Modifiers::NONE, Key::Backspace)
                })
            {
                self.delete_selected(show);
            }
            if cues && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
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
            self.notify(t("This device can only run the show: editing needs the Control & edit role"));
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
        if self.undo_base.is_none() {
            self.undo_base = Some(UndoState::of(show));
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
        let now = UndoState::of(show);
        if self.undo_base.as_ref() != Some(&now)
            && let Some(prev) = self.undo_base.replace(now)
        {
            self.undo.push(prev);
            trim_undo(&mut self.undo);
            self.redo.clear();
        }
    }

    fn apply_undo_state(&mut self, show: &mut Show, st: UndoState) {
        show.ws.cue_list = st.cues.clone();
        show.ws.stages = st.stages.clone();
        show.ws.lighting = st.lighting.clone();
        show.dirty = true;
        show.engine.workspace_changed();
        show.sync_lighting();
        self.undo_base = Some(st);
        self.undo_seen_gen = show.edit_gen;
        self.inline = None;
    }

    fn undo(&mut self, show: &mut Show) {
        self.commit_undo(show);
        if let Some(prev) = self.undo.pop() {
            self.redo.push(UndoState::of(show));
            trim_undo(&mut self.redo);
            self.apply_undo_state(show, prev);
            self.notify(t("Undo"));
        }
    }

    fn redo(&mut self, show: &mut Show) {
        self.commit_undo(show);
        if let Some(next) = self.redo.pop() {
            self.undo.push(UndoState::of(show));
            trim_undo(&mut self.undo);
            self.apply_undo_state(show, next);
            self.notify(t("Redo"));
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
        // Don't retry a port that failed until the setting changes (but turning OSC off must
        // always go through, to drop a running listener).
        if want.is_some() && want == self.osc_failed_for {
            return;
        }
        self.osc = None;
        self.osc_error = None;
        self.osc_failed_for = None;
        if let Some(port) = want {
            let c = ctx.clone();
            match crate::osc::OscServer::start_with(port, self.show.clone(), self.surfaces.clone(), move || c.request_repaint())
            {
                Ok(s) => self.osc = Some(s),
                Err(e) => {
                    self.osc_error = Some(tr!("OSC port {port}: {e}", port, e));
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
            self.notify(t("Switch to edit mode (Ctrl+E) to add cues"));
            return;
        }
        if self.remote.is_replica() && !files.is_empty() {
            self.notify(t("Media files live on the main device: add them there"));
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
            ui.menu_button(t("File"), |ui| {
                if ui.button(wi(I::NOTE_ADD, t("New"))).clicked() {
                    self.deferred.push(Deferred::New);
                }
                if ui.button(wi(I::FOLDER_OPEN, t("Open…"))).clicked() {
                    self.deferred.push(Deferred::Open);
                }
                if ui.button(wi(I::SAVE, t("Save"))).clicked() {
                    self.deferred.push(Deferred::Save);
                }
                if ui.button(wi(I::SAVE, t("Save As…"))).clicked() {
                    self.deferred.push(Deferred::SaveAs);
                }
                ui.separator();
                if ui.button(wi(I::ADD, t("Import media files…"))).clicked() {
                    self.deferred.push(Deferred::AddMediaCues);
                }
                ui.separator();
                if ui.button(wi(I::CLOSE, t("Quit"))).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button(t("Edit"), |ui| {
                if self.mode == Mode::Show {
                    ui.disable();
                }
                let can_undo = !self.undo.is_empty() || self.undo_pending.is_some();
                if ui
                    .add_enabled(
                        can_undo,
                        egui::Button::new(wi(I::UNDO, t("Undo"))).shortcut_text("Ctrl+Z"),
                    )
                    .clicked()
                {
                    self.undo(show);
                }
                if ui
                    .add_enabled(
                        !self.redo.is_empty(),
                        egui::Button::new(wi(I::REDO, t("Redo"))).shortcut_text(t("Ctrl+Shift+Z")),
                    )
                    .clicked()
                {
                    self.redo(show);
                }
                ui.separator();
                if ui
                    .add(
                        egui::Button::new(wi(I::CONTENT_COPY, t("Duplicate"))).shortcut_text("Ctrl+D"),
                    )
                    .clicked()
                {
                    self.duplicate_selected(show);
                }
                if ui
                    .add(egui::Button::new(wi(I::DELETE, t("Delete"))).shortcut_text(t("Del")))
                    .clicked()
                {
                    self.delete_selected(show);
                }
                if ui
                    .add(
                        egui::Button::new(wi(I::CREATE_NEW_FOLDER, t("Group selected")))
                            .shortcut_text("Ctrl+G"),
                    )
                    .clicked()
                {
                    self.group_selected(show);
                }
                ui.separator();
                if ui.button(wi(I::REFRESH, t("Renumber all cues"))).clicked() {
                    self.renumber_all(show);
                }
                if ui.button(wi(I::SETTINGS, t("Settings…"))).clicked() {
                    self.show_settings = true;
                }
            });
            ui.menu_button(t("Cue"), |ui| {
                if self.mode == Mode::Show {
                    ui.disable();
                }
                for (i, tag) in KindTag::ALL.iter().enumerate() {
                    let mut b = egui::Button::new(wi(
                        theme::kind_icon(*tag),
                        &tr!("New {kind} cue", kind = tag.label()),
                    ));
                    if i < 10 {
                        b = b.shortcut_text(format!("Ctrl+{}", (i + 1) % 10));
                    }
                    if ui.add(b).clicked() {
                        self.add_cue(show, *tag);
                    }
                }
            });
            ui.menu_button(t("Show"), |ui| {
                if ui
                    .add(egui::Button::new(wi(I::PLAY_ARROW, t("GO"))).shortcut_text(t("Space")))
                    .clicked()
                {
                    self.control(show, Command::Go);
                }
                if ui
                    .add(egui::Button::new(wi(I::STOP, t("Stop all"))).shortcut_text("Esc"))
                    .clicked()
                {
                    self.control(show, Command::Stop);
                }
                if ui
                    .add(egui::Button::new(wi(I::PAUSE, t("Pause all"))).shortcut_text("P"))
                    .clicked()
                {
                    self.control(show, Command::PauseAll);
                }
                if ui
                    .add(egui::Button::new(wi(I::PLAY_ARROW, t("Resume all"))).shortcut_text("R"))
                    .clicked()
                {
                    self.control(show, Command::ResumeAll);
                }
                ui.separator();
                let label = if self.mode == Mode::Edit {
                    wi(I::THEATER_COMEDY, t("Enter show mode"))
                } else {
                    wi(I::EDIT, t("Back to edit mode"))
                };
                if ui
                    .add(egui::Button::new(label).shortcut_text("Ctrl+E"))
                    .clicked()
                {
                    self.toggle_mode();
                }
            });
            ui.menu_button(t("Video"), |ui| {
                let mut edited = false;
                for st in show.ws.stages.iter_mut() {
                    let open = self.stage_open.contains(&st.id);
                    let glyph = if open { I::DESKTOP_WINDOWS } else { I::MONITOR };
                    ui.menu_button(wi(glyph, &tr!("Stage: {name}", name = st.name)), |ui| {
                        let mut open = open;
                        if ui.checkbox(&mut open, t("Show stage window")).changed() {
                            if open {
                                self.stage_open.insert(st.id);
                            } else {
                                self.stage_open.remove(&st.id);
                            }
                        }
                        let mut editing = self.mapping_edit.contains(&st.id);
                        if ui.checkbox(&mut editing, t("Edit mapping")).changed() {
                            if editing {
                                self.mapping_edit.insert(st.id);
                                self.stage_open.insert(st.id);
                            } else {
                                self.mapping_edit.remove(&st.id);
                            }
                        }
                        ui.separator();
                        ui.label(theme::caption(t("Output")));
                        edited |= self.output_choices(ui, st);
                    });
                }
                if edited {
                    show.edited();
                }
                ui.separator();
                if ui.button(wi(I::CROP_FREE, t("Stages & mapping…"))).clicked() {
                    self.show_settings = true;
                    self.settings_tab = SettingsTab::Stages;
                    ui.close();
                }
            });
            ui.menu_button(t("Remote"), |ui| {
                if ui.button(wi(I::LAN, t("Share this OpenQ…"))).clicked() {
                    self.remote.window = true;
                    self.remote.tab = remote::RemoteTab::Share;
                }
                if ui.button(wi(I::CABLE, t("Connect to another OpenQ…"))).clicked() {
                    self.remote.window = true;
                    self.remote.tab = remote::RemoteTab::Connect;
                }
                if self.remote.is_replica() && ui.button(wi(I::CLOSE, t("Disconnect"))).clicked() {
                    self.disconnect(show);
                }
            });
            ui.menu_button(t("View"), |ui| {
                #[cfg(feature = "video")]
                ui.checkbox(&mut self.show_preview, wi(I::PREVIEW, t("Geometry preview")));
                ui.checkbox(&mut self.show_log, wi(I::RECEIPT_LONG, t("Engine log")));
            });
            ui.menu_button(t("Help"), |ui| {
                if ui.add(egui::Button::new(wi(I::NOTES, t("User manual"))).shortcut_text("F1")).clicked() {
                    self.open_manual();
                }
                ui.separator();
                if ui.button(wi(I::RECEIPT_LONG, t("Error log"))).on_hover_text(t("Warnings, errors and crashes, kept in a local file")).clicked() {
                    self.open_error_log();
                }
                ui.separator();
                if ui.button(wi(I::INFO, t("About OpenQ and licenses…"))).clicked() {
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
                t("SPACE"),
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
                            ui.label(theme::caption(t("Standby")));
                            if show.ws.cue_list.list_count() > 1 {
                                ui.label(theme::caption(&format!("·  {}", show.ws.cue_list.info.name)));
                            }
                            if self.mode == Mode::Show {
                                ui.label(
                                    RichText::new(t("SHOW MODE"))
                                        .font(theme::semibold(10.0))
                                        .color(theme::RED),
                                );
                            }
                            // The current list's incoming timecode.
                            let sync = &show.ws.cue_list.info.timecode;
                            if sync.source != crate::model::TimecodeSource::Off {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    let (text, color) = match show.timecode.position(sync, std::time::Instant::now()) {
                                        Some(r) => (crate::timecode::format_secs(r.secs, r.rate), theme::GREEN),
                                        None => (t("no timecode").to_string(), AMBER),
                                    };
                                    ui.label(RichText::new(text).font(theme::mono(13.0)).color(color))
                                        .on_hover_text(t("Timecode this cue list follows (right-click its tab → Timecode…)"));
                                    ui.label(theme::icon(icons::TIMER_PLAY, 15.0).color(color));
                                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(40));
                                });
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
                                    RichText::new(t("End of cue list"))
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
                    t("Stop now")
                } else {
                    t("Stop all (Esc) — fades out; press again to stop instantly")
                })
                .clicked()
                {
                    self.control(show, Command::Stop);
                }
                if round_button(ui, icons::PLAY_ARROW, theme::BG_HOVER, 40.0)
                    .on_hover_text(t("Resume all (R)"))
                    .clicked()
                {
                    self.control(show, Command::ResumeAll);
                }
                if round_button(ui, icons::PAUSE, theme::BG_HOVER, 40.0)
                    .on_hover_text(t("Pause all (P)"))
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
        // Sized for a 26 pt tall control; smaller ones scale the type down.
        let k = (rect.height() / 26.0).min(1.0);
        let (icon_size, text_size) = (16.0 * k, 12.5 * k.max(0.9));
        let w = (inner.width() - gap) / 2.0;
        let halves = [
            (
                Mode::Edit,
                icons::EDIT,
                t("Edit"),
                theme::ACCENT,
                Rect::from_min_size(inner.min, vec2(w, inner.height())),
            ),
            (
                Mode::Show,
                icons::THEATER_COMEDY,
                t("Show"),
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
                .on_hover_text(t("Ctrl+E"));
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
            let icon_g = p.layout_no_wrap(glyph.to_string(), theme::icon_font(icon_size), fg);
            let text_g = p.layout_no_wrap(label.to_string(), theme::semibold(text_size), fg);
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

    /// Whether the menus are in the system menu bar (macOS), so the window has none of its own.
    #[cfg(not(target_os = "macos"))]
    fn has_native_menu(&self) -> bool {
        false
    }

    /// Branding strip above the menu bar.
    fn title_bar(&mut self, ui: &mut egui::Ui, show: &Show) {
        use egui::text::{LayoutJob, TextFormat};
        let custom = custom_window_frame();
        let mac = mac_title_bar();
        let h = if custom { CAPTION_H } else if mac { MAC_TITLE_H } else { 30.0 };
        let sense = if custom || mac { egui::Sense::click_and_drag() } else { egui::Sense::hover() };
        let (rect, bar) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), sense);
        // macOS draws its window buttons over the left end of the bar (hidden in fullscreen).
        let fullscreen = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
        let logo_x = rect.left() + if mac && !fullscreen { MAC_BUTTONS_W } else { 0.0 };
        let mut job = LayoutJob::default();
        job.append("Open", 0.0, TextFormat::simple(theme::title(26.0), theme::TEXT));
        job.append("Q", 0.0, TextFormat::simple(theme::title(26.0), theme::GREEN));
        let logo = ui.painter().layout_job(job);
        ui.painter().galley(egui::pos2(logo_x, rect.center().y - logo.size().y / 2.0), logo, theme::TEXT);
        // Workspace name, centred in the window.
        let name = show
            .path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| t("Untitled").into());
        let (name, dirty) = self.remote.title().unwrap_or((name, show.dirty));
        let label = format!("{name}{}", if dirty { "  •" } else { "" });
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(14.0), theme::TEXT_DIM);
        if custom || mac {
            // The bar moves the window; a double-click maximises or restores it.
            let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
            if bar.double_clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            } else if bar.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if custom {
                self.caption_buttons(ui, rect, maximized);
            }
        }
    }

    /// Windows-style minimise / maximise / close buttons at the right end of the title bar.
    fn caption_buttons(&mut self, ui: &mut egui::Ui, bar: egui::Rect, maximized: bool) {
        use egui::{Rect, Sense, pos2, vec2};
        let w = 46.0;
        let tips = [t("Minimise"), if maximized { t("Restore") } else { t("Maximise") }, t("Close")];
        for (k, tip) in tips.into_iter().enumerate() {
            let close = k == 2;
            let r = Rect::from_min_size(pos2(bar.right() - w * (3 - k) as f32, bar.top()), vec2(w, bar.height()));
            let resp = ui.interact(r, ui.id().with(("caption", k)), Sense::click()).on_hover_text(tip);
            let (fill, fg) = match (resp.hovered(), close) {
                (true, true) => (Color32::from_rgb(0xc4, 0x2b, 0x1c), Color32::WHITE),
                (true, false) => (theme::BG_HOVER, Color32::WHITE),
                _ => (Color32::TRANSPARENT, theme::TEXT_DIM),
            };
            let p = ui.painter();
            p.rect_filled(r, 0.0, fill);
            // Thin 10 pt glyphs drawn like the system's own caption buttons.
            let c = r.center();
            let st = egui::Stroke::new(1.0, fg);
            let g = Rect::from_center_size(c, vec2(10.0, 10.0));
            match k {
                0 => {
                    p.line_segment([pos2(g.left(), c.y), pos2(g.right(), c.y)], st);
                }
                1 if maximized => {
                    let back = g.translate(vec2(2.0, -2.0)).shrink(1.0);
                    p.line_segment([back.left_top(), back.right_top()], st);
                    p.line_segment([back.right_top(), back.right_bottom()], st);
                    p.rect_stroke(g.translate(vec2(-1.0, 1.0)).shrink(1.0), 1.0, st, egui::StrokeKind::Middle);
                }
                1 => {
                    p.rect_stroke(g, 1.0, st, egui::StrokeKind::Middle);
                }
                _ => {
                    p.line_segment([g.left_top(), g.right_bottom()], st);
                    p.line_segment([g.right_top(), g.left_bottom()], st);
                }
            }
            if resp.clicked() {
                let cmd = match k {
                    0 => egui::ViewportCommand::Minimized(true),
                    1 => egui::ViewportCommand::Maximized(!maximized),
                    _ => egui::ViewportCommand::Close,
                };
                ui.ctx().send_viewport_cmd(cmd);
            }
        }
    }

    /// Without a system frame the window edges don't resize it, so thin zones along them do.
    fn resize_edges(&self, ui: &mut egui::Ui) {
        use egui::{CursorIcon as C, Rect, ResizeDirection as D, Sense, pos2, vec2};
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false)) {
            return;
        }
        let s = ctx.content_rect();
        let (t, c) = (5.0, 12.0); // edge thickness, corner size
        let zones = [
            (Rect::from_min_size(s.left_top(), vec2(c, c)), D::NorthWest, C::ResizeNorthWest),
            (Rect::from_min_size(pos2(s.right() - c, s.top()), vec2(c, c)), D::NorthEast, C::ResizeNorthEast),
            (Rect::from_min_size(pos2(s.left(), s.bottom() - c), vec2(c, c)), D::SouthWest, C::ResizeSouthWest),
            (Rect::from_min_size(s.right_bottom() - vec2(c, c), vec2(c, c)), D::SouthEast, C::ResizeSouthEast),
            (Rect::from_min_size(pos2(s.left() + c, s.top()), vec2(s.width() - 2.0 * c, t)), D::North, C::ResizeVertical),
            (Rect::from_min_size(pos2(s.left() + c, s.bottom() - t), vec2(s.width() - 2.0 * c, t)), D::South, C::ResizeVertical),
            (Rect::from_min_size(pos2(s.left(), s.top() + c), vec2(t, s.height() - 2.0 * c)), D::West, C::ResizeHorizontal),
            (Rect::from_min_size(pos2(s.right() - t, s.top() + c), vec2(t, s.height() - 2.0 * c)), D::East, C::ResizeHorizontal),
        ];
        for (k, (r, dir, cursor)) in zones.into_iter().enumerate() {
            let resp = ui.interact(r, egui::Id::new(("window-resize", k)), Sense::drag());
            if resp.hovered() || resp.dragged() {
                ctx.set_cursor_icon(cursor);
            }
            if resp.drag_started() {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
            }
        }
    }

    /// Output level meter: one slim bar per device channel (up to 8), no scale or labels. The
    /// green/amber/red zones show faintly at rest so the meter reads even when nothing plays.
    fn meter(&mut self, ui: &mut egui::Ui, show: &Show) {
        use egui::{Align2, CornerRadius, Rect, Sense, pos2, vec2};
        let n = self.meters.len().clamp(1, 8);
        let bar = 4.0;
        let gap = 3.0;
        let pad = 7.0;
        let w = 2.0 * pad + n as f32 * bar + (n - 1) as f32 * gap;
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 92.0), Sense::hover());
        let p = ui.painter();
        p.rect(rect, CornerRadius::same(6), theme::BG_INPUT, egui::Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
        let inner = rect.shrink2(vec2(pad, 8.0));
        let y_of = |db: f32| {
            inner.bottom() - ((db - crate::model::MIN_DB) / -crate::model::MIN_DB).clamp(0.0, 1.0) * inner.height()
        };
        // Green up to -12 dB, amber to -3 dB, red above.
        let zones = [(crate::model::MIN_DB, -12.0, theme::GREEN), (-12.0, -3.0, theme::AMBER), (-3.0, 0.0, theme::RED)];
        let audio_ok = show.media.audio.is_some();
        for i in 0..n {
            let x = inner.left() + i as f32 * (bar + gap);
            let db = crate::model::gain_to_db(self.meters.get(i).copied().unwrap_or(0.0));
            for (k, (lo, hi, c)) in zones.into_iter().enumerate() {
                // Round only the ends of the whole bar.
                let r = |top: bool| if (top && k == 2) || (!top && k == 0) { 2 } else { 0 };
                let radius = CornerRadius { nw: r(true), ne: r(true), sw: r(false), se: r(false) };
                let zone = Rect::from_min_max(pos2(x, y_of(hi)), pos2(x + bar, y_of(lo)));
                p.rect_filled(zone, radius, c.gamma_multiply(if audio_ok { 0.32 } else { 0.12 }));
                if db > lo {
                    let lit = Rect::from_min_max(pos2(x, y_of(db.min(hi))), zone.max);
                    p.rect_filled(lit, radius, c);
                }
            }
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
            Some(a) => tr!("Output: {device} · {rate} Hz · {n} ch", device = a.device_name, rate = a.rate, n = a.channels),
            None => t("No audio output").into(),
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
            ui.label(theme::caption(t("Active cues")));
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
                ui.label(RichText::new(t("Nothing running")).color(theme::TEXT_FAINT));
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
                            tr!("pre-wait −{time}", time = fmt_time(inst.pre_wait - e)),
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
                                        if theme::icon_button(ui, icons::STOP, t("Stop this cue"))
                                            .clicked()
                                        {
                                            self.control(show, Command::Cue(id, CueAction::Stop));
                                        }
                                        let (g, tip) = if paused {
                                            (icons::PLAY_ARROW, t("Resume"))
                                        } else {
                                            (icons::PAUSE, t("Pause"))
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
            ui.label(theme::caption(t("Engine log")));
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

    /// Bottom bar: the Edit/Show toggle, show status, the engine log's last line and Settings.
    fn status_bar(&mut self, ui: &mut egui::Ui, show: &Show) {
        let item = |ui: &mut egui::Ui, glyph: &str, text: String, color: Color32| {
            ui.label(theme::icon(glyph, 15.0).color(color));
            ui.label(RichText::new(text).small().color(theme::TEXT_DIM));
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            let (toggle, _) = ui.allocate_exact_size(egui::vec2(148.0, 24.0), egui::Sense::hover());
            self.mode_toggle(ui, toggle);
            ui.add_space(12.0);
            let file = if let Some((t, _)) = self.remote.title() {
                t
            } else {
                show
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| t("Untitled").into())
            };
            let replica = self.remote.is_replica();
            item(ui, icons::FOLDER, file, theme::TEXT_DIM);
            if show.dirty && !replica {
                ui.label(RichText::new("●").small().color(AMBER))
                    .on_hover_text(t("Unsaved changes"));
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
                (None, _) if show.media.audio_reconnecting() => {
                    item(ui, icons::SPEAKER, t("audio lost · reconnecting…").into(), AMBER)
                }
                (None, e) => item(
                    ui,
                    icons::SPEAKER,
                    tr!("no audio: {error}", error = e.as_deref().unwrap_or(t("off"))),
                    RED,
                ),
            }
            ui.add_space(10.0);
            match &show.media.video_error {
                _ if replica => {}
                None => item(ui, icons::VIDEOCAM, t("video ready").into(), theme::TEXT_DIM),
                Some(e) => item(ui, icons::VIDEOCAM, tr!("video: {e}", e), AMBER),
            }
            ui.add_space(10.0);
            match (&self.osc, &self.osc_error) {
                (Some(o), _) => item(
                    ui,
                    icons::SETTINGS_REMOTE,
                    format!("OSC :{}", o.port),
                    theme::GREEN,
                ),
                (None, Some(_)) => item(ui, icons::SETTINGS_REMOTE, t("OSC error").into(), RED),
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
                tr!("{n} cues", n = show.ws.cue_list.iter_all().len()),
                theme::TEXT_DIM,
            );
            if let Some((msg, t)) = &self.status {
                if t.elapsed() < STATUS_TIMEOUT {
                    ui.ctx().request_repaint_after(STATUS_TIMEOUT.saturating_sub(t.elapsed()));
                    ui.add_space(10.0);
                    // At most 60 % of what's left, so the engine warning on the right keeps some room.
                    let w = ui.available_width() * 0.6;
                    ui.scope(|ui| {
                        ui.set_max_width(w);
                        ui.add(egui::Label::new(RichText::new(msg).small().color(AMBER)).truncate()).on_hover_text(msg);
                    });
                } else {
                    self.status = None;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let open = self.show_settings;
                let r = ui
                    .add(
                        egui::Button::new(theme::icon(icons::SETTINGS, 18.0).color(if open { theme::BLUE } else { theme::TEXT_DIM }))
                            .frame(false)
                            .min_size(egui::vec2(26.0, 24.0)),
                    )
                    .on_hover_text(t("Settings"));
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.show_settings = !open;
                }
                let log = &show.engine.log;
                if let Some(l) = log.last()
                    && (log.len(), l) != (self.log_tail.0, &self.log_tail.1)
                {
                    self.log_tail = (log.len(), l.clone(), Instant::now());
                }
                if let Some(l) = log.last().filter(|_| self.log_tail.2.elapsed() < STATUS_TIMEOUT) {
                    ui.ctx().request_repaint_after(STATUS_TIMEOUT.saturating_sub(self.log_tail.2.elapsed()));
                    ui.add_space(8.0);
                    // Truncated to the space left, so a long entry never runs over the items on the left.
                    let text = RichText::new(theme::with_icon(icons::WARNING, l)).color(AMBER).small();
                    let r = ui.add(egui::Label::new(text).truncate().sense(egui::Sense::click()));
                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(l).clicked() {
                        self.show_log = true;
                    }
                }
            });
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        let mut open = self.show_settings;
        egui::Window::new(t("Settings"))
            .id(egui::Id::new("settings-window"))
            .open(&mut open)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    theme::tab(ui, &mut self.settings_tab, SettingsTab::General, icons::SETTINGS, t("General"));
                    theme::tab(ui, &mut self.settings_tab, SettingsTab::Audio, icons::SPEAKER, t("Audio"));
                    theme::tab(ui, &mut self.settings_tab, SettingsTab::Stages, icons::DESKTOP_WINDOWS, t("Video stages"));
                    theme::tab(ui, &mut self.settings_tab, SettingsTab::Surfaces, icons::PIANO, t("Control surfaces"));
                });
                ui.separator();
                match self.settings_tab {
                    SettingsTab::General => self.settings_general(ui, show),
                    SettingsTab::Audio => self.settings_audio(ui, show),
                    SettingsTab::Stages => self.settings_stages(ui, show),
                    SettingsTab::Surfaces => self.settings_surfaces(ui, show),
                }
            });
        self.show_settings = open;
    }

    fn settings_general(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        let before = show.ws.settings.clone();
        egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
            ui.label(theme::with_icon(icons::LANGUAGE, t("Language")));
            self.language_setting(ui);
            ui.end_row();
            ui.label(t("Stop-all fade time"));
            widgets::time_edit(ui, &mut show.ws.settings.panic_fade);
            ui.end_row();
            ui.label(t("Double-GO protection"));
            widgets::time_edit(ui, &mut show.ws.settings.double_go_protection);
            ui.end_row();
            ui.label(t("Master volume"));
            widgets::db_edit(ui, &mut show.ws.settings.master_volume_db);
            ui.end_row();
            ui.label(t("OSC remote control"));
            ui.horizontal(|ui| {
                ui.checkbox(&mut show.ws.settings.osc_enabled, t("Listen on UDP port"));
                ui.add(egui::DragValue::new(&mut show.ws.settings.osc_port).range(1024..=65535));
            });
            ui.end_row();
            ui.label("");
            match (&self.osc, &self.osc_error) {
                (Some(o), _) => ui.label(
                    RichText::new(tr!("listening on {port} · /go /stop /pause /resume /cue/<n>/start …", port = o.port))
                    .weak(),
                ),
                (None, Some(e)) => ui.label(RichText::new(e).color(RED)),
                _ => ui.label(RichText::new(t("off")).weak()),
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
            ui.label(RichText::new(t("Audio output is set up on the main device.")).weak());
            return;
        }
        if self.devices.is_none() {
            self.devices = Some(media::audio::list_output_devices());
        }
        let devices = self.devices.clone().unwrap_or_default();
        let current = show.ws.settings.audio_device.clone();
        let label = |id: &Option<String>| match id {
            None => t("System default").to_string(),
            Some(id) => devices
                .iter()
                .find(|d| &d.id == id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| tr!("{id} (missing)", id)),
        };
        let mut chosen = current.clone();
        egui::Grid::new("audio_settings")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label(t("Output device"));
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("device")
                        .width(320.0)
                        .selected_text(label(&chosen))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut chosen, None, t("System default"));
                            for d in &devices {
                                ui.selectable_value(&mut chosen, Some(d.id.clone()), &d.name)
                                    .on_hover_text(&d.id);
                            }
                        });
                    if theme::icon_button(ui, icons::REFRESH, t("Rescan devices")).clicked() {
                        self.devices = None;
                    }
                });
                ui.end_row();
                ui.label(t("Active"));
                match &show.media.audio {
                    Some(a) => ui.label(tr!("{device} · {rate} Hz · {n} outputs", device = a.device_name, rate = a.rate, n = a.channels)),
                    None => ui.label(
                        RichText::new(show.media.audio_error.clone().unwrap_or_default())
                            .color(RED),
                    ),
                };
                ui.end_row();
            });
        ui.label(RichText::new(t("Changing the device stops all playback. Cue routing matrices address the device's outputs.")).weak());
        ui.separator();
        let channels = show.media.audio.as_ref().map_or(2, |a| a.channels);
        let before = show.ws.settings.speakers.clone();
        room::speaker_editor(ui, &mut show.ws.settings.speakers, channels);
        if show.ws.settings.speakers != before {
            show.edited();
        }
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
                self.notify_error(tr!("Audio device failed: {e}", e));
            }
        }
    }

    fn settings_stages(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        let before = show.ws.stages.clone();
        let mut remove = None;
        let n = show.ws.stages.len();
        // Surfaces another stage can span: stages that don't span anything themselves.
        let surfaces: Vec<(StageId, String)> =
            show.ws.stages.iter().filter(|s| s.span_of.is_none()).map(|s| (s.id, s.name.clone())).collect();
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
                    if n > 1 && theme::icon_button(ui, icons::DELETE, t("Delete stage")).clicked() {
                        remove = Some(i);
                    }
                });
                ui.horizontal(|ui| {
                    let mut open = self.stage_open.contains(&st.id);
                    if ui
                        .toggle_value(
                            &mut open,
                            theme::with_icon(icons::DESKTOP_WINDOWS, t("Window")),
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
                        .button(theme::with_icon(icons::FULLSCREEN, t("Fullscreen")))
                        .on_hover_text(t("Also: double-click or F11 in the stage window"))
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
                            theme::with_icon(icons::CROP_FREE, t("Edit mapping")),
                        )
                        .on_hover_text(t("Shows a test pattern and corner pins in the stage window"))
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
                        .button(theme::with_icon(icons::REPLAY, t("Reset mapping")))
                        .clicked()
                    {
                        st.corners = Stage::IDENTITY;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(t("Output"));
                    let label = st.output.clone().unwrap_or_else(|| t("Window").into());
                    egui::ComboBox::from_id_salt(("stage-output", st.id))
                        .selected_text(label)
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            self.output_choices(ui, st);
                        });
                });
                stage_span_blend(ui, st, &surfaces);
                if st.corners != Stage::IDENTITY {
                    ui.label(
                        RichText::new(tr!(
                            "corners: {list}",
                            list =
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
            for s in &mut show.ws.stages {
                if s.span_of == Some(id) {
                    s.span_of = None;
                }
            }
            self.stage_open.remove(&id);
            self.mapping_edit.remove(&id);
        }
        if ui
            .button(theme::with_icon(icons::ADD, t("Add stage")))
            .clicked()
        {
            let st = Stage::new(&tr!("Stage {n}", n = show.ws.stages.len() + 1));
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
                .set_title(t("Unsaved changes"))
                .set_description(t("Discard unsaved changes to this workspace?"))
                .set_buttons(rfd::MessageButtons::YesNo)
                .show(),
            rfd::MessageDialogResult::Yes
        )
    }

    fn save_to(&mut self, path: Option<PathBuf>) {
        let path = match path.or_else(|| self.show.lock().path.clone()) {
            Some(p) => p,
            None => match rfd::FileDialog::new()
                .add_filter(t("OpenQ workspace"), &[Workspace::EXTENSION])
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
                self.notify(tr!("Saved {path}", path = path.display()));
            }
            Err(e) => self.notify_error(tr!("Save failed: {e}", e = format!("{e:#}"))),
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
                self.notify(tr!("Opened {path}", path = path.display()));
            }
            Err(e) => self.notify_error(tr!("Open failed: {e}", e = format!("{e:#}"))),
        }
    }

    fn run_deferred(&mut self, ctx: &egui::Context) {
        for d in std::mem::take(&mut self.deferred) {
            if self.remote.is_replica() {
                let why = match d {
                    Deferred::New | Deferred::Open | Deferred::Recover(_) => Some(t("Disconnect from the main device first (Remote → Disconnect)")),
                    Deferred::PickMedia(_) | Deferred::AddMediaCues => Some(t("Media files live on the main device: add them there")),
                    _ => None,
                };
                if let Some(why) = why {
                    self.notify(why);
                    continue;
                }
            }
            match d {
                Deferred::ImportFixture => self.import_fixture(ctx),
                Deferred::ExportFixture(ty) => self.export_fixture(*ty),
                Deferred::ImportEffects => self.import_effects(),
                Deferred::ExportEffect(id) => self.export_effect(id),
                Deferred::ExportPatch(plot) => self.export_patch(plot),
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
                            .add_filter(t("OpenQ workspace"), &[Workspace::EXTENSION])
                            .pick_file()
                    {
                        self.open_path(p);
                    }
                }
                Deferred::Recover(o) => self.recover(o),
                Deferred::Save => self.save_to(None),
                Deferred::SaveAs => {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter(t("OpenQ workspace"), &[Workspace::EXTENSION])
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
                            t("Video and images"),
                            &[
                                "mp4", "mov", "mkv", "avi", "webm", "m4v", "mpg", "mpeg", "ogv",
                                "mxf", "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "svg", "svgz",
                            ],
                        )
                    } else {
                        dlg.add_filter(
                            t("Audio"),
                            &[
                                "wav", "aif", "aiff", "flac", "mp3", "ogg", "opus", "m4a", "aac",
                                "caf",
                            ],
                        )
                    };
                    if let Some(p) = dlg.add_filter(t("All files"), &["*"]).pick_file() {
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
                            t("Media"),
                            &[
                                "wav", "aif", "aiff", "flac", "mp3", "ogg", "opus", "m4a", "aac",
                                "caf", "mp4", "mov", "mkv", "avi", "webm", "m4v", "png", "jpg", "jpeg",
                                "webp", "svg", "svgz",
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
    /// `OPENQ_SCREENSHOT_CUE` / `OPENQ_SCREENSHOT_TAB` pick the cue and inspector tab shown
    /// (`OPENQ_SCREENSHOT_ANIM_TIME` sets the Animation tab playhead);
    /// `OPENQ_SCREENSHOT_LIGHTING=<page>` opens the Lighting workspace on a page (`colour`: the
    /// programmer's Colour encoders; `plan`: the patch's top view with four fixtures selected;
    /// `types`: the fixture type editor on a type with ranges; `beam`: the Beam encoders with
    /// range buttons for every unit with a gobo);
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
            // Optional: select cue N and an inspector tab (time|levels|eq|geometry|animation|settings).
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
            // Optional: the Animation tab playhead, in seconds.
            if let Some(t) = std::env::var("OPENQ_SCREENSHOT_ANIM_TIME").ok().and_then(|v| v.parse().ok()) {
                // Select the first track's second key, so the Keyframe card has something to show.
                let sel = self.primary().and_then(|id| show.ws.cue_list.find(id)).and_then(|c| match &c.kind {
                    CueKind::Video(v) => v.animation.tracks.first().map(|t| t.prop),
                    CueKind::Text(t) => t.animation.tracks.first().map(|t| t.prop),
                    _ => None,
                });
                self.anim_edit = animation::AnimEdit { cue: self.primary(), time: t, sel: sel.map(|p| (p, 1)) };
            }
            self.inspector_tab = match std::env::var("OPENQ_SCREENSHOT_TAB").as_deref() {
                Ok("time") => inspector::Tab::Time,
                Ok("io") => inspector::Tab::Io,
                Ok("levels") => inspector::Tab::Levels,
                Ok("eq") => inspector::Tab::Eq,
                Ok("geometry") => inspector::Tab::Geometry,
                Ok("animation") => inspector::Tab::Animation,
                Ok("settings") => inspector::Tab::Settings,
                Ok("triggers") => inspector::Tab::Triggers,
                Ok("audiofx") => inspector::Tab::AudioFx,
                Ok("videofx") => inspector::Tab::VideoFx,
                _ => inspector::Tab::Basics,
            };
            if std::env::var("OPENQ_SCREENSHOT_MODE").as_deref() == Ok("show") {
                self.mode = Mode::Show;
            }
            if let Ok(page) = std::env::var("OPENQ_SCREENSHOT_LIGHTING") {
                self.view = lighting::View::Lighting;
                self.light.tab = lighting::LightTab::from_name(&page);
                if page == "colour" {
                    self.light.feature = Some(crate::light::Feature::Colour);
                }
                self.light.keypad = page == "keypad";
                if page == "effects" {
                    self.light.effect = show.ws.lighting.effects.first().map(|e| e.id);
                }
                if page == "plan" {
                    self.light.patch_view = lighting::PatchView::Plan;
                    self.light.patch_sel = show.ws.lighting.fixtures.iter().take(4).map(|f| f.id).collect();
                    self.light.shape_sel = show.ws.lighting.shapes.first().map(|s| s.id);
                }
                if page == "types"
                    && let Some(ty) = show.ws.lighting.fixture_types.iter().find(|t| t.modes.iter().any(|m| m.all_channels().any(|c| !c.ranges.is_empty())))
                {
                    self.light.patch_view = lighting::PatchView::Types;
                    self.light.types.edit = Some(fixture_types::TypeEdit::open_ranges(ty.clone()));
                }
                // A selection and some programmer values to look at.
                if page == "beam" {
                    use crate::light::Attribute;
                    let l = &show.ws.lighting;
                    self.light.feature = Some(crate::light::Feature::Beam);
                    self.light.selection = l.units().into_iter().filter(|u| l.unit_attributes(*u).contains(&Attribute::Gobo)).collect();
                    for f in &self.light.selection {
                        let v = self.light.programmer.entry(*f).or_default();
                        v.insert(Attribute::Dimmer, 1.0.into());
                        let chans = l.unit_channels(*f).unwrap_or_default();
                        for a in [Attribute::Gobo, Attribute::Shutter] {
                            if let Some(r) = chans.iter().find(|c| c.attribute == a).and_then(|c| c.ranges.get(1)) {
                                v.insert(a, r.value().into());
                            }
                        }
                    }
                } else if let Some(g) = show.ws.lighting.groups.first() {
                    self.light.selection = g.fixtures.clone();
                    for f in g.fixtures.iter().take(4) {
                        let v = self.light.programmer.entry(*f).or_default();
                        v.insert(crate::light::Attribute::Dimmer, 0.75.into());
                        v.insert(crate::light::Attribute::Red, 1.0.into());
                        v.insert(crate::light::Attribute::Green, 0.45.into());
                        v.insert(crate::light::Attribute::Blue, 0.1.into());
                    }
                }
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
            if std::env::var_os("OPENQ_SCREENSHOT_LANGUAGE").is_some() {
                self.ask_language = true;
            }
            if let Ok(t) = std::env::var("OPENQ_SCREENSHOT_SETTINGS") {
                self.show_settings = true;
                self.settings_tab = match t.as_str() {
                    "audio" => SettingsTab::Audio,
                    "general" => SettingsTab::General,
                    "surfaces" => SettingsTab::Surfaces,
                    _ => SettingsTab::Stages,
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

/// UI texts that reach [`crate::i18n::t`] through a name rather than a literal at the call site
/// (inspector tab names, effect parameter names and units), for the translation test.
#[cfg(test)]
pub(crate) fn translated_names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = inspector::tab_names();
    for k in crate::effects::AudioFxKind::ALL {
        v.extend(k.params().iter().flat_map(|p| [p.name, p.unit]));
    }
    for k in crate::effects::VideoFxKind::ALL {
        v.extend(k.params().iter().flat_map(|p| [p.name, p.unit]));
    }
    v.retain(|s| s.chars().any(|c| c.is_alphabetic()));
    v
}

/// True when OpenQ draws its own window frame: the system title bar is hidden and the OpenQ title
/// bar carries minimise, maximise and close (always on Windows; `OPENQ_CUSTOM_FRAME=1` elsewhere,
/// for testing).
pub fn custom_window_frame() -> bool {
    cfg!(target_os = "windows") || std::env::var_os("OPENQ_CUSTOM_FRAME").is_some()
}

/// The app icon (`assets/icon/openq-256.png`) for the main and stage windows: taskbar, Alt-Tab and,
/// while OpenQ runs, the macOS Dock. Installed apps also carry it in the `.exe` (`build.rs`), the
/// macOS bundle and the Linux icon theme (oqman).
pub fn app_icon() -> std::sync::Arc<egui::IconData> {
    static ICON: std::sync::OnceLock<std::sync::Arc<egui::IconData>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| {
        std::sync::Arc::new(eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon/openq-256.png")).expect("bad app icon"))
    })
    .clone()
}

/// Height of the title bar in a custom window frame (the caption buttons fill it).
const CAPTION_H: f32 = 34.0;

/// True on macOS, where the system title bar is transparent and OpenQ's title bar takes its place:
/// the window content starts at the top edge and macOS draws its close / minimise / zoom buttons
/// over the left end of OpenQ's bar (`OPENQ_MAC_TITLEBAR=1` shows the layout elsewhere, for testing).
pub fn mac_title_bar() -> bool {
    cfg!(target_os = "macos") || std::env::var_os("OPENQ_MAC_TITLEBAR").is_some()
}

/// Height of OpenQ's title bar on macOS: taller than the system's 28 pt so the title has room;
/// `native_menu::place_window_buttons` keeps the window buttons centred in it.
const MAC_TITLE_H: f32 = 40.0;

/// Room kept free for the macOS window buttons at the left of the title bar.
const MAC_BUTTONS_W: f32 = 64.0;

/// Fonts and visuals; call once at startup.
pub fn install_theme(ctx: &egui::Context) {
    theme::install(ctx);
}

/// Stage settings for splitting a surface across projectors: which stage this one shows part
/// of, the part (crop) and the soft-edge blend zones.
fn stage_span_blend(ui: &mut egui::Ui, st: &mut Stage, surfaces: &[(StageId, String)]) {
    let pct = |v: f64, _| format!("{:.1}%", v * 100.0);
    let parse = |t: &str| t.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|v| v / 100.0);
    egui::CollapsingHeader::new(t("Span & edge blend")).id_salt(("span", st.id)).default_open(st.span_of.is_some() || st.blend != [0.0; 4]).show(ui, |ui| {
        egui::Grid::new(("span-grid", st.id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(t("Shows"));
            let shown = st.span_of.and_then(|id| surfaces.iter().find(|s| s.0 == id)).map(|s| tr!("Part of {name}", name = s.1));
            egui::ComboBox::from_id_salt(("span-of", st.id))
                .selected_text(shown.unwrap_or_else(|| t("Its own cues").into()))
                .width(220.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut st.span_of, None, t("Its own cues"));
                    for (id, name) in surfaces.iter().filter(|s| s.0 != st.id) {
                        ui.selectable_value(&mut st.span_of, Some(*id), tr!("Part of {name}", name));
                    }
                })
                .response
                .on_hover_text(t("Split one wide stage (a surface) across several outputs: each shows a part of it"));
            ui.end_row();
            if st.span_of.is_some() {
                ui.label(t("Part"));
                ui.horizontal(|ui| {
                    for (i, side) in [t("Left"), t("Top"), t("Right"), t("Bottom")].into_iter().enumerate() {
                        ui.label(side);
                        // x0/y0 stay below x1/y1.
                        let range = if i < 2 { 0.0..=(st.crop[i + 2] - 0.01).max(0.0) } else { (st.crop[i - 2] + 0.01).min(1.0)..=1.0 };
                        ui.add(egui::DragValue::new(&mut st.crop[i]).speed(0.001).range(range).custom_formatter(pct).custom_parser(parse));
                    }
                })
                .response
                .on_hover_text(t("Edges of the part this output shows, in % of the surface. Neighbouring outputs overlap by their blend width."));
                ui.end_row();
            }
            ui.label(t("Blend"));
            ui.horizontal(|ui| {
                for (i, side) in ["L", "T", "R", "B"].into_iter().enumerate() {
                    ui.label(side);
                    let max = (1.0 - st.blend[(i + 2) % 4]).clamp(0.0, 0.5);
                    ui.add(egui::DragValue::new(&mut st.blend[i]).speed(0.001).range(0.0..=max).custom_formatter(pct).custom_parser(parse));
                }
            })
            .response
            .on_hover_text(t("Soft-edge widths in % of this output: set them to the overlap with the neighbouring projector"));
            ui.end_row();
            ui.label(t("Gamma"));
            ui.add(egui::DragValue::new(&mut st.blend_gamma).speed(0.01).range(1.0..=3.0).max_decimals(2))
                .on_hover_text(t("Projector gamma: raise it if the overlap looks too bright, lower it if it looks dark"));
            ui.end_row();
        });
    });
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        self.show.lock().shutdown();
        // A normal quit (unsaved changes were confirmed away): nothing to recover next time.
        if let Some(a) = &self.recovery.autosave {
            a.lock().clear();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        #[cfg(feature = "video")]
        if self.gpu.is_none() {
            self.gpu = frame.wgpu_render_state().cloned();
        }
        self.draw(ui, frame.info().cpu_usage);
        #[cfg(target_os = "macos")]
        native_menu::place_window_buttons(frame, MAC_TITLE_H as f64);
    }
}

impl App {
    /// One frame of the whole window (`cpu`: the previous frame's CPU time, for the debug log).
    /// Separate from [`eframe::App::ui`] so tests can run it headless.
    fn draw(&mut self, ui: &mut egui::Ui, cpu: Option<f32>) {
        let frame_start = Instant::now();
        let ctx = ui.ctx().clone();
        media::set_max_texture_side(ctx.input(|i| i.max_texture_side));
        let show_arc = self.show.clone();
        #[cfg(feature = "video")]
        let stage_input: Option<(Vec<crate::model::Stage>, Vec<media::video::StageLayer>)>;
        {
            let mut guard = show_arc.lock();

            let show: &mut Show = &mut guard;
            self.remote_tick(&ctx, show);
            self.sync_playhead(show);
            self.handle_keys(&ctx, show);
            self.handle_dropped_files(&ctx, show);
            self.debug_screenshot(&ctx, show);
            self.debug_script(&ctx, show);
            if let Some(msg) = show.recovered.take() {
                self.notify(msg);
                self.recovery.save_soon();
            }
            let reconnecting = show.media.audio_reconnecting();
            if reconnecting != self.audio_reconnecting {
                self.audio_reconnecting = reconnecting;
                self.notify(if reconnecting {
                    t("Audio output lost — its sounds stopped. Reconnecting…").to_string()
                } else {
                    t("Audio output reconnected").to_string()
                });
            }

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
            // In a custom frame the caption buttons sit flush with the window's top-right corner.
            let title_margin =
                if custom_window_frame() || mac_title_bar() { egui::Margin { left: 14, right: 0, top: 0, bottom: 0 } } else { egui::Margin::symmetric(14, 6) };
            egui::Panel::top("titlebar")
                .frame(egui::Frame::new().fill(Color32::from_rgb(0x0e, 0x0f, 0x12)).inner_margin(title_margin))
                .show_separator_line(false)
                .show(ui, |ui| self.title_bar(ui, show));
            #[cfg(target_os = "macos")]
            self.native_menu_frame(&ctx, show);
            if !self.has_native_menu() {
                egui::Panel::top("menu")
                    .frame(bar(theme::BG_APP))
                    .show(ui, |ui| self.menu_bar(ui, show));
            }
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
            // Let a waiting engine tick in between panels: the engine never waits for a whole frame.
            parking_lot::MutexGuard::bump(&mut guard);
            let show: &mut Show = &mut guard;
            if self.mode == Mode::Edit && self.view == lighting::View::Cues {
                // Tall enough for every tab's cards without scrolling. Until the user drags the edge,
                // forget the stored height so a tiny first frame (or a window resize) can't shrink it.
                let id = egui::Id::new("inspector");
                if !self.inspector_resized {
                    if ctx.read_response(id.with("__resize")).is_some_and(|r| r.dragged()) {
                        self.inspector_resized = true;
                    } else {
                        ctx.data_mut(|d| d.remove::<egui::containers::panel::PanelState>(id));
                    }
                }
                egui::Panel::bottom(id)
                    .frame(egui::Frame::new().fill(theme::BG_APP).inner_margin(egui::Margin::symmetric(10, 6)))
                    .resizable(true)
                    .default_size(INSPECTOR_HEIGHT)
                    .min_size(120.0)
                    .show(ui, |ui| self.inspector(ui, show));
            }
            // Let a waiting engine tick in between panels: the engine never waits for a whole frame.
            parking_lot::MutexGuard::bump(&mut guard);
            let show: &mut Show = &mut guard;
            egui::Panel::right("active")
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG_PANEL)
                        .inner_margin(egui::Margin::same(10)),
                )
                .resizable(true)
                .default_size(280.0)
                // Narrow in the Lighting workspace, which needs the width for its own panels.
                .max_size(if self.view == lighting::View::Lighting { 220.0 } else { f32::INFINITY })
                .show(ui, |ui| self.active_panel(ui, show));
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG_APP)
                        .inner_margin(egui::Margin::symmetric(10, 8)),
                )
                .show(ui, |ui| {
                    self.view_tabs(ui);
                    if self.view == lighting::View::Lighting {
                        self.lighting_view(ui, show);
                        return;
                    }
                    if show.ws.cue_list.list_count() > 1 || self.mode == Mode::Edit {
                        self.list_tabs(ui, show);
                    }
                    if self.mode == Mode::Edit {
                        self.toolbar(ui, show);
                        ui.separator();
                    }
                    self.cue_list(ui, show);
                });

            // Let a waiting engine tick in between panels: the engine never waits for a whole frame.
            parking_lot::MutexGuard::bump(&mut guard);
            let show: &mut Show = &mut guard;
            if self.show_settings {
                self.settings_window(&ctx, show);
            }
            self.list_timecode_window(&ctx, show);
            if custom_window_frame() {
                self.resize_edges(ui);
            }
            #[cfg(feature = "video")]
            self.geometry_preview(&ctx, show);
            self.remote_window(&ctx, show);
            self.about_window(&ctx);
            // One centred window at a time: the language first, then lost work.
            if self.ask_language {
                self.language_window(&ctx);
            } else {
                self.recovery_window(&ctx);
            }
            self.track_undo(&ctx, show);
            self.sync_osc(&ctx, show);
            self.sync_surfaces(&ctx, show);
            self.lighting_frame(show);
            self.poll_fixture_imports(show);

            // Stage windows are drawn after the lock is released, from this snapshot.
            #[cfg(feature = "video")]
            let video_active = {
                let layers = show.media.stage_layers();
                let active = !layers.is_empty();
                stage_input = Some((show.ws.stages.clone(), layers));
                active
            };
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
                    t("Drop audio / video files to create cues"),
                    egui::FontId::proportional(26.0),
                    Color32::WHITE,
                );
            }

            let name = show
                .path
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| t("Untitled").into());
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
        #[cfg(feature = "video")]
        if let Some((stages, layers)) = stage_input {
            let moved = self.stages(&ctx, stages, layers);
            if !moved.is_empty() {
                let mut show = self.show.lock();
                for (id, corners) in moved {
                    if let Some(s) = show.ws.stages.iter_mut().find(|s| s.id == id) {
                        s.corners = corners;
                    }
                }
                show.edited();
            }
        }
        self.run_deferred(&ctx);
        self.debug_frame_done(frame_start, cpu);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Undo keeps full copies of the cue lists: a big show must keep fewer steps instead of
    /// holding hundreds of megabytes, a small one keeps the full history.
    #[test]
    fn undo_history_has_a_memory_budget() {
        let step = |size| UndoState { cues: Default::default(), stages: Vec::new(), lighting: Default::default(), size };
        let mut small: Vec<UndoState> = (0..UNDO_STEPS + 50).map(|_| step(50)).collect();
        trim_undo(&mut small);
        assert_eq!(small.len(), UNDO_STEPS);
        let mut big: Vec<UndoState> = (0..UNDO_STEPS).map(|_| step(5000)).collect();
        trim_undo(&mut big);
        assert_eq!(big.len(), UNDO_CUE_BUDGET / 5000);
        let mut huge: Vec<UndoState> = (0..UNDO_STEPS).map(|_| step(UNDO_CUE_BUDGET)).collect();
        trim_undo(&mut huge);
        assert_eq!(huge.len(), UNDO_MIN_STEPS, "a few steps stay whatever the size");
    }

    /// The OSC checkbox: on starts the listener, off stops it (it once stayed running, and shown
    /// in the status bar, after being turned off), on again restarts it on the same port.
    #[test]
    fn osc_setting_starts_and_stops_the_listener() {
        let prober = Prober::new();
        let shared: SharedShow = Arc::new(parking_lot::Mutex::new(Show::new(prober.clone())));
        let mut app = App::new(shared.clone(), prober);
        let ctx = egui::Context::default();
        let port = std::net::UdpSocket::bind(("0.0.0.0", 0)).unwrap().local_addr().unwrap().port();
        let set = |app: &mut App, on: bool| {
            let mut s = shared.lock();
            s.ws.settings.osc_port = port;
            s.ws.settings.osc_enabled = on;
            app.sync_osc(&ctx, &s);
        };
        set(&mut app, true);
        assert_eq!(app.osc.as_ref().map(|o| o.port), Some(port), "{:?}", app.osc_error);
        set(&mut app, false);
        assert!(app.osc.is_none() && app.osc_error.is_none(), "OSC still listening after being turned off");
        std::net::UdpSocket::bind(("0.0.0.0", port)).expect("port still taken after OSC was turned off");
        set(&mut app, true);
        assert_eq!(app.osc.as_ref().map(|o| o.port), Some(port), "{:?}", app.osc_error);
    }
}
