//! Light plot: the patch seen from above as a drawing (the Plan view on paper), as SVG or PDF.
//!
//! x to the right, the audience at the bottom (+z), a 1 m grid. Rectangles (stage, risers, set)
//! and lighting bars are drawn under the fixtures. Each fixture is a symbol ([`PlotSymbol`]: a
//! circle for moving heads, else guessed from the type's name, or the user's pick per type or per
//! fixture) turned by its heading, with its number inside, its patch (universe.address) above and
//! its distance from the centre line below; a legend lists the fixture types with their symbols. Labels never land on each other or on a symbol: a crowded one moves out of the
//! way (`Labels`); bar and rectangle names stay where the user dragged them in the Plan view
//! ([`super::model::PlotShape::label`]).
//!
//! The plot editor's changes ([`super::model::PlotLayout`]: items moved, re-worded or hidden, texts
//! added) are applied while drawing; [`page`] paints the PDF's page onto any [`Canvas`] (the editor
//! draws it on screen) and says where each item landed.
//!
//! One drawing routine (`draw`) paints onto a `Canvas`; the SVG is drawn at a fixed 60 px per
//! metre on a page of its own size, the PDF on an A3 landscape sheet at the largest standard
//! scale (1:20 … 1:1000) that fits, written by a small PDF writer (base-14 Helvetica, no
//! embedded fonts).

use super::model::{Fixture, Lighting, PlotEdit, PlotItem, PlotSymbol, ShapeKind};
use std::collections::HashMap;
use std::fmt::Write;

/// File extension of the SVG plot.
pub const EXTENSION: &str = "svg";
/// File extension of the PDF plot.
pub const PDF_EXTENSION: &str = "pdf";
/// SVG drawing scale, units (px) per metre.
const SCALE: f32 = 60.0;
/// A3 landscape, points.
pub const A3: [f32; 2] = [1190.55, 841.89];
/// PDF page margin, points.
const MARGIN: f32 = 28.0;
/// Standard drawing scales tried for the PDF, largest first (1:n).
const PDF_SCALES: [u32; 10] = [20, 25, 50, 75, 100, 125, 200, 250, 500, 1000];

/// Text alignment around its x position.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Anchor {
    /// The text starts at x.
    Start,
    /// The text is centred on x.
    Middle,
}

/// A text colour (sRGB).
pub type Rgb = [u8; 3];

/// Where a plot is painted. y grows downwards; lines and shapes are greys (0 black … 255 white).
pub trait Canvas {
    /// A straight line, `width` wide, dashed or with round caps.
    fn line(&mut self, a: [f32; 2], b: [f32; 2], width: f32, grey: u8, dashed: bool, round: bool);
    /// A rectangle (top-left corner, size) with rounded corners, filled or not, outlined.
    fn rect(&mut self, at: [f32; 2], size: [f32; 2], radius: f32, fill: Option<u8>, stroke: u8, width: f32);
    /// A closed convex polygon, filled or not, outlined.
    fn polygon(&mut self, points: &[[f32; 2]], fill: Option<u8>, stroke: u8, width: f32);
    /// A circle, filled or not, outlined.
    fn circle(&mut self, c: [f32; 2], r: f32, fill: Option<u8>, stroke: u8, width: f32);
    /// One line of text, `at` its baseline.
    fn text(&mut self, at: [f32; 2], size: f32, bold: bool, colour: Rgb, anchor: Anchor, text: &str);
    /// Move the origin to `at` and turn by `deg` (clockwise on paper) until [`Canvas::pop`].
    fn push(&mut self, at: [f32; 2], deg: f32);
    /// Undo the last [`Canvas::push`].
    fn pop(&mut self);
}

/// Where an item of the plot landed, for the plot editor.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// The item.
    pub item: PlotItem,
    /// Its box on the page (left, top, right, bottom), page units, y down.
    pub rect: [f32; 4],
    /// Its usual text (before the editor's change).
    pub text: String,
    /// Its usual text size, in [`PlotEdit::size`]'s units.
    pub size: f32,
    /// Its usual text colour.
    pub colour: Rgb,
    /// How far it was moved out of the other labels' way, page units.
    pub nudged: [f32; 2],
}

/// The PDF's page as [`page`] painted it.
#[derive(Clone, Debug)]
pub struct Page {
    /// Page size, points.
    pub size: [f32; 2],
    /// Points per metre on the grid.
    pub scale: f32,
    /// Middle of the grid, metres (x, z).
    pub centre: [f32; 2],
    /// Where each item landed, in drawing order.
    pub hits: Vec<Hit>,
}

/// Extents and legend of a design, in metres.
struct Layout {
    x0: f32,
    x1: f32,
    z0: f32,
    z1: f32,
    /// Fixture types: (name, usual symbol, count).
    types: Vec<(String, PlotSymbol, usize)>,
}

