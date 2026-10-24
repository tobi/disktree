//! The window's title strip, and the frame around the window where disktree
//! has to draw one.
//!
//! The native title bar is gone wherever the platform lets it go: the strip
//! is the title bar, the masthead and the view settings in one band. Each
//! end of it may hold window controls — the stoplights on the left on
//! macOS, painted caption buttons on the right on Windows, and, under a Linux
//! compositor that leaves the frame to the client, whichever buttons its
//! `button-layout` asks for on whichever side it names. That is the one
//! platform decision: [`Frame`] says what each end holds, and the strip lays
//! the brand and its contents out between them.
//!
//! The window title is still set and kept current (see `state.rs`): the
//! Window menu, Mission Control, the taskbar and Waybar read it. It is only
//! no longer painted.

use std::sync::{Arc, LazyLock};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, CursorStyle, Decorations, Div, Edges, FontWeight,
    Hsla, Image, ImageFormat, InteractiveElement as _, IntoElement as _,
    MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement as _, Pixels,
    ResizeEdge, Stateful, StatefulInteractiveElement as _, Styled as _, Tiling,
    Window, WindowButton, WindowButtonLayout, WindowControlArea,
    WindowControls, div, img, px, rgb, svg, white,
};
use gpui_omarchy::{ActiveTheme as _, IconName, Theme, with_tooltip};

use crate::palette;
use crate::state::Disktree;
use crate::ui::{chrome, space};

/// Which system the window is on, as far as its frame is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Windows,
    Linux,
}

impl Platform {
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// What one end of the strip holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Controls {
    /// Nothing: the strip's own padding.
    None,
    /// The macOS stoplights. The system draws them; the strip leaves room.
    Stoplights,
    /// Window buttons disktree paints, in this order.
    Buttons(Vec<WindowButton>),
}

/// The window's frame: what each end of the strip holds, and whether
/// disktree draws the edges too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub platform: Platform,
    pub leading: Controls,
    pub trailing: Controls,
    /// Set when disktree draws the whole frame — a Linux compositor that
    /// offers no server-side decorations, such as GNOME's — with the sides
    /// the compositor has tiled, which keep no resize ring.
    pub client_side: Option<Tiling>,
}

/// GPUI's own fallback when the desktop's setting cannot be read: every
/// button, on the right. It only ever reports none off Linux.
const FALLBACK_LAYOUT: WindowButtonLayout = WindowButtonLayout {
    left: [None; 3],
    right: [
        Some(WindowButton::Minimize),
        Some(WindowButton::Maximize),
        Some(WindowButton::Close),
    ],
};

impl Frame {
    /// The frame this window has now.
    pub fn detect(window: &Window, cx: &gpui_kit::App) -> Self {
        Self::new(
            Platform::current(),
            window.window_decorations(),
            cx.button_layout(),
            window.window_controls(),
            window.is_fullscreen(),
        )
    }

    pub fn new(
        platform: Platform,
        decorations: Decorations,
        layout: Option<WindowButtonLayout>,
        controls: WindowControls,
        fullscreen: bool,
    ) -> Self {
        let none = Self {
            platform,
            leading: Controls::None,
            trailing: Controls::None,
            client_side: None,
        };
        match platform {
            // In full screen AppKit takes the stoplights away with the menu
            // bar, and the room kept for them would be a dead gap.
            Platform::MacOs if fullscreen => none,
            Platform::MacOs => Self {
                leading: Controls::Stoplights,
                ..none
            },
            // GPUI removes the caption and draws no buttons in its place.
            Platform::Windows => Self {
                trailing: Controls::Buttons(vec![
                    WindowButton::Minimize,
                    WindowButton::Maximize,
                    WindowButton::Close,
                ]),
                ..none
            },
            // Hyprland and KDE grant server-side decorations: Hyprland draws
            // no frame by design and KDE draws its own, so the strip keeps
            // out of both.
            Platform::Linux => match decorations {
                Decorations::Server => none,
                Decorations::Client { tiling } => {
                    let layout = layout.unwrap_or(FALLBACK_LAYOUT);
                    // A button for something the compositor will not do
                    // would do nothing; close is always honoured.
                    let side = |buttons: [Option<WindowButton>; 3]| {
                        let shown: Vec<_> = buttons
                            .into_iter()
                            .flatten()
                            .filter(|button| match button {
                                WindowButton::Minimize => controls.minimize,
                                WindowButton::Maximize => controls.maximize,
                                WindowButton::Close => true,
                            })
                            .collect();
                        if shown.is_empty() {
                            Controls::None
                        } else {
                            Controls::Buttons(shown)
                        }
                    };
                    Self {
                        leading: side(layout.left),
                        trailing: side(layout.right),
                        client_side: Some(tiling),
                        ..none
                    }
                }
            },
        }
    }

