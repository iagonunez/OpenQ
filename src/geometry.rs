//! Stage geometry: layer placement on a stage canvas and corner-pin (keystone) warping.

use crate::model::{Geometry, VideoFit};

/// Projective map from the unit square to an arbitrary quad (Heckbert's square-to-quad).
#[derive(Clone, Copy, Debug)]
pub struct Homography {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
    g: f32,
    h: f32,
}

impl Homography {
    /// `q` = images of (0,0), (1,0), (1,1), (0,1).
    pub fn square_to_quad(q: [[f32; 2]; 4]) -> Self {
        let [[x0, y0], [x1, y1], [x2, y2], [x3, y3]] = q;
        let (dx1, dx2, dx3) = (x1 - x2, x3 - x2, x0 - x1 + x2 - x3);
        let (dy1, dy2, dy3) = (y1 - y2, y3 - y2, y0 - y1 + y2 - y3);
        let det = dx1 * dy2 - dx2 * dy1;
        let (g, h) = if (dx3.abs() < 1e-9 && dy3.abs() < 1e-9) || det.abs() < 1e-12 {
            (0.0, 0.0)
        } else {
            ((dx3 * dy2 - dx2 * dy3) / det, (dx1 * dy3 - dx3 * dy1) / det)
        };
        Self {
            a: x1 - x0 + g * x1,
            b: x3 - x0 + h * x3,
            c: x0,
            d: y1 - y0 + g * y1,
            e: y3 - y0 + h * y3,
            f: y0,
            g,
            h,
        }
    }

    pub fn map(&self, u: f32, v: f32) -> [f32; 2] {
        let w = self.g * u + self.h * v + 1.0;
        let w = if w.abs() < 1e-9 { 1e-9 } else { w };
        [(self.a * u + self.b * v + self.c) / w, (self.d * u + self.e * v + self.f) / w]
    }
}

/// A layer placed on a canvas: corners (TL, TR, BR, BL) in normalised canvas coordinates
/// (0..1, may extend beyond) and the source UV rect `[u0, v0, u1, v1]`.
#[derive(Clone, Copy, Debug)]
pub struct LayerQuad {
    pub corners: [[f32; 2]; 4],
    pub uv: [f32; 4],
}

impl LayerQuad {
    /// Point at (s, t) ∈ [0,1]² across the quad (exact: the quad is a parallelogram).
    pub fn point(&self, s: f32, t: f32) -> [f32; 2] {
        let [p0, p1, _, p3] = self.corners;
        [p0[0] + (p1[0] - p0[0]) * s + (p3[0] - p0[0]) * t, p0[1] + (p1[1] - p0[1]) * s + (p3[1] - p0[1]) * t]
    }

    pub fn uv_at(&self, s: f32, t: f32) -> [f32; 2] {
        [self.uv[0] + (self.uv[2] - self.uv[0]) * s, self.uv[1] + (self.uv[3] - self.uv[1]) * t]
    }
}

pub fn layer_quad(canvas: [f32; 2], image: [f32; 2], fit: VideoFit, g: &Geometry) -> LayerQuad {
    let [cw, ch] = canvas;
    let [iw, ih] = [image[0].max(1.0), image[1].max(1.0)];
    let (bw, bh, uv) = match fit {
        VideoFit::Stretch => (cw, ch, [0.0, 0.0, 1.0, 1.0]),
        VideoFit::Native => (iw, ih, [0.0, 0.0, 1.0, 1.0]),
        VideoFit::Fit => {
            let s = (cw / iw).min(ch / ih);
            (iw * s, ih * s, [0.0, 0.0, 1.0, 1.0])
        }
        VideoFit::Fill => {
            let s = (cw / iw).max(ch / ih);
            let (vw, vh) = (cw / (iw * s), ch / (ih * s));
            (cw, ch, [0.5 - vw / 2.0, 0.5 - vh / 2.0, 0.5 + vw / 2.0, 0.5 + vh / 2.0])
        }
    };
    let (sin, cos) = g.rotation.to_radians().sin_cos();
    let centre = [cw / 2.0 + g.x * cw, ch / 2.0 + g.y * ch];
    let local = [[-bw / 2.0, -bh / 2.0], [bw / 2.0, -bh / 2.0], [bw / 2.0, bh / 2.0], [-bw / 2.0, bh / 2.0]];
    let corners = local.map(|[x, y]| {
        let (x, y) = (x * g.scale_x, y * g.scale_y);
        // Clockwise rotation in y-down screen space.
        let (rx, ry) = (x * cos - y * sin, x * sin + y * cos);
        [(centre[0] + rx) / cw, (centre[1] + ry) / ch]
    });
    LayerQuad { corners, uv }
}

