//! Where the main window opens: where the user last left it, else centred
//! on the display the user is working on (see `placement.rs`), and always
//! inside the display it opens on.
//!
//! The frame is kept in `window-frame.json` in the settings directory, with
//! the display it was on. GPUI reads a window's bounds relative to the
//! display it is given (macOS) or in the desktop's coordinates (Windows),
//! and reports them the same way, so the saved bounds go back to the same
//! display as `WindowOptions::display_id`. A display that is gone, or one
//! that shrank, still gets the whole window: the frame goes to the display
//! a new window would, is cut to that display's visible area and moved
//! inside it, so no edge opens under the
//! menu bar, the Dock or off the side, as 1440 points did on a laptop.
//!
//! The frame is remembered as the window moves and written once, as the app
//! quits; the window may already be gone by then.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use gpui_kit::{
    App, Bounds, DisplayId, Pixels, Size, Window, WindowBounds, point, px, size,
};
use serde::{Deserialize, Serialize};

const FILE: &str = "window-frame.json";

/// The frame the window last had, waiting for the quit hook to write it.
static LATEST: Mutex<Option<Frame>> = Mutex::new(None);

/// A main window's place, as saved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Frame {
    /// The display's UUID; `None` where the platform has none to give.
    display: Option<String>,
    state: State,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Windowed,
    Maximized,
    Fullscreen,
}

/// Where the window opens, and its smallest size, both fitted to the
/// display it opens on.
pub struct Placement {
    pub display: Option<DisplayId>,
    pub bounds: WindowBounds,
    pub min_size: Size<Pixels>,
}

/// The main window's placement: the saved frame if there is one, else
/// `default` centred on the display a new window opens on.
pub fn initial(
    default: Size<Pixels>,
    min: Size<Pixels>,
    cx: &mut App,
) -> Placement {
    let saved = path().and_then(|path| load(&path));
    let fallback = crate::placement::display_for_new_window(cx);
    placement(saved, fallback, default, min, cx)
}

/// [`initial`], given what was saved and `fallback`, the display for a
/// window with no saved display to return to (`None`: the primary one).
fn placement(
    saved: Option<Frame>,
    fallback: Option<DisplayId>,
    default: Size<Pixels>,
    min: Size<Pixels>,
    cx: &App,
) -> Placement {
    let display = saved
        .as_ref()
        .and_then(|frame| {
            let uuid = frame.display.as_deref()?;
            cx.displays().into_iter().find(|display| {
                display.uuid().is_ok_and(|id| id.to_string() == uuid)
            })
        })
        .or_else(|| fallback.and_then(|id| cx.find_display(id)));
    let display_id = display.as_ref().map(|display| display.id());
    let area = display
        .or_else(|| cx.primary_display())
        .map(|display| display.visible_bounds());
    let min_size = area.map_or(min, |area| min.min(&area.size));
    let bounds = match (saved, area) {
        (Some(frame), Some(area)) => {
            let fitted = fit(frame.bounds(), area);
            match frame.state {
                State::Windowed => WindowBounds::Windowed(fitted),
                State::Maximized => WindowBounds::Maximized(fitted),
                State::Fullscreen => WindowBounds::Fullscreen(fitted),
            }
        }
        // No display to measure (a headless test): the saved size as is.
        (Some(frame), None) => WindowBounds::Windowed(frame.bounds()),
        // `centered` caps the size to the display's visible area itself.
        (None, _) => {
            WindowBounds::Windowed(Bounds::centered(display_id, default, cx))
        }
    };
    Placement {
        display: display_id,
        bounds,
        min_size,
    }
}

/// Note where the window is now, for [`save`] to write as the app quits.
pub fn remember(window: &Window, cx: &App) {
    let display = window
        .display(cx)
        .and_then(|display| display.uuid().ok())
        .map(|uuid| uuid.to_string());
    // Inner: on Linux the outer bounds include the client-side frame's
    // shadow, and opening with those would grow the window by it each
    // launch. Elsewhere the two are the same.
    let frame = Frame::new(display, window.inner_window_bounds());
    *LATEST.lock().unwrap_or_else(PoisonError::into_inner) = Some(frame);
}

