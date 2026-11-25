//! Interactive Ratatui terminal UI with instant SWR rendering, per-subtree
//! freshness badges (`fresh` / `stale` / `updating`), and click/enter
//! priority-interruptible revalidation.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use disktree_scanner::{
    ActiveStorageMode, FallbackReason, Freshness, Metric, NodeId, ScanEvent,
    ScanOptions, ScanSession, ScanSnapshot, ScanStats, SpaceInfo,
    SubtreeStatus, TreeArena, unix_now,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, Paragraph, Row, Table, TableState,
};
use ratatui::{Frame, Terminal};
use rustc_hash::FxHashSet;

use crate::format::{human_age, human_bytes, usage_bar};

/// Sort key for the directory table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortBy {
    #[default]
    Size,
    Files,
    Modified,
    Name,
}

impl SortBy {
    const fn next(self) -> Self {
        match self {
            Self::Size => Self::Files,
            Self::Files => Self::Modified,
            Self::Modified => Self::Name,
            Self::Name => Self::Size,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Size => "size",
            Self::Files => "files",
            Self::Modified => "mtime",
            Self::Name => "name",
        }
    }
}

/// Explicit modal/input state machine for the TUI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputMode {
    #[default]
    Normal,
    Filter,
    Help,
}

/// Explicit exit transition for the TUI event loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitAction {
    Quit,
    ExportMarked,
}

/// Interactive application state (decoupled from terminal I/O for unit tests).
pub struct App {
    pub root_path: PathBuf,
    pub arena: Option<TreeArena>,
    /// Path of the currently viewed directory (tracked by path so live arena
    /// refreshes keep the user in the exact same directory!).
    pub current_path: PathBuf,
    pub freshness: Option<Freshness>,
    pub stats: ScanStats,
    pub storage_mode: ActiveStorageMode,
    pub fallback_reason: Option<FallbackReason>,
    pub space: Option<SpaceInfo>,
    pub options: ScanOptions,
    pub sort_by: SortBy,
    pub revalidating: bool,
    pub active_scan_path: Option<PathBuf>,
    pub updating_paths: FxHashSet<PathBuf>,
    pub marks: FxHashSet<PathBuf>,
    pub mode: InputMode,
    pub filter_text: String,
    pub status_message: Option<String>,
    pub selected: usize,
    pub exit: Option<ExitAction>,
    pub tick: usize,
}

impl App {
    pub fn new(
        root_path: PathBuf,
        options: ScanOptions,
        storage_mode: ActiveStorageMode,
        fallback_reason: Option<FallbackReason>,
        space: Option<SpaceInfo>,
    ) -> Self {
        Self {
            current_path: root_path.clone(),
            root_path,
            arena: None,
            freshness: None,
            stats: ScanStats::default(),
            storage_mode,
            fallback_reason,
            space,
            options,
            sort_by: SortBy::Size,
            revalidating: true,
            active_scan_path: None,
            updating_paths: FxHashSet::default(),
            marks: FxHashSet::default(),
            mode: InputMode::Normal,
            filter_text: String::new(),
            status_message: None,
            selected: 0,
            exit: None,
            tick: 0,
        }
    }

