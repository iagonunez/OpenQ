//! Rasterises Text cues into RGBA images (bundled Inter font), so they play like still images.

use crate::model::{TextAlign, TextCue};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};

static REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
static BOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");

/// RGBA (unpremultiplied) image of the cue's text.
pub fn render(cue: &TextCue) -> (u32, u32, Vec<u8>) {
    let font = FontRef::try_from_slice(if cue.bold { BOLD } else { REGULAR }).expect("bundled font");
    let scale = PxScale::from(cue.size.clamp(4.0, 1000.0));
    let sf = font.as_scaled(scale);
    let line_h = sf.ascent() - sf.descent() + sf.line_gap();
    let lines: Vec<&str> = if cue.text.is_empty() { vec![" "] } else { cue.text.lines().collect() };
    let width_of = |line: &str| {
        let mut w = 0.0;
        let mut prev = None;
        for c in line.chars() {
            let g = sf.glyph_id(c);
            if let Some(p) = prev {
                w += sf.kern(p, g);
            }
            w += sf.h_advance(g);
            prev = Some(g);
        }
        w
    };
    let widths: Vec<f32> = lines.iter().map(|l| width_of(l)).collect();
    let pad = (cue.size * 0.15).ceil();
    let w = (widths.iter().cloned().fold(1.0, f32::max) + 2.0 * pad).ceil() as u32;
    let h = (line_h * lines.len() as f32 + 2.0 * pad).ceil() as u32;
    let mut px = vec![0u8; (w * h * 4) as usize];
    let [r, g, b, a] = cue.color;
    for (i, line) in lines.iter().enumerate() {
        let x0 = match cue.align {
            TextAlign::Left => pad,
            TextAlign::Center => (w as f32 - widths[i]) / 2.0,
            TextAlign::Right => w as f32 - pad - widths[i],
        };
        let baseline = pad + sf.ascent() + i as f32 * line_h;
        let mut x = x0;
        let mut prev = None;
        for c in line.chars() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                x += sf.kern(p, id);
            }
            let glyph = id.with_scale_and_position(scale, point(x, baseline));
            x += sf.h_advance(id);
            prev = Some(id);
            if let Some(outline) = font.outline_glyph(glyph) {
                let bb = outline.px_bounds();
                outline.draw(|gx, gy, cov| {
                    let (xx, yy) = (bb.min.x as i32 + gx as i32, bb.min.y as i32 + gy as i32);
                    if xx < 0 || yy < 0 || xx >= w as i32 || yy >= h as i32 {
                        return;
                    }
                    let o = ((yy as u32 * w + xx as u32) * 4) as usize;
                    let alpha = (cov.clamp(0.0, 1.0) * a as f32) as u8;
                    if alpha > px[o + 3] {
                        px[o..o + 4].copy_from_slice(&[r, g, b, alpha]);
                    }
                });
            }
        }
    }
    (w, h, px)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_visible_text_with_alignment() {
        let cue = TextCue { text: "Hi\nthere".into(), size: 40.0, ..Default::default() };
        let (w, h, px) = render(&cue);
        assert!(w > 60 && h > 80, "{w}x{h}");
        let lit = px.chunks_exact(4).filter(|p| p[3] > 128).count();
        assert!(lit > 200, "{lit} lit pixels");
        // Text is white.
        assert!(px.chunks_exact(4).filter(|p| p[3] > 0).all(|p| p[0] == 255));
    }
}
