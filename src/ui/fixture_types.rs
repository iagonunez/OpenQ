//! Fixture types on the Patch page: the types of the show, the user's library and the built-in
//! ones, import (OFL / GDTF / QLC+ / `.oqfixture`), export, and the fixture type editor
//! (modes, cells, channels, named ranges and wheel slots).
//!
//! The editor works on a copy ([`TypeEdit`]); nothing changes until "Save to library" or
//! "Use in this show". Built-in types are never changed in place: saving one makes a user type
//! with its own id. Library reads and writes run on the library's thread
//! ([`crate::light::fixlib::Library`]); their results come back as notifications.

use super::{App, icons, theme};
use crate::i18n::t;
use crate::light::fixlib::{Library, Snapshot};
use crate::light::import::type_id;
use crate::light::model::{Attribute, ChannelDef, ChannelRange, Feature, FixtureMode, FixtureType, Head, Lighting, MAX_CELLS};
use egui::{Color32, DragValue, RichText, Ui};
use parking_lot::Mutex;
use std::sync::Arc;

/// Where the type being edited came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Origin {
    /// The show's own copy.
    Show,
    /// The user's library.
    Library,
    /// Built into OpenQ (saving makes a new type).
    BuiltIn,
    /// Made from scratch.
    New,
}

/// The type open in the editor: a working copy and what's selected in it.
pub(crate) struct TypeEdit {
    pub(crate) ty: FixtureType,
    origin: Origin,
    mode: usize,
    /// None: master channels; Some(i): cell i (0-based).
    unit: Option<usize>,
    channel: Option<usize>,
    /// Cell generator: number of cells to make from cell 1.
    make_cells: u32,
    /// Range generator: number of equal slots.
    split: u32,
}

impl TypeEdit {
    pub(crate) fn new(ty: FixtureType, origin: Origin) -> Self {
        Self { ty, origin, mode: 0, unit: None, channel: None, make_cells: 8, split: 8 }
    }

    /// A show type opened on its first channel with ranges (screenshots).
    pub(crate) fn open_ranges(ty: FixtureType) -> Self {
        let mut e = Self::new(ty, Origin::Show);
        e.channel = e.ty.modes.first().and_then(|m| m.channels.iter().position(|c| !c.ranges.is_empty()));
        e
    }
}

/// Fixture types view state.
#[derive(Default)]
pub(crate) struct TypeUi {
    /// The user's library (opened the first time the Patch page is shown).
    lib: Option<Library>,
    /// Library contents as of `snap.generation`.
    snap: Arc<Snapshot>,
    pub(crate) edit: Option<TypeEdit>,
    filter: String,
    /// Results of background saves, shown as notifications.
    results: Arc<Mutex<Vec<Result<String, String>>>>,
    /// Fixture files parsed in the background, waiting to go into the show.
    imports: Arc<Mutex<Vec<ImportDone>>>,
    /// Open Fixture Library catalogue and downloads.
    ofl: crate::light::oflsite::Browser,
    /// The "Open Fixture Library" search window is open.
    pub(crate) ofl_open: bool,
    ofl_query: String,
    /// Put the cursor in the search field (the window was just opened).
    pub(crate) ofl_focus: bool,
}

/// One batch of imported fixture files.
struct ImportDone {
    imported: Vec<FixtureType>,
    failed: Vec<String>,
    notes: usize,
}

impl TypeUi {
    /// The library, opened on first use.
    pub(crate) fn library(&mut self) -> &Library {
        self.lib.get_or_insert_with(Library::default)
    }

    /// Refresh the snapshot when the library changed (cheap otherwise).
    pub(crate) fn refresh(&mut self) -> Arc<Snapshot> {
        let lib = self.library().clone();
        let s = lib.snapshot_if_newer(self.snap.generation);
        if let Some(s) = s {
            self.snap = s;
        }
        self.snap.clone()
    }

    /// Save into the library in the background; the result becomes a notification.
    pub(crate) fn save_to_library(&mut self, types: Vec<FixtureType>) {
        let results = self.results.clone();
        let n = types.len();
        let name = types.first().map(|t| format!("{} {}", t.manufacturer, t.name).trim().to_string()).unwrap_or_default();
        self.library().save(types, move |r| {
            results.lock().push(match r {
                Ok(()) if n == 1 => Ok(tr!("{name} saved to the fixture library", name = name)),
                Ok(()) => Ok(tr!("{n} fixture types saved to the library", n = n.to_string())),
                Err(e) => Err(tr!("Can't save to the fixture library: {e}", e = format!("{e:#}"))),
            });
        });
    }

    /// Open the Open Fixture Library window searching `query` (screenshots).
    pub(crate) fn open_ofl(&mut self, query: String) {
        self.ofl_open = true;
        self.ofl_focus = true;
        self.ofl_query = query;
    }

    /// Results of finished background jobs.
    pub(crate) fn take_results(&mut self) -> Vec<Result<String, String>> {
        std::mem::take(&mut *self.results.lock())
    }
}

/// Fixture types offered when patching: the show's own first, then the library's, then the
/// built-in ones not already listed (each id once). Also returns how many came from the show and
/// from the library.
pub(crate) fn available_types<'a>(l: &'a Lighting, library: &'a [FixtureType], generics: &'a [FixtureType]) -> (Vec<&'a FixtureType>, usize, usize) {
    let mut seen = std::collections::HashSet::new();
    let mut v: Vec<&FixtureType> = l.fixture_types.iter().filter(|t| seen.insert(t.id.as_str())).collect();
    let in_show = v.len();
    v.extend(library.iter().filter(|t| seen.insert(t.id.as_str())));
    let in_lib = v.len() - in_show;
    v.extend(generics.iter().filter(|t| seen.insert(t.id.as_str())));
    (v, in_show, in_lib)
}

