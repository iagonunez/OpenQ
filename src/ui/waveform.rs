//! Waveform display with draggable start/end trim markers, zoom and pan.

use super::widgets::{AMBER, BLUE, GREEN, RED, fmt_time};
use crate::model::SliceMarker;
use crate::media::decoder::Waveform;
use egui::{Align2, Color32, CursorIcon, FontId, Id, Rect, Sense, Stroke, Ui, pos2, vec2};

const HEIGHT: f32 = 130.0;
const RULER: f32 = 16.0;
const MIN_REGION: f64 = 0.01;

/// Visible time range, kept in egui memory per waveform.
#[derive(Clone, Copy)]
struct View {
    t0: f64,
    t1: f64,
}

/// Returns true if `start`/`end`/slices changed.
#[allow(clippy::too_many_arguments)]
pub fn waveform_editor(
    ui: &mut Ui,
    id: Id,
    wf: Option<&Waveform>,
    duration: f64,
    start: &mut f64,
    end: &mut Option<f64>,
    slices: &mut Vec<SliceMarker>,
    playhead: Option<f64>,
) -> bool {
    let duration = duration.max(MIN_REGION);
    let mut view = ui.data(|d| d.get_temp::<View>(id)).unwrap_or(View { t0: 0.0, t1: duration });
    let mut changed = false;

    ui.horizontal(|ui| {
        let span = view.t1 - view.t0;
        if super::theme::icon_button(ui, super::icons::ZOOM_OUT, "Zoom out").clicked() {
            let c = (view.t0 + view.t1) / 2.0;
            zoom(&mut view, duration, c, 2.0);
        }
        if super::theme::icon_button(ui, super::icons::ZOOM_IN, "Zoom in (or Ctrl+scroll on the waveform)").clicked() {
            let c = (view.t0 + view.t1) / 2.0;
            zoom(&mut view, duration, c, 0.5);
        }
        if super::theme::icon_button(ui, super::icons::FIT_SCREEN, "Show the whole file").clicked() {
            view = View { t0: 0.0, t1: duration };
        }
        if super::theme::icon_button(ui, super::icons::CROP_FREE, "Zoom to the start–end region").clicked() {
            let e = end.unwrap_or(duration);
            let pad = (e - *start) * 0.05;
            view = View { t0: (*start - pad).max(0.0), t1: (e + pad).min(duration) };
        }
        ui.label(egui::RichText::new(format!("view {} – {}  ({:.0}×)", fmt_time(view.t0), fmt_time(view.t1), duration / span)).weak().small());
    });

    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), HEIGHT), Sense::click_and_drag());
    let wave = Rect::from_min_max(pos2(rect.left(), rect.top() + RULER), rect.max);
    let p = ui.painter_at(rect);
    let vis = ui.visuals();
    p.rect_filled(rect, 3.0, vis.extreme_bg_color);

    let span = (view.t1 - view.t0).max(1e-6);
    let v0 = view.t0;
    let x_of = |t: f64| wave.left() + ((t - v0) / span) as f32 * wave.width();
    let t_of = |x: f32| v0 + ((x - wave.left()) / wave.width()) as f64 * span;

    // Ruler with "nice" tick spacing.
    let step = nice_step(span / 8.0);
    let mut t = (view.t0 / step).ceil() * step;
    while t <= view.t1 {
        let x = x_of(t);
        p.line_segment([pos2(x, rect.top()), pos2(x, rect.top() + RULER * 0.5)], Stroke::new(1.0, vis.weak_text_color()));
        p.text(pos2(x + 2.0, rect.top()), Align2::LEFT_TOP, fmt_time(t), FontId::monospace(9.0), vis.weak_text_color());
        t += step;
    }

    let end_t = end.unwrap_or(duration);
    let mid = wave.center().y;
    let half = wave.height() / 2.0 - 2.0;
    match wf {
        Some(wf) => {
            let cols = wave.width().max(1.0) as usize;
            for c in 0..cols {
                let x = wave.left() + c as f32 + 0.5;
                let (a, b) = (t_of(x - 0.5), t_of(x + 0.5));
                if a > wf.duration {
                    break;
                }
                let [mn, mx] = wf.range(a, b);
                let inside = a >= *start && a < end_t;
                let color = if inside { Color32::from_rgb(90, 170, 230) } else { Color32::from_gray(80) };
                p.line_segment([pos2(x, mid - mx * half), pos2(x, mid - mn.min(mx - 0.002) * half)], Stroke::new(1.0, color));
            }
        }
        None => {
            p.text(wave.center(), Align2::CENTER_CENTER, "building waveform…", FontId::proportional(13.0), vis.weak_text_color());
        }
    }
    p.line_segment([pos2(wave.left(), mid), pos2(wave.right(), mid)], Stroke::new(0.5, Color32::from_gray(60)));

    // Dim outside the region.
    let shade = Color32::from_black_alpha(110);
    let xs = x_of(*start).clamp(wave.left(), wave.right());
    let xe = x_of(end_t).clamp(wave.left(), wave.right());
    p.rect_filled(Rect::from_x_y_ranges(wave.left()..=xs, wave.y_range()), 0.0, shade);
    p.rect_filled(Rect::from_x_y_ranges(xe..=wave.right(), wave.y_range()), 0.0, shade);

    // Markers + handles.
    let handle = |x: f32| Rect::from_center_size(pos2(x, wave.center().y), vec2(12.0, wave.height()));
    let hs = ui.interact(handle(x_of(*start)), id.with("start"), Sense::drag());
    let he = ui.interact(handle(x_of(end_t)), id.with("end"), Sense::drag());
    for (r, x, c, label) in [(&hs, x_of(*start), GREEN, "start"), (&he, x_of(end_t), RED, "end")] {
        let w = if r.hovered() || r.dragged() { 3.0 } else { 2.0 };
        p.line_segment([pos2(x, wave.top()), pos2(x, wave.bottom())], Stroke::new(w, c));
        p.add(egui::Shape::convex_polygon(
            vec![pos2(x - 6.0, wave.top()), pos2(x + 6.0, wave.top()), pos2(x, wave.top() + 8.0)],
            c,
            Stroke::NONE,
        ));
        if r.hovered() || r.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            p.text(pos2(x + 4.0, wave.bottom() - 2.0), Align2::LEFT_BOTTOM, label, FontId::proportional(10.0), c);
        }
    }
    if hs.dragged()
        && let Some(pos) = hs.interact_pointer_pos()
    {
        *start = t_of(pos.x).clamp(0.0, end_t - MIN_REGION);
        changed = true;
    }
    if he.dragged()
        && let Some(pos) = he.interact_pointer_pos()
    {
        let t = t_of(pos.x).clamp(*start + MIN_REGION, duration);
        *end = if t >= duration - 0.005 { None } else { Some(t) };
        changed = true;
    }

    // Slice markers: drag to move, right-click to delete; double-click the waveform to add.
    let mut remove = None;
    for (i, m) in slices.iter_mut().enumerate() {
        let x = x_of(m.time);
        if !(wave.left()..=wave.right()).contains(&x) {
            continue;
        }
        let r = ui.interact(handle(x), id.with(("slice", i)), Sense::click_and_drag());
        let w = if r.hovered() || r.dragged() { 2.5 } else { 1.5 };
        let dash = 6.0;
        let mut y = wave.top();
        while y < wave.bottom() {
            p.line_segment([pos2(x, y), pos2(x, (y + dash).min(wave.bottom()))], Stroke::new(w, BLUE));
            y += dash * 2.0;
        }
        let label = if m.plays == 0 { "∞".to_string() } else { format!("{}×", m.plays) };
        p.text(pos2(x + 3.0, wave.top() + 2.0), Align2::LEFT_TOP, label, FontId::proportional(11.0), BLUE);
        if r.hovered() || r.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        if r.dragged()
            && let Some(pos) = r.interact_pointer_pos()
        {
            m.time = t_of(pos.x).clamp(*start + MIN_REGION, end_t - MIN_REGION);
            changed = true;
        }
        if r.secondary_clicked() {
            remove = Some(i);
        }
        r.on_hover_text("Slice marker — drag to move, right-click to delete. Set loops in the slice table.");
    }
    if let Some(i) = remove {
        slices.remove(i);
        changed = true;
    }
    if resp.double_clicked()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let t = t_of(pos.x);
        if t > *start + MIN_REGION && t < end_t - MIN_REGION {
            slices.push(SliceMarker { time: t, plays: 1 });
            slices.sort_by(|a, b| a.time.total_cmp(&b.time));
            changed = true;
        }
    }

    if let Some(ph) = playhead {
        let x = x_of(ph);
        if (wave.left()..=wave.right()).contains(&x) {
            p.line_segment([pos2(x, rect.top()), pos2(x, wave.bottom())], Stroke::new(1.5, AMBER));
        }
    }

    // Pan by dragging the background; Ctrl+scroll zooms around the pointer.
    if resp.dragged() && !hs.dragged() && !he.dragged() {
        let dt = -(resp.drag_delta().x / wave.width()) as f64 * span;
        let dt = dt.clamp(-view.t0, duration - view.t1);
        view.t0 += dt;
        view.t1 += dt;
    }
    if resp.hovered() {
        let (scroll, ctrl) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.command));
        if ctrl
            && scroll != 0.0
            && let Some(pos) = resp.hover_pos()
        {
            zoom(&mut view, duration, t_of(pos.x), (-scroll as f64 * 0.005).exp());
        }
    }

    ui.data_mut(|d| d.insert_temp(id, view));
    changed
}

fn zoom(view: &mut View, duration: f64, around: f64, factor: f64) {
    let span = ((view.t1 - view.t0) * factor).clamp(0.05, duration);
    let frac = ((around - view.t0) / (view.t1 - view.t0)).clamp(0.0, 1.0);
    let mut t0 = around - span * frac;
    t0 = t0.clamp(0.0, (duration - span).max(0.0));
    *view = View { t0, t1: t0 + span };
}

fn nice_step(raw: f64) -> f64 {
    let steps = [0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0];
    steps.iter().copied().find(|s| *s >= raw).unwrap_or(600.0)
}
