//! The Lighting workspace: lighting design (patch, programmer, sequences, outputs) in Edit mode,
//! playback only in Show mode.
//!
//! Every design edit goes through [`App::can_design`], so Show mode (and replicas without the
//! Edit role) can't change the design. The UI never compiles or sends DMX itself: edits mark the
//! show edited and the light thread picks up the recompiled design.

use super::{App, Mode, icons, theme};
use crate::i18n::t;
use crate::light::model::{FixtureId, FixtureType, Lighting, PatchIssue};
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
}

impl LightTab {
    /// A page by name (`OPENQ_SCREENSHOT_LIGHTING`).
    pub(crate) fn from_name(_name: &str) -> Self {
        LightTab::Patch
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
        });
        ui.separator();
        let before = show.ws.lighting.clone();
        match self.light.tab {
            LightTab::Patch => self.patch_page(ui, &mut show.ws.lighting),
        }
        if show.ws.lighting != before {
            show.edited();
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
