//! Stage output window: composites running video cues by layer.

use super::App;
use crate::model::VideoFit;
use crate::show::Show;
use egui::{Color32, Rect, ViewportBuilder, ViewportCommand, ViewportId, pos2, vec2};

impl App {
    /// Draw the stage window. Returns true if any video is on screen.
    pub(super) fn stage(&mut self, ctx: &egui::Context, show: &Show) -> bool {
        let layers = show.media.stage_layers();
        if !layers.is_empty() {
            self.stage_open = true;
        }
        self.textures.retain(|id, _| layers.iter().any(|l| l.id == *id));
        if !self.stage_open {
            return !layers.is_empty();
        }

        let fullscreen = self.stage_fullscreen;
        let mut close = false;
        let mut toggle_fs = false;
        let textures = &mut self.textures;
        ctx.show_viewport_immediate(
            ViewportId::from_hash_of("openq-stage"),
            ViewportBuilder::default().with_title("OpenQ Stage").with_inner_size([960.0, 540.0]),
            |ui, _class| {
                let ctx = ui.ctx().clone();
                let rect = ui.max_rect();
                let painter = ui.painter();
                painter.rect_filled(rect, 0.0, Color32::BLACK);

                for l in &layers {
                    let f = l.frame.lock();
                    if f.width == 0 || f.data.is_empty() {
                        continue;
                    }
                    let size = [f.width as usize, f.height as usize];
                    let entry = textures.entry(l.id).or_insert_with(|| {
                        let img = egui::ColorImage::from_rgba_unmultiplied(size, &f.data);
                        (ctx.load_texture(format!("video-{}", l.id), img, egui::TextureOptions::LINEAR), f.generation)
                    });
                    if entry.1 != f.generation {
                        let img = egui::ColorImage::from_rgba_unmultiplied(size, &f.data);
                        entry.0.set(img, egui::TextureOptions::LINEAR);
                        entry.1 = f.generation;
                    }
                    let (w, h) = (f.width as f32, f.height as f32);
                    drop(f);
                    let (dst, uv) = fit_rect(rect, w, h, l.fit);
                    painter.image(entry.0.id(), dst, uv, Color32::WHITE.gamma_multiply(l.opacity));
                }

                if layers.is_empty() {
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Stage — double-click for fullscreen",
                        egui::FontId::proportional(16.0),
                        Color32::from_gray(60),
                    );
                }
                let resp = ui.interact(rect, ui.id().with("stage-bg"), egui::Sense::click());
                if resp.double_clicked() || ctx.input(|i| i.key_pressed(egui::Key::F11)) {
                    toggle_fs = true;
                }
                if ctx.input(|i| i.viewport().close_requested()) {
                    close = true;
                }
            },
        );
        if toggle_fs {
            self.stage_fullscreen = !fullscreen;
            ctx.send_viewport_cmd_to(ViewportId::from_hash_of("openq-stage"), ViewportCommand::Fullscreen(self.stage_fullscreen));
        }
        if close {
            self.stage_open = false;
            self.stage_fullscreen = false;
        }
        !layers.is_empty()
    }
}

/// Destination rect and UV rect for drawing a `w`×`h` image into `area`.
fn fit_rect(area: Rect, w: f32, h: f32, fit: VideoFit) -> (Rect, Rect) {
    let full_uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    let (aw, ah) = (area.width(), area.height());
    match fit {
        VideoFit::Stretch => (area, full_uv),
        VideoFit::Fit => {
            let s = (aw / w).min(ah / h);
            (Rect::from_center_size(area.center(), vec2(w * s, h * s)), full_uv)
        }
        VideoFit::Fill => {
            let s = (aw / w).max(ah / h);
            let (vw, vh) = (aw / (w * s), ah / (h * s));
            let uv = Rect::from_center_size(pos2(0.5, 0.5), vec2(vw, vh));
            (area, uv)
        }
    }
}
