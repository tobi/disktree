//! The scale this app draws with: spacing, type, icons and region widths.
//!
//! Everything is in `rem`, following the gpui-kit design guide: type, spacing,
//! controls and icons share one zoom axis, so interface zoom (`ctrl =`,
//! `ctrl -`, `ctrl 0`) keeps every relationship intact instead of only
//! enlarging text. Choose a step by what two things mean to each other, not by
//! the pixels it happens to resolve to today.
//!
//! Radius has a single tier here: Omarchy's surfaces are square, so nothing in
//! this app rounds a corner, and nested surfaces stay concentric by
//! construction.
//!
//! Pixels remain only where a value is physical: hairline borders, the
//! treemap's own geometry (laid out in viewport pixels, then scaled by the
//! view), and positions that come from the pointer.

use gpui_kit::Rems;

/// The guide's semantic spacing scale: 2, 4, 8, 12, 16, 24 and 32 px at the
/// default 16 px rem.
pub mod space {
    use super::Rems;

    /// Optical correction: an icon baseline, a compact separator.
    pub const XXS: Rems = Rems(0.125);
    /// Parts of one control: icon and label, title and its description.
    pub const XS: Rems = Rems(0.25);
    /// Closely related controls: a button group, dialog actions.
    pub const SM: Rems = Rems(0.5);
    /// One content group: columns of a row, compact form rows.
    pub const MD: Rems = Rems(0.75);
    /// Separate groups in one section, and region padding.
    pub const LG: Rems = Rems(1.0);
    /// Separate sections.
    pub const XL: Rems = Rems(1.5);
    /// A major region boundary: empty-state breathing room.
    pub const XXL: Rems = Rems(2.0);
}

/// Type steps. Omarchy's own sizes, so the app reads like its components.
pub mod text {
    use super::Rems;

    /// Metadata, key caps and tooltips.
    pub const CAPTION: Rems = Rems(0.6875);
    /// Body text and control labels.
    pub const BODY: Rems = Rems(0.75);
    /// Window, section and dialog titles.
    pub const TITLE: Rems = Rems(0.875);
    /// The app name and the selection's name.
    pub const HEADING: Rems = Rems(1.125);
    /// A figure worth reading from across the room: the free space.
    pub const FIGURE: Rems = Rems(1.625);
    /// The selection's size: the one number the panel exists to show.
    pub const DISPLAY: Rems = Rems(2.5);
}

/// Icon slots, sized with the text they sit beside.
pub mod icon {
    use super::Rems;

    pub const SM: Rems = Rems(0.75);
    pub const MD: Rems = Rems(0.875);
}

/// Region and lane widths. Each is the comfortable default for its content at
/// the default rem; they scale with zoom like everything else.
pub mod size {
    /// Minimum physical pointer target, also at the smallest UI zoom.
    pub const POINTER_TARGET: gpui_kit::Pixels = gpui_kit::px(24.0);
    /// Below this height budget, reserve a reading lane instead of repeating
    /// the color legend and long action captions in the semantic file list.
    pub const FILE_LIST_FULL_DETAILS: Rems = Rems(32.0);
    /// One text line plus padding; still 24 logical pixels at 75% zoom.
    pub const FILE_ENTRY: Rems = Rems(2.0);
    use super::Rems;

    /// A crumb's sibling menu.
    pub const SIBLING_MENU: Rems = Rems(24.0);
    pub const SIBLING_MENU_HEIGHT: Rems = Rems(32.0);
    /// The Power Efficiency menu: narrower than a sibling menu, since its
    /// names are four known words, but wide enough for Drain My Battery
    /// beside the check, bar and count.
    pub const POWER_MENU: Rems = Rems(22.0);
    /// The Treemap | List glyphs: a little shorter than wide, like a text
    /// line's x-height plus ascender, so they sit on the label's centre line.
    pub const VIEW_GLYPH_HEIGHT: Rems = Rems(0.625);
    /// The treemap glyph's tall tile: just over half its width, as the
    /// largest tile in a squarified layout tends to be.
    pub const VIEW_GLYPH_TILE: Rems = Rems(0.375);
    /// ⋮, the bar's overflow: its longest row is Depth with its stepper.
    pub const MORE_MENU: Rems = Rems(16.0);
    /// One bar of the Power Efficiency gauge.
    pub const SIGNAL_BAR: Rems = Rems(0.1875);
    /// A list row's share bar.
    pub const ROW_BAR: Rems = Rems(5.5);
    /// A legend or identity swatch.
    pub const SWATCH: Rems = Rems(0.625);
    /// A thin meter: share of the scan, the disk.
    pub const METER: Rems = Rems(0.3125);
    /// Fixed progress rows keep paths and changing counters from reflowing.
    pub const PROGRESS_ROW: Rems = Rems(1.375);
    /// The scan heading's box and its one line, in `text::FIGURE` type.
    /// It clips, so the line must hold the mono font's whole ascent and
    /// descent: `JetBrains Mono`, the usual Linux fallback, needs 1.32 em.
    pub const SCAN_STATUS: Rems = Rems(2.25);
    pub const PROGRESS_VALUE: Rems = Rems(9.0);
    /// A side panel section's header: its caption eyebrow and total on one
    /// line. Fixed, since a pinned header is pushed up by exactly this.
    pub const SECTION_HEAD: Rems = Rems(1.25);

