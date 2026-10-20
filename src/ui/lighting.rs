//! The Lighting workspace: lighting design (patch, programmer, sequences, outputs) in Edit mode,
//! playback only in Show mode.
//!
//! Every design edit goes through [`App::can_design`], so Show mode (and replicas without the
//! Edit role) can't change the design. The UI never compiles or sends DMX itself: edits mark the
//! show edited and the light thread picks up the recompiled design.

use super::{App, Mode, icons, theme};
use crate::i18n::t;
use crate::light::model::{
    fixture_label, from_stage, to_stage, Attribute, Feature, FixtureId, FixtureType, Group, LightCue, LightCueId, Lighting, Look, PatchIssue, PlotShape, Preset, PresetId, ShapeKind, Sequence, SequenceId, Value,
};
use crate::light::effects::{Effect, EffectId, EffectRun, EffectTable, Shape};
use crate::light::formula;
use crate::light::Fixture;
use crate::show::Show;
use egui::{Color32, DragValue, RichText, Ui};
use std::collections::HashSet;

/// Whether this build has the lighting workspace (cargo feature `lighting`).
pub(crate) const ENABLED: bool = cfg!(feature = "lighting");

/// Which half of the main window is shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum View {
    /// The cue lists.
    #[default]
    Cues,
    /// The lighting workspace.
    Lighting,
}

/// Lighting workspace pages.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum LightTab {
    /// Fixtures and addresses.
    #[default]
    Patch,
    /// Select fixtures, set values, record cues.
    Programmer,
    /// Effects pool and editor.
    Effects,
    /// Cue stacks.
    Sequences,
    /// Sequences and playback buttons as cards (touch screens).
    Playback,
    /// DMX outputs.
    Outputs,
    /// DMX monitor.
    Monitor,
}

impl LightTab {
    /// A page by name (`OPENQ_SCREENSHOT_LIGHTING`).
    pub(crate) fn from_name(name: &str) -> Self {
        match name {
            "programmer" | "colour" | "keypad" | "beam" | "slots" | "points" => LightTab::Programmer,
            "patch" | "plan" | "plot" | "types" | "ofl" => LightTab::Patch,
            "effects" => LightTab::Effects,
            "sequences" => LightTab::Sequences,
            "playback" | "executor" => LightTab::Playback,
            "outputs" => LightTab::Outputs,
            "monitor" => LightTab::Monitor,
            _ => LightTab::Patch,
        }
    }
}

/// Patch page views.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum PatchView {
    /// The fixture sheet.
    #[default]
    Sheet,
    /// Top view.
    Plan,
    /// Fixture types: library, import, editor.
    Types,
}

impl LightUi {
    /// Choose a type in the Add fixtures form.
    pub(crate) fn set_add_type(&mut self, id: &str) {
        self.add.type_id = id.to_string();
        self.add.mode.clear();
    }
}

/// The "add fixtures" form.
#[derive(Clone, Debug)]
struct AddForm {
    type_id: String,
    mode: String,
    count: u32,
    first_id: FixtureId,
    universe: u16,
    /// 0 = next free address.
    address: u16,
}

impl Default for AddForm {
    fn default() -> Self {
        Self { type_id: "generic/drgb".into(), mode: String::new(), count: 1, first_id: 1, universe: 1, address: 0 }
    }
}

/// UI state of the Lighting workspace.
#[derive(Default)]
pub(crate) struct LightUi {
    pub(crate) tab: LightTab,
    /// Selected rows of the patch sheet.
    pub(crate) patch_sel: HashSet<FixtureId>,
    add: AddForm,
    /// Programmer selection, in selection order.
    pub(crate) selection: Vec<FixtureId>,
    /// Fixture tiles: a drag selecting a band of tiles (where it started, in tile-grid
    /// coordinates, and the selection before it).
    band: Option<(egui::Pos2, Vec<FixtureId>)>,
    /// Programmer values (override playback while set; recorded into cues).
    pub(crate) programmer: crate::light::model::Shared<Look>,
    /// Effects running from the programmer (recorded into cues).
    pub(crate) programmer_effects: Vec<EffectRun>,
    /// Effects as last sent to the light thread.
    sent_effects: Option<Vec<(Effect, Vec<FixtureId>)>>,
    /// Show edit generation when they were sent.
    sent_gen: u64,
    /// Effect shown in the editor.
    pub(crate) effect: Option<EffectId>,
    /// Tap tempo: recent taps.
    pub(super) taps: Vec<std::time::Instant>,
    /// Command line text, history and the result of the last command.
    cmd_line: String,
    cmd_history: Vec<String>,
    cmd_back: Option<usize>,
    cmd_result: Option<Result<String, String>>,
    /// Keypad window open.
    pub(crate) keypad: bool,
    /// "Capture input" was pressed: read the DMX inputs into the programmer.
    capture: bool,
    /// Audio envelope of the light thread this frame (shown by audio-reactive effects).
    audio_level: f32,
    /// Effects tempo this frame (the preview runs at it).
    bpm: f32,
    /// Effects page: search text and attribute filter for the effect lists.
    fx_search: String,
    fx_filter: Option<FxFilter>,
    /// Wheel slot pictures on the GPU by (fixture type, image name, stored length); None = not a
    /// picture (decoded once either way).
    gobo_textures: std::collections::HashMap<(String, String, usize), Option<egui::TextureHandle>>,
    /// `Show::light_selection_gen` last taken (a remote device set the selection).
    selection_gen: u64,
    /// The programmer as last sent to the light thread.
    sent: Option<(crate::light::model::Shared<Look>, bool, Option<Vec<FixtureId>>)>,
    /// Blind: the programmer isn't output (still recordable).
    blind: bool,
    /// Highlight: selected fixtures at open white full on the output (not recorded).
    highlight: bool,
    /// Highlight steps through the selection one fixture at a time (index into it).
    step: Option<usize>,
    /// Spread tool settings.
    fan: FanTool,
    /// Open this attribute's range drop-down on the next frame (`OPENQ_SCREENSHOT_LIGHTING=slots`).
    pub(crate) open_slots: Option<Attribute>,
    /// Encoder page.
    pub(crate) feature: Option<Feature>,
    /// Sequence shown on the Sequences page / recorded into.
    sequence: Option<SequenceId>,
    /// Selected cue on the Sequences page.
    cue: Option<LightCueId>,
    /// Number for the next recorded cue (empty = after the last).
    record_number: String,
    /// Universe shown in the DMX monitor.
    monitor_universe: u16,
    /// Art-Net nodes / serial ports found in the background.
    found: std::sync::Arc<parking_lot::Mutex<Found>>,
    /// What the Patch page shows.
    pub(crate) patch_view: PatchView,
    /// Fixture types view (library, editor).
    pub(crate) types: super::fixture_types::TypeUi,
    /// MVR and USITT ASCII files being read.
    pub(crate) design_imports: super::light_files::Imports,
    /// Arrange tool settings.
    arrange: ArrangeTool,
    /// Plan view: pixels per metre (0 = default) and pan in pixels.
    plan_zoom: f32,
    plan_pan: egui::Vec2,
    /// Plan view tool, selected plot shape and the drag in progress.
    plan_tool: PlanTool,
    pub(crate) shape_sel: Option<u32>,
    plan_drag: Option<PlanDrag>,
    /// Plot editor window.
    pub(crate) plot: super::plot_editor::PlotUi,
    /// Where the Plan view drew each shape's label last frame (shape, screen rect): to drag them.
    plan_labels: Vec<(u32, egui::Rect)>,
    /// Formula being edited: (effect, text); written into the effect when run or left.
    formula_buf: Option<(EffectId, String)>,
    /// Formula being worked out for an effect (true: a new effect, which takes the formula's name).
    formula_run: Option<(EffectId, formula::Run, bool)>,
    /// Last run of an effect's formula: (effect, error).
    formula_out: Option<(EffectId, Option<String>)>,
    /// The effect library (opened the first time the Effects page is shown) and its contents.
    fx_lib: Option<crate::light::fxfile::EffectLibrary>,
    fx_snap: std::sync::Arc<crate::light::folder::Snapshot<crate::light::fxfile::LibraryEffect>>,
    /// Results of effect library saves/removes, shown as notifications.
    fx_results: std::sync::Arc<parking_lot::Mutex<Vec<Result<String, String>>>>,
    /// Patch sheet column widths and the texts they were measured for (hash).
    sheet_widths: Option<(u64, [f32; 10])>,
    /// Feature the presets and effects panels show, and "new preset" stores (None = all).
    preset_filter: Option<Feature>,
    /// Playback page and window.
    pub(crate) playback: super::light_playback::PlaybackUi,
    /// Focus point whose settings show under the points pool.
    pub(super) point_sel: Option<crate::light::model::PointId>,
    /// Rig correction: the group that moves (None = the aimed fixtures) and along which axes.
    pub(super) fix_group: Option<crate::light::model::GroupId>,
    pub(super) fix_axes: crate::light::focus::Axes,
    /// Last settings of the Position page's "Spread round the point" row.
    pub(super) point_spread: crate::light::focus::Spread,
}

/// Arrange tool: a layout kind and the settings of each.
#[derive(Clone, Copy)]
struct ArrangeTool {
    /// 0 line, 1 arc, 2 grid.
    kind: u8,
    from: [f32; 3],
    to: [f32; 3],
    centre: [f32; 3],
    radius: f32,
    angles: [f32; 2],
    cols: u32,
    spacing: [f32; 2],
}

impl Default for ArrangeTool {
    fn default() -> Self {
        Self { kind: 0, from: [-4.0, 6.0, 0.0], to: [4.0, 6.0, 0.0], centre: [0.0, 6.0, -2.0], radius: 4.0, angles: [-60.0, 60.0], cols: 4, spacing: [1.0, 1.0] }
    }
}

impl ArrangeTool {
    fn layout(&self) -> crate::light::place::Layout {
        use crate::light::place::Layout;
        match self.kind {
            1 => Layout::Arc { centre: self.centre, radius: self.radius, from: self.angles[0], to: self.angles[1] },
            2 => Layout::Grid { origin: self.from, cols: self.cols, spacing: self.spacing },
            _ => Layout::Line { from: self.from, to: self.to },
        }
    }
}

/// Spread tool: attribute, from, to, centre-out.
#[derive(Clone, Copy)]
struct FanTool(Option<Attribute>, f32, f32, bool);

impl Default for FanTool {
    fn default() -> Self {
        Self(None, 0.0, 1.0, false)
    }
}

/// Results of background searches for output devices.
#[derive(Default)]
struct Found {
    searching: bool,
    nodes: Vec<crate::light::artnet::Node>,
    ports: Vec<String>,
    error: Option<String>,
}


fn issue_text(i: &PatchIssue) -> String {
    match i {
        PatchIssue::DuplicateId(f) => tr!("fixture {f}: number used twice", f = f.to_string()),
        PatchIssue::UnknownType(f) => tr!("fixture {f}: unknown type or mode", f = f.to_string()),
        PatchIssue::OutOfRange(f) => tr!("fixture {f}: address out of range", f = f.to_string()),
        PatchIssue::Overlap(a, b) => tr!("fixtures {a} and {b} overlap", a = a.to_string(), b = b.to_string()),
    }
}

impl App {
    /// Pick effect files (`.oqfx`) and add them to the effects pool (outside the show lock: the
    /// dialog blocks). Files are data only: no script comes with them ([`crate::light::fxfile`]).
    pub(super) fn import_effects(&mut self) {
        if !self.can_design() {
            return;
        }
        let Some(paths) = rfd::FileDialog::new().add_filter(t("OpenQ effect"), &[crate::light::fxfile::EXTENSION]).pick_files() else { return };
        let mut added = Vec::new();
        for path in paths {
            let read = || -> anyhow::Result<Effect> {
                Ok(crate::light::fxfile::read(&crate::recovery::read_text(&path, crate::light::fxfile::MAX_FILE as u64)?)?.effect)
            };
            match read() {
                Ok(e) => added.push(e),
                Err(e) => {
                    let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    self.notify_error(tr!("Can't import {file}: {e}", file = file, e = format!("{e:#}")));
                }
            }
        }
        if added.is_empty() {
            return;
        }
        let n = added.len();
        {
            let mut show = self.show.lock();
            let l = &mut show.ws.lighting;
            for mut e in added {
                e.id = l.effects.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                self.light.effect = Some(e.id);
                l.effects.push(e);
            }
            show.edited_outside_cues();
        }
        self.light.tab = LightTab::Effects;
        self.notify(tr!("Imported {n} effects", n = n.to_string()));
    }

