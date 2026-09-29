//! What the panel says about an item beyond its size: a card for a
//! checkout, and one for a folder of worktrees, each drawn from a reading
//! that arrives after the pointer has settled.

use std::path::PathBuf;

use disktree_core::checkout::{Checkout, Head, Landed, Reading, Upstream};
use disktree_core::details::{CheckoutItem, CheckoutKind, Detail};
use disktree_core::size::human_bytes;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, ElementId, FontWeight, Hsla,
    InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div,
};
use gpui_omarchy::{IconName, Theme, with_tooltip};

use crate::palette;
use crate::state::{Disktree, now_seconds};
use crate::ui::{icon, size, space, text};
use crate::widgets::{self, human_count};

/// One section per detail of the item keys act on.
pub fn detail_sections(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Vec<Div> {
    app.panel_details()
        .into_iter()
        .map(|detail| match detail {
            Detail::Checkout(item) => checkout_card(app, &item, theme, cx),
            Detail::Worktrees { repository, items } => {
                worktrees_card(app, &repository, &items, theme, cx)
            }
        })
        .collect()
}

fn caption(content: impl Into<gpui_kit::SharedString>, theme: &Theme) -> Div {
    div()
        .text_size(text::CAPTION)
        .text_color(theme.secondary)
        .child(content.into())
}

/// A caption on one line, cut short where it runs out of room.
fn line(content: impl Into<gpui_kit::SharedString>, theme: &Theme) -> Div {
    caption(content, theme)
        .min_w_0()
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        human_count(count as u64),
        if count == 1 { one } else { many }
    )
}

const fn kind_label(kind: &CheckoutKind) -> &'static str {
    match kind {
        CheckoutKind::Repository => "Git repository",
        CheckoutKind::Worktree { locked: true, .. } => "Locked git worktree",
        CheckoutKind::Worktree { .. } => "Git worktree",
        CheckoutKind::Submodule => "Git submodule",
        CheckoutKind::Orphaned => "Orphaned git worktree",
        CheckoutKind::Bare => "Bare git repository",
    }
}

fn checkout_card(
    app: &Disktree,
    item: &CheckoutItem,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let reading = app.checkouts.get(&item.item);
    let reread = item.clone();
    let secondary = theme.secondary;
    let bright = theme.bright;
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .child(widgets::eyebrow(kind_label(&item.kind), cx))
        .child(div().flex_1())
        .when(reading.is_some(), |this| {
            this.child(with_tooltip(
                div()
                    .id("checkout-reread")
                    .debug_selector(|| "checkout-reread".into())
                    .text_color(secondary)
                    .hover(move |style| style.text_color(bright))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.reread(&reread, cx);
                        window.focus(&this.focus, cx);
                    }))
                    .child(
                        gpui_omarchy::icon(IconName::RotateCw).size(icon::SM),
                    ),
                "Read git again",
            ))
        });
    let section = div().flex().flex_col().gap(space::MD).child(header);
    match reading {
        Some(Reading::Done(checkout)) => {
            section.children(checkout_details(app, item, checkout, theme, cx))
        }
        Some(Reading::Failed(reason)) => section
            .child(caption(format!("Git could not read it: {reason}"), theme)),
        None => section.child(caption("Reading git\u{2026}", theme)),
    }
}

fn head_label(head: &Head) -> String {
    match head {
        Head::Branch(name) => name.clone(),
        Head::Detached(sha) => {
            format!("Detached at {}", sha.chars().take(9).collect::<String>())
        }
        Head::Unborn => "No commits yet".into(),
    }
}

