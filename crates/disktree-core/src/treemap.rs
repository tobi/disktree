//! Squarified treemap layout.
//!
//! Bruls, Huizing and van Wijk's squarified algorithm: grow a row of tiles
//! while the worst aspect ratio inside it keeps improving, then start a new row
//! in the remaining space. That is what produces the legible mosaic of
//! KDirStat-style explorers instead of the slivers a naive slice-and-dice
//! gives.
//!
//! Layout runs in pixels, in the coordinate space of an unzoomed viewport. The
//! view applies its own pan and zoom when painting, so re-layout is only needed
//! when the tree, the viewport size, or the requested depth changes.

use crate::filter::{Keep, Matches};
use crate::tree::{Metric, Node};

/// An axis-aligned rectangle in viewport pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn area(&self) -> f32 {
        (self.w.max(0.0)) * (self.h.max(0.0))
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// Shrink on every side, never past empty.
    #[must_use]
    pub fn inset(&self, padding: f32) -> Self {
        let w = padding.mul_add(-2.0, self.w).max(0.0);
        let h = padding.mul_add(-2.0, self.h).max(0.0);
        Self::new(self.x + padding, self.y + padding, w, h)
    }

    #[must_use]
    pub fn scaled(&self, scale: f32) -> Self {
        Self::new(
            self.x * scale,
            self.y * scale,
            self.w * scale,
            self.h * scale,
        )
    }

    #[must_use]
    pub fn translated(&self, dx: f32, dy: f32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w, self.h)
    }
}

/// What a tile stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TileKind {
    /// A real node at `crumbs`, which are child indices from the scanned root.
    Node { crumbs: Vec<usize> },
    /// The tail of a long child list, merged so its area is still accounted
    /// for instead of silently dropped. `crumbs` are its folder's, then
    /// [`REST`]; `bytes` and `files` total what it stands for, filtered like
    /// the rest of the layout.
    Others {
        crumbs: Vec<usize>,
        count: usize,
        bytes: u64,
        files: u64,
    },
}

/// The last crumb of a merged tail: an index no child has, so its crumbs
/// resolve to no node and nothing acts on its folder through it.
pub const REST: usize = usize::MAX;

pub fn is_rest(crumbs: &[usize]) -> bool {
    crumbs.last() == Some(&REST)
}

/// One rectangle of the mosaic.
#[derive(Clone, Debug)]
pub struct Tile {
    pub kind: TileKind,
    pub rect: Rect,
    /// Nesting level: 0 tiles are children of the current root.
    pub depth: u32,
    /// The band this directory reserved for its own name, when it is
    /// subdivided. Its children are laid out below it, so a parent's name
    /// never sits on top of a child — and the band is part of the parent for
    /// hit-testing, which makes a parent reachable without aiming at its
    /// border.
    pub header: Option<Rect>,
}

impl Tile {
    pub fn crumbs(&self) -> &[usize] {
        match &self.kind {
            TileKind::Node { crumbs } | TileKind::Others { crumbs, .. } => {
                crumbs
            }
        }
    }

    /// Where a subdivided directory's children sit: below its name band.
    pub fn body(&self) -> Rect {
        self.header.map_or(self.rect, |header| {
            Rect::new(
                self.rect.x,
                header.bottom(),
                self.rect.w,
                self.rect.bottom() - header.bottom(),
            )
        })
    }
}

/// How much of the tree to draw, and how finely.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutOptions {
    /// Nesting levels drawn at once; 1 draws only the root's children.
    pub max_depth: u32,
    /// Gap between siblings inside a directory, and inset into it.
    pub padding: f32,
    /// Gap between the top-level directories: wider than `padding`, so the
    /// first level of structure reads before the detail inside it.
    pub padding_outer: f32,
    /// Tiles below this many pixels are dropped; they cannot be read or hit.
    pub min_tile: f32,
    /// Children kept per directory. The tail merges into one `Others` tile.
    pub max_children: usize,
    /// Height of the band a top-level directory keeps for its name.
    pub header: f32,
    /// Height of the slimmer band a deeper directory keeps when it is drawn
    /// open. A directory drawn closed has no band: its label sits in its
    /// corner, over nothing but its own fill.
    pub header_inner: f32,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            max_depth: 3,
            padding: 1.0,
            min_tile: 5.0,
            max_children: 96,
            padding_outer: 3.0,
            header: 20.0,
            header_inner: 15.0,
        }
    }
}

