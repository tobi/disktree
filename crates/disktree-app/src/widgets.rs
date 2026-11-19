//! Small presentation pieces shared by the screens.
//!
//! Everything is built from the active Omarchy theme's tokens, and every piece
//! is a plain function over borrowed data so the screens stay readable.

use disktree_core::size::{human_bytes, human_bytes_short, share, share_bar};
use disktree_core::space::SpaceInfo;
use disktree_core::tree::{Metric, Node};
use gpui_kit::base::SelectableText;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Div, ElementId, FontWeight, Hsla, InteractiveElement as _,
    ParentElement, SharedString, Styled, div, relative,
};
use gpui_omarchy::{ActiveTheme, Status};

use crate::ui::{space, text};

use crate::state::Disktree;

/// Text a person can select and copy with Ctrl+C (⌘C).
///
/// `key` names the run among the others on screen and must not repeat
/// there, so a list row puts its index in it. The text is part of the id
/// too: when the words change, the run is a new one, and a selection never
/// outlives what it selected.
pub fn selectable(key: &str, text: impl Into<SharedString>) -> SelectableText {
    let text: SharedString = text.into();
    SelectableText::new(ElementId::Name(format!("{key}:{text}").into()), text)
}

/// The scanned root, shortened to `~` where it is the home directory.
pub fn display_root(app: &Disktree) -> String {
    crate::marks::display_path(&app.root_path, app.home.as_deref())
}

/// File counts, which is what a directory count is too.
pub fn human_count(value: u64) -> String {
    disktree_core::size::human_count(value)
}

/// The value to show for a node under the active metric.
pub fn short_value(node: &Node, metric: Metric) -> String {
    match metric {
        Metric::Bytes => human_bytes_short(node.bytes),
        Metric::Files => human_count(node.files),
    }
}

/// A dim label above a number, for the header strip.
pub fn stat(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let theme = cx.omarchy();
    let label: SharedString = label.into();
    let key = format!("stat-{label}");
    div()
        .flex()
        .flex_col()
        .gap(space::XXS)
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary.opacity(0.75))
                .child(selectable(&key, label)),
        )
        .child(
            div()
                .text_size(text::TITLE)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.bright)
                .child(selectable(&key, value)),
        )
}

/// A value with a status colour rather than the neutral one.
pub fn stat_colored(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    color: Hsla,
    cx: &App,
) -> Div {
    let theme = cx.omarchy();
    let label: SharedString = label.into();
    let key = format!("stat-{label}");
    div()
        .flex()
        .flex_col()
        .gap(space::XXS)
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary.opacity(0.75))
                .child(selectable(&key, label)),
        )
        .child(
            div()
                .text_size(text::TITLE)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(selectable(&key, value)),
        )
}

/// A square chip, used for counts and states.
pub fn chip(label: impl Into<SharedString>, color: Hsla, cx: &App) -> Div {
    let theme = cx.omarchy();
    div()
        .px(space::XS)
        .py(space::XXS)
        .border_1()
        .border_color(color.opacity(0.5))
        .bg(color.opacity(0.1))
        .text_color(color)
        .text_size(text::CAPTION)
        .font_family(theme.font.clone())
        .child(label.into())
}

/// A labelled meter: `label · bar · value`.
pub fn meter_row(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    fraction: f32,
    color: Hsla,
    cx: &App,
) -> Div {
    let theme = cx.omarchy();
    let label: SharedString = label.into();
    let key = format!("meter-{label}");
    div()
        .flex()
        .flex_col()
        .gap(space::XS)
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(div().child(selectable(&key, label)))
                .child(
                    div()
                        .text_color(theme.bright)
                        .font_weight(FontWeight::MEDIUM)
                        .child(selectable(&key, value)),
                ),
        )
        .child(
            div()
                .w_full()
                .h(space::XS)
                .bg(theme.foreground.opacity(0.08))
                .child(
                    div()
                        .h_full()
                        .w(relative(fraction.clamp(0.0, 1.0)))
                        .bg(color),
                ),
        )
}

