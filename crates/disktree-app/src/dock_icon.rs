//! The Dock icon of a run outside the app bundle.
//!
//! `make run`, `cargo run` and the bare release binary all start disktree
//! with no bundle around it, so macOS has no icon to draw and the Dock shows
//! the generic executable. `AppKit` lets a running app replace its Dock icon
//! with an image, so the app carries its own and paints it whenever the
//! bundle it runs in, if any, names none.
//!
//! The image is drawn literally. macOS 26 gives Liquid Glass only to a
//! bundle's Icon Composer icon and has no API to ask for it at run time, so
//! the glass is baked into the image instead: `DockIcon-glass.png` is the
//! shipped icon as macOS 26 itself draws it, rendered by
//! `packaging/macos/render-dock-icons.swift`. macOS 15 and earlier draw a
//! flat icon on Apple's legacy tile, and get `DockIcon.png`, which is that.

#![allow(
    unsafe_code,
    reason = "one AppKit setter objc2 marks unsafe; the call says why it is \
              sound"
)]

use objc2::{AllocAnyThread as _, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::{NSBundle, NSData, NSString};

const FLAT: &[u8] = include_bytes!("../../../packaging/macos/DockIcon.png");
const GLASS: &[u8] =
    include_bytes!("../../../packaging/macos/DockIcon-glass.png");

/// Give the Dock an icon unless the bundle the app runs in has one.
///
/// For the main thread, once `AppKit` is up: the closure `Application::run`
/// calls back is that, and by then the Dock tile exists to paint. Anywhere
/// else this does nothing.
pub fn install() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    if bundle_names_an_icon() {
        return;
    }
    let data = NSData::with_bytes(embedded(draws_liquid_glass()));
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: the setter is main-thread only, which `mtm` proves, and takes
    // an image AppKit retains for itself.
    unsafe { app.setApplicationIconImage(Some(&image)) };
}

/// Whether the main bundle names an icon for macOS to draw. Outside a bundle
/// `NSBundle.mainBundle` is the executable's directory and names nothing.
fn bundle_names_an_icon() -> bool {
    let bundle = NSBundle::mainBundle();
    ["CFBundleIconName", "CFBundleIconFile"].iter().any(|key| {
        bundle
            .objectForInfoDictionaryKey(&NSString::from_str(key))
            .is_some()
    })
}

/// Whether this Mac draws app icons in Liquid Glass: macOS 26 and later,
/// which is Darwin 25.
///
/// The kernel release, not `NSProcessInfo`: macOS 26 reports itself as 16.0
/// to a binary linked against an older SDK, which is what a Mac with the
/// Command Line Tools or Xcode 16 builds.
fn draws_liquid_glass() -> bool {
    darwin_major(&rustix::system::uname().release().to_string_lossy())
        .is_some_and(|major| major >= 25)
}

/// The major version of a Darwin release such as `25.6.0`.
fn darwin_major(release: &str) -> Option<u32> {
    release.split('.').next()?.parse().ok()
}

/// The icon for the look this Mac gives its icons.
const fn embedded(liquid_glass: bool) -> &'static [u8] {
    if liquid_glass { GLASS } else { FLAT }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both files decode, and to the size the renderer promises: the Dock
    /// draws at most 128 pt, twice that on a Retina display.
    #[test]
    fn both_dock_icons_are_512_pixel_pngs() {
        for liquid_glass in [false, true] {
            let bytes = embedded(liquid_glass);
            assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
            let image = NSImage::initWithData(
                NSImage::alloc(),
                &NSData::with_bytes(bytes),
            )
            .expect("a decodable PNG");
            let size = image.size();
            assert_eq!((size.width, size.height), (512., 512.));
        }
        // The glass one is a render of the flat one, never the same file.
        assert_ne!(embedded(true), embedded(false));
    }

    #[test]
    fn darwin_25_is_the_first_to_draw_liquid_glass() {
        assert_eq!(darwin_major("25.6.0"), Some(25));
        assert_eq!(darwin_major("24.6.0"), Some(24));
        assert_eq!(darwin_major(""), None);
    }
}
