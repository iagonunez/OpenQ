//! Still images: bitmaps through the `image` crate, SVG through resvg.
//!
//! SVG has no pixel size of its own, so it is rasterised for the size it will be shown at:
//! [`load`] takes a scale (image pixels per SVG pixel) and the layer keeps that as its density,
//! so the picture covers the same stage area at any resolution and stays sharp when an
//! animation scales it up.

use anyhow::{Context, anyhow};
use std::path::Path;
use std::sync::{Arc, OnceLock};


/// A decoded still.
pub struct Still {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Straight (not premultiplied) RGBA.
    pub rgba: Vec<u8>,
    /// Image pixels per source pixel (1 for bitmaps).
    pub density: f32,
    /// Size of the source picture (SVG size in its own pixels).
    pub source_size: [u32; 2],
}

/// True for `.svg` / `.svgz` files.
pub fn is_svg(p: &Path) -> bool {
    matches!(p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(), Some("svg" | "svgz"))
}

/// System fonts for SVG `<text>` (resvg has its own font database; loaded once, on first use).
fn svg_fonts() -> Arc<resvg::usvg::fontdb::Database> {
    static DB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        db.load_font_data(include_bytes!("../../assets/fonts/Inter-Regular.ttf").to_vec());
        db.set_sans_serif_family(super::text::BUILT_IN);
        Arc::new(db)
    })
    .clone()
}

fn parse_svg(path: &Path) -> anyhow::Result<resvg::usvg::Tree> {
    let data = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut opt = resvg::usvg::Options { resources_dir: path.parent().map(Path::to_path_buf), ..Default::default() };
    // Only pay for the system font scan when the drawing has text.
    if data.windows(5).any(|w| w == b"<text") || data.starts_with(&[0x1f, 0x8b]) {
        opt.fontdb = svg_fonts();
    }
    resvg::usvg::Tree::from_data(&data, &opt).map_err(|e| anyhow!("cannot read SVG {}: {e}", path.display()))
}

/// Pixel size of an image (an SVG's own size, rounded).
pub fn dimensions(path: &Path) -> anyhow::Result<(u32, u32)> {
    if is_svg(path) {
        let s = parse_svg(path)?.size();
        return Ok((s.width().round().max(1.0) as u32, s.height().round().max(1.0) as u32));
    }
    image::image_dimensions(path).with_context(|| format!("cannot read image {}", path.display()))
}

/// Decode a still. `svg_scale` is the SVG rasterising scale; bitmaps ignore it. Either is made
/// to fit the GPU's largest texture ([`super::max_texture_side`]): a bigger bitmap is scaled down
/// and keeps covering the same stage area (its density drops below 1).
pub fn load(path: &Path, svg_scale: f32) -> anyhow::Result<Still> {
    if !is_svg(path) {
        let img = image::open(path).with_context(|| format!("cannot read image {}", path.display()))?;
        let source_size = [img.width(), img.height()];
        let max = super::max_texture_side();
        let img = if img.width().max(img.height()) > max { img.resize(max, max, image::imageops::FilterType::Triangle) } else { img }.to_rgba8();
        let (width, height) = img.dimensions();
        let density = width as f32 / source_size[0].max(1) as f32;
        return Ok(Still { width, height, rgba: img.into_raw(), density, source_size });
    }
    let tree = parse_svg(path)?;
    let size = tree.size();
    let longest = size.width().max(size.height()).max(1.0);
    let k = svg_scale.max(0.01).min(super::max_texture_side() as f32 / longest);
    let (w, h) = ((size.width() * k).ceil().max(1.0) as u32, (size.height() * k).ceil().max(1.0) as u32);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).context("SVG too large")?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(k, k), &mut pixmap.as_mut());
    // tiny-skia is premultiplied; layers are straight alpha.
    let mut rgba = pixmap.take();
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3];
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
            }
        }
    }
    Ok(Still {
        width: w,
        height: h,
        rgba,
        density: k,
        source_size: [size.width().round().max(1.0) as u32, size.height().round().max(1.0) as u32],
    })
}

/// A still scaled down so neither side exceeds `max_side` (previews).
pub fn thumbnail(path: &Path, max_side: u32) -> anyhow::Result<Still> {
    if is_svg(path) {
        let (w, h) = dimensions(path)?;
        let mut s = load(path, max_side as f32 / w.max(h) as f32)?;
        s.density = 1.0;
        return Ok(s);
    }
    let img = image::open(path).with_context(|| format!("cannot read image {}", path.display()))?;
    let source_size = [img.width(), img.height()];
    let img = img.thumbnail(max_side, max_side).to_rgba8();
    let (width, height) = img.dimensions();
    Ok(Still { width, height, rgba: img.into_raw(), density: 1.0, source_size })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_rasterises_at_scale() {
        let dir = std::env::temp_dir().join(format!("openq-svg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("box.svg");
        std::fs::write(
            &p,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect x="0" y="0" width="20" height="20" fill="#ff0000" fill-opacity="0.5"/></svg>"##,
        )
        .unwrap();
        assert_eq!(dimensions(&p).unwrap(), (40, 20));
        let s = load(&p, 3.0).unwrap();
        assert_eq!((s.width, s.height, s.density, s.source_size), (120, 60, 3.0, [40, 20]));
        // Straight alpha: half-transparent pure red, then nothing on the right half.
        assert_eq!(&s.rgba[..4], &[255, 0, 0, 128]);
        assert_eq!(s.rgba[(30 * 120 + 100) * 4 + 3], 0);
        let t = thumbnail(&p, 10).unwrap();
        assert_eq!((t.width, t.height), (10, 5));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
