//! Plot editor: the PDF plot's page in a window before it is exported. Items (title, labels,
//! fixture symbols, the legend, added texts) are dragged to move them; the selected ones get their
//! text, size and colour changed, hidden or reset in the side column, and lined up or spaced
//! evenly with the alignment tools (on their boxes). Changes live in the design
//! ([`crate::light::model::PlotLayout`], `Lighting.plot`), so the SVG/PDF export and a later
//! visit keep them; bar and rectangle names move through `PlotShape::label`, like in the Plan view.
//!
//! The page is drawn once per design change into a list of drawing calls (`Recorder`) and replayed
//! each frame onto the screen (`Screen`): label placement isn't redone while only the view moves.

use super::{App, icons, theme};
use crate::i18n::t;
use crate::light::Lighting;
use crate::light::model::{PlotItem, PlotNote, PlotSymbol};
use crate::light::plot::{self, Anchor, Canvas, Page, Rgb};
use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, vec2};

/// Plot editor state (in [`super::lighting::LightUi`]).
#[derive(Default)]
pub(crate) struct PlotUi {
    /// The window is open.
    pub(crate) open: bool,
    /// Selected items.
    pub(crate) sel: Vec<PlotItem>,
    /// The drag in progress.
    drag: Option<Drag>,
    /// View zoom (1 = the page fits) and pan, screen points.
    zoom: f32,
    pan: egui::Vec2,
    /// Put the keyboard in the text field on the next frame.
    focus_text: bool,
    /// The drawn page and the design and title it was drawn from.
    cache: Option<(Lighting, String, Recorder, Page)>,
}

/// One drawing call of the plot.
#[derive(Clone)]
enum Op {
    Line([f32; 2], [f32; 2], f32, u8, bool),
    Rect([f32; 2], [f32; 2], f32, Option<u8>, u8, f32),
    Polygon(Vec<[f32; 2]>, Option<u8>, u8, f32),
    Circle([f32; 2], f32, Option<u8>, u8, f32),
    Text([f32; 2], f32, bool, Rgb, Anchor, String),
    Push([f32; 2], f32),
    Pop,
}

/// The plot's drawing calls, to replay.
#[derive(Clone, Default)]
struct Recorder(Vec<Op>);

impl Canvas for Recorder {
    fn line(&mut self, a: [f32; 2], b: [f32; 2], width: f32, grey: u8, dashed: bool, _round: bool) {
        self.0.push(Op::Line(a, b, width, grey, dashed));
    }
    fn rect(&mut self, at: [f32; 2], size: [f32; 2], radius: f32, fill: Option<u8>, stroke: u8, width: f32) {
        self.0.push(Op::Rect(at, size, radius, fill, stroke, width));
    }
    fn polygon(&mut self, points: &[[f32; 2]], fill: Option<u8>, stroke: u8, width: f32) {
        self.0.push(Op::Polygon(points.to_vec(), fill, stroke, width));
    }
    fn circle(&mut self, c: [f32; 2], r: f32, fill: Option<u8>, stroke: u8, width: f32) {
        self.0.push(Op::Circle(c, r, fill, stroke, width));
    }
    fn text(&mut self, at: [f32; 2], size: f32, bold: bool, colour: Rgb, anchor: Anchor, text: &str) {
        self.0.push(Op::Text(at, size, bold, colour, anchor, text.to_string()));
    }
    fn push(&mut self, at: [f32; 2], deg: f32) {
        self.0.push(Op::Push(at, deg));
    }
    fn pop(&mut self) {
        self.0.push(Op::Pop);
    }
}

/// Page points onto the screen: `origin + zoom * (m · p)`, with `m` the pushed turns and moves.
struct Screen<'a> {
    painter: &'a egui::Painter,
    origin: Pos2,
    zoom: f32,
    /// Affine transforms [a, b, c, d, e, f]: x' = a·x + c·y + e, y' = b·x + d·y + f.
    stack: Vec<[f32; 6]>,
}

impl Screen<'_> {
    fn map(&self, p: [f32; 2]) -> Pos2 {
        let m = self.stack.last().copied().unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        self.origin + vec2(m[0] * p[0] + m[2] * p[1] + m[4], m[1] * p[0] + m[3] * p[1] + m[5]) * self.zoom
    }

    fn stroke(&self, width: f32, grey: u8) -> Stroke {
        Stroke::new((width * self.zoom).max(0.5), Color32::from_gray(grey))
    }

    fn replay(&mut self, ops: &[Op]) {
        for op in ops {
            match op {
                Op::Line(a, b, w, g, dashed) => self.line(*a, *b, *w, *g, *dashed, false),
                Op::Rect(at, size, r, fill, stroke, w) => self.rect(*at, *size, *r, *fill, *stroke, *w),
                Op::Polygon(pts, fill, stroke, w) => self.polygon(pts, *fill, *stroke, *w),
                Op::Circle(c, r, fill, stroke, w) => self.circle(*c, *r, *fill, *stroke, *w),
                Op::Text(at, size, bold, g, anchor, text) => self.text(*at, *size, *bold, *g, *anchor, text),
                Op::Push(at, deg) => self.push(*at, *deg),
                Op::Pop => self.pop(),
            }
        }
    }
}

