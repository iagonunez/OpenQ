//! Animation tab of video and text cues: a keyframe timeline (one row per animated property),
//! the selected keyframe's value and easing, and the animation's length and repeat mode.
//!
//! The editor playhead also drives the Geometry preview, which shows the layer as it looks at
//! that moment of the animation.

use super::icons as ic;
use super::theme::{self, Card};
use super::widgets::{fmt_time, time_edit};
use crate::animation::{AnimProp, Animation, Ease, Key, Repeat, Track};
use crate::model::CueId;
use egui::{Color32, ComboBox, DragValue, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, pos2, vec2};

/// Editor state kept between frames.
#[derive(Default)]
pub(super) struct AnimEdit {
    /// Cue being edited (the state resets when it changes).
    pub cue: Option<CueId>,
    /// Editor playhead, in seconds.
    pub time: f64,
    /// Selected keyframe: track property and key index.
    pub sel: Option<(AnimProp, usize)>,
}

impl AnimEdit {
    /// The playhead for `cue`, if it is the cue being edited (read by the Geometry preview).
    #[cfg_attr(not(feature = "video"), allow(dead_code))]
    pub fn time_for(&self, cue: CueId) -> Option<f64> {
        (self.cue == Some(cue)).then_some(self.time)
    }
}

const ROW_H: f32 = 24.0;
const RULER_H: f32 = 22.0;
const LABEL_W: f32 = 120.0;

/// The Animation tab. `base` holds a Text cue's own colours (background, shadow, outline,
/// fill), which new colour tracks start from; `None` for video.
pub(super) fn animation_tab(ui: &mut Ui, id: CueId, anim: &mut Animation, base: Option<[[u8; 4]; 4]>, ed: &mut AnimEdit, preview: &mut bool) {
    if ed.cue != Some(id) {
        *ed = AnimEdit { cue: Some(id), ..Default::default() };
    }
    if ed.sel.is_some_and(|(p, i)| anim.tracks.iter().find(|t| t.prop == p).is_none_or(|t| i >= t.keys.len())) {
        ed.sel = None;
    }
    let specs = [Card::new(ic::TIMER, "Timeline", f32::INFINITY), Card::new(ic::ANIMATION, "Animation", 330.0), Card::new(ic::DIAMOND, "Keyframe", 360.0)];
    theme::cards(ui, "anim", &specs, |ui, i| match i {
        0 => timeline(ui, anim, ed),
        1 => settings(ui, anim, base, ed, preview),
        _ => key_editor(ui, anim, ed),
    });
}

fn settings(ui: &mut Ui, anim: &mut Animation, base: Option<[[u8; 4]; 4]>, ed: &mut AnimEdit, preview: &mut bool) {
    egui::Grid::new("anim-settings").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        ui.label("Length");
        ui.horizontal(|ui| {
            time_edit(ui, &mut anim.duration).on_hover_text("Length of one pass; 0 = up to the last keyframe");
            if anim.duration <= 0.0 {
                ui.label(RichText::new(format!("auto · {}", fmt_time(anim.length()))).weak());
            }
        });
        ui.end_row();

        ui.label("Repeat");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut anim.repeat, Repeat::Once, "Once").on_hover_text("Play once, then hold the last values");
            ui.selectable_value(&mut anim.repeat, Repeat::Loop, "Loop");
            ui.selectable_value(&mut anim.repeat, Repeat::PingPong, "Ping-pong").on_hover_text("Forwards, then backwards");
        });
        ui.end_row();

        ui.label("Playhead");
        ui.horizontal(|ui| {
            time_edit(ui, &mut ed.time);
            super::inspector::preview_toggle(ui, preview);
        });
        ui.end_row();

        ui.label("Add track");
        let props: Vec<AnimProp> =
            AnimProp::LAYER.iter().chain(if base.is_some() { &AnimProp::TEXT[..] } else { &[] }).copied().filter(|p| !anim.has(*p)).collect();
        let mut add = None;
        ComboBox::from_id_salt("anim-add").selected_text("Choose…").width(170.0).show_ui(ui, |ui| {
            for p in props {
                if ui.selectable_label(false, p.label()).clicked() {
                    add = Some(p);
                }
            }
        });
        ui.end_row();
        if let Some(p) = add {
            // The first key keeps the layer as it is: neutral values, or the text's own colour.
            let v = match (base, AnimProp::TEXT.iter().position(|q| *q == p)) {
                (Some(b), Some(i)) => b[i].map(|c| c as f32 / 255.0),
                _ => p.neutral(),
            };
            anim.tracks.retain(|t| t.prop != p);
            anim.tracks.push(Track { prop: p, keys: vec![Key { t: ed.time, v, ease: Ease::default() }] });
            ed.sel = Some((p, 0));
        }
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.add_enabled(!anim.tracks.is_empty(), egui::Button::new(theme::with_icon(ic::DELETE, "Clear animation"))).clicked() {
            anim.tracks.clear();
            ed.sel = None;
        }
    });
    ui.label(
        RichText::new("Values are relative: position, rotation and crop add to the Geometry tab, scale and opacity multiply it.")
            .weak()
            .small(),
    );
}

