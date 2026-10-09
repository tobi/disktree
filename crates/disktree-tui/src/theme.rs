//! Terminal colors. Omarchy is an optional palette source, not a dependency.
//!
//! The renderer paints with a small reference palette, then maps its color
//! roles onto this palette. That keeps the mosaic's geometry and labels
//! independent of whether the terminal has an Omarchy theme.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use disktree_core::classify::Category;
use ratatui::buffer::Buffer;
use ratatui::style::Color;

pub const BG: Color = Color::Rgb(18, 20, 32);
pub const PANEL: Color = Color::Rgb(28, 32, 50);
pub const TEXT: Color = Color::Rgb(213, 220, 239);
pub const MUTED: Color = Color::Rgb(132, 145, 179);
pub const ACCENT: Color = Color::Rgb(229, 184, 105);
pub const RED: Color = Color::Rgb(221, 111, 119);
const MARKED: Color = Color::Rgb(92, 43, 54);

/// Automatic Omarchy/system colors, or an explicit terminal appearance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeChoice {
    #[default]
    Auto,
    Dark,
    Light,
}

impl ThemeChoice {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "auto" => Some(Self::Auto),
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }
}

/// What the active terminal colors mean to the renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    background: Color,
    panel: Color,
    text: Color,
    muted: Color,
    accent: Color,
    red: Color,
    categories: [Color; 9],
    light: bool,
}

impl Palette {
    pub(crate) const fn dark() -> Self {
        Self {
            background: BG,
            panel: PANEL,
            text: TEXT,
            muted: MUTED,
            accent: ACCENT,
            red: RED,
            categories: [
                base_category_color(Category::Code),
                base_category_color(Category::AgentScratch),
                base_category_color(Category::Toolchain),
                base_category_color(Category::Synced),
                base_category_color(Category::Git),
                base_category_color(Category::Media),
                base_category_color(Category::Documents),
                base_category_color(Category::Cache),
                base_category_color(Category::Other),
            ],
            light: false,
        }
    }

    const fn light() -> Self {
        Self {
            background: Color::Rgb(255, 252, 240),
            panel: Color::Rgb(234, 231, 218),
            text: Color::Rgb(26, 29, 38),
            muted: Color::Rgb(100, 104, 113),
            accent: Color::Rgb(38, 90, 161),
            red: Color::Rgb(169, 53, 58),
            categories: [
                Color::Rgb(48, 95, 176),
                Color::Rgb(170, 90, 41),
                Color::Rgb(49, 128, 87),
                Color::Rgb(35, 119, 142),
                Color::Rgb(171, 65, 81),
                Color::Rgb(118, 71, 153),
                Color::Rgb(100, 103, 111),
                Color::Rgb(141, 112, 36),
                Color::Rgb(126, 128, 132),
            ],
            light: true,
        }
    }

    fn from_omarchy(source: &str) -> Option<Self> {
        let doc: toml::Value = source.parse().ok()?;
        let table = doc.as_table()?;
        let background = color_key(table, &["background", "bg", "color0"])?;
        let text = color_key(table, &["foreground", "fg", "color7"])?;
        let light = match table.get("mode").and_then(toml::Value::as_str) {
            Some("light") => true,
            Some("dark") => false,
            _ => is_light(background),
        };
        let fallback = if light { Self::light() } else { Self::dark() };
        let panel = color_key(
            table,
            &["lighter_background", "lighter_bg", "dark_background"],
        )
        .unwrap_or_else(|| mix(background, text, 0.07));
        let muted = color_key(table, &["dark_foreground", "dark_fg", "color8"])
            .unwrap_or_else(|| mix(background, text, 0.55));
        let accent = color_key(
            table,
            if light {
                &["accent", "blue", "color4"]
            } else {
                &["yellow", "color3", "accent"]
            },
        )
        .unwrap_or(fallback.accent);
        let red = color_key(table, &["red", "color1"]).unwrap_or(fallback.red);
        let pairs: [&[&str]; 9] = [
            &["blue", "color4"],
            &["orange", "color11", "color3"],
            &["green", "color2"],
            &["cyan", "color6"],
            &["red", "color1"],
            &["magenta", "color5"],
            &["light_foreground", "light_fg", "color7"],
            &["yellow", "color3"],
            &["muted", "color8"],
        ];
        let categories = std::array::from_fn(|index| {
            color_key(table, pairs[index]).unwrap_or(fallback.categories[index])
        });
        Some(Self {
            background,
            panel,
            text,
            muted,
            accent,
            red,
            categories,
            light,
        })
    }

