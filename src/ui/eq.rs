//! Parametric EQ editor: a band table ([`eq_bands`]) and a response curve with draggable band
//! handles ([`eq_graph`]), laid out side by side by the inspector.

use super::widgets::{AMBER, BLUE, GREEN};
use crate::dsp::{Coeffs, eq_response_db};
use crate::model::{Eq, EqBand, EqKind, MAX_EQ_BANDS};
use egui::{Align2, Color32, ComboBox, DragValue, FontId, Id, Rect, Sense, Stroke, Ui, pos2, vec2};
use crate::i18n::t;

const F_MIN: f32 = 20.0;
const F_MAX: f32 = 20_000.0;
const DB_RANGE: f32 = 18.0;
/// Display rate for the response plot.
const RATE: f32 = 48_000.0;

/// Height of the response plot.
const GRAPH_H: f32 = 190.0;

/// The enable/flat/defaults row and the table of bands.
pub fn eq_bands(ui: &mut Ui, id: Id, eq: &mut Eq) {
    ui.horizontal(|ui| {
        ui.checkbox(&mut eq.enabled, t("EQ enabled"));
        if ui.button(super::theme::with_icon(super::icons::REMOVE, t("Flat"))).on_hover_text(t("Reset all gains to 0 dB")).clicked() {
            for b in &mut eq.bands {
                b.gain_db = 0.0;
            }
        }
        if ui.button(super::theme::with_icon(super::icons::REPLAY, t("Defaults"))).clicked() {
            *eq = Eq { enabled: eq.enabled, ..Eq::default() };
        }
    });

    ui.add_space(4.0);
    let mut remove = None;
    egui::Grid::new(id.with("bands")).num_columns(6).spacing([8.0, 4.0]).show(ui, |ui| {
        for h in ["#", t("Type"), t("Freq"), t("Gain"), "Q", ""] {
            ui.label(egui::RichText::new(h).weak());
        }
        ui.end_row();
        for (i, b) in eq.bands.iter_mut().enumerate() {
            ui.checkbox(&mut b.enabled, format!("{}", i + 1));
            ComboBox::from_id_salt(id.with(("kind", i))).width(100.0).selected_text(b.kind.label()).show_ui(ui, |ui| {
                for k in EqKind::ALL {
                    ui.selectable_value(&mut b.kind, k, k.label());
                }
            });
            let speed = b.freq * 0.01;
            ui.add(DragValue::new(&mut b.freq).range(F_MIN..=F_MAX).speed(speed).suffix(" Hz").max_decimals(0));
            ui.add_enabled(b.kind.has_gain(), DragValue::new(&mut b.gain_db).range(-24.0..=24.0).speed(0.1).suffix(" dB").max_decimals(1));
            ui.add(DragValue::new(&mut b.q).range(0.1..=18.0).speed(0.01).max_decimals(2));
            if super::theme::icon_button(ui, super::icons::DELETE, t("Remove band")).clicked() {
                remove = Some(i);
            }
            ui.end_row();
        }
    });
    if let Some(i) = remove {
        eq.bands.remove(i);
    }
    if eq.bands.len() < MAX_EQ_BANDS && ui.button(super::theme::with_icon(super::icons::ADD, t("Add band"))).clicked() {
        eq.bands.push(EqBand { enabled: true, kind: EqKind::Peak, freq: 1000.0, gain_db: 0.0, q: 1.0 });
    }
}

/// The response curve; drag a band's handle to move it, scroll on it to change its Q.
pub fn eq_graph(ui: &mut Ui, id: Id, eq: &mut Eq) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), GRAPH_H), Sense::hover());
    let p = ui.painter_at(rect);
    let vis = ui.visuals();
    p.rect_filled(rect, 3.0, vis.extreme_bg_color);
    let x_of = |f: f32| rect.left() + (f / F_MIN).ln() / (F_MAX / F_MIN).ln() * rect.width();
    let f_of = |x: f32| F_MIN * ((x - rect.left()) / rect.width() * (F_MAX / F_MIN).ln()).exp();
    let y_of = |db: f32| rect.center().y - db.clamp(-DB_RANGE * 1.5, DB_RANGE * 1.5) / DB_RANGE * (rect.height() / 2.0 - 6.0);
    let db_of = |y: f32| (rect.center().y - y) / (rect.height() / 2.0 - 6.0) * DB_RANGE;
    let grid = Stroke::new(0.5, Color32::from_gray(55));
    for f in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0] {
        let x = x_of(f);
        p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], grid);
        let label = if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") };
        p.text(pos2(x + 2.0, rect.bottom() - 1.0), Align2::LEFT_BOTTOM, label, FontId::proportional(9.0), vis.weak_text_color());
    }
    for db in [-12.0, -6.0, 0.0, 6.0, 12.0] {
        let y = y_of(db);
        p.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], if db == 0.0 { Stroke::new(0.8, Color32::from_gray(80)) } else { grid });
        p.text(pos2(rect.left() + 2.0, y), Align2::LEFT_CENTER, format!("{db:+}"), FontId::proportional(9.0), vis.weak_text_color());
    }
    // Curve (drawn even when bypassed, dimmed).
    let preview = Eq { enabled: true, bands: eq.bands.clone() };
    let pts: Vec<_> = (0..rect.width() as usize)
        .map(|i| {
            let x = rect.left() + i as f32;
            pos2(x, y_of(eq_response_db(&preview, f_of(x), RATE)))
        })
        .collect();
    let color = if eq.enabled { GREEN } else { Color32::from_gray(110) };
    p.add(egui::Shape::line(pts, Stroke::new(2.0, color)));

    // Band handles.
    for (i, b) in eq.bands.iter_mut().enumerate() {
        if !b.enabled {
            continue;
        }
        let y = if b.kind.has_gain() { y_of(b.gain_db) } else { y_of(Coeffs::design(b, RATE).magnitude_db(b.freq, RATE)) };
        let c = pos2(x_of(b.freq), y);
        let r = ui.interact(Rect::from_center_size(c, vec2(16.0, 16.0)), id.with(("band", i)), Sense::drag());
        let col = if r.hovered() || r.dragged() { AMBER } else { BLUE };
        p.circle_filled(c, 6.0, col);
        p.text(c, Align2::CENTER_CENTER, format!("{}", i + 1), FontId::proportional(9.0), Color32::WHITE);
        if r.dragged()
            && let Some(pos) = r.interact_pointer_pos()
        {
            b.freq = f_of(pos.x.clamp(rect.left(), rect.right())).round();
            if b.kind.has_gain() {
                b.gain_db = (db_of(pos.y) * 10.0).round() / 10.0;
                b.gain_db = b.gain_db.clamp(-24.0, 24.0);
            }
        }
        if r.hovered() {
            let (scroll, _) = ui.input(|i| (i.smooth_scroll_delta.y, 0));
            if scroll != 0.0 {
                b.q = (b.q * (scroll * 0.004).exp()).clamp(0.1, 18.0);
            }
            r.on_hover_text(format!("{} {:.0} Hz  {:+.1} dB  Q {:.2}\n{}", b.kind.label(), b.freq, b.gain_db, b.q, t("Drag to move · scroll to change Q")));
        }
    }
}
