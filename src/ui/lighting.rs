//! The Lighting workspace: lighting design (patch, programmer, sequences, outputs) in Edit mode,
//! playback only in Show mode.
//!
//! Every design edit goes through [`App::can_design`], so Show mode (and replicas without the
//! Edit role) can't change the design. The UI never compiles or sends DMX itself: edits mark the
//! show edited and the light thread picks up the recompiled design.

use super::{App, Mode, icons, theme};
use crate::i18n::t;
use crate::light::model::{
    Attribute, Feature, FixtureId, FixtureType, Group, LightCue, LightCueId, Lighting, Look, PatchIssue, Preset, PresetId, Sequence, SequenceId, Value,
};
use crate::light::effects::{Effect, EffectId, EffectRun, Shape};
use crate::light::{Fixture, library};
use crate::show::Show;
use egui::{Color32, DragValue, RichText, Ui};
use std::collections::HashSet;

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
    /// 3D visualiser.
    Stage,
    /// DMX outputs.
    Outputs,
    /// DMX monitor.
    Monitor,
}

impl LightTab {
    /// A page by name (`OPENQ_SCREENSHOT_LIGHTING`).
    pub(crate) fn from_name(name: &str) -> Self {
        match name {
            "programmer" | "colour" | "keypad" => LightTab::Programmer,
            "effects" => LightTab::Effects,
            "3d" => LightTab::Stage,
            "sequences" => LightTab::Sequences,
            "outputs" => LightTab::Outputs,
            "monitor" => LightTab::Monitor,
            _ => LightTab::Patch,
        }
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
    patch_sel: HashSet<FixtureId>,
    add: AddForm,
    /// Programmer selection, in selection order.
    pub(crate) selection: Vec<FixtureId>,
    /// Programmer values (override playback while set; recorded into cues).
    pub(crate) programmer: Look,
    /// Effects running from the programmer (recorded into cues).
    pub(crate) programmer_effects: Vec<EffectRun>,
    /// Effects as last sent to the light thread.
    sent_effects: Option<Vec<(Effect, Vec<FixtureId>)>>,
    /// Show edit generation when they were sent.
    sent_gen: u64,
    /// Effect shown in the editor.
    pub(crate) effect: Option<EffectId>,
    /// Tap tempo: recent taps.
    taps: Vec<std::time::Instant>,
    /// Command line text, history and the result of the last command.
    cmd_line: String,
    cmd_history: Vec<String>,
    cmd_back: Option<usize>,
    cmd_result: Option<Result<String, String>>,
    /// Keypad window open.
    pub(crate) keypad: bool,
    /// The programmer as last sent to the light thread.
    sent: Option<(Look, bool, Option<Vec<FixtureId>>)>,
    /// Blind: the programmer isn't output (still recordable).
    blind: bool,
    /// Highlight: selected fixtures at open white full on the output (not recorded).
    highlight: bool,
    /// Highlight steps through the selection one fixture at a time (index into it).
    step: Option<usize>,
    /// Spread tool settings.
    fan: FanTool,
    /// Encoder page.
    pub(crate) feature: Option<Feature>,
    /// Sequence shown on the Sequences page / recorded into.
    sequence: Option<SequenceId>,
    /// Selected cue on the Sequences page.
    cue: Option<LightCueId>,
    /// Number for the next recorded cue (empty = after the last).
    record_number: String,
    /// Feature stored by "New preset" (None = every attribute).
    preset_feature: Option<Feature>,
    /// Universe shown in the DMX monitor.
    monitor_universe: u16,
    /// Art-Net nodes / serial ports found in the background.
    found: std::sync::Arc<parking_lot::Mutex<Found>>,
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

/// Fixture types offered when patching: the show's own first, then built-ins not in the show.
fn available_types(l: &Lighting) -> Vec<FixtureType> {
    let mut v = l.fixture_types.clone();
    v.extend(library::generics().into_iter().filter(|g| l.fixture_type(&g.id).is_none()));
    v
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
    /// Pick an Open Fixture Library file and add its type to the design (outside the show lock:
    /// the dialog blocks). The Add form switches to it.
    pub(super) fn import_fixture(&mut self) {
        if !self.can_design() {
            return;
        }
        let Some(path) = rfd::FileDialog::new().add_filter(t("Open Fixture Library fixture"), &["json"]).pick_file() else { return };
        let manufacturer = path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let result = std::fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|s| crate::light::ofl::import(&s, &manufacturer));
        match result {
            Ok(imp) => {
                let id = imp.fixture.id.clone();
                let name = format!("{} {}", imp.fixture.manufacturer, imp.fixture.name);
                {
                    let mut show = self.show.lock();
                    let l = &mut show.ws.lighting;
                    // Importing again replaces the type (patched fixtures keep their mode by name).
                    l.fixture_types.retain(|t| t.id != id);
                    l.fixture_types.push(imp.fixture);
                    show.edited();
                }
                self.light.add.type_id = id;
                self.light.add.mode.clear();
                let msg = if imp.notes.is_empty() {
                    tr!("Imported {name}", name = name)
                } else {
                    tr!("Imported {name} ({n} notes in the error log)", name = name, n = imp.notes.len().to_string())
                };
                for n in &imp.notes {
                    log::warn!("fixture import: {n}");
                }
                self.notify(msg);
            }
            Err(e) => self.notify_error(tr!("Can't import the fixture: {e}", e = format!("{e:#}"))),
        }
    }

    /// Design edits are allowed (Edit mode, and this device may edit the show).
    pub(super) fn can_design(&self) -> bool {
        self.mode == Mode::Edit && self.can_edit_show()
    }

    /// Cues / Lighting switch above the central panel.
    pub(super) fn view_tabs(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            theme::tab(ui, &mut self.view, View::Cues, icons::RECEIPT_LONG, t("Cues"));
            theme::tab(ui, &mut self.view, View::Lighting, icons::LIGHTBULB, t("Lighting"));
        });
    }

    /// The Lighting workspace (central panel).
    pub(super) fn lighting_view(&mut self, ui: &mut Ui, show: &mut Show) {
        egui::Panel::bottom("executors")
            .frame(egui::Frame::new().fill(theme::BG_PANEL).inner_margin(egui::Margin::symmetric(8, 6)))
            .resizable(false)
            .show(ui, |ui| self.executor_bar(ui, show));
        egui::Panel::top("command_line").frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 4))).show(ui, |ui| self.command_bar(ui, show));
        self.keypad_window(ui.ctx(), show);
        if !self.can_design() {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icons::LOCK).font(theme::icon_font(16.0)).color(theme::TEXT_DIM));
                ui.label(RichText::new(t("The lighting design is locked in Show mode.")).color(theme::TEXT_DIM));
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let mut tab = if self.light.tab == LightTab::Stage { LightTab::Stage } else { LightTab::Monitor };
                theme::tab(ui, &mut tab, LightTab::Stage, icons::VIEW_IN_AR, t("3D"));
                theme::tab(ui, &mut tab, LightTab::Monitor, icons::GRID_VIEW, t("DMX monitor"));
                self.light.tab = tab;
            });
            ui.separator();
            if self.light.tab == LightTab::Stage {
                self.visualiser_page(ui, show);
            } else {
                self.monitor_page(ui, show);
            }
            return;
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            theme::tab(ui, &mut self.light.tab, LightTab::Patch, icons::CABLE, t("Patch"));
            theme::tab(ui, &mut self.light.tab, LightTab::Programmer, icons::TUNE, t("Programmer"));
            theme::tab(ui, &mut self.light.tab, LightTab::Effects, icons::AUTO_FIX_HIGH, t("Effects"));
            theme::tab(ui, &mut self.light.tab, LightTab::Sequences, icons::PLAY_CIRCLE, t("Sequences"));
            theme::tab(ui, &mut self.light.tab, LightTab::Stage, icons::VIEW_IN_AR, t("3D"));
            theme::tab(ui, &mut self.light.tab, LightTab::Outputs, icons::ROUTER, t("Outputs"));
            theme::tab(ui, &mut self.light.tab, LightTab::Monitor, icons::GRID_VIEW, t("DMX monitor"));
        });
        ui.separator();
        let before = show.ws.lighting.clone();
        match self.light.tab {
            LightTab::Patch => self.patch_page(ui, &mut show.ws.lighting),
            LightTab::Programmer => self.programmer_page(ui, &mut show.ws.lighting),
            LightTab::Effects => self.effects_page(ui, &mut show.ws.lighting),
            LightTab::Sequences => self.sequences_page(ui, show),
            LightTab::Stage => self.visualiser_page(ui, show),
            LightTab::Outputs => self.outputs_page(ui, show),
            LightTab::Monitor => self.monitor_page(ui, show),
        }
        if show.ws.lighting != before {
            show.edited();
        }
    }

    /// Per-frame lighting housekeeping: the programmer is cleared outside design (Show mode),
    /// and handed to the light thread when it changed.
    pub(super) fn lighting_frame(&mut self, show: &mut Show) {
        let designing = self.can_design();
        let l = &mut self.light;
        if !designing && (!l.programmer.is_empty() || !l.programmer_effects.is_empty()) {
            l.programmer.clear();
            l.programmer_effects.clear();
        }
        let design = &show.ws.lighting;
        let effects: Vec<(Effect, Vec<FixtureId>)> = if l.blind {
            Vec::new()
        } else {
            l.programmer_effects.iter().filter_map(|r| Some((design.effect(r.effect)?.clone(), r.fixtures.clone()))).collect()
        };
        // Sent again after a recompile too (the light thread drops them when slots move).
        if l.sent_effects.as_ref() != Some(&effects) || show.edit_gen != l.sent_gen {
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
        if l.sent.as_ref() != Some(&key) {
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
                let Some(mode) = design.fixture(*f).and_then(|x| design.mode_of(x)) else { continue };
                for a in mode.attributes() {
                    if let Some(v) = highlight_value(mode, a) {
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
        let types = available_types(l);
        // Add fixtures.
        let form = &mut self.light.add;
        let mut short = None;
        let ty = types.iter().find(|t| t.id == form.type_id).or(types.first()).cloned();
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::caption(t("Add fixtures")));
            let label = |t: &FixtureType| format!("{} {}", t.manufacturer, t.name);
            egui::ComboBox::from_id_salt("patch_type")
                .selected_text(ty.as_ref().map(label).unwrap_or_default())
                .width(200.0)
                .show_ui(ui, |ui| {
                    for t in &types {
                        ui.selectable_value(&mut form.type_id, t.id.clone(), label(t));
                    }
                });
            if let Some(ty) = &ty {
                let mode = ty.mode(&form.mode).map(|m| m.name.clone()).unwrap_or_default();
                egui::ComboBox::from_id_salt("patch_mode").selected_text(&mode).show_ui(ui, |ui| {
                    for m in &ty.modes {
                        ui.selectable_value(&mut form.mode, m.name.clone(), &m.name);
                    }
                });
            }
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
            if ui.button(theme::with_icon(icons::ADD, t("Add"))).clicked()
                && let Some(ty) = &ty
            {
                let added = add_fixtures(l, ty, form);
                if added < form.count {
                    short = Some(added);
                }
            }
        });
        if let Some(n) = short {
            self.notify(tr!("Only {n} fixtures fit in the universe", n = n.to_string()));
        }
        // Selection actions and problems.
        let issues = l.validate();
        if ui
            .button(theme::with_icon(icons::UPLOAD_FILE, t("Import fixture…")))
            .on_hover_text(t("A fixture file from the Open Fixture Library (open-fixture-library.org), .json"))
            .clicked()
        {
            self.deferred.push(super::Deferred::ImportFixture);
        }
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
        ui.add_space(4.0);
        self.patch_sheet(ui, l, &issues);
    }

    /// The fixture table (only visible rows are laid out, so big rigs stay fast).
    fn patch_sheet(&mut self, ui: &mut Ui, l: &mut Lighting, issues: &[PatchIssue]) {
        const COLS: [f32; 8] = [60.0, 180.0, 200.0, 120.0, 70.0, 70.0, 50.0, 200.0];
        let bad: HashSet<FixtureId> = issues
            .iter()
            .flat_map(|i| match i {
                PatchIssue::Overlap(a, b) => vec![*a, *b],
                PatchIssue::DuplicateId(f) | PatchIssue::UnknownType(f) | PatchIssue::OutOfRange(f) => vec![*f],
            })
            .collect();
        let header = [t("No."), t("Name"), t("Type"), t("Mode"), t("Universe"), t("Address"), t("Slots"), t("Position (m)")];
        ui.horizontal(|ui| {
            for (h, w) in header.iter().zip(COLS) {
                ui.add_sized([w, 18.0], egui::Label::new(theme::caption(h)));
            }
        });
        let row_h = 24.0;
        let mut renumber = None;
        let mut click: Option<(FixtureId, egui::Modifiers)> = None;
        let Lighting { fixtures, fixture_types, .. } = l;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, row_h, fixtures.len(), |ui, range| {
            for f in &mut fixtures[range] {
                let ty = fixture_types.iter().find(|t| t.id == f.fixture_type);
                let selected = self.light.patch_sel.contains(&f.id);
                let row = ui.horizontal(|ui| {
                    ui.set_height(row_h - 2.0);
                    let mut id = f.id;
                    let r = ui.add_sized([COLS[0], 20.0], DragValue::new(&mut id).range(1..=99_999));
                    if id != f.id {
                        renumber = Some((f.id, id, r.dragged()));
                    }
                    ui.add_sized([COLS[1], 20.0], egui::TextEdit::singleline(&mut f.name).hint_text(ty.map_or("", |t| t.name.as_str())));
                    let ty_label = ty.map_or_else(|| f.fixture_type.clone(), |t| format!("{} {}", t.manufacturer, t.name));
                    let sel = ui.add_sized([COLS[2], 20.0], egui::Button::selectable(selected, ty_label));
                    if sel.clicked() {
                        click = Some((f.id, ui.input(|i| i.modifiers)));
                    }
                    match ty {
                        Some(ty) if ty.modes.len() > 1 => {
                            egui::ComboBox::from_id_salt(("fx_mode", f.id)).selected_text(&f.mode).width(COLS[3] - 8.0).show_ui(ui, |ui| {
                                for m in &ty.modes {
                                    ui.selectable_value(&mut f.mode, m.name.clone(), &m.name);
                                }
                            });
                        }
                        _ => {
                            ui.add_sized([COLS[3], 20.0], egui::Label::new(RichText::new(&f.mode).color(theme::TEXT_DIM)));
                        }
                    }
                    ui.add_sized([COLS[4], 20.0], DragValue::new(&mut f.universe).range(1..=crate::light::model::MAX_UNIVERSE));
                    ui.add_sized([COLS[5], 20.0], DragValue::new(&mut f.address).range(0..=512).custom_formatter(|v, _| {
                        if v == 0.0 { "—".to_string() } else { format!("{v}") }
                    }));
                    let slots = ty.and_then(|t| t.mode(&f.mode)).map_or(0, |m| m.footprint());
                    ui.add_sized([COLS[6], 20.0], egui::Label::new(RichText::new(slots.to_string()).color(theme::TEXT_DIM)));
                    for v in &mut f.position {
                        ui.add(DragValue::new(v).speed(0.05).max_decimals(2));
                    }
                });
                let r = row.response.rect;
                if bad.contains(&f.id) {
                    ui.painter().rect_stroke(r, 3.0, egui::Stroke::new(1.0, theme::AMBER), egui::StrokeKind::Inside);
                } else if selected {
                    ui.painter().rect_filled(r, 3.0, Color32::from_rgba_unmultiplied(0x4a, 0x7c, 0xf0, 24));
                }
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


/// What highlight puts on an attribute: open white at full (a channel's own highlight value
/// wins); None leaves the attribute alone.
fn highlight_value(mode: &crate::light::model::FixtureMode, a: Attribute) -> Option<f32> {
    if let Some(h) = mode.channel(a).and_then(|c| c.highlight) {
        return Some(h);
    }
    use Attribute::*;
    match a {
        Dimmer | Red | Green | Blue | White => Some(1.0),
        Amber | Lime | Uv | Cyan | Magenta | Yellow | Cto | ColorWheel | Gobo | Prism | Frost => Some(0.0),
        Shutter | Iris => Some(1.0),
        _ => None,
    }
}

/// A cue number as typed ("5", "2.5").
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 { format!("{}", n as i64) } else { format!("{n}") }
}

/// Tap tempo: add a tap and return the tempo of the recent taps (a pause over 2 s starts again).
fn tap(taps: &mut Vec<std::time::Instant>, now: std::time::Instant) -> Option<f32> {
    if taps.last().is_some_and(|l| now.duration_since(*l).as_secs_f32() > 2.0) {
        taps.clear();
    }
    taps.push(now);
    if taps.len() > 5 {
        taps.remove(0);
    }
    let n = taps.len();
    (n >= 2).then(|| 60.0 * (n - 1) as f32 / taps[n - 1].duration_since(taps[0]).as_secs_f32())
}

/// Effect parameters.
fn effect_editor(ui: &mut Ui, e: &mut Effect) {
    egui::Grid::new("effect_editor").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label(t("Name"));
        ui.add(egui::TextEdit::singleline(&mut e.name).desired_width(220.0));
        ui.end_row();
        ui.label(t("Shape"));
        egui::ComboBox::from_id_salt("fx_shape").selected_text(e.shape.label()).show_ui(ui, |ui| {
            for s in Shape::ALL {
                ui.selectable_value(&mut e.shape, s, s.label());
            }
        });
        ui.end_row();
        ui.label(t("Attribute"));
        if e.shape == Shape::Circle {
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
    });
}

/// The waveform over two cycles with a moving dot per fixture.
fn effect_preview(ui: &mut Ui, e: &Effect, n: usize) {
    let w = ui.available_width().min(640.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 140.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 6.0, theme::BG_INPUT);
    let plot = rect.shrink(10.0);
    let y = |v: f32| plot.bottom() - v.clamp(0.0, 1.0) * plot.height();
    let pts: Vec<egui::Pos2> = (0..=200)
        .map(|i| {
            let x = i as f64 / 100.0;
            egui::pos2(plot.left() + plot.width() * (x / 2.0) as f32, y(e.shape.eval(x, e.duty, 1)))
        })
        .collect();
    p.add(egui::Shape::line(pts, egui::Stroke::new(1.5, theme::ACCENT)));
    let t = ui.input(|i| i.time);
    let rate = e.rate(120.0) as f64;
    for i in 0..n {
        let x = (t * rate + e.phase(i, n) as f64).rem_euclid(1.0);
        let v = e.shape.eval(t * rate + e.phase(i, n) as f64, e.duty, i as u32 + 1);
        let pos = egui::pos2(plot.left() + plot.width() * (x / 2.0) as f32, y(v));
        p.circle_filled(pos, 4.0, theme::AMBER);
        // A bar per fixture along the bottom shows its level now.
        let bw = plot.width() / n as f32;
        let bar = egui::Rect::from_min_size(egui::pos2(plot.left() + i as f32 * bw + 1.0, rect.bottom() - 6.0), egui::vec2(bw - 2.0, 4.0));
        p.rect_filled(bar, 1.0, theme::AMBER.gamma_multiply(v.clamp(0.05, 1.0)));
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

/// Attributes of a fixture (its mode's, plus a virtual dimmer).
fn attributes_of(l: &Lighting, id: FixtureId) -> Vec<Attribute> {
    l.fixture(id).and_then(|f| l.mode_of(f)).map(|m| m.attributes()).unwrap_or_default()
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
        // Groups pool and programmer actions.
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::caption(t("Groups")));
            let mut remove = None;
            for g in &l.groups {
                let label = if g.name.is_empty() { g.id.to_string() } else { format!("{} {}", g.id, g.name) };
                let r = ui.button(label);
                if r.clicked() {
                    let add = ui.input(|i| i.modifiers.command || i.modifiers.shift);
                    if !add {
                        self.light.selection.clear();
                    }
                    for f in &g.fixtures {
                        if !self.light.selection.contains(f) {
                            self.light.selection.push(*f);
                        }
                    }
                }
                r.context_menu(|ui| {
                    if ui.button(theme::with_icon(icons::SAVE, t("Store the selection here"))).clicked() {
                        remove = Some((g.id, false));
                    }
                    if ui.button(theme::with_icon(icons::DELETE, t("Delete group"))).clicked() {
                        remove = Some((g.id, true));
                    }
                });
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
            ui.add_enabled_ui(!self.light.selection.is_empty(), |ui| {
                if ui.button(theme::with_icon(icons::ADD, t("Group from selection"))).on_hover_text(t("Right-click a group to overwrite or delete it")).clicked() {
                    let id = l.groups.iter().map(|g| g.id).max().unwrap_or(0) + 1;
                    l.groups.push(Group { id, name: String::new(), fixtures: self.light.selection.clone() });
                }
            });
        });
        ui.add_space(2.0);
        self.preset_pool(ui, l);
        ui.add_space(2.0);
        self.effect_pool_row(ui, l);
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let n = self.light.selection.len();
            ui.label(RichText::new(tr!("{n} selected", n = n.to_string())).color(theme::TEXT_DIM));
            if ui.button(theme::with_icon(icons::SELECT_ALL, t("Select all"))).clicked() {
                self.light.selection = l.fixtures.iter().map(|f| f.id).collect();
            }
            if ui.button(theme::with_icon(icons::DESELECT, t("Select none"))).clicked() {
                self.light.selection.clear();
            }
            ui.separator();
            let busy = !self.light.programmer.is_empty() || !self.light.programmer_effects.is_empty();
            ui.add_enabled_ui(busy, |ui| {
                if ui.button(theme::with_icon(icons::CLOSE, t("Clear programmer"))).clicked() {
                    self.light.programmer.clear();
                    self.light.programmer_effects.clear();
                }
            });
            if busy {
                let n: usize = self.light.programmer.values().map(|v| v.len()).sum();
                ui.label(RichText::new(tr!("{n} values in the programmer", n = n.to_string())).color(theme::RED));
            }
            ui.separator();
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
                    ui.label(RichText::new(tr!("fixture {f}", f = f.to_string())).color(theme::AMBER));
                }
            }
            ui.separator();
            self.record_controls(ui, l);
        });
        ui.separator();
        // Encoders in a side panel of their own (resizable, scrolls), tiles fill the rest.
        egui::Panel::right("encoders")
            .frame(egui::Frame::new().fill(theme::BG_PANEL).corner_radius(8.0).inner_margin(egui::Margin::same(10)))
            .resizable(true)
            .default_size(420.0)
            .size_range(320.0..=640.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("encoders_scroll").auto_shrink(false).show(ui, |ui| self.encoders(ui, l));
            });
        ui.add_space(2.0);
        self.fixture_tiles(ui, l);
    }

    /// Presets: click applies to the selection (as references), right-click overwrites/deletes.
    fn preset_pool(&mut self, ui: &mut Ui, l: &mut Lighting) {
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::caption(t("Presets")));
            enum Act {
                Apply(PresetId),
                Store(PresetId),
                Delete(PresetId),
            }
            let mut action = None;
            for p in &l.presets {
                let glyph = p.feature.map_or(icons::CATEGORY, feature_icon);
                let label = if p.name.is_empty() { p.id.to_string() } else { format!("{} {}", p.id, p.name) };
                let r = ui.add(egui::Button::new(theme::with_icon(glyph, &label)).min_size(egui::vec2(0.0, 26.0)));
                // A colour preset shows its colour (from its first fixture).
                if p.feature == Some(Feature::Colour)
                    && let Some(v) = p.values.values().next()
                    && let (Some(r_), Some(g), Some(b)) = (v.get(&Attribute::Red), v.get(&Attribute::Green), v.get(&Attribute::Blue))
                {
                    let bar = egui::Rect::from_min_size(egui::pos2(r.rect.left() + 4.0, r.rect.bottom() - 4.0), egui::vec2(r.rect.width() - 8.0, 2.5));
                    ui.painter().rect_filled(bar, 1.0, to_c32([*r_, *g, *b]));
                }
                let r = r.on_hover_text(p.feature.map_or_else(|| t("All attributes").to_string(), |f| f.label().to_string()));
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
            }
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
                        p.values = values;
                    }
                }
                None => {}
            }
            let label = self.light.preset_feature.map_or_else(|| t("All attributes").to_string(), |f| f.label().to_string());
            egui::ComboBox::from_id_salt("preset_feature").selected_text(label).width(110.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.light.preset_feature, None, t("All attributes"));
                for f in Feature::ALL {
                    ui.selectable_value(&mut self.light.preset_feature, Some(f), f.label());
                }
            });
            let values = self.programmer_values(l, self.light.preset_feature);
            if ui
                .add_enabled(!values.is_empty(), egui::Button::new(theme::with_icon(icons::ADD, t("Preset from programmer"))))
                .on_hover_text(t("Stores the selected fixtures' programmer values; right-click a preset to overwrite or delete it"))
                .clicked()
            {
                let id = l.presets.iter().map(|p| p.id).max().unwrap_or(0) + 1;
                l.presets.push(Preset { id, name: String::new(), feature: self.light.preset_feature, values });
            }
        });
    }

    /// Effects: click runs the effect on the selection from the programmer (click again stops).
    fn effect_pool_row(&mut self, ui: &mut Ui, l: &Lighting) {
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::caption(t("Effects")));
            for e in &l.effects {
                let stopping = self.light.programmer_effects.iter().any(|r| r.effect == e.id && r.stop);
                let running = self.light.programmer_effects.iter().any(|r| r.effect == e.id && !r.stop);
                let label = if e.name.is_empty() { e.id.to_string() } else { format!("{} {}", e.id, e.name) };
                let (fill, fg) = match (running, stopping) {
                    (true, _) => (theme::RED.gamma_multiply(0.6), Color32::WHITE),
                    (_, true) => (theme::BG_RAISED, theme::RED),
                    _ => (theme::BG_RAISED, theme::TEXT),
                };
                let r = ui
                    .add(egui::Button::new(RichText::new(theme::with_icon(icons::AUTO_FIX_HIGH, &label)).color(fg)).fill(fill).min_size(egui::vec2(0.0, 26.0)))
                    .on_hover_text(format!("{} · {}", e.attribute.label(), e.shape.label()));
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
            }
            if l.effects.is_empty() {
                ui.label(RichText::new(t("Build effects on the Effects page.")).color(theme::TEXT_DIM));
            }
        });
    }

    /// The effects pool and the editor of the chosen effect, with a live preview.
    fn effects_page(&mut self, ui: &mut Ui, l: &mut Lighting) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(240.0);
                ui.horizontal(|ui| {
                    ui.label(theme::caption(t("Effects")));
                    if ui.small_button(icons::ADD).on_hover_text(t("New effect")).clicked() {
                        let id = l.effects.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                        l.effects.push(Effect { id, ..Default::default() });
                        self.light.effect = Some(id);
                    }
                });
                let mut delete = None;
                for e in &l.effects {
                    ui.horizontal(|ui| {
                        let label = if e.name.is_empty() { format!("{} · {}", e.id, e.shape.label()) } else { format!("{} {}", e.id, e.name) };
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
            });
            ui.separator();
            ui.vertical(|ui| {
                let Some(e) = self.light.effect.and_then(|id| l.effects.iter_mut().find(|e| e.id == id)) else {
                    ui.label(RichText::new(t("Choose or add an effect.")).color(theme::TEXT_DIM));
                    return;
                };
                effect_editor(ui, e);
                ui.add_space(10.0);
                let n = self.light.selection.len().clamp(1, 24);
                ui.label(theme::caption(&tr!("Preview across {n} fixtures", n = n.to_string())));
                effect_preview(ui, e, n);
            });
        });
    }

    /// The command line: Enter runs it, Up/Down walk the history, Esc clears.
    fn command_bar(&mut self, ui: &mut Ui, show: &mut Show) {
        let id = egui::Id::new("light_command_line");
        ui.horizontal(|ui| {
            ui.label(RichText::new(icons::KEYBOARD).font(theme::icon_font(18.0)).color(theme::TEXT_DIM));
            let w = ui.available_width() - 330.0;
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

    fn run_commands(&mut self, show: &mut Show, cmds: &[crate::light::command::Cmd]) -> Result<String, String> {
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
            match *c {
                Cmd::Select(op, set) => {
                    let ids: Vec<FixtureId> = match set {
                        SelSet::Fixture(f) => l.fixture(f).map(|x| vec![x.id]).ok_or_else(|| tr!("no fixture {f}", f = f.to_string()))?,
                        SelSet::Range(a, b) => {
                            let mut v: Vec<FixtureId> = l.fixtures.iter().map(|f| f.id).filter(|id| (a..=b).contains(id)).collect();
                            v.sort();
                            v
                        }
                        SelSet::Group(g) => l.groups.iter().find(|x| x.id == g).map(|x| x.fixtures.clone()).ok_or_else(|| tr!("no group {g}", g = g.to_string()))?,
                        SelSet::All => l.fixtures.iter().map(|f| f.id).collect(),
                    };
                    let sel = &mut self.light.selection;
                    match op {
                        SelOp::Replace => *sel = ids,
                        SelOp::Add => sel.extend(ids.into_iter().filter(|f| !sel.contains(f)).collect::<Vec<_>>()),
                        SelOp::Remove => sel.retain(|f| !ids.contains(f)),
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
                    let feature = l.preset(p).map_or(self.light.preset_feature, |x| x.feature);
                    let values = self.programmer_values(l, feature);
                    match l.presets.iter_mut().find(|x| x.id == p) {
                        Some(x) => x.values = values,
                        None => l.presets.push(Preset { id: p, name: String::new(), feature, values }),
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
            show.edited();
        }
        Ok(done)
    }

    /// Record / update controls (programmer → a sequence).
    fn record_controls(&mut self, ui: &mut Ui, l: &mut Lighting) {
        let seq_id = self.light.sequence.filter(|s| l.sequence(*s).is_some()).or(l.sequences.first().map(|s| s.id));
        egui::ComboBox::from_id_salt("rec_seq")
            .selected_text(seq_id.and_then(|s| l.sequence_label(s)).unwrap_or_else(|| t("new sequence").into()))
            .show_ui(ui, |ui| {
                for s in &l.sequences {
                    ui.selectable_value(&mut self.light.sequence, Some(s.id), l.sequence_label(s.id).unwrap_or_default());
                }
            });
        let hint = seq_id.and_then(|s| l.sequence(s)).map_or_else(|| "1".to_string(), next_cue_number);
        ui.add(egui::TextEdit::singleline(&mut self.light.record_number).hint_text(hint.clone()).desired_width(50.0));
        let can = !self.light.programmer.is_empty() || !self.light.programmer_effects.is_empty();
        if ui.add_enabled(can, egui::Button::new(theme::with_icon(icons::FIBER_MANUAL_RECORD, t("Record cue")))).clicked() {
            let number = Some(self.light.record_number.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or(hint);
            self.record_cue(l, seq_id, number);
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
        let seq = match seq.filter(|s| l.sequence(*s).is_some()) {
            Some(id) => id,
            None => {
                let id = seq.unwrap_or(1);
                l.sequences.push(Sequence { id, name: t("Main").into(), ..Default::default() });
                id
            }
        };
        let s = l.sequences.iter_mut().find(|s| s.id == seq).expect("sequence");
        let values = self.light.programmer.clone();
        let effects = self.light.programmer_effects.clone();
        if let Some(c) = s.cues.iter_mut().find(|c| c.number == number) {
            c.values = values;
            c.effects = effects;
            self.light.cue = Some(c.id);
        } else {
            let c = LightCue { number, values, effects, ..Default::default() };
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

    /// Fixture tiles: click to select (Ctrl/Shift adds). Each shows its number, name, type, the
    /// programmer intensity (bar) and colour (swatch), and the selection order.
    fn fixture_tiles(&mut self, ui: &mut Ui, l: &Lighting) {
        const GAP: f32 = 6.0;
        // Tiles stretch so a row fills the width exactly.
        let avail = ui.available_width() - 14.0;
        let per_row = ((avail + GAP) / (132.0 + GAP)).floor().max(1.0);
        let tile = egui::vec2(((avail + GAP) / per_row - GAP).floor(), 86.0);
        egui::ScrollArea::vertical().id_salt("tiles").auto_shrink(false).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
                for f in &l.fixtures {
                    let (rect, r) = ui.allocate_exact_size(tile, egui::Sense::click());
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    let order = self.light.selection.iter().position(|x| *x == f.id);
                    let prog = self.light.programmer.get(&f.id);
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
                    p.text(inner.left_top(), egui::Align2::LEFT_TOP, f.id.to_string(), theme::semibold(17.0), theme::TEXT);
                    if let Some(i) = order {
                        p.text(inner.right_top(), egui::Align2::RIGHT_TOP, format!("#{}", i + 1), theme::mono(10.5), theme::BLUE);
                    }
                    let name = if f.name.is_empty() { ty.map_or("", |t| t.name.as_str()) } else { f.name.as_str() };
                    let clip = p.with_clip_rect(inner);
                    clip.text(inner.left_top() + egui::vec2(0.0, 24.0), egui::Align2::LEFT_TOP, name, egui::FontId::proportional(12.5), theme::TEXT);
                    if !f.name.is_empty() && let Some(ty) = ty {
                        clip.text(inner.left_top() + egui::vec2(0.0, 41.0), egui::Align2::LEFT_TOP, &ty.name, egui::FontId::proportional(10.5), theme::TEXT_FAINT);
                    }
                    // Programmer intensity bar and colour swatch.
                    let bar = egui::Rect::from_min_max(egui::pos2(inner.left(), inner.bottom() - 5.0), inner.right_bottom());
                    p.rect_filled(bar, 2.0, theme::BG_INPUT);
                    if let Some(v) = prog {
                        let c = |a| v.get(&a).and_then(|x| l.resolve(f.id, a, *x));
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
                    if r.clicked() {
                        let add = ui.input(|i| i.modifiers.command || i.modifiers.shift);
                        let sel = &mut self.light.selection;
                        match (add, order) {
                            (true, Some(i)) => {
                                sel.remove(i);
                            }
                            (true, None) => sel.push(f.id),
                            (false, _) => *sel = vec![f.id],
                        }
                    }
                }
            });
        });
    }

    /// Value of an attribute for the selection (the first selected fixture that has it).
    fn selection_value(&self, l: &Lighting, attr: Attribute) -> Option<f32> {
        self.light.selection.iter().find_map(|f| self.light.programmer.get(f).and_then(|v| v.get(&attr)).and_then(|v| l.resolve(*f, attr, *v)))
    }

    /// The preset an attribute of the selection refers to (first selected fixture that has it).
    fn selection_preset(&self, attr: Attribute) -> Option<PresetId> {
        self.light.selection.iter().find_map(|f| match self.light.programmer.get(f)?.get(&attr)? {
            Value::Preset { preset } => Some(*preset),
            Value::Level(_) => None,
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

    /// Set an attribute on every selected fixture that has it.
    fn set_selection(&mut self, l: &Lighting, attr: Attribute, value: f32) {
        for f in &self.light.selection {
            if attributes_of(l, *f).contains(&attr) {
                self.light.programmer.entry(*f).or_default().insert(attr, Value::Level(value.clamp(0.0, 1.0)));
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
            Feature::Colour => self.colour_picker(ui, l, &page),
            Feature::Position if page.contains(&Attribute::Pan) && page.contains(&Attribute::Tilt) => self.pan_tilt_pad(ui, l),
            _ => {}
        }
        ui.add_space(4.0);
        let gap = ui.spacing().item_spacing.x;
        // Room for the scroll bar on the right.
        let bar_w = ui.available_width() - 92.0 - 26.0 - 2.0 * gap - 10.0;
        for a in page {
            let current = self.selection_value(l, a);
            let preset = self.selection_preset(a);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(92.0, 26.0), egui::Sense::hover());
                let fg = if current.is_some() { theme::TEXT } else { theme::TEXT_DIM };
                ui.painter().with_clip_rect(r).text(r.left_center(), egui::Align2::LEFT_CENTER, a.label(), egui::FontId::proportional(13.0), fg);
                let w = bar_w;
                if let Some(v) = attr_bar(ui, current, preset, w) {
                    self.set_selection(l, a, v);
                }
                let clear = ui.add_enabled(current.is_some(), egui::Button::new(icons::CLOSE).min_size(egui::vec2(26.0, 26.0))).on_hover_text(t("Remove from the programmer"));
                if clear.clicked() {
                    for f in &self.light.selection {
                        if let Some(vals) = self.light.programmer.get_mut(f) {
                            vals.remove(&a);
                        }
                    }
                    self.light.programmer.retain(|_, v| !v.is_empty());
                }
            });
        }
        ui.add_space(6.0);
        ui.label(RichText::new(t("Drag a bar to set it, scroll over it for 1 % steps.")).weak().small());
        // Fan: spread one attribute across the selection in selection order.
        if self.light.selection.len() > 1 {
            ui.add_space(8.0);
            ui.separator();
            let attrs: Vec<Attribute> = {
                let mut v: Vec<Attribute> = self.light.selection.iter().flat_map(|f| attributes_of(l, *f)).collect();
                v.sort();
                v.dedup();
                v
            };
            let FanTool(attr, mut a, mut b, mut centre) = self.light.fan;
            let attr = attr.filter(|x| attrs.contains(x)).or(attrs.first().copied());
            let mut pick = attr;
            ui.horizontal_wrapped(|ui| {
                ui.label(theme::caption(t("Spread")));
                egui::ComboBox::from_id_salt("fan_attr").selected_text(pick.map_or("", |x| x.label())).width(110.0).show_ui(ui, |ui| {
                    for x in &attrs {
                        ui.selectable_value(&mut pick, Some(*x), x.label());
                    }
                });
                ui.label(t("from"));
                ui.add(percent_drag(&mut a));
                ui.label(t("to"));
                ui.add(percent_drag(&mut b));
                ui.checkbox(&mut centre, t("centre out"));
            });
            self.light.fan = FanTool(pick, a, b, centre);
            if ui.button(theme::with_icon(icons::TUNE, t("Spread across the selection"))).clicked()
                && let Some(attr) = pick
            {
                let who: Vec<FixtureId> = self.light.selection.iter().copied().filter(|f| attributes_of(l, *f).contains(&attr)).collect();
                for (f, v) in who.iter().zip(fan_values(who.len(), a, b, centre)) {
                    self.light.programmer.entry(*f).or_default().insert(attr, Value::Level(v));
                }
            }
        }
    }

    /// RGB/CMY picker for the selection.
    fn colour_picker(&mut self, ui: &mut Ui, l: &Lighting, page: &[Attribute]) {
        let rgb = page.contains(&Attribute::Red) && page.contains(&Attribute::Green) && page.contains(&Attribute::Blue);
        let cmy = page.contains(&Attribute::Cyan) && page.contains(&Attribute::Magenta) && page.contains(&Attribute::Yellow);
        if !rgb && !cmy {
            return;
        }
        let get = |a| self.selection_value(l, a).unwrap_or(0.0);
        let c = if rgb {
            [get(Attribute::Red), get(Attribute::Green), get(Attribute::Blue)]
        } else {
            [1.0 - get(Attribute::Cyan), 1.0 - get(Attribute::Magenta), 1.0 - get(Attribute::Yellow)]
        };
        if let Some(new) = colour_field(ui, c) {
            if rgb {
                self.set_selection(l, Attribute::Red, new[0]);
                self.set_selection(l, Attribute::Green, new[1]);
                self.set_selection(l, Attribute::Blue, new[2]);
            }
            if cmy {
                self.set_selection(l, Attribute::Cyan, 1.0 - new[0]);
                self.set_selection(l, Attribute::Magenta, 1.0 - new[1]);
                self.set_selection(l, Attribute::Yellow, 1.0 - new[2]);
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
            .frame(egui::Frame::new().fill(theme::BG_PANEL).corner_radius(8.0).inner_margin(egui::Margin::same(10)))
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
            // Cues of the chosen sequence.
            ui.vertical(|ui| {
                let Some(seq) = self.light.sequence.filter(|s| l.sequence(*s).is_some()).or(l.sequences.first().map(|s| s.id)) else {
                    ui.label(RichText::new(t("No sequences yet: record a cue from the Programmer.")).color(theme::TEXT_DIM));
                    return;
                };
                let current = status.iter().find(|x| x.id == seq).and_then(|x| x.cue);
                let now = std::time::Instant::now();
                ui.horizontal(|ui| {
                    if ui.button(theme::with_icon(icons::PLAY_ARROW, t("GO next"))).clicked() {
                        light.go(seq, None, now);
                    }
                    if ui.button(theme::with_icon(icons::STOP, t("Release"))).clicked() {
                        light.release(seq, 1.0, now);
                    }
                    let sel = self.light.cue.filter(|c| l.sequence(seq).is_some_and(|s| s.index_of(*c).is_some()));
                    ui.add_enabled_ui(sel.is_some(), |ui| {
                        if ui.button(theme::with_icon(icons::PLAY_CIRCLE, t("GO selected"))).clicked()
                            && let Some(c) = sel
                        {
                            light.go(seq, Some(c), now);
                        }
                        if ui.button(theme::with_icon(icons::UPLOAD_FILE, t("Load into programmer"))).clicked()
                            && let Some(c) = sel.and_then(|c| l.find_cue(c))
                        {
                            self.light.programmer = c.0.cues[c.1].values.clone();
                            self.light.programmer_effects = c.0.cues[c.1].effects.iter().filter(|r| !r.stop).cloned().collect();
                            self.light.selection = self.light.programmer.keys().copied().collect();
                        }
                        if ui.button(theme::with_icon(icons::DELETE, t("Delete cue"))).clicked()
                            && let Some(s) = l.sequences.iter_mut().find(|s| s.id == seq)
                        {
                            s.cues.retain(|c| Some(c.id) != sel);
                        }
                    });
                });
                ui.add_space(4.0);
                let Some(s) = l.sequences.iter_mut().find(|s| s.id == seq) else { return };
                const W: [f32; 6] = [24.0, 60.0, 220.0, 80.0, 80.0, 80.0];
                ui.horizontal(|ui| {
                    for (h, w) in ["", t("No."), t("Name"), t("Fade"), t("Down"), t("Delay")].iter().zip(W) {
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
                            ui.add_sized([W[0], 20.0], egui::Label::new(mark.font(theme::icon_font(14.0))));
                            ui.add(egui::TextEdit::singleline(&mut c.number).desired_width(W[1] - 8.0));
                            let name = if c.name.is_empty() { "—" } else { c.name.as_str() };
                            let (rect, r) = ui.allocate_exact_size(egui::vec2(W[2], 22.0), egui::Sense::click());
                            let fill = if selected { theme::accent_bg() } else if r.hovered() { theme::BG_HOVER } else { Color32::TRANSPARENT };
                            ui.painter().rect_filled(rect, 4.0, fill);
                            ui.painter().with_clip_rect(rect).text(
                                rect.left_center() + egui::vec2(6.0, 0.0),
                                egui::Align2::LEFT_CENTER,
                                name,
                                egui::FontId::proportional(13.0),
                                if selected { theme::BLUE } else { theme::TEXT },
                            );
                            if r.clicked() {
                                self.light.cue = Some(c.id);
                            }
                            ui.add_sized([W[3], 20.0], DragValue::new(&mut c.fade).range(0.0..=600.0).speed(0.05).suffix(" s"));
                            let mut down = c.down_fade.unwrap_or(c.fade);
                            if ui.add_sized([W[4], 20.0], DragValue::new(&mut down).range(0.0..=600.0).speed(0.05).suffix(" s")).changed() {
                                c.down_fade = Some(down);
                            }
                            ui.add_sized([W[5], 20.0], DragValue::new(&mut c.delay).range(0.0..=600.0).speed(0.05).suffix(" s"));
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
            let mut block = false;
            let c = &mut s.cues[i];
            ui.label(theme::caption(&tr!("Cue {n}", n = c.number.clone())));
            ui.add(egui::TextEdit::singleline(&mut c.name).hint_text(t("Name")).desired_width(f32::INFINITY));
            ui.add_space(4.0);
            ui.checkbox(&mut c.cue_only, t("Cue only")).on_hover_text(t("Its changes don't track: the next cue goes back to what was there before"));
            if ui.button(theme::with_icon(icons::LOCK, t("Block"))).on_hover_text(t("Store every tracked value in this cue, so edits to earlier cues no longer change it")).clicked() {
                block = true;
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
            if block {
                s.block(i);
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
        ui.checkbox(&mut l.stop_all_releases, t("Stop all also fades the lighting out"))
            .on_hover_text(t("Stop all, Panic and Stop cues without a target release every sequence"));
        ui.horizontal(|ui| {
            ui.label(t("Release fade"));
            ui.add(DragValue::new(&mut l.release_fade).range(0.0..=600.0).speed(0.05).suffix(" s"))
                .on_hover_text(t("Used when a Stop cue aimed at a Light cue releases its sequence"));
        });
    }
}


impl App {
    /// Executors (one per sequence: GO, release, level) plus grand master and blackout.
    /// Works in both modes; actions go through [`App::control`] so a replica drives the main device.
    fn executor_bar(&mut self, ui: &mut Ui, show: &mut Show) {
        use crate::control::{Command, LightControl};
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        let status = show.media.light.status();
        let masters = show.media.light.masters().clone();
        let l = &show.ws.lighting;
        let mut cmds = Vec::new();
        let card = |ui: &mut Ui, add: &mut dyn FnMut(&mut Ui)| {
            egui::Frame::new()
                .fill(theme::BG_RAISED)
                .corner_radius(8.0)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.set_width(150.0);
                        add(ui)
                    })
                });
        };
        egui::ScrollArea::horizontal().id_salt("executors").show(ui, |ui| {
            ui.horizontal(|ui| {
                // Masters.
                card(ui, &mut |ui| {
                    ui.label(theme::caption(t("Grand master")));
                    let mut gm = masters.grand_master;
                    if level_bar(ui, &mut gm, theme::AMBER).changed() {
                        cmds.push(LightControl::GrandMaster(gm));
                    }
                    let (fill, fg) = if masters.blackout { (theme::RED, Color32::WHITE) } else { (theme::BG_INPUT, theme::TEXT) };
                    if ui.add(egui::Button::new(RichText::new(t("Blackout")).color(fg)).fill(fill).min_size(egui::vec2(138.0, 24.0))).clicked() {
                        cmds.push(LightControl::Blackout(!masters.blackout));
                    }
                });
                card(ui, &mut |ui| {
                    ui.label(theme::caption(t("Tempo")));
                    let mut bpm = masters.bpm;
                    if ui.add(DragValue::new(&mut bpm).range(20.0..=300.0).speed(0.5).max_decimals(1).suffix(" BPM")).changed() {
                        cmds.push(LightControl::Tempo(bpm));
                    }
                    if ui.add(egui::Button::new(t("Tap")).min_size(egui::vec2(138.0, 24.0))).on_hover_text(t("Tap on the beat")).clicked()
                        && let Some(bpm) = tap(&mut self.light.taps, std::time::Instant::now())
                    {
                        cmds.push(LightControl::Tempo(bpm));
                    }
                });
                for seq in &l.sequences {
                    let st = status.iter().find(|x| x.id == seq.id);
                    let cue = st.and_then(|x| x.cue).and_then(|c| seq.index_of(c)).map(|i| &seq.cues[i]);
                    let next = match cue {
                        Some(c) => seq.index_of(c.id).and_then(|i| seq.cues.get(i + 1)),
                        None => seq.cues.first(),
                    };
                    card(ui, &mut |ui| {
                        let active = st.is_some_and(|x| x.active);
                        ui.horizontal(|ui| {
                            let mark = if active { theme::GREEN } else { theme::TEXT_FAINT };
                            ui.label(RichText::new(icons::FIBER_MANUAL_RECORD).font(theme::icon_font(10.0)).color(mark));
                            ui.label(RichText::new(l.sequence_label(seq.id).unwrap_or_default()).font(theme::semibold(12.5)));
                        });
                        let now_text = cue.map_or_else(|| "—".to_string(), |c| format!("{} {}", c.number, c.name));
                        ui.label(RichText::new(now_text).color(theme::TEXT).small());
                        let next_text = next.map_or_else(|| t("end").to_string(), |c| tr!("next: {cue}", cue = format!("{} {}", c.number, c.name)));
                        ui.label(RichText::new(next_text).color(theme::TEXT_DIM).small());
                        let mut level = masters.level(seq.id);
                        if level_bar(ui, &mut level, theme::ACCENT).changed() {
                            cmds.push(LightControl::Level(seq.id, level));
                        }
                        ui.horizontal(|ui| {
                            if ui.add_enabled(next.is_some(), egui::Button::new(RichText::new("GO").font(theme::semibold(13.0))).fill(theme::GO).min_size(egui::vec2(66.0, 24.0))).clicked() {
                                cmds.push(LightControl::Go(seq.id, None));
                            }
                            if ui.add_enabled(active, egui::Button::new(t("Release")).min_size(egui::vec2(66.0, 24.0))).clicked() {
                                cmds.push(LightControl::Release(Some(seq.id), 1.0));
                            }
                        });
                    });
                }
                if l.sequences.is_empty() {
                    ui.label(RichText::new(t("Sequences appear here as executors.")).color(theme::TEXT_DIM));
                }
            });
        });
        for c in cmds {
            self.control(show, Command::Light(c));
        }
    }

    /// DMX outputs: protocol, target, universes, live status; Art-Net discovery.
    fn outputs_page(&mut self, ui: &mut Ui, show: &mut Show) {
        use crate::light::dmx::{OutputConfig, OutputProtocol, UniverseRoute};
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        let handles = show.media.light.outputs().to_vec();
        let l = &mut show.ws.lighting;
        ui.horizontal(|ui| {
            for (label, p) in [
                ("Art-Net", OutputProtocol::ArtNet { target: String::new() }),
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
                let _ = std::thread::Builder::new().name("openq-dmx-find".into()).spawn(move || {
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
                            OutputProtocol::ArtNet { target } => {
                                ui.label(t("Destination"));
                                ui.add(egui::TextEdit::singleline(target).hint_text(t("broadcast")).desired_width(140.0));
                                if !found.nodes.is_empty() {
                                    egui::ComboBox::from_id_salt(("nodes", i)).selected_text(t("Nodes")).show_ui(ui, |ui| {
                                        for n in &found.nodes {
                                            let outs = n.outputs.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(", ");
                                            if ui.selectable_label(false, format!("{} {} ({outs})", n.ip, n.short_name)).clicked() {
                                                *target = n.ip.to_string();
                                            }
                                        }
                                    });
                                }
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
                    ui.horizontal_wrapped(|ui| {
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
                p.text(r.left_top() + egui::vec2(4.0, 2.0), egui::Align2::LEFT_TOP, slot.to_string(), egui::FontId::proportional(9.5), theme::TEXT_FAINT);
                let size = (cell.y * 0.38).clamp(11.0, 16.0);
                p.text(r.right_bottom() - egui::vec2(4.0, 3.0), egui::Align2::RIGHT_BOTTOM, v.to_string(), theme::mono(size), if *v > 0 { Color32::WHITE } else { theme::TEXT_FAINT });
            }
        });
    }
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

fn to_c32(c: [f32; 3]) -> Color32 {
    Color32::from_rgb((c[0].clamp(0.0, 1.0) * 255.0) as u8, (c[1].clamp(0.0, 1.0) * 255.0) as u8, (c[2].clamp(0.0, 1.0) * 255.0) as u8)
}

/// Colour picker sized to the width it gets: preset swatches, a saturation (x) / brightness (y)
/// field and a hue strip. Returns the new RGB when the user changed it.
fn colour_field(ui: &mut Ui, rgb: [f32; 3]) -> Option<[f32; 3]> {
    use egui::ecolor::Hsva;
    let id = ui.id().with("colour_field_hue");
    let mut hsv = Hsva::from_rgb(rgb);
    // Keep the hue while saturation or brightness is 0 (it can't be read back from the colour).
    let stored: Option<f32> = ui.data(|d| d.get_temp(id));
    if (hsv.s < 0.01 || hsv.v < 0.01) && let Some(h) = stored {
        hsv.h = h;
    }
    let mut out = None;
    let w = ui.available_width();
    // Swatches.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let n = SWATCHES.len() as f32;
        let side = ((w - 4.0 * (n - 1.0)) / n).clamp(16.0, 30.0);
        for sw in SWATCHES {
            let (r, resp) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
            ui.painter().rect_filled(r, 4.0, to_c32(sw));
            let stroke = if resp.hovered() { egui::Stroke::new(2.0, Color32::WHITE) } else { egui::Stroke::new(1.0, theme::BORDER) };
            ui.painter().rect_stroke(r, 4.0, stroke, egui::StrokeKind::Inside);
            if resp.clicked() {
                out = Some(sw);
            }
        }
    });
    ui.add_space(6.0);
    // Saturation / brightness field for the current hue (bilinear in s and v, so a grid of
    // vertex-coloured quads draws it exactly enough).
    let (field, fr) = ui.allocate_exact_size(egui::vec2(w, (w * 0.45).clamp(110.0, 170.0)), egui::Sense::click_and_drag());
    let mut mesh = egui::Mesh::default();
    const N: usize = 8;
    for yi in 0..=N {
        for xi in 0..=N {
            let (sx, vy) = (xi as f32 / N as f32, 1.0 - yi as f32 / N as f32);
            let c = Hsva::new(hsv.h, sx, vy, 1.0).to_rgb();
            let pos = egui::pos2(field.left() + sx * field.width(), field.top() + (1.0 - vy) * field.height());
            mesh.colored_vertex(pos, to_c32(c));
        }
    }
    for yi in 0..N {
        for xi in 0..N {
            let a = (yi * (N + 1) + xi) as u32;
            let b = a + 1;
            let c = a + (N + 1) as u32;
            mesh.add_triangle(a, b, c);
            mesh.add_triangle(b, c + 1, c);
        }
    }
    let p = ui.painter();
    p.add(mesh);
    p.rect_stroke(field, 4.0, egui::Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
    let dot = egui::pos2(field.left() + hsv.s * field.width(), field.top() + (1.0 - hsv.v) * field.height());
    p.circle_stroke(dot, 6.0, egui::Stroke::new(2.0, Color32::WHITE));
    p.circle_stroke(dot, 7.5, egui::Stroke::new(1.0, Color32::BLACK));
    if let Some(pos) = fr.interact_pointer_pos()
        && (fr.dragged() || fr.clicked())
    {
        let s = ((pos.x - field.left()) / field.width()).clamp(0.0, 1.0);
        let v = 1.0 - ((pos.y - field.top()) / field.height()).clamp(0.0, 1.0);
        out = Some(Hsva::new(hsv.h, s, v, 1.0).to_rgb());
    }
    ui.add_space(4.0);
    // Hue strip.
    let (strip, sr) = ui.allocate_exact_size(egui::vec2(w, 18.0), egui::Sense::click_and_drag());
    let mut mesh = egui::Mesh::default();
    const H: usize = 24;
    for i in 0..=H {
        let h = i as f32 / H as f32;
        let c = to_c32(Hsva::new(h, 1.0, 1.0, 1.0).to_rgb());
        let x = strip.left() + h * strip.width();
        mesh.colored_vertex(egui::pos2(x, strip.top()), c);
        mesh.colored_vertex(egui::pos2(x, strip.bottom()), c);
    }
    for i in 0..H as u32 {
        let a = i * 2;
        mesh.add_triangle(a, a + 1, a + 2);
        mesh.add_triangle(a + 1, a + 3, a + 2);
    }
    let p = ui.painter();
    p.add(mesh);
    let x = strip.left() + hsv.h * strip.width();
    p.rect_stroke(egui::Rect::from_center_size(egui::pos2(x, strip.center().y), egui::vec2(6.0, 22.0)), 2.0, egui::Stroke::new(2.0, Color32::WHITE), egui::StrokeKind::Middle);
    if let Some(pos) = sr.interact_pointer_pos()
        && (sr.dragged() || sr.clicked())
    {
        let h = ((pos.x - strip.left()) / strip.width()).clamp(0.0, 0.999);
        ui.data_mut(|d| d.insert_temp(id, h));
        // A hue pick on a grey/black colour makes it fully saturated and bright enough to see.
        let sat = if hsv.s < 0.01 { 1.0 } else { hsv.s };
        let val = if hsv.v < 0.01 { 1.0 } else { hsv.v };
        out = Some(Hsva::new(h, sat, val, 1.0).to_rgb());
    }
    if let Some(c) = out {
        let h = Hsva::from_rgb(c);
        if h.s > 0.01 && h.v > 0.01 {
            ui.data_mut(|d| d.insert_temp(id, h.h));
        }
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

/// A horizontal level (0..=1) set by clicking or dragging, with the percentage on it.
fn level_bar(ui: &mut Ui, value: &mut f32, color: Color32) -> egui::Response {
    let (rect, mut r) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::click_and_drag());
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

    #[test]
    fn tap_tempo() {
        let t0 = std::time::Instant::now();
        let mut taps = Vec::new();
        assert_eq!(tap(&mut taps, t0), None);
        for i in 1..4 {
            let bpm = tap(&mut taps, t0 + std::time::Duration::from_millis(500 * i)).unwrap();
            assert!((bpm - 120.0).abs() < 0.01);
        }
        assert_eq!(tap(&mut taps, t0 + std::time::Duration::from_secs(10)), None, "a pause starts again");
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
        let ty = library::generic("generic/rgb").unwrap();
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
