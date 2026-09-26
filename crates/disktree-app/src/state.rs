//! Application state and every mutation the UI can perform.
//!
//! The screens in [`crate::views`] are pure functions of this state; all the
//! decisions — what is selected, what a mark means, what a key does, when to
//! re-scan — live here so they can be reasoned about in one place.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use disktree_core::filter::{Keep, Matches, filter};
use disktree_core::insights::{Candidate, worth_a_look};
use disktree_core::removal::{
    Plan, RemovalEvent, RemovalHandle, RemovalMode, Target, TrashBackend,
    detect_trash_backend, plan,
};
use disktree_core::scan::{Known, ScanHandle, ScanOptions, ScanSnapshot};
use disktree_core::space::{
    SpaceInfo, device_for, space_info, volume_root_for,
};
use disktree_core::tree::{Metric, Node, path_of};
use disktree_core::treemap::{
    LayoutOptions, Rect, Tile, TileKind, hit, layout_filtered,
};
use gpui_kit::{
    Context, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, NavigationDirection, Pixels, Point, Render, ScrollDelta,
    ScrollWheelEvent, Size, Window, px, size,
};
use gpui_omarchy::Status;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::git::GitState;

use crate::marks::{Marks, display_path, is_hidden};
use crate::treemap_view::{Mosaic, TileDeco};

/// What a tile's colour says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    /// The kind of data it is.
    #[default]
    Kind,
    /// How long since anything in it was written.
    Age,
}

/// A trail crumb's sibling menu, open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrumbMenu {
    /// Crumbs of the directory whose children are listed.
    pub parent: Vec<usize>,
    /// The listed child the crumb stands for.
    pub current: usize,
    /// The row the arrow keys are on, as an index into [`Disktree::siblings`].
    pub highlighted: usize,
}

/// One row of a sibling menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sibling {
    /// Index among the parent's children.
    pub index: usize,
    pub name: String,
    pub value: u64,
    pub category: disktree_core::classify::Category,
    pub is_dir: bool,
}

/// Rows a sibling menu lists; the rest are counted.
pub const SIBLING_ROWS: usize = 24;

/// One step of the trail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Crumb {
    /// A directory above the scanned root: going there scans only what is
    /// new at that level.
    Above(PathBuf),
    /// A directory in the tree, by crumbs from the scanned root.
    Tree(Vec<usize>),
}

/// The side panel's width in rem: default and limits. In rem, so interface
/// zoom scales it with everything it holds.
pub const PANEL_REMS: f32 = 23.0;
pub const PANEL_MIN_REMS: f32 = 17.0;
pub const PANEL_MAX_REMS: f32 = 44.0;

/// The panel width, in rem, for its left edge at `pointer_x` in a window
/// `viewport` pixels wide: between its limits, and never so wide that the
/// mosaic is squeezed below the panel's own minimum.
pub fn panel_width(pointer_x: f32, viewport: f32, rem: f32) -> f32 {
    let width = (viewport - pointer_x) / rem;
    let max =
        (viewport / rem - PANEL_MIN_REMS).clamp(PANEL_MIN_REMS, PANEL_MAX_REMS);
    width.clamp(PANEL_MIN_REMS, max)
}

/// How many "worth a look" findings the panel lists.
const INSIGHT_LIMIT: usize = 6;

/// Which screen the app is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    /// Walk the treemap and mark what should go.
    #[default]
    Explore,
    /// Review every marked path and choose how to remove them.
    Review,
    /// A removal is running.
    Running,
    /// The removal finished; show what happened.
    Done,
}

/// The treemap view transform: `screen = (base - origin) * scale`.
///
/// All viewport geometry stays in the base space of an unzoomed layout, so pan
/// and zoom never require a re-layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub scale: f32,
    pub origin_x: f32,
    pub origin_y: f32,
}

impl Default for View {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl View {
    pub const IDENTITY: Self = Self {
        scale: 1.0,
        origin_x: 0.0,
        origin_y: 0.0,
    };
    pub const MIN_SCALE: f32 = 1.0;
    /// Past this, zooming again descends into whatever is under the cursor
    /// instead of magnifying further: the continuous "keep zooming and you are
    /// inside" gesture, with a breadcrumb to walk back out.
    pub const MAX_SCALE: f32 = 5.0;

    pub fn project(&self, rect: Rect) -> Rect {
        Rect::new(
            (rect.x - self.origin_x) * self.scale,
            (rect.y - self.origin_y) * self.scale,
            rect.w * self.scale,
            rect.h * self.scale,
        )
    }

    /// Viewport point to base-space point.
    pub fn unproject(&self, x: f32, y: f32) -> (f32, f32) {
        (
            x / self.scale + self.origin_x,
            y / self.scale + self.origin_y,
        )
    }

    /// Zoom by `factor` toward `(x, y)` and no further than `ceiling`.
    ///
    /// The base point under `(x, y)` stays put, which is what makes a wheel
    /// zoom feel pointed at the tile rather than at the window.
    pub fn zoomed_at(&self, x: f32, y: f32, factor: f32, ceiling: f32) -> Self {
        let scale = (self.scale * factor).clamp(Self::MIN_SCALE, ceiling);
        let (base_x, base_y) = self.unproject(x, y);
        Self {
            scale,
            origin_x: base_x - x / scale,
            origin_y: base_y - y / scale,
        }
    }

    /// The scale at which `rect` exactly fits the viewport.
    pub fn fit_scale(rect: Rect, area: Size<Pixels>) -> f32 {
        let width = area.width.as_f32().max(1.0);
        let height = area.height.as_f32().max(1.0);
        if rect.w <= 0.0 || rect.h <= 0.0 {
            return Self::MIN_SCALE;
        }
        (width / rect.w).min(height / rect.h)
    }

    /// The region of the base layout the viewport currently shows.
    pub fn visible_base(&self, area: Size<Pixels>) -> Rect {
        let width = area.width.as_f32().max(1.0);
        let height = area.height.as_f32().max(1.0);
        Rect::new(
            self.origin_x,
            self.origin_y,
            width / self.scale,
            height / self.scale,
        )
    }

    /// Keep the viewport inside the layout: no empty margins, ever.
    pub fn clamped(self, area: Size<Pixels>) -> Self {
        let width = area.width.as_f32();
        let height = area.height.as_f32();
        let max_x = (width - width / self.scale).max(0.0);
        let max_y = (height - height / self.scale).max(0.0);
        Self {
            scale: self.scale,
            origin_x: self.origin_x.clamp(0.0, max_x),
            origin_y: self.origin_y.clamp(0.0, max_y),
        }
    }
}

/// A layout transition: the region the user is looking at, and where that same
/// region lands in the layout being switched to.
///
/// Every descending or ascending move is one region of the tree changing
/// address. Rather than moving a camera, each tile is drawn from where that
/// region *was* to where it *is*, so the tiles inside the directory that is
/// being entered grow into place and the ones being left slide out. That is
/// what makes "zoom in and descend" read as one motion: the newly visible
/// level is always larger than it was a moment ago, never smaller.
#[derive(Clone, Copy, Debug)]
pub struct LayoutTransition {
    /// The region in the old frame, in viewport coordinates.
    src: Rect,
    /// The same region in the new frame, in viewport coordinates.
    dst: Rect,
    started: Instant,
    duration: Duration,
}

impl LayoutTransition {
    fn new(src: Rect, dst: Rect) -> Self {
        // Longer pulls take a little longer, but never enough to feel slow.
        let ratio = if dst.w > 1.0 { src.w / dst.w } else { 1.0 };
        let magnitude = ratio.abs().max(1.0).log2().clamp(0.0, 4.0);
        Self {
            src,
            dst,
            started: Instant::now(),
            duration: Duration::from_millis(130 + (magnitude * 45.0) as u64),
        }
    }

    /// Where a rectangle in the new layout was, before the switch.
    fn origin_of(&self, rect: Rect) -> Rect {
        let scale = if self.dst.w > 0.0 {
            self.src.w / self.dst.w
        } else {
            1.0
        };
        Rect::new(
            (rect.x - self.dst.x).mul_add(scale, self.src.x),
            (rect.y - self.dst.y).mul_add(scale, self.src.y),
            rect.w * scale,
            rect.h * scale,
        )
    }

    /// The rectangle to draw at this instant, and whether the transition is
    /// still running.
    fn sample(&self, rect: Rect) -> (Rect, bool) {
        let elapsed =
            self.started.elapsed().as_secs_f32() / self.duration.as_secs_f32();
        if elapsed >= 1.0 {
            return (rect, false);
        }
        let eased = 1.0 - (1.0 - elapsed).powi(3);
        let from = self.origin_of(rect);
        let lerp = |a: f32, b: f32| (b - a).mul_add(eased, a);
        (
            Rect::new(
                lerp(from.x, rect.x),
                lerp(from.y, rect.y),
                lerp(from.w, rect.w),
                lerp(from.h, rect.h),
            ),
            true,
        )
    }
}

/// Layout for one (crumbs, area, options) combination.
struct LayoutCache {
    key: LayoutKey,
    tiles: Vec<Tile>,
}

#[derive(Clone, PartialEq)]
struct LayoutKey {
    crumbs: Vec<usize>,
    width: f32,
    height: f32,
    options: LayoutOptions,
    /// Bumped whenever the applied filter changes.
    filter: u64,
}

/// Directories left behind and come back from, for `<` and `>`.
///
/// Kept as absolute paths, not crumbs: a re-scan or a widening renumbers the
/// tree, and crumbs would then silently point at other directories.
#[derive(Clone, Debug, Default)]
pub struct History {
    pub back: Vec<PathBuf>,
    pub forward: Vec<PathBuf>,
}

/// How many directories back is remembered. Far more than anyone clicks
/// through, and small enough that pruning after a scan costs nothing.
const HISTORY_DEPTH: usize = 100;

/// Result of the removal run, summarised for the final screen.
#[derive(Clone, Debug, Default)]
pub struct RunSummary {
    pub total: usize,
    pub removed: u64,
    pub bytes: u64,
    pub failed: usize,
}

/// Everything the app knows and everything it can do.
pub struct Disktree {
    /// The directory the tree was scanned from.
    pub root_path: PathBuf,
    pub home: Option<PathBuf>,
    pub options: ScanOptions,
    pub tree: Option<Arc<Node>>,
    pub scan: Option<ScanHandle>,
    pub scan_epoch: u64,
    pub progress: ScanSnapshot,
    pub scan_error: Option<String>,

    pub screen: Screen,
    /// Path from the scanned root to the directory currently drawn.
    pub crumbs: Vec<usize>,
    pub selected: Option<Vec<usize>>,
    pub hovered: Option<Vec<usize>>,
    pub history: History,
    /// The history button under the pointer, `true` for `<`: its card is
    /// drawn below it for as long as it is.
    pub history_hover: Option<bool>,
    /// The pointer moved more recently than the keyboard navigated. Then the
    /// tile under the pointer is what Space, X and Enter act on; after an
    /// arrow or Tab it is the keyboard selection again.
    pub pointer_active: bool,
    pub view: View,
    /// The in-flight layout transition, if a level was just entered or left.
    pub transition: Option<LayoutTransition>,
    pub layout_options: LayoutOptions,
    cache: Option<LayoutCache>,

