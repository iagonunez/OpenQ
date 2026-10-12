//! The cue list: a table with playhead, live status, progress bars and drag & drop.

use crate::control::{Command, CueAction};
use super::{App, Mode, icons, theme};
use super::widgets::{AMBER, BLUE, GREEN, RED, fmt_time, parse_time};
use crate::media::effective_duration;
use crate::model::{ContinueMode, Cue, CueId, CueKind, CueList, GroupMode, KindTag};
use crate::show::Show;
use egui::{Align2, Color32, CursorIcon, FontId, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use std::time::Instant;

const ROW_H: f32 = 30.0;
/// Taller rows in show mode, readable from across a booth.
const ROW_H_SHOW: f32 = 40.0;
const W_MARK: f32 = 18.0;
const W_STATE: f32 = 20.0;
const W_ICON: f32 = 24.0;
const W_NUM: f32 = 54.0;
const W_TARGET: f32 = 170.0;
const W_TIME: f32 = 92.0;
const W_CONT: f32 = 26.0;
const INDENT: f32 = 18.0;

/// A cue-row cell being edited in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InlineField {
    Number,
    Name,
    PreWait,
    PostWait,
}

pub(super) struct InlineEdit {
    pub id: CueId,
    pub field: InlineField,
    pub text: String,
    focused: bool,
}

