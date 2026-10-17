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

/// Rasterise SVG markup made by OpenQ itself (the mapping test pattern and pins) at `w`×`h`
/// pixels. Returns premultiplied RGBA. The markup reaches no files: no linked pictures.
pub fn render_svg_markup(svg: &str, w: u32, h: u32) -> anyhow::Result<Vec<u8>> {
    use resvg::usvg;
    let opt = usvg::Options {
        fontdb: svg_fonts(),
        image_href_resolver: usvg::ImageHrefResolver { resolve_data: Box::new(|_, _, _| None), resolve_string: Box::new(|_, _| None) },
        ..Default::default()
    };
    let tree = parse_svg_text(svg, &opt).map_err(|e| anyhow!("bad SVG: {e:#}"))?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w.max(1), h.max(1)).context("picture too large")?;
    let size = tree.size();
    let t = resvg::tiny_skia::Transform::from_scale(w as f32 / size.width().max(1.0), h as f32 / size.height().max(1.0));
    resvg::render(&tree, t, &mut pixmap.as_mut());
    Ok(pixmap.take())
}

/// Most work the expensive SVG filters may take for one picture (pixel operations; about a
/// second). Above it the drawing is refused: a hostile file could otherwise keep the renderer
/// busy for days.
const MAX_FILTER_WORK: f64 = 2e9;

/// Rasterise `tree` scaled by `k` into a `w`×`h` picture, refusing drawings whose filters would
/// take far too long (erode/dilate, convolution and turbulence with absurd sizes). The rasteriser
/// panics on some absurd shapes (a circle of radius 10³⁷): that is an error here, not a crash.
pub fn render_svg(tree: &resvg::usvg::Tree, k: f32, w: u32, h: u32) -> anyhow::Result<resvg::tiny_skia::Pixmap> {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).context("SVG too large")?;
    check_filter_cost(tree, k, w, h)?;
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        resvg::render(tree, resvg::tiny_skia::Transform::from_scale(k, k), &mut pixmap.as_mut());
    }))
    .map_err(|_| anyhow!("the SVG couldn't be drawn"))?;
    Ok(pixmap)
}

/// Refuse a drawing whose filters would cost more than [`MAX_FILTER_WORK`] at scale `k` on a
/// `w`×`h` picture. resvg's erode/dilate visits a (2r)² window per pixel (capped at the picture),
/// a convolution its whole kernel and turbulence every octave, and none of them limits the
/// numbers a file can ask for.
fn check_filter_cost(tree: &resvg::usvg::Tree, k: f32, w: u32, h: u32) -> anyhow::Result<()> {
    use resvg::usvg::{Node, filter::Kind};
    fn walk(g: &resvg::usvg::Group, k: f32, w: f64, h: f64, total: &mut f64) {
        let (sx, sy) = g.abs_transform().get_scale();
        for f in g.filters() {
            for p in f.primitives() {
                let per_pixel = match p.kind() {
                    Kind::Morphology(m) => {
                        let rx = (2.0 * (m.radius_x().get() * sx * k) as f64).ceil().clamp(1.0, w);
                        let ry = (2.0 * (m.radius_y().get() * sy * k) as f64).ceil().clamp(1.0, h);
                        rx * ry
                    }
                    Kind::ConvolveMatrix(c) => c.matrix().columns() as f64 * c.matrix().rows() as f64,
                    Kind::Turbulence(t) => t.num_octaves() as f64 * 4.0,
                    _ => 1.0,
                };
                *total += per_pixel * w * h;
            }
        }
        for n in g.children() {
            if let Node::Group(c) = n {
                walk(c, k, w, h, total);
            }
        }
    }
    let mut total = 0.0;
    walk(tree.root(), k, w as f64, h as f64, &mut total);
    if total.is_nan() || total > MAX_FILTER_WORK {
        anyhow::bail!("the SVG's filters would take too long to draw");
    }
    Ok(())
}

/// Largest SVG file read, and largest picture an SVG may link to.
const MAX_SVG_FILE: u64 = 64 << 20;

/// True for a regular file of at most `max` bytes (not a device such as `/dev/zero`, which would
/// be read forever, nor a [network path](super::is_network_path)).
fn small_file(path: &Path, max: u64) -> bool {
    !super::is_network_path(path) && std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= max)
}

fn parse_svg(path: &Path) -> anyhow::Result<resvg::usvg::Tree> {
    use resvg::usvg;
    let data = crate::recovery::read_file(path, MAX_SVG_FILE).with_context(|| format!("cannot read SVG {}", path.display()))?;
    let text = svg_text(&data)?;
    // Linked pictures: only small regular files, through the same checks as embedded ones.
    let resolve_data = guarded_data_resolver();
    let image_href_resolver = usvg::ImageHrefResolver {
        resolve_data: guarded_data_resolver(),
        resolve_string: Box::new(move |href: &str, opts: &usvg::Options| {
            let file = opts.get_abs_path(Path::new(href));
            let data = small_file(&file, MAX_SVG_FILE).then(|| crate::recovery::read_file(&file, MAX_SVG_FILE).ok()).flatten()?;
            resolve_data("text/plain", Arc::new(data), opts)
        }),
    };
    let mut opt = usvg::Options { resources_dir: path.parent().map(Path::to_path_buf), image_href_resolver, ..Default::default() };
    // Only pay for the system font scan when the drawing has text.
    if text.contains("<text") {
        opt.fontdb = svg_fonts();
    }
    parse_svg_text(&text, &opt).map_err(|e| anyhow!("cannot read SVG {}: {e:#}", path.display()))
}