/// Write the frame [`remember`] last noted. Nothing moved, nothing written:
/// the frame saved last time still stands.
pub fn save() {
    let Some(frame) =
        LATEST.lock().unwrap_or_else(PoisonError::into_inner).take()
    else {
        return;
    };
    let Some(path) = path() else {
        return;
    };
    if let Err(error) = write(&path, &frame) {
        eprintln!("could not save the window's place: {error}");
    }
}

fn path() -> Option<PathBuf> {
    crate::updater::settings_dir().map(|dir| dir.join(FILE))
}

/// `path`'s frame; a missing, unreadable or nonsensical file is none.
fn load(path: &Path) -> Option<Frame> {
    let text = std::fs::read_to_string(path).ok()?;
    let frame: Frame = serde_json::from_str(&text).ok()?;
    frame.is_usable().then_some(frame)
}

fn write(path: &Path, frame: &Frame) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no settings directory",
        )
    })?;
    std::fs::create_dir_all(dir)?;
    let mut text =
        serde_json::to_string_pretty(frame).map_err(std::io::Error::other)?;
    text.push('\n');
    // Atomic replacement: a failed write leaves the last frame intact.
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(text.as_bytes())?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

impl Frame {
    fn new(display: Option<String>, bounds: WindowBounds) -> Self {
        let (state, bounds) = match bounds {
            WindowBounds::Windowed(bounds) => (State::Windowed, bounds),
            WindowBounds::Maximized(bounds) => (State::Maximized, bounds),
            WindowBounds::Fullscreen(bounds) => (State::Fullscreen, bounds),
        };
        Self {
            display,
            state,
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        }
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds::new(
            point(px(self.x), px(self.y)),
            size(px(self.width), px(self.height)),
        )
    }

    /// A hand-edited or damaged file must not open a window of no size,
    /// or one GPUI cannot place.
    fn is_usable(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|value| value.is_finite())
            && self.width >= 1.0
            && self.height >= 1.0
    }
}

