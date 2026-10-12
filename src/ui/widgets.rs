//! Small shared UI helpers.

use egui::{Color32, DragValue, Rect, Response, Ui};

/// "m:ss.cc" (or "h:mm:ss.cc").
pub fn fmt_time(secs: f64) -> String {
    let neg = secs < 0.0;
    let s = secs.abs();
    let total_cs = (s * 100.0).round() as u64;
    let cs = total_cs % 100;
    let total_s = total_cs / 100;
    let (h, m, sec) = (total_s / 3600, (total_s / 60) % 60, total_s % 60);
    let sign = if neg { "-" } else { "" };
    if h > 0 {
        format!("{sign}{h}:{m:02}:{sec:02}.{cs:02}")
    } else {
        format!("{sign}{m}:{sec:02}.{cs:02}")
    }
}

/// Parse "ss", "ss.s", "m:ss.s" or "h:m:ss.s".
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return Some(0.0);
    }
    let mut total = 0.0;
    for part in s.split(':') {
        let v: f64 = part.trim().parse().ok()?;
        total = total * 60.0 + v;
    }
    Some(total.max(0.0))
}

/// Drag/typed time editor in seconds.
pub fn time_edit(ui: &mut Ui, value: &mut f64) -> Response {
    ui.add(
        DragValue::new(value)
            .speed(0.05)
            .range(0.0..=86_400.0)
            .custom_formatter(|v, _| fmt_time(v))
            .custom_parser(parse_time),
    )
}

pub fn db_edit(ui: &mut Ui, value: &mut f32) -> Response {
    ui.add(
        egui::Slider::new(value, crate::model::MIN_DB..=12.0)
            .suffix(" dB")
            .custom_formatter(|v, _| if v <= crate::model::MIN_DB as f64 { "-inf".into() } else { format!("{v:.1}") }),
    )
}

/// Top of a fader's travel, in dB.
pub const FADER_MAX_DB: f32 = 12.0;

/// Position (0 = bottom, 1 = top) of a level on a fader. Like a console fader law, the upper
/// three quarters of the travel cover -20..+12 dB and the bottom quarter -60..-20 dB.
pub fn fader_pos(db: f32) -> f32 {
    let min = crate::model::MIN_DB;
    if db >= -20.0 {
        0.25 + 0.75 * ((db.min(FADER_MAX_DB) + 20.0) / (FADER_MAX_DB + 20.0))
    } else {
        0.25 * ((db.max(min) - min) / (-20.0 - min))
    }
}

/// Inverse of [`fader_pos`].
pub fn fader_db(pos: f32) -> f32 {
    let min = crate::model::MIN_DB;
    let pos = pos.clamp(0.0, 1.0);
    if pos >= 0.25 { -20.0 + (pos - 0.25) / 0.75 * (FADER_MAX_DB + 20.0) } else { min + pos / 0.25 * (-20.0 - min) }
}

/// Width of a [`vfader`] column.
pub const FADER_W: f32 = 46.0;

fn db_text(v: f64) -> String {
    if v <= crate::model::MIN_DB as f64 { "-inf".into() } else { format!("{v:.1}") }
}

/// Narrowest a [`vfader`] column gets (its readout still shows "-inf" / "-12.5").
pub const FADER_MIN_W: f32 = 34.0;