    /// Mouse position in treemap-local pixels, for hit-testing and the tooltip.
    pub pointer: Option<Point<Pixels>>,
    /// The treemap area in window space, recorded while painting.
    pub treemap_origin: Rc<Cell<Point<Pixels>>>,
    pub treemap_size: Rc<Cell<Size<Pixels>>>,

    pub marks: Marks,
    pub removal_mode: RemovalMode,
    pub trash_backend: TrashBackend,
    /// The permanent-deletion alert dialog is open. Trash needs no dialog: it
    /// is reversible, so it commits directly.
    pub confirm_open: bool,
    /// Focus owner for the alert dialog while it is open.
    pub confirm_focus: FocusHandle,
    /// Focus to move on the next occasion a window is in hand. Key handling
    /// has no window, and opening or closing the dialog must move focus.
    pub focus_request: Option<FocusTarget>,
    /// What the titlebar says, so it is only set when it changes.
    window_title: String,
    /// The window's `rem` in pixels, read each frame. The mosaic is laid out
    /// in pixels, so its header band and label thresholds are scaled by this
    /// to follow interface zoom like the rest of the interface.
    pub rem: f32,
    pub run: Option<RemovalHandle>,
    pub run_epoch: u64,
    pub run_summary: RunSummary,
    pub run_log: Vec<(PathBuf, Result<(), String>)>,
    pub space: Option<SpaceInfo>,
    /// Free space before the removal, for the honest "what did it actually
    /// free" number rather than the sum of what was marked.
    pub space_baseline: Option<SpaceInfo>,
    pub notice: Option<(String, Status)>,

    pub find: String,
    pub find_open: bool,
    /// What the find text matches in the directory on screen, recomputed
    /// on every keystroke. While typing it only dims what does not match.
    pub matches: Option<Arc<Matches>>,
    /// Enter was pressed: the mosaic lays out only the matches.
    pub filter_applied: bool,
    filter_epoch: u64,
    /// Bumped per keystroke; only the newest search's result is kept.
    find_epoch: u64,
    /// A search is running off the UI thread.
    pub finding: bool,
    /// Enter was pressed before the search finished: apply on arrival.
    apply_pending: bool,
    pub show_help: bool,
    pub show_selection: bool,
    pub focus: FocusHandle,
    /// A trail crumb's sibling menu, when open.
    pub crumb_menu: Option<CrumbMenu>,

    pub color_mode: ColorMode,
    /// The largest things worth clearing, recomputed when a scan lands.
    pub insights: Vec<Candidate>,
    /// What git knows about each checkout that has been selected; `None`
    /// once asked and found not to be one.
    pub git: FxHashMap<PathBuf, Option<GitState>>,
    git_pending: FxHashSet<PathBuf>,
    /// The device the scanned volume is mounted from.
    pub device: Option<String>,
    /// Whether macOS lets this process read everything: asked once, since a
    /// grant only takes effect after a relaunch. `None` off macOS.
    pub full_disk_access: Option<bool>,
    /// The top of the disk the scanned root lives on: what "Whole disk"
    /// scans. Follows the root when a folder is opened.
    pub disk_root: Option<PathBuf>,
    /// The side panel's width, in rem; dragged from its left edge.
    pub panel_rems: f32,
    pub scan_started: Option<Instant>,
    /// Where the scan in flight is rooted. Differs from `root_path` while
    /// widening, when the old tree stays on screen until the wider one lands.
    pub scan_root: PathBuf,
    pub scan_elapsed: Option<Duration>,
    /// Unix seconds when the tree landed: the "now" ages are measured from.
    pub scanned_at: i64,
}

impl Disktree {
    pub fn new(
        root_path: PathBuf,
        options: ScanOptions,
        depth: u32,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let home = std::env::home_dir();
        let space = space_info(&root_path).ok();
        let trash_backend = detect_trash_backend();
        let mut tree = Self {
            root_path,
            home,
            options,
            tree: None,
            scan: None,
            scan_epoch: 0,
            progress: ScanSnapshot::default(),
            scan_error: None,
            screen: Screen::Explore,
            crumbs: Vec::new(),
            selected: None,
            hovered: None,
            history: History::default(),
            history_hover: None,
            pointer_active: false,
            view: View::default(),
            transition: None,
            layout_options: LayoutOptions {
                max_depth: depth.clamp(1, 6),
                ..LayoutOptions::default()
            },
            cache: None,
            pointer: None,
            treemap_origin: Rc::new(Cell::new(Point::new(px(0.), px(0.)))),
            treemap_size: Rc::new(Cell::new(size(px(0.), px(0.)))),
            marks: Marks::default(),
            // Reversible by default whenever this machine has a trash: the
            // permanent path stays one choice away, behind a dialog.
            removal_mode: if trash_backend.is_available() {
                RemovalMode::Trash
            } else {
                RemovalMode::Permanent
            },
            trash_backend,
            confirm_open: false,
            confirm_focus: cx.focus_handle(),
            focus_request: None,
            window_title: String::new(),
            rem: crate::ui::BASE_REM,
            run: None,
            run_epoch: 0,
            run_summary: RunSummary::default(),
            run_log: Vec::new(),
            space,
            space_baseline: None,
            notice: None,
            find: String::new(),
            find_open: false,
            matches: None,
            filter_applied: false,
            filter_epoch: 0,
            find_epoch: 0,
            finding: false,
            apply_pending: false,
            show_help: false,
            show_selection: true,
            focus: cx.focus_handle(),
            crumb_menu: None,
            color_mode: ColorMode::Kind,
            insights: Vec::new(),
            git: FxHashMap::default(),
            git_pending: FxHashSet::default(),
            device: None,
            full_disk_access: None,
            disk_root: None,
            panel_rems: PANEL_REMS,
            scan_started: None,
            scan_root: PathBuf::new(),
            scan_elapsed: None,
            scanned_at: now_seconds(),
        };
        tree.device = device_for(&tree.root_path);
        tree.full_disk_access = tree
            .home
            .as_deref()
            .and_then(disktree_core::access::full_disk_access);
        // The disk of what is on screen, as `set_root` keeps it: `disktree
        // /Volumes/Ext` then `g` measures that drive, like opening it with ⌘O.
        tree.disk_root = volume_root_for(&tree.root_path);
        disktree_core::removal::prime_mount_points();
        tree.start_scan(cx);
        Self::start_space_ticker(cx);
        tree
    }

    /// Build a view over a tree that is already known.
    ///
    /// Mirrors [`Self::new`] without starting a walk, so a test can hand the
    /// screens a tree it already has instead of waiting on a real one. A
    /// feature that opens an archived scan would use this too.
    #[cfg(test)]
    pub fn with_tree(
        root_path: PathBuf,
        tree: Node,
        options: ScanOptions,
        depth: u32,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let mut app = Self::new(root_path, options, depth, cx);
        app.marks.refresh(&app.root_path, &tree, app.options.metric);
        app.tree = Some(Arc::new(tree));
        app.cache = None;
        app.refresh_insights();
        app.select_largest(cx);
        app
    }

    /// Scan a different root from scratch. Marks are kept; a mark outside
    /// the new root is shown as kept back, never removed.
    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<'_, Self>) {
        self.space = space_info(&root).ok();
        self.device = device_for(&root);
        // "Whole disk" means the disk of what is on screen, which a new root
        // can change: an external drive has its own.
        self.disk_root =
            volume_root_for(&root).or_else(|| self.disk_root.take());
        self.root_path = root;
        self.screen = Screen::Explore;
        self.start_scan(cx);
    }

    /// Go up to `above`, a directory containing the scanned root.
    ///
    /// Memoized: the tree already measured is handed to the walk and reused
    /// where it is reached, so only what is new at the wider level is read,
    /// and the current view stays on screen until the wider tree lands.
    pub fn widen_to(&mut self, above: PathBuf, cx: &mut Context<'_, Self>) {
        let Some(tree) = self.tree.clone() else {
            self.set_root(above, cx);
            return;
        };
        if !self.root_path.starts_with(&above) || self.root_path == above {
            return;
        }
        if let Some(scan) = &self.scan {
            scan.cancel();
        }
        self.scan_epoch += 1;
        let epoch = self.scan_epoch;
        self.progress = ScanSnapshot::default();
        self.scan_error = None;
        self.scan_started = Some(Instant::now());
        self.scan_elapsed = None;
        self.scan_root.clone_from(&above);
        self.remember();
        let known = Known {
            path: self.root_path.clone(),
            tree,
        };
        self.scan = Some(ScanHandle::spawn_with(
            above,
            self.options.clone(),
            Some(known),
        ));
        Self::poll_scan(epoch, cx);
        cx.notify();
    }

    /// `g`: the whole disk. Widens when the disk is above the scanned root,
    /// and goes to its top when it already is the root.
    pub fn go_to_disk(&mut self, cx: &mut Context<'_, Self>) {
        let Some(disk) = self.disk_root.clone() else {
            return;
        };
        if disk == self.root_path {
            self.go_to(Vec::new(), cx);
        } else {
            self.widen_to(disk, cx);
        }
    }

    /// Recompute "worth a look" from the tree on screen.
    fn refresh_insights(&mut self) {
        self.scanned_at = now_seconds();
        self.insights = self.tree.as_deref().map_or_else(Vec::new, |tree| {
            worth_a_look(tree, self.scanned_at, INSIGHT_LIMIT)
        });
    }

    /// Ask git about `path` once, off the UI thread, if it is a checkout.
    pub fn ensure_git(&mut self, path: &Path, cx: &Context<'_, Self>) {
        if self.git.contains_key(path) || self.git_pending.contains(path) {
            return;
        }
        let path = path.to_path_buf();
        self.git_pending.insert(path.clone());
        let task = cx.background_executor().spawn({
            let path = path.clone();
            async move { crate::git::state(&path) }
        });
        cx.spawn(async move |this, cx| {
            let state = task.await;
            let _ = this.update(cx, |this, cx| {
                this.git_pending.remove(&path);
                this.git.insert(path, state);
                cx.notify();
            });
        })
        .detach();
    }

    /// Select the largest entry of the current root, so the selection line, the
    /// tooltip and the mark key all have something to act on from the first
    /// frame. The largest entry is also the answer to "what is eating my disk"
    /// most of the time.
    fn select_largest(&mut self, cx: &mut Context<'_, Self>) {
        if self.selected.is_some() {
            return;
        }
        if let Some(index) = self.current().and_then(Node::largest_child) {
            self.selected = Some(vec![index]);
            cx.notify();
        }
    }

    // ── scanning ────────────────────────────────────────────────────────