impl Canvas for Screen<'_> {
    fn line(&mut self, a: [f32; 2], b: [f32; 2], width: f32, grey: u8, dashed: bool, _round: bool) {
        let pts = [self.map(a), self.map(b)];
        let stroke = self.stroke(width, grey);
        if dashed {
            self.painter.extend(egui::Shape::dashed_line(&pts, stroke, 8.0 * self.zoom, 4.0 * self.zoom));
        } else {
            self.painter.line_segment(pts, stroke);
        }
    }
    fn rect(&mut self, at: [f32; 2], size: [f32; 2], radius: f32, fill: Option<u8>, stroke: u8, width: f32) {
        let [x, y] = at;
        let [w, h] = size;
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        // Corners as quarter circles of a few segments (the shape may be turned).
        let mut pts = Vec::with_capacity(20);
        for (cx, cy, a0) in [(x + w - r, y + r, -90.0f32), (x + w - r, y + h - r, 0.0), (x + r, y + h - r, 90.0), (x + r, y + r, 180.0)] {
            let steps = if r > 0.0 { 4 } else { 0 };
            for i in 0..=steps {
                let a = (a0 + 90.0 * i as f32 / steps.max(1) as f32).to_radians();
                pts.push(self.map([cx + r * a.cos(), cy + r * a.sin()]));
            }
        }
        let fill = fill.map_or(Color32::TRANSPARENT, Color32::from_gray);
        self.painter.add(egui::Shape::convex_polygon(pts, fill, self.stroke(width, stroke)));
    }
    fn polygon(&mut self, points: &[[f32; 2]], fill: Option<u8>, stroke: u8, width: f32) {
        let pts = points.iter().map(|p| self.map(*p)).collect();
        let fill = fill.map_or(Color32::TRANSPARENT, Color32::from_gray);
        self.painter.add(egui::Shape::convex_polygon(pts, fill, self.stroke(width, stroke)));
    }
    fn circle(&mut self, c: [f32; 2], r: f32, fill: Option<u8>, stroke: u8, width: f32) {
        let fill = fill.map_or(Color32::TRANSPARENT, Color32::from_gray);
        self.painter.circle(self.map(c), r * self.zoom, fill, self.stroke(width, stroke));
    }
    fn text(&mut self, at: [f32; 2], size: f32, bold: bool, colour: Rgb, anchor: Anchor, text: &str) {
        let px = size * self.zoom;
        if px < 3.0 {
            return;
        }
        // `at` is the baseline: the galley's bottom sits a descent below it.
        let pos = self.map([at[0], at[1] + size * 0.22]);
        let align = if anchor == Anchor::Middle { egui::Align2::CENTER_BOTTOM } else { egui::Align2::LEFT_BOTTOM };
        let font = if bold { theme::semibold(px) } else { egui::FontId::proportional(px) };
        self.painter.text(pos, align, text, font, Color32::from_rgb(colour[0], colour[1], colour[2]));
    }
    fn push(&mut self, at: [f32; 2], deg: f32) {
        let m = self.stack.last().copied().unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        let (s, c) = deg.to_radians().sin_cos();
        self.stack.push([
            m[0] * c + m[2] * s,
            m[1] * c + m[3] * s,
            -m[0] * s + m[2] * c,
            -m[1] * s + m[3] * c,
            m[0] * at[0] + m[2] * at[1] + m[4],
            m[1] * at[0] + m[3] * at[1] + m[5],
        ]);
    }
    fn pop(&mut self) {
        self.stack.pop();
    }
}

/// What the side column calls an item.
fn describe(item: &PlotItem, l: &Lighting) -> String {
    match item {
        PlotItem::Title => t("Title").into(),
        PlotItem::Subtitle => t("Scale line").into(),
        PlotItem::Audience => t("Audience").into(),
        PlotItem::Legend => t("Legend").into(),
        PlotItem::LegendRow(name) => tr!("Legend: {name}", name = name.clone()),
        PlotItem::Fixture(id) => tr!("Fixture {n}", n = id.to_string()),
        PlotItem::Patch(id) => tr!("Patch of fixture {n}", n = id.to_string()),
        PlotItem::Distance(id) => tr!("Distance of fixture {n}", n = id.to_string()),
        PlotItem::Shape(id) => match l.shapes.iter().find(|s| s.id == *id) {
            Some(s) if !s.name.is_empty() => tr!("Name of {name}", name = s.name.clone()),
            _ => tr!("Shape {n}", n = id.to_string()),
        },
        PlotItem::Note(_) => t("Text").into(),
    }
}

/// A symbol's name in the picker.
fn symbol_name(s: PlotSymbol) -> &'static str {
    match s {
        PlotSymbol::Circle => t("Moving head"),
        PlotSymbol::Rectangle => t("Lantern"),
        PlotSymbol::Profile => t("Profile"),
        PlotSymbol::Fresnel => t("Fresnel / PC"),
        PlotSymbol::Par => t("PAR"),
        PlotSymbol::Flood => t("Flood / cyc"),
        PlotSymbol::Strobe => t("Strobe / blinder"),
        PlotSymbol::Batten => t("Batten / bar"),
        PlotSymbol::Effect => t("Effect (smoke, haze)"),
    }
}

