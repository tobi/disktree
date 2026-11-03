//! Removing and Done: the run's list, checked off as it goes, beside its
//! running figures. Both screens draw the same list in the same place, so
//! nothing moves when the run ends and the list never disappears.

use std::fmt::Write as _;

use disktree_core::removal::RemovalMode;
use disktree_core::size::human_bytes;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Div, ElementId, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, Window, div, px, relative,
};
use gpui_omarchy::{ActiveTheme, ButtonVariant, Theme, button, with_tooltip};

use crate::run_list::{Item, Status, Totals};
use crate::semantics::AccessibleButton as _;
use crate::state::{Disktree, Screen};
use crate::ui::{icon, size, space, text};
use crate::views::{
    REVIEW_COLUMNS_REMS, dialog_footer, ledger_table, screen_footer,
    screen_header, signed_bytes,
};
use crate::{palette, widgets};

/// Rems a row spends on everything but its name and folder: the checkbox,
/// the swatch, the kind chip, the size and status lanes and the gaps.
const ROW_FIXED_REMS: f32 = 22.0;
/// The same, without the status lane, in a narrow window.
const ROW_FIXED_COMPACT_REMS: f32 = 16.0;
/// Review's rows: the icon, the swatch, the kind chip, the share bar, the
/// size, Unmark and the gaps.
pub const REVIEW_ROW_FIXED_REMS: f32 = 30.0;
/// A caption character's average advance in ems, on the generous side: the
/// shortened folder should fit, and the row clips whatever does not.
const CAPTION_ADVANCE_EMS: f32 = 0.6;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Done,
}

/// How one row reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row<'a> {
    Waiting,
    Current,
    /// Never reached: the run was stopped first.
    Skipped,
    Removed,
    Failed(&'a str),
}

impl Row<'_> {
    const fn dimmed(self) -> bool {
        matches!(self, Self::Waiting | Self::Skipped)
    }

    const fn label(self, mode: RemovalMode) -> &'static str {
        match (self, mode) {
            (Self::Waiting, _) => "waiting",
            (Self::Current, RemovalMode::Trash) => "moving\u{2026}",
            (Self::Current, RemovalMode::Permanent) => "removing\u{2026}",
            (Self::Skipped, _) => "not reached",
            (Self::Removed, RemovalMode::Trash) => "in trash",
            (Self::Removed, RemovalMode::Permanent) => "removed",
            (Self::Failed(_), _) => "failed",
        }
    }

    const fn color(self, theme: &Theme) -> Hsla {
        match self {
            Self::Waiting | Self::Skipped => theme.secondary,
            Self::Current => theme.accent,
            Self::Removed => theme.success,
            Self::Failed(_) => theme.danger,
        }
    }
}

const fn mode_phrase(mode: RemovalMode, done: bool) -> &'static str {
    match (mode, done) {
        (RemovalMode::Trash, false) => "moving to trash",
        (RemovalMode::Trash, true) => "moved to trash",
        (RemovalMode::Permanent, false) => "deleting permanently",
        (RemovalMode::Permanent, true) => "deleted permanently",
    }
}

// ── running ─────────────────────────────────────────────────────────────

pub fn running(
    app: &Disktree,
    toasts: Option<AnyElement>,
    window: &Window,
    cx: &gpui_kit::Context<'_, Disktree>,
) -> Div {
    let theme = cx.omarchy().clone();
    let totals = app.run_list.totals();
    let settled = totals.removed + totals.failed;
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(screen_header(
            "Removing",
            &format!(
                "{settled} of {} \u{b7} {}",
                totals.total,
                mode_phrase(app.removal_mode, false)
            ),
            &theme,
            cx,
        ))
        .child(body(app, Phase::Running, &totals, toasts, window, cx))
        .child(screen_footer(
            dialog_footer(
                // Linux keeps its keys in view, as review does; macOS and
                // Windows have the button and `?`.
                (crate::chrome::Platform::current()
                    == crate::chrome::Platform::Linux)
                    .then(|| {
                        widgets::hint("esc", "stop after this item", cx)
                            .into_any_element()
                    }),
                None,
                // Outlined, not danger: it loses nothing, and what was
                // removed stays removed either way.
                Some(
                    button(
                        "stop-removal",
                        if app.stop_requested {
                            "Stopping\u{2026}"
                        } else {
                            "Stop after this item"
                        },
                        ButtonVariant::Outline,
                        cx,
                    )
                    .debug_selector(|| "stop-removal".into())
                    .semantic_disabled(app.stop_requested)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.stop_removal(cx);
                        window.focus(&this.focus, cx);
                    }))
                    .into_any_element(),
                ),
            ),
            &theme,
        ))
}

