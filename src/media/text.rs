//! Rasterises Text cues into RGBA images, so they play like still images.
//!
//! Text goes through [cosmic-text]: any font installed on the system (plus the bundled Inter),
//! real shaping (kerning, ligatures, complex scripts, right-to-left), and automatic fallback to
//! another font for characters the chosen one lacks, such as emoji or CJK. Glyphs are rasterised
//! by swash with sub-pixel positioning. The outline is built from a distance field, so it has
//! round, even corners at any width, and the drop shadow is a three-pass box blur (close to a
//! Gaussian).
//!
//! Images are rendered at [`TextImage::density`] pixels per stage pixel (2× normally), so text
//! stays sharp when a layer is scaled up. The stage windows use mipmaps, so it also stays smooth
//! when scaled down.
//!
//! [cosmic-text]: https://docs.rs/cosmic-text

use crate::model::{TextAlign, TextCue};
use cosmic_text::{
    Align, Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent, Weight,
};
use parking_lot::Mutex;
use std::sync::OnceLock;

static REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
static BOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");

/// Family name of the bundled font, used when a cue names none.
pub const BUILT_IN: &str = "Inter";
/// Pixels rendered per stage pixel.
const DENSITY: f32 = 2.0;
/// Largest image side we hand to the GPU.
const MAX_SIDE: f32 = 8192.0;

struct Fonts {
    system: FontSystem,
    cache: SwashCache,
}

static FONTS: OnceLock<Mutex<Fonts>> = OnceLock::new();
static FAMILIES: OnceLock<Vec<String>> = OnceLock::new();

fn fonts() -> &'static Mutex<Fonts> {
    FONTS.get_or_init(|| {
        let t = std::time::Instant::now();
        let mut system = FontSystem::new();
        system.db_mut().load_font_data(REGULAR.to_vec());
        system.db_mut().load_font_data(BOLD.to_vec());
        log::info!("loaded {} font faces in {:?}", system.db().len(), t.elapsed());
        Mutex::new(Fonts { system, cache: SwashCache::new() })
    })
}

/// Start loading the system font list in the background (it can take a moment), so the first
/// Text cue or font menu doesn't wait for it.
pub fn preload() {
    let _ = std::thread::Builder::new().name("openq-fonts".into()).spawn(|| {
        families();
    });
}

/// Every installed font family, sorted, including the built-in one.
pub fn families() -> &'static [String] {
    FAMILIES.get_or_init(|| {
        let f = fonts().lock();
        let mut names: Vec<String> = f.system.db().faces().filter_map(|face| face.families.first().map(|(n, _)| n.clone())).collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        names
    })
}

/// True once the font list has loaded (so `families()` won't block).
pub fn families_ready() -> bool {
    FAMILIES.get().is_some()
}

/// The styles installed for a family: (weight, italic) pairs, sorted.
pub fn styles_of(family: &str) -> Vec<(u16, bool)> {
    let family = if family.is_empty() { BUILT_IN } else { family };
    let f = fonts().lock();
    let mut v: Vec<(u16, bool)> = f
        .system
        .db()
        .faces()
        .filter(|face| face.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(family)))
        .map(|face| (face.weight.0, face.style != cosmic_text::fontdb::Style::Normal))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// True if `family` is installed (or is the built-in font). Empty means the built-in font.
pub fn is_installed(family: &str) -> bool {
    family.is_empty() || families().iter().any(|f| f.eq_ignore_ascii_case(family))
}

/// A rendered Text cue.
pub struct TextImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Straight (not premultiplied) RGBA pixels.
    pub rgba: Vec<u8>,
    /// Image pixels per stage pixel: the layer covers `width / density` stage pixels.
    pub density: f32,
}

/// Render a Text cue at the normal density.
pub fn render(cue: &TextCue) -> TextImage {
    render_at(cue, DENSITY)
}

