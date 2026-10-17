//! Lighting playback for touch screens: every executor (masters, sequences, playback buttons —
//! "executors" in the UI) as cards on a grid.
//!
//! Shown as the Playback page of the Lighting workspace (both modes; buttons are added and edited
//! there in Edit mode) and, on request, in a window of its own that can go fullscreen on a chosen
//! display. Every action goes through [`App::control`], so a replica drives the main device and
//! nothing here needs the programmer: buttons play as hidden sequences
//! ([`crate::light::model::LightButton`]).
//!
//! Buttons sit on pages ([`Lighting::button_pages`]; buttons on a page not shown keep playing) and
//! are put in order by dragging their handle in Edit mode (onto another button, or onto a page
//! tab). Whether the window is open and its display are kept per computer
//! (`<config>/playback-window`), so a touch screen comes back by itself.

use super::lighting::{level_bar, to_c32};
use super::{App, icons, theme};
use crate::control::{Command, LightControl};
use crate::i18n::t;
use crate::light::model::{ButtonId, LightButton, Lighting};
use crate::light::playback::SequenceStatus;
use crate::show::Show;
use egui::{Color32, DragValue, RichText, Ui, ViewportBuilder, ViewportCommand, ViewportId};

/// Card sizes: the smallest a card gets (cards stretch so a row fills the width).
const SEQUENCE_CARD: egui::Vec2 = egui::vec2(220.0, 168.0);
const BUTTON_CARD: egui::Vec2 = egui::vec2(150.0, 104.0);
const GAP: f32 = 10.0;

/// State of the Playback page and window.
#[derive(Default)]
pub(crate) struct PlaybackUi {
    /// The Playback window is open.
    pub(crate) window: bool,
    /// Display the window is fullscreen on (None = an ordinary window).
    pub(crate) display: Option<String>,
    /// `window` and `display` as read from or last written to this computer's config.
    saved: Option<WindowPlace>,
    /// Button page shown.
    page: u16,
    /// Page whose name is being edited.
    renaming: Option<u16>,
    /// Monitor index the window was last placed on (None until it opened).
    placed: Option<Option<usize>>,
    /// Button whose editor is open.
    edit: Option<ButtonId>,
    /// Flash buttons held down: (the card's widget id, button).
    held: Vec<(egui::Id, ButtonId)>,
}

/// Where the Playback window goes on this computer.
#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Default, Debug)]
#[serde(default)]
struct WindowPlace {
    open: bool,
    display: Option<String>,
}

fn place_file() -> Option<std::path::PathBuf> {
    crate::remote::config_dir().map(|d| d.join("playback-window"))
}

impl PlaybackUi {
    /// Read this computer's window place the first time, write it when it changed.
    fn keep_place(&mut self) {
        let now = WindowPlace { open: self.window, display: self.display.clone() };
        match &self.saved {
            None => {
                let read = place_file().and_then(|p| crate::recovery::read_text(&p, 4096).ok()).and_then(|t| serde_json::from_str::<WindowPlace>(&t).ok()).unwrap_or_default();
                self.window = read.open;
                self.display = read.display.clone();
                self.saved = Some(read);
            }
            Some(saved) if *saved != now => {
                if let Some(path) = place_file() {
                    let write = || -> anyhow::Result<()> {
                        if let Some(dir) = path.parent() {
                            std::fs::create_dir_all(dir)?;
                        }
                        crate::recovery::write_atomic(&path, serde_json::to_string(&now)?.as_bytes())
                    };
                    if let Err(e) = write() {
                        log::warn!("playback window: can't save its place: {e:#}");
                    }
                }
                self.saved = Some(now);
            }
            Some(_) => {}
        }
    }
}

/// Put button `id` where button `onto` is (taking its page); the buttons between move up or down.
fn move_button(l: &mut Lighting, id: ButtonId, onto: ButtonId) {
    let (Some(from), Some(to)) = (l.buttons.iter().position(|b| b.id == id), l.buttons.iter().position(|b| b.id == onto)) else { return };
    if from == to {
        return;
    }
    let mut b = l.buttons.remove(from);
    b.page = l.buttons[if to > from { to - 1 } else { to }].page;
    l.buttons.insert(to, b);
}