// ── done ────────────────────────────────────────────────────────────────

pub fn done(
    app: &Disktree,
    toasts: Option<AnyElement>,
    window: &Window,
    cx: &gpui_kit::Context<'_, Disktree>,
) -> Div {
    let theme = cx.omarchy().clone();
    let totals = app.run_list.totals();
    let mut subtitle = format!("{} removed", totals.removed);
    if totals.failed_all() > 0 {
        let _ = write!(subtitle, " \u{b7} {} failed", totals.failed_all());
    }
    let skipped = totals.total - totals.removed - totals.failed;
    if skipped > 0 {
        let _ = write!(subtitle, " \u{b7} {skipped} not reached");
    }
    subtitle.push_str(" \u{b7} ");
    subtitle.push_str(mode_phrase(app.removal_mode, true));

    // Rescan is offered while none runs: it is how the sharing figures
    // catch up, and nothing else on this screen starts one.
    let rescan = (app.scan.is_none()).then(|| {
        button("done-rescan", "Rescan", ButtonVariant::Outline, cx)
            .debug_selector(|| "done-rescan".into())
            .on_click(cx.listener(|this, _, window, cx| {
                this.screen = Screen::Explore;
                this.start_scan(cx);
                window.focus(&this.focus, cx);
            }))
            .into_any_element()
    });
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(screen_header("Done", &subtitle, &theme, cx))
        .child(body(app, Phase::Done, &totals, toasts, window, cx))
        .child(screen_footer(
            dialog_footer(
                [],
                rescan,
                // An acknowledgement, the default: Enter and Escape press it.
                Some(
                    button("continue", "Done", ButtonVariant::Primary, cx)
                        .debug_selector(|| "continue".into())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.screen = Screen::Explore;
                            cx.notify();
                            window.focus(&this.focus, cx);
                        }))
                        .into_any_element(),
                ),
            ),
            &theme,
        ))
}

// ── shared ──────────────────────────────────────────────────────────────

/// The list on the left and the figures on the right, or the figures
/// under the list in a window too narrow for both, as review does.
fn body(
    app: &Disktree,
    phase: Phase,
    totals: &Totals,
    toasts: Option<AnyElement>,
    window: &Window,
    cx: &gpui_kit::Context<'_, Disktree>,
) -> Div {
    let theme = cx.omarchy().clone();
    let compact = window.viewport_size().width.as_f32()
        / window.rem_size().as_f32()
        < REVIEW_COLUMNS_REMS;
    let budget = folder_budget(
        window,
        if compact {
            ROW_FIXED_COMPACT_REMS
        } else {
            ROW_FIXED_REMS
        },
    );

    let sidebar = sidebar(app, phase, totals, &theme, cx)
        .when(compact, |panel| panel.w_full().max_h(relative(0.45)));
    div()
        .relative()
        .flex()
        .flex_row()
        .when(compact, gpui_kit::Styled::flex_col)
        .gap(space::LG)
        .p(space::LG)
        .flex_1()
        .min_h_0()
        .child(list_column(app, phase, totals, budget, compact, &theme, cx))
        .child(sidebar)
        .children(toasts)
}

/// How many caption characters a row's folder line has to itself, in a
/// list beside the summary panel or, in a narrow window, across it.
/// `fixed` is what the row's other lanes take, in rems.
pub fn folder_budget(window: &Window, fixed: f32) -> usize {
    let width =
        window.viewport_size().width.as_f32() / window.rem_size().as_f32();
    let list = if width < REVIEW_COLUMNS_REMS {
        width
    } else {
        width - size::REVIEW_SUMMARY.0
    };
    // At least a name or two, however narrow: the row clips the rest.
    ((list - fixed) / (text::CAPTION.0 * CAPTION_ADVANCE_EMS)).max(16.0)
        as usize
}

