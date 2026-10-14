//! The Lighting workspace: lighting design (patch, programmer, sequences, outputs) in Edit mode,
//! playback only in Show mode.
//!
//! Every design edit goes through [`App::can_design`], so Show mode (and replicas without the
//! Edit role) can't change the design. The UI never compiles or sends DMX itself: edits mark the
//! show edited and the light thread picks up the recompiled design.

use super::{App, Mode, icons, theme};
use crate::i18n::t;
use crate::light::model::{Attribute, Feature, FixtureId, FixtureType, Group, LightCue, LightCueId, Lighting, Look, PatchIssue, Sequence, SequenceId};
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
    /// Cue stacks.
    Sequences,
}

impl LightTab {
    /// A page by name (`OPENQ_SCREENSHOT_LIGHTING`).
    pub(crate) fn from_name(name: &str) -> Self {
        match name {
            "programmer" => LightTab::Programmer,
            "sequences" => LightTab::Sequences,
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
    /// The programmer as last sent to the light thread.
    sent: Option<Look>,
    /// Encoder page.
    feature: Option<Feature>,
    /// Sequence shown on the Sequences page / recorded into.
    sequence: Option<SequenceId>,
    /// Selected cue on the Sequences page.
    cue: Option<LightCueId>,
    /// Number for the next recorded cue (empty = after the last).
    record_number: String,
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
        if !self.can_design() {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icons::LOCK).font(theme::icon_font(16.0)).color(theme::TEXT_DIM));
                ui.label(RichText::new(t("The lighting design is locked in Show mode.")).color(theme::TEXT_DIM));
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            theme::tab(ui, &mut self.light.tab, LightTab::Patch, icons::CABLE, t("Patch"));
            theme::tab(ui, &mut self.light.tab, LightTab::Programmer, icons::TUNE, t("Programmer"));
            theme::tab(ui, &mut self.light.tab, LightTab::Sequences, icons::PLAY_CIRCLE, t("Sequences"));
        });
        ui.separator();
        let before = show.ws.lighting.clone();
        match self.light.tab {
            LightTab::Patch => self.patch_page(ui, &mut show.ws.lighting),
            LightTab::Programmer => self.programmer_page(ui, &mut show.ws.lighting),
            LightTab::Sequences => self.sequences_page(ui, show),
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
        if !designing && !l.programmer.is_empty() {
            l.programmer.clear();
        }
        if l.sent.as_ref() != Some(&l.programmer) {
            let flat = l.programmer.iter().flat_map(|(f, v)| v.iter().map(move |(a, x)| (*f, *a, *x))).collect();
            show.media.light.set_programmer(flat);
            l.sent = Some(l.programmer.clone());
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
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let n = self.light.selection.len();
            ui.label(RichText::new(tr!("{n} selected", n = n.to_string())).color(theme::TEXT_DIM));
            if ui.button(theme::with_icon(icons::SELECT_ALL, t("Select all"))).clicked() {
                self.light.selection = l.fixtures.iter().map(|f| f.id).collect();
            }
            if ui.button(theme::with_icon(icons::DESELECT, t("Select none"))).clicked() {
                self.light.selection.clear();
            }
            ui.separator();
            let busy = !self.light.programmer.is_empty();
            ui.add_enabled_ui(busy, |ui| {
                if ui.button(theme::with_icon(icons::CLOSE, t("Clear programmer"))).clicked() {
                    self.light.programmer.clear();
                }
            });
            if busy {
                let n: usize = self.light.programmer.values().map(|v| v.len()).sum();
                ui.label(RichText::new(tr!("{n} values in the programmer", n = n.to_string())).color(theme::RED));
            }
            ui.separator();
            self.record_controls(ui, l);
        });
        ui.separator();
        // Fixture tiles on the left, encoders on the right.
        let enc_w = 430.0;
        ui.horizontal_top(|ui| {
            let tiles_w = (ui.available_width() - enc_w - 12.0).max(200.0);
            ui.allocate_ui(egui::vec2(tiles_w, ui.available_height()), |ui| self.fixture_tiles(ui, l));
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(enc_w);
                self.encoders(ui, l);
            });
        });
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
        let can = !self.light.programmer.is_empty();
        if ui.add_enabled(can, egui::Button::new(theme::with_icon(icons::FIBER_MANUAL_RECORD, t("Record cue")))).clicked() {
            let number = Some(self.light.record_number.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or(hint);
            let seq = match seq_id {
                Some(id) => id,
                None => {
                    l.sequences.push(Sequence { id: 1, name: t("Main").into(), cues: Vec::new() });
                    1
                }
            };
            let s = l.sequences.iter_mut().find(|s| s.id == seq).expect("sequence");
            let values = self.light.programmer.clone();
            if let Some(c) = s.cues.iter_mut().find(|c| c.number == number) {
                c.values = values; // re-record over an existing number
                self.light.cue = Some(c.id);
            } else {
                let c = LightCue { number, values, ..Default::default() };
                self.light.cue = Some(c.id);
                let at = s.cues.iter().position(|x| number_key(&x.number) > number_key(&c.number)).unwrap_or(s.cues.len());
                s.cues.insert(at, c);
            }
            self.light.sequence = Some(seq);
            self.light.record_number.clear();
        }
        let target = self.light.cue.and_then(|c| l.cue_label(c));
        if ui
            .add_enabled(can && target.is_some(), egui::Button::new(theme::with_icon(icons::SAVE, t("Update cue"))))
            .on_hover_text(target.map_or_else(|| t("Select a cue on the Sequences page").to_string(), |c| tr!("Merge the programmer into cue {cue}", cue = c)))
            .clicked()
            && let Some(c) = self.light.cue.and_then(|id| l.sequences.iter_mut().flat_map(|s| s.cues.iter_mut()).find(|c| c.id == id))
        {
            for (f, vals) in &self.light.programmer {
                c.values.entry(*f).or_default().extend(vals.iter().map(|(a, v)| (*a, *v)));
            }
        }
    }

    /// Fixture tiles: click to select (Ctrl/Shift adds), showing the programmer intensity.
    fn fixture_tiles(&mut self, ui: &mut Ui, l: &Lighting) {
        let tile = egui::vec2(84.0, 52.0);
        egui::ScrollArea::vertical().id_salt("tiles").auto_shrink(false).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                for f in &l.fixtures {
                    let (rect, r) = ui.allocate_exact_size(tile, egui::Sense::click());
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    let order = self.light.selection.iter().position(|x| *x == f.id);
                    let prog = self.light.programmer.get(&f.id);
                    let p = ui.painter();
                    let fill = if order.is_some() { theme::accent_bg() } else if r.hovered() { theme::BG_HOVER } else { theme::BG_RAISED };
                    p.rect_filled(rect, 6.0, fill);
                    if prog.is_some() {
                        p.rect_stroke(rect, 6.0, egui::Stroke::new(1.5, theme::RED), egui::StrokeKind::Inside);
                    }
                    p.text(rect.left_top() + egui::vec2(6.0, 4.0), egui::Align2::LEFT_TOP, f.id.to_string(), theme::semibold(13.0), theme::TEXT);
                    let name = if f.name.is_empty() { l.fixture_type(&f.fixture_type).map_or("", |t| t.name.as_str()) } else { f.name.as_str() };
                    p.text(rect.left_bottom() + egui::vec2(6.0, -4.0), egui::Align2::LEFT_BOTTOM, name, egui::FontId::proportional(10.5), theme::TEXT_DIM);
                    // Programmer intensity and colour.
                    if let Some(v) = prog {
                        if let Some(d) = v.get(&Attribute::Dimmer) {
                            p.text(rect.right_top() + egui::vec2(-6.0, 4.0), egui::Align2::RIGHT_TOP, format!("{:.0}", d * 100.0), theme::mono(11.0), theme::RED);
                        }
                        let c = |a| v.get(&a).copied();
                        if let (Some(r), Some(g), Some(b)) = (c(Attribute::Red), c(Attribute::Green), c(Attribute::Blue)) {
                            let sw = egui::Rect::from_min_size(rect.right_bottom() - egui::vec2(18.0, 16.0), egui::vec2(12.0, 10.0));
                            p.rect_filled(sw, 2.0, Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8));
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
    fn selection_value(&self, attr: Attribute) -> Option<f32> {
        self.light.selection.iter().find_map(|f| self.light.programmer.get(f).and_then(|v| v.get(&attr)).copied())
    }

    /// Set an attribute on every selected fixture that has it.
    fn set_selection(&mut self, l: &Lighting, attr: Attribute, value: f32) {
        for f in &self.light.selection {
            if attributes_of(l, *f).contains(&attr) {
                self.light.programmer.entry(*f).or_default().insert(attr, value.clamp(0.0, 1.0));
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
                    for (label, v) in [(t("Full"), 1.0), ("50 %", 0.5), (t("Zero"), 0.0)] {
                        if ui.button(label).clicked() {
                            self.set_selection(l, Attribute::Dimmer, v);
                        }
                    }
                });
            }
            Feature::Colour => self.colour_picker(ui, l, &page),
            Feature::Position if page.contains(&Attribute::Pan) && page.contains(&Attribute::Tilt) => self.pan_tilt_pad(ui, l),
            _ => {}
        }
        egui::Grid::new("encoders").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
            for a in page {
                ui.label(a.label());
                let current = self.selection_value(a);
                let mut v = current.unwrap_or(0.0) * 100.0;
                let r = ui.add(egui::Slider::new(&mut v, 0.0..=100.0).suffix(" %").max_decimals(1));
                if r.changed() {
                    self.set_selection(l, a, v / 100.0);
                }
                if current.is_some() {
                    if ui.small_button(icons::CLOSE).on_hover_text(t("Remove from the programmer")).clicked() {
                        for f in &self.light.selection {
                            if let Some(vals) = self.light.programmer.get_mut(f) {
                                vals.remove(&a);
                            }
                        }
                        self.light.programmer.retain(|_, v| !v.is_empty());
                    }
                } else {
                    ui.label("");
                }
                ui.end_row();
            }
        });
    }

    /// RGB/CMY picker for the selection.
    fn colour_picker(&mut self, ui: &mut Ui, l: &Lighting, page: &[Attribute]) {
        let rgb = page.contains(&Attribute::Red) && page.contains(&Attribute::Green) && page.contains(&Attribute::Blue);
        let cmy = page.contains(&Attribute::Cyan) && page.contains(&Attribute::Magenta) && page.contains(&Attribute::Yellow);
        if !rgb && !cmy {
            return;
        }
        let get = |a| self.selection_value(a).unwrap_or(0.0);
        let mut c = if rgb {
            [get(Attribute::Red), get(Attribute::Green), get(Attribute::Blue)]
        } else {
            [1.0 - get(Attribute::Cyan), 1.0 - get(Attribute::Magenta), 1.0 - get(Attribute::Yellow)]
        };
        let mut hsva = egui::ecolor::Hsva::from_rgb(c);
        if egui::color_picker::color_picker_hsva_2d(ui, &mut hsva, egui::color_picker::Alpha::Opaque) {
            c = hsva.to_rgb();
            if rgb {
                self.set_selection(l, Attribute::Red, c[0]);
                self.set_selection(l, Attribute::Green, c[1]);
                self.set_selection(l, Attribute::Blue, c[2]);
            }
            if cmy {
                self.set_selection(l, Attribute::Cyan, 1.0 - c[0]);
                self.set_selection(l, Attribute::Magenta, 1.0 - c[1]);
                self.set_selection(l, Attribute::Yellow, 1.0 - c[2]);
            }
        }
        ui.add_space(4.0);
    }

    /// Pan (x) / tilt (y) pad for the selection.
    fn pan_tilt_pad(&mut self, ui: &mut Ui, l: &Lighting) {
        let size = egui::vec2(180.0, 180.0);
        let (rect, r) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
        let p = ui.painter();
        p.rect_filled(rect, 6.0, theme::BG_INPUT);
        p.rect_stroke(rect, 6.0, egui::Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
        p.line_segment([rect.center_top(), rect.center_bottom()], egui::Stroke::new(1.0, theme::BORDER));
        p.line_segment([rect.left_center(), rect.right_center()], egui::Stroke::new(1.0, theme::BORDER));
        let pan = self.selection_value(Attribute::Pan).unwrap_or(0.5);
        let tilt = self.selection_value(Attribute::Tilt).unwrap_or(0.5);
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
        ui.horizontal_top(|ui| {
            // Sequence list.
            ui.vertical(|ui| {
                ui.set_width(220.0);
                ui.horizontal(|ui| {
                    ui.label(theme::caption(t("Sequences")));
                    if ui.small_button(icons::ADD).on_hover_text(t("New sequence")).clicked() {
                        let id = l.sequences.iter().map(|s| s.id).max().unwrap_or(0) + 1;
                        l.sequences.push(Sequence { id, name: String::new(), cues: Vec::new() });
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
                        ui.add_sized([w, 18.0], egui::Label::new(theme::caption(h)));
                    }
                    ui.label(theme::caption(t("Fixtures")));
                });
                egui::ScrollArea::vertical().id_salt("light_cues").auto_shrink(false).show_rows(ui, 26.0, s.cues.len(), |ui, range| {
                    for c in &mut s.cues[range] {
                        let selected = self.light.cue == Some(c.id);
                        ui.horizontal(|ui| {
                            let live = current == Some(c.id);
                            let mark = if live { RichText::new(icons::PLAY_ARROW).color(theme::GREEN) } else { RichText::new("") };
                            ui.add_sized([W[0], 20.0], egui::Label::new(mark.font(theme::icon_font(14.0))));
                            ui.add_sized([W[1], 20.0], egui::TextEdit::singleline(&mut c.number));
                            if ui.add_sized([W[2], 20.0], egui::Button::selectable(selected, if c.name.is_empty() { "—" } else { c.name.as_str() })).clicked() {
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
                if let Some(c) = self.light.cue.and_then(|id| s.cues.iter_mut().find(|c| c.id == id)) {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(t("Name"));
                        ui.add(egui::TextEdit::singleline(&mut c.name).desired_width(260.0));
                    });
                }
            });
        });
    }
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