/// Delete button page `k`: its buttons go to the page before (or the next one), later pages move down.
fn delete_page(l: &mut Lighting, k: u16) {
    if (k as usize) >= l.button_pages.len() {
        return;
    }
    l.button_pages.remove(k as usize);
    for b in &mut l.buttons {
        if b.page > k || (b.page == k && k > 0) {
            b.page -= 1;
        }
    }
    if l.button_pages.len() <= 1 {
        l.button_pages.clear();
        for b in &mut l.buttons {
            b.page = 0;
        }
    }
}

fn viewport_id() -> ViewportId {
    ViewportId::from_hash_of("openq-light-playback")
}

/// Columns of a card grid and the card width that fills the row.
fn grid(avail: f32, min: f32) -> (usize, f32) {
    let cols = ((avail + GAP) / (min + GAP)).floor().max(1.0);
    (cols as usize, ((avail + GAP) / cols - GAP).floor())
}

impl App {
    /// The Playback page: window/display controls, then the grid.
    pub(super) fn playback_page(&mut self, ui: &mut Ui, show: &mut Show) {
        self.light.playback.keep_place();
        ui.horizontal_wrapped(|ui| {
            let open = self.light.playback.window;
            let label = if open { t("Close the playback window") } else { t("Open in a window") };
            if ui.button(theme::with_icon(icons::OPEN_IN_NEW, label)).on_hover_text(t("Sequences and executors in a window of their own, e.g. on a touch screen")).clicked() {
                self.light.playback.window = !open;
            }
            ui.label(t("Display"));
            self.playback_display_choice(ui);
            // Card size of the executors (saved with the show, so the window looks the same everywhere).
            if self.can_design() {
                ui.separator();
                ui.label(t("Executor size"));
                let l = &mut show.ws.lighting;
                let mut v = l.button_scale;
                let r = ui.add(egui::Slider::new(&mut v, 0.6..=2.5).step_by(0.05).custom_formatter(|v, _| format!("{:.0} %", v * 100.0)));
                if r.changed() {
                    l.button_scale = v;
                    show.edited();
                }
            }
        });
        ui.add_space(6.0);
        let edit = self.can_design();
        egui::ScrollArea::vertical().id_salt("playback_page").auto_shrink(false).show(ui, |ui| self.playback_grid(ui, show, edit));
    }

    /// Window or fullscreen display for the Playback window.
    fn playback_display_choice(&mut self, ui: &mut Ui) {
        let d = self.displays.lock();
        let (loaded, loading) = (d.loaded, d.loading);
        let current = match &self.light.playback.display {
            None => t("Window").to_string(),
            Some(name) => d.list.iter().find(|x| &x.name == name).map_or_else(|| tr!("{name} (not connected)", name = name.clone()), |x| x.label()),
        };
        let list = d.list.clone();
        drop(d);
        let r = egui::ComboBox::from_id_salt("playback_display").selected_text(current).width(220.0).show_ui(ui, |ui| {
            let mut pick = self.light.playback.display.clone();
            ui.selectable_value(&mut pick, None, theme::with_icon(icons::DESKTOP_WINDOWS, t("Window")));
            for disp in &list {
                ui.selectable_value(&mut pick, Some(disp.name.clone()), theme::with_icon(icons::TV, &disp.label()));
            }
            if loading {
                ui.label(RichText::new(t("Looking for displays…")).weak());
            }
            pick
        });
        if let Some(pick) = r.inner {
            if pick.is_some() {
                self.light.playback.window = true;
            }
            self.light.playback.display = pick;
        }
        if r.response.clicked() && !loaded && !loading {
            self.refresh_displays(ui.ctx());
        }
        if ui.button(icons::REFRESH).on_hover_text(t("Refresh displays")).clicked() {
            self.refresh_displays(ui.ctx());
        }
    }