/// The folder `path` is in, as a person reads it, shortened to `budget`.
pub fn folder_line(
    app: &Disktree,
    path: &std::path::Path,
    budget: usize,
) -> String {
    let style = app.path_style();
    style.parent(path).map_or_else(String::new, |parent| {
        crate::run_list::elide_middle(
            &app.display_path(parent),
            style.separator(),
            budget,
        )
    })
}

fn list_column(
    app: &Disktree,
    phase: Phase,
    totals: &Totals,
    budget: usize,
    compact: bool,
    theme: &Theme,
    cx: &gpui_kit::Context<'_, Disktree>,
) -> Div {
    let note = match phase {
        Phase::Running => "in the order they are removed".to_string(),
        Phase::Done if totals.failed == 1 => {
            "1 could not be removed".to_string()
        }
        Phase::Done if totals.failed > 0 => {
            format!("{} could not be removed", totals.failed)
        }
        Phase::Done => String::new(),
    };
    // Only the rows in view are built: this screen redraws with every
    // report and every free-space sample, and a run can be thousands long.
    // Every row is two lines, so the list can be uniform.
    let rows = gpui_kit::uniform_list(
        "run-rows",
        app.run_list.items.len(),
        cx.processor(
            move |this: &mut Disktree,
                  range: std::ops::Range<usize>,
                  _,
                  cx: &mut gpui_kit::Context<'_, Disktree>| {
                let theme = cx.omarchy().clone();
                let app = &*this;
                let current = (phase == Phase::Running)
                    .then(|| app.run_list.current())
                    .flatten();
                range
                    .filter_map(|index| {
                        let item = app.run_list.items.get(index)?;
                        let state = match &item.status {
                            Status::Removed => Row::Removed,
                            Status::Failed(reason) => Row::Failed(reason),
                            Status::Waiting if current == Some(index) => {
                                Row::Current
                            }
                            Status::Waiting if phase == Phase::Done => {
                                Row::Skipped
                            }
                            Status::Waiting => Row::Waiting,
                        };
                        let folder =
                            folder_line(app, &item.target.path, budget);
                        Some(row(
                            index, item, state, folder, compact, app, &theme,
                            cx,
                        ))
                    })
                    .collect::<Vec<_>>()
            },
        ),
    )
    .track_scroll(&app.run_scroll)
    .size_full();
    let list = div()
        .id("run-list")
        .debug_selector(|| "run-list".into())
        .role(gpui_kit::Role::List)
        .aria_label(format!(
            "{} items to remove; {} removed; {} failed",
            totals.total, totals.removed, totals.failed
        ))
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .border_1()
        .border_color(theme.border)
        .bg(theme.inset)
        // The next poll reads where this left the item in hand, so the
        // list has scrolled by then whichever handler runs first.
        .on_scroll_wheel(cx.listener(|this, _, _, cx| {
            this.run_list.scrolled_by_hand = true;
            cx.notify();
        }))
        .child(rows);

    let cohorts = app.ledger.cohorts();
    div()
        .flex()
        .flex_col()
        .gap(space::XS)
        .flex_1()
        .min_w_0()
        .min_h_0()
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(space::SM)
                .child(widgets::eyebrow(
                    format!(
                        "{} {}",
                        totals.total,
                        if totals.total == 1 { "item" } else { "items" }
                    ),
                    cx,
                ))
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(
                            if phase == Phase::Done && totals.failed > 0 {
                                theme.danger
                            } else {
                                theme.secondary
                            },
                        )
                        .child(note),
                ),
        )
        .child(
            div()
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(list)
                .children(
                    (phase == Phase::Running && !app.run_list.follow)
                        .then(|| follow_pill(app, totals, theme, cx)),
                ),
        )
        .when(phase == Phase::Done && !cohorts.is_empty(), |column| {
            column.child(
                div()
                    .flex_shrink_0()
                    .pt(space::SM)
                    .child(ledger_table(cohorts, theme, cx)),
            )
        })
}

/// Where the item in hand sits against the list's viewport: `Equal` when
/// any of it shows, `Less` above it, `Greater` below. `None` before the
/// list is first laid out. Worked out from the rows' one height, so a row
/// need not have been drawn to be placed.
pub fn focus_place(app: &Disktree) -> Option<std::cmp::Ordering> {
    let row = app.run_list.focus()?;
    let count = app.run_list.items.len();
    let state = app.run_scroll.0.borrow();
    let size = state.last_item_size?;
    if count == 0 {
        return None;
    }
    let height = size.contents.height / count as f32;
    let top = -state.base_handle.offset().y;
    let bottom = top + size.item.height;
    let row_top = height * row as f32;
    Some(if row_top + height <= top {
        std::cmp::Ordering::Less
    } else if row_top >= bottom {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Equal
    })
}

