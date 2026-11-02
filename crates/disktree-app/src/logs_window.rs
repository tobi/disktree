//! Help › Logs: `main.log`, live, in its own window. The family's viewer,
//! as PwrRoute has it: search that filters (with context and gap rows) or
//! highlights, previous and next match, the level toggles with Error and
//! Warning counts, Wrap with a hanging indent, Mark and Select since mark,
//! Live / Paused with a "new lines" pill, line selection in the number
//! gutter with Copy (and the diagnostics report, when asked), and the
//! file's Copy path and Reveal. Wrap, the search mode, Context, Include
//! diagnostics and the levels but Debug are remembered in `logs-view.json`
//! in the settings folder.
//!
//! The rows are a GPUI `list`, measured row by row: a wrapped line is as
//! tall as its text. GPUI's text has no hanging indent, so a wrapped line
//! is cut into rows here ([`LogsWindow::segments`]) and each continuation
//! is drawn indented. Unwrapped, every row is one line high and the text
//! scrolls sideways under a fixed gutter, so no line is ever clipped.
//!
//! The log never carries a scanned path (`diag_log`), so nothing here has
//! one to hide, and nothing here writes a line anywhere but the clipboard.
//! The model, parsing and the file reads are `log_view.rs`.

use crate::semantics::AccessibleButton as _;
use disktree_core::path_style::PathStyle;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity,
    FocusHandle, Focusable as _, Font, FontWeight, HighlightStyle, Hsla,
    IndentAdjustment, InteractiveElement as _, IntoElement, KeyDownEvent,
    LineFragment, ListAlignment, ListOffset, ListState, MouseButton,
    MouseDownEvent, ParentElement as _, Pixels, Rems, Render, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement as _, Styled as _, StyledText,
    Window, WindowHandle, deferred, div, list, px,
};
use gpui_omarchy::{
    ActiveTheme as _, ButtonVariant, IconName, Theme, button, icon,
    with_tooltip,
};
use serde::{Deserialize, Serialize};

use crate::aux_window::{self, Spec};
use crate::chrome::Drags;
use crate::log_view::{
    CONTEXT_CHOICES, Level, LogLine, LogView, Read, Row, RowKey, SearchMode,
    Shown, Tail, thousands,
};
use crate::ui::{icon as icon_size, size, space, text};

/// How often the file is read for new lines.
const POLL_EVERY: Duration = Duration::from_millis(500);
/// How long a status note ("Copied 3 lines") stays.
const NOTE_FOR: Duration = Duration::from_secs(3);
/// Rows kept above a line the window scrolls to (a match, the line Esc
/// returns to), so what led to it shows.
const REVEAL_ABOVE: usize = 2;

/// What the reader is and which keys it takes, for assistive technology.
const READER_LABEL: &str = "Logs; Tab moves through controls; Escape \
     leaves search; Up/Down selects lines, Shift extends selection, \
     Home/End reaches either end, PageUp/PageDown scroll within wrapped \
     lines; Command or Control C copies selected lines";
/// Where the view's settings are kept, in the settings folder.
const PREFS_FILE: &str = "logs-view.json";

pub const SEARCH_ID: &str = "logs-search";
pub const LIVE_ID: &str = "logs-live";
pub const COPY_ID: &str = "logs-copy";
pub const COPY_PATH_ID: &str = "logs-copy-path";
pub const REVEAL_ID: &str = "logs-reveal";
pub const PILL_ID: &str = "logs-new-lines";
pub const FILTER_ID: &str = "logs-mode-filter";
pub const HIGHLIGHT_ID: &str = "logs-mode-highlight";
pub const CONTEXT_ID: &str = "logs-context";
pub const WRAP_ID: &str = "logs-wrap";
pub const MARK_ID: &str = "logs-mark";
pub const PREV_ID: &str = "logs-prev-match";
pub const NEXT_ID: &str = "logs-next-match";
pub const SINCE_MARK_ID: &str = "logs-select-since-mark";
pub const CLEAR_MARK_ID: &str = "logs-clear-mark";
pub const DIAGNOSTICS_ID: &str = "logs-include-diagnostics";

/// Wide enough for the toolbar on one row with Context showing.
const SPEC: Spec = Spec {
    title: "PwrDisk Logs",
    size: (1120.0, 720.0),
    min: (880.0, 420.0),
};

/// The status bar's hint while a search is active.
const SEARCH_HINT: &str =
    "Enter next \u{b7} Shift-Enter previous \u{b7} Esc clears";

/// What `logs-view.json` keeps: never Debug, never the search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LogsPrefs {
    pub wrap: bool,
    pub search_mode: SearchMode,
    pub context_lines: usize,
    pub include_diagnostics: bool,
    pub error: bool,
    pub warning: bool,
    pub info: bool,
}

impl Default for LogsPrefs {
    fn default() -> Self {
        Self {
            // No line is clipped until someone asks for it.
            wrap: true,
            search_mode: SearchMode::Filter,
            context_lines: 0,
            include_diagnostics: false,
            error: true,
            warning: true,
            info: true,
        }
    }
}

impl LogsPrefs {
    /// `path`'s settings; a missing or unreadable file is the defaults.
    pub fn load(path: Option<&Path>) -> Self {
        path.and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .map(|mut prefs| {
                // An edited file cannot ask for a Context the menu lacks.
                if !CONTEXT_CHOICES.contains(&prefs.context_lines) {
                    prefs.context_lines = 0;
                }
                prefs
            })
            .unwrap_or_default()
    }

    fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)?;
        text.push('\n');
        std::fs::write(path, text)
    }
}

/// Opens Logs, or brings it forward.
pub fn open(cx: &mut App) {
    let existing = aux_window::find::<LogsWindow>(cx, |_| true);
    if aux_window::activate(existing, cx) {
        return;
    }
    let prefs = crate::updater::settings_dir().map(|dir| dir.join(PREFS_FILE));
    open_window(crate::diag_log::path().map(Path::to_path_buf), prefs, cx);
}

/// A new Logs window over `path` (`None`: there is no log file), keeping
/// its view in `prefs`; for `open` and the tests.
pub fn open_window(
    path: Option<PathBuf>,
    prefs: Option<PathBuf>,
    cx: &mut App,
) -> Option<WindowHandle<LogsWindow>> {
    aux_window::open(&SPEC, cx, move |window, cx| {
        let view = LogsWindow::new(path, prefs, window, cx);
        view.search
            .update(cx, |search, cx| search.focus(window, cx));
        view
    })
}

/// The window's view.
pub struct LogsWindow {
    pub view: LogView,
    tail: Option<Tail>,
    /// Why there are no lines to show, when there are none.
    pub problem: Option<String>,
    /// Where the view's settings are saved; `None` keeps them for now.
    prefs_path: Option<PathBuf>,
    search: Entity<InputState>,
    /// The search as last applied.
    query: String,
    focus: FocusHandle,
    list: ListState,
    /// The rows the list was last told about, and the model's generation
    /// they came from.
    list_keys: Vec<RowKey>,
    list_generation: Option<u64>,
    /// New lines scroll into view.
    pub live: bool,
    /// Lines and errors that came in while paused.
    pub new_lines: usize,
    new_errors: usize,
    /// Selected lines, by number (`LogView::first_number` counting): the
    /// anchor and the other end.
    pub selection: Option<(usize, usize)>,
    /// The selection's far end moves with new lines: Select since mark,
    /// or a Shift-click that reached the newest line. A plain click or
    /// Select All stays put.
    follows_tail: bool,
    pub include_diagnostics: bool,
    /// Long lines wrap; off, they scroll sideways.
    pub wrap: bool,
    /// With Wrap off: how far the text is scrolled left, and how far it can
    /// go, in pixels the pointer moved.
    pub h_offset: f32,
    h_max: f32,
    /// The mono face's advance and the room a line's text has, measured
    /// on the last frame: what [`Self::segments`] wraps to.
    pub char_width: f32,
    pub text_width: f32,
    context_open: bool,
    /// Whether the Context menu was open when the press that may close it
    /// began, so the press on its button does not open it again.
    context_was_open: bool,
    note: Option<(SharedString, u64)>,
    note_generation: u64,
    /// What the next frame scrolls to, once the list has the new rows.
    pending_reveal: Reveal,
    strip_drag: bool,
}

