//! The list view: the directory on screen as rows, with its figures in
//! columns.
//!
//! Where the mosaic answers "what shape is this", the list answers "what is in
//! it and how much is each thing": names in full, a size, counts, a share and a
//! last write, one row per entry, ranked by whatever the Size / Files / Age
//! choice measures.
//!
//! It is composed from elements rather than painted, unlike the mosaic. A
//! directory's children are countable — a few hundred at the top of a
//! disk — and rows have to be individually clickable and hoverable, which
//! painting would have to fake.

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Div, ElementId, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Rems, Stateful, StatefulInteractiveElement as _,
    Styled, Window, div, px, size,
};
use gpui_omarchy::{ActiveTheme, Theme};

use disktree_core::size::human_bytes;
use disktree_core::tree::Metric;

use crate::palette;
use crate::state::{Disktree, Filtered, LIST_ROW_LIMIT, ListRow, now_seconds};
use crate::ui::{icon, size, space, text};
use crate::widgets;

/// The list: a heading row, then the rows the viewport can show.
///
/// Virtualized, unlike the rest of this file's neighbour the mosaic. Opening
/// rows makes a list as long as the tree is deep, and the root of a whole disk
/// has hundreds of thousands of entries a level down — composing a row per
/// entry would spend the whole frame in layout, which is exactly what painting
/// the mosaic exists to avoid. Only the rows on screen are built.
pub fn list(
    app: &Disktree,
    window: &Window,
    cx: &Context<'_, Disktree>,
) -> impl IntoElement {
    let theme = cx.omarchy().clone();
    let flat = app.flat_rows();
    let count = flat.len();
    let body = div()
        .id("disktree-list")
        .debug_selector(|| "list".into())
        .flex()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .flex_col()
        .bg(theme.inset)
        .child(header(app, &theme));

    if count == 0 {
        return body
            .child(
                div()
                    .p(space::XXL)
                    .text_color(theme.secondary)
                    .child("Nothing to list here."),
            )
            .into_any_element();
    }

    // Every row is the same height, so the sizes are one value repeated. The
    // virtual list scrolls by these, so they have to match what a row draws.
    let row_px = size::LIST_ROW.to_pixels(window.rem_size());
    let sizes = Rc::new(vec![size(px(0.), row_px); count]);
    let entity = cx.entity();
    let row_theme = theme.clone();
    let body = body.child(
        gpui_kit::base::v_virtual_list(
            entity,
            "list-rows",
            sizes,
            // One call for the whole range, not one per row: the index and the
            // marks are read once here instead of once per row the window
            // shows, which is the difference between one index a frame and
            // forty of them.
            move |app: &mut Disktree, range: Range<usize>, _, cx| {
                app.list_rows(range)
                    .iter()
                    .map(|(index, row)| list_row(*index, row, &row_theme, cx))
                    .collect::<Vec<_>>()
            },
        )
        .flex_1()
        .min_h_0(),
    );

    if count >= LIST_ROW_LIMIT {
        let body = body.child(
            div()
                .px(space::MD)
                .py(space::SM)
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(format!(
                    "The listing stops at {LIST_ROW_LIMIT} rows. Open a \
                     directory to go on from there."
                )),
        );
        return body.into_any_element();
    }
    body.into_any_element()
}

/// The headings, in the lanes' order. The first column names the metric the
/// rows are ranked by, so a list of file counts does not claim to be sizes.
///
/// Every lane a row draws has a heading here, in the same order and the same
/// width: the percentage beside the bar is one of them, and it keeps the lane
/// its heading holds even though it has no label of its own. An unlabelled lane
/// does not take no space — leaving one out shifts every heading after it off
/// its column.
fn header(app: &Disktree, theme: &Theme) -> Div {
    let ranked = match app.options.metric {
        Metric::Bytes if app.options.apparent_size => "Length",
        Metric::Bytes => "On disk",
        Metric::Files => "Files",
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::MD)
        .w_full()
        .px(space::MD)
        .py(space::SM)
        .border_b_1()
        .border_color(theme.divider())
        // Matches the rows' reserved selection edge, so the first lane starts
        // at the same x in the heading and in every row.
        .border_l_2()
        // A step under the rows it heads: the heading labels the column
        // without competing with the figures below it.
        .text_size(text::BODY)
        .text_color(theme.secondary)
        .child(div().flex_1().min_w_0().child("Name"))
        .child(lane(size::LIST_COUNT, "head-files", ranked))
        .child(lane(size::LIST_COUNT, "head-dirs", "Folders"))
        .child(lane(size::LIST_SHARE, "head-share", "Share"))
        .child(lane(size::LIST_PERCENT, "head-percent", ""))
        .child(lane(size::LIST_SIZE, "head-size", "Size"))
        .child(lane(size::LIST_AGE, "head-age", "Last write"))
}

