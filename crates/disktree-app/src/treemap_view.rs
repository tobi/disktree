//! The mosaic: tiles painted on a canvas, labels shaped straight into it.
//!
//! Tiles are painted rather than composed from elements. A treemap can put
//! thousands of rectangles on screen, and an element per rectangle would spend
//! the frame in layout. Painting also means the marked hatch, the selection
//! ring and the hover outline are drawn in one place, in one order.
//!
//! Text is shaped here too. GPUI caches shaped lines, so re-shaping the visible
//! labels every frame costs a lookup, and it lets a label clip exactly to its
//! own tile instead of bleeding into the neighbour.

use std::rc::Rc;

use disktree_core::treemap::Rect;
use gpui_kit::{
    App, Bounds, ContentMask, Context, Corners, Edges, Font, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, Pixels, Point, ScrollWheelEvent, SharedString, Size,
    StatefulInteractiveElement as _, StrikethroughStyle, Styled, TextAlign,
    TextRun, Window, canvas, div, pattern_slash, px, quad, underline_y_offset,
};
use gpui_omarchy::{ActiveTheme, Theme};

use disktree_core::classify::Category;

use crate::palette;
use crate::state::{Disktree, Filtered, Label, View};

/// How one tile should be drawn, resolved before the paint callback runs so
/// that painting never has to look anything up.
#[derive(Clone, Debug)]
pub struct TileDeco {
    /// Base-space rectangle: the view transform is applied while painting.
    pub rect: Rect,
    /// Nesting depth in this view; `0` is the first level.
    pub depth: u32,
    /// What kind of data it is: the hue.
    pub category: Category,
    /// In age mode, which [`palette::AGE_BUCKETS`] entry it falls in.
    pub age_bucket: Option<usize>,
    /// Its space can be had back: hatched.
    pub reclaimable: bool,
    /// How it stands against the find text.
    pub filtered: Filtered,
    /// Part of it could not be read: flagged in its corner.
    pub unreadable: bool,
    pub marked: bool,
    /// Inside another marked directory, so it goes with its parent.
    pub covered: bool,
    pub hovered: bool,
    pub selected: bool,
}

/// Everything the mosaic needs for one frame.
#[derive(Clone, Debug, Default)]
pub struct Mosaic {
    pub tiles: Vec<TileDeco>,
    pub labels: Vec<Label>,
    pub view: View,
}

/// Build the treemap viewport: canvas, input, and the cursor tooltip.
pub fn mosaic(
    mosaic: Mosaic,
    app: &Disktree,
    window: &Window,
    cx: &Context<'_, Disktree>,
) -> impl IntoElement {
    mosaic_inner(mosaic, app, window, cx, true)
}

/// A coarse, explicitly non-actionable picture of measured contributions.
pub fn preview(
    preview: &disktree_core::scan_preview::Preview,
    app: &Disktree,
    window: &Window,
    cx: &Context<'_, Disktree>,
) -> impl IntoElement {
    use disktree_core::tree::{Facts, NodeKind, Tree};
    use disktree_core::treemap::{LayoutOptions, layout};
    // These are display buckets, never exposed through state.tree/path_at.
    let root = Tree::build(|builder| {
        let buckets = preview
            .entries
            .iter()
            .map(|entry| {
                let facts = Facts {
                    files: entry.files,
                    ..Facts::entry(NodeKind::File, entry.bytes)
                };
                builder.leaf(entry.name.as_str(), facts, None)
            })
            .collect();
        builder.directory("preview", Facts::directory(), None, buckets)
    });
    let area = app.treemap_size.get();
    let tiles = layout(
        root.root(),
        &[],
        Rect::new(0., 0., area.width.as_f32(), area.height.as_f32()),
        app.options.metric,
        &LayoutOptions {
            max_depth: 1,
            max_children: 129,
            ..LayoutOptions::default()
        },
    );
    let mut mosaic = Mosaic::default();
    for tile in tiles {
        let Some(node) = root.root().resolve(tile.crumbs()) else {
            continue;
        };
        let decoration = TileDeco {
            rect: tile.rect,
            depth: 0,
            category: Category::Other,
            age_bucket: None,
            reclaimable: false,
            filtered: Filtered::Shown,
            unreadable: false,
            marked: false,
            covered: false,
            hovered: false,
            selected: false,
        };
        mosaic.labels.push(Label {
            text: node.name().to_string(),
            found: None,
            rect: tile.rect,
            header: None,
            depth: 0,
            dim: false,
            marked: false,
            size_text: crate::widgets::short_value(node, app.options.metric),
            decoration: decoration.clone(),
        });
        mosaic.tiles.push(decoration);
    }
    mosaic_inner(mosaic, app, window, cx, false)
}