/// "Manufacturer Name".
pub(crate) fn type_label(t: &FixtureType) -> String {
    format!("{} {}", t.manufacturer, t.name).trim().to_string()
}

/// Problems of a type shown in the editor (it can still be saved).
pub(crate) fn type_issues(t: &FixtureType) -> Vec<String> {
    let mut out = Vec::new();
    for m in &t.modes {
        let mut used: std::collections::HashMap<u16, String> = std::collections::HashMap::new();
        // (The unit name is kept for a future "used by" note.)
        let units = std::iter::once((t_master(), &m.channels)).chain(m.heads.iter().map(|h| (tr!("cell {name}", name = h.name.clone()), &h.channels)));
        for (unit, chans) in units {
            let mut attrs = std::collections::HashSet::new();
            for c in chans {
                if !attrs.insert(c.attribute) {
                    out.push(tr!("{mode}: {attr} twice in {unit}", mode = m.name.clone(), attr = c.attribute.label(), unit = unit.clone()));
                }
                for slot in std::iter::once(c.offset).chain(c.fine) {
                    if used.insert(slot, unit.clone()).is_some() {
                        out.push(tr!("{mode}: DMX slot {n} used twice", mode = m.name.clone(), n = (slot + 1).to_string()));
                    }
                }
                let mut r: Vec<&ChannelRange> = c.ranges.iter().collect();
                r.sort_by_key(|r| r.from.min(r.to));
                if r.windows(2).any(|w| w[0].from.max(w[0].to) >= w[1].from.min(w[1].to)) {
                    out.push(tr!("{mode}: ranges of {attr} overlap", mode = m.name.clone(), attr = c.attribute.label()));
                }
            }
        }
        if m.channels.is_empty() && m.heads.is_empty() {
            out.push(tr!("{mode}: no channels", mode = m.name.clone()));
        }
    }
    let mut names: Vec<&str> = t.modes.iter().map(|m| m.name.as_str()).collect();
    names.sort();
    if names.windows(2).any(|w| w[0] == w[1]) {
        out.push(crate::i18n::t("Two modes have the same name").to_string());
    }
    out.dedup();
    out
}

fn t_master() -> String {
    t("master").to_string()
}

/// DMX slots spanned by a channel list (first, last), 0-based.
fn span(chans: &[ChannelDef]) -> Option<(u16, u16)> {
    let lo = chans.iter().map(|c| c.offset.min(c.fine.unwrap_or(u16::MAX))).min()?;
    let hi = chans.iter().map(|c| c.offset.max(c.fine.unwrap_or(0))).max()?;
    Some((lo, hi))
}

/// Make `n` cells repeating cell 1's channels, each shifted by cell 1's width.
pub(crate) fn repeat_cells(m: &mut FixtureMode, n: u32) {
    let Some(first) = m.heads.first().cloned() else { return };
    let Some((lo, hi)) = span(&first.channels) else { return };
    let width = hi - lo + 1;
    let n = n.clamp(1, MAX_CELLS);
    m.heads = (0..n)
        .map_while(|i| {
            let shift = (i * width as u32) as u16;
            if hi as u32 + shift as u32 >= 512 {
                return None;
            }
            let channels = first.channels.iter().map(|c| ChannelDef { offset: c.offset + shift, fine: c.fine.map(|f| f + shift), ..c.clone() }).collect();
            Some(Head { name: (i + 1).to_string(), channels })
        })
        .collect();
}

/// `n` equal ranges over 0..=255 named "Slot 1"…
pub(crate) fn split_ranges(n: u32) -> Vec<ChannelRange> {
    let n = n.clamp(1, 256);
    (0..n)
        .map(|i| {
            let from = (i * 256 / n) as u8;
            let to = (((i + 1) * 256 / n) - 1) as u8;
            ChannelRange { from, to, name: tr!("Slot {n}", n = (i + 1).to_string()), ..Default::default() }
        })
        .collect()
}

/// A new id for a type saved from a built-in or new one: `user/<manufacturer>/<name>`, made
/// unique against `taken`.
pub(crate) fn new_id(t: &FixtureType, taken: &dyn Fn(&str) -> bool) -> String {
    let base = type_id("user", &t.manufacturer, &t.name);
    let mut id = base.clone();
    let mut k = 2;
    while taken(&id) {
        id = format!("{base}-{k}");
        k += 1;
    }
    id
}