    /// The Playback window (fullscreen on its display if one is chosen). Playback only: buttons
    /// are edited on the Playback page.
    pub(super) fn playback_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        self.light.playback.keep_place();
        if !self.light.playback.window {
            self.light.playback.placed = None;
            return;
        }
        let monitor = {
            let d = self.displays.lock();
            let needed = self.light.playback.display.is_some() && !d.loaded && !d.loading;
            let i = self.light.playback.display.as_ref().and_then(|n| d.list.iter().position(|x| &x.name == n));
            drop(d);
            if needed {
                self.refresh_displays(ctx);
            }
            i
        };
        let mut builder = ViewportBuilder::default().with_title(t("OpenQ Playback")).with_icon(super::app_icon()).with_inner_size([960.0, 640.0]);
        if let Some(i) = monitor {
            builder = builder.with_monitor(i);
        }
        let mut close = false;
        let mut toggle_fs = false;
        ctx.clone().show_viewport_immediate(viewport_id(), builder, |ui, _| {
            egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG_APP).inner_margin(egui::Margin::same(14))).show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("playback_window").auto_shrink(false).show(ui, |ui| self.playback_grid(ui, show, false));
            });
            let ctx = ui.ctx().clone();
            toggle_fs = ctx.input(|i| i.key_pressed(egui::Key::F11));
            close = ctx.input(|i| i.viewport().close_requested());
        });
        // Follow display changes on an open window (a new window starts on its display).
        let p = &mut self.light.playback;
        if p.placed != Some(monitor) {
            if p.placed.is_some() {
                let cmd = match monitor {
                    Some(i) => ViewportCommand::SetMonitor(i),
                    None => ViewportCommand::Fullscreen(false),
                };
                ctx.send_viewport_cmd_to(viewport_id(), cmd);
            }
            p.placed = Some(monitor);
        }
        if toggle_fs {
            let fs = ctx.input_for(viewport_id(), |i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd_to(viewport_id(), ViewportCommand::Fullscreen(!fs));
        }
        if close {
            p.window = false;
            p.placed = None;
            p.held.clear();
        }
    }

    /// Every executor, as cards: the masters (grand master, blackout, tempo) on top, then the
    /// sequences, then the playback buttons ("executors" in the UI). `edit`: executors can be
    /// added and edited.
    pub(super) fn playback_grid(&mut self, ui: &mut Ui, show: &mut Show, edit: bool) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        ui.label(theme::caption(t("Masters")));
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| self.masters_strip(ui, show, true));
        ui.add_space(12.0);
        let status = show.media.light.status();
        let masters = show.media.light.masters().clone();
        let mut cmds = Vec::new();
        let l = &show.ws.lighting;
        let avail = ui.available_width();
        ui.label(theme::caption(t("Sequences")));
        ui.add_space(2.0);
        if l.sequences.is_empty() {
            ui.label(RichText::new(t("Sequences appear here as executors.")).color(theme::TEXT_DIM));
        }
        let (cols, w) = grid(avail, SEQUENCE_CARD.x);
        for row in l.sequences.chunks(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for seq in row {
                    sequence_card(ui, l, seq.id, status.iter().find(|x| x.id == seq.id), masters.level(seq.id), w, &mut cmds);
                }
            });
            ui.add_space(GAP - ui.spacing().item_spacing.y);
        }
        ui.add_space(8.0);
        ui.label(theme::caption(t("Executors")));
        ui.add_space(2.0);
        let mut page_edit: Option<PageEdit> = None;
        let pages = l.button_page_count();
        let page = self.light.playback.page.min(pages - 1);
        self.light.playback.page = page;
        if pages > 1 || edit {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for k in 0..pages {
                    let name = l.button_pages.get(k as usize).filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| tr!("Page {n}", n = (k + 1).to_string()));
                    if edit && self.light.playback.renaming == Some(k) {
                        let mut text = l.button_pages.get(k as usize).cloned().unwrap_or_default();
                        let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(120.0));
                        r.request_focus();
                        if r.changed() {
                            page_edit = Some(PageEdit::Rename(k, text));
                        }
                        if r.lost_focus() {
                            self.light.playback.renaming = None;
                        }
                        continue;
                    }
                    let r = ui.add(egui::Button::selectable(page == k, RichText::new(name).font(theme::semibold(13.0))).min_size(egui::vec2(64.0, 30.0)));
                    if r.clicked() {
                        self.light.playback.page = k;
                    }
                    if edit {
                        // A button dropped on a tab moves to that page (at its end).
                        if r.dnd_hover_payload::<ButtonId>().is_some() {
                            ui.painter().rect_stroke(r.rect, 6.0, egui::Stroke::new(2.0, theme::ACCENT), egui::StrokeKind::Outside);
                        }
                        if let Some(id) = r.dnd_release_payload::<ButtonId>() {
                            page_edit = Some(PageEdit::MoveTo(*id, k));
                        }
                        if r.double_clicked() {
                            self.light.playback.renaming = Some(k);
                        }
                        r.context_menu(|ui| {
                            if ui.button(theme::with_icon(icons::EDIT, t("Rename page"))).clicked() {
                                self.light.playback.renaming = Some(k);
                                ui.close();
                            }
                            if ui.add_enabled(pages > 1, egui::Button::new(theme::with_icon(icons::DELETE, t("Delete page")))).on_hover_text(t("Its executors go to the page before")).clicked() {
                                page_edit = Some(PageEdit::Delete(k));
                                ui.close();
                            }
                        });
                    }
                }
                if edit && (pages as usize) < crate::limits::MAX_BUTTON_PAGES && ui.add(egui::Button::new(icons::ADD).min_size(egui::vec2(30.0, 30.0))).on_hover_text(t("New page")).clicked() {
                    page_edit = Some(PageEdit::Add);
                }
            });
            ui.add_space(6.0);
        }
        if l.buttons.is_empty() && !edit {
            ui.label(RichText::new(t("Add executors on the Playback page in Edit mode.")).color(theme::TEXT_DIM));
        }
        let scale = button_scale(l);
        let (cols, w) = grid(avail, BUTTON_CARD.x * scale);
        let mut add = false;
        let shown: Vec<&LightButton> = l.page_buttons(page).collect();
        let n = shown.len() + edit as usize;
        let mut dropped: Option<(ButtonId, ButtonId)> = None;
        let mut i = 0;
        while i < n {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for k in i..(i + cols).min(n) {
                    match shown.get(k) {
                        Some(b) => {
                            let on = status.iter().find(|x| x.id == b.sequence_id()).is_some_and(|x| x.cue.is_some());
                            if let Some(from) = self.button_card(ui, l, b, on, w, edit, &mut cmds) {
                                dropped = Some((from, b.id));
                            }
                        }
                        None => add = add_card(ui, w, scale),
                    }
                }
            });
            ui.add_space(GAP - ui.spacing().item_spacing.y);
            i += cols;
        }
        // Flash buttons let go of (or no longer drawn: the window closed) are released.
        self.light.playback.held.retain(|(id, b)| {
            let down = ui.ctx().read_response(*id).is_some_and(|r| r.is_pointer_button_down_on());
            if !down && let Some(b) = l.button(*b) {
                cmds.push(LightControl::Release(Some(b.sequence_id()), b.fade as f32));
            }
            down
        });
        if add && edit {
            let l = &mut show.ws.lighting;
            let id = l.buttons.iter().map(|b| b.id).max().unwrap_or(0) + 1;
            l.buttons.push(LightButton { id, name: tr!("Executor {n}", n = id.to_string()), page, ..Default::default() });
            self.light.playback.edit = Some(id);
            show.edited();
        }
        if edit && (dropped.is_some() || page_edit.is_some()) {
            let l = &mut show.ws.lighting;
            if let Some((from, onto)) = dropped
                && from != onto
            {
                move_button(l, from, onto);
            }
            match page_edit {
                Some(PageEdit::Add) => {
                    if l.button_pages.is_empty() {
                        l.button_pages.push(tr!("Page {n}", n = "1"));
                    }
                    l.button_pages.push(tr!("Page {n}", n = (l.button_pages.len() + 1).to_string()));
                    self.light.playback.page = l.button_pages.len() as u16 - 1;
                }
                Some(PageEdit::Rename(k, text)) => {
                    if l.button_pages.is_empty() {
                        l.button_pages.push(String::new());
                    }
                    if let Some(p) = l.button_pages.get_mut(k as usize) {
                        *p = text.chars().take(crate::limits::MAX_PAGE_NAME).collect();
                    }
                }
                Some(PageEdit::Delete(k)) => delete_page(l, k),
                Some(PageEdit::MoveTo(id, k)) => {
                    // To the end of that page.
                    if let Some(from) = l.buttons.iter().position(|b| b.id == id) {
                        let mut b = l.buttons.remove(from);
                        b.page = k;
                        let at = l.buttons.iter().rposition(|x| x.page == k).map_or(l.buttons.len(), |i| i + 1);
                        l.buttons.insert(at, b);
                    }
                }
                None => {}
            }
            show.edited();
        }
        if edit {
            self.button_editor(ui.ctx(), show);
        } else {
            self.light.playback.edit = None;
        }
        for c in cmds {
            self.control(show, Command::Light(c));
        }
    }

    /// One playback button: click toggles it (a flash button is on while held). In Edit mode it
    /// has a drag handle; returns the button dropped onto it.
    #[allow(clippy::too_many_arguments)]
    fn button_card(&mut self, ui: &mut Ui, l: &Lighting, b: &LightButton, on: bool, w: f32, edit: bool, cmds: &mut Vec<LightControl>) -> Option<ButtonId> {
        let k = button_scale(l);
        let (rect, r) = ui.allocate_exact_size(egui::vec2(w, BUTTON_CARD.y * k), egui::Sense::click_and_drag());
        let colour = Color32::from_rgb(b.color[0], b.color[1], b.color[2]);
        let p = ui.painter();
        let fill = if on { colour.gamma_multiply(0.85) } else if r.hovered() { theme::BG_HOVER } else { theme::BG_RAISED };
        p.rect_filled(rect, 10.0, fill);
        p.rect_stroke(rect, 10.0, egui::Stroke::new(if on { 2.0 } else { 1.5 }, if on { Color32::WHITE } else { colour }), egui::StrokeKind::Inside);
        let inner = rect.shrink2(egui::vec2(12.0, 10.0) * k);
        let fg = if on { Color32::WHITE } else { theme::TEXT };
        let clip = p.with_clip_rect(inner);
        let name = if b.name.is_empty() { b.id.to_string() } else { b.name.clone() };
        // In Edit mode the name stops before the drag handle and the pencil.
        let name_clip = if edit { p.with_clip_rect(inner.with_max_x(inner.max.x.min(rect.right() - 60.0))) } else { clip.clone() };
        name_clip.text(inner.left_top(), egui::Align2::LEFT_TOP, name, theme::semibold(16.0 * k), fg);
        let what = match (b.preset.and_then(|id| l.preset(id)), b.fixtures.len(), b.values.len()) {
            (Some(pr), n, _) => {
                let preset = if pr.name.is_empty() { pr.id.to_string() } else { pr.name.clone() };
                tr!("{preset} · {n} fixtures", preset = preset, n = n.to_string())
            }
            (None, _, 0) if b.effects.is_empty() => t("Empty").to_string(),
            (None, _, n) => tr!("{n} fixtures", n = n.to_string()),
        };
        clip.text(inner.left_top() + egui::vec2(0.0, 24.0 * k), egui::Align2::LEFT_TOP, what, egui::FontId::proportional(12.0 * k), if on { Color32::WHITE } else { theme::TEXT_DIM });
        let mode = if b.flash { t("Flash") } else { t("Toggle") };
        p.text(inner.left_bottom(), egui::Align2::LEFT_BOTTOM, mode, theme::semibold(10.5 * k), if on { Color32::WHITE } else { theme::TEXT_FAINT });
        if on {
            p.text(inner.right_bottom(), egui::Align2::RIGHT_BOTTOM, t("ON"), theme::semibold(13.0 * k), Color32::WHITE);
        }
        let seq = b.sequence_id();
        let go = LightControl::Go(seq, Some(b.cue_id()));
        let held = self.light.playback.held.iter().any(|(id, _)| *id == r.id);
        if b.flash {
            if r.is_pointer_button_down_on() && !held {
                self.light.playback.held.push((r.id, b.id));
                cmds.push(go);
            }
        } else if r.clicked() {
            cmds.push(if on { LightControl::Release(Some(seq), b.fade as f32) } else { go });
        }
        if edit {
            let pencil = egui::Rect::from_min_size(egui::pos2(rect.right() - 30.0, rect.top() + 6.0), egui::vec2(24.0, 24.0));
            let pr = ui.interact(pencil, r.id.with("edit"), egui::Sense::click());
            p.text(pencil.center(), egui::Align2::CENTER_CENTER, icons::EDIT, theme::icon_font(16.0), if pr.hovered() { Color32::WHITE } else { theme::TEXT_DIM });
            if pr.on_hover_text(t("Edit the executor")).clicked() {
                self.light.playback.edit = Some(b.id);
            }
            r.context_menu(|ui| {
                if ui.button(theme::with_icon(icons::EDIT, t("Edit the executor"))).clicked() {
                    self.light.playback.edit = Some(b.id);
                }
            });
            let handle = egui::Rect::from_min_size(egui::pos2(rect.right() - 56.0, rect.top() + 6.0), egui::vec2(24.0, 24.0));
            let hr = ui.interact(handle, r.id.with("drag"), egui::Sense::drag()).on_hover_cursor(egui::CursorIcon::Grab);
            p.text(handle.center(), egui::Align2::CENTER_CENTER, icons::DRAG_INDICATOR, theme::icon_font(16.0), if hr.hovered() || hr.dragged() { Color32::WHITE } else { theme::TEXT_DIM });
            hr.on_hover_text(t("Drag onto another executor or a page to move it")).dnd_set_drag_payload(b.id);
            if r.dnd_hover_payload::<ButtonId>().is_some_and(|id| *id != b.id) {
                p.rect_stroke(rect.expand(3.0), 12.0, egui::Stroke::new(2.0, theme::ACCENT), egui::StrokeKind::Outside);
            }
            return r.dnd_release_payload::<ButtonId>().map(|id| *id).filter(|id| *id != b.id);
        }
        None
    }

    /// Settings of the button being edited: name, colour, preset and fixtures, stored values.
    fn button_editor(&mut self, ctx: &egui::Context, show: &mut Show) {
        let Some(id) = self.light.playback.edit else { return };
        let l = &mut show.ws.lighting;
        let Some(k) = l.buttons.iter().position(|b| b.id == id) else {
            self.light.playback.edit = None;
            return;
        };
        let mut b = l.buttons[k].clone();
        let mut open = true;
        let mut delete = false;
        egui::Window::new(tr!("Executor {n}", n = id.to_string()))
            .id(egui::Id::new("button_editor"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .show(ctx, |ui| {
                egui::Grid::new("button_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label(t("Name"));
                    ui.add(egui::TextEdit::singleline(&mut b.name).desired_width(220.0));
                    ui.end_row();
                    ui.label(t("Colour"));
                    ui.color_edit_button_srgb(&mut b.color);
                    ui.end_row();
                    ui.label(t("Preset"));
                    let label = |id: Option<u32>| match id.and_then(|id| l.preset(id)) {
                        Some(p) if p.name.is_empty() => p.id.to_string(),
                        Some(p) => format!("{} {}", p.id, p.name),
                        None => t("None").to_string(),
                    };
                    egui::ComboBox::from_id_salt("button_preset").selected_text(label(b.preset)).width(220.0).height(360.0).show_ui(ui, |ui| {
                        ui.selectable_value(&mut b.preset, None, t("None"));
                        for p in &l.presets {
                            ui.selectable_value(&mut b.preset, Some(p.id), label(Some(p.id)));
                        }
                    });
                    ui.end_row();
                    ui.label(t("Fixtures"));
                    // One line: the window grows to fit it.
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(tr!("{n} fixtures", n = b.fixtures.len().to_string())).color(theme::TEXT_DIM));
                        if !b.fixtures.is_empty() && ui.small_button(icons::CLOSE).on_hover_text(t("No fixtures")).clicked() {
                            b.fixtures.clear();
                        }
                        let sel = &self.light.selection;
                        if ui.add_enabled(!sel.is_empty(), egui::Button::new(t("Use selected"))).on_hover_text(t("The fixtures selected in the programmer")).clicked() {
                            b.fixtures = sel.clone();
                        }
                        ui.menu_button(t("Group"), |ui| {
                            for g in &l.groups {
                                let name = if g.name.is_empty() { g.id.to_string() } else { format!("{} {}", g.id, g.name) };
                                if ui.button(name).clicked() {
                                    b.fixtures = g.fixtures.clone();
                                }
                            }
                            if l.groups.is_empty() {
                                ui.label(RichText::new(t("No groups yet.")).color(theme::TEXT_DIM));
                            }
                        });
                    });
                    ui.end_row();
                    ui.label(t("Values"));
                    ui.horizontal_wrapped(|ui| {
                        let n: usize = b.values.values().map(|v| v.len()).sum();
                        let text = tr!("{n} values", n = n.to_string());
                        let text = if b.effects.is_empty() { text } else { format!("{text} · {}", tr!("{n} effects", n = b.effects.len().to_string())) };
                        ui.label(RichText::new(text).color(theme::TEXT_DIM));
                        if (n > 0 || !b.effects.is_empty()) && ui.small_button(icons::CLOSE).on_hover_text(t("Clear the values")).clicked() {
                            b.values = Default::default();
                            b.effects.clear();
                        }
                        let busy = !self.light.programmer.is_empty() || !self.light.programmer_effects.is_empty();
                        if ui
                            .add_enabled(busy, egui::Button::new(theme::with_icon(icons::SAVE, t("Store the programmer"))))
                            .on_hover_text(t("The programmer's values and effects (e.g. the fog machine at 80 %)"))
                            .clicked()
                        {
                            b.values = self.light.programmer.clone();
                            b.effects = self.light.programmer_effects.iter().filter(|r| !r.stop).cloned().collect();
                        }
                    });
                    ui.end_row();
                    ui.label(t("Fade"));
                    ui.add(DragValue::new(&mut b.fade).range(0.0..=600.0).speed(0.05).max_decimals(2).suffix(" s"));
                    ui.end_row();
                    ui.label(t("Mode"));
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut b.flash, false, t("Toggle")).on_hover_text(t("Each press switches it on or off"));
                        ui.selectable_value(&mut b.flash, true, t("Flash")).on_hover_text(t("On while held down"));
                    });
                    ui.end_row();
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(theme::with_icon(icons::DELETE, t("Delete executor"))).clicked() {
                        delete = true;
                    }
                    let swatch = to_c32(b.color.map(|c| c as f32 / 255.0));
                    ui.label(RichText::new(icons::SMART_BUTTON).font(theme::icon_font(18.0)).color(swatch));
                });
            });
        if delete {
            let seq = b.sequence_id();
            l.buttons.remove(k);
            self.light.playback.edit = None;
            show.edited();
            self.control(show, Command::Light(LightControl::Release(Some(seq), 0.0)));
            return;
        }
        if b != l.buttons[k] {
            l.buttons[k] = b;
            show.edited();
        }
        if !open {
            self.light.playback.edit = None;
        }
    }
}

