//! The timeline of a Timeline group (inspector Settings tab): one row per child, a bar from its
//! start (its pre-wait) to its end where the length is known. Dragging a bar moves the child's
//! start, snapped to tenths of a second and to the other children's starts and ends (Shift: free).

use super::theme;
use super::widgets::fmt_time;
use crate::i18n::t;
use crate::media::Prober;
use crate::model::{Cue, CueKind};
use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, Ui, pos2, vec2};

const LABEL_W: f32 = 170.0;
const ROW_H: f32 = 24.0;
const RULER_H: f32 = 20.0;
/// Snap to another child's start or end within this many pixels.
const SNAP_PX: f32 = 6.0;

/// Seconds a child's action lasts, if known.
fn length(c: &Cue, prober: &Prober) -> Option<f64> {
    match &c.kind {
        CueKind::Wait { duration } => Some(*duration),
        CueKind::Fade(f) => Some(f.duration),
        CueKind::Audio(_) | CueKind::Video(_) => crate::media::effective_duration(c, c.kind.media_path().and_then(|p| prober.get(p)).and_then(|r| r.ok()).and_then(|i| i.duration)),
        _ => None,
    }
}

/// A tick step (seconds) giving at least `min_px` between ticks.
fn tick_step(span: f64, width: f32, min_px: f32) -> f64 {
    const STEPS: [f64; 14] = [0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0];
    STEPS.into_iter().find(|s| (s / span) as f32 * width >= min_px).unwrap_or(3600.0)
}

/// Where a dragged start lands: snapped to `edges` within [`SNAP_PX`], else to tenths of a second
/// (unless `free`), never before 0.
fn snapped(raw: f64, edges: &[f64], px_per_sec: f32, free: bool) -> f64 {
    let raw = raw.max(0.0);
    if free {
        return raw;
    }
    if let Some(e) = edges.iter().copied().filter(|e| ((e - raw).abs() as f32) * px_per_sec <= SNAP_PX).min_by(|a, b| (a - raw).abs().total_cmp(&(b - raw).abs())) {
        return e;
    }
    (raw * 10.0).round() / 10.0
}

