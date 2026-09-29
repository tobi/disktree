//! The themes a person can pick, and how each one tints the mosaic.
//!
//! Match System keeps what disktree has always done: the Omarchy theme on
//! Omarchy, and elsewhere the system's light or dark setting. High Contrast
//! follows that setting too; the others are fixed. A theme sets the chrome and
//! the colours disktree owns: the mosaic, its tone, and the single strong
//! highlight.

use gpui_kit::base::ThemeAppearance;
use gpui_kit::{App, Global, Hsla, Rgba, WindowAppearance, rgb};
use gpui_omarchy::Theme;

use crate::palette::mix;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeChoice {
    #[default]
    System,
    HighContrast,
    Light,
    Paper,
    Glacier,
    Dark,
    Midnight,
    Forest,
    Ember,
}

impl ThemeChoice {
    pub const ALL: [Self; 9] = [
        Self::System,
        Self::HighContrast,
        Self::Light,
        Self::Paper,
        Self::Glacier,
        Self::Dark,
        Self::Midnight,
        Self::Forest,
        Self::Ember,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "Match System",
            Self::HighContrast => "High Contrast",
            Self::Light => "Light",
            Self::Paper => "Paper",
            Self::Glacier => "Glacier",
            Self::Dark => "Dark",
            Self::Midnight => "Midnight",
            Self::Forest => "Forest",
            Self::Ember => "Ember",
        }
    }

    /// The name settings keep it under.
    pub const fn key(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::HighContrast => "high-contrast",
            Self::Light => "light",
            Self::Paper => "paper",
            Self::Glacier => "glacier",
            Self::Dark => "dark",
            Self::Midnight => "midnight",
            Self::Forest => "forest",
            Self::Ember => "ember",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|choice| choice.key() == key)
    }

    /// Whether the theme is always dark (`Some(true)`), always light, or
    /// follows the system (`None`).
    pub const fn fixed_dark(self) -> Option<bool> {
        match self {
            Self::System | Self::HighContrast => None,
            Self::Light | Self::Paper | Self::Glacier => Some(false),
            Self::Dark | Self::Midnight | Self::Forest | Self::Ember => {
                Some(true)
            }
        }
    }

    /// The palette disktree draws with for this choice, or `None` when the
    /// system's own theme decides.
    pub const fn palette(self, dark: bool) -> Option<Palette> {
        Some(match self {
            Self::System => return None,
            Self::HighContrast if dark => HIGH_CONTRAST_DARK,
            Self::HighContrast => HIGH_CONTRAST_LIGHT,
            Self::Light => LIGHT,
            Self::Paper => PAPER,
            Self::Glacier => GLACIER,
            Self::Dark => DARK,
            Self::Midnight => MIDNIGHT,
            Self::Forest => FOREST,
            Self::Ember => EMBER,
        })
    }
}

/// How a theme's fills are derived from a category's hue: one muted
/// saturation and lightness for every colourful kind, lifted with depth.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tone {
    pub fill_saturation: f32,
    pub fill_lightness: f32,
    /// Lightness per level of nesting: up on dark, down on light.
    pub depth_step: f32,
    pub accent_saturation: f32,
    pub accent_lightness: f32,
    /// How far every fill is pulled toward the theme's inset surface.
    pub inset_mix: f32,
    pub age_lightness: f32,
}

/// The tone of an Omarchy theme, which has none of its own.
const OMARCHY_DARK: Tone = Tone {
    fill_saturation: 0.26,
    fill_lightness: 0.215,
    depth_step: 0.028,
    accent_saturation: 0.42,
    accent_lightness: 0.52,
    inset_mix: 0.12,
    age_lightness: 0.29,
};

const OMARCHY_LIGHT: Tone = Tone {
    fill_saturation: 0.30,
    fill_lightness: 0.84,
    depth_step: -0.03,
    accent_saturation: 0.45,
    accent_lightness: 0.46,
    inset_mix: 0.12,
    age_lightness: 0.74,
};

/// The tone `theme` is drawn with: its own when it is one of disktree's,
/// otherwise the one for its appearance.
pub fn tone(theme: &Theme) -> Tone {
    PALETTES
        .iter()
        .find(|palette| palette.name == theme.name.as_ref())
        .map_or_else(
            || {
                if matches!(theme.appearance, ThemeAppearance::Dark) {
                    OMARCHY_DARK
                } else {
                    OMARCHY_LIGHT
                }
            },
            |palette| palette.tone,
        )
}

/// One of disktree's own themes, in the colours it is defined by.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub name: &'static str,
    pub dark: bool,
    /// The window around everything; between inset and surface when absent.
    pub chrome: Option<u32>,
    pub inset: u32,
    pub surface: u32,
    pub foreground: u32,
    pub bright: u32,
    pub secondary: u32,
    pub accent: u32,
    /// The one strong colour: the selection, the main action, what can be
    /// had back.
    pub highlight: u32,
    pub danger: u32,
    pub success: u32,
    pub tone: Tone,
}