    /// The ring kept on each side for the shadow and the resize grip: none
    /// on a side the compositor has tiled, nor anywhere it draws the frame.
    fn ring(&self) -> Edges<Pixels> {
        let open = |tiled: bool| {
            if tiled { px(0.) } else { chrome::RESIZE_RING }
        };
        self.client_side
            .map_or_else(Edges::default, |tiling| Edges {
                top: open(tiling.top),
                right: open(tiling.right),
                bottom: open(tiling.bottom),
                left: open(tiling.left),
            })
    }

    /// How far the content sits inside the window on each side: the ring,
    /// then the hairline border, where disktree draws the frame. The
    /// pointer and the window's size come in window coordinates, and
    /// anything placed against them in the content has to take this off.
    pub fn content_edges(&self) -> Edges<Pixels> {
        let border = if self.client_side.is_some() {
            px(1.)
        } else {
            px(0.)
        };
        let ring = self.ring();
        Edges {
            top: ring.top + border,
            right: ring.right + border,
            bottom: ring.bottom + border,
            left: ring.left + border,
        }
    }

    /// Whether dragging the strip has to move the window by asking for it.
    /// Windows does it from the caption area it is told about, and a
    /// compositor that draws the frame has its own way to move a window.
    const fn moves_itself(&self) -> bool {
        match self.platform {
            Platform::MacOs => true,
            Platform::Windows => false,
            Platform::Linux => self.client_side.is_some(),
        }
    }
}

/// The app icon, from the same file the Dock, the desktop entry and the
/// installer use, so the window shows the mark people already know.
static MARK: LazyLock<Arc<Image>> = LazyLock::new(|| {
    Arc::new(Image::from_bytes(
        ImageFormat::Svg,
        include_bytes!("../../../assets/disktree.svg").to_vec(),
    ))
});

/// Windows' own close-button red, which it uses whatever the theme.
const CLOSE_RED: u32 = 0x00c4_2b1c;

/// The strip: window controls at either end, the brand, then `content` —
/// the trail and the settings on explore, nothing on the other screens,
/// which carry their own heading.
pub fn strip(
    frame: &Frame,
    wordmark: bool,
    content: impl gpui_kit::IntoElement,
    window: &Window,
    cx: &Context<'_, Disktree>,
) -> Stateful<Div> {
    let theme = cx.omarchy().clone();
    let maximized = window.is_maximized();
    let leading = controls(
        frame,
        &frame.leading,
        "window-controls-leading",
        maximized,
        &theme,
        cx,
    );
    let trailing = controls(
        frame,
        &frame.trailing,
        "window-controls-trailing",
        maximized,
        &theme,
        cx,
    );
    div()
        .id("title-strip")
        .debug_selector(|| "title-strip".into())
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        // The band, then the divider under it: a divider inside the band
        // would leave an odd height the stoplights cannot centre in.
        .min_h(chrome::STRIP + px(1.))
        .border_b_1()
        .border_color(theme.divider())
        .when(frame.trailing == Controls::None, |strip| strip.pr(space::LG))
        .children(leading)
        // The brand replaces the old logo's source picker action. Keep
        // its full-height padding clickable so the target has no dead edge.
        .child(with_tooltip(
            div()
                .id("strip-brand")
                .debug_selector(|| "strip-brand".into())
                .flex()
                .items_center()
                .self_stretch()
                .flex_shrink_0()
                .cursor_pointer()
                .when(frame.leading == Controls::None, |area| {
                    area.pl(space::LG)
                })
                .pr(space::LG)
                .child(brand(wordmark, &theme))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_volumes(cx);
                    this.apply_focus(window, cx);
                })),
            "Choose local or SSH source · v",
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .self_stretch()
                .flex_1()
                .min_w_0()
                .when(frame.trailing != Controls::None, |row| {
                    row.mr(space::SM)
                })
                .child(content),
        )
        .children(trailing)
}