/// Lay out everything beneath `root` inside `area`.
///
/// `root_crumbs` is where `root` sits in the scanned tree. Every tile's
/// crumbs extend it, so they always address the scanned tree, never the node
/// being drawn: a caller that resolves a tile from the scanned root gets that
/// tile, at any depth.
pub fn layout(
    root: &Node,
    root_crumbs: &[usize],
    area: Rect,
    metric: Metric,
    options: &LayoutOptions,
) -> Vec<Tile> {
    layout_filtered(root, root_crumbs, area, metric, options, None, 0)
}

/// [`layout`], showing only what `filter` keeps and leaving out the root's
/// `skipping` largest children.
///
/// A filtered layout shows the matches at the size of what matched, inside
/// ancestors sized the same way. Skipping lets the children a tail stood for
/// be laid out on their own.
pub fn layout_filtered(
    root: &Node,
    root_crumbs: &[usize],
    area: Rect,
    metric: Metric,
    options: &LayoutOptions,
    filter: Option<&Matches>,
    skipping: usize,
) -> Vec<Tile> {
    let mut tiles = Vec::new();
    let mut crumbs = root_crumbs.to_vec();
    let place = Placement {
        metric,
        options,
        filter,
    };
    place_children(root, area, &place, 0, skipping, &mut crumbs, &mut tiles);
    tiles
}

/// What stays the same for every level of one layout.
struct Placement<'a> {
    metric: Metric,
    options: &'a LayoutOptions,
    /// Set while the level being placed is inside a filtered region; a
    /// match clears it for everything beneath.
    filter: Option<&'a Matches>,
}

/// A child as the layout weighs it, with what it stands for in bytes and
/// files, filtered like its weight.
#[derive(Clone, Copy)]
pub(crate) struct Ranked {
    pub index: usize,
    pub value: f64,
    pub bytes: u64,
    pub files: u64,
}

/// `node`'s children the filter keeps, heaviest first. Ties break by
/// index, so a later page starts exactly where the one before it ended.
pub(crate) fn rank(
    node: &Node,
    metric: Metric,
    filter: Option<&Matches>,
    crumbs: &mut Vec<usize>,
) -> Vec<Ranked> {
    let mut ranked: Vec<Ranked> = node
        .children
        .iter()
        .enumerate()
        .filter_map(|(index, child)| {
            let keep = match filter {
                None => Keep::Whole,
                Some(filter) => {
                    crumbs.push(index);
                    let keep = filter.keep(crumbs);
                    crumbs.pop();
                    keep?
                }
            };
            let value = Matches::value(keep, child, metric);
            (value > 0).then(|| Ranked {
                index,
                value: value as f64,
                bytes: Matches::value(keep, child, Metric::Bytes),
                files: Matches::value(keep, child, Metric::Files),
            })
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .value
            .total_cmp(&left.value)
            .then(left.index.cmp(&right.index))
    });
    ranked
}

/// The merged tail standing for `rest`, as a tile of its own.
pub(crate) fn others(
    crumbs: &[usize],
    rest: &[Ranked],
    rect: Rect,
    depth: u32,
) -> Tile {
    let mut tail = crumbs.to_vec();
    tail.push(REST);
    Tile {
        kind: TileKind::Others {
            crumbs: tail,
            count: rest.len(),
            bytes: rest.iter().map(|entry| entry.bytes).sum(),
            files: rest.iter().map(|entry| entry.files).sum(),
        },
        rect,
        depth,
        header: None,
    }
}