/// Render a Text cue at `density` image pixels per stage pixel (reduced if the image would be
/// too large for the GPU).
pub fn render_at(cue: &TextCue, density: f32) -> TextImage {
    let mut guard = fonts().lock();
    let Fonts { system, cache } = &mut *guard;
    let size = cue.size.clamp(4.0, 1000.0);
    let attrs = Attrs::new()
        .family(if cue.font.is_empty() { Family::Name(BUILT_IN) } else { Family::Name(&cue.font) })
        .weight(Weight(cue.effective_weight()))
        .style(if cue.italic { Style::Italic } else { Style::Normal })
        .letter_spacing(cue.letter_spacing);
    let align = match cue.align {
        TextAlign::Left => Align::Left,
        TextAlign::Center => Align::Center,
        TextAlign::Right => Align::Right,
    };
    let text = if cue.text.trim().is_empty() { " " } else { cue.text.as_str() };

    // Lay out once at the stage size to measure, then pick a density that fits the GPU.
    let line_h = size * cue.line_height.clamp(0.5, 4.0);
    let measure = |system: &mut FontSystem, scale: f32| {
        let mut buf = Buffer::new(system, Metrics::new(size * scale, line_h * scale));
        buf.set_size(None, None);
        buf.set_text(text, &attrs, Shaping::Advanced, Some(align));
        buf.shape_until_scroll(system, false);
        let w = buf.layout_runs().map(|r| r.line_w).fold(0.0f32, f32::max);
        let h = buf.layout_runs().map(|r| r.line_top + r.line_height).fold(0.0f32, f32::max);
        // Alignment needs a box as wide as the longest line.
        buf.set_size(Some(w.ceil().max(1.0)), None);
        buf.shape_until_scroll(system, false);
        (buf, w, h)
    };
    let effects = cue.outline_width.max(0.0) + if cue.shadow_color[3] > 0 {
        cue.shadow_blur.max(0.0) * 1.5 + cue.shadow_offset[0].abs().max(cue.shadow_offset[1].abs())
    } else {
        0.0
    };
    let pad_stage = (size * 0.12 + effects + 2.0).ceil();
    let (_, w1, h1) = measure(system, 1.0);
    let longest = (w1.max(h1) + 2.0 * pad_stage).max(1.0);
    let density = density.min(MAX_SIDE / longest).max(0.25);
    let (buf, w, h) = measure(system, density);

    let pad = (pad_stage * density).ceil();
    let (iw, ih) = ((w + 2.0 * pad).ceil() as usize, (h + 2.0 * pad).ceil() as usize);
    let mut fill = vec![0f32; iw * ih];
    // Colour glyphs (emoji), premultiplied.
    let mut colour = vec![[0f32; 4]; iw * ih];
    let mut has_colour = false;
    for run in buf.layout_runs() {
        for g in run.glyphs {
            let pg = g.physical((pad, pad + run.line_y), 1.0);
            let Some(img) = cache.get_image(system, pg.cache_key) else { continue };
            let p = img.placement;
            for y in 0..p.height as i32 {
                for x in 0..p.width as i32 {
                    let (px, py) = (pg.x + p.left + x, pg.y - p.top + y);
                    if px < 0 || py < 0 || px >= iw as i32 || py >= ih as i32 {
                        continue;
                    }
                    let o = py as usize * iw + px as usize;
                    let i = (y * p.width as i32 + x) as usize;
                    match img.content {
                        SwashContent::Mask => fill[o] = fill[o].max(img.data[i] as f32 / 255.0),
                        SwashContent::SubpixelMask => {
                            let d = &img.data[i * 4..i * 4 + 3];
                            fill[o] = fill[o].max((d[0] as f32 + d[1] as f32 + d[2] as f32) / 765.0);
                        }
                        SwashContent::Color => {
                            let d = &img.data[i * 4..i * 4 + 4];
                            let a = d[3] as f32 / 255.0;
                            // swash colour bitmaps are premultiplied.
                            let src = [d[0] as f32 / 255.0, d[1] as f32 / 255.0, d[2] as f32 / 255.0, a];
                            colour[o] = over(src, colour[o]);
                            has_colour = true;
                        }
                    }
                }
            }
        }
    }
    drop(guard);

    // Outline: everything within `outline_width` of the glyphs, from a distance field.
    let outline_px = cue.outline_width.max(0.0) * density;
    let solid: Vec<f32> = if has_colour { fill.iter().zip(&colour).map(|(f, c)| f.max(c[3])).collect() } else { fill.clone() };
    let outline = (outline_px > 0.0 && cue.outline_color[3] > 0).then(|| {
        let dist = distance_field(&solid, iw, ih);
        dist.iter().zip(&solid).map(|(d, s)| (outline_px + 0.5 - d).clamp(0.0, 1.0).max(*s)).collect::<Vec<f32>>()
    });

    // Shadow: the text's silhouette (with outline), blurred and offset.
    let shadow = (cue.shadow_color[3] > 0).then(|| {
        let base = outline.as_ref().unwrap_or(&solid);
        let (dx, dy) = ((cue.shadow_offset[0] * density).round() as i32, (cue.shadow_offset[1] * density).round() as i32);
        let mut m = vec![0f32; iw * ih];
        for y in 0..ih as i32 {
            for x in 0..iw as i32 {
                let (sx, sy) = (x - dx, y - dy);
                if sx >= 0 && sy >= 0 && sx < iw as i32 && sy < ih as i32 {
                    m[(y as usize) * iw + x as usize] = base[sy as usize * iw + sx as usize];
                }
            }
        }
        let r = (cue.shadow_blur.max(0.0) * density / 2.0).round() as usize;
        if r > 0 {
            for _ in 0..3 {
                box_blur(&mut m, iw, ih, r);
            }
        }
        m
    });

    // Composite back to front: shadow, outline, fill, colour glyphs.
    let mut acc = vec![[0f32; 4]; iw * ih];
    let mut layer = |mask: &[f32], c: [u8; 4]| {
        let ca = c[3] as f32 / 255.0;
        let rgb = [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0];
        for (o, m) in acc.iter_mut().zip(mask) {
            let a = m * ca;
            if a > 0.0 {
                *o = over([rgb[0] * a, rgb[1] * a, rgb[2] * a, a], *o);
            }
        }
    };
    if let Some(s) = &shadow {
        layer(s, cue.shadow_color);
    }
    if let Some(o) = &outline {
        layer(o, cue.outline_color);
    }
    layer(&fill, cue.color);
    if has_colour {
        let fade = cue.color[3] as f32 / 255.0;
        for (o, c) in acc.iter_mut().zip(&colour) {
            if c[3] > 0.0 {
                *o = over(c.map(|v| v * fade), *o);
            }
        }
    }
    let mut rgba = vec![0u8; iw * ih * 4];
    for (px, a) in rgba.chunks_exact_mut(4).zip(&acc) {
        if a[3] > 0.0 {
            let inv = 1.0 / a[3];
            px[0] = (a[0] * inv * 255.0).round().clamp(0.0, 255.0) as u8;
            px[1] = (a[1] * inv * 255.0).round().clamp(0.0, 255.0) as u8;
            px[2] = (a[2] * inv * 255.0).round().clamp(0.0, 255.0) as u8;
            px[3] = (a[3] * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    TextImage { width: iw as u32, height: ih as u32, rgba, density }
}

/// Premultiplied "source over destination".
fn over(src: [f32; 4], dst: [f32; 4]) -> [f32; 4] {
    let k = 1.0 - src[3];
    [src[0] + dst[0] * k, src[1] + dst[1] * k, src[2] + dst[2] * k, src[3] + dst[3] * k]
}

/// Euclidean distance (in pixels) from each pixel to the nearest pixel at least half covered.
fn distance_field(cov: &[f32], w: usize, h: usize) -> Vec<f32> {
    const INF: f32 = 1e20;
    let mut d: Vec<f32> = cov.iter().map(|c| if *c >= 0.5 { 0.0 } else { INF }).collect();
    let n = w.max(h);
    let (mut f, mut out, mut v, mut z) = (vec![0f32; n], vec![0f32; n], vec![0usize; n], vec![0f32; n + 1]);
    for x in 0..w {
        for y in 0..h {
            f[y] = d[y * w + x];
        }
        edt_1d(&f[..h], &mut out[..h], &mut v, &mut z);
        for y in 0..h {
            d[y * w + x] = out[y];
        }
    }
    for y in 0..h {
        f[..w].copy_from_slice(&d[y * w..(y + 1) * w]);
        edt_1d(&f[..w], &mut out[..w], &mut v, &mut z);
        d[y * w..(y + 1) * w].copy_from_slice(&out[..w]);
    }
    d.iter().map(|v| v.sqrt()).collect()
}

/// Felzenszwalb–Huttenlocher squared distance transform of one row.
fn edt_1d(f: &[f32], d: &mut [f32], v: &mut [usize], z: &mut [f32]) {
    let n = f.len();
    if n == 0 {
        return;
    }
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;
    for q in 1..n {
        loop {
            let p = v[k];
            let s = ((f[q] + (q * q) as f32) - (f[p] + (p * p) as f32)) / (2.0 * q as f32 - 2.0 * p as f32);
            if s <= z[k] && k > 0 {
                k -= 1;
                continue;
            }
            if s <= z[k] {
                // k == 0: replace the first parabola.
                v[0] = q;
                z[0] = f32::NEG_INFINITY;
                z[1] = f32::INFINITY;
                break;
            }
            k += 1;
            v[k] = q;
            z[k] = s;
            z[k + 1] = f32::INFINITY;
            break;
        }
    }
    k = 0;
    for (q, out) in d.iter_mut().enumerate().take(n) {
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let p = v[k];
        let dq = q as f32 - p as f32;
        *out = dq * dq + f[p];
    }
}

/// In-place box blur of radius `r`, horizontal then vertical.
fn box_blur(m: &mut [f32], w: usize, h: usize, r: usize) {
    let win = (2 * r + 1) as f32;
    let mut tmp = vec![0f32; w.max(h)];
    for y in 0..h {
        let row = &mut m[y * w..(y + 1) * w];
        let mut sum: f32 = (0..=r.min(w - 1)).map(|i| row[i]).sum();
        for x in 0..w {
            tmp[x] = sum / win;
            if x + r + 1 < w {
                sum += row[x + r + 1];
            }
            if x >= r {
                sum -= row[x - r];
            }
        }
        row.copy_from_slice(&tmp[..w]);
    }
    for x in 0..w {
        let mut sum: f32 = (0..=r.min(h - 1)).map(|i| m[i * w + x]).sum();
        for y in 0..h {
            tmp[y] = sum / win;
            if y + r + 1 < h {
                sum += m[(y + r + 1) * w + x];
            }
            if y >= r {
                sum -= m[(y - r) * w + x];
            }
        }
        for y in 0..h {
            m[y * w + x] = tmp[y];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(img: &TextImage) -> usize {
        img.rgba.chunks_exact(4).filter(|p| p[3] > 128).count()
    }

    #[test]
    fn renders_visible_text_at_density() {
        let cue = TextCue { text: "Hi\nthere".into(), size: 40.0, ..Default::default() };
        let img = render(&cue);
        assert_eq!(img.density, DENSITY);
        assert!(img.width as f32 / img.density > 60.0 && img.height as f32 / img.density > 80.0, "{}x{}", img.width, img.height);
        assert!(lit(&img) > 800, "{} lit pixels", lit(&img));
        // White text with straight alpha: every visible pixel is white.
        assert!(img.rgba.chunks_exact(4).filter(|p| p[3] > 0).all(|p| p[0] == 255));
    }

    #[test]
    fn outline_and_shadow_add_coverage() {
        let base = TextCue { text: "OpenQ".into(), size: 48.0, ..Default::default() };
        let plain = lit(&render_at(&base, 1.0));
        let outlined = render_at(&TextCue { outline_width: 3.0, ..base.clone() }, 1.0);
        assert!(lit(&outlined) > plain * 3 / 2, "{} vs {plain}", lit(&outlined));
        // The outline is black, the fill white: both colours appear.
        assert!(outlined.rgba.chunks_exact(4).any(|p| p[3] == 255 && p[0] < 10));
        assert!(outlined.rgba.chunks_exact(4).any(|p| p[3] == 255 && p[0] > 245));
        let shadowed = render_at(&TextCue { shadow_color: [0, 0, 0, 200], shadow_blur: 0.0, ..base }, 1.0);
        assert!(shadowed.rgba.chunks_exact(4).filter(|p| p[3] > 100).count() > plain * 3 / 2);
    }

    #[test]
    fn unknown_font_falls_back() {
        let cue = TextCue { text: "Fallback".into(), font: "No Such Font 12345".into(), ..Default::default() };
        assert!(lit(&render_at(&cue, 1.0)) > 200);
        assert!(!is_installed("No Such Font 12345"));
        assert!(is_installed(BUILT_IN) && is_installed(""));
    }

    #[test]
    fn distance_field_is_euclidean() {
        let (w, h) = (9, 9);
        let mut cov = vec![0f32; w * h];
        cov[4 * w + 4] = 1.0;
        let d = distance_field(&cov, w, h);
        assert_eq!(d[4 * w + 4], 0.0);
        assert!((d[4 * w + 7] - 3.0).abs() < 1e-4);
        assert!((d[7 * w + 8] - 5.0).abs() < 1e-4); // 3-4-5 triangle
    }
}
