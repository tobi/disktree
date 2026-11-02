//! Which display a new window opens on: the one the user is working on, as
//! macOS places a new window, rather than the primary display GPUI falls
//! back to when it is given none.
//!
//! The display, in order:
//! 1. the one PwrDisk's focused window is on: Settings, Help's documents
//!    and Logs open beside the main window;
//! 2. the one under the pointer, on macOS: the Dock icon or the Finder
//!    window just clicked to launch PwrDisk;
//! 3. none, and GPUI's primary display.
//!
//! The main window only asks when it has no saved frame, or that frame's
//! display is gone (see `window_frame.rs`). Every opener passes the display
//! both to `Bounds::centered` and as `WindowOptions::display_id`: GPUI
//! reads the bounds relative to that display, and opens on the primary one
//! without it.

use gpui_kit::{App, DisplayId};

/// The display a new PwrDisk window should open on, or `None` for the
/// primary display.
pub fn display_for_new_window(cx: &mut App) -> Option<DisplayId> {
    let focused = cx.active_window().and_then(|window| {
        // Err while that window is the one being updated (a click in it
        // opened this one): the pointer is on it then.
        window
            .update(cx, |_, window, cx| window.display(cx).map(|d| d.id()))
            .ok()
            .flatten()
    });
    let connected: Vec<DisplayId> =
        cx.displays().iter().map(|display| display.id()).collect();
    choose(focused, under_pointer(), &connected)
}

/// [`display_for_new_window`]'s rule, apart from the platform: the focused
/// window's display, else the pointer's, skipping any that is no longer
/// connected.
fn choose(
    focused: Option<DisplayId>,
    pointer: Option<DisplayId>,
    connected: &[DisplayId],
) -> Option<DisplayId> {
    [focused, pointer]
        .into_iter()
        .flatten()
        .find(|display| connected.contains(display))
}

/// The display under the mouse pointer, as GPUI names displays: the
/// screen's `NSScreenNumber`, the `CGDirectDisplayID` GPUI keys them by.
/// `None` off the main thread, or when no screen holds the pointer.
#[cfg(target_os = "macos")]
fn under_pointer() -> Option<DisplayId> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSEvent, NSScreen};
    use objc2_foundation::{NSNumber, NSString};

    let mtm = MainThreadMarker::new()?;
    // Both in AppKit's global space: bottom-left origin, in points.
    let mouse = NSEvent::mouseLocation();
    let key = NSString::from_str("NSScreenNumber");
    NSScreen::screens(mtm).iter().find_map(|screen| {
        let frame = screen.frame();
        let held = in_frame(
            (
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height,
            ),
            (mouse.x, mouse.y),
        );
        if !held {
            return None;
        }
        let number = screen
            .deviceDescription()
            .objectForKey(&key)?
            .downcast::<NSNumber>()
            .ok()?;
        Some(DisplayId::new(u64::from(number.unsignedIntValue())))
    })
}

/// Windows and Linux: GPUI has no pointer position outside a window, so a
/// new window follows the focused one or opens on the primary display.
#[cfg(not(target_os = "macos"))]
const fn under_pointer() -> Option<DisplayId> {
    None
}

/// Whether `point` lies in `frame`, both in the bottom-left-origin space
/// `AppKit` uses. A screen's frame holds its left and top edges: the
/// pointer at the very top of a screen is at `y + height`, and at its left
/// at `x`.
#[cfg(any(target_os = "macos", test))]
fn in_frame(
    (x, y, width, height): (f64, f64, f64, f64),
    (px, py): (f64, f64),
) -> bool {
    (x..x + width).contains(&px) && py > y && py <= y + height
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> DisplayId {
        DisplayId::new(n)
    }

    #[test]
    fn the_focused_window_wins_then_the_pointer() {
        let both = [id(1), id(2)];
        assert_eq!(choose(Some(id(2)), Some(id(1)), &both), Some(id(2)));
        // No PwrDisk window yet (a launch): the pointer's.
        assert_eq!(choose(None, Some(id(2)), &both), Some(id(2)));
        // Neither: GPUI's primary display.
        assert_eq!(choose(None, None, &both), None);
    }

    #[test]
    fn a_display_no_longer_connected_is_skipped() {
        let one = [id(1)];
        assert_eq!(choose(Some(id(2)), Some(id(1)), &one), Some(id(1)));
        assert_eq!(choose(Some(id(2)), Some(id(3)), &one), None);
    }

    #[test]
    fn a_screen_holds_its_top_and_left_edges_only() {
        // A second screen to the right of a 1512-point laptop.
        let right = (1512.0, 0.0, 2560.0, 1440.0);
        assert!(in_frame(right, (1512.0, 700.0)), "its left edge");
        assert!(in_frame(right, (2000.0, 1440.0)), "its top edge");
        assert!(!in_frame(right, (1511.9, 700.0)), "the laptop's");
        assert!(!in_frame(right, (4072.0, 700.0)), "past its right edge");
        assert!(!in_frame(right, (2000.0, 0.0)), "its bottom edge");
    }
}