    /// Apply an incoming [`ScanEvent`] from `disktree-scanner`.
    pub fn apply_event(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Snapshot(snap) => self.apply_snapshot(snap),
            ScanEvent::SubtreeUpdating { path, .. } => {
                self.updating_paths.insert(path.clone());
                self.revalidating = true;
                self.active_scan_path = Some(path);
            }
            ScanEvent::SubtreeRefreshed {
                path,
                arena: sub,
                scanned_at,
                ..
            } => {
                self.updating_paths.remove(&path);
                if let Some(main_arena) = &mut self.arena {
                    if path == main_arena.root_path() {
                        *main_arena = (*sub).clone();
                        self.freshness = Some(Freshness::Live { scanned_at });
                    } else if let Ok(rel) =
                        path.strip_prefix(main_arena.root_path())
                    {
                        let parts: Vec<String> = rel
                            .components()
                            .map(|c| {
                                c.as_os_str().to_string_lossy().into_owned()
                            })
                            .collect();
                        if let Some(target_id) = main_arena
                            .find_relative(parts.iter().map(String::as_str))
                        {
                            main_arena.replace_node_contents(target_id, &sub);
                            main_arena.aggregate(
                                self.options.dedup_hardlinks,
                                self.options.include_hidden,
                            );
                            main_arena
                                .sort_children(self.options.byte_metric());
                        }
                    }
                    self.clamp_selection();
                } else {
                    self.arena = Some((*sub).clone());
                }
                self.status_message =
                    Some(format!("Refreshed {}", path.display()));
            }
            ScanEvent::Progress { stats, active_path } => {
                self.stats = stats;
                self.active_scan_path = active_path;
            }
            ScanEvent::StorageFallback { to, reason, .. } => {
                self.storage_mode = to;
                self.fallback_reason = Some(reason);
            }
            ScanEvent::Error { message } => {
                self.status_message = Some(format!("Error: {message}"));
            }
            ScanEvent::Finished => {
                self.revalidating = false;
                self.active_scan_path = None;
                self.updating_paths.clear();
            }
        }
    }

    fn apply_snapshot(&mut self, snap: ScanSnapshot) {
        let mut arena = (*snap.arena).clone();
        arena.aggregate(
            self.options.dedup_hardlinks,
            self.options.include_hidden,
        );
        arena.sort_children(self.options.byte_metric());
        self.revalidating = !matches!(
            snap.freshness,
            Freshness::Fresh { .. } | Freshness::Live { .. }
        );
        self.freshness = Some(snap.freshness);
        self.stats = snap.stats;
        self.storage_mode = snap.storage_mode;
        self.fallback_reason = snap.fallback_reason;
        self.arena = Some(arena);
        self.clamp_selection();
    }

    /// Resolve `self.current_path` to its [`NodeId`] in the active arena.
    pub fn current_node_id(&self) -> Option<NodeId> {
        let arena = self.arena.as_ref()?;
        if self.current_path == arena.root_path() {
            return Some(arena.root());
        }
        let rel = self.current_path.strip_prefix(arena.root_path()).ok()?;
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        arena
            .find_relative(parts.iter().map(String::as_str))
            .or_else(|| Some(arena.root()))
    }

    /// Visible, filtered, sorted child [`NodeId`]s for the current directory.
    pub fn visible_children(&self) -> Vec<NodeId> {
        let Some(arena) = &self.arena else {
            return Vec::new();
        };
        let Some(cur) = self.current_node_id() else {
            return Vec::new();
        };
        let needle = self.filter_text.trim().to_ascii_lowercase();
        let mut kids: Vec<NodeId> = arena
            .children(cur)
            .filter(|&id| {
                let node = arena.node(id);
                if !self.options.include_hidden && node.hidden() {
                    return false;
                }
                if !needle.is_empty()
                    && !arena.name(id).to_ascii_lowercase().contains(&needle)
                {
                    return false;
                }
                true
            })
            .collect();

        let metric = self.active_metric();
        kids.sort_unstable_by(|&a, &b| {
            let na = arena.node(a);
            let nb = arena.node(b);
            match self.sort_by {
                SortBy::Size => nb
                    .value(metric)
                    .cmp(&na.value(metric))
                    .then_with(|| arena.name(a).cmp(arena.name(b))),
                SortBy::Files => nb
                    .files
                    .cmp(&na.files)
                    .then_with(|| arena.name(a).cmp(arena.name(b))),
                SortBy::Modified => nb
                    .modified
                    .cmp(&na.modified)
                    .then_with(|| arena.name(a).cmp(arena.name(b))),
                SortBy::Name => arena.name(a).cmp(arena.name(b)),
            }
        });
        kids
    }

    const fn active_metric(&self) -> Metric {
        if self.options.apparent_size {
            Metric::ApparentBytes
        } else {
            Metric::Bytes
        }
    }

    fn clamp_selection(&mut self) {
        let count = self.visible_children().len();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Handle a single keystroke, returning an optional path to immediately
    /// prioritize in the scanner.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PathBuf> {
        if key.kind != KeyEventKind::Press {
            return None;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
        {
            self.exit = Some(ExitAction::Quit);
            return None;
        }

        match self.mode {
            InputMode::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('?' | 'q')) {
                    self.mode = InputMode::Normal;
                }
                return None;
            }
            InputMode::Filter => {
                match key.code {
                    KeyCode::Esc => {
                        self.mode = InputMode::Normal;
                        self.filter_text.clear();
                        self.clamp_selection();
                    }
                    KeyCode::Enter => {
                        self.mode = InputMode::Normal;
                        self.clamp_selection();
                    }
                    KeyCode::Backspace => {
                        self.filter_text.pop();
                        self.clamp_selection();
                    }
                    KeyCode::Char(c) => {
                        self.filter_text.push(c);
                        self.clamp_selection();
                    }
                    _ => {}
                }
                return None;
            }
            InputMode::Normal => {}
        }

        match key.code {
            KeyCode::Char('q') => {
                self.exit = Some(ExitAction::Quit);
            }
            KeyCode::Char('?') => {
                self.mode = InputMode::Help;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let len = self.visible_children().len();
                if len > 0 && self.selected + 1 < len {
                    self.selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                let len = self.visible_children().len();
                self.selected = len.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                let kids = self.visible_children();
                if let Some(&child_id) = kids.get(self.selected)
                    && let Some(arena) = &self.arena
                    && arena.node(child_id).is_dir()
                {
                    let target = arena.path_of(child_id);
                    let status = arena.node(child_id).status();
                    self.current_path.clone_from(&target);
                    self.filter_text.clear();
                    self.selected = 0;
                    // Priority-interrupt background refresh when descending
                    // into a stale or unscanned directory!
                    if matches!(
                        status,
                        SubtreeStatus::Stale { .. } | SubtreeStatus::Unscanned
                    ) {
                        self.updating_paths.insert(target.clone());
                        return Some(target);
                    }
                }
            }
            KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left => {
                if self.current_path != self.root_path
                    && let Some(parent) = self.current_path.parent()
                {
                    let prev_path = self.current_path.clone();
                    self.current_path = parent.to_path_buf();
                    self.filter_text.clear();
                    // Re-select the directory we just climbed out of.
                    let kids = self.visible_children();
                    if let Some(arena) = &self.arena {
                        self.selected = kids
                            .iter()
                            .position(|&id| arena.path_of(id) == prev_path)
                            .unwrap_or(0);
                    } else {
                        self.selected = 0;
                    }
                }
            }
            KeyCode::Char('a') => {
                self.options.apparent_size = !self.options.apparent_size;
                if let Some(arena) = &mut self.arena {
                    arena.sort_children(self.options.byte_metric());
                }
                self.status_message = Some(if self.options.apparent_size {
                    "Showing apparent size (zero rescan)".to_owned()
                } else {
                    "Showing allocated disk blocks (zero rescan)".to_owned()
                });
            }
            KeyCode::Char('H') => {
                self.options.include_hidden = !self.options.include_hidden;
                if let Some(arena) = &mut self.arena {
                    arena.aggregate(
                        self.options.dedup_hardlinks,
                        self.options.include_hidden,
                    );
                    arena.sort_children(self.options.byte_metric());
                }
                self.clamp_selection();
                self.status_message = Some(if self.options.include_hidden {
                    "Showing hidden entries".to_owned()
                } else {
                    "Hiding dotfiles".to_owned()
                });
            }
            KeyCode::Char('s') => {
                self.sort_by = self.sort_by.next();
                self.status_message =
                    Some(format!("Sorted by {}", self.sort_by.label()));
            }
            KeyCode::Char('/') => {
                self.mode = InputMode::Filter;
            }
            KeyCode::Char(' ') => {
                let kids = self.visible_children();
                if let Some(&child_id) = kids.get(self.selected)
                    && let Some(arena) = &self.arena
                {
                    let path = arena.path_of(child_id);
                    if !self.marks.remove(&path) {
                        self.marks.insert(path);
                    }
                }
            }
            KeyCode::Char('e') => {
                if self.marks.is_empty() {
                    self.status_message = Some(
                        "No paths marked (press Space to mark)".to_owned(),
                    );
                } else {
                    self.exit = Some(ExitAction::ExportMarked);
                }
            }
            KeyCode::Char('r') => {
                let kids = self.visible_children();
                let target = if let Some(&child_id) = kids.get(self.selected)
                    && let Some(arena) = &self.arena
                    && arena.node(child_id).is_dir()
                {
                    arena.path_of(child_id)
                } else {
                    self.current_path.clone()
                };
                self.updating_paths.insert(target.clone());
                self.status_message =
                    Some(format!("Prioritizing refresh: {}", target.display()));
                return Some(target);
            }
            KeyCode::Char('R') => {
                let target = self.root_path.clone();
                self.updating_paths.insert(target.clone());
                self.status_message = Some("Refreshing full root…".to_owned());
                return Some(target);
            }
            _ => {}
        }
        None
    }

    /// Render the current state to a Ratatui [`Frame`].
    pub fn draw(&self, frame: &mut Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Min(5),
                Constraint::Length(2),
            ])
            .split(frame.area());

        self.draw_header(frame, chunks[0]);
        self.draw_space_bar(frame, chunks[1]);
        self.draw_table(frame, chunks[2]);
        self.draw_footer(frame, chunks[3]);

        if self.mode == InputMode::Help {
            draw_help_overlay(frame, frame.area());
        }
    }

    fn draw_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let now = unix_now();
        let spinner =
            ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][self.tick % 10];

        let (badge_text, badge_color) = if !self.updating_paths.is_empty() {
            (format!("{spinner} UPDATING"), Color::Cyan)
        } else if self.revalidating {
            match &self.freshness {
                Some(Freshness::Stale { newest_at, .. }) => (
                    format!(
                        "{spinner} STALE ({}) · REVALIDATING",
                        human_age(now.saturating_sub(*newest_at))
                    ),
                    Color::Yellow,
                ),
                Some(Freshness::Partial { .. }) => {
                    (format!("{spinner} PARTIAL · SCANNING"), Color::Yellow)
                }
                _ => (format!("{spinner} SCANNING"), Color::Cyan),
            }
        } else {
            match &self.freshness {
                Some(Freshness::Fresh { newest_at, .. }) => (
                    format!(
                        "CACHED · {}",
                        human_age(now.saturating_sub(*newest_at))
                    ),
                    Color::Green,
                ),
                Some(Freshness::Live { .. }) => {
                    ("LIVE".to_owned(), Color::Green)
                }
                Some(Freshness::Stale { newest_at, .. }) => (
                    format!(
                        "STALE · {}",
                        human_age(now.saturating_sub(*newest_at))
                    ),
                    Color::Yellow,
                ),
                _ => ("READY".to_owned(), Color::DarkGray),
            }
        };

        let storage_label = match (&self.storage_mode, &self.fallback_reason) {
            (ActiveStorageMode::Persistent, _) => "db".to_owned(),
            (ActiveStorageMode::TempDb, _) => "temp-db".to_owned(),
            (ActiveStorageMode::InMemory, Some(reason)) => {
                format!("in-memory ({reason:?})")
            }
            (ActiveStorageMode::InMemory, None) => "in-memory".to_owned(),
            (ActiveStorageMode::Streaming, _) => "stream".to_owned(),
        };

        let mode_label = if self.options.apparent_size {
            "apparent"
        } else {
            "blocks"
        };

        let title = Line::from(vec![
            Span::styled(
                " disktree-cli ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                self.current_path.display().to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("[{badge_text}]"),
                Style::default()
                    .fg(badge_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!(
                    "{mode_label} · sort:{} · {storage_label}",
                    self.sort_by.label()
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        let current_summary = if let Some(arena) = &self.arena
            && let Some(cur) = self.current_node_id()
        {
            let node = arena.node(cur);
            let bytes = if self.options.apparent_size {
                node.apparent_bytes
            } else {
                node.allocated_bytes
            };
            format!(
                " {} · {} files · {} dirs · {} marked",
                human_bytes(bytes),
                node.files,
                node.dirs,
                self.marks.len()
            )
        } else {
            " Scanning filesystem…".to_owned()
        };

        let header = Paragraph::new(current_summary)
            .block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(header, area);
    }

    fn draw_space_bar(&self, frame: &mut Frame<'_>, area: Rect) {
        if let Some(space) = self.space {
            let ratio = f64::from(space.used_fraction()).clamp(0.0, 1.0);
            let label = format!(
                "Volume: {} used / {} total ({} available)",
                human_bytes(space.used()),
                human_bytes(space.total),
                human_bytes(space.available),
            );
            let color = if ratio > 0.90 {
                Color::Red
            } else if ratio > 0.75 {
                Color::Yellow
            } else {
                Color::Blue
            };
            let gauge = Gauge::default()
                .ratio(ratio)
                .label(label)
                .gauge_style(Style::default().fg(color).bg(Color::DarkGray));
            frame.render_widget(gauge, area);
        }
    }

    fn draw_table(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(arena) = &self.arena else {
            let placeholder = Paragraph::new(
                "Scanning… (subsequent runs within 15m load from cache in milliseconds)",
            )
            .block(Block::default().borders(Borders::ALL).title("Entries"));
            frame.render_widget(placeholder, area);
            return;
        };

        let cur = self.current_node_id().unwrap_or_else(|| arena.root());
        let parent_total = arena.node(cur).value(self.active_metric()).max(1);
        let kids = self.visible_children();
        let now = unix_now();

        let rows: Vec<Row<'_>> = kids
            .iter()
            .map(|&id| {
                let node = arena.node(id);
                let path = arena.path_of(id);
                let marked = if self.marks.contains(&path) { "*" } else { " " };
                let bytes = if self.options.apparent_size {
                    node.effective_apparent_bytes()
                } else {
                    node.effective_allocated_bytes()
                };
                let frac = node.value(self.active_metric()) as f64
                    / parent_total as f64;
                let pct = format!("{:>5.1}%", frac * 100.0);
                let bar = usage_bar(frac, 14);

                let (status_str, status_color) = if !node.is_dir() {
                    (String::new(), Color::DarkGray)
                } else if self.updating_paths.contains(&path) {
                    ("updating…".to_owned(), Color::Cyan)
                } else {
                    match node.status() {
                        SubtreeStatus::Fresh { scanned_at } => (
                            format!(
                                "fresh {}",
                                human_age(now.saturating_sub(scanned_at))
                            ),
                            Color::Green,
                        ),
                        SubtreeStatus::Stale { scanned_at } => (
                            format!(
                                "stale {}",
                                human_age(now.saturating_sub(scanned_at))
                            ),
                            Color::Yellow,
                        ),
                        SubtreeStatus::Updating { .. } => {
                            ("updating…".to_owned(), Color::Cyan)
                        }
                        SubtreeStatus::Unscanned => {
                            ("unscanned".to_owned(), Color::DarkGray)
                        }
                    }
                };

                let mut display_name = arena.name(id).to_owned();
                if node.is_dir() {
                    display_name.push('/');
                }
                if node.read_error() {
                    display_name.push_str(" [unreadable]");
                }

                let name_style = if node.read_error() {
                    Style::default().fg(Color::Red)
                } else if node.hidden() {
                    Style::default().fg(Color::DarkGray)
                } else if node.is_dir() {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                Row::new(vec![
                    Span::styled(
                        marked,
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("{:>10}", human_bytes(bytes))),
                    Span::styled(pct, Style::default().fg(Color::DarkGray)),
                    Span::styled(bar, Style::default().fg(Color::Blue)),
                    Span::styled(
                        format!("{:>7}", node.files),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(status_str, Style::default().fg(status_color)),
                    Span::styled(display_name, name_style),
                ])
            })
            .collect();

        let header = Row::new(vec![
            " ",
            "Size",
            "Share",
            "Usage",
            "Files",
            "Freshness",
            "Name",
        ])
        .style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );

        let title = if self.mode == InputMode::Filter {
            format!("Filter: {}_", self.filter_text)
        } else if !self.filter_text.is_empty() {
            format!("Entries (filter: {})", self.filter_text)
        } else {
            format!("Entries ({})", kids.len())
        };

        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(11),
                Constraint::Length(7),
                Constraint::Length(15),
                Constraint::Length(8),
                Constraint::Length(16),
                Constraint::Min(16),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

        let mut table_state = TableState::default();
        if !kids.is_empty() {
            table_state.select(Some(self.selected));
        }
        frame.render_stateful_widget(table, area, &mut table_state);
    }

    fn draw_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let line1 = if let Some(active) = &self.active_scan_path {
            Line::from(vec![
                Span::styled(
                    format!(
                        "walked:{} skipped:{} err:{} ",
                        self.stats.walked_dirs,
                        self.stats.skipped_fresh_dirs,
                        self.stats.errors,
                    ),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    active.display().to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        } else if let Some(msg) = &self.status_message {
            Line::from(Span::styled(
                msg.as_str(),
                Style::default().fg(Color::Yellow),
            ))
        } else {
            Line::from(Span::styled(
                format!(
                    "walked:{} skipped-fresh:{} ({} ms)",
                    self.stats.walked_dirs,
                    self.stats.skipped_fresh_dirs,
                    self.stats.duration_ms
                ),
                Style::default().fg(Color::DarkGray),
            ))
        };

        let line2 = Line::from(Span::styled(
            "j/k move · Enter/l open · h/Bksp up · r refresh · R full · a apparent · H hidden · s sort · / filter · Space mark · e export · ? help · q quit",
            Style::default().fg(Color::DarkGray),
        ));

        frame.render_widget(Paragraph::new(vec![line1, line2]), area);
    }
}

fn draw_help_overlay(frame: &mut Frame<'_>, area: Rect) {
    let width = 62u16.min(area.width.saturating_sub(4));
    let height = 16u16.min(area.height.saturating_sub(4));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let help = Paragraph::new(vec![
        Line::from("  j / k / Up / Down    Move selection"),
        Line::from("  Enter / l / Right    Descend (prioritizes stale dir)"),
        Line::from("  Backspace / h / Left Ascend to parent directory"),
        Line::from("  r                    Immediate priority refresh of dir"),
        Line::from("  R                    Force full refresh of root"),
        Line::from("  a                    Toggle apparent vs allocated bytes"),
        Line::from("  H                    Toggle hidden dotfiles"),
        Line::from("  s                    Cycle sort (size/files/mtime/name)"),
        Line::from("  /                    Filter current directory"),
        Line::from("  Space                Mark / unmark path (read-only)"),
        Line::from("  e                    Exit & print marked paths list"),
        Line::from("  ? / Esc / q          Close help / quit"),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" disktree-cli keys ")
            .style(Style::default().bg(Color::Black)),
    );
    frame.render_widget(help, rect);
}

/// Run the interactive Ratatui event loop until the user quits.
/// Returns the sorted list of marked paths if the user pressed `e` to export.
pub fn run_tui(
    session: &ScanSession,
    root: &Path,
    options: ScanOptions,
    storage_mode: ActiveStorageMode,
    fallback_reason: Option<FallbackReason>,
    space: Option<SpaceInfo>,
) -> io::Result<Vec<PathBuf>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(
        root.to_path_buf(),
        options,
        storage_mode,
        fallback_reason,
        space,
    );

    let loop_res = (|| -> io::Result<()> {
        while app.exit.is_none() {
            // Drain all ready scanner events without blocking.
            while let Ok(ev) = session.try_recv() {
                app.apply_event(ev);
            }
            app.tick = app.tick.wrapping_add(1);
            terminal.draw(|frame| app.draw(frame))?;

            let poll_ms = if app.revalidating || !app.updating_paths.is_empty()
            {
                33
            } else {
                120
            };
            if event::poll(Duration::from_millis(poll_ms))?
                && let Event::Key(key) = event::read()?
                && let Some(prio_path) = app.handle_key(key)
            {
                let _ = session.prioritize(&prio_path);
            }
        }
        Ok(())
    })();

    session.cancel();
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    loop_res?;

    if app.exit == Some(ExitAction::ExportMarked) {
        let mut exported: Vec<PathBuf> = app.marks.into_iter().collect();
        exported.sort();
        Ok(exported)
    } else {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use disktree_scanner::{NodeFlags, NodeKind};
    use ratatui::backend::TestBackend;

    fn sample_app() -> App {
        let mut arena = TreeArena::new(PathBuf::from("/demo"), "demo");
        let stale_dir = arena.add_child(
            arena.root(),
            "stale_dir",
            NodeKind::Directory,
            0,
            0,
            100,
            NodeFlags::new(NodeKind::Directory),
        );
        arena
            .node_mut(stale_dir)
            .set_status(SubtreeStatus::Stale { scanned_at: 10 });
        arena.add_child(
            stale_dir,
            "big.bin",
            NodeKind::File,
            8192,
            4000,
            100,
            NodeFlags::new(NodeKind::File),
        );
        let mut hidden_flags = NodeFlags::new(NodeKind::File);
        hidden_flags.set(NodeFlags::HIDDEN, true);
        arena.add_child(
            arena.root(),
            ".env",
            NodeKind::File,
            4096,
            20,
            100,
            hidden_flags,
        );
        arena.aggregate(true, true);
        arena.sort_children(Metric::Bytes);

        let mut app = App::new(
            PathBuf::from("/demo"),
            ScanOptions::default(),
            ActiveStorageMode::Persistent,
            None,
            Some(SpaceInfo {
                total: 1_000_000,
                free: 400_000,
                available: 350_000,
            }),
        );
        app.apply_event(ScanEvent::Snapshot(ScanSnapshot {
            arena: Arc::new(arena),
            freshness: Freshness::Stale {
                oldest_at: 10,
                newest_at: 10,
                stale_dirs: 1,
            },
            stats: ScanStats::default(),
            storage_mode: ActiveStorageMode::Persistent,
            fallback_reason: None,
        }));
        app
    }

    #[test]
    fn tui_draws_frames_and_prioritizes_stale_dir_on_enter() {
        let mut app = sample_app();
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal.draw(|f| app.draw(f)).expect("draw initial");
        let buf = terminal.backend().buffer();
        let rendered: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("disktree-cli"));
        assert!(rendered.contains("stale_dir/"));

        // Pressing Enter on `stale_dir` descends AND returns it for immediate
        // priority revalidation!
        let prio =
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(prio, Some(PathBuf::from("/demo/stale_dir")));
        assert_eq!(app.current_path, PathBuf::from("/demo/stale_dir"));

        // Press Backspace to climb back up, toggle hidden (`H`) and apparent (`a`),
        // open help (`?`), and draw again without panicking.
        assert!(
            app.handle_key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::NONE
            ))
            .is_none()
        );
        assert_eq!(app.visible_children().len(), 2);
        app.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT));
        assert_eq!(app.visible_children().len(), 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(app.options.apparent_size);
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Help);
        terminal.draw(|f| app.draw(f)).expect("draw help");
    }
}