/// One row: indent, chevron, icon, size, name, counts, share, size, age.
///
/// Lanes are fixed, so the figures line up down the list and can be compared by
/// eye — which is the whole reason this view exists.
fn list_row(
    index: usize,
    row: &ListRow,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Stateful<Div> {
    // Age mode says the same thing here as it does in a tile: the colour is
    // the last write, not the kind of data.
    let accent = row.age_bucket.map_or_else(
        || palette::category_accent(theme, row.category),
        |bucket| palette::age_accent(theme, bucket),
    );
    let marked = row.marked || row.covered;
    let name_color = if marked {
        theme.danger
    } else if row.unreadable {
        theme.warning
    } else {
        theme.foreground
    };
    // A live filter dims what does not match; an applied one has already left
    // it out of the rows entirely.
    let dim = row.filtered == Filtered::Out;
    let current = row.is_current;
    let crumbs = row.crumbs.clone();
    let chevron_crumbs = crumbs.clone();
    let openable = row.has_children;

    let line = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::MD)
        // The one fixed height, and nothing else. The virtual list is told
        // LIST_ROW per row and lays them out by it, so this has to be the
        // whole of the row's height: `py` on top of it is border-box padding,
        // which squeezes the content box to 12px of a 28px row and lets the
        // text spill out of it.
        .h(size::LIST_ROW)
        // A row is read at a glance down a column, so it sets the step itself
        // instead of inheriting the root's body text: the list's emphasis is
        // type, where the mosaic's is area.
        .text_size(text::TITLE)
        // The list hands each row a definite width and lays it out as a root,
        // which sizes to content unless asked. Without this the row stopped
        // short of the right edge while the header, a plain flex child, ran
        // the full width: the two sets of lanes drifted apart.
        .w_full()
        .pr(space::MD)
        .border_b_1()
        .border_color(theme.divider())
        .when(row.selected, |this| this.bg(theme.accent.opacity(0.18)))
        // The selection's edge is drawn on every row, not only the selected
        // one, and only coloured on that one. A border that appears with the
        // selection widens that row's content box and slides its lanes off
        // the column every other row and the heading sit on.
        .border_l_2()
        .when(
            row.selected,
            |this| this.border_color(theme.accent),
        )
        .when(row.hovered && !row.selected, |this| {
            this.bg(theme.hover_fill())
        })
        .when(dim, |this| this.opacity(0.4))
        // The indent, so a child sits under its parent the way a tile does.
        .pl(Rems(size::LIST_INDENT.0.mul_add(row.depth as f32, space::MD.0)))
        .child(chevron(index, row, theme, cx))
        .child(
            gpui_omarchy::icon(if row.is_dir {
                gpui_omarchy::IconName::FolderOpen
            } else {
                gpui_omarchy::IconName::File
            })
            // Sits with the row's own step, not the caption one.
            .size(icon::MD)
            .flex_shrink_0()
            .text_color(if current { accent } else { theme.secondary }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(space::MD)
                .text_color(name_color)
                .when(current, |this| this.font_weight(FontWeight::BOLD))
                // The size beside the name, as the tree explorers show it: it
                // is the figure the row is about, and it is read before the
                // name is.
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.secondary)
                        .child(human_bytes(row.bytes)),
                )
                .child(
                    // One line by construction: the slot is a fixed height
                    // and the virtual list lays rows out by it, so a wrapped
                    // name would spill into the row below.
                    div()
                        .min_w_0()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(row.name.clone()),
                ),
        )
        .child(count(row.files))
        .child(count(row.dirs))
        .child(
            // A painted bar, not block glyphs: at caption size a row of block
            // glyphs reads as a stipple, not a bar. The bar and the percentage
            // say the same thing twice on purpose — the bar compares by eye,
            // the figure compares exactly.
            div()
                .w(size::LIST_SHARE)
                .flex_shrink_0()
                .debug_selector(|| "list-lane-share".into())
                .child(widgets::bar(
                    row.share,
                    if marked { theme.danger } else { accent },
                    cx,
                )),
        )
        .child(
            // The share as a number as well as a bar, so it can be compared
            // without measuring the bar.
            div()
                .w(size::LIST_PERCENT)
                .flex_shrink_0()
                .flex()
                .justify_end()
                .text_color(theme.secondary)
                .child(format!("{:.1}%", row.share * 100.0)),
        )
        .child(
            // Comparable numbers right-align.
            div()
                .w(size::LIST_SIZE)
                .flex_shrink_0()
                .debug_selector(|| "list-lane-size".into())
                .flex()
                .justify_end()
                .text_color(name_color)
                .child(human_bytes(row.bytes)),
        )
        .child(
            div()
                .w(size::LIST_AGE)
                .flex_shrink_0()
                .text_color(theme.secondary)
                .child(widgets::ago(now_seconds(), row.modified)),
        );

    if current {
        return line
            .id(ElementId::Name("list-current".into()))
            .debug_selector(|| "list-current".into());
    }
    // The selector is formatted inside the closure, from a `Copy` index: a
    // captured `String` would have to be cloned on every frame, since a
    // `Fn` closure cannot hand its capture away.
    line.id(ElementId::Name(format!("list-row-{index}").into()))
        .debug_selector(move || format!("list-row-{index}"))
        .on_click(cx.listener(move |this, _, _, cx| {
            // The mosaic's click model, in a row: the first click selects, and
            // a click on the row that is already selected opens it — which in
            // a list is what its own arrow does: it opens in place, or goes in
            // when it is already open. A click that went into the directory at
            // once took the whole screen away from a pointer that was only
            // reading the list, and did something `→` does not.
            //
            // Whether the row is already selected is asked here rather than
            // read off the row: the answer a click acts on is this one, not
            // the one the frame being painted was built with.
            let chosen = this.selected.as_deref() == Some(crumbs.as_slice());
            if openable && chosen {
                this.open_row(crumbs.clone(), cx);
            } else {
                this.select(Some(crumbs.clone()), cx);
            }
            this.pointer_active = false;
        }))
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            this.hover_row(hovered.then_some(chevron_crumbs.as_slice()));
            cx.notify();
        }))
}