    // Checked at build time: the headless tests cannot see a clipped glyph.
    const _: () = assert!(SCAN_STATUS.0 >= super::text::FIGURE.0 * 1.32);
    /// The Size | Files | Age choice in the settings row.
    pub const RANKING_CHOICE: Rems = Rems(11.0);
    /// The review screen's summary column.
    pub const REVIEW_SUMMARY: Rems = Rems(22.5);
    /// The review list's share-bar lane.
    pub const SHARE_LANE: Rems = Rems(6.0);
    /// The review list's size lane: right-aligned, so sizes compare.
    pub const SIZE_LANE: Rems = Rems(5.0);
    /// The cursor tooltip.
    pub const TOOLTIP: Rems = Rems(16.75);
    /// A figure column of the removal ledger: "-1023.9 GB" in caption
    /// type, with room to spare.
    pub const LEDGER_FIGURE: Rems = Rems(5.5);
    /// Source/root chooser, with room for host, path and mount labels.
    pub const SOURCE_PICKER: Rems = Rems(40.0);
    pub const SOURCE_LIST_HEIGHT: Rems = Rems(18.0);
    /// Leave a visible backdrop even at large interface zoom.
    pub const SOURCE_VIEWPORT_FRACTION: f32 = 0.92;
    /// The keyboard overlay.
    pub const HELP: Rems = Rems(32.5);
    /// A toast: PwrDisk's 330 px, a little under the family's 380 to 460,
    /// since it sits over the treemap.
    pub const TOAST: Rems = Rems(20.625);
    /// The answer's countdown strip along a toast's top edge: the family's
    /// 2 px.
    pub const TOAST_TIMER: Rems = Rems(0.125);
    /// A toast's progress track: the family's 4 px.
    pub const TOAST_TRACK: Rems = Rems(0.25);
    /// The key column of the keyboard overlay.
    pub const KEY_LANE: Rems = Rems(7.0);
    /// A document's measure (the changelog): about 90 characters.
    pub const DOCUMENT: Rems = Rems(47.5);
    /// A changelog bullet's square.
    pub const BULLET: Rems = Rems(0.25);
    /// Settings: the nav, the panes' measure and a field's label lane.
    pub const SETTINGS_NAV: Rems = Rems(12.25);
    pub const SETTINGS_PANE: Rems = Rems(47.5);
    pub const SETTINGS_LABEL: Rems = Rems(10.0);
    /// A pane's lede: the family's 56 characters or so.
    pub const SETTINGS_LEDE: Rems = Rems(36.0);
    /// A Settings nav row: the family's 28 to 30 px.
    pub const SETTINGS_NAV_ROW: Rems = Rems(1.75);
    /// The app icon in About's header: the family's 44 px, two lines of
    /// the name and build line beside it.
    pub const ABOUT_ICON: Rems = Rems(2.75);
    /// Logs: the search field's least width, so a query still reads
    /// beside its match count and chevrons.
    pub const LOG_FILTER: Rems = Rems(16.25);
    /// Logs: the line-number gutter, room for five digits.
    pub const LOG_GUTTER: Rems = Rems(3.5);
    /// Logs: one row of a line, body mono text at 1.67.
    pub const LOG_ROW: Rems = Rems(1.25);
    /// Logs: a gap row and the mark's row, a little taller than a line so
    /// they read as breaks rather than lines.
    pub const LOG_GAP: Rems = Rems(1.375);
    pub const LOG_MARK: Rems = Rems(1.75);
    /// Logs: the Context menu.
    pub const LOG_CONTEXT_MENU: Rems = Rems(10.0);
    /// Logs: the new-lines pill sits above the selection bar.
    pub const LOG_PILL_ABOVE_BAR: Rems = Rems(3.75);
    /// The strip's menu button off macOS: as tall as a window button.
    pub const MENU_BUTTON: Rems = Rems(1.625);
    /// Its dropdown: wide enough for a row and its shortcut.
    pub const APP_MENU: Rems = Rems(18.0);
    /// The review footer's Export menu: its longest row and its key.
    pub const EXPORT_MENU: Rems = Rems(14.0);
    pub const APP_MENU_HEIGHT: Rems = Rems(36.0);
}