fn mosaic_inner(
    mosaic: Mosaic,
    app: &Disktree,
    window: &Window,
    cx: &Context<'_, Disktree>,
    interactive: bool,
) -> impl IntoElement {
    let theme = cx.omarchy().clone();
    let rem = window.rem_size();
    let origin = Rc::clone(&app.treemap_origin);
    let measured = Rc::clone(&app.treemap_size);

    let colors = Colors::new(&theme);
    let font = theme.font.clone();
    let name_size = rem_px(0.75, rem);
    let size_size = rem_px(0.6875, rem);

    let Mosaic {
        tiles,
        labels,
        view,
    } = mosaic;
    let canvas_origin = Rc::clone(&origin);
    let canvas_measured = Rc::clone(&measured);

    div()
        .id("disktree-treemap")
        .debug_selector(move || if interactive { "treemap" } else { "scan-preview" }.into())
        .relative()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_hidden()
        .bg(theme.inset)
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if interactive && !hovered {
                this.on_mouse_leave(cx);
            }
        }))
        .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
            if interactive { this.on_mouse_move(event, cx); }
        }))
        // One listener for every button, because the interesting ones are not
        // the three the platform names: buttons 8 and 9 arrive as
        // `MouseButton::Navigate`, and a per-button registration would have
        // to be repeated for each.
        .on_any_mouse_down(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
            if interactive { this.on_mouse_down(event, cx); }
        }))
        .on_scroll_wheel(cx.listener(
            move |this, event: &ScrollWheelEvent, _, cx| {
                if interactive { this.on_scroll_wheel(event, cx); }
            },
        ))
        .child(
            canvas(
                move |bounds, window, _| {
                    canvas_origin.set(bounds.origin);
                    // The layout was computed from the previous frame's size. Ask
                    // for one more frame whenever the area is not what we assumed,
                    // which is what makes the first paint and a resize settle.
                    if canvas_measured.get() != bounds.size {
                        canvas_measured.set(bounds.size);
                        window.request_animation_frame();
                    }
                    bounds.size
                },
                move |bounds, _, window, cx| {
                    paint_tiles(&tiles, bounds, view, &colors, window);
                    paint_labels(
                        &labels, bounds, view, &colors, &font, name_size,
                        size_size, window, cx,
                    );
                },
            )
            .absolute()
            .inset_0(),
        )
}

/// Theme colours resolved once per frame.
struct Colors {
    label: [Hsla; 2],
    label_dim: Hsla,
    hover_border: Hsla,
    selected_border: Hsla,
    marked_border: Hsla,
    marked_label: Hsla,
    warning: Hsla,
    hatch: Hsla,
    /// Fills per category, then per depth.
    fill: Vec<[Hsla; DEPTHS]>,
    /// The strip over a top-level directory, per category.
    strip: Vec<Hsla>,
    /// Fills per age bucket, then per depth.
    age: Vec<[Hsla; DEPTHS]>,
    marked_fill: Hsla,
    /// The surface a filtered-out fill steps back toward.
    inset: Hsla,
}

/// Depth steps a fill distinguishes; deeper clamps.
const DEPTHS: usize = 5;

/// Every category, in the order [`category_index`] numbers them.
const CATEGORIES: [Category; 11] = [
    Category::Code,
    Category::AgentScratch,
    Category::Toolchain,
    Category::Synced,
    Category::Git,
    Category::Media,
    Category::Documents,
    Category::Cache,
    Category::FilesystemEvents,
    Category::Spotlight,
    Category::Other,
];