/// A vertical fader: a dB readout (drag or type), the fader itself (drag; double-click
/// for 0 dB) and a label underneath. `strong` paints the fill in the accent colour (main fader).
/// The column is `width` points wide (clamped to [`FADER_MIN_W`]..=[`FADER_W`]), so a bank of many
/// outputs fits without scrolling.
pub fn vfader(ui: &mut Ui, value: &mut f32, label: &str, track_h: f32, strong: bool, width: f32) -> Response {
    use super::theme;
    use egui::{Align2, CornerRadius, Sense, Stroke, pos2, vec2};
    let min = crate::model::MIN_DB;
    let w = width.clamp(FADER_MIN_W, FADER_W);
    ui.vertical(|ui| {
        ui.set_width(w);
        ui.spacing_mut().interact_size.x = w;
        let mut resp = ui.add_sized(
            [w, 20.0],
            DragValue::new(value).range(min..=FADER_MAX_DB).speed(0.2).custom_formatter(|v, _| db_text(v)),
        );
        let (rect, track) = ui.allocate_exact_size(vec2(w, track_h), Sense::click_and_drag());
        let track = track.on_hover_cursor(egui::CursorIcon::ResizeVertical).on_hover_text("Drag to set · double-click for 0 dB");
        // The handle's centre travels between these.
        let (top, bottom) = (rect.top() + 7.0, rect.bottom() - 7.0);
        if track.double_clicked() {
            *value = 0.0;
            resp.mark_changed();
        } else if (track.dragged() || track.drag_started())
            && let Some(p) = track.interact_pointer_pos()
        {
            let db = fader_db((bottom - p.y) / (bottom - top));
            // Snap to 0 dB near unity, like a detent.
            let db = if db.abs() < 0.4 { 0.0 } else { (db * 10.0).round() / 10.0 };
            if db != *value {
                *value = db;
                resp.mark_changed();
            }
        }
        let y = bottom - fader_pos(*value) * (bottom - top);
        let p = ui.painter();
        let cx = rect.center().x;
        let groove = egui::Rect::from_min_max(pos2(cx - 2.5, top), pos2(cx + 2.5, bottom));
        p.rect_filled(groove, CornerRadius::same(2), theme::BG_INPUT);
        if *value > min {
            let fill = egui::Rect::from_min_max(pos2(groove.left(), y), groove.max);
            p.rect_filled(fill, CornerRadius::same(2), if strong { theme::ACCENT } else { theme::BLUE.gamma_multiply(0.6) });
        }
        // Unity mark.
        let y0 = bottom - fader_pos(0.0) * (bottom - top);
        p.line_segment([pos2(cx - 9.0, y0), pos2(cx - 4.0, y0)], Stroke::new(1.0, theme::TEXT_FAINT));
        p.line_segment([pos2(cx + 4.0, y0), pos2(cx + 9.0, y0)], Stroke::new(1.0, theme::TEXT_FAINT));
        let handle = egui::Rect::from_center_size(pos2(cx, y), vec2(28.0f32.min(w - 6.0), 12.0));
        let hot = track.hovered() || track.dragged();
        p.rect_filled(handle, CornerRadius::same(3), if hot { theme::TEXT } else { theme::TEXT_DIM });
        p.line_segment([pos2(handle.left() + 4.0, y), pos2(handle.right() - 4.0, y)], Stroke::new(1.5, theme::BG_INPUT));
        let (lr, _) = ui.allocate_exact_size(vec2(w, 16.0), Sense::hover());
        ui.painter().text(lr.center(), Align2::CENTER_CENTER, label, theme::semibold(11.0), theme::TEXT_DIM);
        resp |= track;
        resp
    })
    .inner
}

/// The dB scale drawn beside a bank of [`vfader`]s, lined up with their tracks.
pub fn fader_scale(ui: &mut Ui, track_h: f32) {
    use super::theme;
    use egui::{Align2, Sense, pos2, vec2};
    ui.vertical(|ui| {
        ui.allocate_exact_size(vec2(26.0, 20.0), Sense::hover());
        let (rect, _) = ui.allocate_exact_size(vec2(26.0, track_h), Sense::hover());
        let (top, bottom) = (rect.top() + 7.0, rect.bottom() - 7.0);
        for db in [12.0, 6.0, 0.0, -10.0, -20.0, -40.0, -60.0] {
            let y = bottom - fader_pos(db) * (bottom - top);
            let text = if db <= crate::model::MIN_DB { "-∞".to_string() } else { format!("{db:+.0}").replace("+0", "0") };
            ui.painter().text(pos2(rect.right() - 2.0, y), Align2::RIGHT_CENTER, text, theme::mono(9.0), theme::TEXT_FAINT);
        }
    });
}

