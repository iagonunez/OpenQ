//! Light plot: the patch seen from above as a drawing (the Plan view on paper), as SVG or PDF.
//!
//! x to the right, the audience at the bottom (+z), a 1 m grid. Rectangles (stage, risers, set)
//! and lighting bars are drawn under the fixtures. Each fixture is a symbol (a circle for moving
//! heads, a rectangle for the rest) turned by its heading, with its number inside, its patch
//! (universe.address) above and its distance from the centre line below; a legend lists the
//! fixture types.
//!
//! One drawing routine (`draw`) paints onto a `Canvas`; the SVG is drawn at a fixed 60 px per
//! metre on a page of its own size, the PDF on an A3 landscape sheet at the largest standard
//! scale (1:20 … 1:1000) that fits, written by a small PDF writer (base-14 Helvetica, no
//! embedded fonts).

use super::model::{Lighting, ShapeKind};
use std::fmt::Write;

/// File extension of the SVG plot.
pub const EXTENSION: &str = "svg";
/// File extension of the PDF plot.
pub const PDF_EXTENSION: &str = "pdf";
/// SVG drawing scale, units (px) per metre.
const SCALE: f32 = 60.0;
/// A3 landscape, points.
const A3: [f32; 2] = [1190.55, 841.89];
/// PDF page margin, points.
const MARGIN: f32 = 28.0;
/// Standard drawing scales tried for the PDF, largest first (1:n).
const PDF_SCALES: [u32; 10] = [20, 25, 50, 75, 100, 125, 200, 250, 500, 1000];

/// Text alignment around its x position.
#[derive(Clone, Copy, PartialEq)]
enum Anchor {
    Start,
    Middle,
}

/// Where a plot is painted. y grows downwards; colours are greys (0 black … 255 white).
trait Canvas {
    fn line(&mut self, a: [f32; 2], b: [f32; 2], width: f32, grey: u8, dashed: bool, round: bool);
    fn rect(&mut self, at: [f32; 2], size: [f32; 2], radius: f32, fill: Option<u8>, stroke: u8, width: f32);
    fn circle(&mut self, c: [f32; 2], r: f32, fill: Option<u8>, stroke: u8, width: f32);
    fn text(&mut self, at: [f32; 2], size: f32, bold: bool, grey: u8, anchor: Anchor, text: &str);
    /// Move the origin to `at` and turn by `deg` (clockwise on paper) until [`Canvas::pop`].
    fn push(&mut self, at: [f32; 2], deg: f32);
    fn pop(&mut self);
}

/// Extents and legend of a design, in metres.
struct Layout {
    x0: f32,
    x1: f32,
    z0: f32,
    z1: f32,
    /// Fixture types: (name, moving head, count).
    types: Vec<(String, bool, usize)>,
}

impl Layout {
    fn of(l: &Lighting) -> Self {
        let (mut x0, mut x1, mut z0, mut z1) = (-2.0f32, 2.0f32, -2.0f32, 2.0f32);
        let fixtures = l.fixtures.iter().map(|f| [f.position[0], f.position[2]]);
        for p in fixtures.chain(l.shapes.iter().flat_map(|s| [s.a, s.b])) {
            x0 = x0.min(p[0]);
            x1 = x1.max(p[0]);
            z0 = z0.min(p[1]);
            z1 = z1.max(p[1]);
        }
        let mut types: Vec<(String, bool, usize)> = Vec::new();
        for f in &l.fixtures {
            let (name, moving) = l.fixture_type(&f.fixture_type).map_or((f.fixture_type.clone(), false), |t| {
                let n = if t.manufacturer.is_empty() { t.name.clone() } else { format!("{} {}", t.manufacturer, t.name) };
                (n, t.physical.moving)
            });
            match types.iter_mut().find(|t| t.0 == name) {
                Some(t) => t.2 += 1,
                None => types.push((name, moving, 1)),
            }
        }
        // Whole metres with a margin.
        Self { x0: (x0 - 1.0).floor(), x1: (x1 + 1.0).ceil(), z0: (z0 - 1.0).floor(), z1: (z1 + 1.0).ceil(), types }
    }