/// One sequence as a card: current and next cue, level, GO and Release.
fn sequence_card(ui: &mut Ui, l: &Lighting, id: u32, st: Option<&SequenceStatus>, level: f32, w: f32, cmds: &mut Vec<LightControl>) {
    let Some(seq) = l.sequence(id) else { return };
    let cue = st.and_then(|x| x.cue).and_then(|c| seq.index_of(c)).map(|i| &seq.cues[i]);
    let next = match cue {
        Some(c) => seq.index_of(c.id).and_then(|i| seq.cues.get(i + 1)),
        None => seq.cues.first(),
    };
    let active = st.is_some_and(|x| x.active);
    egui::Frame::new()
        .fill(theme::BG_RAISED)
        .corner_radius(10.0)
        .stroke(egui::Stroke::new(if active { 1.5 } else { 1.0 }, if active { theme::GREEN } else { theme::BORDER }))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(w - 20.0);
            ui.set_height(SEQUENCE_CARD.y - 20.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let mark = if active { theme::GREEN } else { theme::TEXT_FAINT };
                    ui.label(RichText::new(icons::FIBER_MANUAL_RECORD).font(theme::icon_font(11.0)).color(mark));
                    ui.add(egui::Label::new(RichText::new(l.sequence_label(id).unwrap_or_default()).font(theme::semibold(15.0))).truncate());
                });
                let now_text = cue.map_or_else(|| "—".to_string(), |c| format!("{} {}", c.number, c.name));
                ui.add(egui::Label::new(RichText::new(now_text).color(theme::TEXT)).truncate());
                let next_text = next.map_or_else(|| t("end").to_string(), |c| tr!("next: {cue}", cue = format!("{} {}", c.number, c.name)));
                ui.add(egui::Label::new(RichText::new(next_text).color(theme::TEXT_DIM).small()).truncate());
                let mut v = level;
                if level_bar(ui, &mut v, theme::ACCENT).changed() {
                    cmds.push(LightControl::Level(id, v));
                }
                ui.horizontal(|ui| {
                    let half = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
                    let go = egui::Button::new(RichText::new("GO").font(theme::semibold(16.0))).fill(theme::GO).min_size(egui::vec2(half, 40.0));
                    if ui.add_enabled(next.is_some(), go).clicked() {
                        cmds.push(LightControl::Go(id, None));
                    }
                    if ui.add_enabled(active, egui::Button::new(t("Release")).min_size(egui::vec2(half, 40.0))).clicked() {
                        cmds.push(LightControl::Release(Some(id), 1.0));
                    }
                });
            });
        });
}

