//! Stage outputs. The stage windows themselves live in the video output process
//! ([`crate::vout`]): this side tells it which windows to open (and how), and takes back what
//! the user does in them (corner pins dragged in mapping mode, windows closed, fullscreen).
//! The drawing helpers below are for the geometry preview.

use super::App;
use crate::geometry::LayerQuad;
use crate::media::video::{Pixels, StageLayer};
use crate::model::CueId;
use crate::show::Show;
use egui::epaint::Vertex;
use egui::{Color32, Mesh, Pos2, Shape, pos2};
use std::collections::HashMap;

/// Mesh subdivisions per side.
const GRID: usize = 16;

impl App {
    /// Keep the video output's windows in step with the Video menu and settings, and apply what
    /// the user did in them. Called every frame under the show lock (it only compares and
    /// queues messages).
    #[cfg(feature = "video")]
    pub(super) fn stages(&mut self, ctx: &egui::Context, show: &mut Show) {
        use crate::vout::link::StageEvent;
        {
            let d = self.displays.lock();
            let needed = !d.loading && !d.loaded;
            drop(d);
            if needed {
                self.refresh_displays(ctx);
            }
        }
        let stages = show.ws.stages.clone();
        self.stage_open.retain(|id| stages.iter().any(|s| s.id == *id));
        let Some(link) = show.media.video.as_mut() else { return };
        let mut corners = Vec::new();
        for e in link.take_stage_events() {
            match e {
                StageEvent::Opened(id) => {
                    self.stage_open.insert(id);
                }
                StageEvent::Closed(id) => {
                    self.stage_open.remove(&id);
                    self.stage_fullscreen.remove(&id);
                    self.mapping_edit.remove(&id);
                }
                StageEvent::Fullscreen(id, true) => {
                    self.stage_fullscreen.insert(id);
                }
                StageEvent::Fullscreen(id, false) => {
                    self.stage_fullscreen.remove(&id);
                }
                StageEvent::Corners(id, c) => corners.push((id, c)),
                StageEvent::Snapshot(r) => {
                    match r {
                        Ok(p) => log::info!("stage snapshot saved: {}", p.display()),
                        Err(e) => log::warn!("stage snapshot failed: {e}"),
                    }
                    self.debug_stage_shot = Some(true);
                }
            }
        }
        let sorted = |set: &std::collections::HashSet<crate::model::StageId>| {
            stages.iter().map(|s| s.id).filter(|id| set.contains(id)).collect::<Vec<_>>()
        };
        link.set_windows(crate::vout::proto::Windows {
            open: sorted(&self.stage_open),
            mapping: sorted(&self.mapping_edit),
            fullscreen: sorted(&self.stage_fullscreen),
        });
        link.set_stages(&stages);
        if !corners.is_empty() {
            for (id, c) in corners {
                if let Some(s) = show.ws.stages.iter_mut().find(|s| s.id == id) {
                    s.corners = c;
                }
            }
            show.edited_outside_cues();
        }
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
    if !f.has_picture() {
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
    let q = crate::geometry::layer_quad(canvas, f.stage_size(), l.fit, &l.geometry);
    let mut out = Vec::new();
    match &f.planes {
        None => {
            // Only still pictures reach the preview (movies are shown by the video process).
            let Pixels::Rgba { data, premultiplied } = &f.pixels else { return Vec::new() };
            let id = tex(0, &|| {
                if *premultiplied {
                    egui::ColorImage::from_rgba_premultiplied(size, data)
                } else {
                    egui::ColorImage::from_rgba_unmultiplied(size, data)
                }
            });
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
    q: &LayerQuad,
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