impl Palette {
    /// As a gpui-omarchy theme, set in `font`.
    pub fn theme(&self, font: gpui_kit::SharedString) -> Theme {
        let color = |hex: u32| -> Hsla { rgb(hex).into() };
        let (inset, surface) = (color(self.inset), color(self.surface));
        let background =
            self.chrome.map_or_else(|| mix(inset, surface, 0.5), color);
        let (foreground, accent) = (color(self.foreground), color(self.accent));
        Theme {
            name: self.name.into(),
            appearance: if self.dark {
                ThemeAppearance::Dark
            } else {
                ThemeAppearance::Light
            },
            background,
            surface,
            inset,
            foreground,
            secondary: color(self.secondary),
            bright: color(self.bright),
            accent,
            on_accent: if self.dark {
                inset
            } else {
                Hsla::from(Rgba {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                })
            },
            selection: mix(background, accent, 0.22),
            border: mix(background, foreground, 0.28),
            danger: color(self.danger),
            warning: color(self.highlight),
            success: color(self.success),
            font,
            // gpui-omarchy's system face, which its own themes all carry.
            mono_font: Theme::tokyo_night().mono_font,
        }
    }
}

const LIGHT: Palette = Palette {
    name: "disktree Light",
    dark: false,
    chrome: None,
    inset: 0x00E9_E8E4,
    surface: 0x00F6_F5F2,
    foreground: 0x002B_2A28,
    bright: 0x0011_1111,
    secondary: 0x006F_6D68,
    accent: 0x002F_6FCF,
    highlight: 0x00B9_770E,
    danger: 0x00C5_3030,
    success: 0x004F_7A12,
    tone: Tone {
        fill_saturation: 0.30,
        fill_lightness: 0.84,
        depth_step: -0.03,
        accent_saturation: 0.45,
        accent_lightness: 0.46,
        inset_mix: 0.12,
        age_lightness: 0.76,
    },
};

const DARK: Palette = Palette {
    name: "disktree Dark",
    dark: true,
    chrome: None,
    inset: 0x0015_161B,
    surface: 0x001F_2027,
    foreground: 0x00C8_CCD8,
    bright: 0x00EE_F0F6,
    secondary: 0x008A_8FA3,
    accent: 0x007A_A2F7,
    highlight: 0x00E3_B06A,
    danger: 0x00F2_7A8C,
    success: 0x009C_CB70,
    tone: OMARCHY_DARK,
};

const HIGH_CONTRAST_LIGHT: Palette = Palette {
    name: "disktree High Contrast Light",
    dark: false,
    chrome: Some(0x00FF_FFFF),
    inset: 0x00FF_FFFF,
    surface: 0x00FF_FFFF,
    foreground: 0x0000_0000,
    bright: 0x0000_0000,
    secondary: 0x003A_3A3A,
    accent: 0x0000_47B3,
    highlight: 0x00B8_4A00,
    danger: 0x00B8_001B,
    success: 0x001D_6600,
    tone: Tone {
        fill_saturation: 0.55,
        fill_lightness: 0.86,
        depth_step: -0.03,
        accent_saturation: 0.72,
        accent_lightness: 0.40,
        inset_mix: 0.0,
        age_lightness: 0.84,
    },
};

const HIGH_CONTRAST_DARK: Palette = Palette {
    name: "disktree High Contrast Dark",
    dark: true,
    chrome: Some(0x0000_0000),
    inset: 0x0000_0000,
    surface: 0x000A_0A0A,
    foreground: 0x00FF_FFFF,
    bright: 0x00FF_FFFF,
    secondary: 0x00CC_CCCC,
    accent: 0x005C_AEFF,
    highlight: 0x00FF_D60A,
    danger: 0x00FF_5C66,
    success: 0x007E_E081,
    tone: Tone {
        fill_saturation: 0.45,
        fill_lightness: 0.15,
        depth_step: 0.02,
        accent_saturation: 0.68,
        accent_lightness: 0.62,
        inset_mix: 0.0,
        age_lightness: 0.2,
    },
};

const PAPER: Palette = Palette {
    name: "disktree Paper",
    dark: false,
    chrome: Some(0x00F4_EDDF),
    inset: 0x00E8_DFCC,
    surface: 0x00F5_EFE2,
    foreground: 0x003B_3024,
    bright: 0x001F_170E,
    secondary: 0x007D_6E5A,
    accent: 0x003E_6A86,
    highlight: 0x00B4_5A14,
    danger: 0x00A3_263A,
    success: 0x005A_7A1E,
    tone: Tone {
        fill_saturation: 0.30,
        fill_lightness: 0.80,
        depth_step: -0.032,
        accent_saturation: 0.42,
        accent_lightness: 0.42,
        inset_mix: 0.30,
        age_lightness: 0.77,
    },
};