/// "↓ Follow the removal · 3 of 8", while following is paused: the live
/// log's pill. A click brings the item in hand back and follows it again.
fn follow_pill(
    app: &Disktree,
    totals: &Totals,
    theme: &Theme,
    cx: &gpui_kit::Context<'_, Disktree>,
) -> Div {
    let above = focus_place(app) == Some(std::cmp::Ordering::Less);
    let label = format!(
        "{} Follow the removal \u{b7} {} of {}",
        if above { '\u{2191}' } else { '\u{2193}' },
        totals.removed + totals.failed,
        totals.total
    );
    div()
        .absolute()
        .bottom(space::LG)
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            div()
                .id("follow-removal")
                .debug_selector(|| "follow-removal".into())
                .role(gpui_kit::Role::Button)
                .aria_label(label.clone())
                .px(space::LG)
                .py(space::XS)
                .border_1()
                .border_color(theme.accent)
                .bg(theme.surface)
                .shadow_lg()
                .cursor_pointer()
                .text_color(theme.accent)
                .font_weight(FontWeight::SEMIBOLD)
                .child(label)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.follow_removal();
                    cx.notify();
                })),
        )
}

/// One item: a checkbox that fills in as it goes, its kind's colour, its
/// name over the folder it is in, why it is listed, its size and what
/// happened to it. Lanes are fixed so sizes line up down the list.
#[allow(
    clippy::too_many_arguments,
    reason = "a row is drawn from the run, the theme and its own layout"
)]
fn row(
    index: usize,
    item: &Item,
    state: Row<'_>,
    folder: String,
    compact: bool,
    app: &Disktree,
    theme: &Theme,
    cx: &gpui_kit::App,
) -> Stateful<Div> {
    let accent = palette::category_accent(theme, item.category);
    let kind = item.kind();
    let full = app.display_path(&item.target.path);
    let label = state.label(app.removal_mode);
    let name = app
        .path_style()
        .file_name(&item.target.path)
        .map_or_else(|| full.clone(), |name| name.to_string_lossy().into());
    let second = match state {
        Row::Failed(reason) => {
            div().text_color(theme.warning).child(reason.to_string())
        }
        _ => div().text_color(theme.secondary).child(folder),
    };
    let row = div()
        .id(ElementId::Name(SharedString::from(format!(
            "run-row-{index}"
        ))))
        .debug_selector(move || format!("run-row-{index}"))
        .role(gpui_kit::Role::ListItem)
        .aria_label(format!(
            "{full}; {}; {}{label}",
            human_bytes(item.target.bytes),
            kind.map_or_else(String::new, |kind| format!("{kind}; "))
        ))
        .relative()
        // A virtualized row is only as wide as its content otherwise, and
        // the size and status lanes would wander with the kind chip.
        .w_full()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .items_center()
        .gap(space::MD)
        .px(space::MD)
        .py(space::SM)
        .border_b_1()
        .border_color(theme.divider())
        .when(state.dimmed(), |row| row.opacity(0.55))
        .when(state == Row::Current, |row| {
            row.bg(theme.accent.opacity(0.1)).child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.))
                    .bg(theme.accent),
            )
        })
        .child(checkbox(state, theme))
        .child(div().flex_shrink_0().w(px(2.)).h(space::XL).bg(accent))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(if matches!(state, Row::Failed(_)) {
                            theme.danger
                        } else {
                            theme.bright
                        })
                        .child(name),
                )
                .child(
                    second
                        .text_size(text::CAPTION)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis(),
                ),
        )
        .children(
            kind.map(|kind| widgets::chip(kind, accent, cx).flex_shrink_0()),
        )
        .child(
            // Comparable numbers right-align.
            div()
                .w(size::SIZE_LANE)
                .flex_shrink_0()
                .flex()
                .justify_end()
                .text_color(theme.bright)
                .child(human_bytes(item.target.bytes)),
        )
        .when(!compact, |row| {
            row.child(
                div()
                    .w(size::SIZE_LANE)
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .text_size(text::CAPTION)
                    .text_color(state.color(theme))
                    .child(label),
            )
        });
    with_tooltip(row, full)
}