    /// Save the patch as an MVR file, or the plan as an SVG or PDF plot (by the extension picked;
    /// outside the show lock: the dialog blocks).
    pub(super) fn export_patch(&mut self, plot: bool) {
        let (design, scope) = {
            let show = self.show.lock();
            let scope = show.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "OpenQ".into());
            (show.ws.lighting.clone(), scope)
        };
        use crate::light::plot;
        let (kind, ext) = if plot { ("PDF", plot::PDF_EXTENSION) } else { ("MVR", crate::light::mvr::EXTENSION) };
        let mut dialog = rfd::FileDialog::new().add_filter(kind, &[ext]);
        if plot {
            dialog = dialog.add_filter("SVG", &[plot::EXTENSION]);
        }
        let Some(path) = dialog.set_file_name(format!("{scope}.{ext}")).save_file() else { return };
        let svg = path.extension().is_some_and(|e| e.eq_ignore_ascii_case(plot::EXTENSION));
        let kind = if plot && svg { "SVG" } else { kind };
        let bytes = match (plot, svg) {
            (true, true) => Ok(plot::svg(&design, &scope).into_bytes()),
            (true, false) => Ok(plot::pdf(&design, &scope)),
            (false, _) => crate::light::mvr::export(&design, &scope),
        };
        match bytes.and_then(|b| crate::recovery::write_atomic(&path, &b)) {
            Ok(()) => self.notify(tr!("Saved {file}", file = path.display().to_string())),
            Err(e) => self.notify_error(tr!("Can't export {kind}: {e}", kind = kind, e = format!("{e:#}"))),
        }
    }

    /// Save an effect as an effect file (outside the show lock).
    pub(super) fn export_effect(&mut self, id: EffectId) {
        let Some(e) = self.show.lock().ws.lighting.effect(id).cloned() else { return };
        let name = if e.name.trim().is_empty() { format!("effect-{id}") } else { e.name.trim().to_string() };
        let Some(path) = rfd::FileDialog::new()
            .add_filter(t("OpenQ effect"), &[crate::light::fxfile::EXTENSION])
            .set_file_name(format!("{name}.{}", crate::light::fxfile::EXTENSION))
            .save_file()
        else {
            return;
        };
        let text = crate::light::fxfile::write(&e, "", "");
        match crate::recovery::write_atomic(&path, text.as_bytes()) {
            Ok(()) => self.notify(tr!("Saved {file}", file = path.display().to_string())),
            Err(e) => self.notify_error(tr!("Can't save the effect: {e}", e = format!("{e:#}"))),
        }
    }

    /// Design edits are allowed (Edit mode, and this device may edit the show).
    pub(super) fn can_design(&self) -> bool {
        self.mode == Mode::Edit && self.can_edit_show()
    }

    /// Cues / Lighting switch, in a bar of its own above the GO / standby header (which only the Cues view shows).
    pub(super) fn view_tabs(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            theme::tab(ui, &mut self.view, View::Cues, icons::RECEIPT_LONG, t("Cues"));
            theme::tab(ui, &mut self.view, View::Lighting, icons::LIGHTBULB, t("Lighting"));
        });
    }

    /// The Lighting workspace (central panel).
    pub(super) fn lighting_view(&mut self, ui: &mut Ui, show: &mut Show) {
        egui::Panel::top("command_line").frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 4))).show(ui, |ui| self.command_bar(ui, show));
        self.keypad_window(ui.ctx(), show);
        // Clear of the command line panel's edge.
        ui.add_space(6.0);
        if !self.can_design() {
            // Show mode: playback and the monitor only.
            if !matches!(self.light.tab, LightTab::Playback | LightTab::Monitor) {
                self.light.tab = LightTab::Playback;
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                theme::tab(ui, &mut self.light.tab, LightTab::Playback, icons::TOUCH_APP, t("Playback"));
                theme::tab(ui, &mut self.light.tab, LightTab::Monitor, icons::GRID_VIEW, t("DMX monitor"));
                ui.add_space(12.0);
                ui.label(RichText::new(icons::LOCK).font(theme::icon_font(16.0)).color(theme::TEXT_DIM));
                ui.label(RichText::new(t("The lighting design is locked in Show mode.")).color(theme::TEXT_DIM));
            });
            ui.separator();
            if self.light.tab == LightTab::Playback {
                self.playback_page(ui, show);
            } else {
                self.monitor_page(ui, show);
            }
            return;
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            theme::tab(ui, &mut self.light.tab, LightTab::Outputs, icons::ROUTER, t("Outputs"));
            theme::tab(ui, &mut self.light.tab, LightTab::Patch, icons::CABLE, t("Patch"));
            theme::tab(ui, &mut self.light.tab, LightTab::Programmer, icons::TUNE, t("Programmer"));
            theme::tab(ui, &mut self.light.tab, LightTab::Sequences, icons::PLAY_CIRCLE, t("Sequences"));
            theme::tab(ui, &mut self.light.tab, LightTab::Effects, icons::AUTO_FIX_HIGH, t("Effects"));
            theme::tab(ui, &mut self.light.tab, LightTab::Playback, icons::TOUCH_APP, t("Playback"));
            theme::tab(ui, &mut self.light.tab, LightTab::Monitor, icons::GRID_VIEW, t("DMX monitor"));
        });
        ui.separator();
        let before = show.ws.lighting.clone();
        self.light.audio_level = show.media.light.audio_level();
        self.light.bpm = show.media.light.masters().bpm;
        match self.light.tab {
            LightTab::Patch => self.patch_page(ui, &mut show.ws.lighting),
            LightTab::Programmer => self.programmer_page(ui, &mut show.ws.lighting),
            LightTab::Effects => self.effects_page(ui, &mut show.ws.lighting),
            LightTab::Sequences => self.sequences_page(ui, show),
            LightTab::Playback => self.playback_page(ui, show),
            LightTab::Outputs => self.outputs_page(ui, show),
            LightTab::Monitor => self.monitor_page(ui, show),
        }
        if self.light.plot.open {
            let title = show.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "OpenQ".into());
            self.plot_editor(ui.ctx(), &mut show.ws.lighting, &title);
        }
        if show.ws.lighting != before {
            show.edited_outside_cues();
        }
        if std::mem::take(&mut self.light.capture) {
            let frame = show.media.light.input_frame();
            let values = crate::light::dmxin::capture(&show.ws.lighting, &frame, &self.light.selection);
            if frame.universes.is_empty() {
                self.notify(t("Nothing received on the DMX inputs."));
            } else {
                let units: HashSet<FixtureId> = values.iter().map(|v| v.0).collect();
                for (f, a, v) in values {
                    self.light.programmer.entry(f).or_default().insert(a, v.into());
                }
                self.notify(tr!("Captured {n} fixtures from the DMX inputs.", n = units.len().to_string()));
            }
        }
    }

    /// Per-frame lighting housekeeping: the programmer is cleared outside design (Show mode),
    /// and handed to the light thread when it changed.
    pub(super) fn lighting_frame(&mut self, show: &mut Show) {
        let designing = self.can_design();
        let l = &mut self.light;
        // The selection is shared with remote devices (a visualiser shows it, and may pick).
        if show.light_selection_gen != l.selection_gen {
            l.selection_gen = show.light_selection_gen;
            l.selection = show.light_selection.clone();
        } else if show.light_selection != l.selection {
            show.light_selection.clone_from(&l.selection);
        }
        if !designing && (!l.programmer.is_empty() || !l.programmer_effects.is_empty()) {
            l.programmer.clear();
            l.programmer_effects.clear();
        }
        let l = &mut self.light;
        let design = &show.ws.lighting;
        let effects: Vec<(Effect, Vec<FixtureId>)> = if l.blind {
            Vec::new()
        } else {
            l.programmer_effects.iter().filter_map(|r| Some((design.effect(r.effect)?.clone(), r.fixtures.clone()))).collect()
        };
        // Sent again after an edit too (the light thread drops effects when slots move, and
        // programmer values may refer to presets that changed).
        let edited = show.edit_gen != l.sent_gen;
        if l.sent_effects.as_ref() != Some(&effects) || edited {
            show.media.light.set_programmer_effects(effects.clone());
            l.sent_effects = Some(effects);
            l.sent_gen = show.edit_gen;
        }
        if !designing {
            l.highlight = false;
        }
        let lit: Option<Vec<FixtureId>> = l.highlight.then(|| match l.step {
            Some(i) => l.selection.get(i % l.selection.len().max(1)).copied().into_iter().collect(),
            None => l.selection.clone(),
        });
        let key = (l.programmer.clone(), l.blind, lit);
        if l.sent.as_ref() != Some(&key) || edited {
            let design = &show.ws.lighting;
            let mut flat: Vec<(FixtureId, Attribute, f32)> = if l.blind {
                Vec::new()
            } else {
                l.programmer
                    .iter()
                    .flat_map(|(f, v)| v.iter().filter_map(move |(a, x)| design.resolve(*f, *a, *x).map(|x| (*f, *a, x))))
                    .collect()
            };
            for f in key.2.iter().flatten() {
                let Some(chans) = design.unit_channels(*f) else { continue };
                for a in design.unit_attributes(*f) {
                    if let Some(v) = highlight_value(chans, a) {
                        flat.retain(|x| !(x.0 == *f && x.1 == a));
                        flat.push((*f, a, v));
                    }
                }
            }
            show.media.light.set_programmer(flat);
            l.sent = Some(key);
        }
    }

    fn patch_page(&mut self, ui: &mut Ui, l: &mut Lighting) {
        self.poll_fixture_library();
        let library = self.light.types.refresh();
        static GENERICS: std::sync::LazyLock<Vec<FixtureType>> = std::sync::LazyLock::new(crate::light::library::generics);
        // Borrowed, not cloned: this runs every frame under the show lock.
        let (types, in_show, in_lib) = super::fixture_types::available_types(l, &library.items, &GENERICS);
        theme::wrapped_row(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            theme::tab(ui, &mut self.light.patch_view, PatchView::Sheet, icons::NOTES, t("Sheet"));
            theme::tab(ui, &mut self.light.patch_view, PatchView::Plan, icons::OPEN_WITH, t("Plan"));
            theme::tab(ui, &mut self.light.patch_view, PatchView::Types, icons::CATEGORY, t("Fixture types"));
            ui.add_space(8.0);
            // Imports on the sheet and the plan, exports on the plan only.
            let view = self.light.patch_view;
            if view != PatchView::Types
                && ui
                .button(theme::with_icon(icons::UPLOAD_FILE, t("Import fixture…")))
                .on_hover_text(t("Open Fixture Library (.json), GDTF (.gdtf), QLC+ (.qxf) or OpenQ (.oqfixture) files"))
                .clicked()
            {
                self.deferred.push(super::Deferred::ImportFixture);
            }
            if cfg!(feature = "fixture-download")
                && view != PatchView::Types
                && ui.button(theme::with_icon(icons::DOWNLOAD, t("Open Fixture Library…"))).on_hover_text(t("Search and download fixtures from open-fixture-library.org")).clicked() {
                self.light.patch_view = PatchView::Types;
                self.light.types.ofl_open = true;
                self.light.types.ofl_focus = true;
            }
            if view != PatchView::Types
                && ui.button(theme::with_icon(icons::UPLOAD_FILE, t("Import MVR…"))).on_hover_text(t("Add the fixtures, positions and trusses of an MVR file (from a visualiser, CAD or another desk)")).clicked()
            {
                self.deferred.push(super::Deferred::ImportMvr);
            }
            if view != PatchView::Plan {
                return;
            }
            if ui
                .add_enabled(!l.fixtures.is_empty(), egui::Button::new(theme::with_icon(icons::DOWNLOAD, t("Export MVR…"))))
                .on_hover_text(t("The patch with positions as an MVR file, for visualisers"))
                .clicked()
            {
                self.deferred.push(super::Deferred::ExportPatch(false));
            }
            if ui
                .add_enabled(!l.fixtures.is_empty(), egui::Button::new(theme::with_icon(icons::VIEW_IN_AR, t("Open in OQVis"))))
                .on_hover_text(t("Show the patch in the OQVis 3D visualiser"))
                .clicked()
            {
                self.deferred.push(super::Deferred::OQVis);
            }
            if ui
                .add_enabled(!l.fixtures.is_empty(), egui::Button::new(theme::with_icon(icons::EDIT, t("Plot…"))))
                .on_hover_text(t("Arrange the plan as a drawing (move items, change texts), then save it as a PDF (A3, to scale) or SVG"))
                .clicked()
            {
                self.light.plot.open = true;
            }
        });
        if self.light.patch_view == PatchView::Types {
            ui.add_space(4.0);
            self.types_view(ui, l);
            return;
        }
        // Add fixtures.
        let form = &mut self.light.add;
        let mut short = None;
        let mut add = false;
        let ty = types.iter().find(|t| t.id == form.type_id).or(types.first()).copied();
        theme::wrapped_row(ui, |ui| {
            theme::keep_together(ui, "patch_type", |ui| {
                ui.label(theme::caption(t("Add fixtures")));
                let label = |t: &FixtureType| format!("{} {}", t.manufacturer, t.name);
                egui::ComboBox::from_id_salt("patch_type")
                    .selected_text(ty.map(label).unwrap_or_default())
                    .width(200.0)
                    .height(480.0)
                    .show_ui(ui, |ui| {
                        for (i, ty) in types.iter().enumerate() {
                            let head = if i == 0 && in_show > 0 {
                                Some(t("In this show"))
                            } else if i == in_show && in_lib > 0 {
                                Some(t("Library"))
                            } else if i == in_show + in_lib {
                                Some(t("Built-in"))
                            } else {
                                None
                            };
                            if let Some(h) = head {
                                ui.label(theme::caption(h));
                            }
                            let mut text = label(ty);
                            if let Some(m) = ty.modes.iter().find(|m| !m.heads.is_empty()) {
                                text = format!("{text} · {}", tr!("{n} cells", n = m.heads.len().to_string()));
                            }
                            ui.selectable_value(&mut form.type_id, ty.id.clone(), text);
                        }
                    });
                if let Some(ty) = ty {
                    let mode = ty.mode(&form.mode).map(|m| m.name.clone()).unwrap_or_default();
                    egui::ComboBox::from_id_salt("patch_mode").selected_text(&mode).show_ui(ui, |ui| {
                        for m in &ty.modes {
                            ui.selectable_value(&mut form.mode, m.name.clone(), &m.name);
                        }
                    });
                }
            });
            ui.label(t("Count"));
            ui.add(DragValue::new(&mut form.count).range(1..=512));
            ui.label(t("First number"));
            ui.add(DragValue::new(&mut form.first_id).range(1..=99_999));
            ui.label(t("Universe"));
            ui.add(DragValue::new(&mut form.universe).range(1..=crate::light::model::MAX_UNIVERSE));
            ui.label(t("Address"));
            ui.add(DragValue::new(&mut form.address).range(0..=512).custom_formatter(|v, _| {
                if v == 0.0 { t("auto").to_string() } else { format!("{v}") }
            }));
            add = ui.button(theme::with_icon(icons::ADD, t("Add"))).clicked();
        });
        // The type is cloned only when used: `add_fixtures` changes the design it may come from.
        if let Some(ty) = ty.filter(|_| add).cloned() {
            let added = add_fixtures(l, &ty, form);
            if added < form.count {
                short = Some(added);
            }
        }
        if let Some(n) = short {
            self.notify(tr!("Only {n} fixtures fit in the universe", n = n.to_string()));
        }
        // Selection actions and problems.
        let issues = l.validate();
        ui.horizontal(|ui| {
            let n = self.light.patch_sel.len();
            ui.add_enabled_ui(n > 0, |ui| {
                if ui.button(theme::with_icon(icons::DELETE, &tr!("Remove {n} fixtures", n = n.to_string()))).clicked() {
                    l.remove_fixtures(&self.light.patch_sel);
                    self.light.patch_sel.clear();
                }
            });
            if ui.button(theme::with_icon(icons::SELECT_ALL, t("Select all"))).clicked() {
                self.light.patch_sel = l.fixtures.iter().map(|f| f.id).collect();
            }
            if n > 0 && ui.button(theme::with_icon(icons::DESELECT, t("Select none"))).clicked() {
                self.light.patch_sel.clear();
            }
            ui.label(RichText::new(tr!("{n} fixtures", n = l.fixtures.len().to_string())).color(theme::TEXT_DIM));
            if let Some(first) = issues.first() {
                let more = if issues.len() > 1 { format!(" (+{})", issues.len() - 1) } else { String::new() };
                ui.label(RichText::new(format!("{} {}{more}", icons::WARNING, issue_text(first))).color(theme::AMBER));
            }
        });
        self.arrange_bar(ui, l);
        if self.light.patch_view == PatchView::Plan {
            self.shape_bar(ui, l);
        }
        ui.add_space(4.0);
        if self.light.patch_view == PatchView::Plan {
            plan_view(ui, &mut self.light, l, &issues);
        } else {
            self.patch_sheet(ui, l, &issues);
        }
    }

    /// Plan drawing tools, and the settings of the selected bar or rectangle.
    fn shape_bar(&mut self, ui: &mut Ui, l: &mut Lighting) {
        let light = &mut self.light;
        let mut hang = None;
        let mut delete = false;
        theme::wrapped_row(ui, |ui| {
            ui.label(theme::caption(t("Draw")));
            for (tool, glyph, label, tip) in [
                (PlanTool::Bar, icons::REMOVE, t("Bar"), t("Drag on the plan to draw a lighting bar (pipe, truss)")),
                (PlanTool::Rect, icons::CROP_FREE, t("Rectangle"), t("Drag on the plan to draw a rectangle (stage, riser, set piece)")),
            ] {
                let on = light.plan_tool == tool;
                if ui.selectable_label(on, theme::with_icon(glyph, label)).on_hover_text(tip).clicked() {
                    light.plan_tool = if on { PlanTool::Select } else { tool };
                }
            }
            let Some(s) = light.shape_sel.and_then(|id| l.shapes.iter_mut().find(|s| s.id == id)) else {
                if !l.shapes.is_empty() {
                    ui.add(egui::Label::new(RichText::new(t("Click a bar or the outline of a rectangle to edit it.")).color(theme::TEXT_DIM)).wrap());
                }
                return;
            };
            theme::separated(ui, "shape_name", |ui| {
                ui.label(RichText::new(if s.kind == ShapeKind::Bar { t("Bar") } else { t("Rectangle") }).color(theme::TEXT_DIM));
                ui.add(egui::TextEdit::singleline(&mut s.name).hint_text(t("Name")).desired_width(90.0));
            });
            // Stored as (x, towards the audience); shown as stage x and y (depth).
            let xz = |ui: &mut Ui, v: &mut [f32; 2]| {
                let m = crate::limits::MAX_DISTANCE;
                ui.add(DragValue::new(&mut v[0]).range(-m..=m).speed(0.05).max_decimals(2).prefix("x ").suffix(" m"));
                let mut depth = 0.0 - v[1];
                if ui.add(DragValue::new(&mut depth).range(-m..=m).speed(0.05).max_decimals(2).prefix("y ").suffix(" m")).changed() {
                    v[1] = 0.0 - depth;
                }
            };
            ui.label(t("From"));
            xz(ui, &mut s.a);
            ui.label(t("to"));
            xz(ui, &mut s.b);
            ui.label(t("Height"));
            ui.add(DragValue::new(&mut s.height).range(-50.0..=100.0).speed(0.05).max_decimals(2).suffix(" m"));
            if s.kind == ShapeKind::Bar {
                ui.label(RichText::new(tr!("{len} m long", len = format!("{:.2}", s.length()))).color(theme::TEXT_DIM));
                let n = light.patch_sel.len();
                if ui
                    .add_enabled(n > 0, egui::Button::new(theme::with_icon(icons::CHECK_CIRCLE, &tr!("Hang {n} fixtures", n = n.to_string()))))
                    .on_hover_text(t("Spread the selected fixtures evenly along the bar, at its height"))
                    .clicked()
                {
                    hang = Some(s.clone());
                }
            }
            if s.label != [0.0, 0.0] && ui.small_button(icons::REPLAY).on_hover_text(t("Put the label back in its place")).clicked() {
                s.label = [0.0, 0.0];
            }
            delete = ui.small_button(icons::DELETE).on_hover_text(t("Delete (or press Delete over the plan)")).clicked();
        });
        if let Some(bar) = hang {
            let mut ids: Vec<FixtureId> = light.patch_sel.iter().copied().collect();
            ids.sort_unstable();
            for (id, p) in ids.iter().zip(bar.hang(ids.len())) {
                if let Some(f) = l.fixtures.iter_mut().find(|f| f.id == *id) {
                    f.position = in_venue(p);
                }
            }
        }
        if delete {
            l.shapes.retain(|s| Some(s.id) != light.shape_sel);
            light.shape_sel = None;
        }
    }

    /// Lay the selected fixtures out (in fixture-number order) along a line, an arc or a grid.
    fn arrange_bar(&mut self, ui: &mut Ui, l: &mut Lighting) {
        let n = self.light.patch_sel.len();
        if n == 0 {
            return;
        }
        let a = &mut self.light.arrange;
        let m = crate::limits::MAX_DISTANCE;
        // Stage axes: x across, y depth, z height.
        let xyz = |ui: &mut Ui, stored: &mut [f32; 3]| {
            let mut v = to_stage(*stored);
            let mut changed = false;
            for (c, axis) in v.iter_mut().zip(["x ", "y ", "z "]) {
                changed |= ui.add(DragValue::new(c).range(-m..=m).speed(0.05).max_decimals(2).prefix(axis).suffix(" m")).changed();
            }
            if changed {
                *stored = from_stage(v);
            }
        };
        let mut apply = false;
        theme::wrapped_row(ui, |ui| {
            theme::keep_together(ui, "arrange_kind", |ui| {
                ui.label(theme::caption(t("Arrange")));
                let kinds = [t("Line"), t("Arc"), t("Grid")];
                egui::ComboBox::from_id_salt("arrange_kind").selected_text(kinds[a.kind as usize % 3]).show_ui(ui, |ui| {
                    for (i, k) in kinds.iter().enumerate() {
                        ui.selectable_value(&mut a.kind, i as u8, *k);
                    }
                });
            });
            match a.kind {
                1 => {
                    ui.label(t("Centre"));
                    xyz(ui, &mut a.centre);
                    ui.label(t("Radius"));
                    ui.add(DragValue::new(&mut a.radius).range(0.0..=100.0).speed(0.05).max_decimals(2).suffix(" m"));
                    ui.label(t("From"));
                    ui.add(DragValue::new(&mut a.angles[0]).range(-360.0..=360.0).suffix("°"));
                    ui.label(t("to"));
                    ui.add(DragValue::new(&mut a.angles[1]).range(-360.0..=360.0).suffix("°"));
                }
                2 => {
                    ui.label(t("First"));
                    xyz(ui, &mut a.from);
                    ui.label(t("Columns"));
                    ui.add(DragValue::new(&mut a.cols).range(1..=512));
                    ui.label(t("Spacing"));
                    // Stored as (x, towards the audience); shown as (x, depth).
                    ui.add(DragValue::new(&mut a.spacing[0]).range(-m..=m).speed(0.05).max_decimals(2).prefix("x ").suffix(" m"));
                    let mut depth = 0.0 - a.spacing[1];
                    if ui.add(DragValue::new(&mut depth).range(-m..=m).speed(0.05).max_decimals(2).prefix("y ").suffix(" m")).changed() {
                        a.spacing[1] = 0.0 - depth;
                    }
                }
                _ => {
                    ui.label(t("From"));
                    xyz(ui, &mut a.from);
                    ui.label(t("to"));
                    xyz(ui, &mut a.to);
                }
            }
            apply = ui.button(theme::with_icon(icons::CHECK_CIRCLE, &tr!("Arrange {n} fixtures", n = n.to_string()))).clicked();
        });
        if apply {
            let mut ids: Vec<FixtureId> = self.light.patch_sel.iter().copied().collect();
            ids.sort_unstable();
            let pos = crate::light::place::positions(self.light.arrange.layout(), ids.len());
            for (id, p) in ids.iter().zip(pos) {
                if let Some(f) = l.fixtures.iter_mut().find(|f| f.id == *id) {
                    f.position = in_venue(p);
                }
            }
        }
    }

    /// The fixture table (only visible rows are laid out, so big rigs stay fast). Every column is
    /// a cell of one width for all rows (fitted to what the cells hold, see [`sheet_widths`]), so
    /// headers and rows line up, and every row takes exactly `ROW` (what the scroll area assumes:
    /// taller rows made it jitter at the end).
    fn patch_sheet(&mut self, ui: &mut Ui, l: &mut Lighting, issues: &[PatchIssue]) {
        const ROW: f32 = 26.0;
        let bad: HashSet<FixtureId> = issues
            .iter()
            .flat_map(|i| match i {
                PatchIssue::Overlap(a, b) => vec![*a, *b],
                PatchIssue::DuplicateId(f) | PatchIssue::UnknownType(f) | PatchIssue::OutOfRange(f) => vec![*f],
            })
            .collect();
        let header =
            [t("No."), t("Name"), t("Type"), t("Mode"), t("Universe"), t("Address"), t("Slots"), t("Position x y z (m)"), t("Rotation x y z (°)"), t("Invert")];
        // Fitted widths, measured again only when the texts change.
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            header.hash(&mut h);
            (t("Pan"), t("Tilt")).hash(&mut h);
            for f in l.fixtures.iter() {
                (&f.name, &f.fixture_type, &f.mode).hash(&mut h);
            }
            l.fixture_types.len().hash(&mut h);
            h.finish()
        };
        let cols = match self.light.sheet_widths {
            Some((k, w)) if k == key => w,
            _ => {
                let w = sheet_widths(ui, l, &header);
                self.light.sheet_widths = Some((key, w));
                w
            }
        };
        let width: f32 = cols.iter().sum::<f32>() + ui.spacing().item_spacing.x * (cols.len() - 1) as f32;
        egui::ScrollArea::horizontal().id_salt("patch_sheet_h").auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                for (h, w) in header.iter().zip(cols) {
                    sheet_cell(ui, w, 18.0, |ui| {
                        ui.label(theme::caption(h));
                    });
                }
            });
            self.patch_rows(ui, l, &bad, cols, ROW, width);
        });
    }

    /// The rows of [`App::patch_sheet`].
    fn patch_rows(&mut self, ui: &mut Ui, l: &mut Lighting, bad: &HashSet<FixtureId>, cols: [f32; 10], row_h: f32, width: f32) {
        let mut renumber = None;
        let mut click: Option<(FixtureId, egui::Modifiers)> = None;
        let Lighting { fixtures, fixture_types, .. } = &mut *l;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, row_h, fixtures.len(), |ui, range| {
            ui.spacing_mut().interact_size.y = row_h - 4.0;
            for f in &mut fixtures[range] {
                let ty = fixture_types.iter().find(|t| t.id == f.fixture_type);
                let selected = self.light.patch_sel.contains(&f.id);
                let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, row_h));
                if bad.contains(&f.id) {
                    ui.painter().rect_stroke(rect, 3.0, egui::Stroke::new(1.0, theme::AMBER), egui::StrokeKind::Inside);
                } else if selected {
                    ui.painter().rect_filled(rect, 3.0, Color32::from_rgba_unmultiplied(0x4a, 0x7c, 0xf0, 24));
                }
                let layout = egui::Layout::left_to_right(egui::Align::Center);
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(0.0, 2.0))).layout(layout), |ui| {
                    let h = row_h - 4.0;
                    sheet_cell(ui, cols[0], h, |ui| {
                        let mut id = f.id;
                        let r = ui.add_sized([cols[0], h], DragValue::new(&mut id).range(1..=99_999));
                        if id != f.id {
                            renumber = Some((f.id, id, r.dragged()));
                        }
                    });
                    sheet_cell(ui, cols[1], h, |ui| {
                        ui.add_sized([cols[1], h], egui::TextEdit::singleline(&mut f.name).vertical_align(egui::Align::Center).hint_text(ty.map_or("", |t| t.name.as_str())));
                    });
                    sheet_cell(ui, cols[2], h, |ui| {
                        let ty_label = ty.map_or_else(|| f.fixture_type.clone(), |t| format!("{} {}", t.manufacturer, t.name));
                        let sel = ui.add_sized([cols[2], h], egui::Button::selectable(selected, ty_label).truncate());
                        if sel.clicked() {
                            click = Some((f.id, ui.input(|i| i.modifiers)));
                        }
                    });
                    sheet_cell(ui, cols[3], h, |ui| match ty {
                        Some(ty) if ty.modes.len() > 1 => {
                            egui::ComboBox::from_id_salt(("fx_mode", f.id)).selected_text(&f.mode).width(cols[3]).truncate().show_ui(ui, |ui| {
                                for m in &ty.modes {
                                    ui.selectable_value(&mut f.mode, m.name.clone(), &m.name);
                                }
                            });
                        }
                        _ => {
                            ui.put(ui.max_rect(), egui::Label::new(RichText::new(&f.mode).color(theme::TEXT_DIM)).truncate());
                        }
                    });
                    sheet_cell(ui, cols[4], h, |ui| {
                        ui.add_sized([cols[4], h], DragValue::new(&mut f.universe).range(1..=crate::light::model::MAX_UNIVERSE));
                    });
                    sheet_cell(ui, cols[5], h, |ui| {
                        ui.add_sized([cols[5], h], DragValue::new(&mut f.address).range(0..=512).custom_formatter(|v, _| {
                            if v == 0.0 { "—".to_string() } else { format!("{v}") }
                        }));
                    });
                    let mode = ty.and_then(|t| t.mode(&f.mode));
                    let slots = mode.map_or(0, |m| m.footprint());
                    let breaks = mode.map_or(1, |m| m.breaks());
                    sheet_cell(ui, cols[6], h, |ui| {
                        if breaks > 1 {
                            // Multi-break modes: the other breaks' addresses in a menu.
                            let label = slots_label(slots, breaks);
                            let button = egui::Button::new(label).truncate().min_size(egui::vec2(ui.available_width(), h));
                            egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
                                for b in 1..breaks {
                                    if f.breaks.len() < b as usize {
                                        f.breaks.resize(b as usize, Default::default());
                                    }
                                    let at = &mut f.breaks[b as usize - 1];
                                    if at.universe == 0 {
                                        at.universe = f.universe;
                                    }
                                    ui.horizontal(|ui| {
                                        ui.label(tr!("Break {n}", n = (b + 1).to_string()));
                                        ui.add(DragValue::new(&mut at.universe).range(1..=crate::light::model::MAX_UNIVERSE));
                                        ui.label(".");
                                        ui.add(DragValue::new(&mut at.address).range(0..=512).custom_formatter(|v, _| if v == 0.0 { "—".to_string() } else { format!("{v}") }));
                                        let n = mode.map_or(0, |m| m.footprint_of(b));
                                        ui.label(RichText::new(tr!("{n} slots", n = n.to_string())).color(theme::TEXT_DIM));
                                    });
                                }
                            });
                        } else {
                            ui.put(ui.max_rect(), egui::Label::new(RichText::new(slots.to_string()).color(theme::TEXT_DIM)));
                        }
                    });
                    // Three values per cell, each a third of it.
                    // Shown in stage axes (x across, y depth, z height).
                    let triple = |ui: &mut Ui, w: f32, stored: &mut [f32; 3], rotation: bool| {
                        sheet_cell(ui, w, h, |ui| {
                            let each = (w - 8.0) / 3.0;
                            let top = ui.max_rect().left_top();
                            let mut v = to_stage(*stored);
                            let before = v;
                            for (i, x) in v.iter_mut().enumerate() {
                                let d = if rotation { DragValue::new(x).range(-360.0..=360.0).speed(1.0).max_decimals(1) } else {
                                    let m = crate::limits::MAX_DISTANCE;
                                    DragValue::new(x).range(-m..=m).speed(0.05).max_decimals(2)
                                };
                                ui.put(egui::Rect::from_min_size(top + egui::vec2(i as f32 * (each + 4.0), 0.0), egui::vec2(each, h)), d);
                            }
                            if v != before {
                                *stored = from_stage(v);
                            }
                        });
                    };
                    triple(ui, cols[7], &mut f.position, false);
                    triple(ui, cols[8], &mut f.rotation, true);
                    sheet_cell(ui, cols[9], h, |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.toggle_value(&mut f.invert_pan, t("Pan")).on_hover_text(t("Reverse pan"));
                        ui.toggle_value(&mut f.invert_tilt, t("Tilt")).on_hover_text(t("Reverse tilt"));
                    });
                });
                ui.advance_cursor_after_rect(rect);
            }
        });
        if let Some((old, new, dragging)) = renumber
            && !l.renumber_fixture(old, new)
            && !dragging
        {
            self.notify(tr!("Fixture number {n} is taken", n = new.to_string()));
        }
        if let Some((id, m)) = click {
            let sel = &mut self.light.patch_sel;
            if m.command || m.shift {
                if !sel.remove(&id) {
                    sel.insert(id);
                }
            } else {
                *sel = [id].into_iter().collect();
            }
        }
    }
}


/// What the Slots cell of a patch sheet row shows.
fn slots_label(slots: u16, breaks: u8) -> String {
    if breaks > 1 { tr!("{slots} · {n} breaks", slots = slots.to_string(), n = breaks.to_string()) } else { slots.to_string() }
}

/// Patch sheet column widths: each fits its header and the widest thing its cells show, within
/// a minimum and a maximum (longer texts are cut with "…").
fn sheet_widths(ui: &Ui, l: &Lighting, header: &[&str; 10]) -> [f32; 10] {
    let text = |s: &str| egui::WidgetText::from(s).into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Body).size().x;
    let head: Vec<f32> = header
        .iter()
        .map(|h| egui::WidgetText::from(theme::caption(h)).into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Body).size().x + 4.0)
        .collect();
    // Padding of a button / text field, and a combo box's arrow.
    let pad = ui.spacing().button_padding.x * 2.0 + 4.0;
    let arrow = ui.spacing().icon_width + ui.spacing().icon_spacing;
    let (mut name, mut ty, mut mode, mut slots) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let mut types: std::collections::HashMap<&str, (f32, f32)> = Default::default();
    for f in l.fixtures.iter() {
        let t = l.fixture_type(&f.fixture_type);
        let (tw, mw) = *types.entry(f.fixture_type.as_str()).or_insert_with(|| match t {
            Some(t) => {
                let modes = t.modes.iter().map(|m| text(&m.name)).fold(0.0, f32::max);
                (text(&format!("{} {}", t.manufacturer, t.name)), if t.modes.len() > 1 { modes + arrow } else { modes })
            }
            None => (text(&f.fixture_type), text(&f.mode)),
        });
        ty = ty.max(tw);
        mode = mode.max(mw);
        name = name.max(text(if f.name.is_empty() { t.map_or("", |t| t.name.as_str()) } else { &f.name }));
        let m = t.and_then(|t| t.mode(&f.mode));
        slots = slots.max(text(&slots_label(m.map_or(0, |m| m.footprint()), m.map_or(1, |m| m.breaks()))));
    }
    let fit = |i: usize, content: f32, min: f32, max: f32| (content.min(max)).max(head[i]).max(min).ceil();
    let toggles = text(t("Pan")) + text(t("Tilt")) + 2.0 * pad + 4.0;
    [
        fit(0, 0.0, 56.0, 56.0),
        fit(1, name + pad, 120.0, 240.0),
        fit(2, ty + pad, 120.0, 260.0),
        fit(3, mode + pad, 80.0, 200.0),
        fit(4, 0.0, 56.0, 56.0),
        fit(5, 0.0, 56.0, 56.0),
        fit(6, slots + pad, 44.0, 160.0),
        fit(7, 0.0, 176.0, 176.0),
        fit(8, 0.0, 176.0, 176.0),
        fit(9, toggles, 80.0, 200.0),
    ]
}

/// One cell of the patch sheet: exactly `w` wide (content is clipped to it), content on the left.
fn sheet_cell(ui: &mut Ui, w: f32, h: f32, add: impl FnOnce(&mut Ui)) {
    // From the top of the row (a centred layout moves the cursor's y between cells).
    let rect = egui::Rect::from_min_size(egui::pos2(ui.cursor().min.x, ui.max_rect().top()), egui::vec2(w, h));
    let layout = egui::Layout::left_to_right(egui::Align::Center);
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(layout), |ui| {
        // Clipped across only (a widget's frame may poke out a pixel above or below).
        ui.set_clip_rect(rect.expand2(egui::vec2(1.0, 4.0)).intersect(ui.clip_rect()));
        add(ui)
    });
    ui.advance_cursor_after_rect(rect);
}

/// What highlight puts on an attribute: open white at full (a channel's own highlight value
/// wins); None leaves the attribute alone.
fn highlight_value(chans: &[crate::light::model::ChannelDef], a: Attribute) -> Option<f32> {
    let chan = chans.iter().find(|c| c.attribute == a);
    if let Some(h) = chan.and_then(|c| c.highlight) {
        return Some(h);
    }
    // A wheel or shutter with an "Open" range: open it.
    if let Some(r) = chan.and_then(|c| c.ranges.iter().find(|r| r.name.eq_ignore_ascii_case("open")))
        && matches!(a, Attribute::ColorWheel | Attribute::Gobo | Attribute::Gobo2 | Attribute::Prism | Attribute::Shutter | Attribute::Frost | Attribute::Iris)
    {
        return Some(r.value());
    }
    use Attribute::*;
    match a {
        Dimmer | Red | Green | Blue | White => Some(1.0),
        Amber | Lime | Uv | Cyan | Magenta | Yellow | Cto | ColorWheel | ColorMacro | Gobo | Gobo2 | Prism | Frost | Iris => Some(0.0),
        Shutter => Some(1.0),
        _ => None,
    }
}

/// A cue number as typed ("5", "2.5").
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 { format!("{}", n as i64) } else { format!("{n}") }
}

/// Effect parameters.
fn effect_editor(ui: &mut Ui, e: &mut Effect, audio_level: f32) {
    egui::Grid::new("effect_editor").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label(t("Name"));
        ui.add(egui::TextEdit::singleline(&mut e.name).desired_width(220.0));
        ui.end_row();
        ui.label(t("Shape"));
        if let (Shape::Custom, Some(table)) = (e.shape, &e.table) {
            let summary = tr!(
                "Custom: {steps} steps, {rows} pattern rows",
                steps = table.steps.to_string(),
                rows = table.width.to_string()
            );
            ui.label(RichText::new(theme::with_icon(icons::CODE, &summary)).color(theme::TEXT_DIM));
        } else {
            egui::ComboBox::from_id_salt("fx_shape").selected_text(e.shape.label()).show_ui(ui, |ui| {
                for s in Shape::ALL {
                    ui.selectable_value(&mut e.shape, s, s.label());
                }
            });
        }
        ui.end_row();
        ui.label(t("Attribute"));
        if let (Shape::Custom, Some(table)) = (e.shape, &e.table) {
            let names: Vec<&str> = table.attributes.iter().map(|a| a.label()).collect();
            ui.label(RichText::new(names.join(", ")).color(theme::TEXT_DIM));
        } else if e.shape == Shape::Circle {
            e.attribute = Attribute::Pan;
            ui.label(RichText::new(t("Pan and tilt")).color(theme::TEXT_DIM));
        } else {
            egui::ComboBox::from_id_salt("fx_attr").selected_text(e.attribute.label()).show_ui(ui, |ui| {
                for a in Attribute::ALL {
                    ui.selectable_value(&mut e.attribute, a, a.label());
                }
            });
        }
        ui.end_row();
        ui.label(t("Speed"));
        ui.horizontal(|ui| {
            let unit = if e.sync { t(" per beat") } else { " Hz" };
            ui.add(DragValue::new(&mut e.speed).range(0.01..=50.0).speed(0.01).max_decimals(2).suffix(unit));
            ui.checkbox(&mut e.sync, t("Follow tempo"));
        });
        ui.end_row();
        ui.label(t("Size"));
        ui.add(percent_drag(&mut e.size));
        ui.end_row();
        ui.label(t("Mode"));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut e.relative, true, t("Around the cue's value"));
            ui.selectable_value(&mut e.relative, false, t("Absolute"));
        });
        ui.end_row();
        if !e.relative {
            ui.label(t("Base"));
            ui.add(percent_drag(&mut e.base));
            ui.end_row();
        }
        if e.shape == Shape::Square {
            ui.label(t("On time"));
            ui.add(percent_drag(&mut e.duty));
            ui.end_row();
        }
        ui.label(t("Phase spread"));
        ui.add(DragValue::new(&mut e.spread).range(0.0..=1440.0).speed(1.0).suffix("°"));
        ui.end_row();
        ui.label(t("Groups"));
        ui.add(DragValue::new(&mut e.groups).range(1..=64)).on_hover_text(t("Fixtures per group: a group moves together"));
        ui.end_row();
        ui.label(t("Wings"));
        ui.add(DragValue::new(&mut e.wings).range(1..=16)).on_hover_text(t("Mirror the pattern across this many parts of the selection"));
        ui.end_row();
        ui.label(t("Audio"));
        ui.horizontal(|ui| {
            let mut on = e.audio.is_some();
            if ui.checkbox(&mut on, t("Size follows the sound")).on_hover_text(t("The louder OpenQ plays, the bigger the effect")).changed() {
                e.audio = on.then(Default::default);
            }
            if let Some(a) = &mut e.audio {
                ui.label(t("Gain"));
                ui.add(DragValue::new(&mut a.gain).range(0.0..=100.0).speed(0.05).max_decimals(2).prefix("× "));
                ui.label(t("Floor"));
                ui.add(percent_drag(&mut a.floor)).on_hover_text(t("Size left in silence"));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
                let (r, _) = ui.allocate_exact_size(egui::vec2(60.0, 10.0), egui::Sense::hover());
                ui.painter().rect_filled(r, 2.0, theme::BG_INPUT);
                let lit = egui::Rect::from_min_size(r.min, egui::vec2(r.width() * a.depth(audio_level), r.height()));
                ui.painter().rect_filled(lit, 2.0, theme::GREEN);
            }
        });
        ui.end_row();
    });
}