/// Part of the strip that moves the window: everything that is not a
/// control. On Windows the system is told where its caption is and does
/// the rest — moving, snapping, maximizing on a double click and the system
/// menu on a right click.
pub fn drag_area(
    frame: &Frame,
    id: &'static str,
    cx: &Context<'_, Disktree>,
) -> Stateful<Div> {
    let area = div().id(id).debug_selector(move || id.into());
    let platform = frame.platform;
    if platform == Platform::Windows {
        return area.window_control_area(WindowControlArea::Drag);
    }
    if !frame.moves_itself() {
        return area;
    }
    area.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, event: &MouseDownEvent, window, _| {
            this.strip_drag = event.click_count < 2;
            if event.click_count >= 2 {
                if platform == Platform::MacOs {
                    // Zoom, minimize or nothing, as System Settings says.
                    window.titlebar_double_click();
                } else {
                    window.zoom_window();
                }
            }
        }),
    )
    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, _| {
        // The move starts on the first motion, not on the press, so that
        // a second press can still arrive as a double click.
        if this.strip_drag && event.pressed_button == Some(MouseButton::Left) {
            this.strip_drag = false;
            window.start_window_move();
        }
    }))
    .on_mouse_up(
        MouseButton::Left,
        cx.listener(|this, _, _, _| this.strip_drag = false),
    )
    // Motion only reaches the area while the pointer is over it, so a
    // press that leaves before its first motion is released elsewhere; it
    // must not stay armed for the next drag that crosses the strip.
    .on_mouse_up_out(
        MouseButton::Left,
        cx.listener(|this, _, _, _| this.strip_drag = false),
    )
    .when(platform == Platform::Linux, |area| {
        area.on_mouse_down(MouseButton::Right, |event, window, _| {
            window.show_window_menu(event.position);
        })
    })
}

/// The icon and the name, centred on the band.
fn brand(wordmark: bool, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(chrome::LOCKUP_GAP)
        .child(img(MARK.clone()).size(chrome::MARK).flex_shrink_0())
        .when(wordmark, |brand| {
            brand.child(
                div()
                    .text_size(chrome::WORDMARK)
                    .line_height(chrome::WORDMARK)
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.bright)
                    .whitespace_nowrap()
                    .child("disktree"),
            )
        })
}

/// One end of the strip.
fn controls(
    frame: &Frame,
    controls: &Controls,
    selector: &'static str,
    maximized: bool,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Option<AnyElement> {
    match controls {
        Controls::None => None,
        Controls::Stoplights => Some(
            drag_area(frame, selector, cx)
                .w(chrome::STOPLIGHT_ZONE)
                .h(chrome::STRIP)
                .flex_shrink_0()
                .into_any_element(),
        ),
        Controls::Buttons(buttons) => {
            let row = div()
                .id(selector)
                .debug_selector(move || selector.into())
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0();
            let row =
                if frame.platform == Platform::Windows {
                    row.children(buttons.iter().map(|&button| {
                        caption_button(button, maximized, theme)
                    }))
                } else {
                    row.gap(space::SM).px(space::SM).children(
                        buttons.iter().map(|&button| {
                            window_button(button, maximized, theme)
                        }),
                    )
                };
            Some(row.into_any_element())
        }
    }
}

/// A Windows caption button. It has no click handler: the system hit-tests
/// it as the real button, so hovering maximize offers snap layouts and the
/// press does what Windows does.
fn caption_button(
    button: WindowButton,
    maximized: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let group = button.id();
    let area = match button {
        WindowButton::Minimize => WindowControlArea::Min,
        WindowButton::Maximize => WindowControlArea::Max,
        WindowButton::Close => WindowControlArea::Close,
    };
    let close = button == WindowButton::Close;
    let hover = theme.hover_fill();
    let ink = theme.foreground;
    let lit = if close { white() } else { ink };
    div()
        .id(button.id())
        .group(group)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .w(chrome::CAPTION_BUTTON)
        .h(chrome::STRIP)
        .window_control_area(area)
        .hover(move |style| {
            if close {
                style.bg(rgb(CLOSE_RED))
            } else {
                style.bg(hover)
            }
        })
        .child(glyph(button, maximized, ink, group, lit))
}

/// A painted Linux window button. Square, like every other surface here.
fn window_button(
    button: WindowButton,
    maximized: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let group = button.id();
    let (rest, hover) = palette::window_button(theme);
    div()
        .id(button.id())
        .group(group)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .size(chrome::WINDOW_BUTTON)
        .bg(rest)
        .hover(move |style| style.bg(hover))
        .on_click(move |_, window, _| match button {
            WindowButton::Minimize => window.minimize_window(),
            WindowButton::Maximize => window.zoom_window(),
            WindowButton::Close => window.remove_window(),
        })
        .child(glyph(button, maximized, theme.bright, group, theme.bright))
}

/// The mark on a window button, drawn rather than taken from a font so it
/// looks the same everywhere; `lit` is its colour while `group` is hovered.
fn glyph(
    button: WindowButton,
    maximized: bool,
    ink: Hsla,
    group: &'static str,
    lit: Hsla,
) -> AnyElement {
    match button {
        WindowButton::Minimize => div()
            .w(chrome::GLYPH)
            .h(px(1.))
            .bg(ink)
            .group_hover(group, move |style| style.bg(lit))
            .into_any_element(),
        // Restore: the window it would go back to, behind the one it is.
        WindowButton::Maximize if maximized => div()
            .relative()
            .size(chrome::GLYPH)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .size(chrome::GLYPH_BEHIND)
                    .border_t_1()
                    .border_r_1()
                    .border_color(ink),
            )
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .size(chrome::GLYPH_BEHIND)
                    .border_1()
                    .border_color(ink),
            )
            .into_any_element(),
        WindowButton::Maximize => div()
            .size(chrome::GLYPH)
            .border_1()
            .border_color(ink)
            .group_hover(group, move |style| style.border_color(lit))
            .into_any_element(),
        WindowButton::Close => svg()
            .path(IconName::Close.path())
            .size(chrome::CLOSE_GLYPH)
            .text_color(ink)
            .group_hover(group, move |style| style.text_color(lit))
            .into_any_element(),
    }
}

