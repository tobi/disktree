//! The menu bar and the application-wide shortcuts.
//!
//! GPUI adds neither on its own: without them a Mac app has an empty menu
//! bar and no ⌘Q. The layout is the Pwr family's (PwrSuite menus v1, as
//! PwrRoute, PwrGit and PwrAgnt have it): [App] · File · Edit · View · Go ·
//! Window · Help, with Go for back and forward as Finder has it. [`menus`]
//! is a pure template for either platform, so the tests can hold both to
//! the standard. macOS installs it as the global menu bar; Windows and
//! Linux have none, so the strip's menu button opens the same template as
//! one dropdown (`views::app_menu_dropdown`). Design: "Menus and Help
//! Windows" in the PwrDisk Claude Design project, turns 1 and 2.

use gpui_kit::{App, KeyBinding, Keystroke, Menu, MenuItem, SystemMenuType};

use crate::{help, settings};

pub use menu_actions::*;

#[allow(
    clippy::derive_partial_eq_without_eq,
    reason = "GPUI's actions! macro writes the derives, not us"
)]
mod menu_actions {
    use gpui_kit::actions;

    actions!(
        disktree,
        [
            /// Quit the application.
            Quit,
            /// Hide the application's windows.
            Hide,
            /// Hide every other application.
            HideOthers,
            /// Show every application again.
            ShowAll,
            /// Close the focused window. Closing the main one quits.
            CloseWindow,
            /// Minimize the focused window.
            Minimize,
            /// Zoom the focused window.
            Zoom,
            /// Scan the current root again.
            Rescan,
            /// Choose a directory to scan.
            OpenFolder,
            /// The local and SSH source chooser.
            ChooseSource,
            /// Reveal the tile a key acts on in Finder.
            ShowInFinder,
            /// Open Settings › Updates and check now.
            CheckForUpdates,
            /// Open Settings › About.
            About,
            /// Back through the directories visited.
            GoBack,
            /// Forward again, after going back.
            GoForward,
            /// Show how to connect an AI agent over MCP.
            ConnectAgent,
            /// Put back the marks the review screen's Unmark all took.
            Undo,
            /// Edit's rows: the focused text field's own keys, replayed
            /// (see [`replay`]).
            Cut,
            Copy,
            Paste,
            SelectAll,
            /// View: the main window's screens and its keyboard overlay.
            ShowExplore,
            ShowReview,
            ShowKeyboardShortcuts,
            /// View: the explore screen as a treemap or as a file list, the
            /// way Finder's ⌘1 and ⌘2 switch icons and list.
            ShowTreemap,
            ShowFileList,
            /// View: interface zoom, the window's own `ctrl =` / `ctrl -` /
            /// `ctrl 0`.
            ZoomIn,
            ZoomOut,
            ActualSize,
            /// Off macOS: open or close the strip's menu (Alt or F10).
            ToggleAppMenu,
        ]
    );
}

/// The key context no element ever has. Edit's and zoom's shortcuts are
/// bound in it so the menu shows ⌘C and ⌘= while the keys themselves still
/// reach the focused text field and the window's own key handler, which
/// read raw keystrokes. Bound anywhere a window has, ⌘V would become the
/// Paste action and the field would never see it.
const MENU_ONLY: &str = "MenuShortcutOnly";

/// Which menu bar to build: macOS's, or the one Windows and Linux use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Mac,
    Other,
}

impl Platform {
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Other
        }
    }

    /// The modifier its shortcuts take.
    const fn modifier(self) -> &'static str {
        match self {
            Self::Mac => "cmd",
            Self::Other => "ctrl",
        }
    }

    /// What the file manager is called, for Show in Finder.
    const fn file_manager(self) -> &'static str {
        // The Mac menu is checked first so its template reads the same
        // wherever the tests that build it run.
        if matches!(self, Self::Mac) {
            "Show in Finder"
        } else if cfg!(windows) {
            "Show in Explorer"
        } else {
            "Show in File Manager"
        }
    }
}

