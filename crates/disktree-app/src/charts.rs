//! The three ways the same folder is drawn, and where a partition chart's
//! unit space lands on screen.
//!
//! A treemap lays out in viewport pixels. A sunburst and an icicle share one
//! layout, [`disktree_core::partition`], in unit space: `x` a share, `y` a
//! level. [`ChartGeometry`] maps that to rings around a hole or to columns,
//! and painting and hit-testing both go through it, so they cannot disagree.

use disktree_core::treemap::Rect;

/// How the folder on screen is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Chart {
    #[default]
    Treemap,
    /// A ring per level around the folder, whose centre goes up a level.
    Sunburst,
    /// A column per level, after one for the folder itself.
    Icicle,
}

impl Chart {
    pub const ALL: [Self; 3] = [Self::Treemap, Self::Sunburst, Self::Icicle];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Treemap => "Treemap",
            Self::Sunburst => "Sunburst",
            Self::Icicle => "Icicle",
        }
    }

    /// Its name as a choice in the top bar.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Treemap => "treemap",
            Self::Sunburst => "sunburst",
            Self::Icicle => "icicle",
        }
    }

    /// Laid out by [`disktree_core::partition`], in unit space.
    pub const fn is_partition(self) -> bool {
        !matches!(self, Self::Treemap)
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Treemap => Self::Sunburst,
            Self::Sunburst => Self::Icicle,
            Self::Icicle => Self::Treemap,
        }
    }

    pub const fn index(self) -> usize {
        match self {
            Self::Treemap => 0,
            Self::Sunburst => 1,
            Self::Icicle => 2,
        }
    }
}

/// Where a point falls in a partition chart.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Location {
    /// The sunburst's hole, or the icicle's first column: the folder drawn.
    Hole,
    /// A point of the layout's unit space.
    Unit {
        x: f32,
        y: f32,
    },
    Outside,
}

/// A partition chart's unit space on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChartGeometry {
    pub chart: Chart,
    pub width: f32,
    pub height: f32,
    pub levels: u32,
    /// The icicle's first column, which stands for the folder itself.
    pub root_width: f32,
}

impl ChartGeometry {
    pub fn centre(&self) -> (f32, f32) {
        (self.width / 2.0, self.height / 2.0)
    }

    pub fn outer(&self) -> f32 {
        (self.width.min(self.height) / 2.0 - 8.0).max(1.0)
    }

    pub fn hole(&self) -> f32 {
        self.outer() * 0.3
    }

    pub fn ring(&self) -> f32 {
        (self.outer() - self.hole()) / self.levels.max(1) as f32
    }

    pub fn column(&self) -> f32 {
        (self.width - self.root_width) / self.levels.max(1) as f32
    }

    /// The icicle's columns, beside its first one.
    pub fn columns(&self) -> Rect {
        Rect::new(
            self.root_width,
            0.0,
            (self.width - self.root_width).max(0.0),
            self.height,
        )
    }

    pub fn locate(&self, x: f32, y: f32) -> Location {
        match self.chart {
            Chart::Sunburst => {
                let (cx, cy) = self.centre();
                let (dx, dy) = (x - cx, y - cy);
                let radius = dx.hypot(dy);
                if radius < self.hole() {
                    return Location::Hole;
                }
                if radius >= self.outer() {
                    return Location::Outside;
                }
                // Clockwise from twelve o'clock; y grows downwards.
                let turn = (dy.atan2(dx) + std::f32::consts::FRAC_PI_2)
                    / std::f32::consts::TAU;
                Location::Unit {
                    x: if turn < 0.0 { turn + 1.0 } else { turn },
                    y: (radius - self.hole()) / self.ring(),
                }
            }
            Chart::Icicle => {
                if x < 0.0 || x >= self.width || y < 0.0 || y >= self.height {
                    return Location::Outside;
                }
                if x < self.root_width {
                    return Location::Hole;
                }
                Location::Unit {
                    x: y / self.height,
                    y: (x - self.root_width) / self.column(),
                }
            }
            Chart::Treemap => Location::Outside,
        }
    }

    /// The narrowest share worth drawing at `depth`: `points` along the
    /// ring's inner edge, where it is shortest, or down the column.
    pub fn min_span(&self, depth: u32, points: f32) -> f32 {
        match self.chart {
            Chart::Sunburst => {
                points
                    / (std::f32::consts::TAU
                        * self.ring().mul_add(depth as f32, self.hole()))
            }
            Chart::Icicle => points / self.height.max(1.0),
            Chart::Treemap => 0.0,
        }
    }