fn category_index(category: Category) -> usize {
    CATEGORIES
        .iter()
        .position(|&known| known == category)
        .unwrap_or(CATEGORIES.len() - 1)
}

impl Colors {
    fn new(theme: &Theme) -> Self {
        let ladder = |fill: &dyn Fn(u32) -> Hsla| {
            std::array::from_fn(|depth| fill(depth as u32))
        };
        Self {
            label: [
                palette::label_color(theme, 0),
                palette::label_color(theme, 1),
            ],
            label_dim: theme.secondary,
            hover_border: theme.bright.opacity(0.55),
            selected_border: palette::highlight(theme),
            marked_border: theme.danger,
            marked_label: theme.danger,
            warning: theme.warning,
            hatch: palette::hatch(theme),
            fill: CATEGORIES
                .iter()
                .map(|&category| {
                    ladder(&|depth| {
                        palette::category_fill(theme, category, depth)
                    })
                })
                .collect(),
            strip: CATEGORIES
                .iter()
                .map(|&category| palette::category_accent(theme, category))
                .collect(),
            age: (0..palette::AGE_BUCKETS.len())
                .map(|bucket| {
                    ladder(&|depth| palette::age_fill(theme, bucket, depth))
                })
                .collect(),
            marked_fill: palette::mix(theme.inset, theme.danger, 0.16),
            inset: theme.inset,
        }
    }

    fn fill(&self, tile: &TileDeco) -> Hsla {
        // Marked, or inside something marked: it all goes together.
        if tile.marked || tile.covered {
            return self.marked_fill;
        }
        let depth = (tile.depth as usize).min(DEPTHS - 1);
        let fill = match tile.age_bucket {
            Some(bucket) => self.age[bucket.min(self.age.len() - 1)][depth],
            None => self.fill[category_index(tile.category)][depth],
        };
        // Only what matches keeps its colour; a directory holding matches
        // steps back less, so the way to them stays readable.
        match tile.filtered {
            Filtered::Shown => fill,
            Filtered::Holds => palette::mix(fill, self.inset, 0.55),
            Filtered::Out => palette::mix(fill, self.inset, 0.82),
        }
    }

    fn label_inks(
        &self,
        tile: &TileDeco,
        dim: bool,
        marked: bool,
    ) -> (Hsla, Hsla, Hsla) {
        let fill = self.fill(tile);
        let preferred = if marked {
            self.marked_label
        } else if dim {
            self.label_dim
        } else {
            self.label(tile.depth)
        };
        (
            fill,
            palette::ink_on(preferred, fill, 4.5),
            palette::ink_on(self.label_dim, fill, 4.5),
        )
    }

    const fn label(&self, depth: u32) -> Hsla {
        self.label[if depth == 0 { 0 } else { 1 }]
    }
}

