//! The cue list: a table with playhead, live status, progress bars and drag & drop.

use super::{App, Mode};
use super::widgets::{AMBER, BLUE, GREEN, RED, fmt_time, paint_progress, parse_time};
use crate::media::effective_duration;
use crate::model::{ContinueMode, Cue, CueId, CueKind, CueList, GroupMode, KindTag};
use crate::show::Show;
use egui::{Align2, Color32, CursorIcon, FontId, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use std::time::Instant;

const ROW_H: f32 = 26.0;
/// Taller rows in show mode, readable from across a booth.
const ROW_H_SHOW: f32 = 34.0;
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
    pub(super) fn toolbar(&mut self, ui: &mut Ui, show: &mut Show) {
        ui.horizontal_wrapped(|ui| {
            for (i, tag) in KindTag::ALL.iter().enumerate() {
                let key = if i < 10 { format!(" (Ctrl+{})", (i + 1) % 10) } else { String::new() };
                if ui
                    .button(format!("{} {}", tag.icon(), tag.label()))
                    .on_hover_text(format!("New {} cue{key}", tag.label()))
                    .clicked()
                {
                    self.add_cue(show, *tag);
                }
            }
        });
    }

    pub(super) fn cue_list(&mut self, ui: &mut Ui, show: &mut Show) {
        let now = Instant::now();
        let collapsed = self.collapsed.clone();
        let rows = show.ws.cue_list.visible_rows(&|id| collapsed.contains(&id));

        // Header.
        let (hdr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
        let cols = Columns::new(hdr.left(), hdr.width());
        let hp = ui.painter();
        let hf = FontId::proportional(12.0);
        let hc = ui.visuals().weak_text_color();
        for (x, t) in [
            (cols.num, "#"),
            (cols.name, "Name"),
            (cols.target, "Target"),
            (cols.pre, "Pre-wait"),
            (cols.action, "Action"),
            (cols.post, "Post-wait"),
        ] {
            hp.text(pos2(x + 4.0, hdr.center().y), Align2::LEFT_CENTER, t, hf.clone(), hc);
        }
        hp.line_segment([hdr.left_bottom(), hdr.right_bottom()], ui.visuals().widgets.noninteractive.bg_stroke);

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
            for (depth, id) in &rows {
                let Some(cue) = show.ws.cue_list.find(*id) else { continue };
                let sense = if editing { Sense::click_and_drag() } else { Sense::click() };
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), sense);
                list_rect = list_rect.union(rect);
                row_rects.push((rect, *id, cue.is_group()));
                if scroll_to == Some(*id) {
                    ui.scroll_to_rect(rect, None);
                }
                if !ui.is_rect_visible(rect) {
                    continue;
                }
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
                        ui.painter().text(cont.center(), Align2::CENTER_CENTER, "↓", FontId::proportional(14.0), ui.visuals().weak_text_color());
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
        let now = Instant::now();
        match action {
            RowAction::SetPlayhead => self.select(show, Some(id)),
            RowAction::Start => {
                let Show { ws, engine, media, .. } = show;
                engine.start_cue(ws, media, id, now);
            }
            RowAction::Stop => {
                let Show { ws, engine, media, .. } = show;
                engine.stop_cue(ws, media, id);
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
        let v = ui.visuals();
        let p = ui.painter();
        let id = cue.id;
        let selected = self.selection.contains(&id);
        let is_playhead = show.engine.playhead == Some(id);
        let inst = show.engine.instances.get(&id);

        // Background.
        let mut bg = if selected {
            v.selection.bg_fill
        } else if inst.is_some() {
            Color32::from_rgba_unmultiplied(60, 170, 80, 40)
        } else {
            Color32::TRANSPARENT
        };
        if bg == Color32::TRANSPARENT && (rect.top() / rect.height()) as i64 % 2 == 0 {
            bg = v.faint_bg_color;
        }
        p.rect_filled(rect, 0.0, bg);
        if let Some([r, g, b]) = cue.color.rgb() {
            p.rect_filled(Rect::from_min_size(rect.min, vec2(4.0, rect.height())), 0.0, Color32::from_rgb(r, g, b));
            if !selected {
                p.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(r, g, b, 28));
            }
        }
        if is_playhead {
            p.rect_stroke(rect.shrink(1.0), 2.0, Stroke::new(1.5, GREEN), StrokeKind::Inside);
            p.text(pos2(cols.mark + 9.0, rect.center().y), Align2::CENTER_CENTER, "▶", FontId::proportional(13.0), GREEN);
        }

        let text_color = if cue.armed { v.text_color() } else { v.weak_text_color() };
        let big = self.mode == Mode::Show;
        let font = FontId::proportional(if big { 18.0 } else { 14.0 });
        let mono = FontId::monospace(if big { 15.0 } else { 12.5 });
        let cy = rect.center().y;

        // Live state / broken indicator.
        let media_path = cue.kind.media_path().filter(|p| !p.as_os_str().is_empty());
        let probe = media_path.and_then(|path| self.prober.get(path));
        let broken = match (&cue.kind, media_path) {
            (CueKind::Audio(_) | CueKind::Video(_), None) => true,
            (_, Some(path)) => !path.exists() || matches!(probe, Some(Err(_))),
            _ => cue.kind.target_missing(&show.ws),
        };
        let state = match inst {
            Some(i) if i.is_paused() => Some(("⏸", AMBER)),
            Some(i) if i.in_pre_wait(now) => Some(("◔", AMBER)),
            Some(_) => Some(("▶", GREEN)),
            None if broken => Some(("⚠", RED)),
            None => None,
        };
        if let Some((s, c)) = state {
            p.text(pos2(cols.state + W_STATE / 2.0, cy), Align2::CENTER_CENTER, s, FontId::proportional(13.0), c);
        }
        if !cue.armed {
            p.text(pos2(cols.state + W_STATE / 2.0, cy), Align2::CENTER_CENTER, "⊘", FontId::proportional(13.0), v.weak_text_color());
        }

        p.text(pos2(cols.icon + W_ICON / 2.0, cy), Align2::CENTER_CENTER, cue.kind.tag().icon(), FontId::proportional(14.0), text_color);
        let clip = |l: f32, r: f32| p.with_clip_rect(Rect::from_x_y_ranges(l..=r - 4.0, rect.y_range()));
        clip(cols.num, cols.name).text(pos2(cols.num + 4.0, cy), Align2::LEFT_CENTER, &cue.number, font.clone(), text_color);

        // Name with indentation and group triangle.
        let mut x = cols.name + depth as f32 * INDENT;
        if let CueKind::Group { mode, .. } = &cue.kind {
            let tri = if self.collapsed.contains(&id) { "⏵" } else { "⏷" };
            p.text(pos2(x + INDENT / 2.0, cy), Align2::CENTER_CENTER, tri, FontId::proportional(13.0), text_color);
            x += INDENT;
            let tag = match mode {
                GroupMode::StartFirstAndEnter => "enter",
                GroupMode::StartFirst => "playlist",
                GroupMode::StartAll => "all",
                GroupMode::StartRandom => "random",
            };
            let name_end = clip(x, cols.target)
                .text(pos2(x + 2.0, cy), Align2::LEFT_CENTER, cue.display_name(), font.clone(), text_color)
                .right();
            clip(x, cols.target).text(pos2(name_end + 8.0, cy), Align2::LEFT_CENTER, tag, FontId::proportional(11.0), v.weak_text_color());
        } else {
            clip(x, cols.target).text(pos2(x + 2.0, cy), Align2::LEFT_CENTER, cue.display_name(), font.clone(), text_color);
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
            if broken { RED } else { v.weak_text_color() },
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
        let cell = |x: f32| Rect::from_min_max(pos2(x + 3.0, rect.top() + 5.0), pos2(x + W_TIME - 5.0, rect.bottom() - 5.0));
        let dim = v.weak_text_color();
        let time_text = |x: f32, t: f64, color: Color32| {
            p.text(pos2(x + W_TIME - 8.0, cy), Align2::RIGHT_CENTER, fmt_time(t), mono.clone(), color);
        };
        let bar = |x: f32, frac: f32, label: String, color: Color32| {
            let r = cell(x);
            paint_progress(ui, r, frac, color);
            p.text(pos2(r.right() - 3.0, cy), Align2::RIGHT_CENTER, label, mono.clone(), Color32::WHITE);
        };

        // Pre-wait.
        match inst {
            Some(i) if i.in_pre_wait(now) && i.pre_wait > 0.0 => {
                let e = i.pre_wait_elapsed(now);
                bar(cols.pre, (e / i.pre_wait) as f32, fmt_time(i.pre_wait - e), AMBER);
            }
            _ => time_text(cols.pre, cue.pre_wait, if cue.pre_wait > 0.0 { text_color } else { dim }),
        }

        // Action.
        match inst {
            Some(i) if !i.in_pre_wait(now) && !i.action_done => {
                let e = match cue.kind {
                    CueKind::Audio(_) | CueKind::Video(_) => show.media.elapsed(id).unwrap_or(i.action_elapsed(now)),
                    _ => i.action_elapsed(now),
                };
                let d = i.action_duration.or(action_dur);
                match d {
                    Some(d) if d > 0.0 => bar(cols.action, (e / d) as f32, format!("-{}", fmt_time((d - e).max(0.0))), if i.is_paused() { AMBER } else { GREEN }),
                    _ => bar(cols.action, 1.0, fmt_time(e), if i.is_paused() { AMBER } else { BLUE }),
                }
            }
            _ => {
                if let Some(d) = action_dur {
                    time_text(cols.action, d, text_color);
                } else if matches!(cue.kind, CueKind::Audio(ref a) if a.infinite_loop)
                    || matches!(cue.kind, CueKind::Video(ref v) if v.infinite_loop)
                {
                    p.text(pos2(cols.action + W_TIME - 8.0, cy), Align2::RIGHT_CENTER, "∞", mono.clone(), text_color);
                }
            }
        }

        // Post-wait.
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

        // Continue mode: single arrow = auto-continue, double arrow = auto-follow.
        let arrows = match cue.continue_mode {
            ContinueMode::DoNotContinue => 0,
            ContinueMode::AutoContinue => 1,
            ContinueMode::AutoFollow => 2,
        };
        let stroke = Stroke::new(1.8, BLUE);
        for i in 0..arrows {
            let x = cols.cont + W_CONT / 2.0 + (i as f32 - (arrows as f32 - 1.0) / 2.0) * 6.0;
            let (top, bot) = (cy - 7.0, cy + 7.0);
            p.line_segment([pos2(x, top), pos2(x, bot)], stroke);
            p.line_segment([pos2(x - 3.5, bot - 4.0), pos2(x, bot)], stroke);
            p.line_segment([pos2(x + 3.5, bot - 4.0), pos2(x, bot)], stroke);
        }
        let _ = cols.end;
    }
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
    if ui.button("Set playhead here").clicked() {
        a = Some(RowAction::SetPlayhead);
    }
    if ui.button("▶ Start this cue").clicked() {
        a = Some(RowAction::Start);
    }
    if ui.button("⏹ Stop this cue").clicked() {
        a = Some(RowAction::Stop);
    }
    if !editing {
        if a.is_some() {
            ui.close();
        }
        return a;
    }
    ui.separator();
    if ui.button(if cue.armed { "Disarm" } else { "Arm" }).clicked() {
        a = Some(RowAction::ToggleArmed);
    }
    if ui.button("Duplicate").clicked() {
        a = Some(RowAction::Duplicate);
    }
    if ui.button("Group selected").clicked() {
        a = Some(RowAction::Group);
    }
    if ui.button("Delete").clicked() {
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
        let mut l = CueList { cues: vec![a, b, c, d] };
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
