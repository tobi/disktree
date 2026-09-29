//! Sunburst and icicle layout.
//!
//! Both charts are the same partition of the tree, drawn two ways: each
//! level is a band, and each child takes the share of its parent's span that
//! it takes of its parent's size. Layout runs in unit space: a tile's `x` is
//! where its share starts along the band, `w` the share, `y` its level and
//! `h` one level. Where that lands on screen — rings around a hole, or
//! columns — is the view's business, so hit-testing and painting share one
//! mapping and cannot disagree.

use crate::filter::{Keep, Matches};
use crate::tree::{Metric, Node};
use crate::treemap::{Rect, Tile, TileKind, others, rank};

/// How much of the tree a partition draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    pub metric: Metric,
    /// Levels drawn at once; 1 draws only the root's children.
    pub max_depth: u32,
    /// Children kept per directory. The tail merges into one `Others` tile.
    pub max_children: usize,
}

/// Lay out everything beneath `root` as a partition, in unit space.
///
/// `root_crumbs` is where `root` sits in the scanned tree, as for
/// [`crate::treemap::layout`]. `min_span` is the narrowest share worth
/// drawing at each level; narrower children join their level's tail, so a
/// page that skips the drawn children misses nothing. `skipping` leaves out
/// the root's largest children, as the treemap's does.
pub fn partition(
    root: &Node,
    root_crumbs: &[usize],
    shape: Partition,
    min_span: impl Fn(u32) -> f32,
    filter: Option<&Matches>,
    skipping: usize,
) -> Vec<Tile> {
    let narrowest: Vec<f32> = (0..shape.max_depth).map(min_span).collect();
    let mut tiles = Vec::new();
    let mut crumbs = root_crumbs.to_vec();
    let place = Place {
        shape,
        narrowest: &narrowest,
    };
    place.children(
        root,
        0.0,
        1.0,
        0,
        filter,
        skipping,
        &mut crumbs,
        &mut tiles,
    );
    tiles
}

struct Place<'a> {
    shape: Partition,
    narrowest: &'a [f32],
}

