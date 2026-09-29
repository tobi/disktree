//! Terminal session state. Filesystem decisions remain in `disktree-core`.

use std::cell::{Cell, RefCell};
use std::fs::OpenOptions;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use disktree_core::filter::{Matches, filter};
use disktree_core::insights::{Candidate, INSIGHT_LIMIT, worth_a_look};
use disktree_core::removal::{
    Plan, RemovalEvent, RemovalHandle, RemovalMode, Target, TrashBackend,
    detect_trash_backend, plan,
};
use disktree_core::scan::{ScanHandle, ScanOptions, ScanSnapshot};
use disktree_core::space::{
    SpaceInfo, Volume, device_for, space_info, volume_root_for, volumes,
};
use disktree_core::tree::{
    Metric, Node, aggregate_view, path_of, switch_measure,
};
use disktree_core::treemap::{
    LayoutOptions, Rect as MapRect, Tile, TileKind, layout_filtered,
};
use ratatui::layout::Rect;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Explore,
    Review,
    Confirm,
    Running,
    Done,
    Help,
    Insights,
    SavePrompt,
    Volumes,
}

/// The GUI's Size | Files | Age choice. Age keeps size-based geometry and
/// changes the mosaic's color encoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    Size,
    Files,
    Age,
}

impl ViewMode {
    pub const fn next(self) -> Self {
        match self {
            Self::Size => Self::Files,
            Self::Files => Self::Age,
            Self::Age => Self::Size,
        }
    }