/// The volume meter: what is used, what is free, and what the marks will free.
///
/// The projection is drawn as a separate segment so "this much comes back" is
/// visible rather than only stated.
pub fn space_meter(space: SpaceInfo, reclaiming: u64, cx: &App) -> Div {
    let theme = cx.omarchy();
    let total = space.total.max(1);
    let projected = space.after_removing(reclaiming);
    let used_now = space.used() as f32 / total as f32;
    let gained = (projected.used() as f32 / total as f32).max(0.0);
    let used_now = used_now.clamp(0.0, 1.0);
    let gained = gained.clamp(0.0, used_now);

    let label = if reclaiming > 0 {
        format!(
            "{} free · {} after removing {}",
            human_bytes(space.available),
            human_bytes(projected.available),
            human_bytes(reclaiming)
        )
    } else {
        format!(
            "{} free of {}",
            human_bytes(space.available),
            human_bytes(space.total)
        )
    };

    div()
        .flex()
        .flex_col()
        .gap(space::XS)
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(div().child(selectable("space-meter", label))),
        )
        .child(
            div()
                .relative()
                .w_full()
                .h(space::XS)
                .bg(theme.foreground.opacity(0.08))
                .child(
                    // Used space, as a bar from the left.
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(relative(used_now))
                        .bg(theme.foreground.opacity(0.22)),
                )
                .child(
                    // What stays used after the removals: the bar shrinks to
                    // here, so the gap is exactly what comes back.
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(relative(gained))
                        .bg(theme.danger.opacity(0.55)),
                )
                .child(
                    // The reclaimed slice sits at the right edge of what is
                    // currently used.
                    div()
                        .absolute()
                        .left(relative(gained))
                        .top_0()
                        .h_full()
                        .w(relative(used_now - gained))
                        .bg(theme.success.opacity(0.65)),
                ),
        )
}

/// A `key value` hint pair for the bottom bar.
pub fn hint(
    keys: impl Into<SharedString>,
    label: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let theme = cx.omarchy();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::XS)
        .child(gpui_omarchy::keycap(keys, cx))
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(selectable("hint", label)),
        )
}

/// A section heading inside a panel.
pub fn section(label: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.omarchy();
    div()
        .text_size(text::CAPTION)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.secondary.opacity(0.85))
        .child(selectable("section", label))
}

/// A definition row: label on the left, value on the right.
pub fn row(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let theme = cx.omarchy();
    let label: SharedString = label.into();
    let key = format!("row-{label}");
    div()
        .flex()
        .flex_row()
        .justify_between()
        .gap(space::SM)
        .text_size(text::CAPTION)
        .child(
            div()
                .text_color(theme.secondary)
                .child(selectable(&key, label)),
        )
        .child(
            div()
                .min_w_0()
                .text_color(theme.bright)
                .child(selectable(&key, value)),
        )
}

/// A block-glyph share bar, which reads as a bar at any font size.
pub fn glyph_bar(
    part: u64,
    total: u64,
    width: usize,
    color: Hsla,
    cx: &App,
) -> Div {
    let theme = cx.omarchy();
    div()
        .text_size(text::CAPTION)
        .font_family(theme.font.clone())
        .text_color(color)
        .child(SharedString::from(share_bar(part, total, width)))
}

/// A percentage with one decimal below ten percent.
pub fn percent(part: u64, total: u64) -> String {
    let value = share(part, total);
    if value < 9.95 {
        format!("{value:.1}%")
    } else {
        format!("{value:.0}%")
    }
}

/// The status colour for an alert.
pub fn alert_color(status: Status, cx: &App) -> Hsla {
    let theme = cx.omarchy();
    match status {
        Status::Neutral => theme.secondary,
        Status::Success => theme.success,
        Status::Warning => theme.warning,
        Status::Error => theme.danger,
    }
}

/// A clickable breadcrumb segment.
pub fn crumb(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    cx: &App,
) -> gpui_kit::Stateful<Div> {
    let theme = cx.omarchy();
    let color = if active {
        theme.bright
    } else {
        theme.secondary
    };
    div()
        .id(id)
        .px(space::XS)
        .py(space::XXS)
        .text_size(text::BODY)
        .text_color(color)
        .when(active, |this| this.font_weight(FontWeight::SEMIBOLD))
        .when(!active, |this| {
            this.hover(|style| style.bg(theme.hover_fill()))
        })
        .child(label.into())
}

/// A small uppercase label over a region or a figure.
pub fn eyebrow(label: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.omarchy();
    let label: SharedString = label.into();
    div()
        .text_size(text::CAPTION)
        .text_color(theme.secondary.opacity(0.7))
        .whitespace_nowrap()
        .child(selectable("eyebrow", label.to_uppercase()))
}

/// An eyebrow over a value, for the top bar and the selection grid.
pub fn figure(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    color: Hsla,
    cx: &App,
) -> Div {
    let label: SharedString = label.into();
    let key = format!("figure-{label}");
    div()
        .flex()
        .flex_col()
        .gap(space::XXS)
        .min_w_0()
        .child(eyebrow(label, cx))
        .child(
            div()
                .text_size(text::TITLE)
                .text_color(color)
                .whitespace_nowrap()
                .overflow_hidden()
                .child(selectable(&key, value)),
        )
}

/// A size split into number and unit, so the number can be set large:
/// `90.1 GiB` is `("90.1", "GiB")`.
pub fn split_size(text: &str) -> (String, String) {
    text.split_once(' ').map_or_else(
        || (text.to_string(), String::new()),
        |(number, unit)| (number.to_string(), unit.to_string()),
    )
}