/// SVG markup from file data, inflating `.svgz` (at most [`MAX_SVG_FILE`] bytes either way).
fn svg_text(data: &[u8]) -> anyhow::Result<std::borrow::Cow<'_, str>> {
    use std::io::Read;
    if !data.starts_with(&[0x1f, 0x8b]) {
        return Ok(std::str::from_utf8(data).context("the SVG is not UTF-8 text")?.into());
    }
    let mut text = String::new();
    flate2::read::GzDecoder::new(data).take(MAX_SVG_FILE + 1).read_to_string(&mut text).context("damaged .svgz")?;
    if text.len() as u64 > MAX_SVG_FILE {
        anyhow::bail!("the SVG is larger than {} MB", MAX_SVG_FILE >> 20);
    }
    Ok(text.into())
}

/// Parse SVG markup, refusing markup nested deeper than [`crate::light::import::MAX_XML_DEPTH`]
/// first: the parser recurses, so a hostile file would overflow the stack and abort OpenQ.
fn parse_svg_text(text: &str, opt: &resvg::usvg::Options) -> anyhow::Result<resvg::usvg::Tree> {
    crate::light::import::check_depth(text)?;
    Ok(resvg::usvg::Tree::from_str(text, opt)?)
}

/// Parse SVG file data (plain or `.svgz`), refusing markup nested too deeply for the parser (it
/// recurses: a hostile file would overflow the stack); pictures it embeds go through
/// [`guarded_data_resolver`] when `opt` uses it.
pub fn parse_svg_data(data: &[u8], opt: &resvg::usvg::Options) -> anyhow::Result<resvg::usvg::Tree> {
    parse_svg_text(&svg_text(data)?, opt)
}

/// usvg's resolver for embedded pictures (`data:` URLs), except that an SVG inside one goes
/// through the checks of [`parse_svg_data`] first (usvg would parse it unchecked).
pub fn guarded_data_resolver() -> resvg::usvg::ImageHrefDataResolverFn<'static> {
    let inner = resvg::usvg::ImageHrefResolver::default_data_resolver();
    Box::new(move |mime, data, opts| {
        let raster = data.starts_with(b"\x89PNG") || data.starts_with(&[0xff, 0xd8]) || data.starts_with(b"GIF8") || (data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP"));
        if !raster && svg_text(&data).ok().is_none_or(|t| crate::light::import::check_depth(&t).is_err()) {
            return None;
        }
        inner(mime, data, opts)
    })
}

/// Pixel size of an image (an SVG's own size, rounded).
pub fn dimensions(path: &Path) -> anyhow::Result<(u32, u32)> {
    super::check_local(path)?;
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
    super::check_local(path)?;
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
    let pixmap = render_svg(&tree, k, w, h)?;
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
    super::check_local(path)?;
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
    fn hostile_svgs_are_refused() {
        let opt = resvg::usvg::Options::default();
        let deep = format!(r#"<svg xmlns="http://www.w3.org/2000/svg">{}{}</svg>"#, "<g>".repeat(100_000), "</g>".repeat(100_000));
        assert!(parse_svg_data(deep.as_bytes(), &opt).is_err(), "nesting that would overflow the stack");
        let morph = r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><filter id="f"><feMorphology radius="1e9"/></filter><rect width="64" height="64" filter="url(#f)"/></svg>"#;
        let tree = parse_svg_data(morph.as_bytes(), &opt).unwrap();
        assert!(render_svg(&tree, 64.0, 4096, 4096).is_err(), "erode/dilate that would take days");
        assert!(render_svg(&tree, 0.25, 16, 16).is_ok(), "the same at a tiny size is cheap");
        let octaves = r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><filter id="f"><feTurbulence numOctaves="4000000000"/></filter><rect width="64" height="64" filter="url(#f)"/></svg>"#;
        assert!(render_svg(&parse_svg_data(octaves.as_bytes(), &opt).unwrap(), 1.0, 64, 64).is_err());
        // An SVG embedded as a data URL gets the same checks.
        use base64::Engine as _;
        let inner = base64::engine::general_purpose::STANDARD.encode(&deep);
        let outer = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><image width="8" height="8" href="data:image/svg+xml;base64,{inner}"/></svg>"#);
        let opt = resvg::usvg::Options {
            image_href_resolver: resvg::usvg::ImageHrefResolver { resolve_data: guarded_data_resolver(), resolve_string: Box::new(|_, _| None) },
            ..Default::default()
        };
        assert!(parse_svg_data(outer.as_bytes(), &opt).is_ok(), "the outer drawing loads, without the hostile picture");
        // .svgz
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, morph.as_bytes()).unwrap();
        assert!(parse_svg_data(&gz.finish().unwrap(), &opt).is_ok());
    }

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