/// Install the menus, the shortcuts and the quit-on-close rule.
pub fn install(cx: &mut App) {
    // Closing the main window quits: a utility with nothing left to show
    // would leave a Dock icon with no window behind it. Settings, Help's
    // documents and Logs come and go without that.
    cx.on_window_closed(|cx, _| {
        let main_open = cx.windows().into_iter().any(|window| {
            window.downcast::<crate::state::Disktree>().is_some()
        });
        if !main_open {
            cx.quit();
        }
    })
    .detach();

    // A downloaded update is handed to Velopack by the quit hook
    // `updater::install` registers, which every route to quitting runs;
    // handing it off here as well started a second updater.
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &CloseWindow, cx| {
        in_front_window(cx, |window, _| window.remove_window());
    });
    cx.on_action(|_: &Minimize, cx| {
        in_front_window(cx, |window, _| window.minimize_window());
    });
    cx.on_action(|_: &Zoom, cx| {
        in_front_window(cx, |window, _| window.zoom_window());
    });
    cx.on_action(|_: &Cut, cx| replay("x", cx));
    cx.on_action(|_: &Copy, cx| replay("c", cx));
    cx.on_action(|_: &Paste, cx| replay("v", cx));
    cx.on_action(|_: &SelectAll, cx| replay("a", cx));
    cx.on_action(|_: &ZoomIn, cx| replay("=", cx));
    cx.on_action(|_: &ZoomOut, cx| replay("-", cx));
    cx.on_action(|_: &ActualSize, cx| replay("0", cx));
    cx.on_action(|_: &About, cx| {
        cx.defer(|cx| settings::open(settings::Pane::About, cx));
    });
    cx.on_action(|_: &CheckForUpdates, cx| {
        // The main window answers in a toast, from whichever window asked:
        // it comes forward so the answer is seen.
        cx.defer(|cx| {
            let Some(main) = main_window(cx) else {
                return;
            };
            let _ = main.update(cx, |this, window, cx| {
                window.activate_window();
                this.check_for_updates(cx);
            });
        });
    });
    // The main window's rows (Rescan, Open Folder, Choose Source, Show in
    // Finder, the screens and the history) are handled by that window,
    // which owns the scan and the selection; see `views::root`. From
    // another window they bring it forward first.
    for forward in MAIN_WINDOW_ACTIONS {
        forward(cx);
    }
    settings::install(cx);
    help::install(cx);

    let platform = Platform::current();
    cx.bind_keys(bindings(platform));
    if platform == Platform::Mac {
        cx.on_action(|_: &Hide, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        cx.set_menus(menus(platform));
    }
}

/// Actions the main window answers. Chosen while another PwrDisk window is
/// in front, each brings the main window forward and dispatches there, so
/// File › Rescan from the Logs window still rescans.
const MAIN_WINDOW_ACTIONS: [fn(&mut App); 12] = [
    |cx| forward_to_main::<Rescan>(cx),
    |cx| forward_to_main::<OpenFolder>(cx),
    |cx| forward_to_main::<ChooseSource>(cx),
    |cx| forward_to_main::<ShowInFinder>(cx),
    |cx| forward_to_main::<GoBack>(cx),
    |cx| forward_to_main::<GoForward>(cx),
    |cx| forward_to_main::<ShowExplore>(cx),
    |cx| forward_to_main::<ShowReview>(cx),
    |cx| forward_to_main::<ShowTreemap>(cx),
    |cx| forward_to_main::<ShowFileList>(cx),
    |cx| forward_to_main::<ShowKeyboardShortcuts>(cx),
    |cx| forward_to_main::<ConnectAgent>(cx),
];

/// An app-level fallback for `A`: it only runs when no element in the
/// window in front handled it, which is when that window is not the main
/// one.
fn forward_to_main<A: gpui_kit::Action + Default>(cx: &mut App) {
    cx.on_action(|_: &A, cx| {
        cx.defer(|cx| {
            let Some(main) = main_window(cx) else {
                return;
            };
            let _ = main.update(cx, |_, window, cx| {
                window.activate_window();
                window.dispatch_action(Box::new(A::default()), cx);
            });
        });
    });
}

/// The main window, which owns the scan, the marks and the toasts.
fn main_window(
    cx: &App,
) -> Option<gpui_kit::WindowHandle<crate::state::Disktree>> {
    cx.windows()
        .into_iter()
        .find_map(|window| window.downcast::<crate::state::Disktree>())
}