/// How long ago a Unix time was, in the unit a person would use.
pub fn ago(now: i64, then: i64) -> String {
    if then <= 0 {
        return "unknown".to_string();
    }
    let seconds = (now - then).max(0);
    let plural = |count: i64, unit: &str| {
        if count == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{count} {unit}s ago")
        }
    };
    match seconds {
        0..60 => "just now".to_string(),
        60..3_600 => plural(seconds / 60, "minute"),
        3_600..86_400 => plural(seconds / 3_600, "hour"),
        86_400..5_184_000 => plural(seconds / 86_400, "day"),
        5_184_000..63_072_000 => plural(seconds / 2_592_000, "month"),
        _ => plural(seconds / 31_536_000, "year"),
    }
}

/// A thin bar: `fraction` of a track, in `color`.
pub fn bar(fraction: f32, color: Hsla, cx: &App) -> Div {
    let theme = cx.omarchy();
    div()
        .relative()
        .w_full()
        .h(crate::ui::size::METER)
        .bg(theme.foreground.opacity(0.08))
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .h_full()
                .w(relative(fraction.clamp(0.0, 1.0)))
                .bg(color),
        )
}

/// Four rising bars with `filled` of them lit, as a signal meter reads:
/// how hard a scan is allowed to work, without a word in the bar.
pub fn signal(filled: usize, cx: &App) -> Div {
    let theme = cx.omarchy();
    let mut bars = div()
        .flex()
        .flex_row()
        .items_end()
        .gap(space::XXS)
        .h(crate::ui::icon::SM);
    for step in 1..=SIGNAL_STEPS {
        // Heights step by quarters of the icon slot, so the tallest bar is
        // the height of the text beside it.
        let height = crate::ui::icon::SM.0 * step as f32 / SIGNAL_STEPS as f32;
        bars = bars.child(
            div()
                .w(crate::ui::size::SIGNAL_BAR)
                .h(gpui_kit::Rems(height))
                .bg(if step <= filled {
                    theme.bright
                } else {
                    theme.control_border()
                }),
        );
    }
    bars
}

/// One bar per Power Efficiency preset.
const SIGNAL_STEPS: usize = crate::power::PowerEfficiency::ALL.len();

/// A square of colour, for a legend or an identity.
pub fn swatch(color: Hsla) -> Div {
    div()
        .flex_shrink_0()
        .size(crate::ui::size::SWATCH)
        .bg(color)
}

/// The hatch swatch that stands for reclaimable space in the legend.
pub fn hatch_swatch(color: Hsla, ground: Hsla) -> Div {
    div()
        .flex_shrink_0()
        .size(crate::ui::size::SWATCH)
        .bg(ground)
        .child(
            div()
                .size_full()
                .bg(gpui_kit::pattern_slash(color, 1.0, 3.0)),
        )
}

/// A large number followed by its small unit, sharing one baseline.
///
/// Flex baseline alignment does not line up text of different sizes here,
/// so both are set on bottom-aligned boxes exactly one line tall, and the
/// unit is lifted by the difference in their descents: a font's descent is
/// close to a fifth of its size.
pub fn measure(
    number: impl Into<SharedString>,
    number_size: gpui_kit::Rems,
    unit: impl Into<SharedString>,
    unit_size: gpui_kit::Rems,
    cx: &App,
) -> Div {
    let theme = cx.omarchy();
    let lift = gpui_kit::Rems((number_size.0 - unit_size.0) * DESCENT);
    let unit: SharedString = unit.into();
    let key = format!("measure-{unit}");
    div()
        .flex()
        .flex_row()
        .items_end()
        .gap(space::SM)
        .child(
            div()
                .text_size(number_size)
                .line_height(number_size)
                .font_weight(FontWeight::BOLD)
                .text_color(theme.bright)
                .child(selectable(&key, number)),
        )
        .child(
            div()
                .text_size(unit_size)
                .line_height(unit_size)
                .pb(lift)
                .text_color(theme.secondary)
                .whitespace_nowrap()
                .child(selectable(&key, unit)),
        )
}

/// A font's descent as a share of its size, for lining up baselines.
const DESCENT: f32 = 0.2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_read_in_the_unit_a_person_would_use() {
        let now = 1_800_000_000;
        assert_eq!(ago(now, now - 5), "just now");
        assert_eq!(ago(now, now - 120), "2 minutes ago");
        assert_eq!(ago(now, now - 3_600), "1 hour ago");
        assert_eq!(ago(now, now - 19 * 86_400), "19 days ago");
        assert_eq!(ago(now, now - 90 * 86_400), "3 months ago");
        assert_eq!(ago(now, now - 800 * 86_400), "2 years ago");
        assert_eq!(ago(now, 0), "unknown");
    }

    #[test]
    fn sizes_split_into_number_and_unit() {
        assert_eq!(split_size("90.1 GiB"), ("90.1".into(), "GiB".into()));
        assert_eq!(split_size("0"), ("0".into(), String::new()));
    }
}