const GLACIER: Palette = Palette {
    name: "disktree Glacier",
    dark: false,
    chrome: Some(0x00ED_F3F8),
    inset: 0x00D9_E3EC,
    surface: 0x00F4_F8FB,
    foreground: 0x001E_2A35,
    bright: 0x000A_121A,
    secondary: 0x005B_6B79,
    accent: 0x002A_6DB5,
    highlight: 0x00D2_501A,
    danger: 0x00B8_234F,
    success: 0x002C_7A55,
    tone: Tone {
        fill_saturation: 0.32,
        fill_lightness: 0.84,
        depth_step: -0.03,
        accent_saturation: 0.50,
        accent_lightness: 0.46,
        inset_mix: 0.26,
        age_lightness: 0.76,
    },
};

const MIDNIGHT: Palette = Palette {
    name: "disktree Midnight",
    dark: true,
    chrome: Some(0x0011_1829),
    inset: 0x000B_1120,
    surface: 0x0013_1A2C,
    foreground: 0x00C3_CCE0,
    bright: 0x00ED_F1FA,
    secondary: 0x0081_90AD,
    accent: 0x006E_A6FF,
    highlight: 0x00F0_BE4A,
    danger: 0x00FF_7A93,
    success: 0x0086_D38A,
    tone: Tone {
        fill_saturation: 0.36,
        fill_lightness: 0.215,
        depth_step: 0.032,
        accent_saturation: 0.55,
        accent_lightness: 0.60,
        inset_mix: 0.22,
        age_lightness: 0.29,
    },
};

const FOREST: Palette = Palette {
    name: "disktree Forest",
    dark: true,
    chrome: Some(0x0011_1B16),
    inset: 0x000C_1410,
    surface: 0x0015_211B,
    foreground: 0x00C7_D4CB,
    bright: 0x00EE_F5F0,
    secondary: 0x0087_9A8D,
    accent: 0x0086_C9A0,
    highlight: 0x00EB_A83F,
    danger: 0x00F2_7B7B,
    success: 0x009F_D57C,
    tone: Tone {
        fill_saturation: 0.28,
        fill_lightness: 0.21,
        depth_step: 0.03,
        accent_saturation: 0.44,
        accent_lightness: 0.55,
        inset_mix: 0.26,
        age_lightness: 0.21,
    },
};

const EMBER: Palette = Palette {
    name: "disktree Ember",
    dark: true,
    chrome: Some(0x001C_1612),
    inset: 0x0015_100D,
    surface: 0x0023_1B16,
    foreground: 0x00DA_CDC2,
    bright: 0x00F8_F0E9,
    secondary: 0x00A3_8F80,
    accent: 0x005F_B8B0,
    highlight: 0x00FF_B84D,
    danger: 0x00FF_6E86,
    success: 0x00A6_D17A,
    tone: Tone {
        fill_saturation: 0.30,
        fill_lightness: 0.215,
        depth_step: 0.03,
        accent_saturation: 0.46,
        accent_lightness: 0.56,
        inset_mix: 0.24,
        age_lightness: 0.2,
    },
};

const PALETTES: [Palette; 9] = [
    LIGHT,
    DARK,
    HIGH_CONTRAST_LIGHT,
    HIGH_CONTRAST_DARK,
    PAPER,
    GLACIER,
    MIDNIGHT,
    FOREST,
    EMBER,
];

/// The choice in effect, for the appearance observer to re-apply.
#[derive(Clone, Copy, Debug)]
struct Current {
    choice: ThemeChoice,
    /// No Omarchy theme to follow: the system's light or dark decides.
    native: bool,
}

impl Global for Current {}

/// Apply `choice` now. `native` says whether the system's appearance,
/// rather than an Omarchy theme, is what Match System follows.
pub fn apply(choice: ThemeChoice, native: bool, cx: &mut App) {
    cx.set_global(Current { choice, native });
    // Fixed themes fix the window chrome too, or a dark theme on a light
    // system would sit inside a light titlebar.
    cx.set_window_appearance(choice.fixed_dark().map(|dark| {
        if dark {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        }
    }));
    if choice == ThemeChoice::System && !native {
        Theme::follow_system(cx);
        return;
    }
    follow_appearance(cx.window_appearance(), cx);
}