/// Every shortcut for `platform`.
fn bindings(platform: Platform) -> Vec<KeyBinding> {
    let modifier = platform.modifier();
    let key = |chord: &str| format!("{modifier}-{chord}");
    let mut bindings = vec![
        KeyBinding::new(&key("q"), Quit, None),
        KeyBinding::new(&key("w"), CloseWindow, None),
        KeyBinding::new(&key("m"), Minimize, None),
        KeyBinding::new(&key("o"), OpenFolder, None),
        KeyBinding::new(&key("shift-o"), ChooseSource, None),
        KeyBinding::new(&key("r"), Rescan, None),
        KeyBinding::new(&key("1"), ShowTreemap, None),
        KeyBinding::new(&key("2"), ShowFileList, None),
        // ⌘R is taken by Rescan, as in a browser; `o` reveals without a
        // modifier too.
        KeyBinding::new(&key("shift-r"), ShowInFinder, None),
        KeyBinding::new(&key(","), settings::OpenSettings, None),
        // In the main window only, where the marks are; a text field's own
        // binding is deeper, so it still undoes its typing.
        KeyBinding::new(&key("z"), Undo, Some("Disktree")),
        KeyBinding::new(&key("x"), Cut, Some(MENU_ONLY)),
        KeyBinding::new(&key("c"), Copy, Some(MENU_ONLY)),
        KeyBinding::new(&key("v"), Paste, Some(MENU_ONLY)),
        KeyBinding::new(&key("a"), SelectAll, Some(MENU_ONLY)),
        KeyBinding::new(&key("="), ZoomIn, Some(MENU_ONLY)),
        KeyBinding::new(&key("-"), ZoomOut, Some(MENU_ONLY)),
        KeyBinding::new(&key("0"), ActualSize, Some(MENU_ONLY)),
        // The keyboard overlay's own key, shown beside its row.
        KeyBinding::new("?", ShowKeyboardShortcuts, Some(MENU_ONLY)),
    ];
    match platform {
        Platform::Mac => bindings.extend([
            KeyBinding::new("cmd-h", Hide, None),
            KeyBinding::new("alt-cmd-h", HideOthers, None),
            // Finder's and Safari's back and forward; alt-arrows work too.
            KeyBinding::new("cmd-[", GoBack, None),
            KeyBinding::new("cmd-]", GoForward, None),
        ]),
        Platform::Other => bindings.extend([
            KeyBinding::new("f5", Rescan, None),
            KeyBinding::new("f10", ToggleAppMenu, None),
            // A lone Alt: GPUI's modifier-only keystroke, pressed and
            // released with nothing else.
            KeyBinding::new("alt", ToggleAppMenu, None),
        ]),
    }
    bindings
}

/// Edit's and View's zoom rows: the same key, sent to the window in front
/// as if pressed. The text fields handle ⌘X/⌘C/⌘V/⌘A (Ctrl elsewhere)
/// themselves and the main window its zoom keys, so a menu click does
/// exactly what the shortcut does. The keys are bound only in
/// [`MENU_ONLY`], so this never comes back here.
fn replay(key: &str, cx: &mut App) {
    let modifier = Platform::current().modifier();
    let Ok(keystroke) = Keystroke::parse(&format!("{modifier}-{key}")) else {
        return;
    };
    in_front_window(cx, move |window, cx| {
        window.dispatch_keystroke(keystroke, cx);
    });
}

/// Runs `f` on the window in front, once the action that asked is done:
/// GPUI can still hold the window an action was dispatched in, and an
/// update inside that dispatch fails without a word.
fn in_front_window(
    cx: &mut App,
    f: impl FnOnce(&mut gpui_kit::Window, &mut App) + 'static,
) {
    let Some(window) = cx.active_window() else {
        return;
    };
    cx.defer(move |cx| {
        let _ = window.update(cx, |_, window, cx| f(window, cx));
    });
}