/// Units of a property in the editor: (display factor, suffix, drag speed).
fn units(p: AnimProp) -> (f32, &'static str, f64) {
    match p {
        AnimProp::Rotation => (1.0, "°", 0.5),
        AnimProp::ScaleX | AnimProp::ScaleY => (100.0, "%", 0.5),
        _ => (100.0, "%", 0.1),
    }
}

/// Edit one keyframe value; true if it changed.
fn value_edit(ui: &mut Ui, p: AnimProp, v: &mut [f32; 4]) -> bool {
    if p.is_color() {
        let b = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
        let mut c = Color32::from_rgba_unmultiplied(b(v[0]), b(v[1]), b(v[2]), b(v[3]));
        let changed = egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::OnlyBlend).changed();
        if changed {
            *v = c.to_srgba_unmultiplied().map(|x| x as f32 / 255.0);
        }
        return changed;
    }
    let (k, suffix, speed) = units(p);
    let mut shown = v[0] * k;
    let mut dv = DragValue::new(&mut shown).speed(speed).suffix(suffix).max_decimals(2);
    dv = match p {
        AnimProp::Opacity => dv.range(0.0..=100.0),
        AnimProp::ScaleX | AnimProp::ScaleY => dv.range(0.0..=10_000.0),
        AnimProp::CropLeft | AnimProp::CropTop | AnimProp::CropRight | AnimProp::CropBottom => dv.range(-100.0..=100.0),
        _ => dv,
    };
    let changed = ui.add(dv).changed();
    if changed {
        v[0] = shown / k;
    }
    changed
}

fn key_editor(ui: &mut Ui, anim: &mut Animation, ed: &mut AnimEdit) {
    let Some((prop, i)) = ed.sel else {
        ui.label(RichText::new("Click a keyframe on the timeline to edit it. Double-click a track to add one.").weak());
        return;
    };
    let Some(ti) = anim.tracks.iter().position(|t| t.prop == prop) else { return };
    let mut delete_key = false;
    let mut delete_track = false;
    let mut add_here = false;
    {
        let track = &mut anim.tracks[ti];
        let key = &mut track.keys[i];
        egui::Grid::new("anim-key").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Track");
            ui.label(RichText::new(prop.label()).strong());
            ui.end_row();

            ui.label("Time");
            let mut t = key.t;
            if time_edit(ui, &mut t).changed() {
                key.t = t;
            }
            ui.end_row();

            ui.label("Value");
            value_edit(ui, prop, &mut key.v);
            ui.end_row();

            ui.label("Then");
            ui.horizontal(|ui| {
                let custom = matches!(key.ease, Ease::Bezier(_));
                ComboBox::from_id_salt("anim-ease").selected_text(key.ease.label()).show_ui(ui, |ui| {
                    for e in [Ease::Hold, Ease::Linear, Ease::In, Ease::Out, Ease::InOut] {
                        ui.selectable_value(&mut key.ease, e, e.label());
                    }
                    if ui.selectable_label(custom, Ease::Bezier([0.0; 4]).label()).clicked() && !custom {
                        key.ease = Ease::Bezier([0.25, 0.1, 0.25, 1.0]);
                    }
                })
                .response
                .on_hover_text("How the value moves on to the next keyframe");
            });
            ui.end_row();
            if let Ease::Bezier(b) = &mut key.ease {
                ui.label("Curve");
                ui.horizontal(|ui| {
                    for (k, v) in b.iter_mut().enumerate() {
                        let r = if k % 2 == 0 { 0.0..=1.0 } else { -2.0..=3.0 };
                        ui.add(DragValue::new(v).range(r).speed(0.01).fixed_decimals(2));
                    }
                })
                .response
                .on_hover_text("Cubic Bézier x1 y1 x2 y2, as in CSS");
                ui.end_row();
            }
        });
        // Keep the keys in time order, following the edited one.
        if track.keys.windows(2).any(|w| w[0].t > w[1].t) {
            let moved = track.keys[i];
            track.sort();
            ed.sel = track.keys.iter().position(|k| *k == moved).map(|i| (prop, i));
        }
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        add_here = ui.button(theme::with_icon(ic::ADD, "Key at playhead")).on_hover_text("Add a key with the value at the playhead").clicked();
        delete_key = ui.button(theme::with_icon(ic::REMOVE, "Delete key")).clicked();
        delete_track = ui.button(theme::with_icon(ic::DELETE, "Delete track")).clicked();
    });
    if add_here {
        let v = anim.tracks[ti].sample(anim.local_time(ed.time));
        anim.set_key(prop, ed.time, v);
        ed.sel = anim.tracks[ti].keys.iter().position(|k| (k.t - ed.time).abs() < 1e-3).map(|i| (prop, i));
    } else if delete_key {
        anim.tracks[ti].keys.remove(i);
        if anim.tracks[ti].keys.is_empty() {
            anim.tracks.remove(ti);
        }
        ed.sel = None;
    } else if delete_track {
        anim.tracks.remove(ti);
        ed.sel = None;
    }
}

