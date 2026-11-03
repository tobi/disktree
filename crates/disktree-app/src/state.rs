//! Application state and every mutation the UI can perform.
//!
//! The screens in [`crate::views`] are pure functions of this state; all the
//! decisions — what is selected, what a mark means, what a key does, when to
//! re-scan — live here so they can be reasoned about in one place.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use disktree_core::access::file_table_readable;
use disktree_core::filter::{Keep, Matches, filter_with, find_ignoring_case};
use disktree_core::insights::{Candidate, worth_a_look};
use disktree_core::path_style::PathStyle;
use disktree_core::removal::{
    Plan, RemovalEvent, RemovalHandle, RemovalMode, Target, TrashBackend,
    detect_trash_backend, plan,
};
use disktree_core::scan::{Known, ScanHandle, ScanOptions, ScanSnapshot};
use disktree_core::space::{
    SpaceInfo, device_for, space_info, volume_root_for,
};
use disktree_core::tree::{Metric, Node, Tree, path_of};
use disktree_core::treemap::{
    LayoutOptions, Rect, Tile, TileKind, hit, layout_filtered,
};
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, NavigationDirection, Pixels, Point, Render,
    ScrollDelta, ScrollWheelEvent, Size, Window, px, size,
};
use gpui_omarchy::{ActiveTheme as _, Status};
use rustc_hash::FxHashSet;

use crate::git_store::{self, Ask, GitProbe, GitStore, Wanted};

use crate::marks::{Marks, is_hidden};
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

/// The menu bar's Undo, as each system spells it; `u` works everywhere.
pub const UNDO_KEYS: &str = if cfg!(target_os = "macos") {
    "\u{2318}Z"
} else {
    "Ctrl+Z"
};

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

/// Scan-derived display estimates; never cached removal authorization.
#[derive(Default)]
struct AccountingCache {
    tree: Weak<Tree>,
    root: PathBuf,
    // Total and scanned-volume subsets only: bounded by the current marks.
    estimates: Vec<(Vec<PathBuf>, disktree_core::sharing::Estimate)>,
}

/// Everything the app knows and everything it can do.
pub struct Disktree {
    /// The directory the tree was scanned from.
    pub root_path: PathBuf,
    pub remote: Option<String>,
    remote_disk: bool,
    pub remote_scan: Option<disktree_core::remote::RemoteScan>,
    pub remote_progress: disktree_core::remote_progress::Progress,
    pub scan_preview: Option<Arc<disktree_core::scan_preview::Preview>>,
    preview_updated: Option<Instant>,
    remote_platform: String,
    pub remote_identity: Option<disktree_core::identity::HostIdentity>,
    pub home: Option<PathBuf>,
    pub options: ScanOptions,
    pub power_choice: Option<crate::power::PowerEfficiency>,
    pub power_settings_path: Option<PathBuf>,
    pub removal_settings_dir: Option<PathBuf>,
    /// The Power Efficiency menu, when open: the row the arrow keys are on,
    /// as an index into [`crate::power::PowerEfficiency::ALL`].
    pub power_menu: Option<usize>,
    /// The pointer is on the menu's button. A press there is the button's
    /// to toggle, not a press outside the menu that closes it first.
    pub power_hover: bool,
    /// The ⋮ menu, when open: the row the arrow keys are on, as an index
    /// into [`crate::toolbar::BarFit::more_items`].
    pub more_menu: Option<usize>,
    /// The pointer is on ⋮, as [`Self::power_hover`] is for its button.
    pub more_hover: bool,
    /// How much of the explore bar fit when it was last drawn, which says
    /// what ⋮ holds.
    pub bar_fit: crate::toolbar::BarFit,
    pub cpu_threads: usize,
    pub tree: Option<Arc<Tree>>,
    pub scan: Option<ScanHandle>,
    pub scan_epoch: u64,
    pub progress: ScanSnapshot,
    /// Sampled progress is announced at most once every two seconds. Errors
    /// and phase changes bypass this cache; sampled paths are never announced.
    pub accessible_progress: Option<(Instant, String)>,
    pub scan_error: Option<String>,

    pub screen: Screen,
    /// Path from the scanned root to the directory currently drawn.
    pub crumbs: Vec<usize>,
    pub selected: Option<Vec<usize>>,
    /// A bounded, semantic alternative to the painted mosaic.
    pub file_list: bool,
    pub file_page: usize,
    pub file_scroll: gpui_kit::ScrollHandle,
    pub file_focus: FocusHandle,
    restore_file_focus: bool,
    pub review_page: usize,
    pub review_scroll: gpui_kit::ScrollHandle,
    pub hovered: Option<Vec<usize>>,
    pub history: History,
    /// The history button under the pointer, `true` for `<`: its card is
    /// drawn below it for as long as it is.
    pub history_hover: Option<bool>,
    /// The pointer moved more recently than the keyboard navigated. Then the
    /// tile under the pointer is what Space, X and Enter act on; after an
    /// arrow or n it is the keyboard selection again.
    pub pointer_active: bool,
    /// A press of the click sequence under way (a double or triple click)
    /// already opened a level. Its later presses land on the level it
    /// opened, so they are ignored rather than opening another.
    click_opened: bool,
    pub view: View,
    /// The in-flight layout transition, if a level was just entered or left.
    pub transition: Option<LayoutTransition>,
    pub layout_options: LayoutOptions,
    cache: Option<LayoutCache>,
    accounting: RefCell<AccountingCache>,

    /// Mouse position in treemap-local pixels, for hit-testing and the tooltip.
    pub pointer: Option<Point<Pixels>>,
    /// The treemap area in window space, recorded while painting.
    pub treemap_origin: Rc<Cell<Point<Pixels>>>,
    pub treemap_size: Rc<Cell<Size<Pixels>>>,

    pub marks: Marks,
    /// Paths removed since the scan. They stay in the tree, drawn as
    /// removed, until the next scan, and cannot be marked again.
    pub removed: Marks,
    /// What Unmark all took, so one key or ⌘Z can put it back. Dropped
    /// wherever the marks stop meaning this machine's paths.
    pub unmarked: Option<Vec<Target>>,
    /// The review footer's Export menu is open.
    pub export_menu_open: bool,
    /// Whether it was open when the press that may close it began, so
    /// pressing its button toggles instead of reopening.
    pub export_menu_was_open: bool,
    pub removal_mode: RemovalMode,
    pub trash_backend: TrashBackend,
    /// The permanent-deletion alert dialog is open. Trash needs no dialog: it
    /// is reversible, so it commits directly.
    pub confirm_open: bool,
    /// Focus owner for the alert dialog while it is open.
    pub confirm_focus: FocusHandle,
    pub overlay_return: Option<FocusHandle>,
    pub app_menu_index: usize,
    pub menu_scroll: gpui_kit::ScrollHandle,
    pub help_scroll: gpui_kit::ScrollHandle,
    pub source_roots_focus: FocusHandle,
    /// Focus to move on the next occasion a window is in hand. Key handling
    /// has no window, and opening or closing the dialog must move focus.
    pub focus_request: Option<FocusTarget>,
    /// What the titlebar says, so it is only set when it changes. The strip
    /// paints no title, but the Window menu, Mission Control, the taskbar
    /// and Waybar still read it.
    window_title: String,
    /// The left button went down on the title strip and has not moved yet:
    /// the first motion starts a window move.
    pub strip_drag: bool,
    /// A window frame to draw instead of asking the platform. The
    /// window-harness tests pin one per layout: their window has no
    /// compositor to ask.
    pub chrome_pin: Option<crate::chrome::Frame>,
    /// How far the content sits inside the window, from this frame's
    /// [`crate::chrome::Frame`]: what turns a pointer position into one in
    /// the root's coordinates.
    pub content_edges: gpui_kit::Edges<Pixels>,
    /// The window's `rem` in pixels, read each frame. The mosaic is laid out
    /// in pixels, so its header band and label thresholds are scaled by this
    /// to follow interface zoom like the rest of the interface.
    pub rem: f32,
    pub run: Option<RemovalHandle>,
    /// Stop was asked for: the removal ends after the current item.
    pub stop_requested: bool,
    pub run_epoch: u64,
    pub run_summary: RunSummary,
    pub run_log: Vec<(PathBuf, Result<(), String>)>,
    pub space: Option<SpaceInfo>,
    /// Free space before the removal, for the honest "what did it actually
    /// free" number rather than the sum of what was marked.
    pub space_baseline: Option<SpaceInfo>,
    /// Rescan once the running removal finishes: Remove & rescan.
    pub rescan_after_removal: bool,
    /// What each removal since the last scan freed.
    pub ledger: crate::ledger::Ledger,
    /// Scans that finished and replaced the tree. What the ledger is
    /// counted against: `scan_epoch` also moves when a scan starts or is
    /// cancelled, and a cancelled widening keeps the tree it had.
    pub scans_done: u64,
    /// The running removal as it started.
    run_start: Option<crate::ledger::Start>,
    pub notice: Option<(String, Status)>,
    /// Toasts over the lower left of the screen's content: the updater's
    /// answers, which run out.
    pub toasts: crate::toasts::Toasts,
    /// Wakes when the next toast runs out.
    pub toast_timer: Option<gpui_kit::Task<()>>,
    /// What the updater has said as of its last change: the live card, the
    /// ready release and whether it was put away.
    pub update_watch: crate::toasts::UpdateWatch,

    pub find: String,
    pub find_open: bool,
    /// What the find text matches in the directory on screen, recomputed
    /// on every keystroke. While typing it only dims what does not match.
    pub matches: Option<Arc<Matches>>,
    /// Enter was pressed: the mosaic lays out only the matches.
    pub filter_applied: bool,
    /// Bumped whenever what the mosaic lays out may have changed: a search
    /// or the legend.
    pub filter_epoch: u64,
    /// The legend's chips: what kinds, ages and reclaimable space to show.
    pub legend: crate::legend::LegendFilter,
    /// What the legend keeps, from the scanned root down. Laid out in
    /// place of the tree whenever no search is applied.
    pub legend_matches: Option<Arc<Matches>>,
    /// Bumped per legend change; only the newest result is kept.
    pub legend_epoch: u64,
    /// The legend's filter is being worked out off the UI thread.
    pub legend_pending: bool,
    /// Bumped per keystroke; only the newest search's result is kept.
    find_epoch: u64,
    /// A search is running off the UI thread.
    pub finding: bool,
    /// Enter was pressed before the search finished: apply on arrival.
    apply_pending: bool,
    pub show_help: bool,
    /// The "work with an AI agent" overlay is open.
    pub show_agent: bool,
    /// Off macOS, the strip's menu: the menu bar's template as one
    /// dropdown (`app_menu::menus`), since there is no global menu bar.
    pub app_menu_open: bool,
    pub show_selection: bool,
    pub focus: FocusHandle,
    /// The app is quitting: what this window holds is left to the system,
    /// which takes a process's memory back whole when it exits.
    quitting: bool,
    _on_quit: gpui_kit::Subscription,
    /// A trail crumb's sibling menu, when open.
    pub crumb_menu: Option<CrumbMenu>,