/// The menu bar, in the family's order, for `platform`.
pub fn menus(platform: Platform) -> Vec<Menu> {
    let mac = platform == Platform::Mac;
    let mut menus = Vec::new();
    if mac {
        menus.push(Menu::new("PwrDisk").items([
            MenuItem::action("About PwrDisk", About),
            MenuItem::action("Check for Updates\u{2026}", CheckForUpdates),
            MenuItem::separator(),
            MenuItem::action("Settings\u{2026}", settings::OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide PwrDisk", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit PwrDisk", Quit),
        ]));
    }

    let mut file = vec![
        MenuItem::action("Open Folder\u{2026}", OpenFolder),
        MenuItem::action("Choose Source\u{2026}", ChooseSource),
        MenuItem::separator(),
        MenuItem::action(platform.file_manager(), ShowInFinder),
        MenuItem::action("Rescan", Rescan),
        MenuItem::separator(),
    ];
    if !mac {
        file.push(MenuItem::action("Settings\u{2026}", settings::OpenSettings));
        file.push(MenuItem::separator());
    }
    file.push(MenuItem::action("Close Window", CloseWindow));
    if !mac {
        file.push(MenuItem::action("Quit", Quit));
    }
    menus.push(Menu::new("File").items(file));

    // Undo is Unmark all's; no field here keeps a history, so no Redo.
    menus.push(Menu::new("Edit").items([
        MenuItem::action("Undo", Undo),
        MenuItem::separator(),
        MenuItem::action("Cut", Cut),
        MenuItem::action("Copy", Copy),
        MenuItem::action("Paste", Paste),
        MenuItem::separator(),
        MenuItem::action("Select All", SelectAll),
    ]));

    // The app's own screens open View; then interface zoom, which PwrDisk
    // has (a GPUI app usually does not). macOS adds Enter Full Screen.
    menus.push(Menu::new("View").items([
        MenuItem::action("Explore", ShowExplore),
        MenuItem::action("Review Marked", ShowReview),
        MenuItem::action("Keyboard Shortcuts", ShowKeyboardShortcuts),
        MenuItem::separator(),
        MenuItem::action("as Treemap", ShowTreemap),
        MenuItem::action("as File List", ShowFileList),
        MenuItem::separator(),
        MenuItem::action("Zoom In", ZoomIn),
        MenuItem::action("Zoom Out", ZoomOut),
        MenuItem::action("Actual Size", ActualSize),
    ]));

    menus.push(Menu::new("Go").items([
        MenuItem::action("Back", GoBack),
        MenuItem::action("Forward", GoForward),
    ]));

    // macOS lists the open windows under these itself.
    let mut window = vec![MenuItem::action("Minimize", Minimize)];
    if mac {
        window.push(MenuItem::action("Zoom", Zoom));
    }
    menus.push(Menu::new("Window").items(window));

    menus.push(Menu::new("Help").items(help_items(platform)));
    menus
}