/// The title strip across the top of the window.
///
/// Pixels, not rems: these line up with what the system draws or measures —
/// the macOS stoplights, Windows' caption buttons, a compositor's resize
/// grip — and none of those follow interface zoom. The strip's own contents
/// (the trail, the settings) stay in rems and scale as they always have.
pub mod chrome {
    use gpui_kit::{Pixels, px};

    /// The band's height. Even, so a 14 pt stoplight centres on a whole
    /// point; the divider below it is drawn under the band, not inside it.
    pub const STRIP: Pixels = px(40.);
    /// Where macOS puts the close button, from the window's top-left:
    /// 16 in, and `(40 - 14) / 2` down, so the stoplights centre on y=20.
    pub const STOPLIGHTS: (f32, f32) = (16., 13.);
    /// Room left for the stoplights: they end at x=76, and the brand keeps
    /// 20 clear of them.
    pub const STOPLIGHT_ZONE: Pixels = px(96.);
    /// A Windows caption button: the system's own 46 wide, the strip tall.
    pub const CAPTION_BUTTON: Pixels = px(46.);
    /// A painted Linux window button, square like the rest of the app.
    pub const WINDOW_BUTTON: Pixels = px(24.);
    /// The glyph inside either kind of button.
    pub const GLYPH: Pixels = px(10.);
    /// Each of the two windows in the restore glyph.
    pub const GLYPH_BEHIND: Pixels = px(8.);
    /// The close cross, a little larger: its strokes are diagonal, so at
    /// the same box it reads smaller than the square beside it.
    pub const CLOSE_GLYPH: Pixels = px(12.);
    /// The app icon beside the wordmark.
    pub const MARK: Pixels = px(20.);
    /// The wordmark, bold, and as tall as its line so it centres by cap
    /// height rather than by the font's ascent.
    pub const WORDMARK: Pixels = px(17.);
    /// The mark and the wordmark.
    pub const LOCKUP_GAP: Pixels = px(8.);
    /// The ring a client-side window keeps for its shadow and resize grip.
    pub const RESIZE_RING: Pixels = px(10.);
}

/// Interface zoom steps, as a factor of the 16 px default rem.
pub const ZOOM_STEPS: [f32; 8] =
    [0.75, 0.875, 1.0, 1.125, 1.25, 1.5, 1.75, 2.0];

/// The default rem, in pixels.
pub const BASE_REM: f32 = 16.0;

/// Below this many rems across (a narrow window, or 200% zoom of a normal
/// one) an auxiliary window drops its wordmark and Settings stacks its panes
/// above the content: a side column and the content no longer both fit.
pub const COMPACT_WINDOW_REMS: f32 = 50.0;

/// A session-wide scale: existing and newly opened auxiliary windows agree.
/// It is not persisted and never changes an operating-system preference.
struct InterfaceScale(f32);
impl gpui_kit::Global for InterfaceScale {}

pub fn sync_scale(window: &mut gpui_kit::Window, cx: &gpui_kit::App) -> bool {
    let Some(scale) = cx.try_global::<InterfaceScale>() else {
        return false;
    };
    let rem = BASE_REM * scale.0;
    if (window.rem_size().as_f32() - rem).abs() < 0.01 {
        return false;
    }
    window.set_rem_size(gpui_kit::px(rem));
    true
}

pub fn zoom_interface(
    event: &gpui_kit::KeyDownEvent,
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::App,
) -> bool {
    let key = &event.keystroke;
    if !(key.modifiers.control || key.modifiers.platform) {
        return false;
    }
    let current = window.rem_size().as_f32() / BASE_REM;
    let index = ZOOM_STEPS
        .iter()
        .position(|step| (step - current).abs() < 0.01)
        .unwrap_or(2);
    let next = match key.key.as_str() {
        "=" | "+" => (index + 1).min(ZOOM_STEPS.len() - 1),
        "-" => index.saturating_sub(1),
        "0" => 2,
        _ => return false,
    };
    cx.set_global(InterfaceScale(ZOOM_STEPS[next]));
    sync_scale(window, cx);
    cx.refresh_windows();
    true
}

/// Keyboard scrolling is explicit, so a virtual/overflow region needs no
/// pointer wheel. Offsets are clamped to the measured content's bounds.
pub fn scroll_key(
    key: &str,
    scroll: &gpui_kit::ScrollHandle,
    rem: gpui_kit::Pixels,
) -> bool {
    let mut offset = scroll.offset();
    let page = scroll.bounds().size.height * 0.8;
    offset.y = match key {
        "up" => offset.y + rem * 2.0,
        "down" => offset.y - rem * 2.0,
        "pageup" => offset.y + page,
        "pagedown" => offset.y - page,
        "home" => gpui_kit::px(0.0),
        "end" => -scroll.max_offset().y,
        _ => return false,
    }
    .clamp(
        -scroll.max_offset().y.max(gpui_kit::px(0.0)),
        gpui_kit::px(0.0),
    );
    scroll.set_offset(offset);
    true
}
