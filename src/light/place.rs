//! Fixture placement helpers: positions along a line, an arc or a grid, for the Arrange tool.
//!
//! Positions are metres in the patch's space (x right, y up, z towards the audience); the plan
//! view looks down on x/z. They are used by visualisers (a separate program) and plots, not by
//! playback.

/// How to lay out a set of fixtures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Layout {
    /// Evenly from `from` to `to` (the first fixture at `from`, the last at `to`).
    Line {
        /// First position.
        from: [f32; 3],
        /// Last position.
        to: [f32; 3],
    },
    /// Evenly on a horizontal arc around `centre`, angles in degrees (0 = towards the audience,
    /// +z; 90 = stage left, +x).
    Arc {
        /// Centre (its y is the height of the arc).
        centre: [f32; 3],
        /// Radius in metres.
        radius: f32,
        /// Angle of the first fixture.
        from: f32,
        /// Angle of the last fixture.
        to: f32,
    },
    /// Rows of `cols` fixtures from `origin`, `spacing` apart in x and z.
    Grid {
        /// First fixture.
        origin: [f32; 3],
        /// Fixtures per row.
        cols: u32,
        /// Distance between columns (x) and rows (z).
        spacing: [f32; 2],
    },
}

/// `n` positions for a layout. Coordinates are rounded to millimetres.
pub fn positions(layout: Layout, n: usize) -> Vec<[f32; 3]> {
    let t = |i: usize| if n <= 1 { 0.0 } else { i as f32 / (n - 1) as f32 };
    let round = |p: [f32; 3]| p.map(|v| (v * 1000.0).round() / 1000.0);
    (0..n)
        .map(|i| match layout {
            Layout::Line { from, to } => round(std::array::from_fn(|k| from[k] + (to[k] - from[k]) * t(i))),
            Layout::Arc { centre, radius, from, to } => {
                let a = (from + (to - from) * t(i)).to_radians();
                round([centre[0] + radius * a.sin(), centre[1], centre[2] + radius * a.cos()])
            }
            Layout::Grid { origin, cols, spacing } => {
                let cols = cols.max(1) as usize;
                round([origin[0] + (i % cols) as f32 * spacing[0], origin[1], origin[2] + (i / cols) as f32 * spacing[1]])
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts() {
        let line = positions(Layout::Line { from: [-2.0, 6.0, 0.0], to: [2.0, 6.0, 0.0] }, 5);
        assert_eq!(line.iter().map(|p| p[0]).collect::<Vec<_>>(), [-2.0, -1.0, 0.0, 1.0, 2.0]);
        assert!(line.iter().all(|p| p[1] == 6.0));
        assert_eq!(positions(Layout::Line { from: [1.0; 3], to: [5.0; 3] }, 1), [[1.0; 3]], "one fixture: at the start");
        let arc = positions(Layout::Arc { centre: [0.0, 5.0, 0.0], radius: 2.0, from: -90.0, to: 90.0 }, 3);
        assert_eq!(arc, [[-2.0, 5.0, 0.0], [0.0, 5.0, 2.0], [2.0, 5.0, 0.0]]);
        let grid = positions(Layout::Grid { origin: [0.0, 4.0, -1.0], cols: 2, spacing: [1.5, 2.0] }, 5);
        assert_eq!(grid, [[0.0, 4.0, -1.0], [1.5, 4.0, -1.0], [0.0, 4.0, 1.0], [1.5, 4.0, 1.0], [0.0, 4.0, 3.0]]);
        assert!(positions(Layout::Grid { origin: [0.0; 3], cols: 0, spacing: [1.0, 1.0] }, 3).len() == 3, "0 columns: one per row");
    }
}