    pub const fn metric(self) -> Metric {
        match self {
            Self::Files => Metric::Files,
            Self::Size | Self::Age => Metric::Bytes,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Size => "Size",
            Self::Files => "Files",
            Self::Age => "Age",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    fn gap(self, from: Rect, target: Rect) -> Option<f32> {
        let gap = match self {
            Self::Left => f32::from(from.x) - f32::from(target.right()),
            Self::Right => f32::from(target.x) - f32::from(from.right()),
            Self::Up => f32::from(from.y) - f32::from(target.bottom()),
            Self::Down => f32::from(target.y) - f32::from(from.bottom()),
        };
        (gap >= -0.5).then_some(gap.max(0.0))
    }

    fn offset(self, from: Rect, target: Rect) -> f32 {
        let overlap = |a: u16, a_end: u16, b: u16, b_end: u16| {
            f32::from(a_end.min(b_end).saturating_sub(a.max(b)))
        };
        match self {
            Self::Left | Self::Right => {
                f32::from(from.height.min(target.height))
                    - overlap(from.y, from.bottom(), target.y, target.bottom())
            }
            Self::Up | Self::Down => {
                f32::from(from.width.min(target.width))
                    - overlap(from.x, from.right(), target.x, target.right())
            }
        }
        .max(0.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub total: usize,
    pub removed: u64,
    pub bytes: u64,
    pub failed: usize,
}

/// Direct child positions need no index allocation; filtered positions are
/// cached because checking every child on each animated frame is expensive.
#[derive(Clone, Debug)]
pub enum VisibleChildren {
    All(usize),
    Filtered(Rc<Vec<usize>>),
}

impl VisibleChildren {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::All(count) => *count,
            Self::Filtered(indices) => indices.len(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn get(&self, position: usize) -> Option<usize> {
        match self {
            Self::All(count) => (position < *count).then_some(position),
            Self::Filtered(indices) => indices.get(position).copied(),
        }
    }

    fn position(&self, child: usize) -> Option<usize> {
        match self {
            Self::All(count) => (child < *count).then_some(child),
            Self::Filtered(indices) => indices.binary_search(&child).ok(),
        }
    }
}

#[derive(Debug)]
pub struct App {
    pub root: PathBuf,
    pub options: ScanOptions,
    pub mode: ViewMode,
    pub tree: Option<Node>,
    pub scan: Option<ScanHandle>,
    pub progress: ScanSnapshot,
    pub scan_error: Option<String>,
    pub current: Vec<usize>,
    pub selected: Option<Vec<usize>>,
    pub screen: Screen,
    pub marks: Vec<Target>,
    pub space: Option<SpaceInfo>,
    pub device: Option<String>,
    pub view_depth: u32,
    pub scan_elapsed: Option<Duration>,
    pub scanned_at: i64,
    pub trash_backend: TrashBackend,
    pub removal_mode: RemovalMode,
    pub run: Option<RemovalHandle>,
    pub run_cancelled: bool,
    pub run_summary: RunSummary,
    pub run_log: Vec<(PathBuf, Result<(), String>)>,
    pub measured_gain: Option<i128>,
    pub baseline_space: Option<SpaceInfo>,
    pub notice: Option<String>,
    pub search_open: bool,
    pub search: String,
    pub matches: Option<Matches>,
    pub list_view: bool,
    pub list_scroll: usize,
    pub scroll: usize,
    pub insights: Vec<Candidate>,
    pub insight_index: usize,
    pub volumes: Vec<Volume>,
    pub volume_highlight: usize,
    pub volumes_loading: bool,
    pub save_path: String,
    pub quit: bool,
    pub stdout_prompt: Option<String>,
    viewport: Rect,
    scan_started: Option<Instant>,
    scan_metric: Metric,
    scan_apparent: bool,
    scan_hidden: bool,
    full_hidden_loaded: bool,
    hidden_stash: Vec<(PathBuf, Vec<Node>)>,
    last_space_check: Instant,
    marquee_started: Instant,
    focused_marquee_started: Instant,
    animation_wake: Cell<Option<Instant>>,
    // Layout sorts children; only navigation, scans, filters and resize can
    // change it, so animation frames can reuse the same tiles.
    tiles_cache: RefCell<Option<(Rect, Rc<Vec<Tile>>)>>,
    children_cache: RefCell<Option<Rc<Vec<usize>>>>,
    volume_scan: Option<Receiver<Vec<Volume>>>,
}

impl App {
    pub fn new(root: PathBuf, options: ScanOptions) -> Self {
        let trash_backend = detect_trash_backend();
        let mode = if options.metric == Metric::Files {
            ViewMode::Files
        } else {
            ViewMode::Size
        };
        let mut app = Self {
            space: space_info(&root).ok(),
            device: device_for(&root),
            root,
            mode,
            scan_metric: options.metric,
            scan_apparent: options.apparent_size,
            scan_hidden: options.include_hidden,
            full_hidden_loaded: false,
            hidden_stash: Vec::new(),
            options,
            tree: None,
            scan: None,
            progress: ScanSnapshot::default(),
            scan_error: None,
            current: Vec::new(),
            selected: None,
            screen: Screen::Explore,
            marks: Vec::new(),
            view_depth: 3,
            scan_elapsed: None,
            scanned_at: now_seconds(),
            trash_backend,
            removal_mode: if trash_backend.is_available() {
                RemovalMode::Trash
            } else {
                RemovalMode::Permanent
            },
            run: None,
            run_cancelled: false,
            run_summary: RunSummary::default(),
            run_log: Vec::new(),
            measured_gain: None,
            baseline_space: None,
            notice: None,
            search_open: false,
            search: String::new(),
            matches: None,
            list_view: false,
            list_scroll: 0,
            scroll: 0,
            insights: Vec::new(),
            insight_index: 0,
            volumes: Vec::new(),
            volume_highlight: 0,
            volumes_loading: false,
            save_path: String::new(),
            quit: false,
            stdout_prompt: None,
            viewport: Rect::new(0, 0, 120, 35),
            scan_started: None,
            last_space_check: Instant::now(),
            marquee_started: Instant::now(),
            focused_marquee_started: Instant::now(),
            animation_wake: Cell::new(None),
            tiles_cache: RefCell::new(None),
            children_cache: RefCell::new(None),
            volume_scan: None,
        };
        disktree_core::removal::prime_mount_points();
        app.start_scan();
        app
    }

    pub fn start_scan(&mut self) {
        if let Some(scan) = &self.scan {
            scan.cancel();
        }
        self.tree = None;
        self.hidden_stash.clear();
        self.scan_error = None;
        self.scan_elapsed = None;
        self.scan_started = Some(Instant::now());
        self.progress = ScanSnapshot::default();
        self.current.clear();
        self.selected = None;
        self.matches = None;
        self.search.clear();
        self.search_open = false;
        self.insights.clear();
        self.scroll = 0;
        self.list_scroll = 0;
        self.reset_marquee();
        self.invalidate_view_cache();
        self.scan_metric = self.options.metric;
        self.scan_apparent = self.options.apparent_size;
        self.scan_hidden =
            self.options.include_hidden || self.full_hidden_loaded;
        let mut scan_options = self.options.clone();
        scan_options.include_hidden = self.scan_hidden;
        self.scan = Some(ScanHandle::spawn(self.root.clone(), scan_options));
    }

    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        if let Some(receiver) = &self.volume_scan {
            match receiver.try_recv() {
                Ok(volumes) => {
                    self.volumes = volumes;
                    self.volumes_loading = false;
                    self.volume_scan = None;
                    changed = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.volumes_loading = false;
                    self.volume_scan = None;
                    self.notice = Some("Could not list volumes".to_string());
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(scan) = &self.scan {
            let snapshot = scan.progress.snapshot();
            if self.progress != snapshot {
                self.progress = snapshot;
                changed = true;
            }
            if let Some(outcome) = scan.poll() {
                self.progress = scan.progress.snapshot();
                self.scan_elapsed =
                    self.scan_started.take().map(|start| start.elapsed());
                self.scan = None;
                match outcome {
                    Ok(mut tree) => {
                        if self.scan_apparent != self.options.apparent_size {
                            switch_measure(
                                &mut tree,
                                self.options.metric,
                                self.options.dedup_hardlinks,
                            );
                        } else if self.scan_metric != self.options.metric {
                            aggregate_view(
                                &mut tree,
                                self.options.metric,
                                self.options.dedup_hardlinks,
                            );
                        }
                        self.refresh_marks(&tree);
                        self.full_hidden_loaded = self.scan_hidden;
                        self.hidden_stash.clear();
                        if !self.options.include_hidden
                            && self.full_hidden_loaded
                        {
                            hide_hidden(
                                &mut tree,
                                &self.root,
                                &mut self.hidden_stash,
                            );
                            aggregate_view(
                                &mut tree,
                                self.options.metric,
                                self.options.dedup_hardlinks,
                            );
                            disktree_core::classify::classify(&mut tree);
                        }
                        self.insights =
                            worth_a_look(&tree, now_seconds(), INSIGHT_LIMIT);
                        self.scanned_at = now_seconds();
                        self.selected = first_child(&tree, &[]);
                        self.tree = Some(tree);
                        self.invalidate_view_cache();
                        self.keep_selection_visible();
                        self.reset_marquee();
                    }
                    Err(error) => self.scan_error = Some(error.to_string()),
                }
                changed = true;
            }
        }
        if let Some(run) = &self.run {
            let mut finished = false;
            while let Some(event) = run.poll() {
                changed = true;
                match event {
                    RemovalEvent::Start { total } => {
                        self.run_summary.total = total;
                    }
                    RemovalEvent::Item {
                        path,
                        bytes,
                        outcome,
                    } => {
                        if outcome.is_ok() {
                            self.run_summary.removed += 1;
                            self.run_summary.bytes += bytes;
                        } else {
                            self.run_summary.failed += 1;
                        }
                        self.run_log.push((path, outcome));
                    }
                    RemovalEvent::Done {
                        removed,
                        bytes,
                        failed,
                    } => {
                        self.run_summary.removed = removed;
                        self.run_summary.bytes = bytes;
                        self.run_summary.failed = failed;
                        finished = true;
                    }
                }
            }
            if finished {
                self.run = None;
                self.marks.retain(|target| {
                    !self.run_log.iter().any(|(path, outcome)| {
                        outcome.is_ok() && target.path.starts_with(path)
                    })
                });
                let _ = self.refresh_space();
                self.measured_gain = self.baseline_space.zip(self.space).map(
                    |(before, after)| {
                        i128::from(after.available)
                            - i128::from(before.available)
                    },
                );
                self.screen = Screen::Done;
                self.start_scan();
            }
        }
        if self.last_space_check.elapsed() >= Duration::from_millis(1200) {
            changed |= self.refresh_space();
        }
        if self
            .animation_wake
            .get()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            changed = true;
        }
        changed
    }

    fn refresh_space(&mut self) -> bool {
        let old = self.space;
        self.space = space_info(&self.root).ok();
        self.last_space_check = Instant::now();
        self.space != old
    }

    fn refresh_marks(&mut self, tree: &Node) {
        for target in &mut self.marks {
            if let Some(node) = node_at_path(&self.root, tree, &target.path) {
                target.bytes = node.bytes;
                target.is_dir = node.is_dir();
            } else {
                target.bytes = 0;
            }
        }
    }

    pub fn current_node(&self) -> Option<&Node> {
        self.tree.as_ref()?.resolve(&self.current)
    }

    pub fn selected_node(&self) -> Option<&Node> {
        self.tree.as_ref()?.resolve(self.selected.as_deref()?)
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        Some(path_of(
            &self.root,
            self.tree.as_ref()?,
            self.selected.as_deref()?,
        ))
    }

    pub fn current_path(&self) -> Option<PathBuf> {
        Some(path_of(&self.root, self.tree.as_ref()?, &self.current))
    }

    pub fn plan(&self) -> Plan {
        plan(&self.marks, &self.root)
    }

    pub fn marked_bytes(&self) -> u64 {
        self.plan().bytes()
    }

    pub fn is_marked(&self, path: &Path) -> bool {
        self.marks.iter().any(|target| target.path == path)
    }

    pub fn marked_ancestor(&self, path: &Path) -> Option<&Path> {
        self.marks
            .iter()
            .map(|target| target.path.as_path())
            .find(|marked| *marked != path && path.starts_with(marked))
    }

    pub fn set_viewport(&mut self, width: u16, height: u16) {
        self.viewport = Rect::new(0, 0, width, height);
        self.invalidate_view_cache();
        self.keep_selection_visible();
        self.reset_marquee();
    }

    #[cfg(test)]
    pub(crate) const fn viewport(&self) -> Rect {
        self.viewport
    }

    pub(crate) fn marquee_elapsed(&self) -> Duration {
        self.marquee_started.elapsed()
    }

    pub(crate) fn focused_marquee_elapsed(&self) -> Duration {
        self.focused_marquee_started.elapsed()
    }

    #[cfg(test)]
    pub(crate) fn set_marquee_elapsed_for_test(&mut self, elapsed: Duration) {
        self.marquee_started = Instant::now()
            .checked_sub(elapsed)
            .expect("test animation elapsed fits in Instant");
        self.focused_marquee_started = self.marquee_started;
        self.animation_wake.set(None);
    }

    fn reset_marquee(&mut self) {
        let now = Instant::now();
        self.marquee_started = now;
        self.focused_marquee_started = now;
        self.animation_wake.set(None);
    }

    fn reset_focused_marquee(&mut self) {
        self.focused_marquee_started = Instant::now();
        self.animation_wake.set(None);
    }

    pub(crate) fn begin_animation_frame(&self) {
        self.animation_wake.set(None);
    }

    pub(crate) fn schedule_animation(&self, delay: Duration) {
        let deadline = Instant::now() + delay;
        if self.animation_wake.get().is_none_or(|next| deadline < next) {
            self.animation_wake.set(Some(deadline));
        }
    }

    fn invalidate_view_cache(&self) {
        self.tiles_cache.borrow_mut().take();
        self.children_cache.borrow_mut().take();
    }

    pub(crate) fn header_height(&self, width: u16) -> u16 {
        if width < 60 || (self.screen == Screen::Explore && self.list_view) {
            2
        } else {
            3
        }
    }

    pub(crate) fn body_area(&self, size: Rect) -> Rect {
        let head = self.header_height(size.width);
        Rect::new(
            size.x,
            size.y + head,
            size.width,
            size.height.saturating_sub(head + 2),
        )
    }

    pub(crate) fn split_explore(area: Rect) -> (Rect, Rect, bool) {
        if area.width >= 105 && area.height >= 18 {
            // The side panel keeps enough cells for typical disk names.
            let side_width = area.width.min(40);
            let map =
                Rect::new(area.x, area.y, area.width - side_width, area.height);
            let side = Rect::new(map.right(), area.y, side_width, area.height);
            (map, side, true)
        } else {
            let detail_height = if area.height >= 16 { 5 } else { 3 };
            let map = Rect::new(
                area.x,
                area.y,
                area.width,
                area.height.saturating_sub(detail_height),
            );
            let detail =
                Rect::new(area.x, map.bottom(), area.width, detail_height);
            (map, detail, false)
        }
    }

    pub(crate) const fn is_list(&self, map: Rect) -> bool {
        self.list_view || map.height < 11 || map.width < 60
    }

    fn map_area(&self) -> Rect {
        Self::split_explore(self.body_area(self.viewport)).0
    }

    pub(crate) fn tiles(&self, area: Rect) -> Rc<Vec<Tile>> {
        if let Some((cached_area, tiles)) = self.tiles_cache.borrow().as_ref()
            && *cached_area == area
        {
            return Rc::clone(tiles);
        }
        let Some(node) = self.current_node() else {
            return Rc::new(Vec::new());
        };
        let options = LayoutOptions {
            max_depth: self.view_depth,
            padding: 1.0,
            padding_outer: 1.0,
            min_tile: 2.0,
            max_children: 72,
            header: 6.0,
            header_inner: 4.0,
            // A compact mosaic still has room for a named parent band at
            // twelve cells. The wider view keeps more space for labels.
            min_header_width: if area.width >= 105 { 20.0 } else { 12.0 },
        };
        let tiles = Rc::new(layout_filtered(
            node,
            &self.current,
            MapRect::new(
                0.0,
                0.0,
                f32::from(area.width),
                f32::from(area.height) * 2.0,
            ),
            self.options.metric,
            &options,
            self.matches.as_ref(),
        ));
        *self.tiles_cache.borrow_mut() = Some((area, Rc::clone(&tiles)));
        tiles
    }

    pub(crate) fn tile_rect(tile: &Tile, area: Rect) -> Rect {
        let x = area.x + tile.rect.x.max(0.0) as u16;
        let y = area.y + (tile.rect.y.max(0.0) / 2.0) as u16;
        let width = tile.rect.w.max(0.0) as u16;
        let height = (tile.rect.h.max(0.0) / 2.0).ceil() as u16;
        Rect::new(x, y, width, height).intersection(area)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let selected = self.selected.clone();
        let current = self.current.clone();
        let root = self.root.clone();
        let screen = self.screen;
        let depth = self.view_depth;
        let list_view = self.list_view;
        let insight_index = self.insight_index;
        let volume_highlight = self.volume_highlight;
        self.handle_key_inner(key);
        if self.selected != selected
            || self.current != current
            || self.root != root
            || self.screen != screen
            || self.view_depth != depth
            || self.list_view != list_view
            || self.insight_index != insight_index
            || self.volume_highlight != volume_highlight
        {
            self.reset_focused_marquee();
        }
    }

    fn handle_key_inner(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
        {
            if self.screen == Screen::Running {
                self.cancel_removal();
            } else {
                self.quit = true;
            }
            return;
        }
        self.notice = None;
        match self.screen {
            Screen::Explore => self.on_explore_key(key),
            Screen::Review => self.on_review_key(key),
            Screen::Confirm => self.on_confirm_key(key),
            Screen::Running => {
                if key.code == KeyCode::Esc {
                    self.cancel_removal();
                }
            }
            Screen::Done => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                    self.screen = Screen::Explore;
                }
            }
            Screen::Help => {
                self.screen = Screen::Explore;
            }
            Screen::Insights => self.on_insights_key(key),
            Screen::SavePrompt => self.on_save_key(key),
            Screen::Volumes => self.on_volumes_key(key),
        }
    }

    fn on_explore_key(&mut self, key: KeyEvent) {
        if self.search_open {
            match key.code {
                KeyCode::Esc => {
                    self.search_open = false;
                    self.search.clear();
                    self.matches = None;
                    self.invalidate_view_cache();
                }
                KeyCode::Enter => {
                    self.search_open = false;
                    self.apply_search();
                }
                KeyCode::Backspace => {
                    self.search.pop();
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.search.push(ch);
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.screen = Screen::Help,
            KeyCode::Char('/') => {
                self.search_open = true;
                self.search.clear();
                self.matches = None;
                self.invalidate_view_cache();
            }
            KeyCode::Char('r') => self.start_scan(),
            KeyCode::Char('[' | '-') => {
                self.view_depth = self.view_depth.saturating_sub(1).max(1);
                self.invalidate_view_cache();
            }
            KeyCode::Char(']' | '+') => {
                self.view_depth = (self.view_depth + 1).min(6);
                self.invalidate_view_cache();
            }
            KeyCode::Esc if self.scan.is_some() => {
                if let Some(scan) = self.scan.take() {
                    scan.cancel();
                    self.progress = scan.progress.snapshot();
                    self.scan_elapsed =
                        self.scan_started.take().map(|start| start.elapsed());
                    self.notice = Some("Scan stopped".to_string());
                }
            }
            KeyCode::Esc if self.matches.is_some() => {
                self.matches = None;
                self.search.clear();
                self.invalidate_view_cache();
            }
            KeyCode::Esc | KeyCode::Backspace => self.ascend(),
            KeyCode::Enter => self.descend(),
            KeyCode::Left | KeyCode::Char('h') => {
                self.move_direction(Direction::Left);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.move_direction(Direction::Right);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_direction(Direction::Down);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_direction(Direction::Up);
            }
            KeyCode::Tab => self.move_selection(1),
            KeyCode::BackTab => self.move_selection(-1),
            KeyCode::Char(' ' | 'x') => self.toggle_mark(),
            KeyCode::Char('c') if !self.marks.is_empty() => {
                self.scroll = 0;
                self.screen = Screen::Review;
            }
            KeyCode::Char('c') => {
                self.notice = Some("Mark a path with Space first".to_string());
            }
            KeyCode::Char('w') => {
                self.insight_index = 0;
                self.screen = Screen::Insights;
            }
            KeyCode::Char('V') => self.open_volumes(),
            KeyCode::Char('g') => self.go_to_disk(),
            KeyCode::Char('v') => {
                let map = self.map_area();
                if map.width < 60 || map.height < 11 {
                    self.notice = Some(
                        "This size uses the list; widen for the map"
                            .to_string(),
                    );
                } else {
                    self.list_view = !self.list_view;
                    self.keep_list_selection_visible();
                }
            }
            KeyCode::Char('t') => self.cycle_mode(),
            KeyCode::Char('d') => {
                self.options.apparent_size = !self.options.apparent_size;
                self.switch_measure();
            }
            KeyCode::Char('i') => {
                self.options.include_hidden = !self.options.include_hidden;
                self.toggle_hidden();
            }
            _ => {}
        }
        self.keep_selection_visible();
    }

    fn on_review_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.screen = Screen::Explore,
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::Char('m') => {
                if self.trash_backend.is_available() {
                    self.removal_mode = RemovalMode::Trash;
                } else {
                    self.notice = Some("Trash unavailable here".to_string());
                }
            }
            KeyCode::Char('p') => self.removal_mode = RemovalMode::Permanent,
            KeyCode::Char('!') => {
                self.marks.clear();
                self.screen = Screen::Explore;
            }
            KeyCode::Char('a') => self.handoff_to_stdout(),
            KeyCode::Char('s') => {
                if self.plan().is_empty() {
                    self.notice =
                        Some("No removable paths to save".to_string());
                } else {
                    self.save_path.clear();
                    self.screen = Screen::SavePrompt;
                }
            }
            KeyCode::Enter if !self.plan().is_empty() => {
                if self.removal_mode == RemovalMode::Permanent {
                    self.screen = Screen::Confirm;
                } else {
                    self.begin_removal();
                }
            }
            _ => {}
        }
    }

    fn on_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => self.screen = Screen::Review,
            KeyCode::Char('y') => self.begin_removal(),
            _ => {}
        }
    }

    fn on_insights_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('w') => {
                self.screen = Screen::Explore;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.insights.is_empty() {
                    self.insight_index =
                        (self.insight_index + 1) % self.insights.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.insights.is_empty() {
                    self.insight_index = self
                        .insight_index
                        .checked_sub(1)
                        .unwrap_or(self.insights.len() - 1);
                }
            }
            KeyCode::Enter => {
                if let Some(crumbs) = self
                    .insights
                    .get(self.insight_index)
                    .map(|candidate| candidate.crumbs.clone())
                {
                    self.current =
                        crumbs[..crumbs.len().saturating_sub(1)].to_vec();
                    self.selected = Some(crumbs);
                    self.matches = None;
                    self.screen = Screen::Explore;
                    self.invalidate_view_cache();
                }
            }
            _ => {}
        }
    }

    fn on_save_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.screen = Screen::Review,
            KeyCode::Backspace => {
                self.save_path.pop();
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.save_path.push(ch);
            }
            KeyCode::Enter => {
                let path = if self.save_path.is_empty() {
                    PathBuf::from("disktree-agent-prompt.txt")
                } else {
                    PathBuf::from(&self.save_path)
                };
                let prompt = self.agent_prompt();
                let outcome = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .and_then(|mut file| file.write_all(prompt.as_bytes()));
                self.notice = Some(match outcome {
                    Ok(()) => {
                        format!("Agent prompt saved to {}", path.display())
                    }
                    Err(error) => {
                        format!("Could not save {}: {error}", path.display())
                    }
                });
                self.screen = Screen::Review;
            }
            _ => {}
        }
    }

    fn apply_search(&mut self) {
        let Some(node) = self.current_node() else {
            return;
        };
        self.matches = filter(node, &self.current, &self.search);
        self.invalidate_view_cache();
        self.selected = self.visible_children().get(0).map(|index| {
            let mut crumbs = self.current.clone();
            crumbs.push(index);
            crumbs
        });
        self.list_scroll = 0;
        self.keep_selection_visible();
        if self.selected.is_none() {
            self.notice = Some(format!("No matches for {}", self.search));
        }
    }

    pub(crate) fn visible_children(&self) -> VisibleChildren {
        let Some(node) = self.current_node() else {
            return VisibleChildren::All(0);
        };
        let Some(matches) = self.matches.as_ref() else {
            return VisibleChildren::All(node.children.len());
        };
        if let Some(indices) = self.children_cache.borrow().as_ref() {
            return VisibleChildren::Filtered(Rc::clone(indices));
        }
        // The core filter has already recorded every kept ancestor. Read
        // its direct children instead of walking a huge directory again.
        let base_len = self.current.len();
        let mut indices: Vec<usize> = matches
            .keep
            .keys()
            .filter_map(|crumbs| {
                (crumbs.len() == base_len + 1
                    && crumbs.starts_with(&self.current))
                .then_some(crumbs[base_len])
            })
            .collect();
        indices.sort_unstable();
        let indices = Rc::new(indices);
        *self.children_cache.borrow_mut() = Some(Rc::clone(&indices));
        VisibleChildren::Filtered(indices)
    }

    fn visible_map_nodes(&self, area: Rect) -> Vec<(Vec<usize>, Rect)> {
        self.tiles(area)
            .iter()
            .filter_map(|tile| {
                let TileKind::Node { crumbs } = &tile.kind else {
                    return None;
                };
                let rect = Self::tile_rect(tile, area);
                (rect.width > 0 && rect.height > 0)
                    .then(|| (crumbs.clone(), rect))
            })
            .collect()
    }

    fn keep_list_selection_visible(&mut self) {
        let map = self.map_area();
        if !self.is_list(map) {
            return;
        }
        let visible = self.visible_children();
        let capacity = usize::from(map.height.saturating_sub(1)).max(1);
        self.list_scroll =
            self.list_scroll.min(visible.len().saturating_sub(capacity));
        let Some(position) = self.selected.as_ref().and_then(|crumbs| {
            crumbs
                .get(self.current.len())
                .and_then(|index| visible.position(*index))
        }) else {
            return;
        };
        if position < self.list_scroll {
            self.list_scroll = position;
        } else if position >= self.list_scroll + capacity {
            self.list_scroll = position + 1 - capacity;
        }
    }

    fn keep_selection_visible(&mut self) {
        let map = self.map_area();
        if self.is_list(map) {
            let visible = self.visible_children();
            let selected = self.selected.as_ref().and_then(|crumbs| {
                let index = *crumbs.get(self.current.len())?;
                visible.position(index).map(|_| {
                    let mut direct = self.current.clone();
                    direct.push(index);
                    direct
                })
            });
            self.selected = selected.or_else(|| {
                visible.get(0).map(|index| {
                    let mut direct = self.current.clone();
                    direct.push(index);
                    direct
                })
            });
            self.keep_list_selection_visible();
            return;
        }
        let visible = self.visible_map_nodes(map);
        if !visible
            .iter()
            .any(|(crumbs, _)| self.selected.as_ref() == Some(crumbs))
        {
            self.selected = visible.first().map(|(crumbs, _)| crumbs.clone());
        }
    }

    fn move_selection(&mut self, step: isize) {
        let map = self.map_area();
        if !self.is_list(map) {
            let visible = self.visible_map_nodes(map);
            if visible.is_empty() {
                return;
            }
            let current = self.selected.as_ref().and_then(|selected| {
                visible.iter().position(|(crumbs, _)| crumbs == selected)
            });
            let depth = current
                .map_or(self.current.len() + 1, |index| visible[index].0.len());
            let peers: Vec<&Vec<usize>> = visible
                .iter()
                .filter(|(crumbs, _)| crumbs.len() == depth)
                .map(|(crumbs, _)| crumbs)
                .collect();
            if peers.is_empty() {
                return;
            }
            let position = current
                .and_then(|index| {
                    peers.iter().position(|crumbs| **crumbs == visible[index].0)
                })
                .map_or(0, |position| {
                    (position.cast_signed() + step)
                        .rem_euclid(peers.len().cast_signed())
                        as usize
                });
            self.selected = Some(peers[position].clone());
            return;
        }
        let visible = self.visible_children();
        if visible.is_empty() {
            return;
        }
        let current = self.selected.as_ref().and_then(|crumbs| {
            crumbs
                .starts_with(&self.current)
                .then(|| crumbs.get(self.current.len()).copied())
                .flatten()
        });
        let position = current
            .and_then(|index| visible.position(index))
            .map_or(0, |position| {
                (position.cast_signed() + step)
                    .rem_euclid(visible.len().cast_signed())
                    as usize
            });
        let mut crumbs = self.current.clone();
        crumbs.push(visible.get(position).expect("visible position in range"));
        self.selected = Some(crumbs);
        self.keep_list_selection_visible();
    }

    fn move_direction(&mut self, direction: Direction) {
        let map = self.map_area();
        if self.is_list(map) {
            self.move_selection(
                if matches!(direction, Direction::Left | Direction::Up) {
                    -1
                } else {
                    1
                },
            );
            return;
        }
        let visible = self.visible_map_nodes(map);
        let Some((from_crumbs, from_rect)) =
            self.selected.as_ref().and_then(|selected| {
                visible.iter().find(|(crumbs, _)| crumbs == selected)
            })
        else {
            self.selected = visible.first().map(|(crumbs, _)| crumbs.clone());
            return;
        };
        let mut best: Option<(f32, Vec<usize>)> = None;
        for (crumbs, rect) in &visible {
            if crumbs.len() != from_crumbs.len() || crumbs == from_crumbs {
                continue;
            }
            let Some(gap) = direction.gap(*from_rect, *rect) else {
                continue;
            };
            let score = direction.offset(*from_rect, *rect).mul_add(2.5, gap);
            if best
                .as_ref()
                .is_none_or(|(score_best, _)| score < *score_best)
            {
                best = Some((score, crumbs.clone()));
            }
        }
        if let Some((_, crumbs)) = best {
            self.selected = Some(crumbs);
        } else if matches!(direction, Direction::Left | Direction::Up)
            && from_crumbs.len() > self.current.len() + 1
        {
            let parent = &from_crumbs[..from_crumbs.len() - 1];
            if visible.iter().any(|(crumbs, _)| crumbs == parent) {
                self.selected = Some(parent.to_vec());
            }
        }
    }

    fn descend(&mut self) {
        if self.tree.is_none() {
            self.notice = Some("Wait for the scan to finish".to_string());
            return;
        }
        let Some(crumbs) = self.selected.clone() else {
            self.notice = Some("Select a directory to open".to_string());
            return;
        };
        let Some(node) =
            self.tree.as_ref().and_then(|tree| tree.resolve(&crumbs))
        else {
            self.notice =
                Some("Selection is no longer in the scan".to_string());
            return;
        };
        if !node.is_dir() {
            self.notice = Some("File selected; Space marks it".to_string());
            return;
        }
        if node.children.is_empty() {
            self.notice = Some("Directory is empty".to_string());
            return;
        }
        self.current = crumbs;
        self.selected = first_child(node, &self.current);
        self.matches = None;
        self.invalidate_view_cache();
        self.search.clear();
        self.scroll = 0;
        self.list_scroll = 0;
        self.keep_selection_visible();
    }

    fn ascend(&mut self) {
        if self.current.is_empty() {
            if volume_root_for(&self.root)
                .is_some_and(|volume| same_path(&volume, &self.root))
            {
                self.notice = Some(
                    "At the volume root; press V to choose a volume"
                        .to_string(),
                );
            } else if let Some(parent) =
                self.root.parent().map(Path::to_path_buf)
            {
                self.set_root(&parent);
            }
            return;
        }
        let old = self.current.clone();
        self.current.pop();
        self.selected = Some(old);
        self.matches = None;
        self.invalidate_view_cache();
        self.search.clear();
        self.scroll = 0;
        self.list_scroll = 0;
        self.keep_selection_visible();
    }

    fn set_root(&mut self, root: &Path) {
        let Ok(root) = canonicalize_path(root) else {
            self.notice = Some(format!("Cannot access {}", root.display()));
            return;
        };
        if !root.is_dir() {
            self.notice =
                Some(format!("{} is not a directory", root.display()));
            return;
        }
        self.screen = Screen::Explore;
        if same_path(&root, &self.root) {
            self.current.clear();
            self.selected =
                self.tree.as_ref().and_then(|tree| first_child(tree, &[]));
            self.invalidate_view_cache();
            return;
        }
        let before = self.marks.len();
        self.marks.retain(|mark| mark.path.starts_with(&root));
        self.root = root;
        self.full_hidden_loaded = false;
        self.space = space_info(&self.root).ok();
        self.device = device_for(&self.root);
        self.start_scan();
        if self.marks.len() < before {
            self.notice =
                Some("Marks outside this scan were cleared".to_string());
        }
    }

    fn go_to_disk(&mut self) {
        if let Some(root) = volume_root_for(&self.root) {
            self.set_root(&root);
        } else {
            self.notice = Some("Cannot find this volume's root".to_string());
        }
    }

    fn open_volumes(&mut self) {
        let root = self.root.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(volume_choices(&root));
        });
        self.volumes.clear();
        self.volume_highlight = 0;
        self.volumes_loading = true;
        self.volume_scan = Some(receiver);
        self.screen = Screen::Volumes;
    }

    fn on_volumes_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.volume_scan = None;
                self.volumes_loading = false;
                self.screen = Screen::Explore;
            }
            KeyCode::Up | KeyCode::Char('k') if !self.volumes.is_empty() => {
                self.volume_highlight = self
                    .volume_highlight
                    .checked_sub(1)
                    .unwrap_or(self.volumes.len() - 1);
            }
            KeyCode::Down | KeyCode::Char('j') if !self.volumes.is_empty() => {
                self.volume_highlight =
                    (self.volume_highlight + 1) % self.volumes.len();
            }
            KeyCode::Enter if self.volumes_loading => {
                self.notice = Some("Still looking for volumes".to_string());
            }
            KeyCode::Enter => {
                let point = self
                    .volumes
                    .get(self.volume_highlight)
                    .map(|volume| volume.point.clone());
                if let Some(point) = point {
                    self.volume_scan = None;
                    self.volumes.clear();
                    self.set_root(&point);
                } else {
                    self.notice = Some("No readable volumes found".to_string());
                }
            }
            _ => {}
        }
    }

    fn toggle_mark(&mut self) {
        let Some(node) = self.selected_node() else {
            return;
        };
        let Some(path) = self.selected_path() else {
            return;
        };
        if path == self.root {
            self.notice = Some("The scanned root cannot be marked".to_string());
            return;
        }
        if let Some(index) =
            self.marks.iter().position(|item| item.path == path)
        {
            self.marks.remove(index);
            self.notice = Some(format!("Unmarked {}", path.display()));
            return;
        }
        if let Some(ancestor) = self.marked_ancestor(&path) {
            self.notice =
                Some(format!("Already covered by {}", ancestor.display()));
            return;
        }
        let target = Target {
            bytes: node.bytes,
            is_dir: node.is_dir(),
            hidden: node.hidden,
            path: path.clone(),
        };
        self.marks.retain(|item| !item.path.starts_with(&path));
        self.marks.push(target);
        self.notice = Some(format!("Marked {}", path.display()));
    }

    fn cycle_mode(&mut self) {
        let directory = self.current_path();
        let selected = self.selected_path();
        self.mode = self.mode.next();
        let metric = self.mode.metric();
        if self.options.metric == metric {
            return;
        }
        self.options.metric = metric;
        if self.scan.is_some() {
            // The finish pass will sort the result to the current mode.
            return;
        }
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        // Reorder the existing tree instead of walking a large disk again.
        // Crumbs change when sibling order changes; restore both places by
        // their absolute paths, just as marks are kept by path.
        aggregate_view(tree, self.options.metric, self.options.dedup_hardlinks);
        self.refresh_present_marks();
        self.refresh_view_after_change(directory, selected);
    }

    fn switch_measure(&mut self) {
        if self.scan.is_some() {
            // The result contains both measurements; choose on completion.
            return;
        }
        let directory = self.current_path();
        let selected = self.selected_path();
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        if !self.options.include_hidden && self.full_hidden_loaded {
            restore_hidden(tree, &self.root, &mut self.hidden_stash);
        }
        switch_measure(tree, self.options.metric, self.options.dedup_hardlinks);
        for target in &mut self.marks {
            if let Some(node) = node_at_path(&self.root, tree, &target.path) {
                target.bytes = node.bytes;
            }
        }
        if !self.options.include_hidden && self.full_hidden_loaded {
            hide_hidden(tree, &self.root, &mut self.hidden_stash);
            aggregate_view(
                tree,
                self.options.metric,
                self.options.dedup_hardlinks,
            );
            disktree_core::classify::classify(tree);
        }
        self.refresh_view_after_change(directory, selected);
    }

    fn toggle_hidden(&mut self) {
        if self.scan.is_some() {
            if self.options.include_hidden && !self.scan_hidden {
                self.start_scan();
            }
            return;
        }
        if self.options.include_hidden && !self.full_hidden_loaded {
            self.start_scan();
            return;
        }
        let directory = self.current_path();
        let selected = self.selected_path();
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        if self.options.include_hidden {
            restore_hidden(tree, &self.root, &mut self.hidden_stash);
        } else {
            hide_hidden(tree, &self.root, &mut self.hidden_stash);
        }
        aggregate_view(tree, self.options.metric, self.options.dedup_hardlinks);
        disktree_core::classify::classify(tree);
        self.refresh_present_marks();
        self.refresh_view_after_change(directory, selected);
    }

    fn refresh_present_marks(&mut self) {
        let Some(tree) = self.tree.as_ref() else {
            return;
        };
        for target in &mut self.marks {
            if let Some(node) = node_at_path(&self.root, tree, &target.path) {
                target.bytes = node.bytes;
            }
        }
    }

    fn refresh_view_after_change(
        &mut self,
        directory: Option<PathBuf>,
        selected: Option<PathBuf>,
    ) {
        let Some(tree) = self.tree.as_ref() else {
            return;
        };
        self.current = directory
            .and_then(|path| crumbs_for_path(&self.root, tree, &path))
            .unwrap_or_default();
        self.selected = selected
            .and_then(|path| crumbs_for_path(&self.root, tree, &path))
            .or_else(|| {
                tree.resolve(&self.current)
                    .and_then(|node| first_child(node, &self.current))
            });
        self.insights = worth_a_look(tree, now_seconds(), INSIGHT_LIMIT);
        self.matches = None;
        self.search.clear();
        self.search_open = false;
        self.scroll = 0;
        self.list_scroll = 0;
        self.invalidate_view_cache();
        self.keep_selection_visible();
    }

    fn begin_removal(&mut self) {
        let plan = self.plan();
        if plan.is_empty() {
            self.notice = Some("No removable paths in the plan".to_string());
            self.screen = Screen::Review;
            return;
        }
        self.baseline_space = self.space;
        self.run_cancelled = false;
        self.run_summary = RunSummary {
            total: plan.targets.len(),
            ..RunSummary::default()
        };
        self.run_log.clear();
        self.run = Some(disktree_core::removal::spawn(plan, self.removal_mode));
        self.screen = Screen::Running;
    }

    fn cancel_removal(&mut self) {
        if let Some(run) = &self.run {
            run.cancel();
            self.run_cancelled = true;
            self.notice = Some("Stopping after the current path".to_string());
        }
    }

    pub fn agent_prompt(&self) -> String {
        let plan = self.plan();
        disktree_core::export::agent_prompt(
            &plan.targets,
            &self.root,
            self.space,
        )
    }

    fn handoff_to_stdout(&mut self) {
        if self.plan().is_empty() {
            self.notice = Some("No removable paths to hand off".to_string());
            return;
        }
        self.stdout_prompt = Some(self.agent_prompt());
        self.quit = true;
    }
}