fn place_children(
    node: &Node,
    area: Rect,
    place: &Placement<'_>,
    depth: u32,
    skipping: usize,
    crumbs: &mut Vec<usize>,
    out: &mut Vec<Tile>,
) {
    let options = place.options;
    if node.children.is_empty() || area.w <= 0.0 || area.h <= 0.0 {
        return;
    }

    // Rank by importance ourselves: the tree is already sorted, but a metric
    // switch or a hand-built tree must not produce a bad layout.
    let mut ranked = rank(node, place.metric, place.filter, crumbs);
    ranked.drain(..skipping.min(ranked.len()));
    if ranked.is_empty() {
        return;
    }

    let padding = if depth == 0 {
        options.padding_outer
    } else {
        options.padding
    };
    let squarified = |kept: usize| -> Vec<Rect> {
        let mut values: Vec<f64> =
            ranked[..kept].iter().map(|entry| entry.value).collect();
        if kept < ranked.len() {
            values.push(ranked[kept..].iter().map(|entry| entry.value).sum());
        }
        squarify(&values, area)
            .iter()
            .map(|rect| rect.inset(padding))
            .collect()
    };
    let too_small =
        |rect: &Rect| rect.w < options.min_tile || rect.h < options.min_tile;
    // A child too small to draw joins the tail, so a page that skips the
    // drawn children misses nothing. Each pass gives the tail more room, and
    // a handful settle it.
    let mut kept = ranked.len().min(options.max_children);
    let mut rects = squarified(kept);
    for _ in 0..8 {
        let Some(small) = rects[..kept].iter().position(too_small) else {
            break;
        };
        kept = small;
        rects = squarified(kept);
    }

    for (slot, rect) in rects.into_iter().enumerate() {
        if too_small(&rect) {
            continue;
        }
        if slot >= kept {
            out.push(others(crumbs, &ranked[kept..], rect, depth));
            continue;
        }
        let index = ranked[slot].index;
        let child = &node.children[index];
        let subdividable = child.is_dir() && depth + 1 < options.max_depth;
        // A directory that is about to be subdivided claims a header band for
        // its own name. When there is no room for one it stays whole: a name
        // drawn over its own children is worse than one level less of detail,
        // and zooming in gives it the room back.
        let header = subdividable
            .then(|| header_band(rect, options, depth))
            .flatten();
        crumbs.push(index);
        let tile = Tile {
            kind: TileKind::Node {
                crumbs: crumbs.clone(),
            },
            rect,
            depth,
            header,
        };
        let body = tile.body();
        out.push(tile);
        if header.is_some() {
            // Beneath a match everything is shown; above one, only matches.
            let inner = Placement {
                filter: place
                    .filter
                    .filter(|filter| filter.keep(crumbs) != Some(Keep::Whole)),
                ..*place
            };
            place_children(child, body, &inner, depth + 1, 0, crumbs, out);
        }
        crumbs.pop();
    }
}

/// The band a directory keeps for its name, or `None` when the tile is too
/// small to leave its children a usable area below it.
///
/// The top level gets the full header; deeper directories a slimmer band.
fn header_band(
    rect: Rect,
    options: &LayoutOptions,
    depth: u32,
) -> Option<Rect> {
    let height = if depth == 0 {
        options.header
    } else {
        options.header_inner
    };
    let body = rect.h - height;
    if rect.w < 44.0 || body < options.min_tile * 3.0 {
        return None;
    }
    Some(Rect::new(rect.x, rect.y, rect.w, height))
}