/// Wrap the window's content in the frame a client-side window needs: a
/// ring for the shadow and the resize grip on every side the compositor has
/// not tiled, and a hairline border. Anywhere else the content is the whole
/// window.
pub fn client_frame(
    frame: &Frame,
    content: Stateful<Div>,
    window: &mut Window,
    theme: &Theme,
) -> AnyElement {
    let Some(tiling) = frame.client_side else {
        // A compositor that takes the frame back takes the whole surface:
        // an inset left over would draw its frame inside the shadow ring.
        if window.client_inset().is_some_and(|inset| inset > px(0.)) {
            window.set_client_inset(px(0.));
        }
        return content.into_any_element();
    };
    let ring = chrome::RESIZE_RING;
    if window.client_inset() != Some(ring) {
        window.set_client_inset(ring);
    }
    let open = frame.ring();
    let mut outer = div()
        .id("client-frame")
        .debug_selector(|| "client-frame".into())
        .relative()
        .size_full()
        .pt(open.top)
        .pb(open.bottom)
        .pl(open.left)
        .pr(open.right)
        .child(
            div()
                .size_full()
                .border_1()
                .border_color(theme.border)
                .when(!tiling.is_tiled(), gpui_kit::Styled::shadow_lg)
                .child(content),
        );
    for (edge, tiled) in [
        (ResizeEdge::Top, tiling.top),
        (ResizeEdge::Bottom, tiling.bottom),
        (ResizeEdge::Left, tiling.left),
        (ResizeEdge::Right, tiling.right),
    ] {
        if !tiled {
            outer = outer.child(grip(edge, ring));
        }
    }
    for (edge, a, b) in [
        (ResizeEdge::TopLeft, tiling.top, tiling.left),
        (ResizeEdge::TopRight, tiling.top, tiling.right),
        (ResizeEdge::BottomLeft, tiling.bottom, tiling.left),
        (ResizeEdge::BottomRight, tiling.bottom, tiling.right),
    ] {
        if !a && !b {
            outer = outer.child(grip(edge, ring));
        }
    }
    outer.into_any_element()
}