impl Place<'_> {
    #[expect(
        clippy::too_many_arguments,
        reason = "one level of a recursive walk; a struct per call would only \
                  rename these"
    )]
    fn children(
        &self,
        node: &Node,
        start: f32,
        width: f32,
        depth: u32,
        filter: Option<&Matches>,
        skipping: usize,
        crumbs: &mut Vec<usize>,
        out: &mut Vec<Tile>,
    ) {
        if node.children.is_empty()
            || width <= 0.0
            || depth >= self.shape.max_depth
        {
            return;
        }
        let mut ranked = rank(node, self.shape.metric, filter, crumbs);
        ranked.drain(..skipping.min(ranked.len()));
        let total: f64 = ranked.iter().map(|entry| entry.value).sum();
        if total <= 0.0 {
            return;
        }
        let share = |value: f64| (f64::from(width) * value / total) as f32;
        let narrowest = self.narrowest[depth as usize];
        let kept = ranked
            .iter()
            .take(self.shape.max_children)
            .take_while(|entry| share(entry.value) >= narrowest)
            .count();

        let mut x = start;
        for entry in &ranked[..kept] {
            let span = share(entry.value);
            let child = &node.children[entry.index];
            crumbs.push(entry.index);
            out.push(Tile {
                kind: TileKind::Node {
                    crumbs: crumbs.clone(),
                },
                rect: Rect::new(x, depth as f32, span, 1.0),
                depth,
                header: None,
            });
            if child.is_dir() {
                // Beneath a match everything is shown; above one, only
                // matches.
                let inner = filter
                    .filter(|filter| filter.keep(crumbs) != Some(Keep::Whole));
                self.children(child, x, span, depth + 1, inner, 0, crumbs, out);
            }
            crumbs.pop();
            x += span;
        }

        let tail = &ranked[kept..];
        let span = share(tail.iter().map(|entry| entry.value).sum());
        if !tail.is_empty() && span >= narrowest {
            out.push(others(
                crumbs,
                tail,
                Rect::new(x, depth as f32, span, 1.0),
                depth,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{NodeKind, aggregate};
    use crate::treemap::{REST, is_rest};

    fn file(name: &str, bytes: u64) -> Node {
        Node::entry(name, NodeKind::File, bytes)
    }

    fn dir(name: &str, children: Vec<Node>) -> Node {
        let mut node = Node::directory(name);
        node.children = children;
        aggregate(&mut node, Metric::Bytes);
        node
    }

    fn shape(max_depth: u32) -> Partition {
        Partition {
            metric: Metric::Bytes,
            max_depth,
            max_children: 96,
        }
    }

    fn close(left: f32, right: f32) -> bool {
        (left - right).abs() < 1e-5
    }

    #[test]
    fn children_split_their_parents_span_by_size() {
        let root = dir(
            "root",
            vec![
                dir("big", vec![file("a", 60), file("b", 20)]),
                file("mid", 15),
                file("small", 5),
            ],
        );
        let tiles = partition(&root, &[], shape(3), |_| 0.0, None, 0);
        let crumbs: Vec<&[usize]> = tiles.iter().map(Tile::crumbs).collect();
        assert_eq!(
            crumbs,
            vec![&[0][..], &[0, 0], &[0, 1], &[1], &[2]],
            "parent before its children"
        );
        let big = tiles[0].rect;
        assert!(close(big.w, 0.8) && close(big.x, 0.0) && close(big.y, 0.0));
        let (a, b) = (tiles[1].rect, tiles[2].rect);
        assert!(close(a.y, 1.0) && close(b.y, 1.0));
        assert!(close(a.w, 0.6) && close(b.x, 0.6));
        assert!(close(b.right(), big.right()));
        assert!(close(tiles[3].rect.x, 0.8));
    }

    #[test]
    fn too_narrow_children_join_the_tail() {
        let root = dir(
            "root",
            vec![
                file("big", 900),
                file("a", 40),
                file("b", 30),
                file("c", 20),
                file("d", 10),
            ],
        );
        let tiles = partition(&root, &[], shape(1), |_| 0.05, None, 0);
        let crumbs: Vec<&[usize]> = tiles.iter().map(Tile::crumbs).collect();
        assert_eq!(crumbs, vec![&[0][..], &[REST]]);
        assert!(matches!(
            tiles[1].kind,
            TileKind::Others {
                count: 4,
                bytes: 100,
                files: 4,
                ..
            }
        ));
        assert!(
            close(tiles[1].rect.w, 0.1) && close(tiles[1].rect.right(), 1.0)
        );

        let none = partition(&root, &[], shape(1), |_| 0.2, None, 0);
        assert!(
            none.iter().all(|tile| !is_rest(tile.crumbs())),
            "a tail too narrow to draw is left out"
        );
    }

    #[test]
    fn max_depth_stops_the_levels() {
        let root =
            dir("root", vec![dir("a", vec![dir("b", vec![file("c", 1)])])]);
        let one = partition(&root, &[], shape(1), |_| 0.0, None, 0);
        assert_eq!(one.len(), 1);
        let three = partition(&root, &[], shape(3), |_| 0.0, None, 0);
        let depths: Vec<u32> = three.iter().map(|tile| tile.depth).collect();
        assert_eq!(depths, vec![0, 1, 2]);
    }

    #[test]
    fn crumbs_resolve_from_the_scanned_root() {
        let root = dir(
            "root",
            vec![dir(
                "inner",
                vec![dir("deep", vec![file("x", 7), file("y", 3)])],
            )],
        );
        let inner = root.resolve(&[0]).expect("inner");
        let tiles = partition(inner, &[0], shape(2), |_| 0.0, None, 0);
        let crumbs: Vec<&[usize]> = tiles.iter().map(Tile::crumbs).collect();
        assert_eq!(crumbs, vec![&[0, 0][..], &[0, 0, 0], &[0, 0, 1]]);
        assert_eq!(root.resolve(&[0, 0, 1]).map(|n| &*n.name), Some("y"));
    }

    #[test]
    fn skipping_lays_out_what_the_tail_stood_for() {
        let root =
            dir("root", vec![file("big", 900), file("a", 60), file("b", 40)]);
        let tiles = partition(&root, &[], shape(1), |_| 0.0, None, 1);
        let crumbs: Vec<&[usize]> = tiles.iter().map(Tile::crumbs).collect();
        assert_eq!(crumbs, vec![&[1][..], &[2]]);
        assert!(
            close(tiles[0].rect.w, 0.6),
            "the page's own total is the whole"
        );
    }
}