    pub color_mode: ColorMode,
    /// The largest things worth clearing, recomputed when a scan lands.
    pub insights: Vec<Candidate>,
    /// What git knows about the checkouts looked at, and which to ask
    /// next: see `git_store`.
    pub git: GitStore,
    /// Runs one probe, off the UI thread. Tests count calls with a fake.
    pub git_probe: GitProbe,
    /// Draws again when a dwell, a token or a stale answer comes due, so
    /// a pointer resting still is still answered.
    git_wake: Option<(Instant, gpui_kit::Task<()>)>,
    /// When the pointer came to the tile it is on: a hover asks git only
    /// once it has stayed `git_store::DWELL`.
    hover_since: Option<Instant>,
    /// Whether the directory the panel describes is a checkout, by path.
    /// Finding out is a `stat`, and the panel draws on every frame, so it
    /// is asked again only when the selection or the tree changes.
    checkout: Option<(PathBuf, bool)>,
    /// The device the scanned volume is mounted from.
    pub device: Option<String>,
    /// Whether macOS lets this process read everything: asked once, since a
    /// grant only takes effect after a relaunch. `None` off macOS.
    pub full_disk_access: Option<bool>,
    /// Whether disktree runs as an administrator, which lets a whole drive
    /// be read from its file table instead of walked. `None` off Windows.
    pub administrator: Option<bool>,
    /// Whether the root is a whole NTFS drive, which an administrator reads
    /// from its file table. Follows the root; `false` off Windows.
    pub file_table: bool,
    /// A restart as administrator was asked for and not yet declined, so a
    /// second click does not raise a second prompt.
    pub restarting: bool,
    /// The top of the disk the scanned root lives on: what "Whole disk"
    /// scans. Follows the root when a folder is opened.
    pub disk_root: Option<PathBuf>,
    /// The volume picker: open, what it listed, and which row the keys are
    /// on. `None` when closed; empty when no volume besides the current one
    /// could be read.
    pub volumes_open: bool,
    pub volumes_loading: bool,
    pub awaiting_root: bool,
    pub volumes_source: Option<String>,
    /// The chooser's source switch is on SSH host: the host field shows,
    /// and nothing is listed until Connect.
    pub picker_ssh: bool,
    pub volumes_error: Option<String>,
    pub picker_error: Option<String>,
    pub picker_host: Option<Entity<InputState>>,
    pub picker_path: Option<Entity<InputState>>,
    roots_job: Option<disktree_core::remote::RemoteRoots>,
    volume_epoch: u64,
    pub volumes: Vec<disktree_core::space::Volume>,
    pub volume_highlight: usize,
    /// The side panel's width, in rem; dragged from its left edge.
    pub panel_rems: f32,
    pub scan_started: Option<Instant>,
    /// Where the scan in flight is rooted. Differs from `root_path` while
    /// widening, when the old tree stays on screen until the wider one lands.
    pub scan_root: PathBuf,
    pub scan_elapsed: Option<Duration>,
    /// Unix seconds when the tree landed: the "now" ages are measured from.
    pub scanned_at: i64,
    /// The agent link, once a listener is attached; see [`crate::agent`].
    pub agent: Option<crate::agent::AgentLink>,
}

impl Disktree {
    #[cfg(test)]
    pub fn new(
        root_path: PathBuf,
        options: ScanOptions,
        depth: u32,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        Self::open(root_path, options, depth, None, false, cx)
    }

    pub fn open(
        root_path: PathBuf,
        options: ScanOptions,
        depth: u32,
        remote: Option<String>,
        remote_disk: bool,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        Self::build(root_path, options, depth, remote, remote_disk, true, cx)
    }

    /// Startup without a scan: discovering roots is not consent to walk one.
    pub fn choose(
        options: ScanOptions,
        depth: u32,
        remote: Option<String>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        Self::build(PathBuf::new(), options, depth, remote, false, false, cx)
    }

    fn build(
        root_path: PathBuf,
        options: ScanOptions,
        depth: u32,
        remote: Option<String>,
        remote_disk: bool,
        start: bool,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let local = remote.is_none();
        let home = if local { std::env::home_dir() } else { None };
        let space = if local && start {
            space_info(&root_path).ok()
        } else {
            None
        };
        let trash_backend = detect_trash_backend();
        let mut tree = Self {
            root_path,
            remote,
            remote_disk,
            remote_scan: None,
            scan_preview: None,
            preview_updated: None,
            remote_progress: disktree_core::remote_progress::Progress::default(
            ),
            remote_platform: String::new(),
            remote_identity: None,
            home,
            options,
            power_choice: None,
            power_settings_path: crate::power::settings_path(),
            removal_settings_dir: crate::updater::settings_dir(),
            power_menu: None,
            power_hover: false,
            more_menu: None,
            more_hover: false,
            bar_fit: crate::toolbar::BarFit::default(),
            cpu_threads: crate::power::cpu_threads(),
            tree: None,
            scan: None,
            scan_epoch: 0,
            progress: ScanSnapshot::default(),
            accessible_progress: None,
            scan_error: None,
            screen: Screen::Explore,
            crumbs: Vec::new(),
            selected: None,
            hovered: None,
            history: History::default(),
            history_hover: None,
            pointer_active: false,
            click_opened: false,
            view: View::default(),
            transition: None,
            file_list: false,
            file_page: 0,
            file_scroll: gpui_kit::ScrollHandle::new(),
            // Explicit handles own tab metadata; element styles do not override it.
            file_focus: cx.focus_handle().tab_stop(true),
            restore_file_focus: false,
            review_page: 0,
            review_scroll: gpui_kit::ScrollHandle::new(),
            layout_options: LayoutOptions {
                max_depth: depth.clamp(1, 6),
                ..LayoutOptions::default()
            },
            cache: None,
            accounting: RefCell::new(AccountingCache::default()),
            pointer: None,
            treemap_origin: Rc::new(Cell::new(Point::new(px(0.), px(0.)))),
            treemap_size: Rc::new(Cell::new(size(px(0.), px(0.)))),
            marks: Marks::default(),
            removed: Marks::default(),
            unmarked: None,
            export_menu_open: false,
            export_menu_was_open: false,
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
            overlay_return: None,
            app_menu_index: 0,
            menu_scroll: gpui_kit::ScrollHandle::new(),
            help_scroll: gpui_kit::ScrollHandle::new(),
            source_roots_focus: cx.focus_handle().tab_stop(true),
            focus_request: None,
            window_title: String::new(),
            strip_drag: false,
            chrome_pin: None,
            content_edges: gpui_kit::Edges::default(),
            rem: crate::ui::BASE_REM,
            run: None,
            stop_requested: false,
            run_epoch: 0,
            run_summary: RunSummary::default(),
            run_log: Vec::new(),
            space,
            space_baseline: None,
            rescan_after_removal: false,
            ledger: crate::ledger::Ledger::default(),
            scans_done: 0,
            run_start: None,
            notice: None,
            toasts: crate::toasts::Toasts::default(),
            toast_timer: None,
            update_watch: crate::toasts::UpdateWatch::default(),
            find: String::new(),
            find_open: false,
            matches: None,
            filter_applied: false,
            filter_epoch: 0,
            legend: crate::legend::LegendFilter::default(),
            legend_matches: None,
            legend_epoch: 0,
            legend_pending: false,
            find_epoch: 0,
            finding: false,
            apply_pending: false,
            show_help: false,
            show_agent: false,
            app_menu_open: false,
            show_selection: true,
            focus: cx.focus_handle(),
            crumb_menu: None,
            color_mode: ColorMode::Kind,
            insights: Vec::new(),
            git: GitStore::new(cx.background_executor().now()),
            git_probe: Arc::new(|path, cancel| {
                let limit = crate::git::Limit::new(
                    git_store::PROBE_LIMIT,
                    cancel.clone(),
                );
                crate::git::probe(path, &limit)
            }),
            git_wake: None,
            hover_since: None,
            checkout: None,
            device: None,
            full_disk_access: None,
            administrator: disktree_core::access::administrator(),
            file_table: false,
            restarting: false,
            disk_root: None,
            volumes_open: false,
            volumes_loading: false,
            awaiting_root: !start,
            volumes_source: None,
            picker_ssh: false,
            volumes_error: None,
            picker_error: None,
            picker_host: None,
            picker_path: None,
            roots_job: None,
            volume_epoch: 0,
            volumes: Vec::new(),
            volume_highlight: 0,
            panel_rems: PANEL_REMS,
            scan_started: None,
            scan_root: PathBuf::new(),
            scan_elapsed: None,
            scanned_at: now_seconds(),
            agent: None,
            quitting: false,
            // GPUI runs quit hooks before it drops the windows, so the flag
            // is set by the time `Drop` reads it.
            _on_quit: cx.on_app_quit(|this, _| {
                this.quitting = true;
                std::future::ready(())
            }),
        };
        if local && start {
            tree.device = device_for(&tree.root_path);
            tree.full_disk_access = tree
                .home
                .as_deref()
                .and_then(disktree_core::access::full_disk_access);
            // The disk of what is on screen, as `set_root` keeps it: `disktree
            // /Volumes/Ext` then `g` measures that drive, like opening it with ⌘O.
            tree.disk_root = volume_root_for(&tree.root_path);
            tree.file_table = file_table_readable(&tree.root_path);
            disktree_core::removal::prime_mount_points();
        }
        if start {
            tree.start_scan(cx);
        } else {
            tree.open_volumes(cx);
        }
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
        tree: Tree,
        options: ScanOptions,
        depth: u32,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let mut app = Self::new(root_path, options, depth, cx);
        // `new` started a walk; this tree stands in for its result.
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.scan_epoch += 1;
        app.marks.refresh(
            &app.root_path,
            tree.root(),
            app.options.metric,
            app.path_style(),
        );
        app.tree = Some(Arc::new(tree));
        app.cache = None;
        app.refresh_insights();
        app.refresh_legend(cx);
        app.select_largest(cx);
        app
    }

    /// Scan a different root from scratch. Marks are kept; a mark outside
    /// the new root is shown as kept back, never removed.
    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<'_, Self>) {
        // Accepting a new source removes its originating chooser control.
        self.overlay_return = None;
        if self.remote.is_some() {
            self.root_path = root;
            self.remote_disk = false;
            self.start_scan(cx);
            return;
        }
        self.space = space_info(&root).ok();
        self.device = device_for(&root);
        // "Whole disk" means the disk of what is on screen, which a new root
        // can change: an external drive has its own.
        self.disk_root =
            volume_root_for(&root).or_else(|| self.disk_root.take());
        self.file_table = file_table_readable(&root);
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
        if self.remote.is_some() {
            self.set_root(above, cx);
            return;
        }
        let Some(tree) = self.tree.clone() else {
            self.set_root(above, cx);
            return;
        };
        if !self.root_path.starts_with(&above) || self.root_path == above {
            return;
        }
        // The worker's root changes immediately, even within a sampled report.
        self.accessible_progress = None;
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

    /// `V`: list every volume and let the scan move to one. The in-flight walk
    /// is left alone until a volume is picked; picking calls [`set_root`],
    /// which cancels it.
    pub fn open_volumes(&mut self, cx: &mut Context<'_, Self>) {
        if self.run.is_some() || self.screen != Screen::Explore {
            return;
        }
        self.volumes_open = true;
        self.picker_host = None;
        self.picker_path = None;
        self.picker_ssh = self.remote.is_some();
        self.focus_request = Some(FocusTarget::Dialog);
        self.load_volume_source(self.remote.clone(), cx);
    }

