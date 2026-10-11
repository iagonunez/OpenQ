//! Stage output windows. Each stage composites its video layers onto a canvas, which is warped
//! onto the window through four corner pins (keystone / projection mapping).

use super::App;
use super::widgets::{AMBER, GREEN};
use crate::geometry::{Homography, layer_quad};
use crate::model::{Stage, StageId};
use crate::show::Show;
use egui::epaint::Vertex;
use egui::{Color32, Mesh, Pos2, Rect, Sense, Shape, Stroke, ViewportBuilder, ViewportCommand, ViewportId, pos2, vec2};

/// Mesh subdivisions per side: enough for perspective warps to look straight.
const GRID: usize = 16;

pub(super) fn viewport_id(stage: StageId) -> ViewportId {
    ViewportId::from_hash_of(("openq-stage", stage))
}

fn to_screen(h: &Homography, rect: Rect, u: f32, v: f32) -> Pos2 {
    let [x, y] = h.map(u, v);
    pos2(rect.left() + x * rect.width(), rect.top() + y * rect.height())
}

impl App {
    /// Index (in winit's monitor order) of the display a stage should be fullscreen on.
    fn monitor_index(&self, stage: &Stage) -> Option<usize> {
        let name = stage.output.as_ref()?;
        self.displays.lock().list.iter().position(|d| &d.name == name)
    }

