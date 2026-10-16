//! Mapping mode pictures: the test pattern (drawn on the stage canvas, so it goes through the
//! corner-pin warp) and the corner pins with their instructions (drawn on the window). Both are
//! SVG rasterised with resvg, only while a stage is being mapped.

use crate::model::Stage;
use std::fmt::Write;

/// The mapping test pattern of `stage` at `size` pixels: a white canvas with a grid, pixel
/// rulers along every edge, circles and a centre cross. Circles that come out as ellipses or
/// grid squares that aren't square show what is still off. Premultiplied RGBA.
pub fn test_pattern(stage: &Stage, size: [u32; 2]) -> anyhow::Result<Vec<u8>> {
    crate::media::still::render_svg_markup(&pattern_svg(stage), size[0], size[1])
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The test pattern as SVG, in stage pixels.
fn pattern_svg(stage: &Stage) -> String {
    let (w, h) = (stage.width.max(1) as f32, stage.height.max(1) as f32);
    let mut s = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">"#);
    s.push_str(r#"<rect width="100%" height="100%" fill="white"/>"#);
    let line = |s: &mut String, a: [f32; 2], b: [f32; 2], width: f32, gray: u8| {
        let _ = write!(s, r#"<line x1="{}" y1="{}" x2="{}" y2="{}" stroke="rgb({gray},{gray},{gray})" stroke-width="{width}"/>"#, a[0], a[1], b[0], b[1]);
    };
    let circle = |s: &mut String, c: [f32; 2], r: f32, width: f32, gray: u8| {
        let _ = write!(s, r#"<circle cx="{}" cy="{}" r="{r}" fill="none" stroke="rgb({gray},{gray},{gray})" stroke-width="{width}"/>"#, c[0], c[1]);
    };
    // Grid: square cells, a tenth of the height, centred on the canvas.
    let cell = h / 10.0;
    let (cx, cy) = (w / 2.0, h / 2.0);
    let n = (w / cell / 2.0).ceil() as i32;
    for k in -n..=n {
        let x = cx + k as f32 * cell;
        if x > 0.0 && x < w {
            line(&mut s, [x, 0.0], [x, h], 1.0, if k % 5 == 0 { 120 } else { 190 });
        }
    }
    for k in -5..=5 {
        let y = cy + k as f32 * cell;
        if y > 0.0 && y < h {
            line(&mut s, [0.0, y], [w, y], 1.0, if k % 5 == 0 { 120 } else { 190 });
        }
    }
    // Diagonals, circles and the centre cross.
    line(&mut s, [0.0, 0.0], [w, h], 1.0, 160);
    line(&mut s, [w, 0.0], [0.0, h], 1.0, 160);
    let r = h.min(w) / 2.0;
    circle(&mut s, [cx, cy], r * 0.92, 2.5, 20);
    circle(&mut s, [cx, cy], r * 0.5, 1.5, 60);
    for (qx, qy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
        circle(&mut s, [w * qx, h * qy], r * 0.22, 1.5, 60);
    }
    line(&mut s, [cx - cell, cy], [cx + cell, cy], 2.0, 20);
    line(&mut s, [cx, cy - cell], [cx, cy + cell], 2.0, 20);
    // Rulers: ticks every 10 px, longer every 50 and 100 px, numbered every 100 (or more).
    let label_every = [100.0, 200.0, 500.0, 1000.0].into_iter().find(|e| w / e <= 24.0).unwrap_or(1000.0);
    let text = |s: &mut String, at: [f32; 2], anchor: &str, t: String| {
        let _ = write!(
            s,
            r#"<text x="{}" y="{}" text-anchor="{anchor}" font-family="monospace" font-size="11" fill="rgb(20,20,20)">{t}</text>"#,
            at[0], at[1]
        );
    };
    let tick = |v: f32| if v % 100.0 == 0.0 { 24.0 } else if v % 50.0 == 0.0 { 14.0 } else { 7.0 };
    let mut x = 0.0;
    while x <= w {
        let len = tick(x);
        line(&mut s, [x, 0.0], [x, len], 1.0, 20);
        line(&mut s, [x, h], [x, h - len], 1.0, 20);
        if x % label_every == 0.0 && x > 150.0 && x < w - 150.0 {
            text(&mut s, [x + 3.0, 36.0], "start", format!("{x}"));
            text(&mut s, [x + 3.0, h - 28.0], "start", format!("{x}"));
        }
        x += 10.0;
    }
    let mut y = 0.0;
    while y <= h {
        let len = tick(y);
        line(&mut s, [0.0, y], [len, y], 1.0, 20);
        line(&mut s, [w, y], [w - len, y], 1.0, 20);
        if y % label_every == 0.0 && y > 120.0 && y < h - 120.0 {
            text(&mut s, [27.0, y + 12.0], "start", format!("{y}"));
            text(&mut s, [w - 27.0, y + 12.0], "end", format!("{y}"));
        }
        y += 10.0;
    }
    let _ = write!(s, r#"<rect x="1.5" y="1.5" width="{}" height="{}" fill="none" stroke="rgb(20,20,20)" stroke-width="3"/>"#, w - 3.0, h - 3.0);
    let _ = write!(
        s,
        r#"<text x="{cx}" y="{}" text-anchor="middle" font-family="sans-serif" font-size="18" fill="rgb(20,20,20)">{}  ·  {} × {}</text>"#,
        cy + cell * 1.4 + 18.0,
        esc(&stage.name),
        stage.width,
        stage.height
    );
    s.push_str("</svg>");
    s
}

/// The corner pins (`corners` in 0..1 of the window, `hot` highlighted) and the instructions,
/// at `size` window pixels. Premultiplied RGBA.
pub fn pins(size: [u32; 2], corners: [[f32; 2]; 4], hot: Option<usize>, message: &str) -> anyhow::Result<Vec<u8>> {
    let (w, h) = (size[0].max(1) as f32, size[1].max(1) as f32);
    let mut s = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">"#);
    for (k, c) in corners.iter().enumerate() {
        let col = if hot == Some(k) { "rgb(255,176,32)" } else { "rgb(74,124,240)" };
        let (x, y) = (c[0] * w, c[1] * h);
        let _ = write!(
            s,
            r#"<circle cx="{x}" cy="{y}" r="16" fill="{col}" fill-opacity="0.25" stroke="{col}" stroke-width="2.5"/><line x1="{}" y1="{y}" x2="{}" y2="{y}" stroke="{col}" stroke-width="1.5"/><line x1="{x}" y1="{}" x2="{x}" y2="{}" stroke="{col}" stroke-width="1.5"/>"#,
            x - 24.0,
            x + 24.0,
            y - 24.0,
            y + 24.0
        );
        // The number sits on the side facing into the picture.
        let (ox, oy) = (if k == 1 || k == 2 { -26.0 } else { 26.0 }, if k >= 2 { -26.0 } else { 26.0 });
        let _ = write!(
            s,
            r#"<circle cx="{}" cy="{}" r="10" fill="{col}"/><text x="{}" y="{}" text-anchor="middle" font-family="sans-serif" font-size="12" fill="white">{}</text>"#,
            x + ox,
            y + oy,
            x + ox,
            y + oy + 4.0,
            k + 1
        );
    }
    // Instructions on a dark pill so they read over the white pattern.
    let pill_w = (message.chars().count() as f32 * 7.4 + 24.0).min(w - 8.0);
    let _ = write!(
        s,
        r#"<rect x="{}" y="10" width="{pill_w}" height="28" rx="8" fill="black" fill-opacity="0.8"/><text x="{}" y="29" text-anchor="middle" font-family="sans-serif" font-size="14" fill="white">{}</text>"#,
        (w - pill_w) / 2.0,
        w / 2.0,
        esc(message)
    );
    s.push_str("</svg>");
    crate::media::still::render_svg_markup(&s, size[0], size[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_is_white_with_a_dark_border() {
        let st = Stage { width: 320, height: 180, ..Stage::new("A <B>") };
        let px = test_pattern(&st, [320, 180]).unwrap();
        let at = |x: usize, y: usize| &px[(y * 320 + x) * 4..(y * 320 + x) * 4 + 4];
        assert_eq!(at(0, 0)[..3], [20, 20, 20]);
        assert!(at(55, 40).iter().all(|c| *c == 255), "{:?}", at(55, 40));
        let ov = pins([200, 100], [[0.1, 0.1], [0.9, 0.1], [0.9, 0.9], [0.1, 0.9]], Some(0), "Map").unwrap();
        // Transparent away from the pins and the pill, coloured on a pin ring.
        assert_eq!(ov[(60 * 200 + 100) * 4 + 3], 0);
        assert!(ov[(10 * 200 + (20 + 16)) * 4 + 3] > 100);
    }
}