impl InlineEdit {
    pub fn new(id: CueId, field: InlineField, text: String) -> Self {
        Self { id, field, text, focused: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DropTarget {
    Before(CueId),
    After(CueId),
    Into(CueId),
    End,
}

struct Columns {
    mark: f32,
    #[allow(dead_code)]
    state: f32,
    icon: f32,
    num: f32,
    name: f32,
    target: f32,
    pre: f32,
    action: f32,
    post: f32,
    cont: f32,
    end: f32,
}

impl Columns {
    fn new(left: f32, width: f32) -> Self {
        let fixed = W_MARK + W_STATE + W_ICON + W_NUM + W_TARGET + 3.0 * W_TIME + W_CONT;
        let name_w = (width - fixed).max(120.0);
        let mark = left;
        let state = mark + W_MARK;
        let icon = state + W_STATE;
        let num = icon + W_ICON;
        let name = num + W_NUM;
        let target = name + name_w;
        let pre = target + W_TARGET;
        let action = pre + W_TIME;
        let post = action + W_TIME;
        let cont = post + W_TIME;
        Self { mark, state, icon, num, name, target, pre, action, post, cont, end: cont + W_CONT }
    }
}

impl App {
    /// Cue-type palette (toolbox): icon tiles grouped by kind, plus edit actions.
    pub(super) fn toolbar(&mut self, ui: &mut Ui, show: &mut Show) {
        use KindTag::*;
        let groups: [&[KindTag]; 4] = [
            &[Audio, Mic, Video, Camera, Text],
            &[Group, Wait, Memo],
            &[Fade, Start, Stop, Pause, Goto, Devamp],
            &[Network],
        ];
        // Room kept for the edit buttons on the right; the palette scrolls if the window is narrow.
        const EDIT_BUTTONS_W: f32 = 200.0;
        ui.horizontal(|ui| {
            let palette_w = (ui.available_width() - EDIT_BUTTONS_W).max(120.0);
            egui::ScrollArea::horizontal().id_salt("palette").max_width(palette_w).scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (gi, group) in groups.iter().enumerate() {
                        if gi > 0 {
                            ui.add_space(5.0);
                            let (r, _) = ui.allocate_exact_size(vec2(1.0, 40.0), Sense::hover());
                            ui.painter().rect_filled(r, 0.0, theme::BORDER);
                            ui.add_space(5.0);
                        }
                        for tag in *group {
                            let i = KindTag::ALL.iter().position(|t| t == tag).unwrap_or(99);
                            let key = if i < 10 { format!("  ·  Ctrl+{}", (i + 1) % 10) } else { String::new() };
                            if tile(ui, theme::kind_icon(*tag), tag.label())
                                .on_hover_text(format!("New {} cue{key}", tag.label()))
                                .clicked()
                            {
                                self.add_cue(show, *tag);
                            }
                        }
                    }
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(ui, icons::DELETE, "Delete (Del)").clicked() {
                    self.delete_selected(show);
                }
                if theme::icon_button(ui, icons::CONTENT_COPY, "Duplicate (Ctrl+D)").clicked() {
                    self.duplicate_selected(show);
                }
                if theme::icon_button(ui, icons::CREATE_NEW_FOLDER, "Group selected (Ctrl+G)").clicked() {
                    self.group_selected(show);
                }
                ui.add_space(8.0);
                if theme::icon_button(ui, icons::REDO, "Redo (Ctrl+Shift+Z)").clicked() {
                    self.redo(show);
                }
                if theme::icon_button(ui, icons::UNDO, "Undo (Ctrl+Z)").clicked() {
                    self.undo(show);
                }
            });
        });
    }

    /// Tabs for the workspace's cue lists: click to bring one forward; in edit mode, + adds a list
    /// and right-click renames, moves or deletes one.
    pub(super) fn list_tabs(&mut self, ui: &mut Ui, show: &mut Show) {
        let (names, current) = show.ws.cue_list.list_names();
        let editing = self.mode == super::Mode::Edit && self.can_edit_show();
        let mut switch = None;
        let mut edited = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (i, name) in names.iter().enumerate() {
                if editing && i == current && let Some(text) = &mut self.list_rename {
                    let r = ui.add(egui::TextEdit::singleline(text).desired_width(160.0));
                    if !r.has_focus() && !r.lost_focus() {
                        r.request_focus();
                    }
                    if r.lost_focus() {
                        let t = text.trim().to_string();
                        if !t.is_empty() && !ui.input(|i| i.key_pressed(egui::Key::Escape)) && t != show.ws.cue_list.info.name {
                            show.ws.cue_list.info.name = t;
                            edited = true;
                        }
                        self.list_rename = None;
                    }
                    continue;
                }
                let mut sel = current;
                let r = theme::tab(ui, &mut sel, i, icons::RECEIPT_LONG, name);
                if sel != current {
                    switch = Some(i);
                }
                if editing {
                    if r.double_clicked() && i == current {
                        self.list_rename = Some(name.clone());
                    }
                    r.context_menu(|ui| {
                        if i != current {
                            if ui.button("Bring forward").clicked() {
                                switch = Some(i);
                            }
                            return;
                        }
                        if ui.button(theme::with_icon(icons::EDIT, "Rename")).clicked() {
                            self.list_rename = Some(name.clone());
                        }
                        if ui.button(theme::with_icon(icons::TIMER_PLAY, "Timecode…")).clicked() {
                            self.show_list_timecode = true;
                        }
                        ui.add_enabled_ui(i > 0, |ui| {
                            if ui.button("Move left").clicked() {
                                show.ws.cue_list.move_current_list(-1);
                                edited = true;
                            }
                        });
                        ui.add_enabled_ui(i + 1 < names.len(), |ui| {
                            if ui.button("Move right").clicked() {
                                show.ws.cue_list.move_current_list(1);
                                edited = true;
                            }
                        });
                        ui.separator();
                        let n = show.ws.cue_list.cues.len();
                        ui.add_enabled_ui(names.len() > 1, |ui| {
                            let label = if n == 0 { "Delete list".to_string() } else { format!("Delete list and its {n} cues") };
                            if ui.button(theme::with_icon(icons::DELETE, &label)).clicked() {
                                self.delete_current_list(show);
                            }
                        });
                    });
                }
            }
            if editing && theme::icon_button(ui, icons::ADD, "New cue list").clicked() {
                let name = format!("Cue List {}", names.len() + 1);
                show.ws.cue_list.add_list(&name, show.engine.playhead);
                self.selection.clear();
                self.control(show, Command::SetPlayhead(None));
                edited = true;
            }
        });
        if edited {
            show.edited();
        }
        if let Some(tab) = switch {
            self.selection.clear();
            self.list_rename = None;
            self.control(show, Command::SwitchList(tab));
        }
    }

    /// The current cue list's timecode input: source, rate and device, with what it receives.
    pub(super) fn list_timecode_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        use crate::model::{TimecodeRate, TimecodeSource};
        if !self.show_list_timecode {
            return;
        }
        super::devices::ensure(&self.inputs, ctx);
        let devs = self.inputs.lock().clone();
        let mut open = true;
        let mut sync = show.ws.cue_list.info.timecode.clone();
        let name = show.ws.cue_list.info.name.clone();
        let mut refresh = false;
        egui::Window::new(format!("Timecode · {name}")).open(&mut open).resizable(false).default_width(420.0).show(ctx, |ui| {
            ui.add_enabled_ui(self.can_edit_show(), |ui| {
                egui::Grid::new("list-tc").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                    ui.label("Follow");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut sync.source, TimecodeSource::Off, "Off");
                        ui.selectable_value(&mut sync.source, TimecodeSource::Ltc, "LTC (audio in)");
                        ui.selectable_value(&mut sync.source, TimecodeSource::Mtc, "MTC (MIDI in)");
                    });
                    ui.end_row();
                    ui.label("Rate");
                    egui::ComboBox::from_id_salt("list-tc-rate").selected_text(sync.rate.label()).show_ui(ui, |ui| {
                        for r in TimecodeRate::ALL {
                            ui.selectable_value(&mut sync.rate, r, r.label());
                        }
                    });
                    ui.end_row();
                    match sync.source {
                        TimecodeSource::Off => {}
                        TimecodeSource::Ltc => {
                            ui.label("Input");
                            ui.horizontal(|ui| {
                                let label = sync.ltc_device.as_ref().map_or("System default input".to_string(), |d| {
                                    devs.audio_in.iter().find(|x| &x.id == d).map_or(d.clone(), |x| x.name.clone())
                                });
                                egui::ComboBox::from_id_salt("list-tc-dev").width(240.0).selected_text(label).show_ui(ui, |ui| {
                                    ui.selectable_value(&mut sync.ltc_device, None, "System default input");
                                    for d in &devs.audio_in {
                                        ui.selectable_value(&mut sync.ltc_device, Some(d.id.clone()), &d.name);
                                    }
                                });
                                if theme::icon_button(ui, icons::REFRESH, "Look for devices again").clicked() {
                                    refresh = true;
                                }
                            });
                            ui.end_row();
                            ui.label("Channel");
                            let mut ch = sync.ltc_channel + 1;
                            ui.add(egui::DragValue::new(&mut ch).range(1..=64));
                            sync.ltc_channel = ch - 1;
                            ui.end_row();
                        }
                        TimecodeSource::Mtc => {
                            ui.label("MIDI port");
                            ui.horizontal(|ui| {
                                let label = sync.mtc_port.clone().unwrap_or_else(|| "First MIDI input".into());
                                egui::ComboBox::from_id_salt("list-tc-port").width(240.0).selected_text(label).show_ui(ui, |ui| {
                                    ui.selectable_value(&mut sync.mtc_port, None, "First MIDI input");
                                    for p in &devs.midi_in {
                                        ui.selectable_value(&mut sync.mtc_port, Some(p.clone()), p);
                                    }
                                });
                                if theme::icon_button(ui, icons::REFRESH, "Look for devices again").clicked() {
                                    refresh = true;
                                }
                            });
                            ui.end_row();
                        }
                    }
                    if sync.source != TimecodeSource::Off {
                        ui.label("Receiving");
                        let now = Instant::now();
                        match (show.timecode.position(&sync, now), show.timecode.error(&sync)) {
                            (Some(r), _) => {
                                ui.label(
                                    RichText::new(crate::timecode::format_secs(r.secs, r.rate))
                                        .font(theme::mono(16.0))
                                        .color(GREEN),
                                );
                                ui.ctx().request_repaint_after(std::time::Duration::from_millis(40));
                            }
                            (None, Some(e)) => {
                                ui.label(RichText::new(e).color(RED));
                            }
                            (None, None) => {
                                ui.label(RichText::new("no signal").color(AMBER));
                                ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                            }
                        }
                        ui.end_row();
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Cues in this list with a timecode trigger (Basics tab) fire as the timecode runs past their time. \
                     Jumps and dropouts don't fire cues.",
                )
                .weak()
                .small(),
            );
        });
        if refresh {
            super::devices::refresh(&self.inputs, ctx);
        }
        if sync != show.ws.cue_list.info.timecode && self.can_edit_show() {
            show.ws.cue_list.info.timecode = sync;
            show.edited();
        }
        self.show_list_timecode = open;
    }

    fn delete_current_list(&mut self, show: &mut Show) {
        // Stop what is running from the list before its cues go.
        let doomed: Vec<CueId> = show.ws.cue_list.cues.iter().map(|c| c.id).collect();
        for id in doomed {
            if show.engine.instances.contains_key(&id) {
                self.control(show, Command::Cue(id, crate::control::CueAction::Stop));
            }
        }
        if let Some((_, ph)) = show.ws.cue_list.remove_current_list() {
            self.selection.clear();
            show.edited();
            let ph = ph.or(show.ws.cue_list.first());
            self.control(show, Command::SetPlayhead(ph));
        }
    }

    pub(super) fn cue_list(&mut self, ui: &mut Ui, show: &mut Show) {
        let now = Instant::now();
        let collapsed = self.collapsed.clone();
        let rows = show.ws.cue_list.visible_rows(&|id| collapsed.contains(&id));

        // Header.
        let (hdr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
        let cols = Columns::new(hdr.left(), hdr.width());
        let hp = ui.painter();
        let hf = theme::semibold(10.5);
        for (x, t, right) in [
            (cols.num, "#", false),
            (cols.name, "NAME", false),
            (cols.target, "TARGET", false),
            (cols.pre + W_TIME - 8.0, "PRE-WAIT", true),
            (cols.action + W_TIME - 8.0, "ACTION", true),
            (cols.post + W_TIME - 8.0, "POST-WAIT", true),
        ] {
            let align = if right { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER };
            hp.text(pos2(x + if right { 0.0 } else { 4.0 }, hdr.center().y), align, t, hf.clone(), theme::TEXT_FAINT);
        }
        hp.line_segment([hdr.left_bottom(), hdr.right_bottom()], Stroke::new(1.0, theme::BORDER));

        let mut row_rects: Vec<(Rect, CueId, bool)> = Vec::new();
        let mut clicked: Option<(CueId, egui::Modifiers)> = None;
        let mut toggle_collapse: Option<CueId> = None;
        let mut context_action: Option<(CueId, RowAction)> = None;
        let mut list_rect = Rect::NOTHING;
        let scroll_to = self.scroll_to.take();
        let editing = self.mode == Mode::Edit;
        let row_h = if editing { ROW_H } else { ROW_H_SHOW };
        let mut start_inline: Option<InlineEdit> = None;
        let mut commit: Option<InlineEdit> = None;
        let mut cycle_continue: Option<CueId> = None;

        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("cuelist").show(ui, |ui| {
            // The cues in row order, found in one pass (a lookup per row made frames O(n²)).
            let cues = show.ws.cue_list.visible_cues(&|id| collapsed.contains(&id));
            for ((depth, id), &(_, cue)) in rows.iter().zip(&cues) {
                // Rows outside the view only take space: no widget, no painting.
                let rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), row_h));
                list_rect = list_rect.union(rect);
                row_rects.push((rect, *id, cue.is_group()));
                if scroll_to == Some(*id) {
                    ui.scroll_to_rect(rect, None);
                }
                if !ui.is_rect_visible(rect) {
                    ui.advance_cursor_after_rect(rect);
                    continue;
                }
                let sense = if editing { Sense::click_and_drag() } else { Sense::click() };
                let (rect, resp) = ui.allocate_exact_size(rect.size(), sense);
                let cols = Columns::new(rect.left(), rect.width());
                self.paint_row(ui, show, cue, *depth, rect, &cols, now);

                // Collapse triangle for groups.
                if cue.is_group() {
                    let tri = Rect::from_min_size(
                        pos2(cols.name + *depth as f32 * INDENT, rect.top()),
                        vec2(INDENT, row_h),
                    );
                    if ui.interact(tri, ui.id().with(("tri", id)), Sense::click()).clicked() {
                        toggle_collapse = Some(*id);
                    }
                }

                // Edit mode: double-click a cell to edit it in place; click the continue column to
                // cycle do-not-continue → auto-continue → auto-follow.
                if editing {
                    let name_x = cols.name + *depth as f32 * INDENT + if cue.is_group() { INDENT } else { 0.0 };
                    let y = rect.y_range();
                    let cells = [
                        (InlineField::Number, Rect::from_x_y_ranges(cols.num..=cols.name - 2.0, y)),
                        (InlineField::Name, Rect::from_x_y_ranges(name_x..=cols.target - 2.0, y)),
                        (InlineField::PreWait, Rect::from_x_y_ranges(cols.pre..=cols.pre + W_TIME, y)),
                        (InlineField::PostWait, Rect::from_x_y_ranges(cols.post..=cols.post + W_TIME, y)),
                    ];
                    for (field, cell) in cells {
                        let r = ui.interact(cell, ui.id().with(("cell", id, field as u8)), Sense::click());
                        if r.double_clicked() {
                            let follow = field == InlineField::PostWait && cue.continue_mode == ContinueMode::AutoFollow;
                            if !follow {
                                let text = match field {
                                    InlineField::Number => cue.number.clone(),
                                    InlineField::Name => cue.display_name(),
                                    InlineField::PreWait => fmt_time(cue.pre_wait),
                                    InlineField::PostWait => fmt_time(cue.post_wait),
                                };
                                start_inline = Some(InlineEdit { id: *id, field, text, focused: false });
                            }
                        } else if r.clicked() {
                            clicked = Some((*id, ui.input(|i| i.modifiers)));
                        }
                    }
                    let cont = Rect::from_x_y_ranges(cols.cont..=cols.cont + W_CONT, y);
                    let r = ui
                        .interact(cont, ui.id().with(("cont", id)), Sense::click())
                        .on_hover_text("Click to cycle: do not continue → auto-continue → auto-follow");
                    if r.clicked() {
                        cycle_continue = Some(*id);
                    }
                    if r.hovered() && cue.continue_mode == ContinueMode::DoNotContinue {
                        ui.painter().text(cont.center(), Align2::CENTER_CENTER, icons::ARROW_DOWNWARD, theme::icon_font(16.0), theme::TEXT_FAINT);
                    }
                }

                // In-place editor overlay.
                if let Some(ed) = &mut self.inline
                    && ed.id == *id
                {
                    let name_x = cols.name + *depth as f32 * INDENT + if cue.is_group() { INDENT } else { 0.0 };
                    let x = match ed.field {
                        InlineField::Number => cols.num..=cols.name - 2.0,
                        InlineField::Name => name_x..=cols.target - 2.0,
                        InlineField::PreWait => cols.pre..=cols.pre + W_TIME,
                        InlineField::PostWait => cols.post..=cols.post + W_TIME,
                    };
                    let cell = Rect::from_x_y_ranges(x, rect.y_range()).shrink2(vec2(1.0, 2.0));
                    let te = ui.put(cell, egui::TextEdit::singleline(&mut ed.text).margin(vec2(3.0, 1.0)));
                    let mut done = false;
                    if !ed.focused {
                        te.request_focus();
                        ed.focused = true;
                    } else if te.lost_focus() {
                        done = true;
                    }
                    if done {
                        commit = self.inline.take();
                    }
                }

                if resp.clicked() {
                    clicked = Some((*id, ui.input(|i| i.modifiers)));
                }
                if resp.double_clicked() && cue.is_group() {
                    toggle_collapse = Some(*id);
                }
                if editing && resp.drag_started() {
                    let ids = if self.selection.contains(id) {
                        rows.iter().map(|r| r.1).filter(|r| self.selection.contains(r)).collect()
                    } else {
                        vec![*id]
                    };
                    self.drag = Some(ids);
                }
                resp.context_menu(|ui| {
                    if let Some(a) = row_context_menu(ui, cue, editing) {
                        context_action = Some((*id, a));
                    }
                });
            }
            // Empty space below the rows: click clears selection, drop appends.
            let rest = ui.available_rect_before_wrap();
            if rest.height() > 0.0 {
                let r = ui.allocate_rect(rest, Sense::click());
                if r.clicked() {
                    self.selection.clear();
                }
                if rows.is_empty() {
                    ui.painter().text(
                        rest.center_top() + vec2(0.0, 40.0),
                        Align2::CENTER_CENTER,
                        "Empty cue list — add cues from the toolbar or drop media files here",
                        FontId::proportional(15.0),
                        ui.visuals().weak_text_color(),
                    );
                }
            }
        });

        if let Some(id) = toggle_collapse
            && !self.collapsed.remove(&id)
        {
            self.collapsed.insert(id);
        }

        if let Some(ed) = start_inline {
            self.inline = Some(ed);
        }
        if let Some(ed) = commit
            && let Some(c) = show.ws.cue_list.find_mut(ed.id)
        {
            let before = c.clone();
            match ed.field {
                InlineField::Number => c.number = ed.text.trim().to_string(),
                InlineField::Name => {
                    // Keep the automatic name (file name) unless it was actually changed.
                    if ed.text != c.display_name() {
                        c.name = ed.text.trim().to_string();
                    }
                }
                InlineField::PreWait => {
                    if let Some(t) = parse_time(&ed.text) {
                        c.pre_wait = t;
                    }
                }
                InlineField::PostWait => {
                    if let Some(t) = parse_time(&ed.text) {
                        c.post_wait = t;
                    }
                }
            }
            if *c != before {
                show.edited();
            }
        }
        if let Some(id) = cycle_continue
            && let Some(c) = show.ws.cue_list.find_mut(id)
        {
            c.continue_mode = match c.continue_mode {
                ContinueMode::DoNotContinue => ContinueMode::AutoContinue,
                ContinueMode::AutoContinue => ContinueMode::AutoFollow,
                ContinueMode::AutoFollow => ContinueMode::DoNotContinue,
            };
            show.edited();
        }

        if let Some((id, mods)) = clicked {
            self.click_row(show, id, mods, &rows);
        }

        if let Some((id, action)) = context_action {
            self.run_row_action(show, id, action);
        }

        // Drag & drop.
        if let Some(dragging) = self.drag.clone() {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            let pointer = ui.input(|i| i.pointer.interact_pos());
            let target = pointer.and_then(|p| drop_target(p, &row_rects, list_rect));
            if let Some(t) = target {
                paint_drop_indicator(ui, t, &row_rects);
            }
            if ui.input(|i| i.pointer.any_released()) {
                self.drag = None;
                if let Some(t) = target {
                    apply_drop(&mut show.ws.cue_list, &dragging, t);
                    show.edited();
                }
            }
        }
    }

    fn click_row(&mut self, show: &mut Show, id: CueId, mods: egui::Modifiers, rows: &[(usize, CueId)]) {
        if mods.command {
            if let Some(i) = self.selection.iter().position(|s| *s == id) {
                self.selection.remove(i);
            } else {
                self.selection.push(id);
            }
            show.engine.set_playhead(Some(id));
            self.last_playhead = Some(id);
        } else if mods.shift
            && let Some(anchor) = self.primary()
        {
            let a = rows.iter().position(|r| r.1 == anchor);
            let b = rows.iter().position(|r| r.1 == id);
            if let (Some(a), Some(b)) = (a, b) {
                let (lo, hi) = (a.min(b), a.max(b));
                let mut sel: Vec<CueId> = rows[lo..=hi].iter().map(|r| r.1).filter(|r| *r != id).collect();
                sel.push(id);
                self.selection = sel;
                show.engine.set_playhead(Some(id));
                self.last_playhead = Some(id);
            }
        } else {
            self.select(show, Some(id));
        }
    }

    fn run_row_action(&mut self, show: &mut Show, id: CueId, action: RowAction) {
        match action {
            RowAction::SetPlayhead => self.select(show, Some(id)),
            RowAction::Start => {
                self.control(show, Command::Cue(id, CueAction::Start));
            }
            RowAction::Stop => {
                self.control(show, Command::Cue(id, CueAction::Stop));
            }
            RowAction::Duplicate => {
                if !self.selection.contains(&id) {
                    self.selection = vec![id];
                }
                self.duplicate_selected(show);
            }
            RowAction::Delete => {
                if !self.selection.contains(&id) {
                    self.selection = vec![id];
                }
                self.delete_selected(show);
            }
            RowAction::Group => {
                if !self.selection.contains(&id) {
                    self.selection = vec![id];
                }
                self.group_selected(show);
            }
            RowAction::ToggleArmed => {
                if let Some(c) = show.ws.cue_list.find_mut(id) {
                    c.armed = !c.armed;
                    show.edited();
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_row(&self, ui: &Ui, show: &Show, cue: &Cue, depth: usize, rect: Rect, cols: &Columns, now: Instant) {
        let p = ui.painter();
        let id = cue.id;
        let selected = self.selection.contains(&id);
        let is_playhead = show.engine.playhead == Some(id);
        let inst = show.engine.instances.get(&id);
        let big = self.mode == Mode::Show;
        let cy = rect.center().y;

        // Background: zebra, colour tag, running tint, selection, playhead.
        let row = rect.shrink2(vec2(0.0, 1.0));
        let zebra = (rect.top() / rect.height()) as i64 % 2 == 0;
        let mut bg = if zebra { Color32::from_rgb(0x18, 0x19, 0x1d) } else { theme::BG_APP };
        if let Some([r, g, b]) = cue.color.rgb() {
            bg = Color32::from_rgb(r / 5 + bg.r() / 2, g / 5 + bg.g() / 2, b / 5 + bg.b() / 2);
        }
        if inst.is_some() {
            bg = Color32::from_rgb(0x1a, 0x2e, 0x22);
        }
        if selected {
            bg = Color32::from_rgb(0x22, 0x33, 0x58);
        }
        p.rect_filled(row, 5.0, bg);
        if let Some([r, g, b]) = cue.color.rgb() {
            p.rect_filled(Rect::from_min_size(row.min, vec2(4.0, row.height())), 2.0, Color32::from_rgb(r, g, b));
        }
        if is_playhead {
            p.rect_stroke(row, 5.0, Stroke::new(1.5, theme::GREEN), StrokeKind::Inside);
        } else if selected {
            p.rect_stroke(row, 5.0, Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.6)), StrokeKind::Inside);
        }

        let text_color = if cue.armed { theme::TEXT } else { theme::TEXT_FAINT };
        let font = if big { theme::semibold(18.0) } else { FontId::proportional(14.0) };
        let mono = theme::mono(if big { 15.0 } else { 12.5 });
        let icon_size = if big { 22.0 } else { 18.0 };

        // Status column: playhead / live state / problems.
        let media_path = cue.kind.media_path().filter(|p| !p.as_os_str().is_empty());
        let probe = media_path.and_then(|path| self.prober.get(path));
        let broken = match (&cue.kind, media_path) {
            (CueKind::Audio(_) | CueKind::Video(_), None) => true,
            (_, Some(path)) => !self.prober.exists(path) || matches!(probe, Some(Err(_))),
            _ => cue.kind.target_missing(&show.ws),
        };
        let state = match inst {
            Some(i) if i.is_paused() => Some((icons::PAUSE, AMBER)),
            Some(i) if i.in_pre_wait(now) => Some((icons::SCHEDULE, AMBER)),
            Some(_) => Some((icons::PLAY_ARROW, theme::GREEN)),
            None if !cue.armed => Some((icons::BLOCK, theme::TEXT_FAINT)),
            None if broken => Some((icons::ERROR, RED)),
            None if is_playhead => Some((icons::ARROW_RIGHT, theme::GREEN)),
            None => None,
        };
        if let Some((g, c)) = state {
            let font = if matches!(g, s if s == icons::PLAY_ARROW || s == icons::PAUSE || s == icons::ARROW_RIGHT) {
                theme::icon_font_filled(icon_size)
            } else {
                theme::icon_font(icon_size)
            };
            p.text(pos2(cols.mark + (W_MARK + W_STATE) / 2.0, cy), Align2::CENTER_CENTER, g, font, c);
        }

        p.text(
            pos2(cols.icon + W_ICON / 2.0, cy),
            Align2::CENTER_CENTER,
            theme::kind_icon(cue.kind.tag()),
            theme::icon_font(icon_size),
            if cue.armed { theme::TEXT_DIM } else { theme::TEXT_FAINT },
        );
        let clip = |l: f32, r: f32| p.with_clip_rect(Rect::from_x_y_ranges(l..=r - 4.0, rect.y_range()));
        clip(cols.num, cols.name).text(pos2(cols.num + 4.0, cy), Align2::LEFT_CENTER, &cue.number, theme::semibold(if big { 17.0 } else { 13.5 }), text_color);

        // Name with indentation and group chevron.
        let mut x = cols.name + depth as f32 * INDENT;
        if let CueKind::Group { mode, .. } = &cue.kind {
            let chev = if self.collapsed.contains(&id) { icons::CHEVRON_RIGHT } else { icons::EXPAND_MORE };
            p.text(pos2(x + INDENT / 2.0, cy), Align2::CENTER_CENTER, chev, theme::icon_font(icon_size), theme::TEXT_DIM);
            x += INDENT;
            let tag = match mode {
                GroupMode::StartFirstAndEnter => "enter",
                GroupMode::StartFirst => "playlist",
                GroupMode::StartAll => "all",
                GroupMode::StartRandom => "random",
            };
            let name_end = clip(x, cols.target).text(pos2(x + 2.0, cy), Align2::LEFT_CENTER, cue.display_name(), font.clone(), text_color).right();
            // Mode pill.
            let galley = p.layout_no_wrap(tag.to_string(), theme::semibold(9.5), theme::TEXT_DIM);
            let pill = Rect::from_min_size(pos2(name_end + 8.0, cy - 8.0), vec2(galley.size().x + 10.0, 16.0));
            if pill.right() < cols.target - 4.0 {
                p.rect_filled(pill, 8.0, theme::BG_HOVER);
                p.galley(pill.min + vec2(5.0, (16.0 - galley.size().y) / 2.0), galley, theme::TEXT_DIM);
            }
        } else {
            clip(x, cols.target).text(pos2(x + 2.0, cy), Align2::LEFT_CENTER, cue.display_name(), font.clone(), text_color);
        }
        if let Some(k) = &cue.hotkey {
            let galley = p.layout_no_wrap(k.clone(), theme::semibold(9.5), theme::TEXT_DIM);
            let pill = Rect::from_min_size(pos2(cols.target - galley.size().x - 18.0, cy - 8.0), vec2(galley.size().x + 10.0, 16.0));
            p.rect_stroke(pill, 4.0, Stroke::new(1.0, theme::BORDER), StrokeKind::Inside);
            p.galley(pill.min + vec2(5.0, (16.0 - galley.size().y) / 2.0), galley, theme::TEXT_DIM);
        }

        // Target column.
        let target_text = match &cue.kind {
            CueKind::Audio(_) | CueKind::Video(_) => match &probe {
                Some(Ok(info)) => info.detail.clone(),
                Some(Err(_)) => "unreadable".into(),
                None if media_path.is_none() => "no file".into(),
                None => "…".into(),
            },
            CueKind::Network(n) => format!("{}:{} {}", n.host, n.port, n.address),
            CueKind::Text(t) => format!("“{}”", t.text.lines().next().unwrap_or("")),
            CueKind::Mic(m) => {
                let ch = match m.channels {
                    1 => format!("in {}", m.first_channel + 1),
                    n => format!("in {}–{}", m.first_channel + 1, m.first_channel + n),
                };
                let dev = m.device.as_ref().map_or("default input".to_string(), |d| {
                    self.inputs.lock().audio_in.iter().find(|x| &x.id == d).map_or(d.clone(), |x| x.name.clone())
                });
                format!("{dev} · {ch}")
            }
            CueKind::Camera(c) => match &c.device {
                None => "first camera".into(),
                Some(d) => self.inputs.lock().cameras.iter().find(|x| &x.0 == d).map_or(d.clone(), |x| x.1.clone()),
            },
            k => match k.target() {
                Some(t) => match show.ws.cue_list.find(t) {
                    Some(tc) => format!("{} {}", tc.number, tc.display_name()),
                    None => "missing target".into(),
                },
                None if matches!(k, CueKind::Stop { .. } | CueKind::Pause { .. }) => "all cues".into(),
                None if k.needs_target() => "no target".into(),
                None => String::new(),
            },
        };
        clip(cols.target, cols.pre).text(
            pos2(cols.target + 4.0, cy),
            Align2::LEFT_CENTER,
            target_text,
            FontId::proportional(12.0),
            if broken { RED } else { theme::TEXT_DIM },
        );

        // Time columns.
        let action_dur = match &cue.kind {
            CueKind::Wait { duration } => Some(*duration),
            CueKind::Fade(f) => Some(f.duration),
            CueKind::Audio(_) | CueKind::Video(_) => {
                effective_duration(cue, probe.as_ref().and_then(|r| r.as_ref().ok()).and_then(|i| i.duration))
            }
            _ => None,
        };
        let cell = |x: f32| Rect::from_min_max(pos2(x + 4.0, cy - 9.0), pos2(x + W_TIME - 4.0, cy + 9.0));
        let dim = theme::TEXT_FAINT;
        let time_text = |x: f32, t: f64, color: Color32| {
            p.text(pos2(x + W_TIME - 8.0, cy), Align2::RIGHT_CENTER, fmt_time(t), mono.clone(), color);
        };
        let bar = |x: f32, frac: f32, label: String, color: Color32| {
            let r = cell(x);
            p.rect_filled(r, 4.0, color.gamma_multiply(0.18));
            let mut f = r;
            f.set_width(r.width() * frac.clamp(0.0, 1.0));
            p.rect_filled(f, 4.0, color.gamma_multiply(0.75));
            p.text(pos2(r.right() - 4.0, cy), Align2::RIGHT_CENTER, label, mono.clone(), Color32::WHITE);
        };

        match inst {
            Some(i) if i.in_pre_wait(now) && i.pre_wait > 0.0 => {
                let e = i.pre_wait_elapsed(now);
                bar(cols.pre, (e / i.pre_wait) as f32, fmt_time(i.pre_wait - e), AMBER);
            }
            _ => time_text(cols.pre, cue.pre_wait, if cue.pre_wait > 0.0 { text_color } else { dim }),
        }

        match inst {
            Some(i) if !i.in_pre_wait(now) && !i.action_done => {
                let e = match cue.kind {
                    CueKind::Audio(_) | CueKind::Video(_) => show.media.elapsed(id).unwrap_or(i.action_elapsed(now)),
                    _ => i.action_elapsed(now),
                };
                let color = if i.is_paused() { AMBER } else { theme::GREEN };
                match i.action_duration.or(action_dur) {
                    Some(d) if d > 0.0 => bar(cols.action, (e / d) as f32, format!("-{}", fmt_time((d - e).max(0.0))), color),
                    _ => bar(cols.action, 1.0, fmt_time(e), if i.is_paused() { AMBER } else { BLUE }),
                }
            }
            _ => {
                if let Some(d) = action_dur {
                    time_text(cols.action, d, text_color);
                } else if matches!(cue.kind, CueKind::Audio(ref a) if a.infinite_loop)
                    || matches!(cue.kind, CueKind::Video(ref v) if v.infinite_loop)
                {
                    p.text(pos2(cols.action + W_TIME - 8.0, cy), Align2::RIGHT_CENTER, icons::ALL_INCLUSIVE, theme::icon_font(16.0), text_color);
                }
            }
        }

        match cue.continue_mode {
            ContinueMode::AutoContinue => match inst {
                Some(i) if !i.in_pre_wait(now) && !i.continue_done && cue.post_wait > 0.0 => {
                    let e = i.action_elapsed(now);
                    bar(cols.post, (e / cue.post_wait) as f32, format!("-{}", fmt_time((cue.post_wait - e).max(0.0))), BLUE);
                }
                _ => time_text(cols.post, cue.post_wait, text_color),
            },
            ContinueMode::AutoFollow => {
                if let Some(d) = action_dur {
                    time_text(cols.post, d, dim);
                }
            }
            ContinueMode::DoNotContinue => {
                if cue.post_wait > 0.0 {
                    time_text(cols.post, cue.post_wait, dim);
                }
            }
        }

        let cont = match cue.continue_mode {
            ContinueMode::DoNotContinue => None,
            ContinueMode::AutoContinue => Some(icons::ARROW_DOWNWARD),
            ContinueMode::AutoFollow => Some(icons::KEYBOARD_DOUBLE_ARROW_DOWN),
        };
        if let Some(g) = cont {
            p.text(pos2(cols.cont + W_CONT / 2.0, cy), Align2::CENTER_CENTER, g, theme::icon_font(icon_size), BLUE);
        }
        let _ = cols.end;
    }
}

/// Toolbox tile: icon over a small label.
fn tile(ui: &mut Ui, glyph: &str, label: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(54.0, 46.0), Sense::click());
    let c = theme::tool_highlight(ui, rect, &resp, 8.0, theme::TEXT_DIM);
    let p = ui.painter();
    p.text(rect.center_top() + vec2(0.0, 15.0), Align2::CENTER_CENTER, glyph, theme::icon_font(22.0), c);
    p.text(rect.center_bottom() - vec2(0.0, 10.0), Align2::CENTER_CENTER, label, FontId::proportional(10.5), c);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[derive(Clone, Copy)]
enum RowAction {
    SetPlayhead,
    Start,
    Stop,
    Duplicate,
    Delete,
    Group,
    ToggleArmed,
}

fn row_context_menu(ui: &mut Ui, cue: &Cue, editing: bool) -> Option<RowAction> {
    let mut a = None;
    ui.label(RichText::new(format!("{} {}", cue.number, cue.display_name())).strong());
    ui.separator();
    if ui.button(theme::with_icon(icons::ARROW_RIGHT, "Set playhead here")).clicked() {
        a = Some(RowAction::SetPlayhead);
    }
    if ui.button(theme::with_icon(icons::PLAY_ARROW, "Start this cue")).clicked() {
        a = Some(RowAction::Start);
    }
    if ui.button(theme::with_icon(icons::STOP, "Stop this cue")).clicked() {
        a = Some(RowAction::Stop);
    }
    if !editing {
        if a.is_some() {
            ui.close();
        }
        return a;
    }
    ui.separator();
    if ui.button(if cue.armed { theme::with_icon(icons::BLOCK, "Disarm") } else { theme::with_icon(icons::CHECK_CIRCLE, "Arm") }).clicked() {
        a = Some(RowAction::ToggleArmed);
    }
    if ui.button(theme::with_icon(icons::CONTENT_COPY, "Duplicate")).clicked() {
        a = Some(RowAction::Duplicate);
    }
    if ui.button(theme::with_icon(icons::CREATE_NEW_FOLDER, "Group selected")).clicked() {
        a = Some(RowAction::Group);
    }
    if ui.button(theme::with_icon(icons::DELETE, "Delete")).clicked() {
        a = Some(RowAction::Delete);
    }
    if a.is_some() {
        ui.close();
    }
    a
}

fn drop_target(p: egui::Pos2, rows: &[(Rect, CueId, bool)], list_rect: Rect) -> Option<DropTarget> {
    for (r, id, is_group) in rows {
        if p.y >= r.top() && p.y < r.bottom() {
            let rel = (p.y - r.top()) / r.height();
            return Some(if *is_group {
                if rel < 0.25 {
                    DropTarget::Before(*id)
                } else if rel > 0.75 {
                    DropTarget::After(*id)
                } else {
                    DropTarget::Into(*id)
                }
            } else if rel < 0.5 {
                DropTarget::Before(*id)
            } else {
                DropTarget::After(*id)
            });
        }
    }
    if list_rect.is_positive() && p.y >= list_rect.bottom() || rows.is_empty() {
        return Some(DropTarget::End);
    }
    None
}

fn paint_drop_indicator(ui: &Ui, t: DropTarget, rows: &[(Rect, CueId, bool)]) {
    let p = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("dnd")));
    let stroke = Stroke::new(2.0, GREEN);
    let find = |id: CueId| rows.iter().find(|r| r.1 == id).map(|r| r.0);
    match t {
        DropTarget::Before(id) => {
            if let Some(r) = find(id) {
                p.line_segment([r.left_top(), r.right_top()], stroke);
            }
        }
        DropTarget::After(id) => {
            if let Some(r) = find(id) {
                p.line_segment([r.left_bottom(), r.right_bottom()], stroke);
            }
        }
        DropTarget::Into(id) => {
            if let Some(r) = find(id) {
                p.rect_stroke(r, 3.0, stroke, StrokeKind::Inside);
            }
        }
        DropTarget::End => {
            if let Some((r, _, _)) = rows.last() {
                p.line_segment([r.left_bottom(), r.right_bottom()], stroke);
            }
        }
    }
}