fn first_child(node: &Node, base: &[usize]) -> Option<Vec<usize>> {
    (!node.children.is_empty()).then(|| {
        let mut crumbs = base.to_vec();
        crumbs.push(0);
        crumbs
    })
}

fn node_at_path<'a>(
    root_path: &Path,
    root: &'a Node,
    path: &Path,
) -> Option<&'a Node> {
    let relative = path.strip_prefix(root_path).ok()?;
    let mut node = root;
    for component in relative.components() {
        let name = component.as_os_str().to_string_lossy();
        node = node
            .children
            .iter()
            .find(|child| child.name.as_ref() == name)?;
    }
    Some(node)
}

/// Keep hidden subtrees intact while presenting a visible-only tree. Moving
/// them out costs one traversal and no extra copy of a large scan.
fn hide_hidden(
    node: &mut Node,
    path: &Path,
    stash: &mut Vec<(PathBuf, Vec<Node>)>,
) {
    let hidden: Vec<Node> =
        node.children.extract_if(.., |child| child.hidden).collect();
    if !hidden.is_empty() {
        stash.push((path.to_path_buf(), hidden));
    }
    for child in &mut node.children {
        if child.is_dir() {
            hide_hidden(child, &path.join(child.name.as_ref()), stash);
        }
    }
}