impl App {
    /// The Types view of the Patch page: lists on the left, the editor on the right.
    pub(super) fn types_view(&mut self, ui: &mut Ui, l: &mut Lighting) {
        let snap = self.light.types.refresh();
        let mut open: Option<TypeEdit> = None;
        let mut export: Option<FixtureType> = None;
        let mut to_library: Vec<FixtureType> = Vec::new();
        let mut remove: Option<FixtureType> = None;
        let mut update_from_library: Option<FixtureType> = None;
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(300.0);
                ui.horizontal(|ui| {
                    if ui.button(theme::with_icon(icons::ADD, t("New type"))).clicked() {
                        let ty = FixtureType {
                            id: String::new(),
                            manufacturer: String::new(),
                            name: t("New fixture").into(),
                            modes: vec![FixtureMode::sequential("1 channel", &[(Attribute::Dimmer, false)])],
                            physical: Default::default(),
                            images: Default::default(),
                        };
                        open = Some(TypeEdit::new(ty, Origin::New));
                    }
                    if cfg!(feature = "fixture-download")
                        && ui.button(theme::with_icon(icons::DOWNLOAD, t("Open Fixture Library…"))).on_hover_text(t("Search and download fixtures from open-fixture-library.org")).clicked() {
                        self.light.types.ofl_open = true;
                        self.light.types.ofl_focus = true;
                    }
                    if let Some(dir) = self.light.types.library().dir().map(|d| d.to_path_buf())
                        && ui.button(icons::FOLDER).on_hover_text(t("Open the fixture library folder")).clicked()
                    {
                        // In the file manager (open_path would try to load it as a show).
                        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| super::manual::open_with_default_app(&dir)) {
                            self.notify_error(tr!("Can't open {path}: {e}", path = dir.display().to_string(), e = e.to_string()));
                        }
                    }
                });
                ui.add(egui::TextEdit::singleline(&mut self.light.types.filter).hint_text(t("Search")).desired_width(290.0));
                let filter = self.light.types.filter.to_lowercase();
                let shown = |t: &FixtureType| filter.is_empty() || type_label(t).to_lowercase().contains(&filter);
                let editing = self.light.types.edit.as_ref().map(|e| (e.ty.id.clone(), e.origin));
                egui::ScrollArea::vertical().id_salt("type_lists").auto_shrink(false).show(ui, |ui| {
                    ui.label(theme::caption(t("In this show")));
                    if l.fixture_types.is_empty() {
                        ui.label(RichText::new(t("No fixture types yet")).color(theme::TEXT_FAINT).small());
                    }
                    for ty in l.fixture_types.iter().filter(|t| shown(t)) {
                        let n = l.fixtures.iter().filter(|f| f.fixture_type == ty.id).count();
                        let on = editing.as_ref() == Some(&(ty.id.clone(), Origin::Show));
                        let in_lib = snap.items.iter().find(|x| x.id == ty.id);
                        // The show's copy and the library's have drifted apart.
                        let diff = in_lib.filter(|x| *x != ty).map(|x| crate::light::fixlib::differences(ty, x));
                        let r = match &diff {
                            Some(d) => {
                                let text = RichText::new(theme::with_icon(icons::WARNING, &format!("{}  ·  {n}", type_label(ty)))).color(theme::AMBER);
                                let why = format!("{}\n{}\n· {}", ty.id, t("Differs from the library (right-click → Update from library):"), d.join("\n· "));
                                ui.selectable_label(on, text).on_hover_text(why)
                            }
                            None => ui.selectable_label(on, format!("{}  ·  {n}", type_label(ty))).on_hover_text(&ty.id),
                        };
                        if r.clicked() {
                            open = Some(TypeEdit::new(ty.clone(), Origin::Show));
                        }
                        r.context_menu(|ui| {
                            if ui.button(theme::with_icon(icons::SAVE, t("Save to library"))).clicked() {
                                to_library.push(ty.clone());
                                ui.close();
                            }
                            if let Some(lib) = in_lib.filter(|x| *x != ty)
                                && ui.button(theme::with_icon(icons::REFRESH, t("Update from library"))).clicked()
                            {
                                update_from_library = Some(lib.clone());
                                ui.close();
                            }
                            if ui.button(theme::with_icon(icons::DOWNLOAD, t("Export…"))).clicked() {
                                export = Some(ty.clone());
                                ui.close();
                            }
                        });
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(theme::caption(t("Library")));
                        if snap.loading {
                            ui.spinner();
                        }
                    });
                    if snap.items.is_empty() && !snap.loading {
                        ui.label(RichText::new(t("Imported and saved types appear here, in every show")).color(theme::TEXT_FAINT).small());
                    }
                    for ty in snap.items.iter().filter(|t| shown(t)) {
                        let on = editing.as_ref() == Some(&(ty.id.clone(), Origin::Library));
                        let r = ui.selectable_label(on, type_label(ty)).on_hover_text(&ty.id);
                        if r.clicked() {
                            open = Some(TypeEdit::new(ty.clone(), Origin::Library));
                        }
                        r.context_menu(|ui| {
                            if ui.button(theme::with_icon(icons::DOWNLOAD, t("Export…"))).clicked() {
                                export = Some(ty.clone());
                                ui.close();
                            }
                            if ui.button(theme::with_icon(icons::DELETE, t("Remove from library"))).clicked() {
                                remove = Some(ty.clone());
                                ui.close();
                            }
                        });
                    }
                    for (file, e) in &snap.errors {
                        ui.label(RichText::new(format!("{} {file}: {e}", icons::WARNING)).color(theme::AMBER).small());
                    }
                    ui.add_space(6.0);
                    ui.label(theme::caption(t("Built-in")));
                    for ty in crate::light::library::generics().into_iter().filter(|t| shown(t)) {
                        let on = editing.as_ref() == Some(&(ty.id.clone(), Origin::BuiltIn));
                        if ui.selectable_label(on, type_label(&ty)).on_hover_text(t("Built in: saving makes a copy of your own")).clicked() {
                            open = Some(TypeEdit::new(ty, Origin::BuiltIn));
                        }
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical().id_salt("type_editor").auto_shrink(false).show(ui, |ui| {
                ui.vertical(|ui| {
                    if self.light.types.edit.is_some() {
                        self.type_editor(ui, l, &snap);
                    } else {
                        ui.label(RichText::new(t("Choose a fixture type to see or edit it, or import one.")).color(theme::TEXT_DIM));
                    }
                });
            });
        });
        if self.light.types.ofl_open {
            self.ofl_window(ui.ctx(), l, &snap);
        }
        if let Some(e) = open {
            self.light.types.edit = Some(e);
        }
        if let Some(ty) = export {
            self.deferred.push(super::Deferred::ExportFixture(Box::new(ty)));
        }
        if !to_library.is_empty() {
            self.light.types.save_to_library(to_library);
        }
        if let Some(ty) = update_from_library {
            replace_in_show(l, ty);
        }
        if let Some(ty) = remove {
            let results = self.light.types.results.clone();
            let name = type_label(&ty);
            self.light.types.library().remove(ty, move |r| {
                results.lock().push(match r {
                    Ok(()) => Ok(tr!("{name} removed from the library", name = name)),
                    Err(e) => Err(tr!("Can't remove it: {e}", e = format!("{e:#}"))),
                })
            });
        }
    }

    /// The editor of the open type.
    fn type_editor(&mut self, ui: &mut Ui, l: &mut Lighting, snap: &Snapshot) {
        let Some(e) = self.light.types.edit.as_mut() else { return };
        let mut close = false;
        let mut save_lib = false;
        let mut use_show = false;
        let mut export = false;
        // Header.
        ui.horizontal_wrapped(|ui| {
            ui.label(t("Manufacturer"));
            ui.add(egui::TextEdit::singleline(&mut e.ty.manufacturer).desired_width(160.0));
            ui.label(t("Name"));
            ui.add(egui::TextEdit::singleline(&mut e.ty.name).desired_width(200.0));
            let origin = match e.origin {
                Origin::Show => t("in this show"),
                Origin::Library => t("in the library"),
                Origin::BuiltIn => t("built in"),
                Origin::New => t("new"),
            };
            let id = if e.ty.id.is_empty() { "—".to_string() } else { e.ty.id.clone() };
            ui.label(RichText::new(format!("{id} · {origin}")).color(theme::TEXT_FAINT).small());
        });
        ui.horizontal_wrapped(|ui| {
            if ui.button(theme::with_icon(icons::SAVE, t("Save to library"))).clicked() {
                save_lib = true;
            }
            let in_show = l.fixture_type(&e.ty.id).is_some() && matches!(e.origin, Origin::Show | Origin::Library);
            let label = if in_show { t("Update in this show") } else { t("Add to this show") };
            if ui.button(theme::with_icon(icons::CHECK_CIRCLE, label)).on_hover_text(t("Patched fixtures keep their mode (by name)")).clicked() {
                use_show = true;
            }
            if ui.button(theme::with_icon(icons::DOWNLOAD, t("Export…"))).clicked() {
                export = true;
            }
            if ui.button(theme::with_icon(icons::CLOSE, t("Close"))).clicked() {
                close = true;
            }
        });
        let issues = type_issues(&e.ty);
        for i in issues.iter().take(4) {
            ui.label(RichText::new(format!("{} {i}", icons::WARNING)).color(theme::AMBER).small());
        }
        if issues.len() > 4 {
            ui.label(RichText::new(tr!("and {n} more", n = (issues.len() - 4).to_string())).color(theme::AMBER).small());
        }
        ui.add_space(4.0);
        // Physical.
        ui.horizontal_wrapped(|ui| {
            let p = &mut e.ty.physical;
            ui.label(theme::caption(t("Physical")));
            ui.label(t("Pan range"));
            ui.add(DragValue::new(&mut p.pan_range).range(0.0..=3600.0).suffix("°"));
            ui.label(t("Tilt range"));
            ui.add(DragValue::new(&mut p.tilt_range).range(0.0..=3600.0).suffix("°"));
            ui.label(t("Beam"));
            ui.add(DragValue::new(&mut p.beam_min).range(0.0..=180.0).suffix("°"));
            ui.label(t("to"));
            ui.add(DragValue::new(&mut p.beam_max).range(0.0..=180.0).suffix("°"));
            ui.checkbox(&mut p.moving, t("Moving head"));
        });
        ui.separator();
        // Modes.
        e.mode = e.mode.min(e.ty.modes.len().saturating_sub(1));
        let mut mode_action = None;
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::caption(t("Mode")));
            let names: Vec<String> = e.ty.modes.iter().map(|m| m.name.clone()).collect();
            egui::ComboBox::from_id_salt("type_mode").selected_text(names.get(e.mode).cloned().unwrap_or_default()).width(160.0).show_ui(ui, |ui| {
                for (i, n) in names.iter().enumerate() {
                    if ui.selectable_label(e.mode == i, n).clicked() && e.mode != i {
                        e.mode = i;
                        e.unit = None;
                        e.channel = None;
                    }
                }
            });
            if let Some(m) = e.ty.modes.get_mut(e.mode) {
                ui.label(t("Name"));
                ui.add(egui::TextEdit::singleline(&mut m.name).desired_width(130.0));
                ui.label(t("Slots"));
                let fp = m.footprint();
                ui.add(DragValue::new(&mut m.slots).range(0..=512).custom_formatter(move |v, _| if v == 0.0 { format!("{fp}") } else { format!("{v}") }))
                    .on_hover_text(t("DMX slots the mode takes, unused ones included (0: up to the last channel)"));
            }
            if ui.small_button(icons::ADD).on_hover_text(t("New mode")).clicked() {
                mode_action = Some(0);
            }
            if ui.small_button(icons::CONTENT_COPY).on_hover_text(t("Duplicate mode")).clicked() {
                mode_action = Some(1);
            }
            if e.ty.modes.len() > 1 && ui.small_button(icons::DELETE).on_hover_text(t("Delete mode")).clicked() {
                mode_action = Some(2);
            }
        });
        match mode_action {
            Some(0) => {
                e.ty.modes.push(FixtureMode { name: tr!("Mode {n}", n = (e.ty.modes.len() + 1).to_string()), ..Default::default() });
                e.mode = e.ty.modes.len() - 1;
            }
            Some(1) => {
                let mut m = e.ty.modes[e.mode].clone();
                m.name = tr!("{name} copy", name = m.name);
                e.ty.modes.push(m);
                e.mode = e.ty.modes.len() - 1;
            }
            Some(2) => {
                e.ty.modes.remove(e.mode);
                e.mode = 0;
            }
            _ => {}
        }
        if e.ty.modes.is_empty() {
            e.ty.modes.push(FixtureMode { name: tr!("Mode {n}", n = "1".to_string()), ..Default::default() });
        }
        unit_bar(ui, e);
        ui.add_space(4.0);
        channel_table(ui, e);
        ui.add_space(6.0);
        range_editor(ui, e);
        // Actions (after the edits of this frame).
        if save_lib || use_show {
            let taken = |id: &str| l.fixture_type(id).is_some() || snap.items.iter().any(|t| t.id == id);
            if matches!(e.origin, Origin::BuiltIn | Origin::New) {
                e.ty.id = new_id(&e.ty, &taken);
                e.origin = if use_show { Origin::Show } else { Origin::Library };
            }
            match crate::light::fixlib::sanitize(e.ty.clone()) {
                Ok(ty) => {
                    if use_show {
                        replace_in_show(l, ty.clone());
                        self.light.set_add_type(&ty.id);
                    }
                    if save_lib {
                        self.light.types.save_to_library(vec![ty]);
                    }
                }
                Err(err) => self.notify_error(tr!("Can't save the fixture type: {e}", e = format!("{err:#}"))),
            }
        }
        if export && let Some(e) = &self.light.types.edit {
            let mut ty = e.ty.clone();
            if ty.id.is_empty() {
                ty.id = new_id(&ty, &|_| false);
            }
            self.deferred.push(super::Deferred::ExportFixture(Box::new(ty)));
        }
        if close {
            self.light.types.edit = None;
        }
    }

    /// Pick fixture files (outside the show lock: the dialog and the reading block) and import
    /// them: each goes into the show and into the user's library.
    pub(super) fn import_fixture(&mut self, ctx: &egui::Context) {
        if !self.can_design() {
            return;
        }
        use crate::light::import::{Format, import_path};
        let Some(paths) = rfd::FileDialog::new()
            .add_filter(t("Fixture files"), &Format::EXTENSIONS)
            .add_filter(t("Open Fixture Library"), &["json"])
            .add_filter("GDTF", &["gdtf"])
            .add_filter("QLC+", &["qxf"])
            .add_filter("OpenQ", &[crate::light::fixlib::EXTENSION])
            .pick_files()
        else {
            return;
        };
        // Parsing a big GDTF can take a while: not in a UI frame. Panics become errors.
        let done = self.light.types.imports.clone();
        let ctx = ctx.clone();
        let parse = move || {
            let mut batch = ImportDone { imported: Vec::new(), failed: Vec::new(), notes: 0 };
            for path in &paths {
                match crate::recovery::catch(|| import_path(path)) {
                    Ok(imp) => {
                        for n in &imp.notes {
                            log::warn!("fixture import ({}): {n}", path.display());
                        }
                        batch.notes += imp.notes.len();
                        batch.imported.push(imp.fixture);
                    }
                    Err(e) => batch.failed.push(format!("{}: {e:#}", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())),
                }
            }
            done.lock().push(batch);
            ctx.request_repaint();
        };
        if let Err(e) = std::thread::Builder::new().name("openq-fixture-import".into()).spawn(parse) {
            self.notify_error(tr!("Can't import the fixture: {e}", e = e.to_string()));
        }
    }

    /// The Open Fixture Library window: search the site's catalogue, download into the show and
    /// the library (through the same path as imported files).
    fn ofl_window(&mut self, ctx: &egui::Context, l: &Lighting, snap: &Snapshot) {
        let browser = self.light.types.ofl.clone();
        let repaint = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        browser.open(repaint.clone());
        let state = browser.state();
        let mut open = true;
        let mut fetch: Vec<crate::light::oflsite::Entry> = Vec::new();
        let types = &mut self.light.types;
        egui::Window::new(t("Open Fixture Library")).id(egui::Id::new("ofl_window")).open(&mut open).default_size([560.0, 480.0]).pivot(egui::Align2::CENTER_CENTER).default_pos(ctx.content_rect().center()).show(ctx, |ui| {
            let search = ui.add(egui::TextEdit::singleline(&mut types.ofl_query).hint_text(t("Manufacturer, name or category")).desired_width(f32::INFINITY));
            if std::mem::take(&mut types.ofl_focus) {
                search.request_focus();
            }
            ui.horizontal(|ui| {
                if let Some((done, total)) = state.fetching {
                    ui.spinner();
                    let text = if total == 0 { t("Fetching the fixture list…").to_string() } else { tr!("Fetching the fixture list… {done}/{total}", done = done.to_string(), total = total.to_string()) };
                    ui.label(RichText::new(text).color(theme::TEXT_DIM));
                } else if let Some(c) = &state.catalogue {
                    let stamp = crate::errlog::timestamp(std::time::UNIX_EPOCH + std::time::Duration::from_secs(c.fetched));
                    let date = stamp.get(..10).unwrap_or_default().to_string();
                    let text = tr!("{n} fixtures from {m} manufacturers · updated {date}", n = c.fixtures.len().to_string(), m = c.makers().to_string(), date = date);
                    ui.label(RichText::new(text).color(theme::TEXT_DIM).small());
                }
                if state.fetching.is_none() && ui.button(icons::REFRESH).on_hover_text(t("Fetch the fixture list again")).clicked() {
                    browser.refresh(repaint.clone());
                }
            });
            if let Some(e) = &state.error {
                ui.label(RichText::new(format!("{} {}", icons::WARNING, tr!("Can't reach the Open Fixture Library: {e}", e = e.clone()))).color(theme::AMBER).small());
            }
            ui.separator();
            let Some(c) = &state.catalogue else { return };
            let found = c.search(&types.ofl_query, usize::MAX);
            if found.is_empty() {
                ui.label(RichText::new(t("No fixtures found")).color(theme::TEXT_FAINT));
                return;
            }
            let row_h = ui.spacing().interact_size.y + 4.0;
            egui::ScrollArea::vertical().id_salt("ofl_results").auto_shrink(false).show_rows(ui, row_h, found.len(), |ui, rows| {
                for e in &found[rows] {
                    let id = e.type_id();
                    ui.horizontal(|ui| {
                        ui.set_height(row_h - 4.0);
                        ui.label(RichText::new(&e.name).strong()).on_hover_text(e.url());
                        ui.label(RichText::new(&e.maker_name).color(theme::TEXT_DIM));
                        ui.label(RichText::new(e.categories.join(", ")).color(theme::TEXT_FAINT).small());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if state.downloading.contains(&id) {
                                ui.spinner();
                                return;
                            }
                            let have = if l.fixture_type(&id).is_some() {
                                Some(t("in this show"))
                            } else if snap.items.iter().any(|x| x.id == id) {
                                Some(t("in the library"))
                            } else {
                                None
                            };
                            let label = if have.is_some() { t("Download again") } else { t("Download") };
                            if ui.button(theme::with_icon(icons::DOWNLOAD, label)).on_hover_text(t("Add it to this show and to the fixture library")).clicked() {
                                fetch.push((*e).clone());
                            }
                            if let Some(h) = have {
                                ui.label(RichText::new(h).color(theme::GREEN).small());
                            }
                        });
                    });
                }
            });
        });
        types.ofl_open = open;
        if !fetch.is_empty() {
            let done = types.imports.clone();
            let ctx = ctx.clone();
            browser.download(fetch, move |e, r| {
                let mut batch = ImportDone { imported: Vec::new(), failed: Vec::new(), notes: 0 };
                match r {
                    Ok(imp) => {
                        for n in &imp.notes {
                            log::warn!("fixture download ({}): {n}", e.url());
                        }
                        batch.notes = imp.notes.len();
                        batch.imported.push(imp.fixture);
                    }
                    Err(err) => batch.failed.push(format!("{} {}: {err:#}", e.maker_name, e.name)),
                }
                done.lock().push(batch);
                ctx.request_repaint();
            });
        }
    }

    /// Put fixture files parsed in the background into the show (called every frame).
    pub(super) fn poll_fixture_imports(&mut self, show: &mut crate::show::Show) {
        let batches = std::mem::take(&mut *self.light.types.imports.lock());
        for ImportDone { imported, failed, notes } in batches {
            if let Some(last) = imported.last() {
                let id = last.id.clone();
                let name = type_label(last);
                let l = &mut show.ws.lighting;
                for ty in &imported {
                    // Importing again replaces the type (patched fixtures keep their mode by name).
                    replace_in_show(l, ty.clone());
                }
                show.edited();
                self.light.set_add_type(&id);
                self.light.types.edit = Some(TypeEdit::new(last.clone(), Origin::Show));
                let msg = match (imported.len(), notes) {
                    (1, 0) => tr!("Imported {name}", name = name),
                    (1, n) => tr!("Imported {name} ({n} notes in the error log)", name = name, n = n.to_string()),
                    (k, 0) => tr!("Imported {n} fixture types", n = k.to_string()),
                    (k, n) => tr!("Imported {k} fixture types ({n} notes in the error log)", k = k.to_string(), n = n.to_string()),
                };
                self.notify(msg);
                self.light.types.save_to_library(imported);
            }
            if !failed.is_empty() {
                self.notify_error(tr!("Can't import the fixture: {e}", e = failed.join("; ")));
            }
        }
    }

    /// Save a fixture type as an `.oqfixture` file (outside the show lock: the dialog blocks).
    pub(super) fn export_fixture(&mut self, ty: FixtureType) {
        let stem: String = type_label(&ty).chars().map(|c| if c.is_alphanumeric() || c == '-' || c == ' ' { c } else { '_' }).collect();
        let Some(path) = rfd::FileDialog::new()
            .add_filter(t("OpenQ fixture"), &[crate::light::fixlib::EXTENSION])
            .set_file_name(format!("{}.{}", stem.trim(), crate::light::fixlib::EXTENSION))
            .save_file()
        else {
            return;
        };
        match crate::recovery::write_atomic(&path, crate::light::fixlib::write(&ty).as_bytes()) {
            Ok(()) => self.notify(tr!("Saved {file}", file = path.display().to_string())),
            Err(e) => self.notify_error(tr!("Can't save the fixture type: {e}", e = format!("{e:#}"))),
        }
    }

    /// Show the results of finished library jobs.
    pub(super) fn poll_fixture_library(&mut self) {
        for r in self.light.types.take_results() {
            match r {
                Ok(m) => self.notify(m),
                Err(e) => self.notify_error(e),
            }
        }
    }
}