fn checkout_details(
    app: &Disktree,
    item: &CheckoutItem,
    checkout: &Checkout,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Vec<AnyElement> {
    let mut parts: Vec<AnyElement> = Vec::new();
    if let Some(head) = &checkout.head {
        let mut place = Vec::new();
        if let CheckoutKind::Worktree { repository, .. } = &item.kind {
            place.push(format!(
                "of {}",
                crate::marks::display_path(repository, app.home.as_deref())
            ));
        }
        if !checkout.refs_here.is_empty() {
            let refs: Vec<&str> = checkout
                .refs_here
                .iter()
                .take(2)
                .map(String::as_str)
                .collect();
            place.push(format!("at {}", refs.join(", ")));
        }
        parts.push(
            div()
                .flex()
                .flex_col()
                .gap(space::XS)
                .min_w_0()
                .child(
                    div()
                        .text_size(text::TITLE)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.bright)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(head_label(head)),
                )
                .when(!place.is_empty(), |this| {
                    this.child(line(place.join(" \u{00b7} "), theme))
                })
                .into_any_element(),
        );
        parts.push(landing(checkout, theme, cx).into_any_element());
        parts.push(figures(checkout, theme, cx).into_any_element());
        parts.extend(
            lists(checkout, theme, cx)
                .into_iter()
                .map(Div::into_any_element),
        );
    } else if checkout.worktrees > 0 {
        let gone = if checkout.prunable > 0 {
            format!(", {} with their folders gone", checkout.prunable)
        } else {
            String::new()
        };
        parts.push(
            caption(
                format!(
                    "{}{gone}",
                    plural(checkout.worktrees, "worktree", "worktrees")
                ),
                theme,
            )
            .into_any_element(),
        );
    }
    let verdict = div().flex().flex_col().gap(space::XS).children(
        verdict(app, item, checkout, theme).into_iter().map(
            |(words, color)| {
                div()
                    .text_size(text::CAPTION)
                    .text_color(color)
                    .child(words)
            },
        ),
    );
    parts.push(verdict.into_any_element());
    parts
}

/// Whether its work is on the base, as a chip, and what the chip cannot
/// say beneath it.
fn landing(
    checkout: &Checkout,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let base = checkout.base.as_deref().unwrap_or("the base");
    let (label, color) = match &checkout.landed {
        Landed::Merged => (format!("Merged into {base}"), theme.success),
        Landed::OnBase => (format!("Changes already on {base}"), theme.success),
        Landed::Squashed { commit, .. } => {
            (format!("Squash-merged as {commit}"), theme.success)
        }
        Landed::NotLanded | Landed::Unfinished => (
            format!(
                "Not merged \u{00b7} {}",
                plural(checkout.commit_count, "commit", "commits")
            ),
            theme.warning,
        ),
        Landed::NoBase => ("Nothing to compare with".into(), theme.secondary),
        Landed::Unknown(_) => ("Merge status unknown".into(), theme.secondary),
    };
    let chips = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(space::XS)
        .child(widgets::chip(label, color, cx))
        .when(matches!(checkout.upstream, Upstream::Gone(_)), |this| {
            this.child(widgets::chip(
                "Remote branch deleted",
                theme.secondary,
                cx,
            ))
        });
    let note = match &checkout.landed {
        Landed::Squashed { subject, .. } => Some(subject.clone()),
        Landed::Unfinished => Some(
            "Stopped looking for a squash merge before finding one.".into(),
        ),
        Landed::Unknown(reason) if !reason.is_empty() => Some(reason.clone()),
        _ => None,
    };
    div()
        .flex()
        .flex_col()
        .gap(space::XS)
        .child(chips)
        .children(note.map(|note| line(note, theme)))
}

fn figures(
    checkout: &Checkout,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let now = now_seconds();
    let files = plural(checkout.change_count, "file", "files");
    let uncommitted = if checkout.change_count == 0 {
        ("Clean".to_string(), theme.success)
    } else if checkout.unsaved_count == 0 {
        (format!("{files} deleted"), theme.bright)
    } else {
        (files, theme.warning)
    };
    let unpushed = match &checkout.upstream {
        Upstream::Absent => ("No upstream".to_string(), theme.bright),
        Upstream::Gone(_) => ("Upstream gone".to_string(), theme.bright),
        Upstream::Tracking { unpushed: 0, .. } => {
            ("None".to_string(), theme.bright)
        }
        Upstream::Tracking { unpushed, .. } => {
            (plural(*unpushed, "commit", "commits"), theme.warning)
        }
    };
    let cell = |label: &'static str, (value, color): (String, Hsla)| {
        div()
            .flex_1()
            .min_w_0()
            .child(widgets::figure(label, value, color, cx))
    };
    let row = |left: Div, right: Div| {
        div().flex().flex_row().child(left).child(right)
    };
    div()
        .flex()
        .flex_col()
        .gap(space::MD)
        .child(row(
            cell("Uncommitted", uncommitted),
            cell("Unpushed", unpushed),
        ))
        .child(row(
            cell(
                "Last commit",
                (
                    widgets::ago(now, checkout.last_commit.unwrap_or(0)),
                    theme.bright,
                ),
            ),
            cell(
                "Fetched",
                (
                    checkout.base_fetched.map_or_else(
                        || "never".into(),
                        |time| widgets::ago(now, time),
                    ),
                    theme.bright,
                ),
            ),
        ))
}