    /// Draw every open stage window. Returns true if any video is on screen.
    pub(super) fn stages(&mut self, ctx: &egui::Context, show: &mut Show) -> bool {
        {
            let d = self.displays.lock();
            let needed = !d.loaded && !d.loading;
            drop(d);
            if needed {
                self.refresh_displays(ctx);
            }
        }
        let layers = show.media.stage_layers();
        let first = show.ws.stages.first().map(|s| s.id);
        let ids: Vec<StageId> = show.ws.stages.iter().map(|s| s.id).collect();
        let resolve = |s: Option<StageId>| s.filter(|id| ids.contains(id)).or(first);
        self.textures.retain(|id, _| layers.iter().any(|l| l.id == *id));
        // Stages showing video open automatically.
        for l in &layers {
            if let Some(s) = resolve(l.stage) {
                self.stage_open.insert(s);
            }
        }
        self.stage_open.retain(|id| show.ws.stages.iter().any(|s| s.id == *id));

        let stages = show.ws.stages.clone();
        for stage in stages {
            if !self.stage_open.contains(&stage.id) {
                continue;
            }
            let mine: Vec<_> = layers.iter().filter(|l| resolve(l.stage) == Some(stage.id)).cloned().collect();
            let editing = self.mapping_edit.contains(&stage.id);
            let mut corners = stage.corners;
            let mut close = false;
            let mut toggle_fs = false;
            let aspect = stage.height.max(1) as f32 / stage.width.max(1) as f32;
            let monitor = self.monitor_index(&stage);
            let mut builder = ViewportBuilder::default()
                .with_title(format!("{} — OpenQ Stage", stage.name))
                .with_inner_size([960.0, 960.0 * aspect]);
            if let Some(i) = monitor {
                builder = builder.with_monitor(i);
            }
            let textures = &mut self.textures;
            ctx.show_viewport_immediate(
                viewport_id(stage.id),
                builder,
                |ui, _class| {
                    let ctx = ui.ctx().clone();
                    let rect = ui.max_rect();
                    let painter = ui.painter().clone();
                    painter.rect_filled(rect, 0.0, Color32::BLACK);
                    let h = Homography::square_to_quad(corners);
                    let canvas = [stage.width.max(1) as f32, stage.height.max(1) as f32];

                    if editing {
                        paint_test_pattern(&painter, &h, rect, &stage);
                    }

                    for l in &mine {
                        let f = l.frame.lock();
                        if f.width == 0 || f.data.is_empty() {
                            continue;
                        }
                        let size = [f.width as usize, f.height as usize];
                        let opts = texture_options(f.density);
                        let entry = textures.entry(l.id).or_insert_with(|| {
                            let img = egui::ColorImage::from_rgba_unmultiplied(size, &f.data);
                            (ctx.load_texture(format!("video-{}", l.id), img, opts), f.generation)
                        });
                        if entry.1 != f.generation {
                            let img = egui::ColorImage::from_rgba_unmultiplied(size, &f.data);
                            entry.0.set(img, opts);
                            entry.1 = f.generation;
                        }
                        let img = f.stage_size();
                        drop(f);
                        let q = layer_quad(canvas, img, l.fit, &l.geometry);
                        let tint = Color32::WHITE.gamma_multiply(l.opacity.clamp(0.0, 1.0));
                        painter.add(layer_mesh(entry.0.id(), &q, tint, |u, v| to_screen(&h, rect, u, v)));
                    }

                    if editing {
                        // Corner handles: drag to pin the canvas corners.
                        for (k, c) in corners.iter_mut().enumerate() {
                            let p = pos2(rect.left() + c[0] * rect.width(), rect.top() + c[1] * rect.height());
                            let r = ui.interact(Rect::from_center_size(p, vec2(26.0, 26.0)), ui.id().with(("pin", k)), Sense::drag());
                            if r.dragged() {
                                let d = r.drag_delta();
                                c[0] += d.x / rect.width();
                                c[1] += d.y / rect.height();
                            }
                            let col = if r.hovered() || r.dragged() { AMBER } else { GREEN };
                            painter.circle_stroke(p, 10.0, Stroke::new(2.0, col));
                            painter.circle_filled(p, 3.0, col);
                            if r.hovered() || r.dragged() {
                                ctx.set_cursor_icon(egui::CursorIcon::Move);
                            }
                        }
                        painter.text(
                            rect.center_top() + vec2(0.0, 12.0),
                            egui::Align2::CENTER_TOP,
                            format!("Mapping {} — drag the corners · arrow keys nudge the hovered corner", stage.name),
                            egui::FontId::proportional(14.0),
                            Color32::WHITE,
                        );
                        // Keyboard nudge for the corner nearest the pointer.
                        if let Some(ptr) = ctx.input(|i| i.pointer.hover_pos()) {
                            let near = (0..4).min_by(|a, b| {
                                let d = |k: usize| {
                                    let c = corners[k];
                                    pos2(rect.left() + c[0] * rect.width(), rect.top() + c[1] * rect.height()).distance(ptr)
                                };
                                d(*a).total_cmp(&d(*b))
                            });
                            if let Some(k) = near {
                                let step = 1.0 / rect.width().max(1.0);
                                let (dx, dy) = ctx.input(|i| {
                                    let mut d = (0.0, 0.0);
                                    if i.key_pressed(egui::Key::ArrowLeft) { d.0 -= step; }
                                    if i.key_pressed(egui::Key::ArrowRight) { d.0 += step; }
                                    if i.key_pressed(egui::Key::ArrowUp) { d.1 -= step; }
                                    if i.key_pressed(egui::Key::ArrowDown) { d.1 += step; }
                                    d
                                });
                                corners[k][0] += dx;
                                corners[k][1] += dy;
                            }
                        }
                    } else if mine.is_empty() {
                        painter.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            format!("{} — double-click for fullscreen", stage.name),
                            egui::FontId::proportional(16.0),
                            Color32::from_gray(60),
                        );
                    }

                    let bg = ui.interact(rect, ui.id().with("stage-bg"), Sense::click());
                    if bg.double_clicked() || ctx.input(|i| i.key_pressed(egui::Key::F11)) {
                        toggle_fs = true;
                    }
                    if ctx.input(|i| i.viewport().close_requested()) {
                        close = true;
                    }
                },
            );
            // Follow output changes on an open window (a new window starts on its monitor).
            match self.stage_monitor.insert(stage.id, monitor) {
                Some(prev) if prev != monitor => {
                    let cmd = match monitor {
                        Some(i) => ViewportCommand::SetMonitor(i),
                        None => ViewportCommand::Fullscreen(false),
                    };
                    ctx.send_viewport_cmd_to(viewport_id(stage.id), cmd);
                    if monitor.is_some() {
                        self.stage_fullscreen.insert(stage.id);
                    } else {
                        self.stage_fullscreen.remove(&stage.id);
                    }
                }
                None if monitor.is_some() => {
                    self.stage_fullscreen.insert(stage.id);
                }
                _ => {}
            }
            if toggle_fs {
                let fs = !self.stage_fullscreen.remove(&stage.id);
                if fs {
                    self.stage_fullscreen.insert(stage.id);
                }
                ctx.send_viewport_cmd_to(viewport_id(stage.id), ViewportCommand::Fullscreen(fs));
            }
            if close {
                self.stage_open.remove(&stage.id);
                self.stage_monitor.remove(&stage.id);
                self.stage_fullscreen.remove(&stage.id);
                self.mapping_edit.remove(&stage.id);
            }
            if corners != stage.corners
                && let Some(s) = show.ws.stages.iter_mut().find(|s| s.id == stage.id)
            {
                s.corners = corners;
                show.edited();
            }
        }
        !layers.is_empty()
    }
}

