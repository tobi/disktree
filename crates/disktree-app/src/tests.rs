//! End-to-end tests through the real window harness.
//!
//! These drive the application the way a person does — draw a frame, press
//! keys, type — so they catch what unit tests on the state cannot: a screen
//! that panics while painting, a binding that never fires, a removal that
//! reports success without removing anything.

use std::path::{Path, PathBuf};

use disktree_core::path_style::PathStyle;
use disktree_core::removal::RemovalMode;
use disktree_core::scan::{ScanOptions, scan};
use disktree_core::space::{SpaceInfo, Volume};
use disktree_core::treemap::Tile;
use gpui_kit::{
    Bounds, Context, Entity, Pixels, Point, TestAppContext, VisualTestContext,
    px,
};
use gpui_omarchy::Theme;

use crate::state::{Disktree, Screen};

/// Native filesystem callbacks cannot run on GPUI's deterministic scheduler.
///
/// `gpui_omarchy::init` watches `$HOME` even without an Omarchy theme. Its
/// callback wakes the deterministic executor from a foreign thread. The
/// control initializers are private in gpui-omarchy 0.1.3, so mirror their
/// bindings here and apply a fixed theme without creating a watcher at all.
fn init(cx: &mut gpui_kit::App) {
    use gpui_kit::base::actions::{Cancel, Confirm, SelectLeft, SelectRight};
    use gpui_kit::{KeyBinding, NoAction};
    use gpui_omarchy::focus::{Next, Previous};

    gpui_kit::base::init(cx);
    cx.bind_keys([
        KeyBinding::new(
            if cfg!(target_os = "macos") {
                "cmd-c"
            } else {
                "ctrl-c"
            },
            gpui_kit::base::input::Copy,
            Some("OmarchyFocusScope"),
        ),
        KeyBinding::new("tab", Next, Some("OmarchyFocusScope")),
        KeyBinding::new("shift-tab", Previous, Some("OmarchyFocusScope")),
        KeyBinding::new("left", SelectLeft, Some("OmarchyOptionGroup")),
        KeyBinding::new("h", SelectLeft, Some("OmarchyOptionGroup")),
        KeyBinding::new("right", SelectRight, Some("OmarchyOptionGroup")),
        KeyBinding::new("l", SelectRight, Some("OmarchyOptionGroup")),
        KeyBinding::new(
            "enter",
            Confirm { secondary: false },
            Some("OmarchyOptionGroup"),
        ),
        KeyBinding::new(
            "space",
            Confirm { secondary: false },
            Some("OmarchyOptionGroup"),
        ),
        KeyBinding::new("enter", NoAction, Some("OmarchyPopoverContent")),
        KeyBinding::new("space", NoAction, Some("OmarchyPopoverContent")),
        KeyBinding::new(
            "enter",
            Confirm { secondary: false },
            Some("OmarchyDatePicker"),
        ),
        KeyBinding::new(
            "space",
            Confirm { secondary: false },
            Some("OmarchyDatePicker"),
        ),
        KeyBinding::new("escape", Cancel, Some("OmarchyDatePicker")),
    ]);
    Theme::tokyo_night().apply(cx);
}

/// A small tree on disk: two directories, a nested file, and a hidden one.
///
/// The hidden directory holds the largest file, which is both the common case
/// in a home directory and the one the ranking has to get right.
fn fixture() -> tempfile::TempDir {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let root = temp.path();
    for (path, bytes) in [
        ("keep/notes.txt", 1_000_usize),
        ("junk/blob.bin", 200_000),
        ("junk/deeper/more.bin", 100_000),
        (".cache/blob.bin", 300_000),
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, vec![b'x'; bytes]).expect("write");
    }
    temp
}

/// Apparent sizes, so the assertions are about the tree and not about how the
/// filesystem rounds a small file up to a block.
fn options() -> ScanOptions {
    ScanOptions {
        apparent_size: true,
        ..ScanOptions::default()
    }
}

type Window = VisualTestContext;

/// Open a window over a real scan of `root`, without the background walk.
fn view_over<'a>(
    root: &Path,
    cx: &'a mut TestAppContext,
) -> (Entity<Disktree>, &'a mut Window) {
    let tree = scan(root, options()).expect("scan the fixture");
    let root_path = root.to_path_buf();
    let (view, cx) = cx.add_window_view(move |_, cx| {
        let mut app =
            Disktree::with_tree(root_path.clone(), tree, options(), 3, cx);
        app.removal_settings_dir = None;
        app
    });
    let focus = view.read_with(cx, |app, _| app.focus.clone());
    cx.update(|window, cx| window.focus(&focus, cx));
    (view, cx)
}

/// The fixture as an SSH session holds it. Remote paths are POSIX on every
/// client, so the root must be `/`-rooted: the temporary directory itself on
/// Unix, which keeps "nothing on disk changed" checks meaningful, and a Unix
/// path on Windows, whose temporary directory is `C:\…`.
fn remote_view_over<'a>(
    temp: &Path,
    cx: &'a mut TestAppContext,
) -> (Entity<Disktree>, &'a mut Window, PathBuf) {
    let root = if cfg!(unix) {
        temp.to_path_buf()
    } else {
        PathBuf::from("/srv/fixture")
    };
    let tree = scan(temp, options()).expect("scan the fixture");
    let root_path = root.clone();
    let (view, cx) = cx.add_window_view(move |_, cx| {
        Disktree::with_tree(root_path.clone(), tree, options(), 3, cx)
    });
    let focus = view.read_with(cx, |app, _| app.focus.clone());
    cx.update(|window, cx| window.focus(&focus, cx));
    (view, cx, root)
}

fn draw(cx: &mut Window) {
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
}

#[cfg(target_os = "macos")]
#[gpui_kit::test]
fn private_service_stores_draw_unavailable_and_partial_figures(
    cx: &mut TestAppContext,
) {
    use disktree_core::macos_services::{Usage, usage};
    use disktree_core::tree::{Draft, Metric, NodeKind, Tree, aggregate};
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    assert!(cx.debug_bounds("macos-services").is_none());
    update(&view, cx, |app, cx| {
        // Replace the fixture's tree without starting a real system scan.
        let mut history = Draft::directory(".fseventsd");
        history.read_error = true;
        let mut index = Draft::directory(".Spotlight-V100");
        index
            .children
            .push(Draft::entry("store", NodeKind::File, 8192));
        index.read_error = true;
        let mut data = Draft::directory("Data");
        data.children = vec![history, index];
        let mut volumes = Draft::directory("Volumes");
        volumes.children.push(data);
        let mut system = Draft::directory("System");
        system.children.push(volumes);
        let mut root = Draft::directory("/");
        root.children.push(system);
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);
        disktree_core::classify::classify(&mut root);
        app.tree = Some(std::sync::Arc::new(root));
        app.root_path = PathBuf::from("/");
        app.selected = None;
        app.hovered = None;
        app.options = ScanOptions::default();
        app.progress.errors = 2;
        app.progress.service_errors = 2;
        app.full_disk_access = Some(false);
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("macos-services").is_some());
    assert!(cx.debug_bounds("privacy-hint").is_none());
    read(&view, cx, |app| {
        let tree = app.tree().expect("fixture tree");
        let data = Path::new(disktree_core::space::MACOS_DATA_VOLUME);
        assert_eq!(
            usage(&app.root_path, tree, &app.options, &data.join(".fseventsd")),
            Usage::Unavailable
        );
        assert_eq!(
            usage(
                &app.root_path,
                tree,
                &app.options,
                &data.join(".Spotlight-V100")
            ),
            Usage::Incomplete(8192)
        );
    });
    update(&view, cx, |app, cx| {
        app.remote = Some("fixture-host".into());
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("macos-services").is_none());
    update(&view, cx, |app, cx| {
        app.remote = None;
        app.selected = app.tree().and_then(|tree| {
            tree.find(".Spotlight-V100").map(|(crumbs, _)| crumbs)
        });
        cx.notify();
    });
    draw(cx);
    // The guards refuse a service store, so it is never offered: the
    // inspector says why, and Space marks nothing and says so.
    assert!(cx.debug_bounds("mark-guard").is_some());
    press(cx, "space");
    assert!(read(&view, cx, |app| app.marks.is_empty()));
    assert!(read(&view, cx, |app| {
        app.notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("macOS manages"))
    }));
}

#[cfg(target_os = "macos")]
#[gpui_kit::test]
fn unreadable_service_stores_stay_quiet_without_hiding_other_privacy_errors(
    cx: &mut TestAppContext,
) {
    use disktree_core::tree::{Draft, Tree};
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        let mut tree = Draft::directory(".fseventsd");
        tree.read_error = true;
        app.tree = Some(std::sync::Arc::new(Tree::from(tree)));
        app.root_path = PathBuf::from("/System/Volumes/Data/.fseventsd");
        app.selected = None;
        app.hovered = None;
        app.progress.errors = 1;
        app.progress.service_errors = 1;
        cx.notify();
    });
    for access in [None, Some(false), Some(true)] {
        update(&view, cx, |app, cx| {
            app.full_disk_access = access;
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("macos-services").is_none());
        assert!(cx.debug_bounds("privacy-hint").is_none());
    }
    update(&view, cx, |app, cx| {
        app.full_disk_access = Some(false);
        app.progress.errors += 1;
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("privacy-hint").is_some());
    assert!(cx.debug_bounds("macos-services").is_none());
}

fn press(cx: &mut Window, keys: &str) {
    cx.simulate_keystrokes(keys);
    draw(cx);
}

fn update<R>(
    view: &Entity<Disktree>,
    cx: &mut Window,
    f: impl FnOnce(&mut Disktree, &mut Context<'_, Disktree>) -> R,
) -> R {
    view.update(cx, f)
}

fn read<R>(
    view: &Entity<Disktree>,
    cx: &Window,
    f: impl FnOnce(&Disktree) -> R,
) -> R {
    view.read_with(cx, |app, _| f(app))
}

#[gpui_kit::test]
fn remote_indexing_draws_live_activity_and_the_sampled_directory(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, _| {
        app.tree = None;
        app.remote = Some("fixture-only".into());
        app.scan = Some(disktree_core::scan::ScanHandle::spawn(
            temp.path().into(),
            options(),
        ));
        app.scan_started = Some(std::time::Instant::now());
        app.progress.files = 1234;
        app.progress.bytes = 123_456;
        app.remote_progress.stage =
            disktree_core::remote_progress::Stage::Indexing;
        app.remote_progress.directory = Some(temp.path().join("junk/deeper"));
    });
    draw(cx);
    assert!(cx.debug_bounds("indexing-progress").is_some());
    assert!(cx.debug_bounds("scan-directory").is_some());
    update(&view, cx, |app, _| {
        app.scan_error = Some("fixture scan failed".into());
    });
    draw(cx);
    assert!(cx.debug_bounds("scan-error").is_some());
    assert!(cx.debug_bounds("scan-reading").is_none());
}

#[gpui_kit::test]
fn loading_values_and_paths_do_not_move_the_sidebar(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, _| {
        app.tree = None;
        app.scan = Some(disktree_core::scan::ScanHandle::spawn(
            temp.path().into(),
            options(),
        ));
        app.scan_preview =
            Some(std::sync::Arc::new(disktree_core::scan_preview::Preview {
                entries: vec![disktree_core::scan_preview::Entry {
                    name: "junk".into(),
                    bytes: 100,
                    files: 1,
                }],
                directory: Some(temp.path().join("short")),
            }));
    });
    draw(cx);
    draw(cx);
    let panel = cx.debug_bounds("indexing-progress").unwrap();
    let files = cx.debug_bounds("indexing-files").unwrap();
    let path = cx.debug_bounds("scan-directory").unwrap();
    assert!(cx.debug_bounds("scan-preview").is_some());
    assert!(cx.debug_bounds("scan-reading").is_none());
    update(&view, cx, |app, _| {
        app.options.apparent_size = false;
        app.progress.volume_used = Some(1000);
        app.progress.files = 99_999_999;
        app.progress.bytes = u64::MAX / 2;
        app.remote = Some("fixture-only".into());
        app.remote_progress.volume_used = Some(1000);
        app.remote_progress.directory = Some(PathBuf::from(
            "/a/very/long/directory/whose/text/must/not/move/the/layout",
        ));
    });
    draw(cx);
    assert_eq!(panel, cx.debug_bounds("indexing-progress").unwrap());
    assert_eq!(files, cx.debug_bounds("indexing-files").unwrap());
    assert_eq!(path, cx.debug_bounds("scan-directory").unwrap());
    update(&view, cx, |app, _| {
        app.remote = None;
        app.progress.finalizing = true;
    });
    draw(cx);
    assert_eq!(panel, cx.debug_bounds("indexing-progress").unwrap());
    let preview = cx.debug_bounds("scan-preview").unwrap();
    cx.simulate_click(preview.center(), gpui_kit::Modifiers::none());
    press(cx, "space");
    read(&view, cx, |app| {
        assert!(app.marks.is_empty());
        assert!(app.tree.is_none());
    });
    update(&view, cx, Disktree::cancel_scan);
    draw(cx);
    assert!(cx.debug_bounds("scan-preview").is_none());
    assert!(read(&view, cx, |app| app.scan_preview.is_none()));
}

#[gpui_kit::test]
fn scan_percentage_is_bounded_and_rejects_incompatible_totals(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, _| {
        app.scan = Some(disktree_core::scan::ScanHandle::spawn(
            temp.path().into(),
            options(),
        ));
        app.options.apparent_size = false;
        app.progress.volume_used = Some(1000);
        for (bytes, expected) in [(0, 0.0), (450, 0.45), (2000, 0.99)] {
            app.progress.bytes = bytes;
            assert!((app.scan_fraction().unwrap() - expected).abs() < 0.001);
        }
        let allocated = app.options.clone();
        for incompatible in [
            ScanOptions {
                apparent_size: true,
                ..allocated
            },
            ScanOptions {
                one_filesystem: false,
                ..allocated
            },
            ScanOptions {
                include_hidden: false,
                ..allocated
            },
            ScanOptions {
                follow_links: true,
                ..allocated
            },
            ScanOptions {
                max_depth: Some(1),
                ..allocated
            },
        ] {
            app.options = incompatible;
            assert!(app.scan_fraction().is_none());
        }
        app.options = allocated;
        app.progress.finalizing = true;
        assert!(app.scan_fraction().is_none());
        app.progress.finalizing = false;
        app.progress.volume_used = None;
        assert!(app.scan_fraction().is_none());
        app.remote = Some("fixture-only".into());
        app.remote_progress.volume_used = Some(1000);
        assert!((app.scan_fraction().unwrap() - 0.99).abs() < 0.001);
        app.remote_progress.stage =
            disktree_core::remote_progress::Stage::Finalizing;
        assert!(app.scan_fraction().is_none());
        app.remote = None;
        app.scan = None;
        app.progress.volume_used = Some(1000);
        assert!(app.scan_fraction().is_none());
    });
}

#[gpui_kit::test]
fn indexing_keeps_free_space_visible_in_short_zoomed_windows(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, _| {
        app.tree = None;
        app.scan = Some(disktree_core::scan::ScanHandle::spawn(
            temp.path().into(),
            options(),
        ));
        app.options.apparent_size = false;
        app.progress.volume_used = Some(1000);
        app.progress.bytes = 650;
        app.space = Some(SpaceInfo {
            total: 1000,
            free: 350,
            available: 350,
        });
    });
    for (width, height, rem) in
        [(1200., 700., 20.), (1400., 600., 24.), (1440., 900., 16.)]
    {
        cx.simulate_resize(gpui_kit::size(px(width), px(height)));
        cx.update(|window, _| window.set_rem_size(px(rem)));
        draw(cx);
        draw(cx);
        let disk = cx.debug_bounds("disk-section").unwrap();
        assert!(
            disk.bottom() <= px(height),
            "{width}×{height}, {rem}: {disk:?}"
        );
        assert!(disk.size.height >= px(rem * 4.0), "{disk:?}");
        let scroll = cx.debug_bounds("side-panel-scroll").unwrap();
        assert!(scroll.bottom() <= disk.origin.y, "{scroll:?}, {disk:?}");
        let progress = cx.debug_bounds("indexing-progress").unwrap();
        cx.simulate_event(gpui_kit::ScrollWheelEvent {
            position: scroll.center(),
            delta: gpui_kit::ScrollDelta::Pixels(Point::new(
                px(0.),
                px(-1000.),
            )),
            modifiers: gpui_kit::Modifiers::none(),
            touch_phase: gpui_kit::TouchPhase::Moved,
        });
        draw(cx);
        let scrolled = cx.debug_bounds("indexing-progress").unwrap();
        if progress.bottom() > scroll.bottom() {
            assert!(scrolled.origin.y < progress.origin.y);
        }
        // The stop button is the last progress row; it must be reachable.
        assert!(
            scrolled.bottom() <= scroll.bottom(),
            "{width}×{height}, rem {rem}: {scrolled:?}, viewport {scroll:?}"
        );
        assert!(scrolled.bottom() > scroll.origin.y, "{scrolled:?}");
        assert_eq!(disk, cx.debug_bounds("disk-section").unwrap());
        update(&view, cx, |app, _| app.progress.finalizing = true);
        draw(cx);
        assert_eq!(disk, cx.debug_bounds("disk-section").unwrap());
        update(&view, cx, |app, _| app.progress.finalizing = false);
    }
}

#[gpui_kit::test]
fn narrow_indexing_keeps_a_preview_and_compact_progress_in_bounds(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, _| {
        app.tree = None;
        app.scan = Some(disktree_core::scan::ScanHandle::spawn(
            temp.path().into(),
            options(),
        ));
        app.scan_preview =
            Some(std::sync::Arc::new(disktree_core::scan_preview::Preview {
                entries: vec![disktree_core::scan_preview::Entry {
                    name: "junk".into(),
                    bytes: 100,
                    files: 1,
                }],
                directory: None,
            }));
    });
    cx.simulate_resize(gpui_kit::size(px(900.), px(600.)));
    cx.update(|window, _| window.set_rem_size(px(28.)));
    draw(cx);
    draw(cx);
    let compact = cx.debug_bounds("indexing-compact").unwrap();
    assert!(compact.origin.x >= px(0.) && compact.right() <= px(900.));
    assert!(compact.origin.y >= px(0.) && compact.bottom() <= px(600.));
    let preview = cx.debug_bounds("scan-preview").unwrap();
    assert!(preview.size.height > px(50.), "{preview:?}");
    assert!(preview.bottom() <= compact.origin.y, "{preview:?}");
}

#[gpui_kit::test]
fn startup_chooser_never_scans_before_explicit_acceptance(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let (view, cx) =
        cx.add_window_view(|_, cx| Disktree::choose(options(), 3, None, cx));
    cx.run_until_parked();
    draw(cx);
    assert!(cx.debug_bounds("choose-root-idle").is_some());
    assert!(cx.debug_bounds("source-switch").is_some());
    read(&view, cx, |app| {
        assert!(app.awaiting_root);
        assert_eq!(app.scan_epoch, 0);
        assert!(app.scan.is_none() && app.remote_scan.is_none());
    });
    press(cx, "escape");
    assert!(!read(&view, cx, |app| app.volumes_open));
    press(cx, "r");
    assert!(read(&view, cx, |app| app.volumes_open
        && app.awaiting_root
        && app.scan.is_none()));
}

#[gpui_kit::test]
fn title_brand_opens_source_picker(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let brand = cx.debug_bounds("strip-brand").expect("title brand");
    cx.simulate_click(brand.center(), gpui_kit::Modifiers::none());
    draw(cx);
    assert!(read(&view, cx, |app| app.volumes_open));
    assert!(cx.debug_bounds("source-picker").is_some());
}

#[gpui_kit::test]
fn entering_a_directory_scans_only_that_explicit_root(cx: &mut TestAppContext) {
    cx.update(init);
    let selected = fixture();
    let other = fixture();
    let (view, cx) =
        cx.add_window_view(|_, cx| Disktree::choose(options(), 3, None, cx));
    cx.run_until_parked();
    update(&view, cx, |app, _| {
        app.volumes = vec![Volume {
            point: other.path().into(),
            device: None,
            space: None,
        }];
        app.volumes_loading = false;
    });
    draw(cx);
    let input = cx.debug_bounds("source-path").expect("path editor");
    cx.simulate_click(input.center(), gpui_kit::Modifiers::none());
    cx.simulate_input(selected.path().to_str().unwrap());
    assert_eq!(
        update(&view, cx, |app, cx| app
            .picker_path
            .as_ref()
            .unwrap()
            .read(cx)
            .value()
            .to_string()),
        selected.path().to_str().unwrap()
    );
    press(cx, "enter");
    assert_eq!(
        read(&view, cx, |app| app.root_path.clone()),
        dunce::canonicalize(selected.path()).unwrap()
    );
    assert!(!read(&view, cx, |app| app.awaiting_root || app.volumes_open));
    finish_scan(&view, cx);
}

#[gpui_kit::test]
fn invalid_ssh_input_does_not_start_a_scan_or_change_the_source(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let (view, cx) =
        cx.add_window_view(|_, cx| Disktree::choose(options(), 3, None, cx));
    cx.run_until_parked();
    draw(cx);
    choose_ssh(cx);
    let input = cx.debug_bounds("source-host").expect("host editor");
    cx.simulate_click(input.center(), gpui_kit::Modifiers::none());
    cx.simulate_input("invalid host");
    press(cx, "enter");
    read(&view, cx, |app| {
        assert!(app.volumes_error.is_some());
        assert!(app.remote.is_none());
        assert!(app.scan.is_none() && app.remote_scan.is_none());
        assert_eq!(app.scan_epoch, 0);
    });
    press(cx, "escape");
    assert!(!read(&view, cx, |app| app.volumes_open));
}

#[gpui_kit::test]
fn switching_from_remote_to_local_clears_machine_specific_marks(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let target = fixture();
    let (view, cx, root) = remote_view_over(temp.path(), cx);
    let junk = PathStyle::Posix.join(&root, "junk");
    update(&view, cx, |app, cx| {
        app.remote = Some("old-source".into());
        app.remote_identity = Some(disktree_core::identity::HostIdentity {
            hostname: "old-source".into(),
            machine_id: None,
        });
        let crumbs = app.crumbs_for_path(&junk).unwrap();
        app.select(Some(crumbs), cx);
        app.toggle_mark_selected(cx);
        assert!(!app.marks.is_empty());
        app.volumes_open = true;
        app.volumes_source = None;
        app.choose_source_root(target.path().to_path_buf(), cx);
        assert!(app.remote.is_none() && app.remote_identity.is_none());
        assert!(app.marks.is_empty());
        assert!(app.tree.is_none());
    });
    finish_scan(&view, cx);
}

#[gpui_kit::test]
fn chooser_fits_a_small_window_at_large_interface_zoom(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let (_, cx) =
        cx.add_window_view(|_, cx| Disktree::choose(options(), 3, None, cx));
    cx.run_until_parked();
    cx.simulate_resize(gpui_kit::size(px(900.), px(600.)));
    for rem in [16., 28., 32.] {
        cx.update(|window, _| window.set_rem_size(px(rem)));
        draw(cx);
        let bounds = cx
            .debug_bounds("source-picker")
            .expect("scrollable chooser");
        assert!(bounds.size.width > px(200.), "{bounds:?}");
        assert!(
            bounds.origin.x >= px(0.) && bounds.right() <= px(900.),
            "{bounds:?}"
        );
        assert!(
            bounds.origin.y >= px(0.) && bounds.bottom() <= px(600.),
            "{bounds:?}"
        );
    }
}

#[gpui_kit::test]
fn editing_a_host_never_retargets_an_existing_scan(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, Disktree::open_volumes);
    cx.run_until_parked();
    draw(cx);
    let before = read(&view, cx, |app| app.scan_epoch);
    choose_ssh(cx);
    let input = cx.debug_bounds("source-host").unwrap();
    cx.simulate_click(input.center(), gpui_kit::Modifiers::none());
    cx.simulate_input("new-host-not-connected");
    update(&view, cx, |app, cx| {
        app.choose_source_root(temp.path().into(), cx);
        assert!(app.picker_error.is_some());
        assert!(app.remote.is_none());
        assert_eq!(app.scan_epoch, before);
        assert!(app.tree.is_some());
    });
    press(cx, "escape");
    assert!(!read(&view, cx, |app| app.volumes_open));
}

#[gpui_kit::test]
fn a_directory_error_can_be_corrected_without_reconnecting(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) =
        cx.add_window_view(|_, cx| Disktree::choose(options(), 3, None, cx));
    cx.run_until_parked();
    update(&view, cx, |app, cx| {
        app.choose_source_root(PathBuf::from("relative"), cx);
        assert!(app.picker_error.is_some());
        assert!(app.awaiting_root);
        app.choose_source_root(temp.path().into(), cx);
        assert!(app.picker_error.is_none());
        assert!(!app.awaiting_root);
    });
    finish_scan(&view, cx);
}

#[gpui_kit::test]
fn stale_local_discovery_cannot_overwrite_a_new_source_error(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let (view, cx) =
        cx.add_window_view(|_, cx| Disktree::choose(options(), 3, None, cx));
    // The old discovery is still queued. The new host fails validation
    // without any network connection; only its error may remain.
    update(&view, cx, |app, cx| {
        app.load_volume_source(Some("-invalid".into()), cx);
    });
    cx.run_until_parked();
    draw(cx);
    read(&view, cx, |app| {
        assert_eq!(app.volumes_source.as_deref(), Some("-invalid"));
        assert!(app.volumes_error.is_some());
        assert!(app.volumes.is_empty());
        assert!(app.remote.is_none());
    });
    update(&view, cx, |app, cx| {
        app.close_volumes(cx);
        assert!(!app.poll_roots_once(1, cx));
    });
}

#[gpui_kit::test]
fn remote_root_acceptance_never_stats_the_path_locally(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    // A Unix host sends a POSIX root on every client. Its absence here
    // proves that accepting it skips local directory validation.
    let remote_root = if cfg!(unix) {
        temp.path().join("only-on-the-remote-host")
    } else {
        PathBuf::from("/srv/pwrdisk-fixture/only-on-the-remote-host")
    };
    assert!(
        PathStyle::Posix.is_absolute(&remote_root) && !remote_root.exists()
    );
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        let crumbs = app.crumbs_for_path(&temp.path().join("junk")).unwrap();
        app.select(Some(crumbs), cx);
        app.toggle_mark_selected(cx);
        app.volumes_open = true;
        // An intentionally invalid SSH destination ensures acceptance cannot
        // contact any machine. The listing is a decoded fixture, not a walk.
        app.volumes_source = Some("-test-fixture".into());
        app.finish_volume_listing(
            Ok((
                vec![Volume {
                    point: remote_root.clone(),
                    device: Some("/dev/remote-disk".into()),
                    space: None,
                }],
                None,
            )),
            cx,
        );
    });
    draw(cx);
    update(&view, cx, |app, cx| {
        app.choose_volume(cx);
        assert_eq!(app.remote.as_deref(), Some("-test-fixture"));
        assert_eq!(app.root_path, remote_root);
        assert!(app.picker_error.is_none());
        assert!(app.marks.is_empty() && app.remote_identity.is_none());
        assert!(
            app.tree.is_none()
                && app.scan.is_none()
                && app.remote_scan.is_none()
        );
        assert!(
            app.scan_error
                .as_ref()
                .is_some_and(|error| error.contains("invalid SSH"))
        );
    });
}

#[gpui_kit::test]
fn repointing_an_ssh_alias_never_reuses_old_machine_marks(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx, root) = remote_view_over(temp.path(), cx);
    let junk = PathStyle::Posix.join(&root, "junk");
    update(&view, cx, |app, cx| {
        let old = disktree_core::identity::HostIdentity {
            hostname: "same-name".into(),
            machine_id: Some("machine-a".into()),
        };
        app.remote = Some("stable-alias".into());
        app.update_remote_identity(Some(old.clone()));
        let crumbs = app.crumbs_for_path(&junk).unwrap();
        app.select(Some(crumbs), cx);
        app.toggle_mark_selected(cx);
        app.update_remote_identity(Some(old.clone()));
        assert!(!app.marks.is_empty(), "same machine keeps its marks");
        app.update_remote_identity(Some(
            disktree_core::identity::HostIdentity {
                hostname: old.hostname,
                machine_id: Some("machine-b".into()),
            },
        ));
        assert!(app.marks.is_empty(), "same alias is not proof of identity");
        app.toggle_mark_selected(cx);
        assert!(!app.marks.is_empty());
        app.update_remote_identity(None);
        assert!(app.marks.is_empty(), "lost identity also fails closed");
    });
}

#[gpui_kit::test]
fn remote_review_never_deletes_even_through_direct_actions(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx, root) = remote_view_over(temp.path(), cx);
    let junk = PathStyle::Posix.join(&root, "junk");
    update(&view, cx, |app, cx| {
        app.remote = Some("dev".into());
        let crumbs = app.crumbs_for_path(&junk).unwrap();
        app.select(Some(crumbs), cx);
        app.toggle_mark_selected(cx);
        app.screen = Screen::Review;
    });
    draw(cx);
    press(cx, "enter");
    press(cx, "p");
    press(cx, "m");
    update(&view, cx, |app, cx| {
        app.commit(false, cx);
        app.confirm_delete(cx);
        app.begin_removal(cx);
        assert!(!app.confirm_open);
        assert!(app.run.is_none());
        assert_eq!(app.screen, Screen::Review);
        let selection = app.plan();
        assert_eq!(selection.targets.len(), 1);
        assert!(selection.bytes() > 0);
        assert_eq!(selection.reclaim(), 0);
        assert_eq!(selection.foreign(), 0);
        assert_eq!(selection.unattributed(), selection.bytes());
        app.open_volumes(cx);
        app.restart_as_administrator(cx);
        assert!(!app.volumes_open);
        assert!(!app.restarting);
    });
    draw(cx);
    assert!(temp.path().join("junk/blob.bin").exists());
    assert!(temp.path().join("keep/notes.txt").exists());
}

#[gpui_kit::test]
fn a_failed_remote_scan_stops_showing_the_reading_screen(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, _| {
        app.tree = None;
        app.scan = None;
        app.remote = Some("dev".into());
        app.scan_error =
            Some("SSH dev: Permission denied (publickey,password).".into());
    });
    draw(cx);
    assert!(cx.debug_bounds("scan-error").is_some());
    assert!(cx.debug_bounds("scan-reading").is_none());
}

#[gpui_kit::test]
fn the_window_draws_a_treemap_with_tiles(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    let bounds: Bounds<Pixels> =
        cx.debug_bounds("disktree-root").expect("root element");
    assert!(bounds.size.width > px(0.) && bounds.size.height > px(0.));

    let (size, tiles) = update(&view, cx, |app, _| {
        (
            app.treemap_size.get(),
            app.layout().map(<[Tile]>::len).unwrap_or_default(),
        )
    });
    assert!(size.width > px(0.), "treemap width {size:?}");
    assert!(size.height > px(0.), "treemap height {size:?}");
    assert!(tiles >= 3, "tiles laid out: {tiles}");
}

#[gpui_kit::test]
fn the_first_scan_shows_what_it_is_doing_then_the_treemap(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let root = temp.path().to_path_buf();
    // Through the real constructor, so this is the path every first run takes.
    let (view, cx) =
        cx.add_window_view(move |_, cx| Disktree::new(root, options(), 3, cx));
    draw(cx);

    // Nothing is known yet, so the viewport counts the walk instead of showing
    // an empty mosaic.
    assert!(read(&view, cx, |app| app.tree().is_none()));
    assert!(cx.debug_bounds("disktree-root").is_some());

    finish_scan(&view, cx);
    draw(cx);

    let (tiles, selected, hidden_present) = update(&view, cx, |app, _| {
        (
            app.layout().map(<[Tile]>::len).unwrap_or_default(),
            app.selected.is_some(),
            app.tree().is_some_and(|tree| {
                tree.children().iter().any(|c| c.name().starts_with('.'))
            }),
        )
    });
    assert!(tiles >= 3, "tiles after the scan: {tiles}");
    assert!(
        selected,
        "the largest entry is selected for the selection line"
    );
    assert!(hidden_present, "hidden directories are part of the tree");
}

#[gpui_kit::test]
fn keys_walk_the_tree_and_mark_what_is_selected(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    // Largest first: .cache holds the biggest file.
    let (selected, name) = read(&view, cx, |app| {
        (
            app.selected.clone(),
            app.node_at(&[0]).map(|node| node.name().to_string()),
        )
    });
    assert_eq!(selected, Some(vec![0]));
    assert_eq!(name.as_deref(), Some(".cache"));

    press(cx, "space");
    let (marks, bytes) =
        read(&view, cx, |app| (app.marks.len(), app.plan().bytes()));
    assert_eq!(marks, 1);
    assert_eq!(bytes, 300_000, "the whole hidden directory");

    // Enter descends, Escape comes back out.
    press(cx, "enter");
    assert!(!read(&view, cx, |app| app.crumbs.is_empty()));
    press(cx, "escape");
    press(cx, "escape");
    assert!(read(&view, cx, |app| app.crumbs.is_empty()));

    // Space twice more leaves the mark as it was.
    press(cx, "space");
    press(cx, "space");
    let marks = read(&view, cx, |app| app.marks.items().to_vec());
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].bytes, 300_000);
}

#[gpui_kit::test]
fn a_permanent_deletion_asks_in_an_alert_dialog_then_removes(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    // Mark "junk" directly, which is what clicking its tile would do, and
    // choose the permanent path: the trash path would reach the real trash.
    update(&view, cx, |app, cx| {
        app.removal_mode = RemovalMode::Permanent;
        let junk = app
            .tree()
            .and_then(|tree| {
                tree.children()
                    .iter()
                    .position(|child| child.name() == "junk")
                    .map(|index| vec![index])
            })
            .expect("the junk directory");
        app.select(Some(junk.clone()), cx);
        app.toggle_mark(&junk, cx);
        assert_eq!(app.plan().bytes(), 300_000);
    });

    press(cx, "c");
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Review);

    // Enter on the review screen opens the alert dialog; nothing is deleted.
    press(cx, "enter");
    assert!(read(&view, cx, |app| app.confirm_open));
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Review);
    assert!(
        temp.path().join("junk").exists(),
        "nothing has happened yet"
    );

    // Enter in the dialog is its confirm action.
    press(cx, "enter");
    assert!(!read(&view, cx, |app| app.confirm_open));
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Running);

    // The removal runs on a real worker thread, so the test drives the same
    // poll the UI's ticker drives rather than waiting on the test clock.
    let epoch = read(&view, cx, |app| app.run_epoch);
    let mut finished = false;
    for _ in 0..400 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        finished = update(&view, cx, |app, cx| {
            app.poll_removal_once(epoch, cx);
            app.screen == Screen::Done
        });
        if finished {
            break;
        }
    }
    assert!(finished, "the removal finished");
    draw(cx);

    let summary = read(&view, cx, |app| app.run_list.totals());
    assert_eq!(summary.removed, 1, "{summary:?}");
    assert_eq!(summary.failed, 0, "{summary:?}");
    assert!(
        read(&view, cx, |app| app.marks.is_empty()),
        "the list clears after the run"
    );
    assert!(!temp.path().join("junk").exists(), "junk is gone");
    assert!(
        temp.path().join("keep/notes.txt").exists(),
        "an unmarked directory is untouched"
    );
    assert!(
        temp.path().join(".cache/blob.bin").exists(),
        "an unmarked hidden directory is untouched"
    );
}

/// A subdivided directory keeps a band at the top for its own name, and its
/// children start below it. That is what makes the inner tiles selectable: the
/// parent's name never covers them.
#[gpui_kit::test]
fn a_parents_name_gets_its_own_band_above_its_children(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    let (band, lowest_child_top, bands_in_mosaic) =
        update(&view, cx, |app, _| {
            let tiles = app.layout().map(<[Tile]>::to_vec).unwrap_or_default();
            let parent = tiles
                .iter()
                .find(|tile| tile.crumbs() == [1])
                .expect("the junk directory");
            let band = parent
                .header
                .expect("junk is subdivided, so it keeps a band");
            let children_top = tiles
                .iter()
                .filter(|tile| {
                    tile.crumbs().starts_with(&[1]) && tile.crumbs().len() > 1
                })
                .map(|tile| tile.rect.y)
                .fold(f32::INFINITY, f32::min);
            let mosaic = app.prepare();
            let bands_in_mosaic = mosaic
                .labels
                .iter()
                .filter(|label| label.header.is_some())
                .count();
            (band, children_top, bands_in_mosaic)
        });

    assert!(
        lowest_child_top >= band.bottom() - f32::EPSILON,
        "children start at {} but the band ends at {}",
        lowest_child_top,
        band.bottom()
    );
    assert!(
        band.h >= 8.0,
        "a band has to be tall enough to read: {}",
        band.h
    );
    assert!(
        bands_in_mosaic >= 1,
        "the parent's own label is placed in a band"
    );
}

#[gpui_kit::test]
fn hovering_reports_the_tile_under_the_pointer(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    let (origin, biggest, centre) = update(&view, cx, |app, _| {
        // A leaf tile, so "the deepest tile under the pointer" is unambiguous.
        let biggest = app
            .layout()
            .and_then(|tiles| {
                tiles
                    .iter()
                    .filter(|tile| {
                        let crumbs = tile.crumbs();
                        !tiles.iter().any(|other| {
                            other.crumbs().len() > crumbs.len()
                                && other.crumbs().starts_with(crumbs)
                        })
                    })
                    .max_by(|left, right| {
                        left.rect.area().total_cmp(&right.rect.area())
                    })
                    .map(|tile| tile.crumbs().to_vec())
            })
            .expect("a tile to hover");
        let rect = app.tile_rect(&biggest).expect("a rectangle");
        let screen = app.view.project(rect);
        (
            app.treemap_origin.get(),
            biggest,
            (screen.x + screen.w / 2.0, screen.y + screen.h / 2.0),
        )
    });

    let (x, y) = centre;
    cx.simulate_mouse_move(
        Point::new(origin.x + px(x), origin.y + px(y)),
        None,
        gpui_kit::Modifiers::default(),
    );
    draw(cx);

    let hovered = read(&view, cx, |app| app.hovered.clone());
    assert_eq!(hovered.as_deref(), Some(biggest.as_slice()));
}

/// Press the left button over the drawn tile for `path`, as the
/// `count`th press of a click sequence, and draw the frame after it.
fn press_on(
    view: &Entity<Disktree>,
    cx: &mut Window,
    path: &Path,
    count: usize,
) {
    let at = update(view, cx, |app, _| {
        let crumbs = app.crumbs_for_path(path).expect("a path in the tree");
        let rect = app.tile_rect(&crumbs).expect("the tile is drawn");
        let screen = app.view.project(rect);
        let (x, y) = (screen.x + screen.w / 2.0, screen.y + screen.h / 2.0);
        assert_eq!(
            app.tile_at(x, y),
            Some(crumbs),
            "the press lands on that tile"
        );
        let origin = app.treemap_origin.get();
        Point::new(origin.x + px(x), origin.y + px(y))
    });
    cx.simulate_event(gpui_kit::MouseDownEvent {
        button: gpui_kit::MouseButton::Left,
        position: at,
        modifiers: gpui_kit::Modifiers::none(),
        click_count: count,
        first_mouse: false,
    });
    cx.simulate_event(gpui_kit::MouseUpEvent {
        button: gpui_kit::MouseButton::Left,
        position: at,
        modifiers: gpui_kit::Modifiers::none(),
        click_count: count,
    });
    draw(cx);
}

/// Where the window is and what is selected, as paths.
fn place(view: &Entity<Disktree>, cx: &Window) -> (PathBuf, Option<PathBuf>) {
    read(view, cx, |app| {
        (
            app.current_path(),
            app.selected
                .as_deref()
                .and_then(|crumbs| app.path_at(crumbs)),
        )
    })
}

#[gpui_kit::test]
fn a_double_click_on_a_nested_tile_opens_one_level(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let more = temp.path().join("junk/deeper/more.bin");

    // Press 1 selects the deepest tile under the pointer and opens nothing.
    press_on(&view, cx, &more, 1);
    assert_eq!(
        place(&view, cx),
        (temp.path().to_path_buf(), Some(more.clone()))
    );

    // Press 2 opens one level toward it, not the three it is drawn down.
    press_on(&view, cx, &more, 2);
    assert_eq!(
        place(&view, cx),
        (temp.path().join("junk"), Some(more.clone())),
        "one level, with the tile still selected"
    );

    // A triple click is still the same gesture.
    press_on(&view, cx, &more, 3);
    assert_eq!(place(&view, cx).0, temp.path().join("junk"));

    // A new click on the selection opens the next level.
    press_on(&view, cx, &more, 1);
    assert_eq!(
        place(&view, cx),
        (temp.path().join("junk/deeper"), Some(more))
    );
}

#[gpui_kit::test]
fn a_double_click_on_the_selection_opens_one_level(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let more = temp.path().join("junk/deeper/more.bin");

    // Click once: it is selected, and the hover card says what a second
    // click does.
    press_on(&view, cx, &more, 1);
    let opens = update(&view, cx, |app, _| {
        app.hovered = app.selected.clone();
        app.click_opens_hovered()
    });
    assert!(opens, "a click on the selection opens");

    // Then double-click it. Press 1 opens a level; press 2 lands on the
    // level press 1 opened and used to open whatever was under it there.
    press_on(&view, cx, &more, 1);
    assert_eq!(place(&view, cx).0, temp.path().join("junk"));
    let at_junk = read(&view, cx, |app| app.crumbs.clone());
    cx.simulate_event(gpui_kit::MouseDownEvent {
        button: gpui_kit::MouseButton::Left,
        position: read(&view, cx, |app| {
            let origin = app.treemap_origin.get();
            let size = app.treemap_size.get();
            Point::new(
                origin.x + size.width / 2.0,
                origin.y + size.height / 2.0,
            )
        }),
        modifiers: gpui_kit::Modifiers::none(),
        click_count: 2,
        first_mouse: false,
    });
    draw(cx);
    assert_eq!(
        read(&view, cx, |app| app.crumbs.clone()),
        at_junk,
        "the second press of the gesture is ignored"
    );
}

#[gpui_kit::test]
fn a_click_on_a_file_at_the_top_selects_it_and_opens_nothing(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    std::fs::write(temp.path().join("top.bin"), vec![b'x'; 150_000])
        .expect("write");
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let top = temp.path().join("top.bin");
    press_on(&view, cx, &top, 1);
    press_on(&view, cx, &top, 2);
    press_on(&view, cx, &top, 1);
    assert_eq!(place(&view, cx), (temp.path().to_path_buf(), Some(top)));
}

#[gpui_kit::test]
fn right_click_selects_the_tile_and_reveals_it(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    let (at, target, path) = update(&view, cx, |app, _| {
        let biggest = app
            .layout()
            .and_then(|tiles| {
                tiles
                    .iter()
                    .max_by(|left, right| {
                        left.rect.area().total_cmp(&right.rect.area())
                    })
                    .map(|tile| tile.crumbs().to_vec())
            })
            .expect("a tile");
        let rect = app.tile_rect(&biggest).expect("a rectangle");
        let screen = app.view.project(rect);
        let (x, y) = (screen.x + screen.w / 2.0, screen.y + screen.h / 2.0);
        // The deepest tile there, which is what a click resolves to.
        let target = app.tile_at(x, y).expect("a tile under the pointer");
        let path = app.path_at(&target).expect("a path");
        let origin = app.treemap_origin.get();
        (Point::new(origin.x + px(x), origin.y + px(y)), target, path)
    });

    // The test platform cannot open a file manager, so take the path away
    // first: the reveal then reports it instead of reaching the platform.
    if path.is_dir() {
        std::fs::remove_dir_all(&path).expect("remove");
    } else {
        std::fs::remove_file(&path).expect("remove");
    }

    cx.simulate_mouse_down(
        at,
        gpui_kit::MouseButton::Right,
        gpui_kit::Modifiers::none(),
    );
    cx.simulate_mouse_up(
        at,
        gpui_kit::MouseButton::Right,
        gpui_kit::Modifiers::none(),
    );
    draw(cx);

    let (selected, notice) = read(&view, cx, |app| {
        (
            app.selected.clone(),
            app.notice.as_ref().map(|(text, _)| text.clone()),
        )
    });
    assert_eq!(selected, Some(target), "the clicked tile is selected");
    assert!(
        notice.is_some_and(|text| text.contains("no longer on disk")),
        "the reveal ran for it"
    );
}

#[gpui_kit::test]
fn find_field_never_draws_over_the_legend(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (_view, cx) = view_over(temp.path(), cx);
    draw(cx);
    press(cx, "/");
    // Matches bring the longest hint, "enter shows only these".
    cx.simulate_input("BLOB");
    cx.run_until_parked();
    // The minimum window width up to a wide one, at 100%, 150% and 200%.
    for width in [900., 1100., 1440.] {
        for rem in [16., 24., 32.] {
            cx.simulate_resize(gpui_kit::size(px(width), px(900.)));
            cx.update(|window, _| window.set_rem_size(px(rem)));
            draw(cx);
            let at = format!("{width}x900 at rem {rem}");
            let field = cx.debug_bounds("find-field").expect("find drawn");
            let legend = cx.debug_bounds("legend").expect("legend drawn");
            // Half a pixel for rounding where two edges meet. The field
            // clips what it holds, so its own bounds are what is drawn.
            // The legend has its own row under the field's.
            assert!(
                field.bottom() <= legend.top() + px(0.5)
                    || field.right() <= legend.left() + px(0.5),
                "{at}: field {field:?} overlaps legend {legend:?}"
            );
            assert!(
                field.right() <= px(width) + px(0.5),
                "{at}: the field stays in the window: {field:?}"
            );
        }
    }
    // Where the overlap was seen, and narrower: the whole hint reads, and
    // the legend, on its own row now, is still drawn.
    for width in [1000., 1440.] {
        cx.simulate_resize(gpui_kit::size(px(width), px(900.)));
        cx.update(|window, _| window.set_rem_size(px(16.)));
        draw(cx);
        let hint = cx.debug_bounds("find-hint").expect("hint drawn");
        let field = cx.debug_bounds("find-field").expect("find drawn");
        let legend = cx.debug_bounds("legend").expect("legend drawn");
        assert!(
            hint.size.width > px(100.)
                && hint.right() <= field.right() + px(0.5),
            "{width}: the whole hint fits in the field: {hint:?} {field:?}"
        );
        assert!(legend.size.width > px(0.), "{width}: legend {legend:?}");
    }
}

#[gpui_kit::test]
fn typing_filters_live_and_enter_shows_only_the_matches(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let names_drawn = |view: &Entity<Disktree>, cx: &mut Window| {
        update(view, cx, |app, _| {
            let tiles = app.layout().map(<[Tile]>::to_vec).unwrap_or_default();
            tiles
                .iter()
                .filter_map(|tile| app.node_at(tile.crumbs()))
                .map(|node| node.name().to_string())
                .collect::<Vec<_>>()
        })
    };

    press(cx, "/");
    cx.simulate_input("BLOB");
    // The search runs off the UI thread; let it land.
    cx.run_until_parked();
    draw(cx);
    // Live: two matches, nothing hidden yet, the rest only dimmed.
    let (count, applied, keep_filtered) = read(&view, cx, |app| {
        let matches = app.matches.as_deref().expect("matching as it is typed");
        let keep = app.tree().and_then(|tree| {
            tree.children()
                .iter()
                .position(|child| child.name() == "keep")
        });
        (
            matches.count,
            app.filter_applied,
            keep.is_some_and(|index| matches.keep(&[index]).is_none()),
        )
    });
    assert_eq!(count, 2, "junk/blob.bin and .cache/blob.bin");
    assert!(!applied);
    assert!(keep_filtered, "keep holds no blob");
    // Dimmed, not hidden: a non-match is still drawn while typing.
    assert!(names_drawn(&view, cx).contains(&"deeper".to_string()));
    // A match says so without colour (WCAG 1.4.1): the part of its name the
    // find text matched is underlined, whatever case it was typed in. A
    // non-match keeps its plain name, with no word added to every tile.
    let mosaic = update(&view, cx, |app, _| app.prepare());
    let matched: Vec<_> = mosaic
        .labels
        .iter()
        .filter_map(|label| Some((label, label.found.clone()?)))
        .collect();
    assert!(!matched.is_empty(), "a blob is labelled");
    for (label, found) in &matched {
        assert_eq!(label.text, "blob.bin");
        assert_eq!(&label.text[found.clone()], "blob");
    }
    let unmatched: Vec<_> = mosaic
        .labels
        .iter()
        .filter(|label| label.found.is_none())
        .map(|label| label.text.as_str())
        .collect();
    assert!(
        unmatched.iter().all(|text| !text.starts_with('[')),
        "{unmatched:?}"
    );

    // Enter: only the matches, and the largest selected for marking.
    press(cx, "enter");
    let drawn = names_drawn(&view, cx);
    assert!(read(&view, cx, |app| app.filter_applied));
    assert!(
        !drawn.iter().any(|name| name == "keep" || name == "deeper"),
        "{drawn:?}"
    );
    assert_eq!(
        drawn.iter().filter(|name| *name == "blob.bin").count(),
        2,
        "{drawn:?}"
    );
    let selected = read(&view, cx, |app| {
        app.selected
            .as_deref()
            .and_then(|crumbs| app.path_at(crumbs))
    });
    assert_eq!(selected, Some(temp.path().join(".cache/blob.bin")));

    // Escape gives everything back.
    press(cx, "escape");
    assert!(read(&view, cx, |app| app.matches.is_none()
        && !app.filter_applied));
    assert!(names_drawn(&view, cx).contains(&"deeper".to_string()));
}

#[gpui_kit::test]
fn the_help_overlay_opens_and_closes(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    press(cx, "?");
    assert!(read(&view, cx, |app| app.show_help));
    press(cx, "escape");
    assert!(!read(&view, cx, |app| app.show_help));
}

/// No dialog draws the treemap's hover card behind it.
#[gpui_kit::test]
fn the_tooltip_hides_under_an_overlay(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let origin = read(&view, cx, |app| app.treemap_origin.get());
    let at = origin + Point::new(px(40.), px(40.));
    cx.simulate_mouse_move(at, None, gpui_kit::Modifiers::default());
    draw(cx);
    assert!(cx.debug_bounds("cursor-tooltip").is_some(), "a tooltip");

    // The overlays' backdrop takes the mouse, so the treemap drops its
    // hover; the volumes and delete dialogs leave it, and rely on the rule.
    let dialogs: [OpenOverlay; 2] =
        [|app| app.show_help = true, |app| app.volumes_open = true];
    for open in dialogs {
        update(&view, cx, |app, cx| {
            open(app);
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("cursor-tooltip").is_none(), "hidden");
        update(&view, cx, |app, cx| {
            app.show_help = false;
            app.volumes_open = false;
            cx.notify();
        });
        cx.simulate_mouse_move(at, None, gpui_kit::Modifiers::default());
        draw(cx);
        assert!(cx.debug_bounds("cursor-tooltip").is_some(), "back");
    }
}

/// The menu opens one overlay at a time: the other one closes.
#[gpui_kit::test]
fn the_menu_shows_one_overlay_at_a_time(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    cx.dispatch_action(crate::app_menu::ConnectAgent);
    cx.dispatch_action(crate::app_menu::ShowKeyboardShortcuts);
    draw(cx);
    assert!(read(&view, cx, |app| app.show_help && !app.show_agent));
    cx.dispatch_action(crate::app_menu::ConnectAgent);
    draw(cx);
    assert!(read(&view, cx, |app| app.show_agent && !app.show_help));
}

/// Opens one overlay, for the cases of `overlays_fit_a_short_window`.
type OpenOverlay = fn(&mut Disktree);

/// A window shorter than an overlay keeps the card, and its Done, inside
/// the window: the list scrolls instead of spilling past the bottom.
#[gpui_kit::test]
fn overlays_fit_a_short_window(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(900.), px(420.)));
    let cases: [(&str, OpenOverlay); 2] = [
        ("help-overlay", |app| app.show_help = true),
        ("agent-overlay", |app| app.show_agent = true),
    ];
    for (card, open) in cases {
        update(&view, cx, |app, cx| {
            app.show_help = false;
            app.show_agent = false;
            open(app);
            cx.notify();
        });
        draw(cx);
        draw(cx);
        let window = cx.update(|window, _| window.viewport_size());
        let bounds = cx.debug_bounds(card).expect("card drawn");
        let done = cx.debug_bounds("overlay-done").expect("Done drawn");
        assert!(
            bounds.bottom() <= window.height,
            "{card} ends at {:?}, past {:?}",
            bounds.bottom(),
            window.height
        );
        assert!(done.bottom() <= bounds.bottom(), "{card}: Done inside");
    }
}

#[gpui_kit::test]
fn audit_help_contains_navigation_and_restores_keyboard_operation(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    press(cx, "n");
    let selected = read(&view, cx, |app| app.selected.clone());
    let root = read(&view, cx, |app| app.crumbs.clone());
    press(cx, "?");
    for key in ["right", "enter", "space", "c", "tab", "shift-tab"] {
        press(cx, key);
    }
    read(&view, cx, |app| {
        assert!(app.show_help);
        assert_eq!(app.selected, selected);
        assert_eq!(app.crumbs, root);
        assert!(app.marks.is_empty());
        assert_eq!(app.screen, Screen::Explore);
    });
    press(cx, "escape");
    let focus = read(&view, cx, |app| app.focus.clone());
    assert!(cx.update(|window, _| focus.is_focused(window)));
    press(cx, "space");
    assert_eq!(read(&view, cx, |app| app.marks.len()), 1);
}

#[gpui_kit::test]
fn audit_source_picker_contains_shortcuts_and_restores_focus(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let root = read(&view, cx, |app| app.root_path.clone());
    let epoch = read(&view, cx, |app| app.scan_epoch);
    press(cx, "v");
    for key in ["space", "c", "/", "r", "tab", "shift-tab"] {
        press(cx, key);
    }
    read(&view, cx, |app| {
        assert!(app.volumes_open);
        assert_eq!(app.screen, Screen::Explore);
        assert_eq!(app.root_path, root);
        assert_eq!(app.scan_epoch, epoch);
        assert!(app.marks.is_empty());
        assert!(!app.find_open);
    });
    press(cx, "escape");
    let focus = read(&view, cx, |app| app.focus.clone());
    assert!(cx.update(|window, _| focus.is_focused(window)));
    press(cx, "tab");
    assert!(read(&view, cx, |app| app.selected.is_some()));
}

#[gpui_kit::test]
fn audit_confirmation_contains_shortcuts_and_cancel_restores_review(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.removal_mode = RemovalMode::Permanent;
        let junk = app.tree().unwrap().find("junk").unwrap().0;
        app.select(Some(junk.clone()), cx);
        app.toggle_mark(&junk, cx);
    });
    press(cx, "c");
    // Only the harness opens this dialog; no confirmation action is sent.
    press(cx, "enter");
    for key in ["c", "r", "p", "m", "!", "tab", "shift-tab"] {
        press(cx, key);
    }
    read(&view, cx, |app| {
        assert!(app.confirm_open);
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.removal_mode, RemovalMode::Permanent);
        assert_eq!(app.marks.len(), 1);
        assert!(app.run.is_none());
    });
    let dialog = read(&view, cx, |app| app.confirm_focus.clone());
    assert!(cx.update(|window, cx| dialog.contains_focused(window, cx)));
    press(cx, "escape");
    let focus = read(&view, cx, |app| app.focus.clone());
    assert!(cx.update(|window, _| focus.is_focused(window)));
    assert!(!read(&view, cx, |app| app.confirm_open));
    assert!(temp.path().join("junk/blob.bin").exists());
    assert!(temp.path().join("keep/notes.txt").exists());
}

/// Settings › Updates draws, keeps the panel's keys and saves the channel
/// picked. A test binary is no Velopack install, so it offers no restart;
/// nothing reaches the network.
#[gpui_kit::test]
fn settings_updates_picks_a_channel_and_keeps_the_keys(
    cx: &mut TestAppContext,
) {
    use crate::settings::{Pane, SettingsWindow};
    use crate::update_channel::{Slot, UpdateSettings};

    cx.update(init);
    let saved_in = tempfile::TempDir::new().expect("tempdir");
    let (view, cx) = cx.add_window_view(|window, cx| {
        let view = SettingsWindow::new(Pane::Updates, cx);
        window.focus(&cx.focus_handle(), cx);
        view
    });
    let root = cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.updates_dir = Some(saved_in.path().to_path_buf());
            cx.focus_handle()
        })
    });
    drop(root);
    draw(cx);
    let focus = cx.update(|window, cx| {
        let _ = window;
        view.read(cx).pane
    });
    assert_eq!(focus, Pane::Updates);
    assert!(cx.debug_bounds(crate::settings::ROOT_ID).is_some());
    assert!(cx.debug_bounds("update-slot-3").is_some());
    assert!(cx.debug_bounds(crate::settings::CHECK_ID).is_some());
    assert!(
        cx.debug_bounds(crate::settings::RESTART_ID).is_none(),
        "nothing is downloaded, so there is nothing to restart into"
    );

    // The keys, as the in-window panel had them, once the pane has focus.
    let slot = cx.debug_bounds("update-slot-0").expect("first row");
    cx.simulate_click(slot.center(), gpui_kit::Modifiers::none());
    draw(cx);
    // Latest sits beside Prerelease in the grid.
    press(cx, "right");
    press(cx, "enter");
    assert_eq!(
        crate::updater::selection().resolve("0.12.0"),
        Slot::STABLE_PRERELEASE
    );
    let saved =
        std::fs::read_to_string(saved_in.path().join("update-channel.json"))
            .expect("the pick is saved");
    assert!(saved.contains("\"prerelease\""), "{saved}");

    // Ctrl+PageDown walks the panes; every one draws. Safety saves only in the fixture.
    press(cx, "ctrl-pagedown");
    // One control of six equal segments, 1 5 10 25 50 100: 10% is third.
    let limits = cx.debug_bounds("safety-limit").expect("the limit control");
    cx.simulate_click(
        gpui_kit::point(
            limits.origin.x + limits.size.width * 2.5 / 6.,
            limits.center().y,
        ),
        gpui_kit::Modifiers::none(),
    );
    draw(cx);
    assert_eq!(
        crate::safety::load(Some(saved_in.path()))
            .expect("saved limit")
            .percent(),
        10
    );
    press(cx, "ctrl-pagedown");
    assert!(
        cx.debug_bounds(crate::settings::COPY_DIAGNOSTICS_ID)
            .is_some()
    );
    press(cx, "ctrl-pagedown");
    assert_eq!(view.read_with(cx, |settings, _| settings.pane), Pane::About);
    // The header leads the pane, and its Copy build info sits at the
    // right of the same card.
    let header = cx
        .debug_bounds(crate::settings::ABOUT_HEADER_ID)
        .expect("About's header");
    let copy = cx
        .debug_bounds(crate::settings::COPY_BUILD_ID)
        .expect("Copy build info");
    assert!(header.contains(&copy.center()), "{header:?} {copy:?}");
    assert!(copy.center().x > header.center().x, "{header:?} {copy:?}");
    let line = cx
        .debug_bounds(crate::settings::ABOUT_BUILD_LINE_ID)
        .expect("the build line");
    assert!(header.contains(&line.center()), "{header:?} {line:?}");
    assert!(line.right() < copy.left(), "{line:?} {copy:?}");
    let notices = cx.debug_bounds("about-notices").expect("the notices");
    assert!(header.bottom() < notices.top(), "{header:?} {notices:?}");

    // The selection is process-wide; leave the default for other tests.
    let _ = crate::updater::set_selection(UpdateSettings::default(), None);
}

/// Settings › Safety reads the saved limit when the pane opens and after
/// a pick is saved, not on every frame, and shows the pick once saved.
#[gpui_kit::test]
fn settings_safety_reads_the_limit_once_and_shows_a_saved_pick(
    cx: &mut TestAppContext,
) {
    use crate::settings::{Pane, SettingsWindow, work};
    use disktree_core::removal_limit::Limit;

    cx.update(init);
    let saved_in = tempfile::TempDir::new().expect("tempdir");
    crate::safety::save(
        Some(saved_in.path()),
        Limit::new(25).expect("a limit"),
    )
    .expect("save the starting limit");
    let (view, cx) = cx.add_window_view(|window, cx| {
        let view = SettingsWindow::new(Pane::Updates, cx);
        window.focus(&cx.focus_handle(), cx);
        view
    });
    cx.update(|_, cx| {
        view.update(cx, |view, _| {
            view.updates_dir = Some(saved_in.path().to_path_buf());
        });
    });
    let shown = |cx: &mut Window| {
        view.read_with(cx, |view, _| {
            view.safety
                .as_ref()
                .ok()
                .and_then(|policy| policy.limit.map(Limit::percent))
        })
    };
    draw(cx);
    work::SAFETY_LOADS.set(0);
    click_on(cx, Pane::Safety.nav_id());
    assert_eq!(view.read_with(cx, |view, _| view.pane), Pane::Safety);
    for _ in 0..5 {
        draw(cx);
    }
    assert_eq!(shown(cx), Some(25));
    assert_eq!(work::SAFETY_LOADS.get(), 1, "opening the pane reads it");

    // 1 5 10 25 50 100: 10% is the third segment.
    let limits = cx.debug_bounds("safety-limit").expect("the limit control");
    cx.simulate_click(
        gpui_kit::point(
            limits.origin.x + limits.size.width * 2.5 / 6.,
            limits.center().y,
        ),
        gpui_kit::Modifiers::none(),
    );
    for _ in 0..5 {
        draw(cx);
    }
    assert_eq!(shown(cx), Some(10), "the pane shows the pick once saved");
    assert_eq!(work::SAFETY_LOADS.get(), 2, "and read it back once");
    assert_eq!(
        crate::safety::load(Some(saved_in.path()))
            .expect("saved limit")
            .percent(),
        10
    );
}

/// Three checkouts side by side: each a worktree's `.git` file and some
/// data, largest first, so the first is selected when the window opens.
fn checkouts() -> tempfile::TempDir {
    let temp = tempfile::TempDir::new().expect("tempdir");
    for (name, bytes) in
        [("alpha", 300_000), ("beta", 200_000), ("gamma", 100_000)]
    {
        let dir = temp.path().join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(".git"), "gitdir: elsewhere").expect("write");
        std::fs::write(dir.join("data.bin"), vec![b'x'; bytes]).expect("write");
    }
    temp
}

/// Give the window a probe that counts its runs and gives `answer` for
/// every checkout, in place of git. Anything the real one started is
/// forgotten.
fn count_probes(
    view: &Entity<Disktree>,
    cx: &mut Window,
    answer: crate::git_store::Probe,
) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runs = Arc::new(AtomicUsize::new(0));
    update(view, cx, |app, _| {
        let counter = runs.clone();
        app.git_probe = Arc::new(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            crate::git_store::ready(answer.clone())
        });
        app.git.clear();
    });
    cx.run_until_parked();
    runs
}

fn clean_checkout() -> crate::git::GitState {
    crate::git::GitState {
        changed: Some(0),
        unpushed: Some(0),
        ..crate::git::GitState::default()
    }
}

/// Move the pointer onto the drawn tile for the directory `path`: onto its
/// name band when it has children, where no child tile is.
fn hover_on(view: &Entity<Disktree>, cx: &mut Window, path: &Path) {
    let at = update(view, cx, |app, _| {
        let crumbs = app.crumbs_for_path(path).expect("a path in the tree");
        let tile = app
            .layout()
            .and_then(|tiles| {
                tiles.iter().find(|tile| tile.crumbs() == crumbs).cloned()
            })
            .expect("the tile is drawn");
        let screen = app.view.project(tile.header.unwrap_or(tile.rect));
        let (x, y) = (screen.x + screen.w / 2.0, screen.y + screen.h / 2.0);
        assert_eq!(app.tile_at(x, y), Some(crumbs), "the pointer is on it");
        let origin = app.treemap_origin.get();
        Point::new(origin.x + px(x), origin.y + px(y))
    });
    cx.simulate_mouse_move(at, None, gpui_kit::Modifiers::default());
    draw(cx);
}

fn probe_runs(runs: &std::sync::atomic::AtomicUsize) -> usize {
    runs.load(std::sync::atomic::Ordering::SeqCst)
}

/// Sweeping the pointer across checkouts runs no git; resting on one runs
/// it once, and the panel shows the answer.
#[gpui_kit::test]
fn a_sweep_over_checkouts_asks_nothing_and_a_rest_asks_once(
    cx: &mut TestAppContext,
) {
    use crate::git_store::{DWELL, Probe, Shown};

    cx.update(init);
    let temp = checkouts();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1400.), px(900.)));
    // Nothing selected, so only the pointer chooses what the panel shows.
    press(cx, "escape");
    assert_eq!(read(&view, cx, |app| app.selected.clone()), None);
    let runs = count_probes(&view, cx, Probe::Known(clean_checkout()));

    for name in ["alpha", "beta", "gamma", "alpha", "beta"] {
        hover_on(&view, cx, &temp.path().join(name));
        cx.run_until_parked();
    }
    assert_eq!(probe_runs(&runs), 0, "a sweep asks nothing");

    // The pointer stays on beta.
    cx.executor().advance_clock(DWELL);
    cx.run_until_parked();
    draw(cx);
    cx.run_until_parked();
    assert_eq!(probe_runs(&runs), 1, "resting asks once");
    let beta = temp.path().join("beta");
    assert!(read(&view, cx, |app| {
        app.git.shown(&beta) == Shown::Answer(&Probe::Known(clean_checkout()))
    }));

    // Resting longer asks nothing more.
    for _ in 0..3 {
        cx.executor().advance_clock(DWELL);
        cx.run_until_parked();
        draw(cx);
        cx.run_until_parked();
    }
    assert_eq!(probe_runs(&runs), 1);
}

/// Selecting a checkout runs git once at once; selecting it again reads
/// the answer instead.
#[gpui_kit::test]
fn selecting_a_checkout_asks_once_and_then_reads_the_answer(
    cx: &mut TestAppContext,
) {
    use crate::git_store::Probe;

    cx.update(init);
    let temp = checkouts();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1400.), px(900.)));
    let runs = count_probes(&view, cx, Probe::Known(clean_checkout()));
    let alpha = temp.path().join("alpha");
    let beta = temp.path().join("beta");
    let crumbs = |path: &Path| {
        read(&view, cx, |app| app.crumbs_for_path(path)).expect("in the tree")
    };
    let (alpha_at, beta_at) = (crumbs(&alpha), crumbs(&beta));
    assert_eq!(
        read(&view, cx, |app| app.selected.clone()),
        Some(alpha_at.clone()),
        "the largest is selected"
    );

    for _ in 0..5 {
        draw(cx);
        cx.run_until_parked();
    }
    assert_eq!(probe_runs(&runs), 1, "five frames, one probe");

    let select = |at: &Vec<usize>, cx: &mut Window| {
        let at = at.clone();
        update(&view, cx, move |app, cx| {
            app.selected = Some(at);
            cx.notify();
        });
        draw(cx);
        cx.run_until_parked();
    };
    select(&beta_at, cx);
    assert_eq!(probe_runs(&runs), 2, "another checkout, another probe");
    select(&alpha_at, cx);
    draw(cx);
    cx.run_until_parked();
    assert_eq!(probe_runs(&runs), 2, "alpha's answer is remembered");
}

/// A checkout left selected is not asked again while the window is in
/// the background, however stale its answer gets; it is refreshed once
/// when the window comes back.
#[gpui_kit::test]
fn a_background_window_leaves_a_stale_answer_until_it_is_active(
    cx: &mut TestAppContext,
) {
    use crate::git_store::{FRESH_FOR, Probe};

    cx.update(init);
    let temp = checkouts();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1400.), px(900.)));
    // A test window opens in the background; bring it to the front.
    cx.update(|window, _| window.activate_window());
    let runs = count_probes(&view, cx, Probe::Known(clean_checkout()));
    draw(cx);
    cx.run_until_parked();
    assert!(cx.update(|window, _| window.is_window_active()));
    assert_eq!(probe_runs(&runs), 1, "the selection is asked about");

    cx.deactivate_window();
    for _ in 0..5 {
        cx.executor().advance_clock(FRESH_FOR);
        cx.run_until_parked();
        draw(cx);
        cx.run_until_parked();
    }
    assert_eq!(probe_runs(&runs), 1, "nobody is looking");

    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    draw(cx);
    cx.run_until_parked();
    assert_eq!(probe_runs(&runs), 2, "back in front, it is refreshed");
    draw(cx);
    cx.run_until_parked();
    assert_eq!(probe_runs(&runs), 2, "once");
}

/// A checkout git could not read within its limit is drawn as such, and
/// not asked again on the frames that follow.
#[gpui_kit::test]
fn a_timed_out_checkout_is_drawn_and_not_asked_again(cx: &mut TestAppContext) {
    use crate::git_store::{Probe, Shown};

    cx.update(init);
    let temp = checkouts();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1400.), px(900.)));
    let runs = count_probes(&view, cx, Probe::TimedOut);
    for _ in 0..5 {
        draw(cx);
        cx.run_until_parked();
    }
    assert_eq!(probe_runs(&runs), 1);
    let alpha = temp.path().join("alpha");
    assert!(read(&view, cx, |app| {
        app.git.shown(&alpha) == Shown::Answer(&Probe::TimedOut)
    }));
}

/// The side panel asks whether the selection is a checkout once per
/// selection, not on every frame it draws.
#[gpui_kit::test]
fn the_panel_checks_for_a_checkout_once_per_selection(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    // Opening the window draws its first frame, so count from before it.
    crate::git::CHECKS.set(0);
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1400.), px(900.)));
    for _ in 0..5 {
        draw(cx);
    }
    assert!(
        cx.debug_bounds("side-panel-scroll").is_some(),
        "the panel is shown"
    );
    assert_eq!(read(&view, cx, |app| app.selected.clone()), Some(vec![0]));
    assert_eq!(crate::git::CHECKS.get(), 1, "five frames, one check");

    // Another directory is another question, asked once.
    update(&view, cx, |app, cx| {
        app.selected = Some(vec![1]);
        cx.notify();
    });
    for _ in 0..5 {
        draw(cx);
    }
    let next_is_dir = read(&view, cx, |app| {
        app.node_at(&[1])
            .is_some_and(disktree_core::index::Node::is_dir)
    });
    assert!(next_is_dir, "the fixture's second entry is a directory");
    assert_eq!(crate::git::CHECKS.get(), 2);

    // With nothing selected the panel describes the scan root, which is
    // never asked about: not even its `.git`, let alone `git status`.
    std::fs::create_dir(temp.path().join(".git")).expect("mkdir .git");
    press(cx, "escape");
    for _ in 0..5 {
        draw(cx);
    }
    assert_eq!(read(&view, cx, |app| app.selected.clone()), None);
    assert_eq!(crate::git::CHECKS.get(), 2, "the root is not checked");
    // `git` fills only when a background ask finishes; let any finish.
    cx.run_until_parked();
    let root = temp.path().to_path_buf();
    assert!(
        read(&view, cx, |app| {
            app.git.shown(&root) == crate::git_store::Shown::Asking
        }),
        "no git status"
    );
}

/// Shift-U, which opened the Updates panel, opens Settings › Updates in
/// its own window now; the main window is left as it was.
#[gpui_kit::test]
fn shift_u_opens_settings_updates(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let crumbs = read(&view, cx, |app| app.crumbs.clone());
    press(cx, "shift-u");
    cx.run_until_parked();
    let settings = cx.update(|_, cx| {
        crate::aux_window::find::<crate::settings::SettingsWindow>(cx, |_| true)
            .and_then(|handle| handle.read(cx).ok().map(|view| view.pane))
    });
    assert_eq!(settings, Some(crate::settings::Pane::Updates));
    assert_eq!(read(&view, cx, |app| app.crumbs.clone()), crumbs);
}

/// Help's documents draw from what this build ships, and Copy puts the
/// whole document on the clipboard.
#[gpui_kit::test]
fn help_documents_draw_and_copy(cx: &mut TestAppContext) {
    use crate::doc_window::{Doc, DocWindow};

    cx.update(init);
    for doc in [Doc::Changelog, Doc::License, Doc::ThirdPartyNotices] {
        let (_, cx) = cx.add_window_view(move |_, cx| DocWindow::new(doc, cx));
        draw(cx);
        assert!(cx.debug_bounds("title-strip").is_some(), "{doc:?}");
        let copy = cx
            .debug_bounds(crate::doc_window::COPY_ID)
            .unwrap_or_else(|| panic!("{doc:?} has Copy"));
        cx.simulate_click(copy.center(), gpui_kit::Modifiers::none());
        draw(cx);
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        assert_eq!(copied, doc.source(), "{doc:?}");
    }
}

/// A contrived `main.log` in a temporary folder, and the Logs window over
/// it at its opening size. Nothing is remembered: there is no settings
/// file.
fn logs_over<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Entity<crate::logs_window::LogsWindow>,
    &'a mut Window,
) {
    cx.update(init);
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("main.log");
    std::fs::write(&path, text).expect("write the log");
    let log = path.clone();
    let (view, cx) = cx.add_window_view(move |window, cx| {
        crate::logs_window::LogsWindow::new(Some(log), None, window, cx)
    });
    cx.simulate_resize(gpui_kit::size(
        gpui_kit::px(1120.0),
        gpui_kit::px(720.0),
    ));
    draw(cx);
    draw(cx);
    (dir, path, view, cx)
}

fn append_log(path: &std::path::Path, text: &str) {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open the log");
    std::io::Write::write_all(&mut file, text.as_bytes()).expect("append");
}

/// One poll of the file, as the window's timer runs it.
fn poll_logs(cx: &mut Window) {
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(500));
    cx.run_until_parked();
    draw(cx);
}

fn click_on(cx: &mut Window, selector: &'static str) {
    draw(cx);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is drawn"));
    cx.simulate_click(bounds.center(), gpui_kit::Modifiers::none());
    draw(cx);
}

fn clipboard(cx: &Window) -> String {
    cx.read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default()
}

/// Logs reads the file, filters by level and by search, steps through the
/// matches, wraps long lines rather than clip them, follows lines written
/// after it opened (or counts them while paused), and copies the lines
/// picked in the gutter exactly as the file has them.
#[gpui_kit::test]
fn logs_follow_the_file_and_filter(cx: &mut TestAppContext) {
    use crate::log_view::Level;
    use crate::logs_window::{
        LIVE_ID, MARK_ID, NEXT_ID, PILL_ID, PREV_ID, SINCE_MARK_ID, WRAP_ID,
    };

    let long = format!(
        "2026-10-30T00:00:02.000-04:00 WARN  pwrdisk::updater: GitHub said \
         403 {}",
        "and kept saying so ".repeat(30)
    );
    let started =
        "2026-10-30T00:00:01.000-04:00 INFO  pwrdisk: started PwrDisk";
    let workers =
        "2026-10-30T00:00:03.000-04:00 DEBUG pwrdisk::scan: 8 workers";
    let failed = "2026-10-30T00:00:04.000-04:00 ERROR pwrdisk::scan: \
                  local scan failed after 0.2 s";
    let (_dir, path, view, cx) =
        logs_over(cx, &format!("{started}\n{long}\n{workers}\n{failed}\n"));
    let visible =
        |cx: &mut Window| cx.update(|_, cx| view.read(cx).view.visible().len());
    assert!(cx.debug_bounds("logs-count").is_some());
    // Debug starts off; its toggle shows it.
    assert_eq!(visible(cx), 3);
    click_on(cx, "logs-level-debug");
    assert_eq!(visible(cx), 4);
    click_on(cx, "logs-level-debug");

    // Wrap is on: the long line takes several rows, the short one one, and
    // its rows cover it at word boundaries within the room it has.
    let row = cx.debug_bounds("log-row-1").expect("line 1").size.height;
    let tall = cx.debug_bounds("log-row-2").expect("line 2").size.height;
    assert!(tall >= row * 3.0, "{tall:?} vs {row:?}");
    let (segments, len) = cx.update(|window, cx| {
        let logs = view.read(cx);
        let raw = logs.view.lines[1].raw().clone();
        (logs.segments(&raw, window, cx), raw.len())
    });
    assert!(segments.len() >= 3, "{segments:?}");
    assert_eq!(segments.first().map(|s| s.start), Some(0));
    assert_eq!(segments.last().map(|s| s.end), Some(len));
    assert!(segments.windows(2).all(|pair| pair[0].end == pair[1].start));
    // Off, every line is one row, and the text scrolls sideways.
    click_on(cx, WRAP_ID);
    assert!(!cx.update(|_, cx| view.read(cx).wrap));
    let unwrapped = cx.debug_bounds("log-row-2").expect("line 2").size.height;
    assert_eq!(unwrapped, row);
    let at = cx.debug_bounds("log-row-2").expect("line 2").center();
    cx.simulate_event(gpui_kit::ScrollWheelEvent {
        position: at,
        delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
            gpui_kit::px(-120.0),
            gpui_kit::px(0.0),
        )),
        ..Default::default()
    });
    draw(cx);
    assert!(cx.update(|_, cx| view.read(cx).h_offset) > 0.0);
    click_on(cx, WRAP_ID);

    // A search filters, and Enter, Shift-Enter and the chevrons step
    // through what it found.
    let search = cx.update(|_, cx| view.read(cx).search_state());
    cx.update(|window, cx| {
        search.update(cx, |search, cx| search.focus(window, cx));
    });
    cx.simulate_input("pwrdisk");
    draw(cx);
    let active =
        |cx: &mut Window| cx.update(|_, cx| view.read(cx).view.active());
    assert!(!cx.update(|_, cx| view.read(cx).live), "a search pauses");
    assert_eq!(cx.update(|_, cx| view.read(cx).view.match_count()), 3);
    assert_eq!(active(cx), Some(1));
    cx.simulate_keystrokes("enter");
    assert_eq!(active(cx), Some(2));
    cx.simulate_keystrokes("shift-enter shift-enter");
    assert_eq!(active(cx), Some(4));
    click_on(cx, NEXT_ID);
    assert_eq!(active(cx), Some(1));
    click_on(cx, PREV_ID);
    assert_eq!(active(cx), Some(4));
    // Esc clears it, and every line is back.
    cx.update(|window, cx| {
        search.update(cx, |search, cx| search.focus(window, cx));
    });
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(!cx.update(|_, cx| view.read(cx).view.searching()));
    assert_eq!(visible(cx), 3);
    click_on(cx, LIVE_ID);
    assert!(cx.update(|_, cx| view.read(cx).live));

    // Lines picked in the gutter copy as the file has them; Select All
    // takes every line shown.
    click_on(cx, "log-number-1");
    let gutter = cx.debug_bounds("log-number-4").expect("line 4's number");
    cx.simulate_click(gutter.center(), gpui_kit::Modifiers::shift());
    draw(cx);
    assert_eq!(cx.update(|_, cx| view.read(cx).selection), Some((1, 4)));
    cx.simulate_keystrokes("ctrl-c");
    assert_eq!(clipboard(cx), format!("{started}\n{long}\n{failed}\n"));
    click_on(cx, "log-number-4");
    cx.simulate_keystrokes("ctrl-c");
    assert_eq!(clipboard(cx), format!("{failed}\n"));
    cx.simulate_keystrokes("ctrl-a ctrl-c");
    assert_eq!(clipboard(cx), format!("{started}\n{long}\n{failed}\n"));
    cx.simulate_keystrokes("escape");
    assert_eq!(cx.update(|_, cx| view.read(cx).selection), None);

    // A line written later is read on the next poll.
    let refused = "2026-10-30T00:00:05.000-04:00 ERROR pwrdisk::removal: \
                   removal finished: 0 removed, 0 bytes, 1 failed";
    append_log(&path, &format!("{refused}\n"));
    poll_logs(cx);
    assert_eq!(visible(cx), 4);
    assert_eq!(cx.update(|_, cx| view.read(cx).view.count(Level::Error)), 2);

    // Paused, new lines are counted on the pill, which follows again.
    click_on(cx, LIVE_ID);
    append_log(&path, &format!("{started}\n{refused}\n"));
    poll_logs(cx);
    assert_eq!(cx.update(|_, cx| view.read(cx).new_lines), 2);
    click_on(cx, PILL_ID);
    assert!(cx.update(|_, cx| view.read(cx).live));

    // The mark selects what came after it, and that grows with the log.
    click_on(cx, MARK_ID);
    assert!(cx.debug_bounds("log-mark").is_some());
    append_log(&path, &format!("{started}\n"));
    poll_logs(cx);
    click_on(cx, SINCE_MARK_ID);
    assert_eq!(cx.update(|_, cx| view.read(cx).selection), Some((8, 8)));
    append_log(&path, &format!("{refused}\n"));
    poll_logs(cx);
    assert_eq!(cx.update(|_, cx| view.read(cx).selection), Some((8, 9)));
    cx.simulate_keystrokes("ctrl-c");
    assert_eq!(clipboard(cx), format!("{started}\n{refused}\n"));
}

/// A rotation reads the files again, even when it leaves `main.log` empty:
/// what was shown pointed into the old file, and drawing it would index
/// past the end.
#[gpui_kit::test]
fn logs_survive_a_rotation_to_an_empty_file(cx: &mut TestAppContext) {
    let (dir, path, view, cx) = logs_over(
        cx,
        "2026-10-30T00:00:01.000-04:00 WARN  pwrdisk: one\n\
         2026-10-30T00:00:02.000-04:00 ERROR pwrdisk: two\n",
    );
    assert_eq!(cx.update(|_, cx| view.read(cx).view.visible().len()), 2);
    // Paused, with a line waiting behind the pill.
    view.update(cx, |logs, cx| logs.set_live(false, cx));
    append_log(&path, "2026-10-30T00:00:02.500-04:00 INFO  pwrdisk: more\n");
    poll_logs(cx);
    assert_eq!(cx.update(|_, cx| view.read(cx).new_lines), 1);

    std::fs::rename(&path, dir.path().join("main.old.log")).expect("rotate");
    std::fs::write(&path, "").expect("a new main.log");
    poll_logs(cx);
    // The old file's lines, from main.old.log, and nothing past them;
    // the pill counted lines of a reading that is gone.
    assert_eq!(cx.update(|_, cx| view.read(cx).view.visible().len()), 3);
    assert_eq!(cx.update(|_, cx| view.read(cx).new_lines), 0);
    view.update(cx, |logs, cx| logs.set_live(true, cx));
    // A real rotation only ever moves a main.log with lines in it: an
    // empty file swapped for another empty one may keep its inode (Linux)
    // or have none (Windows), and there is nothing to notice then.
    append_log(
        &path,
        "2026-10-30T00:00:03.000-04:00 INFO  pwrdisk: three\n",
    );
    poll_logs(cx);
    assert_eq!(cx.update(|_, cx| view.read(cx).view.visible().len()), 4);
    std::fs::write(dir.path().join("main.old.log"), "").expect("emptied");
    std::fs::write(&path, "").expect("truncated");
    poll_logs(cx);
    assert_eq!(cx.update(|_, cx| view.read(cx).view.visible().len()), 0);
    assert!(cx.debug_bounds("logs-count").is_some());
}

/// Restart to update never exits while a removal runs, wherever it is
/// asked from (Settings goes through the main window): it says why.
#[gpui_kit::test]
fn restart_to_update_waits_for_a_removal(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.run = Some(disktree_core::removal::spawn(
            disktree_core::removal::Plan::default(),
            RemovalMode::Trash,
        ));
        assert!(!app.can_restart_to_update());
        app.restart_to_update(cx);
    });
    let notice = read(&view, cx, |app| app.notice.clone());
    assert!(
        notice.is_some_and(|(text, _)| text.contains("removal")),
        "restart refused while removing"
    );
    update(&view, cx, |app, _| app.run = None);
}

/// Off macOS the strip carries the menu button, and its dropdown lists the
/// menu bar's template; Escape closes it.
#[gpui_kit::test]
fn the_strip_menu_lists_the_menus_off_macos(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    for (name, frame) in window_frames() {
        update(&view, cx, |app, cx| {
            app.chrome_pin = Some(frame.clone());
            cx.notify();
        });
        draw(cx);
        let mac = frame.platform == crate::chrome::Platform::MacOs;
        assert_eq!(
            cx.debug_bounds("app-menu-button").is_some(),
            !mac,
            "{name}"
        );
    }
    let button = cx.debug_bounds("app-menu-button").expect("menu button");
    cx.simulate_click(button.center(), gpui_kit::Modifiers::none());
    draw(cx);
    assert!(read(&view, cx, |app| app.app_menu_open));
    assert!(cx.debug_bounds("app-menu").is_some());
    // The first group's first row: File › Open Folder….
    assert!(cx.debug_bounds("app-menu-0-0").is_some());
    press(cx, "escape");
    assert!(!read(&view, cx, |app| app.app_menu_open));
    assert!(cx.debug_bounds("app-menu").is_none());
}

/// Copy Diagnostics Info writes the report, with the main window's
/// session and no path it scanned, and says so in the notice line.
#[gpui_kit::test]
fn copy_diagnostics_info_names_no_scanned_path(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    // As the menu runs it: after the dispatch that asked.
    cx.update(|_, cx| cx.defer(crate::help::copy_diagnostics));
    cx.run_until_parked();
    let report = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default();
    assert!(
        report.starts_with("PwrDisk diagnostics, collected "),
        "{report}"
    );
    assert!(report.contains("\n[Session]\nScan: local"), "{report}");
    let root = temp.path().display().to_string();
    assert!(!report.contains(&root), "{report}");
    let notice = read(&view, cx, |app| app.notice.clone());
    assert!(
        notice.is_some_and(|(text, _)| text.starts_with("Diagnostics copied")),
        "the main window says it copied"
    );
}

#[gpui_kit::test]
fn the_volume_picker_opens_moves_and_closes(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    // The picker lists volumes and draws them without panicking; Escape
    // leaves the scan where it was.
    press(cx, "v");
    assert!(read(&view, cx, |app| app.volumes_open));
    draw(cx);
    assert!(cx.debug_bounds("disktree-root").is_some());
    let before = read(&view, cx, |app| app.root_path.clone());
    press(cx, "down");
    press(cx, "up");
    press(cx, "escape");
    assert!(!read(&view, cx, |app| app.volumes_open));
    assert_eq!(read(&view, cx, |app| app.root_path.clone()), before);
}

/// Enter must reach the picker even when its dialog owns keyboard focus.
#[gpui_kit::test]
fn enter_in_the_focused_volume_picker_scans_the_selected_root(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    // Accepting a local root canonicalizes it, and the temporary directory
    // is behind a link on macOS (/var -> /private/var) and a short 8.3 name
    // on Windows runners, so compare against the resolved path.
    let target = dunce::canonicalize(temp.path().join("junk")).unwrap();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, _| {
        app.volumes = vec![disktree_core::space::Volume {
            point: target.clone(),
            device: None,
            space: None,
        }];
        app.volume_highlight = 0;
        app.volumes_open = true;
    });
    let focus = read(&view, cx, |app| app.confirm_focus.clone());
    cx.update(|window, cx| window.focus(&focus, cx));
    draw(cx);
    assert!(!read(&view, cx, Disktree::can_start_over));
    press(cx, "enter");
    assert!(!read(&view, cx, |app| app.volumes_open));
    assert_eq!(
        read(&view, cx, |app| app.root_path.clone()),
        dunce::canonicalize(target).expect("canonical selected root")
    );
    finish_scan(&view, cx);
    assert!(read(&view, cx, |app| app.tree().is_some()));
}

#[gpui_kit::test]
fn the_volume_picker_is_centred_and_ignores_clicks_outside(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    // Fixed rows, so this is about where the popup lands and not about which
    // disks the machine running the test happens to have.
    update(&view, cx, |app, cx| {
        app.volumes = vec![
            Volume {
                point: PathBuf::from("/one"),
                device: Some("/dev/one".into()),
                space: Some(SpaceInfo {
                    total: 1_000,
                    free: 500,
                    available: 400,
                }),
            },
            Volume {
                point: PathBuf::from("/two"),
                device: None,
                space: None,
            },
        ];
        app.volumes_open = true;
        cx.notify();
    });
    draw(cx);

    let viewport = cx.update(|window, _| window.viewport_size());
    let rows = cx.debug_bounds("volume-rows").expect("the rows are drawn");
    let centre = rows.center();
    assert!(
        (centre.x - viewport.width / 2.0).abs() < px(4.),
        "the popup is not centred across: {centre:?} in {viewport:?}"
    );
    assert!(
        (centre.y - viewport.height / 2.0).abs() < px(150.),
        "the popup is not centred down: {centre:?} in {viewport:?}"
    );

    // A dialog keeps what was typed: a click in the corner, outside the
    // popup, closes nothing. Cancel and Escape do.
    cx.simulate_click(
        gpui_kit::point(px(4.), px(4.)),
        gpui_kit::Modifiers::none(),
    );
    draw(cx);
    assert!(
        read(&view, cx, |app| app.volumes_open),
        "clicking outside the picker leaves it open"
    );
}

#[gpui_kit::test]
fn showing_a_tile_that_is_gone_says_so_instead(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    // The selection is .cache, the largest; take it away behind the tree.
    std::fs::remove_dir_all(temp.path().join(".cache")).expect("remove");
    press(cx, "o");
    let notice = read(&view, cx, |app| app.notice.clone());
    let (message, _) = notice.expect("a notice");
    assert!(message.contains("no longer on disk"), "{message}");
}

#[gpui_kit::test]
fn command_chords_are_not_read_as_plain_letters(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    // ⌘P and ⌘D are the menu bar's or nobody's; read as `p` and `d` they
    // would hide the selection and re-scan.
    let shown = read(&view, cx, |app| app.show_selection);
    press(cx, "cmd-p cmd-d");
    assert_eq!(read(&view, cx, |app| app.show_selection), shown);
    assert!(read(&view, cx, |app| app.options.apparent_size));
    press(cx, "p");
    assert_eq!(read(&view, cx, |app| app.show_selection), !shown);
}

#[gpui_kit::test]
fn the_treemap_zooms_with_the_wheel_and_resets(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    let middle = read(&view, cx, |app| {
        let size = app.treemap_size.get();
        Point::new(size.width / 2.0, size.height / 2.0)
    });
    update(&view, cx, |app, cx| {
        app.zoom_at(middle.x.as_f32(), middle.y.as_f32(), 1.5, false, cx);
    });
    let zoomed = read(&view, cx, |app| app.view.scale);
    assert!(zoomed > 1.0, "scale after zooming: {zoomed}");

    press(cx, "0");
    assert!((read(&view, cx, |app| app.view.scale) - 1.0).abs() < f32::EPSILON);
}

/// Both appearances Omarchy ships must draw: the palette is derived from the
/// theme, so a light theme is a real second configuration.
#[gpui_kit::test]
fn both_appearances_draw(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    for theme in [Theme::tokyo_night(), Theme::flexoki_light()] {
        view.update(cx, |_, cx| {
            theme.clone().apply(cx);
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("disktree-root").is_some());
    }
}

/// A view built without a scan keeps working: the screens must not assume a
/// walk is in flight.
#[gpui_kit::test]
fn a_view_without_flags_or_marks_still_draws(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.screen = Screen::Review;
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("disktree-root").is_some());

    update(&view, cx, |app, cx| {
        app.screen = Screen::Running;
        cx.notify();
    });
    draw(cx);

    update(&view, cx, |app, cx| {
        app.screen = Screen::Done;
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("disktree-root").is_some());
}

/// In a short window what does not fit in the summary is reached by
/// scrolling it. Its box ended above the footer even before the fix; its
/// content painted past the box, so the wheel checks are the guard.
#[gpui_kit::test]
fn the_review_summary_fits_a_short_window(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1100.), px(420.)));
    update(&view, cx, |app, cx| {
        app.marks.toggle(
            disktree_core::removal::Target {
                path: temp.path().join("junk"),
                bytes: 300_000,
                is_dir: true,
                hidden: false,
            },
            app.path_style(),
        );
        app.screen = Screen::Review;
        cx.notify();
    });
    draw(cx);
    draw(cx);
    let summary = cx.debug_bounds("review-summary").expect("summary");
    let footer = cx.debug_bounds("review-footer").expect("footer");
    assert!(
        summary.bottom() <= footer.top(),
        "summary ends at {:?}, under the footer at {:?}",
        summary.bottom(),
        footer.top()
    );
    // The panel's end starts below its bottom edge, and the wheel brings
    // it up into view. Without scrolling it stays put and paints over the
    // footer.
    let end = cx.debug_bounds("review-summary-end").expect("summary end");
    assert!(end.bottom() > summary.bottom(), "starts out of view");
    cx.simulate_event(gpui_kit::ScrollWheelEvent {
        position: summary.center(),
        delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
            px(0.),
            px(-2000.),
        )),
        ..Default::default()
    });
    draw(cx);
    let scrolled = cx.debug_bounds("review-summary-end").expect("summary end");
    assert!(scrolled.top() < end.top(), "{scrolled:?} did not move");
    assert!(
        scrolled.bottom() <= summary.bottom(),
        "{scrolled:?} still past {summary:?}"
    );
}

/// The removal mode is a choice with two independent channels: the key and the
/// button both have to reach the same state.
#[gpui_kit::test]
fn the_review_screen_switches_removal_mode(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.marks.toggle(
            disktree_core::removal::Target {
                path: temp.path().join("junk"),
                bytes: 300_000,
                is_dir: true,
                hidden: false,
            },
            app.path_style(),
        );
        app.screen = Screen::Review;
        cx.notify();
    });
    draw(cx);

    press(cx, "m");
    assert_eq!(read(&view, cx, |app| app.removal_mode), RemovalMode::Trash);
    // A Ctrl chord is not its letter: Edit's rows replay them (Select All
    // is ctrl-a), and ctrl-p must not choose permanent removal.
    press(cx, "ctrl-p");
    assert_eq!(read(&view, cx, |app| app.removal_mode), RemovalMode::Trash);
    press(cx, "p");
    assert_eq!(
        read(&view, cx, |app| app.removal_mode),
        RemovalMode::Permanent
    );
    press(cx, "!");
    assert!(read(&view, cx, |app| app.marks.is_empty()));
}

/// Put `paths` on the marked list, as the treemap would.
fn mark_paths(view: &Entity<Disktree>, cx: &mut Window, paths: &[PathBuf]) {
    update(view, cx, |app, cx| {
        for path in paths {
            app.marks.toggle(
                disktree_core::removal::Target {
                    path: path.clone(),
                    bytes: 1_000,
                    is_dir: true,
                    hidden: false,
                },
                app.path_style(),
            );
        }
        cx.notify();
    });
}

fn marked(view: &Entity<Disktree>, cx: &Window) -> Vec<PathBuf> {
    read(view, cx, |app| {
        app.marks
            .items()
            .iter()
            .map(|item| item.path.clone())
            .collect()
    })
}

/// Unmark all keeps what it took: `u`, or Edit › Undo, puts it back,
/// and a source switch forgets it, since the paths were that machine's.
#[gpui_kit::test]
fn unmark_all_can_be_undone(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let both = vec![temp.path().join("junk"), temp.path().join("keep")];
    mark_paths(&view, cx, &both);
    update(&view, cx, |app, cx| {
        app.screen = Screen::Review;
        cx.notify();
    });
    click_on(cx, "unmark-all");
    assert!(marked(&view, cx).is_empty());
    let notice = read(&view, cx, |app| app.notice.clone());
    assert!(
        notice.is_some_and(|(message, _)| message.contains("to undo")),
        "the way back is said"
    );
    press(cx, "u");
    assert_eq!(marked(&view, cx), both);

    // The menu's Undo does the same, once.
    press(cx, "!");
    assert!(marked(&view, cx).is_empty());
    cx.dispatch_action(crate::app_menu::Undo);
    draw(cx);
    assert_eq!(marked(&view, cx), both);
    press(cx, "u");
    assert_eq!(marked(&view, cx), both, "nothing more to undo");

    // Keys still reach the window after a click on Export, whose button
    // Unmark all then disables: a disabled button must not keep focus.
    click_on(cx, "export");
    press(cx, "escape");
    press(cx, "!");
    assert!(marked(&view, cx).is_empty());
    press(cx, "u");
    assert_eq!(marked(&view, cx), both);

    // A mark made since stays, and a path it now covers is not re-added.
    press(cx, "!");
    mark_paths(&view, cx, &[temp.path().to_path_buf()]);
    press(cx, "u");
    assert_eq!(marked(&view, cx), vec![temp.path().to_path_buf()]);
    let notice = read(&view, cx, |app| app.notice.clone());
    assert!(
        notice.is_some_and(|(message, _)| message.contains("Nothing to put")),
        "no count of nothing"
    );

    // During a removal the marks are the running list's: Undo leaves
    // them alone and keeps what it holds.
    press(cx, "!");
    update(&view, cx, |app, cx| {
        app.screen = Screen::Running;
        cx.notify();
    });
    cx.dispatch_action(crate::app_menu::Undo);
    draw(cx);
    assert!(marked(&view, cx).is_empty());
    assert!(read(&view, cx, |app| app.unmarked.is_some()));
}

/// The review screen's footer: Export and Unmark all on the left, Back
/// and the commit on the right, the commit in the corner except on
/// Windows, where the default comes first.
#[gpui_kit::test]
fn the_review_footer_puts_the_commit_with_back_on_the_right(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let junk = temp.path().join("junk");
    let (view, cx) = view_over(temp.path(), cx);
    mark_paths(&view, cx, std::slice::from_ref(&junk));
    update(&view, cx, |app, cx| {
        app.screen = Screen::Review;
        cx.notify();
    });
    draw(cx);
    let bounds = |cx: &mut Window, selector| {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"))
    };
    let export = bounds(cx, "export");
    let unmark = bounds(cx, "unmark-all");
    let back = bounds(cx, "back");
    let commit = bounds(cx, "commit");
    assert!(export.right() <= unmark.origin.x, "{export:?} {unmark:?}");
    assert!(unmark.right() < back.origin.x.min(commit.origin.x));
    if cfg!(windows) {
        assert!(commit.right() <= back.origin.x, "{commit:?} {back:?}");
    } else {
        assert!(back.right() <= commit.origin.x, "{back:?} {commit:?}");
    }
    // One row, along the bottom.
    assert_eq!(back.center().y, commit.center().y);
    assert_eq!(export.center().y, commit.center().y);

    // Export opens a menu of the four ways to hand the list on; a row
    // runs and closes it, removing nothing.
    click_on(cx, "export");
    assert!(cx.debug_bounds("export-menu").is_some());
    press(cx, "escape");
    assert!(cx.debug_bounds("export-menu").is_none());
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Review);
    // The keys the menu shows run their row and close it.
    click_on(cx, "export");
    press(cx, "y");
    assert!(cx.debug_bounds("export-menu").is_none());
    assert!(clipboard(cx).contains(&junk.display().to_string()));
    click_on(cx, "export");
    click_on(cx, "copy-prompt");
    assert!(cx.debug_bounds("export-menu").is_none());
    assert!(
        clipboard(cx).contains(&format!("- {}", junk.display())),
        "the prompt names the path"
    );
    assert!(junk.exists(), "nothing was removed");
}

/// While a removal runs, Stop is a button; when it ends, Done is the
/// footer's right-hand button.
#[gpui_kit::test]
fn removing_shows_stop_and_done_sits_on_the_right(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let width = cx.update(|window, _| window.viewport_size().width);
    update(&view, cx, |app, cx| {
        app.screen = Screen::Running;
        cx.notify();
    });
    draw(cx);
    let stop = cx.debug_bounds("stop-removal").expect("a Stop button");
    assert!(stop.origin.x > width / 2., "{stop:?}");
    update(&view, cx, |app, cx| {
        app.screen = Screen::Done;
        cx.notify();
    });
    draw(cx);
    let done = cx.debug_bounds("continue").expect("a Done button");
    assert!(done.origin.x > width / 2., "{done:?}");
    press(cx, "enter");
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Explore);
}

/// The chooser asks, so a stray click outside it neither answers nor
/// throws away what was typed: only Cancel or Escape close it.
#[gpui_kit::test]
fn a_click_outside_the_chooser_keeps_it_open(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, Disktree::open_volumes);
    cx.run_until_parked();
    draw(cx);
    let picker = cx.debug_bounds("source-picker").expect("the chooser");
    let outside = gpui_kit::point(picker.origin.x / 2., picker.center().y);
    cx.simulate_click(outside, gpui_kit::Modifiers::none());
    draw(cx);
    assert!(read(&view, cx, |app| app.volumes_open));
    click_on(cx, "source-cancel");
    assert!(!read(&view, cx, |app| app.volumes_open));
}

/// The source switch: SSH host shows the host field, This computer hides
/// it, and the typed directory is the list's last row, which typing
/// selects so the one Scan acts on it.
#[gpui_kit::test]
fn the_chooser_switches_source_and_scans_the_selected_row(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let selected = fixture();
    let (view, cx) =
        cx.add_window_view(|_, cx| Disktree::choose(options(), 3, None, cx));
    cx.run_until_parked();
    update(&view, cx, |app, _| {
        app.volumes = vec![Volume {
            point: selected.path().join("keep"),
            device: None,
            space: None,
        }];
        app.volumes_loading = false;
    });
    draw(cx);
    assert!(cx.debug_bounds("source-host").is_none());
    choose_ssh(cx);
    assert!(read(&view, cx, |app| app.picker_ssh));
    assert!(cx.debug_bounds("source-host").is_some());
    assert!(
        cx.debug_bounds("source-path").is_none(),
        "nothing to choose from before Connect"
    );
    let switch = cx.debug_bounds("source-switch").expect("the switch");
    cx.simulate_click(
        gpui_kit::point(
            switch.origin.x + switch.size.width / 4.,
            switch.center().y,
        ),
        gpui_kit::Modifiers::none(),
    );
    draw(cx);
    assert!(!read(&view, cx, |app| app.picker_ssh));
    assert!(cx.debug_bounds("source-host").is_none());

    assert_eq!(read(&view, cx, |app| app.volume_highlight), 0);
    let input = cx.debug_bounds("source-path").expect("the Directory row");
    cx.simulate_click(input.center(), gpui_kit::Modifiers::none());
    cx.simulate_input(selected.path().to_str().unwrap());
    draw(cx);
    assert_eq!(read(&view, cx, |app| app.volume_highlight), 1);
    click_on(cx, "source-scan");
    assert_eq!(
        read(&view, cx, |app| app.root_path.clone()),
        dunce::canonicalize(selected.path()).unwrap()
    );
    finish_scan(&view, cx);
}

/// Click the chooser's SSH host segment.
fn choose_ssh(cx: &mut Window) {
    let switch = cx.debug_bounds("source-switch").expect("the switch");
    cx.simulate_click(
        gpui_kit::point(
            switch.origin.x + switch.size.width * 3. / 4.,
            switch.center().y,
        ),
        gpui_kit::Modifiers::none(),
    );
    draw(cx);
}

/// `a` on the review screen copies a prompt for an agent naming the marked
/// path, and removes nothing.
#[gpui_kit::test]
fn the_review_screen_copies_the_list_as_an_agent_prompt(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let junk = temp.path().join("junk");
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.marks.toggle(
            disktree_core::removal::Target {
                path: junk.clone(),
                bytes: 300_000,
                is_dir: true,
                hidden: false,
            },
            app.path_style(),
        );
        app.screen = Screen::Review;
        cx.notify();
    });
    draw(cx);

    press(cx, "a");
    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("a prompt on the clipboard");
    assert!(copied.contains("free up disk space"), "{copied}");
    assert!(
        copied.contains(&format!("- {}", junk.display())),
        "{copied}"
    );
    assert!(junk.exists(), "nothing was removed");
    let notice = read(&view, cx, |app| app.notice.clone());
    assert!(
        notice.is_some_and(|(message, _)| message.contains("copied")),
        "the copy is confirmed"
    );
}

#[gpui_kit::test]
fn remote_review_copies_host_guarded_script_and_prompt(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx, root) = remote_view_over(temp.path(), cx);
    let junk = PathStyle::Posix.join(&root, "junk");
    update(&view, cx, |app, cx| {
        app.remote = Some("dev-alias".into());
        app.remote_identity = Some(disktree_core::identity::HostIdentity {
            hostname: "actual-server".into(),
            machine_id: Some("abcdefabcdefabcdefabcdefabcdefab".into()),
        });
        app.marks.toggle(
            disktree_core::removal::Target {
                path: junk.clone(),
                bytes: 300_000,
                is_dir: true,
                hidden: false,
            },
            app.path_style(),
        );
        app.screen = Screen::Review;
        cx.notify();
    });
    draw(cx);
    press(cx, "y");
    let script = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("script on clipboard");
    assert!(script.contains("actual-server"));
    assert!(script.contains("/etc/machine-id"));
    assert!(script.contains("Wrong hostname"));
    assert!(script.contains("ssh -T"));
    assert!(script.contains("PREVIEW ONLY"));
    press(cx, "a");
    let prompt = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("prompt on clipboard");
    assert!(prompt.contains("actual-server"));
    assert!(prompt.contains("abcdefabcdefabcdefabcdefabcdefab"));
    assert!(prompt.contains("MANDATORY HOST CHECK"));
    assert!(temp.path().join("junk/blob.bin").exists());
}

#[gpui_kit::test]
fn remote_history_keeps_literal_and_nested_names_separate(
    cx: &mut TestAppContext,
) {
    use disktree_core::tree::{Draft, Tree};
    cx.update(init);
    let mut root = Draft::directory("srv");
    root.children.push(Draft::directory(r"a\b"));
    let mut a = Draft::directory("a");
    a.children.push(Draft::directory("b"));
    root.children.push(a);
    root.children.push(Draft::directory("other"));
    let root = Tree::from(root);
    let (view, cx) = cx.add_window_view(move |_, cx| {
        let mut app =
            Disktree::with_tree(PathBuf::from("/srv"), root, options(), 3, cx);
        app.remote = Some("fixture".into());
        app
    });
    update(&view, cx, |app, cx| {
        for path in [r"/srv/a\b", "/srv/a/b", "/srv/other"] {
            let crumbs = app.crumbs_for_path(Path::new(path)).unwrap();
            app.go_to(crumbs, cx);
        }
        let paths: Vec<_> = app
            .history
            .back
            .iter()
            .map(|path| path.to_str().unwrap())
            .collect();
        assert_eq!(paths, ["/srv", r"/srv/a\b", "/srv/a/b"]);
        app.go_back(cx);
        assert_eq!(app.current_path().to_str(), Some("/srv/a/b"));
        app.go_back(cx);
        assert_eq!(app.current_path().to_str(), Some(r"/srv/a\b"));
    });
    draw(cx);
}

/// A Unix host's paths are POSIX on every client. A Windows client must not
/// join them with `\`, split a name such as `logs\old` or `a:b`, or export a
/// delete script that names a different file on the host.
#[gpui_kit::test]
fn remote_paths_keep_the_hosts_separators_on_every_client(
    cx: &mut TestAppContext,
) {
    use disktree_core::tree::{Draft, Metric, NodeKind, Tree, aggregate};

    cx.update(init);
    let mut root = Draft::directory("data");
    let mut logs = Draft::directory(r"logs\old");
    logs.children
        .push(Draft::entry("a:b", NodeKind::File, 4_000));
    root.children.push(logs);
    root.children
        .push(Draft::entry("notes", NodeKind::File, 1_000));
    let mut root = Tree::from(root);
    aggregate(&mut root, Metric::Bytes);
    let root_path = PathBuf::from("/srv/data");
    let (view, cx) = cx.add_window_view(move |_, cx| {
        let mut app =
            Disktree::with_tree(root_path.clone(), root, options(), 3, cx);
        app.remote = Some("dev".into());
        app
    });
    let focus = read(&view, cx, |app| app.focus.clone());
    cx.update(|window, cx| window.focus(&focus, cx));
    draw(cx);
    update(&view, cx, |app, cx| {
        let file = Path::new(r"/srv/data/logs\old/a:b");
        let crumbs = app.crumbs_for_path(file).expect("one name, not two");
        assert_eq!(app.path_at(&crumbs).as_deref(), Some(file));
        assert!(
            app.crumbs_for_path(Path::new("/srv/data/logs/old"))
                .is_none()
        );
        app.select(Some(crumbs), cx);
        app.toggle_mark_selected(cx);
        assert!(app.marks.contains(file, app.path_style()));
        let labels: Vec<_> = app
            .breadcrumbs()
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert_eq!(labels, ["/", "srv", "data"]);
        app.home = Some(PathBuf::from("/srv"));
        assert_eq!(app.display_path(file), r"~/data/logs\old/a:b");
        app.screen = Screen::Review;
        cx.notify();
    });
    draw(cx);
    press(cx, "y");
    let script = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("script on clipboard");
    assert!(script.contains(r"'/srv/data/logs\old/a:b'"), "{script}");
    assert!(!script.contains(r"data\logs"), "{script}");
}

/// Escape in the alert dialog cancels: the dialog closes, the review screen
/// stays, and nothing on disk changes.
#[gpui_kit::test]
fn escape_in_the_delete_dialog_cancels(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.removal_mode = RemovalMode::Permanent;
        app.marks.toggle(
            disktree_core::removal::Target {
                path: temp.path().join("junk"),
                bytes: 300_000,
                is_dir: true,
                hidden: false,
            },
            app.path_style(),
        );
        app.screen = Screen::Review;
        cx.notify();
    });
    draw(cx);

    press(cx, "enter");
    assert!(read(&view, cx, |app| app.confirm_open));
    press(cx, "escape");
    assert!(!read(&view, cx, |app| app.confirm_open), "Escape cancels");
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Review);
    assert!(temp.path().join("junk").exists());
    assert_eq!(
        read(&view, cx, |app| app.marks.len()),
        1,
        "the list is kept"
    );
}

/// With a trash on the machine, the reversible path is the default and
/// commits without a dialog; the permanent one is a deliberate choice.
#[gpui_kit::test]
fn the_trash_is_the_default_when_there_is_one(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let (mode, available) = read(&view, cx, |app| {
        (app.removal_mode, app.trash_backend.is_available())
    });
    if available {
        assert_eq!(mode, RemovalMode::Trash);
    } else {
        assert_eq!(mode, RemovalMode::Permanent);
    }
}

/// `ctrl =` / `ctrl -` / `ctrl 0` change the window's rem, which every size
/// in the app is expressed in, and the mosaic's header band follows it.
#[gpui_kit::test]
fn interface_zoom_scales_the_rem_and_the_header_band(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let rem =
        |cx: &mut Window| cx.update(|window, _| window.rem_size().as_f32());
    let base = rem(cx);
    let band = |view: &Entity<Disktree>, cx: &mut Window| {
        update(view, cx, |app, _| {
            app.layout();
            app.layout_options.header
        })
    };
    let base_band = band(&view, cx);

    press(cx, "ctrl-=");
    let zoomed = rem(cx);
    assert!(zoomed > base, "{zoomed} after zooming in from {base}");
    let zoomed_band = band(&view, cx);
    assert!(
        (zoomed_band / base_band - zoomed / base).abs() < 0.01,
        "the band scales with the rem: {base_band} -> {zoomed_band}"
    );

    press(cx, "ctrl--");
    press(cx, "ctrl--");
    assert!(rem(cx) < base, "zooming out goes below the default");
    press(cx, "ctrl-0");
    assert!((rem(cx) - base).abs() < 0.01, "ctrl 0 resets");
}

/// The crumbs of the directory named `name` directly inside `parent`.
fn child_crumbs(app: &Disktree, parent: &[usize], name: &str) -> Vec<usize> {
    let node = app.node_at(parent).expect("the parent");
    let index = node
        .children()
        .iter()
        .position(|child| child.name() == name)
        .unwrap_or_else(|| panic!("no {name}"));
    let mut crumbs = parent.to_vec();
    crumbs.push(index);
    crumbs
}

/// Regression: the wheel magnified toward the deepest directory under the
/// pointer, then went into the *top-level* one containing it, so the screen
/// after the descent was never the area that was zoomed into.
#[gpui_kit::test]
fn the_wheel_goes_into_the_directory_it_zoomed_into(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    // Point at the middle of junk/deeper's contents: a directory one level
    // below a top-level one.
    let (deeper, point) = update(&view, cx, |app, _| {
        let junk = child_crumbs(app, &[], "junk");
        let deeper = child_crumbs(app, &junk, "deeper");
        let body = app.tile_body(&deeper).expect("deeper is drawn");
        (deeper, (body.x + body.w / 2.0, body.y + body.h / 2.0))
    });

    let mut entered = None;
    for _ in 0..40 {
        let crumbs = update(&view, cx, |app, cx| {
            let (x, y) = app.view.unproject(point.0, point.1);
            let screen = app
                .view
                .project(disktree_core::treemap::Rect::new(x, y, 0.0, 0.0));
            app.zoom_at(screen.x, screen.y, 1.15, true, cx);
            app.crumbs.clone()
        });
        draw(cx);
        if !crumbs.is_empty() {
            entered = Some(crumbs);
            break;
        }
    }
    assert_eq!(
        entered.as_deref(),
        Some(deeper.as_slice()),
        "descended into the pointed-at directory, not its top-level ancestor"
    );
}

/// Regression, keyboard side: Enter on a deep selection enters that
/// directory, and on a file enters the directory holding it.
#[gpui_kit::test]
fn enter_opens_the_selected_directory_at_any_depth(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    let deeper = update(&view, cx, |app, cx| {
        let junk = child_crumbs(app, &[], "junk");
        let deeper = child_crumbs(app, &junk, "deeper");
        app.select(Some(deeper.clone()), cx);
        app.descend(cx);
        deeper
    });
    assert_eq!(read(&view, cx, |app| app.crumbs.clone()), deeper);

    press(cx, "escape");
    press(cx, "escape");
    press(cx, "escape");
    let junk = update(&view, cx, |app, cx| {
        app.go_to(Vec::new(), cx);
        let junk = child_crumbs(app, &[], "junk");
        let blob = child_crumbs(app, &junk, "blob.bin");
        app.select(Some(blob), cx);
        app.descend(cx);
        junk
    });
    assert_eq!(
        read(&view, cx, |app| app.crumbs.clone()),
        junk,
        "a file opens its directory"
    );
}

/// The mark key acts on what the pointer is over when the pointer moved
/// last, and on the keyboard selection after an arrow.
#[gpui_kit::test]
fn the_mark_key_follows_the_pointer_until_the_keyboard_moves(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    // Point at junk's name band, which belongs to junk itself (its body
    // belongs to its children). junk is not the default selection, .cache.
    let (keep, origin, centre) = update(&view, cx, |app, _| {
        let keep = child_crumbs(app, &[], "junk");
        let body = app
            .layout()
            .and_then(|tiles| {
                tiles.iter().find(|tile| tile.crumbs() == keep.as_slice())
            })
            .and_then(|tile| tile.header)
            .expect("junk is drawn with a band");
        (
            keep,
            app.treemap_origin.get(),
            (body.x + body.w / 2.0, body.y + body.h / 2.0),
        )
    });
    assert_ne!(
        read(&view, cx, |app| app.selected.clone()),
        Some(keep.clone())
    );
    cx.simulate_mouse_move(
        Point::new(origin.x + px(centre.0), origin.y + px(centre.1)),
        None,
        gpui_kit::Modifiers::default(),
    );
    draw(cx);

    press(cx, "space");
    let marked = read(&view, cx, |app| app.marks.items().to_vec());
    assert_eq!(marked.len(), 1);
    assert!(
        marked[0].path.ends_with("junk"),
        "marked what the pointer is on: {:?}",
        marked[0].path
    );
    assert_eq!(read(&view, cx, |app| app.selected.clone()), Some(keep));

    // An arrow hands control back to the keyboard selection.
    press(cx, "right");
    assert!(!read(&view, cx, |app| app.pointer_active));
}

/// Regression: after descending, tiles resolved against the scanned root
/// instead of the directory drawn, so labels, hover and marks named strangers.
#[gpui_kit::test]
fn after_descending_every_tile_is_inside_the_directory_drawn(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    let junk = update(&view, cx, |app, cx| {
        let junk = child_crumbs(app, &[], "junk");
        app.select(Some(junk.clone()), cx);
        app.descend(cx);
        junk
    });
    assert_eq!(read(&view, cx, |app| app.crumbs.clone()), junk);
    draw(cx);

    let (paths, labels) = update(&view, cx, |app, _| {
        let tiles = app.layout().map(<[Tile]>::to_vec).unwrap_or_default();
        let paths: Vec<_> = tiles
            .iter()
            .filter_map(|tile| app.path_at(tile.crumbs()))
            .collect();
        let labels: Vec<String> = app
            .prepare()
            .labels
            .into_iter()
            .map(|label| label.text)
            .collect();
        (paths, labels)
    });
    assert!(!paths.is_empty());
    let junk_path = temp.path().join("junk");
    for path in &paths {
        assert!(path.starts_with(&junk_path), "{path:?} is outside junk");
    }
    for label in &labels {
        assert!(
            ["blob.bin", "deeper", "more.bin"].contains(&label.as_str())
                || label.starts_with('+'),
            "label {label} does not belong to junk"
        );
    }

    // Space on the selection marks something inside junk, never elsewhere.
    update(&view, cx, |app, cx| {
        let blob = child_crumbs(app, &junk, "blob.bin");
        app.select(Some(blob), cx);
    });
    press(cx, "space");
    let marked = read(&view, cx, |app| app.marks.items().to_vec());
    assert_eq!(marked.len(), 1);
    assert_eq!(marked[0].path, junk_path.join("blob.bin"));
    assert_eq!(marked[0].bytes, 200_000);

    // And the mark is drawn on the right tile: its crumbs resolve here.
    let hatched = update(&view, cx, |app, _| {
        app.prepare()
            .tiles
            .iter()
            .filter(|tile| tile.marked)
            .count()
    });
    assert_eq!(hatched, 1, "exactly the marked tile is hatched");
}

/// Drive the scan the view started until its outcome has been applied.
///
/// The scan is what is waited on, not the tree: widening keeps the old tree
/// on screen while the wider root is read, so a tree being there does not
/// mean the requested scan has completed.
fn finish_scan(view: &Entity<Disktree>, cx: &mut Window) {
    // Without a scan in flight the poller reports "stopped" at once, and the
    // wait would pass without having waited for anything.
    assert!(
        read(view, cx, |app| app.scan.is_some()),
        "no scan to finish"
    );
    let epoch = read(view, cx, |app| app.scan_epoch);
    for _ in 0..600 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        let running = update(view, cx, |app, cx| app.poll_scan_once(epoch, cx));
        if !running {
            let (error, tree) = read(view, cx, |app| {
                (app.scan_error.clone(), app.tree().is_some())
            });
            assert_eq!(error, None, "the scan failed");
            assert!(tree, "the scan landed without a tree");
            return;
        }
    }
    panic!("the scan never landed");
}

#[gpui_kit::test]
fn dragging_the_panel_edge_resizes_it_within_its_limits(
    cx: &mut TestAppContext,
) {
    use crate::state::{PANEL_MAX_REMS, PANEL_MIN_REMS, panel_width};
    use gpui_kit::{Modifiers, MouseButton};

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1400.), px(900.)));
    draw(cx);
    let handle = cx.debug_bounds("panel-handle").expect("the panel is shown");
    let start = handle.center();
    let before = read(&view, cx, |app| app.panel_rems);

    // A drag starts on the first move and reports moves after it, as a real
    // pointer does in many small steps.
    let drag = |cx: &mut Window, from: Point<Pixels>, to: Point<Pixels>| {
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        for step in 1..=4 {
            let t = step as f32 / 4.0;
            let at = Point::new(from.x + (to.x - from.x) * t, to.y);
            cx.simulate_mouse_move(
                at,
                Some(MouseButton::Left),
                Modifiers::none(),
            );
        }
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        draw(cx);
    };

    // Wider: drag the edge 160 px to the left.
    let wider = Point::new(start.x - px(160.), start.y);
    drag(cx, start, wider);
    let after = read(&view, cx, |app| app.panel_rems);
    assert!(
        (after - before - 10.0).abs() < 0.5,
        "160 px is 10 rem: {before} -> {after}"
    );

    // The limits, for any edge position: a minimum, a maximum, and never
    // so wide that the mosaic gets less room than the panel's own minimum.
    assert!((panel_width(1390., 1400., 16.) - PANEL_MIN_REMS).abs() < 1e-3);
    assert!((panel_width(0., 3000., 16.) - PANEL_MAX_REMS).abs() < 1e-3);
    let squeezed = panel_width(0., 800., 16.);
    assert!((squeezed - (800. / 16. - PANEL_MIN_REMS)).abs() < 1e-3);
    assert!((panel_width(0., 300., 16.) - PANEL_MIN_REMS).abs() < 1e-3);
}

#[gpui_kit::test]
fn widening_reuses_the_tree_it_has_and_reads_only_the_rest(
    cx: &mut TestAppContext,
) {
    use crate::state::Crumb;

    cx.update(init);
    let temp = fixture();
    let inner = temp.path().join("junk");
    let (view, cx) = view_over(&inner, cx);
    update(&view, cx, |app, _| {
        app.disk_root = Some(temp.path().to_path_buf());
        // Stale on purpose: the wider root, a folder, must reset it.
        app.file_table = true;
    });
    let before = read(&view, cx, |app| app.tree().map(|tree| tree.files));

    // The trail runs from the top of the filesystem — "/", or a drive such
    // as "C:\" — and the scanned root sits under its parents.
    let trail = read(&view, cx, Disktree::breadcrumbs);
    let top = temp.path().ancestors().last().expect("a top");
    assert_eq!(trail[0].0, top.display().to_string());
    assert!(
        trail.contains(&(
            temp.path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            Crumb::Above(temp.path().to_path_buf())
        ))
    );

    // Written after the first scan: a memoized subtree cannot see it.
    std::fs::write(inner.join("late.bin"), vec![0_u8; 4096]).expect("write");

    press(cx, "g");
    // The old tree stays on screen while the wider one is read.
    assert!(read(&view, cx, |app| app.tree().is_some()));
    finish_scan(&view, cx);
    assert_eq!(read(&view, cx, |app| app.root_path.clone()), temp.path());
    assert!(
        !read(&view, cx, |app| app.file_table),
        "the offer follows the root it widened to"
    );
    let (reused, rest, selected) = read(&view, cx, |app| {
        let tree = app.tree().expect("the wider tree");
        let junk = tree.child_named("junk").map(|node| node.files);
        let selected = app
            .selected
            .as_deref()
            .and_then(|crumbs| app.node_at(crumbs))
            .map(|node| node.name().to_string());
        (junk, tree.child_named("keep").is_some(), selected)
    });
    assert_eq!(reused, before, "junk was reused, not walked again");
    assert!(rest, "what is outside it was walked");
    assert_eq!(selected.as_deref(), Some("junk"), "where you came from");
}

#[gpui_kit::test]
fn enter_before_the_search_lands_applies_it_when_it_does(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    press(cx, "/");
    cx.simulate_input("blob");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    draw(cx);
    let (applied, count, open) = read(&view, cx, |app| {
        (
            app.filter_applied,
            app.matches.as_deref().map(|matches| matches.count),
            app.find_open,
        )
    });
    assert!(applied, "Enter was not lost to a search in flight");
    assert_eq!(count, Some(2));
    assert!(!open);
}

#[gpui_kit::test]
fn a_crumb_lists_its_siblings_and_jumps_sideways(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton};

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1400.), px(900.)));
    draw(cx);
    // Into junk, so the trail ends in a crumb that has siblings.
    let junk = read(&view, cx, |app| {
        app.tree()
            .and_then(|tree| {
                tree.children().iter().position(|c| c.name() == "junk")
            })
            .expect("junk")
    });
    update(&view, cx, |app, cx| app.go_to(vec![junk], cx));
    draw(cx);

    let last = read(&view, cx, |app| app.breadcrumbs().len() - 1);
    let chevron = cx
        .debug_bounds(Box::leak(format!("crumb-{last}-menu").into_boxed_str()))
        .expect("the current crumb has a menu");
    cx.simulate_click(chevron.center(), Modifiers::none());
    draw(cx);
    assert!(
        cx.debug_bounds("sibling-menu").is_some(),
        "the menu is drawn"
    );
    let (names, highlighted) = read(&view, cx, |app| {
        let menu = app.crumb_menu.clone().expect("open");
        let (rows, _) = app.siblings(&menu.parent);
        let names: Vec<String> =
            rows.iter().map(|row| row.name.clone()).collect();
        let highlighted = names_at(&names, menu.highlighted);
        (names, highlighted)
    });
    assert!(
        names.contains(&".cache".to_string())
            && names.contains(&"keep".to_string())
    );
    assert_eq!(highlighted, "junk", "it opens on where you are");

    // Walk to .cache by name, not by rank: it and junk are close in size.
    let target = names
        .iter()
        .position(|name| name == ".cache")
        .expect("listed");
    let here = names
        .iter()
        .position(|name| name == "junk")
        .expect("listed");
    let key = if target < here { "up" } else { "down" };
    for _ in 0..target.abs_diff(here) {
        press(cx, key);
    }
    press(cx, "enter");
    let (crumbs, open) = read(&view, cx, |app| {
        (app.crumbs.clone(), app.crumb_menu.is_some())
    });
    let cache = read(&view, cx, |app| {
        app.tree()
            .and_then(|tree| {
                tree.children().iter().position(|c| c.name() == ".cache")
            })
            .expect(".cache")
    });
    assert_eq!(crumbs, vec![cache], "went sideways into .cache");
    assert!(!open);

    // A click outside closes it.
    let chevron = cx
        .debug_bounds(Box::leak(format!("crumb-{last}-menu").into_boxed_str()))
        .expect("menu chevron");
    cx.simulate_click(chevron.center(), Modifiers::none());
    draw(cx);
    assert!(read(&view, cx, |app| app.crumb_menu.is_some()));
    cx.simulate_mouse_down(
        gpui_kit::point(px(700.), px(600.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_up(
        gpui_kit::point(px(700.), px(600.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    draw(cx);
    assert!(read(&view, cx, |app| app.crumb_menu.is_none()));
}

fn names_at(names: &[String], index: usize) -> String {
    names.get(index).cloned().unwrap_or_default()
}

#[gpui_kit::test]
fn marking_a_directory_marks_everything_inside_it(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let crumbs = |view: &Entity<Disktree>, cx: &mut Window, rel: &str| {
        let path = temp.path().join(rel);
        read(view, cx, |app| app.crumbs_for_path(&path)).expect(rel)
    };
    let junk = crumbs(&view, cx, "junk");
    let deeper = crumbs(&view, cx, "junk/deeper");

    // Marked inside first, then the directory around it: one mark remains,
    // and it covers the inner one.
    update(&view, cx, |app, cx| app.toggle_mark(&deeper, cx));
    update(&view, cx, |app, cx| app.toggle_mark(&junk, cx));
    let marks = read(&view, cx, |app| {
        app.marks
            .items()
            .iter()
            .map(|item| item.path.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        marks,
        vec![temp.path().join("junk")],
        "the inner mark is absorbed"
    );

    // Every tile inside it is drawn marked: the ones below the mark are
    // "covered", which paints the same fill and label.
    let mosaic = update(&view, cx, |app, _| app.prepare());
    let (inside, covered) = update(&view, cx, |app, _| {
        let tiles = app.layout().map(<[Tile]>::to_vec).unwrap_or_default();
        let inside: Vec<bool> = tiles
            .iter()
            .zip(&mosaic.tiles)
            .filter(|(tile, _)| {
                tile.crumbs().starts_with(&junk) && tile.crumbs() != junk
            })
            .map(|(_, deco)| deco.covered)
            .collect();
        (inside.len(), inside.iter().all(|covered| *covered))
    });
    assert!(inside >= 2, "blob.bin and deeper are drawn inside junk");
    assert!(covered, "every tile inside junk goes with it");

    // The mark is not said by colour alone (WCAG 1.4.1): its name, and every
    // name inside it, is struck through. The name itself is left whole, so
    // a narrow tile still shows it rather than a repeated word.
    let label = mosaic
        .labels
        .iter()
        .find(|label| label.text == "junk")
        .expect("junk is large enough to be labelled, under its own name");
    assert!(label.marked, "a marked name is struck through");
    assert!(label.decoration.marked, "the label paints its tile's state");
    let covered: Vec<_> = mosaic
        .labels
        .iter()
        .filter(|label| label.decoration.covered)
        .collect();
    assert!(!covered.is_empty(), "something inside junk is labelled");
    assert!(
        covered.iter().all(|label| label.marked),
        "what goes with a mark is struck through as going"
    );
    assert!(
        covered.iter().all(|label| matches!(
            &*label.text,
            "blob.bin" | "deeper" | "more.bin"
        )),
        "covered names carry no prefix: {:?}",
        covered.iter().map(|label| &label.text).collect::<Vec<_>>()
    );
    assert!(
        mosaic
            .labels
            .iter()
            .filter(
                |label| !label.decoration.marked && !label.decoration.covered
            )
            .all(|label| !label.marked),
        "nothing else is struck"
    );
    // Gone inside the mark, the trail still shows it: the crumbs for junk
    // and anything in it are struck, the rest are not.
    let keep = crumbs(&view, cx, "keep");
    read(&view, cx, |app| {
        assert!(app.goes_with_a_mark(&junk));
        assert!(app.goes_with_a_mark(&deeper));
        assert!(!app.goes_with_a_mark(&keep));
        assert!(!app.goes_with_a_mark(&[]), "the scanned root");
    });

    // Marking something inside it is refused, and says why.
    update(&view, cx, |app, cx| app.toggle_mark(&deeper, cx));
    let (count, notice) = read(&view, cx, |app| {
        (
            app.marks.len(),
            app.notice.as_ref().map(|(text, _)| text.clone()),
        )
    });
    assert_eq!(count, 1);
    assert!(notice.is_some_and(|text| text.contains("goes with the marked")));

    // Unmarking the directory unmarks everything.
    update(&view, cx, |app, cx| app.toggle_mark(&junk, cx));
    assert!(read(&view, cx, |app| app.marks.is_empty()));
}

/// `<` and `>` retrace the directories visited, are disabled when there is
/// nowhere to go, and a fresh move ends what was ahead.
#[gpui_kit::test]
fn back_and_forward_retrace_where_you_have_been(cx: &mut TestAppContext) {
    use gpui_kit::Modifiers;

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1400.), px(900.)));
    draw(cx);
    let can = |view: &Entity<Disktree>, cx: &Window| {
        read(view, cx, |app| (app.can_go_back(), app.can_go_forward()))
    };
    assert_eq!(can(&view, cx), (false, false), "nowhere to go yet");

    let (junk, deeper, keep) = update(&view, cx, |app, cx| {
        let junk = child_crumbs(app, &[], "junk");
        let deeper = child_crumbs(app, &junk, "deeper");
        let keep = child_crumbs(app, &[], "keep");
        app.select(Some(deeper.clone()), cx);
        app.descend(cx);
        (junk, deeper, keep)
    });
    draw(cx);
    assert_eq!(can(&view, cx), (true, false));

    // Hovering `<` shows the card for where it goes, as hovering a tile
    // does: here, the scanned root. It hangs below the button, never on it.
    let back = cx
        .debug_bounds("history-back")
        .expect("the button is drawn");
    cx.simulate_mouse_move(back.center(), None, Modifiers::none());
    draw(cx);
    let card = cx.debug_bounds("history-tip").expect("the card is shown");
    assert!(card.top() >= back.bottom(), "{card:?} covers {back:?}");
    assert_eq!(
        read(&view, cx, |app| app.history_target(true)),
        Some((0, Vec::new()))
    );

    let click = |cx: &mut Window, selector: &'static str| {
        let bounds = cx.debug_bounds(selector).expect("the button is drawn");
        cx.simulate_click(bounds.center(), Modifiers::none());
        draw(cx);
    };
    click(cx, "history-back");
    assert_eq!(
        read(&view, cx, |app| app.crumbs.clone()),
        Vec::<usize>::new()
    );
    assert_eq!(can(&view, cx), (false, true), "back at the start");

    click(cx, "history-forward");
    assert_eq!(read(&view, cx, |app| app.crumbs.clone()), deeper);
    assert_eq!(can(&view, cx), (true, false));

    // Up a level is a move of its own, and the keys retrace it too.
    press(cx, "backspace");
    assert_eq!(read(&view, cx, |app| app.crumbs.clone()), junk);
    press(cx, "alt-left");
    assert_eq!(read(&view, cx, |app| app.crumbs.clone()), deeper);
    press(cx, "alt-left");
    assert_eq!(
        read(&view, cx, |app| app.crumbs.clone()),
        Vec::<usize>::new()
    );
    press(cx, "alt-right");
    assert_eq!(read(&view, cx, |app| app.crumbs.clone()), deeper);

    // Going somewhere new from the middle of the history drops what was
    // ahead of it, as in a browser.
    press(cx, "alt-left");
    update(&view, cx, |app, cx| app.go_to(keep, cx));
    draw(cx);
    assert_eq!(can(&view, cx), (true, false), "the forward trail is gone");
    press(cx, "alt-left");
    assert_eq!(
        read(&view, cx, |app| app.crumbs.clone()),
        Vec::<usize>::new()
    );
}

/// Regression: children are ordered by the metric, so switching between
/// Size and Files reorders them. The directory on screen and the selection
/// are found again by path, rather than following their old positions into
/// a sibling.
#[gpui_kit::test]
fn switching_the_metric_keeps_the_directory_and_the_selection(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = tempfile::TempDir::new().expect("tempdir");
    let root = temp.path();
    // Largest by size, but fewest files: the two swap places.
    std::fs::create_dir_all(root.join("big")).expect("mkdir");
    std::fs::write(root.join("big/one.bin"), vec![b'x'; 500_000])
        .expect("write");
    std::fs::write(root.join("big/two.bin"), vec![b'x'; 1_000]).expect("write");
    for index in 0..5 {
        let file = root.join(format!("many/{index}.txt"));
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, b"x").expect("write");
    }
    let (view, cx) = view_over(root, cx);
    draw(cx);

    update(&view, cx, |app, cx| {
        let big = child_crumbs(app, &[], "big");
        assert_eq!(big, vec![0], "largest first by size");
        app.go_to(big.clone(), cx);
        let one = child_crumbs(app, &big, "one.bin");
        app.select(Some(one), cx);
    });
    draw(cx);

    update(&view, cx, |app, cx| app.set_mode(1, cx));
    draw(cx);
    let (here, selected) = read(&view, cx, |app| {
        (
            app.current_path(),
            app.selected
                .as_deref()
                .and_then(|crumbs| app.path_at(crumbs)),
        )
    });
    assert_eq!(here, root.join("big"));
    assert_eq!(selected, Some(root.join("big/one.bin")));

    update(&view, cx, |app, cx| app.set_mode(0, cx));
    draw(cx);
    assert_eq!(read(&view, cx, Disktree::current_path), root.join("big"));
}

/// The size/files toggle reorders the window's tree where it lies rather
/// than copying it, yet still hands out a new tree; a tree another task is
/// reading is copied and left in its old order.
#[gpui_kit::test]
fn toggling_the_metric_reorders_without_copying_the_tree(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    std::fs::create_dir_all(root.join("big")).expect("mkdir");
    std::fs::write(root.join("big/one.bin"), vec![b'x'; 500_000])
        .expect("write");
    for index in 0..5 {
        let file = root.join(format!("many/{index}.txt"));
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, b"x").expect("write");
    }
    let (view, cx) = view_over(root, cx);
    draw(cx);
    let first = |app: &Disktree| {
        app.tree
            .as_ref()
            .expect("tree")
            .root()
            .children()
            .at(0)
            .name()
            .to_string()
    };

    let (tree, children) = read(&view, cx, |app| {
        let tree = app.tree.as_ref().expect("tree");
        // Where the root's children start: an in-place reorder keeps it.
        let children = tree.root().children().at(0).facts();
        (std::sync::Arc::as_ptr(tree), std::ptr::from_ref(children))
    });
    assert_eq!(read(&view, cx, first), "big");
    update(&view, cx, Disktree::toggle_metric);
    draw(cx);
    assert_eq!(read(&view, cx, first), "many", "most files first");
    let (after, children_after) = read(&view, cx, |app| {
        let tree = app.tree.as_ref().expect("tree");
        // Where the root's children start: an in-place reorder keeps it.
        let children = tree.root().children().at(0).facts();
        (std::sync::Arc::as_ptr(tree), std::ptr::from_ref(children))
    });
    assert_eq!(children_after, children, "the nodes were not copied");
    assert_ne!(after, tree, "but the tree is a new one");

    let held = read(&view, cx, |app| app.tree.clone().expect("tree"));
    update(&view, cx, Disktree::toggle_metric);
    draw(cx);
    assert_eq!(read(&view, cx, first), "big");
    assert_eq!(
        held.root().children().at(0).name(),
        "many",
        "a reader keeps its order"
    );
}

/// Escape stops a first scan. What the walk found so far is not shown as a
/// tree, a late result is ignored, and `r` starts over.
#[gpui_kit::test]
fn escape_cancels_the_first_scan_and_r_starts_it_again(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let root = temp.path().to_path_buf();
    let (view, cx) =
        cx.add_window_view(move |_, cx| Disktree::new(root, options(), 3, cx));
    let focus = view.read_with(cx, |app, _| app.focus.clone());
    cx.update(|window, cx| window.focus(&focus, cx));
    draw(cx);
    let epoch = read(&view, cx, |app| app.scan_epoch);

    press(cx, "escape");
    let (scanning, cancelled) = read(&view, cx, |app| {
        (app.scan.is_some(), app.progress.cancelled)
    });
    assert!(!scanning, "the walk was dropped");
    assert!(cancelled, "the panel can say it stopped");
    std::thread::sleep(std::time::Duration::from_millis(50));
    let polling = update(&view, cx, |app, cx| app.poll_scan_once(epoch, cx));
    assert!(!polling, "the old poller stops");
    assert!(read(&view, cx, |app| app.tree().is_none()));
    draw(cx);

    press(cx, "r");
    finish_scan(&view, cx);
    assert!(read(&view, cx, |app| app.tree().is_some()));
}

/// Escape stops a widening scan and keeps the tree it started from.
#[gpui_kit::test]
fn escape_cancels_widening_and_keeps_the_tree_on_screen(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let inner = temp.path().join("junk");
    let (view, cx) = view_over(&inner, cx);
    update(&view, cx, |app, _| {
        app.disk_root = Some(temp.path().to_path_buf());
    });
    let before = read(&view, cx, |app| app.tree().map(|tree| tree.files));

    press(cx, "g");
    assert!(read(&view, cx, |app| app.scan.is_some()));
    press(cx, "escape");
    let (scanning, root, scan_root, files) = read(&view, cx, |app| {
        (
            app.scan.is_some(),
            app.root_path.clone(),
            app.scan_root.clone(),
            app.tree().map(|tree| tree.files),
        )
    });
    assert!(!scanning);
    assert_eq!(root, inner);
    assert_eq!(scan_root, inner, "the trail stops showing a widening");
    assert_eq!(files, before, "the tree on screen is unchanged");
}

/// The mouse's back and forward buttons retrace the same history as
/// alt-arrows and the header buttons, and only on the explore screen.
#[gpui_kit::test]
fn mouse_side_buttons_go_back_and_forward(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton, NavigationDirection};

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1400.), px(900.)));
    draw(cx);

    let (junk, deeper) = update(&view, cx, |app, cx| {
        let junk = child_crumbs(app, &[], "junk");
        let deeper = child_crumbs(app, &junk, "deeper");
        app.select(Some(junk.clone()), cx);
        app.descend(cx);
        app.select(Some(deeper.clone()), cx);
        app.descend(cx);
        (junk, deeper)
    });
    draw(cx);
    assert_eq!(read(&view, cx, |app| app.crumbs.clone()), deeper);

    // Over the mosaic, buttons 8 and 9 step the history, as alt-arrows do.
    let mosaic = cx.debug_bounds("treemap").expect("the mosaic is drawn");
    let at = mosaic.center();
    let press_button = |cx: &mut Window, button: MouseButton| {
        cx.simulate_mouse_move(at, None, Modifiers::none());
        cx.simulate_mouse_down(at, button, Modifiers::none());
        cx.simulate_mouse_up(at, button, Modifiers::none());
        draw(cx);
    };
    press_button(cx, MouseButton::Navigate(NavigationDirection::Back));
    assert_eq!(read(&view, cx, |app| app.crumbs.clone()), junk);
    press_button(cx, MouseButton::Navigate(NavigationDirection::Forward));
    assert_eq!(read(&view, cx, |app| app.crumbs.clone()), deeper);

    // On the review screen the same press changes nothing: the marked list
    // is not somewhere the history can take you back to.
    update(&view, cx, |app, cx| app.toggle_mark(&junk, cx));
    press(cx, "c");
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Review);
    press_button(cx, MouseButton::Navigate(NavigationDirection::Back));
    assert_eq!(read(&view, cx, |app| app.crumbs.clone()), deeper);
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Review);
}

/// The restart as administrator reopens the same root and options: every
/// flag it writes is one the command line reads back.
#[test]
fn restart_arguments_parse_back_to_the_same_scan() {
    use disktree_core::tree::Metric;
    let temp = tempfile::TempDir::new().expect("tempdir");
    let root = temp.path().join("a folder");
    std::fs::create_dir(&root).expect("mkdir");
    let root = dunce::canonicalize(&root).expect("canonical root");
    let changed = ScanOptions {
        apparent_size: true,
        follow_links: true,
        include_hidden: false,
        one_filesystem: false,
        metric: Metric::Files,
        threads: disktree_core::scan_threads::ScanThreads {
            max_threads: 8,
            adaptive: true,
            retained_throughput: 0.85,
            system_cpu_limit: Some(0.70),
        },
        ..ScanOptions::default()
    };
    for (options, depth) in [(changed, 5), (ScanOptions::default(), 1)] {
        let args = crate::state::restart_args(&options, depth, &root);
        let parsed = crate::parse_args(args.into_iter()).expect("parses");
        assert_eq!(parsed.root, root);
        assert_eq!(parsed.depth, depth);
        let got = &parsed.options;
        assert_eq!(got.threads.max_threads, options.threads.max_threads);
        assert_eq!(got.threads.adaptive, options.threads.adaptive);
        assert!(
            (got.threads.retained_throughput
                - options.threads.retained_throughput)
                .abs()
                < f64::EPSILON
        );
        assert_eq!(
            got.threads.system_cpu_limit,
            options.threads.system_cpu_limit
        );
        assert_eq!(
            (
                got.apparent_size,
                got.follow_links,
                got.include_hidden,
                got.one_filesystem,
                got.metric,
            ),
            (
                options.apparent_size,
                options.follow_links,
                options.include_hidden,
                options.one_filesystem,
                options.metric,
            )
        );
    }
}

#[cfg(target_os = "macos")]
#[gpui_kit::test]
fn apfs_clones_use_private_bytes_in_review_and_refresh(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    // The macOS system-tree guard deliberately refuses canonical /private.
    let temp = tempfile::tempdir_in(std::env::current_dir().expect("cwd"))
        .expect("tempdir in the checkout");
    let root = std::fs::canonicalize(temp.path()).expect("canonical root");
    std::fs::create_dir(root.join("copies")).expect("mkdir");
    let original = root.join("original");
    std::fs::write(&original, vec![0x5a; 8 * 1024 * 1024]).expect("write");
    assert!(
        std::process::Command::new("cp")
            .arg("-c")
            .arg(&original)
            .arg(root.join("copies/clone"))
            .status()
            .expect("cp -c")
            .success()
    );
    let options = ScanOptions::default();
    let tree = scan(&root, options.clone()).expect("scan");
    let root_for_view = root.clone();
    let (view, cx) = cx.add_window_view(move |_, cx| {
        Disktree::with_tree(root_for_view.clone(), tree, options, 3, cx)
    });
    draw(cx);
    // The window answers a listing with the clone split `pwrdisk mcp`
    // computes in process, and draws the same split for the selection.
    update(&view, cx, |app, cx| {
        let answer = app.answer_agent(
            &disktree_link::Request::Ls {
                path: root.clone(),
                limit: 10,
            },
            cx,
        );
        let disktree_link::Response::Listing(listing) = answer else {
            panic!("{answer:?}");
        };
        let tree = app.tree().expect("tree");
        let here =
            disktree_link::Listing::of(&root, tree.bytes, &root, tree, 10);
        assert_eq!(listing, here);
        let copies = listing
            .children
            .iter()
            .find(|child| child.name == "copies")
            .and_then(|child| child.clones)
            .expect("clone facts for copies");
        assert!(copies.private <= 8 * 1024, "{copies:?}");
        assert!(copies.shared >= 8 * 1024 * 1024 - 64 * 1024, "{copies:?}");
        let node = tree
            .children()
            .iter()
            .find(|node| node.name() == "copies")
            .expect("copies");
        let split = crate::views::clone_split(node).expect("a split");
        assert!(split.contains("shared with other APFS clones"), "{split}");
    });
    // The accessible file list draws the split on its rows too.
    update(&view, cx, |app, _| app.file_list = true);
    draw(cx);
    update(&view, cx, |app, _| app.file_list = false);
    update(&view, cx, |app, cx| {
        let index = app
            .tree()
            .expect("tree")
            .children()
            .iter()
            .position(|node| node.name() == "copies")
            .expect("copies");
        app.toggle_mark(&[index], cx);
        app.screen = Screen::Review;
    });
    draw(cx);
    read(&view, cx, |app| {
        assert_eq!(app.plan().bytes(), 8 * 1024 * 1024);
        assert_eq!(app.plan().reclaimable_bytes(), 0);
        let state = app.agent_state();
        let estimate = state.estimate.expect("linked estimate");
        assert_eq!(estimate.paths, vec![root.join("copies")]);
        assert_eq!((estimate.lower, estimate.upper), (0, Some(0)));
        assert_eq!(estimate.on_volume, 0);
        // The agent hears why: the listed bytes are clone blocks.
        assert!(estimate.shared >= 8 * 1024 * 1024 - 64 * 1024);
    });
    // Remote selections are exports: local clone metadata and mount
    // attribution cannot authorize or project a remote saving.
    update(&view, cx, |app, _| {
        app.remote = Some("test-fixture".into());
        let selection = app.plan();
        assert_eq!(selection.targets.len(), 1);
        assert_eq!(selection.reclaim(), 0);
        assert_eq!(selection.unattributed(), 8 * 1024 * 1024);
        assert!(app.run.is_none());
    });
    draw(cx);
    update(&view, cx, |app, _| {
        app.remote = None;
        assert_eq!(app.plan().reclaimable_bytes(), 0);
    });
    update(&view, cx, |app, _| {
        app.removal_mode = RemovalMode::Permanent;
        app.confirm_open = true;
    });
    draw(cx);
    update(&view, cx, |app, _| app.confirm_open = false);
    // Separate marks complete the group. Changing tile weights must never
    // turn the review's byte values into file counts.
    update(&view, cx, |app, cx| {
        app.options.metric = disktree_core::tree::Metric::Files;
        let index = app
            .tree()
            .expect("tree")
            .children()
            .iter()
            .position(|node| node.name() == "original")
            .expect("original");
        app.toggle_mark(&[index], cx);
        let plan = app.plan();
        assert_eq!(plan.bytes(), 16 * 1024 * 1024);
        assert_eq!(plan.reclaimable_bytes(), 8 * 1024 * 1024);
        assert_eq!(plan.possible_bytes(), 8 * 1024 * 1024);
        let estimate = app.agent_state().estimate.expect("linked union");
        assert_eq!(estimate.lower, 8 * 1024 * 1024);
        assert_eq!(estimate.upper, Some(estimate.lower));
    });
    draw(cx);
    crate::marks::work::reset();
    read(&view, cx, |app| {
        for _ in 0..4 {
            assert_eq!(app.plan().reclaimable_bytes(), 8 * 1024 * 1024);
        }
    });
    assert_eq!(
        crate::marks::work::RESOLUTION.get(),
        0,
        "warm plans must not resolve paths again"
    );
    update(&view, cx, |app, _| {
        app.marks.remove(&root.join("original"), app.path_style());
        assert_eq!(app.plan().reclaimable_bytes(), 0);
        app.options.metric = disktree_core::tree::Metric::Bytes;
    });
    // Break sharing and refresh the same absolute-path mark.
    std::fs::remove_file(root.join("copies/clone")).expect("remove clone");
    std::fs::write(root.join("copies/clone"), vec![0x31; 8 * 1024 * 1024])
        .expect("replace");
    let refreshed = scan(&root, ScanOptions::default()).expect("rescan");
    update(&view, cx, |app, _| {
        app.marks.refresh(
            &root,
            refreshed.root(),
            disktree_core::tree::Metric::Bytes,
            app.path_style(),
        );
        app.tree = Some(std::sync::Arc::new(refreshed.clone()));
    });
    assert_eq!(
        read(&view, cx, |app| app.plan().reclaimable_bytes()),
        8 * 1024 * 1024
    );
    draw(cx);
}

#[gpui_kit::test]
fn power_efficiency_menu_saves_without_discarding_the_tree(
    cx: &mut TestAppContext,
) {
    use crate::power::{self, PowerEfficiency as Power};
    use gpui_kit::Modifiers;

    cx.update(init);
    let temp = fixture();
    let config = tempfile::tempdir().expect("config");
    let path = config.path().join("power-efficiency");
    let (view, cx) = view_over(temp.path(), cx);
    // Wide enough for the gauge to sit in the bar rather than in ⋮.
    cx.simulate_resize(gpui_kit::size(px(1280.), px(800.)));
    update(&view, cx, |app, _| {
        app.power_settings_path = Some(path.clone());
        // Enough CPUs that every preset is offered, whatever runs the test.
        app.cpu_threads = 18;
    });
    let epoch = read(&view, cx, |app| app.scan_epoch);
    for preset in Power::ALL {
        draw(cx);
        let button = cx.debug_bounds("power-efficiency").expect("control");
        cx.simulate_click(button.center(), Modifiers::none());
        draw(cx);
        let choice = cx.debug_bounds(preset.key()).expect("preset");
        cx.simulate_click(choice.center(), Modifiers::none());
        draw(cx);
        assert_eq!(power::load(&path).expect("saved"), preset);
        read(&view, cx, |app| {
            assert_eq!(app.power_choice, Some(preset));
            assert_eq!(
                app.options.threads.max_threads,
                preset.threads(app.cpu_threads)
            );
            assert!(!app.options.threads.adaptive);
            assert!(app.power_menu.is_none());
            assert_eq!(app.scan_epoch, epoch);
            assert!(app.tree.is_some());
        });
    }
    // Persistence failure must not masquerade as a saved preference.
    update(&view, cx, |app, cx| {
        app.power_settings_path = Some(config.path().to_owned());
        app.set_power_efficiency(Power::Miser, cx);
    });
    assert!(read(&view, cx, |app| app.notice.as_ref().is_some_and(
        |(message, _)| message.contains("could not be saved")
    )));
    update(&view, cx, Disktree::start_scan);
    assert_eq!(read(&view, cx, |app| app.scan_epoch), epoch + 1);
}

#[gpui_kit::test]
fn the_power_menu_offers_only_what_the_cpus_can_tell_apart(
    cx: &mut TestAppContext,
) {
    use crate::power::PowerEfficiency as Power;
    use gpui_kit::Modifiers;

    cx.update(init);
    let temp = fixture();
    let config = tempfile::tempdir().expect("config");
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1280.), px(800.)));
    update(&view, cx, |app, _| {
        app.power_settings_path = Some(config.path().join("power"));
        // Aggressive and Drain My Battery would both run four workers.
        app.cpu_threads = 4;
        app.power_choice = Some(Power::Balanced);
    });
    draw(cx);
    let toggle = cx.debug_bounds("power-efficiency").expect("control");
    cx.simulate_mouse_move(toggle.center(), None, Modifiers::none());
    cx.simulate_click(toggle.center(), Modifiers::none());
    draw(cx);
    assert_eq!(read(&view, cx, |app| app.power_menu), Some(1));

    // The pointer takes the highlight, and the check stays on what is saved.
    let miser = cx.debug_bounds(Power::Miser.key()).expect("row");
    cx.simulate_mouse_move(miser.center(), None, Modifiers::none());
    draw(cx);
    assert_eq!(read(&view, cx, |app| app.power_menu), Some(0));
    let balanced = cx.debug_bounds(Power::Balanced.key()).expect("row");
    let check = cx.debug_bounds("power-check").expect("check");
    assert!(balanced.contains(&check.center()), "check on {check:?}");

    // A disabled row ignores the click and leaves the menu to be dismissed.
    let drain = cx.debug_bounds(Power::DrainMyBattery.key()).expect("row");
    cx.simulate_click(drain.center(), Modifiers::none());
    draw(cx);
    read(&view, cx, |app| {
        assert_eq!(app.power_choice, Some(Power::Balanced));
        assert!(app.notice.is_none());
    });

    // The arrows step over what is not offered, and Enter chooses.
    update(&view, cx, |app, cx| {
        app.power_menu = None;
        app.toggle_power_menu(cx);
    });
    press(cx, "down");
    assert_eq!(read(&view, cx, |app| app.power_menu), Some(1));
    press(cx, "up");
    assert_eq!(read(&view, cx, |app| app.power_menu), Some(0));
    press(cx, "enter");
    read(&view, cx, |app| {
        assert_eq!(app.power_choice, Some(Power::Miser));
        assert_eq!(app.options.threads.max_threads, 2);
        assert!(app.power_menu.is_none());
    });

    // The button closes what it opened, rather than the press outside the
    // menu closing it only for the click to open it again.
    draw(cx);
    let toggle = cx.debug_bounds("power-efficiency").expect("control");
    cx.simulate_mouse_move(toggle.center(), None, Modifiers::none());
    cx.simulate_click(toggle.center(), Modifiers::none());
    draw(cx);
    assert!(read(&view, cx, |app| app.power_menu.is_some()));
    cx.simulate_click(toggle.center(), Modifiers::none());
    draw(cx);
    assert!(read(&view, cx, |app| app.power_menu.is_none()));
}

/// Exercise prepare through actual window draws, including repeated renders.
#[gpui_kit::test]
fn marked_window_resolution_budget(cx: &mut TestAppContext) {
    use crate::marks::work;
    use disktree_core::removal::Target;
    use disktree_core::tree::{Draft, Metric, NodeKind, Tree, aggregate};

    cx.update(init);
    let mut measure = |count| {
        let temp = tempfile::tempdir().expect("tempdir");
        let root_path = temp.path().to_path_buf();
        let mut tree = Draft::directory("root");
        for index in 0..count {
            let mut folder = Draft::directory(format!("folder-{index:05}"));
            folder
                .children
                .push(Draft::entry("payload", NodeKind::File, 1));
            tree.children.push(folder);
        }
        let mut tree = Tree::from(tree);
        aggregate(&mut tree, Metric::Bytes);
        let (view, window) = cx.add_window_view(move |_, cx| {
            Disktree::with_tree(root_path.clone(), tree, options(), 3, cx)
        });
        update(&view, window, |app, _| {
            for index in 0..count {
                app.marks.toggle(
                    Target {
                        path: temp
                            .path()
                            .join(format!("folder-{index:05}"))
                            .join("payload"),
                        bytes: 1,
                        is_dir: false,
                        hidden: false,
                    },
                    app.path_style(),
                );
            }
        });
        work::reset();
        let start = std::time::Instant::now();
        for _ in 0..4 {
            update(&view, window, |_, cx| cx.notify());
            draw(window);
        }
        let elapsed = start.elapsed();
        let frames = work::FRAMES.get();
        let operations = work::RESOLUTION.get();
        let tiles = work::TILES.get();
        assert!(tiles > 0 && tiles <= count * 4 * frames);
        work::reset();
        let warm_start = std::time::Instant::now();
        for _ in 0..4 {
            update(&view, window, |_, cx| cx.notify());
            draw(window);
        }
        let warm_elapsed = warm_start.elapsed();
        let warm_frames = work::FRAMES.get();
        let warm_work = work::RESOLUTION.get();
        assert!(warm_frames >= 4);
        assert!(
            warm_work <= 64 * warm_frames,
            "unchanged renders must not resolve all marks: {warm_work}"
        );
        assert_eq!(
            work::TILES.get() * frames,
            tiles * warm_frames,
            "visible tile work remains linear in frame count"
        );
        eprintln!(
            "warm window M={count} prepares={warm_frames} resolution={warm_work} tiles={} wall={warm_elapsed:?}",
            work::TILES.get()
        );
        window.update(|window, _| window.remove_window());
        assert!(frames >= 4, "budget must cover actual renders: {frames}");
        assert!(operations > 0, "resolution instrumentation must run");
        eprintln!(
            "window M={count} N={} draws=4 prepares={frames} work={operations} wall={elapsed:?}",
            count * 2
        );
        (operations, frames)
    };
    let (small, small_frames) = measure(128);
    let (large, large_frames) = measure(1024);
    assert!(
        large * small_frames <= small * large_frames * 16,
        "8x marks/folders per frame: {small}/{small_frames} -> {large}/{large_frames}"
    );
    assert!(
        large <= 1024 * large_frames * 16,
        "absolute work cap: {large}"
    );
}

#[gpui_kit::test]
fn marked_tiles_follow_metric_rescan_and_mark_changes(cx: &mut TestAppContext) {
    use disktree_core::removal::Target;
    use std::sync::Arc;

    fn marked_paths(app: &mut Disktree) -> Vec<PathBuf> {
        let mosaic = app.prepare();
        let tiles = app.layout().expect("layout").to_vec();
        tiles
            .iter()
            .zip(mosaic.tiles)
            .filter_map(|(tile, deco)| {
                deco.marked.then(|| app.path_at(tile.crumbs())).flatten()
            })
            .collect()
    }

    cx.update(init);
    let temp = fixture();
    let (view, window) = view_over(temp.path(), cx);
    let junk = temp.path().join("junk");
    update(&view, window, |app, cx| {
        app.marks.toggle(
            Target {
                path: junk.clone(),
                bytes: 300_000,
                is_dir: true,
                hidden: false,
            },
            app.path_style(),
        );
        cx.notify();
    });
    draw(window);
    assert_eq!(
        update(&view, window, |app, _| marked_paths(app)),
        std::slice::from_ref(&junk)
    );
    update(&view, window, Disktree::toggle_metric);
    draw(window);
    assert_eq!(
        update(&view, window, |app, _| marked_paths(app)),
        std::slice::from_ref(&junk)
    );
    update(&view, window, |app, cx| {
        // Child lists are fixed once built: reorder through a draft.
        let mut tree = app.tree.as_ref().expect("tree").root().to_draft();
        tree.children.reverse();
        app.tree = Some(Arc::new(disktree_core::tree::Tree::from(tree)));
        cx.notify();
    });
    draw(window);
    assert_eq!(
        update(&view, window, |app, _| marked_paths(app)),
        std::slice::from_ref(&junk)
    );
    update(&view, window, |app, cx| app.unmark(&junk, cx));
    draw(window);
    assert!(update(&view, window, |app, _| marked_paths(app)).is_empty());
}

/// Opt-in latency evidence, separate from deterministic CI work budgets.
#[gpui_kit::test]
#[ignore = "manual interaction timing on synthetic temporary files"]
fn marked_interaction_timings(cx: &mut TestAppContext) {
    use crate::marks::work;
    use disktree_core::removal::Target;
    use gpui_kit::Modifiers;

    fn measure(
        view: &Entity<Disktree>,
        window: &mut Window,
        action: &str,
        repeat: usize,
        run: impl FnOnce(&mut Window),
    ) {
        let before = read(view, window, |app| app.marks.len());
        work::reset();
        let start = std::time::Instant::now();
        run(window);
        let elapsed = start.elapsed();
        let after = read(view, window, |app| app.marks.len());
        assert!(work::RENDERS.get() > 0, "must render the real screen");
        assert!(work::PLANS.get() > 0, "must include removal projections");
        assert_eq!(work::REFRESHES.get(), 0, "interaction must not rescan");
        eprintln!(
            "interaction action={action} repeat={repeat} marks={before}->{after} renders={} prepares={} plans={} refreshes={} wall={elapsed:?}",
            work::RENDERS.get(),
            work::FRAMES.get(),
            work::PLANS.get(),
            work::REFRESHES.get(),
        );
    }

    cx.update(init);
    let temp = tempfile::tempdir().expect("tempdir");
    // One broad directory, kept fixed while only the number of marks grows.
    // Each marked folder contains a file: descendants still make one mark.
    for index in 0..1025 {
        let folder = temp.path().join(format!("folder-{index:05}"));
        std::fs::create_dir(&folder).expect("mkdir");
        std::fs::write(folder.join("payload"), [b'x'; 64]).expect("write");
    }
    let tree = scan(temp.path(), options()).expect("scan private fixture");
    let extra_path = temp.path().join("folder-01024");
    for count in [0, 1, 8, 32, 128, 1024] {
        let root_path = temp.path().to_path_buf();
        let snapshot = tree.clone();
        let (view, window) = cx.add_window_view(move |_, cx| {
            Disktree::with_tree(root_path.clone(), snapshot, options(), 3, cx)
        });
        window.simulate_resize(gpui_kit::size(px(1400.), px(900.)));
        let focus = read(&view, window, |app| app.focus.clone());
        window.update(|window, cx| window.focus(&focus, cx));
        update(&view, window, |app, cx| {
            assert!(app.space.is_some() && app.show_selection);
            for index in 0..count {
                app.marks.toggle(
                    Target {
                        path: temp.path().join(format!("folder-{index:05}")),
                        bytes: 64,
                        is_dir: true,
                        hidden: false,
                    },
                    app.path_style(),
                );
            }
            cx.notify();
        });
        draw(window);
        for repeat in 1..=3 {
            update(&view, window, |app, cx| {
                app.pointer_active = false;
                app.hovered = None;
                app.pointer = None;
                let crumbs =
                    app.crumbs_for_path(&extra_path).expect("extra folder");
                app.select(Some(crumbs), cx);
            });
            draw(window);
            measure(&view, window, "mark-space", repeat, |window| {
                press(window, "space");
            });
            assert_eq!(read(&view, window, |app| app.marks.len()), count + 1);
            let at = update(&view, window, |app, _| {
                let tile = app
                    .layout()
                    .expect("layout")
                    .iter()
                    .find(|tile| {
                        matches!(
                            tile.kind,
                            disktree_core::treemap::TileKind::Node { .. }
                        )
                    })
                    .expect("node tile")
                    .rect;
                let rect = app.view.project(tile);
                let origin = app.treemap_origin.get();
                Point::new(
                    origin.x + px(rect.x + rect.w / 2.),
                    origin.y + px(rect.y + rect.h / 2.),
                )
            });
            measure(&view, window, "hover", repeat, |window| {
                window.simulate_mouse_move(at, None, Modifiers::none());
                draw(window);
            });
            assert!(read(&view, window, |app| app.hovered.is_some()));
            measure(&view, window, "review-c", repeat, |window| {
                press(window, "c");
            });
            assert_eq!(read(&view, window, |app| app.screen), Screen::Review);
            press(window, "escape");
            update(&view, window, |app, cx| {
                app.pointer_active = false;
                app.hovered = None;
                app.pointer = None;
                let crumbs =
                    app.crumbs_for_path(&extra_path).expect("extra folder");
                app.select(Some(crumbs), cx);
            });
            draw(window);
            press(window, "space");
            assert_eq!(read(&view, window, |app| app.marks.len()), count);
        }
        window.update(|window, _| window.remove_window());
    }
}

/// The disk root is private and empty; only the displayed tree is synthetic.
fn queue_budget_view(
    count: usize,
    cx: &mut TestAppContext,
) -> (tempfile::TempDir, Entity<Disktree>, &mut Window) {
    use disktree_core::tree::{Draft, Metric, NodeKind, Tree, aggregate};

    let temp = tempfile::tempdir().expect("tempdir");
    let root_path = temp.path().to_path_buf();
    let mut tree = Draft::directory("root");
    let mut parent = Draft::directory("container");
    for index in 0..count {
        parent.children.push(Draft::entry(
            format!("file-{index:05}"),
            NodeKind::File,
            1,
        ));
    }
    tree.children.push(parent);
    tree.children.push(Draft::entry("keep", NodeKind::File, 1));
    let mut tree = Tree::from(tree);
    aggregate(&mut tree, Metric::Bytes);
    let (view, window) = cx.add_window_view(move |_, cx| {
        Disktree::with_tree(root_path.clone(), tree, options(), 3, cx)
    });
    (temp, view, window)
}

#[gpui_kit::test]
fn queue_construction_budget(cx: &mut TestAppContext) {
    use crate::marks::work;

    cx.update(init);
    let mut measure = |count| {
        let (_temp, view, window) = queue_budget_view(count, cx);
        let operations = update(&view, window, |app, cx| {
            work::reset();
            let start = std::time::Instant::now();
            for index in 0..count {
                app.toggle_mark(&[0, index], cx);
            }
            assert_eq!(app.marks.len(), count);
            assert_eq!(
                work::FRAMES.get(),
                0,
                "measure insertion before rendering"
            );
            let operations = work::MARKING.get() + work::COMPACTION.get();
            assert!(operations > 0);
            eprintln!(
                "queue-construction M={count} work={operations} wall={:?}",
                start.elapsed()
            );
            operations
        });
        window.update(|window, _| window.remove_window());
        operations
    };
    let small = measure(128);
    let large = measure(1024);
    assert!(large <= small * 16, "8x marks: {small} -> {large}");
    assert!(large <= 1024 * 32, "absolute insertion budget: {large}");
}

#[gpui_kit::test]
fn parent_absorption_budget(cx: &mut TestAppContext) {
    use crate::marks::work;

    cx.update(init);
    let mut measure = |count| {
        let (temp, view, window) = queue_budget_view(count, cx);
        let operations = update(&view, window, |app, cx| {
            let keep = app.target_at(&[1]).expect("neighbour");
            app.marks.toggle(keep.clone(), app.path_style());
            for index in 0..count {
                let target = app.target_at(&[0, index]).expect("file");
                app.marks.toggle(target, app.path_style());
            }
            work::reset();
            let start = std::time::Instant::now();
            app.toggle_mark(&[0], cx);
            assert_eq!(app.marks.len(), 2);
            assert_eq!(app.marks.items()[0], keep, "neighbour stays in order");
            assert_eq!(
                app.marks.items()[1].path,
                temp.path().join("container")
            );
            assert_eq!(app.marks.items()[1].bytes, count as u64);
            let operations = work::MARKING.get() + work::COMPACTION.get();
            assert!(operations > 0);
            eprintln!(
                "parent-absorption M={count} work={operations} compactions={} wall={:?}",
                work::COMPACTION.get(),
                start.elapsed()
            );
            operations
        });
        window.update(|window, _| window.remove_window());
        operations
    };
    let small = measure(128);
    let large = measure(1024);
    assert!(large <= small * 16, "8x descendants: {small} -> {large}");
    assert!(large <= 1024 * 32, "absolute absorption budget: {large}");
}

fn review_status_work(
    count: usize,
    covered: bool,
    cx: &mut TestAppContext,
) -> (usize, usize) {
    use crate::marks::work;
    use disktree_core::removal::Target;

    let (temp, view, window) = queue_budget_view(count, cx);
    update(&view, window, |app, _| {
        if covered {
            let parent = app.target_at(&[0]).expect("parent");
            app.marks.toggle(parent, app.path_style());
        }
        for index in 0..count {
            let target = if covered {
                app.target_at(&[0, index]).expect("file")
            } else {
                Target {
                    path: temp
                        .path()
                        .with_extension("outside")
                        .join(format!("file-{index:05}")),
                    bytes: 1,
                    is_dir: false,
                    hidden: false,
                }
            };
            app.marks.toggle(target, app.path_style());
        }
        app.screen = Screen::Review;
    });
    work::reset();
    let start = std::time::Instant::now();
    for _ in 0..4 {
        update(&view, window, |_, cx| cx.notify());
        draw(window);
    }
    let elapsed = start.elapsed();
    let frames = work::RENDERS.get();
    let operations = work::REVIEW.get();
    assert!(
        frames >= 4 && work::PLANS.get() >= 4,
        "must render and plan Review"
    );
    let rows = (count + usize::from(covered)).min(1200);
    assert_eq!(
        work::REVIEW_ROWS.get(),
        rows * frames,
        "retain the display cap"
    );
    assert_eq!(
        work::BLOCKED_ROWS.get(),
        if covered { 0 } else { rows * frames }
    );
    assert_eq!(
        work::COVERED_ROWS.get(),
        if covered { (rows - 1) * frames } else { 0 }
    );
    assert!(operations > 0);
    eprintln!(
        "review-status covered={covered} M={count} work={operations} renders={frames} rows={rows} per-render={} wall={elapsed:?}",
        operations / frames
    );
    window.update(|window, _| window.remove_window());
    (operations, frames)
}

fn review_status_budget(cx: &mut TestAppContext, covered: bool) {
    let (small, small_frames) = review_status_work(128, covered, cx);
    let (large, large_frames) = review_status_work(1024, covered, cx);
    assert!(
        large * small_frames <= small * large_frames * 16,
        "8x status rows: {small}/{small_frames} -> {large}/{large_frames}"
    );
    assert!(
        large <= 1024 * large_frames * 8,
        "absolute row-status budget: {large}"
    );
    let (capped, capped_frames) = review_status_work(8192, covered, cx);
    assert!(
        capped <= 8192 * capped_frames * 8,
        "status budget above row cap: {capped}"
    );
}

#[gpui_kit::test]
fn blocked_review_status_budget(cx: &mut TestAppContext) {
    cx.update(init);
    review_status_budget(cx, false);
}

#[gpui_kit::test]
fn covered_review_status_budget(cx: &mut TestAppContext) {
    cx.update(init);
    review_status_budget(cx, true);
}

#[gpui_kit::test]
fn folder_queue_construction_budget(cx: &mut TestAppContext) {
    use crate::marks::work;
    use disktree_core::tree::{Draft, NodeKind, Tree, aggregate};
    use std::sync::Arc;

    cx.update(init);
    let mut measure = |count| {
        let (_temp, view, window) = queue_budget_view(count, cx);
        let operations = update(&view, window, |app, cx| {
            let metric = app.options.metric;
            // Child lists are fixed once built: turn each file into a
            // folder through a draft, then build the tree again.
            let mut draft = app.tree.as_ref().expect("tree").root().to_draft();
            for child in &mut draft.children[0].children {
                child.kind = NodeKind::Directory;
                child
                    .children
                    .push(Draft::entry("payload", NodeKind::File, 1));
            }
            let mut tree = Tree::from(draft);
            aggregate(&mut tree, metric);
            app.tree = Some(Arc::new(tree));
            work::reset();
            let start = std::time::Instant::now();
            for index in 0..count {
                app.toggle_mark(&[0, index], cx);
            }
            assert_eq!(app.marks.len(), count);
            assert!(app.marks.items().iter().all(|item| item.is_dir));
            assert_eq!(work::FRAMES.get(), 0);
            let operations = work::MARKING.get() + work::COMPACTION.get();
            assert!(operations > 0);
            eprintln!(
                "folder-queue M={count} work={operations} wall={:?}",
                start.elapsed()
            );
            operations
        });
        window.update(|window, _| window.remove_window());
        operations
    };
    let small = measure(128);
    let large = measure(1024);
    assert!(large <= small * 16, "8x folders: {small} -> {large}");
    assert!(
        large <= 1024 * 32,
        "absolute folder insertion budget: {large}"
    );
}

/// Every frame the title strip can be drawn in, as the platforms report
/// them: the window harness has no compositor to ask, so each is pinned.
fn window_frames() -> Vec<(&'static str, crate::chrome::Frame)> {
    use crate::chrome::{Frame, Platform};
    use gpui_kit::{
        Decorations, Tiling, WindowButton, WindowButtonLayout, WindowControls,
    };

    let frame = |platform, decorations, layout, fullscreen| {
        Frame::new(
            platform,
            decorations,
            layout,
            WindowControls::default(),
            fullscreen,
        )
    };
    let floating = Decorations::Client {
        tiling: Tiling::default(),
    };
    // Close, minimize, maximize on the left: the layout that puts the
    // controls where macOS has its stoplights.
    let on_the_left = WindowButtonLayout {
        left: [
            Some(WindowButton::Close),
            Some(WindowButton::Minimize),
            Some(WindowButton::Maximize),
        ],
        right: [None; 3],
    };
    vec![
        (
            "macOS",
            frame(Platform::MacOs, Decorations::Server, None, false),
        ),
        (
            "macOS full screen",
            frame(Platform::MacOs, Decorations::Server, None, true),
        ),
        (
            "Windows",
            frame(Platform::Windows, Decorations::Server, None, false),
        ),
        (
            "Hyprland or KDE",
            frame(Platform::Linux, Decorations::Server, None, false),
        ),
        (
            "GNOME, controls right",
            frame(Platform::Linux, floating, None, false),
        ),
        (
            "GNOME, controls left",
            frame(Platform::Linux, floating, Some(on_the_left), false),
        ),
        (
            "GNOME, maximized",
            frame(
                Platform::Linux,
                Decorations::Client {
                    tiling: Tiling::tiled(),
                },
                None,
                false,
            ),
        ),
    ]
}

#[gpui_kit::test]
fn the_title_strip_draws_on_every_window_frame(cx: &mut TestAppContext) {
    use crate::chrome::Controls;
    use crate::ui::chrome;

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    for (name, frame) in window_frames() {
        for screen in [Screen::Explore, Screen::Review, Screen::Done] {
            update(&view, cx, |app, cx| {
                app.chrome_pin = Some(frame.clone());
                app.screen = screen;
                cx.notify();
            });
            draw(cx);

            let strip = cx.debug_bounds("title-strip").expect(name);
            // The band the stoplights centre in, then the divider below it.
            assert_eq!(
                strip.size.height,
                chrome::STRIP + px(1.),
                "{name}, {screen:?}"
            );
            let brand = cx.debug_bounds("strip-brand").expect(name);
            assert!(brand.size.width > px(0.), "{name}: the brand is drawn");

            // The brand stays clear of the controls at either end.
            match &frame.leading {
                // The brand's area runs to the edge, padding and all.
                Controls::None => assert_eq!(brand.left(), strip.left()),
                Controls::Stoplights => assert!(
                    brand.left() - strip.left() >= chrome::STOPLIGHT_ZONE,
                    "{name}: brand at {:?}",
                    brand.left() - strip.left()
                ),
                Controls::Buttons(_) => {
                    let controls =
                        cx.debug_bounds("window-controls-leading").expect(name);
                    assert!(controls.left() >= strip.left());
                    assert!(brand.left() >= controls.right(), "{name}");
                }
            }
            if let Controls::Buttons(_) = &frame.trailing {
                let controls =
                    cx.debug_bounds("window-controls-trailing").expect(name);
                assert!(
                    (strip.right() - controls.right()).abs() <= px(1.),
                    "{name}: the controls sit at the right edge"
                );
                assert!(brand.right() < controls.left(), "{name}");
            }
        }
    }
}

#[gpui_kit::test]
fn a_client_side_frame_keeps_a_resize_ring_until_it_is_tiled(
    cx: &mut TestAppContext,
) {
    use crate::ui::chrome;

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let frames = window_frames();
    let pin = |name: &str| {
        frames
            .iter()
            .find(|(each, _)| *each == name)
            .map(|(_, frame)| frame.clone())
    };

    update(&view, cx, |app, cx| {
        app.chrome_pin = pin("GNOME, controls right");
        cx.notify();
    });
    draw(cx);
    let root = cx.debug_bounds("disktree-root").expect("root");
    let frame = cx.debug_bounds("client-frame").expect("client frame");
    // The ring, then the window's hairline border.
    let inset = chrome::RESIZE_RING + px(1.);
    assert_eq!(root.left() - frame.left(), inset);
    assert_eq!(root.top() - frame.top(), inset);
    for grip in ["grip-top", "grip-bottom-right", "grip-left"] {
        assert!(cx.debug_bounds(grip).is_some(), "{grip}");
    }

    // Maximized or tiled, the compositor owns the edges: no ring, no grips.
    update(&view, cx, |app, cx| {
        app.chrome_pin = pin("GNOME, maximized");
        cx.notify();
    });
    draw(cx);
    let root = cx.debug_bounds("disktree-root").expect("root");
    let frame = cx.debug_bounds("client-frame").expect("client frame");
    assert_eq!(root.left() - frame.left(), px(1.));
    assert_eq!(root.top() - frame.top(), px(1.));
    assert!(cx.debug_bounds("grip-top").is_none());

    // Where the compositor draws the frame, the content is the window.
    update(&view, cx, |app, cx| {
        app.chrome_pin = pin("Hyprland or KDE");
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("client-frame").is_none());
}

#[gpui_kit::test]
fn the_wordmark_gives_way_in_a_narrow_window(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.chrome_pin = window_frames().first().map(|(_, f)| f.clone());
        cx.notify();
    });
    let brand_width = |cx: &mut Window| {
        draw(cx);
        cx.debug_bounds("strip-brand").expect("brand").size.width
    };

    cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
    let wide = brand_width(cx);
    cx.simulate_resize(gpui_kit::size(px(900.), px(600.)));
    let narrow = brand_width(cx);
    assert!(
        narrow < wide / 2.,
        "only the mark is left at 900 pt: {narrow:?} of {wide:?}"
    );
    // The trail keeps the crumb you are in.
    assert!(cx.debug_bounds("title-strip").is_some());
}

#[gpui_kit::test]
fn a_press_on_the_strip_arms_a_move_and_a_click_disarms_it(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.chrome_pin = window_frames().first().map(|(_, f)| f.clone());
        cx.notify();
    });
    draw(cx);
    let empty = cx.debug_bounds("strip-drag").expect("empty strip");
    let at = empty.center();
    let none = gpui_kit::Modifiers::none();
    let left = gpui_kit::MouseButton::Left;

    // A press arms the move. The move itself, on the first motion with
    // the button down, cannot run here: the test platform leaves
    // `start_window_move` unimplemented.
    cx.simulate_mouse_down(at, left, none);
    assert!(read(&view, cx, |app| app.strip_drag));

    // Released without moving, it was a click, and leaves nothing armed
    // for the next time the pointer passes over with a button held.
    cx.simulate_mouse_up(at, left, none);
    assert!(!read(&view, cx, |app| app.strip_drag));

    // The keyboard still belongs to the treemap.
    let focused = cx.update(|window, cx| {
        let focus = view.read(cx).focus.clone();
        focus.is_focused(window)
    });
    assert!(focused, "a press on the strip leaves the treemap focused");
}

#[gpui_kit::test]
fn the_tooltip_follows_the_pointer_inside_a_client_side_frame(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    // Floating under GNOME: the content sits inside the resize ring.
    update(&view, cx, |app, cx| {
        app.chrome_pin = window_frames()
            .into_iter()
            .find(|(name, _)| *name == "GNOME, controls right")
            .map(|(_, frame)| frame);
        cx.notify();
    });
    draw(cx);
    let origin = read(&view, cx, |app| app.treemap_origin.get());
    let at = origin + Point::new(px(40.), px(40.));
    cx.simulate_mouse_move(at, None, gpui_kit::Modifiers::default());
    draw(cx);

    let tip = cx.debug_bounds("cursor-tooltip").expect("a tooltip");
    // Beside the pointer by the tooltip's gap, wherever the root starts.
    let gap = crate::ui::space::MD.0 * crate::ui::BASE_REM;
    assert_eq!(
        tip.left(),
        at.x + px(gap),
        "{tip:?} for a pointer at {at:?}"
    );
    assert_eq!(tip.top(), at.y + px(gap));
}

/// Drive the window as `pwrdisk mcp` does, over the real socket: say
/// hello, mark, show, open the review, unmark, hang up. The marks land in
/// the window, the screen follows, and the header names the agent while it
/// is there.
#[cfg(any(unix, windows))]
#[gpui_kit::test]
fn an_agent_marks_shows_and_reviews_over_the_link(cx: &mut TestAppContext) {
    use disktree_link::{Link, MarkRequest, Request, Response};

    cx.update(init);
    let temp = fixture();
    let root = temp.path().to_path_buf();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    #[cfg(not(windows))]
    let socket = temp.path().join("link.sock");
    #[cfg(windows)]
    let socket = PathBuf::from(format!(
        r"\\.\pipe\pwrdisk-test-{}",
        temp.path()
            .file_name()
            .expect("temp name")
            .to_string_lossy()
    ));
    let listener =
        disktree_link::Listener::bind_at(&socket).expect("bind the link");
    update(&view, cx, |app, cx| app.attach_agent(listener, cx));

    // The client blocks on each answer, so it runs on its own thread while
    // the test pumps the window's side, as the timer task would.
    let mut link = Some(Link::connect_at(&socket).expect("connect"));
    let mut ask = |cx: &mut Window, request: Request| -> Response {
        let mut taken = link.take().expect("the link is at hand");
        let worker = std::thread::spawn(move || {
            let response = taken.call(&request).expect("an answer");
            (taken, response)
        });
        for _ in 0..400 {
            update(&view, cx, |app, cx| {
                app.poll_agent_once(cx);
            });
            if worker.is_finished() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (returned, response) = worker.join().expect("client thread");
        link = Some(returned);
        draw(cx);
        response
    };

    let refused = ask(
        cx,
        Request::Hello {
            client: "Old MCP fixture".into(),
            version: disktree_link::VERSION - 1,
        },
    );
    let Response::Error { message } = refused else {
        panic!("incompatible Hello must be refused");
    };
    assert!(message.contains("Incompatible PwrDisk link protocols"));
    assert!(message.contains("restart/reconnect"));
    assert!(message.contains("updated installation"));
    assert!(
        message.contains(concat!("PwrDisk GUI ", env!("CARGO_PKG_VERSION")))
    );
    let hello = ask(
        cx,
        Request::Hello {
            client: "Claude Code".into(),
            version: disktree_link::VERSION,
        },
    );
    let Response::State(state) = hello else {
        panic!("hello answers with the state: {hello:?}");
    };
    assert_eq!(state.root, root);
    assert!(state.scanned && !state.scanning);
    assert!(state.marks.is_empty());
    assert_eq!(
        read(&view, cx, |app| app
            .agent
            .as_ref()
            .and_then(|agent| agent.presence.clone())
            .map(|presence| presence.client)),
        Some("Claude Code".to_string())
    );
    assert!(
        cx.debug_bounds("agent-pill").is_some(),
        "the header names the connected agent"
    );

    // Marking lands in the window's marks; the agent's own number is only
    // a stand-in, and a wrong one is corrected from the tree.
    let marked = ask(
        cx,
        Request::Mark {
            root: root.clone(),
            targets: vec![MarkRequest {
                path: root.join("junk"),
                bytes: 7,
                is_dir: false,
            }],
        },
    );
    let Response::Marked { state, refused } = marked else {
        panic!("mark answers with the state: {marked:?}");
    };
    assert!(refused.is_empty(), "{refused:?}");
    assert_eq!(state.marks.len(), 1);
    // The window measured the mark from its own tree, not from the
    // agent's number.
    let measured = read(&view, cx, |app| {
        app.crumbs_for_path(&root.join("junk"))
            .and_then(|crumbs| app.node_at(&crumbs).map(|node| node.bytes))
    })
    .expect("junk is in the tree");
    assert_eq!(state.marks[0].bytes, measured);
    assert!(state.marks[0].is_dir);
    assert_ne!(measured, 7);
    assert_eq!(read(&view, cx, |app| app.plan().bytes()), measured);
    assert!(state.tree.is_some_and(|tree| tree.files > 0));

    // A path the tree does not have is refused, not guessed at.
    let missing = ask(
        cx,
        Request::Mark {
            root: root.clone(),
            targets: vec![MarkRequest {
                path: root.join("nowhere"),
                bytes: 1,
                is_dir: false,
            }],
        },
    );
    let Response::Marked { refused, .. } = missing else {
        panic!("{missing:?}");
    };
    assert_eq!(refused.len(), 1);
    assert_eq!(read(&view, cx, |app| app.marks.len()), 1);

    // The agent reads the window's tree: a listing and a search.
    let listed = ask(
        cx,
        Request::Ls {
            path: root.clone(),
            limit: 10,
        },
    );
    let Response::Listing(listing) = listed else {
        panic!("{listed:?}");
    };
    assert!(listing.children.iter().any(|child| child.name == "junk"));
    assert_eq!(listing.root_bytes, listing.entry.bytes);
    let outside = ask(
        cx,
        Request::Ls {
            path: PathBuf::from("/definitely/elsewhere"),
            limit: 10,
        },
    );
    assert!(
        matches!(outside, Response::Error { ref message } if message.contains("outside")),
        "{outside:?}"
    );
    let found = ask(
        cx,
        Request::Find(disktree_link::FindRequest {
            kind: "file".into(),
            name: Some("blob".into()),
            limit: 5,
            ..disktree_link::FindRequest::default()
        }),
    );
    let Response::Found(found) = found else {
        panic!("{found:?}");
    };
    // Two blobs, the larger first.
    assert_eq!(found.count, 2);
    assert_eq!(found.matches[0].path, root.join(".cache/blob.bin"));
    assert_eq!(found.matches[1].path, root.join("junk/blob.bin"));

    // A scan the agent asks for runs in the window; the state says when it
    // has landed, and the marks survive it.
    let rescanned = ask(
        cx,
        Request::Scan {
            root: root.clone(),
            options: disktree_link::ScanOptionsRequest::default(),
        },
    );
    assert!(
        matches!(rescanned, Response::State(ref s) if s.scanning),
        "{rescanned:?}"
    );
    finish_scan(&view, cx);
    let after = ask(cx, Request::State);
    let Response::State(after) = after else {
        panic!("{after:?}");
    };
    assert!(!after.scanning && after.scanned);
    assert_eq!(after.marks.len(), 1);

    // A path inside the mark is left alone, not toggled off or added.
    ask(
        cx,
        Request::Mark {
            root: root.clone(),
            targets: vec![MarkRequest {
                path: root.join("junk/deeper"),
                bytes: 100_000,
                is_dir: true,
            }],
        },
    );
    assert_eq!(read(&view, cx, |app| app.marks.len()), 1);

    // Showing a directory opens it; the view follows the agent.
    let shown = ask(
        cx,
        Request::Show {
            root: root.clone(),
            path: root.join("junk/deeper"),
        },
    );
    assert_eq!(shown, Response::Ok);
    let (crumbs, expected) = read(&view, cx, |app| {
        (
            app.crumbs.clone(),
            app.crumbs_for_path(&root.join("junk/deeper")),
        )
    });
    assert_eq!(Some(crumbs), expected);

    // Review opens the review screen over the marks.
    assert_eq!(ask(cx, Request::Review), Response::Ok);
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Review);
    assert!(
        temp.path().join("junk/blob.bin").exists(),
        "nothing removed"
    );

    // Unmarking everything empties the list and leaves the review screen.
    let cleared = ask(cx, Request::Unmark { paths: vec![] });
    assert!(matches!(cleared, Response::State(ref s) if s.marks.is_empty()));
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Explore);
    assert_eq!(
        ask(cx, Request::Review),
        Response::error("nothing is marked")
    );

    // Removal is a separate, explicit protocol operation, not a UI key.
    ask(
        cx,
        Request::Mark {
            root: root.clone(),
            targets: vec![MarkRequest {
                path: root.join("junk"),
                bytes: 0,
                is_dir: true,
            }],
        },
    );
    let preview_request = Request::PrepareRemoval {
        mode: disktree_link::RemovalChoice::Permanent,
    };
    let Response::RemovalPreview(preview) = ask(cx, preview_request.clone())
    else {
        panic!("preview");
    };
    assert_eq!(preview.estimate.paths, preview.paths);
    assert_eq!(preview.estimate.lower, 0, "apparent fixture sizes");
    assert_eq!(preview.estimate.upper, None);
    assert!(preview.estimate.partial);
    // Estimation must leave the pending single-use removal intent intact.
    let before = ask(cx, Request::State);
    let estimated = ask(
        cx,
        Request::Estimate {
            paths: vec![root.join("junk")],
            peer_scope: None,
            peer_limit: 10,
        },
    );
    let Response::Estimate(report) = estimated else {
        panic!("estimate response");
    };
    assert_eq!(report.accepted, [root.join("junk")]);
    assert!(report.reclaim.is_none(), "apparent fixture");
    assert_eq!(ask(cx, Request::State), before);
    assert!(read(&view, cx, |app| app
        .agent
        .as_ref()
        .expect("agent")
        .removal_preview
        .is_some()));
    let confirmed = |preview: &disktree_link::RemovalPreview| {
        disktree_link::ConfirmedRemoval {
            rescan: false,
            preview_id: preview.preview_id.clone(),
            mode: preview.mode,
            paths: preview.paths.clone(),
            confirmation: preview.mode.confirmation().into(),
        }
    };
    let mut wrong = confirmed(&preview);
    wrong.paths.push(root.join("keep"));
    assert!(matches!(
        ask(cx, Request::Remove(wrong)),
        Response::Error { .. }
    ));
    assert!(root.join("junk/blob.bin").exists());
    // A failed attempt consumes its preview as well.
    assert!(matches!(
        ask(cx, Request::Remove(confirmed(&preview))),
        Response::Error { .. }
    ));
    let Response::RemovalPreview(changed) = ask(cx, preview_request.clone())
    else {
        panic!("preview");
    };
    ask(
        cx,
        Request::Unmark {
            paths: vec![root.join("junk")],
        },
    );
    assert!(matches!(
        ask(cx, Request::Remove(confirmed(&changed))),
        Response::Error { .. }
    ));
    ask(
        cx,
        Request::Mark {
            root: root.clone(),
            targets: vec![MarkRequest {
                path: root.join("junk"),
                bytes: 0,
                is_dir: true,
            }],
        },
    );
    // A capacity-policy change also requires a new preview and confirmation.
    let settings = temp.path().join("fixture-settings");
    update(&view, cx, |app, _| {
        app.removal_settings_dir = Some(settings.clone());
    });
    let Response::RemovalPreview(old_policy) = ask(cx, preview_request.clone())
    else {
        panic!("preview");
    };
    crate::safety::save(
        Some(&settings),
        disktree_core::removal_limit::Limit::new(10).expect("10%"),
    )
    .expect("save");
    assert!(matches!(
        ask(cx, Request::Remove(confirmed(&old_policy))),
        Response::Error { .. }
    ));
    assert!(root.join("junk/blob.bin").exists());
    crate::safety::save(
        Some(&settings),
        disktree_core::removal_limit::Limit::default(),
    )
    .expect("restore fixture policy");
    let Response::RemovalPreview(preview) = ask(cx, preview_request) else {
        panic!("preview");
    };
    assert_eq!(preview.paths, [root.join("junk")]);
    assert_eq!(preview.limit_percent, 5);
    let started = ask(cx, Request::Remove(confirmed(&preview)));
    assert!(
        matches!(started, Response::State(ref state) if state.screen == "running"),
        "{started:?}"
    );
    assert!(
        matches!(
            ask(cx, Request::Remove(confirmed(&preview))),
            Response::State(_)
        ),
        "a duplicate reports the running operation without replay"
    );
    let epoch = read(&view, cx, |app| app.run_epoch);
    let mut done = false;
    for _ in 0..400 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        done = update(&view, cx, |app, cx| {
            app.poll_removal_once(epoch, cx);
            app.screen == Screen::Done
        });
        if done {
            break;
        }
    }
    assert!(done);
    assert!(!root.join("junk").exists());
    assert!(root.join("keep/notes.txt").exists());
    assert!(root.join(".cache/blob.bin").exists());
    assert_eq!(read(&view, cx, |app| app.run_list.totals().failed), 0);

    // A pending MCP form does not own native UI approval. Complete another
    // exact preview through the same entry point as native confirmation.
    ask(
        cx,
        Request::Mark {
            root: root.clone(),
            targets: vec![MarkRequest {
                path: root.join(".cache"),
                bytes: 0,
                is_dir: true,
            }],
        },
    );
    let Response::RemovalPreview(native) = ask(
        cx,
        Request::PrepareRemoval {
            mode: disktree_link::RemovalChoice::Permanent,
        },
    ) else {
        panic!("native preview");
    };
    update(&view, cx, |app, cx| {
        app.removal_mode = RemovalMode::Permanent;
        app.begin_removal(cx);
    });
    let native_epoch = read(&view, cx, |app| app.run_epoch);
    let Response::State(running) = ask(cx, Request::State) else {
        panic!("state");
    };
    let operation = running.removal_operation.expect("native run correlated");
    assert_eq!(operation.preview_id, native.preview_id);
    assert!(!operation.finished);
    for _ in 0..400 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        if update(&view, cx, |app, cx| {
            !app.poll_removal_once(native_epoch, cx)
        }) {
            break;
        }
    }
    assert!(!root.join(".cache").exists());
    let Response::State(completed) =
        ask(cx, Request::Remove(confirmed(&native)))
    else {
        panic!("late request reports outcome");
    };
    let operation = completed.removal_operation.expect("completed outcome");
    assert!(operation.finished);
    assert_eq!(
        (operation.removed, operation.failed, operation.pending),
        (1, 0, 0)
    );
    assert_eq!(
        read(&view, cx, |app| app.run_epoch),
        native_epoch,
        "no second run"
    );
    assert!(root.join("keep/notes.txt").exists());

    // Hanging up takes the name out of the header.
    drop(link);
    for _ in 0..400 {
        let gone = update(&view, cx, |app, cx| {
            app.poll_agent_once(cx);
            app.agent
                .as_ref()
                .is_some_and(|agent| agent.presence.is_none())
        });
        if gone {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    draw(cx);
    assert!(cx.debug_bounds("agent-pill").is_none(), "pill gone");
}

/// The agent overlay opens from the panel line, lists a command for each
/// client with the binary's own path, and closes on Escape.
#[gpui_kit::test]
fn the_agent_overlay_opens_and_closes(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    assert!(cx.debug_bounds("agent-line").is_some());
    assert!(cx.debug_bounds("agent-overlay").is_none());

    update(&view, cx, |app, cx| {
        app.show_agent = true;
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("agent-overlay").is_some());

    press(cx, "escape");
    assert!(!read(&view, cx, |app| app.show_agent));
    assert!(cx.debug_bounds("agent-overlay").is_none());

    // Done closes it, and so does a click outside the card; a click on the
    // card itself does not.
    update(&view, cx, |app, cx| {
        app.show_agent = true;
        cx.notify();
    });
    draw(cx);
    click_on(cx, "overlay-done");
    draw(cx);
    assert!(!read(&view, cx, |app| app.show_agent), "Done closes");

    update(&view, cx, |app, cx| {
        app.show_agent = true;
        cx.notify();
    });
    draw(cx);
    click_on(cx, "agent-overlay");
    draw(cx);
    assert!(read(&view, cx, |app| app.show_agent), "card click stays");
    let card = cx.debug_bounds("agent-overlay").expect("card");
    let outside = card.origin - gpui_kit::point(px(8.), px(8.));
    cx.simulate_click(outside, gpui_kit::Modifiers::none());
    draw(cx);
    assert!(!read(&view, cx, |app| app.show_agent), "outside closes");

    // The Help menu opens the same overlay.
    cx.dispatch_action(crate::app_menu::ConnectAgent);
    draw(cx);
    assert!(read(&view, cx, |app| app.show_agent));
    assert!(cx.debug_bounds("agent-overlay").is_some());
    press(cx, "escape");

    let commands =
        crate::agent::connect_commands(Path::new("/opt/disktree/disktree"));
    assert_eq!(commands.len(), 3);
    assert!(
        commands[0]
            .1
            .starts_with("claude mcp add --scope user pwrdisk")
    );
    assert!(
        commands
            .iter()
            .all(|(_, command)| command.contains("/opt/disktree/disktree"))
    );
    assert!(commands[2].1.contains("\"args\": [\"mcp\"]"));
}

/// A window showing an SSH host answers an agent with where it is and
/// nothing more: an agent's paths are this computer's, and a mark or a
/// scan read against the host would land on the wrong machine.
#[gpui_kit::test]
fn a_window_on_an_ssh_host_refuses_the_agent(cx: &mut TestAppContext) {
    use disktree_link::{MarkRequest, Request, Response, VERSION};

    cx.update(init);
    let temp = fixture();
    let (view, window) = view_over(temp.path(), cx);
    let junk = temp.path().join("junk");
    let answers = update(&view, window, |app, cx| {
        app.remote = Some("build-box".into());
        let hello = app.answer_agent(
            &Request::Hello {
                client: "test".into(),
                version: VERSION,
            },
            cx,
        );
        let mark = app.answer_agent(
            &Request::Mark {
                root: temp.path().to_path_buf(),
                targets: vec![MarkRequest {
                    path: junk.clone(),
                    bytes: 1,
                    is_dir: true,
                }],
            },
            cx,
        );
        let review = app.answer_agent(&Request::Review, cx);
        (hello, mark, review, app.marks.is_empty())
    });
    let (hello, mark, review, unmarked) = answers;
    assert!(
        matches!(&hello, Response::State(state)
            if state.remote.as_deref() == Some("build-box")),
        "{hello:?}"
    );
    for refused in [mark, review] {
        assert!(
            matches!(&refused, Response::Error { message }
                if message.contains("SSH host build-box")),
            "{refused:?}"
        );
    }
    assert!(unmarked, "a refused mark leaves no mark behind");
}

/// A window still on the source chooser scans the root an agent names:
/// the request is explicit, and the chooser would otherwise reopen and
/// leave the agent waiting on a scan that never starts.
#[gpui_kit::test]
fn an_agent_scan_leaves_the_chooser(cx: &mut TestAppContext) {
    use disktree_link::{Request, ScanOptionsRequest};

    cx.update(init);
    let temp = fixture();
    let (view, cx) =
        cx.add_window_view(|_, cx| Disktree::choose(options(), 3, None, cx));
    cx.run_until_parked();
    update(&view, cx, |app, cx| {
        assert!(app.awaiting_root);
        app.answer_agent(
            &Request::Scan {
                root: temp.path().to_path_buf(),
                options: ScanOptionsRequest::default(),
            },
            cx,
        );
        assert!(!app.awaiting_root);
        assert!(!app.volumes_open, "the chooser is closed");
        assert!(app.scan.is_some(), "the agent's root is being scanned");
        assert_eq!(app.root_path, temp.path());
    });
    finish_scan(&view, cx);
}

/// UI removal cannot bypass the same capacity check used by MCP.
#[gpui_kit::test]
fn removal_capacity_limit_blocks_the_entire_ui_selection(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.marks.toggle(
            disktree_core::removal::Target {
                path: temp.path().join("junk"),
                bytes: u64::MAX / 4,
                is_dir: true,
                hidden: false,
            },
            app.path_style(),
        );
        app.screen = Screen::Review;
        app.removal_mode = RemovalMode::Permanent;
        app.begin_removal(cx);
        assert_eq!(app.screen, Screen::Review);
        assert!(app.run.is_none());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|(text, _)| text.contains("limit"))
        );
        assert_eq!(app.marks.len(), 1);
    });
    assert!(temp.path().join("junk/blob.bin").exists());
    assert!(temp.path().join("keep/notes.txt").exists());
    draw(cx);
}

#[gpui_kit::test]
fn accessible_file_pages_reach_every_entry_and_share_safe_marks(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = tempfile::tempdir().expect("tempdir");
    for i in 0..235 {
        std::fs::write(
            temp.path().join(format!("Résumé 日本語-{i:03}.txt")),
            b"fixture",
        )
        .expect("file");
    }
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.file_list = true;
        app.selected = None;
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("file-row-99").is_some());
    assert!(cx.debug_bounds("file-row-100").is_none());
    click_on(cx, "file-next");
    assert_eq!(read(&view, cx, |app| app.selected.clone()), Some(vec![100]));
    press(cx, "pagedown");
    assert!(cx.debug_bounds("file-row-200").is_some());
    assert!(cx.debug_bounds("file-row-199").is_none());
    read(&view, cx, |app| {
        assert_eq!(app.file_entries().len(), 235);
        assert_eq!(app.selected.as_deref(), Some([200].as_slice()));
    });
    press(cx, "space c");
    read(&view, cx, |app| {
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.marks.len(), 1);
        assert!(
            app.marks.items()[0]
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("200")
        );
    });
    press(cx, "escape");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 235);
}

#[gpui_kit::test]
fn accessible_file_navigation_uses_absolute_paths_and_filter(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.file_list = true;
        app.selected = Some(child_crumbs(app, &[], "junk"));
        cx.notify();
    });
    press(cx, "right down space");
    read(&view, cx, |app| {
        assert_eq!(app.current_path(), temp.path().join("junk"));
        assert!(
            app.marks.items()[0]
                .path
                .starts_with(temp.path().join("junk"))
        );
    });
    press(cx, "left / k e e p enter");
    read(&view, cx, |app| {
        assert_eq!(app.file_entries().len(), 1);
    });
}

fn paginated_fixture(count: usize) -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    for index in 0..count {
        let prefix = if index < 150 { "match" } else { "other" };
        std::fs::write(
            temp.path().join(format!("{prefix}-{index:06}.txt")),
            b"fixture",
        )
        .expect("file");
    }
    temp
}

fn focus_file_list(view: &Entity<Disktree>, cx: &mut Window) {
    update(view, cx, |app, cx| {
        app.file_list = true;
        cx.notify();
    });
    draw(cx);
    let focus = view.read_with(cx, |app, _| app.file_focus.clone());
    cx.update(|window, cx| window.focus(&focus, cx));
}

#[gpui_kit::test]
fn accessible_list_review_rescan_preserves_keyboard_focus(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    focus_file_list(&view, cx);
    press(cx, "r");
    assert!(cx.debug_bounds("file-list").is_none());
    let root_focused =
        cx.update(|window, cx| view.read(cx).focus.is_focused(window));
    press(cx, "escape");
    assert!(read(&view, cx, |app| app.scan.is_none()), "Escape cancels");
    assert!(root_focused, "the persistent root owns scan keys");

    press(cx, "r");
    finish_scan(&view, cx);
    draw(cx);
    assert!(cx.debug_bounds("file-list").is_some());
    assert!(
        cx.update(|window, cx| { view.read(cx).file_focus.is_focused(window) })
    );
    assert!(temp.path().join("keep/notes.txt").exists());
}

#[gpui_kit::test]
fn accessible_list_review_sibling_navigation_tracks_page(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = paginated_fixture(235);
    let (view, cx) = view_over(temp.path(), cx);
    focus_file_list(&view, cx);
    update(&view, cx, |app, cx| app.select(Some(vec![99]), cx));
    // Tab traverses controls; n retains the shared sibling-navigation path.
    press(cx, "n");
    assert_eq!(read(&view, cx, |app| app.selected.clone()), Some(vec![100]));
    assert_eq!(read(&view, cx, |app| app.file_page), 1);
    assert!(cx.debug_bounds("file-row-100").is_some());
    assert!(cx.debug_bounds("file-row-99").is_none());
    let marked = read(&view, cx, |app| app.path_at(&[100]).unwrap());
    press(cx, "space shift-n");
    assert_eq!(read(&view, cx, |app| app.selected.clone()), Some(vec![99]));
    assert_eq!(read(&view, cx, |app| app.file_page), 0);
    assert!(cx.debug_bounds("file-row-99").is_some());
    assert!(read(&view, cx, |app| {
        app.marks.contains(&marked, app.path_style())
    }));
    assert!(marked.exists(), "marking never removes the file");
}

#[gpui_kit::test]
fn accessible_list_review_previous_uses_filtered_page(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = paginated_fixture(450);
    let (view, cx) = view_over(temp.path(), cx);
    focus_file_list(&view, cx);
    press(cx, "down pagedown pagedown pagedown pagedown");
    assert_eq!(read(&view, cx, |app| app.file_page), 4);
    press(cx, "/");
    cx.simulate_input("match");
    cx.run_until_parked();
    press(cx, "enter");
    assert_eq!(read(&view, cx, |app| app.file_entries().len()), 150);
    assert!(cx.debug_bounds("file-row-100").is_some());
    click_on(cx, "file-previous");
    assert_eq!(read(&view, cx, |app| app.file_page), 0);
    assert!(cx.debug_bounds("file-row-0").is_some());
    assert!(cx.debug_bounds("file-row-100").is_none());
}

#[gpui_kit::test]
fn accessible_list_review_previous_uses_shrunken_marks_page(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = paginated_fixture(4801);
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        for index in 0..4801 {
            let target = app.target_at(&[index]).expect("file");
            app.marks.toggle(target, app.path_style());
        }
        app.review_page = 4;
        app.open_review(cx);
    });
    draw(cx);
    // Removing marks leaves one row on the effective last page, so the
    // real Previous button remains visible without scrolling 1,200 rows.
    update(&view, cx, |app, cx| {
        let keep = app.marks.items()[..1201].to_vec();
        app.marks.clear();
        for target in keep {
            app.marks.toggle(target, app.path_style());
        }
        cx.notify();
    });
    draw(cx);
    assert_eq!(read(&view, cx, |app| app.marks.len()), 1201);
    click_on(cx, "review-previous");
    assert_eq!(read(&view, cx, |app| app.review_page), 0);
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 4801);
}

#[gpui_kit::test]
fn accessible_list_review_noop_navigation_preserves_page(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = paginated_fixture(235);
    std::fs::create_dir(temp.path().join("nested")).expect("directory");
    std::fs::write(temp.path().join("nested/child.txt"), b"x").expect("file");
    let (view, cx) = view_over(temp.path(), cx);
    focus_file_list(&view, cx);
    let selected = update(&view, cx, |app, cx| {
        let selected = child_crumbs(app, &[], "other-000150.txt");
        assert_eq!(selected, [150]);
        app.file_page = 1;
        app.select(Some(selected.clone()), cx);
        selected
    });
    for key in ["enter", "right", "left", "backspace", "u"] {
        press(cx, key);
        assert_eq!(read(&view, cx, |app| app.file_page), 1, "{key}");
        assert_eq!(
            read(&view, cx, |app| app.selected.clone()),
            Some(selected.clone()),
            "{key}"
        );
        assert!(cx.debug_bounds("file-row-150").is_some(), "{key}");
    }
    update(&view, cx, |app, cx| {
        let nested = child_crumbs(app, &[], "nested");
        app.go_to(nested, cx);
    });
    draw(cx);
    assert_eq!(read(&view, cx, |app| app.file_page), 0);
    assert_eq!(
        read(&view, cx, Disktree::current_path),
        temp.path().join("nested")
    );
}

#[gpui_kit::test]
fn readable_progress_is_throttled_but_errors_and_completion_are_immediate(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.start_scan(cx);
        app.progress.files = 100;
        let first = app.accessibility_status();
        assert!(first.contains("100 files") && first.contains("indeterminate"));
        app.progress.files = 200;
        assert_eq!(app.accessibility_status(), first);
        app.accessible_progress.as_mut().unwrap().0 = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(3))
            .expect("test clock");
        assert!(app.accessibility_status().contains("200 files"));
        app.scan_error = Some("contrived scan error".into());
        assert!(app.accessibility_status().contains("contrived scan error"));
        app.cancel_scan(cx);
        assert!(app.scan.is_none() && app.progress.cancelled);
        app.scan_error = None;
        assert!(app.accessibility_status().contains("Scan cancelled"));
        app.progress.cancelled = false;
        app.progress.finished = true;
        assert!(app.accessibility_status().contains("Scan complete"));
    });
}

#[gpui_kit::test]
fn status_review_cancelled_widening_does_not_hide_the_active_screen(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(&temp.path().join("junk"), cx);
    update(&view, cx, |app, cx| {
        app.widen_to(temp.path().to_path_buf(), cx);
        app.cancel_scan(cx);
        app.open_review(cx);
        for (screen, phase) in [
            (Screen::Review, "Review marked paths"),
            (Screen::Running, "Removal running"),
            (Screen::Done, "Removal finished"),
        ] {
            app.screen = screen;
            let label = app.accessibility_status();
            assert!(label.starts_with(phase), "{label}");
            assert!(label.contains("Previous scan cancelled"), "{label}");
            app.scan_error = Some("contrived widening error".into());
            let label = app.accessibility_status();
            assert!(label.starts_with(phase), "{label}");
            assert!(label.contains("contrived widening error"), "{label}");
            app.scan_error = None;
        }
    });
    draw(cx);
    assert!(temp.path().join("junk/blob.bin").exists());
}

#[gpui_kit::test]
fn status_review_removal_completion_survives_its_background_rescan(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let epoch = update(&view, cx, |app, cx| {
        let keep = child_crumbs(app, &[], "keep");
        let notes = child_crumbs(app, &keep, "notes.txt");
        app.toggle_mark(&notes, cx);
        app.removal_mode = RemovalMode::Permanent;
        // Remove & rescan.
        app.rescan_after_removal = true;
        app.begin_removal(cx);
        assert_eq!(app.screen, Screen::Running, "{:?}", app.notice);
        app.run_epoch
    });
    let mut status = None;
    for _ in 0..400 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        status = update(&view, cx, |app, cx| {
            app.poll_removal_once(epoch, cx);
            (app.screen == Screen::Done).then(|| {
                assert!(app.is_indexing(), "completion starts a refresh");
                app.accessibility_status()
            })
        });
        if status.is_some() {
            break;
        }
    }
    let status = status.expect("removal completes");
    assert!(status.starts_with("Removal finished"), "{status}");
    assert!(status.contains("Background scan"), "{status}");
    assert!(!status.contains("Escape cancels"), "{status}");
    finish_scan(&view, cx);
    draw(cx);
    assert!(!temp.path().join("keep/notes.txt").exists());
    assert!(temp.path().join("junk/blob.bin").exists());
}

/// Run a contrived permanent removal of `names`, each under the root, to
/// its Done screen.
fn remove_now(view: &Entity<Disktree>, cx: &mut Window, names: &[&str]) {
    let epoch = update(view, cx, |app, cx| {
        for name in names {
            let crumbs = app
                .crumbs_for_path(&app.root_path.join(name))
                .expect("in the tree");
            app.toggle_mark(&crumbs, cx);
        }
        app.removal_mode = RemovalMode::Permanent;
        app.begin_removal(cx);
        assert_eq!(app.screen, Screen::Running, "{:?}", app.notice);
        app.run_epoch
    });
    for _ in 0..400 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        let done = update(view, cx, |app, cx| {
            app.poll_removal_once(epoch, cx);
            app.screen == Screen::Done
        });
        if done {
            return;
        }
    }
    panic!("the removal did not finish");
}

/// The run's list is on screen from Removing's first frame, says what
/// each item is, and stays on Done, every item checked off, rather than
/// only the names of what already went.
#[gpui_kit::test]
fn the_removal_list_stays_on_screen_from_start_to_done(
    cx: &mut TestAppContext,
) {
    use crate::run_list::Status;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let epoch = update(&view, cx, |app, cx| {
        for name in [".cache", "junk/deeper"] {
            let crumbs = app
                .crumbs_for_path(&app.root_path.join(name))
                .expect("in the tree");
            app.toggle_mark(&crumbs, cx);
        }
        app.removal_mode = RemovalMode::Permanent;
        app.begin_removal(cx);
        assert_eq!(app.screen, Screen::Running, "{:?}", app.notice);
        app.run_epoch
    });
    draw(cx);
    // Nothing is reported until the poll: both are listed, waiting, and
    // the first is the one in hand.
    assert!(cx.debug_bounds("run-list").is_some());
    assert!(cx.debug_bounds("run-row-0").is_some());
    assert!(cx.debug_bounds("run-row-1").is_some());
    assert!(cx.debug_bounds("run-summary").is_some());
    update(&view, cx, |app, _| {
        let items = &app.run_list.items;
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| item.status == Status::Waiting));
        assert_eq!(app.run_list.current(), Some(0));
        let cache = items
            .iter()
            .find(|item| item.target.path.ends_with(".cache"))
            .expect("the cache is listed");
        assert_eq!(cache.kind(), Some("regenerable"), "and says why");
        assert_eq!(cache.target.bytes, 300_000);
    });

    let mut finished = false;
    for _ in 0..400 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        finished = update(&view, cx, |app, cx| {
            app.poll_removal_once(epoch, cx);
            app.screen == Screen::Done
        });
        if finished {
            break;
        }
    }
    assert!(finished, "the removal finished");
    draw(cx);
    assert!(
        cx.debug_bounds("run-row-0").is_some(),
        "Done keeps the list"
    );
    assert!(cx.debug_bounds("run-row-1").is_some());
    update(&view, cx, |app, _| {
        assert!(
            app.run_list
                .items
                .iter()
                .all(|item| item.status == Status::Removed),
            "every item is checked off"
        );
        let totals = app.run_list.totals();
        assert_eq!((totals.removed, totals.failed), (2, 0));
        assert_eq!(totals.reported_done, 400_000);
    });
    assert!(!temp.path().join(".cache").exists());
    assert!(temp.path().join("keep/notes.txt").exists());
}

/// Review draws its rows as Removing does: a deep path is shortened from
/// the middle onto one line, so its row is as tall as a short one's, and
/// the row says why the path is listed.
#[gpui_kit::test]
fn review_rows_shorten_deep_paths_and_say_why(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let deep: PathBuf = (0..30)
        .map(|level| format!("nested-directory-{level:02}"))
        .collect();
    let file = temp.path().join(&deep).join("leaf.bin");
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, vec![b'x'; 5_000]).expect("write");
    let (view, cx) = view_over(temp.path(), cx);
    mark_paths(&view, cx, &[temp.path().join(".cache"), file.clone()]);
    update(&view, cx, |app, cx| {
        app.screen = Screen::Review;
        cx.notify();
    });
    draw(cx);
    let rows: Vec<_> = (0..2)
        .map(|index| {
            let selector: &'static str =
                Box::leak(format!("review-row-{index}").into_boxed_str());
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is drawn"))
        })
        .collect();
    assert_eq!(
        rows[0].size.height, rows[1].size.height,
        "the deep path does not wrap: {rows:?}"
    );
    // The cache says it is regenerable; the plain file has nothing to say.
    assert!(cx.debug_bounds("review-kind-0").is_some(), "a kind chip");
    assert!(cx.debug_bounds("review-kind-1").is_none());
    // What each mark is was read once: a later frame walks no directory.
    crate::marks::work::reset();
    update(&view, cx, |_, cx| cx.notify());
    draw(cx);
    assert!(crate::marks::work::RENDERS.get() > 0, "it drew again");
    assert_eq!(
        crate::marks::work::RESOLUTION.get(),
        0,
        "and walked nothing"
    );
    let folder = read(&view, cx, |app| {
        crate::run_view::folder_line(app, &file, 40)
    });
    assert!(folder.contains('\u{2026}'), "{folder}");
    assert!(folder.ends_with("nested-directory-29"), "{folder}");
}

/// A long run scrolls with the item in hand, as the live log follows its
/// newest line: scrolling it out of view pauses that and shows the pill,
/// and the pill follows again.
#[gpui_kit::test]
fn the_removal_list_follows_the_item_in_hand_like_the_live_log(
    cx: &mut TestAppContext,
) {
    use crate::run_list::{Item, RunList, Status};
    use crate::run_view::focus_place;
    use std::cmp::Ordering;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let path = |index: usize| temp.path().join(format!("dir-{index:03}"));
    update(&view, cx, |app, cx| {
        app.run_list = RunList::new(
            (0..80)
                .map(|index| Item {
                    target: disktree_core::removal::Target {
                        path: path(index),
                        bytes: 1_000,
                        is_dir: true,
                        hidden: false,
                    },
                    category: disktree_core::classify::Category::Other,
                    // Every other row has a kind chip, so rows differ in
                    // what they hold.
                    reclaim: (index % 2 == 1).then_some(
                        disktree_core::classify::Reclaim::BuildOutput,
                    ),
                    private: None,
                    status: Status::Waiting,
                })
                .collect(),
        );
        app.screen = Screen::Running;
        cx.notify();
    });
    draw(cx);
    // Every row spans the list, whatever it holds, so its size and status
    // lanes line up with the rows around it. A virtualized row is only as
    // wide as its content unless it says otherwise.
    let list = cx.debug_bounds("run-list").expect("the list");
    for index in ["run-row-0", "run-row-1"] {
        let row = cx.debug_bounds(index).expect("a row");
        assert!(
            row.size.width >= list.size.width - px(4.),
            "{index} is {:?} wide in a {:?} list",
            row.size.width,
            list.size.width
        );
    }
    let place =
        |cx: &mut Window, view: &Entity<Disktree>| read(view, cx, focus_place);
    assert_eq!(place(cx, &view), Some(Ordering::Equal), "row 0 shows");
    let settle = |cx: &mut Window, view: &Entity<Disktree>, upto| {
        update(view, cx, |app, cx| {
            app.update_follow();
            for index in 0..upto {
                if app.run_list.settle(&path(index), &Ok(())).is_some() {
                    app.scroll_to_focus();
                }
            }
            cx.notify();
        });
        draw(cx);
        draw(cx);
    };

    // Most of the way down: the list went with it.
    settle(cx, &view, 60);
    assert_eq!(read(&view, cx, |app| app.run_list.current()), Some(60));
    assert_eq!(place(cx, &view), Some(Ordering::Equal), "row 60 shows");
    assert!(cx.debug_bounds("run-row-60").is_some(), "and is drawn");
    // Only the rows in view are built, however long the run.
    assert!(cx.debug_bounds("run-row-0").is_none(), "row 0 is not");
    assert!(cx.debug_bounds("follow-removal").is_none());

    // Scrolled back to the top, away from it: following pauses, the pill
    // shows, and the list stays where it was put.
    let list = cx.debug_bounds("run-list").expect("the list");
    cx.simulate_event(gpui_kit::ScrollWheelEvent {
        position: list.center(),
        delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
            px(0.),
            px(100_000.),
        )),
        ..Default::default()
    });
    draw(cx);
    settle(cx, &view, 65);
    assert!(!read(&view, cx, |app| app.run_list.follow), "paused");
    assert_eq!(place(cx, &view), Some(Ordering::Greater));
    assert!(cx.debug_bounds("follow-removal").is_some(), "the pill");

    // The pill follows again.
    click_on(cx, "follow-removal");
    draw(cx);
    assert!(read(&view, cx, |app| app.run_list.follow));
    assert_eq!(place(cx, &view), Some(Ordering::Equal), "row 65 shows");
    assert!(cx.debug_bounds("follow-removal").is_none());
}

#[gpui_kit::test]
fn a_removal_leaves_what_went_in_the_tree_as_removed_and_counts_it(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    // Stand in the directory that is about to lose a child.
    update(&view, cx, |app, cx| {
        let junk = app
            .crumbs_for_path(&temp.path().join("junk"))
            .expect("junk");
        app.go_to(junk, cx);
    });
    let (scan, before) = read(&view, cx, |app| {
        (
            app.scan_epoch,
            app.tree.as_ref().expect("a tree").root().bytes,
        )
    });

    remove_now(&view, cx, &["junk/deeper"]);
    assert!(read(&view, cx, |app| app.scan.is_none()), "no rescan");
    draw(cx);
    let deeper = temp.path().join("junk/deeper");
    update(&view, cx, |app, _| {
        let root = app.tree.as_ref().expect("a tree").root();
        assert_eq!(root.bytes, before, "the scan's figures stay");
        assert!(app.crumbs_for_path(&deeper).is_some(), "and so does it");
        assert!(app.was_removed(&deeper));
        assert_eq!(app.current_path(), temp.path().join("junk"));
        assert!(
            app.prepare().tiles.iter().any(|tile| tile.removed),
            "it is drawn as removed"
        );
        let cohorts = app.ledger.since(app.scans_done);
        assert_eq!(app.scan_epoch, scan);
        assert_eq!(cohorts.len(), 1);
        assert_eq!(cohorts[0].removed, 1);
        assert_eq!(cohorts[0].reported, 100_000);
        assert!(cohorts[0].observed().is_some(), "free space is measured");
    });
    assert!(cx.debug_bounds("removal-ledger").is_some(), "Done lists it");

    // The inspector offers no mark for it, and says why.
    update(&view, cx, |app, cx| {
        app.screen = Screen::Explore;
        app.selected = app.crumbs_for_path(&deeper);
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("inspector-mark").is_none());
    update(&view, cx, |app, cx| {
        app.selected = app.crumbs_for_path(&temp.path().join("junk/blob.bin"));
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("inspector-mark").is_some(), "its neighbour");

    update(&view, cx, |app, cx| {
        // What went cannot be marked again.
        let crumbs = app.crumbs_for_path(&deeper).expect("still drawn");
        app.toggle_mark(&crumbs, cx);
        assert!(app.marks.is_empty());
        let notice = app.notice.as_ref().map(|(text, _)| text.as_str());
        assert!(notice.is_some_and(|text| text.contains("was removed")));
        // Its directory can, for what is left in it.
        let junk = app.crumbs_for_path(&temp.path().join("junk")).unwrap();
        let listed = app.node_at(&junk).unwrap().bytes;
        app.toggle_mark(&junk, cx);
        assert_eq!(app.marks.items()[0].bytes, listed - 100_000);
        app.toggle_mark(&junk, cx);
        assert!(app.marks.is_empty());
    });

    // A second batch, still without a scan: two rows and a total.
    remove_now(&view, cx, &["keep"]);
    draw(cx);
    let totals = read(&view, cx, |app| {
        assert!(app.was_removed(&temp.path().join("keep/notes.txt")));
        crate::ledger::Ledger::totals(app.ledger.since(app.scans_done))
    })
    .expect("totals");
    assert_eq!(totals.count, 2);
    assert_eq!(totals.reported, 101_000);
    assert!(
        temp.path().join("junk/blob.bin").exists(),
        "neighbours stay"
    );

    update(&view, cx, |app, cx| {
        app.screen = Screen::Explore;
        // A scan started and stopped keeps the tree, and so the account.
        app.scan_epoch += 1;
        cx.notify();
    });
    draw(cx);
    assert!(
        cx.debug_bounds("freed-since-scan").is_some(),
        "the Disk section keeps the account"
    );

    // A rescan starts a new account and forgets what was removed.
    update(&view, cx, Disktree::start_scan);
    finish_scan(&view, cx);
    draw(cx);
    read(&view, cx, |app| {
        assert!(app.ledger.since(app.scans_done).is_empty());
        assert!(app.removed.is_empty());
        assert!(app.crumbs_for_path(&deeper).is_none(), "the disk now");
    });
    assert!(cx.debug_bounds("freed-since-scan").is_none());
}

#[gpui_kit::test]
fn removing_a_directory_after_part_of_it_counts_only_the_rest(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    remove_now(&view, cx, &["junk/deeper"]);
    update(&view, cx, |app, cx| {
        app.screen = Screen::Explore;
        cx.notify();
    });
    remove_now(&view, cx, &["junk"]);
    read(&view, cx, |app| {
        let cohorts = app.ledger.since(app.scans_done);
        assert_eq!(cohorts.len(), 2);
        let rest = cohorts[1].reported;
        assert!(
            cohorts[1].private.is_none_or(|private| private <= rest),
            "{:?} private of {rest} reported",
            cohorts[1].private
        );
        let totals = crate::ledger::Ledger::totals(cohorts).unwrap();
        assert!(totals.private.is_none_or(|p| p <= totals.reported));
    });
}

#[gpui_kit::test]
fn an_agent_listing_says_what_was_removed_since_the_scan(
    cx: &mut TestAppContext,
) {
    use disktree_link::{FindRequest, Request, Response};
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let root = temp.path().to_path_buf();
    let ask = |request: Request, cx: &mut Window| {
        update(&view, cx, |app, cx| app.answer_agent(&request, cx))
    };
    // Before any removal, nothing says removed.
    let Response::Listing(before) = ask(
        Request::Ls {
            path: root.clone(),
            limit: 10,
        },
        cx,
    ) else {
        panic!("a listing");
    };
    assert!(
        before
            .children
            .iter()
            .all(|child| !child.removed && child.removed_bytes == 0)
    );

    remove_now(&view, cx, &["junk/deeper"]);
    let Response::Listing(listing) = ask(
        Request::Ls {
            path: root.clone(),
            limit: 10,
        },
        cx,
    ) else {
        panic!("a listing");
    };
    let junk = listing
        .children
        .iter()
        .find(|child| child.name == "junk")
        .expect("junk is still listed");
    assert!(!junk.removed);
    assert_eq!(junk.removed_bytes, 100_000, "part of it went");
    assert_eq!(listing.entry.removed_bytes, 100_000, "and so of the root");
    let keep = listing.children.iter().find(|c| c.name == "keep").unwrap();
    assert!(
        !keep.removed && keep.removed_bytes == 0,
        "neighbours say nothing"
    );

    let Response::Listing(inside) = ask(
        Request::Ls {
            path: root.join("junk"),
            limit: 10,
        },
        cx,
    ) else {
        panic!("a listing");
    };
    let deeper = inside.children.iter().find(|c| c.name == "deeper").unwrap();
    assert!(deeper.removed, "it went whole");
    assert_eq!(deeper.removed_bytes, 0);

    let Response::Found(found) = ask(
        Request::Find(FindRequest {
            kind: "any".into(),
            name: Some("deeper".into()),
            limit: 5,
            ..FindRequest::default()
        }),
        cx,
    ) else {
        panic!("matches");
    };
    let hit = found
        .matches
        .iter()
        .find(|hit| hit.path == root.join("junk/deeper"))
        .expect("still found");
    assert!(hit.entry.removed);

    // An estimate over what went, or around it, would price space already
    // gone: it asks for a rescan instead.
    for path in [root.join("junk/deeper"), root.join("junk")] {
        let answer = ask(
            Request::Estimate {
                paths: vec![path],
                peer_scope: None,
                peer_limit: 0,
            },
            cx,
        );
        assert!(
            matches!(&answer, Response::Error { message } if message.contains("rescan")),
            "{answer:?}"
        );
    }

    // A rescan sees the disk as it is, and nothing is flagged.
    update(&view, cx, Disktree::start_scan);
    finish_scan(&view, cx);
    let Response::Listing(after) = ask(
        Request::Ls {
            path: root.join("junk"),
            limit: 10,
        },
        cx,
    ) else {
        panic!("a listing");
    };
    assert!(after.children.iter().all(|c| c.name != "deeper"));
    assert_eq!(after.entry.removed_bytes, 0);
}

#[gpui_kit::test]
fn what_was_removed_stays_removed_across_a_metric_change(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    remove_now(&view, cx, &["junk/deeper"]);
    let junk = temp.path().join("junk");
    update(&view, cx, |app, cx| {
        app.screen = Screen::Explore;
        let crumbs = app.crumbs_for_path(&junk).unwrap();
        app.toggle_mark(&crumbs, cx);
        // Size | Files reorders the tree and re-reads every mark's size.
        app.toggle_metric(cx);
        app.toggle_metric(cx);
    });
    read(&view, cx, |app| {
        assert!(app.was_removed(&junk.join("deeper")));
        let crumbs = app.crumbs_for_path(&junk).unwrap();
        let listed = app.node_at(&crumbs).unwrap().bytes;
        assert_eq!(
            app.marks.items()[0].bytes,
            listed - 100_000,
            "a mark around it still leaves it out"
        );
    });
}

#[gpui_kit::test]
fn status_review_widening_announces_each_in_flight_root_immediately(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let inner = temp.path().join("junk/deeper");
    let (view, cx) = view_over(&inner, cx);
    update(&view, cx, |app, cx| {
        for target in [temp.path().join("junk"), temp.path().to_path_buf()] {
            app.widen_to(target.clone(), cx);
            assert_eq!(app.root_path, inner, "the old tree stays usable");
            assert_eq!(app.scan_root, target);
            let status = app.accessibility_status();
            assert!(
                status.starts_with(&format!("Scanning {}.", target.display())),
                "{status}"
            );
            app.progress.files = 500;
            assert_eq!(app.accessibility_status(), status, "counts sampled");
        }
        app.cancel_scan(cx);
        assert!(app.accessibility_status().starts_with("Scan cancelled"));
    });
    draw(cx);
    assert!(inner.join("more.bin").exists());
}

#[gpui_kit::test]
fn source_tab_traversal_stays_in_the_modal_and_escape_restores_origin(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    press(cx, "tab");
    let origin = cx
        .update(|window, cx| window.focused(cx))
        .expect("control focus");
    let root_focus = read(&view, cx, |app| app.focus.clone());
    assert!(!cx.update(|window, _| root_focus.is_focused(window)));
    for ssh in [false, true] {
        update(&view, cx, |app, cx| {
            app.open_volumes(cx);
            app.volumes = vec![Volume {
                point: temp.path().into(),
                device: None,
                space: None,
            }];
            app.volumes_loading = false;
            app.picker_ssh = ssh;
        });
        draw(cx);
        assert_eq!(cx.debug_bounds("source-host").is_some(), ssh);
        let (host, path) = view.read_with(cx, |app, cx| {
            use gpui_kit::Focusable as _;
            (
                app.picker_host.as_ref().unwrap().read(cx).focus_handle(cx),
                app.picker_path.as_ref().unwrap().read(cx).focus_handle(cx),
            )
        });
        let roots = read(&view, cx, |app| app.source_roots_focus.clone());
        let mut saw_roots = false;
        let mut saw_host = false;
        let mut saw_path = false;
        for key in ["tab", "shift-tab"] {
            for _ in 0..24 {
                press(cx, key);
                saw_roots |= cx.update(|window, _| roots.is_focused(window));
                saw_host |= cx.update(|window, _| host.is_focused(window));
                saw_path |= cx.update(|window, _| path.is_focused(window));
                let modal = read(&view, cx, |app| app.confirm_focus.clone());
                assert!(
                    cx.update(|window, cx| modal.contains_focused(window, cx))
                );
                assert!(read(&view, cx, |app| app.marks.is_empty()
                    && app.scan.is_none()));
            }
        }
        assert!(saw_roots, "the root list remains a tab stop");
        assert_eq!(saw_host, ssh, "only SSH mode shows the host field");
        assert_eq!(saw_path, !ssh, "Directory appears after a source is ready");
        press(cx, "escape");
        assert!(cx.update(|window, _| origin.is_focused(window)));
    }
}

#[gpui_kit::test]
fn help_focus_returns_to_the_originating_control(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    press(cx, "tab");
    let origin = cx
        .update(|window, cx| window.focused(cx))
        .expect("control focus");
    for dismissal in ["escape", "done", "backdrop"] {
        update(&view, cx, Disktree::open_help);
        draw(cx);
        press(cx, "tab shift-tab");
        match dismissal {
            "escape" => press(cx, "escape"),
            "done" => click_on(cx, "overlay-done"),
            _ => {
                let card = cx.debug_bounds("help-overlay").expect("card");
                let outside = card.origin - Point::new(px(8.), px(8.));
                cx.simulate_click(outside, gpui_kit::Modifiers::none());
                draw(cx);
            }
        }
        assert!(cx.update(|window, _| origin.is_focused(window)));
        assert!(read(&view, cx, |app| !app.show_help && app.marks.is_empty()));
    }
}

#[gpui_kit::test]
fn custom_menu_arrows_and_escape_leave_files_untouched(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, Disktree::toggle_app_menu);
    draw(cx);
    press(cx, "down down up");
    assert_eq!(read(&view, cx, |app| app.app_menu_index), 1);
    press(cx, "escape");
    assert!(read(&view, cx, |app| !app.app_menu_open && app.marks.is_empty()));
    assert!(temp.path().join("junk/blob.bin").exists());
}

#[gpui_kit::test]
fn custom_menu_enter_activates_the_named_safe_action(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let index = crate::app_menu::menus(crate::app_menu::Platform::Other)
        .into_iter()
        .flat_map(|menu| menu.items)
        .filter_map(|item| {
            if let gpui_kit::MenuItem::Action { name, .. } = item {
                Some(name)
            } else {
                None
            }
        })
        .position(|name| name == "Keyboard Shortcuts")
        .expect("safe menu action");
    update(&view, cx, |app, cx| {
        app.toggle_app_menu(cx);
        app.app_menu_index = index;
    });
    draw(cx);
    press(cx, "enter");
    assert!(read(&view, cx, |app| app.show_help
        && !app.app_menu_open
        && app.marks.is_empty()));
    press(cx, "escape");
    assert!(temp.path().join("junk/blob.bin").exists());
}

#[gpui_kit::test]
fn a_large_directory_keeps_zero_weight_files_reachable_in_bounded_pages(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = tempfile::tempdir().expect("tempdir");
    for index in 0..50_000 {
        std::fs::write(temp.path().join(format!("entry-{index:05}.txt")), [])
            .expect("empty fixture file");
    }
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.file_list = true;
        app.selected = None;
        app.file_step(1, cx);
        app.file_step(isize::MAX, cx);
    });
    draw(cx);
    assert!(cx.debug_bounds("file-row-49999").is_some());
    assert!(cx.debug_bounds("file-row-49899").is_none());
    read(&view, cx, |app| {
        assert_eq!(app.current().unwrap().children().len(), 50_000);
        assert_eq!(app.selected.as_deref(), Some([49_999].as_slice()));
        assert!(app.marks.is_empty());
    });
}

#[gpui_kit::test]
fn tab_reaches_file_navigation_without_changing_selection(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let navigation = read(&view, cx, |app| app.file_focus.clone());
    let selected = read(&view, cx, |app| app.selected.clone());
    let mut reached = false;
    for _ in 0..20 {
        press(cx, "tab");
        if cx.update(|window, _| navigation.is_focused(window)) {
            reached = true;
            break;
        }
    }
    assert!(reached, "file navigation participates in the tab order");
    assert_eq!(read(&view, cx, |app| app.selected.clone()), selected);
}

/// Keyboard activation of Cancel must never authorize the marked removal.
#[gpui_kit::test]
fn focus_review_delete_cancel_enter_preserves_the_fixture(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.removal_mode = RemovalMode::Permanent;
        let junk = child_crumbs(app, &[], "junk");
        app.toggle_mark(&junk, cx);
        app.open_review(cx);
    });
    draw(cx);
    let cancel_tabs = if crate::chrome::Platform::current()
        == crate::chrome::Platform::Windows
    {
        2
    } else {
        1
    };
    for activation in ["enter", "space"] {
        press(cx, "enter");
        for _ in 0..cancel_tabs {
            press(cx, "tab");
        }
        let keystroke = gpui_kit::Keystroke::parse(activation).unwrap();
        cx.simulate_event(gpui_kit::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(gpui_kit::KeyUpEvent { keystroke });
        draw(cx);
        read(&view, cx, |app| {
            assert!(!app.confirm_open, "Cancel {activation} closes the alert");
            assert!(app.run.is_none(), "Cancel never starts removal");
            assert_eq!(app.screen, Screen::Review);
            assert_eq!(app.marks.len(), 1);
        });
        assert!(temp.path().join("junk/blob.bin").exists());
        assert!(temp.path().join("keep/notes.txt").exists());
    }
    press(cx, "enter");
    for _ in 0..(3 - cancel_tabs) {
        press(cx, "tab");
    }
    press(cx, "enter");
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Running);
    let epoch = read(&view, cx, |app| app.run_epoch);
    let mut finished = false;
    for _ in 0..400 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        finished = update(&view, cx, |app, cx| {
            app.poll_removal_once(epoch, cx);
            app.screen == Screen::Done
        });
        if finished {
            break;
        }
    }
    assert!(finished, "focused Delete completes the contrived removal");
    assert!(!temp.path().join("junk").exists());
    assert!(temp.path().join("keep/notes.txt").exists());
    read(&view, cx, |app| {
        assert!(app.scan.is_none(), "Delete alone does not rescan");
        let root = app.tree.as_ref().expect("a tree").root();
        assert!(root.child_named("junk").is_some(), "the tree keeps it");
        assert!(app.was_removed(&temp.path().join("junk")), "as removed");
    });
}

#[gpui_kit::test]
fn focus_review_source_cancel_enter_does_not_scan(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let epoch = read(&view, cx, |app| app.scan_epoch);
    // Supply only a contrived root, without starting host discovery.
    update(&view, cx, |app, cx| {
        app.volumes_open = true;
        app.volumes_loading = false;
        app.volumes = vec![Volume {
            point: temp.path().into(),
            device: None,
            space: None,
        }];
        app.focus_request = Some(crate::state::FocusTarget::Dialog);
        cx.notify();
    });
    draw(cx);
    let cancel_tabs = if crate::chrome::Platform::current()
        == crate::chrome::Platform::Windows
    {
        1
    } else {
        2
    };
    for _ in 0..cancel_tabs {
        press(cx, "shift-tab");
    }
    press(cx, "enter");
    read(&view, cx, |app| {
        assert!(!app.volumes_open, "Cancel closes the picker");
        assert!(app.scan.is_none());
        assert_eq!(app.scan_epoch, epoch, "Cancel does not accept a root");
        assert_eq!(app.root_path, temp.path());
    });
}

#[gpui_kit::test]
fn focus_review_escape_cancels_removal_before_background_scan(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(&temp.path().join("junk"), cx);
    update(&view, cx, |app, cx| {
        app.widen_to(temp.path().into(), cx);
        app.screen = Screen::Running;
        // No paths are removed: this checks delivery of the cancellation
        // request while a real, contrived widening scan is still active.
        app.run = Some(disktree_core::removal::spawn(
            disktree_core::removal::Plan::default(),
            RemovalMode::Permanent,
        ));
        cx.notify();
    });
    draw(cx);
    press(cx, "escape");
    read(&view, cx, |app| {
        assert!(app.scan.is_some(), "foreground Escape preserves the scan");
        assert!(
            app.notice.as_ref().is_some_and(
                |(text, _)| text == "stopping after the current item"
            )
        );
    });
    update(&view, cx, |app, cx| {
        app.run = None;
        app.cancel_scan(cx);
    });
    assert!(temp.path().join("junk/blob.bin").exists());
}

#[gpui_kit::test]
fn focus_review_escape_leaves_review_and_done_with_refresh_intact(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(&temp.path().join("junk"), cx);
    for screen in [Screen::Review, Screen::Done] {
        update(&view, cx, |app, cx| {
            app.widen_to(temp.path().into(), cx);
            app.screen = screen;
            cx.notify();
        });
        draw(cx);
        press(cx, "escape");
        read(&view, cx, |app| {
            assert_eq!(app.screen, Screen::Explore);
            assert!(
                app.scan.is_some(),
                "foreground Escape preserves the refresh"
            );
        });
        update(&view, cx, Disktree::cancel_scan);
    }
    assert!(temp.path().join("junk/blob.bin").exists());
}

fn cycle_back_to_focus(
    cx: &mut Window,
    target: &gpui_kit::FocusHandle,
    key: &str,
) {
    press(cx, key);
    for _ in 0..60 {
        if cx.update(|window, _| target.is_focused(window)) {
            return;
        }
        press(cx, key);
    }
    panic!("{key} traversal did not reach the navigation control");
}

#[gpui_kit::test]
fn focus_review_settings_panes_are_forward_and_reverse_tab_stops(
    cx: &mut TestAppContext,
) {
    use crate::settings::{Pane, SettingsWindow};
    cx.update(init);
    let (_view, cx) =
        cx.add_window_view(|_, cx| SettingsWindow::new(Pane::About, cx));
    draw(cx);
    for pane in Pane::ALL {
        click_on(cx, pane.nav_id());
        let target = cx
            .update(|window, cx| window.focused(cx))
            .expect("pane focus");
        cycle_back_to_focus(cx, &target, "tab");
        cycle_back_to_focus(cx, &target, "shift-tab");
    }
}

#[gpui_kit::test]
fn focus_review_application_menu_is_a_forward_and_reverse_tab_stop(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.chrome_pin = window_frames()
            .into_iter()
            .find(|(name, _)| *name == "GNOME, controls right")
            .map(|(_, frame)| frame);
        cx.notify();
    });
    draw(cx);
    click_on(cx, "app-menu-button");
    press(cx, "escape");
    let target = cx
        .update(|window, cx| window.focused(cx))
        .expect("menu trigger focus");
    cycle_back_to_focus(cx, &target, "tab");
    cycle_back_to_focus(cx, &target, "shift-tab");
    press(cx, "enter");
    assert!(read(&view, cx, |app| app.app_menu_open));
    press(cx, "escape");
    assert!(temp.path().join("junk/blob.bin").exists());
}

#[gpui_kit::test]
fn routing_review_search_escape_preserves_the_widening_scan(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(&temp.path().join("junk"), cx);
    for file_list in [false, true] {
        update(&view, cx, |app, cx| {
            app.file_list = file_list;
            app.widen_to(temp.path().into(), cx);
        });
        draw(cx);
        let epoch = read(&view, cx, |app| app.scan_epoch);
        press(cx, "/ b l o b");
        assert!(read(&view, cx, |app| app.find_open && app.find == "blob"));
        press(cx, "escape");
        read(&view, cx, |app| {
            assert!(!app.find_open, "Escape dismisses the foreground search");
            assert!(app.find.is_empty());
            assert!(app.scan.is_some(), "dismissing search preserves the scan");
            assert_eq!(app.scan_epoch, epoch);
            assert!(!app.progress.cancelled);
            assert!(app.tree().is_some());
        });
        press(cx, "escape");
        read(&view, cx, |app| {
            assert!(app.scan.is_none(), "the next Escape cancels the scan");
            assert!(app.progress.cancelled);
        });
    }
    assert!(temp.path().join("junk/blob.bin").exists());
}

#[gpui_kit::test]
fn routing_review_menu_escape_preserves_the_widening_scan(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(&temp.path().join("junk"), cx);
    for menu in ["crumb", "power", "application"] {
        update(&view, cx, |app, cx| {
            app.widen_to(temp.path().into(), cx);
            match menu {
                "crumb" => {
                    let deeper = child_crumbs(app, &[], "deeper");
                    app.open_crumb_menu(&deeper, cx);
                }
                "power" => {
                    app.cpu_threads = 8;
                    app.toggle_power_menu(cx);
                }
                _ => app.toggle_app_menu(cx),
            }
        });
        draw(cx);
        let epoch = read(&view, cx, |app| app.scan_epoch);
        press(cx, "escape");
        read(&view, cx, |app| {
            assert!(app.crumb_menu.is_none(), "{menu}");
            assert!(app.power_menu.is_none(), "{menu}");
            assert!(!app.app_menu_open, "{menu}");
            assert!(app.scan.is_some(), "{menu} Escape preserves the scan");
            assert_eq!(app.scan_epoch, epoch);
            assert!(!app.progress.cancelled);
        });
        press(cx, "escape");
        assert!(read(&view, cx, |app| app.scan.is_none()));
    }
}

#[gpui_kit::test]
fn routing_review_help_dismisses_menus_and_contains_navigation(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let junk = read(&view, cx, |app| child_crumbs(app, &[], "junk"));
    update(&view, cx, |app, cx| {
        app.go_to(junk.clone(), cx);
        app.power_settings_path = None;
        app.cpu_threads = 8;
        app.chrome_pin = window_frames()
            .into_iter()
            .find(|(name, _)| *name == "GNOME, controls right")
            .map(|(_, frame)| frame);
        cx.notify();
    });
    draw(cx);
    for menu in ["crumb", "power", "application"] {
        for native_action in [false, true] {
            update(&view, cx, |app, cx| match menu {
                "crumb" => app.open_crumb_menu(&junk, cx),
                "power" => app.toggle_power_menu(cx),
                _ => app.toggle_app_menu(cx),
            });
            draw(cx);
            if native_action {
                cx.update(|window, cx| {
                    window.dispatch_action(
                        Box::new(crate::app_menu::ShowKeyboardShortcuts),
                        cx,
                    );
                });
                draw(cx);
            } else {
                press(cx, "?");
            }
            read(&view, cx, |app| {
                assert!(app.show_help, "{menu}, action={native_action}");
                assert!(app.crumb_menu.is_none(), "Help closes {menu}");
                assert!(app.power_menu.is_none(), "Help closes {menu}");
                assert!(!app.app_menu_open, "Help closes {menu}");
            });
            press(cx, "down enter");
            read(&view, cx, |app| {
                assert!(app.show_help);
                assert_eq!(app.crumbs, junk, "Help contains navigation");
                assert!(app.marks.is_empty());
                assert_eq!(app.screen, Screen::Explore);
            });
            press(cx, "escape");
            assert!(read(&view, cx, |app| !app.show_help));
        }
    }
    assert!(temp.path().join("junk/blob.bin").exists());
}

/// A made-up updater state: `finished` checks done, the last ending in
/// `outcome`, and the download held for the default channel, if any.
fn update_state(
    finished: u64,
    outcome: crate::updater::Outcome,
    held: Option<&str>,
) -> crate::updater::State {
    use crate::update_channel::Slot;
    crate::updater::State {
        finished,
        last: Some((
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(finished),
            outcome,
        )),
        held: held.map(|version| crate::updater::Held {
            version: version.to_owned(),
            file_name: format!("PwrDisk-{version}-full.nupkg"),
            slot: Slot::default().key(),
            switch_back: false,
        }),
        ..crate::updater::State::default()
    }
}

/// Asks as Check for Updates… does, against a made-up check that has not
/// finished yet, without starting a real one.
fn ask_for_updates(view: &Entity<Disktree>, cx: &mut Window) {
    use crate::update_channel::Slot;
    update(view, cx, |app, cx| {
        app.update_watch.ask(0);
        let state = crate::updater::State {
            checking: true,
            ..crate::updater::State::default()
        };
        app.follow_updates(state, "0.14.0", Slot::default(), cx);
    });
}

/// Check for Updates… shows a live card, then answers in its place: here,
/// that this copy is the latest.
#[gpui_kit::test]
fn a_check_asked_for_answers_in_a_toast(cx: &mut TestAppContext) {
    use crate::update_channel::Slot;
    use crate::updater::Outcome;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    ask_for_updates(&view, cx);
    draw(cx);
    assert!(cx.debug_bounds("toast-update").is_some(), "checking shows");
    assert!(cx.debug_bounds("toast-track").is_some(), "with its sweep");
    assert!(
        cx.debug_bounds("toast-update-timer").is_none(),
        "work under way has no countdown"
    );

    let state = update_state(1, Outcome::UpToDate("0.14.0".into()), None);
    update(&view, cx, |app, cx| {
        app.follow_updates(state, "0.14.0", Slot::default(), cx);
    });
    draw(cx);
    let title = read(&view, cx, |app| {
        app.toasts
            .get(crate::toasts::UPDATE)
            .map(|t| t.title.clone())
    });
    assert_eq!(
        title.as_deref(),
        Some("PwrDisk is up to date"),
        "replaced in place"
    );
    assert!(cx.debug_bounds("toast-release-notes").is_some());
    assert!(
        cx.debug_bounds("toast-update-timer").is_some(),
        "counts down"
    );
    assert!(cx.debug_bounds("toast-track").is_none());
    assert!(cx.debug_bounds("toast-restart").is_none());
}

/// A download's percent redraws the live card, and only Check for
/// Updates… shows one: a background download raises nothing until it is
/// ready.
#[gpui_kit::test]
fn a_menu_check_shows_its_download_percent(cx: &mut TestAppContext) {
    use crate::updater::Download;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let download = Download {
        version: "0.15.0".into(),
        switch_back: false,
        percent: Some(42),
    };
    update(&view, cx, |app, _| {
        app.update_watch.follow_progress(Some(download.clone()));
    });
    draw(cx);
    assert!(
        cx.debug_bounds("toast-stack").is_none(),
        "nobody asked: a background download is silent"
    );

    ask_for_updates(&view, cx);
    update(&view, cx, |app, _| {
        app.update_watch.follow_progress(Some(download));
    });
    draw(cx);
    let card = read(&view, cx, crate::toasts::update_card).expect("live");
    assert_eq!(card.title, "Downloading update");
    assert_eq!(card.body, "PwrDisk 0.15.0 \u{00b7} 42%");
    assert!(cx.debug_bounds("toast-track").is_some());
}

/// A download's percent is drawn by the toast alone: it never rebuilds
/// Settings, which collects the diagnostics again. A real change does.
#[gpui_kit::test]
fn a_progress_tick_does_not_rebuild_settings(cx: &mut TestAppContext) {
    use crate::settings::REFRESHES;
    use crate::updater::Change;
    cx.update(init);
    let temp = fixture();
    let (_view, cx) = view_over(temp.path(), cx);
    let before = REFRESHES.get();
    for _ in 0..100 {
        cx.update(|_, cx| crate::toasts::updater_changed(Change::Progress, cx));
    }
    assert_eq!(REFRESHES.get(), before, "a hundred ticks, no rebuild");
    cx.update(|_, cx| crate::toasts::updater_changed(Change::State, cx));
    assert_eq!(REFRESHES.get(), before + 1, "a real change refreshes");
}

/// A ready release stays: a background check offers it, Later puts that
/// version away (the strip's chip still offers it), and a newer one comes
/// back.
#[gpui_kit::test]
fn a_ready_download_stays_until_later(cx: &mut TestAppContext) {
    use crate::update_channel::Slot;
    use crate::updater::Outcome;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let slot = Slot::default();

    let quiet = update_state(1, Outcome::UpToDate("0.14.0".into()), None);
    update(&view, cx, |app, cx| {
        app.follow_updates(quiet, "0.14.0", slot, cx);
    });
    draw(cx);
    assert!(cx.debug_bounds("toast-stack").is_none(), "nothing new");
    assert!(cx.debug_bounds(crate::views::UPDATE_CHIP_ID).is_none());

    let ready =
        update_state(2, Outcome::Downloaded("0.15.0".into()), Some("0.15.0"));
    update(&view, cx, |app, cx| {
        app.follow_updates(ready, "0.14.0", slot, cx);
    });
    draw(cx);
    assert!(cx.debug_bounds("toast-restart").is_some());
    assert!(cx.debug_bounds(crate::views::UPDATE_CHIP_ID).is_some());
    let later = cx.debug_bounds("toast-later").expect("ready toast");
    cx.simulate_click(later.center(), gpui_kit::Modifiers::none());
    draw(cx);
    assert!(cx.debug_bounds("toast-update").is_none(), "put away");
    assert!(
        cx.debug_bounds(crate::views::UPDATE_CHIP_ID).is_some(),
        "the chip still offers it"
    );

    // The next background check finds the same download: still down.
    let again =
        update_state(3, Outcome::Pending("0.15.0".into()), Some("0.15.0"));
    update(&view, cx, |app, cx| {
        app.follow_updates(again, "0.14.0", slot, cx);
    });
    draw(cx);
    assert!(
        cx.debug_bounds("toast-update").is_none(),
        "once per version"
    );

    let newer =
        update_state(4, Outcome::Downloaded("0.15.1".into()), Some("0.15.1"));
    update(&view, cx, |app, cx| {
        app.follow_updates(newer, "0.14.0", slot, cx);
    });
    draw(cx);
    let body = read(&view, cx, crate::toasts::update_card).map(|t| t.body);
    assert_eq!(
        body.as_deref(),
        Some(
            "PwrDisk 0.15.1 is downloaded. It installs when you restart or quit."
        )
    );
}

/// An answer takes the ready card's place while it stands, and the ready
/// card comes back once it leaves: a failed check never hides a release.
#[gpui_kit::test]
fn the_ready_card_comes_back_after_an_answer(cx: &mut TestAppContext) {
    use crate::update_channel::Slot;
    use crate::updater::Outcome;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let slot = Slot::default();
    let ready =
        update_state(1, Outcome::Downloaded("0.15.0".into()), Some("0.15.0"));
    update(&view, cx, |app, cx| {
        app.follow_updates(ready, "0.14.0", slot, cx);
    });
    ask_for_updates(&view, cx);
    let mut failed =
        update_state(2, Outcome::Failed("timed out".into()), Some("0.15.0"));
    failed.checking = false;
    update(&view, cx, |app, cx| {
        app.update_watch.ask(1);
        app.follow_updates(failed, "0.14.0", slot, cx);
    });
    draw(cx);
    assert!(cx.debug_bounds("toast-check-again").is_some(), "the answer");
    assert!(cx.debug_bounds("toast-restart").is_none(), "in its place");
    cx.executor().advance_clock(crate::toasts::ANSWER);
    cx.run_until_parked();
    draw(cx);
    assert!(cx.debug_bounds("toast-check-again").is_none(), "gone");
    assert!(cx.debug_bounds("toast-restart").is_some(), "offered again");
}

/// Restart pressed says so and cannot be pressed again; a failed restart
/// is offered again, with why.
#[gpui_kit::test]
fn restarting_waits_and_a_failure_is_offered_again(cx: &mut TestAppContext) {
    use crate::update_channel::Slot;
    use crate::updater::Outcome;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let slot = Slot::default();
    let mut state =
        update_state(1, Outcome::Downloaded("0.15.0".into()), Some("0.15.0"));
    state.restarting = true;
    update(&view, cx, |app, cx| {
        app.follow_updates(state.clone(), "0.14.0", slot, cx);
    });
    draw(cx);
    let card = read(&view, cx, crate::toasts::update_card).expect("ready");
    assert!(card.actions.contains(&crate::toasts::Action::Restart {
        switch_back: false,
        restarting: true,
    }));

    state.restarting = false;
    state.restart_failed = Some("the updater did not start".into());
    update(&view, cx, |app, cx| {
        app.follow_updates(state, "0.14.0", slot, cx);
    });
    draw(cx);
    assert!(cx.debug_bounds("toast-update-problem").is_some());
    let notice = read(&view, cx, |app| app.notice.clone());
    assert!(notice.is_some_and(|(text, _)| text.contains("did not start")));
}

/// A failed check asked for by hand says why, with the log one click away.
#[gpui_kit::test]
fn a_failed_check_says_why(cx: &mut TestAppContext) {
    use crate::update_channel::Slot;
    use crate::updater::Outcome;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    ask_for_updates(&view, cx);
    let failed = update_state(1, Outcome::Failed("rate limited".into()), None);
    update(&view, cx, |app, cx| {
        app.follow_updates(failed, "0.14.0", Slot::default(), cx);
    });
    draw(cx);
    assert!(cx.debug_bounds("toast-open-logs").is_some());
    assert!(cx.debug_bounds("toast-check-again").is_some());
    assert!(
        !read(&view, cx, |app| app.update_watch.is_asking()),
        "answered"
    );
}

/// An answer leaves on its own after 8 s, and under Reduce Motion too: the
/// strip only stops draining.
#[gpui_kit::test]
fn an_answer_leaves_on_its_own(cx: &mut TestAppContext) {
    use crate::update_channel::Slot;
    use crate::updater::Outcome;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    ask_for_updates(&view, cx);
    let up = update_state(1, Outcome::UpToDate("0.14.0".into()), None);
    update(&view, cx, |app, cx| {
        app.follow_updates(up, "0.14.0", Slot::default(), cx);
    });
    draw(cx);
    let full = cx.debug_bounds("toast-update").expect("answer").size.width;
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(5));
    cx.run_until_parked();
    draw(cx);
    assert!(
        !read(&view, cx, |app| app.toasts.is_empty()),
        "still reading"
    );
    let strip = cx.debug_bounds("toast-update-timer").expect("strip");
    // The card's border lies outside the strip: within two pixels is full.
    assert!(
        strip.size.width >= full - px(2.),
        "Reduce Motion: the strip holds still"
    );
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(4));
    cx.run_until_parked();
    draw(cx);
    assert!(
        read(&view, cx, |app| app.toasts.is_empty()),
        "gone after 8 s"
    );
    assert!(cx.debug_bounds("toast-update").is_none());
    cx.update(|_, cx| cx.set_reduce_motion(false));
}

/// The toasts sit above each screen's footer: on Review they never cover
/// Export, Unmark all, Back or the removal button.
#[gpui_kit::test]
fn a_toast_never_covers_the_review_footer(cx: &mut TestAppContext) {
    use crate::update_channel::Slot;
    use crate::updater::Outcome;
    cx.update(init);
    let temp = fixture();
    let (view, cx, _tree) = review_with_tree(temp.path(), cx);
    // Short enough that a stack anchored to the window would reach the
    // footer.
    cx.simulate_resize(gpui_kit::size(px(900.), px(420.)));
    ask_for_updates(&view, cx);
    let failed = update_state(1, Outcome::Failed("timed out".into()), None);
    update(&view, cx, |app, cx| {
        app.follow_updates(failed, "0.14.0", Slot::default(), cx);
    });
    draw(cx);
    let stack = cx.debug_bounds("toast-stack").expect("the answer");
    let footer = cx.debug_bounds("review-footer").expect("footer");
    assert!(
        stack.bottom() <= footer.top(),
        "the stack ends above the footer: {stack:?} over {footer:?}"
    );
    for id in ["unmark-all", "back", "commit"] {
        let button = cx.debug_bounds(id).expect(id);
        assert!(!stack.intersects(&button), "{id} is clear");
    }
}

/// A toast drawn over the treemap takes the pointer: the tile under it
/// shows no tooltip while the pointer is on the card, and a click or a
/// scroll on the card never reaches the mosaic.
#[gpui_kit::test]
fn a_toast_hides_the_treemap_under_it(cx: &mut TestAppContext) {
    use crate::update_channel::Slot;
    use crate::updater::Outcome;
    use gpui_kit::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent};
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    ask_for_updates(&view, cx);
    let failed = update_state(1, Outcome::Failed("timed out".into()), None);
    update(&view, cx, |app, cx| {
        app.follow_updates(failed, "0.14.0", Slot::default(), cx);
    });
    draw(cx);
    let stack = cx.debug_bounds("toast-stack").expect("the answer");
    let treemap = cx.debug_bounds("treemap").expect("the mosaic");
    // On the card's padding, clear of its buttons, and over the mosaic.
    let on_card = stack.origin + Point::new(px(6.), px(6.));
    assert!(treemap.contains(&on_card), "{stack:?} over {treemap:?}");

    // Into the mosaic first, so there is a tooltip for the card to end.
    let beside = Point::new(stack.right() + px(20.), on_card.y);
    assert!(treemap.contains(&beside), "{beside:?} in {treemap:?}");
    cx.simulate_mouse_move(beside, None, Modifiers::none());
    draw(cx);
    assert!(cx.debug_bounds("cursor-tooltip").is_some(), "a tile's card");

    cx.simulate_mouse_move(on_card, None, Modifiers::none());
    draw(cx);
    assert!(
        cx.debug_bounds("cursor-tooltip").is_none(),
        "the pointer is on the toast, not the tile under it"
    );
    assert!(
        read(&view, cx, |app| app.hovered.is_none()),
        "nothing hovered"
    );
    assert!(
        read(&view, cx, |app| app.toasts.is_hovered()),
        "the stack is"
    );

    let (selected, zoom) =
        read(&view, cx, |app| (app.selected.clone(), app.view));
    cx.simulate_event(MouseDownEvent {
        button: MouseButton::Left,
        position: on_card,
        modifiers: Modifiers::none(),
        click_count: 1,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        button: MouseButton::Left,
        position: on_card,
        modifiers: Modifiers::none(),
        click_count: 1,
    });
    cx.simulate_event(gpui_kit::ScrollWheelEvent {
        position: on_card,
        delta: gpui_kit::ScrollDelta::Lines(Point::new(0., 3.)),
        modifiers: Modifiers::none(),
        touch_phase: gpui_kit::TouchPhase::Moved,
    });
    draw(cx);
    assert_eq!(
        read(&view, cx, |app| (app.selected.clone(), app.view)),
        (selected, zoom),
        "a click or a scroll on the card leaves the mosaic alone"
    );
}

/// Review's export menu opens over the marked list and takes the pointer:
/// resting on it never raises the path tooltip of the row underneath, and
/// a press on its padding reaches no row.
#[gpui_kit::test]
fn the_export_menu_hides_the_list_under_it(cx: &mut TestAppContext) {
    use gpui_kit::Modifiers;
    const TIP: &str = "omarchy-tooltip-surface";
    cx.update(init);
    let temp = fixture();
    let (view, cx, _tree) = review_with_tree(temp.path(), cx);
    // Short enough that the menu, which opens upward from the footer,
    // reaches the first row.
    cx.simulate_resize(gpui_kit::size(px(900.), px(300.)));
    draw(cx);
    let row = cx.debug_bounds("review-row-0").expect("the marked row");

    // The row's own tooltip, so its absence below means something.
    rest_at(cx, row.center());
    assert!(cx.debug_bounds(TIP).is_some(), "the row names its path");
    let back = cx.debug_bounds("back").expect("Back");
    rest_at(cx, back.center());

    click_on(cx, "export");
    let menu = cx.debug_bounds("export-menu").expect("the menu");
    // In the menu's top padding, above its first item, and over the row.
    let on_menu = menu.origin + Point::new(px(24.), px(2.));
    assert!(row.contains(&on_menu), "{menu:?} over {row:?}");

    rest_at(cx, on_menu);
    assert!(
        cx.debug_bounds(TIP).is_none(),
        "the pointer is on the menu, not the row under it"
    );

    let marks = read(&view, cx, |app| app.marks.len());
    cx.simulate_click(on_menu, Modifiers::none());
    draw(cx);
    assert_eq!(read(&view, cx, |app| app.marks.len()), marks, "no row hit");
    assert!(
        cx.debug_bounds("export-menu").is_some(),
        "a press inside the menu leaves it open"
    );
}

/// Rest the pointer at `at` past a tooltip's delay, and draw what shows.
fn rest_at(cx: &mut Window, at: Point<Pixels>) {
    cx.simulate_mouse_move(at, None, gpui_kit::Modifiers::none());
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(600));
    cx.run_until_parked();
    draw(cx);
}

/// The first log line drawn, by its number: it moves when the list scrolls.
fn first_log_row(cx: &mut Window) -> Option<usize> {
    (1..=400).find(|number| {
        cx.debug_bounds(Box::leak(format!("log-row-{number}").into_boxed_str()))
            .is_some()
    })
}

fn scroll_at(cx: &mut Window, at: Point<Pixels>, dy: f32) {
    cx.simulate_event(gpui_kit::ScrollWheelEvent {
        position: at,
        delta: gpui_kit::ScrollDelta::Pixels(Point::new(px(0.), px(dy))),
        ..Default::default()
    });
    draw(cx);
}

/// Logs' context menu drops over the lines and takes the pointer: a scroll
/// on it moves nothing. The selection bar floats over the lines too: a
/// press on it never picks the line number beneath, though a scroll there
/// still moves the lines, as it does over the new-lines pill.
#[gpui_kit::test]
fn logs_overlays_hide_the_lines_under_them(cx: &mut TestAppContext) {
    use crate::logs_window::{CONTEXT_ID, LIVE_ID, PILL_ID};
    use gpui_kit::Modifiers;
    let line = |number: usize| {
        format!("2026-10-31T12:00:00.000-04:00 INFO  pwrdisk: line {number}\n")
    };
    let text: String = (1..=200).map(line).collect();
    let (_dir, path, view, cx) = logs_over(cx, &text);
    let viewport = cx.debug_bounds("logs-viewport").expect("the lines");

    // The context menu: a search in Filter mode offers it.
    let search = cx.update(|_, cx| view.read(cx).search_state());
    cx.update(|window, cx| {
        search.update(cx, |search, cx| search.focus(window, cx));
    });
    cx.simulate_input("line");
    draw(cx);
    click_on(cx, CONTEXT_ID);
    let menu = cx.debug_bounds("logs-context-menu").expect("the menu");
    let on_menu = Point::new(menu.left() + px(24.), menu.bottom() - px(2.));
    assert!(viewport.contains(&on_menu), "{menu:?} over {viewport:?}");
    let first = first_log_row(cx);
    scroll_at(cx, on_menu, -400.);
    assert_eq!(first_log_row(cx), first, "the lines stay put");
    assert!(cx.debug_bounds("logs-context-menu").is_some(), "still open");
    cx.update(|window, cx| {
        search.update(cx, |search, cx| search.focus(window, cx));
    });
    cx.simulate_keystrokes("escape");
    draw(cx);

    // The selection bar, at double text size in a narrow window, where it
    // reaches the line numbers; tall enough that lines show beneath it.
    cx.update(|window, _| window.set_rem_size(px(32.)));
    cx.simulate_resize(gpui_kit::size(px(640.), px(1000.)));
    draw(cx);
    let first = first_log_row(cx).expect("a line");
    let picked: &'static str =
        Box::leak(format!("log-number-{first}").into_boxed_str());
    click_on(cx, picked);
    let selection = cx.update(|_, cx| view.read(cx).selection);
    assert!(selection.is_some(), "a line is picked");
    let bar = cx.debug_bounds("logs-selection-bar").expect("the bar");
    // The line number that lies most under the bar, within the lines'
    // visible frame: a row's part outside it is clipped and takes no press.
    let lines = cx.debug_bounds("logs-viewport").expect("the lines");
    let clear = bar.intersect(&lines);
    let under = (first..first + 60)
        .filter_map(|number| {
            cx.debug_bounds(Box::leak(
                format!("log-number-{number}").into_boxed_str(),
            ))
        })
        .max_by(|a, b| {
            let tall = |g: &Bounds<Pixels>| g.intersect(&clear).size.height;
            tall(a).as_f32().total_cmp(&tall(b).as_f32())
        })
        .filter(|gutter| gutter.intersect(&clear).size.height > px(8.))
        .unwrap_or_else(|| panic!("a line number under {bar:?}"));
    // In the middle of where the two overlap: the bar's label there is
    // only text, so the press is the bar's.
    let on_bar = under.intersect(&clear).center();
    assert!(bar.contains(&on_bar), "{bar:?} at {on_bar:?}");
    assert!(under.contains(&on_bar), "{bar:?} over {under:?}");
    cx.simulate_click(on_bar, Modifiers::none());
    draw(cx);
    assert_eq!(
        cx.update(|_, cx| view.read(cx).selection),
        selection,
        "a press on the bar picks no line"
    );
    let top = first_log_row(cx);
    scroll_at(cx, on_bar, -400.);
    assert_ne!(first_log_row(cx), top, "a scroll there moves the lines");
    cx.simulate_keystrokes("escape");
    draw(cx);

    // The pill: paused, new lines count on it, and a scroll over it still
    // reaches the lines.
    cx.update(|window, _| window.set_rem_size(px(16.)));
    cx.simulate_resize(gpui_kit::size(px(1120.), px(720.)));
    draw(cx);
    if cx.update(|_, cx| view.read(cx).live) {
        click_on(cx, LIVE_ID);
    }
    append_log(&path, &line(201));
    poll_logs(cx);
    let pill = cx.debug_bounds(PILL_ID).expect("the pill");
    let top = first_log_row(cx);
    scroll_at(cx, pill.center(), 400.);
    assert_ne!(first_log_row(cx), top, "a scroll over the pill moves them");
}

/// The note that nothing passes the legend's filter sits over the mosaic:
/// the pointer on it raises no tile's card and a press selects no tile.
#[gpui_kit::test]
fn the_empty_filter_note_hides_the_treemap_under_it(cx: &mut TestAppContext) {
    use crate::legend::ChipId;
    use gpui_kit::Modifiers;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
    draw(cx);
    update(&view, cx, |app, cx| {
        app.cycle_legend(ChipId::Reclaimable, cx);
        app.cycle_legend(ChipId::Kind(2), cx);
    });
    cx.run_until_parked();
    draw(cx);
    let note = cx.debug_bounds("legend-empty").expect("the note");
    let on_note = note.origin + Point::new(px(4.), px(4.));
    cx.simulate_mouse_move(on_note, None, Modifiers::none());
    draw(cx);
    assert!(
        cx.debug_bounds("cursor-tooltip").is_none(),
        "the pointer is on the note"
    );
    let selected = read(&view, cx, |app| app.selected.clone());
    cx.simulate_click(on_note, Modifiers::none());
    draw(cx);
    assert_eq!(read(&view, cx, |app| app.selected.clone()), selected);
}

/// Removing's Follow pill floats over the list: resting on it raises no
/// row's path, though a scroll over it still scrolls the list.
#[gpui_kit::test]
fn the_follow_pill_hides_the_row_under_it(cx: &mut TestAppContext) {
    use crate::run_list::{Item, RunList, Status};
    const TIP: &str = "omarchy-tooltip-surface";
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let path = |index: usize| temp.path().join(format!("dir-{index:03}"));
    update(&view, cx, |app, cx| {
        app.run_list = RunList::new(
            (0..80)
                .map(|index| Item {
                    target: disktree_core::removal::Target {
                        path: path(index),
                        bytes: 1_000,
                        is_dir: true,
                        hidden: false,
                    },
                    category: disktree_core::classify::Category::Other,
                    reclaim: None,
                    private: None,
                    status: Status::Waiting,
                })
                .collect(),
        );
        app.screen = Screen::Running;
        cx.notify();
    });
    draw(cx);
    let settle = |cx: &mut Window, view: &Entity<Disktree>, upto| {
        update(view, cx, |app, cx| {
            app.update_follow();
            for index in 0..upto {
                if app.run_list.settle(&path(index), &Ok(())).is_some() {
                    app.scroll_to_focus();
                }
            }
            cx.notify();
        });
        draw(cx);
        draw(cx);
    };
    settle(cx, &view, 60);
    let list = cx.debug_bounds("run-list").expect("the list");
    scroll_at(cx, list.center(), 100_000.);
    settle(cx, &view, 65);
    let pill = cx.debug_bounds("follow-removal").expect("the pill");

    // A row's own tooltip, so its absence below means something.
    rest_at(cx, Point::new(list.left() + px(40.), pill.center().y));
    assert!(cx.debug_bounds(TIP).is_some(), "the row names its path");
    rest_at(cx, list.origin - Point::new(px(4.), px(4.)));
    rest_at(cx, pill.origin + Point::new(px(4.), px(4.)));
    assert!(
        cx.debug_bounds(TIP).is_none(),
        "the pointer is on the pill, not the row under it"
    );

    update(&view, cx, |app, _| app.run_list.scrolled_by_hand = false);
    scroll_at(cx, pill.center(), -200.);
    assert!(
        read(&view, cx, |app| app.run_list.scrolled_by_hand),
        "a scroll over the pill still scrolls the list"
    );
}

/// The strip's menu drops over the treemap and takes the pointer: a press
/// on its padding selects no tile, and a scroll there zooms nothing.
#[gpui_kit::test]
fn the_app_menu_hides_the_treemap_under_it(cx: &mut TestAppContext) {
    use gpui_kit::Modifiers;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, Disktree::toggle_app_menu);
    draw(cx);
    let menu = cx.debug_bounds("app-menu").expect("the menu");
    let treemap = cx.debug_bounds("treemap").expect("the mosaic");
    // On a group's heading, which is only text: just above the group's
    // first row, in the first group drawn over the mosaic.
    let on_menu = (1..8)
        .filter_map(|group| {
            let first = (0..8).find_map(|index| {
                cx.debug_bounds(Box::leak(
                    format!("app-menu-{group}-{index}").into_boxed_str(),
                ))
            })?;
            Some(Point::new(menu.origin.x + px(24.), first.top() - px(4.)))
        })
        .find(|at| treemap.contains(at))
        .unwrap_or_else(|| panic!("a heading over {treemap:?} in {menu:?}"));

    let (selected, zoom) =
        read(&view, cx, |app| (app.selected.clone(), app.view));
    cx.simulate_click(on_menu, Modifiers::none());
    cx.simulate_event(gpui_kit::ScrollWheelEvent {
        position: on_menu,
        delta: gpui_kit::ScrollDelta::Lines(Point::new(0., 3.)),
        modifiers: Modifiers::none(),
        touch_phase: gpui_kit::TouchPhase::Moved,
    });
    draw(cx);
    assert_eq!(
        read(&view, cx, |app| (app.selected.clone(), app.view)),
        (selected, zoom),
        "a press or a scroll on the menu leaves the mosaic alone"
    );
    assert!(read(&view, cx, |app| app.app_menu_open), "still open");

    // The menu is taller than its cap, and still scrolls itself.
    cx.simulate_event(gpui_kit::ScrollWheelEvent {
        position: on_menu,
        delta: gpui_kit::ScrollDelta::Lines(Point::new(0., -3.)),
        modifiers: Modifiers::none(),
        touch_phase: gpui_kit::TouchPhase::Moved,
    });
    draw(cx);
    assert!(
        read(&view, cx, |app| app.menu_scroll.offset().y) < px(0.),
        "the list scrolled"
    );
    assert_eq!(read(&view, cx, |app| app.view), zoom, "the mosaic did not");
}

/// The ready card gives way while a removal runs (Restart would wait for
/// it anyway); the strip's chip still offers the release. The pointer that
/// rested on the card is forgotten with it, so the next answer still runs
/// out.
#[gpui_kit::test]
fn a_removal_keeps_the_ready_card_down(cx: &mut TestAppContext) {
    use crate::update_channel::Slot;
    use crate::updater::Outcome;
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    let ready =
        update_state(1, Outcome::Downloaded("0.15.0".into()), Some("0.15.0"));
    update(&view, cx, |app, cx| {
        app.follow_updates(ready, "0.14.0", Slot::default(), cx);
    });
    draw(cx);
    assert!(cx.debug_bounds("toast-update").is_some());
    update(&view, cx, |app, cx| {
        // The pointer is on the card when the removal starts.
        let now = cx.background_executor().now();
        app.toasts.set_hovered(true, now);
        app.run = Some(disktree_core::removal::spawn(
            disktree_core::removal::Plan::default(),
            RemovalMode::Trash,
        ));
    });
    draw(cx);
    assert!(cx.debug_bounds("toast-update").is_none());
    assert!(cx.debug_bounds(crate::views::UPDATE_CHIP_ID).is_some());
    assert!(
        !read(&view, cx, |app| app.toasts.is_hovered()),
        "a stack that is not drawn holds no pointer"
    );
    update(&view, cx, |app, _| app.run = None);
}

/// A window over the fixture, on the review screen with `junk` marked, and
/// a weak handle to its tree.
fn review_with_tree<'a>(
    temp: &Path,
    cx: &'a mut TestAppContext,
) -> (
    Entity<Disktree>,
    &'a mut Window,
    std::sync::Weak<disktree_core::tree::Tree>,
) {
    let (view, cx) = view_over(temp, cx);
    draw(cx);
    let tree = update(&view, cx, |app, cx| {
        let junk = child_crumbs(app, &[], "junk");
        app.toggle_mark(&junk, cx);
        std::sync::Arc::downgrade(app.tree.as_ref().expect("a scanned tree"))
    });
    press(cx, "c");
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Review);
    (view, cx, tree)
}

/// Quitting from the review screen drops the window and its tree. Freed on
/// the main thread, a home directory's tens of millions of nodes kept the
/// app from exiting for seconds after Quit, and macOS called it not
/// responding. On quit nothing is freed: the process exits and the system
/// takes the memory back whole.
#[gpui_kit::test]
fn quitting_from_review_leaves_the_tree_to_the_system(cx: &mut TestAppContext) {
    use crate::marks::work;

    cx.update(init);
    let temp = fixture();
    let (view, cx, tree) = review_with_tree(temp.path(), cx);

    work::reset();
    drop(view);
    // What Quit runs on every platform: the quit hooks, then every window
    // and the state it held is dropped, on this thread.
    cx.cx.update(gpui_kit::App::shutdown);

    assert_eq!(work::DISCARDED.get(), 0, "no thread is started to free it");
    assert_eq!(tree.strong_count(), 1, "nothing walks the nodes on quit");
}

/// Closing the window while the app stays frees the tree, off the main
/// thread: nothing is leaked and nothing waits on it.
#[gpui_kit::test]
fn closing_a_window_frees_its_tree_off_the_main_thread(
    cx: &mut TestAppContext,
) {
    use crate::marks::work;
    use std::time::{Duration, Instant};

    cx.update(init);
    let temp = fixture();
    let (view, cx, tree) = review_with_tree(temp.path(), cx);

    work::reset();
    drop(view);
    cx.update(|window, _| window.remove_window());
    cx.run_until_parked();

    assert_eq!(work::DISCARDED.get(), 1, "the tree is handed off");
    let deadline = Instant::now() + Duration::from_secs(10);
    while tree.strong_count() > 0 {
        assert!(Instant::now() < deadline, "the tree is never freed");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[gpui_kit::test]
fn main_integration_help_dismisses_the_export_menu(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        let junk = child_crumbs(app, &[], "junk");
        app.toggle_mark(&junk, cx);
        app.open_review(cx);
    });
    draw(cx);
    click_on(cx, "export");
    assert!(read(&view, cx, |app| app.export_menu_open));
    press(cx, "?");
    read(&view, cx, |app| {
        assert!(app.show_help);
        assert!(!app.export_menu_open);
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.marks.len(), 1);
    });
    press(cx, "escape");
    assert!(read(&view, cx, |app| !app.show_help && app.marks.len() == 1));
    assert!(temp.path().join("junk/blob.bin").exists());
}

/// Offscreen document text becomes a real row before its active semantic
/// descendant changes. Copy remains complete, independent of virtualization.
#[gpui_kit::test]
fn documents_reach_the_end_and_copy_by_keyboard(cx: &mut TestAppContext) {
    use crate::doc_window::{Doc, DocWindow};
    cx.update(init);
    let (view, cx) = cx.add_window_view(|window, cx| {
        let view = DocWindow::new(Doc::ThirdPartyNotices, cx);
        window.focus(&view.focus, cx);
        view
    });
    draw(cx);
    press(cx, "end");
    let last = view.read_with(cx, |view, _| view.position);
    assert!(last > 1_000, "a long document, beyond its first viewport");
    assert!(cx.debug_bounds("document-current").is_some());
    press(cx, "home");
    assert_eq!(view.read_with(cx, |view, _| view.position), 0);
    assert!(cx.debug_bounds("document-current").is_some());
    press(cx, "ctrl-c");
    assert_eq!(
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref(),
        Some(Doc::ThirdPartyNotices.source())
    );
}

/// Keyboard selection reaches virtual log rows and copying retains the full
/// Unicode line. No append is a live announcement.
#[gpui_kit::test]
fn logs_read_and_copy_offscreen_lines_by_keyboard(cx: &mut TestAppContext) {
    use std::fmt::Write as _;
    let mut text = String::new();
    for n in 0..300 {
        writeln!(text, "2026-10-31T12:00:00.000-04:00 INFO  fixture: invented line {n} 日本語")
            .expect("string");
    }
    let (_dir, _path, view, cx) = logs_over(cx, &text);
    cx.update(|window, cx| {
        view.read(cx)
            .search_state()
            .update(cx, |search, cx| search.focus(window, cx));
    });
    draw(cx);
    press(cx, "escape");
    press(cx, "end");
    assert_eq!(
        view.read_with(cx, |view, _| view.selection),
        Some((300, 300))
    );
    assert!(cx.debug_bounds("log-row-300").is_some());
    press(cx, "ctrl-c");
    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default();
    assert!(copied.contains("invented line 299 日本語"));
    assert!(!copied.contains("invented line 298"));
    press(cx, "home");
    assert_eq!(view.read_with(cx, |view, _| view.selection), Some((1, 1)));
    assert!(cx.debug_bounds("log-row-1").is_some());
}

fn invented_log(lines: usize) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    for n in 0..lines {
        writeln!(
            text,
            "2026-10-31T12:00:00.000-04:00 INFO  fixture: invented line {n}"
        )
        .expect("string");
    }
    text
}

/// The level and mode segments are reachable by Tab, both ways, and Space
/// works them: focusable alone is not a tab stop in GPUI.
#[gpui_kit::test]
fn aux_review_log_toggles_are_tab_stops(cx: &mut TestAppContext) {
    use crate::log_view::Level;
    let (_dir, _path, view, cx) = logs_over(cx, &invented_log(30));
    draw(cx);
    click_on(cx, "logs-level-info");
    assert!(!view.read_with(cx, |view, _| view.view.shows(Level::Info)));
    let target = cx
        .update(|window, cx| window.focused(cx))
        .expect("segment focus");
    cycle_back_to_focus(cx, &target, "tab");
    cycle_back_to_focus(cx, &target, "shift-tab");
    press(cx, "space");
    assert!(view.read_with(cx, |view, _| view.view.shows(Level::Info)));
}

/// With nothing selected, Down starts at the first line and Up at the
/// last, rather than stepping past either.
#[gpui_kit::test]
fn aux_review_first_log_step_selects_an_end(cx: &mut TestAppContext) {
    let (_dir, _path, view, cx) = logs_over(cx, &invented_log(30));
    cx.update(|window, cx| {
        view.read(cx)
            .search_state()
            .update(cx, |search, cx| search.focus(window, cx));
    });
    draw(cx);
    press(cx, "escape");
    press(cx, "down");
    assert_eq!(view.read_with(cx, |view, _| view.selection), Some((1, 1)));
    press(cx, "escape");
    assert_eq!(view.read_with(cx, |view, _| view.selection), None);
    press(cx, "up");
    assert_eq!(view.read_with(cx, |view, _| view.selection), Some((30, 30)));
}

/// The release channels are one radio group with one Tab stop, on the
/// channel picked.
#[gpui_kit::test]
fn aux_review_picked_channel_is_a_tab_stop(cx: &mut TestAppContext) {
    use crate::settings::{Pane, SettingsWindow};
    cx.update(init);
    let saved_in = tempfile::TempDir::new().expect("tempdir");
    let (view, cx) =
        cx.add_window_view(|_, cx| SettingsWindow::new(Pane::Updates, cx));
    cx.update(|_, cx| {
        view.update(cx, |view, _| {
            view.updates_dir = Some(saved_in.path().to_path_buf());
        });
    });
    draw(cx);
    click_on(cx, "update-slot-0");
    let target = cx
        .update(|window, cx| window.focused(cx))
        .expect("channel focus");
    cycle_back_to_focus(cx, &target, "tab");
    cycle_back_to_focus(cx, &target, "shift-tab");
}

/// Tab leaves the title strip's Copy for the reader and comes back: the
/// frame takes Tab above the strip, where the reader's own keys cannot.
#[gpui_kit::test]
fn aux_review_document_tab_leaves_the_title_strip(cx: &mut TestAppContext) {
    use crate::doc_window::{COPY_ID, Doc, DocWindow};
    cx.update(init);
    let (view, cx) = cx.add_window_view(|window, cx| {
        let view = DocWindow::new(Doc::License, cx);
        window.focus(&view.focus, cx);
        view
    });
    draw(cx);
    let reader = view.read_with(cx, |view, _| view.focus.clone());
    cycle_back_to_focus(cx, &reader, "tab");
    click_on(cx, COPY_ID);
    let copy = cx
        .update(|window, cx| window.focused(cx))
        .expect("copy focus");
    assert!(copy != reader, "the strip's Copy takes focus");
    cycle_back_to_focus(cx, &reader, "tab");
    cycle_back_to_focus(cx, &copy, "shift-tab");
}

/// Three 10 GiB worktrees sharing all but 100 MiB each, marked one by one
/// in the window: the meter and the review button project each one's own
/// 100 MiB until the last clone of the group is marked, then the shared
/// data once. Synthetic clone facts, so it runs on every system.
#[gpui_kit::test]
fn marking_a_worktree_trio_one_by_one_frees_one_copy_at_the_end(
    cx: &mut TestAppContext,
) {
    use disktree_core::sharing::{Allocation, CloneInfo};
    use disktree_core::tree::{Draft, Metric, NodeKind, Tree, aggregate};
    const MIB: u64 = 1024 * 1024;

    cx.update(init);
    let temp = tempfile::tempdir_in(std::env::current_dir().expect("cwd"))
        .expect("tempdir in the checkout");
    let root_path = std::fs::canonicalize(temp.path()).expect("root");
    let mut root = Draft::directory("fixture");
    for name in ["wt-a", "wt-b", "wt-c"] {
        std::fs::create_dir(root_path.join(name)).expect("mkdir");
        let mut dir = Draft::directory(name);
        let mut file = Draft::entry("objects", NodeKind::File, 10_140 * MIB);
        file.allocation = Some(Box::new(Allocation {
            bytes: 10_140 * MIB,
            identity: None,
            links: 1,
            followed: false,
            clone: Some(CloneInfo {
                device: 1,
                id: 77,
                references: 3,
                private_bytes: Some(0),
                data_bytes: 10_140 * MIB,
            }),
        }));
        dir.children.push(file);
        dir.children
            .push(Draft::entry("own", NodeKind::File, 100 * MIB));
        root.children.push(dir);
    }
    let mut root = Tree::from(root);
    aggregate(&mut root, Metric::Bytes);
    let path = root_path.clone();
    let (view, cx) = cx.add_window_view(move |_, cx| {
        // Allocated sizes: the helper's apparent sizes project nothing.
        Disktree::with_tree(path.clone(), root, ScanOptions::default(), 3, cx)
    });
    // 30 GiB of synthetic worktrees on whatever disk runs the tests: past
    // 5% of a small CI disk, which is not what this test is about.
    update(&view, cx, |app, _| {
        app.safety_policy.replace(Some(crate::safety::Policy {
            limit: None,
            ..crate::safety::Policy::default()
        }));
    });
    draw(cx);
    let mut seen = Vec::new();
    for name in ["wt-a", "wt-b", "wt-c"] {
        update(&view, cx, |app, cx| {
            let crumbs = app
                .crumbs_for_path(&root_path.join(name))
                .expect("in the tree");
            app.toggle_mark(&crumbs, cx);
        });
        draw(cx);
        seen.push(read(&view, cx, |app| {
            let plan = app.plan();
            let linked = app.agent_state().estimate.expect("an estimate");
            assert_eq!(linked.lower, plan.reclaimable_bytes());
            (
                plan.reclaimable_bytes(),
                plan.possible_bytes(),
                plan.reclaim(),
            )
        }));
    }
    assert_eq!(
        seen,
        [
            (100 * MIB, 100 * MIB, 100 * MIB),
            (200 * MIB, 200 * MIB, 200 * MIB),
            (10_440 * MIB, 10_440 * MIB, 10_440 * MIB),
        ]
    );
}

/// Help keeps its heading/Done on screen and scrolls its reading lane at
/// minimum dimensions; neither zoom nor keys act on the marked background.
#[gpui_kit::test]
fn help_reflows_and_scrolls_at_minimum_size_and_double_scale(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    for rem in [16., 32.] {
        cx.simulate_resize(gpui_kit::size(px(900.), px(600.)));
        cx.update(|window, _| window.set_rem_size(px(rem)));
        update(&view, cx, Disktree::open_help);
        draw(cx);
        let popup = cx.debug_bounds("help-overlay").expect("Help popup");
        let close = cx.debug_bounds("overlay-done").expect("Done");
        assert!(
            cx.debug_bounds("help-scroll")
                .expect("reading lane")
                .size
                .height
                >= px(rem * 3.0)
        );
        assert!(
            popup.left() >= px(0.) && popup.right() <= px(900.),
            "{popup:?}"
        );
        assert!(
            popup.top() >= px(0.) && popup.bottom() <= px(600.),
            "{popup:?}"
        );
        assert!(
            close.top() >= px(0.) && close.bottom() <= px(600.),
            "{close:?}"
        );
        press(cx, "end");
        assert!(read(&view, cx, |app| app.help_scroll.offset().y) < px(0.));
        press(cx, "home");
        assert!(
            read(&view, cx, |app| app.help_scroll.offset().y).abs() < px(1.)
        );
        press(cx, "escape");
        assert!(!read(&view, cx, |app| app.show_help));
        assert!(temp.path().join("junk/blob.bin").exists());
    }
}

/// Controls stay reachable when the toolbar wraps; tiny tiles have the
/// same larger file-list target alternative at every zoom step.
#[gpui_kit::test]
fn double_scale_wraps_controls_and_keeps_pointer_targets(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (_view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(900.), px(600.)));
    for _ in 0..5 {
        press(cx, "ctrl-=");
    }
    assert!(
        (cx.update(|window, _| window.rem_size().as_f32()) - 32.).abs() < 0.01
    );
    // At 28 rem the bar keeps only the view switch; Depth is in ⋮.
    assert!(cx.debug_bounds("depth-less-target").is_none());
    click_on(cx, "more-toggle");
    for selector in ["more-toggle", "more-depth-less", "more-depth-more"] {
        let bounds = cx.debug_bounds(selector).expect("depth target");
        assert!(
            bounds.right() <= px(900.) && bounds.bottom() <= px(600.),
            "{bounds:?}"
        );
        assert!(
            bounds.size.width >= px(24.) && bounds.size.height >= px(24.),
            "{bounds:?}"
        );
    }
    press(cx, "ctrl-0");
    for _ in 0..2 {
        press(cx, "ctrl--");
    }
    let bounds = cx.debug_bounds("depth-more-target").expect("depth target");
    assert!(bounds.size.width >= px(24.) && bounds.size.height >= px(24.));
}

#[gpui_kit::test]
fn settings_panes_scroll_at_minimum_size_and_double_scale(
    cx: &mut TestAppContext,
) {
    use crate::settings::{Pane, SettingsWindow};
    cx.update(init);
    let (view, cx) =
        cx.add_window_view(|_, cx| SettingsWindow::new(Pane::About, cx));
    cx.simulate_resize(gpui_kit::size(px(640.), px(480.)));
    cx.update(|window, _| window.set_rem_size(px(32.)));
    draw(cx);
    let scroll = cx.debug_bounds("settings-scroll").expect("pane scroll");
    assert!(
        scroll.size.width > px(300.),
        "navigation reflows above the pane: {scroll:?}"
    );
    let nav = cx.debug_bounds("settings-nav-about").expect("About tab");
    cx.simulate_click(nav.center(), gpui_kit::Modifiers::none());
    press(cx, "end");
    assert!(view.read_with(cx, |view, _| view.pane_scroll.offset().y) < px(0.));
}

/// Reflow is measured in glyph widths rather than a fixed 100 characters.
/// Complete copy and end navigation remain available at the document minimum.
#[gpui_kit::test]
fn documents_reflow_at_minimum_size_and_double_scale(cx: &mut TestAppContext) {
    use crate::doc_window::{Doc, DocWindow};
    cx.update(init);
    let (view, cx) = cx.add_window_view(|window, cx| {
        let view = DocWindow::new(Doc::ThirdPartyNotices, cx);
        window.focus(&view.focus, cx);
        view
    });
    cx.simulate_resize(gpui_kit::size(px(640.), px(480.)));
    cx.update(|window, _| window.set_rem_size(px(32.)));
    draw(cx);
    press(cx, "end");
    assert!(view.read_with(cx, |view, _| view.position) > 1_000);
    let row = cx.debug_bounds("document-current").expect("last row");
    assert!(row.left() >= px(0.) && row.right() <= px(640.), "{row:?}");
    press(cx, "ctrl--");
    assert!(
        (cx.update(|window, _| window.rem_size().as_f32()) - 28.).abs() < 0.01
    );
    press(cx, "ctrl-c");
    assert_eq!(clipboard(cx), Doc::ThirdPartyNotices.source());
}

/// The review's list and explanation stack in a narrow text viewport.
/// Scrolling never commits a removal.
#[gpui_kit::test]
fn review_scrolls_at_minimum_size_and_double_scale(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        let junk = child_crumbs(app, &[], "junk");
        app.toggle_mark(&junk, cx);
        app.open_review(cx);
    });
    cx.simulate_resize(gpui_kit::size(px(900.), px(600.)));
    cx.update(|window, _| window.set_rem_size(px(32.)));
    draw(cx);
    press(cx, "end");
    assert!(read(&view, cx, |app| app.review_scroll.offset().y) < px(0.));
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Review);
    assert!(temp.path().join("junk/blob.bin").exists());
    assert!(temp.path().join("keep/notes.txt").exists());
}

#[gpui_kit::test]
fn logs_wrap_at_minimum_size_and_double_scale(cx: &mut TestAppContext) {
    let text = format!(
        "2026-10-31T12:00:00.000-04:00 INFO  fixture: {}\n",
        "invented 日本語 ".repeat(50)
    );
    let (_dir, _path, view, cx) = logs_over(cx, &text);
    cx.update(|window, _| window.set_rem_size(px(32.)));
    for width in [640., 880., 1120.] {
        cx.simulate_resize(gpui_kit::size(px(width), px(480.)));
        draw(cx);
        let search = cx
            .debug_bounds(crate::logs_window::SEARCH_ID)
            .expect("named search");
        assert!(search.right() <= px(width), "{search:?}");
        assert!(
            search.size.width >= px(400.),
            "search retains room for input and match controls: {search:?}"
        );
        let row = cx.debug_bounds("log-row-1").unwrap_or_else(|| {
            panic!(
                "wrapped line at {width}: toolbar={:?}, viewport={:?}",
                cx.debug_bounds("logs-toolbar"),
                cx.debug_bounds("logs-viewport")
            )
        });
        assert!(row.right() <= px(width), "{row:?}");
    }
    assert!(view.read_with(cx, |view, _| view.wrap));
}

/// A single wrapped log line can be taller than the entire window. Paging
/// must expose its lower text instead of repeatedly selecting its start.
#[gpui_kit::test]
fn page_scrolls_inside_an_oversized_log_line(cx: &mut TestAppContext) {
    let text = format!(
        "2026-10-31T12:00:00.000-04:00 INFO  fixture: {}\n",
        "invented 日本語 ".repeat(300)
    );
    let (_dir, _path, view, cx) = logs_over(cx, &text);
    cx.simulate_resize(gpui_kit::size(px(640.), px(480.)));
    cx.update(|window, cx| {
        window.set_rem_size(px(32.));
        view.read(cx)
            .search_state()
            .update(cx, |search, cx| search.focus(window, cx));
    });
    draw(cx);
    press(cx, "escape");
    press(cx, "home");
    let before = view.read_with(cx, |view, _| view.scroll_offset());
    press(cx, "pagedown");
    let after = view.read_with(cx, |view, _| view.scroll_offset());
    assert!(
        after < before,
        "paging progresses inside one log line: {before:?} -> {after:?}"
    );
    assert_eq!(view.read_with(cx, |view, _| view.selection), Some((1, 1)));
}

/// Low-vision readers need a complete file entry between the fixed heading
/// and actions, including when a long root path would otherwise wrap.
#[gpui_kit::test]
fn file_list_keeps_a_complete_entry_visible_at_double_scale(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.file_list = true;
        app.selected = Some(vec![0]);
        cx.notify();
    });
    cx.simulate_resize(gpui_kit::size(px(900.), px(600.)));
    cx.update(|window, _| window.set_rem_size(px(32.)));
    draw(cx);
    draw(cx);
    let viewport = read(&view, cx, |app| app.file_scroll.bounds());
    let row = cx.debug_bounds("file-row-0").expect("first file entry");
    // 24 px type plus 32 px vertical padding at 200%, before a border.
    assert!(
        row.size.height >= px(56.),
        "text must fit inside its row: {row:?}"
    );
    assert!(
        viewport.size.height >= row.size.height,
        "{viewport:?}, row {row:?}"
    );
    assert!(
        row.bottom() <= viewport.bottom() + px(1.),
        "{viewport:?}, row {row:?}"
    );
    assert!(row.size.height >= px(24.));
}

/// Reduced motion lands on the same real layout and preserves the ordinary
/// selection/history/mark behavior, including wheel-driven descent.
#[gpui_kit::test]
fn reduced_motion_lands_navigation_history_and_zoom(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    update(&view, cx, |app, cx| {
        let junk = child_crumbs(app, &[], "junk");
        let keep = child_crumbs(app, &[], "keep");
        app.toggle_mark(&keep, cx);
        app.selected = Some(junk.clone());
        app.descend(cx);
        assert!(app.transition.is_none(), "descent lands immediately");
        assert_eq!(app.crumbs, junk);
        assert_eq!(app.selected.as_ref(), Some(&junk));
        let expected: Vec<_> = app
            .layout()
            .expect("layout")
            .iter()
            .map(|tile| tile.rect)
            .collect();
        let drawn: Vec<_> = app
            .prepare()
            .tiles
            .into_iter()
            .map(|tile| tile.rect)
            .collect();
        assert_eq!(drawn, expected, "no transient geometry");
        app.ascend(cx);
        assert!(app.transition.is_none());
        assert!(app.crumbs.is_empty());
        app.go_back(cx);
        assert_eq!(app.crumbs, junk);
        assert!(app.transition.is_none());
        app.go_forward(cx);
        assert!(app.crumbs.is_empty());
        assert!(app.transition.is_none());
        app.go_to(junk.clone(), cx);
        app.zoom_at(100., 100., 1.2, false, cx);
        assert!(app.view.scale > 1.0, "user-controlled zoom still works");
        assert!(app.transition.is_none());
        app.reset_view(cx);
        let deeper = child_crumbs(app, &junk, "deeper");
        let body = app.tile_body(&deeper).expect("nested directory");
        let pivot = disktree_core::treemap::Rect::new(
            body.x + body.w / 2.,
            body.y + body.h / 2.,
            0.,
            0.,
        );
        for _ in 0..80 {
            if app.crumbs == deeper {
                break;
            }
            let screen = app.view.project(pivot);
            app.zoom_at(screen.x, screen.y, 1.15, true, cx);
        }
        assert_eq!(app.crumbs, deeper, "wheel zoom reaches its directory");
        assert!(app.transition.is_none());
        assert_eq!(app.marks.len(), 1);
    });
    draw(cx);
    assert!(temp.path().join("keep/notes.txt").exists());
}

/// A live policy change cancels an in-flight transition. Turning the policy
/// off restores ordinary navigation motion without changing the destination.
#[gpui_kit::test]
fn reduced_motion_cancels_active_transition_and_can_be_disabled(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    cx.update(|_, cx| cx.set_reduce_motion(false));
    update(&view, cx, |app, cx| {
        let junk = child_crumbs(app, &[], "junk");
        app.selected = Some(junk);
        app.descend(cx);
        assert!(app.transition.is_some(), "normal motion remains available");
    });
    let destination = read(&view, cx, Disktree::current_path);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    draw(cx);
    assert!(read(&view, cx, |app| app.transition.is_none()));
    assert_eq!(read(&view, cx, Disktree::current_path), destination);
    cx.update(|_, cx| cx.set_reduce_motion(false));
    update(&view, cx, |app, cx| {
        app.ascend(cx);
        assert!(app.transition.is_some(), "normal ascent animates again");
    });
    cx.update(|_, cx| cx.set_reduce_motion(true));
    draw(cx);
    assert!(read(&view, cx, |app| app.transition.is_none()));
    assert!(read(&view, cx, |app| app.crumbs.is_empty()));
}

#[gpui_kit::test]
fn the_view_switch_and_the_view_menu_show_the_file_list(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
    draw(cx);
    // The switch is in the title bar, not a row of its own over the mosaic.
    let strip = cx.debug_bounds("title-strip").expect("strip");
    let list = cx.debug_bounds("view-list").expect("switch");
    assert!(strip.contains(&list.center()), "{list:?} in {strip:?}");

    click_on(cx, "view-list");
    assert!(read(&view, cx, |app| app.file_list));
    assert!(cx.debug_bounds("file-list").is_some());
    assert!(
        cx.update(|window, cx| view.read(cx).file_focus.is_focused(window)),
        "the list takes the keyboard, so a screen reader lands in it"
    );

    cx.dispatch_action(crate::app_menu::ShowTreemap);
    draw(cx);
    assert!(!read(&view, cx, |app| app.file_list));
    assert!(cx.update(|window, cx| view.read(cx).focus.is_focused(window)));
    cx.dispatch_action(crate::app_menu::ShowFileList);
    draw(cx);
    assert!(read(&view, cx, |app| app.file_list));
    click_on(cx, "view-treemap");
    assert!(!read(&view, cx, |app| app.file_list));
}

#[gpui_kit::test]
fn the_bar_folds_into_more_as_the_window_narrows(cx: &mut TestAppContext) {
    use crate::toolbar::BarFit;

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    // Window size and interface zoom presses, then the tier it lands in.
    let cases = [
        (1440., 900., 0, BarFit::Full),
        (1100., 700., 0, BarFit::Folded),
        (900., 600., 0, BarFit::Folded),
        (900., 600., 2, BarFit::Narrow),
        (900., 600., 4, BarFit::Tight),
    ];
    let mut labelled_width = None;
    for (width, height, zoom, fit) in cases {
        press(cx, "ctrl-0");
        cx.simulate_resize(gpui_kit::size(px(width), px(height)));
        for _ in 0..zoom {
            press(cx, "ctrl-=");
        }
        draw(cx);
        assert_eq!(read(&view, cx, |app| app.bar_fit), fit, "{width} {zoom}");
        let full = fit == BarFit::Full;
        assert_eq!(
            cx.debug_bounds("power-efficiency").is_some(),
            full,
            "{fit:?}"
        );
        assert_eq!(cx.debug_bounds("more-toggle").is_none(), full, "{fit:?}");
        assert_eq!(
            cx.debug_bounds("history-back").is_some(),
            fit.shows_history_and_depth(),
            "{fit:?}"
        );
        assert_eq!(
            cx.debug_bounds("depth-less-target").is_some(),
            fit.shows_history_and_depth(),
            "{fit:?}"
        );
        assert_eq!(cx.debug_bounds("measure-mode").is_some(), fit.shows_mode());

        // Whatever is left fits in one bar, beside the brand.
        let strip = cx.debug_bounds("title-strip").expect("strip");
        let brand = cx.debug_bounds("strip-brand").expect("brand");
        let switch = cx.debug_bounds("view-treemap").expect("switch");
        let last = cx
            .debug_bounds(if full {
                "power-efficiency"
            } else {
                "more-toggle"
            })
            .expect("last control");
        assert!(switch.left() >= brand.right(), "{fit:?}: {switch:?}");
        assert!(last.right() <= px(width), "{fit:?}: {last:?}");
        assert!(strip.contains(&last.center()), "{fit:?}: one row");
        assert!(switch.size.height >= px(24.), "{fit:?}: {switch:?}");

        // The switch keeps its words until 64 rem.
        if zoom == 0 && width >= 1100. {
            labelled_width = Some(switch.size.width);
        } else if zoom == 0 {
            let labelled = labelled_width.expect("a labelled switch");
            assert!(switch.size.width < labelled, "glyph alone at {width}");
        }
    }
}

#[gpui_kit::test]
fn more_holds_the_folded_controls_and_takes_the_keys(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1100.), px(700.)));
    update(&view, cx, |app, _| app.cpu_threads = 18);
    let hidden = read(&view, cx, |app| app.options.include_hidden);
    let epoch = read(&view, cx, |app| app.scan_epoch);

    click_on(cx, "more-toggle");
    assert_eq!(read(&view, cx, |app| app.more_menu), Some(0));
    assert!(cx.debug_bounds("more-menu").is_some());
    // Hidden files rescans, as its checkbox does.
    press(cx, "enter");
    read(&view, cx, |app| {
        assert_eq!(app.options.include_hidden, !hidden);
        assert_eq!(app.scan_epoch, epoch + 1);
        assert!(app.more_menu.is_none());
    });
    update(&view, cx, |app, _| {
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
    });

    // Scan workers opens the power menu in ⋮'s place; ⋮ closes it again.
    click_on(cx, "more-toggle");
    press(cx, "end");
    press(cx, "enter");
    assert!(read(&view, cx, |app| app.power_menu.is_some()));
    assert!(cx.debug_bounds("power-menu").is_some());
    // A real press arrives after the pointer is on ⋮.
    let more = cx.debug_bounds("more-toggle").expect("more");
    cx.simulate_mouse_move(more.center(), None, gpui_kit::Modifiers::none());
    click_on(cx, "more-toggle");
    read(&view, cx, |app| {
        assert!(app.power_menu.is_none());
        assert!(app.more_menu.is_none());
    });

    // Escape closes without acting; a screen key closes and still acts.
    click_on(cx, "more-toggle");
    press(cx, "escape");
    assert!(read(&view, cx, |app| app.more_menu.is_none()));
    click_on(cx, "more-toggle");
    press(cx, "?");
    assert!(read(&view, cx, |app| app.more_menu.is_none() && app.show_help));
}

#[gpui_kit::test]
fn more_steps_depth_in_place_when_the_stepper_folds(cx: &mut TestAppContext) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(900.), px(600.)));
    press(cx, "ctrl-=");
    press(cx, "ctrl-=");
    let depth = read(&view, cx, |app| app.layout_options.max_depth);
    click_on(cx, "more-toggle");
    // Nowhere to go back or forward yet, so ⋮ opens on Depth, past the
    // disabled history rows; the up arrow does not land on them either.
    assert_eq!(read(&view, cx, |app| app.more_menu), Some(2));
    press(cx, "up");
    assert_eq!(read(&view, cx, |app| app.more_menu), Some(2));
    // The row stays open while the arrows step it.
    press(cx, "left");
    assert_eq!(
        read(&view, cx, |app| app.layout_options.max_depth),
        depth - 1
    );
    assert_eq!(read(&view, cx, |app| app.more_menu), Some(2));
    click_on(cx, "more-depth-more");
    assert_eq!(read(&view, cx, |app| app.layout_options.max_depth), depth);
}

/// The names the mosaic lays out, top to bottom of the tree.
fn laid_out(view: &Entity<Disktree>, cx: &mut Window) -> Vec<String> {
    update(view, cx, |app, _| {
        let tiles = app.layout().map(<[Tile]>::to_vec).unwrap_or_default();
        tiles
            .iter()
            .filter_map(|tile| app.node_at(tile.crumbs()))
            .map(|node| node.name().to_string())
            .collect()
    })
}

#[gpui_kit::test]
fn a_legend_chip_shows_only_then_hides_its_kind(cx: &mut TestAppContext) {
    use crate::legend::{Chip, ChipId};

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
    draw(cx);
    assert!(
        cx.debug_bounds("legend-clear").is_none(),
        "nothing filtered"
    );

    // Reclaimable, once: only `.cache`, the fixture's one regenerable tree.
    click_on(cx, "legend-reclaimable");
    cx.run_until_parked();
    let chip = read(&view, cx, |app| app.legend.get(ChipId::Reclaimable));
    assert_eq!(chip, Chip::Only);
    let drawn = laid_out(&view, cx);
    assert!(drawn.contains(&".cache".to_string()), "{drawn:?}");
    assert!(
        !drawn.iter().any(|name| name == "junk" || name == "keep"),
        "{drawn:?}"
    );
    // The totals line says the share on screen, and Show all appears.
    let shown = read(&view, cx, |app| {
        app.legend_matches.as_deref().map(|matches| matches.bytes)
    });
    assert_eq!(shown, Some(300_000));
    assert!(cx.debug_bounds("legend-clear").is_some());

    // Again: hidden, so everything but `.cache`.
    click_on(cx, "legend-reclaimable");
    cx.run_until_parked();
    let drawn = laid_out(&view, cx);
    assert!(!drawn.contains(&".cache".to_string()), "{drawn:?}");
    assert!(drawn.contains(&"junk".to_string()), "{drawn:?}");

    // Show all gives everything back, and keys reach the mosaic again.
    click_on(cx, "legend-clear");
    cx.run_until_parked();
    assert!(!read(&view, cx, |app| app.legend.is_active()));
    assert!(cx.update(|window, cx| view.read(cx).focus.is_focused(window)));
    let drawn = laid_out(&view, cx);
    assert!(
        drawn.contains(&".cache".to_string())
            && drawn.contains(&"junk".to_string())
    );

    // A kind set to only, cleared by Escape.
    update(&view, cx, |app, cx| app.cycle_legend(ChipId::Kind(7), cx));
    cx.run_until_parked();
    let drawn = laid_out(&view, cx);
    assert!(drawn.contains(&".cache".to_string()), "Cache: {drawn:?}");
    assert!(!drawn.contains(&"keep".to_string()), "{drawn:?}");
    press(cx, "escape");
    cx.run_until_parked();
    assert!(!read(&view, cx, |app| app.legend.is_active()));
}

#[gpui_kit::test]
fn the_legend_filter_follows_into_folders_and_into_a_search(
    cx: &mut TestAppContext,
) {
    use crate::legend::ChipId;

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
    draw(cx);
    // Hide Cache: `.cache` goes, and a search for blobs finds only junk's.
    update(&view, cx, |app, cx| {
        app.cycle_legend(ChipId::Kind(7), cx);
        app.cycle_legend(ChipId::Kind(7), cx);
    });
    cx.run_until_parked();
    press(cx, "/");
    cx.simulate_input("blob");
    cx.run_until_parked();
    let count = read(&view, cx, |app| app.matches.as_deref().map(|m| m.count));
    assert_eq!(count, Some(1), "the cached blob is hidden");
    press(cx, "escape");
    cx.run_until_parked();

    // Descending keeps the filter: no search runs, the root's answer holds.
    let junk = read(&view, cx, |app| {
        app.crumbs_for_path(&temp.path().join("junk"))
            .expect("junk")
    });
    update(&view, cx, |app, cx| app.go_to(junk, cx));
    draw(cx);
    assert!(read(&view, cx, |app| app.legend.is_active()));
    let drawn = laid_out(&view, cx);
    assert!(drawn.contains(&"blob.bin".to_string()), "{drawn:?}");
}

#[gpui_kit::test]
fn marking_a_partly_shown_folder_says_it_takes_all_of_it(
    cx: &mut TestAppContext,
) {
    use crate::legend::ChipId;

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
    draw(cx);
    update(&view, cx, |app, cx| {
        app.cycle_legend(ChipId::Reclaimable, cx);
        app.cycle_legend(ChipId::Reclaimable, cx);
    });
    cx.run_until_parked();
    // The scanned root holds `.cache`, which the filter hides.
    update(&view, cx, |app, _| app.selected = Some(Vec::new()));
    draw(cx);
    assert!(cx.debug_bounds("selection-partly-shown").is_some());
    // A folder that passes whole needs no such note.
    let junk = read(&view, cx, |app| {
        app.crumbs_for_path(&temp.path().join("junk"))
            .expect("junk")
    });
    update(&view, cx, |app, _| app.selected = Some(junk));
    draw(cx);
    assert!(cx.debug_bounds("selection-partly-shown").is_none());
}

/// Every chip is a filter, so none may be clipped away: the legend wraps
/// instead, at any width and text size, in both colour modes.
#[gpui_kit::test]
fn every_legend_chip_stays_inside_the_window(cx: &mut TestAppContext) {
    use crate::legend::ChipId;
    use disktree_core::classify::Category;

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    for age in [false, true] {
        update(&view, cx, |app, cx| {
            app.color_mode = if age {
                crate::state::ColorMode::Age
            } else {
                crate::state::ColorMode::Kind
            };
            cx.notify();
        });
        let chips: Vec<ChipId> = if age {
            (0..crate::palette::AGE_BUCKETS.len())
                .map(ChipId::Age)
                .collect()
        } else {
            (0..Category::LEGEND.len()).map(ChipId::Kind).collect()
        };
        for width in [900., 1100., 1440.] {
            for rem in [16., 24., 32.] {
                cx.simulate_resize(gpui_kit::size(px(width), px(900.)));
                cx.update(|window, _| window.set_rem_size(px(rem)));
                draw(cx);
                let legend = cx.debug_bounds("legend").expect("legend");
                for id in chips.iter().copied().chain([ChipId::Reclaimable]) {
                    let at = format!("{id:?} at {width} rem {rem}");
                    let key = Box::leak(id.key().into_boxed_str());
                    let chip = cx.debug_bounds(key).expect(&at);
                    assert!(
                        chip.right() <= px(width) + px(0.5)
                            && chip.left() >= px(0.)
                            && chip.size.width > px(0.),
                        "{at}: {chip:?}"
                    );
                    // Clipped chips used to run on past the legend's edge,
                    // under the side panel, while still inside the window.
                    assert!(
                        chip.right() <= legend.right() + px(0.5)
                            && chip.bottom() <= legend.bottom() + px(0.5),
                        "{at}: {chip:?} inside the legend {legend:?}"
                    );
                }
            }
        }
    }
}

/// A filter that keeps nothing in the folder on screen says so, says
/// whether anything elsewhere passes, and offers Show all, in the mosaic
/// and in List view alike.
#[gpui_kit::test]
fn a_legend_filter_that_keeps_nothing_says_so(cx: &mut TestAppContext) {
    use crate::legend::ChipId;

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
    draw(cx);
    assert!(cx.debug_bounds("legend-empty").is_none());

    // Reclaimable toolchains: the fixture's only reclaimable tree is a
    // cache, so nothing anywhere passes.
    update(&view, cx, |app, cx| {
        app.cycle_legend(ChipId::Reclaimable, cx);
        app.cycle_legend(ChipId::Kind(2), cx);
    });
    cx.run_until_parked();
    draw(cx);
    assert_eq!(read(&view, cx, Disktree::legend_empty_here), Some((0, 0)));
    assert!(cx.debug_bounds("legend-empty").is_some(), "in the mosaic");
    update(&view, cx, |app, cx| {
        app.file_list = true;
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("legend-empty").is_some(), "in List view");
    assert!(
        cx.debug_bounds("legend-empty-status").is_some(),
        "announced"
    );
    // Compact List view draws no legend, so its header says what filters.
    let note = read(&view, cx, Disktree::list_filter_note);
    assert!(note.contains("reclaimable Toolchains"), "{note}");
    click_on(cx, "legend-empty-clear");
    cx.run_until_parked();
    draw(cx);
    assert!(!read(&view, cx, |app| app.legend.is_active()));
    assert!(cx.debug_bounds("legend-empty").is_none());

    // Reclaimable alone, from inside `keep`: nothing here, but `.cache`
    // passes elsewhere.
    let keep = read(&view, cx, |app| {
        app.crumbs_for_path(&temp.path().join("keep"))
            .expect("keep")
    });
    update(&view, cx, |app, cx| {
        app.file_list = false;
        app.crumbs = keep;
        app.selected = None;
        app.cycle_legend(ChipId::Reclaimable, cx);
    });
    cx.run_until_parked();
    draw(cx);
    assert_eq!(
        read(&view, cx, Disktree::legend_empty_here),
        Some((300_000, 1))
    );
    assert!(cx.debug_bounds("legend-empty").is_some());
}

/// The age chips judge by the scan's clock, as the age colours do, and
/// a partly shown folder's row in List view says what share passed.
#[gpui_kit::test]
fn the_age_filter_keeps_the_scan_clock_and_lists_partial_shares(
    cx: &mut TestAppContext,
) {
    use crate::legend::ChipId;

    cx.update(init);
    let temp = fixture();
    // Two years old: in the Older bucket, unlike everything else.
    let old = std::time::SystemTime::now()
        - std::time::Duration::from_hours(800 * 24);
    std::fs::File::options()
        .write(true)
        .open(temp.path().join("junk/deeper/more.bin"))
        .and_then(|file| file.set_modified(old))
        .expect("age the file");
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.file_list = true;
        app.scanned_at -= 3 * 86_400;
        app.cycle_legend(ChipId::Age(4), cx);
    });
    cx.run_until_parked();
    let (now, scanned) = read(&view, cx, |app| {
        (app.legend_rule().map(|rule| rule.now), app.scanned_at)
    });
    assert_eq!(now, Some(scanned), "ages measured from the scan");

    let junk = read(&view, cx, |app| {
        app.crumbs_for_path(&temp.path().join("junk"))
            .expect("junk")
    });
    let size = read(&view, cx, |app| {
        app.entry_size(&junk, app.node_at(&junk).expect("junk"))
    });
    assert!(size.contains(" of "), "partly shown: {size}");
    let keep = read(&view, cx, |app| {
        app.crumbs_for_path(&temp.path().join("keep"))
            .expect("keep")
    });
    let listed = read(&view, cx, Disktree::file_entries);
    assert!(
        !listed.contains(keep.last().expect("a child")),
        "{listed:?}"
    );
}

/// Switching size and file count reorders every folder, so the legend's
/// result, keyed by position, is dropped at once and worked out again.
#[gpui_kit::test]
fn switching_the_metric_recomputes_the_legend(cx: &mut TestAppContext) {
    use crate::legend::ChipId;

    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
    draw(cx);
    update(&view, cx, |app, cx| {
        app.cycle_legend(ChipId::Reclaimable, cx);
    });
    cx.run_until_parked();
    assert!(read(&view, cx, |app| app.legend_matches.is_some()));
    update(&view, cx, |app, cx| {
        app.toggle_metric(cx);
        assert!(app.legend_matches.is_none(), "old positions dropped");
    });
    cx.run_until_parked();
    let drawn = laid_out(&view, cx);
    assert!(drawn.contains(&".cache".to_string()), "{drawn:?}");
    assert!(!drawn.contains(&"junk".to_string()), "{drawn:?}");
}

#[gpui_kit::test]
fn the_marked_total_is_pinned_above_the_disk_with_the_review(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
    draw(cx);
    assert!(cx.debug_bounds("marked-total").is_none(), "nothing marked");

    for name in ["junk", ".cache"] {
        let crumbs = read(&view, cx, |app| {
            app.crumbs_for_path(&temp.path().join(name))
                .expect("present")
        });
        update(&view, cx, |app, cx| app.toggle_mark(&crumbs, cx));
    }
    draw(cx);
    let total = cx.debug_bounds("marked-total").expect("marked total drawn");
    let disk = cx.debug_bounds("disk-section").expect("disk drawn");
    let scroll = cx.debug_bounds("side-panel-scroll").expect("lists drawn");
    assert!(total.bottom() <= disk.top() + px(0.5), "{total:?} {disk:?}");
    assert!(
        scroll.bottom() <= total.top() + px(0.5),
        "pinned below lists"
    );
    // The list leads the scroll, so on a short window it is suggestions
    // that scroll out of sight, not what is marked.
    let row = cx.debug_bounds("marked-row-0").expect("marked row drawn");
    let worth = cx.debug_bounds("worth-section").expect("worth drawn");
    assert!(row.bottom() <= worth.top(), "{row:?} {worth:?}");
    assert!(row.bottom() <= scroll.bottom(), "{row:?} {scroll:?}");
    // Without clones what is marked is what is freed, said once.
    assert!(cx.debug_bounds("marked-frees").is_some());
    assert!(cx.debug_bounds("marked-split").is_none());
    let review = cx.debug_bounds("review").expect("review button");
    assert!(
        total.contains(&review.center()),
        "the review is in the block"
    );
    click_on(cx, "review");
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Review);
}

/// The fixture on a pretend 1 MB disk, with `junk` (300 kB, 30% of it)
/// selected: past the default 5% limit.
fn over_limit_view<'a>(
    temp: &Path,
    cx: &'a mut TestAppContext,
) -> (Entity<Disktree>, &'a mut Window, Vec<usize>) {
    let (view, cx) = view_over(temp, cx);
    let junk = update(&view, cx, |app, cx| {
        app.space = Some(SpaceInfo {
            total: 1_000_000,
            free: 100_000,
            available: 100_000,
        });
        let junk = app
            .tree()
            .and_then(|tree| tree.find("junk").map(|(crumbs, _)| crumbs))
            .expect("the junk directory");
        app.select(Some(junk.clone()), cx);
        junk
    });
    draw(cx);
    (view, cx, junk)
}

/// Past the limit, Space marks nothing; only the chevron's Mark anyway
/// does, and Review then wants the phrase before anything is removed.
#[gpui_kit::test]
fn past_the_limit_marking_needs_mark_anyway_and_review_the_phrase(
    cx: &mut TestAppContext,
) {
    cx.update(init);
    let temp = fixture();
    let (view, cx, _) = over_limit_view(temp.path(), cx);
    assert!(cx.debug_bounds("mark-guard").is_some());
    assert!(cx.debug_bounds("inspector-mark-more").is_some());

    press(cx, "space");
    read(&view, cx, |app| {
        assert!(app.marks.is_empty());
        assert!(app.notice.as_ref().is_some_and(|(notice, _)| {
            notice.contains("30%") && notice.contains("5% limit")
        }));
    });

    click_on(cx, "inspector-mark-more");
    assert!(cx.debug_bounds("mark-menu").is_some());
    click_on(cx, "mark-anyway");
    read(&view, cx, |app| {
        assert_eq!(app.marks.len(), 1);
        assert_eq!(app.mark_menu, None);
    });

    update(&view, cx, |app, _| {
        app.removal_mode = RemovalMode::Permanent;
    });
    press(cx, "c");
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Review);
    assert!(cx.debug_bounds("over-limit-gate").is_some());
    assert!(cx.debug_bounds("over-limit-matched").is_none());

    // Enter commits nothing until the phrase is typed.
    press(cx, "enter");
    read(&view, cx, |app| {
        assert!(!app.confirm_open);
        assert!(app.run.is_none());
        assert!(app.notice.as_ref().is_some_and(|(notice, _)| {
            notice.contains("delete 30% of my disk")
        }));
    });

    // Close is not enough, and neither is the other mode's verb.
    click_on(cx, "over-limit-phrase");
    cx.simulate_input("trash 30% of my disk");
    draw(cx);
    assert!(cx.debug_bounds("over-limit-matched").is_none());
    let typed = "trash 30% of my disk".chars().count();
    cx.simulate_keystrokes(&vec!["backspace"; typed].join(" "));
    cx.simulate_input("Delete 30%  of my disk");
    draw(cx);
    assert!(cx.debug_bounds("over-limit-matched").is_some());
    // What the worker is held to is what was typed, not the 5% setting:
    // a share that grew past 30% since is refused there.
    update(&view, cx, |app, cx| {
        let plan = app.plan();
        assert_eq!(
            app.removal_limit(&plan, cx)
                .map(disktree_core::removal_limit::Limit::percent),
            Ok(30)
        );
    });

    // Enter in the field commits, and the permanent dialog still asks.
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert!(read(&view, cx, |app| app.confirm_open));
    assert!(
        temp.path().join("junk").exists(),
        "nothing has happened yet"
    );
    press(cx, "enter");
    assert_eq!(read(&view, cx, |app| app.screen), Screen::Running);
    let epoch = read(&view, cx, |app| app.run_epoch);
    let mut finished = false;
    for _ in 0..400 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        finished = update(&view, cx, |app, cx| {
            app.poll_removal_once(epoch, cx);
            app.screen == Screen::Done
        });
        if finished {
            break;
        }
    }
    assert!(finished, "the removal finished");
    assert!(!temp.path().join("junk").exists());
    assert!(temp.path().join("keep/notes.txt").exists());
    // The phrase confirmed that removal only: the next one asks again.
    read(&view, cx, |app| {
        assert!(app.over_phrase.is_none());
        assert_eq!(app.over_ticked, None);
    });
}

/// Settings › Safety relaxes the window, never an agent: Off gives the
/// plain Mark back, Tick a box asks for a tick, and an agent is still held
/// to 5%.
#[gpui_kit::test]
fn safety_settings_relax_the_window_but_never_an_agent(
    cx: &mut TestAppContext,
) {
    use crate::safety::{Confirm, save_confirm, save_limit};

    cx.update(init);
    let temp = fixture();
    let settings = tempfile::TempDir::new().expect("settings");
    save_confirm(Some(settings.path()), Confirm::Tick).expect("tick");
    let (view, cx, junk) = over_limit_view(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.removal_settings_dir = Some(settings.path().to_path_buf());
        app.safety_policy.replace(None);
        cx.notify();
    });
    draw(cx);
    assert!(cx.debug_bounds("inspector-mark-more").is_some());

    // Ticked for 30% to the trash, then the mode changes: the tick was
    // for the other one.
    update(&view, cx, |app, cx| {
        app.mark_anyway(&junk, cx);
        app.removal_mode = RemovalMode::Trash;
        app.open_review(cx);
    });
    draw(cx);
    assert!(cx.debug_bounds("over-limit-phrase").is_none());
    click_on(cx, "over-limit-tick");
    let met = |view: &Entity<Disktree>, cx: &mut Window| {
        update(view, cx, |app, cx| {
            let plan = app.plan();
            let share = app.removal_gate(&plan).expect("past the limit");
            app.gate_met(&share, cx)
        })
    };
    assert!(met(&view, cx));
    update(&view, cx, |app, _| {
        app.removal_mode = RemovalMode::Permanent;
    });
    assert!(!met(&view, cx), "a tick is for one mode");

    // Off: no gate in Review, the plain Mark in the inspector, and an
    // agent still held to 5%.
    save_limit(Some(settings.path()), None).expect("off");
    update(&view, cx, |app, cx| {
        app.safety_policy.replace(None);
        let plan = app.plan();
        assert_eq!(app.removal_gate(&plan), None);
        app.clear_marks(cx);
        app.screen = Screen::Explore;
        app.select(Some(junk.clone()), cx);
    });
    draw(cx);
    assert!(cx.debug_bounds("inspector-mark-more").is_none());
    assert!(cx.debug_bounds("mark-guard").is_none());
    update(&view, cx, |app, _| {
        let target = app.target_at(&junk).expect("junk");
        assert!(
            app.agent_mark_refusal(&target)
                .is_some_and(|reason| reason.contains("5% limit"))
        );
    });
    press(cx, "space");
    assert_eq!(read(&view, cx, |app| app.marks.len()), 1);
}