    /// Start a fresh scan, abandoning any walk still in progress.
    pub fn start_scan(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(scan) = &self.scan {
            scan.cancel();
        }
        self.scan_epoch += 1;
        let epoch = self.scan_epoch;
        self.progress = ScanSnapshot::default();
        self.scan_error = None;
        // A new scan starts at its root; where it was is one `<` away.
        if !self.crumbs.is_empty() {
            self.remember();
        }
        self.tree = None;
        self.crumbs.clear();
        self.selected = None;
        self.hovered = None;
        self.view = View::default();
        self.cache = None;
        self.insights.clear();
        self.git.clear();
        self.clear_filter();
        self.scan_started = Some(Instant::now());
        self.scan_elapsed = None;
        self.scan_root.clone_from(&self.root_path);
        self.scan = Some(ScanHandle::spawn(
            self.root_path.clone(),
            self.options.clone(),
        ));
        Self::poll_scan(epoch, cx);
        cx.notify();
    }

    fn poll_scan(epoch: u64, cx: &Context<'_, Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(110))
                    .await;
                let keep_going = this
                    .update(cx, |this, cx| this.poll_scan_once(epoch, cx))
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Drain the scan channel once. Returns whether the poller should keep
    /// ticking. Driven by the poll task above, and by the tests, which cannot
    /// wait on the test clock while a real worker thread walks a directory.
    pub(crate) fn poll_scan_once(
        &mut self,
        epoch: u64,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if epoch != self.scan_epoch {
            return false;
        }
        let Some(scan) = &self.scan else {
            return false;
        };
        self.progress = scan.progress.snapshot();
        let Some(outcome) = scan.poll() else {
            cx.notify();
            return true;
        };
        match outcome {
            Ok(node) => {
                // A widening scan lands on a new root: move the view up to it,
                // with the directory it came from selected.
                let came_from = (self.scan_root != self.root_path)
                    .then(|| self.root_path.clone());
                if came_from.is_some() {
                    self.clear_filter();
                    self.root_path.clone_from(&self.scan_root);
                    self.space = space_info(&self.root_path).ok();
                    self.device = device_for(&self.root_path);
                }
                let metric = self.options.metric;
                self.marks.refresh(&self.root_path, &node, metric);
                self.tree = Some(Arc::new(node));
                self.cache = None;
                self.refresh_insights();
                self.scan_elapsed =
                    self.scan_started.map(|started| started.elapsed());
                if let Some(from) = came_from {
                    let crumbs = self.crumbs_for_path(&from);
                    self.crumbs.clear();
                    self.view = View::default();
                    self.transition = None;
                    self.forget_hover();
                    self.selected = crumbs.and_then(|crumbs| {
                        crumbs.first().map(|&top| vec![top])
                    });
                }
                self.keep_selection_valid();
                self.prune_history();
                self.select_largest(cx);
            }
            Err(error) => self.scan_error = Some(error.to_string()),
        }
        self.scan = None;
        self.progress.finished = true;
        cx.notify();
        false
    }

    fn keep_selection_valid(&mut self) {
        let Some(tree) = &self.tree else {
            return;
        };
        if tree.resolve(&self.crumbs).is_none() {
            self.crumbs.clear();
        }
        if let Some(selected) = self.selected.clone()
            && tree.resolve(&selected).is_none()
        {
            self.selected = None;
        }
    }

    /// Space changes under us all the time; poll it so the meter is live
    /// without repainting when nothing moved.
    fn start_space_ticker(cx: &Context<'_, Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let interval = this
                    .update(cx, |this, _| {
                        if this.screen == Screen::Running {
                            Duration::from_millis(150)
                        } else {
                            Duration::from_millis(1200)
                        }
                    })
                    .unwrap_or(Duration::from_millis(1200));
                cx.background_executor().timer(interval).await;
                let ok = this
                    .update(cx, |this, cx| {
                        let path = this.root_path.clone();
                        let fresh = space_info(&path).ok();
                        if fresh != this.space {
                            this.space = fresh;
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !ok {
                    break;
                }
            }
        })
        .detach();
    }

    // ── navigation ──────────────────────────────────────────────────────

    pub fn tree(&self) -> Option<&Node> {
        self.tree.as_deref()
    }

    /// The node the treemap is currently rooted at.
    pub fn current(&self) -> Option<&Node> {
        self.tree
            .as_ref()
            .and_then(|tree| tree.resolve(&self.crumbs))
    }

    pub fn node_at(&self, crumbs: &[usize]) -> Option<&Node> {
        self.tree.as_ref().and_then(|tree| tree.resolve(crumbs))
    }

    pub fn path_at(&self, crumbs: &[usize]) -> Option<PathBuf> {
        let tree = self.tree.as_ref()?;
        Some(path_of(&self.root_path, tree, crumbs))
    }

    /// Path of the directory currently drawn.
    pub fn current_path(&self) -> PathBuf {
        self.path_at(&self.crumbs)
            .unwrap_or_else(|| self.root_path.clone())
    }

    /// Breadcrumb labels from the scanned root to the current directory.
    /// The trail, from `/`: the directories above the scanned root, which
    /// widen the scan, then the root and the path into the tree.
    pub fn breadcrumbs(&self) -> Vec<(String, Crumb)> {
        let mut trail: Vec<(String, Crumb)> = self
            .root_path
            .ancestors()
            .skip(1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|path| (crumb_label(path), Crumb::Above(path.to_path_buf())))
            .collect();
        trail.push((crumb_label(&self.root_path), Crumb::Tree(Vec::new())));
        let Some(tree) = &self.tree else {
            return trail;
        };
        let mut crumbs: Vec<usize> = Vec::new();
        for &index in &self.crumbs {
            let Some(node) =
                tree.resolve(&crumbs).and_then(|node| node.child(index))
            else {
                break;
            };
            crumbs.push(index);
            trail.push((node.name.to_string(), Crumb::Tree(crumbs.clone())));
        }
        trail
    }

    /// The children of `parent`, largest first, for a sibling menu, and how
    /// many more there are beyond [`SIBLING_ROWS`].
    pub fn siblings(&self, parent: &[usize]) -> (Vec<Sibling>, usize) {
        let metric = self.options.metric;
        let Some(node) = self.node_at(parent) else {
            return (Vec::new(), 0);
        };
        let mut rows: Vec<Sibling> = node
            .children
            .iter()
            .enumerate()
            .map(|(index, child)| Sibling {
                index,
                name: child.name.to_string(),
                value: child.value(metric),
                category: child.category,
                is_dir: child.is_dir(),
            })
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.value));
        let more = rows.len().saturating_sub(SIBLING_ROWS);
        rows.truncate(SIBLING_ROWS);
        (rows, more)
    }

    /// Open the sibling menu for the crumb at `crumbs` (not the root).
    pub fn open_crumb_menu(
        &mut self,
        crumbs: &[usize],
        cx: &mut Context<'_, Self>,
    ) {
        let Some((&current, parent)) = crumbs.split_last() else {
            return;
        };
        let (rows, _) = self.siblings(parent);
        let highlighted = rows
            .iter()
            .position(|row| row.index == current)
            .unwrap_or(0);
        self.crumb_menu = Some(CrumbMenu {
            parent: parent.to_vec(),
            current,
            highlighted,
        });
        cx.notify();
    }

    /// Go to a sibling: into it when it is a directory, beside it (selected)
    /// when it is a file.
    pub fn choose_sibling(
        &mut self,
        parent: &[usize],
        index: usize,
        cx: &mut Context<'_, Self>,
    ) {
        self.crumb_menu = None;
        let mut crumbs = parent.to_vec();
        crumbs.push(index);
        if self.node_at(&crumbs).is_some_and(Node::is_dir) {
            self.go_to(crumbs, cx);
        } else {
            self.reveal(crumbs, cx);
        }
    }

    /// Keys while a sibling menu is open: it owns the arrows, Enter and
    /// Escape, and any other key closes it before acting.
    fn on_menu_key(&mut self, key: &str, cx: &mut Context<'_, Self>) -> bool {
        let Some(menu) = self.crumb_menu.clone() else {
            return false;
        };
        let (rows, _) = self.siblings(&menu.parent);
        let last = rows.len().saturating_sub(1);
        match key {
            "down" | "j" => {
                self.set_highlight((menu.highlighted + 1).min(last));
            }
            "up" | "k" => {
                self.set_highlight(menu.highlighted.saturating_sub(1));
            }
            "home" => self.set_highlight(0),
            "end" => self.set_highlight(last),
            "enter" | "space" | "right" | "l" => {
                if let Some(row) = rows.get(menu.highlighted) {
                    self.choose_sibling(&menu.parent, row.index, cx);
                }
            }
            "escape" | "left" | "h" => self.crumb_menu = None,
            _ => {
                self.crumb_menu = None;
                cx.notify();
                return false;
            }
        }
        cx.notify();
        true
    }

    const fn set_highlight(&mut self, row: usize) {
        if let Some(menu) = &mut self.crumb_menu {
            menu.highlighted = row;
        }
    }

    /// Descend into the selected tile, or into the largest child of the
    /// current root when nothing is selected.
    pub fn descend(&mut self, cx: &mut Context<'_, Self>) {
        let target = match self.selected.clone() {
            // Enter what is selected, however deep: a directory opens itself,
            // a file opens the directory holding it.
            Some(selected) if selected.len() > self.crumbs.len() => {
                if self.node_at(&selected).is_some_and(Node::is_dir) {
                    selected
                } else {
                    selected[..selected.len() - 1].to_vec()
                }
            }
            // Nothing below the root is selected: the largest entry.
            _ => match self.current().and_then(Node::largest_child) {
                Some(index) => {
                    let mut crumbs = self.crumbs.clone();
                    crumbs.push(index);
                    crumbs
                }
                None => return,
            },
        };
        let from = self.tile_body(&target).map(|rect| self.view.project(rect));
        self.enter(target, from, cx);
    }

    /// Go inside one child of the current root, so that it fills the viewport.
    ///
    /// `from` is where the child was on screen, when the caller knows it.
    /// Make `target` — any directory below the current root — the root.
    ///
    /// `from` is where that directory's contents were on screen, so the
    /// transition grows them from exactly there into the full viewport.
    fn enter(
        &mut self,
        target: Vec<usize>,
        from: Option<Rect>,
        cx: &mut Context<'_, Self>,
    ) {
        if target.len() <= self.crumbs.len()
            || !target.starts_with(&self.crumbs)
        {
            return;
        }
        let Some(node) = self.node_at(&target) else {
            return;
        };
        if !node.is_dir() || node.children.is_empty() {
            return;
        }
        self.remember();
        self.selected = Some(target.clone());
        self.crumbs = target;
        self.forget_hover();
        self.cache = None;
        let area = self.treemap_size.get();
        let src = from.unwrap_or_else(|| self.view.visible_base(area));
        self.view = View::IDENTITY;
        let dst = Rect::new(
            0.0,
            0.0,
            area.width.as_f32().max(1.0),
            area.height.as_f32().max(1.0),
        );
        self.transition = Some(LayoutTransition::new(src, dst));
        cx.notify();
    }

    /// Where a drawn directory's contents sit: its tile below the name band.
    /// This, not the whole tile, is the region its children occupy, so it is
    /// what a transition into or out of it has to map.
    pub fn tile_body(&mut self, crumbs: &[usize]) -> Option<Rect> {
        let tile =
            self.layout()?.iter().find(|tile| tile.crumbs() == crumbs)?;
        Some(match tile.header {
            Some(header) => Rect::new(
                tile.rect.x,
                header.bottom(),
                tile.rect.w,
                tile.rect.bottom() - header.bottom(),
            ),
            None => tile.rect,
        })
    }

    /// The deepest drawn directory under a viewport point that has contents
    /// to show: what zooming at that point is zooming into.
    fn zoom_target(&mut self, x: f32, y: f32) -> Option<Vec<usize>> {
        let hovered = self.tile_at(x, y)?;
        let root = self.crumbs.len();
        (root + 1..=hovered.len())
            .rev()
            .map(|length| hovered[..length].to_vec())
            .find(|crumbs| {
                self.node_at(crumbs).is_some_and(|node| {
                    node.is_dir() && !node.children.is_empty()
                }) && self.tile_body(crumbs).is_some()
            })
    }

    /// Ascend to the parent directory, keeping the directory we came from in
    /// view so the motion reads as zooming out.
    pub fn ascend(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(parent_crumbs) = self.parent_crumbs() {
            self.ascend_to(parent_crumbs, cx);
        }
    }

    /// Make `ancestor`, a directory above the current root, the root.
    ///
    /// The directory we leave shrinks back into its tile when that tile is
    /// drawn at the new level; from further up it is too small to track, and
    /// the new level simply lands.
    fn ascend_to(&mut self, ancestor: Vec<usize>, cx: &mut Context<'_, Self>) {
        self.remember();
        // The region we are looking at now, and where it sits in the layout
        // we are going back to.
        let area = self.treemap_size.get();
        let child_crumbs = self.crumbs.clone();
        let src = self.view.visible_base(area);
        self.crumbs.clone_from(&ancestor);
        self.forget_hover();
        self.cache = None;
        self.selected = Some(ancestor);
        self.view = View::IDENTITY;
        self.transition = self
            .tile_body(&child_crumbs)
            .filter(|rect| rect.w > 1.0 && rect.h > 1.0)
            .map(|dst| LayoutTransition::new(src, dst));
        cx.notify();
    }

    /// The crumbs of the parent of the current root, if any.
    pub fn parent_crumbs(&self) -> Option<Vec<usize>> {
        if self.crumbs.is_empty() {
            None
        } else {
            Some(self.crumbs[..self.crumbs.len() - 1].to_vec())
        }
    }

    /// Jump straight to a crumb from the breadcrumb bar.
    ///
    /// A jump can skip several levels, so there is no single region to move:
    /// it lands immediately, the way selecting a folder does.
    pub fn go_to(&mut self, crumbs: Vec<usize>, cx: &mut Context<'_, Self>) {
        self.crumb_menu = None;
        // A reveal in the directory on screen goes nowhere.
        if crumbs != self.crumbs {
            self.remember();
        }
        self.crumbs.clone_from(&crumbs);
        self.selected = Some(crumbs);
        self.forget_hover();
        self.view = View::IDENTITY;
        self.transition = None;
        self.cache = None;
        cx.notify();
    }

    /// Note the directory on screen as one to come back to, before leaving
    /// it. A new departure ends whatever was ahead, as in a browser.
    fn remember(&mut self) {
        if self.tree.is_none() {
            return;
        }
        let here = self.current_path();
        if self.history.back.last() != Some(&here) {
            self.history.back.push(here);
        }
        if self.history.back.len() > HISTORY_DEPTH {
            self.history.back.remove(0);
        }
        self.history.forward.clear();
    }

    /// Drop what a new tree no longer holds: a removed directory, or one
    /// outside a new root. What is left always resolves, so `<` and `>` are
    /// enabled exactly when they would go somewhere.
    fn prune_history(&mut self) {
        let mut history = std::mem::take(&mut self.history);
        history
            .back
            .retain(|path| self.crumbs_for_path(path).is_some());
        history
            .forward
            .retain(|path| self.crumbs_for_path(path).is_some());
        self.history = history;
    }

    /// Whether `<` would go anywhere: what drives its button's disabled state.
    pub fn can_go_back(&self) -> bool {
        self.history_target(true).is_some()
    }

    /// Whether `>` would go anywhere.
    pub fn can_go_forward(&self) -> bool {
        self.history_target(false).is_some()
    }

    /// `<`: the directory on screen before this one.
    pub fn go_back(&mut self, cx: &mut Context<'_, Self>) {
        self.step_history(true, cx);
    }

    /// `>`: undo a `<`.
    pub fn go_forward(&mut self, cx: &mut Context<'_, Self>) {
        self.step_history(false, cx);
    }

    /// Where `<` (or `>`) would go, and its place on that stack: the newest
    /// entry that is somewhere else. An entry can be where we already are,
    /// after a widening that was superseded; it is skipped rather than spend
    /// a click on nothing. Every other entry resolves, being pruned whenever
    /// a tree lands.
    pub fn history_target(&self, back: bool) -> Option<(usize, Vec<usize>)> {
        self.tree.as_ref()?;
        let stack = if back {
            &self.history.back
        } else {
            &self.history.forward
        };
        stack.iter().enumerate().rev().find_map(|(at, path)| {
            self.crumbs_for_path(path)
                .filter(|crumbs| *crumbs != self.crumbs)
                .map(|crumbs| (at, crumbs))
        })
    }

    fn step_history(&mut self, back: bool, cx: &mut Context<'_, Self>) {
        let Some((at, target)) = self.history_target(back) else {
            return;
        };
        let here = self.current_path();
        let mut history = std::mem::take(&mut self.history);
        let (from, to) = if back {
            (&mut history.back, &mut history.forward)
        } else {
            (&mut history.forward, &mut history.back)
        };
        from.truncate(at);
        to.push(here);
        // The move is an ordinary one, animation and all; it records itself
        // like any other, so the stacks are put back afterwards.
        self.travel(target, cx);
        self.history = history;
        cx.notify();
    }

    /// Go to `target` with the motion that fits: grow into a directory
    /// below, shrink back out to one above, or land on one beside.
    fn travel(&mut self, target: Vec<usize>, cx: &mut Context<'_, Self>) {
        let enterable = self
            .node_at(&target)
            .is_some_and(|node| node.is_dir() && !node.children.is_empty());
        if target.len() > self.crumbs.len()
            && target.starts_with(&self.crumbs)
            && enterable
        {
            let from =
                self.tile_body(&target).map(|rect| self.view.project(rect));
            self.enter(target, from, cx);
        } else if target.len() < self.crumbs.len()
            && self.crumbs.starts_with(&target)
        {
            self.ascend_to(target, cx);
        } else {
            self.go_to(target, cx);
        }
    }

    /// Show `crumbs` in its directory, selected: what a "worth a look" row
    /// or a found name does.
    pub fn reveal(&mut self, crumbs: Vec<usize>, cx: &mut Context<'_, Self>) {
        let parent = crumbs[..crumbs.len().saturating_sub(1)].to_vec();
        self.go_to(parent, cx);
        self.selected = Some(crumbs);
        self.pointer_active = false;
        cx.notify();
    }

    /// Select the tile at `crumbs` without changing the root.
    pub fn select(
        &mut self,
        crumbs: Option<Vec<usize>>,
        cx: &mut Context<'_, Self>,
    ) {
        self.selected = crumbs;
        cx.notify();
    }

    /// Move the selection geometrically, falling back to the parent at an edge.
    /// The tile a key acts on: under the pointer if the pointer moved last,
    /// otherwise the keyboard selection.
    pub fn action_target(&self) -> Option<Vec<usize>> {
        if self.pointer_active {
            self.hovered.clone().or_else(|| self.selected.clone())
        } else {
            self.selected.clone()
        }
    }

    /// Make the pointed-at tile the selection before a key acts, so marking,
    /// opening and arrow movement all start from what the user is looking at.
    fn adopt_pointer_target(&mut self) {
        if self.pointer_active
            && let Some(hovered) = self.hovered.clone()
        {
            self.selected = Some(hovered);
        }
    }

    /// After the layout changes under a still pointer, its hover is stale
    /// until the pointer moves again.
    fn forget_hover(&mut self) {
        self.hovered = None;
        self.pointer_active = false;
    }

    pub fn move_selection(
        &mut self,
        direction: Direction,
        cx: &mut Context<'_, Self>,
    ) {
        self.pointer_active = false;
        let area = self.treemap_size.get();
        let _ = area;
        let Some(tiles) = self.layout().map(<[Tile]>::to_vec) else {
            return;
        };
        let Some(current) = self.selected.clone() else {
            if let Some(first) = tiles.first() {
                let crumbs = first.crumbs().to_vec();
                self.select(Some(crumbs), cx);
            }
            return;
        };
        let Some(from) = tiles
            .iter()
            .find(|tile| tile.crumbs() == current.as_slice())
            .map(|tile| self.view.project(tile.rect))
        else {
            return;
        };

        // Same-depth neighbours only: stepping into a child by arrow key would
        // make the depth of the selection impossible to predict.
        let depth = current.len();
        let mut best: Option<(f32, Vec<usize>)> = None;
        for tile in &tiles {
            let crumbs = tile.crumbs();
            if crumbs.len() != depth || crumbs == current.as_slice() {
                continue;
            }
            let rect = self.view.project(tile.rect);
            let Some(gap) = direction.gap(&from, &rect) else {
                continue;
            };
            let offset = direction.offset(&from, &rect);
            let score = offset.mul_add(2.5, gap);
            if best.as_ref().is_none_or(|(existing, _)| score < *existing) {
                best = Some((score, crumbs.to_vec()));
            }
        }

        if let Some((_, crumbs)) = best {
            self.select(Some(crumbs), cx);
        } else if direction.is_backwards()
            && let Some(parent) = self.parent_crumbs()
        {
            self.select(Some(parent), cx);
        }
    }

    /// Select the next sibling by rank, which is next-largest by the active
    /// metric. Scanning a directory for space is exactly this walk.
    pub fn cycle_sibling(&mut self, step: isize, cx: &mut Context<'_, Self>) {
        self.pointer_active = false;
        let siblings = self.ranked_siblings();
        if siblings.is_empty() {
            return;
        }
        let current = self.selected.as_ref().and_then(|selected| {
            siblings
                .iter()
                .position(|crumbs| crumbs.as_slice() == selected.as_slice())
        });
        let next = match current {
            Some(index) => {
                let len = siblings.len().cast_signed();
                (((index.cast_signed() + step) % len + len) % len) as usize
            }
            None if step >= 0 => 0,
            None => siblings.len() - 1,
        };
        self.select(Some(siblings[next].clone()), cx);
    }

    fn ranked_siblings(&self) -> Vec<Vec<usize>> {
        let parent = self.selected.as_ref().map_or_else(
            || self.crumbs.clone(),
            |selected| {
                if selected.len() <= self.crumbs.len() {
                    self.crumbs.clone()
                } else {
                    selected[..selected.len() - 1].to_vec()
                }
            },
        );
        let Some(node) = self.node_at(&parent) else {
            return Vec::new();
        };
        (0..node.children.len())
            .map(|index| {
                let mut crumbs = parent.clone();
                crumbs.push(index);
                crumbs
            })
            .collect()
    }

    /// Mark or unmark the current selection.
    pub fn toggle_mark_selected(&mut self, cx: &mut Context<'_, Self>) {
        let Some(crumbs) = self.selected.clone() else {
            return;
        };
        if crumbs.is_empty() {
            self.notice = Some((
                "the scanned root cannot be removed; open a directory first"
                    .into(),
                Status::Warning,
            ));
            cx.notify();
            return;
        }
        self.toggle_mark(&crumbs, cx);
    }

    /// Mark or unmark the node at `crumbs`.
    ///
    /// A mark covers everything beneath it, since removing a directory takes
    /// its contents with it: marking a directory absorbs the marks already
    /// inside it, and a path inside a marked directory cannot be marked or
    /// kept on its own — it says which mark it goes with instead.
    pub fn toggle_mark(
        &mut self,
        crumbs: &[usize],
        cx: &mut Context<'_, Self>,
    ) {
        let Some(target) = self.target_at(crumbs) else {
            return;
        };
        self.notice = None;
        if !self.marks.contains(&target.path)
            && let Some(ancestor) = self.marked_ancestor(&target.path)
        {
            self.notice = Some((
                format!(
                    "{} goes with the marked {}; unmark that to keep it",
                    display_path(&target.path, self.home.as_deref()),
                    display_path(&ancestor, self.home.as_deref())
                ),
                Status::Warning,
            ));
            cx.notify();
            return;
        }
        let path = target.path.clone();
        if self.marks.toggle(target) {
            self.space_baseline = self.space_baseline.or(self.space);
            let inside: Vec<PathBuf> = self
                .marks
                .items()
                .iter()
                .filter(|item| {
                    item.path != path && item.path.starts_with(&path)
                })
                .map(|item| item.path.clone())
                .collect();
            for inner in &inside {
                self.marks.remove(inner);
            }
            if !inside.is_empty() {
                self.notice = Some((
                    format!(
                        "{} now covers {} mark{} inside it",
                        display_path(&path, self.home.as_deref()),
                        inside.len(),
                        if inside.len() == 1 { "" } else { "s" }
                    ),
                    Status::Neutral,
                ));
            }
        }
        cx.notify();
    }

    /// The marked directory `path` is inside, if any; never `path` itself.
    pub fn marked_ancestor(&self, path: &Path) -> Option<PathBuf> {
        self.marks
            .items()
            .iter()
            .filter(|item| item.path != path && path.starts_with(&item.path))
            .map(|item| item.path.clone())
            .min_by_key(|ancestor| ancestor.as_os_str().len())
    }

    pub fn target_at(&self, crumbs: &[usize]) -> Option<Target> {
        let node = self.node_at(crumbs)?;
        let path = self.path_at(crumbs)?;
        Some(Target {
            hidden: is_hidden(&path),
            path,
            bytes: node.value(self.options.metric),
            is_dir: node.is_dir(),
        })
    }

    pub fn unmark(&mut self, path: &Path, cx: &mut Context<'_, Self>) {
        self.marks.remove(path);
        cx.notify();
    }

    pub fn clear_marks(&mut self, cx: &mut Context<'_, Self>) {
        self.marks.clear();
        self.notice = None;
        cx.notify();
    }

    /// The plan the review screen shows and the removal runs.
    pub fn plan(&self) -> Plan {
        plan(self.marks.items(), &self.root_path)
    }

    // ── layout and hit-testing ──────────────────────────────────────────

    /// Resolve everything the mosaic needs for this frame.
    ///
    /// Runs once per render, before the paint callbacks: painting then only has
    /// to draw, and the marked/hidden/selected state of every tile is decided
    /// here, where the tree and the marks are both at hand.
    pub fn prepare(&mut self) -> Mosaic {
        let metric = self.options.metric;
        let view = self.view;
        let hovered = self.hovered.clone();
        let selected = self.selected.clone();

        // Marks are paths; the mosaic thinks in crumbs. Resolve once per frame
        // rather than building a path for every tile.
        let mut marked: FxHashSet<Vec<usize>> = FxHashSet::default();
        for item in self.marks.items() {
            if let Some(crumbs) = self.crumbs_for_path(&item.path) {
                marked.insert(crumbs);
            }
        }

        let Some(tiles) = self.layout().map(<[Tile]>::to_vec) else {
            return Mosaic {
                view,
                ..Mosaic::default()
            };
        };

        // Hue comes from the node's kind; lightness from its depth in this
        // view, so the first level always reads as the first level.
        let age = self.color_mode == ColorMode::Age;
        let now = self.scanned_at;
        let mut decorations = Vec::with_capacity(tiles.len());
        let mut labels = Vec::new();
        for tile in &tiles {
            let crumbs = tile.crumbs();
            let node = match &tile.kind {
                TileKind::Node { crumbs } => self.node_at(crumbs),
                TileKind::Others { .. } => None,
            };
            let category = node.map_or_else(Default::default, |n| n.category);
            let age_bucket = (age && node.is_some_and(|n| n.modified > 0))
                .then(|| {
                    let days =
                        (now - node.map_or(now, |n| n.modified)) / 86_400;
                    crate::palette::age_bucket(days)
                });
            let filtered = match self.matches.as_deref().map(|m| m.keep(crumbs))
            {
                None | Some(Some(Keep::Whole)) => Filtered::Shown,
                Some(Some(Keep::Partial { .. })) => Filtered::Holds,
                Some(None) => Filtered::Out,
            };
            let is_marked = marked.contains(crumbs);
            // Everything inside a marked directory goes with it, so it is
            // drawn marked too.
            let is_covered = !marked.is_empty()
                && (1..crumbs.len())
                    .any(|length| marked.contains(&crumbs[..length]));
            decorations.push(TileDeco {
                rect: self.animated_rect(tile.rect),
                depth: tile.depth,
                category,
                age_bucket,
                reclaimable: node.is_some_and(|n| n.reclaim.is_some()),
                filtered,
                unreadable: node.is_some_and(|n| n.read_error),
                marked: is_marked,
                covered: is_covered,
                hovered: hovered.as_deref() == Some(crumbs),
                selected: selected.as_deref() == Some(crumbs),
            });

            // Labels are chosen in screen space: zooming in makes room for more
            // of them, which is the point of zooming in.
            let drawn = self.animated_rect(tile.rect);
            let screen = view.project(
                tile.header
                    .map_or(drawn, |header| self.animated_rect(header)),
            );
            if screen.w < LABEL_MIN_W_REMS * self.rem
                || screen.h < LABEL_MIN_H_REMS * self.rem
            {
                continue;
            }
            match &tile.kind {
                TileKind::Node { crumbs } => {
                    let Some(node) = self.node_at(crumbs) else {
                        continue;
                    };
                    labels.push(Label {
                        text: node.name.to_string(),
                        rect: self.animated_rect(tile.rect),
                        header: tile
                            .header
                            .map(|header| self.animated_rect(header)),
                        depth: tile.depth,
                        dim: filtered == Filtered::Out,
                        marked: is_marked || is_covered,
                        size_text: crate::widgets::short_value(node, metric),
                    });
                }
                TileKind::Others { count, .. } => labels.push(Label {
                    text: format!("+{count} more"),
                    rect: self.animated_rect(tile.rect),
                    header: None,
                    depth: tile.depth,
                    dim: self.matches.is_some(),
                    marked: false,
                    size_text: String::new(),
                }),
            }
        }

        labels.sort_by(|left, right| {
            let left_area = left.rect.w * left.rect.h;
            let right_area = right.rect.w * right.rect.h;
            right_area.total_cmp(&left_area)
        });
        labels.truncate(MAX_LABELS);

        Mosaic {
            tiles: decorations,
            labels,
            view,
        }
    }

    /// Crumbs for an absolute path, if the tree still contains it.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "a marked path may have been removed from the tree already"
    )]
    pub fn crumbs_for_path(&self, path: &Path) -> Option<Vec<usize>> {
        // The path is relative to the scanned root, so the walk starts there,
        // not at the directory currently drawn.
        let relative = path.strip_prefix(&self.root_path).ok()?;
        let tree = self.tree.clone()?;
        let mut node: &Node = &tree;
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

    /// Tiles for the current root and viewport size, computed once per change.
    pub fn layout(&mut self) -> Option<&[Tile]> {
        // A 17 px band at the default rem, scaled so zoom keeps the band's
        // relationship to the label inside it.
        self.layout_options.header = HEADER_REMS * self.rem;
        self.layout_options.header_inner = HEADER_INNER_REMS * self.rem;
        // A filter is about the directory it was typed in: above it, it
        // would hide everything beside that directory, so it lapses.
        if self
            .matches
            .as_ref()
            .is_some_and(|matches| !self.crumbs.starts_with(&matches.base))
        {
            self.clear_filter();
        }
        let area = self.treemap_size.get();
        let key = LayoutKey {
            crumbs: self.crumbs.clone(),
            width: area.width.as_f32().round(),
            height: area.height.as_f32().round(),
            options: self.layout_options.clone(),
            filter: self.filter_epoch,
        };
        if key.width < 1.0 || key.height < 1.0 {
            return None;
        }
        let stale = self.cache.as_ref().is_none_or(|cache| cache.key != key);
        if stale {
            let tree = self.tree.clone()?;
            let node = tree.resolve(&self.crumbs)?;
            let rect = Rect::new(0.0, 0.0, key.width, key.height);
            let filter =
                self.matches.as_deref().filter(|_| self.filter_applied);
            let tiles = layout_filtered(
                node,
                &key.crumbs,
                rect,
                self.options.metric,
                &key.options,
                filter,
            );
            self.cache = Some(LayoutCache { key, tiles });
        }
        self.cache.as_ref().map(|cache| cache.tiles.as_slice())
    }

    /// Base-space rect of the tile at `crumbs`, if it is currently drawn.
    #[cfg(test)]
    pub fn tile_rect(&mut self, crumbs: &[usize]) -> Option<Rect> {
        self.layout()?
            .iter()
            .find(|tile| tile.crumbs() == crumbs)
            .map(|tile| tile.rect)
    }

    /// The deepest tile under a viewport point.
    pub fn tile_at(&mut self, x: f32, y: f32) -> Option<Vec<usize>> {
        let (base_x, base_y) = self.view.unproject(x, y);
        let tiles = self.layout()?;
        hit(tiles, base_x, base_y).map(|tile| tile.crumbs().to_vec())
    }

    // ── view ────────────────────────────────────────────────────────────

    /// Zoom toward a point.
    ///
    /// When `descend` is set the wheel stops magnifying at the point where the
    /// directory under the pointer exactly fits the viewport, and the next
    /// notch goes inside it. That ceiling is what keeps the two gestures
    /// continuous: at the moment the level changes, the tiles inside the
    /// directory are already as large as they will be, so entering makes them
    /// grow rather than shrink.
    pub fn zoom_at(
        &mut self,
        x: f32,
        y: f32,
        factor: f32,
        descend: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let area = self.treemap_size.get();

        // At the bottom of the zoom, zooming out goes up a level.
        if factor < 1.0 && self.view.scale <= View::MIN_SCALE + f32::EPSILON {
            if descend {
                self.ascend(cx);
            }
            return;
        }

        // One directory decides both how far the wheel magnifies and where it
        // then goes: the deepest one under the pointer. At the ceiling its
        // contents fill the view, so going inside continues the same motion.
        let target = if descend {
            self.zoom_target(x, y)
        } else {
            None
        };
        let body = target.as_ref().and_then(|crumbs| self.tile_body(crumbs));
        let ceiling = body
            .map_or(View::MAX_SCALE, |rect| View::fit_scale(rect, area))
            .clamp(View::MIN_SCALE, View::MAX_SCALE);

        if factor > 1.0
            && self.view.scale >= ceiling - f32::EPSILON
            && let Some(target) = target
        {
            let from = body.map(|rect| self.view.project(rect));
            self.enter(target, from, cx);
            return;
        }

        self.view = self.view.zoomed_at(x, y, factor, ceiling).clamped(area);
        cx.notify();
    }

    pub fn reset_view(&mut self, cx: &mut Context<'_, Self>) {
        self.view = View::default();
        cx.notify();
    }

    /// Change how many levels are drawn, which is the other meaning of zoom in
    /// a treemap: seeing further in without changing what is on screen.
    pub fn adjust_depth(&mut self, step: i32, cx: &mut Context<'_, Self>) {
        let depth = (self.layout_options.max_depth.cast_signed() + step)
            .clamp(1, 6) as u32;
        self.layout_options.max_depth = depth;
        self.cache = None;
        cx.notify();
    }

    pub fn toggle_metric(&mut self, cx: &mut Context<'_, Self>) {
        self.options.metric = self.options.metric.toggled();
        if let Some(tree) = &self.tree {
            let mut tree = (**tree).clone();
            disktree_core::tree::aggregate(&mut tree, self.options.metric);
            self.tree = Some(Arc::new(tree));
            let metric = self.options.metric;
            self.marks.refresh(
                &self.root_path,
                self.tree.as_ref().unwrap(),
                metric,
            );
        }
        // Children are ordered by the metric, so every crumb moved.
        self.refresh_insights();
        self.clear_filter();
        self.cache = None;
        cx.notify();
    }

    /// The Size | Files | Age choice: what areas measure, and what colour
    /// says. Age keeps areas by size, since age has no area of its own.
    pub fn set_mode(&mut self, index: usize, cx: &mut Context<'_, Self>) {
        let (metric, color) = match index {
            1 => (Metric::Files, ColorMode::Kind),
            2 => (Metric::Bytes, ColorMode::Age),
            _ => (Metric::Bytes, ColorMode::Kind),
        };
        self.color_mode = color;
        if self.options.metric != metric {
            self.toggle_metric(cx);
        }
        cx.notify();
    }

    /// The Size | Files | Age choice currently on.
    pub const fn mode_index(&self) -> usize {
        match (self.color_mode, self.options.metric) {
            (ColorMode::Age, _) => 2,
            (ColorMode::Kind, Metric::Files) => 1,
            (ColorMode::Kind, Metric::Bytes) => 0,
        }
    }

    // ── removal ─────────────────────────────────────────────────────────

    /// The review screen's commit: move to the trash at once, or ask first for
    /// a permanent deletion, which cannot be undone.
    pub fn commit(&mut self, cx: &mut Context<'_, Self>) {
        if self.plan().is_empty() {
            return;
        }
        match self.removal_mode {
            RemovalMode::Trash => self.begin_removal(cx),
            RemovalMode::Permanent => {
                self.confirm_open = true;
                self.focus_request = Some(FocusTarget::Dialog);
                cx.notify();
            }
        }
    }

    /// The alert dialog's `Delete`.
    pub fn confirm_delete(&mut self, cx: &mut Context<'_, Self>) {
        self.confirm_open = false;
        self.focus_request = Some(FocusTarget::Root);
        self.begin_removal(cx);
    }

    /// The alert dialog's `Cancel`, or Escape.
    pub fn cancel_delete(&mut self, cx: &mut Context<'_, Self>) {
        self.confirm_open = false;
        self.focus_request = Some(FocusTarget::Root);
        cx.notify();
    }

    /// Move focus if something asked for it. Called wherever a window is in
    /// hand: the key listener and the click listeners.
    pub fn apply_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match self.focus_request.take() {
            Some(FocusTarget::Root) => window.focus(&self.focus, cx),
            Some(FocusTarget::Dialog) => window.focus(&self.confirm_focus, cx),
            None => {}
        }
    }

    /// `ctrl =`, `ctrl -` and `ctrl 0`: interface zoom. Changes the window's
    /// `rem`, which every size in this app is expressed in, so hierarchy and
    /// spacing keep their proportions at every step.
    pub fn zoom_interface(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
    ) -> bool {
        let keystroke = &event.keystroke;
        if !(keystroke.modifiers.control || keystroke.modifiers.platform) {
            return false;
        }
        let steps = crate::ui::ZOOM_STEPS;
        let current = window.rem_size().as_f32() / crate::ui::BASE_REM;
        let index = steps
            .iter()
            .position(|step| (step - current).abs() < 0.01)
            .unwrap_or(2);
        let next = match keystroke.key.as_str() {
            "=" | "+" => (index + 1).min(steps.len() - 1),
            "-" => index.saturating_sub(1),
            "0" => 2,
            _ => return false,
        };
        window.set_rem_size(px(crate::ui::BASE_REM * steps[next]));
        self.cache = None;
        true
    }

    pub fn begin_removal(&mut self, cx: &mut Context<'_, Self>) {
        let plan = self.plan();
        if plan.is_empty() {
            self.notice = Some(("nothing is marked".into(), Status::Warning));
            cx.notify();
            return;
        }
        self.space_baseline = self.space.or(self.space_baseline);
        self.run_epoch += 1;
        let epoch = self.run_epoch;
        self.run_summary = RunSummary {
            total: plan.targets.len(),
            ..RunSummary::default()
        };
        self.run_log.clear();
        self.screen = Screen::Running;
        self.run = Some(disktree_core::removal::spawn(plan, self.removal_mode));
        Self::poll_removal(epoch, cx);
        cx.notify();
    }

    fn poll_removal(epoch: u64, cx: &Context<'_, Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(60))
                    .await;
                let keep_going = this
                    .update(cx, |this, cx| this.poll_removal_once(epoch, cx))
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Drain the removal channel once. Returns whether the run should keep
    /// being polled. Driven by the poll task above, and by the tests, which
    /// cannot wait on the test clock while a real worker thread runs.
    pub(crate) fn poll_removal_once(
        &mut self,
        epoch: u64,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if epoch != self.run_epoch {
            return false;
        }
        let Some(run) = &self.run else {
            return false;
        };
        let mut finished = false;
        while let Some(event) = run.poll() {
            match event {
                RemovalEvent::Start { total } => self.run_summary.total = total,
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
            self.marks.clear();
            self.screen = Screen::Done;
            // The tree on screen is now wrong; start over rather than leaving
            // numbers that include what was just removed.
            self.start_scan(cx);
        }
        cx.notify();
        !finished
    }

    /// Descend to a path found by the search field.
    /// Recompute what the find text matches in the directory on screen.
    ///
    /// Off the UI thread: a whole disk is millions of names, a few hundred
    /// milliseconds to search, and typing must not wait for it. Each
    /// keystroke supersedes the search before it.
    pub fn refresh_matches(&mut self, cx: &Context<'_, Self>) {
        self.find_epoch += 1;
        let epoch = self.find_epoch;
        let needle = self.find.clone();
        let Some(tree) =
            self.tree.clone().filter(|_| !needle.trim().is_empty())
        else {
            self.matches = None;
            self.filter_applied = false;
            self.finding = false;
            self.filter_epoch += 1;
            return;
        };
        self.finding = true;
        let base = self.crumbs.clone();
        let task = cx.background_executor().spawn(async move {
            tree.resolve(&base)
                .and_then(|node| filter(node, &base, &needle))
        });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            let _ = this.update(cx, |this, cx| {
                if epoch != this.find_epoch {
                    return;
                }
                this.finding = false;
                this.matches = found.map(Arc::new);
                this.filter_epoch += 1;
                if std::mem::take(&mut this.apply_pending) {
                    this.apply_filter(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Enter in the find field: lay out only the matches, with the largest
    /// selected so Space marks it.
    pub fn apply_filter(&mut self, cx: &mut Context<'_, Self>) {
        self.find_open = false;
        if self.finding {
            self.apply_pending = true;
            return;
        }
        let Some(matches) = self.matches.clone() else {
            return;
        };
        if matches.count == 0 {
            self.notice = Some((
                format!("nothing here matches {}", matches.needle),
                Status::Warning,
            ));
            cx.notify();
            return;
        }
        self.filter_applied = true;
        self.filter_epoch += 1;
        self.view = View::IDENTITY;
        self.transition = None;
        self.forget_hover();
        self.pointer_active = false;
        let tree = self.tree.clone();
        self.selected = matches
            .keep
            .iter()
            .filter(|(_, keep)| **keep == Keep::Whole)
            .filter_map(|(crumbs, _)| {
                let node = tree.as_deref()?.resolve(crumbs)?;
                Some((node.bytes, crumbs))
            })
            .max_by_key(|(bytes, _)| *bytes)
            .map(|(_, crumbs)| crumbs.clone());
        self.notice = None;
        cx.notify();
    }

    /// Drop the find text and the filter with it.
    pub fn clear_filter(&mut self) {
        self.find_epoch += 1;
        self.finding = false;
        self.apply_pending = false;
        self.find.clear();
        self.find_open = false;
        self.matches = None;
        self.filter_applied = false;
        self.filter_epoch += 1;
    }

    // ── input ───────────────────────────────────────────────────────────

    /// Handle a key press.
    pub fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<'_, Self>,
    ) {
        self.dispatch_key(event, cx);
    }

    /// Whether a menu command may replace the tree: where `r` would, not
    /// behind the confirmation, and not under the review list or a removal
    /// that is still running.
    pub fn can_start_over(&self) -> bool {
        self.screen == Screen::Explore && !self.confirm_open
    }

    /// Ask for a directory and scan it: an app opened from the Dock has no
    /// command line to name one.
    pub fn open_folder(cx: &Context<'_, Self>) {
        let chosen = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Scan".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            // Canonical, like a root from the command line, so a later
            // widening recognises this tree in the wider walk.
            let path = path.canonicalize().unwrap_or(path);
            // The panel does not block the window: the review list or a
            // removal may have started while it was open.
            let _ = this.update(cx, |this, cx| {
                if this.can_start_over() {
                    this.set_root(path, cx);
                }
            });
        })
        .detach();
    }

    /// Save what the plan would act on as a list of paths, one per line,
    /// wherever the user chooses: for a script, or for later.
    pub fn save_delete_list(&mut self, cx: &mut Context<'_, Self>) {
        let plan = self.plan();
        if plan.is_empty() {
            self.notice = Some(("nothing to save".into(), Status::Warning));
            cx.notify();
            return;
        }
        let list = disktree_core::export::delete_list(&plan.targets);
        let count = plan.targets.len();
        let directory =
            self.home.clone().unwrap_or_else(|| self.root_path.clone());
        let chosen = cx
            .prompt_for_new_path(&directory, Some("disktree-delete-list.txt"));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            let notice = match std::fs::write(&path, list) {
                Ok(()) => (
                    format!(
                        "saved {count} {} to {}",
                        if count == 1 { "path" } else { "paths" },
                        path.display()
                    ),
                    Status::Success,
                ),
                Err(error) => (
                    format!("could not save {}: {error}", path.display()),
                    Status::Error,
                ),
            };
            let _ = this.update(cx, |this, cx| {
                this.notice = Some(notice);
                cx.notify();
            });
        })
        .detach();
    }

    /// Copy instructions for a coding agent to free the space by removing
    /// what the plan would act on, after checking each path.
    pub fn copy_agent_prompt(&mut self, cx: &mut Context<'_, Self>) {
        let plan = self.plan();
        if plan.is_empty() {
            self.notice = Some(("nothing to copy".into(), Status::Warning));
            cx.notify();
            return;
        }
        let prompt = disktree_core::export::agent_prompt(
            &plan.targets,
            &self.root_path,
            self.space,
        );
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(prompt));
        let count = plan.targets.len();
        self.notice = Some((
            format!(
                "copied a prompt for your agent: {count} {}, {}",
                if count == 1 { "path" } else { "paths" },
                disktree_core::size::human_bytes(plan.bytes())
            ),
            Status::Success,
        ));
        cx.notify();
    }

    /// Research the path shown beside the clicked button in the browser.
    pub fn search_folder(&self, path: &Path, cx: &Context<'_, Self>) {
        let platform = match std::env::consts::OS {
            "macos" => "macOS",
            "windows" => "Windows",
            "linux" => "Linux",
            other => other,
        };
        let path = crate::marks::display_path(path, self.home.as_deref());
        let query = format!(
            "{platform} \"{path}\" what is this folder, what application \
             does it belong to, and what are the risks of deleting it?"
        );
        let mut url = url::Url::parse("https://www.google.com/search")
            .expect("the Google search URL is valid");
        // Encode the whole query so punctuation in a path stays search text.
        url.query_pairs_mut().append_pair("q", &query);
        cx.open_url(url.as_str());
    }

    /// Show the tile a key acts on in Finder (or the file manager), selected.
    pub fn reveal_target(&mut self, cx: &mut Context<'_, Self>) {
        let crumbs =
            self.action_target().unwrap_or_else(|| self.crumbs.clone());
        let Some(path) = self.path_at(&crumbs) else {
            return;
        };
        // GPUI's reveal cannot report failure, so check what it can't.
        if std::fs::symlink_metadata(&path).is_err() {
            self.notice = Some((
                format!(
                    "{} is no longer on disk",
                    crate::marks::display_path(&path, self.home.as_deref())
                ),
                Status::Warning,
            ));
            cx.notify();
            return;
        }
        cx.reveal_path(&path);
    }

    /// Every binding, in reading order of the hint bar, so the keys and the
    /// documented list cannot drift apart.
    fn dispatch_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<'_, Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let control = event.keystroke.modifiers.control;
        let shift = event.keystroke.modifiers.shift;
        let alt = event.keystroke.modifiers.alt;

        // The alert dialog owns Enter and Escape while it is open; a key that
        // bubbles up to here must not also act on the screen behind it.
        if self.confirm_open {
            return;
        }

        // ⌘ chords belong to the menu bar (⌘Q, ⌘W, ⌘R) or to the system.
        // Read as plain letters they would act twice or by surprise: ⌘D
        // would re-scan with apparent sizes, ⌘H would hide *and* toggle.
        // Zoom (⌘= ⌘- ⌘0) is handled before this, with a window in hand.
        if event.keystroke.modifiers.platform {
            return;
        }

        if self.crumb_menu.is_some() && self.on_menu_key(key, cx) {
            return;
        }

        if self.show_help {
            if matches!(key, "escape" | "?" | "/" | "q") {
                self.show_help = false;
                cx.notify();
            }
            return;
        }

        // The search field takes the keyboard while it is open. It is a field
        // of three keys on purpose: no editor state to keep in sync with the
        // tree, and Escape always means "give the keyboard back".
        if self.screen == Screen::Explore && self.find_open {
            match key {
                "escape" => self.clear_filter(),
                "enter" => self.apply_filter(cx),
                "backspace" => {
                    self.find.pop();
                    self.refresh_matches(cx);
                }
                _ => {
                    // Some platforms report only `key` for a character and
                    // leave `key_char` empty; a one-character key is a
                    // character either way.
                    let typed = event
                        .keystroke
                        .key_char
                        .as_deref()
                        .unwrap_or(event.keystroke.key.as_str());
                    if !control && typed.chars().count() == 1 {
                        self.find.push_str(typed);
                        self.refresh_matches(cx);
                    }
                }
            }
            cx.notify();
            return;
        }

        match self.screen {
            // Alt-arrows are history in every browser and file manager.
            Screen::Explore if alt && key == "left" => self.go_back(cx),
            Screen::Explore if alt && key == "right" => self.go_forward(cx),
            Screen::Explore => self.on_explore_key(key, control, shift, cx),
            Screen::Review => self.on_review_key(key, cx),
            Screen::Running => {
                if key == "escape"
                    && let Some(run) = &self.run
                {
                    run.cancel();
                    self.notice = Some((
                        "stopping after the current item".into(),
                        Status::Warning,
                    ));
                    cx.notify();
                }
            }
            Screen::Done => {
                if matches!(key, "escape" | "enter") {
                    self.screen = Screen::Explore;
                    cx.notify();
                }
            }
        }
    }

    fn on_explore_key(
        &mut self,
        key: &str,
        control: bool,
        shift: bool,
        cx: &mut Context<'_, Self>,
    ) {
        // Keys that act on "the current tile" start from the pointer when it
        // moved last.
        if matches!(
            key,
            "space"
                | "x"
                | "enter"
                | "tab"
                | "left"
                | "right"
                | "up"
                | "down"
                | "h"
                | "j"
                | "k"
                | "l"
        ) {
            self.adopt_pointer_target();
        }
        match key {
            "/" | "s" if !control => {
                self.find_open = true;
                cx.notify();
            }
            "enter" => self.descend(cx),
            "right" | "l" if !control => {
                self.move_selection(Direction::Right, cx);
            }
            "left" | "h" if !control => {
                self.move_selection(Direction::Left, cx);
            }
            "up" | "k" if !control => self.move_selection(Direction::Up, cx),
            "down" | "j" if !control => {
                self.move_selection(Direction::Down, cx);
            }
            "backspace" | "u" if !control => self.ascend(cx),
            // A filter is the first thing Escape takes away.
            "escape" if self.matches.is_some() => self.clear_filter(),
            "escape" => {
                if self.selected.is_some() {
                    self.selected = None;
                } else {
                    self.ascend(cx);
                }
                cx.notify();
            }
            "space" => self.toggle_mark_selected(cx),
            "x" if !control => self.toggle_mark_selected(cx),
            "tab" => self.cycle_sibling(if shift { -1 } else { 1 }, cx),
            "c" if !control => {
                if self.marks.is_empty() {
                    self.notice = Some((
                        "mark something first: space marks the selected tile"
                            .into(),
                        Status::Warning,
                    ));
                } else {
                    self.screen = Screen::Review;
                }
                cx.notify();
            }
            "[" => self.adjust_depth(-1, cx),
            "]" => self.adjust_depth(1, cx),
            "-" => {
                self.zoom_at(
                    self.half_width(),
                    self.half_height(),
                    1.0 / 1.25,
                    false,
                    cx,
                );
            }
            "=" | "+" => {
                self.zoom_at(
                    self.half_width(),
                    self.half_height(),
                    1.25,
                    false,
                    cx,
                );
            }
            "0" => self.reset_view(cx),
            "t" if !control => {
                self.set_mode((self.mode_index() + 1) % 3, cx);
            }
            "r" if !control => self.start_scan(cx),
            "g" if !control => self.go_to_disk(cx),
            "i" if !control => {
                self.options.include_hidden = !self.options.include_hidden;
                self.start_scan(cx);
            }
            "d" if !control => {
                self.options.apparent_size = !self.options.apparent_size;
                self.start_scan(cx);
            }
            "p" if !control => {
                self.show_selection = !self.show_selection;
                cx.notify();
            }
            "o" if !control => self.reveal_target(cx),
            "?" => {
                self.show_help = true;
                cx.notify();
            }
            "q" if !control => cx.quit(),
            _ => {}
        }
    }

    fn on_review_key(&mut self, key: &str, cx: &mut Context<'_, Self>) {
        match key {
            "escape" => {
                self.screen = Screen::Explore;
                cx.notify();
            }
            "enter" => self.commit(cx),
            "!" => self.clear_marks(cx),
            "p" => {
                self.removal_mode = RemovalMode::Permanent;
                cx.notify();
            }
            "m" => {
                self.removal_mode = RemovalMode::Trash;
                cx.notify();
            }
            "s" => self.save_delete_list(cx),
            "a" => self.copy_agent_prompt(cx),
            "?" => {
                self.show_help = true;
                cx.notify();
            }
            _ => {}
        }
    }

    fn half_width(&self) -> f32 {
        self.treemap_size.get().width.as_f32() / 2.0
    }

    fn half_height(&self) -> f32 {
        self.treemap_size.get().height.as_f32() / 2.0
    }

    /// Mouse moved over the treemap.
    pub fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        cx: &mut Context<'_, Self>,
    ) {
        let origin = self.treemap_origin.get();
        let local = Point::new(
            event.position.x - origin.x,
            event.position.y - origin.y,
        );
        // Moves are delivered here even when the pointer is elsewhere in the
        // window. Outside the mosaic there is nothing to hover, and a stale
        // tooltip would cover whatever the pointer went to — the panel's
        // resize handle, for one.
        let area = self.treemap_size.get();
        let inside = local.x >= px(0.)
            && local.y >= px(0.)
            && local.x < area.width
            && local.y < area.height;
        if !inside {
            if self.pointer.is_some() || self.hovered.is_some() {
                self.on_mouse_leave(cx);
            }
            return;
        }
        self.pointer = Some(local);
        self.pointer_active = true;
        let previous = self.hovered.clone();
        self.hovered = self.tile_at(local.x.as_f32(), local.y.as_f32());
        // The cursor tooltip is positioned from `pointer`, so a move *within*
        // one tile still has to repaint — otherwise the tooltip sticks where
        // the tile was first entered until the hover target changes.
        if self.hovered != previous || self.hovered.is_some() {
            cx.notify();
        }
    }

    pub fn on_mouse_leave(&mut self, cx: &mut Context<'_, Self>) {
        self.pointer = None;
        self.pointer_active = false;
        if self.hovered.take().is_some() {
            cx.notify();
        }
    }

    /// Click: select, then act on a repeat click, like a file manager.
    pub fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        cx: &mut Context<'_, Self>,
    ) {
        let origin = self.treemap_origin.get();
        let x = (event.position.x - origin.x).as_f32();
        let y = (event.position.y - origin.y).as_f32();
        let crumbs = self.tile_at(x, y);

        match event.button {
            MouseButton::Left if event.click_count >= 2 => {
                if let Some(crumbs) = crumbs {
                    self.select(Some(crumbs), cx);
                    self.descend(cx);
                }
            }
            MouseButton::Left
                if event.modifiers.control || event.modifiers.platform =>
            {
                if let Some(crumbs) = crumbs {
                    self.toggle_mark(&crumbs, cx);
                }
            }
            MouseButton::Left => {
                let activate = crumbs.as_ref().is_some_and(|crumbs| {
                    self.selected.as_ref() == Some(crumbs)
                        && self.node_at(crumbs).is_some_and(Node::is_dir)
                });
                if activate {
                    // A second click on the selection opens it, like a file
                    // manager, without needing a double click.
                    self.descend(cx);
                    return;
                }
                self.select(crumbs, cx);
            }
            MouseButton::Middle => {
                if let Some(crumbs) = crumbs {
                    self.toggle_mark(&crumbs, cx);
                }
            }
            MouseButton::Navigate(NavigationDirection::Back) => {
                self.go_back(cx);
            }
            MouseButton::Navigate(NavigationDirection::Forward) => {
                self.go_forward(cx);
            }
            _ => {}
        }
    }

    pub fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        cx: &mut Context<'_, Self>,
    ) {
        let origin = self.treemap_origin.get();
        let x = (event.position.x - origin.x).as_f32();
        let y = (event.position.y - origin.y).as_f32();
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => delta.y.as_f32() / 24.0,
        };
        if lines.abs() < f32::EPSILON {
            return;
        }
        if event.modifiers.shift {
            // Pan instead of zoom, for looking around a magnified view.
            self.view.origin_y =
                (self.view.origin_y - lines * 40.0 / self.view.scale).max(0.0);
            cx.notify();
            return;
        }
        let factor = if lines > 0.0 { 1.15 } else { 1.0 / 1.15 };
        self.zoom_at(x, y, factor, true, cx);
    }

    /// Advance the layout transition, if one is running.
    pub fn tick_transition(&mut self, window: &Window) {
        if let Some(transition) = self.transition {
            let (_, running) = transition.sample(Rect::default());
            if running {
                window.request_animation_frame();
            } else {
                self.transition = None;
            }
        }
    }

    /// A tile's rectangle for this frame: mid-transition it is on its way from
    /// where it was to where it is.
    fn animated_rect(&self, rect: Rect) -> Rect {
        match self.transition {
            Some(transition) => transition.sample(rect).0,
            None => rect,
        }
    }
}