/// A change to the button pages asked for this frame.
enum PageEdit {
    Add,
    Rename(u16, String),
    Delete(u16),
    /// A button dropped on a page tab.
    MoveTo(ButtonId, u16),
}

/// Card size factor of the executors (a damaged show file can't make them vanish or explode).
fn button_scale(l: &Lighting) -> f32 {
    if l.button_scale.is_finite() { l.button_scale.clamp(0.6, 2.5) } else { 1.0 }
}

/// The "add an executor" card; true when clicked.
fn add_card(ui: &mut Ui, w: f32, scale: f32) -> bool {
    let (rect, r) = ui.allocate_exact_size(egui::vec2(w, BUTTON_CARD.y * scale), egui::Sense::click());
    let p = ui.painter();
    let stroke = if r.hovered() { theme::ACCENT } else { theme::BORDER };
    p.rect_stroke(rect, 10.0, egui::Stroke::new(1.5, stroke), egui::StrokeKind::Inside);
    p.text(rect.center() - egui::vec2(0.0, 10.0), egui::Align2::CENTER_CENTER, icons::ADD, theme::icon_font(24.0), theme::TEXT_DIM);
    p.text(rect.center() + egui::vec2(0.0, 16.0), egui::Align2::CENTER_CENTER, t("Add executor"), egui::FontId::proportional(13.0), theme::TEXT_DIM);
    r.on_hover_text(t("An executor that puts a preset on chosen fixtures, or stored values (fog, strobe, a special)")).clicked()
}

