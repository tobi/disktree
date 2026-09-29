//! What the system says about how the app should look and move.
//!
//! On Omarchy, gpui-omarchy follows the desktop's theme files. Elsewhere there
//! are none, and gpui-omarchy would then stay on its dark default whatever
//! the system is set to; there the app follows the system appearance instead,
//! with the crate's own dark and light palettes, and switches when it does.

use std::path::Path;

use gpui_kit::{App, Window, WindowAppearance};
use gpui_omarchy::Theme;

/// Whether the system appearance, rather than an Omarchy theme, decides the
/// colours: unless an Omarchy theme is installed, on macOS and Windows, and
/// on a Linux desktop that states a preference.
pub fn follows_system(home: Option<&Path>) -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    states_a_preference(&desktop)
        && !home.is_some_and(|home| {
            [".local/state/omarchy/current", ".config/omarchy/current"]
                .iter()
                .any(|theme| home.join(theme).exists())
        })
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

/// Whether the system asks apps to keep motion to a minimum: Reduce Motion
/// on macOS, animations turned off on GNOME. A level change then lands at
/// once instead of flying there.
pub fn reduces_motion() -> bool {
    let read = |program: &str, arguments: &[&str]| {
        std::process::Command::new(program)
            .args(arguments)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            })
    };
    if cfg!(target_os = "macos") {
        read(
            "/usr/bin/defaults",
            &["read", "com.apple.universalaccess", "reduceMotion"],
        )
        .is_some_and(|value| value == "1")
    } else if cfg!(windows) {
        false
    } else {
        read(
            "gsettings",
            &["get", "org.gnome.desktop.interface", "enable-animations"],
        )
        .is_some_and(|value| value == "false")
    }
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
}
