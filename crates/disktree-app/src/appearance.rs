//! Light or dark when there is no Omarchy theme to follow.
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