/// A "nice" ruler step for `span` seconds.
fn ruler_step(span: f64, px: f32) -> f64 {
    let max_ticks = (px / 70.0).max(2.0) as f64;
    [0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0].into_iter().find(|s| span / s <= max_ticks).unwrap_or(1200.0)
}

fn timeline(ui: &mut Ui, anim: &mut Animation, ed: &mut AnimEdit) {
    let rows = anim.tracks.len();
    let width = ui.available_width().max(200.0);
    let height = RULER_H + ROW_H * rows.max(1) as f32;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let painter = ui.painter_at(rect);
    let lane = Rect::from_min_max(pos2(rect.left() + LABEL_W, rect.top()), rect.max);
    let last = anim.tracks.iter().filter_map(|t| t.keys.last()).map(|k| k.t).fold(0.0, f64::max);
    let span = (anim.length().max(last).max(ed.time) * 1.15).max(2.0);
    let x_of = |t: f64| lane.left() + 8.0 + (t / span) as f32 * (lane.width() - 16.0);
    let t_of = |x: f32| (((x - lane.left() - 8.0) / (lane.width() - 16.0)) as f64 * span).max(0.0);

    painter.rect_filled(rect, 4.0, theme::BG_INPUT);
    // Ruler: click or drag to move the playhead.
    let ruler = Rect::from_min_max(lane.min, pos2(lane.right(), lane.top() + RULER_H));
    let r = ui.interact(ruler, ui.id().with("anim-ruler"), Sense::click_and_drag());
    if (r.clicked() || r.dragged())
        && let Some(p) = r.interact_pointer_pos()
    {
        ed.time = t_of(p.x);
    }
    let step = ruler_step(span, lane.width());
    let mut t = 0.0;
    while t <= span {
        let x = x_of(t);
        painter.line_segment([pos2(x, ruler.bottom() - 5.0), pos2(x, ruler.bottom())], Stroke::new(1.0, theme::TEXT_FAINT));
        painter.text(pos2(x + 3.0, ruler.top() + 3.0), egui::Align2::LEFT_TOP, fmt_secs(t), theme::mono(10.0), theme::TEXT_FAINT);
        t += step;
    }
    if anim.duration > 0.0 {
        let x = x_of(anim.duration);
        painter.extend(Shape::dashed_line(&[pos2(x, ruler.top()), pos2(x, rect.bottom())], Stroke::new(1.0, theme::AMBER), 4.0, 3.0));
    }
    if rows == 0 {
        painter.text(
            pos2(rect.center().x, rect.top() + RULER_H + ROW_H / 2.0),
            egui::Align2::CENTER_CENTER,
            "No tracks yet: pick a property under Add track",
            egui::FontId::proportional(12.0),
            theme::TEXT_FAINT,
        );
    }

    let mut select = None;
    let mut add_at: Option<(AnimProp, f64)> = None;
    for (row, track) in anim.tracks.iter_mut().enumerate() {
        let top = rect.top() + RULER_H + row as f32 * ROW_H;
        let row_rect = Rect::from_min_max(pos2(rect.left(), top), pos2(rect.right(), top + ROW_H));
        if row % 2 == 1 {
            painter.rect_filled(row_rect, 0.0, theme::BG_PANEL);
        }
        painter.text(pos2(rect.left() + 8.0, row_rect.center().y), egui::Align2::LEFT_CENTER, track.prop.label(), theme::semibold(11.5), theme::TEXT_DIM);
        let lane_row = Rect::from_min_max(pos2(lane.left(), top), pos2(lane.right(), top + ROW_H));
        let bg = ui.interact(lane_row, ui.id().with(("anim-row", track.prop)), Sense::click());
        if let Some(p) = bg.interact_pointer_pos() {
            if bg.double_clicked() {
                add_at = Some((track.prop, t_of(p.x)));
            } else if bg.clicked() {
                ed.time = t_of(p.x);
            }
        }
        let cy = row_rect.center().y;
        // Segments between keys: solid for moves, dashed for holds.
        for w in track.keys.windows(2) {
            let (a, b) = (pos2(x_of(w[0].t), cy), pos2(x_of(w[1].t), cy));
            if w[0].ease == Ease::Hold {
                painter.extend(Shape::dashed_line(&[a, b], Stroke::new(1.0, theme::TEXT_FAINT), 3.0, 3.0));
            } else {
                painter.line_segment([a, b], Stroke::new(1.5, theme::BORDER));
            }
        }
        let n = track.keys.len();
        for i in 0..n {
            let x = x_of(track.keys[i].t);
            let hit = Rect::from_center_size(pos2(x, cy), vec2(14.0, ROW_H));
            let r = ui.interact(hit, ui.id().with(("anim-key", track.prop, i)), Sense::click_and_drag());
            if r.clicked() || r.drag_started() {
                select = Some((track.prop, i));
            }
            if r.dragged() {
                // Stay between the neighbours so the order (and this drag) holds.
                let lo = if i > 0 { track.keys[i - 1].t + 0.01 } else { 0.0 };
                let hi = if i + 1 < n { track.keys[i + 1].t - 0.01 } else { f64::MAX };
                let k = &mut track.keys[i];
                k.t = (k.t + (r.drag_delta().x / (lane.width() - 16.0)) as f64 * span).clamp(lo, hi.max(lo));
                ed.time = k.t;
            }
            if r.hovered() || r.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            let r = r.on_hover_text(format!("{} · {}", fmt_time(track.keys[i].t), value_text(track.prop, &track.keys[i].v)));
            let selected = ed.sel == Some((track.prop, i));
            let fill = if track.prop.is_color() {
                let v = track.keys[i].v.map(|c| (c.clamp(0.0, 1.0) * 255.0) as u8);
                Color32::from_rgb(v[0], v[1], v[2])
            } else if selected {
                theme::ACCENT
            } else {
                theme::TEXT_DIM
            };
            diamond(&painter, pos2(x, cy), if r.hovered() { 6.5 } else { 5.5 }, fill, selected, track.prop.is_color());
        }
    }
    if let Some(s) = select {
        ed.sel = Some(s);
    }
    if let Some((p, t)) = add_at {
        let v = anim.track(p).map(|tr| tr.sample(anim.local_time(t))).unwrap_or(p.neutral());
        anim.set_key(p, t, v);
        ed.sel = anim.track(p).and_then(|tr| tr.keys.iter().position(|k| (k.t - t).abs() < 1e-3)).map(|i| (p, i));
        ed.time = t;
    }
    // Playhead over everything.
    let x = x_of(ed.time);
    painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.5, theme::ACCENT));
    painter.add(Shape::convex_polygon(
        vec![pos2(x - 5.0, rect.top()), pos2(x + 5.0, rect.top()), pos2(x, rect.top() + 7.0)],
        theme::ACCENT,
        Stroke::NONE,
    ));
}