    pub fn apply(&self, buffer: &mut Buffer) {
        let area = buffer.area;
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.fg = self.map(cell.fg);
                    cell.bg = self.map(cell.bg);
                }
            }
        }
    }

    fn map(&self, source: Color) -> Color {
        for (from, to) in [
            (BG, self.background),
            (PANEL, self.panel),
            (TEXT, self.text),
            (MUTED, self.muted),
            (ACCENT, self.accent),
            (RED, self.red),
            (
                MARKED,
                mix(
                    self.background,
                    self.red,
                    if self.light { 0.18 } else { 0.4 },
                ),
            ),
        ] {
            if source == from {
                return to;
            }
        }
        for (index, category) in Category::LEGEND
            .into_iter()
            .chain([Category::Other])
            .enumerate()
        {
            let base = base_category_color(category);
            let mapped = self.categories[index];
            if source == base {
                return mapped;
            }
            if source == shade(base, 0.68) {
                return mix(self.background, mapped, 0.6);
            }
            for depth in 0..3 {
                let factor = 0.05_f32.mul_add(depth as f32, 0.32);
                if source == shade(base, factor) {
                    let amount = if self.light {
                        0.03_f32.mul_add(depth as f32, 0.15)
                    } else {
                        0.04_f32.mul_add(depth as f32, 0.27)
                    };
                    return mix(self.background, mapped, amount);
                }
            }
        }
        for bucket in 0..AGE_LABELS.len() {
            let base = base_age_color(bucket);
            let mapped = mix(
                self.accent,
                self.muted,
                bucket as f32 / (AGE_LABELS.len() - 1) as f32,
            );
            if source == base {
                return mapped;
            }
            if source == shade(base, 0.68) {
                return mix(self.background, mapped, 0.6);
            }
            for depth in 0..3 {
                let factor = 0.05_f32.mul_add(depth as f32, 0.32);
                if source == shade(base, factor) {
                    let amount = if self.light {
                        0.03_f32.mul_add(depth as f32, 0.15)
                    } else {
                        0.04_f32.mul_add(depth as f32, 0.27)
                    };
                    return mix(self.background, mapped, amount);
                }
            }
        }
        source
    }
}

/// Read the optional current theme periodically. Omarchy swaps a symlink
/// when its theme changes, so reading the current path also catches swaps.
#[derive(Debug)]
pub struct ThemeState {
    choice: ThemeChoice,
    palette: Palette,
    last_check: Instant,
}

impl ThemeState {
    pub fn new(choice: ThemeChoice) -> Self {
        let mut state = Self {
            choice,
            palette: Palette::dark(),
            last_check: Instant::now(),
        };
        let _ = state.refresh();
        state
    }

    pub const fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn tick(&mut self) -> bool {
        if self.last_check.elapsed() < Duration::from_secs(2) {
            return false;
        }
        self.refresh()
    }

    fn refresh(&mut self) -> bool {
        let next = match self.choice {
            ThemeChoice::Dark => Palette::dark(),
            ThemeChoice::Light => Palette::light(),
            ThemeChoice::Auto => std::env::home_dir()
                .as_deref()
                .and_then(load_omarchy)
                .unwrap_or_else(system_fallback),
        };
        self.last_check = Instant::now();
        if self.palette == next {
            return false;
        }
        self.palette = next;
        true
    }
}

fn load_omarchy(home: &Path) -> Option<Palette> {
    let primary = home.join(".local/state/omarchy/current");
    let legacy = home.join(".config/omarchy/current");
    let active = if fs::symlink_metadata(&primary).is_ok() {
        primary
    } else {
        legacy
    };
    let source = fs::read_to_string(active.join("theme/colors.toml")).ok()?;
    Palette::from_omarchy(&source)
}

fn color_key(table: &toml::value::Table, names: &[&str]) -> Option<Color> {
    names.iter().find_map(|name| {
        table
            .get(*name)
            .and_then(toml::Value::as_str)
            .and_then(hex_color)
    })
}

fn hex_color(source: &str) -> Option<Color> {
    let hex = source.strip_prefix('#')?;
    let rgb = hex.get(..6)?;
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    Some(Color::Rgb(
        u8::from_str_radix(&rgb[0..2], 16).ok()?,
        u8::from_str_radix(&rgb[2..4], 16).ok()?,
        u8::from_str_radix(&rgb[4..6], 16).ok()?,
    ))
}

fn system_fallback() -> Palette {
    if terminal_wants_light().unwrap_or_else(system_wants_light) {
        Palette::light()
    } else {
        Palette::dark()
    }
}

fn terminal_wants_light() -> Option<bool> {
    let value = std::env::var("COLORFGBG").ok()?;
    let background = value.rsplit([';', ':']).next()?.parse::<u8>().ok()?;
    Some(background == 7 || background >= 15)
}

#[cfg(target_os = "macos")]
fn system_wants_light() -> bool {
    std::process::Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .is_ok_and(|result| {
            !result.status.success() || result.stdout != b"Dark\n"
        })
}

#[cfg(windows)]
fn system_wants_light() -> bool {
    std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "AppsUseLightTheme",
        ])
        .output()
        .is_ok_and(|result| {
            result.status.success()
                && String::from_utf8_lossy(&result.stdout).contains("0x1")
        })
}

