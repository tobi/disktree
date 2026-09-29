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
    ParentElement as _, PathBuilder, PinchEvent, Pixels, Point,
    ScrollWheelEvent, SharedString, Size, StatefulInteractiveElement as _,
    Styled, TextAlign, TextRun, Window, canvas, div, pattern_slash, px, quad,
};
use gpui_omarchy::{ActiveTheme, Theme};

use disktree_core::classify::Category;

use crate::charts::{Chart, ChartGeometry};
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
///
/// A sunburst's tiles and labels stay in the partition's unit space, which
/// `geometry` maps to rings; every other chart's are in pixels, before
/// `view`.
#[derive(Clone, Debug, Default)]
pub struct Mosaic {
    pub tiles: Vec<TileDeco>,
    pub labels: Vec<Label>,
    pub view: View,
    pub chart: Chart,
    pub geometry: Option<ChartGeometry>,
    /// The directory a sunburst is drawn around, named in its hole.
    pub center: Option<CenterLabel>,
}

/// What a sunburst's hole says.
#[derive(Clone, Debug)]
pub struct CenterLabel {
    pub name: String,
    pub size_text: String,
    pub hovered: bool,
    /// Clicking it goes somewhere: there is a level above.
    pub can_ascend: bool,
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
        chart,
        geometry,
        center,
    } = mosaic;
    let text = Type {
        font: Font {
            family: font,
            ..Font::default()
        },
        name: name_size,
        size: size_size,
        center: rem_px(0.875, rem),
        figure: rem_px(1.375, rem),
    };
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
        .on_pinch(cx.listener(|this, event: &PinchEvent, _, cx| {
            this.on_pinch(event, cx);
        }))
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
                move |bounds, _, window, cx| match geometry {
                    Some(geometry) if chart == Chart::Sunburst => {
                        paint_sunburst(
                            &tiles, &geometry, bounds, &colors, window,
                        );
                        paint_sunburst_labels(
                            &labels, &geometry, bounds, &colors, &text, window,
                            cx,
                        );
                        if let Some(center) = &center {
                            paint_center(
                                center, &geometry, bounds, &colors, &text,
                                window, cx,
                            );
                        }
                    }
                    _ => {
                        paint_tiles(&tiles, bounds, view, &colors, window);
                        paint_labels(
                            &labels,
                            bounds,
                            view,
                            &colors,
                            &text.font.family,
                            text.name,
                            text.size,
                            window,
                            cx,
                        );
                    }
                },
            )
            .absolute()
            .inset_0(),
        )
}

/// The type a frame's labels are set in, at the window's `rem`.
struct Type {
    font: Font,
    name: Pixels,
    size: Pixels,
    /// The name in a sunburst's hole.
    center: Pixels,
    /// The size under it.
    figure: Pixels,
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

/// A shape as polygons, filled under the even-odd rule.
type Outline = Vec<Vec<Point<Pixels>>>;

fn ellipsized(characters: &[char]) -> String {
    characters
        .iter()
        .chain(std::iter::once(&'\u{2026}'))
        .collect()
}

/// The gap left between neighbouring wedges, in pixels.
const WEDGE_GAP: f32 = 1.5;

/// A point at `radius` from the chart's centre, `turn` of the way round
/// clockwise from twelve o'clock.
fn polar(centre: Point<Pixels>, radius: f32, turn: f32) -> Point<Pixels> {
    let angle =
        turn.mul_add(std::f32::consts::TAU, -std::f32::consts::FRAC_PI_2);
    Point::new(
        centre.x + px(radius * angle.cos()),
        centre.y + px(radius * angle.sin()),
    )
}

/// The outline of a sunburst tile, spanning `low` to `high` in radius, with
/// half a gap taken off every side so neighbours part without the layout
/// leaving room.
fn wedge(rect: Rect, centre: Point<Pixels>, low: f32, high: f32) -> Outline {
    let (low, high) = (low + WEDGE_GAP / 2.0, high - WEDGE_GAP / 2.0);
    if high <= low {
        return Vec::new();
    }
    let circle = |radius: f32| -> Vec<Point<Pixels>> {
        let steps = 120;
        (0..steps)
            .map(|step| polar(centre, radius, step as f32 / steps as f32))
            .collect()
    };
    // A whole turn drawn as one arc closes on itself; as two circles it is
    // a ring under the even-odd rule.
    if rect.w >= 1.0 - 1e-6 {
        return vec![circle(high), circle(low)];
    }
    let arc = |radius: f32| -> (f32, f32) {
        let inset = (WEDGE_GAP / 2.0) / (radius * std::f32::consts::TAU);
        if rect.w > inset * 2.0 {
            (rect.x + inset, rect.right() - inset)
        } else {
            (rect.x, rect.right())
        }
    };
    let steps = ((rect.w * 180.0).ceil() as usize).max(1);
    let (outer_start, outer_end) = arc(high);
    let (inner_start, inner_end) = arc(low);
    let mut points = Vec::with_capacity(steps * 2 + 2);
    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        points.push(polar(
            centre,
            high,
            (outer_end - outer_start).mul_add(t, outer_start),
        ));
    }
    for step in (0..=steps).rev() {
        let t = step as f32 / steps as f32;
        points.push(polar(
            centre,
            low,
            (inner_end - inner_start).mul_add(t, inner_start),
        ));
    }
    vec![points]
}