impl Drags for LogsWindow {
    fn strip_drag(&mut self) -> &mut bool {
        &mut self.strip_drag
    }
}

impl LogsWindow {
    pub fn new(
        path: Option<PathBuf>,
        prefs_path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let prefs = LogsPrefs::load(prefs_path.as_deref());
        // Debug is never remembered: every window opens without it.
        let view = LogView::with(
            [prefs.error, prefs.warning, prefs.info, false],
            prefs.search_mode,
            prefs.context_lines,
        );
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search logs")
                .clean_on_escape()
        });
        cx.subscribe(&search, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => {
                let query = this.search.read(cx).value().to_string();
                this.query_changed(&query);
                cx.notify();
            }
            InputEvent::PressEnter { shift, .. } => this.step(!shift, cx),
            InputEvent::Focus | InputEvent::Blur => {}
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_EVERY).await;
                if this.update(cx, Self::poll).is_err() {
                    break;
                }
            }
        })
        .detach();
        let mut this = Self {
            view,
            tail: path.map(Tail::new),
            problem: None,
            prefs_path,
            search,
            query: String::new(),
            focus: cx.focus_handle().tab_stop(true),
            list: ListState::new(0, ListAlignment::Top, px(240.0)),
            list_keys: Vec::new(),
            list_generation: None,
            live: true,
            new_lines: 0,
            new_errors: 0,
            selection: None,
            follows_tail: false,
            include_diagnostics: prefs.include_diagnostics,
            wrap: prefs.wrap,
            h_offset: 0.0,
            h_max: 0.0,
            char_width: 7.2,
            text_width: 800.0,
            context_open: false,
            context_was_open: false,
            note: None,
            note_generation: 0,
            pending_reveal: Reveal::None,
            strip_drag: false,
        };
        this.reload(window);
        this
    }

    /// Reads everything again.
    fn reload(&mut self, window: &Window) {
        let Some(tail) = &mut self.tail else {
            self.problem = Some(
                "No log: PwrDisk could not create its log folder, so this \
                 session writes only to the terminal."
                    .to_owned(),
            );
            return;
        };
        match tail.read_all() {
            Ok(text) => {
                self.view.replace(&text);
                self.problem = None;
            }
            Err(error) => {
                self.problem =
                    Some(format!("Could not read the log: {error}."));
            }
        }
        self.sync_list(window);
        self.list.scroll_to_end();
    }

    /// Reads what the file gained since the last read; a rotation reads
    /// it all again.
    pub fn poll(&mut self, cx: &mut Context<'_, Self>) {
        let Some(tail) = &mut self.tail else {
            return;
        };
        // Not there yet (nothing logged since a rotation): keep what is
        // shown.
        let Ok(read) = tail.poll() else {
            return;
        };
        match read {
            Read::Nothing => return,
            Read::All(text) => {
                self.view.replace(&text);
                self.problem = None;
                self.selection = None;
                self.follows_tail = false;
                self.new_lines = 0;
                self.new_errors = 0;
                // Numbers start again at 1 for other text, so a row's key
                // no longer names what the list measured: start it over.
                self.list_keys.clear();
            }
            Read::More(text) => {
                self.problem = None;
                let newest = self.view.newest();
                let before = self.view.lines.len() + self.view.first_number;
                let errors = self.view.append(&text);
                if !self.live {
                    self.new_lines +=
                        self.view.lines.len() + self.view.first_number - before;
                    self.new_errors += errors;
                }
                self.follow_selection(newest);
            }
        }
        cx.notify();
    }

    /// A following selection that ended on the newest line grows with the
    /// lines that came after it, so Copy at the end of a repro takes them
    /// too.
    const fn follow_selection(&mut self, newest_before: Option<usize>) {
        let (true, Some((anchor, end)), Some(before), Some(newest)) = (
            self.follows_tail,
            self.selection,
            newest_before,
            self.view.newest(),
        ) else {
            return;
        };
        if end == before {
            self.selection = Some((anchor, newest));
        }
    }

    /// A row's height before it is measured: one line of text.
    fn row_height(window: &Window) -> Pixels {
        window.rem_size() * size::LOG_ROW.0
    }

    /// Tells the list which rows changed since it last drew: lines dropped
    /// from the front, then what differs between the rows both share at
    /// the start and the end. Rows it already measured keep their height.
    fn sync_list(&mut self, window: &Window) {
        let generation = self.view.generation();
        if self.list_generation == Some(generation) {
            return;
        }
        self.list_generation = Some(generation);
        let keys: Vec<RowKey> = self
            .view
            .rows()
            .iter()
            .map(|row| self.view.row_key(*row))
            .collect();
        if keys == self.list_keys {
            return;
        }
        let mut old = std::mem::take(&mut self.list_keys);
        // Paused, the reader stays on the line at the top (or the next one
        // still shown), whatever was spliced around or under it.
        let scroll_top = self.list.logical_scroll_top();
        let top = (!self.live)
            .then(|| {
                old.get(scroll_top.item_ix.min(old.len())..)?
                    .iter()
                    .find_map(|key| match key {
                        RowKey::Line { number, .. } => Some(*number),
                        _ => None,
                    })
            })
            .flatten();
        if let (Some(first), Some(old_first)) = (keys.first(), old.first())
            && first != old_first
            && let Some(dropped) = old.iter().position(|key| key == first)
        {
            self.list.splice(0..dropped, 0);
            old.drain(..dropped);
        }
        let prefix = old.iter().zip(&keys).take_while(|(a, b)| a == b).count();
        let room = old.len().min(keys.len()) - prefix;
        let suffix = old
            .iter()
            .rev()
            .zip(keys.iter().rev())
            .take(room)
            .take_while(|(a, b)| a == b)
            .count();
        if prefix == 0 && suffix == 0 {
            self.list.reset_with_uniform_height(
                keys.len(),
                Self::row_height(window),
            );
        } else {
            self.list.splice(
                prefix..old.len() - suffix,
                keys.len() - prefix - suffix,
            );
        }
        if let Some(top) = top
            && let Some(row) = keys.iter().position(
                |key| matches!(key, RowKey::Line { number, .. } if *number >= top),
            )
        {
            let same = matches!(
                keys[row],
                RowKey::Line { number, .. } if number == top
            );
            self.list.scroll_to(ListOffset {
                item_ix: row,
                offset_in_item: if same {
                    scroll_top.offset_in_item
                } else {
                    px(0.0)
                },
            });
        }
        self.list_keys = keys;
        if self.live {
            self.list.scroll_to_end();
        }
    }

    /// Scrolls so row `row` is near the top, with a little above it.
    fn reveal_row(&self, row: usize) {
        self.list.scroll_to(ListOffset {
            item_ix: row.saturating_sub(REVEAL_ABOVE),
            offset_in_item: px(0.0),
        });
    }

    /// Scrolls to the current match.
    fn reveal_active(&self) {
        if let Some(row) = self
            .view
            .active()
            .and_then(|number| self.view.row_of(number))
        {
            self.reveal_row(row);
        }
    }

    pub fn set_live(&mut self, live: bool, cx: &mut Context<'_, Self>) {
        self.live = live;
        if live {
            self.new_lines = 0;
            self.new_errors = 0;
            self.list.scroll_to_end();
        }
        cx.notify();
    }

    /// Whether the list shows its last row's bottom.
    fn at_end(&self) -> bool {
        let count = self.list.item_count();
        if count == 0 {
            return true;
        }
        if let Some(end) = self.list.is_scrolled_to_end() {
            return end;
        }
        // Not scrollable, or some heights are not known yet.
        match self.list.bounds_for_item(count - 1) {
            Some(last) => {
                last.bottom() <= self.list.viewport_bounds().bottom() + px(1.0)
            }
            None => self.list.logical_scroll_top().item_ix >= count,
        }
    }

    // ---- The view's settings ---------------------------------------------

    const fn prefs(&self) -> LogsPrefs {
        LogsPrefs {
            wrap: self.wrap,
            search_mode: self.view.mode,
            context_lines: self.view.context,
            include_diagnostics: self.include_diagnostics,
            error: self.view.shows(Level::Error),
            warning: self.view.shows(Level::Warn),
            info: self.view.shows(Level::Info),
        }
    }

    fn save_prefs(&self) {
        let Some(path) = &self.prefs_path else {
            return;
        };
        if let Err(error) = self.prefs().save(path) {
            log::warn!(
                target: "pwrdisk::logs",
                "cannot save the Logs window's view: {error}"
            );
        }
    }

    pub fn set_wrap(&mut self, wrap: bool, cx: &mut Context<'_, Self>) {
        self.wrap = wrap;
        self.h_offset = 0.0;
        // Every row's height changes.
        self.list.remeasure();
        if self.live {
            self.list.scroll_to_end();
        }
        self.save_prefs();
        cx.notify();
    }

    pub fn set_mode(&mut self, mode: SearchMode, cx: &mut Context<'_, Self>) {
        self.view.set_mode(mode);
        self.pending_reveal = Reveal::Active;
        self.save_prefs();
        cx.notify();
    }

    pub fn set_context(&mut self, lines: usize, cx: &mut Context<'_, Self>) {
        self.context_open = false;
        self.view.set_context(lines);
        self.pending_reveal = Reveal::Active;
        self.save_prefs();
        cx.notify();
    }

    pub fn toggle_level(&mut self, level: Level, cx: &mut Context<'_, Self>) {
        self.view.toggle_level(level);
        self.save_prefs();
        cx.notify();
    }

    fn toggle_diagnostics(&mut self, cx: &mut Context<'_, Self>) {
        self.include_diagnostics = !self.include_diagnostics;
        self.save_prefs();
        cx.notify();
    }

    // ---- Search ----------------------------------------------------------

    #[cfg(test)]
    pub fn scroll_offset(&self) -> Pixels {
        self.list.scroll_px_offset_for_scrollbar().y
    }

    /// The search field, for the tests to type into.
    #[cfg(test)]
    pub fn search_state(&self) -> Entity<InputState> {
        self.search.clone()
    }

    /// The field changed. Cleared (Esc), the window stays on the line that
    /// was the current match, now among all the others.
    fn query_changed(&mut self, query: &str) {
        // The field reports Enter as a change too, with the same text: that
        // must not send the search back to its first match.
        if query == self.query {
            return;
        }
        query.clone_into(&mut self.query);
        let back = self.view.active();
        self.view.set_query(query);
        if self.view.searching() {
            // Reading a search's results, not the tail.
            self.live = false;
            self.pending_reveal = Reveal::Active;
        } else if !self.live {
            self.pending_reveal = back.map_or(Reveal::None, Reveal::Line);
        }
    }

    /// Enter and Shift-Enter, and the field's chevrons.
    pub fn step(&mut self, forward: bool, cx: &mut Context<'_, Self>) {
        if self.view.step(forward).is_none() {
            return;
        }
        self.live = false;
        self.reveal_active();
        cx.notify();
    }

    fn open_gap(&mut self, first: usize, cx: &mut Context<'_, Self>) {
        self.view.open_gap(first);
        cx.notify();
    }

    // ---- Mark ------------------------------------------------------------

    /// Drops the mark after the newest line, labelled with the time.
    pub fn mark(&mut self, cx: &mut Context<'_, Self>) {
        let now = chrono::Local::now().format("%H:%M:%S").to_string();
        self.view.set_mark(now);
        cx.notify();
    }

    fn clear_mark(&mut self, cx: &mut Context<'_, Self>) {
        self.view.clear_mark();
        cx.notify();
    }

    /// Selects every line after the mark; the selection then grows as new
    /// lines come in.
    pub fn select_since_mark(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(range) = self.view.since_mark() {
            self.selection = Some(range);
            self.follows_tail = true;
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    // ---- Selection and copy ----------------------------------------------

    fn set_note(
        &mut self,
        note: impl Into<SharedString>,
        cx: &mut Context<'_, Self>,
    ) {
        self.note_generation += 1;
        let generation = self.note_generation;
        self.note = Some((note.into(), generation));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTE_FOR).await;
            let _ = this.update(cx, |this, cx| {
                if this.note.as_ref().is_some_and(|(_, g)| *g == generation) {
                    this.note = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// The selected range, low to high, by number.
    fn selected(&self) -> Option<(usize, usize)> {
        self.selection.map(|(a, b)| (a.min(b), a.max(b)))
    }

    /// The shown lines inside the selection, as indexes into the view.
    fn selected_indexes(&self) -> Vec<usize> {
        let Some((low, high)) = self.selected() else {
            return Vec::new();
        };
        let first = self.view.first_number;
        self.view
            .visible()
            .iter()
            .copied()
            .filter(|index| (low..=high).contains(&(first + index)))
            .collect()
    }

    /// A click in the gutter selects line `number`; with Shift it extends
    /// the selection from its anchor.
    pub fn click_number(
        &mut self,
        number: usize,
        extend: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.selection = match (self.selection, extend) {
            (Some((anchor, _)), true) => Some((anchor, number)),
            _ => Some((number, number)),
        };
        self.follows_tail = extend && self.view.newest() == Some(number);
        cx.notify();
    }

    pub fn select_all(&mut self, cx: &mut Context<'_, Self>) {
        let visible = self.view.visible();
        if let (Some(first), Some(last)) = (visible.first(), visible.last()) {
            let base = self.view.first_number;
            self.selection = Some((base + first, base + last));
            self.follows_tail = false;
            cx.notify();
        }
    }

    /// The selection on the clipboard, after the diagnostics report when
    /// asked: what a bug report wants. Neither holds a scanned path.
    pub fn copy_selection(&mut self, cx: &mut Context<'_, Self>) {
        let indexes = self.selected_indexes();
        if indexes.is_empty() {
            return;
        }
        let mut out = String::new();
        if self.include_diagnostics {
            out.push_str(&crate::diagnostics::collect(cx).report());
            out.push('\n');
            out.push_str(&self.copy_header(&indexes));
            out.push('\n');
        }
        for &index in &indexes {
            out.push_str(self.view.lines[index].raw());
            out.push('\n');
        }
        cx.write_to_clipboard(ClipboardItem::new_string(out));
        let count = indexes.len();
        let lines = if count == 1 { "line" } else { "lines" };
        let with = if self.include_diagnostics {
            " with diagnostics"
        } else {
            ""
        };
        self.set_note(format!("Copied {count} {lines}{with}"), cx);
    }

    /// "Log lines: 215–219 (5 of 1,204; levels: Error, Warning, Info)",
    /// and the search with its mode while one is active.
    fn copy_header(&self, indexes: &[usize]) -> String {
        let first = self.view.first_number;
        let low = indexes.first().map_or(0, |index| first + index);
        let high = indexes.last().map_or(0, |index| first + index);
        let levels: Vec<&str> = Level::ALL
            .into_iter()
            .filter(|level| self.view.shows(*level))
            .map(Level::label)
            .collect();
        let mut header = format!(
            "Log lines: {low}\u{2013}{high} ({} of {}; levels: {})\n",
            thousands(indexes.len()),
            thousands(self.view.lines.len()),
            levels.join(", ")
        );
        if self.view.searching() {
            let context = if self.view.mode == SearchMode::Filter
                && self.view.context > 0
            {
                format!(", context \u{b1}{}", self.view.context)
            } else {
                String::new()
            };
            let _ = writeln!(
                header,
                "Log search: \"{}\" ({}{context})",
                self.query.trim(),
                self.view.mode.label()
            );
        }
        header
    }

    fn copy_path(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(tail) = &self.tail {
            cx.write_to_clipboard(ClipboardItem::new_string(
                tail.path.display().to_string(),
            ));
            self.set_note("Copied the log file path", cx);
        }
    }

    // ---- Keys and scrolling ----------------------------------------------

    /// The window's keys, while the search field does not have them: it
    /// edits, Enter steps through the matches and Esc clears it.
    fn key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.search.focus_handle(cx).is_focused(window) {
            return;
        }
        let keystroke = &event.keystroke;
        let command =
            keystroke.modifiers.platform || keystroke.modifiers.control;
        match (command, keystroke.key.as_str()) {
            (true, "f") => {
                self.search
                    .update(cx, |search, cx| search.focus(window, cx));
            }
            (true, "a") => self.select_all(cx),
            (true, "c") => self.copy_selection(cx),
            (false, "pageup" | "pagedown") => {
                // Four fifths of the view, so a line on the edge stays in
                // sight across the page and a long wrapped line can be read
                // a screen at a time.
                let page = self.list.viewport_bounds().size.height * 0.8;
                self.live = false;
                self.list.scroll_by(if keystroke.key == "pageup" {
                    -page
                } else {
                    page
                });
                cx.notify();
            }
            (false, "up" | "down" | "home" | "end") => {
                let numbers: Vec<_> = self
                    .view
                    .rows()
                    .iter()
                    .filter_map(|row| match row {
                        Row::Line { index, .. } => {
                            Some(self.view.first_number + index)
                        }
                        _ => None,
                    })
                    .collect();
                if let Some(last) = numbers.len().checked_sub(1) {
                    let at = self.selection.and_then(|(_, number)| {
                        numbers.iter().position(|n| *n == number)
                    });
                    // With nothing selected yet, moving down starts at
                    // the first line and moving up at the last, rather
                    // than stepping past either.
                    let next = match (keystroke.key.as_str(), at) {
                        ("end", _) | ("up", None) => last,
                        ("home", _) | (_, None) => 0,
                        ("up", Some(at)) => at.saturating_sub(1),
                        (_, Some(at)) => (at + 1).min(last),
                    };
                    let number = numbers[next];
                    self.click_number(number, keystroke.modifiers.shift, cx);
                    self.live = false;
                    if keystroke.key == "end" {
                        self.list.scroll_to_end();
                    } else if let Some(row) = self.view.row_of(number) {
                        self.reveal_row(row);
                    }
                    cx.notify();
                }
            }
            (false, "escape") => {
                self.selection = None;
                self.context_open = false;
                cx.notify();
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The list scrolled first (its handler runs before this one); this
    /// pauses when that left the end, resumes when it reached it, and moves
    /// unwrapped text sideways.
    fn scrolled(
        &mut self,
        event: &ScrollWheelEvent,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let delta = event.delta.pixel_delta(Self::row_height(window));
        let (dx, dy) = (delta.x.as_f32(), delta.y.as_f32());
        if !self.wrap && dx != 0.0 {
            let offset = (self.h_offset - dx).clamp(0.0, self.h_max);
            if (offset - self.h_offset).abs() > f32::EPSILON {
                self.h_offset = offset;
                cx.notify();
            }
        }
        if dy > 0.0 && self.live && !self.at_end() {
            // Scrolled up: stop following.
            self.live = false;
            cx.notify();
        } else if dy < 0.0
            && !self.live
            && !self.view.searching()
            && self.scrollable()
            && self.at_end()
        {
            // Scrolled back down to the end of a list that scrolls; one that
            // fits stays paused until Live or the pill.
            self.set_live(true, cx);
        }
    }

    /// Whether the rows are taller than the viewport.
    fn scrollable(&self) -> bool {
        self.list.max_offset_for_scrollbar().y > px(0.0)
    }

    /// The face the lines are drawn in, for measuring them.
    fn mono_font(cx: &App) -> Font {
        gpui_kit::font(cx.omarchy().mono_font.clone())
    }

    /// The hanging indent of a wrapped line's continuation rows.
    fn indent(window: &Window) -> Pixels {
        window.rem_size() * space::LG.0
    }

    /// Where `raw` breaks into rows when wrapped: byte ranges, one per row.
    /// Every row wraps to the room left after the hanging indent, so a
    /// continuation fits beside it.
    pub fn segments(
        &self,
        raw: &str,
        window: &Window,
        cx: &App,
    ) -> Vec<Range<usize>> {
        let width =
            self.text_width - Self::indent(window).as_f32() - self.char_width;
        if raw.is_empty() || width < self.char_width * 8.0 {
            return std::iter::once(0..raw.len()).collect();
        }
        let font_size = window.rem_size() * text::BODY.0;
        let mut wrapper = window
            .text_system()
            .line_wrapper(Self::mono_font(cx), font_size);
        let fragments = [LineFragment::text(raw)];
        let mut starts = vec![0];
        starts.extend(
            // SameIndent is how GPUI wrapped before the argument existed;
            // only the break offsets are used here.
            wrapper
                .wrap_line(&fragments, px(width), IndentAdjustment::SameIndent)
                .map(|boundary| boundary.ix),
        );
        starts.push(raw.len());
        starts.dedup();
        starts.windows(2).map(|pair| pair[0]..pair[1]).collect()
    }

    /// Measures this frame's room for text and the mono face's advance,
    /// and asks the list to measure again when the room changed.
    fn measure(&mut self, window: &Window, cx: &App) {
        let rem = window.rem_size();
        let to_px = |rems: Rems| (rem * rems.0).as_f32();
        let viewport = self.list.viewport_bounds().size.width.as_f32();
        // Before the first frame the list has no bounds: the window's
        // width less the body's padding and the frame's hairlines.
        let list_width = if viewport > 0.0 {
            viewport
        } else {
            2.0f32.mul_add(
                -(to_px(space::LG) + 1.0),
                window.viewport_size().width.as_f32(),
            )
        };
        let text_width = (list_width
            - to_px(size::LOG_GUTTER)
            - to_px(space::MD)
            - to_px(space::LG))
        .max(0.0);
        let font_size = rem * text::BODY.0;
        let font_id = window.text_system().resolve_font(&Self::mono_font(cx));
        if let Ok(advance) =
            window.text_system().advance(font_id, font_size, 'm')
        {
            self.char_width = advance.width.as_f32();
        }
        if (text_width - self.text_width).abs() >= 1.0 {
            self.text_width = text_width;
            if self.wrap {
                self.list.remeasure();
            }
        }
        #[allow(
            clippy::cast_precision_loss,
            reason = "a line's width in characters is far below 2^24"
        )]
        let widest = self.view.widest() as f32;
        self.h_max = widest.mul_add(self.char_width, -self.text_width).max(0.0);
        self.h_offset = self.h_offset.min(self.h_max);
    }
}

// ---- Drawing --------------------------------------------------------------

const fn level_color(level: Level, theme: &Theme) -> Hsla {
    match level {
        Level::Error => theme.danger,
        Level::Warn => theme.warning,
        Level::Info => theme.accent,
        Level::Debug => theme.secondary,
    }
}

impl Render for LogsWindow {
    fn render(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        crate::ui::sync_scale(window, cx);
        // A test may change the view directly.
        self.sync_list(window);
        match std::mem::replace(&mut self.pending_reveal, Reveal::None) {
            Reveal::None => {}
            Reveal::Active => self.reveal_active(),
            Reveal::Line(number) => {
                if let Some(row) = self.view.row_of(number) {
                    self.reveal_row(row);
                }
            }
        }
        // The Context menu closes with its select.
        if self.view.mode != SearchMode::Filter || !self.view.searching() {
            self.context_open = false;
        }
        self.measure(window, cx);
        let theme = cx.omarchy().clone();
        let actions = div()
            .flex()
            .flex_row()
            .gap(space::SM)
            .child(
                button(COPY_PATH_ID, "Copy path", ButtonVariant::Secondary, cx)
                    .debug_selector(|| COPY_PATH_ID.into())
                    .semantic_disabled(self.tail.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.copy_path(cx))),
            )
            .child(
                button(REVEAL_ID, "Reveal", ButtonVariant::Secondary, cx)
                    .debug_selector(|| REVEAL_ID.into())
                    .semantic_disabled(self.tail.is_none())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(tail) = &this.tail {
                            cx.reveal_path(&tail.path);
                        }
                    })),
            );
        let content = div()
            .id("logs-window")
            // The search input consumes Escape itself, clearing its query,
            // so the action is caught on the way down: an empty search
            // hands the keys back to the reader instead.
            .capture_action(cx.listener(
                |this, _: &gpui_kit::base::input::Escape, window, cx| {
                    if this.search.read(cx).value().is_empty() {
                        window.focus(&this.focus, cx);
                        cx.stop_propagation();
                    }
                },
            ))
            .track_focus(&self.focus)
            .role(gpui_kit::Role::Group)
            .aria_label(READER_LABEL)
            .on_key_down(cx.listener(
                |this, event: &KeyDownEvent, window, cx| {
                    this.key(event, window, cx);
                },
            ))
            .flex()
            .flex_col()
            .size_full()
            .child(self.toolbar(&theme, window, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .px(space::LG)
                    .pb(space::SM)
                    .child(self.viewport(&theme, cx)),
            )
            .child(self.status_bar(&theme));
        aux_window::frame(&["Help", "Logs"], actions, content, window, cx)
    }
}

/// What the next frame scrolls to, once the list knows the new rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Reveal {
    #[default]
    None,
    /// The current match.
    Active,
    /// A line, by number.
    Line(usize),
}