/// Level of the `i`-th of `n` fixtures at `x` cycles in attribute column `col` (custom effects).
fn effect_level(e: &Effect, i: usize, n: usize, x: f64, col: u32) -> f32 {
    let x = x + e.phase(i, n) as f64;
    match (&e.table, e.shape) {
        (Some(t), Shape::Custom) => t.sample((e.position(i) % t.width.max(1) as usize) as u32, col, x),
        _ => e.shape.eval(x, e.duty, i as u32 + 1),
    }
}

/// Colour a curve of an attribute is drawn in.
fn attribute_colour(a: Attribute) -> Color32 {
    match a {
        Attribute::Red => Color32::from_rgb(0xf0, 0x50, 0x50),
        Attribute::Green => Color32::from_rgb(0x50, 0xd0, 0x70),
        Attribute::Blue => Color32::from_rgb(0x60, 0x90, 0xff),
        Attribute::White => Color32::from_gray(0xe0),
        Attribute::Amber => theme::AMBER,
        _ => theme::ACCENT,
    }
}

/// Attribute filter of the effect lists: a family, or one attribute.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FxFilter {
    Feature(Feature),
    Attribute(Attribute),
}

impl FxFilter {
    fn label(self) -> String {
        match self {
            FxFilter::Feature(f) => f.label().to_string(),
            FxFilter::Attribute(a) => a.label().to_string(),
        }
    }
}

/// An effect matches the search text (in its name, lower case) and the attribute filter.
fn fx_matches(e: &Effect, search: &str, filter: Option<FxFilter>) -> bool {
    let named = search.is_empty() || e.name.to_lowercase().contains(search);
    let attrs = e.attributes();
    named
        && match filter {
            None => true,
            Some(FxFilter::Feature(f)) => attrs.iter().any(|a| a.feature() == f),
            Some(FxFilter::Attribute(a)) => attrs.contains(&a),
        }
}

/// What an effect puts out for a shape value `v` (0..1): absolute from `base` by `size`, or
/// around the cue's value (taken as full for intensity, half for the rest), as playback does.
fn effect_output(e: &Effect, attr: Attribute, v: f32) -> f32 {
    let out = match (e.relative, attr.is_htp()) {
        (true, true) => 1.0 - e.size * (1.0 - v),
        (true, false) => 0.5 + e.size * (v - 0.5),
        (false, _) => e.base + e.size * v,
    };
    out.clamp(0.0, 1.0)
}

/// Colour that tells the `i`-th preview fixture apart (dot on the graph, lamp outline).
fn preview_colour(i: usize) -> Color32 {
    const HUES: [Color32; 6] = [
        theme::AMBER,
        theme::ACCENT,
        theme::GREEN,
        Color32::from_rgb(0xe0, 0x70, 0xe0),
        Color32::from_rgb(0x5c, 0xc8, 0xd8),
        theme::RED,
    ];
    HUES[i % HUES.len()]
}

/// The effect preview on `n` fixtures (`labels`: their numbers, if they are real ones):
/// * the speed in words;
/// * the output over two cycles (real levels, with base, size and mode; the cue's value dashed when
///   the effect runs around it), seconds along the bottom, a playhead and a dot per fixture where it
///   is in its own offset cycle; a circle is drawn as the pan/tilt path it makes;
/// * the fixtures as lamps showing their level, colour or position now;
/// * the phase (spread, groups, wings) in words.
///
/// `height`: what the preview may take (the graph fits the rest); None = its usual size.
fn effect_preview(ui: &mut Ui, e: &Effect, n: usize, labels: &[FixtureId], bpm: f32, height: Option<f32>) {
    let n = n.max(1);
    let label = |i: usize| labels.get(i).map_or_else(|| (i + 1).to_string(), |f| fixture_label(*f));
    let rate = e.rate(bpm).max(1e-6) as f64;
    let now = ui.input(|i| i.time) * rate;
    let table = e.table.as_ref().filter(|_| e.shape == Shape::Custom);
    let cycle = 1.0 / rate;
    let speed = if e.sync {
        tr!(
            "One cycle every {beats} beats ({secs} s at {bpm} BPM)",
            beats = format!("{:.2}", 1.0 / e.speed.max(1e-6)),
            secs = format!("{cycle:.2}"),
            bpm = format!("{bpm:.0}")
        )
    } else {
        tr!("One cycle every {secs} s", secs = format!("{cycle:.2}"))
    };
    ui.label(RichText::new(speed).color(theme::TEXT_DIM));
    ui.add_space(4.0);
    let w = ui.available_width();
    let small = egui::FontId::proportional(10.5);
    // The lamp strip's cell size, and what is left for the graph (label, strip, phase text, gaps).
    let gap = 4.0;
    let cell = ((w + gap) / n as f32 - gap).clamp(14.0, 40.0);
    let graph_h = height.map_or(170.0, |h| (h - 18.0 - 4.0 - 8.0 - (cell + 16.0) - 32.0 - 5.0 * ui.spacing().item_spacing.y).max(100.0));
    if e.shape == Shape::Circle {
        // Pan across, tilt up: the path and each fixture on it.
        let side = w.min(graph_h);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(w, side), egui::Sense::hover());
        let sq = egui::Rect::from_center_size(rect.center(), egui::vec2(side, side));
        let p = ui.painter();
        p.rect_filled(sq, 6.0, theme::BG_INPUT);
        for k in 1..4 {
            let f = k as f32 / 4.0;
            let (x, y) = (sq.left() + sq.width() * f, sq.top() + sq.height() * f);
            p.line_segment([egui::pos2(x, sq.top()), egui::pos2(x, sq.bottom())], egui::Stroke::new(1.0, theme::BORDER));
            p.line_segment([egui::pos2(sq.left(), y), egui::pos2(sq.right(), y)], egui::Stroke::new(1.0, theme::BORDER));
        }
        let at = |x: f64| {
            let a = std::f32::consts::TAU * x.rem_euclid(1.0) as f32;
            let (s, c) = a.sin_cos();
            let r = e.size * 0.5;
            let (pan, tilt) = if e.relative { (0.5 + r * s, 0.5 + r * c) } else { (e.base + r * (1.0 + s), e.base + r * (1.0 + c)) };
            egui::pos2(sq.left() + pan.clamp(0.0, 1.0) * sq.width(), sq.bottom() - tilt.clamp(0.0, 1.0) * sq.height())
        };
        let path: Vec<egui::Pos2> = (0..=96).map(|k| at(k as f64 / 96.0)).collect();
        p.add(egui::Shape::line(path, egui::Stroke::new(2.0, theme::ACCENT)));
        for i in 0..n {
            p.circle_filled(at(now + e.phase(i, n) as f64), 5.0, preview_colour(i));
        }
        p.text(sq.center_bottom() - egui::vec2(0.0, 4.0), egui::Align2::CENTER_BOTTOM, t("Pan"), small.clone(), theme::TEXT_FAINT);
        p.text(sq.left_center() + egui::vec2(4.0, 0.0), egui::Align2::LEFT_CENTER, t("Tilt"), small.clone(), theme::TEXT_FAINT);
    } else {
        // Output over two cycles: 0–100 % up the side, seconds along the bottom.
        let (rect, _) = ui.allocate_exact_size(egui::vec2(w, graph_h), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 6.0, theme::BG_INPUT);
        let plot = egui::Rect::from_min_max(rect.min + egui::vec2(40.0, 10.0), rect.max - egui::vec2(12.0, 22.0));
        let y = |v: f32| plot.bottom() - v.clamp(0.0, 1.0) * plot.height();
        for (v, text) in [(0.0, "0 %"), (0.5, "50 %"), (1.0, "100 %")] {
            p.line_segment([egui::pos2(plot.left(), y(v)), egui::pos2(plot.right(), y(v))], egui::Stroke::new(1.0, theme::BORDER));
            p.text(egui::pos2(plot.left() - 6.0, y(v)), egui::Align2::RIGHT_CENTER, text, small.clone(), theme::TEXT_FAINT);
        }
        for c in 0..=2 {
            let x = plot.left() + plot.width() * c as f32 / 2.0;
            p.line_segment([egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())], egui::Stroke::new(1.0, theme::BORDER));
            p.text(egui::pos2(x, plot.bottom() + 4.0), egui::Align2::CENTER_TOP, format!("{:.2} s", c as f64 * cycle), small.clone(), theme::TEXT_FAINT);
        }
        // Around the cue's value: where that value is.
        if e.relative {
            let yy = y(if e.attribute.is_htp() { 1.0 } else { 0.5 });
            let mut x = plot.left();
            while x < plot.right() {
                p.line_segment([egui::pos2(x, yy), egui::pos2((x + 6.0).min(plot.right()), yy)], egui::Stroke::new(1.0, theme::TEXT_DIM));
                x += 10.0;
            }
            p.text(egui::pos2(plot.right() - 2.0, yy + 2.0), egui::Align2::RIGHT_TOP, t("cue's value"), small.clone(), theme::TEXT_DIM);
        }
        let curves: Vec<(u32, Attribute, Color32)> = match table {
            Some(tb) => tb.attributes.iter().enumerate().take(6).map(|(c, a)| (c as u32, *a, attribute_colour(*a))).collect(),
            None => vec![(0, e.attribute, theme::ACCENT)],
        };
        let px = |x: f64| plot.left() + plot.width() * (x / 2.0) as f32;
        for (col, attr, colour) in &curves {
            let pts: Vec<egui::Pos2> = (0..=240)
                .map(|k| {
                    let x = k as f64 / 120.0;
                    let v = match table {
                        Some(tb) => tb.sample(0, *col, x),
                        None => e.shape.eval(x, e.duty, 1),
                    };
                    egui::pos2(px(x), y(effect_output(e, *attr, v)))
                })
                .collect();
            p.add(egui::Shape::line(pts, egui::Stroke::new(2.0, *colour)));
        }
        // Now: a playhead, and each fixture where it is in its own (offset) cycle.
        let head = now.rem_euclid(1.0);
        p.line_segment([egui::pos2(px(head), plot.top()), egui::pos2(px(head), plot.bottom())], egui::Stroke::new(1.0, Color32::from_white_alpha(90)));
        for i in 0..n {
            let x = (now + e.phase(i, n) as f64).rem_euclid(1.0);
            let at = egui::pos2(px(x), y(effect_output(e, curves[0].1, effect_level(e, i, n, now, 0))));
            p.circle_filled(at, 4.5, preview_colour(i));
            p.circle_stroke(at, 4.5, egui::Stroke::new(1.0, theme::BG_INPUT));
        }
    }
    // The fixtures as lamps: what each one shows now.
    ui.add_space(8.0);
    let col_of = |a: Attribute| table.and_then(|tb| tb.attributes.iter().position(|x| *x == a)).map(|c| c as u32);
    let rgb = [col_of(Attribute::Red), col_of(Attribute::Green), col_of(Attribute::Blue)];
    let (strip, _) = ui.allocate_exact_size(egui::vec2(w, cell + 16.0), egui::Sense::hover());
    let p = ui.painter();
    for i in 0..n {
        let r = egui::Rect::from_min_size(egui::pos2(strip.left() + i as f32 * (cell + gap), strip.top()), egui::vec2(cell, cell));
        if r.right() > strip.right() + 0.5 {
            break;
        }
        p.rect_filled(r, 4.0, Color32::from_gray(0x16));
        let level = effect_output(e, e.attribute, effect_level(e, i, n, now, 0));
        if rgb.iter().any(Option::is_some) {
            let c = |col: Option<u32>| col.map_or(0, |c| (effect_output(e, Attribute::Red, effect_level(e, i, n, now, c)) * 255.0) as u8);
            p.rect_filled(r.shrink(2.0), 3.0, Color32::from_rgb(c(rgb[0]), c(rgb[1]), c(rgb[2])));
        } else if e.attribute.is_htp() && e.shape != Shape::Circle {
            p.rect_filled(r.shrink(2.0), 3.0, Color32::from_rgb(0xff, 0xd4, 0x8a).gamma_multiply(level));
        } else {
            // Other attributes: how far along their range.
            let top = r.bottom() - 2.0 - (r.height() - 4.0) * level;
            p.rect_filled(egui::Rect::from_min_max(egui::pos2(r.left() + 2.0, top), r.max - egui::vec2(2.0, 2.0)), 2.0, theme::ACCENT.gamma_multiply(0.8));
        }
        p.rect_stroke(r, 4.0, egui::Stroke::new(1.0, preview_colour(i).gamma_multiply(0.7)), egui::StrokeKind::Inside);
        if cell >= 18.0 {
            p.text(egui::pos2(r.center().x, r.bottom() + 2.0), egui::Align2::CENTER_TOP, label(i), egui::FontId::proportional(9.5), theme::TEXT_DIM);
        }
    }
    // Phase in words.
    let next = e.groups.max(1) as usize;
    let mut how = if e.spread <= 0.0 || n <= next {
        t("All fixtures move together.").to_string()
    } else {
        let deg = (e.phase(next, n) - e.phase(0, n)).abs() * 360.0;
        tr!("Each fixture runs {deg}° behind the one before it.", deg = format!("{deg:.0}"))
    };
    if e.groups > 1 {
        how = format!("{how} {}", tr!("Groups of {n} move as one.", n = e.groups.to_string()));
    }
    if e.wings > 1 {
        how = format!("{how} {}", tr!("{n} wings mirror the pattern.", n = e.wings.to_string()));
    }
    ui.add(egui::Label::new(RichText::new(how).small().color(theme::TEXT_DIM)).wrap());
}

/// Top view of the patch (stage x across, y depth: upstage at the top, the audience at the bottom): click selects
/// (Ctrl/Shift adds), dragging a selected fixture moves the selection (snapped to 5 cm), dragging
/// empty space pans, scrolling zooms, double-click recentres; a bar's or rectangle's label drags
/// on its own ([`PlotShape::label`]).
/// Tool of the Plan view: select and move, or draw a shape by dragging.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PlanTool {
    #[default]
    Select,
    Bar,
    Rect,
}

/// The part of a shape being dragged: the whole of it, or one end/corner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Handle {
    Body,
    A,
    B,
    /// The corner (a.x, b.z) of a rectangle.
    AxBz,
    /// The corner (b.x, a.z) of a rectangle.
    BxAz,
}

/// A drag in progress on the Plan view (positions are taken from the start, so snapping
/// doesn't drift).
#[derive(Clone)]
pub(crate) enum PlanDrag {
    Pan,
    Fixtures { from: egui::Pos2, start: Vec<(FixtureId, [f32; 3])> },
    Shape { id: u32, handle: Handle, from: egui::Pos2, start: PlotShape, new: bool },
    /// A shape's label (its offset at the start).
    Label { id: u32, from: egui::Pos2, start: [f32; 2] },
    /// A focus point (its position at the start).
    Point { id: crate::light::model::PointId, from: egui::Pos2, start: [f32; 3] },
}

/// Plan positions snap to 5 cm and stay within [`crate::limits::MAX_DISTANCE`] of the origin.
fn snap(v: f32) -> f32 {
    let m = crate::limits::MAX_DISTANCE;
    ((v * 20.0).round() / 20.0).clamp(-m, m)
}

/// A position (x, height, z) within [`crate::limits::MAX_DISTANCE`] of the origin.
fn in_venue(p: [f32; 3]) -> [f32; 3] {
    let m = crate::limits::MAX_DISTANCE;
    p.map(|v| v.clamp(-m, m))
}

/// The handles of a shape and where they are (x, z).
fn handles(s: &PlotShape) -> Vec<(Handle, [f32; 2])> {
    let mut v = vec![(Handle::A, s.a), (Handle::B, s.b)];
    if s.kind == ShapeKind::Rect {
        v.push((Handle::AxBz, [s.a[0], s.b[1]]));
        v.push((Handle::BxAz, [s.b[0], s.a[1]]));
    }
    v
}

/// `start` with one handle (or the whole shape) moved by `d` metres.
fn moved(start: &PlotShape, handle: Handle, d: egui::Vec2) -> PlotShape {
    let mut s = start.clone();
    let (dx, dz) = (d.x, d.y);
    match handle {
        Handle::Body => {
            s.a = [snap(start.a[0] + dx), snap(start.a[1] + dz)];
            s.b = [s.a[0] + start.b[0] - start.a[0], s.a[1] + start.b[1] - start.a[1]];
        }
        Handle::A => s.a = [snap(start.a[0] + dx), snap(start.a[1] + dz)],
        Handle::B => s.b = [snap(start.b[0] + dx), snap(start.b[1] + dz)],
        Handle::AxBz => {
            s.a[0] = snap(start.a[0] + dx);
            s.b[1] = snap(start.b[1] + dz);
        }
        Handle::BxAz => {
            s.b[0] = snap(start.b[0] + dx);
            s.a[1] = snap(start.a[1] + dz);
        }
    }
    s
}

/// Distance from `p` to the segment `a`–`b` (screen points).
fn segment_distance(p: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
    let ab = b - a;
    let k = if ab.length_sq() > 0.0 { ((p - a).dot(ab) / ab.length_sq()).clamp(0.0, 1.0) } else { 0.0 };
    p.distance(a + ab * k)
}

fn plan_view(ui: &mut Ui, light: &mut LightUi, l: &mut Lighting, issues: &[PatchIssue]) {
    let size = ui.available_size().max(egui::vec2(200.0, 200.0));
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6.0, theme::BG_INPUT);
    if light.plan_zoom <= 0.0 {
        light.plan_zoom = 40.0;
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            light.plan_zoom = (light.plan_zoom * (scroll / 200.0).exp()).clamp(5.0, 400.0);
        }
    }
    if resp.double_clicked() {
        light.plan_pan = egui::Vec2::ZERO;
    }
    let (zoom, origin) = (light.plan_zoom, rect.center() + light.plan_pan);
    let to_screen = |x: f32, z: f32| origin + egui::vec2(x, z) * zoom;
    let to_world = |p: egui::Pos2| (p - origin) / zoom;
    // A 1 m grid (5 m lines stronger) and the origin.
    let (lo, hi) = (((rect.min - origin) / zoom).floor(), ((rect.max - origin) / zoom).ceil());
    if hi.x - lo.x < 400.0 {
        for i in lo.x as i32..=hi.x as i32 {
            let x = origin.x + i as f32 * zoom;
            let c = if i == 0 { theme::TEXT_FAINT } else if i % 5 == 0 { theme::BG_TOOL_HOVER } else { theme::BG_RAISED };
            p.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], egui::Stroke::new(1.0, c));
        }
        for i in lo.y as i32..=hi.y as i32 {
            let y = origin.y + i as f32 * zoom;
            let c = if i == 0 { theme::TEXT_FAINT } else if i % 5 == 0 { theme::BG_TOOL_HOVER } else { theme::BG_RAISED };
            p.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], egui::Stroke::new(1.0, c));
        }
    }
    p.text(rect.center_bottom() - egui::vec2(0.0, 8.0), egui::Align2::CENTER_BOTTOM, t("Audience"), egui::FontId::proportional(12.0), theme::TEXT_DIM);
    let bad: HashSet<FixtureId> = issues
        .iter()
        .flat_map(|i| match i {
            PatchIssue::Overlap(a, b) => vec![*a, *b],
            PatchIssue::DuplicateId(f) | PatchIssue::UnknownType(f) | PatchIssue::OutOfRange(f) => vec![*f],
        })
        .collect();
    let radius = 10.0;
    let bar_width = (0.08 * zoom).clamp(4.0, 12.0);
    let hit_fixture = |pos: egui::Pos2, l: &Lighting| {
        l.fixtures.iter().rev().find(|f| to_screen(f.position[0], f.position[2]).distance(pos) <= radius).map(|f| f.id)
    };
    // A shape under the pointer: a bar along its length, a rectangle on its outline (so the
    // inside of a big stage still pans).
    let hit_shape = |pos: egui::Pos2, l: &Lighting| {
        l.shapes
            .iter()
            .rev()
            .find(|s| {
                let (a, b) = (to_screen(s.a[0], s.a[1]), to_screen(s.b[0], s.b[1]));
                match s.kind {
                    ShapeKind::Bar => segment_distance(pos, a, b) <= bar_width / 2.0 + 4.0,
                    ShapeKind::Rect => {
                        let c = [a, to_screen(s.b[0], s.a[1]), b, to_screen(s.a[0], s.b[1])];
                        (0..4).any(|i| segment_distance(pos, c[i], c[(i + 1) % 4]) <= 6.0)
                    }
                }
            })
            .map(|s| s.id)
    };
    let hit_point = |pos: egui::Pos2, l: &Lighting| l.points.iter().rev().find(|p| to_screen(p.position[0], p.position[2]).distance(pos) <= 8.0).map(|p| p.id);
    let hit_handle = |pos: egui::Pos2, l: &Lighting, sel: Option<u32>| {
        let s = l.shapes.iter().find(|s| Some(s.id) == sel)?;
        handles(s).into_iter().find(|(_, w)| to_screen(w[0], w[1]).distance(pos) <= 8.0).map(|(h, _)| (s.id, h))
    };
    let pointer = resp.interact_pointer_pos();
    let m = ui.input(|i| i.modifiers);
    let drawing = light.plan_tool != PlanTool::Select;
    let hit_label = |pos: egui::Pos2, labels: &[(u32, egui::Rect)]| labels.iter().rev().find(|(_, r)| r.expand(2.0).contains(pos)).map(|x| x.0);
    if resp.clicked()
        && let Some(pos) = pointer
        && !drawing
    {
        let sel = &mut light.patch_sel;
        if hit_handle(pos, l, light.shape_sel).is_some() {
            // A click on a handle keeps things as they are.
        } else if let Some(id) = hit_fixture(pos, l) {
            if m.command || m.shift {
                if !sel.remove(&id) {
                    sel.insert(id);
                }
            } else {
                *sel = [id].into_iter().collect();
            }
        } else if let Some(id) = hit_label(pos, &light.plan_labels).or_else(|| hit_shape(pos, l)) {
            light.shape_sel = Some(id);
        } else if !(m.command || m.shift) {
            sel.clear();
            light.shape_sel = None;
        }
    }
    if resp.drag_started()
        && let Some(pos) = pointer
    {
        light.plan_drag = Some(if drawing {
            // Draw a new shape from here.
            let w = to_world(pos);
            let at = [snap(w.x), snap(w.y)];
            let kind = if light.plan_tool == PlanTool::Bar { ShapeKind::Bar } else { ShapeKind::Rect };
            let id = l.shapes.iter().map(|s| s.id).max().unwrap_or(0) + 1;
            let name = match kind {
                ShapeKind::Bar => format!("LX{}", l.shapes.iter().filter(|s| s.kind == ShapeKind::Bar).count() + 1),
                ShapeKind::Rect => String::new(),
            };
            let height = if kind == ShapeKind::Bar { 6.0 } else { 0.0 };
            let s = PlotShape { id, kind, name, a: at, b: at, height, label: [0.0, 0.0] };
            l.shapes.push(s.clone());
            light.shape_sel = Some(id);
            PlanDrag::Shape { id, handle: Handle::B, from: pos, start: s, new: true }
        } else if let Some((id, handle)) = hit_handle(pos, l, light.shape_sel) {
            let start = l.shapes.iter().find(|s| s.id == id).cloned().unwrap_or_default();
            PlanDrag::Shape { id, handle, from: pos, start, new: false }
        } else if let Some(id) = hit_point(pos, l) {
            PlanDrag::Point { id, from: pos, start: l.point(id).map_or([0.0; 3], |p| p.position) }
        } else if let Some(id) = hit_fixture(pos, l) {
            if !light.patch_sel.contains(&id) {
                light.patch_sel = [id].into_iter().collect();
            }
            let start = l.fixtures.iter().filter(|f| light.patch_sel.contains(&f.id)).map(|f| (f.id, f.position)).collect();
            PlanDrag::Fixtures { from: pos, start }
        } else if let Some(id) = hit_label(pos, &light.plan_labels) {
            light.shape_sel = Some(id);
            let start = l.shapes.iter().find(|s| s.id == id).map_or([0.0, 0.0], |s| s.label);
            PlanDrag::Label { id, from: pos, start }
        } else if let Some(id) = hit_shape(pos, l) {
            light.shape_sel = Some(id);
            let start = l.shapes.iter().find(|s| s.id == id).cloned().unwrap_or_default();
            PlanDrag::Shape { id, handle: Handle::Body, from: pos, start, new: false }
        } else {
            PlanDrag::Pan
        });
    }
    if resp.dragged() {
        match &light.plan_drag {
            Some(PlanDrag::Pan) => light.plan_pan += resp.drag_delta(),
            Some(PlanDrag::Fixtures { from, start }) => {
                let d = (pointer.unwrap_or(*from) - *from) / zoom;
                for (id, p0) in start {
                    if let Some(f) = l.fixtures.iter_mut().find(|f| f.id == *id) {
                        f.position[0] = snap(p0[0] + d.x);
                        f.position[2] = snap(p0[2] + d.y);
                    }
                }
            }
            Some(PlanDrag::Shape { id, handle, from, start, .. }) => {
                let d = (pointer.unwrap_or(*from) - *from) / zoom;
                if let Some(s) = l.shapes.iter_mut().find(|s| s.id == *id) {
                    *s = moved(start, *handle, d);
                }
            }
            Some(PlanDrag::Label { id, from, start }) => {
                let d = (pointer.unwrap_or(*from) - *from) / zoom;
                if let Some(s) = l.shapes.iter_mut().find(|s| s.id == *id) {
                    s.label = [snap(start[0] + d.x), snap(start[1] + d.y)];
                }
            }
            Some(PlanDrag::Point { id, from, start }) => {
                let d = (pointer.unwrap_or(*from) - *from) / zoom;
                if let Some(p) = l.points.iter_mut().find(|p| p.id == *id) {
                    p.position = [snap(start[0] + d.x), start[1], snap(start[2] + d.y)];
                }
            }
            None => {}
        }
    }
    if resp.drag_stopped() {
        // A new shape too small to see was a slip: drop it. Drawing tools are one-shot.
        if let Some(PlanDrag::Shape { id, new: true, .. }) = light.plan_drag
            && l.shapes.iter().find(|s| s.id == id).is_some_and(|s| (s.b[0] - s.a[0]).abs().max((s.b[1] - s.a[1]).abs()) < 0.1)
        {
            l.shapes.retain(|s| s.id != id);
            light.shape_sel = None;
        }
        light.plan_drag = None;
        light.plan_tool = PlanTool::Select;
    }
    if light.shape_sel.is_some() && resp.hovered() && ui.input(|i| i.key_pressed(egui::Key::Delete)) && !ui.ctx().egui_wants_keyboard_input() {
        l.shapes.retain(|s| Some(s.id) != light.shape_sel);
        light.shape_sel = None;
    }
    // Rectangles under bars under fixtures. Labels sit where the user dragged them (`label`).
    let label_font = egui::FontId::proportional(11.0);
    light.plan_labels.clear();
    for kind in [ShapeKind::Rect, ShapeKind::Bar] {
        for s in l.shapes.iter().filter(|s| s.kind == kind) {
            let selected = light.shape_sel == Some(s.id);
            let (a, b) = (to_screen(s.a[0], s.a[1]), to_screen(s.b[0], s.b[1]));
            let colour = if selected { theme::ACCENT } else { theme::TEXT_DIM };
            let moved = egui::vec2(s.label[0], s.label[1]) * zoom;
            match kind {
                ShapeKind::Rect => {
                    let r = egui::Rect::from_two_pos(a, b);
                    p.rect(r, 0.0, theme::BG_RAISED.gamma_multiply(0.6), egui::Stroke::new(if selected { 2.0 } else { 1.5 }, colour), egui::StrokeKind::Middle);
                    if !s.name.is_empty() {
                        let at = p.text(r.left_top() + egui::vec2(6.0, 4.0) + moved, egui::Align2::LEFT_TOP, &s.name, label_font.clone(), colour);
                        light.plan_labels.push((s.id, at));
                    }
                }
                ShapeKind::Bar => {
                    p.line_segment([a, b], egui::Stroke::new(bar_width, colour));
                    let label = if s.name.is_empty() { format!("{:.2} m", s.height) } else { format!("{} · {:.2} m", s.name, s.height) };
                    // At the left end, just above the bar.
                    let left = if a.x <= b.x { a } else { b };
                    let at = p.text(left - egui::vec2(0.0, bar_width / 2.0 + 3.0) + moved, egui::Align2::LEFT_BOTTOM, label, label_font.clone(), colour);
                    light.plan_labels.push((s.id, at));
                }
            }
            if selected {
                for (_, w) in handles(s) {
                    p.rect_filled(egui::Rect::from_center_size(to_screen(w[0], w[1]), egui::vec2(9.0, 9.0)), 2.0, Color32::WHITE);
                }
            }
        }
    }
    for f in &l.fixtures {
        let c = to_screen(f.position[0], f.position[2]);
        if !rect.expand(radius).contains(c) {
            continue;
        }
        let selected = light.patch_sel.contains(&f.id);
        let fill = if selected { theme::ACCENT } else { theme::BG_RAISED };
        p.circle(c, radius, fill, egui::Stroke::new(1.0, if bad.contains(&f.id) { theme::AMBER } else { theme::TEXT_DIM }));
        // Heading: the rotation round the vertical axis (0 = towards the audience).
        let a = f.rotation[1].to_radians();
        p.line_segment([c, c + egui::vec2(a.sin(), a.cos()) * (radius + 5.0)], egui::Stroke::new(1.5, theme::TEXT_DIM));
        p.text(c, egui::Align2::CENTER_CENTER, f.id.to_string(), egui::FontId::proportional(10.0), if selected { Color32::WHITE } else { theme::TEXT });
    }
    // Focus points: a ring with a cross, and the name.
    for pt in &l.points {
        let c = to_screen(pt.position[0], pt.position[2]);
        if !rect.expand(40.0).contains(c) {
            continue;
        }
        let stroke = egui::Stroke::new(1.5, theme::GREEN);
        p.circle_stroke(c, 6.0, stroke);
        p.line_segment([c - egui::vec2(9.0, 0.0), c + egui::vec2(9.0, 0.0)], stroke);
        p.line_segment([c - egui::vec2(0.0, 9.0), c + egui::vec2(0.0, 9.0)], stroke);
        let name = if pt.name.is_empty() { tr!("Point {n}", n = pt.id.to_string()) } else { pt.name.clone() };
        p.text(c + egui::vec2(9.0, 3.0), egui::Align2::LEFT_TOP, name, label_font.clone(), theme::GREEN);
    }
    if drawing {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    }
    if let Some(pos) = resp.hover_pos() {
        let on_label = !drawing && hit_fixture(pos, l).is_none() && hit_label(pos, &light.plan_labels).is_some();
        if on_label {
            ui.ctx().set_cursor_icon(if light.plan_drag.is_some() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        }
        if on_label {
            resp.on_hover_text(t("Drag to move the label"));
        } else if let Some(pt) = hit_point(pos, l).and_then(|id| l.point(id)) {
            let [x, y, z] = to_stage(pt.position);
            resp.on_hover_text(format!("{} {}\nx {x:.2} · y {y:.2} · z {z:.2} m\n{}", pt.id, pt.name, t("Drag to move the focus point; cues aimed at it follow")));
        } else if let Some(f) = hit_fixture(pos, l).and_then(|id| l.fixture(id)) {
            let [x, y, z] = to_stage(f.position);
            let name = if f.name.is_empty() { f.fixture_type.clone() } else { f.name.clone() };
            resp.on_hover_text(format!("{} {name}\nx {x:.2} · y {y:.2} · z {z:.2} m", f.id));
        } else if let Some(s) = hit_shape(pos, l).and_then(|id| l.shapes.iter().find(|s| s.id == id)) {
            let text = match s.kind {
                ShapeKind::Bar => tr!("Bar {name}: {len} m long, at {h} m", name = s.name.clone(), len = format!("{:.2}", s.length()), h = format!("{:.2}", s.height)),
                ShapeKind::Rect => tr!(
                    "{name} {w} × {d} m",
                    name = s.name.clone(),
                    w = format!("{:.2}", (s.b[0] - s.a[0]).abs()),
                    d = format!("{:.2}", (s.b[1] - s.a[1]).abs())
                ),
            };
            resp.on_hover_text(text);
        }
    }
}

