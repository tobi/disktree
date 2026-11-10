//! Light or dark when there is no Omarchy theme to follow, and the face to
//! draw with when there is.
//!
//! On Omarchy, gpui-omarchy follows the desktop's theme files. Elsewhere there
//! are none, and gpui-omarchy would then stay on its dark default whatever
//! the system is set to; there the app follows the system appearance instead,
//! with the crate's own dark and light palettes, and switches when it does.
//!
//! The face splits the same way. Omarchy has one font setting — the monospace
//! family `omarchy font set` writes to fontconfig — and its shell draws
//! everything in it. gpui-omarchy exposes that family as `mono_font`; `font`
//! is GPUI's system UI face, which no Omarchy setting reaches. On Omarchy the
//! app therefore draws with `mono_font`, so the desktop's font choice reaches
//! it too; elsewhere it keeps the platform's UI face.
//!
//! Two things need that face: the text this app lays out itself, which reads
//! it through [`font`], and the controls gpui-omarchy draws — buttons,
//! checkboxes, text fields, dialogs, tooltips — which style themselves from
//! `Theme::font` and so need [`follow_desktop_face`] to rewrite it. Both
//! follow fontconfig while the app runs: `omarchy font set` only rewrites
//! `~/.config/fontconfig`, so that file is what is watched.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use gpui_kit::{App, SharedString, Window, WindowAppearance};
use gpui_omarchy::{ActiveTheme, Theme};
use notify::{RecursiveMode, Watcher as _};

/// Whether Omarchy's theme files are installed, and gpui-omarchy therefore
/// follows the desktop instead of the system appearance.
pub fn follows_omarchy(home: Option<&Path>) -> bool {
    home.is_some_and(|home| {
        [".local/state/omarchy/current", ".config/omarchy/current"]
            .iter()
            .any(|theme| home.join(theme).exists())
    })
}

/// Whether the system appearance, rather than an Omarchy theme, decides the
/// colours: unless an Omarchy theme is installed, on macOS and Windows, and
/// on a Linux desktop that states a preference.
pub fn follows_system(home: Option<&Path>) -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    states_a_preference(&desktop) && !follows_omarchy(home)
}

/// The family every label is drawn in.
///
/// Resolved once per process, like the theme's own `mono_font`: changing the
/// font takes effect on the next start, the same as the terminal.
pub fn font(theme: &Theme) -> SharedString {
    font_for(theme, on_omarchy())
}

fn font_for(theme: &Theme, on_omarchy: bool) -> SharedString {
    if on_omarchy {
        theme.mono_font.clone()
    } else {
        theme.font.clone()
    }
}

fn on_omarchy() -> bool {
    static ON_OMARCHY: OnceLock<bool> = OnceLock::new();
    *ON_OMARCHY.get_or_init(|| follows_omarchy(std::env::home_dir().as_deref()))
}

/// Point gpui-omarchy's own controls at the desktop face, and follow the font
/// from now on.
///
/// The crate draws buttons, checkboxes, text fields, dialogs and tooltips
/// itself, and each styles itself with `Theme::font`. On Omarchy that becomes
/// the family `omarchy font set` chose, so its controls match the text this
/// app lays out. `Theme::apply` resets `font` whenever the theme files change,
/// so an observer puts it back after every reload.
pub fn follow_desktop_face(cx: &mut App) {
    let omarchy = on_omarchy();
    follow_face(cx, omarchy);
    if omarchy {
        watch_desktop_face(cx);
    }
}

/// [`follow_desktop_face`] with the desktop decision left to the caller, and
/// without the watcher, so a test can exercise it on a machine that is not
/// Omarchy.
pub fn follow_face(cx: &mut App, on_omarchy: bool) {
    if !on_omarchy {
        return;
    }
    desktop_face(cx);
    cx.observe_global::<Theme>(desktop_face).detach();
}

/// Keeps the fontconfig watcher's task alive for as long as the app runs.
struct FaceWatcher {
    _task: gpui_kit::Task<()>,
}

impl gpui_kit::Global for FaceWatcher {}

/// Repaint when `omarchy font set` rewrites fontconfig.
///
/// `omarchy font set` writes `~/.config/fontconfig/fonts.conf` and restarts
/// the shell; it never touches the theme files gpui-omarchy watches, and
/// gpui-omarchy resolves `mono_font` once per process. Watching the directory
/// (not the file, which the rewrite replaces) and asking fontconfig again is
/// what lets the change land without a restart.
fn watch_desktop_face(cx: &mut App) {
    let Some(home) = std::env::home_dir() else {
        return;
    };
    let directory = home.join(".config/fontconfig");
    // A machine with no fontconfig directory yet has nothing to watch; its
    // first `omarchy font set` still needs a restart.
    if !directory.is_dir() {
        return;
    }
    let (sender, events) = async_channel::bounded(1);
    let watcher = notify::recommended_watcher(
        move |event: notify::Result<notify::Event>| match event {
            // `fc-match` reads the file this watches; reacting to access
            // would let each read schedule the next one.
            Ok(event) if event.kind.is_access() => {}
            // A rewrite arrives as several events, and one pending wake-up
            // is enough to handle all of them.
            Ok(_) => {
                let _ = sender.try_send(());
            }
            Err(error) => eprintln!("disktree font watcher error: {error}"),
        },
    );
    let mut watcher = match watcher {
        Ok(watcher) => watcher,
        Err(error) => {
            eprintln!("Cannot watch the Omarchy font: {error}");
            return;
        }
    };
    if let Err(error) = watcher.watch(&directory, RecursiveMode::Recursive) {
        eprintln!("Cannot watch the Omarchy font: {error}");
        return;
    }
    let task = cx.spawn(async move |cx| {
        // The task owns the watcher, and the global below owns the task.
        let _watcher = watcher;
        while events.recv().await.is_ok() {
            // `fonts.conf` is truncated before it is written, so let the
            // rewrite finish rather than reading it empty.
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            let face = cx
                .background_executor()
                .spawn(async { resolved_face() })
                .await;
            let Some(face) = face else { continue };
            cx.update(|cx| set_desktop_face(cx, face));
        }
    });
    cx.set_global(FaceWatcher { _task: task });
}