impl LogsWindow {
    fn toolbar(
        &self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let search = gpui_omarchy::input(SEARCH_ID, &self.search, window, cx)
            .aria_label("Search log text").aria_description("Enter moves to the next match; Shift Enter to the previous match. Tab moves to log controls. Escape clears the query.")
            .aria_value(self.search.read(cx).value())
            .debug_selector(|| SEARCH_ID.into())
            .prefix(
                icon(IconName::Search)
                    .size(icon_size::SM)
                    .text_color(theme.secondary),
            )
            .suffix(self.search_trailing(theme, cx));
        let mode = [
            (
                SearchMode::Filter,
                FILTER_ID,
                "Filter",
                "Show only matching lines",
            ),
            (
                SearchMode::Highlight,
                HIGHLIGHT_ID,
                "Highlight",
                "Show every line and highlight matches",
            ),
        ]
        .into_iter()
        .fold(segmented(theme), |seg, (mode, id, label, hint)| {
            seg.child(with_tooltip(
                // One of two, so radio buttons; the levels are checkboxes.
                segment(
                    id,
                    label,
                    self.view.mode == mode,
                    gpui_kit::Role::RadioButton,
                    theme,
                )
                .on_key_down(cx.listener(
                    move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(
                            event.keystroke.key.as_str(),
                            "enter" | "space"
                        ) {
                            this.set_mode(mode, cx);
                            cx.stop_propagation();
                        }
                    },
                ))
                .on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.set_mode(mode, cx);
                    },
                )),
                hint,
            ))
        });
        let levels =
            Level::ALL.into_iter().fold(segmented(theme), |seg, level| {
                let on = self.view.shows(level);
                let count = match level {
                    Level::Error | Level::Warn => self.view.count(level),
                    Level::Info | Level::Debug => 0,
                };
                let label = if count > 0 {
                    format!("{} {}", level.label(), thousands(count))
                } else {
                    level.label().to_owned()
                };
                let id = level_id(level);
                seg.child(
                    segment(id, label, on, gpui_kit::Role::CheckBox, theme)
                        .on_key_down(cx.listener(
                            move |this, event: &KeyDownEvent, _, cx| {
                                if matches!(
                                    event.keystroke.key.as_str(),
                                    "enter" | "space"
                                ) {
                                    this.toggle_level(level, cx);
                                    cx.stop_propagation();
                                }
                            },
                        ))
                        .when(on, |item| {
                            item.text_color(level_color(level, theme))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_level(level, cx);
                        })),
                )
            });
        let wrap = with_tooltip(
            button(WRAP_ID, "Wrap", ButtonVariant::Outline, cx)
                .debug_selector(|| WRAP_ID.into())
                .selected(self.wrap)
                .on_click(cx.listener(|this, _, _, cx| {
                    let wrap = !this.wrap;
                    this.set_wrap(wrap, cx);
                })),
            "Wrap long lines; off, they scroll sideways",
        );
        let mark = with_tooltip(
            button(MARK_ID, "Mark", ButtonVariant::Outline, cx)
                .debug_selector(|| MARK_ID.into())
                .on_click(cx.listener(|this, _, _, cx| this.mark(cx))),
            "Drop a mark after the newest line",
        );
        let live = with_tooltip(
            button(
                LIVE_ID,
                if self.live { "Live" } else { "Paused" },
                ButtonVariant::Outline,
                cx,
            )
            .debug_selector(|| LIVE_ID.into())
            .selected(self.live)
            .on_click(cx.listener(|this, _, _, cx| {
                let live = !this.live;
                this.set_live(live, cx);
            })),
            if self.live {
                "Pause the live log"
            } else {
                "Follow the log again"
            },
        );
        let show_context =
            self.view.mode == SearchMode::Filter && self.view.searching();
        div()
            .debug_selector(|| "logs-toolbar".into())
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(space::SM)
            .px(space::LG)
            .py(space::SM)
            .child(
                div()
                    // A zero flex basis lets the other controls consume the
                    // row and squeeze the input to its search icon at 200%.
                    .flex_grow(1.)
                    .min_w(size::LOG_FILTER)
                    .w(size::LOG_FILTER)
                    .max_w_full()
                    .child(search),
            )
            .child(mode)
            .when(show_context, |bar| {
                bar.child(self.context_select(theme, cx))
            })
            .child(levels)
            .child(div().w(px(1.)).h(size::LOG_ROW).bg(theme.divider()))
            .child(wrap)
            .child(mark)
            .child(live)
    }

    /// Inside the search field: the shortcut while empty; the current
    /// match as "1 of 4" and the previous and next chevrons while
    /// searching.
    fn search_trailing(
        &self,
        theme: &Theme,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        if !self.view.searching() {
            let hint = if cfg!(target_os = "macos") {
                "\u{2318}F"
            } else {
                "Ctrl+F"
            };
            return caption(hint, theme.secondary, theme).into_any_element();
        }
        let count = self.view.match_count();
        let label = self.view.active_place().map_or_else(
            || "No matches".to_owned(),
            |place| format!("{place} of {}", thousands(count)),
        );
        let step = |id: &'static str, name: IconName, hint: &'static str| {
            let forward = id == NEXT_ID;
            with_tooltip(
                gpui_omarchy::icon_button(
                    id,
                    name,
                    hint,
                    ButtonVariant::Secondary,
                    cx,
                )
                // The stock padding makes the chevrons taller than the
                // field's text, so the toolbar would grow the moment a
                // search starts; trimmed, the field keeps its height.
                .py(space::XXS)
                .debug_selector(move || id.into())
                .semantic_disabled(count == 0)
                .on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.step(forward, cx);
                    },
                )),
                hint,
            )
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::XXS)
            .child(div().mr(space::XS).child(caption(
                label,
                theme.secondary,
                theme,
            )))
            .child(step(
                PREV_ID,
                IconName::ChevronUp,
                "Previous match (Shift-Enter)",
            ))
            .child(step(NEXT_ID, IconName::ChevronDown, "Next match (Enter)"))
            .into_any_element()
    }

    /// Context: lines kept around each match, in Filter mode with a query.
    fn context_select(
        &self,
        theme: &Theme,
        cx: &Context<'_, Self>,
    ) -> impl IntoElement {
        let menu = CONTEXT_CHOICES.into_iter().fold(
            div()
                .id("logs-context-menu")
                .absolute()
                .top_full()
                .left_0()
                .mt(space::XS)
                .w(size::LOG_CONTEXT_MENU)
                .flex()
                .flex_col()
                .py(space::XS)
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .shadow_lg()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.context_open = false;
                    cx.notify();
                })),
            |menu, lines| {
                let id = SharedString::from(format!("logs-context-{lines}"));
                let selector = id.clone();
                menu.child(
                    div()
                        .id(id)
                        .debug_selector(move || selector.to_string())
                        .role(gpui_kit::Role::MenuItem)
                        .aria_label(context_label(lines))
                        .focusable()
                        .tab_stop(true)
                        .on_key_down(cx.listener(
                            move |this, event: &KeyDownEvent, _, cx| {
                                if matches!(
                                    event.keystroke.key.as_str(),
                                    "enter" | "space"
                                ) {
                                    this.set_context(lines, cx);
                                    cx.stop_propagation();
                                }
                            },
                        ))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(space::SM)
                        .px(space::MD)
                        .py(space::XS)
                        .cursor_pointer()
                        .hover(|row| row.bg(theme.hover_fill()))
                        .child(div().w(icon_size::MD).when(
                            lines == self.view.context,
                            |mark| {
                                mark.child(
                                    icon(IconName::Check)
                                        .size(icon_size::SM)
                                        .text_color(theme.accent),
                                )
                            },
                        ))
                        .child(context_label(lines))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_context(lines, cx);
                        })),
                )
            },
        );
        div()
            .relative()
            .flex_none()
            .child(with_tooltip(
                button(
                    CONTEXT_ID,
                    format!("{} \u{25be}", context_label(self.view.context)),
                    ButtonVariant::Outline,
                    cx,
                )
                .debug_selector(|| CONTEXT_ID.into())
                .selected(self.context_open)
                // Capture runs before the open menu's press-outside handler
                // closes it.
                .capture_any_mouse_down(cx.listener(|this, _, _, _| {
                    this.context_was_open = this.context_open;
                }))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.context_open = !this.context_was_open;
                    cx.notify();
                })),
                "Lines kept around each match",
            ))
            .when(self.context_open, |select| select.child(deferred(menu)))
    }

    fn viewport(
        &self,
        theme: &Theme,
        cx: &Context<'_, Self>,
    ) -> impl IntoElement {
        let frame = div()
            .relative()
            .flex_1()
            .min_h_0()
            .border_1()
            .border_color(theme.divider())
            .bg(theme.inset)
            .overflow_hidden();
        if let Some(problem) = &self.problem {
            return frame.child(
                crate::semantics::text("logs-problem", problem.clone())
                    .role(gpui_kit::Role::Alert)
                    .child(centered(problem.clone(), theme)),
            );
        }
        if self.view.lines.is_empty() {
            return frame.child(
                crate::semantics::text("logs-empty", "No log output yet.")
                    .child(centered("No log output yet.", theme)),
            );
        }
        if self.view.rows().is_empty() {
            return frame.child(
                crate::semantics::text("logs-no-matches", "No lines match.")
                    .child(centered("No lines match.", theme)),
            );
        }
        let rows = list(
            self.list.clone(),
            cx.processor(|this, row: usize, window, cx| {
                this.row(row, window, cx)
            }),
        )
        .size_full()
        .py(space::XS);
        frame
            .child(
                div()
                    .id("logs-viewport")
                    .debug_selector(|| "logs-viewport".into())
                    .role(gpui_kit::Role::ListBox)
                    .aria_label(format!(
                        "{}; {}; {}",
                        self.view.summary(),
                        if self.live {
                            "following live"
                        } else {
                            "paused"
                        },
                        self.selected().map_or_else(
                            || "no selection".into(),
                            |(low, high)| format!(
                                "selected lines {low} through {high}"
                            )
                        )
                    ))
                    .size_full()
                    .on_scroll_wheel(cx.listener(
                        |this, event: &ScrollWheelEvent, window, cx| {
                            this.scrolled(event, window, cx);
                        },
                    ))
                    .child(rows),
            )
            .when(!self.live && self.new_lines > 0, |frame| {
                frame.child(self.pill(theme, cx))
            })
            .when(self.selection.is_some(), |frame| {
                frame.child(self.selection_bar(theme, cx))
            })
    }

    fn row(
        &self,
        row: usize,
        window: &Window,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        let theme = cx.omarchy().clone();
        match self.view.rows().get(row).copied() {
            Some(Row::Line { index, shown }) => {
                self.line_row(index, shown, &theme, window, cx)
            }
            Some(Row::Gap { first, count }) => {
                gap_row(first, count, &theme, cx)
            }
            Some(Row::Mark) => self.mark_row(&theme, cx),
            None => div().into_any_element(),
        }
    }

    fn line_row(
        &self,
        index: usize,
        shown: Shown,
        theme: &Theme,
        window: &Window,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        let line = &self.view.lines[index];
        let number = self.view.first_number + index;
        let selected = self
            .selected()
            .is_some_and(|(low, high)| (low..=high).contains(&number));
        let context = shown == Shown::Context;
        let background = if selected {
            Some(theme.selection)
        } else if context {
            None
        } else {
            match line.level {
                Level::Error => Some(theme.danger.opacity(0.1)),
                Level::Warn => Some(theme.warning.opacity(0.08)),
                Level::Info | Level::Debug => None,
            }
        };
        let raw = line.raw().clone();
        let mut highlights = Vec::new();
        // Context lines are muted whole: no token colours.
        if !line.time.is_empty() && !context {
            highlights.push((
                0..line.time.len(),
                HighlightStyle {
                    color: Some(theme.secondary),
                    ..Default::default()
                },
            ));
            highlights.push((
                line.level_range.clone(),
                HighlightStyle {
                    color: Some(level_color(line.level, theme)),
                    font_weight: matches!(
                        line.level,
                        Level::Error | Level::Warn
                    )
                    .then_some(FontWeight::BOLD),
                    ..Default::default()
                },
            ));
            if let Some(target) = line.target_range.clone() {
                highlights.push((
                    target,
                    HighlightStyle {
                        color: Some(theme.foreground),
                        ..Default::default()
                    },
                ));
            }
        }
        // Search hits on top, never overlapping the styles above: GPUI
        // wants the ranges in order and apart, so hits replace them. The
        // current match's first hit is solid.
        if shown == Shown::Match {
            let active = self.view.active() == Some(number);
            let hits = search_hits(line, &self.query);
            if !hits.is_empty() {
                let styled: Vec<_> = hits
                    .into_iter()
                    .enumerate()
                    .map(|(at, hit)| (hit, hit_style(active && at == 0, theme)))
                    .collect();
                highlights = merge_hits(highlights, &styled);
            }
        }
        let ink = if context {
            theme.secondary
        } else {
            theme.bright
        };
        let body = if self.wrap {
            let segments = self.segments(&raw, window, cx);
            div().flex().flex_col().children(
                segments.into_iter().enumerate().map(|(at, range)| {
                    let part = clip_highlights(&highlights, &range);
                    line_text(raw[range].to_owned().into(), part, ink, theme)
                        .when(at > 0, |row| row.pl(space::LG))
                }),
            )
        } else {
            div().overflow_hidden().child(
                line_text(raw, highlights, ink, theme)
                    // Scrolled sideways by the pointer.
                    .ml(px(-self.h_offset))
                    .debug_selector(move || format!("log-text-{number}")),
            )
        };
        div()
            .id(("log-row", number))
            .debug_selector(move || format!("log-row-{number}"))
            .role(gpui_kit::Role::ListBoxOption)
            .aria_label(format!(
                "Line {number}: {}{}",
                line.raw(),
                if context { "; context line" } else { "" }
            ))
            .aria_selected(selected)
            .when(
                self.selection.is_some_and(|(_, end)| end == number),
                gpui_kit::StatefulInteractiveElement::aria_active_descendant,
            )
            .w_full()
            .flex()
            .flex_row()
            .when_some(background, gpui_kit::Styled::bg)
            .child(
                div()
                    .id(("log-number", number))
                    .debug_selector(move || format!("log-number-{number}"))
                    .w(size::LOG_GUTTER)
                    .h(size::LOG_ROW)
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_end()
                    .pr(space::SM)
                    .border_r_1()
                    .border_color(theme.divider())
                    .when(selected, |gutter| {
                        gutter.border_l_2().border_color(theme.accent)
                    })
                    .cursor_pointer()
                    .hover(|gutter| gutter.bg(theme.hover_fill()))
                    .child(caption(
                        number.to_string(),
                        if selected {
                            theme.accent
                        } else {
                            theme.secondary
                        },
                        theme,
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(
                            move |this, event: &MouseDownEvent, window, cx| {
                                // The lines take the keys now: Copy, Select
                                // All and Esc are the selection's.
                                window.focus(&this.focus, cx);
                                this.click_number(
                                    number,
                                    event.modifiers.shift,
                                    cx,
                                );
                            },
                        ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(space::MD)
                    .pr(space::LG)
                    .child(body),
            )
            .into_any_element()
    }

    /// "Mark · 12:39:00", the rule, Select since mark and remove.
    fn mark_row(&self, theme: &Theme, cx: &Context<'_, Self>) -> AnyElement {
        let label = self
            .view
            .mark()
            .map(|mark| format!("Mark \u{b7} {}", mark.label))
            .unwrap_or_default();
        let since = self.view.since_mark().is_some();
        div()
            .id("log-mark")
            .debug_selector(|| "log-mark".into())
            .h(size::LOG_MARK)
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::SM)
            .pl(size::LOG_GUTTER)
            .pr(space::LG)
            .child(
                div()
                    .pl(space::MD)
                    .text_size(text::CAPTION)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.accent)
                    .child(label),
            )
            .child(div().flex_1().h(px(1.)).bg(theme.accent.opacity(0.5)))
            .child(
                button(
                    SINCE_MARK_ID,
                    "Select since mark",
                    ButtonVariant::Outline,
                    cx,
                )
                .debug_selector(|| SINCE_MARK_ID.into())
                .semantic_disabled(!since)
                .on_click(cx.listener(
                    |this, _, window, cx| {
                        this.select_since_mark(window, cx);
                    },
                )),
            )
            .child(with_tooltip(
                gpui_omarchy::icon_button(
                    CLEAR_MARK_ID,
                    IconName::Close,
                    "Remove the mark",
                    ButtonVariant::Secondary,
                    cx,
                )
                .debug_selector(|| CLEAR_MARK_ID.into())
                .on_click(cx.listener(|this, _, _, cx| this.clear_mark(cx))),
                "Remove the mark",
            ))
            .into_any_element()
    }

    /// "↓ 12 new lines · 1 error", while paused; a click follows again.
    fn pill(&self, theme: &Theme, cx: &Context<'_, Self>) -> impl IntoElement {
        let lines = if self.new_lines == 1 { "line" } else { "lines" };
        let mut label =
            format!("\u{2193} {} new {lines}", thousands(self.new_lines));
        if self.new_errors > 0 {
            let errors = if self.new_errors == 1 {
                "error"
            } else {
                "errors"
            };
            let _ = write!(label, " \u{b7} {} {errors}", self.new_errors);
        }
        div()
            .absolute()
            .bottom(if self.selection.is_some() {
                size::LOG_PILL_ABOVE_BAR
            } else {
                space::LG
            })
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .id(PILL_ID)
                    .debug_selector(|| PILL_ID.into())
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
                    .on_click(
                        cx.listener(|this, _, _, cx| this.set_live(true, cx)),
                    ),
            )
    }

    fn selection_bar(
        &self,
        theme: &Theme,
        cx: &Context<'_, Self>,
    ) -> impl IntoElement {
        let indexes = self.selected_indexes();
        let first = self.view.first_number;
        let since_mark = self.view.since_mark().is_some()
            && self.selected() == self.view.since_mark();
        let lines = |count: usize| if count == 1 { "line" } else { "lines" };
        let label = match indexes.as_slice() {
            [] => "No shown lines selected".to_owned(),
            many if since_mark => format!(
                "{} {} since mark",
                thousands(many.len()),
                lines(many.len())
            ),
            [only] => format!("Line {}", first + only),
            many => format!("{} lines selected", thousands(many.len())),
        };
        let range = match (indexes.first(), indexes.last()) {
            (Some(low), Some(high)) if low != high => {
                Some(format!("{}\u{2013}{}", first + low, first + high))
            }
            _ => None,
        };
        let check = if self.include_diagnostics {
            "\u{2611}"
        } else {
            "\u{2610}"
        };
        let copy_keys = if cfg!(target_os = "macos") {
            "\u{2318}C"
        } else {
            "Ctrl+C"
        };
        div()
            .absolute()
            .bottom(space::LG)
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::MD)
                    .py(space::XS)
                    .pl(space::LG)
                    .pr(space::SM)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface)
                    .shadow_lg()
                    .child(div().text_color(theme.bright).child(label))
                    .when_some(range, |bar, range| {
                        bar.child(caption(range, theme.secondary, theme))
                    })
                    .child(with_tooltip(
                        div()
                            .id(DIAGNOSTICS_ID)
                            .debug_selector(|| DIAGNOSTICS_ID.into())
                            .role(gpui_kit::Role::CheckBox)
                            .aria_label(
                                "Include diagnostics in copied log selection",
                            )
                            .aria_toggled(if self.include_diagnostics {
                                gpui_kit::accesskit::Toggled::True
                            } else {
                                gpui_kit::accesskit::Toggled::False
                            })
                            .focusable()
                            .tab_stop(true)
                            .on_key_down(cx.listener(
                                |this, event: &KeyDownEvent, _, cx| {
                                    if matches!(
                                        event.keystroke.key.as_str(),
                                        "enter" | "space"
                                    ) {
                                        this.toggle_diagnostics(cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(space::XS)
                            .cursor_pointer()
                            .text_color(theme.foreground)
                            .child(check)
                            .child("Include diagnostics")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_diagnostics(cx);
                            })),
                        "Start the copy with Copy Diagnostics Info's report",
                    ))
                    .child(with_tooltip(
                        button(COPY_ID, "Copy", ButtonVariant::Primary, cx)
                            .debug_selector(|| COPY_ID.into())
                            .semantic_disabled(indexes.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.copy_selection(cx);
                            })),
                        copy_keys,
                    ))
                    .child(
                        gpui_omarchy::icon_button(
                            "logs-clear-selection",
                            IconName::Close,
                            "Clear the selection",
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(cx.listener(
                            |this, _, _, cx| {
                                this.selection = None;
                                cx.notify();
                            },
                        )),
                    ),
            )
    }

    fn status_bar(&self, theme: &Theme) -> impl IntoElement {
        let mut summary =
            if !self.live && self.new_lines > 0 && !self.view.searching() {
                format!(
                    "Paused \u{b7} {} new since you scrolled",
                    thousands(self.new_lines)
                )
            } else {
                self.view.summary()
            };
        if self.view.truncated {
            let _ = write!(
                summary,
                " \u{b7} the newest {}",
                thousands(crate::log_view::MAX_LINES)
            );
        }
        let searching = self.view.searching();
        let place = self.tail.as_ref().filter(|_| !searching).map(|tail| {
            format!(
                "{} \u{b7} rotates at 1 MB to main.old.log",
                crate::marks::display_path(
                    &tail.path,
                    std::env::home_dir().as_deref(),
                    PathStyle::Native
                )
            )
        });
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .flex_none()
            .items_center()
            .gap(space::MD)
            .px(space::LG)
            .py(space::XS)
            .border_t_1()
            .border_color(theme.divider())
            .text_size(text::CAPTION)
            .text_color(theme.secondary)
            .child(
                crate::semantics::text(
                    "logs-summary",
                    format!(
                        "{}; {}; {}",
                        summary,
                        if self.live {
                            "following live"
                        } else {
                            "paused"
                        },
                        self.selected().map_or_else(
                            || "no selection".into(),
                            |(low, high)| format!(
                                "selected lines {low} through {high}"
                            )
                        )
                    ),
                )
                .debug_selector(|| "logs-count".into())
                .child(summary),
            )
            // The path fills the middle; without it a spacer keeps the note
            // and the search hint at the right edge.
            .map(|bar| match place {
                Some(place) => bar.child(
                    crate::semantics::text("logs-path", place.clone())
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(place),
                ),
                None => bar.child(div().flex_1()),
            })
            .when_some(self.note.as_ref(), |bar, (note, _)| {
                bar.child(
                    div()
                        .text_color(theme.success)
                        .child(format!("\u{2713} {note}")),
                )
            })
            .when(searching, |bar| bar.child(SEARCH_HINT))
    }
}

