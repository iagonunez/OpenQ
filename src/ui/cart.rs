//! Cart lists: a cue list shown as a grid of buttons, each firing its own cue (no playhead).
//!
//! Show mode: click a button to fire its cue (a running cue does what its second-trigger setting
//! says), right-click to stop it. Edit mode: click selects (the inspector edits it), double-click
//! fires, drag moves a button to another cell (swapping with the one there), right-click has the
//! cue list's row menu. Where buttons sit is [`crate::model::Cart`] in the list's info.

use super::cue_list::{action_elapsed, known_duration};
use super::widgets::{AMBER, GREEN, RED, fmt_time};
use super::{App, Mode, icons, theme};
use crate::control::{Command, CueAction};
use crate::i18n::t;
use crate::model::{Cue, CueId};
use crate::show::Show;
use egui::{Align2, Color32, CursorIcon, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use std::time::Instant;

/// Shortest a button gets (the grid scrolls past that).
const MIN_H: f32 = 72.0;
/// Gap between buttons.
const GAP: f32 = 6.0;
/// Name sizes tried, largest first.
const NAME_SIZES: [f32; 6] = [22.0, 19.0, 16.0, 14.0, 12.5, 11.0];

/// Where a button's name goes: between its top line (number, icon) and bottom line (time, key).
fn name_area(button: Rect) -> Rect {
    let inner = button.shrink2(vec2(9.0, 7.0));
    Rect::from_x_y_ranges(inner.x_range(), inner.top() + 20.0..=inner.bottom() - 16.0)
}

/// The largest name size at which `name` fits `area` without breaking a word.
fn name_size(ui: &Ui, name: &str, area: Rect) -> f32 {
    let longest = name.split_whitespace().max_by_key(|w| w.chars().count()).unwrap_or("");
    let fits = |size: f32| {
        let font = theme::semibold(size);
        ui.painter().layout_no_wrap(longest.to_string(), font.clone(), theme::TEXT).size().x <= area.width()
            && ui.painter().layout(name.to_string(), font, theme::TEXT, area.width()).size().y <= area.height()
    };
    NAME_SIZES.into_iter().find(|s| fits(*s)).unwrap_or(NAME_SIZES[NAME_SIZES.len() - 1])
}

impl App {
    /// The current list as a cart grid (see the module docs).
    pub(super) fn cart(&mut self, ui: &mut Ui, show: &mut Show) {
        let Some(cart) = show.ws.cue_list.info.cart.clone() else { return };
        let now = Instant::now();
        let editing = self.mode == Mode::Edit;
        let grid = cart.layout(&show.ws.cue_list.cues);
        let cols = cart.columns.clamp(1, crate::model::MAX_CART_SIDE) as usize;
        let rows = grid.len() / cols;
        let shown_rows = (cart.rows as usize).clamp(1, rows.max(1));
        let width = ui.available_width();
        let w = ((width - GAP * (cols - 1) as f32) / cols as f32).max(40.0);
        let h = ((ui.available_height() - GAP * (shown_rows - 1) as f32) / shown_rows as f32).max(MIN_H);

        let mut cells: Vec<Rect> = Vec::with_capacity(grid.len());
        let mut clicked: Option<(CueId, egui::Modifiers)> = None;
        let mut fire: Option<(CueId, CueAction)> = None;
        let mut context_action = None;
        let mut deselect = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("cart").show(ui, |ui| {
            let (area, _) = ui.allocate_exact_size(vec2(width, rows as f32 * (h + GAP) - GAP), Sense::hover());
            // One name size for every button in view (mixed sizes look untidy): the largest all fit at.
            let visible = |i: usize| ui.is_rect_visible(Rect::from_min_size(area.min + vec2(0.0, (i / cols) as f32 * (h + GAP)), vec2(w, h)));
            let names = grid.iter().enumerate().filter(|(i, _)| visible(*i)).filter_map(|(_, s)| *s).filter_map(|id| show.ws.cue_list.cues.iter().find(|c| c.id == id));
            let name_size = names.map(|c| name_size(ui, &c.display_name(), name_area(Rect::from_min_size(area.min, vec2(w, h))))).fold(NAME_SIZES[0], f32::min);
            for (i, slot) in grid.iter().enumerate() {
                let (r, c) = (i / cols, i % cols);
                let rect = Rect::from_min_size(area.min + vec2(c as f32 * (w + GAP), r as f32 * (h + GAP)), vec2(w, h));
                cells.push(rect);
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let Some(cue) = slot.and_then(|id| show.ws.cue_list.cues.iter().find(|c| c.id == id)) else {
                    let resp = ui.interact(rect, ui.id().with(("cart-empty", i)), Sense::click());
                    if resp.clicked() {
                        deselect = true;
                    }
                    if editing {
                        ui.painter().rect_stroke(rect.shrink(1.0), 8.0, Stroke::new(1.0, theme::BORDER), StrokeKind::Inside);
                    }
                    continue;
                };
                let id = cue.id;
                let sense = if editing { Sense::click_and_drag() } else { Sense::click() };
                let resp = ui.interact(rect, ui.id().with(("cart", id)), sense);
                self.paint_button(ui, show, cue, rect, name_size, resp.hovered(), now);
                if editing {
                    if resp.double_clicked() {
                        fire = Some((id, CueAction::Trigger));
                    } else if resp.clicked() {
                        clicked = Some((id, ui.input(|i| i.modifiers)));
                    }
                    if resp.drag_started() {
                        self.drag = Some(vec![id]);
                    }
                    resp.context_menu(|ui| {
                        if let Some(a) = super::cue_list::row_context_menu(ui, cue, true) {
                            context_action = Some((id, a));
                        }
                    });
                } else if resp.clicked() {
                    fire = Some((id, CueAction::Trigger));
                } else if resp.secondary_clicked() {
                    fire = Some((id, CueAction::Stop));
                }
            }
            if grid.iter().all(Option::is_none) {
                ui.painter().text(
                    area.center_top() + vec2(0.0, 40.0),
                    Align2::CENTER_CENTER,
                    t("Empty cart — add cues from the toolbar or drop media files here"),
                    egui::FontId::proportional(15.0),
                    ui.visuals().weak_text_color(),
                );
            }
        });

        if deselect {
            self.selection.clear();
        }
        if let Some((id, mods)) = clicked {
            // No playhead in a cart: selecting only picks what the inspector edits.
            if mods.command {
                if let Some(i) = self.selection.iter().position(|s| *s == id) {
                    self.selection.remove(i);
                } else {
                    self.selection.push(id);
                }
            } else {
                self.selection = vec![id];
            }
        }
        if let Some((id, action)) = fire {
            self.control(show, Command::Cue(id, action));
        }
        if let Some((id, action)) = context_action {
            self.run_row_action(show, id, action);
        }

        // Dragging a button to another cell.
        if let Some(dragging) = self.drag.clone() {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            let pointer = ui.input(|i| i.pointer.interact_pos());
            let target = pointer.and_then(|p| cells.iter().position(|r| r.contains(p)));
            if let Some(i) = target {
                ui.painter().rect_stroke(cells[i], 8.0, Stroke::new(2.0, theme::ACCENT), StrokeKind::Inside);
            }
            if ui.input(|i| i.pointer.any_released()) {
                self.drag = None;
                if let (Some(i), [id]) = (target, dragging.as_slice()) {
                    let list = &mut show.ws.cue_list;
                    if let Some(cart) = &mut list.info.cart {
                        let before = cart.clone();
                        cart.put(&list.cues, *id, i as u32);
                        if *cart != before {
                            show.edited_outside_cues();
                        }
                    }
                }
            }
        }
    }

    /// One cart button: colour, number, name, kind, hotkey, and while it runs its progress.
    #[allow(clippy::too_many_arguments)]
    fn paint_button(&self, ui: &Ui, show: &Show, cue: &Cue, rect: Rect, size: f32, hovered: bool, now: Instant) {
        let p = ui.painter();
        let inst = show.engine.instances.get(&cue.id);
        let selected = self.selection.contains(&cue.id);
        let mut bg = theme::BG_RAISED;
        if let Some([r, g, b]) = cue.color.rgb() {
            bg = Color32::from_rgb(r / 3 + bg.r() / 2, g / 3 + bg.g() / 2, b / 3 + bg.b() / 2);
        }
        if hovered {
            bg = bg.gamma_multiply(1.25);
        }
        p.rect_filled(rect, 8.0, bg);

        // Progress: the played part fills from the left.
        let probe = cue.kind.media_path().filter(|p| !p.as_os_str().is_empty()).and_then(|path| self.prober.get(path));
        let mut remaining = None;
        let state = match inst {
            Some(i) if i.in_pre_wait(now) => Some(AMBER),
            Some(i) if i.is_paused() => Some(AMBER),
            Some(_) => Some(GREEN),
            None => None,
        };
        if let (Some(i), Some(color)) = (inst, state) {
            let frac = if i.in_pre_wait(now) {
                (i.pre_wait > 0.0).then(|| i.pre_wait_elapsed(now) / i.pre_wait)
            } else {
                let e = action_elapsed(show, cue, i, now);
                let d = i.action_duration.or_else(|| known_duration(cue, probe.as_ref())).filter(|d| *d > 0.0);
                remaining = d.map(|d| (d - e).max(0.0));
                d.map(|d| e / d)
            };
            let mut fill = rect;
            fill.set_width(rect.width() * frac.unwrap_or(1.0).clamp(0.0, 1.0) as f32);
            p.rect_filled(fill, 8.0, color.gamma_multiply(0.28));
            p.rect_stroke(rect, 8.0, Stroke::new(2.0, color), StrokeKind::Inside);
            ui.ctx().request_repaint();
        } else if selected {
            p.rect_stroke(rect, 8.0, Stroke::new(2.0, theme::ACCENT), StrokeKind::Inside);
        }
        let broken = match cue.kind.media_path() {
            Some(path) if path.as_os_str().is_empty() => true,
            Some(path) => !self.prober.exists(path) || matches!(probe, Some(Err(_))),
            None => crate::warnings::target_missing(&cue.kind, &show.ws),
        };
        if broken && inst.is_none() {
            p.rect_stroke(rect, 8.0, Stroke::new(1.5, RED), StrokeKind::Inside);
        }
        if let Some([r, g, b]) = cue.color.rgb() {
            p.rect_filled(Rect::from_min_size(rect.min, vec2(rect.width(), 4.0)), egui::CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, Color32::from_rgb(r, g, b));
        }

        let text = if cue.armed { theme::TEXT } else { theme::TEXT_FAINT };
        let inner = rect.shrink2(vec2(9.0, 7.0));
        let clip = p.with_clip_rect(rect.shrink(2.0));
        clip.text(inner.left_top() + vec2(0.0, 2.0), Align2::LEFT_TOP, &cue.number, theme::semibold(13.0), theme::TEXT_DIM);
        let glyph = if !cue.armed {
            icons::BLOCK
        } else if broken && inst.is_none() {
            icons::ERROR
        } else {
            theme::kind_icon(cue.kind.tag())
        };
        let glyph_color = if broken && inst.is_none() { RED } else { theme::TEXT_DIM };
        clip.text(inner.right_top(), Align2::RIGHT_TOP, glyph, theme::icon_font(18.0), glyph_color);

        // Name: wrapped between the top and bottom lines.
        let area = name_area(rect);
        let galley = p.layout(cue.display_name(), theme::semibold(size), text, area.width());
        let at = pos2(area.center().x - galley.size().x / 2.0, area.center().y - galley.size().y / 2.0);
        p.with_clip_rect(area.expand2(vec2(2.0, 0.0))).galley(at, galley, text);

        // Bottom line: time left (or how long it lasts), hotkey.
        let bottom = inner.left_bottom();
        let time = match remaining {
            Some(r) => Some(format!("-{}", fmt_time(r))),
            None if inst.is_some() => None,
            None => known_duration(cue, probe.as_ref()).map(fmt_time),
        };
        if let Some(time) = time {
            clip.text(bottom, Align2::LEFT_BOTTOM, time, theme::mono(12.5), if inst.is_some() { theme::TEXT } else { theme::TEXT_FAINT });
        }
        if let Some(key) = &cue.hotkey {
            let galley = p.layout_no_wrap(key.clone(), theme::semibold(10.5), theme::TEXT_DIM);
            let pill = Rect::from_min_size(inner.right_bottom() - galley.size() - vec2(10.0, 2.0), galley.size() + vec2(10.0, 2.0));
            clip.rect_stroke(pill, 4.0, Stroke::new(1.0, theme::BORDER), StrokeKind::Inside);
            clip.galley(pill.min + vec2(5.0, 1.0), galley, theme::TEXT_DIM);
        }
    }
}