/// Texture filtering for a layer: supersampled pictures (rendered text) get mipmaps, so they
/// stay smooth however small they are drawn. Video frames change every frame, so they skip the
/// mipmap cost.
pub(super) fn texture_options(density: f32) -> egui::TextureOptions {
    if density > 1.0 {
        egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR }
    } else {
        egui::TextureOptions::LINEAR
    }
}

/// A textured layer as a subdivided mesh; `place` maps canvas coordinates to the screen.
pub(super) fn layer_mesh(
    tex: egui::TextureId,
    q: &crate::geometry::LayerQuad,
    tint: Color32,
    place: impl Fn(f32, f32) -> Pos2,
) -> Shape {
    let mut mesh = Mesh::with_texture(tex);
    for j in 0..=GRID {
        for i in 0..=GRID {
            let (s, t) = (i as f32 / GRID as f32, j as f32 / GRID as f32);
            let [u, v] = q.point(s, t);
            let [tu, tv] = q.uv_at(s, t);
            mesh.vertices.push(Vertex { pos: place(u, v), uv: pos2(tu, tv), color: tint });
        }
    }
    let w = (GRID + 1) as u32;
    for j in 0..GRID as u32 {
        for i in 0..GRID as u32 {
            let a = j * w + i;
            mesh.add_triangle(a, a + 1, a + w);
            mesh.add_triangle(a + 1, a + w + 1, a + w);
        }
    }
    Shape::mesh(mesh)
}

/// Grid, border and diagonals drawn through the mapping, for lining up a projector.
fn paint_test_pattern(p: &egui::Painter, h: &Homography, rect: Rect, stage: &Stage) {
    let line = |a: [f32; 2], b: [f32; 2], stroke: Stroke| {
        let pts: Vec<Pos2> = (0..=24)
            .map(|i| {
                let t = i as f32 / 24.0;
                to_screen(h, rect, a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t)
            })
            .collect();
        p.add(Shape::line(pts, stroke));
    };
    // Canvas fill.
    let mut mesh = Mesh::default();
    for [u, v] in [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]] {
        mesh.colored_vertex(to_screen(h, rect, u, v), Color32::from_gray(25));
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(Shape::mesh(mesh));
    let thin = Stroke::new(1.0, Color32::from_gray(110));
    let n = 8;
    for i in 1..n {
        let t = i as f32 / n as f32;
        line([t, 0.0], [t, 1.0], thin);
        line([0.0, t], [1.0, t], thin);
    }
    let bold = Stroke::new(2.0, Color32::WHITE);
    line([0.0, 0.0], [1.0, 0.0], bold);
    line([1.0, 0.0], [1.0, 1.0], bold);
    line([1.0, 1.0], [0.0, 1.0], bold);
    line([0.0, 1.0], [0.0, 0.0], bold);
    let diag = Stroke::new(1.0, AMBER);
    line([0.0, 0.0], [1.0, 1.0], diag);
    line([1.0, 0.0], [0.0, 1.0], diag);
    let c = to_screen(h, rect, 0.5, 0.5);
    p.circle_stroke(c, 30.0, Stroke::new(1.5, AMBER));
    p.text(
        to_screen(h, rect, 0.5, 0.58),
        egui::Align2::CENTER_TOP,
        format!("{} · {}×{}", stage.name, stage.width, stage.height),
        egui::FontId::proportional(16.0),
        Color32::WHITE,
    );
}