    /// A unit-space tile in pixels, for arrow keys and label room: an
    /// icicle's as drawn, a sunburst's ring unrolled at its outer edge.
    pub fn flat(&self, rect: Rect) -> Rect {
        match self.chart {
            Chart::Icicle => Rect::new(
                rect.y.mul_add(self.column(), self.root_width),
                rect.x * self.height,
                rect.h * self.column(),
                rect.w * self.height,
            ),
            Chart::Sunburst => {
                let around = std::f32::consts::TAU * self.outer();
                Rect::new(
                    rect.x * around,
                    rect.y * self.ring(),
                    rect.w * around,
                    self.ring(),
                )
            }
            Chart::Treemap => rect,
        }
    }
}

pub fn clipped(rect: Rect, bounds: Rect) -> Option<Rect> {
    let (left, top) = (rect.x.max(bounds.x), rect.y.max(bounds.y));
    let (right, bottom) = (
        rect.right().min(bounds.right()),
        rect.bottom().min(bounds.bottom()),
    );
    (right > left && bottom > top)
        .then(|| Rect::new(left, top, right - left, bottom - top))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sunburst() -> ChartGeometry {
        ChartGeometry {
            chart: Chart::Sunburst,
            width: 800.0,
            height: 600.0,
            levels: 4,
            root_width: 0.0,
        }
    }

    fn icicle() -> ChartGeometry {
        ChartGeometry {
            chart: Chart::Icicle,
            width: 900.0,
            height: 600.0,
            levels: 4,
            root_width: 100.0,
        }
    }

    #[test]
    fn a_ring_point_comes_back_as_the_unit_point_it_was_drawn_from() {
        let geometry = sunburst();
        let (cx, cy) = geometry.centre();
        let (unit_x, unit_y): (f32, f32) = (0.3, 1.5);
        let angle =
            unit_x.mul_add(std::f32::consts::TAU, -std::f32::consts::FRAC_PI_2);
        let radius = geometry.ring().mul_add(unit_y, geometry.hole());
        let located = geometry.locate(
            angle.cos().mul_add(radius, cx),
            angle.sin().mul_add(radius, cy),
        );
        let Location::Unit { x, y } = located else {
            panic!("{located:?}");
        };
        assert!((x - unit_x).abs() < 1e-4 && (y - unit_y).abs() < 1e-4);
        assert_eq!(geometry.locate(cx, cy), Location::Hole);
        assert_eq!(geometry.locate(0.0, 0.0), Location::Outside);
    }

    #[test]
    fn an_icicle_column_is_a_level_and_its_first_one_the_folder() {
        let geometry = icicle();
        assert_eq!(geometry.locate(50.0, 300.0), Location::Hole);
        let located = geometry.locate(200.0f32.mul_add(1.5, 100.0), 150.0);
        let Location::Unit { x, y } = located else {
            panic!("{located:?}");
        };
        assert!((x - 0.25).abs() < 1e-5 && (y - 1.5).abs() < 1e-5);
        let flat = geometry.flat(Rect::new(0.25, 1.0, 0.5, 1.0));
        assert_eq!(flat, Rect::new(300.0, 150.0, 200.0, 300.0));
    }

    #[test]
    fn the_narrowest_share_is_measured_where_a_ring_is_shortest() {
        let geometry = sunburst();
        let inner = geometry.min_span(0, 5.0);
        let outer = geometry.min_span(3, 5.0);
        assert!(inner > outer, "deeper rings are longer");
        assert!(
            (inner * std::f32::consts::TAU)
                .mul_add(geometry.hole(), -5.0)
                .abs()
                < 1e-3
        );
    }

    #[test]
    fn charts_cycle_in_the_order_the_switch_shows_them() {
        let mut chart = Chart::Treemap;
        for (index, &shown) in Chart::ALL.iter().enumerate() {
            assert_eq!(chart, shown);
            assert_eq!(chart.index(), index);
            chart = chart.next();
        }
        assert_eq!(chart, Chart::Treemap);
    }

    #[test]
    fn clipping_keeps_only_the_overlap() {
        let bounds = Rect::new(0.0, 0.0, 1.0, 4.0);
        assert_eq!(
            clipped(Rect::new(0.5, 3.0, 1.0, 2.0), bounds),
            Some(Rect::new(0.5, 3.0, 0.5, 1.0))
        );
        assert_eq!(clipped(Rect::new(2.0, 0.0, 1.0, 1.0), bounds), None);
    }
}