/// A direction for geometric selection movement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    const fn is_backwards(self) -> bool {
        matches!(self, Self::Left | Self::Up)
    }

    /// Distance from `from` to `rect` along the axis, if `rect` lies that way.
    fn gap(self, from: &Rect, rect: &Rect) -> Option<f32> {
        let epsilon = 0.5;
        match self {
            Self::Right => {
                let gap = rect.x - from.right();
                (gap >= -epsilon).then_some(gap.max(0.0))
            }
            Self::Left => {
                let gap = from.x - rect.right();
                (gap >= -epsilon).then_some(gap.max(0.0))
            }
            Self::Down => {
                let gap = rect.y - from.bottom();
                (gap >= -epsilon).then_some(gap.max(0.0))
            }
            Self::Up => {
                let gap = from.y - rect.bottom();
                (gap >= -epsilon).then_some(gap.max(0.0))
            }
        }
    }

    /// How far off the direction's axis `rect` sits, so the nearest tile in the
    /// direction wins rather than any tile in that half-plane.
    fn offset(self, from: &Rect, rect: &Rect) -> f32 {
        let overlap =
            |a_start: f32, a_end: f32, b_start: f32, b_end: f32| -> f32 {
                (a_end.min(b_end) - a_start.max(b_start)).max(0.0)
            };
        match self {
            Self::Left | Self::Right => {
                let shared =
                    overlap(from.y, from.bottom(), rect.y, rect.bottom());
                (from.h.min(rect.h) - shared).max(0.0)
            }
            Self::Up | Self::Down => {
                let shared =
                    overlap(from.x, from.right(), rect.x, rect.right());
                (from.w.min(rect.w) - shared).max(0.0)
            }
        }
    }
}