fn paint_tiles(
    tiles: &[TileDeco],
    bounds: Bounds<Pixels>,
    view: View,
    colors: &Colors,
    window: &mut Window,
) {
    let none = Edges::all(px(0.));
    let solid = gpui_kit::BorderStyle::Solid;
    let scale = window.scale_factor();
    // Outlines are drawn after every fill: a directory's children paint over
    // its body, and would otherwise cover its selection ring, leaving only
    // slivers of it showing in the gaps between them.
    let mut outlines: Vec<(u8, Bounds<Pixels>, f32, Hsla)> = Vec::new();
    for tile in tiles {
        let fill = colors.fill(tile);
        let rect = view.project(tile.rect);
        if rect.w <= 0.5 || rect.h <= 0.5 {
            continue;
        }
        let quad_bounds = snap(to_window(&rect, bounds), scale);

        // No border by default: the gaps between tiles, wider between the
        // top-level directories, are what separate them.
        window.paint_quad(quad(
            quad_bounds,
            Corners::default(),
            fill,
            none,
            colors.hover_border,
            solid,
        ));

        // Reclaimable space is hatched, over any hue: the hatch answers
        // "can it go", the colour "what is it". Everything inside a
        // reclaimable directory is reclaimable too, so only the outermost
        // one needs painting; its children repaint their own fill and hatch.
        if tile.reclaimable
            && !tile.marked
            && !tile.covered
            && tile.filtered == Filtered::Shown
        {
            window.paint_quad(quad(
                quad_bounds,
                Corners::default(),
                pattern_slash(colors.hatch, 1.0, 6.0),
                none,
                colors.hatch,
                solid,
            ));
        }

        // A top-level directory carries a thin strip of its colour, so the
        // first level of structure reads before any detail.
        if tile.depth == 0
            && tile.age_bucket.is_none()
            && tile.filtered != Filtered::Out
        {
            let strip = Bounds::new(
                quad_bounds.origin,
                Size::new(
                    quad_bounds.size.width,
                    px(2.0_f32.min(quad_bounds.size.height.as_f32())),
                ),
            );
            window.paint_quad(quad(
                strip,
                Corners::default(),
                colors.strip[category_index(tile.category)],
                none,
                colors.hover_border,
                solid,
            ));
        }

        if tile.unreadable && rect.w > 12.0 && rect.h > 12.0 {
            // A small warning corner: part of this was never measured.
            let mark = Bounds::new(
                Point::new(
                    quad_bounds.origin.x + quad_bounds.size.width - px(6.),
                    quad_bounds.origin.y + px(2.),
                ),
                Size::new(px(4.), px(4.)),
            );
            let warning = palette::boundary_on(colors.warning, fill, 3.0);
            window.paint_quad(quad(
                mark,
                Corners::default(),
                warning,
                none,
                warning,
                solid,
            ));
        }

        // Ranked so the most important ring is painted last, on top.
        let outline = if tile.selected {
            Some((3, 3.0, colors.selected_border))
        } else if tile.hovered {
            Some((2, 1.0, colors.hover_border))
        } else if tile.marked {
            Some((1, 2.0, colors.marked_border))
        } else {
            None
        }
        .map(|(rank, width, preferred)| {
            (rank, width, palette::boundary_on(preferred, fill, 3.0))
        });
        if let Some((rank, width, color)) = outline {
            outlines.push((rank, quad_bounds, width, color));
        }
    }
    outlines.sort_by_key(|(rank, ..)| *rank);
    for (_, ring, width, color) in outlines {
        window.paint_quad(quad(
            ring,
            Corners::default(),
            gpui_kit::transparent_black(),
            Edges::all(px(width)),
            color,
            solid,
        ));
    }
}

