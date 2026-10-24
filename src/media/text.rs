//! Rasterises Text cues so they play like still images.
//!
//! On stage a Text cue is drawn from coverage planes ([`render_planes`]: background box, shadow,
//! outline, fill), each coloured as it is drawn, so colour changes and colour animation cost
//! nothing. [`render`] composites the same planes into one RGBA image for previews.
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

use crate::animation::AnimProp;
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
    let _ = std::thread::Builder::new().name("opnq-fonts".into()).spawn(|| {
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

/// The styles installed for a family: (weight, italic) pairs, sorted. Cached: the Text tab
/// asks every frame, and the font lock may be held by a render on the worker.
pub fn styles_of(family: &str) -> Vec<(u16, bool)> {
    type Styles = std::collections::HashMap<String, Vec<(u16, bool)>>;
    static CACHE: OnceLock<Mutex<Styles>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().get(family) {
        return v.clone();
    }
    let v = styles_uncached(family);
    cache.lock().insert(family.to_string(), v.clone());
    v
}

fn styles_uncached(family: &str) -> Vec<(u16, bool)> {
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
/// too large for the GPU), composited in its own colours.
pub fn render_at(cue: &TextCue, density: f32) -> TextImage {
    let m = masks(cue, density);
    let (iw, ih) = (m.width as usize, m.height as usize);
    let c = |c: [u8; 4]| c.map(|v| v as f32 / 255.0);
    let mut acc = vec![[0f32; 4]; iw * ih];
    for (part, mask) in &m.masks {
        let col = c(match part {
            Part::Background => cue.bg_color,
            Part::Shadow => cue.shadow_color,
            Part::Outline => cue.outline_color,
            _ => cue.color,
        });
        for (o, v) in acc.iter_mut().zip(mask) {
            let a = *v as f32 / 255.0 * col[3];
            if a > 0.0 {
                *o = over([col[0] * a, col[1] * a, col[2] * a, a], *o);
            }
        }
    }
    if let Some(colour) = &m.colour {
        let fade = cue.color[3] as f32 / 255.0;
        for (o, px) in acc.iter_mut().zip(colour.as_chunks::<4>().0) {
            if px[3] > 0 {
                *o = over(std::array::from_fn(|k| px[k] as f32 / 255.0 * fade), *o);
            }
        }
    }
    let mut rgba = vec![0u8; iw * ih * 4];
    for (px, a) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(&acc) {
        if a[3] > 0.0 {
            let inv = 1.0 / a[3];
            px[0] = (a[0] * inv * 255.0).round().clamp(0.0, 255.0) as u8;
            px[1] = (a[1] * inv * 255.0).round().clamp(0.0, 255.0) as u8;
            px[2] = (a[2] * inv * 255.0).round().clamp(0.0, 255.0) as u8;
            px[3] = (a[3] * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    TextImage { width: m.width, height: m.height, rgba, density: m.density }
}

/// Render a Text cue as separate coverage planes (background, shadow, outline, fill, colour
/// glyphs) that the stage windows colour as they draw them, so colour changes and colour
/// animation need no re-rendering. Parts the cue doesn't use (and doesn't animate) are left out.
pub fn render_planes(cue: &TextCue) -> TextPlanes {
    masks(cue, DENSITY)
}

/// Rasterisations per text string (tests use it to catch needless re-rendering; renders are
/// rare, so the lock costs nothing).
static RENDERS: OnceLock<Mutex<std::collections::HashMap<String, u64>>> = OnceLock::new();

/// Distinct strings [`render_count`] remembers before it starts over.
const MAX_COUNTED: usize = 4096;

/// How many times a Text cue showing `text` has been rasterised in this process (planes or
/// composite), on any thread.
pub fn render_count(text: &str) -> u64 {
    RENDERS.get_or_init(Default::default).lock().get(text).copied().unwrap_or(0)
}

/// The settings that change a Text cue's rendered planes. Colours are applied as the planes are
/// drawn, so only whether a part is shown (or animated) matters; placement, timing and the
/// animation itself don't. Two cues with equal looks render identical planes.
pub fn look(t: &TextCue) -> TextCue {
    let on = |c: [u8; 4], p: AnimProp| [0, 0, 0, u8::from(c[3] > 0 || t.animation.has(p))];
    TextCue {
        color: [0; 4],
        bg_color: on(t.bg_color, AnimProp::Background),
        shadow_color: on(t.shadow_color, AnimProp::ShadowColor),
        outline_color: on(t.outline_color, AnimProp::OutlineColor),
        animation: Default::default(),
        video_fx: Vec::new(),
        stage: None,
        geometry: Default::default(),
        layer: 0,
        opacity: 1.0,
        duration: 0.0,
        ..t.clone()
    }
}

/// Result slot of a render job.
type Slot = std::sync::Arc<Mutex<Option<std::sync::Arc<TextPlanes>>>>;
/// A queued job: the look, and where the result goes (gone = cancelled).
type Job = (TextCue, std::sync::Weak<Mutex<Option<std::sync::Arc<TextPlanes>>>>);

/// The text render worker: one thread, jobs in order. A job whose [`TextRender`] was dropped
/// (superseded by a newer look while queued, e.g. while dragging a slider) is skipped.
fn worker() -> &'static Mutex<std::sync::mpsc::Sender<Job>> {
    static TX: OnceLock<Mutex<std::sync::mpsc::Sender<Job>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new().name("opnq-text".into()).spawn(move || {
            while let Ok((look, slot)) = rx.recv() {
                if slot.strong_count() == 0 {
                    continue;
                }
                let planes = crate::recovery::catch(|| Ok(render_planes(&look))).unwrap_or_else(|e| {
                    log::error!("text render: {e:#}");
                    TextPlanes::default()
                });
                if let Some(slot) = slot.upgrade() {
                    *slot.lock() = Some(std::sync::Arc::new(planes));
                }
            }
        });
        if let Err(e) = spawned {
            log::error!("cannot start the text render thread: {e}");
        }
        Mutex::new(tx)
    })
}

/// A Text cue being rendered on the background worker (rendering takes milliseconds: too long
/// for the engine thread or a UI frame). Cheap to clone; clones share the result. Dropping every
/// clone before the worker gets to it cancels the job.
#[derive(Clone)]
pub struct TextRender {
    /// The look being rendered ([`look`]).
    pub look: TextCue,
    out: Slot,
}

impl TextRender {
    /// Queue `cue`'s planes for rendering.
    pub fn spawn(cue: &TextCue) -> Self {
        let look = look(cue);
        let out: Slot = std::sync::Arc::new(Mutex::new(None));
        if worker().lock().send((look.clone(), std::sync::Arc::downgrade(&out))).is_err() {
            // No worker: render here rather than show nothing.
            *out.lock() = Some(std::sync::Arc::new(render_planes(&look)));
        }
        Self { look, out }
    }

    /// The planes, once rendered.
    pub fn ready(&self) -> Option<std::sync::Arc<TextPlanes>> {
        self.out.lock().clone()
    }

    /// Wait for the planes (tests and tools).
    pub fn wait(&self) -> std::sync::Arc<TextPlanes> {
        loop {
            if let Some(p) = self.ready() {
                return p;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// One part of a rendered Text cue, back to front (`part as usize` indexes
/// [`crate::media::video`]'s `text_colors`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Part {
    /// The box behind the text.
    Background,
    /// The drop shadow.
    Shadow,
    /// The outline.
    Outline,
    /// The glyphs.
    Fill,
}

/// A Text cue rendered as coverage masks, drawn back to front.
#[derive(Clone, Default)]
pub struct TextPlanes {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Image pixels per stage pixel.
    pub density: f32,
    /// One coverage byte per pixel for each part, back to front.
    pub masks: Vec<(Part, Vec<u8>)>,
    /// Colour glyphs (emoji) as straight RGBA, drawn on top; faded with the fill's alpha.
    pub colour: Option<Vec<u8>>,
}

/// Coverage masks of every part the cue shows or animates.
fn masks(cue: &TextCue, density: f32) -> TextPlanes {
    let mut renders = RENDERS.get_or_init(Default::default).lock();
    // Every edit of a text is a new key: keep a long session's map from growing without end.
    if renders.len() >= MAX_COUNTED && !renders.contains_key(&cue.text) {
        renders.clear();
    }
    *renders.entry(cue.text.clone()).or_default() += 1;
    drop(renders);
    let anim = &cue.animation;
    let want_bg = cue.bg_color[3] > 0 || anim.has(AnimProp::Background);
    let want_shadow = cue.shadow_color[3] > 0 || anim.has(AnimProp::ShadowColor);
    let want_outline = cue.outline_width > 0.0 && (cue.outline_color[3] > 0 || anim.has(AnimProp::OutlineColor));
    let bg_pad = if want_bg { cue.bg_padding.max(0.0) } else { 0.0 };
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
    let effects = cue.outline_width.max(0.0) + if want_shadow {
        cue.shadow_blur.max(0.0) * 1.5 + cue.shadow_offset[0].abs().max(cue.shadow_offset[1].abs())
    } else {
        0.0
    };
    let pad_stage = (size * 0.12 + effects + 2.0).max(bg_pad + 2.0).ceil();
    let (_, w1, h1) = measure(system, 1.0);
    let longest = (w1.max(h1) + 2.0 * pad_stage).max(1.0);
    // A few pixels spare: the padding is rounded up after scaling.
    let density = density.min((super::max_texture_side() as f32 - 8.0) / longest).max(0.1);
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
    let outline = want_outline.then(|| {
        let dist = distance_field(&solid, iw, ih);
        dist.iter().zip(&solid).map(|(d, s)| (outline_px + 0.5 - d).clamp(0.0, 1.0).max(*s)).collect::<Vec<f32>>()
    });

    // Shadow: the text's silhouette (with outline), blurred and offset.
    let shadow = want_shadow.then(|| {
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

    // Background: a (rounded) box around the text block, `bg_padding` from it.
    let background = want_bg.then(|| {
        let bp = bg_pad * density;
        let (x0, y0, x1, y1) = (pad - bp, pad - bp, pad + w + bp, pad + h + bp);
        let r = (cue.bg_radius.max(0.0) * density).min((x1 - x0).min(y1 - y0) / 2.0);
        let (cx, cy, hx, hy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0, (x1 - x0) / 2.0 - r, (y1 - y0) / 2.0 - r);
        let mut m = vec![0f32; iw * ih];
        for y in 0..ih {
            for x in 0..iw {
                // Signed distance to a rounded box, sampled at the pixel centre.
                let (qx, qy) = (((x as f32 + 0.5) - cx).abs() - hx, ((y as f32 + 0.5) - cy).abs() - hy);
                let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
                let d = outside + qx.max(qy).min(0.0) - r;
                m[y * iw + x] = (0.5 - d).clamp(0.0, 1.0);
            }
        }
        m
    });

    let to_u8 = |m: &[f32]| m.iter().map(|v| (v * 255.0).round().clamp(0.0, 255.0) as u8).collect::<Vec<u8>>();
    let mut masks = Vec::new();
    for (part, m) in [(Part::Background, &background), (Part::Shadow, &shadow), (Part::Outline, &outline)] {
        if let Some(m) = m {
            masks.push((part, to_u8(m)));
        }
    }
    masks.push((Part::Fill, to_u8(&fill)));
    // Colour glyphs keep their own colours: straight RGBA.
    let colour = has_colour.then(|| {
        let mut out = vec![0u8; iw * ih * 4];
        for (px, c) in out.as_chunks_mut::<4>().0.iter_mut().zip(&colour) {
            if c[3] > 0.0 {
                let inv = 1.0 / c[3];
                for k in 0..3 {
                    px[k] = (c[k] * inv * 255.0).round().clamp(0.0, 255.0) as u8;
                }
                px[3] = (c[3] * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
        out
    });
    TextPlanes { width: iw as u32, height: ih as u32, density, masks, colour }
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
        img.rgba.as_chunks::<4>().0.iter().filter(|p| p[3] > 128).count()
    }

    #[test]
    fn renders_visible_text_at_density() {
        let cue = TextCue { text: "Hi\nthere".into(), size: 40.0, ..Default::default() };
        let img = render(&cue);
        assert_eq!(img.density, DENSITY);
        assert!(img.width as f32 / img.density > 60.0 && img.height as f32 / img.density > 80.0, "{}x{}", img.width, img.height);
        assert!(lit(&img) > 800, "{} lit pixels", lit(&img));
        // White text with straight alpha: every visible pixel is white.
        assert!(img.rgba.as_chunks::<4>().0.iter().filter(|p| p[3] > 0).all(|p| p[0] == 255));
    }

    #[test]
    fn outline_and_shadow_add_coverage() {
        let base = TextCue { text: "OpnQ".into(), size: 48.0, ..Default::default() };
        let plain = lit(&render_at(&base, 1.0));
        let outlined = render_at(&TextCue { outline_width: 3.0, ..base.clone() }, 1.0);
        assert!(lit(&outlined) > plain * 3 / 2, "{} vs {plain}", lit(&outlined));
        // The outline is black, the fill white: both colours appear.
        assert!(outlined.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 255 && p[0] < 10));
        assert!(outlined.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 255 && p[0] > 245));
        // An offset well past the stroke width, so the shadow barely overlaps the glyphs (the default 4 px left the
        // count right at the threshold, and font rasterisation differences on CI tipped it under).
        let shadowed = render_at(&TextCue { shadow_color: [0, 0, 0, 200], shadow_blur: 0.0, shadow_offset: [12.0, 12.0], ..base }, 1.0);
        let n = shadowed.rgba.as_chunks::<4>().0.iter().filter(|p| p[3] > 100).count();
        assert!(n > plain * 3 / 2, "{n} vs {plain}");
    }

    #[test]
    fn background_box_and_planes() {
        let base = TextCue { text: "Box".into(), size: 40.0, ..Default::default() };
        assert_eq!(render_planes(&base).masks.iter().map(|m| m.0).collect::<Vec<_>>(), vec![Part::Fill]);
        let boxed = TextCue { bg_color: [0, 0, 255, 255], bg_padding: 10.0, bg_radius: 6.0, ..base.clone() };
        let parts: Vec<Part> = render_planes(&boxed).masks.iter().map(|m| m.0).collect();
        assert_eq!(parts, vec![Part::Background, Part::Fill]);
        let img = render_at(&boxed, 1.0);
        let px = |x: u32, y: u32| &img.rgba[((y * img.width + x) * 4) as usize..][..4];
        // The box starts 2 px in (the padding fits inside the image margin), with rounded corners.
        assert_eq!(px(4, img.height / 2), &[0, 0, 255, 255]);
        assert_eq!(px(0, img.height / 2)[3], 0);
        assert!(px(2, 2)[3] < 40, "{:?}", px(2, 2));
        // An animated colour needs its plane even while the colour is transparent.
        let mut anim = base.clone();
        anim.animation.set_key(AnimProp::ShadowColor, 0.0, [0.0, 0.0, 0.0, 1.0]);
        assert!(render_planes(&anim).masks.iter().any(|m| m.0 == Part::Shadow));
    }

    #[test]
    fn look_ignores_what_is_applied_while_drawing() {
        let a = TextCue { text: "Look".into(), shadow_color: [0, 0, 0, 200], ..Default::default() };
        let mut b = a.clone();
        b.color = [255, 0, 0, 128];
        b.shadow_color = [255, 255, 0, 50];
        b.geometry.x = 0.4;
        b.opacity = 0.3;
        b.layer = 7;
        b.duration = 3.0;
        b.animation.set_key(AnimProp::X, 1.0, [0.5; 4]);
        b.animation.set_key(AnimProp::TextColor, 1.0, [1.0; 4]);
        assert_eq!(look(&a), look(&b), "these are applied while drawing, not rendered");
        for changed in [
            TextCue { size: 20.0, ..a.clone() },
            TextCue { text: "Other".into(), ..a.clone() },
            TextCue { shadow_color: [0; 4], ..a.clone() },
            TextCue { bg_color: [0, 0, 0, 1], ..a.clone() },
        ] {
            assert_ne!(look(&a), look(&changed));
        }
        // Animating a colour that is off brings its plane in.
        let mut c = a.clone();
        c.animation.set_key(AnimProp::Background, 0.0, [1.0; 4]);
        assert_ne!(look(&a), look(&c));
    }

    #[test]
    fn render_jobs_are_off_thread_and_superseded_ones_are_skipped() {
        let text = "superseded render jobs";
        let t0 = std::time::Instant::now();
        // A slider drag: a new look every frame, each replacing the last.
        let mut job = None;
        for i in 0..40 {
            let cue = TextCue { text: text.into(), size: 40.0 + i as f32, outline_width: 3.0, ..Default::default() };
            job = Some(TextRender::spawn(&cue));
        }
        assert!(t0.elapsed() < std::time::Duration::from_millis(20), "queueing renders blocked: {:?}", t0.elapsed());
        let last = job.unwrap().wait();
        assert!(last.width > 0);
        // The worker only rendered the jobs still wanted when it got to them.
        assert!(render_count(text) <= 3, "{} renders for 40 superseded looks", render_count(text));
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