    pub fn load_volume_source(
        &mut self,
        host: Option<String>,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.volumes_open {
            return;
        }
        self.volume_epoch += 1;
        let epoch = self.volume_epoch;
        self.roots_job = None;
        self.volumes_source.clone_from(&host);
        self.volumes.clear();
        self.volume_highlight = 0;
        self.volumes_error = None;
        self.picker_error = None;
        self.volumes_loading = true;
        if let Some(host) = host {
            match disktree_core::remote::RemoteRoots::discover(&host) {
                Ok(job) => {
                    self.roots_job = Some(job);
                    cx.spawn(async move |this, cx| {
                        loop {
                            cx.background_executor()
                                .timer(Duration::from_millis(110))
                                .await;
                            let keep = this
                                .update(cx, |this, cx| {
                                    this.poll_roots_once(epoch, cx)
                                })
                                .unwrap_or(false);
                            if !keep {
                                break;
                            }
                        }
                    })
                    .detach();
                }
                Err(error) => self.finish_volume_listing(Err(error), cx),
            }
        } else {
            let task = cx.background_executor().spawn(async {
                disktree_core::space::volume_roots()
                    .map(|volumes| (volumes, std::env::home_dir()))
            });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    if this.volumes_open && this.volume_epoch == epoch {
                        this.finish_volume_listing(result, cx);
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    pub(crate) fn poll_roots_once(
        &mut self,
        epoch: u64,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if !self.volumes_open || self.volume_epoch != epoch {
            return false;
        }
        let Some(result) = self
            .roots_job
            .as_ref()
            .and_then(disktree_core::remote::RemoteJob::poll)
        else {
            return self.roots_job.is_some();
        };
        self.roots_job = None;
        self.finish_volume_listing(
            result.map(|roots| (roots.volumes, roots.home)),
            cx,
        );
        false
    }

    pub(crate) fn finish_volume_listing(
        &mut self,
        result: std::io::Result<(
            Vec<disktree_core::space::Volume>,
            Option<PathBuf>,
        )>,
        cx: &mut Context<'_, Self>,
    ) {
        self.volumes_loading = false;
        // A path typed while the list loaded keeps its row selected.
        let typed = self
            .picker_path
            .as_ref()
            .is_some_and(|input| !input.read(cx).value().is_empty());
        match result {
            Ok((mut volumes, home)) => {
                if let Some(home) = home
                    && !volumes.iter().any(|volume| volume.point == home)
                {
                    volumes.insert(
                        0,
                        disktree_core::space::Volume {
                            point: home,
                            device: Some("Home directory".into()),
                            space: None,
                        },
                    );
                }
                self.volumes = volumes;
                if typed {
                    self.volume_highlight = self.volumes.len();
                }
            }
            Err(error) => self.volumes_error = Some(error.to_string()),
        }
        cx.notify();
    }

    pub fn prepare_picker_inputs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.picker_host.is_none() {
            let value = self.volumes_source.clone().unwrap_or_default();
            let host = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("SSH alias or user@host")
                    .default_value(value)
            });
            cx.subscribe(&host, |this, _, event, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.connect_picker(cx);
                }
                cx.notify();
            })
            .detach();
            self.picker_host = Some(host);
        }
        if self.picker_path.is_none() {
            let path = cx.new(|cx| {
                InputState::new(window, cx).placeholder("/absolute/path")
            });
            cx.subscribe(&path, |this, _, event, cx| match event {
                InputEvent::PressEnter { .. } => this.choose_picker_path(cx),
                // Typing or clicking in the Directory row selects it, so
                // Scan and Enter act on what was just typed.
                InputEvent::Change | InputEvent::Focus => {
                    this.volume_highlight = this.volumes.len();
                    cx.notify();
                }
                InputEvent::Blur => {}
            })
            .detach();
            self.picker_path = Some(path);
        }
    }

    pub fn picker_source_matches(&self, cx: &Context<'_, Self>) -> bool {
        self.picker_host.as_ref().is_none_or(|input| {
            input.read(cx).value().trim()
                == self.volumes_source.as_deref().unwrap_or_default()
        })
    }

    pub fn choose_local_source(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.picker_ssh = false;
        if let Some(host) = self.picker_host.clone() {
            host.update(cx, |host, cx| host.set_value("", window, cx));
        }
        // This computer's list is already there unless a host replaced it.
        if self.volumes_source.is_some() || self.volumes_error.is_some() {
            self.load_volume_source(None, cx);
        }
        cx.notify();
    }

    /// The source switch's SSH side: show the host field and put the
    /// cursor in it. Nothing is listed until Connect.
    pub fn choose_ssh_source(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.picker_ssh = true;
        if let Some(host) = &self.picker_host {
            let focus = gpui_kit::Focusable::focus_handle(host.read(cx), cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// The Directory row's Choose…: the system's folder picker, and a scan
    /// of what it returns, through the same checks as a typed path.
    pub fn browse_picker_path(&self, cx: &Context<'_, Self>) {
        if self.volumes_source.is_some() {
            return;
        }
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
            let _ = this.update(cx, |this, cx| {
                this.choose_source_root(path, cx);
            });
        })
        .detach();
    }

    pub fn connect_picker(&mut self, cx: &mut Context<'_, Self>) {
        let host = self
            .picker_host
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_owned())
            .unwrap_or_default();
        self.load_volume_source(Some(host), cx);
    }

    pub fn choose_picker_path(&mut self, cx: &mut Context<'_, Self>) {
        let path = self
            .picker_path
            .as_ref()
            .map(|input| PathBuf::from(input.read(cx).value().as_str()));
        if let Some(path) = path {
            self.choose_source_root(path, cx);
        }
    }

    pub fn choose_source_root(
        &mut self,
        mut path: PathBuf,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.volumes_open
            || self.volumes_loading
            || self.volumes_error.is_some()
        {
            return;
        }
        if !self.picker_source_matches(cx) {
            self.picker_error = Some(
                "Connect SSH or choose This computer before scanning.".into(),
            );
            cx.notify();
            return;
        }
        let absolute = if self.volumes_source.is_some() {
            disktree_core::remote::is_remote_absolute(&path)
        } else {
            path.is_absolute()
        };
        if !absolute {
            self.picker_error =
                Some("Enter an absolute directory path.".into());
            cx.notify();
            return;
        }
        if self.volumes_source.is_none() {
            match dunce::canonicalize(&path) {
                Ok(root) if root.is_dir() => path = root,
                _ => {
                    self.picker_error =
                        Some("Cannot open that local directory.".into());
                    cx.notify();
                    return;
                }
            }
        }
        self.picker_error = None;
        if self.remote != self.volumes_source {
            // Absolute paths only identify marks within one machine.
            self.marks.clear();
            self.removed.clear();
            self.unmarked = None;
            self.history = History::default();
            self.remote_identity = None;
            self.remote_platform.clear();
            self.git.clear();
            self.checkout = None;
            self.space = None;
            self.space_baseline = None;
            self.disk_root = None;
            self.device = None;
            self.full_disk_access = None;
            self.file_table = false;
            self.home = if self.volumes_source.is_none() {
                std::env::home_dir()
            } else {
                None
            };
        }
        self.remote.clone_from(&self.volumes_source);
        if self.remote.is_some() {
            self.options.follow_links = false;
            self.options.one_filesystem = true;
        }
        self.awaiting_root = false;
        self.close_volumes(cx);
        self.set_root(path, cx);
    }

    /// Closing cancels discovery, not the scan already on screen.
    pub fn close_volumes(&mut self, cx: &mut Context<'_, Self>) {
        self.volume_epoch += 1;
        self.roots_job = None;
        self.volumes_loading = false;
        self.volumes_open = false;
        self.focus_request = Some(FocusTarget::Root);
        cx.notify();
    }

    /// Scan the highlighted row: a listed mount, or the Directory row
    /// after them.
    pub fn choose_volume(&mut self, cx: &mut Context<'_, Self>) {
        if self.volume_highlight >= self.volumes.len() {
            self.choose_picker_path(cx);
            return;
        }
        if let Some(point) = self
            .volumes
            .get(self.volume_highlight)
            .map(|volume| volume.point.clone())
        {
            self.choose_source_root(point, cx);
        }
    }

    /// Move the highlight in the open picker, wrapping at the ends.
    pub fn move_volume_highlight(
        &mut self,
        step: i32,
        cx: &mut Context<'_, Self>,
    ) {
        // The Directory row follows the mounts.
        let len = self.volumes.len() + 1;
        let at = self.volume_highlight.min(len - 1);
        // Picker steps are ±1, but wrap either way without casting the length
        // down to a narrower type.
        let next = match step.signum() {
            1 => (at + 1) % len,
            -1 => at.checked_sub(1).unwrap_or(len - 1),
            _ => at,
        };
        self.volume_highlight = next;
        cx.notify();
    }

    /// `g`: the whole disk. Widens when the disk is above the scanned root,
    /// and goes to its top when it already is the root.
    pub fn go_to_disk(&mut self, cx: &mut Context<'_, Self>) {
        let Some(disk) = self.disk_root.clone() else {
            return;
        };
        if self.path_style().same(&disk, &self.root_path) {
            self.go_to(Vec::new(), cx);
        } else {
            self.widen_to(disk, cx);
        }
    }

    /// Recompute "worth a look" from the tree on screen.
    fn refresh_insights(&mut self) {
        self.scanned_at = now_seconds();
        self.insights = self.tree.as_deref().map_or_else(Vec::new, |tree| {
            worth_a_look(tree.root(), self.scanned_at, INSIGHT_LIMIT)
        });
        self.forget_removed_insights();
    }

    /// Drop suggestions a removal since the scan already took, and take
    /// what went out of those it took part of.
    fn forget_removed_insights(&mut self) {
        if self.removed.is_empty() {
            return;
        }
        let style = self.path_style();
        let mut insights = std::mem::take(&mut self.insights);
        insights.retain_mut(|candidate| {
            let Some(path) = self.path_at(&candidate.crumbs) else {
                return false;
            };
            if self.was_removed(&path) {
                return false;
            }
            candidate.bytes = candidate
                .bytes
                .saturating_sub(self.removed.below(&path, style));
            candidate.bytes > 0
        });
        self.insights = insights;
    }

    /// Whether `path` is the top of a checkout, going to the disk only
    /// when it is not the path asked about last.
    pub fn checkout_at(&mut self, path: &Path) -> bool {
        if let Some((known, checkout)) = &self.checkout
            && known == path
        {
            return *checkout;
        }
        let checkout = crate::git::is_checkout(path);
        self.checkout = Some((path.to_path_buf(), checkout));
        checkout
    }

    /// What [`Self::checkout_at`] last found for `path`, without asking:
    /// for drawing code, which only reads the window's state.
    pub fn known_checkout(&self, path: &Path) -> bool {
        matches!(&self.checkout, Some((known, true)) if known == path)
    }

    /// The panel shows the checkout at `path`: say so to the git store,
    /// and start what it lets run. A selection asks at once; a hover only
    /// once the pointer has rested on it for `git_store::DWELL`.
    pub fn want_git(&mut self, path: &Path, cx: &Context<'_, Self>) {
        if self.remote.is_some() {
            return;
        }
        let now = cx.background_executor().now();
        let hovering = self.pointer_active
            && self.hovered.is_some()
            && self.hovered != self.selected;
        let ask = if hovering {
            let settled = self.hover_since.unwrap_or(now) + git_store::DWELL;
            if now < settled {
                self.wake_git(settled, cx);
                return;
            }
            Ask::Dwell
        } else {
            Ask::Direct
        };
        match self.git.want(path, ask, now) {
            Wanted::Refused { retry_at } => self.wake_git(retry_at, cx),
            // Only the selection refreshes, so only it wakes for that.
            Wanted::Answered { stale_at } if ask == Ask::Direct => {
                self.wake_git(stale_at, cx);
            }
            Wanted::Answered { .. } | Wanted::Pending => {}
        }
        self.run_git(cx);
    }

    /// Start every probe the store lets run now.
    fn run_git(&mut self, cx: &Context<'_, Self>) {
        let now = cx.background_executor().now();
        for job in self.git.next_jobs(now) {
            let probe = self.git_probe.clone();
            let task = cx.background_executor().spawn({
                let job = job.clone();
                async move { probe(&job.path, &job.cancel) }
            });
            cx.spawn(async move |this, cx| {
                let answer = task.await;
                let _ = this.update(cx, |this, cx| {
                    let now = cx.background_executor().now();
                    if this.git.finish(&job, answer, now) {
                        cx.notify();
                    }
                    this.run_git(cx);
                });
            })
            .detach();
        }
    }

    /// Draw again at `at`, unless a wake already comes sooner.
    fn wake_git(&mut self, at: Instant, cx: &Context<'_, Self>) {
        if self.git_wake.as_ref().is_some_and(|(due, _)| *due <= at) {
            return;
        }
        let wait = at.saturating_duration_since(cx.background_executor().now());
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |this, cx| {
                this.git_wake = None;
                cx.notify();
            });
        });
        self.git_wake = Some((at, task));
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

    /// Stop the walk in progress. A partial tree is never shown as if it
    /// were the whole one: a first scan leaves the panel saying it stopped,
    /// and a widening scan leaves the tree it started from on screen.
    pub fn cancel_scan(&mut self, cx: &mut Context<'_, Self>) {
        self.accessible_progress = None;
        self.scan_preview = None;
        if self.remote_scan.take().is_some() {
            self.scan_epoch += 1;
            self.progress.cancelled = true;
            self.progress.finished = true;
            self.scan_elapsed = self.scan_started.map(|t| t.elapsed());
            cx.notify();
            return;
        }
        let Some(scan) = self.scan.take() else {
            return;
        };
        scan.cancel();
        // The poller stops at its next tick instead of taking the result.
        self.scan_epoch += 1;
        self.progress = scan.progress.snapshot();
        self.progress.cancelled = true;
        self.progress.finished = true;
        self.scan_elapsed = self.scan_started.map(|started| started.elapsed());
        self.scan_root.clone_from(&self.root_path);
        cx.notify();
    }

    /// One line in the diagnostic log for a scan that ended: its kind, the
    /// files and the time, or that it failed. Never the root or the error's
    /// text, which can name a path.
    fn log_scan_outcome(&self, kind: &str) {
        let elapsed = self
            .scan_started
            .map_or(0.0, |started| started.elapsed().as_secs_f64());
        match (&self.scan_error, &self.tree) {
            (Some(_), _) => log::warn!(
                target: "pwrdisk::scan",
                "{kind} scan failed after {elapsed:.1} s"
            ),
            (None, Some(tree)) => log::info!(
                target: "pwrdisk::scan",
                "{kind} scan finished: {} files in {elapsed:.1} s",
                tree.root().files
            ),
            (None, None) => {}
        }
    }

    /// Changing resource policy leaves the current scan and result intact;
    /// rescan, navigation to a new root and restart use the new fixed budget.
    pub fn set_power_efficiency(
        &mut self,
        preset: crate::power::PowerEfficiency,
        cx: &mut Context<'_, Self>,
    ) {
        self.power_menu = None;
        // Not offered here: it would run the same workers as the preset
        // below it, under a name that promises more.
        if !preset.available(self.cpu_threads) {
            cx.notify();
            return;
        }
        self.options.threads = preset.policy(self.cpu_threads);
        self.power_choice = Some(preset);
        let label = preset.label(self.cpu_threads);
        self.notice = Some(match self.power_settings_path.as_deref() {
            Some(path) => match crate::power::save(path, preset) {
                Ok(()) => (
                    format!("{label} from the next scan; r rescans now"),
                    Status::Success,
                ),
                Err(error) => (
                    format!(
                        "{label} from the next scan, but it could not be \
                         saved: {error}"
                    ),
                    Status::Warning,
                ),
            },
            None => (
                format!(
                    "{label} from the next scan; no settings directory to \
                     save it in"
                ),
                Status::Warning,
            ),
        });
        cx.notify();
    }

    /// The review screen, once something is marked; until then the notice
    /// says how to mark.
    pub fn open_review(&mut self, cx: &mut Context<'_, Self>) {
        if self.marks.is_empty() {
            self.notice = Some((
                "mark something first: space marks the selected tile".into(),
                Status::Warning,
            ));
        } else {
            self.screen = Screen::Review;
            self.focus_request = Some(FocusTarget::Root);
        }
        cx.notify();
    }

    /// Whether Restart to update may exit now: never while a removal runs,
    /// which an exit would stop partway through the marked list.
    pub const fn can_restart_to_update(&self) -> bool {
        self.run.is_none()
    }

    /// Install the downloaded release and start again, from the toast,
    /// the strip's chip or Settings. Refused while a removal runs, which
    /// the notice then says. Velopack exits the process without the quit
    /// hooks, so the window's frame is saved here first. A restart that
    /// fails comes back through the updater's state (`toasts.rs`).
    pub fn restart_to_update(&mut self, cx: &mut Context<'_, Self>) {
        if !self.can_restart_to_update() {
            self.notice = Some((
                "Restart to update once the removal has finished".into(),
                Status::Warning,
            ));
            cx.notify();
            return;
        }
        crate::window_frame::save();
        crate::updater::restart_in_background();
    }

    /// Open the Power Efficiency menu on the current preset, or close it.
    pub fn toggle_power_menu(&mut self, cx: &mut Context<'_, Self>) {
        use crate::power::PowerEfficiency;
        self.power_menu = if self.power_menu.is_some() {
            None
        } else {
            // On the current preset, or the nearest one below it that this
            // machine offers: a custom count starts from Balanced.
            let current = self.power_choice.unwrap_or_default() as usize;
            (0..=current)
                .rev()
                .find(|&index| {
                    PowerEfficiency::ALL[index].available(self.cpu_threads)
                })
                .or(Some(0))
        };
        cx.notify();
    }

    /// Keys while the Power Efficiency menu is open, as the sibling menu
    /// takes them. The arrows step over presets this machine cannot offer.
    fn on_power_key(&mut self, key: &str, cx: &mut Context<'_, Self>) -> bool {
        use crate::power::PowerEfficiency;
        let Some(highlighted) = self.power_menu else {
            return false;
        };
        let cpus = self.cpu_threads;
        let offered: Vec<usize> = (0..PowerEfficiency::ALL.len())
            .filter(|&index| PowerEfficiency::ALL[index].available(cpus))
            .collect();
        let step = |forward: bool| {
            let next = if forward {
                offered.iter().find(|&&index| index > highlighted)
            } else {
                offered.iter().rev().find(|&&index| index < highlighted)
            };
            next.copied().unwrap_or(highlighted)
        };
        match key {
            "down" | "j" => self.power_menu = Some(step(true)),
            "up" | "k" => self.power_menu = Some(step(false)),
            "home" => self.power_menu = offered.first().copied(),
            "end" => self.power_menu = offered.last().copied(),
            "enter" | "space" => {
                self.set_power_efficiency(
                    PowerEfficiency::ALL[highlighted],
                    cx,
                );
            }
            "escape" => self.power_menu = None,
            _ => {
                self.power_menu = None;
                cx.notify();
                return false;
            }
        }
        cx.notify();
        true
    }

    /// `i`, the Hidden files checkbox and its ⋮ row: rescan with or without
    /// hidden entries.
    pub fn toggle_hidden(&mut self, cx: &mut Context<'_, Self>) {
        self.options.include_hidden = !self.options.include_hidden;
        self.start_scan(cx);
    }

    /// `d`, the Apparent size checkbox and its ⋮ row.
    pub fn toggle_apparent(&mut self, cx: &mut Context<'_, Self>) {
        self.options.apparent_size = !self.options.apparent_size;
        self.start_scan(cx);
    }

    /// Start a fresh scan, abandoning any walk still in progress.
    pub fn start_scan(&mut self, cx: &mut Context<'_, Self>) {
        self.accessible_progress = None;
        if self.awaiting_root {
            self.open_volumes(cx);
            return;
        }
        if let Some(scan) = self.scan.take() {
            scan.cancel();
        }
        self.scan_epoch += 1;
        let epoch = self.scan_epoch;
        self.progress = ScanSnapshot::default();
        self.scan_error = None;
        self.remote_progress =
            disktree_core::remote_progress::Progress::default();
        self.scan_preview = None;
        self.preview_updated = None;
        // A new scan starts at its root; where it was is one `<` away.
        if !self.crumbs.is_empty() {
            self.remember();
        }
        discard(self.tree.take());
        // Its crumbs were the old tree's; the new one gets its own.
        self.legend_matches = None;
        self.legend_epoch += 1;
        self.file_page = 0;
        self.crumbs.clear();
        self.selected = None;
        self.hovered = None;
        self.view = View::default();
        self.cache = None;
        self.insights.clear();
        self.git.clear();
        // Every new tree arrives through here, removal's rescan included.
        self.checkout = None;
        self.clear_filter();
        self.scan_started = Some(Instant::now());
        self.scan_elapsed = None;
        self.scan_root.clone_from(&self.root_path);
        self.remote_scan = None;
        if let Some(host) = &self.remote {
            let request = disktree_core::remote::Request {
                root: (!self.root_path.as_os_str().is_empty())
                    .then(|| self.root_path.clone()),
                disk: self.remote_disk,
                options: self.options.clone(),
            };
            match disktree_core::remote::RemoteScan::spawn(host, &request) {
                Ok(scan) => {
                    self.remote_scan = Some(scan);
                    Self::poll_scan(epoch, cx);
                }
                Err(error) => self.scan_error = Some(error.to_string()),
            }
            cx.notify();
            return;
        }
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

    pub(crate) fn update_remote_identity(
        &mut self,
        identity: Option<disktree_core::identity::HostIdentity>,
    ) {
        // A stable SSH alias can be repointed to a different machine.
        // Old marks must not acquire that machine's freshly captured guard.
        if self.remote_identity != identity {
            self.unmarked = None;
            self.removed.clear();
        }
        if self.remote_identity != identity && !self.marks.is_empty() {
            self.marks.clear();
            self.notice = Some((
                "Remote identity changed; previous marks were cleared.".into(),
                Status::Warning,
            ));
        }
        self.remote_identity = identity;
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
        if let Some(scan) = &self.remote_scan {
            let outcome = scan.poll();
            self.remote_progress = scan.progress();
            self.scan_preview.clone_from(&self.remote_progress.preview);
            self.progress.files = self.remote_progress.files;
            self.progress.dirs = self.remote_progress.dirs;
            self.progress.bytes = self.remote_progress.bytes;
            self.progress.errors = self.remote_progress.errors;
            let Some(outcome) = outcome else {
                cx.notify();
                return true;
            };
            match outcome {
                Ok(snapshot) => {
                    self.root_path = snapshot.header.root;
                    self.home = snapshot.header.home;
                    self.disk_root = snapshot.header.disk_root;
                    self.space = snapshot.header.space;
                    self.remote_platform = snapshot.header.platform;
                    self.update_remote_identity(snapshot.header.identity);
                    self.device = self.remote.as_ref().map(|host| {
                        format!("{host} · read-only · space at scan")
                    });
                    let node = snapshot.tree;
                    self.marks.refresh(
                        &self.root_path,
                        node.root(),
                        self.options.metric,
                        self.path_style(),
                    );
                    self.tree = Some(Arc::new(node));
                    self.scans_done += 1;
                    self.removed.clear();
                    self.refresh_insights();
                    self.refresh_legend(cx);
                    self.select_largest(cx);
                    self.scan_elapsed = self.scan_started.map(|t| t.elapsed());
                }
                Err(error) => {
                    self.scan_error = Some(error.to_string());
                }
            }
            self.log_scan_outcome("SSH");
            self.remote_scan = None;
            self.scan_preview = None;
            self.progress.finished = true;
            cx.notify();
            return false;
        }
        let Some(scan) = &self.scan else {
            return false;
        };
        self.progress = scan.progress.snapshot();
        if self
            .preview_updated
            .is_none_or(|at| at.elapsed().as_secs() >= 1)
        {
            let preview = scan.progress.preview();
            if !preview.entries.is_empty() {
                self.scan_preview = Some(Arc::new(preview));
                self.preview_updated = Some(Instant::now());
            }
        }
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
                    self.file_table = file_table_readable(&self.root_path);
                }
                let metric = self.options.metric;
                self.marks.refresh(
                    &self.root_path,
                    node.root(),
                    metric,
                    self.path_style(),
                );
                discard(self.tree.replace(Arc::new(node)));
                self.scans_done += 1;
                self.removed.clear();
                self.cache = None;
                self.refresh_insights();
                self.refresh_legend(cx);
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
                self.finish_pending_show(cx);
            }
            Err(error) => self.scan_error = Some(error.to_string()),
        }
        self.log_scan_outcome("local");
        self.scan = None;
        self.scan_preview = None;
        self.progress.finished = true;
        cx.notify();
        false
    }

    fn keep_selection_valid(&mut self) {
        let Some(tree) = &self.tree else {
            return;
        };
        if tree.root().resolve(&self.crumbs).is_none() {
            self.crumbs.clear();
        }
        if let Some(selected) = self.selected.clone()
            && tree.root().resolve(&selected).is_none()
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
                        if this.remote.is_some()
                            || this.awaiting_root
                            || this.tree.is_none()
                        {
                            return;
                        }
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

    pub const fn is_indexing(&self) -> bool {
        self.scan.is_some() || self.remote_scan.is_some()
    }

    /// Byte coverage is an estimate: clones, hardlinks, snapshots and access
    /// errors prevent it from measuring how much enumeration time remains.
    pub fn scan_fraction(&self) -> Option<f32> {
        if !self.is_indexing()
            || self.options.apparent_size
            || !self.options.one_filesystem
            || !self.options.include_hidden
            || self.options.follow_links
            || self.options.max_depth.is_some()
            || self.scan_finalizing()
        {
            return None;
        }
        let used = if self.remote.is_some() {
            self.remote_progress.volume_used
        } else {
            self.progress.volume_used
        }?;
        (used > 0).then(|| {
            (self.progress.bytes as f64 / used as f64).clamp(0.0, 0.99) as f32
        })
    }

    pub fn scan_finalizing(&self) -> bool {
        if self.remote.is_some() {
            self.remote_progress.stage
                == disktree_core::remote_progress::Stage::Finalizing
        } else {
            self.progress.finalizing
        }
    }

    pub fn tree(&self) -> Option<Node<'_>> {
        self.tree.as_deref().map(Tree::root)
    }

    /// The node the treemap is currently rooted at.
    pub fn current(&self) -> Option<Node<'_>> {
        self.tree
            .as_ref()
            .and_then(|tree| tree.root().resolve(&self.crumbs))
    }

    pub fn node_at(&self, crumbs: &[usize]) -> Option<Node<'_>> {
        self.tree
            .as_ref()
            .and_then(|tree| tree.root().resolve(crumbs))
    }

    pub fn path_at(&self, crumbs: &[usize]) -> Option<PathBuf> {
        let tree = self.tree.as_ref()?;
        Some(path_of(
            &self.root_path,
            tree.root(),
            crumbs,
            self.path_style(),
        ))
    }

    /// The path rules of the machine on screen: an SSH host is Unix, so its
    /// paths are POSIX on every client.
    pub const fn path_style(&self) -> PathStyle {
        PathStyle::of(self.remote.is_some())
    }

    /// `path` shortened under the scanned machine's home, in its separators.
    pub fn display_path(&self, path: &Path) -> String {
        crate::marks::display_path(
            path,
            self.home.as_deref(),
            self.path_style(),
        )
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
        let style = self.path_style();
        let mut trail: Vec<(String, Crumb)> = style
            .ancestors(&self.root_path)
            .skip(1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|path| {
                (crumb_label(path, style), Crumb::Above(path.to_path_buf()))
            })
            .collect();
        trail.push((
            crumb_label(&self.root_path, style),
            Crumb::Tree(Vec::new()),
        ));
        let Some(tree) = &self.tree else {
            return trail;
        };
        let mut crumbs: Vec<usize> = Vec::new();
        for &index in &self.crumbs {
            let Some(node) = tree
                .root()
                .resolve(&crumbs)
                .and_then(|node| node.child(index))
            else {
                break;
            };
            crumbs.push(index);
            trail.push((node.name().to_string(), Crumb::Tree(crumbs.clone())));
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
            .children()
            .iter()
            .enumerate()
            .map(|(index, child)| Sibling {
                index,
                name: child.name().to_string(),
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

    /// What a click on `crumbs` opens: the child of the directory on screen
    /// that holds it, one level down however deep the tile is drawn. A
    /// click aims at a tile, and a tile can be several levels down; going
    /// straight there skipped the levels between.
    pub fn click_target(&self, crumbs: &[usize]) -> Option<Vec<usize>> {
        let depth = self.crumbs.len();
        if crumbs.len() <= depth || !crumbs.starts_with(&self.crumbs) {
            return None;
        }
        let target = crumbs[..=depth].to_vec();
        self.node_at(&target)
            .is_some_and(|node| node.is_dir() && !node.children().is_empty())
            .then_some(target)
    }

    /// Whether a click on the hovered tile would open a level: it is the
    /// selection, and something holds it. The pointer says so.
    pub fn click_opens_hovered(&self) -> bool {
        self.hovered.as_deref().is_some_and(|hovered| {
            self.selected.as_deref() == Some(hovered)
                && self.click_target(hovered).is_some()
        })
    }

    /// Open one level toward `crumbs`, keeping it selected when it lies
    /// deeper, so the next click on it opens the next level. Returns whether
    /// a level opened.
    fn open_toward(
        &mut self,
        crumbs: &[usize],
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(target) = self.click_target(crumbs) else {
            return false;
        };
        let from = self.tile_body(&target).map(|rect| self.view.project(rect));
        self.enter(target, from, cx);
        if crumbs.len() > self.crumbs.len() && crumbs.starts_with(&self.crumbs)
        {
            self.selected = Some(crumbs.to_vec());
        }
        true
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
        if !node.is_dir() || node.children().is_empty() {
            return;
        }
        self.remember();
        self.file_page = 0;
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
        self.transition = (!prefers_reduced_motion(cx))
            .then(|| LayoutTransition::new(src, dst));
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
                    node.is_dir() && !node.children().is_empty()
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
        self.file_page = 0;
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
        self.transition = if prefers_reduced_motion(cx) {
            None
        } else {
            self.tile_body(&child_crumbs)
                .filter(|rect| rect.w > 1.0 && rect.h > 1.0)
                .map(|dst| LayoutTransition::new(src, dst))
        };
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
            self.file_page = 0;
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
        if self
            .history
            .back
            .last()
            .is_none_or(|last| !self.path_style().same(last, &here))
        {
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
            .is_some_and(|node| node.is_dir() && !node.children().is_empty());
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
        if self.file_list {
            self.file_step(step, cx);
            return;
        }
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
        (0..node.children().len())
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
        if self.was_removed(&target.path) {
            self.notice = Some((
                format!(
                    "{} was removed; rescan to see the disk as it is now",
                    self.display_path(&target.path)
                ),
                Status::Warning,
            ));
            cx.notify();
            return;
        }
        if !self.marks.contains(&target.path, self.path_style())
            && let Some(ancestor) = self.marked_ancestor(&target.path)
        {
            self.notice = Some((
                format!(
                    "{} goes with the marked {}; unmark that to keep it",
                    self.display_path(&target.path),
                    self.display_path(&ancestor)
                ),
                Status::Warning,
            ));
            cx.notify();
            return;
        }
        let path = target.path.clone();
        let style = self.path_style();
        if self.marks.toggle(target, self.path_style()) {
            self.space_baseline = self.space_baseline.or(self.space);
            let inside = self.marks.absorb(&path, style);
            if inside > 0 {
                self.notice = Some((
                    format!(
                        "{} now covers {} mark{} inside it",
                        self.display_path(&path),
                        inside,
                        if inside == 1 { "" } else { "s" }
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
            .ancestor(path, self.path_style())
            .map(Path::to_path_buf)
    }

    pub fn target_at(&self, crumbs: &[usize]) -> Option<Target> {
        let node = self.node_at(crumbs)?;
        let path = self.path_at(crumbs)?;
        // A directory part of which was removed frees only the rest.
        let bytes = node
            .bytes
            .saturating_sub(self.removed.below(&path, self.path_style()));
        Some(Target {
            hidden: is_hidden(&path, self.path_style()),
            path,
            bytes,
            is_dir: node.is_dir(),
        })
    }

    pub fn unmark(&mut self, path: &Path, cx: &mut Context<'_, Self>) {
        self.marks.remove(path, self.path_style());
        cx.notify();
    }

    /// Review's Unmark all, or `!`. Marks live only in memory, so what goes
    /// is kept for [`Self::undo_unmark`] rather than asked about first.
    pub fn clear_marks(&mut self, cx: &mut Context<'_, Self>) {
        if self.marks.is_empty() {
            return;
        }
        let taken = self.marks.items().to_vec();
        let count = taken.len();
        self.marks.clear();
        self.unmarked = Some(taken);
        // Export and Unmark all are disabled now; neither may keep focus.
        self.focus_request = Some(FocusTarget::Root);
        self.notice = Some((
            format!("Unmarked {count} \u{b7} u or {UNDO_KEYS} to undo"),
            Status::Neutral,
        ));
        cx.notify();
    }

    /// Put back what Unmark all took. Marks made since stay; a path that
    /// is marked again, or now goes with a marked directory, is skipped.
    /// Only where marks are edited: during a removal they are the running
    /// list's, and are cleared when it ends.
    pub fn undo_unmark(&mut self, cx: &mut Context<'_, Self>) {
        if !matches!(self.screen, Screen::Explore | Screen::Review) {
            return;
        }
        let Some(taken) = self.unmarked.take() else {
            return;
        };
        let style = self.path_style();
        let mut restored = 0;
        for target in taken {
            if self.marks.contains(&target.path, style)
                || self.marks.ancestor(&target.path, style).is_some()
            {
                continue;
            }
            let path = target.path.clone();
            self.marks.toggle(target, style);
            self.marks.absorb(&path, style);
            restored += 1;
        }
        self.notice = Some((
            if restored == 0 {
                "Nothing to put back: those paths are marked again".into()
            } else {
                format!("Marked {restored} again")
            },
            Status::Neutral,
        ));
        cx.notify();
    }

    /// The Removing screen's Stop, or Escape: the worker finishes the item
    /// in hand and starts no other.
    pub fn stop_removal(&mut self, cx: &mut Context<'_, Self>) {
        let Some(run) = &self.run else {
            return;
        };
        run.cancel();
        self.stop_requested = true;
        self.notice =
            Some(("stopping after the current item".into(), Status::Warning));
        cx.notify();
    }

    /// The plan the review screen shows and the removal runs.
    pub fn plan(&self) -> Plan {
        #[cfg(test)]
        crate::marks::work::PLANS.set(crate::marks::work::PLANS.get() + 1);
        if self.remote.is_some() {
            // Never judge remote paths against this machine's filesystem.
            // This is an export selection, not an executable removal plan.
            let mut selection = Plan::default();
            selection.targets = self
                .marks
                .items()
                .iter()
                .filter(|target| {
                    self.path_style().is_below(&target.path, &self.root_path)
                })
                .cloned()
                .collect();
            selection.root.clone_from(&self.root_path);
            // Remote volume attribution is unknown; never infer it from
            // this computer's mounts or project an unmeasured saving.
            return selection;
        }
        let mut plan = plan(self.marks.items(), &self.root_path);
        if let Some(tree) = &self.tree {
            let mut cached = self.accounting.borrow_mut();
            if !cached.tree.ptr_eq(&Arc::downgrade(tree))
                || cached.root != self.root_path
            {
                *cached = AccountingCache {
                    tree: Arc::downgrade(tree),
                    root: self.root_path.clone(),
                    estimates: Vec::new(),
                };
            }
            let mut resolver = crate::marks::Resolver::new(
                &self.root_path,
                tree.root(),
                PathStyle::Native,
            );
            let mut next = Vec::new();
            plan.estimate_selection(|targets| {
                let paths: Vec<_> =
                    targets.iter().map(|target| target.path.clone()).collect();
                let value = cached
                    .estimates
                    .iter()
                    .find(|(old, _)| *old == paths)
                    .map_or_else(
                        || {
                            let nodes: Vec<_> = targets
                                .iter()
                                .filter_map(|target| {
                                    resolver
                                        .resolve(&target.path)
                                        .map(|(_, node)| node)
                                })
                                .collect();
                            if let [node] = nodes.as_slice() {
                                node.sharing().estimate.unwrap_or(
                                    disktree_core::sharing::Estimate {
                                        lower: node.bytes,
                                        upper: node.bytes,
                                        partial: false,
                                    },
                                )
                            } else {
                                disktree_core::sharing::estimate(nodes)
                            }
                        },
                        |(_, value)| *value,
                    );
                next.push((paths, value));
                value
            });
            cached.estimates = next;
        }
        plan
    }

    // ── layout and hit-testing ──────────────────────────────────────────

    /// Resolve everything the mosaic needs for this frame.
    ///
    /// Runs once per render, before the paint callbacks: painting then only has
    /// to draw, and the marked/hidden/selected state of every tile is decided
    /// here, where the tree and the marks are both at hand.
    pub fn prepare(&mut self) -> Mosaic {
        #[cfg(test)]
        crate::marks::work::FRAMES.set(crate::marks::work::FRAMES.get() + 1);
        let metric = self.options.metric;
        let view = self.view;
        let hovered = self.hovered.clone();
        let selected = self.selected.clone();

        let style = self.path_style();
        let marked = self.tree.as_ref().map_or_else(
            || Arc::new(FxHashSet::default()),
            |tree| self.marks.crumbs(&self.root_path, tree, metric, style),
        );
        let removed = self.tree.as_ref().map_or_else(
            || Arc::new(FxHashSet::default()),
            |tree| self.removed.crumbs(&self.root_path, tree, metric, style),
        );

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
            #[cfg(test)]
            crate::marks::work::TILES.set(crate::marks::work::TILES.get() + 1);
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
            // A search being typed dims what it does not match; otherwise
            // the legend says which folders hold more than is shown.
            let keep = self
                .matches
                .as_deref()
                .or(self.legend_matches.as_deref())
                .map(|matches| match node {
                    Some(node) => matches.keep_at(crumbs, node),
                    None => matches.keep(crumbs),
                });
            let filtered = match keep {
                None | Some(Some(Keep::Whole)) => Filtered::Shown,
                Some(Some(Keep::Partial { .. })) => Filtered::Holds,
                Some(None) => Filtered::Out,
            };
            // A partly shown folder is laid out by what passed, so its label
            // says that and what it holds in all: "48G of 196G".
            let partial =
                self.layout_filter().and_then(|matches| {
                    match matches.keep(crumbs) {
                        Some(Keep::Partial { bytes, files }) => {
                            Some((bytes, files))
                        }
                        _ => None,
                    }
                });
            let is_marked = marked.contains(crumbs);
            // Everything inside a marked directory goes with it, so it is
            // drawn marked too.
            let is_covered = !marked.is_empty()
                && (1..crumbs.len())
                    .any(|length| marked.contains(&crumbs[..length]));
            // Removed since the scan, itself or with a directory holding it.
            let is_removed = !removed.is_empty()
                && (1..=crumbs.len())
                    .any(|length| removed.contains(&crumbs[..length]));
            let decoration = TileDeco {
                rect: self.animated_rect(tile.rect),
                depth: tile.depth,
                category,
                age_bucket,
                reclaimable: node.is_some_and(|n| n.reclaim.is_some()),
                filtered,
                unreadable: node.is_some_and(|n| n.read_error),
                marked: is_marked,
                covered: is_covered,
                removed: is_removed,
                hovered: hovered.as_deref() == Some(crumbs),
                selected: selected.as_deref() == Some(crumbs),
            };
            decorations.push(decoration.clone());

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
                    // State is said without colour, and without spending
                    // the label's width on it: a marked name, and every name
                    // inside it, is struck through, and a name the find text
                    // matches has the matching part underlined. Words would
                    // repeat on every tile in a mark or a search and push
                    // narrow names out of their tiles. Only "unreadable",
                    // which is rare, keeps a prefix.
                    let unreadable = if node.read_error { "! " } else { "" };
                    // A tile the filter left out holds no match: skip the
                    // search for it.
                    let found = self
                        .matches
                        .as_deref()
                        .filter(|_| filtered != Filtered::Out)
                        .and_then(|matches| {
                            find_ignoring_case(node.name(), &matches.needle)
                        })
                        .map(|range| {
                            range.start + unreadable.len()
                                ..range.end + unreadable.len()
                        });
                    labels.push(Label {
                        text: format!("{unreadable}{}", node.name()),
                        found,
                        rect: self.animated_rect(tile.rect),
                        header: tile
                            .header
                            .map(|header| self.animated_rect(header)),
                        depth: tile.depth,
                        dim: filtered == Filtered::Out || is_removed,
                        marked: is_marked || is_covered,
                        size_text: partial.map_or_else(
                            || crate::widgets::short_value(node, metric),
                            |(bytes, files)| {
                                crate::widgets::short_share(
                                    node, bytes, files, metric,
                                )
                            },
                        ),
                        decoration: decoration.clone(),
                    });
                }
                TileKind::Others { count, .. } => labels.push(Label {
                    text: format!("+{count} more"),
                    found: None,
                    rect: self.animated_rect(tile.rect),
                    header: None,
                    depth: tile.depth,
                    dim: self.matches.is_some(),
                    marked: false,
                    size_text: String::new(),
                    decoration: decoration.clone(),
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

    /// Whether the node at `crumbs` is marked, or inside a marked directory
    /// and so removed with it. The trail strikes such a crumb, as the
    /// mosaic strikes such a tile's name.
    pub fn goes_with_a_mark(&self, crumbs: &[usize]) -> bool {
        let style = self.path_style();
        self.path_at(crumbs).is_some_and(|path| {
            self.marks.contains(&path, style)
                || self.marks.ancestor(&path, style).is_some()
        })
    }

    /// Crumbs for an absolute path, if the tree still contains it.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "a marked path may have been removed from the tree already"
    )]
    pub fn crumbs_for_path(&self, path: &Path) -> Option<Vec<usize>> {
        // The path is relative to the scanned root, so the walk starts there,
        // not at the directory currently drawn.
        // Names are split by the scanned machine's rules, so a Unix host's
        // `a\b` stays one name on a Windows client.
        let relative = self.path_style().relative(&self.root_path, path)?;
        let tree = self.tree.clone()?;
        let mut node = tree.root();
        let mut crumbs = Vec::new();
        for name in relative {
            let index = node.children().iter().position(|child| {
                #[cfg(test)]
                crate::marks::work::step();
                child.name() == name
            })?;
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
            let node = tree.root().resolve(&self.crumbs)?;
            let rect = Rect::new(0.0, 0.0, key.width, key.height);
            let filter = self.layout_filter();
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

    /// What the mosaic lays out instead of the whole tree: an applied
    /// search, which already keeps to the legend, or else the legend.
    pub fn layout_filter(&self) -> Option<&Matches> {
        if self.filter_applied {
            self.matches.as_deref()
        } else {
            self.legend_matches.as_deref()
        }
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
        // Children are ordered by the metric, so every crumb moves. Keep the
        // paths and find them again in the reordered tree.
        let directory = self.current_path();
        let selected = self.selected.as_deref().and_then(|c| self.path_at(c));
        self.options.metric = self.options.metric.toggled();
        if let Some(tree) = self.tree.take() {
            // The window is usually the tree's only owner, and then it is
            // reordered where it lies. A copy of 20 million nodes took 0.8
            // to 0.9 s and briefly held the whole tree twice, 3.2 GiB more
            // on such a scan, where reordering took 0.1 to 0.2 s. A find or
            // a widening walk still reading it gets its own copy, as before.
            // Either way it becomes a new `Arc`, so a removal preview bound
            // to the old order lapses as it did.
            let mut tree = Arc::try_unwrap(tree).unwrap_or_else(|shared| {
                let copy = (*shared).clone();
                // The other reader may let go first; then this is the last
                // reference, and it must not be freed on this thread.
                discard(Some(shared));
                copy
            });
            // Totals and sharing do not depend on the metric; only the order
            // of children does.
            disktree_core::tree::reorder(&mut tree, self.options.metric);
            self.tree = Some(Arc::new(tree));
            let metric = self.options.metric;
            let style = self.path_style();
            self.marks.refresh(
                &self.root_path,
                self.tree.as_ref().unwrap().root(),
                metric,
                style,
            );
            self.marks.discount(&self.removed, style);
        }
        self.crumbs = self.crumbs_for_path(&directory).unwrap_or_default();
        self.selected = selected.and_then(|path| self.crumbs_for_path(&path));
        self.forget_hover();
        self.crumb_menu = None;
        self.transition = None;
        self.refresh_insights();
        // Every crumb moved, so the legend's result points at other
        // children now; drop it rather than lay it out until the new one
        // comes back.
        self.legend_matches = None;
        self.refresh_legend(cx);
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
    /// a permanent deletion, which cannot be undone. `rescan` is Remove &
    /// rescan; without it the tree drops what went and stays.
    pub fn commit(&mut self, rescan: bool, cx: &mut Context<'_, Self>) {
        if self.remote.is_some() || self.plan().is_empty() {
            return;
        }
        self.rescan_after_removal = rescan;
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
        self.overlay_return = None;
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
        let navigation_visible = self.screen == Screen::Explore
            && !self.awaiting_root
            && self.tree.is_some();
        let list_visible = self.file_list && navigation_visible;
        // Both navigation views leave the focus tree during a rescan. Their
        // persistent parent must own Escape until navigation is available.
        if !navigation_visible && self.file_focus.is_focused(window) {
            self.restore_file_focus = true;
            window.focus(&self.focus, cx);
        }
        if self.screen != Screen::Explore {
            self.restore_file_focus = false;
        } else if navigation_visible
            && std::mem::take(&mut self.restore_file_focus)
            && self.focus.is_focused(window)
            && self.focus_request.is_none()
        {
            window.focus(&self.file_focus, cx);
        }
        match self.focus_request.take() {
            Some(FocusTarget::Root) => {
                let target = self.overlay_return.take().unwrap_or_else(|| {
                    if list_visible {
                        self.file_focus.clone()
                    } else {
                        self.focus.clone()
                    }
                });
                window.focus(&target, cx);
            }
            Some(FocusTarget::Dialog) => {
                if self.overlay_return.is_none() {
                    self.overlay_return = window.focused(cx);
                }
                window.focus(&self.confirm_focus, cx);
            }
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
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if crate::ui::zoom_interface(event, window, cx) {
            self.cache = None;
            true
        } else {
            false
        }
    }

    pub fn begin_removal(&mut self, cx: &mut Context<'_, Self>) {
        if self.remote.is_some() {
            return;
        }
        let plan = self.plan();
        if plan.is_empty() {
            self.notice = Some(("nothing is marked".into(), Status::Warning));
            cx.notify();
            return;
        }
        let limit =
            match crate::safety::load(self.removal_settings_dir.as_deref())
                .and_then(|limit| limit.check(&plan.targets).map(|_| limit))
            {
                Ok(limit) => limit,
                Err(reason) => {
                    self.notice = Some((reason, Status::Warning));
                    cx.notify();
                    return;
                }
            };
        // This window quits once the prompt is answered yes, and quitting
        // would stop a permanent delete halfway.
        if self.restarting {
            self.notice = Some((
                "not while restarting as administrator".into(),
                Status::Warning,
            ));
            cx.notify();
            return;
        }
        // Measured now rather than read from the ticker, which can be a
        // second old: the observed figure is only as good as its baseline.
        let before = space_info(&self.root_path).ok().or(self.space);
        self.space_baseline = before.or(self.space_baseline);
        self.ledger.begin(self.scans_done);
        self.run_start = Some(crate::ledger::Start {
            targets: plan.targets.clone(),
            mode: self.removal_mode,
            before: before.map(|space| space.available),
            started: std::time::SystemTime::now(),
        });
        self.run_epoch += 1;
        let epoch = self.run_epoch;
        self.run_summary = RunSummary {
            total: plan.targets.len(),
            ..RunSummary::default()
        };
        self.run_log.clear();
        self.stop_requested = false;
        self.screen = Screen::Running;
        self.run = Some(disktree_core::removal::spawn_limited(
            plan,
            self.removal_mode,
            limit,
        ));
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
                    // Counts only: the paths removed are the user's.
                    log::info!(
                        target: "pwrdisk::removal",
                        "removal finished: {removed} removed, {bytes} bytes, \
                         {failed} failed"
                    );
                    finished = true;
                }
            }
        }
        if finished {
            self.run = None;
            self.record_removal();
            let removed = self.record_cohort();
            self.marks.clear();
            self.unmarked = None;
            self.screen = Screen::Done;
            // What went stays drawn, as removed, until a scan: a rescan
            // of a whole disk takes minutes and is asked for, never implied.
            self.note_removed(removed);
            if std::mem::take(&mut self.rescan_after_removal) {
                self.start_scan(cx);
            }
        }
        cx.notify();
        !finished
    }

    /// Add the run that just finished to the ledger. Returns what it
    /// removed.
    fn record_cohort(&mut self) -> Vec<disktree_core::removal::Target> {
        let Some(start) = self.run_start.take() else {
            return Vec::new();
        };
        let removed = crate::ledger::removed(&start, &self.run_log);
        let private = self.private_estimate(&removed);
        self.ledger.push(crate::ledger::Cohort {
            mode: start.mode,
            removed: removed.len() as u64,
            failed: self.run_summary.failed,
            reported: removed.iter().map(|target| target.bytes).sum(),
            private,
            before: start.before,
            after: space_info(&self.root_path)
                .ok()
                .map(|space| space.available),
            started: start.started,
            finished: std::time::SystemTime::now(),
        });
        removed
    }

    /// Keep what a removal took, so the tree shows it as gone and it is
    /// never marked again before a scan.
    fn note_removed(&mut self, removed: Vec<disktree_core::removal::Target>) {
        let style = self.path_style();
        for target in removed {
            if self.was_removed(&target.path) {
                continue;
            }
            let path = target.path.clone();
            self.removed.toggle(target, style);
            self.removed.absorb(&path, style);
        }
        self.forget_removed_insights();
    }

    /// Whether `path` was removed since the scan, itself or with a
    /// directory holding it.
    pub fn was_removed(&self, path: &Path) -> bool {
        let style = self.path_style();
        self.removed.contains(path, style)
            || self.removed.ancestor(path, style).is_some()
    }

    /// What the scan saw as only these targets', estimated as review does,
    /// from the tree they were removed from. Unknown where review's is.
    fn private_estimate(
        &self,
        targets: &[disktree_core::removal::Target],
    ) -> Option<u64> {
        if self.options.apparent_size
            || (cfg!(target_os = "macos") && !self.options.apfs_clone_metadata)
        {
            return None;
        }
        let tree = self.tree.as_ref()?;
        let mut resolver = crate::marks::Resolver::new(
            &self.root_path,
            tree.root(),
            PathStyle::Native,
        );
        let nodes: Vec<_> = targets
            .iter()
            .filter_map(|target| {
                resolver.resolve(&target.path).map(|(_, node)| node)
            })
            .collect();
        let estimate = match nodes.as_slice() {
            [] => 0,
            [node] => node
                .sharing()
                .estimate
                .map_or(node.bytes, |estimate| estimate.lower),
            _ => disktree_core::sharing::estimate(nodes).lower,
        };
        // The scan's figures still hold what an earlier removal took from
        // inside these targets; their reported bytes already leave it out,
        // and nothing private can exceed what is reported.
        let reported = targets.iter().map(|target| target.bytes).sum();
        Some(estimate.min(reported))
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
        let rule = self.legend_rule();
        let task = cx.background_executor().spawn(async move {
            tree.root().resolve(&base).and_then(|node| {
                filter_with(node, &base, &needle, rule.as_ref())
            })
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
                let node = tree.as_deref()?.root().resolve(crumbs)?;
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
        self.screen == Screen::Explore
            && !self.confirm_open
            && !self.volumes_open
            && !self.show_help
            && !self.show_agent
    }

    pub const fn has_modal(&self) -> bool {
        self.confirm_open
            || self.volumes_open
            || self.show_help
            || self.show_agent
    }

    pub fn open_help(&mut self, cx: &mut Context<'_, Self>) {
        if !self.has_modal() {
            // A modal must not leave another keyboard owner behind it.
            self.crumb_menu = None;
            self.power_menu = None;
            self.more_menu = None;
            self.app_menu_open = false;
            self.export_menu_open = false;
            self.show_help = true;
            self.focus_request = Some(FocusTarget::Dialog);
            cx.notify();
        }
    }

    pub fn close_help(&mut self, cx: &mut Context<'_, Self>) {
        self.show_help = false;
        self.focus_request = Some(FocusTarget::Root);
        cx.notify();
    }

    pub fn toggle_app_menu(&mut self, cx: &mut Context<'_, Self>) {
        if self.has_modal() {
            return;
        }
        self.app_menu_open = !self.app_menu_open;
        self.app_menu_index = 0;
        self.focus_request = Some(if self.app_menu_open {
            FocusTarget::Dialog
        } else {
            FocusTarget::Root
        });
        cx.notify();
    }

    /// Ask for a directory and scan it: an app opened from the Dock has no
    /// command line to name one.
    pub fn open_folder(&self, cx: &Context<'_, Self>) {
        if self.remote.is_some() {
            return;
        }
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
        self.save_export(false, cx);
    }

    pub fn save_delete_script(&mut self, cx: &mut Context<'_, Self>) {
        self.save_export(true, cx);
    }

    fn save_export(&mut self, script: bool, cx: &mut Context<'_, Self>) {
        let plan = self.plan();
        if plan.is_empty() {
            self.notice = Some(("nothing to save".into(), Status::Warning));
            cx.notify();
            return;
        }
        let list = if script {
            self.delete_script(&plan.targets)
        } else {
            disktree_core::export::delete_list(&plan.targets)
        };
        let count = plan.targets.len();
        let directory =
            std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let filename = if script {
            "pwrdisk-delete.sh"
        } else {
            "pwrdisk-delete-list.txt"
        };
        let chosen = cx.prompt_for_new_path(&directory, Some(filename));
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

    fn delete_script(&self, targets: &[Target]) -> String {
        let identity = if self.remote.is_some() {
            self.remote_identity.clone()
        } else {
            disktree_core::identity::HostIdentity::current()
        };
        disktree_core::export::delete_script_for(
            targets,
            &self.root_path,
            self.remote.as_deref(),
            identity.as_ref(),
        )
    }

    pub fn copy_delete_script(&mut self, cx: &mut Context<'_, Self>) {
        let plan = self.plan();
        if plan.is_empty() {
            self.notice = Some(("nothing to copy".into(), Status::Warning));
        } else {
            let script = self.delete_script(&plan.targets);
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(script));
            self.notice = Some((
                "copied delete script; previews by default, checks host before deletion".into(),
                Status::Success,
            ));
        }
        cx.notify();
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
        let mut prompt = if self.remote.is_some() {
            disktree_core::export::agent_prompt_for(
                &plan.targets,
                &self.root_path,
                self.space,
                &self.remote_platform,
                self.remote_identity.as_ref(),
            )
        } else {
            disktree_core::export::agent_prompt(
                &plan.targets,
                &self.root_path,
                self.space,
            )
        };
        if let Some(host) = &self.remote {
            prompt = format!(
                "The scan is from remote SSH host {host} ({}), NOT this \
                 computer. All paths below belong to that server. Connect \
                 via SSH; verify the host and paths before acting. \
                 Free space is a snapshot, not live.\n\n{prompt}",
                self.remote_platform
            );
        }
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

    /// Show the tile a key acts on in Finder (or the file manager), selected.
    pub fn reveal_target(&mut self, cx: &mut Context<'_, Self>) {
        if self.remote.is_some() {
            return;
        }
        let crumbs =
            self.action_target().unwrap_or_else(|| self.crumbs.clone());
        let Some(path) = self.path_at(&crumbs) else {
            return;
        };
        // GPUI's reveal cannot report failure, so check what it can't.
        if std::fs::symlink_metadata(&path).is_err() {
            self.notice = Some((
                format!("{} is no longer on disk", self.display_path(&path)),
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

        // The volume picker owns its keys while open: arrows move, Enter
        // picks, Escape closes, and nothing behind it acts.
        if self.volumes_open {
            match key {
                "escape" => self.close_volumes(cx),
                "enter" => self.choose_volume(cx),
                "up" | "k" if !control => {
                    self.move_volume_highlight(-1, cx);
                }
                "down" | "j" if !control => {
                    self.move_volume_highlight(1, cx);
                }
                _ => {}
            }
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
        if self.power_menu.is_some() && self.on_power_key(key, cx) {
            return;
        }
        if self.more_menu.is_some() && self.on_more_key(key, cx) {
            return;
        }

        if self.app_menu_open {
            if key == "escape" {
                self.app_menu_open = false;
                self.focus_request = Some(FocusTarget::Root);
                cx.notify();
            }
            return;
        }

        if self.show_help {
            if matches!(key, "escape" | "?" | "/" | "q") {
                self.close_help(cx);
            }
            return;
        }
        if self.show_agent {
            if matches!(key, "escape" | "q") {
                self.show_agent = false;
                self.focus_request = Some(FocusTarget::Root);
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
            // No review key is a Ctrl chord, and Edit's rows replay those
            // (Select All is ctrl-a), so a chord must not act as its letter.
            Screen::Review if !control => self.on_review_key(key, shift, cx),
            Screen::Review => {}
            Screen::Running => {
                if key == "escape" {
                    self.stop_removal(cx);
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
        if self.file_list && !control {
            match key {
                "n" => self.file_step(if shift { -1 } else { 1 }, cx),
                "up" | "k" => self.file_step(-1, cx),
                "down" | "j" => self.file_step(1, cx),
                "pageup" => self.file_step(-100, cx),
                "pagedown" => self.file_step(100, cx),
                "right" | "l" => self.descend(cx),
                "left" | "h" => self.ascend(cx),
                _ => self.on_file_shared_key(key, control, shift, cx),
            }
            return;
        }
        self.on_file_shared_key(key, control, shift, cx);
    }

    fn on_file_shared_key(
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
                | "n"
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
            // Shift-U, not `u`: plain `u` goes up a level, as backspace.
            // Updates is Settings › Updates now, in its own window.
            "u" if shift && !control => cx.defer(|cx| {
                crate::settings::open(crate::settings::Pane::Updates, cx);
            }),
            "backspace" | "u" if !control => self.ascend(cx),
            // A filter is the first thing Escape takes away.
            "escape" if self.matches.is_some() => self.clear_filter(),
            "escape" if self.scan.is_some() || self.remote_scan.is_some() => {
                self.cancel_scan(cx);
            }
            // Then the legend's, before Escape goes back to moving around.
            "escape" if self.legend.is_active() => {
                self.clear_legend(None, cx);
            }
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
            "n" if !control => {
                self.cycle_sibling(if shift { -1 } else { 1 }, cx);
            }
            "c" if !control => self.open_review(cx),
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
            "v" if !control && !shift => self.open_volumes(cx),
            "i" if !control => self.toggle_hidden(cx),
            "d" if !control => self.toggle_apparent(cx),
            "p" if !control => {
                self.show_selection = !self.show_selection;
                cx.notify();
            }
            "o" if !control => self.reveal_target(cx),
            "?" => {
                self.open_help(cx);
            }
            // A bare letter quitting is Omarchy's habit, kept there; macOS
            // and Windows quit with ⌘Q and Alt+F4, and q would throw away
            // marks by surprise.
            "q" if !control
                && crate::chrome::Platform::current()
                    == crate::chrome::Platform::Linux =>
            {
                cx.quit();
            }
            _ => {}
        }
    }

    fn on_review_key(
        &mut self,
        key: &str,
        shift: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if self.remote.is_some() && matches!(key, "m" | "p" | "enter") {
            return;
        }
        // The open Export menu shows each row's key: those keys run the
        // row and close it, as a click would; Escape only closes it.
        if self.export_menu_open {
            let row = matches!(key, "s" | "a" | "e" | "y");
            if row || key == "escape" {
                self.export_menu_open = false;
                self.focus_request = Some(FocusTarget::Root);
                cx.notify();
            }
            if !row {
                return;
            }
        }
        if key == "e" {
            self.save_delete_script(cx);
            return;
        }
        match key {
            "escape" => {
                self.screen = Screen::Explore;
                self.focus_request = Some(FocusTarget::Root);
                cx.notify();
            }
            "enter" => self.commit(shift, cx),
            "!" => self.clear_marks(cx),
            "u" => self.undo_unmark(cx),
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
            "y" => self.copy_delete_script(cx),
            "?" => {
                self.open_help(cx);
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
        if self.hovered != previous {
            self.hover_since = Some(cx.background_executor().now());
        }
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
            // A later press of a double or triple click. One gesture opens
            // at most one level: once a press of it has opened, the view
            // has changed under the pointer, and acting on what is there
            // now would open a second level nobody aimed at.
            MouseButton::Left if event.click_count >= 2 => {
                if self.click_opened {
                    return;
                }
                if let Some(crumbs) = crumbs {
                    self.select(Some(crumbs.clone()), cx);
                    self.click_opened = self.open_toward(&crumbs, cx);
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
                self.click_opened = false;
                // A second click on the selection opens it, like a file
                // manager, without needing a double click.
                if let Some(crumbs) = crumbs
                    .as_ref()
                    .filter(|crumbs| self.selected.as_ref() == Some(*crumbs))
                {
                    let crumbs = crumbs.clone();
                    self.click_opened = self.open_toward(&crumbs, cx);
                    if self.click_opened {
                        return;
                    }
                }
                self.select(crumbs, cx);
            }
            MouseButton::Middle => {
                if let Some(crumbs) = crumbs {
                    self.toggle_mark(&crumbs, cx);
                }
            }
            // What `o` does, for the tile under the pointer: select it, then
            // show it in Finder, File Explorer or the file manager.
            MouseButton::Right => {
                if let Some(crumbs) = crumbs {
                    self.select(Some(crumbs), cx);
                    self.reveal_target(cx);
                }
            }
            // Buttons 8 and 9. gpui-pre maps them on X11, Wayland and
            // Windows; a mouse with no side buttons never sends them, and
            // then the header `<` / `>` and alt-arrows are the whole story.
            MouseButton::Navigate(NavigationDirection::Back)
                if self.screen == Screen::Explore =>
            {
                self.go_back(cx);
            }
            MouseButton::Navigate(NavigationDirection::Forward)
                if self.screen == Screen::Explore =>
            {
                self.go_forward(cx);
            }
            // Side buttons off the explore screen.
            MouseButton::Navigate(_) => {}
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
    pub fn tick_transition(&mut self, window: &Window, cx: &mut gpui_kit::App) {
        let Some(transition) = self.transition else {
            return;
        };
        // Asked only mid-flight: the answer is a platform call, and no other
        // frame has motion to drop.
        if prefers_reduced_motion(cx) {
            self.transition = None;
            return;
        }
        let (_, running) = transition.sample(Rect::default());
        if running {
            window.request_animation_frame();
        } else {
            self.transition = None;
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

/// Base 0.6.6 follows Linux portal changes, but reads macOS/Windows only
/// when asked. Re-sample before a transition starts and on each of its
/// frames so a running app honors an OS toggle too. Base preserves explicit
/// application overrides.
pub fn prefers_reduced_motion(cx: &mut gpui_kit::App) -> bool {
    gpui_kit::base::apply_system_reduce_motion(cx);
    cx.reduce_motion()
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
    /// The bytes of `text` the find text matched: underlined, so a match
    /// reads as one without its colour.
    pub found: Option<Range<usize>>,
    /// Base-space rectangle; the view transform is applied while painting.
    pub rect: Rect,
    /// The band this label belongs in, when its tile reserved one. A parent's
    /// name goes in its own band, never over its children.
    pub header: Option<Rect>,
    /// Nesting depth in this view: the first level is set in bold.
    pub depth: u32,
    /// Filtered out while typing: drawn quietly.
    pub dim: bool,
    /// Marked, or inside a mark: struck through, in the marked ink.
    pub marked: bool,
    pub size_text: String,
    /// The exact fill state is needed for contrast after filtering/marking.
    pub decoration: TileDeco,
}

/// How many labels one frame will shape.
const MAX_LABELS: usize = 150;

/// Height of a top-level directory's name band, in rem.
const HEADER_REMS: f32 = 1.375;

/// Height of the slim label row a deeper open directory keeps, in rem.
const HEADER_INNER_REMS: f32 = 1.0;

/// A trail step's label: the directory's own name, or `/` for the root.
fn crumb_label(path: &Path, style: PathStyle) -> String {
    style.file_name(path).map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

impl Disktree {
    /// Start again as an administrator on the root this window is headed
    /// for, with the same options, and close this window once the prompt is
    /// accepted.
    /// Declined, nothing changes. Not while a removal runs: quitting would
    /// stop a permanent delete halfway.
    pub fn restart_as_administrator(&mut self, cx: &mut Context<'_, Self>) {
        if self.remote.is_some() || self.restarting || self.run.is_some() {
            return;
        }
        self.restarting = true;
        cx.notify();
        // While a widening scan runs, the wider root is where this window
        // ends up, and the drive the offer is about: reopening the folder
        // still on screen would walk it again. `scan_root` equals
        // `root_path` for any other scan, and is stale once none runs.
        let root = if self.scan.is_some() {
            &self.scan_root
        } else {
            &self.root_path
        };
        let mut args =
            restart_args(&self.options, self.layout_options.max_depth, root);
        if let Some(preset) = self.power_choice {
            args.extend(["--power-efficiency".into(), preset.key().into()]);
        }
        // Off the UI thread: the prompt runs its own message loop, which
        // would re-enter this window while it is still being handled.
        let restart = cx.background_executor().spawn(async move {
            disktree_core::access::restart_as_administrator(&args)
        });
        cx.spawn(async move |this, cx| {
            let outcome = restart.await;
            let _ = this.update(cx, |this, cx| match outcome {
                Ok(()) => cx.quit(),
                Err(error) => {
                    this.restarting = false;
                    this.notice = Some((
                        format!("still not an administrator: {error}"),
                        Status::Warning,
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

/// The command line that reopens `root` with `options` and `depth`, in the
/// flags `parse_args` reads.
pub fn restart_args(
    options: &ScanOptions,
    depth: u32,
    root: &Path,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    for (on, flag) in [
        (options.apparent_size, "--apparent-size"),
        (options.follow_links, "--follow-links"),
        (!options.include_hidden, "--no-hidden"),
        (!options.one_filesystem, "--cross-filesystems"),
        (options.metric == Metric::Files, "--metric"),
    ] {
        if on {
            args.push(flag.into());
        }
    }
    if options.metric == Metric::Files {
        args.push("files".into());
    }
    args.push("--scan-threads".into());
    args.push(options.threads.max_threads.to_string().into());
    args.push(if options.threads.adaptive {
        "--adaptive-threads".into()
    } else {
        "--fixed-threads".into()
    });
    args.push("--thread-throughput-percent".into());
    args.push(
        format!("{:.0}", options.threads.retained_throughput * 100.0).into(),
    );
    args.push("--thread-system-cpu-percent".into());
    args.push(
        format!(
            "{:.0}",
            options.threads.system_cpu_limit.unwrap_or(0.0) * 100.0
        )
        .into(),
    );
    args.push("--depth".into());
    args.push(depth.to_string().into());
    args.push(root.as_os_str().to_owned());
    args
}

/// Now, in Unix seconds.
pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Closing the window or quitting drops the app, and with it the tree.
/// Freed in place, a home directory's tens of millions of nodes held the
/// main thread for seconds after Quit, long enough for macOS to call the
/// app not responding. On quit nothing is freed at all: the process is
/// about to exit and the system takes its memory back without walking
/// the nodes. A window closed while the app stays hands them to a
/// background thread. The find results (an entry per match and ancestor)
/// and a finished scan not yet taken from its channel can be as large,
/// so they go with the tree.
impl Drop for Disktree {
    fn drop(&mut self) {
        let held = (self.tree.take(), self.matches.take(), self.scan.take());
        if self.quitting {
            std::mem::forget(held);
        } else {
            discard(Some(held));
        }
    }
}

/// Let go of a tree, or anything holding one, off the UI thread: freeing
/// a disk's millions of nodes takes from a good part of a second to many,
/// and nothing waits on it.
fn discard<T: Send + 'static>(held: Option<T>) {
    if let Some(held) = held {
        #[cfg(test)]
        crate::marks::work::DISCARDED
            .set(crate::marks::work::DISCARDED.get() + 1);
        // When no thread can be made, the closure is dropped here instead,
        // and the tree is freed in place rather than taking the window down.
        let _ = std::thread::Builder::new().spawn(move || drop(held));
    }
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
        #[cfg(test)]
        crate::marks::work::RENDERS.set(crate::marks::work::RENDERS.get() + 1);
        if crate::ui::sync_scale(window, cx) {
            self.cache = None;
        }
        self.rem = window.rem_size().as_f32();
        self.tick_transition(window, cx);
        // The titlebar names the directory on screen, however it got there:
        // a key, a click, a rescan or a folder chosen from the menu.
        let title = if self.awaiting_root {
            "PwrDisk · Choose a source".into()
        } else {
            format!(
                "PwrDisk{} · {}",
                self.remote.as_ref().map_or_else(String::new, |host| {
                    format!(" · {host} · read-only")
                }),
                self.display_path(&self.current_path())
            )
        };
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
        let frame = self
            .chrome_pin
            .clone()
            .unwrap_or_else(|| crate::chrome::Frame::detect(window, cx));
        self.content_edges = frame.content_edges();
        let root = crate::views::root(self, &frame, window, cx);
        let theme = cx.omarchy().clone();
        crate::chrome::client_frame(&frame, root, window, &theme)
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