/// Put a type into the show, replacing one with the same id.
pub(crate) fn replace_in_show(l: &mut Lighting, ty: FixtureType) {
    match l.fixture_types.iter_mut().find(|t| t.id == ty.id) {
        Some(t) => *t = ty,
        None => l.fixture_types.push(ty),
    }
}

/// Master / cell picker and the cell tools.
fn unit_bar(ui: &mut Ui, e: &mut TypeEdit) {
    let m = &mut e.ty.modes[e.mode];
    if e.unit.is_some_and(|u| u >= m.heads.len()) {
        e.unit = None;
    }
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(theme::caption(t("Channels of")));
        if ui.selectable_label(e.unit.is_none(), t("Master")).on_hover_text(t("Channels of the whole fixture")).clicked() {
            e.unit = None;
            e.channel = None;
        }
        if m.heads.len() <= 16 {
            for (i, h) in m.heads.iter().enumerate() {
                if ui.selectable_label(e.unit == Some(i), tr!("Cell {name}", name = h.name.clone())).clicked() {
                    e.unit = Some(i);
                    e.channel = None;
                }
            }
        } else {
            let text = e.unit.map_or_else(|| t("choose a cell").to_string(), |i| tr!("Cell {name}", name = m.heads[i].name.clone()));
            egui::ComboBox::from_id_salt("type_cell").selected_text(text).show_ui(ui, |ui| {
                for (i, h) in m.heads.iter().enumerate() {
                    if ui.selectable_label(e.unit == Some(i), tr!("Cell {name}", name = h.name.clone())).clicked() {
                        e.unit = Some(i);
                        e.channel = None;
                    }
                }
            });
        }
        if ui.small_button(icons::ADD).on_hover_text(t("Add a cell (a copy of the last one, after it)")).clicked() {
            action = Some(0);
        }
        if let Some(i) = e.unit {
            ui.label(t("Name"));
            ui.add(egui::TextEdit::singleline(&mut m.heads[i].name).desired_width(70.0));
            if ui.small_button(icons::DELETE).on_hover_text(t("Remove this cell")).clicked() {
                action = Some(1);
            }
        }
        if !m.heads.is_empty() {
            ui.separator();
            ui.add(DragValue::new(&mut e.make_cells).range(1..=MAX_CELLS));
            if ui.button(t("cells like cell 1")).on_hover_text(t("Repeat cell 1's channels, one cell after another")).clicked() {
                action = Some(2);
            }
        }
    });
    match action {
        Some(0) => {
            let head = match m.heads.last() {
                Some(last) => {
                    let (lo, hi) = span(&last.channels).unwrap_or((0, 0));
                    let shift = if last.channels.is_empty() { 0 } else { hi - lo + 1 };
                    Head {
                        name: (m.heads.len() + 1).to_string(),
                        channels: last.channels.iter().map(|c| ChannelDef { offset: c.offset + shift, fine: c.fine.map(|f| f + shift), ..c.clone() }).collect(),
                    }
                }
                None => {
                    let next = m.footprint();
                    Head { name: "1".into(), channels: vec![ChannelDef { attribute: Attribute::Red, offset: next, ..Default::default() }] }
                }
            };
            if m.heads.len() < MAX_CELLS as usize && head.channels.iter().all(|c| c.offset < 512 && c.fine.is_none_or(|f| f < 512)) {
                m.heads.push(head);
                e.unit = Some(m.heads.len() - 1);
            }
        }
        Some(1) => {
            if let Some(i) = e.unit {
                m.heads.remove(i);
                e.unit = None;
            }
        }
        Some(2) => repeat_cells(m, e.make_cells),
        _ => {}
    }
}