/// A tile's label, resolved for painting.
#[derive(Clone, Debug)]
pub struct Label {
    pub text: String,
    /// Base-space rectangle; the view transform is applied while painting.
    pub rect: Rect,
    /// The band this label belongs in, when its tile reserved one. A parent's
    /// name goes in its own band, never over its children.
    pub header: Option<Rect>,
    /// Nesting depth in this view: the first level is set in bold.
    pub depth: u32,
    /// Filtered out while typing: drawn quietly.
    pub dim: bool,
    pub marked: bool,
    pub size_text: String,
}

/// How many labels one frame will shape.
const MAX_LABELS: usize = 150;

/// Height of a top-level directory's name band, in rem.
const HEADER_REMS: f32 = 1.375;

/// Height of the slim label row a deeper open directory keeps, in rem.
const HEADER_INNER_REMS: f32 = 1.0;

/// A trail step's label: the directory's own name, or `/` for the root.
fn crumb_label(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Now, in Unix seconds.
pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Smallest tile, in rem, that gets a label: below this a name cannot be read.
const LABEL_MIN_W_REMS: f32 = 3.375;
const LABEL_MIN_H_REMS: f32 = 0.9375;

/// How a tile stands against the find text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filtered {
    /// It matches, is inside a match, or nothing is being found.
    Shown,
    /// It holds matches: its fill steps back, its name stays readable.
    Holds,
    /// Nothing in it matches.
    Out,
}