/// A keyframe marker; colour keys get a light edge so dark colours stay visible.
fn diamond(p: &egui::Painter, c: Pos2, r: f32, fill: Color32, selected: bool, colour: bool) {
    let pts = vec![pos2(c.x, c.y - r), pos2(c.x + r, c.y), pos2(c.x, c.y + r), pos2(c.x - r, c.y)];
    let stroke = match (selected, colour) {
        (true, _) => Stroke::new(2.0, theme::TEXT),
        (false, true) => Stroke::new(1.0, theme::TEXT_DIM),
        (false, false) => Stroke::new(1.0, theme::BG_APP),
    };
    p.add(Shape::convex_polygon(pts, fill, stroke));
}

fn fmt_secs(t: f64) -> String {
    if t >= 60.0 { fmt_time(t) } else { format!("{}s", (t * 100.0).round() / 100.0) }
}

/// A keyframe value as text (tooltips).
fn value_text(p: AnimProp, v: &[f32; 4]) -> String {
    if p.is_color() {
        let b = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
        return format!("#{:02x}{:02x}{:02x} · {:.0}%", b(v[0]), b(v[1]), b(v[2]), v[3] * 100.0);
    }
    let (k, suffix, _) = units(p);
    format!("{:.1}{suffix}", v[0] * k)
}