fn restore_hidden(
    tree: &mut Node,
    root: &Path,
    stash: &mut Vec<(PathBuf, Vec<Node>)>,
) {
    for (path, children) in stash.drain(..) {
        let parent = node_at_path_mut(root, tree, &path)
            .expect("a hidden entry's parent remains visible");
        parent.children.extend(children);
    }
}

fn node_at_path_mut<'a>(
    root_path: &Path,
    root: &'a mut Node,
    path: &Path,
) -> Option<&'a mut Node> {
    let relative = path.strip_prefix(root_path).ok()?;
    let mut node = root;
    for component in relative.components() {
        let name = component.as_os_str().to_string_lossy();
        let index = node
            .children
            .iter()
            .position(|child| child.name.as_ref() == name)?;
        node = node.children.get_mut(index)?;
    }
    Some(node)
}

fn crumbs_for_path(
    root_path: &Path,
    root: &Node,
    path: &Path,
) -> Option<Vec<usize>> {
    let relative = path.strip_prefix(root_path).ok()?;
    let mut node = root;
    let mut crumbs = Vec::new();
    for component in relative.components() {
        let name = component.as_os_str().to_string_lossy();
        let index = node
            .children
            .iter()
            .position(|child| child.name.as_ref() == name)?;
        crumbs.push(index);
        node = node.child(index)?;
    }
    Some(crumbs)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || canonicalize_path(left)
            .ok()
            .zip(canonicalize_path(right).ok())
            .is_some_and(|(left, right)| left == right)
}