/// Round a rectangle's edges to device pixels. Tiles land on fractional
/// positions, and a 2 px strip or ring there smears across a pixel row and
/// leaves a seam; rounding each edge, not the size, keeps neighbours flush.
fn snap(bounds: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
    let round = |value: Pixels| px((value.as_f32() * scale).round() / scale);
    let left = round(bounds.origin.x);
    let top = round(bounds.origin.y);
    let right = round(bounds.origin.x + bounds.size.width);
    let bottom = round(bounds.origin.y + bounds.size.height);
    Bounds::new(
        Point::new(left, top),
        Size::new((right - left).max(px(0.)), (bottom - top).max(px(0.))),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the paint pass threads window state; a struct would only move
              these fields somewhere else"
)]
fn paint_labels(
    labels: &[Label],
    bounds: Bounds<Pixels>,
    view: View,
    colors: &Colors,
    font: &SharedString,
    name_size: Pixels,
    size_size: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let font = Font {
        family: font.clone(),
        ..Font::default()
    };
    let name_line_height = name_size * 1.35;
    let text_system = window.text_system().clone();
    // Label geometry is proportioned to the label's own type size, which
    // already follows `rem`: padding, thresholds and gaps then keep their
    // relationship to the text at every interface zoom step.
    let text_padding = name_size * 0.42;
    let text_inset = name_size * 0.25;
    let min_width = name_size * 3.3;
    let size_gap = name_size * 0.67;
    // Strike and underline are read as state, so they stay visible at the
    // smallest interface zoom rather than thinning to a hairline.
    let decoration = (name_size * 0.08).max(px(1.));

    for label in labels {
        // A subdivided directory's name lives in the band it reserved; a leaf's
        // sits at the top of its own tile. Either way the mask is the region
        // the label owns, so no label can reach into another tile.
        let owned = label.header.unwrap_or(label.rect);
        let rect = view.project(owned);
        if px(rect.w) < min_width || px(rect.h) < name_size {
            continue;
        }
        let mask = to_window(&rect, bounds);
        let origin = Point::new(
            mask.origin.x + text_padding,
            mask.origin.y + text_inset,
        );
        let (fill, color, value_color) =
            colors.label_inks(&label.decoration, label.dim, label.marked);
        // The first level is set in bold in its band: it names a region.
        let weight = if label.depth == 0 && label.header.is_some() {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };

        let run = TextRun {
            len: label.text.len(),
            font: Font {
                weight,
                ..font.clone()
            },
            color,
            // Glyph backgrounds keep hatch out of both names and values.
            background_color: Some(fill),
            // Marked, or going with a mark: struck through, the usual sign
            // for something on its way out, and one that costs no width.
            strikethrough: label.marked.then_some(StrikethroughStyle {
                thickness: decoration,
                color: Some(color),
            }),
            ..TextRun::default()
        };
        let line = text_system.shape_line(
            SharedString::from(label.text.clone()),
            name_size,
            &[run],
            None,
        );

        window.with_content_mask(
            Some(ContentMask { bounds: mask }),
            |window| {
                let _ = line.paint(
                    origin,
                    name_line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                // The part of a name the find text matched is underlined, so
                // a match reads as one without its colour. Painted here, not
                // as a text run's underline: GPUI sets that below the
                // baseline, which the slim band of a deeper directory clips
                // away. This one rises to stay inside the label's region.
                if let Some(found) = label
                    .found
                    .clone()
                    .filter(|found| found.end <= label.text.len())
                {
                    let left = origin.x + line.x_for_index(found.start);
                    let right = origin.x + line.x_for_index(found.end);
                    let top = underline_top(
                        origin.y
                            + underline_y_offset(
                                name_line_height,
                                line.ascent,
                                line.descent,
                            ),
                        mask.origin.y + mask.size.height,
                        decoration,
                    );
                    window.paint_quad(gpui_kit::fill(
                        Bounds::new(
                            Point::new(left, top),
                            Size::new(right - left, decoration),
                        ),
                        color,
                    ));
                }

                if label.size_text.is_empty() {
                    return;
                }
                let room = mask.size.width - (origin.x - mask.origin.x) * 2.;
                let size_run = TextRun {
                    len: label.size_text.len(),
                    font: font.clone(),
                    color: value_color,
                    background_color: Some(fill),
                    ..TextRun::default()
                };
                let size_line = text_system.shape_line(
                    SharedString::from(label.size_text.clone()),
                    size_size,
                    &[size_run],
                    None,
                );
                // Mixed sizes share the name's baseline, not its box. A line
                // paints centred in its line height, putting the baseline at
                // `height / 2 + (ascent - descent) / 2`; equate the two.
                let baseline = origin.y
                    + ((line.ascent - line.descent)
                        - (size_line.ascent - size_line.descent))
                        * 0.5;
                // A first-level band puts the size at the far end, where the
                // sizes read as a column; a deeper band follows the name. A
                // closed tile stacks it under the name when it is tall enough.
                let stacked = label.header.is_none()
                    && mask.size.height >= name_line_height * 2.0 + text_inset;
                if stacked {
                    let _ = size_line.paint(
                        Point::new(
                            origin.x,
                            origin.y + name_line_height * 0.92,
                        ),
                        name_line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    return;
                }
                let size_origin = if label.header.is_some() && label.depth == 0
                {
                    Point::new(
                        mask.origin.x + mask.size.width
                            - size_line.width()
                            - text_padding,
                        baseline,
                    )
                } else {
                    if room - line.width() < size_size * 3.0 {
                        return;
                    }
                    Point::new(origin.x + line.width() + size_gap, baseline)
                };
                if size_origin.x > origin.x + line.width() + text_padding {
                    let _ = size_line.paint(
                        size_origin,
                        name_line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            },
        );
    }
}

/// The top of a find match's underline: where GPUI would put it, raised
/// as far as needed to stay inside the region the label may draw in.
fn underline_top(natural: Pixels, bottom: Pixels, thickness: Pixels) -> Pixels {
    natural.min(bottom - thickness)
}

fn to_window(rect: &Rect, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        Point::new(bounds.origin.x + px(rect.x), bounds.origin.y + px(rect.y)),
        Size::new(px(rect.w), px(rect.h)),
    )
}

fn rem_px(rems: f32, rem_size: Pixels) -> Pixels {
    px(rems * rem_size.as_f32())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise the production fill/ink path, including the deeper age ramp,
    /// filtered and marked combinations the upstream diagnostic omitted.
    #[test]
    fn actual_tile_states_keep_text_and_boundaries_readable() {
        let mut custom = Theme::flexoki_light();
        custom.bright = gpui_kit::rgb(0x0088_8888).into();
        custom.foreground = custom.bright;
        custom.secondary = custom.bright;
        custom.warning = custom.bright;
        custom.danger = custom.bright;
        for theme in [Theme::tokyo_night(), Theme::flexoki_light(), custom] {
            let colors = Colors::new(&theme);
            for category in CATEGORIES {
                for age_bucket in std::iter::once(None).chain((0..5).map(Some))
                {
                    for depth in 0..=4 {
                        for filtered in
                            [Filtered::Shown, Filtered::Holds, Filtered::Out]
                        {
                            for (marked, covered) in
                                [(false, false), (true, false), (false, true)]
                            {
                                let tile = TileDeco {
                                    rect: Rect::new(0., 0., 300., 200.),
                                    depth,
                                    category,
                                    age_bucket,
                                    reclaimable: true,
                                    filtered,
                                    unreadable: true,
                                    marked,
                                    covered,
                                    hovered: true,
                                    selected: true,
                                };
                                let (fill, name, value) = colors.label_inks(
                                    &tile,
                                    filtered == Filtered::Out,
                                    marked || covered,
                                );
                                for ink in [name, value] {
                                    assert!(
                                        palette::contrast(ink, fill) >= 4.5,
                                        "{category:?} age {age_bucket:?} \
                                         depth {depth} {filtered:?} marked \
                                         {marked} covered {covered}"
                                    );
                                    assert!(
                                        (ink.a - 1.0).abs() < f32::EPSILON,
                                        "text never relies on dim alpha"
                                    );
                                }
                                for preferred in [
                                    colors.selected_border,
                                    colors.hover_border,
                                    colors.marked_border,
                                    colors.warning,
                                ] {
                                    assert!(
                                        palette::contrast(
                                            palette::boundary_on(
                                                preferred, fill, 3.0
                                            ),
                                            fill
                                        ) >= 3.0
                                    );
                                }
                            }
                        }
                    }
                }
            }
            let fill = palette::highlight(&theme);
            assert!(
                palette::contrast(palette::on_highlight(&theme), fill) >= 4.5
            );
            assert!(
                palette::contrast(
                    palette::ink_on(theme.secondary, theme.background, 4.5),
                    theme.background
                ) >= 4.5
            );
        }
    }

    /// A slim band would clip GPUI's own underline position, so the
    /// underline rises to stay inside; where there is room it is unmoved.
    #[test]
    fn a_find_underline_stays_inside_its_label() {
        assert_eq!(underline_top(px(17.), px(16.), px(1.)), px(15.));
        assert_eq!(underline_top(px(17.), px(22.), px(1.)), px(17.));
    }

    #[test]
    fn named_categories_use_their_own_paint_colors() {
        for category in Category::LEGEND {
            assert_eq!(CATEGORIES[category_index(category)], category);
        }
    }
}