/// A heading with a count, the first rows, and how many more there are.
fn detail_list(
    title: &str,
    total: usize,
    rows: Vec<Div>,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let shown = rows.len();
    let heading = if total > 1 {
        format!("{title} \u{00b7} {}", human_count(total as u64))
    } else {
        title.to_owned()
    };
    div()
        .flex()
        .flex_col()
        .gap(space::XS)
        .child(widgets::eyebrow(heading, cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::XXS)
                .text_size(text::CAPTION)
                .children(rows),
        )
        .when(total > shown, |this| {
            this.child(caption(
                format!("+{} more", human_count((total - shown) as u64)),
                theme,
            ))
        })
}

fn lists(
    checkout: &Checkout,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Vec<Div> {
    let now = now_seconds();
    let base = checkout.base.as_deref().unwrap_or("the base");
    let pair = |left: Div, right: Div| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::SM)
            .min_w_0()
            .child(left)
            .child(div().flex_1())
            .child(right.flex_shrink_0())
    };
    let mut lists = Vec::new();
    if !checkout.changes.is_empty() {
        let rows = checkout
            .changes
            .iter()
            .map(|change| {
                div()
                    .flex()
                    .flex_row()
                    .gap(space::SM)
                    .min_w_0()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(if change.is_unsaved() {
                                theme.warning
                            } else {
                                theme.secondary
                            })
                            .child(change.code.replace('.', " ")),
                    )
                    .child(
                        line(change.path.clone(), theme)
                            .text_color(theme.foreground),
                    )
            })
            .collect();
        lists.push(detail_list(
            "Uncommitted",
            checkout.change_count,
            rows,
            theme,
            cx,
        ));
    }
    if !checkout.commits.is_empty() {
        let rows = checkout
            .commits
            .iter()
            .map(|commit| {
                pair(
                    line(commit.subject.clone(), theme)
                        .text_color(theme.foreground),
                    caption(widgets::ago(now, commit.time), theme),
                )
            })
            .collect();
        lists.push(detail_list(
            &format!("Not on {base}"),
            checkout.commit_count,
            rows,
            theme,
            cx,
        ));
    }
    if !checkout.earlier.is_empty() {
        let rows = checkout
            .earlier
            .iter()
            .map(|earlier| {
                let (state, color) = if earlier.landed {
                    ("merged".to_string(), theme.success)
                } else {
                    (format!("not seen on {base}"), theme.secondary)
                };
                pair(
                    line(earlier.branch.clone(), theme)
                        .text_color(theme.foreground),
                    caption(state, theme).text_color(color),
                )
            })
            .collect();
        lists.push(detail_list(
            "Checked out here before",
            checkout.earlier.len(),
            rows,
            theme,
            cx,
        ));
    }
    lists
}

/// What removing the folder would take with it. A worktree's branches and
/// commits stay in its repository; the repository takes everything.
fn verdict(
    app: &Disktree,
    item: &CheckoutItem,
    checkout: &Checkout,
    theme: &Theme,
) -> Vec<(String, Hsla)> {
    match &item.kind {
        CheckoutKind::Repository | CheckoutKind::Bare => {
            let also = if checkout.worktrees > 0 {
                format!(
                    ", and its {} stop working",
                    plural(checkout.worktrees, "worktree", "worktrees")
                )
            } else {
                String::new()
            };
            vec![(
                format!("Removing it removes every branch and commit{also}."),
                theme.danger,
            )]
        }
        CheckoutKind::Submodule => vec![(
            "A submodule belongs to its parent checkout; remove it with git."
                .into(),
            theme.warning,
        )],
        CheckoutKind::Orphaned => vec![(
            "Its repository is gone, so git cannot say what is in it.".into(),
            theme.warning,
        )],
        CheckoutKind::Worktree { repository, locked } => {
            let mut lines = Vec::new();
            if *locked {
                lines.push((
                    "Locked with git worktree lock.".into(),
                    theme.warning,
                ));
            }
            if let Some(operation) = checkout.operation {
                lines.push((
                    format!("A {operation} is in progress."),
                    theme.warning,
                ));
            }
            if checkout.lost > 0 {
                let verb = if checkout.lost == 1 { "is" } else { "are" };
                lines.push((
                    format!(
                        "{} {verb} on no branch; removing it loses them.",
                        plural(checkout.lost, "commit", "commits")
                    ),
                    theme.danger,
                ));
            }
            if checkout.unsaved_count > 0 {
                lines.push((
                    format!(
                        "Removing it loses {}.",
                        plural(
                            checkout.unsaved_count,
                            "uncommitted change",
                            "uncommitted changes"
                        )
                    ),
                    theme.danger,
                ));
            }
            if !checkout.leftovers.is_empty() {
                let names: Vec<String> = checkout
                    .leftovers
                    .iter()
                    .take(3)
                    .map(|path| short_name(path))
                    .collect();
                let more = checkout.leftovers.len().saturating_sub(3);
                let more = if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                };
                lines.push((
                    format!(
                        "Also goes, and git keeps no copy: {}{more}.",
                        names.join(", ")
                    ),
                    theme.warning,
                ));
            }
            if checkout.loses_nothing() {
                lines.push((
                    format!(
                        "Removing it loses nothing: its branch and commits \
                         stay in {}. git worktree prune then drops its entry.",
                        crate::marks::display_path(
                            repository,
                            app.home.as_deref()
                        )
                    ),
                    theme.success,
                ));
            }
            lines
        }
    }
}