pub fn canonicalize_path(path: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        dunce::canonicalize(path)
    }
    #[cfg(not(windows))]
    {
        path.canonicalize()
    }
}

fn volume_choices(root: &Path) -> Vec<Volume> {
    let mut found = volumes();
    if let Some(current) = volume_root_for(root)
        && !found
            .iter()
            .any(|volume| same_path(&volume.point, &current))
    {
        found.push(Volume {
            point: current.clone(),
            device: device_for(&current),
            space: space_info(&current).ok(),
        });
    }
    found.retain(|volume| !same_path(&volume.point, root));
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use disktree_core::classify::Reclaim;
    use disktree_core::insights::Finding;
    use disktree_core::scan::scan;
    use disktree_core::tree::Metric;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn fixture_dir() -> tempfile::TempDir {
        // macOS keeps its default temporary directory under `/private`,
        // which the removal guard correctly treats as system-owned.
        #[cfg(target_os = "macos")]
        {
            let home = std::env::home_dir().expect("home directory");
            tempfile::Builder::new()
                .prefix("disktree-tui-")
                .tempdir_in(home)
                .expect("tempdir in home")
        }
        #[cfg(not(target_os = "macos"))]
        {
            tempfile::tempdir().expect("tempdir")
        }
    }

    #[test]
    fn marks_use_bytes_and_absorb_children() {
        let temp = fixture_dir();
        let root = temp.path();
        std::fs::create_dir(root.join("cache")).expect("mkdir");
        std::fs::write(root.join("cache/file"), vec![b'x'; 1000])
            .expect("write");
        let options = ScanOptions {
            apparent_size: true,
            metric: Metric::Files,
            ..ScanOptions::default()
        };
        let tree = scan(root, options.clone()).expect("scan");
        let mut app = App::new(root.to_path_buf(), options);
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.selected = Some(vec![0, 0]);
        app.toggle_mark();
        app.selected = Some(vec![0]);
        app.toggle_mark();
        assert_eq!(app.marks.len(), 1);
        assert_eq!(app.marks[0].bytes, 1000);
        assert_eq!(app.plan().bytes(), 1000);
    }

    #[test]
    fn metric_switch_reorders_without_rescan_and_keeps_paths() {
        let temp = fixture_dir();
        let root = temp.path();
        std::fs::create_dir(root.join("large")).expect("large dir");
        std::fs::create_dir(root.join("many")).expect("many dir");
        std::fs::write(root.join("large/blob"), vec![b'x'; 10_000])
            .expect("large file");
        for index in 0..3 {
            std::fs::write(root.join("many").join(index.to_string()), b"x")
                .expect("small file");
        }
        let options = ScanOptions {
            apparent_size: true,
            ..ScanOptions::default()
        };
        let tree = scan(root, options.clone()).expect("scan");
        let mut app = App::new(root.to_path_buf(), options);
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        let tree = app.tree.as_ref().expect("tree");
        app.current =
            crumbs_for_path(root, tree, &root.join("large")).expect("large");
        app.selected = crumbs_for_path(root, tree, &root.join("large/blob"));
        app.toggle_mark();
        let marked_bytes = app.marked_bytes();

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(app.options.metric, Metric::Files);
        assert!(app.scan.is_none(), "metric switch should reuse the tree");
        assert_eq!(
            app.current_path().as_deref(),
            Some(root.join("large").as_path())
        );
        assert_eq!(
            app.selected_path().as_deref(),
            Some(root.join("large/blob").as_path())
        );
        assert_eq!(app.marked_bytes(), marked_bytes);
        assert_eq!(
            app.tree.as_ref().expect("tree").children[0].name.as_ref(),
            "many"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(app.mode, ViewMode::Age);
        assert_eq!(app.options.metric, Metric::Bytes);
        assert!(app.scan.is_none(), "age mode should reuse the tree");
        assert_eq!(
            app.tree.as_ref().expect("tree").children[0].name.as_ref(),
            "large"
        );
        assert_eq!(
            app.selected_path().as_deref(),
            Some(root.join("large/blob").as_path())
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(app.mode, ViewMode::Size);
        assert!(app.scan.is_none());
        assert_eq!(app.marked_bytes(), marked_bytes);
    }

    #[test]
    fn apparent_toggle_reuses_scan_and_updates_marked_bytes() {
        let temp = fixture_dir();
        let root = temp.path();
        let file = root.join("sparse");
        std::fs::File::create(&file)
            .expect("create")
            .set_len(1024 * 1024)
            .expect("sparse length");
        let tree = scan(root, ScanOptions::default()).expect("scan");
        let mut app = App::new(root.to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.selected =
            crumbs_for_path(root, app.tree.as_ref().expect("tree"), &file);
        app.toggle_mark();
        let allocated = app.marked_bytes();
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(app.scan.is_none());
        assert_eq!(app.marked_bytes(), 1024 * 1024);
        assert_eq!(app.selected_path().as_deref(), Some(file.as_path()));
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(app.scan.is_none());
        assert_eq!(app.marked_bytes(), allocated);
    }

    #[test]
    fn hidden_toggle_reuses_full_scan_and_restores_paths() {
        let temp = fixture_dir();
        let root = temp.path();
        std::fs::create_dir(root.join(".private")).expect("hidden dir");
        std::fs::write(root.join(".private/secret"), b"secret")
            .expect("hidden file");
        std::fs::write(root.join("visible"), b"visible").expect("visible file");
        let tree = scan(root, ScanOptions::default()).expect("scan");
        let full_bytes = tree.bytes;
        let mut app = App::new(root.to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.full_hidden_loaded = true;
        app.selected = crumbs_for_path(
            root,
            app.tree.as_ref().expect("tree"),
            &root.join(".private"),
        );
        app.toggle_mark();
        let marked_path = app.marks[0].path.clone();
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(app.scan.is_none());
        assert!(app.tree.as_ref().expect("tree").bytes < full_bytes);
        assert!(app.selected_path().is_some());
        assert!(!app.hidden_stash.is_empty());
        assert_eq!(app.marks[0].path, marked_path);
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(app.scan.is_none());
        assert_eq!(app.tree.as_ref().expect("tree").bytes, full_bytes);
        assert!(app.hidden_stash.is_empty());
        assert_eq!(app.marks[0].path, marked_path);
        assert!(
            app.tree
                .as_ref()
                .expect("tree")
                .child_named(".private")
                .is_some()
        );
    }

    #[test]
    fn first_hidden_enable_after_excluding_it_loads_missing_entries() {
        let temp = fixture_dir();
        std::fs::write(temp.path().join(".private"), b"secret")
            .expect("hidden file");
        let options = ScanOptions {
            include_hidden: false,
            ..ScanOptions::default()
        };
        let tree = scan(temp.path(), options.clone()).expect("scan");
        let mut app = App::new(temp.path().to_path_buf(), options);
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(app.scan.is_some());
        assert!(app.scan_hidden);
    }

    #[test]
    fn agent_handoff_contains_only_accepted_marks() {
        let temp = fixture_dir();
        let root = temp.path().join("scan");
        std::fs::create_dir(&root).expect("mkdir");
        std::fs::write(root.join("keep?"), b"data").expect("write");
        let mut app = App::new(root.clone(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.marks = vec![
            Target {
                path: root.join("keep?"),
                bytes: 4,
                is_dir: false,
                hidden: false,
            },
            Target {
                path: temp.path().join("outside"),
                bytes: 100,
                is_dir: false,
                hidden: false,
            },
        ];
        app.screen = Screen::Review;
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(app.quit);
        let prompt = app.stdout_prompt.expect("agent prompt");
        assert!(prompt.contains("keep?"));
        assert!(!prompt.contains("outside"));
    }

    #[test]
    fn filtering_keeps_selection_within_matching_children() {
        let temp = fixture_dir();
        std::fs::write(temp.path().join("alpha"), b"a").expect("write");
        std::fs::write(temp.path().join("beta"), b"b").expect("write");
        let tree = scan(temp.path(), ScanOptions::default()).expect("scan");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.search = "alpha".to_string();
        app.apply_search();
        let original = app.selected.clone();
        assert!(original.is_some());
        let first = app.visible_children();
        let second = app.visible_children();
        assert!(matches!(
            (&first, &second),
            (VisibleChildren::Filtered(left), VisibleChildren::Filtered(right))
                if Rc::ptr_eq(left, right)
        ));
        app.move_selection(1);
        assert_eq!(app.selected, original);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.visible_children(), VisibleChildren::All(2)));
    }

    #[test]
    fn tile_layout_is_reused_until_a_layout_input_changes() {
        let temp = fixture_dir();
        std::fs::create_dir(temp.path().join("Projects")).expect("mkdir");
        std::fs::write(temp.path().join("Projects/file"), b"data")
            .expect("write");
        std::fs::write(temp.path().join("Downloads"), b"more").expect("write");
        let tree = scan(temp.path(), ScanOptions::default()).expect("scan");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.set_viewport(120, 35);
        let map = app.map_area();
        let first = app.tiles(map);
        assert!(Rc::ptr_eq(&first, &app.tiles(map)));
        let selected = app.selected.clone();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_ne!(selected, app.selected);
        assert!(Rc::ptr_eq(&first, &app.tiles(map)));
        app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        let changed = app.tiles(map);
        assert!(!Rc::ptr_eq(&first, &changed));
        assert!(Rc::ptr_eq(&changed, &app.tiles(map)));
    }

    #[test]
    fn filtered_rows_include_ancestors_of_nested_name_matches() {
        let temp = fixture_dir();
        let nested = temp.path().join("Projects/Mozilla/profile");
        std::fs::create_dir_all(nested.parent().expect("parent"))
            .expect("mkdir");
        std::fs::write(nested, b"data").expect("write");
        std::fs::write(temp.path().join("unrelated"), b"data").expect("write");
        let tree = scan(temp.path(), ScanOptions::default()).expect("scan");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.search = "Mozilla".to_string();
        app.apply_search();
        let visible = app.visible_children();
        assert_eq!(visible.len(), 1);
        let index = visible.get(0).expect("matching ancestor");
        assert_eq!(
            app.current_node().expect("root").children[index]
                .name
                .as_ref(),
            "Projects"
        );
    }

    #[test]
    fn depth_keys_follow_the_gui_range() {
        let temp = fixture_dir();
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        for _ in 0..8 {
            app.handle_key(KeyEvent::new(
                KeyCode::Char('['),
                KeyModifiers::NONE,
            ));
        }
        assert_eq!(app.view_depth, 1);
        for _ in 0..8 {
            app.handle_key(KeyEvent::new(
                KeyCode::Char(']'),
                KeyModifiers::NONE,
            ));
        }
        assert_eq!(app.view_depth, 6);
        app.handle_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));
        assert_eq!(app.view_depth, 5);
        app.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
        assert_eq!(app.view_depth, 6);
    }

    #[test]
    fn mosaic_navigation_only_selects_drawn_tiles() {
        let temp = fixture_dir();
        for index in 0..100 {
            std::fs::write(
                temp.path().join(format!("item_{index:03}")),
                vec![b'x'; 4096],
            )
            .expect("write");
        }
        let tree = scan(temp.path(), ScanOptions::default()).expect("scan");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.selected = Some(vec![0]);
        app.set_viewport(80, 24);
        let drawn = app.visible_map_nodes(app.map_area());
        assert!(!drawn.is_empty());
        assert!(drawn.len() < 100);

        for _ in 0..100 {
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert!(
                drawn.iter().any(|(crumbs, _)| {
                    app.selected.as_ref() == Some(crumbs)
                })
            );
        }

        let (from, from_rect) = drawn
            .iter()
            .find(|(crumbs, rect)| {
                drawn.iter().any(|(other, target)| {
                    other != crumbs
                        && Direction::Right.gap(*rect, *target).is_some()
                })
            })
            .expect("horizontal neighbours");
        app.selected = Some(from.clone());
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let selected = app.selected.as_ref().expect("selected");
        let target = drawn
            .iter()
            .find(|(crumbs, _)| crumbs == selected)
            .expect("drawn target");
        assert!(Direction::Right.gap(*from_rect, target.1).is_some());
        assert!(app.current.is_empty());
    }

    #[test]
    fn a_long_list_scrolls_with_the_selection() {
        let temp = fixture_dir();
        for index in 0..40 {
            std::fs::write(
                temp.path().join(format!("item_{index:03}")),
                vec![b'x'; 4096],
            )
            .expect("write");
        }
        let tree = scan(temp.path(), ScanOptions::default()).expect("scan");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.selected = Some(vec![0]);
        app.set_viewport(45, 13);
        for _ in 0..20 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert!(app.list_scroll > 0);
        let selected = app.selected_node().expect("selected").name.to_string();
        let mut terminal =
            Terminal::new(TestBackend::new(45, 13)).expect("terminal");
        terminal
            .draw(|frame| {
                crate::render::draw(
                    frame,
                    &app,
                    &crate::theme::Palette::dark(),
                );
            })
            .expect("draw");
        let map = app.map_area();
        let mut rendered = String::new();
        for y in map.y + 1..map.bottom() {
            for x in map.x..map.right() {
                rendered.push_str(
                    terminal
                        .backend()
                        .buffer()
                        .cell((x, y))
                        .expect("cell")
                        .symbol(),
                );
            }
        }
        assert!(rendered.contains(&selected));
    }

    #[test]
    fn enter_opens_directories_and_explains_a_file_selection() {
        let temp = fixture_dir();
        let root = temp.path().join("scan");
        let nested = root.join("Projects/app");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(nested.join("main.rs"), b"code").expect("write");
        let tree = scan(&root, ScanOptions::default()).expect("scan");
        let mut app = App::new(root.clone(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.selected = first_child(&tree, &[]);
        app.tree = Some(tree);
        for name in ["Projects", "app"] {
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert_eq!(
                app.current_path().expect("current"),
                root.join(if name == "Projects" {
                    "Projects"
                } else {
                    "Projects/app"
                })
            );
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.notice.as_deref(),
            Some("File selected; Space marks it")
        );
        assert_eq!(app.current_path(), Some(nested));
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.current_path(), Some(root.join("Projects")));
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.current_path(), Some(root));
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.root, canonicalize_path(temp.path()).expect("parent"));
    }

    #[test]
    fn volume_picker_switches_roots_and_clears_outside_marks() {
        let temp = fixture_dir();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&first).expect("mkdir");
        std::fs::create_dir_all(&second).expect("mkdir");
        std::fs::write(first.join("marked"), b"data").expect("write");
        let mut app = App::new(first.clone(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.marks.push(Target {
            path: first.join("marked"),
            bytes: 4,
            is_dir: false,
            hidden: false,
        });
        app.screen = Screen::Volumes;
        app.volumes = [first, second.clone()]
            .into_iter()
            .map(|point| Volume {
                point,
                device: None,
                space: None,
            })
            .collect();
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.root, second);
        assert_eq!(app.screen, Screen::Explore);
        assert!(app.marks.is_empty());
        assert_eq!(
            app.notice.as_deref(),
            Some("Marks outside this scan were cleared")
        );
    }

    #[test]
    fn finished_scan_populates_insights_and_duration() {
        let temp = fixture_dir();
        let cache = temp.path().join(".cache");
        std::fs::create_dir(&cache).expect("mkdir");
        let blob = std::fs::File::create(cache.join("blob")).expect("file");
        blob.set_len(128 * 1024 * 1024).expect("size");
        let project = temp.path().join("Projects/app");
        std::fs::create_dir_all(project.join("target")).expect("mkdir");
        std::fs::write(project.join("Cargo.toml"), b"[package]\n")
            .expect("manifest");
        let build = std::fs::File::create(project.join("target/build.bin"))
            .expect("file");
        build.set_len(96 * 1024 * 1024).expect("size");
        let temporary = temp.path().join("Downloads/tmp");
        std::fs::create_dir_all(&temporary).expect("mkdir");
        let session =
            std::fs::File::create(temporary.join("session.bin")).expect("file");
        session.set_len(72 * 1024 * 1024).expect("size");
        let documents = temp.path().join("Documents");
        std::fs::create_dir(&documents).expect("mkdir");
        let personal =
            std::fs::File::create(documents.join("annual.pdf")).expect("file");
        personal.set_len(200 * 1024 * 1024).expect("size");
        let options = ScanOptions {
            apparent_size: true,
            ..ScanOptions::default()
        };
        let mut app = App::new(temp.path().to_path_buf(), options);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.scan.is_some() && Instant::now() < deadline {
            app.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.scan.is_none(), "scan finished");
        assert!(app.scan_error.is_none(), "scan succeeded");
        assert!(app.scan_elapsed.is_some(), "duration recorded");
        assert_eq!(app.progress.files, 5);
        let tree = app.tree.as_ref().expect("tree");
        let findings: Vec<_> = app
            .insights
            .iter()
            .map(|candidate| {
                let node = tree.resolve(&candidate.crumbs).expect("finding");
                (node.name.to_string(), candidate.finding.clone())
            })
            .collect();
        assert_eq!(
            findings,
            vec![
                (
                    ".cache".to_string(),
                    Finding::Reclaimable(Reclaim::Regenerable)
                ),
                (
                    "target".to_string(),
                    Finding::Reclaimable(Reclaim::BuildOutput)
                ),
                ("tmp".to_string(), Finding::Reclaimable(Reclaim::Temporary)),
            ]
        );
        let mut terminal =
            Terminal::new(TestBackend::new(120, 35)).expect("terminal");
        terminal
            .draw(|frame| {
                crate::render::draw(
                    frame,
                    &app,
                    &crate::theme::Palette::dark(),
                );
            })
            .expect("draw");
        let output: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(output.contains("WORTH A LOOK"));
        assert!(output.contains(".cache"));
        assert!(output.contains("target"));
        assert!(output.contains("tmp"));
    }

    #[test]
    fn review_confirmation_removes_only_the_marked_path() {
        let temp = fixture_dir();
        let selected = temp.path().join("selected");
        let neighbor = temp.path().join("neighbor");
        std::fs::write(&selected, vec![b'x'; 8192]).expect("write selected");
        std::fs::write(&neighbor, b"keep").expect("write neighbor");
        let tree = scan(temp.path(), ScanOptions::default()).expect("scan");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.selected = tree
            .children
            .iter()
            .position(|child| child.name.as_ref() == "selected")
            .map(|index| vec![index]);
        app.tree = Some(tree);
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(app.marks.len(), 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(app.screen, Screen::Review);
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.screen, Screen::Confirm);
        assert!(selected.exists());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.screen, Screen::Confirm);
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(app.screen, Screen::Running);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.screen == Screen::Running && Instant::now() < deadline {
            app.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.screen, Screen::Done);
        assert!(!selected.exists());
        assert!(neighbor.exists());
        assert_eq!(app.run_summary.failed, 0);
    }

    #[test]
    fn failed_removal_keeps_the_mark_for_review() {
        let temp = fixture_dir();
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.marks.push(Target {
            path: temp.path().join("missing"),
            bytes: 100,
            is_dir: false,
            hidden: false,
        });
        app.screen = Screen::Review;
        app.removal_mode = RemovalMode::Permanent;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(app.screen, Screen::Running);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.screen == Screen::Running && Instant::now() < deadline {
            app.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.screen, Screen::Done);
        assert_eq!(app.run_summary.failed, 1);
        assert_eq!(app.marks.len(), 1);
    }

    #[test]
    fn control_c_during_removal_requests_stop_without_quitting() {
        let temp = fixture_dir();
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.marks.push(Target {
            path: temp.path().join("missing"),
            bytes: 100,
            is_dir: false,
            hidden: false,
        });
        app.removal_mode = RemovalMode::Permanent;
        app.begin_removal();
        app.handle_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ));
        assert!(!app.quit);
        assert!(app.run_cancelled);
    }

    #[test]
    fn saving_agent_prompt_creates_without_overwriting() {
        let temp = fixture_dir();
        let selected = temp.path().join("selected");
        std::fs::write(&selected, b"content").expect("write selected");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.marks.push(Target {
            path: selected.clone(),
            bytes: 7,
            is_dir: false,
            hidden: false,
        });
        let output = temp.path().join("agent-prompt.txt");
        app.save_path = output.display().to_string();
        app.screen = Screen::SavePrompt;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let prompt = std::fs::read_to_string(&output).expect("saved prompt");
        assert!(prompt.contains(&selected.display().to_string()));
        app.screen = Screen::SavePrompt;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(std::fs::read_to_string(output).expect("prompt"), prompt);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|notice| notice.contains("Could not save"))
        );
    }
}