/// One resize handle in the ring, with the cursor that says so.
fn grip(edge: ResizeEdge, ring: Pixels) -> Stateful<Div> {
    let (cursor, id) = match edge {
        ResizeEdge::Top => (CursorStyle::ResizeUpDown, "grip-top"),
        ResizeEdge::Bottom => (CursorStyle::ResizeUpDown, "grip-bottom"),
        ResizeEdge::Left => (CursorStyle::ResizeLeftRight, "grip-left"),
        ResizeEdge::Right => (CursorStyle::ResizeLeftRight, "grip-right"),
        ResizeEdge::TopLeft => {
            (CursorStyle::ResizeUpLeftDownRight, "grip-top-left")
        }
        ResizeEdge::BottomRight => {
            (CursorStyle::ResizeUpLeftDownRight, "grip-bottom-right")
        }
        ResizeEdge::TopRight => {
            (CursorStyle::ResizeUpRightDownLeft, "grip-top-right")
        }
        ResizeEdge::BottomLeft => {
            (CursorStyle::ResizeUpRightDownLeft, "grip-bottom-left")
        }
    };
    let grip = div()
        .id(id)
        .debug_selector(move || id.into())
        .absolute()
        .cursor(cursor)
        .on_mouse_down(MouseButton::Left, move |_, window, _| {
            window.start_window_resize(edge);
        });
    // Edges run between the corners; corners are a ring-sized square.
    match edge {
        ResizeEdge::Top => grip.top_0().left(ring).right(ring).h(ring),
        ResizeEdge::Bottom => grip.bottom_0().left(ring).right(ring).h(ring),
        ResizeEdge::Left => grip.left_0().top(ring).bottom(ring).w(ring),
        ResizeEdge::Right => grip.right_0().top(ring).bottom(ring).w(ring),
        ResizeEdge::TopLeft => grip.top_0().left_0().size(ring),
        ResizeEdge::TopRight => grip.top_0().right_0().size(ring),
        ResizeEdge::BottomLeft => grip.bottom_0().left_0().size(ring),
        ResizeEdge::BottomRight => grip.bottom_0().right_0().size(ring),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(
        platform: Platform,
        decorations: Decorations,
        layout: Option<WindowButtonLayout>,
    ) -> Frame {
        Frame::new(
            platform,
            decorations,
            layout,
            WindowControls::default(),
            false,
        )
    }

    const CLIENT: Decorations = Decorations::Client {
        tiling: Tiling {
            top: false,
            left: false,
            right: false,
            bottom: false,
        },
    };

    #[test]
    fn macos_leaves_room_for_the_stoplights_on_the_left() {
        let mac = frame(Platform::MacOs, Decorations::Server, None);
        assert_eq!(mac.leading, Controls::Stoplights);
        assert_eq!(mac.trailing, Controls::None);
        assert!(mac.client_side.is_none());
        let full = Frame::new(
            Platform::MacOs,
            Decorations::Server,
            None,
            WindowControls::default(),
            true,
        );
        assert_eq!(full.leading, Controls::None, "no dead gap in full screen");
    }

    #[test]
    fn windows_paints_its_caption_buttons_on_the_right() {
        let windows = frame(Platform::Windows, Decorations::Server, None);
        assert_eq!(windows.leading, Controls::None);
        assert_eq!(
            windows.trailing,
            Controls::Buttons(vec![
                WindowButton::Minimize,
                WindowButton::Maximize,
                WindowButton::Close,
            ])
        );
    }

    #[test]
    fn a_linux_compositor_that_draws_the_frame_gets_no_controls() {
        let hyprland = frame(Platform::Linux, Decorations::Server, None);
        assert_eq!(hyprland.leading, Controls::None);
        assert_eq!(hyprland.trailing, Controls::None);
        assert!(hyprland.client_side.is_none());
    }

    #[test]
    fn a_client_side_frame_follows_the_desktops_button_layout() {
        // GNOME's "close,minimize:maximize": two on the left, one on the
        // right, in that order.
        let layout = WindowButtonLayout {
            left: [
                Some(WindowButton::Close),
                Some(WindowButton::Minimize),
                None,
            ],
            right: [Some(WindowButton::Maximize), None, None],
        };
        let gnome = frame(Platform::Linux, CLIENT, Some(layout));
        assert_eq!(
            gnome.leading,
            Controls::Buttons(vec![
                WindowButton::Close,
                WindowButton::Minimize
            ])
        );
        assert_eq!(
            gnome.trailing,
            Controls::Buttons(vec![WindowButton::Maximize])
        );
        assert!(gnome.client_side.is_some());

        // Unreadable: every button on the right.
        let fallback = frame(Platform::Linux, CLIENT, None);
        assert_eq!(fallback.leading, Controls::None);
        assert_eq!(
            fallback.trailing,
            Controls::Buttons(vec![
                WindowButton::Minimize,
                WindowButton::Maximize,
                WindowButton::Close,
            ])
        );
    }

    #[test]
    fn buttons_for_what_the_compositor_will_not_do_are_left_out() {
        let controls = WindowControls {
            fullscreen: false,
            maximize: false,
            minimize: false,
            window_menu: true,
        };
        let frame = Frame::new(Platform::Linux, CLIENT, None, controls, false);
        assert_eq!(
            frame.trailing,
            Controls::Buttons(vec![WindowButton::Close])
        );
    }
}