/// Draw the timeline and let the children's starts be dragged. `elapsed` = seconds the group has
/// been playing (a line marks it). Returns true if a start moved.
pub(super) fn group_timeline(ui: &mut Ui, children: &mut [Cue], prober: &Prober, elapsed: Option<f64>) -> bool {
    if children.is_empty() {
        ui.label(egui::RichText::new(t("Drag cues onto the middle of a group row to put them inside it.")).weak().small());
        return false;
    }
    let lengths: Vec<Option<f64>> = children.iter().map(|c| length(c, prober)).collect();
    let end = children.iter().zip(&lengths).map(|(c, l)| c.pre_wait.max(0.0) + l.unwrap_or(0.0)).fold(0.0, f64::max);
    // The scale stays put while a bar is dragged (it would follow the bar otherwise).
    let span_id = ui.id().with("timeline-span");
    let dragging = ui.ctx().dragged_id().is_some_and(|d| ui.data(|m| m.get_temp::<egui::Id>(span_id.with("bar")) == Some(d)));
    let span = match ui.data(|m| m.get_temp::<f64>(span_id)) {
        Some(s) if dragging => s,
        _ => (end * 1.15).max(10.0),
    };
    let width = ui.available_width().max(LABEL_W + 240.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, RULER_H + ROW_H * children.len() as f32 + 4.0), Sense::hover());
    let track = Rect::from_min_max(pos2(rect.left() + LABEL_W, rect.top()), pos2(rect.right() - 8.0, rect.bottom()));
    let px_per_sec = track.width() / span as f32;
    let x_of = |t: f64| track.left() + t as f32 * px_per_sec;
    let p = ui.painter_at(rect);
    let mono = theme::mono(11.0);

    // Ruler and grid.
    let step = tick_step(span, track.width(), 64.0);
    let mut tick = 0.0;
    while tick <= span {
        let x = x_of(tick);
        p.line_segment([pos2(x, rect.top() + RULER_H - 5.0), pos2(x, rect.bottom())], Stroke::new(1.0, theme::BORDER));
        p.text(pos2(x + 3.0, rect.top() + 2.0), Align2::LEFT_TOP, fmt_time(tick), mono.clone(), theme::TEXT_FAINT);
        tick += step;
    }

    let edges: Vec<f64> = children.iter().zip(&lengths).flat_map(|(c, l)| [Some(c.pre_wait.max(0.0)), l.map(|l| c.pre_wait.max(0.0) + l)]).flatten().chain([0.0]).collect();
    let free = ui.input(|i| i.modifiers.shift);
    let mut changed = false;
    for (i, (c, len)) in children.iter_mut().zip(&lengths).enumerate() {
        let top = rect.top() + RULER_H + i as f32 * ROW_H;
        let row = Rect::from_min_max(pos2(rect.left(), top), pos2(rect.right(), top + ROW_H));
        if i % 2 == 1 {
            p.rect_filled(row, 0.0, theme::BG_RAISED.gamma_multiply(0.5));
        }
        let label = format!("{} {}", c.number, c.display_name()).trim().to_string();
        let colour = if c.armed { theme::TEXT } else { theme::TEXT_FAINT };
        let galley = p.layout_no_wrap(label, FontId::proportional(13.0), colour);
        let clip = Rect::from_min_max(pos2(rect.left() + 4.0, top), pos2(track.left() - 6.0, top + ROW_H));
        ui.painter_at(clip).galley(pos2(clip.left(), row.center().y - galley.size().y / 2.0), galley, colour);

        let start = c.pre_wait.max(0.0);
        let x0 = x_of(start);
        let x1 = len.map_or(x0 + 10.0, |l| x_of(start + l).max(x0 + 6.0));
        let bar = Rect::from_min_max(pos2(x0, top + 4.0), pos2(x1.min(track.right()), top + ROW_H - 4.0));
        let id = ui.id().with(("timeline-bar", c.id));
        let resp = ui.interact(bar.expand2(vec2(2.0, 0.0)), id, Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        if resp.drag_started() {
            let x = resp.interact_pointer_pos().map_or(x0, |p| p.x);
            ui.data_mut(|m| {
                m.insert_temp(span_id, span);
                m.insert_temp(span_id.with("bar"), id);
                m.insert_temp(id, (start, x));
            });
        }
        if resp.dragged()
            && let (Some((orig, x)), Some(now)) = (ui.data(|m| m.get_temp::<(f64, f32)>(id)), resp.interact_pointer_pos())
        {
            let others: Vec<f64> = edges.iter().copied().filter(|e| *e != start).collect();
            let new = snapped(orig + ((now.x - x) / px_per_sec) as f64, &others, px_per_sec, free).min(crate::limits::MAX_TIME);
            if new != c.pre_wait {
                c.pre_wait = new;
                changed = true;
            }
        }
        let fill = if !c.armed {
            theme::TEXT_FAINT.gamma_multiply(0.5)
        } else if resp.hovered() || resp.dragged() {
            theme::BLUE
        } else {
            theme::ACCENT
        };
        p.rect_filled(bar, 3.0, fill);
        if len.is_none() {
            // Length unknown (a group, a cue that runs until stopped): a fading tail.
            p.rect_filled(Rect::from_min_max(pos2(bar.right(), bar.top()), pos2((bar.right() + 24.0).min(track.right()), bar.bottom())), 0.0, fill.gamma_multiply(0.25));
        }
        resp.on_hover_text(crate::tr!("Starts at {start} (its pre-wait). Drag to move it; Shift: no snapping.", start = fmt_time(c.pre_wait)));
    }
    if let Some(e) = elapsed
        && e <= span
    {
        let x = x_of(e);
        p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.5, theme::GREEN));
        ui.ctx().request_repaint();
    }
    p.line_segment([pos2(track.left(), rect.top()), pos2(track.left(), rect.bottom())], Stroke::new(1.0, Color32::from_gray(70)));
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_snap_to_edges_and_tenths() {
        // 10 px per second: an edge 0.4 s away (4 px) wins over the tenth.
        assert_eq!(snapped(2.43, &[2.8], 10.0, false), 2.8);
        assert_eq!(snapped(2.43, &[5.0], 10.0, false), 2.4);
        assert_eq!(snapped(2.43, &[2.8], 10.0, true), 2.43);
        assert_eq!(snapped(-1.0, &[], 10.0, false), 0.0);
        assert!(tick_step(60.0, 600.0, 64.0) >= 6.4);
    }
}