/// A drawn checkbox, not a glyph: an outline while waiting, a dot in the
/// accent while in hand, filled green with a check once removed, red with
/// a cross when it failed.
fn checkbox(state: Row<'_>, theme: &Theme) -> Div {
    let color = state.color(theme);
    let ink = palette::ink_on(theme.background, color, 4.5);
    let mark = match state {
        Row::Removed => Some(gpui_omarchy::IconName::Check),
        Row::Failed(_) => Some(gpui_omarchy::IconName::X),
        _ => None,
    };
    let filled = mark.is_some();
    div()
        .flex_shrink_0()
        .size(icon::MD)
        .flex()
        .items_center()
        .justify_center()
        .border_1()
        .border_color(if filled || state == Row::Current {
            color
        } else {
            theme.control_border()
        })
        .when(filled, |box_| box_.bg(color))
        .children(mark.map(|mark| {
            gpui_omarchy::icon(mark).size(icon::SM).text_color(ink)
        }))
        .when(state == Row::Current, |box_| {
            box_.child(div().size(space::XS).bg(color))
        })
}

/// What the run has done and what it is worth, beside the list: while it
/// runs, progress and the figures so far against the whole; once it is
/// done, the result.
fn sidebar(
    app: &Disktree,
    phase: Phase,
    totals: &Totals,
    theme: &Theme,
    cx: &gpui_kit::Context<'_, Disktree>,
) -> Stateful<Div> {
    let rule = || div().flex_shrink_0().h(px(1.)).bg(theme.divider());
    let observed = observed(app, phase);
    let mut panel = div()
        .id("run-summary")
        .debug_selector(|| "run-summary".into())
        .flex()
        .flex_col()
        .gap(space::LG)
        .w(size::REVIEW_SUMMARY)
        .flex_shrink_0()
        .min_h_0()
        .overflow_y_scroll()
        .p(space::LG)
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface);

    if phase == Phase::Running {
        let settled = totals.removed + totals.failed;
        let fraction = if totals.total == 0 {
            0.0
        } else {
            settled as f32 / totals.total as f32
        };
        let now = app.run_list.current().map(|index| {
            let item = &app.run_list.items[index];
            let style = app.path_style();
            let name = style.file_name(&item.target.path).map_or_else(
                || app.display_path(&item.target.path),
                |name| name.to_string_lossy().into_owned(),
            );
            div()
                .flex()
                .flex_row()
                .gap(space::XS)
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .whitespace_nowrap()
                .overflow_hidden()
                .child("Now")
                .child(
                    div()
                        .text_color(theme.accent)
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(name),
                )
        });
        panel = panel.child(
            div()
                .flex()
                .flex_col()
                .gap(space::XS)
                .child(widgets::eyebrow("Progress", cx))
                .child(big_figure(
                    settled.to_string(),
                    format!("of {} items", totals.total),
                    theme.bright,
                    theme,
                ))
                .child(widgets::bar(
                    fraction,
                    if app.removal_mode == RemovalMode::Permanent {
                        theme.danger
                    } else {
                        theme.accent
                    },
                    cx,
                ))
                .children(now),
        );
        panel = panel.child(rule());
    }

    // The three figures. While it runs each is what has finished, against
    // what the whole run would come to.
    let of = |done: String, all: String| match phase {
        Phase::Running => format!("{done} / {all}"),
        Phase::Done => done,
    };
    let latest = app.ledger.cohorts().last();
    let (reported, private) = match (phase, latest) {
        // Done reads the ledger: its private part is estimated over the
        // whole run, which counts a clone shared only inside it once.
        (Phase::Done, Some(cohort)) => (cohort.reported, cohort.private),
        _ => (totals.reported_done, totals.private_done),
    };
    let private_text = private.map_or_else(
        || "unknown".to_string(),
        |private| {
            of(
                human_bytes(private),
                totals.private_all.map_or_else(
                    || "?".into(),
                    |all| format!("\u{2248} {}", human_bytes(all)),
                ),
            )
        },
    );
    let mut figures = div()
        .flex()
        .flex_col()
        .gap(space::SM)
        .child(widgets::eyebrow(
            if phase == Phase::Running {
                "Freed so far"
            } else {
                "Freed"
            },
            cx,
        ))
        .children(observed.map(|delta| {
            let (number, unit) = widgets::split_size(&signed_bytes(delta));
            big_figure(
                number,
                format!("{unit} observed"),
                if delta >= 0 {
                    theme.success
                } else {
                    theme.warning
                },
                theme,
            )
        }));
    if phase == Phase::Done {
        figures = figures.child(figure_row(
            "Removed",
            None,
            format!("{} of {}", totals.removed, totals.total),
            theme.bright,
            theme,
        ));
    }
    figures = figures
        .child(figure_row(
            "Reported",
            Some("listed size of what went"),
            of(human_bytes(reported), human_bytes(totals.reported_all)),
            theme.bright,
            theme,
        ))
        .child(figure_row(
            "Private",
            Some("the scan\u{2019}s estimate of theirs alone"),
            private_text,
            theme.bright,
            theme,
        ))
        .child(figure_row(
            "Observed",
            Some("free space now, less at the start"),
            observed.map_or_else(|| "\u{2014}".into(), signed_bytes),
            match observed {
                Some(delta) if delta < 0 => theme.warning,
                Some(_) => theme.success,
                None => theme.secondary,
            },
            theme,
        ));
    panel = panel.child(figures).child(rule());

    if let Some(volume) = app.space {
        panel = panel.child(
            div()
                .flex()
                .flex_col()
                .gap(space::SM)
                .child(widgets::eyebrow("Disk", cx))
                .child(widgets::space_meter(volume, 0, cx)),
        );
    }
    if phase == Phase::Done && app.scan.is_some() {
        panel = panel.child(widgets::meter_row(
            "re-scanning",
            format!("{} files", widgets::human_count(app.progress.files)),
            0.0,
            theme.accent,
            cx,
        ));
    }
    panel.child(caption(note(app, phase), theme))
}