#[cfg(not(any(target_os = "macos", windows)))]
fn system_wants_light() -> bool {
    std::env::var("GTK_THEME")
        .is_ok_and(|theme| theme.to_ascii_lowercase().contains("light"))
}

fn is_light(color: Color) -> bool {
    let Color::Rgb(r, g, b) = color else {
        return false;
    };
    u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114 >= 128_000
}

fn mix(from: Color, to: Color, amount: f32) -> Color {
    let (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) = (from, to) else {
        return from;
    };
    let channel = |start: u8, end: u8| {
        (f32::from(end) - f32::from(start))
            .mul_add(amount, f32::from(start))
            .round() as u8
    };
    Color::Rgb(channel(fr, tr), channel(fg, tg), channel(fb, tb))
}

pub const fn base_category_color(category: Category) -> Color {
    match category {
        Category::Code => Color::Rgb(93, 139, 212),
        Category::AgentScratch => Color::Rgb(208, 141, 93),
        Category::Toolchain => Color::Rgb(79, 182, 144),
        Category::Synced => Color::Rgb(83, 170, 193),
        Category::Git => Color::Rgb(197, 93, 116),
        Category::Media => Color::Rgb(151, 101, 194),
        Category::Documents => Color::Rgb(155, 163, 180),
        Category::Cache => Color::Rgb(197, 169, 83),
        Category::Other => Color::Rgb(112, 123, 149),
    }
}

/// The GUI's five age bands, newest first.
pub const AGE_LABELS: [&str; 5] = [
    "This week",
    "This month",
    "Six months",
    "This year",
    "Older",
];

pub const fn age_bucket(modified: i64, scanned_at: i64) -> Option<usize> {
    if modified <= 0 {
        return None;
    }
    let days = scanned_at.saturating_sub(modified) / 86_400;
    Some(match days {
        ..=7 => 0,
        8..=30 => 1,
        31..=182 => 2,
        183..=365 => 3,
        _ => 4,
    })
}

pub const fn base_age_color(bucket: usize) -> Color {
    match bucket {
        0 => Color::Rgb(230, 186, 106),
        1 => Color::Rgb(203, 177, 122),
        2 => Color::Rgb(176, 163, 138),
        3 => Color::Rgb(151, 150, 151),
        _ => Color::Rgb(134, 140, 150),
    }
}

pub fn shade(color: Color, factor: f32) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return BG;
    };
    Color::Rgb(
        (f32::from(r) * factor) as u8,
        (f32::from(g) * factor) as u8,
        (f32::from(b) * factor) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_dark_and_light_themes_map_roles() {
        let dark = Palette::from_omarchy(
            "mode = 'dark'\nbackground = '#1a1b26'\nforeground = '#a9b1d6'\n\
             yellow = '#e0af68'\nblue = '#7aa2f7'\n",
        )
        .expect("theme");
        assert!(!dark.light);
        assert_eq!(dark.map(ACCENT), Color::Rgb(224, 175, 104));
        assert_eq!(
            dark.map(base_category_color(Category::Code)),
            Color::Rgb(122, 162, 247)
        );
        let light = Palette::from_omarchy(
            "mode = 'light'\nbackground = '#FFFCF0'\nforeground = '#100F0F'\n\
             accent = '#205EA6'\n",
        )
        .expect("theme");
        assert!(light.light);
        assert_eq!(light.map(BG), Color::Rgb(255, 252, 240));
        assert_eq!(light.map(TEXT), Color::Rgb(16, 15, 15));
        assert_eq!(light.map(ACCENT), Color::Rgb(32, 94, 166));
    }

    #[test]
    fn ansi_theme_and_legacy_location_work() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = temp
            .path()
            .join(".config/omarchy/current/theme/colors.toml");
        fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        fs::write(
            &file,
            "color0 = '#101010'\ncolor7 = '#eeeeee'\ncolor4 = '#8899cc'\n",
        )
        .expect("write");
        let palette = load_omarchy(temp.path()).expect("legacy theme");
        assert_eq!(palette.map(BG), Color::Rgb(16, 16, 16));
        assert_eq!(
            palette.map(base_category_color(Category::Code)),
            Color::Rgb(136, 153, 204)
        );
        let primary = temp.path().join(".local/state/omarchy/current");
        fs::create_dir_all(&primary).expect("primary");
        assert!(load_omarchy(temp.path()).is_none());
    }

    #[test]
    fn invalid_color_does_not_create_a_partial_theme() {
        assert!(
            Palette::from_omarchy(
                "background = 'oops'\nforeground = '#ffffff'"
            )
            .is_none()
        );
        assert_eq!(hex_color("#11223344"), Some(Color::Rgb(17, 34, 51)));
        assert_eq!(hex_color("#11GG33"), None);
    }
}