/// "No context", "Context ±3".
fn context_label(lines: usize) -> String {
    if lines == 0 {
        "No context".to_owned()
    } else {
        format!("Context \u{b1}{lines}")
    }
}

const fn level_id(level: Level) -> &'static str {
    match level {
        Level::Error => "logs-level-error",
        Level::Warn => "logs-level-warn",
        Level::Info => "logs-level-info",
        Level::Debug => "logs-level-debug",
    }
}

/// A row of segments sharing one border: the search mode and the levels.
fn segmented(theme: &Theme) -> gpui_kit::Div {
    div()
        .flex()
        .flex_row()
        .flex_none()
        .border_1()
        .border_color(theme.border)
}

/// One segment: on, it takes the accent's fill.
fn segment(
    id: &'static str,
    label: impl Into<SharedString>,
    on: bool,
    role: gpui_kit::Role,
    theme: &Theme,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let label = label.into();
    div()
        .id(id)
        .debug_selector(move || id.into())
        .role(role)
        .aria_label(label.clone())
        .aria_toggled(if on {
            gpui_kit::accesskit::Toggled::True
        } else {
            gpui_kit::accesskit::Toggled::False
        })
        .focusable()
        .tab_stop(true)
        .px(space::SM)
        .py(space::XS)
        .border_r_1()
        .border_color(theme.divider())
        .cursor_pointer()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(if on { theme.bright } else { theme.secondary })
        .when(on, |item| item.bg(theme.accent.opacity(0.18)))
        .hover(|item| item.bg(theme.hover_fill()))
        .child(label)
}

