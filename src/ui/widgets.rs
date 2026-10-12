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

/// A vertical fader: a dB readout (drag or type), the fader itself (drag; double-click
/// for 0 dB) and a label underneath. `strong` paints the fill in the accent colour (main fader).
pub fn vfader(ui: &mut Ui, value: &mut f32, label: &str, track_h: f32, strong: bool) -> Response {
    use super::theme;
    use egui::{Align2, CornerRadius, Sense, Stroke, pos2, vec2};
    let min = crate::model::MIN_DB;
    ui.vertical(|ui| {
        ui.set_width(FADER_W);
        let mut resp = ui.add_sized(
            [FADER_W, 20.0],
            DragValue::new(value).range(min..=FADER_MAX_DB).speed(0.2).custom_formatter(|v, _| db_text(v)),
        );
        let (rect, track) = ui.allocate_exact_size(vec2(FADER_W, track_h), Sense::click_and_drag());
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
        let handle = egui::Rect::from_center_size(pos2(cx, y), vec2(28.0, 12.0));
        let hot = track.hovered() || track.dragged();
        p.rect_filled(handle, CornerRadius::same(3), if hot { theme::TEXT } else { theme::TEXT_DIM });
        p.line_segment([pos2(handle.left() + 4.0, y), pos2(handle.right() - 4.0, y)], Stroke::new(1.5, theme::BG_INPUT));
        let (lr, _) = ui.allocate_exact_size(vec2(FADER_W, 16.0), Sense::hover());
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