/// Symbol tiles for the selected fixtures and legend rows (a legend row sets its whole type; a
/// fixture's own pick wins over its type's). Picking the usual symbol drops the pick.
fn symbol_picker(ui: &mut Ui, st: &mut PlotUi, l: &mut Lighting) {
    // (item, its symbol now, the symbol it gets without its own pick)
    let targets: Vec<(PlotItem, PlotSymbol, PlotSymbol)> = st
        .sel
        .iter()
        .filter_map(|i| match i {
            PlotItem::Fixture(id) => {
                let f = l.fixture(*id)?;
                let ty = plot::type_symbol(l, &plot::type_name(l, f))?.0;
                Some((i.clone(), plot::symbol_of(l, f), ty))
            }
            PlotItem::LegendRow(name) => plot::type_symbol(l, name).map(|(now, usual)| (i.clone(), now, usual)),
            _ => None,
        })
        .collect();
    if targets.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.label(theme::caption(t("Symbol")));
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(3.0, 3.0);
        for sym in PlotSymbol::ALL {
            let on = targets.iter().all(|t| t.1 == sym);
            let (rect, resp) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::click());
            let bg = if on { theme::ACCENT.gamma_multiply(0.45) } else if resp.hovered() { theme::BG_HOVER } else { theme::BG_RAISED };
            ui.painter().rect_filled(rect, 4.0, bg);
            let painter = ui.painter_at(rect);
            plot::paint_symbol(&mut Screen { painter: &painter, origin: rect.center(), zoom: 1.0, stack: Vec::new() }, sym, [0.0, 0.0], 0.0, 0.6);
            if resp.on_hover_text(symbol_name(sym)).clicked() {
                for (item, _, usual) in &targets {
                    l.plot.edit_mut(item).symbol = (sym != *usual).then_some(sym);
                }
            }
        }
    });
    let own = targets.iter().any(|(i, ..)| l.plot.edit(i).is_some_and(|e| e.symbol.is_some()));
    if own && ui.button(theme::with_icon(icons::REPLAY, t("Usual symbol"))).clicked() {
        for (item, ..) in &targets {
            l.plot.edit_mut(item).symbol = None;
        }
    }
    // One fixture: its symbol for the whole type, or select the whole type.
    if let [(PlotItem::Fixture(id), now, _)] = targets.as_slice()
        && let Some(f) = l.fixture(*id)
    {
        let (id, now, name) = (*id, *now, plot::type_name(l, f));
        ui.horizontal_wrapped(|ui| {
            if ui.button(t("Use for the whole type")).on_hover_text(tr!("Every {name} gets this symbol", name = name.clone())).clicked() {
                let usual = plot::type_symbol(l, &name).map_or(now, |t| t.1);
                l.plot.edit_mut(&PlotItem::LegendRow(name.clone())).symbol = (now != usual).then_some(now);
                let same: Vec<u32> = l.fixtures.iter().filter(|g| plot::type_name(l, g) == name).map(|g| g.id).collect();
                for g in same {
                    if let Some(e) = l.plot.edits.iter_mut().find(|e| e.item == PlotItem::Fixture(g)) {
                        e.symbol = None;
                    }
                }
            }
            if ui.button(t("Select the whole type")).clicked() {
                st.sel = l.fixtures.iter().filter(|g| plot::type_name(l, g) == name).map(|g| PlotItem::Fixture(g.id)).collect();
                if let Some(i) = st.sel.iter().position(|i| *i == PlotItem::Fixture(id)) {
                    st.sel.swap(0, i);
                }
            }
        });
    }
}

impl App {
    /// The plot editor window (while `light.plot.open`); edits `l` in place.
    pub(super) fn plot_editor(&mut self, ctx: &egui::Context, l: &mut Lighting, title: &str) {
        let mut open = true;
        let mut export = false;
        let screen = ctx.content_rect();
        egui::Window::new(theme::with_icon(icons::EDIT, t("Plot")))
            .id(egui::Id::new("plot-editor"))
            .open(&mut open)
            .default_size([(screen.width() - 80.0).min(1200.0), (screen.height() - 80.0).min(800.0)])
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(screen.center())
            .min_size([480.0, 320.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| export = plot_body(ui, &mut self.light.plot, l, title));
        self.light.plot.open = open;
        if export {
            self.deferred.push(super::Deferred::ExportPatch(true));
        }
        l.plot.tidy();
    }
}

/// Toolbar, page and side column; true when Export was pressed.
fn plot_body(ui: &mut Ui, st: &mut PlotUi, l: &mut Lighting, title: &str) -> bool {
    if st.cache.as_ref().is_none_or(|(cl, ct, ..)| cl != l || ct != title) {
        let mut rec = Recorder::default();
        let page = plot::page(l, title, &mut rec);
        st.cache = Some((l.clone(), title.to_string(), rec, page));
    }
    let Some((_, _, rec, page)) = &st.cache else { return false };
    let page = page.clone();
    let ops = rec.0.clone();
    let mut export = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if ui.button(theme::with_icon(icons::TEXT_FIELDS, t("Add text"))).on_hover_text(t("A free text in the middle of the plot")).clicked() {
            let id = l.plot.notes.iter().map(|n| n.id).max().unwrap_or(0) + 1;
            if l.plot.notes.len() < crate::limits::MAX_PLOT_NOTES {
                l.plot.notes.push(PlotNote { id, at: page.centre, text: t("Text").into(), ..Default::default() });
                st.sel = vec![PlotItem::Note(id)];
                st.focus_text = true;
            }
        }
        let changed = !l.plot.is_empty() || l.shapes.iter().any(|s| s.label != [0.0, 0.0]);
        if ui.add_enabled(changed, egui::Button::new(theme::with_icon(icons::REPLAY, t("Reset all")))).on_hover_text(t("Every item back in its place, with its own text; added texts are removed")).clicked() {
            l.plot = Default::default();
            for s in &mut l.shapes {
                s.label = [0.0, 0.0];
            }
            st.sel.clear();
        }
        ui.separator();
        let n = movers(&st.sel).len();
        for (align, glyph, tip) in [
            (Align::Left, icons::ALIGN_HORIZONTAL_LEFT, t("Align left edges")),
            (Align::CentreX, icons::ALIGN_HORIZONTAL_CENTER, t("Align centres across")),
            (Align::Right, icons::ALIGN_HORIZONTAL_RIGHT, t("Align right edges")),
            (Align::Top, icons::ALIGN_VERTICAL_TOP, t("Align top edges")),
            (Align::CentreY, icons::ALIGN_VERTICAL_CENTER, t("Align centres down")),
            (Align::Bottom, icons::ALIGN_VERTICAL_BOTTOM, t("Align bottom edges")),
            (Align::SpreadX, icons::HORIZONTAL_DISTRIBUTE, t("Space evenly across (three or more)")),
            (Align::SpreadY, icons::VERTICAL_DISTRIBUTE, t("Space evenly down (three or more)")),
        ] {
            let enough = if matches!(align, Align::SpreadX | Align::SpreadY) { n >= 3 } else { n >= 2 };
            if ui.add_enabled_ui(enough, |ui| theme::icon_button(ui, glyph, tip)).inner.clicked() {
                arrange(l, &page, &st.sel, align);
            }
        }
        ui.separator();
        if theme::icon_button(ui, icons::ZOOM_OUT, t("Zoom out")).clicked() {
            st.zoom = (st.zoom / 1.25).max(1.0);
        }
        if theme::icon_button(ui, icons::FIT_SCREEN, t("Whole page")).clicked() {
            st.zoom = 1.0;
            st.pan = egui::Vec2::ZERO;
        }
        if theme::icon_button(ui, icons::ZOOM_IN, t("Zoom in")).clicked() {
            st.zoom = (st.zoom * 1.25).min(16.0);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            export = ui.button(theme::with_icon(icons::DOWNLOAD, t("Export…"))).on_hover_text(t("Save the plot as a PDF (A3, to scale) or SVG drawing")).clicked();
        });
    });
    ui.add_space(4.0);
    let side_w = 260.0;
    let avail = ui.available_size();
    ui.horizontal_top(|ui| {
        let (rect, resp) = ui.allocate_exact_size(vec2((avail.x - side_w - 8.0).max(200.0), avail.y.max(200.0)), Sense::click_and_drag());
        page_view(ui, st, l, &page, &ops, rect, &resp);
        ui.vertical(|ui| {
            ui.set_width(side_w);
            egui::ScrollArea::vertical().id_salt("plot-side").show(ui, |ui| side_column(ui, st, l, &page));
        });
    });
    export
}