/// "214 lines hidden" between dashed rules; a click shows them.
fn gap_row(
    first: usize,
    count: usize,
    theme: &Theme,
    cx: &Context<'_, LogsWindow>,
) -> AnyElement {
    let rule = theme.border;
    let dashed = move || {
        div()
            .h(px(1.))
            .border_t_1()
            .border_dashed()
            .border_color(rule)
    };
    let lines = if count == 1 { "line" } else { "lines" };
    with_tooltip(
        div()
            .id(("log-gap", first))
            .debug_selector(move || format!("log-gap-{first}"))
            .h(size::LOG_GAP)
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::MD)
            .pl(size::LOG_GUTTER)
            .pr(space::LG)
            .cursor_pointer()
            .hover(|gap| gap.bg(theme.hover_fill()))
            .child(dashed().w(space::LG).ml(space::MD).flex_none())
            .child(caption(
                format!("{} {lines} hidden", thousands(count)),
                theme.secondary,
                theme,
            ))
            .child(dashed().flex_1())
            .on_click(
                cx.listener(move |this, _, _, cx| this.open_gap(first, cx)),
            ),
        "Show these lines",
    )
    .into_any_element()
}

/// One visual row of a line's text, unwrapped.
fn line_text(
    content: SharedString,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    ink: Hsla,
    theme: &Theme,
) -> gpui_kit::Div {
    div()
        .h(size::LOG_ROW)
        .whitespace_nowrap()
        .font_family(theme.mono_font.clone())
        .text_size(text::BODY)
        .line_height(size::LOG_ROW)
        .text_color(ink)
        .child(StyledText::new(content).with_highlights(highlights))
}