fn apply_drop(cues: &mut CueList, dragging: &[CueId], target: DropTarget) {
    let list = &*cues;
    // Drop cues whose ancestor is also being dragged (they move with it).
    let ids: Vec<CueId> = dragging
        .iter()
        .copied()
        .filter(|id| !dragging.iter().any(|o| o != id && list.is_descendant_or_self(*id, *o)))
        .collect();
    let target_id = match target {
        DropTarget::Before(t) | DropTarget::After(t) | DropTarget::Into(t) => Some(t),
        DropTarget::End => None,
    };
    if let Some(t) = target_id
        && ids.iter().any(|d| list.is_descendant_or_self(t, *d))
    {
        return;
    }
    let (parent, anchor) = match target {
        DropTarget::Before(t) => (list.parent_of(t), Some(t)),
        DropTarget::After(t) => {
            let mut a = list.next_sibling(t);
            while let Some(x) = a
                && ids.contains(&x)
            {
                a = list.next_sibling(x);
            }
            (list.parent_of(t), a)
        }
        DropTarget::Into(g) => (Some(g), None),
        DropTarget::End => (None, None),
    };
    for id in ids {
        let Some(cue) = cues.remove(id) else { continue };
        let idx = anchor
            .and_then(|a| cues.location(a))
            .map(|l| l.index)
            .unwrap_or_else(|| cues.list(parent).len());
        cues.insert(parent, idx, cue);
    }
}