/// What the mode means for the figures above it.
const fn note(app: &Disktree, phase: Phase) -> &'static str {
    match (phase, app.removal_mode) {
        (Phase::Running, RemovalMode::Trash) => {
            "Moving to the trash. The space comes back once the trash is \
             emptied, so until then observed stays near zero."
        }
        (Phase::Running, RemovalMode::Permanent) => {
            "Deleting permanently; nothing removed can be recovered. Stop \
             finishes the item in hand first."
        }
        (Phase::Done, _) if app.scan.is_some() => {
            "Rescanning so the numbers on screen match the disk again."
        }
        (Phase::Done, RemovalMode::Trash) => {
            "In the trash, the space comes back once it is emptied. What \
             was removed stays in the treemap, greyed out, until you \
             rescan."
        }
        (Phase::Done, RemovalMode::Permanent) => {
            "What was removed stays in the treemap, greyed out, until you \
             rescan; it cannot be marked again."
        }
    }
}

/// Free space gained since the run started: sampled live while it runs,
/// and as the ledger measured it once it is done.
fn observed(app: &Disktree, phase: Phase) -> Option<i64> {
    match phase {
        Phase::Done => app
            .ledger
            .cohorts()
            .last()
            .and_then(crate::ledger::Cohort::observed),
        Phase::Running => {
            let before = app.run_free_before?;
            let now = app.space?.available;
            Some(now.cast_signed().saturating_sub(before.cast_signed()))
        }
    }
}

/// A large number with a small unit after it.
fn big_figure(number: String, unit: String, color: Hsla, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(space::XS)
        .child(
            div()
                .text_size(text::TITLE)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(number),
        )
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.foreground)
                .child(unit),
        )
}

/// A figure: its name, what it means, and its value on the right.
fn figure_row(
    label: &'static str,
    meaning: Option<&'static str>,
    value: String,
    color: Hsla,
    theme: &Theme,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(space::SM)
        .text_size(text::CAPTION)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(div().text_color(theme.foreground).child(label))
                .children(meaning.map(|meaning| {
                    div().text_color(theme.secondary).child(meaning)
                })),
        )
        .child(
            div()
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_color(color)
                .child(value),
        )
}

fn caption(text_: &'static str, theme: &Theme) -> Div {
    div()
        .text_size(text::CAPTION)
        .text_color(theme.secondary)
        .child(text_)
}