/// Small mono text: numbers, counts and key hints.
fn caption(
    content: impl Into<SharedString>,
    ink: Hsla,
    theme: &Theme,
) -> gpui_kit::Div {
    div()
        .font_family(theme.mono_font.clone())
        .text_size(text::CAPTION)
        .text_color(ink)
        .whitespace_nowrap()
        .child(content.into())
}

fn centered(message: impl Into<SharedString>, theme: &Theme) -> gpui_kit::Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.secondary)
        .child(message.into())
}

/// A search hit: a soft accent fill, or the current match's solid one.
fn hit_style(current: bool, theme: &Theme) -> HighlightStyle {
    if current {
        HighlightStyle {
            color: Some(theme.on_accent),
            background_color: Some(theme.accent),
            ..Default::default()
        }
    } else {
        HighlightStyle {
            color: Some(theme.bright),
            background_color: Some(theme.accent.opacity(0.28)),
            ..Default::default()
        }
    }
}

/// Byte ranges of each search word in `line`, ignoring case, sorted and
/// merged. Found in the lowercase copy the line keeps for the search, so
/// a frame does not lowercase every row again.
fn search_hits(line: &LogLine, query: &str) -> Vec<Range<usize>> {
    let lower = line.lower();
    // Lowercasing can change byte lengths outside ASCII; then skip
    // highlighting rather than cut a character.
    if lower.len() != line.raw().len() {
        return Vec::new();
    }
    let mut hits: Vec<Range<usize>> = Vec::new();
    for word in query.split_whitespace() {
        let word = word.to_lowercase();
        let mut from = 0;
        while let Some(at) = lower[from..].find(&word) {
            let start = from + at;
            hits.push(start..start + word.len());
            from = start + word.len().max(1);
        }
    }
    hits.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for hit in hits {
        match merged.last_mut() {
            Some(last) if hit.start <= last.end => {
                last.end = last.end.max(hit.end);
            }
            _ => merged.push(hit),
        }
    }
    merged
}

