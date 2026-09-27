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
    StatefulInteractiveElement as _, Styled, TextAlign, TextRun, Window,
    canvas, div, pattern_slash, px, quad,
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
        .debug_selector(|| "treemap".into())
        .relative()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_hidden()
        .bg(theme.inset)
        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
            if !hovered {
                this.on_mouse_leave(cx);
            }
        }))
        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
            this.on_mouse_move(event, cx);
        }))
        // One listener for every button, because the interesting ones are not
        // the three the platform names: buttons 8 and 9 arrive as
        // `MouseButton::Navigate`, and a per-button registration would have
        // to be repeated for each.
        .on_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, cx| {
            this.on_mouse_down(event, cx);
        }))
        .on_scroll_wheel(cx.listener(
            |this, event: &ScrollWheelEvent, _, cx| {
                this.on_scroll_wheel(event, cx);
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
const CATEGORIES: [Category; 9] = [
    Category::Code,
    Category::AgentScratch,
    Category::Toolchain,
    Category::Synced,
    Category::Git,
    Category::Media,
    Category::Documents,
    Category::Cache,
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
            label_dim: palette::label_color(theme, 1).opacity(0.5),
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
            colors.fill(tile),
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
            window.paint_quad(quad(
                mark,
                Corners::default(),
                colors.warning,
                none,
                colors.warning,
                solid,
            ));
        }

        // Ranked so the most important ring is painted last, on top.
        let outline = if tile.selected {
            Some((3, 2.0, colors.selected_border))
        } else if tile.hovered {
            Some((2, 1.0, colors.hover_border))
        } else if tile.marked {
            Some((1, 2.0, colors.marked_border))
        } else {
            None
        };
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
        let color = if label.marked {
            colors.marked_label
        } else if label.dim {
            colors.label_dim
        } else {
            colors.label(label.depth)
        };
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

                if label.size_text.is_empty() {
                    return;
                }
                let room = mask.size.width - (origin.x - mask.origin.x) * 2.;
                let size_run = TextRun {
                    len: label.size_text.len(),
                    font: font.clone(),
                    color: colors.label_dim,
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

fn to_window(rect: &Rect, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        Point::new(bounds.origin.x + px(rect.x), bounds.origin.y + px(rect.y)),
        Size::new(px(rect.w), px(rect.h)),
    )
}

fn rem_px(rems: f32, rem_size: Pixels) -> Pixels {
    px(rems * rem_size.as_f32())
}