fn path(
    outline: &[Vec<Point<Pixels>>],
    stroke: Option<f32>,
) -> Option<gpui_kit::Path<Pixels>> {
    let mut builder = stroke
        .map_or_else(PathBuilder::fill, |width| PathBuilder::stroke(px(width)));
    for polygon in outline {
        builder.add_polygon(polygon, true);
    }
    builder.build().ok()
}

fn paint_sunburst(
    tiles: &[TileDeco],
    geometry: &ChartGeometry,
    bounds: Bounds<Pixels>,
    colors: &Colors,
    window: &mut Window,
) {
    let (cx, cy) = geometry.centre();
    let centre = Point::new(bounds.origin.x + px(cx), bounds.origin.y + px(cy));
    let radius = |level: f32| geometry.ring().mul_add(level, geometry.hole());
    let mut outlines: Vec<(u8, Outline, f32, Hsla)> = Vec::new();
    for tile in tiles {
        let outline = wedge(
            tile.rect,
            centre,
            radius(tile.rect.y),
            radius(tile.rect.bottom()),
        );
        if outline.is_empty() {
            continue;
        }
        if let Some(fill) = path(&outline, None) {
            window.paint_path(fill, colors.fill(tile));
        }
        if tile.reclaimable
            && !tile.marked
            && !tile.covered
            && tile.filtered == Filtered::Shown
            && let Some(hatch) = path(&outline, None)
        {
            window.paint_path(hatch, pattern_slash(colors.hatch, 1.0, 6.0));
        }
        // A first-level wedge carries a strip of its colour at its inner
        // edge, as a top-level tile carries one along its top.
        if tile.depth == 0
            && tile.age_bucket.is_none()
            && tile.filtered != Filtered::Out
        {
            let inner = radius(tile.rect.y);
            let strip =
                wedge(tile.rect, centre, inner, inner + 3.0 + WEDGE_GAP);
            if let Some(strip) = path(&strip, None) {
                window.paint_path(
                    strip,
                    colors.strip[category_index(tile.category)],
                );
            }
        }
        let outline_style = if tile.selected {
            Some((3, 2.5, colors.selected_border))
        } else if tile.hovered {
            Some((2, 1.5, colors.hover_border))
        } else if tile.marked {
            Some((1, 2.0, colors.marked_border))
        } else {
            None
        };
        if let Some((rank, width, color)) = outline_style {
            outlines.push((rank, outline, width, color));
        }
    }
    outlines.sort_by_key(|(rank, ..)| *rank);
    for (_, outline, width, color) in outlines {
        if let Some(ring) = path(&outline, Some(width)) {
            window.paint_path(ring, color);
        }
    }
}