/// Help, in the standard's fixed order: learn, get help, the project, the
/// legal notices; off macOS, Check for Updates… and About last. No Website
/// row: PwrDisk has no site yet, and no Replay Onboarding: it has none
/// (every row has something behind it).
fn help_items(platform: Platform) -> Vec<MenuItem> {
    let mut items = vec![
        MenuItem::action("PwrDisk Documentation", help::OpenDocumentation),
        MenuItem::action("Changelog", help::OpenChangelog),
        // Where a person looks for "how do I hook this up"; the same
        // overlay opens from the line under the disk meter.
        MenuItem::action("Work with an AI Agent\u{2026}", ConnectAgent),
        MenuItem::separator(),
        MenuItem::action("Report an Issue\u{2026}", help::ReportIssue),
        MenuItem::action(
            "Report a Security Vulnerability\u{2026}",
            help::ReportVulnerability,
        ),
        MenuItem::action("Copy Diagnostics Info", help::CopyDiagnostics),
        MenuItem::action("Logs", help::OpenLogs),
        MenuItem::separator(),
        MenuItem::action("View Source", help::ViewSource),
        MenuItem::separator(),
        MenuItem::action("View License", help::ViewLicense),
        MenuItem::action("Third-Party Notices", help::ThirdPartyNotices),
    ];
    if platform == Platform::Other {
        items.push(MenuItem::separator());
        items.push(MenuItem::action(
            "Check for Updates\u{2026}",
            CheckForUpdates,
        ));
        items.push(MenuItem::action("About PwrDisk", About));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A menu as its labels, with "---" for a separator and "<Services>"
    /// for the OS's submenu.
    fn labels(menu: &Menu) -> Vec<String> {
        menu.items
            .iter()
            .map(|item| match item {
                MenuItem::Separator => "---".to_owned(),
                MenuItem::Action { name, .. } => name.to_string(),
                MenuItem::Submenu(menu) => format!("[{}]", menu.name),
                MenuItem::SystemMenu(menu) => format!("<{}>", menu.name),
            })
            .collect()
    }

    fn menu<'a>(menus: &'a [Menu], name: &str) -> &'a Menu {
        menus
            .iter()
            .find(|menu| menu.name == name)
            .unwrap_or_else(|| panic!("no {name} menu"))
    }

    #[test]
    fn the_menu_bar_follows_the_family_order() {
        let names = |platform| {
            menus(platform)
                .iter()
                .map(|menu| menu.name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(Platform::Mac),
            ["PwrDisk", "File", "Edit", "View", "Go", "Window", "Help"]
        );
        assert_eq!(
            names(Platform::Other),
            ["File", "Edit", "View", "Go", "Window", "Help"]
        );
    }

    #[test]
    fn the_mac_app_menu_opens_with_about_and_updates() {
        let menus = menus(Platform::Mac);
        assert_eq!(
            labels(menu(&menus, "PwrDisk")),
            [
                "About PwrDisk",
                "Check for Updates…",
                "---",
                "Settings…",
                "---",
                "<Services>",
                "---",
                "Hide PwrDisk",
                "Hide Others",
                "Show All",
                "---",
                "Quit PwrDisk",
            ]
        );
        assert_eq!(
            labels(menu(&menus, "File")),
            [
                "Open Folder…",
                "Choose Source…",
                "---",
                "Show in Finder",
                "Rescan",
                "---",
                "Close Window"
            ]
        );
        assert_eq!(labels(menu(&menus, "Window")), ["Minimize", "Zoom"]);
    }

    #[test]
    fn elsewhere_settings_is_in_file_and_about_ends_help() {
        let menus = menus(Platform::Other);
        let file = labels(menu(&menus, "File"));
        assert_eq!(
            file[file.len() - 5..],
            ["---", "Settings…", "---", "Close Window", "Quit"]
        );
        let help = labels(menu(&menus, "Help"));
        assert_eq!(
            help[help.len() - 3..],
            ["---", "Check for Updates…", "About PwrDisk"]
        );
        assert_eq!(labels(menu(&menus, "Window")), ["Minimize"]);
    }

    #[test]
    fn help_is_in_the_standard_order_on_both_platforms() {
        let expected = [
            "PwrDisk Documentation",
            "Changelog",
            "Work with an AI Agent\u{2026}",
            "---",
            "Report an Issue…",
            "Report a Security Vulnerability…",
            "Copy Diagnostics Info",
            "Logs",
            "---",
            "View Source",
            "---",
            "View License",
            "Third-Party Notices",
        ];
        for platform in [Platform::Mac, Platform::Other] {
            let menus = menus(platform);
            let help = labels(menu(&menus, "Help"));
            assert_eq!(help[..expected.len()], expected, "{platform:?}");
        }
        assert_eq!(
            labels(menu(&menus(Platform::Mac), "Help")).len(),
            expected.len()
        );
    }

    #[test]
    fn view_lists_the_screens_and_zoom_and_edit_has_no_undo() {
        for platform in [Platform::Mac, Platform::Other] {
            let menus = menus(platform);
            assert_eq!(
                labels(menu(&menus, "View")),
                [
                    "Explore",
                    "Review Marked",
                    "Keyboard Shortcuts",
                    "---",
                    "as Treemap",
                    "as File List",
                    "---",
                    "Zoom In",
                    "Zoom Out",
                    "Actual Size"
                ]
            );
            assert_eq!(
                labels(menu(&menus, "Edit")),
                ["Undo", "---", "Cut", "Copy", "Paste", "---", "Select All"]
            );
            assert_eq!(labels(menu(&menus, "Go")), ["Back", "Forward"]);
        }
    }

    #[test]
    fn every_row_but_the_services_menu_has_an_action() {
        for platform in [Platform::Mac, Platform::Other] {
            for menu in menus(platform) {
                for item in &menu.items {
                    assert!(
                        matches!(
                            item,
                            MenuItem::Separator
                                | MenuItem::Action { .. }
                                | MenuItem::SystemMenu(_)
                        ),
                        "{} has a submenu",
                        menu.name
                    );
                }
            }
        }
    }
}