/// Where keyboard focus should go once a window is available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusTarget {
    /// The treemap and screens, which own every key binding.
    Root,
    /// The permanent-deletion alert dialog.
    Dialog,
}

impl Render for Disktree {
    fn render(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl gpui_kit::IntoElement {
        self.rem = window.rem_size().as_f32();
        self.tick_transition(window);
        // The titlebar names the directory on screen, however it got there:
        // a key, a click, a rescan or a folder chosen from the menu.
        let title = format!(
            "disktree · {}",
            crate::marks::display_path(
                &self.current_path(),
                self.home.as_deref()
            )
        );
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
        crate::views::root(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> View {
        View {
            scale: 2.0,
            origin_x: 100.0,
            origin_y: 50.0,
        }
    }

    #[test]
    fn projecting_and_unprojecting_are_inverses() {
        let view = view();
        let (x, y) = view.unproject(400.0, 300.0);
        let rect = Rect::new(x, y, 10.0, 10.0);
        let screen = view.project(rect);
        assert!((screen.x - 400.0).abs() < 0.001);
        assert!((screen.y - 300.0).abs() < 0.001);
        assert!((screen.w - 20.0).abs() < 0.001);
    }

    #[test]
    fn zooming_keeps_the_point_under_the_cursor_still() {
        let view = View::IDENTITY;
        let (before_x, before_y) = view.unproject(300.0, 200.0);
        let zoomed = view.zoomed_at(300.0, 200.0, 2.0, View::MAX_SCALE);
        let rect = Rect::new(before_x, before_y, 1.0, 1.0);
        let screen = zoomed.project(rect);
        assert!((screen.x - 300.0).abs() < 0.01, "{screen:?}");
        assert!((screen.y - 200.0).abs() < 0.01, "{screen:?}");
    }

    #[test]
    fn clamping_never_leaves_a_margin() {
        let area = size(px(800.), px(600.));
        let clamped = View {
            scale: 2.0,
            origin_x: -500.0,
            origin_y: 900.0,
        }
        .clamped(area);
        assert!(clamped.origin_x.abs() < f32::EPSILON);
        assert!((clamped.origin_y - 300.0).abs() < f32::EPSILON);

        let identity = View {
            scale: 1.0,
            origin_x: 40.0,
            origin_y: 40.0,
        }
        .clamped(area);
        assert!(identity.origin_x.abs() < f32::EPSILON);
        assert!(identity.origin_y.abs() < f32::EPSILON);
    }

    #[test]
    fn a_transition_starts_where_the_region_was_and_ends_where_it_is() {
        // A region that occupied the whole viewport, landing in the top-left
        // quarter of the new layout: everything inside it must start twice as
        // large and centred where it was.
        let transition = LayoutTransition::new(
            Rect::new(0.0, 0.0, 800.0, 600.0),
            Rect::new(0.0, 0.0, 400.0, 300.0),
        );
        // A tile inside the destination, at the far corner.
        let tile = Rect::new(200.0, 150.0, 200.0, 150.0);
        let (_, running) = transition.sample(tile);
        assert!(running, "the transition is in flight");
        let origin = transition.origin_of(tile);
        assert!((origin.x - 400.0).abs() < 0.001, "{origin:?}");
        assert!((origin.y - 300.0).abs() < 0.001, "{origin:?}");
        assert!((origin.w - 400.0).abs() < 0.001, "{origin:?}");
        assert!((origin.h - 300.0).abs() < 0.001, "{origin:?}");

        let finished = LayoutTransition {
            started: Instant::now()
                .checked_sub(Duration::from_millis(500))
                .expect("a moment on a running machine"),
            ..transition
        };
        let (end, running) = finished.sample(tile);
        assert!(!running);
        assert_eq!(end, tile, "it ends exactly at the real layout");
    }

    #[test]
    fn a_transition_grows_a_tile_that_is_being_entered() {
        // Entering a child: the child's region becomes the viewport, so
        // everything inside it gets bigger, never smaller.
        let transition = LayoutTransition::new(
            Rect::new(100.0, 100.0, 200.0, 200.0),
            Rect::new(0.0, 0.0, 800.0, 600.0),
        );
        let tile = Rect::new(400.0, 300.0, 100.0, 100.0);
        let origin = transition.origin_of(tile);
        assert!(origin.w < tile.w, "it starts smaller: {origin:?}");
        assert!(
            origin.x > 100.0 && origin.x < 300.0,
            "and where the child was: {origin:?}"
        );
    }

    #[test]
    fn directions_only_see_gaps_on_their_own_side() {
        let from = Rect::new(100.0, 100.0, 50.0, 50.0);
        let right = Rect::new(200.0, 100.0, 50.0, 50.0);
        let left = Rect::new(0.0, 100.0, 50.0, 50.0);
        assert!(Direction::Right.gap(&from, &right).is_some());
        assert!(Direction::Right.gap(&from, &left).is_none());
        assert!(Direction::Left.gap(&from, &left).is_some());
        assert!(Direction::Down.gap(&from, &right).is_none());
        assert_eq!(Direction::Right.gap(&from, &right), Some(50.0));
        // Aligned neighbours have no perpendicular offset; stacked ones do.
        assert!(Direction::Right.offset(&from, &right).abs() < f32::EPSILON);
        assert!(
            Direction::Right
                .offset(&from, &Rect::new(200.0, 160.0, 20.0, 20.0))
                > 0.0
        );
    }
}