/// Labels set level at the middle of their wedge, where the wedge is wide
/// and tall enough for them there: text cannot turn with the ring.
fn paint_sunburst_labels(
    labels: &[Label],
    geometry: &ChartGeometry,
    bounds: Bounds<Pixels>,
    colors: &Colors,
    text: &Type,
    window: &mut Window,
    cx: &mut App,
) {
    let (cx_, cy) = geometry.centre();
    let centre =
        Point::new(bounds.origin.x + px(cx_), bounds.origin.y + px(cy));
    let text_system = window.text_system().clone();
    let line_height = text.name * 1.35;
    let padding = text.name.as_f32() * 0.5;
    for label in labels {
        let inner = geometry.ring().mul_add(label.rect.y, geometry.hole());
        let thickness = geometry.ring() * label.rect.h;
        let middle = inner + thickness / 2.0;
        let turn = label.rect.x + label.rect.w / 2.0;
        let half = label.rect.w * std::f32::consts::PI;
        let chord = if half >= std::f32::consts::FRAC_PI_2 {
            middle * 2.0
        } else {
            2.0 * middle * half.sin()
        };
        // The wedge as a box around its middle, `chord` along the ring and
        // `thickness` across it: how far a level line through the middle
        // runs before it leaves.
        let angle = turn * std::f32::consts::TAU;
        let (along, across) = (angle.cos().abs(), angle.sin().abs());
        let fits = |tangent: f32, radial: f32| {
            (chord / tangent.max(1e-3)).min(thickness / radial.max(1e-3))
        };
        let width = fits(along, across) - padding * 2.0;
        let height = fits(across, along);
        if width < text.name.as_f32() * 3.3 || height < line_height.as_f32() {
            continue;
        }
        let color = if label.marked {
            colors.marked_label
        } else if label.dim {
            colors.label_dim
        } else {
            colors.label(label.depth)
        };
        let weight = if label.depth == 0 {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };
        let Some(name) = fitted(
            &text_system,
            &label.text,
            &Font {
                weight,
                ..text.font.clone()
            },
            text.name,
            color,
            px(width),
        ) else {
            continue;
        };
        let stacked = !label.size_text.is_empty()
            && height >= line_height.as_f32().mul_add(2.0, padding);
        let point = polar(centre, middle, turn);
        let top = if stacked {
            point.y - line_height
        } else {
            point.y - line_height / 2.0
        };
        let _ = name.paint(
            Point::new(point.x - name.width() / 2.0, top),
            line_height,
            TextAlign::Left,
            None,
            window,
            cx,
        );
        if stacked {
            let size = shape(
                &text_system,
                &label.size_text,
                &text.font,
                text.size,
                colors.label_dim,
            );
            if size.width().as_f32() <= width {
                let _ = size.paint(
                    Point::new(
                        point.x - size.width() / 2.0,
                        top + line_height * 0.95,
                    ),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        }
    }
}

/// The directory a sunburst is drawn around, in its hole: its name and
/// size, and when pointed at, that a click goes up.
fn paint_center(
    center: &CenterLabel,
    geometry: &ChartGeometry,
    bounds: Bounds<Pixels>,
    colors: &Colors,
    text: &Type,
    window: &mut Window,
    cx: &mut App,
) {
    let (cx_, cy) = geometry.centre();
    let centre =
        Point::new(bounds.origin.x + px(cx_), bounds.origin.y + px(cy));
    let hole = geometry.hole();
    if center.hovered && center.can_ascend {
        let ring = vec![
            (0..120)
                .map(|step| polar(centre, hole - 4.0, step as f32 / 120.0))
                .collect::<Vec<_>>(),
        ];
        if let Some(ring) = path(&ring, Some(1.5)) {
            window.paint_path(ring, colors.hover_border);
        }
    }
    let text_system = window.text_system().clone();
    let room = px(hole * 1.6);
    let Some(name) = fitted(
        &text_system,
        &center.name,
        &Font {
            weight: FontWeight::SEMIBOLD,
            ..text.font.clone()
        },
        text.center,
        colors.label(0),
        room,
    ) else {
        return;
    };
    let size = shape(
        &text_system,
        &center.size_text,
        &text.font,
        text.figure,
        colors.label(0),
    );
    let name_line = text.center * 1.3;
    let size_line = text.figure * 1.2;
    let hint = (center.hovered && center.can_ascend)
        .then(|| {
            shape(
                &text_system,
                "click to go up",
                &text.font,
                text.size,
                colors.label_dim,
            )
        })
        .filter(|hint| hint.width() <= room);
    let hint_line = text.size * 1.4;
    let total =
        name_line + size_line + hint.as_ref().map_or(px(0.), |_| hint_line);
    let mut top = centre.y - total / 2.0;
    let _ = name.paint(
        Point::new(centre.x - name.width() / 2.0, top),
        name_line,
        TextAlign::Left,
        None,
        window,
        cx,
    );
    top += name_line;
    if size.width() <= room {
        let _ = size.paint(
            Point::new(centre.x - size.width() / 2.0, top),
            size_line,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
    top += size_line;
    if let Some(hint) = hint {
        let _ = hint.paint(
            Point::new(centre.x - hint.width() / 2.0, top),
            hint_line,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

fn shape(
    text_system: &std::sync::Arc<gpui_kit::WindowTextSystem>,
    text: &str,
    font: &Font,
    size: Pixels,
    color: Hsla,
) -> gpui_kit::ShapedLine {
    let run = TextRun {
        len: text.len(),
        font: font.clone(),
        color,
        ..TextRun::default()
    };
    text_system.shape_line(
        SharedString::from(text.to_owned()),
        size,
        &[run],
        None,
    )
}

/// `text` shaped to fit `width`, cut short with an ellipsis if it must be,
/// or `None` when not even a few letters would.
fn fitted(
    text_system: &std::sync::Arc<gpui_kit::WindowTextSystem>,
    text: &str,
    font: &Font,
    size: Pixels,
    color: Hsla,
    width: Pixels,
) -> Option<gpui_kit::ShapedLine> {
    let line = shape(text_system, text, font, size, color);
    if line.width() <= width {
        return Some(line);
    }
    let characters: Vec<char> = text.chars().collect();
    let (mut low, mut high) = (0, characters.len());
    while low < high {
        let middle = (low + high).div_ceil(2);
        let candidate = ellipsized(&characters[..middle]);
        if shape(text_system, &candidate, font, size, color).width() <= width {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    (low >= 2).then(|| {
        shape(
            text_system,
            &ellipsized(&characters[..low]),
            font,
            size,
            color,
        )
    })
}