    /// Drawing size in metres.
    fn size(&self) -> [f32; 2] {
        [self.x1 - self.x0, self.z1 - self.z0]
    }

    /// Legend height for `cols` columns.
    fn legend_height(&self, cols: usize, row: f32) -> f32 {
        24.0 + self.types.len().div_ceil(cols.max(1)) as f32 * row
    }
}

/// How a plot sits on its page.
struct Frame {
    /// Top-left corner of the grid.
    origin: [f32; 2],
    /// Page units per metre.
    scale: f32,
    /// Symbol and label size factor (1 at the SVG scale).
    sym: f32,
    /// Title and the line under it.
    title: String,
    subtitle: String,
    /// Legend left edge, columns and their width.
    legend_x: f32,
    legend_cols: usize,
    legend_col_w: f32,
}

/// Paint the plot of `l`.
fn draw(l: &Lighting, lay: &Layout, fr: &Frame, c: &mut dyn Canvas) {
    let (w, h) = (lay.size()[0] * fr.scale, lay.size()[1] * fr.scale);
    let [ox, oy] = fr.origin;
    let px = |x: f32| ox + (x - lay.x0) * fr.scale;
    let pz = |z: f32| oy + (z - lay.z0) * fr.scale;
    let s = fr.sym;
    c.text([ox + 10.0, oy - 20.0], 20.0, true, 0, Anchor::Start, &fr.title);
    if !fr.subtitle.is_empty() {
        c.text([ox + w - 4.0 - text_width(&fr.subtitle, 11.0), oy - 20.0], 11.0, false, 85, Anchor::Start, &fr.subtitle);
    }
    for i in 0..=(lay.size()[0] as i32) {
        let x = ox + i as f32 * fr.scale;
        c.line([x, oy], [x, oy + h], 1.0, 221, false, false);
    }
    for i in 0..=(lay.size()[1] as i32) {
        let y = oy + i as f32 * fr.scale;
        c.line([ox, y], [ox + w, y], 1.0, 221, false, false);
    }
    for kind in [ShapeKind::Rect, ShapeKind::Bar] {
        for sh in l.shapes.iter().filter(|x| x.kind == kind) {
            let (ax, ay, bx, by) = (px(sh.a[0]), pz(sh.a[1]), px(sh.b[0]), pz(sh.b[1]));
            match kind {
                ShapeKind::Rect => {
                    let (x, y) = (ax.min(bx), ay.min(by));
                    c.rect([x, y], [(bx - ax).abs(), (by - ay).abs()], 0.0, Some(242), 102, 1.5);
                    if !sh.name.is_empty() {
                        c.text([x + 6.0, y + 16.0 * s], 12.0 * s, false, 102, Anchor::Start, &sh.name);
                    }
                }
                ShapeKind::Bar => {
                    c.line([ax, ay], [bx, by], 5.0 * s, 51, false, true);
                    let (lx, ly) = if ax <= bx { (ax, ay) } else { (bx, by) };
                    let label = if sh.name.is_empty() { format!("{:.2} m", sh.height) } else { format!("{} · {:.2} m", sh.name, sh.height) };
                    c.text([lx, ly - 22.0 * s], 11.0 * s, false, 51, Anchor::Start, &label);
                }
            }
        }
    }
    // Centre line and the audience side.
    if lay.x0 < 0.0 && lay.x1 > 0.0 {
        c.line([px(0.0), oy], [px(0.0), oy + h], 1.0, 153, true, false);
    }
    c.text([ox + w / 2.0, oy + h - 6.0], 12.0, false, 153, Anchor::Middle, "AUDIENCE");
    for f in &l.fixtures {
        let moving = l.fixture_type(&f.fixture_type).is_some_and(|t| t.physical.moving);
        let (cx, cy) = (px(f.position[0]), pz(f.position[2]));
        c.push([cx, cy], -f.rotation[1]);
        // The heading mark stays clear of the number in the middle.
        if moving {
            c.circle([0.0, 0.0], 12.0 * s, Some(255), 0, 2.0 * s);
            c.line([0.0, 8.0 * s], [0.0, 12.0 * s], 1.5 * s, 0, false, false);
        } else {
            c.rect([-10.0 * s, -14.0 * s], [20.0 * s, 28.0 * s], 3.0 * s, Some(255), 0, 2.0 * s);
            c.line([-10.0 * s, 10.0 * s], [10.0 * s, 10.0 * s], 1.0, 0, false, false);
        }
        c.pop();
        // The number fits inside the symbol (smaller for long numbers).
        let number = f.id.to_string();
        let size = (17.0 * s / (text_width(&number, 1.0) + 0.01)).min(11.0 * s);
        c.text([cx, cy + size * 0.36], size, true, 0, Anchor::Middle, &number);
        let patch = if f.address > 0 { format!("{}.{}", f.universe, f.address) } else { "—".to_string() };
        c.text([cx, cy - 20.0 * s], 10.0 * s, true, 51, Anchor::Middle, &patch);
        c.text([cx, cy + 27.0 * s], 9.0 * s, false, 102, Anchor::Middle, &centre_distance(f.position[0]));
    }
    let rows = lay.types.len().div_ceil(fr.legend_cols.max(1)).max(1);
    for (i, (name, moving, n)) in lay.types.iter().enumerate() {
        let x = fr.legend_x + (i / rows) as f32 * fr.legend_col_w;
        let y = oy + h + 24.0 + (i % rows) as f32 * 20.0;
        if *moving {
            c.circle([x + 16.0, y - 4.0], 6.0, None, 0, 1.0);
        } else {
            c.rect([x + 11.0, y - 10.0], [10.0, 12.0], 0.0, None, 0, 1.0);
        }
        c.text([x + 30.0, y], 12.0, false, 0, Anchor::Start, &format!("{n} × {name}"));
    }
}