/// Re-apply the choice in effect for the system's `appearance`, when it
/// follows the system.
pub fn follow_appearance(appearance: WindowAppearance, cx: &mut App) {
    let Some(current) = cx.try_global::<Current>().copied() else {
        return;
    };
    let dark = matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    );
    let font = cx.omarchy_font();
    let theme = match current.choice {
        ThemeChoice::System if current.native => {
            if dark {
                Theme::tokyo_night()
            } else {
                Theme::flexoki_light()
            }
        }
        ThemeChoice::System => return,
        ThemeChoice::HighContrast => {
            // On Omarchy the desktop's theme says light or dark.
            let dark = if current.native {
                dark
            } else {
                matches!(
                    Theme::system_or_default().appearance,
                    ThemeAppearance::Dark
                )
            };
            (if dark {
                HIGH_CONTRAST_DARK
            } else {
                HIGH_CONTRAST_LIGHT
            })
            .theme(font)
        }
        choice => match choice.palette(dark) {
            Some(palette) => palette.theme(font),
            None => return,
        },
    };
    theme.apply(cx);
}

/// The font the current theme sets text in, kept across a theme change.
trait OmarchyFont {
    fn omarchy_font(&self) -> gpui_kit::SharedString;
}

impl OmarchyFont for App {
    fn omarchy_font(&self) -> gpui_kit::SharedString {
        use gpui_omarchy::ActiveTheme as _;
        self.try_global::<Theme>().map_or_else(
            || ".SystemUIFont".into(),
            |_| self.omarchy().font.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{age_fill, category_fill, label_color, marked_fill};
    use disktree_core::classify::Category;

    fn luminance(color: Hsla) -> f32 {
        let rgba = color.to_rgb();
        let linear = |value: f32| {
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.0722f32.mul_add(
            linear(rgba.b),
            0.2126f32.mul_add(linear(rgba.r), 0.7152 * linear(rgba.g)),
        )
    }

    fn contrast(text: Hsla, fill: Hsla) -> f32 {
        let shown = mix(fill, text.opacity(1.0), text.a);
        let (a, b) = (luminance(shown), luminance(fill));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    fn themes() -> Vec<(ThemeChoice, Theme)> {
        ThemeChoice::ALL
            .into_iter()
            .flat_map(|choice| {
                [false, true].into_iter().filter_map(move |dark| {
                    choice
                        .palette(dark)
                        .map(|palette| (choice, palette.theme("font".into())))
                })
            })
            .collect()
    }

    /// Tile names are the one text painted on the palette's own colours, so
    /// every theme keeps them at WCAG AA (4.5:1) on every fill it can draw,
    /// and High Contrast at AAA (7:1).
    #[test]
    fn tile_names_stay_readable_on_every_fill() {
        let categories = [
            Category::Code,
            Category::AgentScratch,
            Category::Toolchain,
            Category::Synced,
            Category::Git,
            Category::Media,
            Category::Documents,
            Category::Cache,
            Category::Other,
        ];
        for (choice, theme) in themes() {
            let mut fills = vec![marked_fill(&theme)];
            for depth in 0..5 {
                fills.extend(
                    categories.iter().map(|&category| {
                        category_fill(&theme, category, depth)
                    }),
                );
                fills.extend(
                    (0..crate::palette::AGE_BUCKETS.len())
                        .map(|bucket| age_fill(&theme, bucket, depth)),
                );
            }
            let label = label_color(&theme, 1);
            let worst = fills
                .iter()
                .map(|&fill| contrast(label, fill))
                .fold(f32::INFINITY, f32::min);
            let required = if choice == ThemeChoice::HighContrast {
                7.0
            } else {
                4.5
            };
            assert!(worst >= required, "{}: {worst}", theme.name);
        }
    }

    /// A theme that fixes the appearance draws the same palette whatever the
    /// system is set to; one that follows it has both.
    #[test]
    fn a_palette_matches_the_appearance_it_is_drawn_in() {
        for choice in ThemeChoice::ALL {
            let (light, dark) = (choice.palette(false), choice.palette(true));
            match choice.fixed_dark() {
                Some(fixed) => {
                    assert_eq!(light, dark, "{choice:?}");
                    assert_eq!(light.map(|p| p.dark), Some(fixed));
                }
                None if choice == ThemeChoice::System => {
                    assert!(light.is_none() && dark.is_none());
                }
                None => {
                    assert_eq!(light.map(|p| p.dark), Some(false));
                    assert_eq!(dark.map(|p| p.dark), Some(true));
                }
            }
        }
    }

    #[test]
    fn a_theme_is_drawn_in_its_own_tone_and_an_omarchy_one_in_the_default() {
        let paper = PAPER.theme("font".into());
        assert_eq!(tone(&paper), PAPER.tone);
        assert_eq!(tone(&Theme::tokyo_night()), OMARCHY_DARK);
        assert_eq!(tone(&Theme::flexoki_light()), OMARCHY_LIGHT);
    }

    #[test]
    fn choices_round_trip_through_their_keys() {
        for choice in ThemeChoice::ALL {
            assert_eq!(ThemeChoice::from_key(choice.key()), Some(choice));
        }
        assert_eq!(ThemeChoice::from_key("solarized"), None);
    }
}