/// `frame` cut to `area`'s size and moved, as little as it takes, inside
/// it: a window that fitted stays exactly where it was.
fn fit(frame: Bounds<Pixels>, area: Bounds<Pixels>) -> Bounds<Pixels> {
    let size = frame.size.min(&area.size);
    // `size` is no larger than `area`, so each range is never empty.
    let x = frame
        .origin
        .x
        .max(area.origin.x)
        .min(area.origin.x + area.size.width - size.width);
    let y = frame
        .origin
        .y
        .max(area.origin.y)
        .min(area.origin.y + area.size.height - size.height);
    Bounds::new(point(x, y), size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    /// A 14-inch laptop at its default resolution, under its menu bar.
    const LAPTOP: (f32, f32, f32, f32) = (0.0, 38.0, 1512.0, 944.0);

    fn laptop() -> Bounds<Pixels> {
        rect(LAPTOP.0, LAPTOP.1, LAPTOP.2, LAPTOP.3)
    }

    #[test]
    fn a_window_that_fits_stays_where_it_was() {
        let frame = rect(200.0, 100.0, 1100.0, 700.0);
        assert_eq!(fit(frame, laptop()), frame);
    }

    #[test]
    fn the_old_default_no_longer_hangs_off_a_laptop() {
        // 120 + 1440 ran 48 points past the right edge.
        let fitted = fit(rect(120.0, 90.0, 1440.0, 900.0), laptop());
        assert_eq!(fitted, rect(72.0, 82.0, 1440.0, 900.0));
    }

    #[test]
    fn a_window_from_a_larger_display_is_cut_to_this_one() {
        let fitted = fit(rect(2400.0, 0.0, 2560.0, 1400.0), laptop());
        assert_eq!(fitted, laptop());
    }

    #[test]
    fn a_window_left_off_the_top_left_comes_back() {
        let fitted = fit(rect(-300.0, -50.0, 800.0, 600.0), laptop());
        assert_eq!(fitted, rect(0.0, 38.0, 800.0, 600.0));
    }

    #[test]
    fn displays_to_the_left_keep_their_negative_coordinates() {
        // Windows reports a monitor left of the primary below zero.
        let left = rect(-1920.0, 0.0, 1920.0, 1040.0);
        let frame = rect(-1700.0, 80.0, 1440.0, 900.0);
        assert_eq!(fit(frame, left), frame);
    }

    #[test]
    fn the_frame_survives_a_round_trip_and_bad_files_are_ignored() {
        let dir = tempfile::tempdir().expect("settings");
        let path = dir.path().join("nested").join(FILE);
        assert_eq!(load(&path), None);

        let frame = Frame::new(
            Some("37D8832A-2D66-02CA-B9F7-8F30A301B230".to_owned()),
            WindowBounds::Maximized(rect(10.0, 40.0, 1200.0, 800.0)),
        );
        write(&path, &frame).expect("write");
        assert_eq!(load(&path), Some(frame.clone()));
        assert_eq!(
            frame.bounds(),
            rect(10.0, 40.0, 1200.0, 800.0),
            "the restore bounds come back unchanged"
        );

        std::fs::write(&path, "{not json").expect("damage");
        assert_eq!(load(&path), None);
        let empty = Frame {
            width: 0.0,
            ..frame
        };
        write(&path, &empty).expect("write");
        assert_eq!(load(&path), None, "a window of no size is not opened");
    }

    #[gpui_kit::test]
    fn a_saved_frame_returns_to_its_display_or_the_working_one(
        cx: &gpui_kit::TestAppContext,
    ) {
        let default = size(px(1440.0), px(900.0));
        let min = size(px(900.0), px(600.0));
        cx.update(|cx| {
            let display = cx.primary_display().expect("a test display");
            let uuid = display.uuid().expect("uuid").to_string();
            let area = display.visible_bounds();
            let saved = |display: &str| Frame {
                display: Some(display.to_owned()),
                state: State::Windowed,
                x: 3000.0,
                y: 40.0,
                width: 1200.0,
                height: 800.0,
            };

            let here = placement(Some(saved(&uuid)), None, default, min, cx);
            assert_eq!(here.display, Some(display.id()));
            let gone =
                placement(Some(saved("unplugged")), None, default, min, cx);
            assert_eq!(gone.display, None, "the primary display");
            // Either way the frame is pulled onto the display it opens on.
            for placed in [here, gone] {
                let WindowBounds::Windowed(bounds) = placed.bounds else {
                    panic!("still windowed");
                };
                assert_eq!(bounds.size, size(px(1200.0), px(800.0)));
                assert_eq!(
                    bounds.origin.x + bounds.size.width,
                    area.origin.x + area.size.width
                );
            }

            let fresh = placement(None, None, default, min, cx);
            let WindowBounds::Windowed(bounds) = fresh.bounds else {
                panic!("windowed");
            };
            assert_eq!(bounds.size, default);
            assert_eq!(bounds.center(), area.center());
            assert_eq!(fresh.min_size, min);

            // The display the user is working on, when nothing saved
            // names one still connected; a fallback since unplugged is
            // the primary display again.
            let working = Some(display.id());
            let fresh = placement(None, working, default, min, cx);
            assert_eq!(fresh.display, working);
            let gone =
                placement(Some(saved("unplugged")), working, default, min, cx);
            assert_eq!(gone.display, working);
            let stale = Some(DisplayId::new(u64::from(display.id()) + 1));
            assert_eq!(placement(None, stale, default, min, cx).display, None);
        });
    }
}