/// Distance from the centre line, metres ("CL" on it).
fn centre_distance(x: f32) -> String {
    if x.abs() < 0.005 { "CL".to_string() } else { format!("{:.2} m", x.abs()) }
}

/// Approximate Helvetica advance width of `s` at `size` (centres labels in the PDF and places
/// the subtitle; good enough for numbers and short names).
fn text_width(s: &str, size: f32) -> f32 {
    let em: f32 = s
        .chars()
        .map(|ch| match ch {
            '0'..='9' => 0.556,
            ' ' | '.' | ',' | ':' | 'i' | 'l' | 'j' | 'I' | '!' => 0.278,
            'f' | 't' | 'r' | '(' | ')' | '/' => 0.333,
            'm' | 'w' | 'M' | 'W' => 0.833,
            'A'..='Z' => 0.667,
            _ => 0.52,
        })
        .sum();
    em * size
}

fn grey_hex(g: u8) -> String {
    format!("#{g:02x}{g:02x}{g:02x}")
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// SVG output.
struct Svg(String);

impl Canvas for Svg {
    fn line(&mut self, a: [f32; 2], b: [f32; 2], width: f32, grey: u8, dashed: bool, round: bool) {
        let dash = if dashed { r#" stroke-dasharray="8 4""# } else { "" };
        let cap = if round { r#" stroke-linecap="round""# } else { "" };
        let _ = writeln!(self.0, r#"<line x1="{}" y1="{}" x2="{}" y2="{}" stroke="{}" stroke-width="{width}"{dash}{cap}/>"#, a[0], a[1], b[0], b[1], grey_hex(grey));
    }
    fn rect(&mut self, at: [f32; 2], size: [f32; 2], radius: f32, fill: Option<u8>, stroke: u8, width: f32) {
        let fill = fill.map_or("none".into(), grey_hex);
        let rx = if radius > 0.0 { format!(r#" rx="{radius}""#) } else { String::new() };
        let _ = writeln!(self.0, r#"<rect x="{}" y="{}" width="{}" height="{}"{rx} fill="{fill}" stroke="{}" stroke-width="{width}"/>"#, at[0], at[1], size[0], size[1], grey_hex(stroke));
    }
    fn circle(&mut self, c: [f32; 2], r: f32, fill: Option<u8>, stroke: u8, width: f32) {
        let fill = fill.map_or("none".into(), grey_hex);
        let _ = writeln!(self.0, r#"<circle cx="{}" cy="{}" r="{r}" fill="{fill}" stroke="{}" stroke-width="{width}"/>"#, c[0], c[1], grey_hex(stroke));
    }
    fn text(&mut self, at: [f32; 2], size: f32, bold: bool, grey: u8, anchor: Anchor, text: &str) {
        let weight = if bold { r#" font-weight="bold""# } else { "" };
        let anchor = if anchor == Anchor::Middle { r#" text-anchor="middle""# } else { "" };
        let _ = writeln!(self.0, r#"<text x="{}" y="{}" font-size="{size}"{weight} fill="{}"{anchor}>{}</text>"#, at[0], at[1], grey_hex(grey), esc(text));
    }
    fn push(&mut self, at: [f32; 2], deg: f32) {
        let _ = writeln!(self.0, r#"<g transform="translate({} {}) rotate({deg})">"#, at[0], at[1]);
    }
    fn pop(&mut self) {
        self.0.push_str("</g>\n");
    }
}

/// The plot of a design as an SVG drawing, titled `title`.
pub fn svg(l: &Lighting, title: &str) -> String {
    let lay = Layout::of(l);
    let head = 50.0;
    let (w, h) = (lay.size()[0] * SCALE, lay.size()[1] * SCALE);
    let total_h = head + h + lay.legend_height(1, 20.0);
    let fr = Frame { origin: [0.0, head], scale: SCALE, sym: 1.0, title: title.into(), subtitle: String::new(), legend_x: 0.0, legend_cols: 1, legend_col_w: w };
    let mut c = Svg(String::new());
    let _ = writeln!(c.0, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    let _ = writeln!(c.0, r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{total_h}" viewBox="0 0 {w} {total_h}" font-family="sans-serif">"#);
    let _ = writeln!(c.0, r#"<rect width="{w}" height="{total_h}" fill="white"/>"#);
    draw(l, &lay, &fr, &mut c);
    c.0.push_str("</svg>\n");
    c.0
}

/// PDF page content (y down: the stream starts by flipping the page).
struct Pdf {
    ops: String,
}

impl Pdf {
    fn stroke_style(&mut self, grey: u8, width: f32) {
        let _ = write!(self.ops, "{:.3} G {width:.2} w ", grey as f32 / 255.0);
    }
    fn fill_grey(&mut self, grey: u8) {
        let _ = write!(self.ops, "{:.3} g ", grey as f32 / 255.0);
    }
    fn paint(&mut self, fill: Option<u8>) {
        self.ops.push_str(if fill.is_some() { "B\n" } else { "S\n" });
    }
}

/// `text` as a PDF string in WinAnsiEncoding (Latin-1 plus a few punctuation marks; anything
/// else becomes `?`).
fn pdf_string(text: &str) -> String {
    let mut out = String::from("(");
    for ch in text.chars() {
        let b: u8 = match ch {
            '\u{20}'..='\u{7e}' => ch as u8,
            '\u{a0}'..='\u{ff}' => ch as u32 as u8,
            '€' => 0x80,
            '…' => 0x85,
            '–' => 0x96,
            '—' => 0x97,
            '‘' => 0x91,
            '’' => 0x92,
            '“' => 0x93,
            '”' => 0x94,
            '•' => 0x95,
            _ => b'?',
        };
        match b {
            b'(' | b')' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => {
                let _ = write!(out, "\\{b:03o}");
            }
        }
    }
    out.push(')');
    out
}

impl Canvas for Pdf {
    fn line(&mut self, a: [f32; 2], b: [f32; 2], width: f32, grey: u8, dashed: bool, round: bool) {
        self.stroke_style(grey, width);
        if dashed {
            self.ops.push_str("[8 4] 0 d ");
        }
        if round {
            self.ops.push_str("1 J ");
        }
        let _ = write!(self.ops, "{:.2} {:.2} m {:.2} {:.2} l S", a[0], a[1], b[0], b[1]);
        if dashed {
            self.ops.push_str(" [] 0 d");
        }
        if round {
            self.ops.push_str(" 0 J");
        }
        self.ops.push('\n');
    }
    fn rect(&mut self, at: [f32; 2], size: [f32; 2], radius: f32, fill: Option<u8>, stroke: u8, width: f32) {
        self.stroke_style(stroke, width);
        if let Some(f) = fill {
            self.fill_grey(f);
        }
        let [x, y] = at;
        let [w, h] = size;
        let r = radius.min(w / 2.0).min(h / 2.0);
        if r <= 0.0 {
            let _ = write!(self.ops, "{x:.2} {y:.2} {w:.2} {h:.2} re ");
        } else {
            // Corners as quarter circles (Bézier).
            let k = r * (1.0 - 0.5523);
            let (x1, y1) = (x + w, y + h);
            let _ = write!(
                self.ops,
                "{:.2} {y:.2} m {:.2} {y:.2} l {:.2} {y:.2} {x1:.2} {:.2} {x1:.2} {:.2} c {x1:.2} {:.2} l {x1:.2} {:.2} {:.2} {y1:.2} {:.2} {y1:.2} c \
                 {:.2} {y1:.2} l {:.2} {y1:.2} {x:.2} {:.2} {x:.2} {:.2} c {x:.2} {:.2} l {x:.2} {:.2} {:.2} {y:.2} {:.2} {y:.2} c h ",
                x + r, x1 - r, x1 - k, y + k, y + r, y1 - r, y1 - k, x1 - k, x1 - r,
                x + r, x + k, y1 - k, y1 - r, y + r, y + k, x + k, x + r,
            );
        }
        self.paint(fill);
    }
    fn circle(&mut self, c: [f32; 2], r: f32, fill: Option<u8>, stroke: u8, width: f32) {
        self.stroke_style(stroke, width);
        if let Some(f) = fill {
            self.fill_grey(f);
        }
        let k = r * 0.5523;
        let [x, y] = c;
        let _ = write!(
            self.ops,
            "{:.2} {y:.2} m {:.2} {:.2} {:.2} {:.2} {x:.2} {:.2} c {:.2} {:.2} {:.2} {:.2} {:.2} {y:.2} c \
             {:.2} {:.2} {:.2} {:.2} {x:.2} {:.2} c {:.2} {:.2} {:.2} {:.2} {:.2} {y:.2} c h ",
            x + r, x + r, y + k, x + k, y + r, y + r,
            x - k, y + r, x - r, y + k, x - r,
            x - r, y - k, x - k, y - r, y - r,
            x + k, y - r, x + r, y - k, x + r,
        );
        self.paint(fill);
    }
    fn text(&mut self, at: [f32; 2], size: f32, bold: bool, grey: u8, anchor: Anchor, text: &str) {
        let x = match anchor {
            Anchor::Start => at[0],
            Anchor::Middle => at[0] - text_width(text, size) / 2.0,
        };
        self.fill_grey(grey);
        // The page is flipped: flip the text back.
        let font = if bold { "F2" } else { "F1" };
        let _ = writeln!(self.ops, "BT /{font} {size:.2} Tf 1 0 0 -1 {x:.2} {:.2} Tm {} Tj ET", at[1], pdf_string(text));
    }
    fn push(&mut self, at: [f32; 2], deg: f32) {
        let (s, c) = deg.to_radians().sin_cos();
        let _ = writeln!(self.ops, "q {c:.5} {s:.5} {:.5} {c:.5} {:.2} {:.2} cm", -s, at[0], at[1]);
    }
    fn pop(&mut self) {
        self.ops.push_str("Q\n");
    }
}

/// Points per metre at 1:`n`.
fn points_per_metre(n: u32) -> f32 {
    1000.0 / n as f32 * 72.0 / 25.4
}

/// The plot of a design as a one-page PDF (A3 landscape), titled `title`.
pub fn pdf(l: &Lighting, title: &str) -> Vec<u8> {
    let lay = Layout::of(l);
    let [pw, ph] = A3;
    let head = 44.0;
    let avail_w = pw - 2.0 * MARGIN;
    let legend_col_w = 270.0;
    let legend_cols = ((avail_w / legend_col_w) as usize).max(1);
    let legend_h = lay.legend_height(legend_cols, 20.0);
    let avail_h = ph - 2.0 * MARGIN - head - legend_h;
    // The largest standard scale that fits (the smallest if nothing does).
    let n = PDF_SCALES
        .iter()
        .copied()
        .find(|&n| {
            let s = points_per_metre(n);
            lay.size()[0] * s <= avail_w && lay.size()[1] * s <= avail_h
        })
        .unwrap_or(PDF_SCALES[PDF_SCALES.len() - 1]);
    let scale = points_per_metre(n);
    let w = lay.size()[0] * scale;
    // Centred across, from the top.
    let origin = [MARGIN + (avail_w - w).max(0.0) / 2.0, MARGIN + head];
    // The legend lines up with the grid when it still fits under it that way.
    let cols_under = (((pw - MARGIN - origin[0]) / legend_col_w) as usize).max(1);
    let fits = origin[1] + lay.size()[1] * scale + lay.legend_height(cols_under, 20.0) <= ph - MARGIN;
    let (legend_x, legend_cols) = if fits { (origin[0], cols_under) } else { (MARGIN, legend_cols) };
    let fr = Frame {
        origin,
        scale,
        sym: (scale / SCALE).clamp(0.5, 1.0),
        title: title.into(),
        subtitle: format!("Scale 1:{n} (A3) · {} fixtures · 1 m grid", l.fixtures.len()),
        legend_x,
        legend_cols,
        legend_col_w,
    };
    let mut c = Pdf { ops: format!("1 0 0 -1 0 {ph:.2} cm 1 j\n") };
    draw(l, &lay, &fr, &mut c);
    write_pdf(&c.ops, A3, title)
}

/// A one-page PDF with `content` as its page stream (deflated) and Helvetica / Helvetica-Bold
/// as F1 / F2.
fn write_pdf(content: &str, size: [f32; 2], title: &str) -> Vec<u8> {
    use std::io::Write as _;
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = z.write_all(content.as_bytes());
    let stream = z.finish().unwrap_or_default();
    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {:.2} {:.2}] /Resources << /Font << /F1 5 0 R /F2 6 0 R >> >> /Contents 4 0 R >>",
            size[0], size[1]
        )
        .into_bytes(),
        {
            let mut s = format!("<< /Length {} /Filter /FlateDecode >>\nstream\n", stream.len()).into_bytes();
            s.extend_from_slice(&stream);
            s.extend_from_slice(b"\nendstream");
            s
        },
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>".to_vec(),
        format!("<< /Title {} /Producer (OpenQ) >>", pdf_string(title)).into_bytes(),
    ];
    let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    let mut tail = format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for o in offsets {
        let _ = writeln!(tail, "{o:010} 00000 n ");
    }
    let _ = write!(tail, "trailer\n<< /Size {} /Root 1 0 R /Info 7 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1);
    out.extend_from_slice(tail.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::library;

    fn rig() -> Lighting {
        let mut l = library::bench_rig(6, 1, 1);
        l.shapes.push(crate::light::model::PlotShape { id: 1, kind: ShapeKind::Bar, name: "LX1".into(), a: [-4.0, -2.0], b: [4.0, -2.0], height: 6.0 });
        l.shapes.push(crate::light::model::PlotShape { id: 2, kind: ShapeKind::Rect, name: "Stage".into(), a: [-6.0, -5.0], b: [6.0, 1.0], height: 0.0 });
        for (i, f) in l.fixtures.iter_mut().enumerate() {
            f.position = [i as f32 - 3.0, 5.0, -2.0];
        }
        l
    }

    #[test]
    fn plot_has_every_fixture() {
        let l = rig();
        let s = svg(&l, "Tom & Jerry");
        assert!(s.contains("Tom &amp; Jerry"));
        assert_eq!(s.matches("<g transform=\"translate(").count(), 6, "one symbol per fixture");
        let kinds: std::collections::HashSet<&str> = l.fixtures.iter().map(|f| f.fixture_type.as_str()).collect();
        assert_eq!(s.matches(" × ").count(), kinds.len(), "a legend row per type");
        assert!(s.ends_with("</svg>\n"));
        assert!(s.contains("LX1 · 6.00 m") && s.contains(">Stage</text>"));
        // Patch above, distance from the centre line below.
        let f = &l.fixtures[0];
        assert!(s.contains(&format!(">{}.{}</text>", f.universe, f.address)));
        assert!(s.contains(">3.00 m</text>") && s.contains(">CL</text>"));
        // The stage fits: the drawing is at least 12 m (+ margins) wide.
        assert!(s.contains(r#"width="840""#), "{}", &s[..300]);
    }

    /// The PDF is well formed (xref offsets point at their objects), picks a scale that fits A3
    /// and carries the labels.
    #[test]
    fn pdf_plot() {
        use std::io::Read;
        let l = rig();
        let p = pdf(&l, "Tom & Jerry (×2)");
        assert!(p.starts_with(b"%PDF-1.4\n") && p.ends_with(b"%%EOF\n"));
        // Byte for byte (the deflated stream isn't text).
        let text: String = p.iter().map(|&b| if b < 128 { b as char } else { '?' }).collect();
        let start: usize = text.rsplit("startxref\n").next().unwrap().lines().next().unwrap().parse().unwrap();
        assert!(text[start..].starts_with("xref\n0 8\n"));
        for (i, line) in text[start..].lines().skip(3).take(7).enumerate() {
            let off: usize = line[..10].parse().unwrap();
            assert!(text[off..].starts_with(&format!("{} 0 obj", i + 1)), "object {}", i + 1);
        }
        // The page content.
        let s = text.find("stream\n").unwrap() + 7;
        let len: usize = text[..s].rsplit("/Length ").next().unwrap().split(' ').next().unwrap().parse().unwrap();
        let mut ops = String::new();
        flate2::read::ZlibDecoder::new(&p[s..s + len]).read_to_string(&mut ops).unwrap();
        assert!(ops.contains(r"(Tom & Jerry \(\3272\)) Tj"), "WinAnsi ×, escaped parentheses");
        assert!(ops.contains("(LX1 \\267 6.00 m) Tj"));
        assert_eq!(ops.matches("\nQ\n").count() + ops.starts_with("Q\n") as usize, 6, "one symbol per fixture");
        // 18 m × 13 m with margins fits A3 at 1:50.
        assert!(ops.contains("(Scale 1:50 (A3)") || ops.contains(r"(Scale 1:50 \(A3\)"), "{}", &ops[..400]);
        // A huge rig falls back to a small scale.
        let mut big = rig();
        big.fixtures[0].position = [400.0, 0.0, 300.0];
        let p = pdf(&big, "Big");
        assert!(!p.is_empty());
    }

    #[test]
    fn pdf_strings() {
        assert_eq!(pdf_string(r"a(b)\c"), r"(a\(b\)\\c)");
        assert_eq!(pdf_string("Ñ€✓"), r"(\321\200?)");
    }
}
