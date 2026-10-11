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
}
