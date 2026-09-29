//! What is kept for the next launch: the theme, the interface zoom and,
//! where the window manager lets an app place its window, the window's
//! frame.
//!
//! One `key = value` line each, in the platform's settings directory. A
//! missing or unreadable file is an empty one, and a line that does not
//! parse is ignored, so a newer version's settings never stop an older one
//! from starting.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::themes::ThemeChoice;

/// A window frame in screen points: origin, then size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub theme: Option<ThemeChoice>,
    /// Interface zoom, as a factor of the default `rem`.
    pub zoom: Option<f32>,
    pub frame: Option<Frame>,
}

impl Settings {
    pub fn path() -> Option<PathBuf> {
        let base = if cfg!(target_os = "macos") {
            std::env::home_dir()?.join("Library/Application Support")
        } else if cfg!(windows) {
            PathBuf::from(std::env::var_os("APPDATA")?)
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .or_else(|| {
                    std::env::home_dir().map(|home| home.join(".config"))
                })?
        };
        Some(base.join("disktree").join("settings"))
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    /// Write the settings, creating their directory. Failing is not worth
    /// interrupting anyone for: the next launch starts from the defaults.
    pub fn save(&self, path: &Path) {
        if let Some(directory) = path.parent() {
            let _ = std::fs::create_dir_all(directory);
        }
        let _ = std::fs::write(path, self.render());
    }

    pub fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "theme" => settings.theme = ThemeChoice::from_key(value),
                "zoom" => {
                    settings.zoom = value
                        .parse::<f32>()
                        .ok()
                        .filter(|zoom| (0.5..=2.0).contains(zoom));
                }
                "frame" => settings.frame = parse_frame(value),
                _ => {}
            }
        }
        settings
    }

    pub fn render(&self) -> String {
        let mut text = String::new();
        if let Some(theme) = self.theme {
            let _ = writeln!(text, "theme = {}", theme.key());
        }
        if let Some(zoom) = self.zoom {
            let _ = writeln!(text, "zoom = {zoom}");
        }
        if let Some(frame) = self.frame {
            let _ = writeln!(
                text,
                "frame = {} {} {} {}",
                frame.x, frame.y, frame.width, frame.height
            );
        }
        text
    }
}

fn parse_frame(value: &str) -> Option<Frame> {
    let numbers: Vec<f32> = value
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let [x, y, width, height] = numbers[..] else {
        return None;
    };
    (width >= 100.0 && height >= 100.0 && x.is_finite() && y.is_finite())
        .then_some(Frame {
            x,
            y,
            width,
            height,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_through_their_file() {
        let settings = Settings {
            theme: Some(ThemeChoice::Paper),
            zoom: Some(1.25),
            frame: Some(Frame {
                x: 120.0,
                y: 90.0,
                width: 1440.0,
                height: 900.0,
            }),
        };
        assert_eq!(Settings::parse(&settings.render()), settings);

        let temp = tempfile::TempDir::new().expect("tempdir");
        let path = temp.path().join("nested").join("settings");
        settings.save(&path);
        assert_eq!(Settings::load(&path), settings);
    }

    #[test]
    fn what_does_not_parse_is_left_at_its_default() {
        let settings = Settings::parse(
            "theme = solarized\nchart=sunburst\nzoom = 9\nframe = 1 2 3\n\
             unknown = yes\nnonsense",
        );
        assert_eq!(settings, Settings::default());
        assert_eq!(
            Settings::load(Path::new("/nonexistent/disktree")),
            Settings::default()
        );
    }
}
