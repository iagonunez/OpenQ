//! Geometry preview: a small picture of a video or text cue's stage showing where the layer
//! lands (position, scale, rotation, crop), so geometry can be set up without the projector.
//! While the Animation tab is open, it shows the layer as it looks at the animation playhead,
//! with the cue's own placement as a dashed outline.

use super::stage::{LayerTextures, layer_mesh, paint_layer};
use super::{App, Mode, icons, theme};
use crate::animation::{AnimState, Animation};
use crate::media::text::TextRender;
use crate::media::video::{Frame, FrameSlot, StageLayer};
use crate::geometry::{LayerQuad, layer_quad};
use crate::model::{CueId, CueKind, Geometry, StageId, TextCue, VideoFit};
use crate::show::Show;
use egui::{Color32, Pos2, Rect, RichText, Sense, Shape, Stroke, pos2, vec2};
use std::path::PathBuf;

/// A Text cue's planes for the preview: rendered in the background once per look, then drawn
/// like on stage with the current (or animated) colours, so scrubbing and colour edits cost
/// nothing.
#[derive(Default)]
pub(super) struct PreviewText {
    job: Option<TextRender>,
    frame: FrameSlot,
    textures: LayerTextures,
}

impl PreviewText {
    /// The frame for `tc`, starting a render when its look changed. Empty until the first render.
    fn frame_for(&mut self, tc: &TextCue, ctx: &egui::Context) -> FrameSlot {
        let look = crate::media::text::look(tc);
        if self.job.as_ref().is_none_or(|j| j.look != look) {
            self.job = Some(TextRender::spawn(tc));
        }
        if let Some(job) = &self.job {
            match job.ready() {
                Some(p) if !self.frame.lock().planes.as_ref().is_some_and(|q| std::sync::Arc::ptr_eq(q, &p)) => {
                    let mut f = self.frame.lock();
                    let generation = f.generation.wrapping_add(1);
                    *f = Frame { width: p.width, height: p.height, density: p.density, planes: Some(p), generation, data: Vec::new(), premultiplied: false };
                }
                Some(_) => {}
                None => ctx.request_repaint_after(std::time::Duration::from_millis(15)),
            }
        }
        self.frame.clone()
    }
}

/// What the layer shows.
#[derive(Clone)]
enum Source {
    Poster(PathBuf, f64),
    Text(TextCue),
    /// A live camera: a grey stand-in of its capture size until it runs.
    Camera([u32; 2]),
}

/// The layer being previewed. For a Fade cue it is the fade's target, drawn at the geometry the
/// fade ends on, with the target's own geometry as a ghost.
struct Target {
    layer: CueId,
    stage: Option<StageId>,
    fit: VideoFit,
    geometry: Geometry,
    opacity: f32,
    source: Source,
    ghost: Option<Geometry>,
    /// The layer's animation (empty for a fade's target).
    anim: Animation,
}

fn target_of(show: &Show, id: CueId) -> Option<Target> {
    let cue = show.ws.cue_list.find(id)?;
    let own = |layer: CueId, kind: &CueKind| match kind {
        CueKind::Video(v) => Some(Target {
            layer,
            stage: v.stage,
            fit: v.fit,
            geometry: v.geometry,
            opacity: v.opacity,
            source: Source::Poster(v.path.clone(), v.start_time),
            ghost: None,
            anim: v.animation.clone(),
        }),
        CueKind::Camera(c) => Some(Target {
            layer,
            stage: c.stage,
            fit: c.fit,
            geometry: c.geometry,
            opacity: c.opacity,
            source: Source::Camera(c.size.unwrap_or([1280, 720])),
            ghost: None,
            anim: Animation::default(),
        }),
        CueKind::Text(t) => Some(Target {
            layer,
            stage: t.stage,
            fit: VideoFit::Native,
            geometry: t.geometry,
            opacity: t.opacity,
            source: Source::Text(t.clone()),
            ghost: None,
            anim: t.animation.clone(),
        }),
        _ => None,
    };
    match &cue.kind {
        CueKind::Fade(f) => {
            let g = f.geometry?;
            let t = show.ws.cue_list.find(f.target?)?;
            own(t.id, &t.kind).map(|tg| Target { ghost: Some(tg.geometry), geometry: g, anim: Animation::default(), ..tg })
        }
        k => own(id, k),
    }
}

