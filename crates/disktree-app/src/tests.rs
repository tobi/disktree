//! End-to-end tests through the real window harness.
//!
//! These drive the application the way a person does — draw a frame, press
//! keys, type — so they catch what unit tests on the state cannot: a screen
//! that panics while painting, a binding that never fires, a removal that
//! reports success without removing anything.

use std::path::Path;

use disktree_core::removal::RemovalMode;
use disktree_core::scan::{ScanOptions, scan};
use disktree_core::treemap::Tile;
use gpui_kit::{
    Bounds, Context, Entity, Pixels, Point, TestAppContext, VisualTestContext,
    px,
};
use gpui_omarchy::Theme;

use crate::state::{Disktree, Screen};

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
        Disktree::with_tree(root_path.clone(), tree, options(), 3, cx)
    });
    let focus = view.read_with(cx, |app, _| app.focus.clone());
    cx.update(|window, cx| window.focus(&focus, cx));
    (view, cx)
}

fn draw(cx: &mut Window) {
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
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
fn the_window_draws_a_treemap_with_tiles(cx: &mut TestAppContext) {
    cx.update(gpui_omarchy::init);
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
    cx.update(gpui_omarchy::init);
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

    let epoch = read(&view, cx, |app| app.scan_epoch);
    let mut ready = false;
    for _ in 0..600 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        ready = update(&view, cx, |app, cx| {
            app.poll_scan_once(epoch, cx);
            app.tree().is_some()
        });
        if ready {
            break;
        }
    }
    assert!(ready, "the scan landed");
    draw(cx);

    let (tiles, selected, hidden_present) = update(&view, cx, |app, _| {
        (
            app.layout().map(<[Tile]>::len).unwrap_or_default(),
            app.selected.is_some(),
            app.tree().is_some_and(|tree| {
                tree.children.iter().any(|c| c.name.starts_with('.'))
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
    cx.update(gpui_omarchy::init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    // Largest first: .cache holds the biggest file.
    let (selected, name) = read(&view, cx, |app| {
        (
            app.selected.clone(),
            app.node_at(&[0]).map(|node| node.name.to_string()),
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
    cx.update(gpui_omarchy::init);
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
                tree.children
                    .iter()
                    .position(|child| &*child.name == "junk")
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

    let summary = read(&view, cx, |app| app.run_summary.clone());
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
    cx.update(gpui_omarchy::init);
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
    cx.update(gpui_omarchy::init);
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

#[gpui_kit::test]
fn typing_filters_live_and_enter_shows_only_the_matches(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_omarchy::init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);
    let names_drawn = |view: &Entity<Disktree>, cx: &mut Window| {
        update(view, cx, |app, _| {
            let tiles = app.layout().map(<[Tile]>::to_vec).unwrap_or_default();
            tiles
                .iter()
                .filter_map(|tile| app.node_at(tile.crumbs()))
                .map(|node| node.name.to_string())
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
            tree.children
                .iter()
                .position(|child| &*child.name == "keep")
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
    cx.update(gpui_omarchy::init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    draw(cx);

    press(cx, "?");
    assert!(read(&view, cx, |app| app.show_help));
    press(cx, "escape");
    assert!(!read(&view, cx, |app| app.show_help));
}

#[gpui_kit::test]
fn showing_a_tile_that_is_gone_says_so_instead(cx: &mut TestAppContext) {
    cx.update(gpui_omarchy::init);
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
    cx.update(gpui_omarchy::init);
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
    cx.update(gpui_omarchy::init);
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
    cx.update(gpui_omarchy::init);
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
    cx.update(gpui_omarchy::init);
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

/// The removal mode is a choice with two independent channels: the key and the
/// button both have to reach the same state.
#[gpui_kit::test]
fn the_review_screen_switches_removal_mode(cx: &mut TestAppContext) {
    cx.update(gpui_omarchy::init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.marks.toggle(disktree_core::removal::Target {
            path: temp.path().join("junk"),
            bytes: 300_000,
            is_dir: true,
            hidden: false,
        });
        app.screen = Screen::Review;
        cx.notify();
    });
    draw(cx);

    press(cx, "m");
    assert_eq!(read(&view, cx, |app| app.removal_mode), RemovalMode::Trash);
    press(cx, "p");
    assert_eq!(
        read(&view, cx, |app| app.removal_mode),
        RemovalMode::Permanent
    );
    press(cx, "!");
    assert!(read(&view, cx, |app| app.marks.is_empty()));
}

/// `a` on the review screen copies a prompt for an agent naming the marked
/// path, and removes nothing.
#[gpui_kit::test]
fn the_review_screen_copies_the_list_as_an_agent_prompt(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_omarchy::init);
    let temp = fixture();
    let junk = temp.path().join("junk");
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.marks.toggle(disktree_core::removal::Target {
            path: junk.clone(),
            bytes: 300_000,
            is_dir: true,
            hidden: false,
        });
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

/// Escape in the alert dialog cancels: the dialog closes, the review screen
/// stays, and nothing on disk changes.
#[gpui_kit::test]
fn escape_in_the_delete_dialog_cancels(cx: &mut TestAppContext) {
    cx.update(gpui_omarchy::init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    update(&view, cx, |app, cx| {
        app.removal_mode = RemovalMode::Permanent;
        app.marks.toggle(disktree_core::removal::Target {
            path: temp.path().join("junk"),
            bytes: 300_000,
            is_dir: true,
            hidden: false,
        });
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
    cx.update(gpui_omarchy::init);
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
    cx.update(gpui_omarchy::init);
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
        .children
        .iter()
        .position(|child| &*child.name == name)
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
    cx.update(gpui_omarchy::init);
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
    cx.update(gpui_omarchy::init);
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
    cx.update(gpui_omarchy::init);
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
    cx.update(gpui_omarchy::init);
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

/// Drive the scan the view started until its tree lands.
fn finish_scan(view: &Entity<Disktree>, cx: &mut Window) {
    let epoch = read(view, cx, |app| app.scan_epoch);
    for _ in 0..600 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        let ready = update(view, cx, |app, cx| {
            app.poll_scan_once(epoch, cx);
            app.tree().is_some()
        });
        if ready {
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

    cx.update(gpui_omarchy::init);
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

    cx.update(gpui_omarchy::init);
    let temp = fixture();
    let inner = temp.path().join("junk");
    let (view, cx) = view_over(&inner, cx);
    update(&view, cx, |app, _| {
        app.disk_root = Some(temp.path().to_path_buf());
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
    let (reused, rest, selected) = read(&view, cx, |app| {
        let tree = app.tree().expect("the wider tree");
        let junk = tree.child_named("junk").map(|node| node.files);
        let selected = app
            .selected
            .as_deref()
            .and_then(|crumbs| app.node_at(crumbs))
            .map(|node| node.name.to_string());
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
    cx.update(gpui_omarchy::init);
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

    cx.update(gpui_omarchy::init);
    let temp = fixture();
    let (view, cx) = view_over(temp.path(), cx);
    cx.simulate_resize(gpui_kit::size(px(1400.), px(900.)));
    draw(cx);
    // Into junk, so the trail ends in a crumb that has siblings.
    let junk = read(&view, cx, |app| {
        app.tree()
            .and_then(|tree| {
                tree.children.iter().position(|c| &*c.name == "junk")
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
                tree.children.iter().position(|c| &*c.name == ".cache")
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
    cx.update(gpui_omarchy::init);
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

    cx.update(gpui_omarchy::init);
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

/// Escape stops a first scan. What the walk found so far is not shown as a
/// tree, a late result is ignored, and `r` starts over.
#[gpui_kit::test]
fn escape_cancels_the_first_scan_and_r_starts_it_again(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_omarchy::init);
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
    cx.update(gpui_omarchy::init);
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