/// The token styles with the search hits laid over them: each hit wins
/// where it falls, and what is left of a token keeps its style.
fn merge_hits(
    styles: Vec<(Range<usize>, HighlightStyle)>,
    hits: &[(Range<usize>, HighlightStyle)],
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut out = Vec::new();
    for (range, style) in styles {
        let mut start = range.start;
        for (hit, _) in hits {
            if hit.end <= start || hit.start >= range.end {
                continue;
            }
            if hit.start > start {
                out.push((start..hit.start, style));
            }
            start = hit.end.min(range.end);
        }
        if start < range.end {
            out.push((start..range.end, style));
        }
    }
    out.extend(hits.iter().cloned());
    out.sort_by_key(|(range, _)| range.start);
    out
}

/// The styles that fall inside `segment`, moved to start at its start.
fn clip_highlights(
    highlights: &[(Range<usize>, HighlightStyle)],
    segment: &Range<usize>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    highlights
        .iter()
        .filter_map(|(range, style)| {
            let start = range.start.max(segment.start);
            let end = range.end.min(segment.end);
            (start < end)
                .then(|| (start - segment.start..end - segment.start, *style))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_are_found_ignoring_case_and_merged() {
        assert_eq!(
            search_hits(
                &LogLine::parse("GitHub said 403; github again"),
                "github"
            ),
            [0..6, 17..23]
        );
        // Overlapping words merge into one hit.
        let merged = search_hits(&LogLine::parse("abcdef"), "abc cde");
        assert_eq!((merged.len(), merged.first()), (1, Some(&(0..5))));
        assert!(search_hits(&LogLine::parse("anything"), "").is_empty());
    }

    #[test]
    fn hits_cut_the_token_styles_they_cross() {
        let token = HighlightStyle::default();
        let hit = HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        };
        let merged = merge_hits(vec![(0..10, token)], &[(3..5, hit)]);
        let ranges: Vec<_> = merged.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, [0..3, 3..5, 5..10]);
    }

    #[test]
    fn a_wrapped_rows_styles_are_clipped_to_it() {
        let style = HighlightStyle::default();
        let clipped =
            clip_highlights(&[(2..8, style), (12..14, style)], &(5..10));
        // Only the first style reaches the row, moved to its start.
        assert_eq!(clipped.len(), 1);
        assert_eq!(clipped[0].0, 0..3);
    }

    #[test]
    fn the_view_is_remembered_but_never_debug() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PREFS_FILE);
        assert_eq!(LogsPrefs::load(Some(&path)), LogsPrefs::default());
        let prefs = LogsPrefs {
            wrap: false,
            search_mode: SearchMode::Highlight,
            context_lines: 3,
            include_diagnostics: true,
            error: true,
            warning: false,
            info: true,
        };
        prefs.save(&path).unwrap();
        assert_eq!(LogsPrefs::load(Some(&path)), prefs);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("debug"), "{saved}");
        // A Context the menu lacks, or a broken file, is the default.
        std::fs::write(&path, r#"{"contextLines": 4}"#).unwrap();
        assert_eq!(LogsPrefs::load(Some(&path)).context_lines, 0);
        std::fs::write(&path, "{").unwrap();
        assert_eq!(LogsPrefs::load(Some(&path)), LogsPrefs::default());
    }
}