/// Work `code` out for effect `e` on the formula thread (the code is stored in the effect first).
fn start_formula(light: &mut LightUi, e: &mut Effect, code: String, fresh: bool) {
    e.formula = code.clone();
    light.formula_buf = Some((e.id, code.clone()));
    light.formula_run = Some((e.id, formula::Run::start(code, e.params.clone()), fresh));
    light.formula_out = None;
}

/// Take a finished formula into the design: its first effect replaces the table and sliders of
/// the effect it belongs to (a new effect also takes its name), further effects are added to the
/// pool. Playback settings (speed, size, spread…) stay the effect's own.
fn poll_formula(light: &mut LightUi, l: &mut Lighting) {
    let Some((id, run, fresh)) = &light.formula_run else { return };
    let Some(result) = run.poll() else { return };
    let (id, fresh) = (*id, *fresh);
    light.formula_run = None;
    let err = match result {
        Ok(out) => {
            let mut made = out.effects.into_iter();
            if let (Some((name, table)), Some(e)) = (made.next(), l.effects.iter_mut().find(|e| e.id == id)) {
                e.shape = Shape::Custom;
                e.attribute = table.attributes[0];
                e.table = Some(table.into());
                e.params = out.params;
                if fresh || e.name.is_empty() {
                    e.name = name;
                }
            }
            for (name, table) in made {
                let nid = l.effects.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                l.effects.push(Effect { id: nid, name, shape: Shape::Custom, attribute: table.attributes[0], table: Some(table.into()), ..Default::default() });
            }
            None
        }
        Err(e) => Some(e.to_string()),
    };
    light.formula_out = Some((id, err));
}

/// The formula of a custom effect: sliders, editor, Run and the result of the last run.
fn formula_card(ui: &mut Ui, light: &mut LightUi, e: &mut Effect) {
    theme::card_header(ui, icons::CODE, t("Formula"));
    ui.label(RichText::new(t("One line per attribute, worked out over one cycle for each pattern row. The User manual lists what you can write.")).small().color(theme::TEXT_DIM));
    ui.add_space(4.0);
    if light.formula_buf.as_ref().is_none_or(|(id, _)| *id != e.id) {
        light.formula_buf = Some((e.id, e.formula.clone()));
    }
    let running = light.formula_run.as_ref().is_some_and(|r| r.0 == e.id);
    let mut run = false;
    ui.horizontal(|ui| {
        if running {
            ui.spinner();
        } else {
            run = ui.button(theme::with_icon(icons::PLAY_ARROW, t("Run"))).on_hover_text(t("Ctrl+Enter in the editor")).clicked();
        }
    });
    // The formula's sliders: a change works it out again (once the slider is let go).
    let mut rerun = false;
    if !e.params.is_empty() {
        egui::Grid::new(("formula_params", e.id)).num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            for p in &mut e.params {
                ui.label(p.name.replace('_', " "));
                let mut s = egui::Slider::new(&mut p.value, p.min..=p.max).clamping(egui::SliderClamping::Always);
                if p.integer {
                    s = s.integer();
                }
                let r = ui.add(s);
                rerun |= r.drag_stopped() || (r.changed() && !r.dragged());
                ui.end_row();
            }
        });
    }
    if rerun && !running {
        run = true;
    }
    let (_, buf) = light.formula_buf.as_mut().expect("set");
    // Syntax colours (the layout job is rebuilt per frame only while the editor is drawn).
    let mut layouter = |ui: &Ui, text: &dyn egui::TextBuffer, wrap: f32| {
        ui.fonts_mut(|f| f.layout_job(super::code::highlight(text.as_str(), theme::mono(13.0), wrap)))
    };
    let r = egui::Frame::new().fill(theme::BG_INPUT).corner_radius(6.0).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
        ui.add(
            egui::TextEdit::multiline(buf)
                .code_editor()
                .frame(egui::Frame::NONE)
                .font(theme::mono(13.0))
                .desired_rows(12)
                .desired_width(ui.available_width())
                .lock_focus(true)
                .layouter(&mut layouter),
        )
    }).inner;
    if r.has_focus() && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter)) {
        run = !running;
    }
    if r.lost_focus() && *buf != e.formula {
        e.formula = buf.clone();
    }
    if run {
        let code = buf.clone();
        start_formula(light, e, code, false);
    }
    if let Some((_, err)) = light.formula_out.as_ref().filter(|o| o.0 == e.id) {
        match err {
            Some(err) => ui.label(RichText::new(theme::with_icon(icons::ERROR, err)).color(theme::RED)),
            None => ui.label(RichText::new(theme::with_icon(icons::CHECK_CIRCLE, t("Effect generated."))).color(theme::GREEN)),
        };
    }
    // A Python script from OpenQ 0.3: shown so it can be rewritten as a formula, never run.
    if !e.script.is_empty() {
        ui.add_space(6.0);
        let mut remove = false;
        egui::CollapsingHeader::new(t("Python script from an older version (no longer run)")).id_salt(("old_script", e.id)).show(ui, |ui| {
            ui.label(RichText::new(t("Rewrite it as a formula above; the table it made still plays.")).small().color(theme::TEXT_DIM));
            let mut text = e.script.as_str();
            ui.add(egui::TextEdit::multiline(&mut text).code_editor().font(theme::mono(12.0)).desired_width(ui.available_width()).desired_rows(6));
            remove = ui.button(theme::with_icon(icons::DELETE, t("Remove the old script"))).clicked();
        });
        if remove {
            e.script.clear();
        }
    }
}

/// A DragValue editing a 0..=1 level as a percentage.
fn percent_drag(v: &mut f32) -> DragValue<'_> {
    DragValue::from_get_set(move |n| {
        if let Some(n) = n {
            *v = (n as f32 / 100.0).clamp(0.0, 1.0);
        }
        (*v * 100.0) as f64
    })
    .range(0.0..=100.0)
    .suffix(" %")
    .speed(0.5)
}

/// Values for a fan across `n` fixtures: from `a` to `b` in selection order, or centre-out
/// (the middle at `a`, both ends at `b`).
fn fan_values(n: usize, a: f32, b: f32, centre: bool) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let k = if n < 2 { 0.0 } else { i as f32 / (n - 1) as f32 };
            let k = if centre { (2.0 * k - 1.0).abs() } else { k };
            a + (b - a) * k
        })
        .collect()
}

/// Attributes of a fixture or cell (its channels', plus a virtual dimmer).
fn attributes_of(l: &Lighting, id: FixtureId) -> Vec<Attribute> {
    l.unit_attributes(id)
}

fn feature_icon(f: Feature) -> &'static str {
    match f {
        Feature::Intensity => icons::BRIGHTNESS_6,
        Feature::Colour => icons::PALETTE,
        Feature::Position => icons::CONTROL_CAMERA,
        Feature::Beam => icons::FLASHLIGHT_ON,
        Feature::Control => icons::SETTINGS,
    }
}

/// Sort key of a cue number ("2.5" → 2.5; non-numbers last).
fn number_key(n: &str) -> f64 {
    n.trim().parse::<f64>().unwrap_or(f64::MAX)
}

/// The number after the last numbered cue.
fn next_cue_number(s: &Sequence) -> String {
    let last = s.cues.iter().map(|c| number_key(&c.number)).filter(|n| *n < f64::MAX).fold(0.0, f64::max);
    format!("{}", last.floor() as i64 + 1)
}