/// What a drag on the page does.
#[derive(Clone)]
enum Drag {
    /// Moves these items.
    Items(Vec<PlotItem>),
    /// Selects the items a band from this page point touches.
    Band([f32; 2]),
    /// Pans the view.
    Pan,
}

/// The items that actually move for a selection: legend rows move the legend, and a fixture's
/// labels already go with their fixture.
fn movers(sel: &[PlotItem]) -> Vec<PlotItem> {
    let mut out: Vec<PlotItem> = Vec::new();
    for i in sel {
        let i = if let PlotItem::LegendRow(_) = i { PlotItem::Legend } else { i.clone() };
        let with_fixture = match i {
            PlotItem::Patch(f) | PlotItem::Distance(f) => sel.contains(&PlotItem::Fixture(f)),
            _ => false,
        };
        if !with_fixture && !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

/// The page box of an item (the legend: all its rows).
fn item_rect(page: &Page, item: &PlotItem) -> Option<[f32; 4]> {
    let mine = |h: &&plot::Hit| &h.item == item || (*item == PlotItem::Legend && matches!(h.item, PlotItem::LegendRow(_)));
    page.hits.iter().filter(mine).map(|h| h.rect).reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])
}

/// A label moved out of the others' way keeps the place it is drawn at once the user moves it
/// (a moved label stays put): its nudge becomes its offset.
fn pin(l: &mut Lighting, page: &Page, item: &PlotItem) {
    let Some(h) = page.hits.iter().find(|h| &h.item == item && h.nudged != [0.0, 0.0]) else { return };
    let d = [h.nudged[0] / page.scale, h.nudged[1] / page.scale];
    match item {
        PlotItem::Shape(id) => {
            if let Some(s) = l.shapes.iter_mut().find(|s| s.id == *id) {
                s.label = d;
            }
        }
        i => l.plot.edit_mut(i).offset = d,
    }
}

/// Alignment tools.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Align {
    Left,
    CentreX,
    Right,
    Top,
    CentreY,
    Bottom,
    /// Centres evenly spaced between the outermost two.
    SpreadX,
    SpreadY,
}

/// Line up the selected items' boxes (the selection's outer edge or middle), or space their
/// centres evenly.
fn arrange(l: &mut Lighting, page: &Page, sel: &[PlotItem], align: Align) {
    let items: Vec<(PlotItem, [f32; 4])> = movers(sel).into_iter().filter_map(|i| Some((i.clone(), item_rect(page, &i)?))).collect();
    if items.len() < 2 {
        return;
    }
    let all = items.iter().map(|x| x.1).reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]).expect("two items");
    let mid = |r: &[f32; 4], k: usize| (r[k] + r[k + 2]) / 2.0;
    let mut moves: Vec<(PlotItem, [f32; 2])> = items
        .iter()
        .map(|(i, r)| {
            let d = match align {
                Align::Left => [all[0] - r[0], 0.0],
                Align::Right => [all[2] - r[2], 0.0],
                Align::CentreX => [mid(&all, 0) - mid(r, 0), 0.0],
                Align::Top => [0.0, all[1] - r[1]],
                Align::Bottom => [0.0, all[3] - r[3]],
                Align::CentreY => [0.0, mid(&all, 1) - mid(r, 1)],
                Align::SpreadX | Align::SpreadY => [0.0, 0.0],
            };
            (i.clone(), d)
        })
        .collect();
    if let Align::SpreadX | Align::SpreadY = align {
        let k = if align == Align::SpreadX { 0 } else { 1 };
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by(|&a, &b| mid(&items[a].1, k).total_cmp(&mid(&items[b].1, k)));
        let (first, last) = (mid(&items[order[0]].1, k), mid(&items[order[order.len() - 1]].1, k));
        let step = (last - first) / (order.len() - 1) as f32;
        for (n, &i) in order.iter().enumerate() {
            moves[i].1[k] = first + step * n as f32 - mid(&items[i].1, k);
        }
    }
    for (i, d) in moves {
        if d != [0.0, 0.0] {
            pin(l, page, &i);
            move_item(l, &i, d, page.scale);
        }
    }
}