/// The channels of the chosen unit.
fn channel_table(ui: &mut Ui, e: &mut TypeEdit) {
    let m = &mut e.ty.modes[e.mode];
    let next_free = m.footprint();
    let chans = match e.unit {
        None => &mut m.channels,
        Some(i) => &mut m.heads[i].channels,
    };
    const COLS: [f32; 7] = [64.0, 88.0, 150.0, 96.0, 96.0, 90.0, 52.0];
    ui.horizontal(|ui| {
        for (h, w) in [t("DMX"), t("Fine"), t("Attribute"), t("Default"), t("Highlight"), t("Ranges"), t("Break")].iter().zip(COLS) {
            ui.add_sized([w, 18.0], egui::Label::new(theme::caption(h)));
        }
    });
    let mut delete = None;
    let taken: Vec<Attribute> = chans.iter().map(|c| c.attribute).collect();
    for (i, c) in chans.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            // 1-based DMX slots in the UI.
            let mut slot = c.offset + 1;
            ui.add_sized([COLS[0], 20.0], DragValue::new(&mut slot).range(1..=512));
            c.offset = slot - 1;
            ui.allocate_ui(egui::vec2(COLS[1], 20.0), |ui| {
                ui.set_min_width(COLS[1]);
                let mut has = c.fine.is_some();
                if ui.checkbox(&mut has, "").on_hover_text(t("16-bit: a fine byte")).changed() {
                    c.fine = has.then_some((c.offset + 1).min(511));
                }
                if let Some(f) = &mut c.fine {
                    let mut s = *f + 1;
                    ui.add(DragValue::new(&mut s).range(1..=512));
                    *f = s - 1;
                }
            });
            ui.allocate_ui(egui::vec2(COLS[2], 20.0), |ui| {
                ui.set_min_width(COLS[2]);
                let shown = match c.attribute {
                    Attribute::Extra(_) if !c.name.is_empty() => c.name.clone(),
                    a => a.label().to_string(),
                };
                // Extra channels: the next number free in this unit (or the channel's own).
                let extra = match c.attribute {
                    Attribute::Extra(n) => Attribute::Extra(n),
                    _ => (1..=255).map(Attribute::Extra).find(|a| !taken.contains(a)).unwrap_or(Attribute::Extra(255)),
                };
                egui::ComboBox::from_id_salt(("type_attr", i)).selected_text(shown).width(COLS[2] - 8.0).height(420.0).show_ui(ui, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut c.name).hint_text(t("Channel name")).desired_width(160.0));
                    for f in Feature::ALL {
                        ui.label(theme::caption(f.label()));
                        for a in Attribute::ALL.into_iter().filter(|a| a.feature() == f) {
                            ui.selectable_value(&mut c.attribute, a, a.label());
                        }
                        if f == Feature::Control {
                            ui.selectable_value(&mut c.attribute, extra, extra.label()).on_hover_text(t("A further channel with no attribute of its own (named above)"));
                        }
                    }
                });
            });
            optional_percent(ui, &mut c.default, COLS[3], t("Value with nothing controlling it (empty: the attribute's own)"));
            optional_percent(ui, &mut c.highlight, COLS[4], t("Value while highlighted (empty: open/full)"));
            let n = c.ranges.len();
            let label = if n == 0 { t("none").to_string() } else { n.to_string() };
            if ui.add_sized([COLS[5], 20.0], egui::Button::selectable(e.channel == Some(i), label)).clicked() {
                e.channel = if e.channel == Some(i) { None } else { Some(i) };
            }
            // 1-based in the UI, like GDTF.
            let mut brk = c.dmx_break + 1;
            ui.add_sized([COLS[6], 20.0], DragValue::new(&mut brk).range(1..=8)).on_hover_text(t("DMX break (fixtures with several DMX inputs)"));
            c.dmx_break = brk - 1;
            if ui.small_button(icons::DELETE).on_hover_text(t("Delete channel")).clicked() {
                delete = Some(i);
            }
        });
    }
    if let Some(i) = delete {
        chans.remove(i);
        e.channel = None;
    }
    if ui.button(theme::with_icon(icons::ADD, t("Add channel"))).clicked() && next_free < 512 {
        let attr = Attribute::ALL.into_iter().find(|a| !chans.iter().any(|c| c.attribute == *a)).unwrap_or(Attribute::Control);
        chans.push(ChannelDef { attribute: attr, offset: next_free, ..Default::default() });
    }
}

