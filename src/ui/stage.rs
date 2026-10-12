//! Stage output windows. Each stage composites its video layers onto a canvas, which is warped
//! onto the window through four corner pins (keystone / projection mapping).
//!
//! A stage can instead show part of another stage (`span_of` + `crop`), so one wide surface
//! is split across several projectors; soft-edge blend zones then darken each output's
//! overlapping edges so the overlap adds up to even light. Everything outside the warped canvas
//! is masked black, and layers with a [`crate::model::BlendMode`] other than Normal draw through
//! [`super::blend`].

use super::App;
use super::blend::{Blended, blend};
use super::widgets::AMBER;
use crate::geometry::{Homography, blend_gain, layer_quad, to_crop};
use crate::media::video::StageLayer;
use crate::model::{CueId, Stage, StageId};
use std::collections::HashMap;
use crate::show::Show;
use egui::epaint::Vertex;
use egui::{Color32, Mesh, Pos2, Rect, Sense, Shape, Stroke, ViewportBuilder, ViewportCommand, ViewportId, pos2, vec2};

/// Mesh subdivisions per side: enough for perspective warps to look straight.
const GRID: usize = 16;

/// Steps across a soft-edge blend zone (the gain curve is drawn as a gradient between them).
const BLEND_STEPS: usize = 48;