/// The open/close chevron, or the space one would occupy.
///
/// A row with nothing inside has no chevron but keeps the indent, so the names
/// below it still line up down the list.
fn chevron(
    index: usize,
    row: &ListRow,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> impl IntoElement {
    let crumbs = row.crumbs.clone();
    let button = div()
        .flex_shrink_0()
        .cursor_pointer()
        .text_color(theme.secondary)
        .id(ElementId::Name(format!("list-chevron-{index}").into()));
    if row.has_children {
        button
            .child(
                gpui_omarchy::icon(if row.expanded {
                    gpui_omarchy::IconName::ChevronDown
                } else {
                    gpui_omarchy::IconName::ChevronRight
                })
                .size(icon::MD),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_expand(&crumbs, cx);
            }))
    } else {
        // The same box as the chevron, so the names still line up.
        button.child(div().size(icon::MD))
    }
}

/// A count, right-aligned in a fixed lane.
fn count(value: u64) -> Div {
    div()
        .w(size::LIST_COUNT)
        .flex_shrink_0()
        .flex()
        .justify_end()
        .child(widgets::human_count(value))
}

/// A right-aligned cell of a fixed width, named so a test can find the heading
/// and the lane under it and check the two line up.
fn lane(width: gpui_kit::Rems, selector: &'static str, label: &str) -> Div {
    div()
        .w(width)
        .flex_shrink_0()
        .flex()
        .justify_end()
        .debug_selector(move || selector.into())
        .child(label.to_string())
}