/// The page, with hover and selection boxes. Dragging an item moves it (and the rest of the
/// selection), dragging the background selects with a band; the middle or right button pans,
/// scrolling zooms.
fn page_view(ui: &mut Ui, st: &mut PlotUi, l: &mut Lighting, page: &Page, ops: &[Op], rect: Rect, resp: &egui::Response) {
    if st.zoom <= 0.0 {
        st.zoom = 1.0;
    }
    let fit = (rect.width() / page.size[0]).min(rect.height() / page.size[1]) * 0.96;
    let zoom = fit * st.zoom;
    let origin = rect.center() - vec2(page.size[0], page.size[1]) * zoom / 2.0 + st.pan;
    // Scrolling zooms around the pointer.
    if let Some(p) = resp.hover_pos() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            let z = (st.zoom * (scroll / 200.0).exp()).clamp(1.0, 16.0);
            let k = z / st.zoom;
            // Keep the page point under the pointer still.
            let centre = rect.center() + st.pan;
            st.pan += (p - centre) * (1.0 - k);
            st.zoom = z;
        }
    }
    let to_page = |p: Pos2| [(p.x - origin.x) / zoom, (p.y - origin.y) / zoom];
    let to_screen = |r: [f32; 4]| Rect::from_min_max(origin + vec2(r[0], r[1]) * zoom, origin + vec2(r[2], r[3]) * zoom);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::BG_INPUT);
    let sheet = Rect::from_min_size(origin, vec2(page.size[0], page.size[1]) * zoom);
    painter.rect_filled(sheet.translate(vec2(3.0, 3.0)), 0.0, Color32::from_black_alpha(90));
    painter.rect_filled(sheet, 0.0, Color32::WHITE);
    Screen { painter: &painter, origin, zoom, stack: Vec::new() }.replay(ops);
    // Topmost item under a point (labels are drawn last).
    let hit_at = |p: Pos2| {
        let q = to_page(p);
        page.hits.iter().rev().find(|h| q[0] >= h.rect[0] && q[0] <= h.rect[2] && q[1] >= h.rect[1] && q[1] <= h.rect[3])
    };
    let adding = ui.input(|i| i.modifiers.shift || i.modifiers.command);
    let hovered = resp.hover_pos().and_then(hit_at);
    if let Some(h) = hovered
        && st.drag.is_none()
    {
        painter.rect_stroke(to_screen(h.rect).expand(2.0), 2.0, Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.6)), egui::StrokeKind::Outside);
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    for h in page.hits.iter().filter(|h| st.sel.contains(&h.item) || (st.sel.contains(&PlotItem::Legend) && matches!(h.item, PlotItem::LegendRow(_)))) {
        painter.rect_stroke(to_screen(h.rect).expand(2.0), 2.0, Stroke::new(1.5, theme::ACCENT), egui::StrokeKind::Outside);
    }
    if resp.drag_started() {
        let panning = ui.input(|i| i.pointer.middle_down() || i.pointer.secondary_down());
        let start = resp.interact_pointer_pos();
        st.drag = Some(match start.and_then(hit_at) {
            _ if panning => Drag::Pan,
            Some(h) => {
                if !st.sel.contains(&h.item) {
                    if !adding {
                        st.sel.clear();
                    }
                    st.sel.push(h.item.clone());
                }
                let items = movers(&st.sel);
                for i in &items {
                    pin(l, page, i);
                }
                Drag::Items(items)
            }
            None => Drag::Band(start.map_or([0.0, 0.0], to_page)),
        });
    }
    if resp.dragged() {
        match st.drag.clone() {
            Some(Drag::Pan) | None => st.pan += resp.drag_delta(),
            Some(Drag::Items(items)) => {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                let d = resp.drag_delta() / zoom;
                for i in &items {
                    move_item(l, i, [d.x, d.y], page.scale);
                }
            }
            Some(Drag::Band(a)) => {
                if let Some(p) = resp.interact_pointer_pos() {
                    let b = to_page(p);
                    let band = [a[0].min(b[0]), a[1].min(b[1]), a[0].max(b[0]), a[1].max(b[1])];
                    let r = to_screen(band);
                    painter.rect_filled(r, 0.0, theme::ACCENT.gamma_multiply(0.12));
                    painter.rect_stroke(r, 0.0, Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
                }
            }
        }
    }
    if resp.drag_stopped() {
        if let (Some(Drag::Band(a)), Some(p)) = (&st.drag, resp.interact_pointer_pos()) {
            let b = to_page(p);
            let band = [a[0].min(b[0]), a[1].min(b[1]), a[0].max(b[0]), a[1].max(b[1])];
            if !adding {
                st.sel.clear();
            }
            for h in &page.hits {
                let r = h.rect;
                if r[0] < band[2] && band[0] < r[2] && r[1] < band[3] && band[1] < r[3] && !st.sel.contains(&h.item) {
                    st.sel.push(h.item.clone());
                }
            }
        }
        st.drag = None;
    }
    if resp.clicked() {
        match resp.interact_pointer_pos().and_then(hit_at) {
            Some(h) if adding => {
                if let Some(i) = st.sel.iter().position(|x| *x == h.item) {
                    st.sel.remove(i);
                } else {
                    st.sel.push(h.item.clone());
                }
            }
            Some(h) => st.sel = vec![h.item.clone()],
            None if !adding => st.sel.clear(),
            None => {}
        }
    }
    if resp.double_clicked() {
        match resp.interact_pointer_pos().and_then(hit_at) {
            Some(_) => st.focus_text = true,
            None => {
                st.zoom = 1.0;
                st.pan = egui::Vec2::ZERO;
            }
        }
    }
    // Keys only while the pointer is over the page: the rest of the app keeps its shortcuts.
    if ui.ctx().egui_wants_keyboard_input() || !resp.contains_pointer() {
        return;
    }
    // Ctrl+A selects everything, Escape nothing; Delete hides the selection (removes added
    // texts); arrows nudge it (Shift: more).
    let (all, none, del, nudge) = ui.input(|i| {
        let step = if i.modifiers.shift { 10.0 } else { 1.0 };
        let mut n = [0.0f32, 0.0];
        for (key, d) in [(egui::Key::ArrowLeft, [-1.0, 0.0]), (egui::Key::ArrowRight, [1.0, 0.0]), (egui::Key::ArrowUp, [0.0, -1.0]), (egui::Key::ArrowDown, [0.0, 1.0])] {
            if i.key_pressed(key) {
                n = [n[0] + d[0] * step, n[1] + d[1] * step];
            }
        }
        let all = i.modifiers.command && i.key_pressed(egui::Key::A);
        (all, i.key_pressed(egui::Key::Escape), i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace), n)
    });
    if all {
        st.sel = page.hits.iter().map(|h| h.item.clone()).collect();
        st.sel.dedup();
    }
    if none {
        st.sel.clear();
    }
    if del {
        for item in std::mem::take(&mut st.sel) {
            match item {
                PlotItem::Note(id) => l.plot.notes.retain(|n| n.id != id),
                i => l.plot.edit_mut(&i).hidden = true,
            }
        }
    }
    if nudge != [0.0, 0.0] {
        // Arrows move by a point on the page.
        for i in movers(&st.sel) {
            pin(l, page, &i);
            move_item(l, &i, nudge, page.scale);
        }
    }
}