/// The stage whose cues `stage` shows: the one it spans, or itself.
pub(super) fn source_of<'a>(stage: &'a Stage, stages: &'a [Stage]) -> &'a Stage {
    stage.span_of.filter(|id| *id != stage.id).and_then(|id| stages.iter().find(|s| s.id == id)).unwrap_or(stage)
}

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
        self.textures.retain(|(id, _), _| layers.iter().any(|l| l.id == *id));
        // Stages showing video open automatically: the layer's own stage, or the outputs spanning
        // it (a surface that only exists to be spanned doesn't open on its own).
        for l in &layers {
            if let Some(s) = resolve(l.stage) {
                let spans: Vec<StageId> = show.ws.stages.iter().filter(|o| o.span_of == Some(s) && o.id != s).map(|o| o.id).collect();
                if spans.is_empty() {
                    self.stage_open.insert(s);
                } else {
                    self.stage_open.extend(spans);
                }
            }
        }
        self.stage_open.retain(|id| show.ws.stages.iter().any(|s| s.id == *id));

        let stages = show.ws.stages.clone();
        for stage in stages.clone() {
            if !self.stage_open.contains(&stage.id) {
                continue;
            }
            let source = source_of(&stage, &stages);
            let source_canvas = [source.width.max(1) as f32, source.height.max(1) as f32];
            let crop = if source.id == stage.id { Stage::FULL_CROP } else { stage.crop };
            let mine: Vec<_> = layers.iter().filter(|l| resolve(l.stage) == Some(source.id)).cloned().collect();
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
            let gpu = self.gpu.clone();
            ctx.show_viewport_immediate(
                viewport_id(stage.id),
                builder,
                |ui, _class| {
                    let ctx = ui.ctx().clone();
                    let rect = ui.max_rect();
                    let painter = ui.painter().clone();
                    painter.rect_filled(rect, 0.0, Color32::BLACK);
                    let h = Homography::square_to_quad(corners);

                    if editing {
                        paint_test_pattern(&painter, &h, rect, &stage);
                    }

                    // Source-canvas point → this output's canvas (its crop of the source) → screen.
                    let place = |u: f32, v: f32| {
                        let [x, y] = to_crop([u, v], crop);
                        to_screen(&h, rect, x, y)
                    };
                    for l in &mine {
                        let shapes = paint_layer(&ctx, textures, l, source_canvas, place);
                        match blend(gpu.as_ref(), l.blend_mode, shapes, rect) {
                            Blended::Shape(s) => {
                                painter.add(s);
                            }
                            Blended::Plain(v) => painter.extend(v),
                            Blended::NotReady => ctx.request_repaint(),
                        }
                    }
                    paint_edge_blend(&painter, &h, rect, stage.blend, stage.blend_gamma);
                    paint_outside_mask(&painter, &h, rect);

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
                            paint_pin(&painter, p, k, r.hovered() || r.dragged());
                            if r.hovered() || r.dragged() {
                                ctx.set_cursor_icon(egui::CursorIcon::Move);
                            }
                        }
                        // Instructions on a dark pill so they read over the white pattern.
                        let msg = format!(
                            "Mapping {} — drag the corners · arrow keys nudge the hovered corner (Shift: 10 px)",
                            stage.name
                        );
                        let galley = painter.layout_no_wrap(msg, egui::FontId::proportional(14.0), Color32::WHITE);
                        let pill = Rect::from_center_size(
                            rect.center_top() + vec2(0.0, 24.0),
                            galley.size() + vec2(24.0, 12.0),
                        );
                        painter.rect_filled(pill, 8.0, Color32::from_black_alpha(200));
                        painter.galley(pill.min + vec2(12.0, 6.0), galley, Color32::WHITE);
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
                                let px = if ctx.input(|i| i.modifiers.shift) { 10.0 } else { 1.0 };
                                let (sx, sy) = (px / rect.width().max(1.0), px / rect.height().max(1.0));
                                let (dx, dy) = ctx.input(|i| {
                                    let mut d = (0.0, 0.0);
                                    if i.key_pressed(egui::Key::ArrowLeft) { d.0 -= sx; }
                                    if i.key_pressed(egui::Key::ArrowRight) { d.0 += sx; }
                                    if i.key_pressed(egui::Key::ArrowUp) { d.1 -= sy; }
                                    if i.key_pressed(egui::Key::ArrowDown) { d.1 += sy; }
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
        // Animations move even when the engine is idle (a held last frame keeps animating).
        if layers.iter().any(|l| l.animated) {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
        !layers.is_empty()
    }
}

/// Soft-edge blend zones: black gradients whose transparency follows [`blend_gain`] from the
/// outer edge (dark) to the inner edge of each zone (`widths`: left, top, right, bottom as a
/// fraction of the output). Where two zones meet in a corner their gains multiply.
fn paint_edge_blend(p: &egui::Painter, h: &Homography, rect: Rect, widths: [f32; 4], gamma: f32) {
    for (side, w) in widths.into_iter().enumerate() {
        let w = w.clamp(0.0, 1.0);
        if w <= 0.0 {
            continue;
        }
        // (a, b): a across the zone from the outer edge (0) to the inner edge (1), b along it.
        let at = |a: f32, b: f32| -> (f32, f32) {
            match side {
                0 => (a * w, b),
                1 => (b, a * w),
                2 => (1.0 - a * w, b),
                _ => (b, 1.0 - a * w),
            }
        };
        let mut mesh = Mesh::default();
        for i in 0..=BLEND_STEPS {
            let a = i as f32 / BLEND_STEPS as f32;
            let alpha = 1.0 - blend_gain(a, gamma);
            let col = Color32::from_black_alpha((alpha * 255.0).round() as u8);
            for j in 0..=GRID {
                let (u, v) = at(a, j as f32 / GRID as f32);
                mesh.colored_vertex(to_screen(h, rect, u, v), col);
            }
        }
        let n = (GRID + 1) as u32;
        for i in 0..BLEND_STEPS as u32 {
            for j in 0..GRID as u32 {
                let k = i * n + j;
                mesh.add_triangle(k, k + 1, k + n);
                mesh.add_triangle(k + 1, k + n + 1, k + n);
            }
        }
        p.add(Shape::mesh(mesh));
    }
}

/// Paint black over everything outside the warped canvas, so layers that reach past the canvas
/// (or past this output's crop of a spanned surface) don't spill onto the screen. Each edge masks
/// the half-plane beyond it; a homography keeps the edges straight.
fn paint_outside_mask(p: &egui::Painter, h: &Homography, rect: Rect) {
    let q: Vec<Pos2> = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)].iter().map(|&(u, v)| to_screen(h, rect, u, v)).collect();
    let centre = pos2(q.iter().map(|c| c.x).sum::<f32>() / 4.0, q.iter().map(|c| c.y).sum::<f32>() / 4.0);
    let far = (rect.width() + rect.height()) * 4.0;
    for k in 0..4 {
        let (a, b) = (q[k], q[(k + 1) % 4]);
        let d = (b - a).normalized();
        if !d.x.is_finite() {
            continue;
        }
        let mut n = vec2(-d.y, d.x);
        if n.dot(centre - a) > 0.0 {
            n = -n;
        }
        let (a2, b2) = (a - d * far, b + d * far);
        let pts = vec![a2, b2, b2 + n * far, a2 + n * far];
        p.add(Shape::convex_polygon(pts, Color32::BLACK, Stroke::NONE));
    }
}

/// Stage textures: one per layer, or one per plane of a Text cue (`(cue, plane)`), with the
/// frame generation they hold.
pub(super) type LayerTextures = HashMap<(CueId, usize), (egui::TextureHandle, u64)>;

/// Vertex colour for a layer drawn in colour `c` (sRGB RGBA 0..1): tinted, and faded by the
/// layer's opacity.
fn layer_color(l: &StageLayer, c: [f32; 4]) -> Color32 {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let t = l.tint;
    Color32::from_rgba_unmultiplied(b(c[0] * t[0]), b(c[1] * t[1]), b(c[2] * t[2]), b(c[3] * t[3] * l.opacity))
}

/// Upload a layer's picture when it changed and return the meshes that draw it: one for a
/// movie or still, one per coloured plane for a Text cue.
pub(super) fn paint_layer(
    ctx: &egui::Context,
    textures: &mut LayerTextures,
    l: &StageLayer,
    canvas: [f32; 2],
    place: impl Fn(f32, f32) -> Pos2,
) -> Vec<Shape> {
    let f = l.frame.lock();
    if f.width == 0 || (f.data.is_empty() && f.planes.is_none()) {
        return Vec::new();
    }
    // egui panics on a texture bigger than the GPU takes (an 8K movie on a small GPU): skip it.
    let max = ctx.input(|i| i.max_texture_side);
    if f.width.max(f.height) as usize > max {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| log::error!("a {}×{} picture is larger than this GPU's {max} px textures; not shown", f.width, f.height));
        return Vec::new();
    }
    let size = [f.width as usize, f.height as usize];
    let opts = texture_options(f.density);
    let mut tex = |k: usize, make: &dyn Fn() -> egui::ColorImage| {
        let e = textures
            .entry((l.id, k))
            .or_insert_with(|| (ctx.load_texture(format!("video-{}-{k}", l.id), make(), opts), f.generation));
        if e.1 != f.generation {
            e.0.set(make(), opts);
            e.1 = f.generation;
        }
        e.0.id()
    };
    let q = layer_quad(canvas, f.stage_size(), l.fit, &l.geometry);
    let mut out = Vec::new();
    match &f.planes {
        None => {
            let id = tex(0, &|| egui::ColorImage::from_rgba_unmultiplied(size, &f.data));
            out.push(layer_mesh(id, &q, layer_color(l, [1.0; 4]), &place));
        }
        Some(p) => {
            // Coverage masks become white-with-alpha textures that the vertex colour paints.
            for (k, (part, mask)) in p.masks.iter().enumerate() {
                let c = l.text_colors[*part as usize];
                if c[3] <= 0.0 {
                    continue;
                }
                let make = || egui::ColorImage::new(size, mask.iter().map(|&a| Color32::from_rgba_premultiplied(a, a, a, a)).collect::<Vec<_>>());
                let id = tex(k, &make);
                out.push(layer_mesh(id, &q, layer_color(l, c), &place));
            }
            if let Some(rgba) = &p.colour {
                let id = tex(p.masks.len(), &|| egui::ColorImage::from_rgba_unmultiplied(size, rgba));
                out.push(layer_mesh(id, &q, layer_color(l, [1.0, 1.0, 1.0, l.text_colors[3][3]]), &place));
            }
        }
    }
    out
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

/// The mapping test pattern: a white canvas with a grid, pixel rulers along every
/// edge, circles and a centre cross, all drawn through the corner-pin warp, so keystone and
/// straightness can be judged on the real screen. Circles that come out as ellipses or grid
/// squares that aren't square show what is still off.
fn paint_test_pattern(p: &egui::Painter, h: &Homography, rect: Rect, stage: &Stage) {
    let (w, ht) = (stage.width.max(1) as f32, stage.height.max(1) as f32);
    let at = |x: f32, y: f32| to_screen(h, rect, x / w, y / ht);
    // A straight canvas line, subdivided so the perspective warp keeps it straight on screen.
    let line = |a: [f32; 2], b: [f32; 2], stroke: Stroke| {
        let pts: Vec<Pos2> = (0..=16).map(|i| {
            let t = i as f32 / 16.0;
            at(a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t)
        }).collect();
        p.add(Shape::line(pts, stroke));
    };
    let circle = |c: [f32; 2], r: f32, stroke: Stroke| {
        let pts: Vec<Pos2> = (0..=96).map(|i| {
            let a = i as f32 / 96.0 * std::f32::consts::TAU;
            at(c[0] + r * a.cos(), c[1] + r * a.sin())
        }).collect();
        p.add(Shape::line(pts, stroke));
    };
    let black = Color32::from_gray(20);
    let ink = |width: f32, gray: u8| Stroke::new(width, Color32::from_gray(gray));

    // White canvas.
    let mut mesh = Mesh::default();
    for [x, y] in [[0.0, 0.0], [w, 0.0], [w, ht], [0.0, ht]] {
        mesh.colored_vertex(at(x, y), Color32::WHITE);
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(Shape::mesh(mesh));

    // Grid: square cells, a tenth of the height, centred on the canvas.
    let cell = ht / 10.0;
    let (cx, cy) = (w / 2.0, ht / 2.0);
    let n = (w / cell / 2.0).ceil() as i32;
    for k in -n..=n {
        let x = cx + k as f32 * cell;
        if x > 0.0 && x < w {
            line([x, 0.0], [x, ht], ink(1.0, if k % 5 == 0 { 120 } else { 190 }));
        }
    }
    for k in -5..=5 {
        let y = cy + k as f32 * cell;
        if y > 0.0 && y < ht {
            line([0.0, y], [w, y], ink(1.0, if k % 5 == 0 { 120 } else { 190 }));
        }
    }
    // Diagonals, circles and the centre cross.
    line([0.0, 0.0], [w, ht], ink(1.0, 160));
    line([w, 0.0], [0.0, ht], ink(1.0, 160));
    let r = ht.min(w) / 2.0;
    circle([cx, cy], r * 0.92, ink(2.5, 20));
    circle([cx, cy], r * 0.5, ink(1.5, 60));
    for (qx, qy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
        circle([w * qx, ht * qy], r * 0.22, ink(1.5, 60));
    }
    line([cx - cell, cy], [cx + cell, cy], ink(2.0, 20));
    line([cx, cy - cell], [cx, cy + cell], ink(2.0, 20));

    // Rulers: ticks every 10 px, longer every 50 and 100 px, numbered as often as the screen allows.
    let scale = rect.width() / w; // screen points per canvas pixel (roughly)
    let label_every = [100.0, 200.0, 500.0, 1000.0].into_iter().find(|s| s * scale >= 46.0).unwrap_or(1000.0);
    let font = egui::FontId::monospace(10.0);
    let mut x = 0.0;
    while x <= w {
        let len = if x % 100.0 == 0.0 { 24.0 } else if x % 50.0 == 0.0 { 14.0 } else { 7.0 };
        let stroke = ink(1.0, 20);
        line([x, 0.0], [x, len], stroke);
        line([x, ht], [x, ht - len], stroke);
        if x % label_every == 0.0 && x > 150.0 && x < w - 150.0 {
            p.text(at(x + 3.0, 26.0), egui::Align2::LEFT_TOP, format!("{x}"), font.clone(), black);
            p.text(at(x + 3.0, ht - 26.0), egui::Align2::LEFT_BOTTOM, format!("{x}"), font.clone(), black);
        }
        x += 10.0;
    }
    let mut y = 0.0;
    while y <= ht {
        let len = if y % 100.0 == 0.0 { 24.0 } else if y % 50.0 == 0.0 { 14.0 } else { 7.0 };
        let stroke = ink(1.0, 20);
        line([0.0, y], [len, y], stroke);
        line([w, y], [w - len, y], stroke);
        if y % label_every == 0.0 && y > 120.0 && y < ht - 120.0 {
            p.text(at(27.0, y + 2.0), egui::Align2::LEFT_TOP, format!("{y}"), font.clone(), black);
            p.text(at(w - 27.0, y + 2.0), egui::Align2::RIGHT_TOP, format!("{y}"), font.clone(), black);
        }
        y += 10.0;
    }
    // Border.
    let border = ink(3.0, 20);
    line([0.0, 0.0], [w, 0.0], border);
    line([w, 0.0], [w, ht], border);
    line([w, ht], [0.0, ht], border);
    line([0.0, ht], [0.0, 0.0], border);

    // Name and size under the centre.
    p.text(
        at(cx, cy + cell * 1.4),
        egui::Align2::CENTER_TOP,
        format!("{}  ·  {} × {}", stage.name, stage.width, stage.height),
        egui::FontId::proportional(18.0),
        black,
    );
}

/// A corner pin handle: a ring with a cross and the corner's number.
fn paint_pin(p: &egui::Painter, at: Pos2, k: usize, hot: bool) {
    let col = if hot { AMBER } else { Color32::from_rgb(0x4a, 0x7c, 0xf0) };
    p.circle_filled(at, 16.0, col.gamma_multiply(0.25));
    p.circle_stroke(at, 16.0, Stroke::new(2.5, col));
    p.line_segment([at - vec2(24.0, 0.0), at + vec2(24.0, 0.0)], Stroke::new(1.5, col));
    p.line_segment([at - vec2(0.0, 24.0), at + vec2(0.0, 24.0)], Stroke::new(1.5, col));
    // The number sits on the side facing into the picture.
    let off = vec2(if k == 1 || k == 2 { -26.0 } else { 26.0 }, if k >= 2 { -26.0 } else { 26.0 });
    p.circle_filled(at + off, 10.0, col);
    p.text(at + off, egui::Align2::CENTER_CENTER, format!("{}", k + 1), egui::FontId::proportional(12.0), Color32::WHITE);
}