/// A percentage that can be left empty (a checkbox turns it on).
fn optional_percent(ui: &mut Ui, v: &mut Option<f32>, width: f32, tip: &str) {
    ui.allocate_ui(egui::vec2(width, 20.0), |ui| {
        ui.set_min_width(width);
        let mut on = v.is_some();
        if ui.checkbox(&mut on, "").on_hover_text(tip).changed() {
            *v = on.then_some(0.0);
        }
        if let Some(x) = v {
            let mut p = *x * 100.0;
            ui.add(DragValue::new(&mut p).range(0.0..=100.0).speed(0.5).max_decimals(1).suffix(" %"));
            *x = (p / 100.0).clamp(0.0, 1.0);
        }
    });
}

/// Named ranges of the chosen channel.
fn range_editor(ui: &mut Ui, e: &mut TypeEdit) {
    let m = &mut e.ty.modes[e.mode];
    let chans = match e.unit {
        None => &mut m.channels,
        Some(i) => &mut m.heads[i].channels,
    };
    let Some(c) = e.channel.and_then(|i| chans.get_mut(i)) else {
        ui.label(RichText::new(t("Click a channel's Ranges to name its DMX ranges (wheel slots, shutter, macros).")).color(theme::TEXT_FAINT).small());
        return;
    };
    ui.label(theme::caption(&tr!("Ranges of {attr}", attr = c.attribute.label())));
    let mut delete = None;
    for (i, r) in c.ranges.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add(DragValue::new(&mut r.from).range(0..=255)).on_hover_text(t("First DMX value"));
            ui.label("–");
            ui.add(DragValue::new(&mut r.to).range(0..=255)).on_hover_text(t("Last DMX value"));
            ui.add(egui::TextEdit::singleline(&mut r.name).hint_text(t("Name")).desired_width(180.0));
            let mut has = r.color.is_some();
            if ui.checkbox(&mut has, "").on_hover_text(t("Slot colour")).changed() {
                r.color = has.then_some([255, 255, 255]);
            }
            match &mut r.color {
                Some(rgb) => {
                    let mut c = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                    if ui.color_edit_button_srgba(&mut c).changed() {
                        *rgb = [c.r(), c.g(), c.b()];
                    }
                }
                // Keep the columns lined up.
                None => ui.add_space(ui.spacing().interact_size.x + ui.spacing().item_spacing.x),
            }
            ui.add(egui::TextEdit::singleline(&mut r.image).hint_text(t("Image (gobo)")).desired_width(120.0));
            if ui.small_button(icons::DELETE).clicked() {
                delete = Some(i);
            }
        });
    }
    if let Some(i) = delete {
        c.ranges.remove(i);
    }
    ui.horizontal_wrapped(|ui| {
        if ui.button(theme::with_icon(icons::ADD, t("Add range"))).clicked() {
            let from = c.ranges.iter().map(|r| r.from.max(r.to) as u16 + 1).max().unwrap_or(0).min(255) as u8;
            c.ranges.push(ChannelRange { from, to: 255, name: tr!("Slot {n}", n = (c.ranges.len() + 1).to_string()), ..Default::default() });
        }
        if ui.button(t("Sort")).clicked() {
            c.ranges.sort_by_key(|r| r.from.min(r.to));
        }
        ui.separator();
        ui.add(DragValue::new(&mut e.split).range(1..=256));
        if ui.button(t("equal slots")).on_hover_text(t("Replace the ranges by equal slots over 0–255")).clicked() {
            c.ranges = split_ranges(e.split);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_ranges_ids() {
        let mut m = FixtureMode::sequential("x", &[(Attribute::Dimmer, false)]);
        m.heads.push(Head { name: "1".into(), channels: vec![
            ChannelDef { attribute: Attribute::Red, offset: 1, fine: Some(2), ..Default::default() },
            ChannelDef { attribute: Attribute::Green, offset: 3, ..Default::default() },
        ] });
        repeat_cells(&mut m, 4);
        assert_eq!(m.heads.len(), 4);
        assert_eq!(m.heads[3].channels[0].offset, 10);
        assert_eq!(m.heads[3].channels[0].fine, Some(11));
        assert_eq!(m.footprint(), 13);
        repeat_cells(&mut m, 1000);
        assert!(m.heads.len() < 200, "stops at slot 512");
        let r = split_ranges(4);
        assert_eq!((r[0].from, r[0].to, r[3].from, r[3].to), (0, 63, 192, 255));
        let ty = FixtureType { manufacturer: "Acme".into(), name: "Bar".into(), ..Default::default() };
        assert_eq!(new_id(&ty, &|id| id == "user/acme/bar"), "user/acme/bar-2");
    }

    #[test]
    fn issues_found() {
        let mut ty = crate::light::library::generic("generic/rgb").unwrap();
        assert!(type_issues(&ty).is_empty());
        ty.modes[0].channels[1].offset = 0;
        ty.modes[0].channels[2].attribute = Attribute::Red;
        let i = type_issues(&ty);
        assert!(i.iter().any(|x| x.contains("twice")), "{i:?}");
    }
}