/// Move an item by `d` page points (`scale` points per metre on the grid).
fn move_item(l: &mut Lighting, item: &PlotItem, d: [f32; 2], scale: f32) {
    let m = [d[0] / scale, d[1] / scale];
    let (page_max, max) = (crate::limits::MAX_PLOT_PAGE, crate::limits::MAX_DISTANCE);
    let add = |v: &mut [f32; 2], d: [f32; 2], max: f32| {
        *v = [(v[0] + d[0]).clamp(-max, max), (v[1] + d[1]).clamp(-max, max)];
    };
    match item {
        PlotItem::Shape(id) => {
            if let Some(s) = l.shapes.iter_mut().find(|s| s.id == *id) {
                add(&mut s.label, m, max);
            }
        }
        PlotItem::Note(id) => {
            if let Some(n) = l.plot.note_mut(*id) {
                add(&mut n.at, m, max);
            }
        }
        i if i.on_page() => add(&mut l.plot.edit_mut(i).offset, d, page_max),
        i => add(&mut l.plot.edit_mut(i).offset, m, max),
    }
}

/// An item's text size and colour now (the editor's, else the usual ones from the page).
fn style_of(l: &Lighting, page: &Page, item: &PlotItem) -> Option<(f32, Rgb)> {
    if let PlotItem::Note(id) = item {
        return l.plot.notes.iter().find(|n| n.id == *id).map(|n| (n.size, n.colour));
    }
    let h = page.hits.iter().find(|h| &h.item == item)?;
    let e = l.plot.edit(item);
    Some((e.and_then(|e| e.size).unwrap_or(h.size), e.and_then(|e| e.colour).unwrap_or(h.colour)))
}

/// Give items a text size and/or colour (the usual ones drop the change).
fn set_style(l: &mut Lighting, page: &Page, items: &[PlotItem], size: Option<f32>, colour: Option<Rgb>) {
    for item in items {
        if let PlotItem::Note(id) = item {
            if let Some(n) = l.plot.note_mut(*id) {
                n.size = size.unwrap_or(n.size);
                n.colour = colour.unwrap_or(n.colour);
            }
            continue;
        }
        let usual = page.hits.iter().find(|h| &h.item == item).map(|h| (h.size, h.colour));
        let e = l.plot.edit_mut(item);
        if let Some(v) = size {
            e.size = (usual.map(|u| u.0) != Some(v)).then_some(v);
        }
        if let Some(c) = colour {
            e.colour = (usual.map(|u| u.1) != Some(c)).then_some(c);
        }
    }
}