impl App {
    fn programmer_page(&mut self, ui: &mut Ui, l: &mut Lighting) {
        // Presets in a column of their own (a show has many), the full height of the page.
        // Two containers: groups and focus points on top, then the feature filter with the presets and effects it
        // filters. No divider line, a gap instead (dragging that gap, the panel's edge, still resizes it).
        egui::Panel::left("presets")
            .frame(egui::Frame::new().outer_margin(egui::Margin { left: 0, right: 10, top: 0, bottom: 0 }))
            .show_separator_line(false)
            .resizable(true)
            .default_size(250.0)
            .size_range(190.0..=480.0)
            .show(ui, |ui| {
                let total = ui.available_height();
                let pane = || egui::Frame::new().fill(theme::BG_PANEL).corner_radius(8.0).inner_margin(egui::Margin::same(10));
                pane().show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    self.group_pool(ui, l, (total * 0.22).max(70.0));
                    ui.add_space(8.0);
                    self.point_pool(ui, l, (total * 0.14).max(56.0));
                });
                ui.add_space(8.0);
                pane().show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.set_min_height(ui.available_height() - 10.0);
                    self.pool_filter(ui);
                    ui.add_space(6.0);
                    self.preset_pool(ui, l, (total * 0.4).max(110.0));
                    ui.add_space(8.0);
                    self.effect_pool(ui, l);
                });
            });
        // Programmer actions.
        theme::wrapped_row(ui, |ui| {
            let n = self.light.selection.len();
            ui.label(RichText::new(tr!("{n} selected", n = n.to_string())).color(theme::TEXT_DIM));
            if ui.button(theme::with_icon(icons::SELECT_ALL, t("Select all"))).clicked() {
                self.light.selection = l.units();
            }
            if ui.button(theme::with_icon(icons::DESELECT, t("Select none"))).clicked() {
                self.light.selection.clear();
            }
            let busy = !self.light.programmer.is_empty() || !self.light.programmer_effects.is_empty();
            theme::separated(ui, "clear", |ui| {
                if ui.add_enabled(busy, egui::Button::new(theme::with_icon(icons::CLOSE, t("Clear programmer")))).clicked() {
                    self.light.programmer.clear();
                    self.light.programmer_effects.clear();
                }
            });
            if !l.inputs.is_empty()
                && ui
                    .button(theme::with_icon(icons::DOWNLOAD, t("Capture input")))
                    .on_hover_text(t("Take what the DMX inputs receive into the programmer (the selection, or every fixture that isn't at its defaults)"))
                    .clicked()
            {
                self.light.capture = true;
            }
            if busy {
                let n: usize = self.light.programmer.values().map(|v| v.len()).sum();
                ui.label(RichText::new(tr!("{n} values in the programmer", n = n.to_string())).color(theme::RED));
            }
            theme::separated(ui, "look", |ui| {
                let toggle = |ui: &mut Ui, on: &mut bool, glyph: &str, label: &str, tip: &str, color: Color32| {
                    let (fill, fg) = if *on { (color, Color32::WHITE) } else { (theme::BG_RAISED, theme::TEXT) };
                    if ui.add(egui::Button::new(RichText::new(theme::with_icon(glyph, label)).color(fg)).fill(fill)).on_hover_text(tip).clicked() {
                        *on = !*on;
                    }
                };
                toggle(ui, &mut self.light.blind, icons::VISIBILITY, t("Blind"), t("Program without changing the output"), theme::ACCENT);
                toggle(ui, &mut self.light.highlight, icons::HIGHLIGHT, t("Highlight"), t("Selected fixtures at open white, full (not recorded)"), theme::AMBER);
                if self.light.highlight && self.light.selection.len() > 1 {
                    let n = self.light.selection.len();
                    if ui.button(icons::ARROW_RIGHT).on_hover_text(t("Next fixture of the selection")).clicked() {
                        self.light.step = Some(self.light.step.map_or(0, |i| (i + 1) % n));
                    }
                    if ui.button(icons::SELECT_ALL).on_hover_text(t("Whole selection")).clicked() {
                        self.light.step = None;
                    }
                    if let Some(i) = self.light.step
                        && let Some(f) = self.light.selection.get(i % n)
                    {
                        ui.label(RichText::new(tr!("fixture {f}", f = fixture_label(*f))).color(theme::AMBER));
                    }
                }
            });
            self.record_controls(ui, l);
        });
        ui.separator();
        // Encoders in a side panel of their own (resizable, scrolls), tiles fill the rest.
        // No outer gap: the tiles' scroll-bar gutter makes it, as wide as the presets column's.
        egui::Panel::right("encoders")
            .frame(side_column(false).outer_margin(egui::Margin::ZERO))
            .show_separator_line(false)
            .resizable(true)
            .default_size(420.0)
            .size_range(320.0..=640.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("encoders_scroll").auto_shrink(false).show(ui, |ui| self.encoders(ui, l));
            });
        ui.add_space(2.0);
        self.fixture_tiles(ui, l);
    }

    /// Feature filter of the presets and effects (also the feature "new preset" stores).
    fn pool_filter(&mut self, ui: &mut Ui) {
        theme::wrapped_row(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let f = &mut self.light.preset_filter;
            if ui.selectable_label(f.is_none(), t("All")).on_hover_text(t("Every preset and effect")).clicked() {
                *f = None;
            }
            for feat in Feature::ALL {
                let r = ui.selectable_label(*f == Some(feat), RichText::new(feature_icon(feat)).font(theme::icon_font(16.0))).on_hover_text(feat.label());
                if r.clicked() {
                    *f = if *f == Some(feat) { None } else { Some(feat) };
                }
            }
        });
    }

    /// Groups as cards: click selects the group (Ctrl/Shift adds it), right-click overwrites or
    /// deletes; + makes one from the selection.
    fn group_pool(&mut self, ui: &mut Ui, l: &mut Lighting, max_h: f32) {
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("Groups")));
            ui.label(RichText::new(l.groups.len().to_string()).font(theme::semibold(10.5)).color(theme::TEXT_DIM));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!self.light.selection.is_empty(), egui::Button::new(icons::ADD))
                    .on_hover_text(t("Group from selection; right-click a group to overwrite or delete it"))
                    .clicked()
                {
                    let id = l.groups.iter().map(|g| g.id).max().unwrap_or(0) + 1;
                    l.groups.push(Group { id, name: String::new(), fixtures: self.light.selection.clone() });
                }
            });
        });
        ui.add_space(4.0);
        let mut remove = None;
        let mut pick = None;
        let selected: HashSet<FixtureId> = self.light.selection.iter().copied().collect();
        egui::ScrollArea::vertical().id_salt("group_cards").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            card_grid(ui, &l.groups, |ui, g, size| {
                let on = !g.fixtures.is_empty() && g.fixtures.iter().all(|f| selected.contains(f));
                let name = if g.name.is_empty() { tr!("{n} fixtures", n = g.fixtures.len().to_string()) } else { g.name.clone() };
                let r = pool_card(ui, size, icons::SELECT_ALL, g.id, &name, if on { CardLook::On } else { CardLook::Off }, None);
                let r = r.on_hover_text(format!("{} {} · {}", g.id, g.name, tr!("{n} fixtures", n = g.fixtures.len().to_string())));
                if r.clicked() {
                    pick = Some(g.id);
                }
                r.context_menu(|ui| {
                    if ui.button(theme::with_icon(icons::SAVE, t("Store the selection here"))).clicked() {
                        remove = Some((g.id, false));
                    }
                    if ui.button(theme::with_icon(icons::DELETE, t("Delete group"))).clicked() {
                        remove = Some((g.id, true));
                    }
                });
            });
            if l.groups.is_empty() {
                ui.label(RichText::new(t("Select fixtures, then make a group of them with +.")).color(theme::TEXT_DIM));
            }
        });
        if let Some(g) = pick.and_then(|id| l.groups.iter().find(|g| g.id == id)) {
            if !ui.input(|i| i.modifiers.command || i.modifiers.shift) {
                self.light.selection.clear();
            }
            let mut have: HashSet<FixtureId> = self.light.selection.iter().copied().collect();
            self.light.selection.extend(g.fixtures.iter().copied().filter(|f| have.insert(*f)));
        }
        match remove {
            Some((id, true)) => l.groups.retain(|g| g.id != id),
            Some((id, false)) => {
                if let Some(g) = l.groups.iter_mut().find(|g| g.id == id) {
                    g.fixtures = self.light.selection.clone();
                }
            }
            None => {}
        }
    }

    /// Presets panel: "new preset" (of the filter's feature), then the presets as cards. Click
    /// applies one to the selection (as references), right-click overwrites/deletes.
    fn preset_pool(&mut self, ui: &mut Ui, l: &mut Lighting, max_h: f32) {
        let feature = self.light.preset_filter;
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("Presets")));
            ui.label(RichText::new(l.presets.len().to_string()).font(theme::semibold(10.5)).color(theme::TEXT_DIM));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let values = self.programmer_values(l, feature);
                let what = feature.map_or_else(|| t("All attributes").to_string(), |f| f.label().to_string());
                if ui
                    .add_enabled(!values.is_empty(), egui::Button::new(icons::ADD))
                    .on_hover_text(tr!(
                        "Preset from programmer ({what}): stores the selected fixtures' programmer values; right-click a preset to overwrite or delete it",
                        what = what
                    ))
                    .clicked()
                {
                    let id = l.presets.iter().map(|p| p.id).max().unwrap_or(0) + 1;
                    l.presets.push(Preset { id, name: String::new(), feature, values: values.into() });
                }
            });
        });
        ui.add_space(4.0);
        enum Act {
            Apply(PresetId),
            Store(PresetId),
            Delete(PresetId),
        }
        let mut action = None;
        let filter = self.light.preset_filter;
        let shown: Vec<&Preset> = l.presets.iter().filter(|p| filter.is_none() || p.feature == filter).collect();
        // Presets the selection refers to are marked.
        let used: HashSet<PresetId> = self
            .light
            .selection
            .iter()
            .filter_map(|f| self.light.programmer.get(f))
            .flat_map(|v| v.values())
            .filter_map(|v| match v {
                Value::Preset { preset } => Some(*preset),
                Value::Level(_) | Value::Point { .. } => None,
            })
            .collect();
        egui::ScrollArea::vertical().id_salt("preset_cards").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            card_grid(ui, &shown, |ui, p, size| {
                let glyph = p.feature.map_or(icons::CATEGORY, feature_icon);
                let name = if p.name.is_empty() { p.feature.map_or_else(|| t("All attributes").to_string(), |f| f.label().to_string()) } else { p.name.clone() };
                // A colour preset shows its colour (from its first fixture).
                let colour = (p.feature == Some(Feature::Colour))
                    .then(|| p.values.values().next())
                    .flatten()
                    .and_then(|v| Some(to_c32([*v.get(&Attribute::Red)?, *v.get(&Attribute::Green)?, *v.get(&Attribute::Blue)?])));
                let r = pool_card(ui, size, glyph, p.id, &name, if used.contains(&p.id) { CardLook::On } else { CardLook::Off }, colour);
                let r = r.on_hover_text(format!("{} {} · {}", p.id, p.name, p.feature.map_or_else(|| t("All attributes").to_string(), |f| f.label().to_string())));
                if r.clicked() {
                    action = Some(Act::Apply(p.id));
                }
                r.context_menu(|ui| {
                    if ui.button(theme::with_icon(icons::SAVE, t("Store the programmer here"))).clicked() {
                        action = Some(Act::Store(p.id));
                    }
                    if ui.button(theme::with_icon(icons::DELETE, t("Delete preset"))).clicked() {
                        action = Some(Act::Delete(p.id));
                    }
                });
            });
            if l.presets.is_empty() {
                ui.label(RichText::new(t("Set values in the programmer, then store them as a preset with +.")).color(theme::TEXT_DIM));
            }
        });
        match action {
            Some(Act::Apply(id)) => self.apply_preset(l, id),
            Some(Act::Delete(id)) => {
                crate::light::model::unlink_preset(l, id, &mut self.light.programmer);
                l.delete_preset(id);
            }
            Some(Act::Store(id)) => {
                let feature = l.preset(id).and_then(|p| p.feature);
                let values = self.programmer_values(l, feature);
                if let Some(p) = l.presets.iter_mut().find(|p| p.id == id) {
                    p.values = values.into();
                }
            }
            None => {}
        }
    }

    /// Effects (under the presets) as cards: click runs the effect on the selection from the
    /// programmer (click again stops it), right-click stops it in the recorded cue.
    fn effect_pool(&mut self, ui: &mut Ui, l: &Lighting) {
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("Effects")));
            ui.label(RichText::new(l.effects.len().to_string()).font(theme::semibold(10.5)).color(theme::TEXT_DIM));
        });
        ui.add_space(4.0);
        let filter = self.light.preset_filter.map(FxFilter::Feature);
        let shown: Vec<&Effect> = l.effects.iter().filter(|e| fx_matches(e, "", filter)).collect();
        egui::ScrollArea::vertical().id_salt("effect_cards").auto_shrink(false).show(ui, |ui| {
            card_grid(ui, &shown, |ui, e, size| {
                let stopping = self.light.programmer_effects.iter().any(|r| r.effect == e.id && r.stop);
                let running = self.light.programmer_effects.iter().any(|r| r.effect == e.id && !r.stop);
                let look = if running { CardLook::Running } else if stopping { CardLook::Stopping } else { CardLook::Off };
                let name = if e.name.is_empty() { e.shape.label().to_string() } else { e.name.clone() };
                let r = pool_card(ui, size, icons::AUTO_FIX_HIGH, e.id, &name, look, None);
                let r = r.on_hover_text(format!("{} {} · {} · {}", e.id, e.name, e.attribute.label(), e.shape.label()));
                if r.clicked() {
                    if running || stopping {
                        self.light.programmer_effects.retain(|r| r.effect != e.id);
                    } else if !self.light.selection.is_empty() {
                        self.light.programmer_effects.push(EffectRun { effect: e.id, fixtures: self.light.selection.clone(), stop: false });
                    }
                }
                r.context_menu(|ui| {
                    if ui.button(theme::with_icon(icons::STOP, t("Stop it in the recorded cue"))).clicked() {
                        self.light.programmer_effects.retain(|r| r.effect != e.id);
                        self.light.programmer_effects.push(EffectRun { effect: e.id, fixtures: Vec::new(), stop: true });
                    }
                });
            });
            if l.effects.is_empty() {
                ui.label(RichText::new(t("Build effects on the Effects page.")).color(theme::TEXT_DIM));
            }
        });
    }

    /// The effects pool and the editor of the chosen effect, with a live preview.
    fn effects_page(&mut self, ui: &mut Ui, l: &mut Lighting) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
        poll_formula(&mut self.light, l);
        let lib = self.light.fx_lib.get_or_insert_with(Default::default).clone();
        if let Some(s) = lib.snapshot_if_newer(self.light.fx_snap.generation) {
            self.light.fx_snap = s;
        }
        let results = std::mem::take(&mut *self.light.fx_results.lock());
        for r in results {
            match r {
                Ok(m) => self.notify(m),
                Err(e) => self.notify_error(e),
            }
        }
        ui.horizontal_top(|ui| {
            egui::ScrollArea::vertical().id_salt("effect_lists").max_width(250.0).auto_shrink([false, false]).show(ui, |ui| ui.vertical(|ui| {
                ui.set_width(240.0);
                // Search and attribute filter, for the show's effects and both libraries.
                ui.add(egui::TextEdit::singleline(&mut self.light.fx_search).hint_text(t("Search effects")).desired_width(240.0));
                let label = self.light.fx_filter.map_or_else(|| t("All attributes").to_string(), FxFilter::label);
                egui::ComboBox::from_id_salt("fx_filter").selected_text(label).width(240.0).height(420.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.light.fx_filter, None, t("All attributes"));
                    ui.separator();
                    for f in Feature::ALL {
                        ui.selectable_value(&mut self.light.fx_filter, Some(FxFilter::Feature(f)), FxFilter::Feature(f).label());
                    }
                    ui.separator();
                    for a in Attribute::ALL {
                        ui.selectable_value(&mut self.light.fx_filter, Some(FxFilter::Attribute(a)), a.label());
                    }
                });
                ui.add_space(6.0);
                let (search, filter) = (self.light.fx_search.trim().to_lowercase(), self.light.fx_filter);
                let shown = |e: &Effect| fx_matches(e, &search, filter);
                ui.horizontal(|ui| {
                    ui.label(theme::caption(t("Effects")));
                    if ui.small_button(icons::ADD).on_hover_text(t("New effect")).clicked() {
                        let id = l.effects.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                        l.effects.push(Effect { id, ..Default::default() });
                        self.light.effect = Some(id);
                    }
                    if ui.small_button(icons::CODE).on_hover_text(t("New formula effect")).clicked() {
                        let id = l.effects.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                        let placeholder = EffectTable { attributes: vec![Attribute::Dimmer], steps: 1, width: 1, smooth: false, values: vec![0.0] };
                        l.effects.push(Effect { id, shape: Shape::Custom, table: Some(placeholder.into()), ..Default::default() });
                        self.light.effect = Some(id);
                        let e = l.effects.last_mut().expect("pushed");
                        start_formula(&mut self.light, e, formula::EXAMPLE.to_string(), true);
                    }
                    if ui.small_button(icons::UPLOAD_FILE).on_hover_text(t("Import effect files")).clicked() {
                        self.deferred.push(super::Deferred::ImportEffects);
                    }
                });
                let mut delete = None;
                for e in l.effects.iter().filter(|e| shown(e)) {
                    ui.horizontal(|ui| {
                        let name = if e.name.is_empty() { format!("{} · {}", e.id, e.shape.label()) } else { format!("{} {}", e.id, e.name) };
                        let label = if e.shape == Shape::Custom { theme::with_icon(icons::CODE, &name) } else { name };
                        if ui.selectable_label(self.light.effect == Some(e.id), label).clicked() {
                            self.light.effect = Some(e.id);
                        }
                        if ui.small_button(icons::DELETE).on_hover_text(t("Delete effect")).clicked() {
                            delete = Some(e.id);
                        }
                    });
                }
                if let Some(id) = delete {
                    l.effects.retain(|e| e.id != id);
                    for c in l.sequences.iter_mut().flat_map(|s| s.cues.iter_mut()) {
                        c.effects.retain(|r| r.effect != id);
                    }
                    self.light.programmer_effects.retain(|r| r.effect != id);
                }
                ui.add_space(12.0);
                self.effect_library(ui, l, &lib);
                ui.add_space(12.0);
                // Built-in effects: click adds a copy to the show.
                ui.label(theme::caption(t("Built-in")));
                for b in crate::light::effects::builtins().into_iter().filter(|b| shown(&b.effect)) {
                    let e = &b.effect;
                    let attrs: Vec<&str> = e.attributes().iter().map(|a| a.label()).collect();
                    let label = if e.shape == Shape::Custom { theme::with_icon(icons::PALETTE, &e.name) } else { e.name.clone() };
                    let tip = format!("{}\n{} · {}\n{}", b.description, attrs.join(", "), e.shape.label(), t("Add to the show"));
                    if ui.button(label).on_hover_text(tip).clicked() {
                        let id = l.effects.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                        l.effects.push(Effect { id, ..b.effect.clone() });
                        self.light.effect = Some(id);
                    }
                }
            }));
            ui.separator();
            egui::ScrollArea::vertical().id_salt("effect_scroll").auto_shrink(false).show(ui, |ui| ui.vertical(|ui| {
                let Some(e) = self.light.effect.and_then(|id| l.effects.iter_mut().find(|e| e.id == id)) else {
                    ui.label(RichText::new(t("Choose or add an effect.")).color(theme::TEXT_DIM));
                    return;
                };
                // Settings and, to their right, the preview (under them on a narrow window); the
                // formula of a custom effect in a card across the bottom.
                let total = ui.available_width();
                let side = total >= 800.0;
                let settings_w = if side { 440.0 } else { total };
                // Beside the settings: about half of the rest of the row, at least 320.
                let preview_w = if side { ((total - settings_w - 22.0) * 0.5).max(320.0).min(total - settings_w - 22.0) } else { total };
                let selected: Vec<FixtureId> = self.light.selection.iter().copied().take(24).collect();
                let (bpm, audio) = (self.light.bpm, self.light.audio_level);
                let mut export = false;
                let mut save = false;
                // Returns the card's height (the preview beside it takes the same).
                let settings = |ui: &mut Ui, e: &mut Effect, export: &mut bool, save: &mut bool| -> f32 {
                    theme::card_frame().show(ui, |ui| ui.vertical(|ui| {
                        ui.set_width(settings_w - 26.0);
                        theme::card_header(ui, icons::TUNE, t("Settings"));
                        effect_editor(ui, e, audio);
                        ui.add_space(8.0);
                        theme::wrapped_row(ui, |ui| {
                            *export = ui.button(theme::with_icon(icons::DOWNLOAD, t("Export effect file…"))).clicked();
                            *save = ui
                                .button(theme::with_icon(icons::SAVE, t("Save to library")))
                                .on_hover_text(t("Offer it in every show (replaces a library effect of the same name)"))
                                .clicked();
                        });
                    }))
                    .response
                    .rect
                    .height()
                };
                // `height`: the card's outer height to match (its graph grows or shrinks to fit).
                let preview = |ui: &mut Ui, e: &Effect, height: Option<f32>| {
                    theme::card_frame().show(ui, |ui| ui.vertical(|ui| {
                        ui.set_width(preview_w - 26.0);
                        // Inner height: the card's margins (10 + 10) and border off.
                        let inner = height.map(|h| h - 22.0);
                        if let Some(h) = inner {
                            ui.set_min_height(h);
                        }
                        let top = ui.max_rect().top();
                        theme::card_header(ui, icons::PREVIEW, t("Preview"));
                        let hint = if selected.is_empty() {
                            t("8 sample fixtures: select fixtures to see yours.").to_string()
                        } else {
                            tr!("On the {n} selected fixtures.", n = selected.len().to_string())
                        };
                        ui.label(RichText::new(hint).small().color(theme::TEXT_DIM));
                        let n = if selected.is_empty() { 8 } else { selected.len() };
                        let left = inner.map(|h| h - (ui.cursor().top() - top));
                        effect_preview(ui, e, n, &selected, bpm, left);
                    }));
                };
                if side {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        let h = settings(ui, e, &mut export, &mut save);
                        preview(ui, e, Some(h));
                    });
                } else {
                    settings(ui, e, &mut export, &mut save);
                    ui.add_space(10.0);
                    preview(ui, e, None);
                }
                if export {
                    self.deferred.push(super::Deferred::ExportEffect(e.id));
                }
                if save {
                    let results = self.light.fx_results.clone();
                    let name = if e.name.is_empty() { format!("{} {}", t("Effect"), e.id) } else { e.name.clone() };
                    lib.save(e.clone(), String::new(), String::new(), move |r| {
                        results.lock().push(match r {
                            Ok(()) => Ok(tr!("{name} saved to the effect library", name = name)),
                            Err(e) => Err(tr!("Can't save to the effect library: {e}", e = format!("{e:#}"))),
                        });
                    });
                }
                if e.shape == Shape::Custom {
                    ui.add_space(10.0);
                    theme::card_frame().show(ui, |ui| {
                        ui.set_width(total - 26.0 - 12.0);
                        formula_card(ui, &mut self.light, e);
                    });
                }
            }));
        });
    }

    /// The effect library: its effects (click adds one to the show), remove, open the folder.
    fn effect_library(&mut self, ui: &mut Ui, l: &mut Lighting, lib: &crate::light::fxfile::EffectLibrary) {
        let snap = self.light.fx_snap.clone();
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("Library")));
            if snap.loading {
                ui.spinner();
            }
            if let Some(dir) = lib.dir().map(|d| d.to_path_buf())
                && ui.small_button(icons::FOLDER).on_hover_text(t("Open the effect library folder")).clicked()
                && let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| super::manual::open_with_default_app(&dir))
            {
                self.notify_error(tr!("Can't open {path}: {e}", path = dir.display().to_string(), e = e.to_string()));
            }
            if ui.small_button(icons::REFRESH).on_hover_text(t("Read the folder again")).clicked() {
                lib.reload();
            }
        });
        if snap.items.is_empty() && !snap.loading {
            ui.label(RichText::new(t("Effects saved to the library are offered in every show.")).small().color(theme::TEXT_DIM));
        }
        let mut remove = None;
        let (search, filter) = (self.light.fx_search.trim().to_lowercase(), self.light.fx_filter);
        for item in snap.items.iter().filter(|i| fx_matches(&i.contents.effect, &search, filter)) {
            let e = &item.contents.effect;
            ui.horizontal(|ui| {
                let name = if e.name.is_empty() { item.file.clone() } else { e.name.clone() };
                let label = if e.shape == Shape::Custom { theme::with_icon(icons::CODE, &name) } else { name };
                let mut tip = format!("{}\n{} · {}", t("Add to the show"), e.shape.label(), item.file);
                if !item.contents.description.is_empty() {
                    tip = format!("{}\n{tip}", item.contents.description);
                }
                if ui.button(label).on_hover_text(tip).clicked() {
                    let id = l.effects.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                    l.effects.push(Effect { id, ..e.clone() });
                    self.light.effect = Some(id);
                }
                if ui.small_button(icons::DELETE).on_hover_text(t("Remove from the library")).clicked() {
                    remove = Some(item.file.clone());
                }
            });
        }
        for (file, err) in &snap.errors {
            ui.label(RichText::new(format!("{file}: {err}")).small().color(theme::AMBER));
        }
        if let Some(file) = remove {
            let results = self.light.fx_results.clone();
            lib.remove(file, move |r| {
                if let Err(e) = r {
                    results.lock().push(Err(tr!("Can't remove from the effect library: {e}", e = format!("{e:#}"))));
                }
            });
        }
    }

    /// The command line: Enter runs it, Up/Down walk the history, Esc clears.
    fn command_bar(&mut self, ui: &mut Ui, show: &mut Show) {
        let id = egui::Id::new("light_command_line");
        // Wraps on narrow windows: the masters move to a line of their own and the command line takes the
        // first line's width.
        theme::wrapped_row(ui, |ui| {
            ui.label(RichText::new(icons::KEYBOARD).font(theme::icon_font(18.0)).color(theme::TEXT_DIM));
            // Room for Run, Keypad, the result and the masters.
            let avail = ui.available_width() - 330.0;
            let w = if avail - 470.0 >= 200.0 { avail - 470.0 } else { avail };
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.light.cmd_line)
                    .id(id)
                    .font(theme::mono(14.0))
                    .hint_text(t("1 thru 10 at 50 · group 2 at full · record cue 5 · go 1"))
                    .desired_width(w.max(200.0)),
            );
            if r.has_focus() {
                let (up, down, esc) = ui.input(|i| (i.key_pressed(egui::Key::ArrowUp), i.key_pressed(egui::Key::ArrowDown), i.key_pressed(egui::Key::Escape)));
                let h = &self.light.cmd_history;
                if up && !h.is_empty() {
                    let i = self.light.cmd_back.map_or(h.len() - 1, |i| i.saturating_sub(1));
                    self.light.cmd_back = Some(i);
                    self.light.cmd_line = h[i].clone();
                }
                if down && let Some(i) = self.light.cmd_back {
                    if i + 1 < h.len() {
                        self.light.cmd_back = Some(i + 1);
                        self.light.cmd_line = h[i + 1].clone();
                    } else {
                        self.light.cmd_back = None;
                        self.light.cmd_line.clear();
                    }
                }
                if esc {
                    self.light.cmd_line.clear();
                    self.light.cmd_back = None;
                }
            }
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button(theme::with_icon(icons::ARROW_RIGHT, t("Run"))).clicked() || enter {
                self.run_command_line(show);
                ui.memory_mut(|m| m.request_focus(id));
            }
            let (fill, fg) = if self.light.keypad { (theme::ACCENT, Color32::WHITE) } else { (theme::BG_RAISED, theme::TEXT) };
            if ui.add(egui::Button::new(RichText::new(theme::with_icon(icons::GRID_VIEW, t("Keypad"))).color(fg)).fill(fill)).clicked() {
                self.light.keypad = !self.light.keypad;
            }
            match &self.light.cmd_result {
                Some(Ok(m)) => {
                    ui.label(RichText::new(m).color(theme::GREEN).small());
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e).color(theme::RED).small());
                }
                None => {}
            }
            // A copy of the masters, at hand on every page.
            theme::separated(ui, "masters", |ui| self.masters_strip(ui, show, false));
        });
    }

    /// Desk keypad: buttons type into the command line.
    fn keypad_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        if !self.light.keypad {
            return;
        }
        let mut open = true;
        let mut run = false;
        let pos = ctx.content_rect().right_top() + egui::vec2(-640.0, 190.0);
        egui::Window::new(t("Keypad")).open(&mut open).resizable(false).collapsible(false).default_pos(pos).show(ctx, |ui| {
            let rows: [[&str; 4]; 6] = [
                ["group", "preset", "effect", "clear"],
                ["7", "8", "9", "thru"],
                ["4", "5", "6", "+"],
                ["1", "2", "3", "-"],
                ["0", ".", "at", "full"],
                ["record", "cue", "/", "⏎"],
            ];
            egui::Grid::new("keypad").spacing([4.0, 4.0]).show(ui, |ui| {
                for row in rows {
                    for k in row {
                        let label = match k {
                            "⏎" => t("Enter").to_string(),
                            k if k.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '+' || c == '-' || c == '/') => k.to_string(),
                            k => {
                                let mut c = k.chars();
                                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
                            }
                        };
                        let fill = if k == "⏎" { theme::GO } else { theme::BG_RAISED };
                        if ui.add(egui::Button::new(RichText::new(label).font(theme::semibold(15.0))).fill(fill).min_size(egui::vec2(66.0, 44.0))).clicked() {
                            if k == "⏎" {
                                run = true;
                            } else {
                                let line = &mut self.light.cmd_line;
                                let joins = k.chars().all(|c| c.is_ascii_digit() || c == '.') && line.chars().last().is_some_and(|c| c.is_ascii_digit() || c == '.');
                                if !line.is_empty() && !joins && !line.ends_with(' ') {
                                    line.push(' ');
                                }
                                line.push_str(k);
                            }
                        }
                    }
                    ui.end_row();
                }
            });
            let clear = egui::Button::new(RichText::new(theme::with_icon(icons::CLOSE, t("Clear line"))).font(theme::semibold(15.0))).min_size(egui::vec2(4.0 * 66.0 + 3.0 * 4.0, 40.0));
            if ui.add_enabled(!self.light.cmd_line.is_empty(), clear).on_hover_text(t("Empty the command line")).clicked() {
                self.light.cmd_line.clear();
                self.light.cmd_back = None;
            }
        });
        self.light.keypad = open;
        if run {
            self.run_command_line(show);
        }
    }

    /// Parse and run the command line; its result shows next to it.
    fn run_command_line(&mut self, show: &mut Show) {
        let line = self.light.cmd_line.trim().to_string();
        if line.is_empty() {
            return;
        }
        let r = crate::light::command::parse(&line).and_then(|cmds| self.run_commands(show, &cmds));
        if r.is_ok() {
            if self.light.cmd_history.last() != Some(&line) {
                self.light.cmd_history.push(line);
                if self.light.cmd_history.len() > 100 {
                    self.light.cmd_history.remove(0);
                }
            }
            self.light.cmd_line.clear();
        }
        self.light.cmd_back = None;
        self.light.cmd_result = Some(r);
    }

    pub(super) fn run_commands(&mut self, show: &mut Show, cmds: &[crate::light::command::Cmd]) -> Result<String, String> {
        use crate::control::{Command, LightControl};
        use crate::light::command::{Cmd, SelOp, SelSet, Target};
        let design = self.can_design();
        let before = show.ws.lighting.clone();
        let mut done = String::new();
        for c in cmds {
            let playback = matches!(c, Cmd::Go(..) | Cmd::Release(_));
            if !design && !playback {
                return Err(t("Only GO and release work in Show mode").into());
            }
            let l = &mut show.ws.lighting;
            match c.clone() {
                Cmd::Select(op, set) => {
                    let ids: Vec<FixtureId> = match set {
                        SelSet::Fixture(f) => l.unit(f).map(|_| vec![f]).ok_or_else(|| tr!("no fixture {f}", f = fixture_label(f)))?,
                        SelSet::Range(a, b) => {
                            use crate::light::model::split_id;
                            // Cell ids sit above every fixture number, so a range from a fixture to a cell
                            // ("101 thru 101.5") is compared as (fixture, cell) instead of by id.
                            let key = |id: FixtureId| {
                                let (f, c) = split_id(id);
                                (f, c.unwrap_or(0))
                            };
                            let mixed = split_id(a).1.is_some() != split_id(b).1.is_some();
                            let (ka, kb) = (key(a).min(key(b)), key(a).max(key(b)));
                            let inside = |id: &FixtureId| if mixed { (ka..=kb).contains(&key(*id)) } else { (a..=b).contains(id) };
                            let mut v: Vec<FixtureId> = l.units().into_iter().filter(inside).collect();
                            v.sort();
                            v
                        }
                        SelSet::Group(g) => l.groups.iter().find(|x| x.id == g).map(|x| x.fixtures.clone()).ok_or_else(|| tr!("no group {g}", g = g.to_string()))?,
                        SelSet::All => l.units(),
                    };
                    let sel = &mut self.light.selection;
                    match op {
                        SelOp::Replace => *sel = ids,
                        SelOp::Add => {
                            let mut have: HashSet<FixtureId> = sel.iter().copied().collect();
                            sel.extend(ids.into_iter().filter(|f| have.insert(*f)));
                        }
                        SelOp::Remove => {
                            let ids: HashSet<FixtureId> = ids.into_iter().collect();
                            sel.retain(|f| !ids.contains(f));
                        }
                    }
                    done = tr!("{n} selected", n = self.light.selection.len().to_string());
                }
                Cmd::At(v) => {
                    if self.light.selection.is_empty() {
                        return Err(t("Nothing selected").into());
                    }
                    let snapshot = l.clone();
                    self.set_selection(&snapshot, Attribute::Dimmer, v);
                    done = format!("@ {:.0} %", v * 100.0);
                }
                Cmd::Preset(p) => {
                    if l.preset(p).is_none() {
                        return Err(tr!("no preset {p}", p = p.to_string()));
                    }
                    let snapshot = l.clone();
                    self.apply_preset(&snapshot, p);
                    done = tr!("preset {p}", p = p.to_string());
                }
                Cmd::Point(p) => {
                    if l.point(p).is_none() {
                        return Err(tr!("no point {p}", p = p.to_string()));
                    }
                    let snapshot = l.clone();
                    if self.apply_point(&snapshot, p) == 0 {
                        return Err(t("nothing selected can pan or tilt").to_string());
                    }
                    done = tr!("point {p}", p = p.to_string());
                }
                Cmd::PickRange(attr, name) => {
                    if self.light.selection.is_empty() {
                        return Err(t("Nothing selected").into());
                    }
                    let snapshot = l.clone();
                    let n = self.set_selection_range(&snapshot, attr, &name);
                    if n == 0 {
                        return Err(tr!("no {attr} range \"{name}\" in the selection", attr = attr.label().to_lowercase(), name = name));
                    }
                    done = format!("{} {name}", attr.label());
                }
                Cmd::Effect(e) => {
                    if l.effect(e).is_none() {
                        return Err(tr!("no effect {e}", e = e.to_string()));
                    }
                    self.light.programmer_effects.retain(|r| r.effect != e);
                    self.light.programmer_effects.push(EffectRun { effect: e, fixtures: self.light.selection.clone(), stop: false });
                    done = tr!("effect {e}", e = e.to_string());
                }
                Cmd::Clear => {
                    self.light.programmer.clear();
                    self.light.programmer_effects.clear();
                    done = t("cleared").into();
                }
                Cmd::Blind => self.light.blind = !self.light.blind,
                Cmd::Highlight => self.light.highlight = !self.light.highlight,
                Cmd::Update => {
                    if !self.update_cue(l) {
                        return Err(t("Select a cue on the Sequences page").into());
                    }
                    done = t("updated").into();
                }
                Cmd::Record(Target::Cue(n, seq)) => {
                    let seq = seq.or(self.light.sequence).or(l.sequences.first().map(|s| s.id));
                    self.record_cue(l, seq, format_number(n));
                    done = tr!("recorded cue {n}", n = format_number(n));
                }
                Cmd::Record(Target::Group(g)) => {
                    let fixtures = self.light.selection.clone();
                    match l.groups.iter_mut().find(|x| x.id == g) {
                        Some(x) => x.fixtures = fixtures,
                        None => l.groups.push(Group { id: g, name: String::new(), fixtures }),
                    }
                    done = tr!("recorded group {g}", g = g.to_string());
                }
                Cmd::Record(Target::Preset(p)) => {
                    let feature = l.preset(p).map_or(self.light.preset_filter, |x| x.feature);
                    let values = self.programmer_values(l, feature);
                    match l.presets.iter_mut().find(|x| x.id == p) {
                        Some(x) => x.values = values.into(),
                        None => l.presets.push(Preset { id: p, name: String::new(), feature, values: values.into() }),
                    }
                    done = tr!("recorded preset {p}", p = p.to_string());
                }
                Cmd::Delete(Target::Cue(n, seq)) => {
                    let seq = seq.or(self.light.sequence).or(l.sequences.first().map(|s| s.id));
                    let s = l.sequences.iter_mut().find(|s| Some(s.id) == seq).ok_or(t("No such sequence"))?;
                    let before = s.cues.len();
                    s.cues.retain(|c| c.number != format_number(n));
                    if s.cues.len() == before {
                        return Err(tr!("no cue {n}", n = format_number(n)));
                    }
                    done = tr!("deleted cue {n}", n = format_number(n));
                }
                Cmd::Delete(Target::Group(g)) => {
                    l.groups.retain(|x| x.id != g);
                    done = tr!("deleted group {g}", g = g.to_string());
                }
                Cmd::Delete(Target::Preset(p)) => {
                    crate::light::model::unlink_preset(l, p, &mut self.light.programmer);
                    l.delete_preset(p);
                    done = tr!("deleted preset {p}", p = p.to_string());
                }
                Cmd::Go(seq, cue) => {
                    let seq = seq.or(self.light.sequence).or(l.sequences.first().map(|s| s.id)).ok_or(t("No such sequence"))?;
                    let s = l.sequence(seq).ok_or(t("No such sequence"))?;
                    let cue = match cue {
                        Some(n) => Some(s.cues.iter().find(|c| c.number == format_number(n)).map(|c| c.id).ok_or_else(|| tr!("no cue {n}", n = format_number(n)))?),
                        None => None,
                    };
                    self.control(show, Command::Light(LightControl::Go(seq, cue)));
                    done = "GO".into();
                }
                Cmd::Release(seq) => {
                    self.control(show, Command::Light(LightControl::Release(seq, 1.0)));
                    done = t("released").into();
                }
            }
        }
        if show.ws.lighting != before {
            show.edited_outside_cues();
        }
        Ok(done)
    }

    /// Record / update controls (programmer → a sequence).
    fn record_controls(&mut self, ui: &mut Ui, l: &mut Lighting) {
        let seq_id = self.light.sequence.filter(|s| l.sequence(*s).is_some()).or(l.sequences.first().map(|s| s.id));
        // Sequence and number stay together when the row wraps.
        let hint = seq_id.and_then(|s| l.sequence(s)).map_or_else(|| "1".to_string(), next_cue_number);
        theme::separated(ui, "rec_seq", |ui| {
            egui::ComboBox::from_id_salt("rec_seq")
                .selected_text(seq_id.and_then(|s| l.sequence_label(s)).unwrap_or_else(|| t("new sequence").into()))
                .show_ui(ui, |ui| {
                    for s in &l.sequences {
                        ui.selectable_value(&mut self.light.sequence, Some(s.id), l.sequence_label(s.id).unwrap_or_default());
                    }
                });
            ui.add(egui::TextEdit::singleline(&mut self.light.record_number).hint_text(hint.clone()).desired_width(50.0));
        });
        let can = !self.light.programmer.is_empty() || !self.light.programmer_effects.is_empty();
        if ui.add_enabled(can, egui::Button::new(theme::with_icon(icons::FIBER_MANUAL_RECORD, t("Record cue")))).clicked() {
            let number = Some(self.light.record_number.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or(hint.clone());
            self.record_cue(l, seq_id, number);
            self.light.record_number.clear();
        }
        if ui
            .add_enabled(!l.fixtures.is_empty(), egui::Button::new(theme::with_icon(icons::BRIGHTNESS_6, t("Record blackout"))))
            .on_hover_text(t("Record a cue with every intensity at zero and every effect stopped"))
            .clicked()
        {
            let number = Some(self.light.record_number.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or(hint);
            self.record_blackout(l, seq_id, number);
            self.light.record_number.clear();
        }
        let target = self.light.cue.and_then(|c| l.cue_label(c));
        if ui
            .add_enabled(can && target.is_some(), egui::Button::new(theme::with_icon(icons::SAVE, t("Update cue"))))
            .on_hover_text(target.map_or_else(|| t("Select a cue on the Sequences page").to_string(), |c| tr!("Merge the programmer into cue {cue}", cue = c)))
            .clicked()
        {
            self.update_cue(l);
        }
    }

    /// Record the programmer (values and effects) as cue `number` of a sequence (None: the
    /// first; a new sequence 1 when there is none). An existing number is recorded over.
    fn record_cue(&mut self, l: &mut Lighting, seq: Option<SequenceId>, number: String) {
        let (values, effects) = (self.light.programmer.clone(), self.light.programmer_effects.clone());
        self.record_into(l, seq, number, values, effects, None);
    }

    /// Record a blackout cue ([`Lighting::blackout_look`], every effect stopped) as cue `number`.
    fn record_blackout(&mut self, l: &mut Lighting, seq: Option<SequenceId>, number: String) {
        let values = l.blackout_look().into();
        let effects = l.effects.iter().map(|e| EffectRun { effect: e.id, fixtures: Vec::new(), stop: true }).collect();
        self.record_into(l, seq, number, values, effects, Some(t("Blackout").to_string()));
    }

    /// Record `values` and `effects` as cue `number` (over an existing one; `name` if given).
    fn record_into(
        &mut self,
        l: &mut Lighting,
        seq: Option<SequenceId>,
        number: String,
        values: crate::light::model::Shared<Look>,
        effects: Vec<EffectRun>,
        name: Option<String>,
    ) {
        let seq = match seq.filter(|s| l.sequence(*s).is_some()) {
            Some(id) => id,
            None => {
                let id = seq.unwrap_or(1);
                l.sequences.push(Sequence { id, name: t("Main").into(), ..Default::default() });
                id
            }
        };
        let s = l.sequences.iter_mut().find(|s| s.id == seq).expect("sequence");
        if let Some(c) = s.cues.iter_mut().find(|c| c.number == number) {
            c.values = values;
            c.effects = effects;
            if let Some(name) = name {
                c.name = name;
            }
            self.light.cue = Some(c.id);
        } else {
            let max = crate::limits::MAX_LIGHT_CUES;
            if crate::limits::light_cue_count(l) >= max {
                self.notify(tr!("A lighting design holds at most {max} cues", max = max.to_string()));
                return;
            }
            let s = l.sequences.iter_mut().find(|s| s.id == seq).expect("sequence");
            let c = LightCue { number, values, effects, name: name.unwrap_or_default(), ..Default::default() };
            self.light.cue = Some(c.id);
            let at = s.cues.iter().position(|x| number_key(&x.number) > number_key(&c.number)).unwrap_or(s.cues.len());
            s.cues.insert(at, c);
        }
        self.light.sequence = Some(seq);
    }

    /// Merge the programmer into the selected cue. False when no cue is selected.
    fn update_cue(&mut self, l: &mut Lighting) -> bool {
        let Some(c) = self.light.cue.and_then(|id| l.sequences.iter_mut().flat_map(|s| s.cues.iter_mut()).find(|c| c.id == id)) else {
            return false;
        };
        for (f, vals) in &self.light.programmer {
            c.values.entry(*f).or_default().extend(vals.iter().map(|(a, v)| (*a, *v)));
        }
        for r in &self.light.programmer_effects {
            c.effects.retain(|x| x.effect != r.effect);
            c.effects.push(r.clone());
        }
        true
    }

    /// Fixture tiles: a click adds a fixture to the selection (or takes it out), a drag adds every
    /// tile of the band it covers, a right-click selects only that fixture. Each shows its number,
    /// name, type, the programmer intensity (bar) and colour (swatch), and the selection order.
    fn fixture_tiles(&mut self, ui: &mut Ui, l: &Lighting) {
        const GAP: f32 = 6.0;
        // Tiles stretch so a row fills the width exactly, short of a gutter for the (floating)
        // scroll bar; that gutter is also the gap to the encoders column.
        let avail = ui.available_width() - (ui.spacing().scroll.bar_width + 4.0);
        let per_row = ((avail + GAP) / (132.0 + GAP)).floor().max(1.0);
        let tile = egui::vec2((avail + GAP) / per_row - GAP, 86.0);
        let pitch = tile + egui::vec2(GAP, GAP);
        // Only the visible rows are laid out: a pixel rig has thousands of units.
        let units = l.units();
        let per_row = per_row as usize;
        let rows = units.len().div_ceil(per_row);
        // Empty space between and after the tiles starts a band too (the tiles are on top of it).
        let area = ui.available_rect_before_wrap();
        let back = ui.interact(area, ui.id().with("tiles_back"), egui::Sense::click_and_drag());
        let mut started = back.drag_started();
        let mut clicked: Option<(FixtureId, bool)> = None;
        // Screen position of the grid's top-left corner (scrolls with it).
        let mut origin: Option<egui::Pos2> = None;
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
            egui::ScrollArea::vertical().id_salt("tiles").auto_shrink(false).show_rows(ui, tile.y, rows, |ui, visible| {
                for row in visible {
                    ui.horizontal(|ui| {
                        for (col, &id) in units[row * per_row..((row + 1) * per_row).min(units.len())].iter().enumerate() {
                            let (rect, r) = ui.allocate_exact_size(tile, egui::Sense::click_and_drag());
                            origin.get_or_insert(rect.min - egui::vec2(col as f32 * pitch.x, row as f32 * pitch.y));
                            started |= r.drag_started();
                            if r.clicked() {
                                clicked = Some((id, false));
                            } else if r.secondary_clicked() {
                                clicked = Some((id, true));
                            }
                            let Some((f, cell)) = l.unit(id) else { continue };
                            let order = self.light.selection.iter().position(|x| *x == id);
                            let prog = self.light.programmer.get(&id);
                            let ty = l.fixture_type(&f.fixture_type);
                            let p = ui.painter();
                            let fill = if order.is_some() { theme::accent_bg() } else if r.hovered() { theme::BG_HOVER } else { theme::BG_RAISED };
                            p.rect_filled(rect, 8.0, fill);
                            let border = if prog.is_some() {
                                egui::Stroke::new(1.5, theme::RED)
                            } else if order.is_some() {
                                egui::Stroke::new(1.0, theme::BLUE)
                            } else {
                                egui::Stroke::new(1.0, theme::BORDER)
                            };
                            p.rect_stroke(rect, 8.0, border, egui::StrokeKind::Inside);
                            let inner = rect.shrink2(egui::vec2(9.0, 7.0));
                            p.text(inner.left_top(), egui::Align2::LEFT_TOP, fixture_label(id), theme::semibold(17.0), theme::TEXT);
                            if let Some(i) = order {
                                p.text(inner.right_top(), egui::Align2::RIGHT_TOP, format!("#{}", i + 1), theme::mono(10.5), theme::BLUE);
                            }
                            let name = if f.name.is_empty() { ty.map_or("", |t| t.name.as_str()) } else { f.name.as_str() };
                            // A cell shows its head name ("Cell 3") under the fixture's name.
                            let head = cell.and_then(|c| l.mode_of(f)?.heads.get(c as usize - 1)).map(|h| tr!("Cell {name}", name = h.name.clone()));
                            let name = head.as_deref().unwrap_or(name);
                            let clip = p.with_clip_rect(inner);
                            clip.text(inner.left_top() + egui::vec2(0.0, 24.0), egui::Align2::LEFT_TOP, name, egui::FontId::proportional(12.5), theme::TEXT);
                            if !f.name.is_empty() && let Some(ty) = ty {
                                clip.text(inner.left_top() + egui::vec2(0.0, 41.0), egui::Align2::LEFT_TOP, &ty.name, egui::FontId::proportional(10.5), theme::TEXT_FAINT);
                            }
                            // Programmer intensity bar and colour swatch.
                            let bar = egui::Rect::from_min_max(egui::pos2(inner.left(), inner.bottom() - 5.0), inner.right_bottom());
                            p.rect_filled(bar, 2.0, theme::BG_INPUT);
                            if let Some(v) = prog {
                                let c = |a| v.get(&a).and_then(|x| l.resolve(id, a, *x));
                                if let Some(d) = c(Attribute::Dimmer) {
                                    let lit = egui::Rect::from_min_size(bar.min, egui::vec2(bar.width() * d, bar.height()));
                                    p.rect_filled(lit, 2.0, theme::AMBER);
                                    p.text(egui::pos2(inner.right(), bar.top() - 3.0), egui::Align2::RIGHT_BOTTOM, format!("{:.0}%", d * 100.0), theme::mono(11.0), theme::AMBER);
                                }
                                if let (Some(r), Some(g), Some(b)) = (c(Attribute::Red), c(Attribute::Green), c(Attribute::Blue)) {
                                    let at = egui::pos2(inner.right() - 7.0, inner.top() + if order.is_some() { 24.0 } else { 8.0 });
                                    p.circle_filled(at, 7.0, Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8));
                                    p.circle_stroke(at, 7.0, egui::Stroke::new(1.0, theme::BORDER));
                                }
                            }
                        }
                    });
                }
            });
        });
        let sel = &mut self.light.selection;
        match clicked {
            Some((id, true)) => *sel = vec![id],
            Some((id, false)) => match sel.iter().position(|x| *x == id) {
                Some(i) => {
                    sel.remove(i);
                }
                None => sel.push(id),
            },
            None => {}
        }
        // The band, kept in grid coordinates so scrolling while dragging works.
        let Some(origin) = origin else { return };
        let (press, pointer, down) = ui.input(|i| (i.pointer.press_origin(), i.pointer.latest_pos(), i.pointer.primary_down()));
        if started
            && let Some(p) = press
        {
            self.light.band = Some((egui::pos2(p.x - origin.x, p.y - origin.y), self.light.selection.clone()));
        }
        if !down {
            self.light.band = None;
        }
        let (Some((start, base)), Some(pointer)) = (&self.light.band, pointer) else { return };
        let now = pointer - origin.to_vec2();
        let band = egui::Rect::from_two_pos(*start, now);
        // Tiles the band touches: columns and rows of the grid.
        let first = |v: f32, step: f32| ((v / step).floor().max(0.0)) as usize;
        let (c0, c1) = (first(band.left(), pitch.x), first(band.right(), pitch.x).min(per_row - 1));
        let (r0, r1) = (first(band.top(), pitch.y), first(band.bottom(), pitch.y));
        let mut sel = base.clone();
        let mut have: HashSet<FixtureId> = sel.iter().copied().collect();
        for r in r0..=r1.min(rows.saturating_sub(1)) {
            for c in c0..=c1 {
                if let Some(&id) = units.get(r * per_row + c)
                    && have.insert(id)
                {
                    sel.push(id);
                }
            }
        }
        self.light.selection = sel;
        let shown = band.translate(origin.to_vec2()).intersect(area);
        ui.painter().rect(shown, 2.0, theme::ACCENT.gamma_multiply(0.15), egui::Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
        ui.ctx().request_repaint();
    }

    /// Value of an attribute for the selection (the first selected fixture that has it).
    fn selection_value(&self, l: &Lighting, attr: Attribute) -> Option<f32> {
        self.light.selection.iter().find_map(|f| self.light.programmer.get(f).and_then(|v| v.get(&attr)).and_then(|v| l.resolve(*f, attr, *v)))
    }

    /// The preset an attribute of the selection refers to (first selected fixture that has it).
    fn selection_preset(&self, attr: Attribute) -> Option<PresetId> {
        self.light.selection.iter().find_map(|f| match self.light.programmer.get(f)?.get(&attr)? {
            Value::Preset { preset } => Some(*preset),
            Value::Level(_) | Value::Point { .. } => None,
        })
    }

    /// Put a preset into the programmer for the selected fixtures it has values for.
    fn apply_preset(&mut self, l: &Lighting, preset: PresetId) {
        for f in &self.light.selection {
            for a in l.preset_attributes(preset, *f) {
                self.light.programmer.entry(*f).or_default().insert(a, Value::Preset { preset });
            }
        }
    }

    /// The selection's programmer values (levels, presets resolved) for a feature.
    fn programmer_values(&self, l: &Lighting, feature: Option<Feature>) -> std::collections::BTreeMap<FixtureId, std::collections::BTreeMap<Attribute, f32>> {
        self.light
            .selection
            .iter()
            .filter_map(|f| {
                let v: std::collections::BTreeMap<Attribute, f32> = self.light.programmer.get(f)?
                    .iter()
                    .filter(|(a, _)| feature.is_none_or(|x| a.feature() == x))
                    .filter_map(|(a, v)| l.resolve(*f, *a, *v).map(|v| (*a, v)))
                    .collect();
                (!v.is_empty()).then_some((*f, v))
            })
            .collect()
    }

    /// Put every selected unit with a range called `name` (prefix or number) on `attr` at that
    /// range; returns how many matched. Units have their own ranges, so "Red" can be a
    /// different DMX value on each fixture type.
    fn set_selection_range(&mut self, l: &Lighting, attr: Attribute, name: &str) -> usize {
        let mut n = 0;
        for f in &self.light.selection {
            let range = l.unit_channels(*f).and_then(|ch| ch.iter().find(|c| c.attribute == attr)).and_then(|c| c.find_range(name));
            if let Some(r) = range {
                self.light.programmer.entry(*f).or_default().insert(attr, Value::Level(r.value()));
                n += 1;
            }
        }
        n
    }

    /// Set an attribute on every selected fixture that has it. Pan or tilt of a fixture aimed
    /// from a focus point moves the place it aims at, still measured from the point.
    fn set_selection(&mut self, l: &Lighting, attr: Attribute, value: f32) {
        let value = value.clamp(0.0, 1.0);
        for f in &self.light.selection {
            if attributes_of(l, *f).contains(&attr) {
                let values = self.light.programmer.entry(*f).or_default();
                if !crate::light::focus::move_from_point(l, *f, values, attr, value) {
                    values.insert(attr, Value::Level(value));
                }
            }
        }
    }

    /// Encoder bar: one page per feature with a slider per attribute of the selection.
    fn encoders(&mut self, ui: &mut Ui, l: &Lighting) {
        if self.light.selection.is_empty() {
            ui.label(RichText::new(t("Select fixtures to set their values.")).color(theme::TEXT_DIM));
            return;
        }
        let mut attrs: Vec<Attribute> = self.light.selection.iter().flat_map(|f| attributes_of(l, *f)).collect();
        attrs.sort();
        attrs.dedup();
        let features: Vec<Feature> = Feature::ALL.into_iter().filter(|f| attrs.iter().any(|a| a.feature() == *f)).collect();
        let feature = self.light.feature.filter(|f| features.contains(f)).or(features.first().copied());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for f in &features {
                let mut cur = feature;
                theme::tab(ui, &mut cur, Some(*f), feature_icon(*f), f.label());
                if cur != feature {
                    self.light.feature = cur;
                }
            }
        });
        ui.add_space(6.0);
        let Some(feature) = feature else { return };
        let page: Vec<Attribute> = attrs.into_iter().filter(|a| a.feature() == feature).collect();
        match feature {
            Feature::Intensity => {
                ui.horizontal(|ui| {
                    for (label, v) in [(t("Full"), 1.0), ("75 %", 0.75), ("50 %", 0.5), ("25 %", 0.25), (t("Zero"), 0.0)] {
                        if ui.add(egui::Button::new(label).min_size(egui::vec2(56.0, 28.0))).clicked() {
                            self.set_selection(l, Attribute::Dimmer, v);
                        }
                    }
                });
            }
            Feature::Colour => self.colour_picker(ui, l),
            Feature::Position => {
                if page.contains(&Attribute::Pan) && page.contains(&Attribute::Tilt) {
                    self.pan_tilt_pad(ui, l);
                }
                self.point_offsets(ui, l);
            }
            _ => {}
        }
        ui.add_space(4.0);
        let gap = ui.spacing().item_spacing.x;
        // Room for the scroll bar on the right.
        let bar_w = ui.available_width() - 92.0 - 26.0 - 2.0 * gap - 10.0;
        for &a in &page {
            let current = self.selection_value(l, a);
            let preset = self.selection_preset(a);
            // Extra channels go by their name in the first selected unit that has one.
            let label = match a {
                Attribute::Extra(_) => self.light.selection.iter().find(|f| attributes_of(l, **f).contains(&a)).map_or_else(|| a.label().to_string(), |f| l.attribute_label(*f, a)),
                _ => a.label().to_string(),
            };
            let slots = self.slot_channel(l, a);
            // Continuous attributes keep their slider; their named ranges go on a line below.
            let continuous = a.is_continuous();
            ui.horizontal(|ui| {
                let (r, resp) = ui.allocate_exact_size(egui::vec2(92.0, 26.0), egui::Sense::hover());
                let fg = if current.is_some() { theme::TEXT } else { theme::TEXT_DIM };
                ui.painter().with_clip_rect(r).text(r.left_center(), egui::Align2::LEFT_CENTER, &label, egui::FontId::proportional(13.0), fg);
                if matches!(a, Attribute::Extra(_)) {
                    resp.on_hover_text(a.label());
                }
                match &slots {
                    Some((chan, type_id)) if !continuous => self.slot_picker(ui, l, a, chan, type_id, current, preset, bar_w),
                    _ => {
                        if let Some(v) = attr_bar(ui, current, preset, bar_w) {
                            self.set_selection(l, a, v);
                        }
                    }
                }
                // Only values in the programmer can be removed; the place is kept so rows line up.
                let clear = ui.add_visible(current.is_some(), egui::Button::new(icons::CLOSE).min_size(egui::vec2(26.0, 26.0))).on_hover_text(t("Remove from the programmer"));
                if clear.clicked() {
                    for f in &self.light.selection {
                        if let Some(vals) = self.light.programmer.get_mut(f) {
                            vals.remove(&a);
                        }
                    }
                    self.light.programmer.retain(|_, v| !v.is_empty());
                }
            });
            if continuous && let Some((chan, type_id)) = &slots {
                ui.horizontal(|ui| {
                    ui.add_space(92.0 + gap);
                    self.slot_picker(ui, l, a, chan, type_id, current, preset, bar_w);
                });
            }
            // A range that is a span (a speed, an index angle, "strobe slow → fast"): a bar within it.
            if let Some((chan, _)) = &slots
                && !continuous
                && let Some(v) = current
                && let Some(i) = chan.range_at(v).filter(|r| r.is_span()).and_then(|r| chan.ranges.iter().position(|x| std::ptr::eq(x, r)))
            {
                ui.horizontal(|ui| {
                    ui.add_space(92.0 + gap);
                    if let Some(x) = attr_bar(ui, Some(within_range(&chan.ranges[i], v)), None, bar_w) {
                        self.set_selection_slot(l, a, chan, i, Some(x));
                    }
                });
            }
            ui.add_space(2.0);
        }
        // Fan: spread one attribute of this page across the selection in selection order.
        let spread: Vec<Attribute> = page.iter().copied().filter(|a| a.is_continuous() || self.slot_channel(l, *a).is_none()).collect();
        if self.light.selection.len() > 1 && !spread.is_empty() {
            ui.add_space(8.0);
            egui::CollapsingHeader::new(theme::caption(t("Spread"))).id_salt("fan").default_open(false).show(ui, |ui| {
                let FanTool(attr, mut a, mut b, mut centre) = self.light.fan;
                let mut pick = attr.filter(|x| spread.contains(x)).or(spread.first().copied());
                theme::wrapped_row(ui, |ui| {
                    theme::keep_together(ui, "fan_attr", |ui| {
                        egui::ComboBox::from_id_salt("fan_attr").selected_text(pick.map_or("", |x| x.label())).width(110.0).show_ui(ui, |ui| {
                            for x in &spread {
                                ui.selectable_value(&mut pick, Some(*x), x.label());
                            }
                        });
                    });
                    ui.label(t("from"));
                    ui.add(percent_drag(&mut a));
                    ui.label(t("to"));
                    ui.add(percent_drag(&mut b));
                    ui.checkbox(&mut centre, t("centre out"));
                });
                self.light.fan = FanTool(pick.or(attr), a, b, centre);
                if ui.button(theme::with_icon(icons::TUNE, t("Spread across the selection"))).clicked()
                    && let Some(attr) = pick
                {
                    let who: Vec<FixtureId> = self.light.selection.iter().copied().filter(|f| attributes_of(l, *f).contains(&attr)).collect();
                    for (f, v) in who.iter().zip(fan_values(who.len(), a, b, centre)) {
                        self.light.programmer.entry(*f).or_default().insert(attr, Value::Level(v));
                    }
                }
            });
        }
    }

    /// The channel of an attribute that is a set of named ranges (wheel slots, shutter states,
    /// macros), from the first selected unit that has one, with its fixture type. A single range
    /// (a whole-channel "narrow → wide") isn't a choice: a plain bar.
    fn slot_channel(&self, l: &Lighting, a: Attribute) -> Option<(crate::light::model::ChannelDef, String)> {
        self.light.selection.iter().find_map(|f| {
            let c = l.unit_channels(*f)?.iter().find(|c| c.attribute == a && c.ranges.len() > 1).cloned()?;
            Some((c, l.unit(*f)?.0.fixture_type.clone()))
        })
    }

    /// Drop-down of a channel's named ranges: slot number, picture (gobo image or slot colour) and
    /// name, the DMX span on the right. A pick goes by name, so each fixture type of the selection
    /// gets its own value for it.
    #[allow(clippy::too_many_arguments)]
    fn slot_picker(&mut self, ui: &mut Ui, l: &Lighting, a: Attribute, chan: &crate::light::model::ChannelDef, type_id: &str, current: Option<f32>, preset: Option<PresetId>, width: f32) {
        let images = l.fixture_type(type_id).map(|t| &t.images);
        let pictures: Vec<Option<egui::TextureHandle>> = chan
            .ranges
            .iter()
            .map(|r| images.and_then(|m| m.get(&r.image)).and_then(|data| self.gobo_texture(ui.ctx(), type_id, &r.image, data)))
            .collect();
        let thumbs = pictures.iter().any(Option::is_some) || chan.ranges.iter().any(|r| r.color.is_some());
        let active = current.and_then(|v| chan.range_at(v)).and_then(|r| chan.ranges.iter().position(|x| std::ptr::eq(x, r)));
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(width.max(60.0), 26.0), egui::Sense::click());
        let p = ui.painter();
        p.rect_filled(rect, 5.0, theme::BG_INPUT);
        let stroke = if resp.hovered() { theme::TEXT_FAINT } else { theme::BORDER };
        p.rect_stroke(rect, 5.0, egui::Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
        p.text(rect.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, icons::EXPAND_MORE, egui::FontId::proportional(16.0), theme::TEXT_DIM);
        let face = rect.shrink2(egui::vec2(6.0, 2.0)).with_max_x(rect.right() - 26.0);
        match (active, current) {
            (Some(i), _) => slot_face(ui, face, i, &chan.ranges[i], pictures[i].as_ref(), thumbs, preset),
            // Between named ranges (a gap in the fixture's definition).
            (None, Some(v)) => {
                ui.painter().text(face.left_center(), egui::Align2::LEFT_CENTER, format!("DMX {}", crate::light::model::coarse(v)), theme::mono(12.0), theme::TEXT);
            }
            (None, None) => {
                ui.painter().text(face.left_center(), egui::Align2::LEFT_CENTER, "—", egui::FontId::proportional(13.0), theme::TEXT_FAINT);
            }
        }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        if self.light.open_slots == Some(a) {
            self.light.open_slots = None;
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&resp));
        }
        let mut pick = None;
        // The wheel over the closed drop-down steps through the ranges (down = next).
        if resp.hovered() && !egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp)) {
            let steps = wheel_steps(ui, resp.id);
            if steps != 0 {
                let last = chan.ranges.len() as i32 - 1;
                pick = Some(match active {
                    Some(i) => (i as i32 + steps).clamp(0, last),
                    None if steps > 0 => 0,
                    None => last,
                } as usize)
                .filter(|i| Some(*i) != active);
            }
        }
        egui::Popup::menu(&resp).width(rect.width()).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                let h = if pictures.iter().any(Option::is_some) { 34.0 } else { 24.0 };
                for (i, r) in chan.ranges.iter().enumerate() {
                    let (row, rr) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
                    if active == Some(i) {
                        ui.painter().rect_filled(row, 4.0, theme::ACCENT.gamma_multiply(0.35));
                    } else if rr.hovered() {
                        ui.painter().rect_filled(row, 4.0, theme::BG_HOVER);
                    }
                    let span = format!("{}–{}", r.from, r.to);
                    ui.painter().text(row.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, &span, theme::mono(11.0), theme::TEXT_FAINT);
                    slot_face(ui, row.shrink2(egui::vec2(6.0, 2.0)).with_max_x(row.right() - 60.0), i, r, pictures[i].as_ref(), thumbs, None);
                    let rr = match &pictures[i] {
                        Some(tex) => rr.on_hover_ui(|ui| {
                            ui.add(egui::Image::from_texture((tex.id(), egui::vec2(128.0, 128.0))));
                        }),
                        None => rr,
                    };
                    if rr.clicked() {
                        pick = Some(i);
                        ui.close();
                    }
                }
            });
        });
        if let Some(i) = pick
            && self.set_selection_slot(l, a, chan, i, None) == 0
        {
            self.set_selection(l, a, chan.ranges[i].value());
        }
    }

    /// Put the selection on range `i` of `chan` (its middle, or `x` (0..=1) within it): units with
    /// the same ranges take that one (names can repeat: a shutter has several "Open"s), others the
    /// range of that name. Returns how many units were set.
    fn set_selection_slot(&mut self, l: &Lighting, attr: Attribute, chan: &crate::light::model::ChannelDef, i: usize, x: Option<f32>) -> usize {
        let range = &chan.ranges[i];
        let mut n = 0;
        for f in &self.light.selection {
            let Some(c) = l.unit_channels(*f).and_then(|ch| ch.iter().find(|c| c.attribute == attr)) else { continue };
            let r = if c.ranges == chan.ranges {
                Some(range)
            } else if range.name.trim().is_empty() {
                None
            } else {
                c.find_range(&range.name)
            };
            let Some(r) = r else { continue };
            let v = match x {
                Some(x) if r.is_span() => (r.from as f32 + x.clamp(0.0, 1.0) * (r.to as f32 - r.from as f32)) / 255.0,
                _ => r.value(),
            };
            self.light.programmer.entry(*f).or_default().insert(attr, Value::Level(v));
            n += 1;
        }
        n
    }

    /// A wheel slot picture as a texture (decoded on first use, then cached).
    fn gobo_texture(&mut self, ctx: &egui::Context, type_id: &str, name: &str, data: &str) -> Option<egui::TextureHandle> {
        let key = (type_id.to_string(), name.to_string(), data.len());
        self.light
            .gobo_textures
            .entry(key)
            .or_insert_with(|| {
                let (w, h, rgba) = crate::light::gobo::decode(data)?;
                let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                Some(ctx.load_texture(format!("gobo-{type_id}-{name}"), img, egui::TextureOptions::LINEAR))
            })
            .clone()
    }

    /// Colour picker for the selection: the colours the first selected fixture can make (its
    /// gamut, from the type's emitter colours or typical ones) on a CIE xy diagram. A pick is
    /// mixed by each fixture with its own emitters (brightest at full).
    fn colour_picker(&mut self, ui: &mut Ui, l: &Lighting) {
        use crate::light::colour;
        let mixer_of = |f: FixtureId| colour::mixer(l.unit(f).and_then(|(fx, _)| l.fixture_type(&fx.fixture_type)), &attributes_of(l, f));
        let Some((first, mixer)) = self.light.selection.iter().find_map(|f| Some((*f, mixer_of(*f)?))) else { return };
        let level = |a: Attribute| self.light.programmer.get(&first).and_then(|v| v.get(&a)).and_then(|v| l.resolve(first, a, *v));
        let current = mixer.chromaticity(level);
        let gamut = mixer.gamut();
        let mut pick = None;
        // Swatches, brought into the gamut; those it can't make are struck through.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let n = SWATCHES.len() as f32;
            let side = ((ui.available_width() - 4.0 * (n - 1.0)) / n).clamp(16.0, 30.0);
            for sw in SWATCHES {
                let Some(xy) = colour::srgb_to_xy(sw) else { continue };
                let inside = colour::clamp_into(&gamut, xy);
                let off = (inside[0] - xy[0]).hypot(inside[1] - xy[1]) > 0.01;
                let (r, resp) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
                ui.painter().rect_filled(r, 4.0, to_c32(sw));
                let stroke = if resp.hovered() { egui::Stroke::new(2.0, Color32::WHITE) } else { egui::Stroke::new(1.0, theme::BORDER) };
                ui.painter().rect_stroke(r, 4.0, stroke, egui::StrokeKind::Inside);
                if off {
                    ui.painter().line_segment([r.left_bottom() + egui::vec2(3.0, -3.0), r.right_top() + egui::vec2(-3.0, 3.0)], egui::Stroke::new(1.5, Color32::BLACK));
                }
                let resp = if off { resp.on_hover_text(t("Out of this fixture's range: the nearest colour it can make")) } else { resp };
                if resp.clicked() {
                    pick = Some(inside);
                }
            }
        });
        ui.add_space(6.0);
        if let Some(xy) = cie_field(ui, &gamut, current) {
            pick = Some(xy);
        }
        let note = if mixer.subtractive {
            t("Approximate range of CMY colour mixing")
        } else if mixer.measured {
            t("Colours this fixture type can make (its emitter colours)")
        } else {
            t("Approximate range (the fixture type has no emitter colours)")
        };
        ui.label(RichText::new(note).weak().small());
        if let Some(xy) = pick {
            let sel = self.light.selection.clone();
            for f in sel {
                let Some(m) = mixer_of(f) else { continue };
                let vals = self.light.programmer.entry(f).or_default();
                for (a, v) in m.levels(xy) {
                    vals.insert(a, Value::Level(v));
                }
            }
        }
        ui.add_space(4.0);
    }

    /// Pan (x) / tilt (y) pad for the selection.
    fn pan_tilt_pad(&mut self, ui: &mut Ui, l: &Lighting) {
        let side = ui.available_width().clamp(160.0, 300.0);
        let size = egui::vec2(side, side * 0.75);
        let (rect, r) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
        let p = ui.painter();
        p.rect_filled(rect, 6.0, theme::BG_INPUT);
        p.rect_stroke(rect, 6.0, egui::Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
        p.line_segment([rect.center_top(), rect.center_bottom()], egui::Stroke::new(1.0, theme::BORDER));
        p.line_segment([rect.left_center(), rect.right_center()], egui::Stroke::new(1.0, theme::BORDER));
        let pan = self.selection_value(l, Attribute::Pan).unwrap_or(0.5);
        let tilt = self.selection_value(l, Attribute::Tilt).unwrap_or(0.5);
        let dot = egui::pos2(rect.left() + pan * rect.width(), rect.bottom() - tilt * rect.height());
        p.circle_filled(dot, 6.0, theme::ACCENT);
        if (r.dragged() || r.clicked())
            && let Some(pos) = r.interact_pointer_pos()
        {
            // Shift: fine movement relative to the drag.
            let (np, nt) = if ui.input(|i| i.modifiers.shift) && r.dragged() {
                let d = r.drag_delta() / size * 0.1;
                (pan + d.x, tilt - d.y)
            } else {
                ((pos.x - rect.left()) / rect.width(), (rect.bottom() - pos.y) / rect.height())
            };
            self.set_selection(l, Attribute::Pan, np);
            self.set_selection(l, Attribute::Tilt, nt);
        }
        ui.label(RichText::new(t("Drag to aim; Shift for fine moves.")).weak().small());
        ui.add_space(4.0);
    }

    fn sequences_page(&mut self, ui: &mut Ui, show: &mut Show) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        let status = show.media.light.status();
        let l = &mut show.ws.lighting;
        let light = &show.media.light;
        egui::Panel::right("cue_details")
            .frame(side_column(false))
            .show_separator_line(false)
            .resizable(true)
            .default_size(330.0)
            .size_range(280.0..=480.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("cue_details_scroll").auto_shrink(false).show(ui, |ui| self.cue_details(ui, l));
            });
        ui.horizontal_top(|ui| {
            // Sequence list.
            ui.vertical(|ui| {
                ui.set_width(220.0);
                ui.horizontal(|ui| {
                    ui.label(theme::caption(t("Sequences")));
                    if ui.small_button(icons::ADD).on_hover_text(t("New sequence")).clicked() {
                        let id = l.sequences.iter().map(|s| s.id).max().unwrap_or(0) + 1;
                        l.sequences.push(Sequence { id, ..Default::default() });
                        self.light.sequence = Some(id);
                    }
                    if ui.small_button(icons::UPLOAD_FILE).on_hover_text(t("Import a USITT ASCII file (cues, groups, submasters and patch from another desk)")).clicked() {
                        self.deferred.push(super::Deferred::ImportUsitt);
                    }
                    if ui.small_button(icons::DOWNLOAD).on_hover_text(t("Export the design as a USITT ASCII file (intensities: patch, cues, groups, executors as submasters)")).clicked() {
                        self.deferred.push(super::Deferred::ExportUsitt);
                    }
                });
                let mut delete = None;
                for s in &mut l.sequences {
                    let selected = self.light.sequence == Some(s.id);
                    let st = status.iter().find(|x| x.id == s.id);
                    ui.horizontal(|ui| {
                        let mark = if st.is_some_and(|x| x.active) { theme::GREEN } else { theme::TEXT_FAINT };
                        ui.label(RichText::new(icons::FIBER_MANUAL_RECORD).font(theme::icon_font(10.0)).color(mark));
                        if ui.selectable_label(selected, s.id.to_string()).clicked() {
                            self.light.sequence = Some(s.id);
                        }
                        ui.add(egui::TextEdit::singleline(&mut s.name).hint_text(t("Name")).desired_width(110.0));
                        if ui.small_button(icons::DELETE).on_hover_text(t("Delete sequence")).clicked() {
                            delete = Some(s.id);
                        }
                    });
                }
                if let Some(id) = delete {
                    l.sequences.retain(|s| s.id != id);
                }
            });
            ui.separator();
            // Cues of the chosen sequence; nothing drawn under the details column on a narrow window.
            ui.vertical(|ui| {
                ui.set_clip_rect(ui.clip_rect().intersect(ui.max_rect()));
                let Some(seq) = self.light.sequence.filter(|s| l.sequence(*s).is_some()).or(l.sequences.first().map(|s| s.id)) else {
                    ui.label(RichText::new(t("No sequences yet: record a cue from the Programmer.")).color(theme::TEXT_DIM));
                    return;
                };
                let current = status.iter().find(|x| x.id == seq).and_then(|x| x.cue);
                let now = std::time::Instant::now();
                theme::wrapped_row(ui, |ui| {
                    if ui.button(theme::with_icon(icons::PLAY_ARROW, t("GO next"))).clicked() {
                        light.go(seq, None, now);
                    }
                    if ui.button(theme::with_icon(icons::STOP, t("Release"))).clicked() {
                        light.release(seq, 1.0, now);
                    }
                    let sel = self.light.cue.filter(|c| l.sequence(seq).is_some_and(|s| s.index_of(*c).is_some()));
                    let on = sel.is_some();
                    if ui.add_enabled(on, egui::Button::new(theme::with_icon(icons::PLAY_CIRCLE, t("GO selected")))).clicked()
                        && let Some(c) = sel
                    {
                        light.go(seq, Some(c), now);
                    }
                    if ui.add_enabled(on, egui::Button::new(theme::with_icon(icons::UPLOAD_FILE, t("Load into programmer")))).clicked()
                        && let Some(c) = sel.and_then(|c| l.find_cue(c))
                    {
                        self.light.programmer = c.0.cues[c.1].values.clone();
                        self.light.programmer_effects = c.0.cues[c.1].effects.iter().filter(|r| !r.stop).cloned().collect();
                        self.light.selection = self.light.programmer.keys().copied().collect();
                    }
                    if ui.add_enabled(on, egui::Button::new(theme::with_icon(icons::DELETE, t("Delete cue")))).clicked()
                        && let Some(s) = l.sequences.iter_mut().find(|s| s.id == seq)
                    {
                        s.cues.retain(|c| Some(c.id) != sel);
                    }
                });
                ui.add_space(4.0);
                let Some(s) = l.sequences.iter_mut().find(|s| s.id == seq) else { return };
                // The name column gives up width (down to 80) on a narrow window; 60 for the fixture count.
                let mut w: [f32; 6] = [24.0, 60.0, 220.0, 80.0, 80.0, 80.0];
                let others: f32 = w.iter().sum::<f32>() - w[2] + 6.0 * ui.spacing().item_spacing.x + 60.0;
                w[2] = (ui.available_width() - others).clamp(80.0, w[2]);
                let w = w;
                ui.horizontal(|ui| {
                    for (h, w) in ["", t("No."), t("Name"), t("Fade"), t("Down"), t("Delay")].iter().zip(w) {
                        ui.allocate_ui_with_layout(egui::vec2(w, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_width(w);
                            ui.label(theme::caption(h));
                        });
                    }
                    ui.label(theme::caption(t("Fixtures")));
                });
                egui::ScrollArea::vertical().id_salt("light_cues").auto_shrink(false).show_rows(ui, 28.0, s.cues.len(), |ui, range| {
                    for c in &mut s.cues[range] {
                        let selected = self.light.cue == Some(c.id);
                        ui.horizontal(|ui| {
                            let live = current == Some(c.id);
                            let mark = if live { RichText::new(icons::PLAY_ARROW).color(theme::GREEN) } else { RichText::new("") };
                            ui.add_sized([w[0], 20.0], egui::Label::new(mark.font(theme::icon_font(14.0))));
                            ui.add(egui::TextEdit::singleline(&mut c.number).desired_width(w[1] - 8.0));
                            let name = if c.name.is_empty() { "—" } else { c.name.as_str() };
                            let (rect, r) = ui.allocate_exact_size(egui::vec2(w[2], 22.0), egui::Sense::click());
                            let fill = if selected { theme::accent_bg() } else if r.hovered() { theme::BG_HOVER } else { Color32::TRANSPARENT };
                            ui.painter().rect_filled(rect, 4.0, fill);
                            ui.painter().with_clip_rect(rect).text(
                                rect.left_center() + egui::vec2(6.0, 0.0),
                                egui::Align2::LEFT_CENTER,
                                name,
                                egui::FontId::proportional(13.0),
                                if selected { theme::BLUE } else { theme::TEXT },
                            );
                            if c.block {
                                ui.painter().text(rect.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, icons::LOCK, theme::icon_font(14.0), theme::ACCENT);
                            }
                            let r = if c.block { r.on_hover_text(t("Blocked")) } else { r };
                            if r.clicked() {
                                self.light.cue = Some(c.id);
                            }
                            ui.add_sized([w[3], 20.0], DragValue::new(&mut c.fade).range(0.0..=600.0).speed(0.05).suffix(" s"));
                            let mut down = c.down_fade.unwrap_or(c.fade);
                            if ui.add_sized([w[4], 20.0], DragValue::new(&mut down).range(0.0..=600.0).speed(0.05).suffix(" s")).changed() {
                                c.down_fade = Some(down);
                            }
                            ui.add_sized([w[5], 20.0], DragValue::new(&mut c.delay).range(0.0..=600.0).speed(0.05).suffix(" s"));
                            ui.label(RichText::new(c.values.len().to_string()).color(theme::TEXT_DIM));
                        });
                    }
                });
            });
        });
    }

    /// The selected cue (name, options, timing per feature) and its sequence's settings.
    fn cue_details(&mut self, ui: &mut Ui, l: &mut Lighting) {
        let seq = self.light.sequence.filter(|s| l.sequence(*s).is_some()).or(l.sequences.first().map(|s| s.id));
        let Some(s) = seq.and_then(|id| l.sequences.iter_mut().find(|s| s.id == id)) else { return };
        let index = self.light.cue.and_then(|c| s.index_of(c));
        if let Some(i) = index {
            let mut block = None;
            let c = &mut s.cues[i];
            ui.label(theme::caption(&tr!("Cue {n}", n = c.number.clone())));
            ui.add(egui::TextEdit::singleline(&mut c.name).hint_text(t("Name")).desired_width(f32::INFINITY));
            ui.add_space(4.0);
            ui.checkbox(&mut c.cue_only, t("Cue only")).on_hover_text(t("Its changes don't track: the next cue goes back to what was there before"));
            // Lit while the cue is a block; pressing it again lets its values track again.
            let (glyph, label, fill, fg, tip) = if c.block {
                (icons::LOCK, t("Blocked"), theme::ACCENT, Color32::WHITE, t("Every tracked value is stored in this cue. Click to let values that are the same as before it track again"))
            } else {
                (icons::LOCK_OPEN, t("Block"), theme::BG_RAISED, theme::TEXT, t("Store every tracked value in this cue, so edits to earlier cues no longer change it"))
            };
            if ui.add(egui::Button::new(RichText::new(theme::with_icon(glyph, label)).color(fg)).fill(fill)).on_hover_text(tip).clicked() {
                block = Some(!c.block);
            }
            ui.add_space(8.0);
            ui.label(theme::caption(t("Timing per feature")));
            egui::Grid::new("feature_times").num_columns(3).spacing([8.0, 4.0]).show(ui, |ui| {
                ui.label("");
                ui.label(RichText::new(t("Fade")).small().weak());
                ui.label(RichText::new(t("Delay")).small().weak());
                ui.end_row();
                for f in Feature::ALL {
                    let mut on = c.feature_times.contains_key(&f);
                    if ui.checkbox(&mut on, f.label()).changed() {
                        if on {
                            c.feature_times.insert(f, crate::light::model::FeatureTime { fade: c.fade, delay: c.delay });
                        } else {
                            c.feature_times.remove(&f);
                        }
                    }
                    match c.feature_times.get_mut(&f) {
                        Some(ft) => {
                            ui.add(DragValue::new(&mut ft.fade).range(0.0..=600.0).speed(0.05).suffix(" s"));
                            ui.add(DragValue::new(&mut ft.delay).range(0.0..=600.0).speed(0.05).suffix(" s"));
                        }
                        None => {
                            ui.label(RichText::new(format!("{:.2} s", c.fade)).weak());
                            ui.label(RichText::new(format!("{:.2} s", c.delay)).weak());
                        }
                    }
                    ui.end_row();
                }
            });
            if !c.effects.is_empty() {
                ui.add_space(8.0);
                ui.label(theme::caption(t("Effects")));
                let mut remove = None;
                for (k, r) in c.effects.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let what = if r.stop { tr!("stop effect {n}", n = r.effect.to_string()) } else { tr!("effect {n} on {count} fixtures", n = r.effect.to_string(), count = r.fixtures.len().to_string()) };
                        ui.label(what);
                        if ui.small_button(icons::CLOSE).clicked() {
                            remove = Some(k);
                        }
                    });
                }
                if let Some(k) = remove {
                    c.effects.remove(k);
                }
            }
            match block {
                Some(true) => s.block(i),
                Some(false) => s.unblock(i),
                None => {}
            }
            ui.add_space(8.0);
            ui.separator();
        } else {
            ui.label(RichText::new(t("Select a cue to edit its options.")).color(theme::TEXT_DIM));
            ui.separator();
        }
        ui.label(theme::caption(&tr!("Sequence {n}", n = s.id.to_string())));
        ui.checkbox(&mut s.move_in_black, t("Move in black")).on_hover_text(t("Dark fixtures take their next cue's position, colour and beam before they come up"));
        ui.add_enabled_ui(s.move_in_black, |ui| {
            ui.horizontal(|ui| {
                ui.label(t("Delay"));
                ui.add(DragValue::new(&mut s.mib_delay).range(0.0..=600.0).speed(0.05).suffix(" s"));
                ui.label(t("Move"));
                ui.add(DragValue::new(&mut s.mib_fade).range(0.0..=600.0).speed(0.05).suffix(" s"));
            });
        });        ui.add_space(8.0);
        ui.separator();
        ui.label(theme::caption(t("With the cue list")));
        ui.checkbox(&mut l.stop_all_releases, t("Stop cues without a target also fade the lighting out"))
            .on_hover_text(t("They release every sequence. Stop all and Panic (Esc) never touch the lighting"));
        ui.horizontal(|ui| {
            ui.label(t("Release fade"));
            ui.add(DragValue::new(&mut l.release_fade).range(0.0..=600.0).speed(0.05).suffix(" s"))
                .on_hover_text(t("Used when a Stop cue aimed at a Light cue releases its sequence"));
        });
    }
}