/// The family fontconfig resolves for `monospace` right now.
///
/// The same query gpui-omarchy makes once at startup, repeated so a change
/// can be picked up. `fc-match` lists aliases after the first family,
/// separated by commas.
#[cfg(all(unix, not(target_os = "macos")))]
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "%{family} is fontconfig's format, not a Rust one"
)]
fn resolved_face() -> Option<SharedString> {
    let output = std::process::Command::new("fc-match")
        .args(["monospace", "-f", "%{family}"])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let family = String::from_utf8(output.stdout).ok()?;
    let family = family.lines().next()?.split(',').next()?.trim();
    (!family.is_empty()).then(|| SharedString::from(family.to_owned()))
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn resolved_face() -> Option<SharedString> {
    None
}

/// Put `face` in the theme. The app's own text reads `mono_font` and
/// gpui-omarchy's controls read `font`, so both move together.
fn set_desktop_face(cx: &mut App, face: SharedString) {
    let mut theme = cx.omarchy().clone();
    if theme.font == face && theme.mono_font == face {
        return;
    }
    theme.mono_font = face.clone();
    theme.font = face;
    cx.set_global(theme);
    cx.refresh_windows();
}

/// After a theme reload, `Theme::apply` has put `font` and `mono_font` back to
/// their startup values. Asking fontconfig again, rather than copying the
/// stale field, is what keeps a font change through a colour change.
fn desktop_face(cx: &mut App) {
    // The rewrite notifies this observer a second time; equality ends that
    // pass without asking fontconfig again.
    if cx.omarchy().font == cx.omarchy().mono_font {
        return;
    }
    let face =
        resolved_face().unwrap_or_else(|| cx.omarchy().mono_font.clone());
    set_desktop_face(cx, face);
}

/// Whether the system has a light or dark setting to follow. macOS and
/// Windows always do. On Linux GPUI asks the desktop portal, which reads "no
/// preference" as light: a window manager that never sets one would turn
/// the app light, so only GNOME and KDE, whose settings do, are followed.
fn states_a_preference(desktop: &str) -> bool {
    cfg!(any(target_os = "macos", windows))
        || desktop.split(':').any(|name| {
            ["gnome", "kde", "ubuntu", "unity", "pantheon", "cinnamon"]
                .contains(&name.to_ascii_lowercase().as_str())
        })
}

/// Apply the palette for `appearance`. Applying an explicit theme also stops
/// gpui-omarchy watching for theme files that are not there.
pub fn apply(appearance: WindowAppearance, cx: &mut App) {
    let theme = match appearance {
        WindowAppearance::Light | WindowAppearance::VibrantLight => {
            Theme::flexoki_light()
        }
        WindowAppearance::Dark | WindowAppearance::VibrantDark => {
            Theme::tokyo_night()
        }
    };
    theme.apply(cx);
}

/// Follow `window`'s appearance from now on.
pub fn follow(window: &Window) {
    window
        .observe_window_appearance(|window, cx| apply(window.appearance(), cx))
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linux_desktop_is_followed_only_if_it_states_a_preference() {
        let native = cfg!(any(target_os = "macos", windows));
        assert!(states_a_preference("ubuntu:GNOME"));
        assert!(states_a_preference("KDE"));
        assert_eq!(states_a_preference("Hyprland"), native);
        assert_eq!(states_a_preference(""), native);
    }

    #[test]
    fn either_omarchy_theme_location_marks_the_desktop() {
        let bare = tempfile::tempdir().unwrap();
        assert!(!follows_omarchy(Some(bare.path())));
        assert!(!follows_omarchy(None));
        for location in
            [".local/state/omarchy/current", ".config/omarchy/current"]
        {
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(location)).unwrap();
            assert!(follows_omarchy(Some(home.path())), "{location}");
        }
    }

    /// Omarchy's one font setting must reach the labels: the desktop's
    /// monospace face replaces GPUI's fixed system UI face.
    #[test]
    fn omarchy_draws_with_the_desktop_monospace_face() {
        let mut theme = Theme::tokyo_night();
        theme.font = "System UI".into();
        theme.mono_font = "Desktop Mono".into();
        assert_eq!(font_for(&theme, true).as_ref(), "Desktop Mono");
        assert_eq!(font_for(&theme, false).as_ref(), "System UI");
    }
}