/// Size and colour rows for the selected items that carry text.
fn style_rows(ui: &mut Ui, l: &mut Lighting, page: &Page, items: &[PlotItem]) {
    let texts: Vec<PlotItem> = items.iter().filter(|i| **i != PlotItem::Legend).cloned().collect();
    let Some((mut size, mut colour)) = texts.iter().find_map(|i| style_of(l, page, i)) else { return };
    let (lo, hi) = crate::limits::PLOT_TEXT_SIZE;
    ui.horizontal(|ui| {
        ui.label(theme::with_icon(icons::FORMAT_SIZE, t("Size")));
        if ui.add(egui::DragValue::new(&mut size).range(lo..=hi).speed(0.1).max_decimals(1).suffix(" pt")).changed() {
            set_style(l, page, &texts, Some(size), None);
        }
    });
    ui.horizontal(|ui| {
        ui.label(theme::with_icon(icons::FORMAT_COLOR_TEXT, t("Colour")));
        if ui.color_edit_button_srgb(&mut colour).changed() {
            set_style(l, page, &texts, None, Some(colour));
        }
    });
    let styled = texts.iter().any(|i| l.plot.edit(i).is_some_and(|e| e.size.is_some() || e.colour.is_some()));
    if styled && ui.button(theme::with_icon(icons::REPLAY, t("Usual size and colour"))).clicked() {
        for i in &texts {
            if let Some(e) = l.plot.edits.iter_mut().find(|e| &e.item == i) {
                e.size = None;
                e.colour = None;
            }
        }
    }
}