/// The controls of [`App::masters_strip`]; the commands they make go to `cmds` (also used by
/// the Playback window, which draws outside the main window's frames).
pub(super) fn masters_controls(ui: &mut Ui, show: &Show, taps: &mut Vec<std::time::Instant>, big: bool, cmds: &mut Vec<crate::control::LightControl>) {
    use crate::control::LightControl;
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    let masters = show.media.light.masters().clone();
    let h = if big { 40.0 } else { 26.0 };
    ui.label(theme::caption(t("Grand master")));
    let mut gm = masters.grand_master;
    let bar = ui.allocate_ui(egui::vec2(if big { 220.0 } else { 120.0 }, h), |ui| {
        ui.set_width(if big { 220.0 } else { 120.0 });
        level_bar_sized(ui, &mut gm, theme::AMBER, h)
    });
    if bar.inner.changed() {
        cmds.push(LightControl::GrandMaster(gm));
    }
    let (fill, fg) = if masters.blackout { (theme::RED, Color32::WHITE) } else { (theme::BG_RAISED, theme::TEXT) };
    if ui
        .add(egui::Button::new(RichText::new(t("Blackout")).color(fg)).fill(fill).min_size(egui::vec2(if big { 110.0 } else { 0.0 }, h)))
        .on_hover_text(t("Intensity to zero (the grand master stays where it is)"))
        .clicked()
    {
        cmds.push(LightControl::Blackout(!masters.blackout));
    }
    ui.add_space(6.0);
    ui.label(theme::caption(t("Tempo")));
    let mut bpm = masters.bpm;
    if ui.add_sized([if big { 90.0 } else { 74.0 }, h], DragValue::new(&mut bpm).range(20.0..=300.0).speed(0.5).max_decimals(1).suffix(" BPM")).changed() {
        cmds.push(LightControl::Tempo(bpm));
    }
    if ui.add(egui::Button::new(t("Tap")).min_size(egui::vec2(if big { 80.0 } else { 0.0 }, h))).on_hover_text(t("Tap on the beat")).clicked()
        && let Some(bpm) = crate::surface::tap(taps, std::time::Instant::now())
    {
        cmds.push(LightControl::Tempo(bpm));
    }
}