/// A leftover's last part, keeping the slash that says it is a folder.
fn short_name(leftover: &str) -> String {
    let trimmed = leftover.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if leftover.ends_with('/') {
        format!("{name}/")
    } else {
        name.to_owned()
    }
}

fn worktrees_card(
    app: &Disktree,
    repository: &std::path::Path,
    items: &[CheckoutItem],
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let read = items
        .iter()
        .filter(|item| app.checkouts.get(&item.item).is_some())
        .count();
    let safe = app.merged_and_clean(items);
    let safe_bytes: u64 = safe.iter().map(|item| item.bytes).sum();
    let unmarked: Vec<PathBuf> = safe
        .iter()
        .filter(|item| {
            !app.marks.contains(&item.item)
                && app.marked_ancestor(&item.item).is_none()
        })
        .map(|item| item.item.clone())
        .collect();
    let largest = items.first().map_or(1, |item| item.bytes);
    let highlight = palette::highlight(theme);
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .child(widgets::eyebrow(
            format!("Worktrees \u{00b7} {}", items.len()),
            cx,
        ))
        .child(div().flex_1())
        .when(!safe.is_empty(), |this| {
            this.child(
                div()
                    .text_size(text::CAPTION)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(highlight)
                    .whitespace_nowrap()
                    .child(format!(
                        "{} merged and clean \u{00b7} {}",
                        safe.len(),
                        human_bytes(safe_bytes)
                    )),
            )
        });
    let reading = if read < items.len() {
        format!(" \u{00b7} reading {read} of {}\u{2026}", items.len())
    } else {
        String::new()
    };
    let origin = format!(
        "of {}{reading}",
        crate::marks::display_path(repository, app.home.as_deref())
    );
    let mut section = div()
        .flex()
        .flex_col()
        .gap(space::MD)
        .child(header)
        .child(line(origin, theme));
    if !unmarked.is_empty() {
        let on = palette::on_highlight(theme);
        let count = unmarked.len();
        section = section.child(with_tooltip(
            div()
                .id("mark-merged-worktrees")
                .debug_selector(|| "mark-merged-worktrees".into())
                .flex()
                .justify_center()
                .px(space::MD)
                .py(space::SM)
                .bg(highlight)
                .text_color(on)
                .font_weight(FontWeight::SEMIBOLD)
                .hover(move |style| style.bg(highlight.opacity(0.85)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.mark_worktrees(&unmarked, cx);
                    window.focus(&this.focus, cx);
                }))
                .child(format!("Mark {count} merged and clean")),
            "Mark the worktrees whose work is on the base and that hold \
             nothing git lacks",
        ));
    }
    let active = app.action_target().and_then(|crumbs| app.path_at(&crumbs));
    for (index, item) in items.iter().enumerate() {
        section = section.child(worktree_row(
            app,
            index,
            item,
            active.as_deref() == Some(item.item.as_path()),
            largest,
            theme,
            cx,
        ));
    }
    section
}