/// The selection's text, size, colour, visibility and reset; the hidden items.
fn side_column(ui: &mut Ui, st: &mut PlotUi, l: &mut Lighting, page: &Page) {
    let focus = std::mem::take(&mut st.focus_text);
    st.sel.retain(|i| !matches!(i, PlotItem::Note(id) if !l.plot.notes.iter().any(|n| n.id == *id)));
    let heading = |s: String| egui::RichText::new(s).font(theme::semibold(14.0));
    match st.sel.as_slice() {
        [] => {
            ui.label(theme::caption(t("Nothing selected")));
            ui.label(egui::RichText::new(t("Drag items to move them; click one to change its text, size or colour. Shift-click or drag a band around items to select several and line them up with the tools above. Arrow keys nudge the selection (Shift: more), Delete hides it. Scroll to zoom, drag with the right or middle button to pan.")).color(theme::TEXT_DIM));
        }
        [PlotItem::Note(id)] => {
            let id = *id;
            ui.label(heading(t("Text").into()));
            if let Some(n) = l.plot.note_mut(id) {
                let r = ui.add(egui::TextEdit::multiline(&mut n.text).desired_rows(3).desired_width(f32::INFINITY));
                if focus {
                    r.request_focus();
                }
                if n.text.len() > crate::limits::MAX_PLOT_TEXT {
                    n.text = n.text.chars().take(crate::limits::MAX_PLOT_TEXT).collect();
                }
                ui.checkbox(&mut n.bold, t("Bold"));
            }
            style_rows(ui, l, page, &[PlotItem::Note(id)]);
            if ui.button(theme::with_icon(icons::DELETE, t("Remove text"))).clicked() {
                l.plot.notes.retain(|n| n.id != id);
                st.sel.clear();
            }
        }
        [item] => {
            let item = item.clone();
            ui.label(heading(describe(&item, l)));
            let usual = page.hits.iter().find(|h| h.item == item).map(|h| h.text.clone());
            let edit = l.plot.edit(&item).cloned().unwrap_or_default();
            if item != PlotItem::Legend {
                let mut text = edit.text.clone().or(usual.clone()).unwrap_or_default();
                let r = ui.add(egui::TextEdit::singleline(&mut text).hint_text(usual.clone().unwrap_or_default()).desired_width(f32::INFINITY));
                if focus {
                    r.request_focus();
                }
                if r.changed() {
                    text.truncate(text.char_indices().nth(crate::limits::MAX_PLOT_TEXT).map_or(text.len(), |(i, _)| i));
                    l.plot.edit_mut(&item).text = (Some(&text) != usual.as_ref()).then_some(text);
                }
                style_rows(ui, l, page, std::slice::from_ref(&item));
            }
            symbol_picker(ui, st, l);
            let mut shown = !edit.hidden;
            if ui.checkbox(&mut shown, t("On the plot")).changed() {
                l.plot.edit_mut(&item).hidden = !shown;
            }
            if let PlotItem::LegendRow(_) = item {
                let mut legend = l.plot.edit(&PlotItem::Legend).is_none_or(|e| !e.hidden);
                if ui.checkbox(&mut legend, t("Legend on the plot")).changed() {
                    l.plot.edit_mut(&PlotItem::Legend).hidden = !legend;
                }
            }
            let shape = match &item {
                PlotItem::Shape(id) => l.shapes.iter_mut().find(|s| s.id == *id),
                _ => None,
            };
            let moved = shape.as_ref().is_some_and(|s| s.label != [0.0, 0.0]);
            if ui.add_enabled(!edit.is_noop() || moved, egui::Button::new(theme::with_icon(icons::REPLAY, t("Reset")))).on_hover_text(t("Back in its place, with its own text, size and colour")).clicked() {
                l.plot.edits.retain(|e| e.item != item);
                if let Some(s) = shape {
                    s.label = [0.0, 0.0];
                }
            }
        }
        items => {
            let items = items.to_vec();
            ui.label(heading(tr!("{n} items", n = items.len().to_string())));
            style_rows(ui, l, page, &items);
            symbol_picker(ui, st, l);
            ui.horizontal(|ui| {
                if ui.button(t("Hide")).clicked() {
                    for i in &items {
                        match i {
                            PlotItem::Note(_) => {}
                            i => l.plot.edit_mut(i).hidden = true,
                        }
                    }
                }
                if ui.button(theme::with_icon(icons::REPLAY, t("Reset"))).on_hover_text(t("Back in their places, with their own texts, sizes and colours")).clicked() {
                    l.plot.edits.retain(|e| !items.contains(&e.item));
                    for s in &mut l.shapes {
                        if items.contains(&PlotItem::Shape(s.id)) {
                            s.label = [0.0, 0.0];
                        }
                    }
                }
            });
        }
    }
    let hidden: Vec<PlotItem> = l.plot.edits.iter().filter(|e| e.hidden).map(|e| e.item.clone()).collect();
    if !hidden.is_empty() {
        ui.add_space(12.0);
        ui.separator();
        ui.label(theme::caption(t("Hidden")));
        for item in hidden {
            ui.horizontal(|ui| {
                if theme::icon_button(ui, icons::VISIBILITY, t("Show on the plot")).clicked() {
                    l.plot.edit_mut(&item).hidden = false;
                }
                if ui.selectable_label(st.sel == [item.clone()], describe(&item, l)).clicked() {
                    st.sel = vec![item.clone()];
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Grid items move in metres, page items in points; a bar's name moves its `PlotShape::label`.
    #[test]
    fn moves_in_the_right_units() {
        let mut l = Lighting::default();
        l.shapes.push(crate::light::model::PlotShape { id: 3, ..Default::default() });
        l.plot.notes.push(PlotNote { id: 1, ..Default::default() });
        move_item(&mut l, &PlotItem::Patch(7), [10.0, -20.0], 50.0);
        move_item(&mut l, &PlotItem::Title, [10.0, -20.0], 50.0);
        move_item(&mut l, &PlotItem::Shape(3), [5.0, 0.0], 50.0);
        move_item(&mut l, &PlotItem::Note(1), [0.0, 100.0], 50.0);
        assert_eq!(l.plot.edit(&PlotItem::Patch(7)).unwrap().offset, [0.2, -0.4]);
        assert_eq!(l.plot.edit(&PlotItem::Title).unwrap().offset, [10.0, -20.0]);
        assert_eq!(l.shapes[0].label, [0.1, 0.0]);
        assert_eq!(l.plot.notes[0].at, [0.0, 2.0]);
        assert_eq!(l.plot.edits.len(), 2, "shapes and notes keep their own places");
        // Never past the plausible ranges.
        move_item(&mut l, &PlotItem::Fixture(1), [1e9, 0.0], 1.0);
        assert_eq!(l.plot.edit(&PlotItem::Fixture(1)).unwrap().offset[0], crate::limits::MAX_DISTANCE);
    }

    fn rig() -> Lighting {
        let mut l = crate::light::library::bench_rig(4, 1, 1);
        for (i, f) in l.fixtures.iter_mut().enumerate() {
            f.position = [i as f32 * 2.0 - 3.0, 5.0, i as f32 * 0.3];
        }
        l
    }

    fn rect(l: &Lighting, item: &PlotItem) -> [f32; 4] {
        item_rect(&plot::page(l, "x", &mut Recorder::default()), item).unwrap()
    }

    /// Aligning tops puts every box on the highest top; spreading spaces centres evenly; a
    /// fixture's own labels aren't moved twice.
    #[test]
    fn align_and_spread() {
        let mut l = rig();
        let ids: Vec<u32> = l.fixtures.iter().map(|f| f.id).collect();
        let sel: Vec<PlotItem> = ids.iter().map(|&i| PlotItem::Fixture(i)).chain([PlotItem::Patch(ids[0])]).collect();
        assert_eq!(movers(&sel).len(), 4);
        let page = plot::page(&l, "x", &mut Recorder::default());
        arrange(&mut l, &page, &sel, Align::Top);
        let tops: Vec<f32> = ids.iter().map(|&i| rect(&l, &PlotItem::Fixture(i))[1]).collect();
        assert!(tops.iter().all(|t| (t - tops[0]).abs() < 0.01), "{tops:?}");
        // Pull one out of line, then spread across.
        move_item(&mut l, &PlotItem::Fixture(ids[1]), [30.0, 0.0], page.scale);
        let page = plot::page(&l, "x", &mut Recorder::default());
        arrange(&mut l, &page, &sel, Align::SpreadX);
        let xs: Vec<f32> = ids.iter().map(|&i| rect(&l, &PlotItem::Fixture(i))).map(|r| (r[0] + r[2]) / 2.0).collect();
        let mut sorted = xs.clone();
        sorted.sort_by(f32::total_cmp);
        assert!(((sorted[1] - sorted[0]) - (sorted[3] - sorted[2])).abs() < 0.01 && ((sorted[2] - sorted[1]) - (sorted[1] - sorted[0])).abs() < 0.01, "{xs:?}");
    }

    /// Size and colour changes; the usual values drop the change.
    #[test]
    fn style() {
        let mut l = rig();
        let page = plot::page(&l, "x", &mut Recorder::default());
        let items = [PlotItem::Title, PlotItem::Patch(l.fixtures[0].id)];
        set_style(&mut l, &page, &items, Some(30.0), Some([200, 0, 0]));
        assert_eq!(style_of(&l, &page, &items[0]), Some((30.0, [200, 0, 0])));
        let s = plot::svg(&l, "x");
        assert!(s.contains(r##"font-size="30" font-weight="bold" fill="#c80000">x</text>"##), "{}", &s[..800]);
        set_style(&mut l, &page, &items, Some(20.0), Some([0, 0, 0]));
        assert_eq!(l.plot.edit(&items[0]).unwrap().size, None, "20 is the title's usual size");
        assert_eq!(l.plot.edit(&items[0]).unwrap().colour, None);
        assert_eq!(l.plot.edit(&items[1]).unwrap().size, Some(20.0));
    }
}