impl App {
    pub(super) fn geometry_preview(&mut self, ctx: &egui::Context, show: &mut Show) {
        if !self.show_preview {
            return;
        }
        let mut open = true;
        let screen = ctx.content_rect();
        egui::Window::new(theme::with_icon(icons::PREVIEW, "Geometry preview"))
            .id(egui::Id::new("geometry-preview"))
            .open(&mut open)
            .default_size([520.0, 360.0])
            .pivot(egui::Align2::RIGHT_TOP)
            .default_pos(pos2(screen.right() - 280.0, screen.top() + 200.0))
            .min_size([260.0, 180.0])
            .resizable(true)
            .show(ctx, |ui| self.preview_body(ui, show));
        self.show_preview = open;
    }

    /// Texture and source size for the layer: the live frame while it plays, else a poster.
    fn preview_texture(&mut self, ctx: &egui::Context, t: &Target) -> Result<Option<(egui::TextureId, [f32; 2])>, String> {
        // Movies show their live frame; text is re-rendered here (its live texture is supersampled).
        if let (Source::Poster(..) | Source::Camera(_), Some((tex, _))) = (&t.source, self.textures.get(&(t.layer, 0))) {
            let s = tex.size();
            return Ok(Some((tex.id(), [s[0] as f32, s[1] as f32])));
        }
        let key = match &t.source {
            Source::Poster(p, at) => format!("{}@{at:.2}", p.display()),
            Source::Text(_) => return Ok(None),
            Source::Camera([w, h]) => format!("camera {w}x{h}"),
        };
        if let Some((k, tex, size)) = &self.preview_tex
            && *k == key
        {
            return Ok(Some((tex.id(), *size)));
        }
        let (w, h, rgba, size) = match &t.source {
            Source::Poster(p, at) => match self.posters.get(p, *at) {
                None => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                    return Ok(None);
                }
                Some(Err(e)) => return Err(e),
                Some(Ok(p)) => (p.width, p.height, p.rgba, p.source_size.map(|v| v as f32)),
            },
            Source::Text(_) => return Ok(None),
            Source::Camera([w, h]) => {
                // A 16 × 9 checkerboard of greys, stretched to the capture size.
                let rgba: Vec<u8> = (0..16 * 9).flat_map(|i| {
                    let v = if (i % 16 + i / 16) % 2 == 0 { 70 } else { 90 };
                    [v, v, v, 255]
                }).collect();
                (16, 9, std::sync::Arc::new(rgba), [*w as f32, *h as f32])
            }
        };
        let density = 1.0;
        if w == 0 || h == 0 {
            return Ok(None);
        }
        let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
        let tex = ctx.load_texture("geometry-preview", img, super::stage::texture_options(density));
        let id = tex.id();
        self.preview_tex = Some((key, tex, size));
        Ok(Some((id, size)))
    }

    fn preview_body(&mut self, ui: &mut egui::Ui, show: &mut Show) {
        let Some(t) = self.primary().and_then(|id| target_of(show, id)) else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Select a video or text cue, or a fade that changes geometry").weak());
            });
            return;
        };
        // The animation at the editor playhead, while the Animation tab is open.
        let anim = (self.inspector_tab == super::inspector::Tab::Animation && !t.anim.is_empty())
            .then(|| self.anim_edit.time_for(t.layer))
            .flatten()
            .map(|time| t.anim.sample(time));
        let st = anim.unwrap_or_default();
        // Text: cached planes, coloured as they are drawn.
        let text = match &t.source {
            Source::Text(tc) => {
                let frame = self.preview_text.frame_for(tc, ui.ctx());
                let base = [tc.bg_color, tc.shadow_color, tc.outline_color, tc.color];
                let colors: [[f32; 4]; 4] = std::array::from_fn(|i| st.text[i].unwrap_or(base[i].map(|c| c as f32 / 255.0)));
                Some((frame, colors))
            }
            _ => None,
        };
        let tex = match &text {
            Some((frame, _)) => {
                let f = frame.lock();
                Ok((f.width > 0).then(|| (egui::TextureId::default(), f.stage_size())))
            }
            None => self.preview_texture(ui.ctx(), &t),
        };
        let stage = show
            .ws
            .stages
            .iter()
            .find(|s| Some(s.id) == t.stage)
            .or(show.ws.stages.first())
            .cloned()
            .unwrap_or_default();
        let canvas = [stage.width.max(1) as f32, stage.height.max(1) as f32];
        let aspect = canvas[1] / canvas[0];

        // Fit the canvas plus a margin (so off-stage content shows) into the window.
        const MARGIN: f32 = 0.08;
        let span = 1.0 + 2.0 * MARGIN;
        let avail = ui.available_size() - vec2(0.0, 26.0);
        let px = (avail.x / span).min(avail.y / (span * aspect)).max(40.0);
        let size = vec2(px * span, px * span * aspect);
        let editable = self.mode == Mode::Edit;
        let sense = if editable { Sense::click_and_drag() } else { Sense::hover() };
        let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), size.y), sense);
        let view = Rect::from_center_size(resp.rect.center(), size);
        let canvas_rect = Rect::from_min_size(view.min + vec2(MARGIN * px, MARGIN * px * aspect), vec2(px, px * aspect));
        let place = |u: f32, v: f32| pos2(canvas_rect.left() + u * canvas_rect.width(), canvas_rect.top() + v * canvas_rect.height());
        let painter = painter.with_clip_rect(view);

        painter.rect_filled(view, 6.0, theme::BG_INPUT);
        painter.rect_filled(canvas_rect, 0.0, Color32::BLACK);
        let faint = Stroke::new(1.0, Color32::from_gray(40));
        painter.line_segment([place(0.5, 0.0), place(0.5, 1.0)], faint);
        painter.line_segment([place(0.0, 0.5), place(1.0, 0.5)], faint);

        let img = match &tex {
            Ok(Some((_, s))) => *s,
            _ => canvas,
        };
        let mut g = t.geometry;
        let outline = |q: &LayerQuad| -> Vec<Pos2> { q.corners.iter().map(|c| place(c[0], c[1])).collect() };
        if let Some(ghost) = &t.ghost {
            let mut pts = outline(&layer_quad(canvas, img, t.fit, ghost));
            pts.push(pts[0]);
            painter.extend(Shape::dashed_line(&pts, Stroke::new(1.5, theme::AMBER), 6.0, 4.0));
        }
        if g.crop != [0.0; 4] {
            let full = layer_quad(canvas, img, t.fit, &Geometry { crop: [0.0; 4], ..g });
            let mut pts = outline(&full);
            pts.push(pts[0]);
            painter.extend(Shape::dashed_line(&pts, Stroke::new(1.0, theme::TEXT_FAINT), 4.0, 4.0));
        }
        let shown = st.geometry(&g);
        if anim.is_some() && shown != g {
            let mut pts = outline(&layer_quad(canvas, img, t.fit, &g));
            pts.push(pts[0]);
            painter.extend(Shape::dashed_line(&pts, Stroke::new(1.0, theme::TEXT_DIM), 5.0, 4.0));
        }
        let q = layer_quad(canvas, img, t.fit, &shown);
        match (&tex, &text) {
            (Ok(Some(_)), Some((frame, colors))) => {
                let layer = StageLayer {
                    id: t.layer,
                    stage: None,
                    frame: frame.clone(),
                    opacity: t.opacity * st.opacity,
                    geometry: shown,
                    layer: 0,
                    blend_mode: Default::default(),
                    fit: t.fit,
                    order: 0,
                    tint: st.tint,
                    text_colors: *colors,
                    animated: false,
                };
                self.preview_text.textures.retain(|(id, _), _| *id == t.layer);
                painter.extend(paint_layer(ui.ctx(), &mut self.preview_text.textures, &layer, canvas, place));
            }
            (Ok(Some((id, _))), None) => {
                painter.add(layer_mesh(*id, &q, tint(&st, t.opacity), place));
            }
            _ => {
                painter.add(Shape::convex_polygon(outline(&q), Color32::from_rgb(0x22, 0x2a, 0x3a), Stroke::NONE));
            }
        }
        painter.add(Shape::closed_line(outline(&q), Stroke::new(1.5, theme::ACCENT)));
        painter.rect_stroke(canvas_rect, 0.0, Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Outside);
        painter.text(
            canvas_rect.left_top() + vec2(6.0, 4.0),
            egui::Align2::LEFT_TOP,
            format!("{} · {}×{}", stage.name, stage.width, stage.height),
            theme::semibold(10.5),
            theme::TEXT_DIM,
        );
        match &tex {
            Err(e) => {
                painter.text(view.center(), egui::Align2::CENTER_CENTER, e, egui::FontId::proportional(12.0), theme::RED);
            }
            Ok(None) => {
                painter.text(view.center(), egui::Align2::CENTER_CENTER, "Loading…", egui::FontId::proportional(12.0), theme::TEXT_FAINT);
            }
            _ => {}
        }

        // Direct manipulation: drag to move, scroll to scale, double-click to centre.
        let resp = if editable { resp.on_hover_text("Drag to move · scroll to scale · double-click to centre") } else { resp };
        if editable {
            if resp.dragged() {
                let d = resp.drag_delta();
                g.x += d.x / canvas_rect.width();
                g.y += d.y / canvas_rect.height();
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            } else if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    let k = (scroll * 0.003).exp();
                    g.scale_x = (g.scale_x * k).clamp(0.0, 10.0);
                    g.scale_y = (g.scale_y * k).clamp(0.0, 10.0);
                }
            }
            if resp.double_clicked() {
                g.x = 0.0;
                g.y = 0.0;
            }
        }

        ui.horizontal(|ui| {
            let c = g.crop;
            ui.label(
                RichText::new(format!(
                    "X {:.1}%  Y {:.1}%  ·  Scale {:.0}% × {:.0}%  ·  {:.1}°{}",
                    g.x * 100.0,
                    g.y * 100.0,
                    g.scale_x * 100.0,
                    g.scale_y * 100.0,
                    g.rotation,
                    if c != [0.0; 4] {
                        format!("  ·  Crop {:.0}/{:.0}/{:.0}/{:.0}%", c[0] * 100.0, c[1] * 100.0, c[2] * 100.0, c[3] * 100.0)
                    } else {
                        String::new()
                    }
                ))
                .font(theme::mono(11.0))
                .color(theme::TEXT_DIM),
            );
        });

        if g != t.geometry {
            self.set_preview_geometry(show, g);
        }
    }

    /// Write the geometry back to the selected cue (or its fade) and update it live if playing.
    fn set_preview_geometry(&mut self, show: &mut Show, g: Geometry) {
        let Some(id) = self.primary() else { return };
        let Some(cue) = show.ws.cue_list.find_mut(id) else { return };
        match &mut cue.kind {
            CueKind::Video(v) => v.geometry = g,
            CueKind::Text(t) => t.geometry = g,
            CueKind::Camera(c) => c.geometry = g,
            CueKind::Fade(f) => f.geometry = Some(g),
            _ => return,
        }
        let cue = cue.clone();
        if show.engine.instances.contains_key(&id) {
            show.media.update_live(&cue);
        }
        show.edited();
    }
}

/// Vertex colour for the previewed layer: its opacity and the animation's tint and opacity.
fn tint(st: &AnimState, opacity: f32) -> Color32 {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let t = st.tint;
    Color32::from_rgba_unmultiplied(b(t[0]), b(t[1]), b(t[2]), b(t[3] * st.opacity * opacity))
}