impl App {
    /// Grand master, blackout and the effects tempo (with Tap) on one line: at the top of the
    /// Playback page (`big`: touch sizes), and a copy on the command line row. Works in both modes; actions go through
    /// [`App::control`] so a replica drives the main device.
    pub(super) fn masters_strip(&mut self, ui: &mut Ui, show: &mut Show, big: bool) {
        let mut cmds = Vec::new();
        masters_controls(ui, show, &mut self.light.taps, big, &mut cmds);
        for c in cmds {
            self.control(show, crate::control::Command::Light(c));
        }
    }

    /// DMX outputs: protocol, target, universes, live status; Art-Net discovery.
    fn outputs_page(&mut self, ui: &mut Ui, show: &mut Show) {
        use crate::light::dmx::{OutputConfig, OutputProtocol, UniverseRoute};
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        let handles = show.media.light.outputs().to_vec();
        let input_handles = show.media.light.inputs();
        let l = &mut show.ws.lighting;
        ui.horizontal(|ui| {
            for (label, p) in [
                ("Art-Net", OutputProtocol::ArtNet { target: String::new(), sync: false }),
                ("sACN", OutputProtocol::Sacn { target: String::new(), priority: 100 }),
                ("Enttec USB Pro", OutputProtocol::EnttecPro { port: String::new() }),
            ] {
                if ui.button(theme::with_icon(icons::ADD, label)).clicked() {
                    let wire0 = matches!(p, OutputProtocol::ArtNet { .. });
                    let universe = l.fixtures.iter().map(|f| f.universe).min().unwrap_or(1);
                    l.outputs.push(OutputConfig {
                        name: label.into(),
                        enabled: true,
                        protocol: p,
                        routes: vec![UniverseRoute { universe, wire: if wire0 { universe - 1 } else { universe } }],
                    });
                }
            }
            ui.separator();
            let found = self.light.found.clone();
            let searching = found.lock().searching;
            if ui.add_enabled(!searching, egui::Button::new(theme::with_icon(icons::REFRESH, t("Find devices")))).clicked() {
                found.lock().searching = true;
                let f = found.clone();
                let spawned = std::thread::Builder::new().name("openq-dmx-find".into()).spawn(move || {
                    let nodes = crate::light::artnet::discover(std::time::Duration::from_millis(1500));
                    let ports = crate::light::enttec::ports();
                    let mut f = f.lock();
                    f.searching = false;
                    f.ports = ports;
                    match nodes {
                        Ok(n) => {
                            f.nodes = n;
                            f.error = None;
                        }
                        Err(e) => f.error = Some(format!("{e:#}")),
                    }
                });
                if let Err(e) = spawned {
                    let mut f = found.lock();
                    f.searching = false;
                    f.error = Some(e.to_string());
                }
            }
            let f = self.light.found.lock();
            if f.searching {
                ui.spinner();
            } else if let Some(e) = &f.error {
                ui.label(RichText::new(e).color(theme::RED));
            } else if !f.nodes.is_empty() || !f.ports.is_empty() {
                ui.label(RichText::new(tr!("{n} Art-Net nodes, {p} serial ports", n = f.nodes.len().to_string(), p = f.ports.len().to_string())).color(theme::TEXT_DIM));
            }
        });
        ui.add_space(6.0);
        let found = self.light.found.lock();
        let mut remove = None;
        for (i, o) in l.outputs.iter_mut().enumerate() {
            egui::Frame::new()
                .fill(theme::BG_RAISED)
                .corner_radius(8.0)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut o.enabled, "");
                        ui.add(egui::TextEdit::singleline(&mut o.name).desired_width(140.0));
                        ui.label(RichText::new(o.protocol.label()).color(theme::TEXT_DIM));
                        match &mut o.protocol {
                            OutputProtocol::ArtNet { target, sync } => {
                                ui.label(t("Destination"));
                                ui.add(egui::TextEdit::singleline(target).hint_text(t("broadcast")).desired_width(140.0));
                                if !found.nodes.is_empty() {
                                    egui::ComboBox::from_id_salt(("nodes", i)).selected_text(t("Nodes")).show_ui(ui, |ui| {
                                        for n in &found.nodes {
                                            let outs = n.outputs.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(", ");
                                            let r = ui.selectable_label(false, format!("{} {} ({outs})", n.ip, n.short_name));
                                            if r.clicked() {
                                                *target = n.ip.to_string();
                                            }
                                            // Shift-click also takes the node's universes.
                                            if r.clicked() && ui.input(|i| i.modifiers.shift) && !n.outputs.is_empty() {
                                                o.routes = n.outputs.iter().map(|&w| UniverseRoute { universe: w + 1, wire: w }).collect();
                                            }
                                        }
                                    })
                                    .response
                                    .on_hover_text(t("Shift-click a node to also send it the universes it outputs"));
                                }
                                ui.checkbox(sync, "ArtSync").on_hover_text(t("Nodes change every universe at once (sync packet after each frame)"));
                            }
                            OutputProtocol::Sacn { target, priority } => {
                                ui.label(t("Destination"));
                                ui.add(egui::TextEdit::singleline(target).hint_text(t("multicast")).desired_width(140.0));
                                ui.label(t("Priority"));
                                ui.add(DragValue::new(priority).range(0..=200));
                            }
                            OutputProtocol::EnttecPro { port } => {
                                ui.label(t("Port"));
                                ui.add(egui::TextEdit::singleline(port).hint_text("/dev/ttyUSB0").desired_width(140.0));
                                if !found.ports.is_empty() {
                                    egui::ComboBox::from_id_salt(("ports", i)).selected_text(t("Ports")).show_ui(ui, |ui| {
                                        for p in &found.ports {
                                            if ui.selectable_label(false, p).clicked() {
                                                *port = p.clone();
                                            }
                                        }
                                    });
                                }
                            }
                        }
                        if ui.small_button(icons::DELETE).on_hover_text(t("Remove output")).clicked() {
                            remove = Some(i);
                        }
                    });
                    theme::wrapped_row(ui, |ui| {
                        ui.label(t("Universes"));
                        let mut drop = None;
                        for (ri, r) in o.routes.iter_mut().enumerate() {
                            ui.add(DragValue::new(&mut r.universe).range(1..=crate::light::model::MAX_UNIVERSE));
                            ui.label("→");
                            ui.add(DragValue::new(&mut r.wire).range(0..=32767)).on_hover_text(t("Universe number on the wire (Art-Net counts from 0)"));
                            if ui.small_button(icons::CLOSE).clicked() {
                                drop = Some(ri);
                            }
                            ui.add_space(8.0);
                        }
                        if let Some(ri) = drop {
                            o.routes.remove(ri);
                        }
                        if ui.small_button(icons::ADD).on_hover_text(t("Add universe")).clicked() {
                            let last = o.routes.last().copied().unwrap_or(UniverseRoute { universe: 0, wire: 0 });
                            o.routes.push(UniverseRoute { universe: last.universe + 1, wire: last.wire + 1 });
                        }
                    });
                    // A unicast output to a discovered node that doesn't output some of the universes.
                    if let OutputProtocol::ArtNet { target, .. } = &o.protocol
                        && let Some(node) = found.nodes.iter().find(|n| n.ip.to_string() == target.trim())
                    {
                        let missing: Vec<String> = o.routes.iter().filter(|r| !node.outputs.contains(&r.wire)).map(|r| r.wire.to_string()).collect();
                        if !missing.is_empty() {
                            let text = tr!("{node} doesn't output wire universe {list}", node = node.short_name.clone(), list = missing.join(", "));
                            ui.label(RichText::new(theme::with_icon(icons::WARNING, &text)).color(theme::AMBER).small());
                        }
                    }
                    // Live status (outputs are matched by name: the light thread runs the enabled ones).
                    if o.enabled
                        && let Some(h) = handles.iter().find(|h| h.name == o.name)
                    {
                        let st = h.status();
                        let (c, text) = match (&st.error, st.open) {
                            (Some(e), _) => (theme::RED, e.clone()),
                            (None, true) => (theme::GREEN, tr!("sending · {n} packets", n = st.packets.to_string())),
                            (None, false) => (theme::AMBER, t("opening…").to_string()),
                        };
                        ui.label(RichText::new(text).color(c).small());
                    }
                });
            ui.add_space(4.0);
        }
        drop(found);
        if let Some(i) = remove {
            l.outputs.remove(i);
        }
        if l.outputs.is_empty() {
            ui.label(RichText::new(t("No outputs: add Art-Net, sACN or a USB interface to send DMX.")).color(theme::TEXT_DIM));
        }
        ui.add_space(10.0);
        Self::inputs_section(ui, l, &input_handles);
    }

    /// DMX inputs: Art-Net/sACN from another desk, merged into the output or kept for capture.
    fn inputs_section(ui: &mut Ui, l: &mut Lighting, handles: &[crate::light::dmxin::InputHandle]) {
        use crate::light::dmx::UniverseRoute;
        use crate::light::dmxin::{InputConfig, InputProtocol, MergeMode};
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("Inputs")));
            for p in [InputProtocol::ArtNet, InputProtocol::Sacn] {
                if ui.button(theme::with_icon(icons::ADD, &tr!("{protocol} input", protocol = p.label()))).clicked() {
                    let universe = l.fixtures.iter().map(|f| f.universe).min().unwrap_or(1);
                    let wire = if p == InputProtocol::ArtNet { universe - 1 } else { universe };
                    l.inputs.push(InputConfig {
                        name: format!("{} in", p.label()),
                        protocol: p,
                        routes: vec![UniverseRoute { universe, wire }],
                        ..Default::default()
                    });
                }
            }
        });
        ui.add_space(4.0);
        let mut remove = None;
        for (i, input) in l.inputs.iter_mut().enumerate() {
            egui::Frame::new()
                .fill(theme::BG_RAISED)
                .corner_radius(8.0)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut input.enabled, "");
                        ui.add(egui::TextEdit::singleline(&mut input.name).desired_width(140.0));
                        ui.label(RichText::new(input.protocol.label()).color(theme::TEXT_DIM));
                        egui::ComboBox::from_id_salt(("merge", i)).selected_text(input.merge.label()).show_ui(ui, |ui| {
                            for m in MergeMode::ALL {
                                ui.selectable_value(&mut input.merge, m, m.label());
                            }
                        });
                        if input.protocol == InputProtocol::Sacn {
                            ui.label(t("Interface"));
                            ui.add(egui::TextEdit::singleline(&mut input.interface).hint_text(t("default")).desired_width(120.0))
                                .on_hover_text(t("IPv4 address of the network interface that receives multicast"));
                        }
                        if ui.small_button(icons::DELETE).on_hover_text(t("Remove input")).clicked() {
                            remove = Some(i);
                        }
                    });
                    theme::wrapped_row(ui, |ui| {
                        ui.label(t("Universes"));
                        let mut drop = None;
                        for (ri, r) in input.routes.iter_mut().enumerate() {
                            ui.add(DragValue::new(&mut r.wire).range(0..=32767)).on_hover_text(t("Universe number on the wire (Art-Net counts from 0)"));
                            ui.label("→");
                            ui.add(DragValue::new(&mut r.universe).range(1..=crate::light::model::MAX_UNIVERSE));
                            if ui.small_button(icons::CLOSE).clicked() {
                                drop = Some(ri);
                            }
                            ui.add_space(8.0);
                        }
                        if let Some(ri) = drop {
                            input.routes.remove(ri);
                        }
                        if ui.small_button(icons::ADD).on_hover_text(t("Add universe")).clicked() {
                            let last = input.routes.last().copied().unwrap_or(UniverseRoute { universe: 0, wire: 0 });
                            input.routes.push(UniverseRoute { universe: last.universe + 1, wire: last.wire + 1 });
                        }
                    });
                    if input.enabled
                        && let Some(h) = handles.iter().find(|h| h.name == input.name)
                    {
                        let st = h.status();
                        let (c, text) = match (&st.error, st.open) {
                            (Some(e), _) => (theme::RED, e.clone()),
                            (None, true) if st.sources.is_empty() => (theme::AMBER, t("listening · no source").to_string()),
                            (None, true) => (theme::GREEN, tr!("receiving from {sources} · {n} packets", sources = st.sources.join(", "), n = st.packets.to_string())),
                            (None, false) => (theme::AMBER, t("opening…").to_string()),
                        };
                        ui.label(RichText::new(text).color(c).small());
                    }
                });
            ui.add_space(4.0);
        }
        if let Some(i) = remove {
            l.inputs.remove(i);
        }
        if l.inputs.is_empty() {
            ui.label(RichText::new(t("No inputs: add one to merge or capture DMX from another desk.")).color(theme::TEXT_DIM));
        }
    }

    /// The latest DMX frame, one universe at a time.
    fn monitor_page(&mut self, ui: &mut Ui, show: &mut Show) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        let frame = show.media.light.frame();
        if frame.universes.is_empty() {
            ui.label(RichText::new(t("Nothing patched: no universes to show.")).color(theme::TEXT_DIM));
            return;
        }
        if frame.get(self.light.monitor_universe).is_none() {
            self.light.monitor_universe = frame.universes[0].id;
        }
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("Universe")));
            for u in &frame.universes {
                ui.selectable_value(&mut self.light.monitor_universe, u.id, u.id.to_string());
            }
        });
        let Some(u) = frame.get(self.light.monitor_universe) else { return };
        // Fixture starts, to mark them in the grid.
        let starts: std::collections::HashMap<u16, FixtureId> =
            show.ws.lighting.fixtures.iter().filter(|f| f.universe == u.id && f.address > 0).map(|f| (f.address, f.id)).collect();
        let cols = 32;
        let rows = 512 / cols;
        // Use the room there is: 16 rows filling the height (scrolls below 30 px a row).
        let cell = egui::vec2(((ui.available_width() - 4.0) / cols as f32).floor().max(24.0), ((ui.available_height() - 4.0) / rows as f32).floor().clamp(30.0, 64.0));
        egui::ScrollArea::vertical().id_salt("dmx_monitor").auto_shrink(false).show(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(cell.x * cols as f32, cell.y * rows as f32), egui::Sense::hover());
            let p = ui.painter();
            for (i, v) in u.data.iter().enumerate() {
                let r = egui::Rect::from_min_size(rect.min + egui::vec2((i % cols) as f32 * cell.x, (i / cols) as f32 * cell.y), cell).shrink(1.0);
                if !ui.is_rect_visible(r) {
                    continue;
                }
                let k = *v as f32 / 255.0;
                let fill = theme::BG_RAISED.lerp_to_gamma(theme::ACCENT, k * 0.8);
                p.rect_filled(r, 2.0, fill);
                let slot = i as u16 + 1;
                if starts.contains_key(&slot) {
                    p.line_segment([r.left_top(), r.left_bottom()], egui::Stroke::new(2.0, theme::AMBER));
                }
                p.text(r.left_top() + egui::vec2(4.0, 2.0), egui::Align2::LEFT_TOP, slot.to_string(), egui::FontId::proportional(9.5), Color32::WHITE);
                let size = (cell.y * 0.38).clamp(11.0, 16.0);
                p.text(r.right_bottom() - egui::vec2(4.0, 3.0), egui::Align2::RIGHT_BOTTOM, v.to_string(), theme::mono(size), if *v > 0 { Color32::WHITE } else { theme::TEXT_FAINT });
            }
        });
    }
}