/// Map a point in source-canvas coordinates into an output that shows `crop` of that canvas.
pub fn to_crop(p: [f32; 2], crop: [f32; 4]) -> [f32; 2] {
    let w = (crop[2] - crop[0]).max(1e-6);
    let h = (crop[3] - crop[1]).max(1e-6);
    [(p[0] - crop[0]) / w, (p[1] - crop[1]) / h]
}

/// Brightness multiplier inside a soft-edge blend zone. `t` runs 0 at the outer edge to 1 at
/// the inner edge. The S-curve satisfies f(t) + f(1 - t) = 1, so two overlapping projectors sum
/// to full light once each projector's gamma is undone.
pub fn blend_gain(t: f32, gamma: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let s = if t < 0.5 { 2.0 * t * t } else { 1.0 - 2.0 * (1.0 - t) * (1.0 - t) };
    s.powf(1.0 / gamma.max(0.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4
    }

    #[test]
    fn blend_curves_sum_to_full_light() {
        for i in 0..=10 {
            let t = i as f32 / 10.0;
            let g = 2.2;
            // Light output ∝ value^gamma; two projectors overlapping must add to 1.
            let light = blend_gain(t, g).powf(g) + blend_gain(1.0 - t, g).powf(g);
            assert!((light - 1.0).abs() < 1e-4, "{t}: {light}");
        }
        assert_eq!(blend_gain(0.0, 2.2), 0.0);
        assert!((blend_gain(1.0, 2.2) - 1.0).abs() < 1e-6);
        assert_eq!(to_crop([0.5, 0.5], [0.5, 0.0, 1.0, 1.0]), [0.0, 0.5]);
    }

    #[test]
    fn homography_maps_corners() {
        for q in [
            [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            [[0.1, 0.05], [0.9, 0.0], [1.0, 1.0], [0.0, 0.95]],
            [[10.0, 20.0], [300.0, 40.0], [280.0, 200.0], [30.0, 180.0]],
        ] {
            let h = Homography::square_to_quad(q);
            assert!(close(h.map(0.0, 0.0), q[0]));
            assert!(close(h.map(1.0, 0.0), q[1]));
            assert!(close(h.map(1.0, 1.0), q[2]));
            assert!(close(h.map(0.0, 1.0), q[3]));
        }
        let id = Homography::square_to_quad([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]);
        assert!(close(id.map(0.3, 0.7), [0.3, 0.7]));
    }

    #[test]
    fn fit_letterboxes_and_geometry_moves() {
        // 4:3 image on a 16:9 canvas: pillarboxed.
        let q = layer_quad([1600.0, 900.0], [400.0, 300.0], VideoFit::Fit, &Geometry::default());
        assert!(close(q.corners[0], [0.125, 0.0]));
        assert!(close(q.corners[2], [0.875, 1.0]));
        // Fill crops vertically instead.
        let q = layer_quad([1600.0, 900.0], [400.0, 300.0], VideoFit::Fill, &Geometry::default());
        assert!(close(q.corners[0], [0.0, 0.0]));
        assert!(q.uv[1] > 0.0 && q.uv[3] < 1.0);
        // Half scale, shifted right by a quarter stage, rotated 90°.
        let g = Geometry { x: 0.25, scale_x: 0.5, scale_y: 0.5, rotation: 90.0, ..Default::default() };
        let q = layer_quad([1000.0, 1000.0], [100.0, 100.0], VideoFit::Stretch, &g);
        // Top-left corner rotates to the top-right position.
        assert!(close(q.corners[0], [1.0, 0.25]));
        assert!(close(q.point(0.5, 0.5), [0.75, 0.5]));
    }
}