/// Split `area` into one rectangle per value, proportional to it.
///
/// Returned in the order of `values`, not in layout order.
pub fn squarify(values: &[f64], area: Rect) -> Vec<Rect> {
    let mut rects = vec![Rect::default(); values.len()];
    let total: f64 = values.iter().filter(|value| **value > 0.0).sum();
    if total <= 0.0 || area.w <= 0.0 || area.h <= 0.0 {
        return rects;
    }

    let mut order: Vec<usize> =
        (0..values.len()).filter(|i| values[*i] > 0.0).collect();
    order.sort_by(|left, right| values[*right].total_cmp(&values[*left]));

    let scale = f64::from(area.w) * f64::from(area.h) / total;
    let areas: Vec<f64> =
        order.iter().map(|index| values[*index] * scale).collect();

    let mut free = area;
    let mut start = 0;
    while start < areas.len() {
        let side = f64::from(free.w.min(free.h));
        let mut end = start + 1;
        let mut row_sum = areas[start];
        let mut row_worst = worst_ratio(&areas[start..end], row_sum, side);

        while end < areas.len() {
            let candidate_sum = row_sum + areas[end];
            let candidate_worst =
                worst_ratio(&areas[start..=end], candidate_sum, side);
            if candidate_worst > row_worst {
                break;
            }
            row_sum = candidate_sum;
            row_worst = candidate_worst;
            end += 1;
        }

        if free.w >= free.h {
            // A vertical strip on the left; tiles stack top to bottom.
            let strip_w = (row_sum / f64::from(free.h)) as f32;
            let strip_w = strip_w.min(free.w);
            let mut y = free.y;
            for index in start..end {
                let height = if strip_w > 0.0 {
                    (areas[index] / f64::from(strip_w)) as f32
                } else {
                    0.0
                };
                let height = height.min(free.bottom() - y).max(0.0);
                rects[order[index]] = Rect::new(free.x, y, strip_w, height);
                y += height;
            }
            free.x += strip_w;
            free.w -= strip_w;
        } else {
            // A horizontal strip along the top; tiles run left to right.
            let strip_h = (row_sum / f64::from(free.w)) as f32;
            let strip_h = strip_h.min(free.h);
            let mut x = free.x;
            for index in start..end {
                let width = if strip_h > 0.0 {
                    (areas[index] / f64::from(strip_h)) as f32
                } else {
                    0.0
                };
                let width = width.min(free.right() - x).max(0.0);
                rects[order[index]] = Rect::new(x, free.y, width, strip_h);
                x += width;
            }
            free.y += strip_h;
            free.h -= strip_h;
        }
        start = end;
    }
    rects
}

/// Worst (largest) aspect ratio in a row of `areas` laid along `side`.
fn worst_ratio(areas: &[f64], row_sum: f64, side: f64) -> f64 {
    if row_sum <= 0.0 || side <= 0.0 {
        return f64::INFINITY;
    }
    let thickness = row_sum / side;
    areas.iter().fold(0.0_f64, |worst, area| {
        if *area <= 0.0 || thickness <= 0.0 {
            return worst;
        }
        let other = area / thickness;
        let ratio = (thickness / other).max(other / thickness);
        worst.max(ratio)
    })
}