/// How a pool card is lit: idle, in use by the selection, a running effect, an effect stopped by
/// the programmer.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum CardLook {
    Off,
    On,
    Running,
    Stopping,
}

/// Lay `items` out as cards in rows filling the width (clear of the scroll bar, or the panel
/// would grow a little every frame); `card` draws one at the size given.
pub(super) fn card_grid<T>(ui: &mut Ui, items: &[T], mut card: impl FnMut(&mut Ui, &T, egui::Vec2)) {
    const GAP: f32 = 6.0;
    let avail = ui.available_width() - 14.0;
    let cols = ((avail + GAP) / (76.0 + GAP)).floor().max(1.0);
    let size = egui::vec2(((avail + GAP) / cols - GAP).floor(), 44.0);
    ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
    for row in items.chunks(cols as usize) {
        ui.horizontal(|ui| {
            for item in row {
                card(ui, item, size);
            }
        });
    }
}

/// A card of the programmer's pools (groups, presets, effects): icon, number at the top right,
/// name at the bottom, and a colour bar (colour presets).
pub(super) fn pool_card(ui: &mut Ui, size: egui::Vec2, glyph: &str, number: u32, name: &str, look: CardLook, colour: Option<Color32>) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    let (fill, border, fg, icon) = match look {
        CardLook::On => (theme::accent_bg(), theme::BLUE, theme::TEXT, theme::TEXT_DIM),
        CardLook::Running => (theme::RED.gamma_multiply(0.6), theme::RED, Color32::WHITE, Color32::WHITE),
        CardLook::Stopping => (if r.hovered() { theme::BG_HOVER } else { theme::BG_RAISED }, theme::RED, theme::RED, theme::TEXT_DIM),
        CardLook::Off => (if r.hovered() { theme::BG_HOVER } else { theme::BG_RAISED }, theme::BORDER, theme::TEXT, theme::TEXT_DIM),
    };
    painter.rect_filled(rect, 6.0, fill);
    painter.rect_stroke(rect, 6.0, egui::Stroke::new(1.0, border), egui::StrokeKind::Inside);
    let inner = rect.shrink2(egui::vec2(8.0, 6.0));
    painter.text(inner.left_top(), egui::Align2::LEFT_TOP, glyph, theme::icon_font(14.0), icon);
    painter.text(inner.right_top(), egui::Align2::RIGHT_TOP, number.to_string(), theme::mono(10.5), theme::TEXT_FAINT);
    painter.with_clip_rect(inner).text(inner.left_bottom(), egui::Align2::LEFT_BOTTOM, name, egui::FontId::proportional(12.5), fg);
    if let Some(c) = colour {
        let bar = egui::Rect::from_min_size(egui::pos2(rect.left() + 4.0, rect.bottom() - 4.0), egui::vec2(rect.width() - 8.0, 2.5));
        painter.rect_filled(bar, 1.0, c);
    }
    r
}

/// Frame of a side column of a page (presets, encoders, cue details): a rounded panel with a gap
/// towards the page instead of a divider line (dragging the gap, the panel's edge, resizes it).
fn side_column(left: bool) -> egui::Frame {
    let gap = egui::Margin { left: if left { 0 } else { 10 }, right: if left { 10 } else { 0 }, top: 0, bottom: 0 };
    egui::Frame::new().fill(theme::BG_PANEL).corner_radius(8.0).inner_margin(egui::Margin::same(10)).outer_margin(gap)
}

/// Colour presets (RGB) for the swatch row: white, warm white, then round the hue circle.
const SWATCHES: [[f32; 3]; 12] = [
    [1.0, 1.0, 1.0],
    [1.0, 0.85, 0.6],
    [1.0, 0.0, 0.0],
    [1.0, 0.35, 0.0],
    [1.0, 0.6, 0.0],
    [1.0, 1.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, 1.0, 1.0],
    [0.0, 0.3, 1.0],
    [0.0, 0.0, 1.0],
    [0.6, 0.2, 1.0],
    [1.0, 0.0, 1.0],
];

pub(super) fn to_c32(c: [f32; 3]) -> Color32 {
    Color32::from_rgb((c[0].clamp(0.0, 1.0) * 255.0) as u8, (c[1].clamp(0.0, 1.0) * 255.0) as u8, (c[2].clamp(0.0, 1.0) * 255.0) as u8)
}

/// CIE 1931 xy diagram sized to the width it gets: every visible colour (the spectral locus), the
/// colours of `gamut` bright and the rest dimmed, the gamut's outline and corners (the emitters),
/// and the current colour. Returns the colour picked (clamped into the gamut).
fn cie_field(ui: &mut Ui, gamut: &[[f32; 2]], current: Option<[f32; 2]>) -> Option<[f32; 2]> {
    use crate::light::colour::{self, LOCUS, WHITE_POINT};
    // x 0..0.8, y 0..0.9 at one scale.
    let w = ui.available_width();
    let h = (w * 0.9 / 0.8).clamp(140.0, 320.0);
    let (rect, r) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click_and_drag());
    let scale = (w / 0.8).min(h / 0.9);
    let off = egui::vec2((w - 0.8 * scale) / 2.0, (h - 0.9 * scale) / 2.0);
    let to_screen = |p: [f32; 2]| egui::pos2(rect.left() + off.x + p[0] * scale, rect.bottom() - off.y - p[1] * scale);
    let to_xy = |p: egui::Pos2| [(p.x - rect.left() - off.x) / scale, (rect.bottom() - off.y - p.y) / scale];
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, theme::BG_INPUT);
    // The edge of the visible colours, with the purple line from 700 nm back to 380 nm.
    let mut edge: Vec<[f32; 2]> = LOCUS.to_vec();
    let (end, start) = (LOCUS[LOCUS.len() - 1], LOCUS[0]);
    for k in 1..12 {
        let u = k as f32 / 12.0;
        edge.push([end[0] + (start[0] - end[0]) * u, end[1] + (start[1] - end[1]) * u]);
    }
    let inside = |p: [f32; 2]| {
        let q = colour::clamp_into(gamut, p);
        (q[0] - p[0]).hypot(q[1] - p[1]) < 0.002
    };
    let shade = |p: [f32; 2]| {
        let c = to_c32(colour::display(p));
        if inside(p) { c } else { c.gamma_multiply(0.22) }
    };
    // Rings of quads from the white point out to the edge.
    const RINGS: usize = 8;
    let mut mesh = egui::Mesh::default();
    let n = edge.len();
    for i in 0..n {
        let (a, b) = (edge[i], edge[(i + 1) % n]);
        for k in 0..RINGS {
            let lerp = |e: [f32; 2], u: f32| [WHITE_POINT[0] + (e[0] - WHITE_POINT[0]) * u, WHITE_POINT[1] + (e[1] - WHITE_POINT[1]) * u];
            let (u0, u1) = (k as f32 / RINGS as f32, (k + 1) as f32 / RINGS as f32);
            let quad = [lerp(a, u0), lerp(b, u0), lerp(b, u1), lerp(a, u1)];
            let base = mesh.vertices.len() as u32;
            for p in quad {
                mesh.colored_vertex(to_screen(p), shade(p));
            }
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base, base + 2, base + 3);
        }
    }
    painter.add(mesh);
    if gamut.len() >= 2 {
        let mut outline: Vec<egui::Pos2> = gamut.iter().map(|p| to_screen(*p)).collect();
        outline.push(outline[0]);
        painter.add(egui::Shape::line(outline, egui::Stroke::new(1.5, Color32::WHITE)));
        for p in gamut {
            painter.circle(to_screen(*p), 3.5, to_c32(colour::display(*p)), egui::Stroke::new(1.0, Color32::WHITE));
        }
    }
    if let Some(c) = current {
        let dot = to_screen(c);
        painter.circle_stroke(dot, 6.0, egui::Stroke::new(2.0, Color32::WHITE));
        painter.circle_stroke(dot, 7.5, egui::Stroke::new(1.0, Color32::BLACK));
    }
    let mut out = None;
    if let Some(pos) = r.interact_pointer_pos()
        && (r.dragged() || r.clicked())
    {
        out = Some(colour::clamp_into(gamut, to_xy(pos)));
    }
    if let Some(pos) = r.hover_pos() {
        let [x, y] = to_xy(pos);
        r.on_hover_text(format!("x {x:.3} · y {y:.3}"));
    }
    out
}

/// An attribute value bar (0..=1; `None` = not in the programmer, drawn empty with "—"; a
/// preset reference shows its number).
/// Click/drag sets the value, the mouse wheel steps it by 1 %. Returns the new value if changed.
fn attr_bar(ui: &mut Ui, value: Option<f32>, preset: Option<PresetId>, width: f32) -> Option<f32> {
    let (rect, r) = ui.allocate_exact_size(egui::vec2(width.max(60.0), 26.0), egui::Sense::click_and_drag());
    let mut out = None;
    if let Some(pos) = r.interact_pointer_pos()
        && (r.dragged() || r.clicked())
    {
        out = Some(((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0));
    }
    if r.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y + i.smooth_scroll_delta.x);
        if scroll != 0.0 {
            out = Some((value.unwrap_or(0.0) + scroll.signum() * 0.01).clamp(0.0, 1.0));
        }
    }
    let shown = out.or(value);
    let p = ui.painter();
    p.rect_filled(rect, 5.0, theme::BG_INPUT);
    if let Some(v) = shown {
        let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * v, rect.height()));
        p.rect_filled(fill, 5.0, theme::RED.gamma_multiply(0.55));
    }
    let stroke = if r.hovered() { theme::TEXT_FAINT } else { theme::BORDER };
    p.rect_stroke(rect, 5.0, egui::Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    let mut text = shown.map_or_else(|| "—".to_string(), |v| format!("{:.1} %", v * 100.0));
    if let (Some(p), None) = (preset, out) {
        text = format!("P{p} · {text}");
    }
    p.text(rect.center(), egui::Align2::CENTER_CENTER, text, theme::mono(12.0), if shown.is_some() { Color32::WHITE } else { theme::TEXT_FAINT });
    r.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    out
}

/// Whole wheel notches turned over a hovered widget this frame (down = positive). Touchpad pixels
/// add up per widget (`id`) until they make a notch. The panel under it doesn't scroll.
fn wheel_steps(ui: &Ui, id: egui::Id) -> i32 {
    const NOTCH: f32 = 50.0;
    let (lines, points) = ui.input(|i| {
        i.events.iter().fold((0.0, 0.0), |(l, p), e| match e {
            // Ctrl/Cmd + wheel zooms the UI.
            egui::Event::MouseWheel { modifiers, .. } if modifiers.command || modifiers.ctrl => (l, p),
            egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, .. } => (l, p + delta.y),
            egui::Event::MouseWheel { delta, .. } => (l + delta.y, p),
            _ => (l, p),
        })
    });
    ui.input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
    let key = id.with("wheel");
    let mut acc = ui.data(|d| d.get_temp::<f32>(key)).unwrap_or(0.0) + points / NOTCH + lines;
    let steps = acc.trunc();
    acc -= steps;
    ui.data_mut(|d| d.insert_temp(key, acc));
    -steps as i32
}

/// Where a value sits within a range, 0..=1 from its `from` end to its `to` end.
fn within_range(r: &crate::light::model::ChannelRange, value: f32) -> f32 {
    let span = r.to as f32 - r.from as f32;
    if span == 0.0 { 0.0 } else { ((value * 255.0 - r.from as f32) / span).clamp(0.0, 1.0) }
}

/// One named range drawn in `rect`: slot number, picture (gobo image, else the slot colour as a
/// dot; room kept when other slots have one) and name (after the preset it comes from, if any).
fn slot_face(ui: &Ui, rect: egui::Rect, i: usize, r: &crate::light::model::ChannelRange, picture: Option<&egui::TextureHandle>, thumbs: bool, preset: Option<PresetId>) {
    let p = ui.painter().with_clip_rect(rect);
    let mut x = rect.left();
    p.text(egui::pos2(x, rect.center().y), egui::Align2::LEFT_CENTER, (i + 1).to_string(), theme::mono(11.0), theme::TEXT_FAINT);
    x += 20.0;
    if thumbs {
        let side = (rect.height() - 2.0).min(30.0);
        let thumb = egui::Rect::from_center_size(egui::pos2(x + side / 2.0, rect.center().y), egui::vec2(side, side));
        if let Some(tex) = picture {
            p.circle_filled(thumb.center(), side / 2.0, Color32::BLACK);
            p.image(tex.id(), thumb, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        } else if let Some([cr, cg, cb]) = r.color {
            p.circle_filled(thumb.center(), side * 0.36, Color32::from_rgb(cr, cg, cb));
            p.circle_stroke(thumb.center(), side * 0.36, egui::Stroke::new(1.0, theme::BORDER));
        } else if r.name.trim().eq_ignore_ascii_case("open") {
            // An open slot: the whole beam.
            p.circle_filled(thumb.center(), side / 2.0, Color32::BLACK);
            p.circle_filled(thumb.center(), side * 0.4, Color32::WHITE);
        }
        x += side + 8.0;
    }
    let name = if r.name.is_empty() { format!("DMX {}–{}", r.from, r.to) } else { r.name.clone() };
    let name = match preset {
        Some(pr) => format!("P{pr} · {name}"),
        None => name,
    };
    p.text(egui::pos2(x, rect.center().y), egui::Align2::LEFT_CENTER, name, egui::FontId::proportional(13.0), theme::TEXT);
}

/// A horizontal level (0..=1) set by clicking or dragging, with the percentage on it.
pub(super) fn level_bar(ui: &mut Ui, value: &mut f32, color: Color32) -> egui::Response {
    level_bar_sized(ui, value, color, 22.0)
}

/// [`level_bar`] `height` tall.
pub(super) fn level_bar_sized(ui: &mut Ui, value: &mut f32, color: Color32, height: f32) -> egui::Response {
    let (rect, mut r) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click_and_drag());
    if let Some(pos) = r.interact_pointer_pos()
        && (r.dragged() || r.clicked())
    {
        let v = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        if v != *value {
            *value = v;
            r.mark_changed();
        }
    }
    let p = ui.painter();
    p.rect_filled(rect, 4.0, theme::BG_INPUT);
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * *value, rect.height()));
    p.rect_filled(fill, 4.0, color.gamma_multiply(0.7));
    p.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
    p.text(rect.center(), egui::Align2::CENTER_CENTER, format!("{:.0} %", *value * 100.0), theme::mono(11.5), Color32::WHITE);
    r.on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
}

/// Patch `form.count` fixtures of a type; returns how many fit.
fn add_fixtures(l: &mut Lighting, ty: &FixtureType, form: &mut AddForm) -> u32 {
    let Some(mode) = ty.mode(&form.mode).cloned() else { return 0 };
    l.add_type(ty);
    let footprint = mode.footprint().max(1);
    let mut id = l.next_fixture_id(form.first_id);
    let mut address = form.address;
    let mut added = 0;
    for _ in 0..form.count {
        let start = if address == 0 {
            l.next_free_address(form.universe, footprint)
        } else {
            (address + footprint - 1 <= 512).then_some(address)
        };
        let Some(start) = start else { break };
        l.fixtures.push(Fixture {
            id,
            fixture_type: ty.id.clone(),
            mode: mode.name.clone(),
            universe: form.universe,
            address: start,
            ..Default::default()
        });
        // Further DMX breaks: the next free addresses in the same universe (unpatched if full).
        for b in 1..mode.breaks() {
            let at = l.next_free_address(form.universe, mode.footprint_of(b).max(1)).unwrap_or(0);
            if let Some(f) = l.fixtures.last_mut() {
                f.breaks.push(crate::light::model::DmxAddress { universe: form.universe, address: at });
            }
        }
        added += 1;
        if address != 0 {
            address = start + footprint;
        }
        id = l.next_fixture_id(id + 1);
    }
    form.first_id = id;
    form.address = 0;
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wheel lines are notches; touchpad pixels add up to them; Ctrl + wheel is left to zoom.
    #[test]
    fn wheel_notches() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("slots");
        let frame = |unit, y: f32, ctrl: bool| {
            let modifiers = egui::Modifiers { ctrl, ..Default::default() };
            let ev = egui::Event::MouseWheel { unit, delta: egui::vec2(0.0, y), phase: egui::TouchPhase::Move, modifiers };
            let mut got = 0;
            let _ = ctx.run_ui(egui::RawInput { events: vec![ev], ..Default::default() }, |ui| got = wheel_steps(ui, id));
            got
        };
        assert_eq!(frame(egui::MouseWheelUnit::Line, -1.0, false), 1, "down = next");
        assert_eq!(frame(egui::MouseWheelUnit::Line, 2.0, false), -2);
        assert_eq!(frame(egui::MouseWheelUnit::Line, -1.0, true), 0);
        assert_eq!(frame(egui::MouseWheelUnit::Point, -30.0, false), 0);
        assert_eq!(frame(egui::MouseWheelUnit::Point, -30.0, false), 1, "pixels add up");
    }

    /// A formula's first effect lands in the effect it belongs to; extra effects join the pool.
    #[test]
    fn formula_results_apply_to_the_effect() {
        let mut light = LightUi::default();
        let mut l = Lighting::default();
        l.effects.push(Effect { id: 4, name: "Mine".into(), speed: 2.0, ..Default::default() });
        let code = "param depth 0.5\neffect \"A\" steps 2 width 1\ndimmer = t * depth * 2\neffect \"B\" steps 3 width 1\nred = 1";
        start_formula(&mut light, &mut l.effects[0], code.into(), false);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while light.formula_run.is_some() && std::time::Instant::now() < deadline {
            poll_formula(&mut light, &mut l);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(light.formula_out.clone().unwrap(), (4, None));
        let e = &l.effects[0];
        assert_eq!((e.name.as_str(), e.speed, e.shape), ("Mine", 2.0, Shape::Custom), "an existing effect keeps its name and settings");
        assert_eq!(e.table.as_ref().unwrap().values, [0.0, 0.5]);
        assert_eq!((e.formula.as_str(), e.params.len()), (code, 1));
        assert_eq!((l.effects[1].id, l.effects[1].name.as_str(), l.effects[1].attribute), (5, "B", Attribute::Red));
        // A mistake is reported and leaves the table alone.
        start_formula(&mut light, &mut l.effects[0], "effect \"A\" steps 2 width 1\ndimmer = nope".into(), false);
        while light.formula_run.is_some() {
            poll_formula(&mut light, &mut l);
        }
        assert!(light.formula_out.clone().unwrap().1.unwrap().contains("nope"));
        assert_eq!(l.effects[0].table.as_ref().unwrap().values, [0.0, 0.5]);
    }

    #[test]
    fn fan() {
        assert_eq!(fan_values(3, 0.0, 1.0, false), [0.0, 0.5, 1.0]);
        assert_eq!(fan_values(5, 0.2, 1.0, true), [1.0, 0.6, 0.2, 0.6, 1.0]);
        assert_eq!(fan_values(1, 0.3, 1.0, false), [0.3]);
    }

    #[test]
    fn cue_numbers() {
        let mut s = Sequence::default();
        assert_eq!(next_cue_number(&s), "1");
        for n in ["1", "2.5", "x"] {
            s.cues.push(LightCue { number: n.into(), ..Default::default() });
        }
        assert_eq!(next_cue_number(&s), "3");
        assert!(number_key("2.5") < number_key("10"));
    }

    #[test]
    fn add_fixtures_fills_free_addresses() {
        let mut l = Lighting::default();
        let ty = crate::light::library::generic("generic/rgb").unwrap();
        let mut form = AddForm { type_id: ty.id.clone(), count: 4, address: 10, ..Default::default() };
        assert_eq!(add_fixtures(&mut l, &ty, &mut form), 4);
        assert_eq!(l.fixtures.iter().map(|f| f.address).collect::<Vec<_>>(), [10, 13, 16, 19]);
        assert_eq!(form.first_id, 5);
        form.count = 3;
        assert_eq!(add_fixtures(&mut l, &ty, &mut form), 3);
        assert_eq!(l.fixtures[4].address, 1, "auto: first gap");
        assert_eq!(l.fixtures[5].address, 4);
        assert_eq!(l.fixtures[6].address, 7);
        assert!(l.validate().is_empty());
        form.count = 200;
        assert_eq!(add_fixtures(&mut l, &ty, &mut form), (512 - 7 * 3) / 3);
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    #[test]
    fn shape_handles_move_and_snap() {
        let bar = PlotShape { id: 1, kind: ShapeKind::Bar, a: [-2.0, 0.0], b: [2.0, 0.0], height: 6.0, ..Default::default() };
        assert_eq!(handles(&bar).len(), 2);
        let m = moved(&bar, Handle::Body, egui::vec2(0.52, -1.01));
        assert_eq!((m.a, m.b), ([-1.5, -1.0], [2.5, -1.0]), "moved whole, snapped to 5 cm, length kept");
        let m = moved(&bar, Handle::B, egui::vec2(1.0, 1.0));
        assert_eq!((m.a, m.b), ([-2.0, 0.0], [3.0, 1.0]));
        let r = PlotShape { kind: ShapeKind::Rect, a: [0.0, 0.0], b: [4.0, 3.0], ..Default::default() };
        assert_eq!(handles(&r).len(), 4);
        let m = moved(&r, Handle::AxBz, egui::vec2(-1.0, 1.0));
        assert_eq!((m.a, m.b), ([-1.0, 0.0], [4.0, 4.0]), "corner (a.x, b.z) moves a.x and b.z");
        let m = moved(&r, Handle::BxAz, egui::vec2(1.0, -1.0));
        assert_eq!((m.a, m.b), ([0.0, -1.0], [5.0, 3.0]));
    }

    #[test]
    fn segment_distance_clamps_to_the_ends() {
        let (a, b) = (egui::pos2(0.0, 0.0), egui::pos2(10.0, 0.0));
        assert_eq!(segment_distance(egui::pos2(5.0, 3.0), a, b), 3.0);
        assert_eq!(segment_distance(egui::pos2(13.0, 4.0), a, b), 5.0);
        assert_eq!(segment_distance(egui::pos2(1.0, 1.0), a, a), 2f32.sqrt());
    }
}