impl Layout {
    fn of(l: &Lighting) -> Self {
        let (mut x0, mut x1, mut z0, mut z1) = (-2.0f32, 2.0f32, -2.0f32, 2.0f32);
        let fixtures = l.fixtures.iter().map(|f| [f.position[0], f.position[2]]);
        // Positions out of any venue (a damaged or hostile show) are left out of the frame.
        for p in fixtures.chain(l.shapes.iter().flat_map(|s| [s.a, s.b])).filter(|p| p.iter().all(|v| v.abs() <= MAX_EXTENT)) {
            x0 = x0.min(p[0]);
            x1 = x1.max(p[0]);
            z0 = z0.min(p[1]);
            z1 = z1.max(p[1]);
        }
        let mut types: Vec<(String, PlotSymbol, usize)> = Vec::new();
        for f in &l.fixtures {
            let (name, symbol) = type_of(l, f);
            match types.iter_mut().find(|t| t.0 == name) {
                Some(t) => t.2 += 1,
                None => types.push((name, symbol, 1)),
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

/// A fixture's type as the legend names it ("Manufacturer Name") and its usual symbol.
fn type_of(l: &Lighting, f: &Fixture) -> (String, PlotSymbol) {
    l.fixture_type(&f.fixture_type).map_or((f.fixture_type.clone(), PlotSymbol::Rectangle), |t| {
        let n = if t.manufacturer.is_empty() { t.name.clone() } else { format!("{} {}", t.manufacturer, t.name) };
        let symbol = PlotSymbol::guess(&n, t.physical.moving);
        (n, symbol)
    })
}

/// The legend name of a fixture's type (the key of its [`PlotItem::LegendRow`]).
pub fn type_name(l: &Lighting, f: &Fixture) -> String {
    type_of(l, f).0
}

/// The symbol a fixture is drawn with: its own pick, its type's ([`PlotItem::LegendRow`]), else
/// [`PlotSymbol::guess`].
pub fn symbol_of(l: &Lighting, f: &Fixture) -> PlotSymbol {
    let ty = type_of(l, f);
    let pick = |i: &PlotItem| l.plot.edit(i).and_then(|e| e.symbol);
    pick(&PlotItem::Fixture(f.id)).or_else(|| pick(&PlotItem::LegendRow(ty.0.clone()))).unwrap_or(ty.1)
}

/// The symbol fixtures of the type named `name` get (unless one has its own), and the usual one
/// (None: no fixture of that type).
pub fn type_symbol(l: &Lighting, name: &str) -> Option<(PlotSymbol, PlotSymbol)> {
    let usual = l.fixtures.iter().map(|f| type_of(l, f)).find(|t| t.0 == name)?.1;
    let pick = l.plot.edit(&PlotItem::LegendRow(name.to_string())).and_then(|e| e.symbol);
    Some((pick.unwrap_or(usual), usual))
}

/// Half width and half depth of a symbol facing down the page, at the SVG scale.
fn half_size(sym: PlotSymbol) -> [f32; 2] {
    match sym {
        PlotSymbol::Circle | PlotSymbol::Effect => [12.0, 12.0],
        PlotSymbol::Rectangle | PlotSymbol::Par => [10.0, 14.0],
        PlotSymbol::Profile => [9.0, 14.0],
        PlotSymbol::Fresnel => [11.0, 13.0],
        PlotSymbol::Flood => [13.0, 12.0],
        PlotSymbol::Strobe => [14.0, 7.0],
        PlotSymbol::Batten => [20.0, 4.0],
    }
}

/// Paint a fixture symbol centred on `at`, turned by `deg`, `s` times its SVG size (white with
/// a black outline; the front, where the light comes out, is down before turning).
pub fn paint_symbol(c: &mut dyn Canvas, sym: PlotSymbol, at: [f32; 2], deg: f32, s: f32) {
    let p = |x: f32, y: f32| [x * s, y * s];
    let w = 2.0 * s;
    c.push(at, deg);
    match sym {
        PlotSymbol::Circle => {
            c.circle([0.0, 0.0], 12.0 * s, Some(255), 0, w);
            // The heading mark stays clear of the number in the middle.
            c.line(p(0.0, 8.0), p(0.0, 12.0), 1.5 * s, 0, false, false);
        }
        PlotSymbol::Rectangle => {
            c.rect(p(-10.0, -14.0), p(20.0, 28.0), 3.0 * s, Some(255), 0, w);
            c.line(p(-10.0, 10.0), p(10.0, 10.0), s, 0, false, false);
        }
        PlotSymbol::Profile => {
            c.polygon(&[p(-9.0, -14.0), p(9.0, -14.0), p(9.0, 2.0), p(5.0, 14.0), p(-5.0, 14.0), p(-9.0, 2.0)], Some(255), 0, w);
            c.line(p(-9.0, 2.0), p(9.0, 2.0), s, 0, false, false);
        }
        PlotSymbol::Fresnel => {
            c.rect(p(-10.0, -13.0), p(20.0, 21.0), 2.0 * s, Some(255), 0, w);
            c.rect(p(-11.0, 8.0), p(22.0, 5.0), 1.0 * s, Some(255), 0, w);
        }
        PlotSymbol::Par => {
            c.rect(p(-10.0, -14.0), p(20.0, 28.0), 10.0 * s, Some(255), 0, w);
            c.line(p(-8.0, 8.0), p(8.0, 8.0), s, 0, false, false);
        }
        PlotSymbol::Flood => {
            c.polygon(&[p(-8.0, -12.0), p(8.0, -12.0), p(13.0, 12.0), p(-13.0, 12.0)], Some(255), 0, w);
        }
        PlotSymbol::Strobe => {
            c.rect(p(-14.0, -7.0), p(28.0, 14.0), 2.0 * s, Some(255), 0, w);
            c.line(p(-14.0, 4.0), p(14.0, 4.0), s, 0, false, false);
        }
        PlotSymbol::Batten => {
            c.rect(p(-20.0, -4.0), p(40.0, 8.0), 1.0 * s, Some(255), 0, w);
            for x in [-10.0, 0.0, 10.0] {
                c.line(p(x, -4.0), p(x, 4.0), 0.75 * s, 0, false, false);
            }
        }
        PlotSymbol::Effect => {
            c.polygon(&[p(0.0, -12.0), p(12.0, 10.0), p(-12.0, 10.0)], Some(255), 0, w);
        }
    }
    c.pop();
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

/// Largest coordinate a plot frames, metres.
const MAX_EXTENT: f32 = 100_000.0;

/// Metres between grid lines: 1, or 10, 100... so a big plot has at most about 100 lines a side.
fn grid_step(size: f32) -> f32 {
    let mut step = 1.0;
    while size / step > 100.0 && step < MAX_EXTENT {
        step *= 10.0;
    }
    step
}

/// The plot editor's changes by item.
struct Edits<'a>(HashMap<&'a PlotItem, &'a PlotEdit>);

impl<'a> Edits<'a> {
    fn of(l: &'a Lighting) -> Self {
        Self(l.plot.edits.iter().map(|e| (&e.item, e)).collect())
    }

    /// Moved by (metres, or page units for page items).
    fn offset(&self, item: &PlotItem) -> [f32; 2] {
        self.0.get(item).map_or([0.0, 0.0], |e| e.offset)
    }

    /// The text to draw instead of `usual`; None when the item is hidden.
    fn text(&self, item: &PlotItem, usual: &str) -> Option<String> {
        match self.0.get(item) {
            Some(e) if e.hidden => None,
            Some(PlotEdit { text: Some(t), .. }) => Some(t.clone()),
            _ => Some(usual.to_string()),
        }
    }

    fn hidden(&self, item: &PlotItem) -> bool {
        self.0.get(item).is_some_and(|e| e.hidden)
    }

    /// Text size (in the edit's units) instead of `usual`.
    fn size(&self, item: &PlotItem, usual: f32) -> f32 {
        self.0.get(item).and_then(|e| e.size).unwrap_or(usual)
    }

    /// The symbol a fixture is drawn with: its own choice, its type's, else the type's usual one.
    fn symbol(&self, f: &Fixture, ty: &(String, PlotSymbol)) -> PlotSymbol {
        let pick = |i: &PlotItem| self.0.get(i).and_then(|e| e.symbol);
        pick(&PlotItem::Fixture(f.id)).or_else(|| pick(&PlotItem::LegendRow(ty.0.clone()))).unwrap_or(ty.1)
    }

    /// Text colour instead of the usual `grey`.
    fn colour(&self, item: &PlotItem, grey: u8) -> Rgb {
        self.0.get(item).and_then(|e| e.colour).unwrap_or([grey; 3])
    }
}

/// Paint the plot of `l`; where each item landed.
fn draw(l: &Lighting, lay: &Layout, fr: &Frame, c: &mut dyn Canvas) -> Vec<Hit> {
    let (w, h) = (lay.size()[0] * fr.scale, lay.size()[1] * fr.scale);
    let [ox, oy] = fr.origin;
    let px = |x: f32| ox + (x - lay.x0) * fr.scale;
    let pz = |z: f32| oy + (z - lay.z0) * fr.scale;
    let s = fr.sym;
    let ed = Edits::of(l);
    let mut hits = Vec::new();
    // A text that isn't a label (no way out of the others' way), `size` in page units.
    let mut fixed = |c: &mut dyn Canvas, item: PlotItem, usual: &str, at: [f32; 2], size: f32, bold: bool, grey: u8, anchor: Anchor| {
        let Some(text) = ed.text(&item, usual) else { return };
        let (base, size, colour) = (size, ed.size(&item, size), ed.colour(&item, grey));
        let rect = Label { item: item.clone(), usual: String::new(), base, grey, at, size, bold, colour, anchor, text: text.clone() }.bounds([0.0, 0.0]);
        c.text(at, size, bold, colour, anchor, &text);
        hits.push(Hit { item, rect, text: usual.to_string(), size: base, colour: [grey; 3], nudged: [0.0, 0.0] });
    };
    let to = ed.offset(&PlotItem::Title);
    fixed(c, PlotItem::Title, &fr.title, [ox + 10.0 + to[0], oy - 20.0 + to[1]], 20.0, true, 0, Anchor::Start);
    if !fr.subtitle.is_empty() {
        let item = PlotItem::Subtitle;
        let so = ed.offset(&item);
        let text = ed.text(&item, &fr.subtitle).unwrap_or_default();
        let at = [ox + w - 4.0 - text_width(&text, ed.size(&item, 11.0)) + so[0], oy - 20.0 + so[1]];
        fixed(c, item, &fr.subtitle, at, 11.0, false, 85, Anchor::Start);
    }
    let step = grid_step(lay.size()[0].max(lay.size()[1]));
    for i in 0..=((lay.size()[0] / step) as i32) {
        let x = ox + i as f32 * step * fr.scale;
        c.line([x, oy], [x, oy + h], 1.0, 221, false, false);
    }
    for i in 0..=((lay.size()[1] / step) as i32) {
        let y = oy + i as f32 * step * fr.scale;
        c.line([ox, y], [ox + w, y], 1.0, 221, false, false);
    }
    // A label of the grid: sizes in points at the SVG scale (times `s` on the page).
    let label = |item: PlotItem, usual: String, at: [f32; 2], size: f32, bold: bool, grey: u8, anchor: Anchor| {
        let text = ed.text(&item, &usual)?;
        let (base, size, colour) = (size, ed.size(&item, size) * s, ed.colour(&item, grey));
        Some(Label { item, usual, base, grey, at, size, bold, colour, anchor, text })
    };
    // Labels are placed after everything else is drawn, so none lands on another (see `Labels`).
    let mut labels = Labels { area: [ox, oy, ox + w, oy + h], ..Default::default() };
    for kind in [ShapeKind::Rect, ShapeKind::Bar] {
        for sh in l.shapes.iter().filter(|x| x.kind == kind) {
            let (ax, ay, bx, by) = (px(sh.a[0]), pz(sh.a[1]), px(sh.b[0]), pz(sh.b[1]));
            // Where the user moved the label to.
            let moved = [sh.label[0] * fr.scale, sh.label[1] * fr.scale];
            let item = PlotItem::Shape(sh.id);
            let user = sh.label != [0.0, 0.0];
            match kind {
                ShapeKind::Rect => {
                    let (x, y) = (ax.min(bx), ay.min(by));
                    c.rect([x, y], [(bx - ax).abs(), (by - ay).abs()], 0.0, Some(242), 102, 1.5);
                    let at = [x + 6.0 + moved[0], y + 16.0 * s + moved[1]];
                    if let Some(lb) = label(item, sh.name.clone(), at, 12.0, false, 102, Anchor::Start) {
                        labels.add(lb, user, 2);
                    }
                }
                ShapeKind::Bar => {
                    c.line([ax, ay], [bx, by], 5.0 * s, 51, false, true);
                    let (lx, ly) = if ax <= bx { (ax, ay) } else { (bx, by) };
                    let usual = if sh.name.is_empty() { format!("{:.2} m", sh.height) } else { format!("{} · {:.2} m", sh.name, sh.height) };
                    if let Some(lb) = label(item, usual, [lx + moved[0], ly - 22.0 * s + moved[1]], 11.0, false, 51, Anchor::Start) {
                        labels.add(lb, user, 2);
                    }
                }
            }
        }
    }
    // Centre line and the audience side.
    if lay.x0 < 0.0 && lay.x1 > 0.0 {
        c.line([px(0.0), oy], [px(0.0), oy + h], 1.0, 153, true, false);
    }
    let ao = ed.offset(&PlotItem::Audience);
    fixed(c, PlotItem::Audience, "AUDIENCE", [ox + w / 2.0 + ao[0] * fr.scale, oy + h - 6.0 + ao[1] * fr.scale], 12.0, false, 153, Anchor::Middle);
    for f in &l.fixtures {
        let item = PlotItem::Fixture(f.id);
        if ed.hidden(&item) {
            continue;
        }
        let sym = ed.symbol(f, &type_of(l, f));
        let fo = ed.offset(&item);
        let (cx, cy) = (px(f.position[0] + fo[0]), pz(f.position[2] + fo[1]));
        paint_symbol(c, sym, [cx, cy], -f.rotation[1], s);
        // The box around the turned symbol (a little room for the outline).
        let [hw, hh] = half_size(sym);
        let (sin, cos) = f.rotation[1].to_radians().sin_cos();
        let (bw, bh) = ((cos.abs() * hw + sin.abs() * hh + 2.0) * s, (sin.abs() * hw + cos.abs() * hh + 2.0) * s);
        let body = [cx - bw, cy - bh, cx + bw, cy + bh];
        labels.obstacles.push(body);
        // The number fits inside the symbol (smaller for long numbers) unless the user sized it.
        let usual = f.id.to_string();
        let number = ed.text(&item, &usual).unwrap_or_default();
        let fit = |t: &str| ((17.0f32).min(1.7 * hw) / (text_width(t, 1.0) + 0.01)).min(11.0).min(1.8 * hh);
        let size = ed.size(&item, fit(&number)) * s;
        c.text([cx, cy + size * 0.36], size, true, ed.colour(&item, 0), Anchor::Middle, &number);
        hits.push(Hit { item, rect: body, text: usual.clone(), size: fit(&usual), colour: [0; 3], nudged: [0.0, 0.0] });
        let patch = if f.address > 0 { format!("{}.{}", f.universe, f.address) } else { "—".to_string() };
        for (item, usual, dy, size, bold, grey, kind) in [
            (PlotItem::Patch(f.id), patch, -20.0, 10.0, true, 51, 0),
            (PlotItem::Distance(f.id), centre_distance(f.position[0]), 27.0, 9.0, false, 102, 1),
        ] {
            let o = ed.offset(&item);
            let at = [cx + o[0] * fr.scale, cy + dy * s + o[1] * fr.scale];
            if let Some(lb) = label(item, usual, at, size, bold, grey, Anchor::Middle) {
                labels.add(lb, o != [0.0, 0.0], kind);
            }
        }
    }
    for n in &l.plot.notes {
        let size = n.size * s;
        let item = PlotItem::Note(n.id);
        let (x, y) = (px(n.at[0]), pz(n.at[1]));
        let mut rect = [x, y - size * 0.8, x, y - size * 0.8];
        for (i, line) in n.text.lines().enumerate() {
            let at = [x, y + i as f32 * size * 1.25];
            c.text(at, size, n.bold, n.colour, Anchor::Start, line);
            let lb = Label { item: item.clone(), usual: String::new(), base: n.size, grey: 0, at, size, bold: n.bold, colour: n.colour, anchor: Anchor::Start, text: line.to_string() };
            let b = lb.bounds([0.0, 0.0]);
            rect = [rect[0].min(b[0]), rect[1].min(b[1]), rect[2].max(b[2]), rect[3].max(b[3])];
        }
        // An empty note still has something to grab.
        rect[2] = rect[2].max(rect[0] + size);
        rect[3] = rect[3].max(rect[1] + size);
        labels.obstacles.push(rect);
        hits.push(Hit { item, rect, text: n.text.clone(), size: n.size, colour: n.colour, nudged: [0.0, 0.0] });
    }
    labels.draw(c, &mut hits);
    if !ed.hidden(&PlotItem::Legend) {
        let lo = ed.offset(&PlotItem::Legend);
        let rows = lay.types.len().div_ceil(fr.legend_cols.max(1)).max(1);
        for (i, (name, usual_sym, n)) in lay.types.iter().enumerate() {
            let item = PlotItem::LegendRow(name.clone());
            let usual = format!("{n} × {name}");
            let Some(text) = ed.text(&item, &usual) else { continue };
            let x = fr.legend_x + (i / rows) as f32 * fr.legend_col_w + lo[0];
            let y = oy + h + 24.0 + (i % rows) as f32 * 20.0 + lo[1];
            let sym = ed.0.get(&item).and_then(|e| e.symbol).unwrap_or(*usual_sym);
            paint_symbol(c, sym, [x + 16.0, y - 4.0], 0.0, 0.45);
            let size = ed.size(&item, 12.0);
            c.text([x + 30.0, y], size, false, ed.colour(&item, 0), Anchor::Start, &text);
            let rect = [x + 8.0, y - 13.0, x + 30.0 + text_width(&text, size) * 1.1 + 2.0, y + 5.0];
            hits.push(Hit { item, rect, text: usual, size: 12.0, colour: [0; 3], nudged: [0.0, 0.0] });
        }
    }
    hits
}

/// A text of the plot that may move to stay clear of the others.
struct Label {
    item: PlotItem,
    /// Its usual text, size and grey (`text`, `size` and `colour` may be the editor's).
    usual: String,
    base: f32,
    grey: u8,
    at: [f32; 2],
    size: f32,
    bold: bool,
    colour: Rgb,
    anchor: Anchor,
    text: String,
}

impl Label {
    /// Box (left, top, right, bottom) of the text at `at` + `d`, from the baseline and an
    /// approximate width (with some room: an SVG viewer may use a wider font than Helvetica).
    fn bounds(&self, d: [f32; 2]) -> [f32; 4] {
        let w = text_width(&self.text, self.size) * 1.1 + 2.0;
        let left = self.at[0] + d[0] - if self.anchor == Anchor::Middle { w / 2.0 } else { 0.0 };
        let base = self.at[1] + d[1];
        [left, base - self.size * 0.8, left + w, base + self.size * 0.25]
    }
}

/// The plot's labels, placed so that no two overlap: each in turn takes its own place when it's
/// free, else the nearest free one moving away from its fixture (patch labels up, distance labels
/// down, bar and rectangle names either way, then sideways). Labels the user placed stay put.
#[derive(Default)]
struct Labels {
    /// (label, the user placed it, kind: 0 above a fixture, 1 below, 2 a shape's name).
    list: Vec<(Label, bool, u8)>,
    /// Fixture symbols, which labels keep off too.
    obstacles: Vec<[f32; 4]>,
    /// The grid: a label moved out of the way stays on it.
    area: [f32; 4],
}

fn overlaps(a: &[f32; 4], b: &[f32; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

impl Labels {
    fn add(&mut self, label: Label, user: bool, kind: u8) {
        if !label.text.is_empty() {
            self.list.push((label, user, kind));
        }
    }

    /// Where each label goes: the offset from its own place.
    fn place(&self) -> Vec<[f32; 2]> {
        let mut out = vec![[0.0, 0.0]; self.list.len()];
        let mut taken: Vec<[f32; 4]> = Vec::new();
        // The user's placements first, then fixtures' labels, then the shapes' names.
        let mut order: Vec<usize> = (0..self.list.len()).collect();
        order.sort_by_key(|&i| (!self.list[i].1, self.list[i].2));
        for i in order {
            let (label, user, kind) = &self.list[i];
            let a = &self.area;
            let free = |d: [f32; 2]| {
                let b = label.bounds(d);
                !taken.iter().chain(&self.obstacles).any(|o| overlaps(o, &b))
            };
            let on_grid = |d: [f32; 2]| {
                let b = label.bounds(d);
                b[0] >= a[0] && b[1] >= a[1] && b[2] <= a[2] && b[3] <= a[3]
            };
            let step = label.size * 1.1;
            let width = text_width(&label.text, label.size);
            let dirs: &[[f32; 2]] = match kind {
                0 => &[[0.0, -1.0], [-1.0, 0.0], [1.0, 0.0]],
                1 => &[[0.0, 1.0], [-1.0, 0.0], [1.0, 0.0]],
                _ => &[[0.0, -1.0], [0.0, 1.0], [1.0, 0.0], [-1.0, 0.0]],
            };
            let mut d = [0.0, 0.0];
            if !*user && !free(d) {
                // Up to 12 steps out (sideways in steps of the label's width).
                let found = (1..=12).flat_map(|k| dirs.iter().map(move |v| (k, v))).map(|(k, v)| {
                    let unit = if v[0] != 0.0 { width * 0.5 + 2.0 } else { step };
                    [v[0] * unit * k as f32, v[1] * unit * k as f32]
                });
                if let Some(f) = found.into_iter().find(|d| on_grid(*d) && free(*d)) {
                    d = f;
                }
            }
            taken.push(label.bounds(d));
            out[i] = d;
        }
        out
    }

    fn draw(&self, c: &mut dyn Canvas, hits: &mut Vec<Hit>) {
        for ((l, _, _), d) in self.list.iter().zip(self.place()) {
            c.text([l.at[0] + d[0], l.at[1] + d[1]], l.size, l.bold, l.colour, l.anchor, &l.text);
            hits.push(Hit { item: l.item.clone(), rect: l.bounds(d), text: l.usual.clone(), size: l.base, colour: [l.grey; 3], nudged: d });
        }
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

fn rgb_hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
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
    fn polygon(&mut self, points: &[[f32; 2]], fill: Option<u8>, stroke: u8, width: f32) {
        let fill = fill.map_or("none".into(), grey_hex);
        let pts: Vec<String> = points.iter().map(|p| format!("{},{}", p[0], p[1])).collect();
        let _ = writeln!(self.0, r#"<polygon points="{}" fill="{fill}" stroke="{}" stroke-width="{width}" stroke-linejoin="round"/>"#, pts.join(" "), grey_hex(stroke));
    }
    fn circle(&mut self, c: [f32; 2], r: f32, fill: Option<u8>, stroke: u8, width: f32) {
        let fill = fill.map_or("none".into(), grey_hex);
        let _ = writeln!(self.0, r#"<circle cx="{}" cy="{}" r="{r}" fill="{fill}" stroke="{}" stroke-width="{width}"/>"#, c[0], c[1], grey_hex(stroke));
    }
    fn text(&mut self, at: [f32; 2], size: f32, bold: bool, colour: Rgb, anchor: Anchor, text: &str) {
        let weight = if bold { r#" font-weight="bold""# } else { "" };
        let anchor = if anchor == Anchor::Middle { r#" text-anchor="middle""# } else { "" };
        let _ = writeln!(self.0, r#"<text x="{}" y="{}" font-size="{size}"{weight} fill="{}"{anchor}>{}</text>"#, at[0], at[1], rgb_hex(colour), esc(text));
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
    fn polygon(&mut self, points: &[[f32; 2]], fill: Option<u8>, stroke: u8, width: f32) {
        let Some((first, rest)) = points.split_first() else { return };
        self.stroke_style(stroke, width);
        if let Some(f) = fill {
            self.fill_grey(f);
        }
        let _ = write!(self.ops, "{:.2} {:.2} m ", first[0], first[1]);
        for p in rest {
            let _ = write!(self.ops, "{:.2} {:.2} l ", p[0], p[1]);
        }
        self.ops.push_str("h ");
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
    fn text(&mut self, at: [f32; 2], size: f32, bold: bool, colour: Rgb, anchor: Anchor, text: &str) {
        let x = match anchor {
            Anchor::Start => at[0],
            Anchor::Middle => at[0] - text_width(text, size) / 2.0,
        };
        if colour[0] == colour[1] && colour[1] == colour[2] {
            self.fill_grey(colour[0]);
        } else {
            let _ = write!(self.ops, "{:.3} {:.3} {:.3} rg ", colour[0] as f32 / 255.0, colour[1] as f32 / 255.0, colour[2] as f32 / 255.0);
        }
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

/// Where the plot of `l` sits on the A3 page of the PDF.
fn pdf_frame(l: &Lighting, title: &str) -> (Layout, Frame) {
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
    (lay, fr)
}

/// The plot of a design as a one-page PDF (A3 landscape), titled `title`.
pub fn pdf(l: &Lighting, title: &str) -> Vec<u8> {
    let (lay, fr) = pdf_frame(l, title);
    let mut c = Pdf { ops: format!("1 0 0 -1 0 {:.2} cm 1 j\n", A3[1]) };
    draw(l, &lay, &fr, &mut c);
    write_pdf(&c.ops, A3, title)
}

/// Paint the PDF's page (A3 landscape, points, y down; the white sheet itself is left to `c`) and
/// say where each item landed: what the plot editor shows.
pub fn page(l: &Lighting, title: &str, c: &mut dyn Canvas) -> Page {
    let (lay, fr) = pdf_frame(l, title);
    let hits = draw(l, &lay, &fr, c);
    Page { size: A3, scale: fr.scale, centre: [(lay.x0 + lay.x1) / 2.0, (lay.z0 + lay.z1) / 2.0], hits }
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
        format!("<< /Title {} /Producer (OpnQ) >>", pdf_string(title)).into_bytes(),
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
        l.shapes.push(crate::light::model::PlotShape { id: 1, kind: ShapeKind::Bar, name: "LX1".into(), a: [-4.0, -2.0], b: [4.0, -2.0], height: 6.0, label: [0.0, 0.0] });
        l.shapes.push(crate::light::model::PlotShape { id: 2, kind: ShapeKind::Rect, name: "Stage".into(), a: [-6.0, -5.0], b: [6.0, 1.0], height: 0.0, label: [0.0, 0.0] });
        for (i, f) in l.fixtures.iter_mut().enumerate() {
            f.position = [i as f32 - 3.0, 5.0, -2.0];
        }
        l
    }

    #[test]
    fn far_away_positions_dont_hang_the_plot() {
        let mut l = rig();
        l.fixtures[0].position = [1e30, 0.0, -1e30];
        l.fixtures[1].position = [f32::INFINITY, 0.0, f32::NAN];
        l.shapes[0].b = [1e12, 3e9];
        let t = std::time::Instant::now();
        let s = svg(&l, "x");
        assert!(t.elapsed() < std::time::Duration::from_secs(2) && s.len() < 1 << 20, "{:?}, {} bytes", t.elapsed(), s.len());
    }

    #[test]
    fn plot_has_every_fixture() {
        let l = rig();
        let s = svg(&l, "Tom & Jerry");
        assert!(s.contains("Tom &amp; Jerry"));
        let kinds: std::collections::HashSet<&str> = l.fixtures.iter().map(|f| f.fixture_type.as_str()).collect();
        assert_eq!(s.matches("<g transform=\"translate(").count(), 6 + kinds.len(), "one symbol per fixture and legend row");
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
        let kinds = Layout::of(&l).types.len();
        assert_eq!(ops.matches("\nQ\n").count() + ops.starts_with("Q\n") as usize, 6 + kinds, "one symbol per fixture and legend row");
        // 18 m × 13 m with margins fits A3 at 1:50.
        assert!(ops.contains("(Scale 1:50 (A3)") || ops.contains(r"(Scale 1:50 \(A3\)"), "{}", &ops[..400]);
        // A huge rig falls back to a small scale.
        let mut big = rig();
        big.fixtures[0].position = [400.0, 0.0, 300.0];
        let p = pdf(&big, "Big");
        assert!(!p.is_empty());
    }

    /// Texts drawn, as boxes.
    struct Texts(Vec<(String, [f32; 4])>);

    impl Canvas for Texts {
        fn line(&mut self, _: [f32; 2], _: [f32; 2], _: f32, _: u8, _: bool, _: bool) {}
        fn rect(&mut self, _: [f32; 2], _: [f32; 2], _: f32, _: Option<u8>, _: u8, _: f32) {}
        fn polygon(&mut self, _: &[[f32; 2]], _: Option<u8>, _: u8, _: f32) {}
        fn circle(&mut self, _: [f32; 2], _: f32, _: Option<u8>, _: u8, _: f32) {}
        fn text(&mut self, at: [f32; 2], size: f32, bold: bool, colour: Rgb, anchor: Anchor, text: &str) {
            let l = Label { item: PlotItem::Title, usual: String::new(), base: size, grey: 0, at, size, bold, colour, anchor, text: text.to_string() };
            self.0.push((text.to_string(), l.bounds([0.0, 0.0])));
        }
        fn push(&mut self, _: [f32; 2], _: f32) {}
        fn pop(&mut self) {}
    }

    /// Fixtures packed tight along a bar: their labels and the bar's name still don't overlap,
    /// and a label the user moved is drawn where they put it.
    #[test]
    fn labels_never_overlap() {
        let mut l = rig();
        for (i, f) in l.fixtures.iter_mut().enumerate() {
            f.position = [i as f32 * 0.3 - 4.0, 6.0, -2.0];
        }
        l.shapes[1].label = [1.0, 0.5];
        let lay = Layout::of(&l);
        let fr = Frame { origin: [0.0, 50.0], scale: SCALE, sym: 1.0, title: String::new(), subtitle: String::new(), legend_x: 0.0, legend_cols: 1, legend_col_w: 100.0 };
        let mut c = Texts(Vec::new());
        draw(&l, &lay, &fr, &mut c);
        let ours: Vec<&(String, [f32; 4])> = c.0.iter().filter(|(t, _)| t.contains('.') || t == "Stage" || t.starts_with("LX1")).collect();
        assert!(ours.len() >= 13, "{ours:?}");
        for (i, a) in ours.iter().enumerate() {
            for b in &ours[i + 1..] {
                assert!(!overlaps(&a.1, &b.1), "{a:?} and {b:?}");
            }
        }
        // The stage's corner (-6, -5) is at the grid origin + 1 m margin; moved 1 m right, 0.5 m down.
        let stage = c.0.iter().find(|(t, _)| t == "Stage").unwrap();
        assert!((stage.1[0] - (SCALE + 6.0 + SCALE)).abs() < 0.01, "{stage:?}");
    }

    /// The plot editor's changes: re-worded, hidden and moved items, an added text; `page` says
    /// where each landed.
    #[test]
    fn editor_changes() {
        use crate::light::model::PlotNote;
        let mut l = rig();
        let f = l.fixtures[0].id;
        let ty = Layout::of(&l).types[0].0.clone();
        let p = &mut l.plot;
        p.edit_mut(&PlotItem::Title).text = Some("Hamlet — Act 1".into());
        p.edit_mut(&PlotItem::Distance(f)).hidden = true;
        p.edit_mut(&PlotItem::Fixture(f)).offset = [0.0, 1.0];
        p.edit_mut(&PlotItem::Patch(f)).offset = [0.5, 0.0];
        p.edit_mut(&PlotItem::LegendRow(ty.clone())).text = Some("Wash".into());
        p.edit_mut(&PlotItem::Audience).hidden = true;
        p.notes.push(PlotNote { id: 1, at: [0.0, 0.0], text: "FOH\nboom".into(), ..Default::default() });
        let s = svg(&l, "x");
        assert!(s.contains(">Hamlet — Act 1</text>") && !s.contains(">x</text>"));
        assert!(!s.contains("AUDIENCE") && s.contains(">Wash</text>") && s.contains(">FOH</text>") && s.contains(">boom</text>"));
        // Five distance labels left (one fixture's hidden).
        assert_eq!(s.matches(" m</text>").count() - s.matches("6.00 m</text>").count(), 5 - s.contains(">CL</text>") as usize);
        let mut c = Texts(Vec::new());
        let page = page(&l, "x", &mut c);
        let hit = |item: PlotItem| page.hits.iter().find(|h| h.item == item).cloned();
        assert!(hit(PlotItem::Distance(f)).is_none() && hit(PlotItem::Audience).is_none());
        let body = hit(PlotItem::Fixture(f)).unwrap();
        let patch = hit(PlotItem::Patch(f)).unwrap();
        // The symbol moved 1 m down; the patch label went with it and 0.5 m to the right, and stays put.
        let centre = [(body.rect[0] + body.rect[2]) / 2.0, (body.rect[1] + body.rect[3]) / 2.0];
        let other = hit(PlotItem::Fixture(l.fixtures[1].id)).unwrap();
        assert!((centre[1] - (other.rect[1] + other.rect[3]) / 2.0 - page.scale).abs() < 0.01);
        assert!(((patch.rect[0] + patch.rect[2]) / 2.0 - centre[0] - 0.5 * page.scale).abs() < 0.01, "{patch:?} {centre:?}");
        assert_eq!(patch.nudged, [0.0, 0.0]);
        assert_eq!(hit(PlotItem::Title).unwrap().text, "x", "usual text");
        assert_eq!(hit(PlotItem::LegendRow(ty)).unwrap().text.split(" × ").count(), 2);
        assert!(hit(PlotItem::Note(1)).is_some());
        // Tidy drops changes that change nothing.
        l.plot.edit_mut(&PlotItem::Subtitle);
        l.plot.edit_mut(&PlotItem::Audience).hidden = false;
        l.plot.tidy();
        assert_eq!(l.plot.edits.len(), 5);
    }

    /// Symbols: guessed from the type, picked per type (legend row) or per fixture (which wins);
    /// every symbol draws in both formats.
    #[test]
    fn symbols() {
        use crate::light::model::PlotSymbol as S;
        assert_eq!(S::guess("ETC Source Four 26°", false), S::Profile);
        assert_eq!(S::guess("Generic PAR 64", false), S::Par);
        assert_eq!(S::guess("Generic Fog / haze", false), S::Effect);
        assert_eq!(S::guess("OpnQ Demo Pixel batten 8", false), S::Batten);
        assert_eq!(S::guess("Generic Dimmer", false), S::Rectangle);
        assert_eq!(S::guess("Anything", true), S::Circle);
        let mut l = rig();
        let (a, b) = (l.fixtures[0].clone(), l.fixtures[1].clone());
        let name = type_name(&l, &a);
        let usual = symbol_of(&l, &a);
        l.plot.edit_mut(&PlotItem::LegendRow(name.clone())).symbol = Some(S::Strobe);
        l.plot.edit_mut(&PlotItem::Fixture(a.id)).symbol = Some(S::Batten);
        assert_eq!(symbol_of(&l, &a), S::Batten);
        assert_eq!(type_symbol(&l, &name), Some((S::Strobe, usual)));
        if type_name(&l, &b) == name {
            assert_eq!(symbol_of(&l, &b), S::Strobe);
        }
        // A batten is wide: its box is too, and a turned one is tall.
        let mut c = Texts(Vec::new());
        let hit = |l: &Lighting, c: &mut Texts| page(l, "x", c).hits.into_iter().find(|h| h.item == PlotItem::Fixture(a.id)).unwrap().rect;
        let r = hit(&l, &mut c);
        assert!(r[2] - r[0] > 3.0 * (r[3] - r[1]), "{r:?}");
        l.fixtures[0].rotation[1] = 90.0;
        let r = hit(&l, &mut c);
        assert!(r[3] - r[1] > 3.0 * (r[2] - r[0]), "{r:?}");
        for (i, sym) in S::ALL.into_iter().enumerate() {
            l.plot.edit_mut(&PlotItem::Fixture(l.fixtures[i % 6].id)).symbol = Some(sym);
            assert!(svg(&l, "x").ends_with("</svg>\n") && !pdf(&l, "x").is_empty());
        }
        assert!(svg(&l, "x").contains("<polygon points="));
    }

    #[test]
    fn pdf_strings() {
        assert_eq!(pdf_string(r"a(b)\c"), r"(a\(b\)\\c)");
        assert_eq!(pdf_string("Ñ€✓"), r"(\321\200?)");
    }
}