/// The deepest tile containing a point.
///
/// Children are emitted after their parent and are inset inside it, so the last
/// match in reverse order is the most specific one.
pub fn hit(tiles: &[Tile], x: f32, y: f32) -> Option<&Tile> {
    tiles.iter().rev().find(|tile| tile.rect.contains(x, y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::NodeKind;

    fn file(name: &str, bytes: u64) -> Node {
        Node::entry(name, NodeKind::File, bytes)
    }

    /// A directory whose direct contents are the given children, so aggregate
    /// totals are consistent before layout runs.
    fn dir(name: &str, children: Vec<Node>) -> Node {
        let mut node = Node::directory(name);
        node.children = children;
        crate::tree::aggregate(&mut node, Metric::Bytes);
        node
    }

    fn area() -> Rect {
        Rect::new(0.0, 0.0, 800.0, 500.0)
    }

    #[test]
    fn squarify_fills_the_area() {
        let values = [40.0, 30.0, 20.0, 5.0, 3.0, 2.0];
        let rects = squarify(&values, area());
        let covered: f32 = rects.iter().map(Rect::area).sum();
        assert!(
            (covered - area().area()).abs() < 1.0,
            "covered {covered} of {}",
            area().area()
        );
        for rect in &rects {
            assert!(rect.x >= -0.01 && rect.y >= -0.01);
            assert!(rect.right() <= area().right() + 0.01);
            assert!(rect.bottom() <= area().bottom() + 0.01);
        }
    }

    #[test]
    fn squarify_keeps_aspect_ratios_reasonable() {
        // Same distribution as the canonical squarify example: 6, 6, 4, 3, 2, 2,
        // 1 on a 6x4 canvas.
        let values = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let rects = squarify(&values, Rect::new(0.0, 0.0, 600.0, 400.0));
        for rect in &rects {
            let ratio = (rect.w / rect.h).max(rect.h / rect.w);
            assert!(ratio <= 4.0, "aspect ratio {ratio} in {rect:?}");
        }
    }

    #[test]
    fn squarify_survives_degenerate_input() {
        assert_eq!(squarify(&[], area()).len(), 0);
        assert!(
            squarify(&[0.0, 0.0], area())
                .iter()
                .all(|r| r.area().abs() < f32::EPSILON)
        );
        assert!(
            squarify(&[1.0], Rect::new(0.0, 0.0, 0.0, 10.0))[0]
                .area()
                .abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn layout_nests_children_inside_their_parent() {
        // Depth first, parent before its own children: that order is what lets
        // the view paint in sequence and hit-test in reverse.
        let root = dir(
            "root",
            vec![
                dir("big", vec![file("inside", 100), file("also", 50)]),
                file("small", 10),
            ],
        );

        let tiles = layout(
            &root,
            &[],
            area(),
            Metric::Bytes,
            &LayoutOptions::default(),
        );
        let crumbs: Vec<&[usize]> = tiles.iter().map(Tile::crumbs).collect();
        assert_eq!(crumbs, vec![&[0][..], &[0, 0], &[0, 1], &[1]]);

        let parent = &tiles[0];
        let child = &tiles[1];
        assert_eq!(child.depth, 1);
        assert!(parent.rect.contains(child.rect.x, child.rect.y));
        assert!(
            parent
                .rect
                .contains(child.rect.right() - 0.5, child.rect.bottom() - 0.5)
        );
    }

    #[test]
    fn layout_depth_one_stops_at_direct_children() {
        let root = dir("root", vec![dir("big", vec![file("inside", 100)])]);

        let options = LayoutOptions {
            max_depth: 1,
            ..LayoutOptions::default()
        };
        let tiles = layout(&root, &[], area(), Metric::Bytes, &options);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].crumbs(), &[0]);
    }

    #[test]
    fn a_subdivided_directory_keeps_a_header_for_its_own_name() {
        let root = dir(
            "root",
            vec![dir("big", vec![file("inside", 100), file("also", 50)])],
        );
        let tiles = layout(
            &root,
            &[],
            area(),
            Metric::Bytes,
            &LayoutOptions::default(),
        );

        let parent = tiles
            .iter()
            .find(|tile| tile.crumbs() == [0])
            .expect("the parent tile");
        let header = parent.header.expect("a parent keeps a header");
        assert!(header.w > 0.0 && header.h > 0.0);
        assert!(
            (header.y - parent.rect.y).abs() < f32::EPSILON,
            "the band sits at the top of its tile"
        );
        assert!(header.bottom() <= parent.rect.bottom());

        // Every descendant starts below the band, so no child is drawn under
        // the parent's name.
        for tile in tiles.iter().filter(|tile| tile.crumbs().len() > 1) {
            assert!(
                tile.rect.y >= header.bottom() - f32::EPSILON,
                "{:?} overlaps the header",
                tile.crumbs()
            );
        }

        // And the band belongs to the parent for hit-testing.
        let centre = (header.x + header.w / 2.0, header.y + header.h / 2.0);
        let hit_tile = hit(&tiles, centre.0, centre.1).expect("a hit");
        assert_eq!(hit_tile.crumbs(), &[0]);
    }

    #[test]
    fn a_tile_without_room_for_a_header_stays_whole() {
        let root = dir("root", vec![dir("big", vec![file("inside", 100)])]);
        let options = LayoutOptions {
            // A band this tall leaves no usable body under it.
            header: 400.0,
            min_tile: 150.0,
            ..LayoutOptions::default()
        };
        let tiles = layout(&root, &[], area(), Metric::Bytes, &options);
        assert_eq!(tiles.len(), 1, "the child was not subdivided");
        assert!(tiles[0].header.is_none(), "and it has no band of its own");
    }

    #[test]
    fn tile_crumbs_address_the_scanned_tree_not_the_drawn_node() {
        // Draw the node at [3, 1] of some larger tree: its tiles must be
        // [3, 1, …], or anything resolving them from the scanned root would
        // find a stranger.
        let node = dir("inner", vec![file("a", 10), file("b", 5)]);
        let tiles = layout(
            &node,
            &[3, 1],
            area(),
            Metric::Bytes,
            &LayoutOptions::default(),
        );
        assert!(!tiles.is_empty());
        for tile in &tiles {
            assert!(tile.crumbs().starts_with(&[3, 1]), "{:?}", tile.crumbs());
            assert_eq!(tile.crumbs().len(), 3);
        }
    }

    #[test]
    fn a_leaf_never_claims_a_header() {
        let root = dir("root", vec![file("solo", 10)]);
        let tiles = layout(
            &root,
            &[],
            area(),
            Metric::Bytes,
            &LayoutOptions::default(),
        );
        assert!(tiles.iter().all(|tile| tile.header.is_none()));
    }

    #[test]
    fn layout_merges_the_child_tail_into_one_tile() {
        let children: Vec<Node> = (0..10)
            .map(|index| file(&format!("f{index}"), 10 - index))
            .collect();
        let root = dir("root", children);
        let options = LayoutOptions {
            max_children: 4,
            ..LayoutOptions::default()
        };
        let tiles = layout(&root, &[], area(), Metric::Bytes, &options);
        let others: Vec<&Tile> = tiles
            .iter()
            .filter(|tile| matches!(tile.kind, TileKind::Others { .. }))
            .collect();
        assert_eq!(others.len(), 1);
        assert!(matches!(others[0].kind, TileKind::Others { count: 6, .. }));
    }

    #[test]
    fn the_tail_names_no_node_and_totals_what_it_stands_for() {
        let children: Vec<Node> = (0..10)
            .map(|index| file(&format!("f{index}"), 10 - index))
            .collect();
        let root = dir("root", children);
        let options = LayoutOptions {
            max_children: 4,
            ..LayoutOptions::default()
        };
        let tiles = layout(&root, &[7], area(), Metric::Bytes, &options);
        let tail = tiles
            .iter()
            .find(|tile| is_rest(tile.crumbs()))
            .expect("a tail");
        assert_eq!(tail.crumbs(), &[7, REST]);
        assert!(root.resolve(&tail.crumbs()[1..]).is_none());
        // f4..f9 weigh 6, 5, 4, 3, 2 and 1.
        assert!(matches!(
            tail.kind,
            TileKind::Others {
                count: 6,
                bytes: 21,
                files: 6,
                ..
            }
        ));
    }

    #[test]
    fn children_too_small_to_draw_join_the_tail() {
        // One large file and many tiny ones: the tiny ones cannot be drawn,
        // and every one of them is counted in the tail rather than lost.
        let mut children = vec![file("big", 100_000)];
        children.extend((0..150).map(|index| file(&format!("s{index}"), 20)));
        let root = dir("root", children);
        let tiles = layout(
            &root,
            &[],
            area(),
            Metric::Bytes,
            &LayoutOptions::default(),
        );
        let drawn = tiles
            .iter()
            .filter(|tile| tile.depth == 0 && !is_rest(tile.crumbs()))
            .count();
        let hidden = tiles
            .iter()
            .find_map(|tile| match tile.kind {
                TileKind::Others { count, .. } => Some(count),
                TileKind::Node { .. } => None,
            })
            .expect("a tail");
        assert_eq!(drawn + hidden, 151);
        assert!(hidden > 100, "{hidden} in the tail");
    }

    #[test]
    fn skipping_lays_out_what_the_tail_stood_for() {
        let root =
            dir("root", vec![file("big", 900), file("a", 60), file("b", 40)]);
        let options = LayoutOptions {
            padding: 0.0,
            padding_outer: 0.0,
            ..LayoutOptions::default()
        };
        let tiles = layout_filtered(
            &root,
            &[],
            area(),
            Metric::Bytes,
            &options,
            None,
            1,
        );
        let crumbs: Vec<&[usize]> = tiles.iter().map(Tile::crumbs).collect();
        assert_eq!(crumbs, vec![&[1][..], &[2]]);
        // The page's own total is the whole area.
        let share = tiles[0].rect.area() / area().area();
        assert!((share - 0.6).abs() < 0.01, "share {share}");
    }

    #[test]
    fn a_body_is_the_tile_below_its_band() {
        let root = dir(
            "root",
            vec![dir("big", vec![file("inside", 100), file("also", 50)])],
        );
        let tiles = layout(
            &root,
            &[],
            area(),
            Metric::Bytes,
            &LayoutOptions::default(),
        );
        let parent = &tiles[0];
        let header = parent.header.expect("a band");
        let body = parent.body();
        assert!((body.y - header.bottom()).abs() < f32::EPSILON);
        assert!((body.bottom() - parent.rect.bottom()).abs() < 0.001);
        assert!((tiles[1].body().area() - tiles[1].rect.area()).abs() < 0.001);
    }

    #[test]
    fn hit_returns_the_deepest_tile() {
        let root = dir(
            "root",
            vec![dir("big", vec![file("inside", 100)]), file("small", 1)],
        );

        let tiles = layout(
            &root,
            &[],
            area(),
            Metric::Bytes,
            &LayoutOptions::default(),
        );
        let child = tiles
            .iter()
            .find(|tile| tile.crumbs() == [0, 0])
            .expect("child tile");
        let centre = (
            child.rect.x + child.rect.w / 2.0,
            child.rect.y + child.rect.h / 2.0,
        );
        let found = hit(&tiles, centre.0, centre.1).expect("a hit");
        assert_eq!(found.crumbs(), &[0, 0]);
        assert!(hit(&tiles, -50.0, -50.0).is_none());
    }

    #[test]
    fn a_filtered_layout_shows_only_the_matches_at_their_size() {
        use crate::filter::filter;
        use crate::tree::{NodeKind, aggregate};

        let file = |name: &str, bytes| Node::entry(name, NodeKind::File, bytes);
        let mut src = Node::directory("src");
        src.children = vec![file("big_test.rs", 300), file("other.rs", 700)];
        let mut root = Node::directory("root");
        root.children =
            vec![src, file("unit_test.txt", 100), file("huge.iso", 5000)];
        aggregate(&mut root, Metric::Bytes);

        let matches = filter(&root, &[], "test").expect("needle");
        let area = Rect::new(0.0, 0.0, 400.0, 400.0);
        let options = LayoutOptions {
            padding: 0.0,
            padding_outer: 0.0,
            ..LayoutOptions::default()
        };
        let tiles = layout_filtered(
            &root,
            &[],
            area,
            Metric::Bytes,
            &options,
            Some(&matches),
            0,
        );
        let names: Vec<String> = tiles
            .iter()
            .filter_map(|tile| root.resolve(tile.crumbs()))
            .map(|node| node.name.to_string())
            .collect();
        assert!(names.contains(&"big_test.rs".to_string()));
        assert!(names.contains(&"unit_test.txt".to_string()));
        assert!(
            !names
                .iter()
                .any(|name| name == "huge.iso" || name == "other.rs")
        );

        // src is sized by its 300 matched bytes, not its 1000: three times
        // the 100-byte match beside it.
        let area_of = |name: &str| {
            tiles
                .iter()
                .find(|tile| {
                    root.resolve(tile.crumbs())
                        .is_some_and(|node| &*node.name == name)
                })
                .map(|tile| tile.rect.w * tile.rect.h)
                .expect("drawn")
        };
        let ratio = area_of("src") / area_of("unit_test.txt");
        assert!((ratio - 3.0).abs() < 0.05, "ratio {ratio}");
    }
}
