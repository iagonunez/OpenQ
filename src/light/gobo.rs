//! Gobo and wheel slot pictures: small thumbnails kept in the fixture type
//! ([`super::model::FixtureType::images`]), keyed by the image name a [`super::model::ChannelRange`]
//! gives, and shown on the programmer's range buttons.
//!
//! Importers hand over whatever the fixture file holds (GDTF `wheels/*.png|svg`, OFL embedded
//! resources); [`thumbnail`] decodes it with size limits (an SVG can't load files) and stores a
//! [`THUMB`]-pixel PNG as base64 text, so a show file stays small and readable. Pictures read back
//! from a file go through [`decode`], which refuses anything larger than a thumbnail.

use anyhow::Context;
use base64::Engine;

/// Side of a stored thumbnail, pixels.
pub const THUMB: u32 = 64;
/// Most pictures per fixture type.
pub const MAX_IMAGES: usize = 512;
/// Longest stored picture (base64 text), bytes.
pub const MAX_IMAGE_TEXT: usize = 64 << 10;
/// Largest picture accepted from a fixture file before it is scaled down, pixels per side.
const MAX_SOURCE_SIDE: u32 = 4096;

/// A thumbnail of a PNG/JPEG/... or SVG picture, as base64 PNG text.
pub fn thumbnail(data: &[u8]) -> anyhow::Result<String> {
    let rgba = if looks_like_svg(data) { svg(data)? } else { bitmap(data)? };
    // Fit into THUMB × THUMB, centred on a transparent square (wheel slots are round).
    let (w, h) = fit(rgba.width(), rgba.height());
    let small = image::imageops::resize(&rgba, w, h, image::imageops::FilterType::Triangle);
    let mut square = image::RgbaImage::new(THUMB, THUMB);
    image::imageops::overlay(&mut square, &small, ((THUMB - small.width()) / 2) as i64, ((THUMB - small.height()) / 2) as i64);
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(square).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).context("encoding a thumbnail")?;
    Ok(base64::engine::general_purpose::STANDARD.encode(png))
}

/// A stored thumbnail as RGBA pixels: (width, height, pixels). None if it isn't one.
pub fn decode(text: &str) -> Option<(u32, u32, Vec<u8>)> {
    if text.len() > MAX_IMAGE_TEXT {
        return None;
    }
    let png = base64::engine::general_purpose::STANDARD.decode(text).ok()?;
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(png), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4 * THUMB);
    limits.max_image_height = Some(4 * THUMB);
    limits.max_alloc = Some(4 << 20);
    reader.limits(limits);
    let img = reader.decode().ok()?.to_rgba8();
    Some((img.width(), img.height(), img.into_raw()))
}

fn fit(w: u32, h: u32) -> (u32, u32) {
    let k = THUMB as f32 / w.max(h).max(1) as f32;
    (((w as f32 * k).round() as u32).clamp(1, THUMB), ((h as f32 * k).round() as u32).clamp(1, THUMB))
}

fn looks_like_svg(data: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&data[..data.len().min(1024)]).to_ascii_lowercase();
    head.contains("<svg") || head.trim_start().starts_with("<?xml")
}

fn bitmap(data: &[u8]) -> anyhow::Result<image::RgbaImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(128 << 20);
    reader.limits(limits);
    Ok(reader.decode().context("unreadable picture")?.to_rgba8())
}

fn svg(data: &[u8]) -> anyhow::Result<image::RgbaImage> {
    use resvg::usvg;
    // No files or fonts: a fixture file's SVG may only draw (embedded data images are fine).
    let opt = usvg::Options {
        image_href_resolver: usvg::ImageHrefResolver { resolve_data: crate::media::still::guarded_data_resolver(), resolve_string: Box::new(|_, _| None) },
        ..Default::default()
    };
    let tree = crate::media::still::parse_svg_data(data, &opt).context("unreadable SVG")?;
    let size = tree.size();
    let k = (2 * THUMB) as f32 / size.width().max(size.height()).max(1.0);
    let (w, h) = (((size.width() * k).ceil() as u32).max(1), ((size.height() * k).ceil() as u32).max(1));
    let pixmap = crate::media::still::render_svg(&tree, k, w, h)?;
    // tiny-skia is premultiplied; image wants straight alpha.
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for p in pixmap.pixels() {
        let c = p.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    image::RgbaImage::from_raw(w, h, rgba).context("SVG size")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, _| if x < w / 2 { image::Rgba([255, 255, 255, 255]) } else { image::Rgba([0, 0, 0, 255]) });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn thumbnails_from_bitmaps_and_svg() {
        let t = thumbnail(&png(300, 150)).unwrap();
        let (w, h, px) = decode(&t).unwrap();
        assert_eq!((w, h), (THUMB, THUMB));
        // 64 × 32 picture centred: transparent above, white on the left half, black on the right.
        let at = |x: u32, y: u32| &px[((y * w + x) * 4) as usize..][..4];
        assert_eq!(at(10, 2)[3], 0, "letterbox");
        assert_eq!(at(10, 32), [255, 255, 255, 255]);
        assert_eq!(at(54, 32), [0, 0, 0, 255]);
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><circle cx="5" cy="5" r="5" fill="white"/><image href="/etc/hostname" width="10" height="10"/></svg>"#;
        let (_, _, px) = decode(&thumbnail(svg).unwrap()).unwrap();
        assert_eq!(px[((32 * THUMB + 32) * 4 + 3) as usize], 255, "the circle is drawn");
        assert_eq!(px[3], 0, "corner outside the circle");
    }

    #[test]
    fn bad_input_is_refused() {
        assert!(thumbnail(b"not a picture").is_err());
        assert!(thumbnail(&png(MAX_SOURCE_SIDE + 1, 1)).is_err(), "too wide");
        assert!(decode("%%%").is_none());
        let big = base64::engine::general_purpose::STANDARD.encode(png(4 * THUMB + 1, 1));
        assert!(decode(&big).is_none(), "a stored picture must be a thumbnail");
    }
}