/// The fade curve graph: the shape of a fade over its `duration`, with two handles (at 1/3 and
/// 2/3 of the time) to drag up and down. Dragging a preset curve turns it into a
/// [`FadeCurve::Custom`] that starts from the preset's shape; double-click a handle for a straight
/// line ([`FadeCurve::Linear`]). `falling` draws a fade toward silence going down; `level` draws the level law (the
/// logarithmic curve is in dB) rather than the plain progress used for opacity and geometry.
pub fn curve_graph(ui: &mut Ui, salt: &str, curve: &mut crate::model::FadeCurve, duration: f64, falling: bool, level: bool) -> Response {
    use super::theme;
    use crate::model::FadeCurve;
    use egui::{Align2, Sense, Shape, Stroke, pos2, vec2};
    let (outer, mut resp) = ui.allocate_exact_size(vec2(ui.available_width(), 150.0), Sense::hover());
    let p = ui.painter_at(outer);
    p.rect_filled(outer, 4.0, theme::BG_INPUT);
    let rect = outer.shrink2(vec2(10.0, 12.0));
    let grid = Stroke::new(0.5, Color32::from_gray(50));
    for k in 1..4 {
        let f = k as f32 / 4.0;
        p.line_segment([pos2(rect.left() + f * rect.width(), rect.top()), pos2(rect.left() + f * rect.width(), rect.bottom())], grid);
        p.line_segment([pos2(rect.left(), rect.top() + f * rect.height()), pos2(rect.right(), rect.top() + f * rect.height())], grid);
    }
    p.rect_stroke(rect, 0.0, Stroke::new(0.8, Color32::from_gray(70)), egui::StrokeKind::Middle);
    // Value (0..1, up) at progress t; a falling fade goes from 1 to 0.
    let (a, b) = if falling { (1.0, 0.0) } else { (0.0, 1.0) };
    let value = |c: FadeCurve, t: f32| if level { c.interpolate(a, b, t) } else { a + (b - a) * c.progress(t) };
    let at = |t: f32, v: f32| pos2(rect.left() + t * rect.width(), rect.bottom() - v * rect.height());
    // Handle heights as drawn (a falling fade mirrors them).
    let shown = |h: f32| if falling { 1.0 - h } else { h };
    let handles = curve.handles(falling);
    for (k, &h) in handles.iter().enumerate() {
        let t = (k + 1) as f32 / 3.0;
        let id = ui.id().with((salt, k));
        let c = at(t, shown(h));
        let r = ui.interact(egui::Rect::from_center_size(c, vec2(18.0, 18.0)), id, Sense::click_and_drag());
        if r.double_clicked() {
            *curve = FadeCurve::Linear;
            resp.mark_changed();
        } else if r.dragged()
            && let Some(pos) = r.interact_pointer_pos()
        {
            let v = ((rect.bottom() - pos.y) / rect.height()).clamp(0.0, 1.0);
            let mut hs = handles;
            hs[k] = (if falling { 1.0 - v } else { v } * 100.0).round() / 100.0;
            *curve = FadeCurve::Custom(hs);
            resp.mark_changed();
        }
        let hot = r.hovered() || r.dragged();
        let r = r.on_hover_cursor(egui::CursorIcon::ResizeVertical).on_hover_text("Drag to shape the fade · double-click for a straight line");
        resp |= r;
        // Control arm from the nearer end point.
        let end = if k == 0 { at(0.0, a) } else { at(1.0, b) };
        p.add(Shape::dashed_line(&[end, c], Stroke::new(1.0, theme::TEXT_FAINT), 3.0, 3.0));
        p.circle_filled(c, if hot { 6.0 } else { 5.0 }, if hot { theme::AMBER } else { theme::TEXT_DIM });
    }
    let pts: Vec<_> = (0..=64).map(|i| {
        let t = i as f32 / 64.0;
        at(t, value(*curve, t).clamp(0.0, 1.0))
    }).collect();
    p.add(Shape::line(pts, Stroke::new(2.0, theme::ACCENT)));
    let small = theme::mono(9.0);
    p.text(pos2(rect.left(), outer.bottom() - 1.0), Align2::LEFT_BOTTOM, "0", small.clone(), theme::TEXT_FAINT);
    p.text(pos2(rect.right(), outer.bottom() - 1.0), Align2::RIGHT_BOTTOM, fmt_time(duration), small.clone(), theme::TEXT_FAINT);
    p.text(pos2(rect.left() + 3.0, outer.top() + 1.0), Align2::LEFT_TOP, if falling { "start" } else { "target" }, small, theme::TEXT_FAINT);
    resp
}

/// A thin progress bar painted inside `rect`.
pub fn paint_progress(ui: &Ui, rect: Rect, frac: f32, color: Color32) {
    let p = ui.painter();
    p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    p.rect_filled(fill, 2.0, color);
}

pub const GREEN: Color32 = Color32::from_rgb(60, 170, 80);
pub const AMBER: Color32 = Color32::from_rgb(220, 160, 40);
pub const RED: Color32 = Color32::from_rgb(210, 60, 60);
pub const BLUE: Color32 = Color32::from_rgb(70, 130, 220);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_roundtrip() {
        assert_eq!(fmt_time(0.0), "0:00.00");
        assert_eq!(fmt_time(83.456), "1:23.46");
        assert_eq!(fmt_time(3725.0), "1:02:05.00");
        assert_eq!(parse_time("1:23.5"), Some(83.5));
        assert_eq!(parse_time("90"), Some(90.0));
        assert_eq!(parse_time("1:02:05"), Some(3725.0));
        assert_eq!(parse_time("x"), None);
    }

    #[test]
    fn fader_law_roundtrips() {
        assert_eq!(fader_pos(crate::model::MIN_DB), 0.0);
        assert_eq!(fader_pos(FADER_MAX_DB), 1.0);
        assert!((fader_pos(-20.0) - 0.25).abs() < 1e-6);
        for db in [-60.0, -45.0, -20.0, -6.5, 0.0, 3.0, 12.0] {
            assert!((fader_db(fader_pos(db)) - db).abs() < 1e-3, "{db}");
        }
    }
}