trait TargetExt {
    fn needs_target(&self) -> bool;
    fn target_missing(&self, ws: &crate::model::Workspace) -> bool;
}

impl TargetExt for CueKind {
    /// Whether this kind of cue takes a target at all.
    fn needs_target(&self) -> bool {
        matches!(self, CueKind::Fade(_) | CueKind::Start { .. } | CueKind::Goto { .. } | CueKind::Devamp { .. })
    }

    /// A cue that needs a target but has none (or it was deleted).
    fn target_missing(&self, ws: &crate::model::Workspace) -> bool {
        match self.target() {
            Some(t) => ws.cue_list.find(t).is_none(),
            None => self.needs_target(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memo() -> Cue {
        Cue::new(CueKind::Memo)
    }

    fn ids(l: &[Cue]) -> Vec<CueId> {
        l.iter().map(|c| c.id).collect()
    }

    #[test]
    fn multi_drop_after_keeps_order() {
        let (a, b, c, d) = (memo(), memo(), memo(), memo());
        let (ai, bi, ci, di) = (a.id, b.id, c.id, d.id);
        let mut l = CueList { cues: vec![a, b, c, d], ..Default::default() };
        apply_drop(&mut l, &[ai, bi], DropTarget::After(ci));
        assert_eq!(ids(&l.cues), vec![ci, ai, bi, di]);
        apply_drop(&mut l, &[di], DropTarget::Before(ci));
        assert_eq!(ids(&l.cues), vec![di, ci, ai, bi]);
        // Into a group, then out to the end.
        let g = Cue::new(CueKind::Group { mode: GroupMode::StartAll, children: vec![] });
        let gi = g.id;
        l.cues.push(g);
        apply_drop(&mut l, &[ai, bi], DropTarget::Into(gi));
        assert_eq!(ids(l.list(Some(gi))), vec![ai, bi]);
        apply_drop(&mut l, &[gi], DropTarget::Into(gi));
        assert_eq!(l.parent_of(gi), None);
        apply_drop(&mut l, &[ai], DropTarget::End);
        assert_eq!(l.cues.last().unwrap().id, ai);
    }
}