#[cfg(test)]
mod tests {
    use crate::light::model::{LightButton, Lighting};

    fn buttons(pages: &[u16]) -> Lighting {
        let mut l = Lighting::default();
        for (i, p) in pages.iter().enumerate() {
            l.buttons.push(LightButton { id: i as u32 + 1, page: *p, ..Default::default() });
        }
        l.button_pages = vec!["A".into(), "B".into(), "C".into()];
        l
    }

    fn order(l: &Lighting) -> Vec<(u32, u16)> {
        l.buttons.iter().map(|b| (b.id, b.page)).collect()
    }

    #[test]
    fn buttons_move_and_pages_go() {
        let mut l = buttons(&[0, 0, 0, 1]);
        super::move_button(&mut l, 1, 3);
        assert_eq!(order(&l), [(2, 0), (3, 0), (1, 0), (4, 1)], "forward: into its place");
        super::move_button(&mut l, 1, 2);
        assert_eq!(order(&l), [(1, 0), (2, 0), (3, 0), (4, 1)], "back");
        super::move_button(&mut l, 4, 2);
        assert_eq!(order(&l), [(1, 0), (4, 0), (2, 0), (3, 0)], "onto another page's button: takes its page");
        let mut l = buttons(&[0, 1, 2, 2]);
        super::delete_page(&mut l, 1);
        assert_eq!(order(&l), [(1, 0), (2, 0), (3, 1), (4, 1)]);
        assert_eq!(l.button_pages, ["A", "C"]);
        super::delete_page(&mut l, 0);
        assert!(l.button_pages.is_empty(), "one page left: no pages");
        assert!(l.buttons.iter().all(|b| b.page == 0));
        assert_eq!(l.page_buttons(0).count(), 4);
    }

    #[test]
    fn grid_fills_the_row() {
        let (cols, w) = super::grid(1000.0, 150.0);
        assert_eq!(cols, 6);
        assert!(cols as f32 * w + (cols - 1) as f32 * super::GAP <= 1000.0);
        assert_eq!(super::grid(50.0, 150.0).0, 1);
    }
}