fn worktree_row(
    app: &Disktree,
    index: usize,
    item: &CheckoutItem,
    active: bool,
    largest: u64,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> impl gpui_kit::IntoElement {
    let (branch, state, color) =
        worktree_state(app.checkouts.get(&item.item), theme);
    let name = item
        .item
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let detail = branch.map_or_else(
        || state.clone(),
        |branch| format!("{branch} \u{00b7} {state}"),
    );
    let path = item.item.clone();
    let hover = theme.hover_fill();
    div()
        .id(ElementId::NamedInteger("worktree".into(), index as u64))
        .debug_selector(move || format!("worktree-{index}"))
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .px(space::SM)
        .py(space::XS)
        .when(active, |this| this.bg(hover))
        .hover(move |style| style.bg(hover))
        .on_click(cx.listener(move |this, _, window, cx| {
            if let Some(crumbs) = this.crumbs_for_path(&path) {
                this.reveal(crumbs, cx);
            }
            window.focus(&this.focus, cx);
        }))
        .child(
            div()
                .flex_shrink_0()
                .size(space::SM)
                .rounded_full()
                .bg(color),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_size(text::BODY)
                        .text_color(theme.bright)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(name),
                )
                .child(line(detail, theme)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_end()
                .gap(space::XS)
                .flex_shrink_0()
                .w(size::ROW_BAR)
                .child(
                    div()
                        .text_size(text::BODY)
                        .text_color(theme.bright)
                        .child(human_bytes(item.bytes)),
                )
                .child(widgets::bar(
                    item.bytes as f32 / largest.max(1) as f32,
                    color,
                    cx,
                )),
        )
}

/// A row's branch, what its reading means for removal, and its colour. The
/// branch gives way first when the row is narrow.
fn worktree_state(
    reading: Option<&Reading<Checkout>>,
    theme: &Theme,
) -> (Option<String>, String, Hsla) {
    match reading {
        None => (None, "reading\u{2026}".into(), theme.secondary.opacity(0.5)),
        Some(Reading::Failed(reason)) => {
            (None, reason.clone(), theme.secondary)
        }
        Some(Reading::Done(checkout)) => {
            let branch = match &checkout.head {
                Some(Head::Branch(name)) => name.clone(),
                Some(Head::Detached(sha)) => {
                    format!(
                        "detached {}",
                        sha.chars().take(9).collect::<String>()
                    )
                }
                Some(Head::Unborn) | None => "no commits".into(),
            };
            if checkout.loses_nothing() && checkout.is_landed() {
                return (Some(branch), "merged, clean".into(), theme.success);
            }
            let mut parts = vec![if checkout.is_landed() {
                "merged".to_string()
            } else {
                "not merged".to_string()
            }];
            if checkout.unsaved_count > 0 {
                parts.push(format!(
                    "{} uncommitted",
                    human_count(checkout.unsaved_count as u64)
                ));
            }
            if checkout.lost > 0 {
                parts.push(format!("{} on no branch", checkout.lost));
            }
            if matches!(
                checkout.kind,
                CheckoutKind::Worktree { locked: true, .. }
            ) {
                parts.push("locked".into());
            }
            if let Some(operation) = checkout.operation {
                parts.push(format!("mid-{operation}"));
            }
            if !checkout.leftovers.is_empty() {
                parts.push("holds more".into());
            }
            (Some(branch), parts.join(" \u{00b7} "), theme.warning)
        }
    }
}

/// Advice only, on a review row: what git says about a marked checkout,
/// read again when the review opened.
pub fn review_chip(
    app: &Disktree,
    path: &std::path::Path,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Option<Div> {
    let item = app.marked_checkout(path)?;
    let (label, color) = match app.checkouts.get(&item.item) {
        None => ("Reading git\u{2026}".to_string(), theme.secondary),
        Some(Reading::Failed(_)) => {
            ("Git could not read it".into(), theme.secondary)
        }
        Some(Reading::Done(checkout)) => match &checkout.kind {
            CheckoutKind::Repository | CheckoutKind::Bare => {
                ("Whole repository".into(), theme.danger)
            }
            CheckoutKind::Submodule => ("Submodule".into(), theme.warning),
            CheckoutKind::Orphaned => {
                ("Orphaned worktree".into(), theme.warning)
            }
            CheckoutKind::Worktree { locked, .. } => {
                if checkout.unsaved_count > 0 {
                    (
                        format!(
                            "{} uncommitted",
                            human_count(checkout.unsaved_count as u64)
                        ),
                        theme.danger,
                    )
                } else if checkout.lost > 0 {
                    (format!("{} on no branch", checkout.lost), theme.danger)
                } else if *locked {
                    ("Locked".into(), theme.warning)
                } else if let Some(operation) = checkout.operation {
                    let mut letters = operation.chars();
                    let capital: String = letters
                        .next()
                        .map(|first| {
                            first.to_uppercase().chain(letters).collect()
                        })
                        .unwrap_or_default();
                    (format!("{capital} in progress"), theme.warning)
                } else if !checkout.loses_nothing() {
                    ("Holds what git lacks".into(), theme.warning)
                } else if checkout.is_landed() {
                    ("Merged \u{00b7} clean".into(), theme.success)
                } else {
                    ("Not merged".into(), theme.warning)
                }
            }
        },
    };
    Some(widgets::chip(label, color, cx))
}
