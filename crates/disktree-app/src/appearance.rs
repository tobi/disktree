//! Light or dark when there is no Omarchy theme to follow.
//!
//! On Omarchy, gpui-omarchy follows the desktop's theme files. Elsewhere there
//! are none, and gpui-omarchy would then stay on its dark default whatever
//! the system is set to; there the app follows the system appearance instead,
//! with the crate's own dark and light palettes, and switches when it does.
//!
//! Either can be overruled: a [`ThemeChoice`] of light or dark stays put
//! whatever the desktop does, until the choice goes back to the system's.

use std::path::Path;

use gpui_kit::{App, Window, WindowAppearance};
use gpui_omarchy::Theme;

/// Whose palette is drawn: the system's, or one picked here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeChoice {
    /// The Omarchy theme, or the system's light or dark setting.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const fn name(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// The choice after this one, for a control that steps through them.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }
}

/// The choice in force, where the appearance observer can see it: a system
/// that turns dark at dusk must not repaint a window that was told light.
struct Chosen(ThemeChoice);

impl gpui_kit::Global for Chosen {}

/// Draw with `choice` from now on. `appearance` is the window's, used when
/// the choice is the system's and there is no Omarchy theme to follow.
pub fn choose(
    choice: ThemeChoice,
    appearance: WindowAppearance,
    home: Option<&Path>,
    cx: &mut App,
) {
    cx.set_global(Chosen(choice));
    match choice {
        ThemeChoice::Light => Theme::flexoki_light().apply(cx),
        ThemeChoice::Dark => Theme::tokyo_night().apply(cx),
        ThemeChoice::System if follows_system(home) => apply(appearance, cx),
        // Back to the Omarchy theme files, which an explicit theme stopped
        // watching.
        ThemeChoice::System => Theme::follow_system(cx),
    }
}

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
        .observe_window_appearance(|window, cx| {
            let chosen = cx
                .try_global::<Chosen>()
                .map_or(ThemeChoice::System, |chosen| chosen.0);
            if chosen == ThemeChoice::System {
                apply(window.appearance(), cx);
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_control_steps_through_every_choice_and_back() {
        let mut choice = ThemeChoice::default();
        for expected in
            [ThemeChoice::Light, ThemeChoice::Dark, ThemeChoice::System]
        {
            choice = choice.next();
            assert_eq!(choice, expected);
        }
    }

    #[test]
    fn a_linux_desktop_is_followed_only_if_it_states_a_preference() {
        let native = cfg!(any(target_os = "macos", windows));
        assert!(states_a_preference("ubuntu:GNOME"));
        assert!(states_a_preference("KDE"));
        assert_eq!(states_a_preference("Hyprland"), native);
        assert_eq!(states_a_preference(""), native);
    }
}
