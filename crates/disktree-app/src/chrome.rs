//! The window's own chrome, for compositors that leave it to the app.
//!
//! disktree asks for server-side decorations, GPUI's default. Hyprland, KDE
//! and most wlroots compositors, every X11 window manager, macOS and Windows
//! honour that and draw the title bar themselves; disktree then draws nothing
//! extra. GNOME's Mutter does not offer server-side decorations to Wayland
//! apps, so there each screen's header becomes the title bar: a drag on it
//! moves the window, a double-click maximizes or restores, a right-click opens
//! the compositor's window menu, the controls at its right end minimize,
//! maximize and close, and the window's free edges and corners resize it.

use gpui_kit::{
    AnyElement, Context, CursorStyle, Decorations, Div,
    InteractiveElement as _, IntoElement as _, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement as _, Pixels, Point, ResizeEdge, Stateful,
    StatefulInteractiveElement as _, Styled as _, Tiling, Window,
    WindowControls, div, px,
};
use gpui_omarchy::{ActiveTheme as _, IconName};

use crate::state::Disktree;
use crate::ui::{icon, size};

/// How far, in pixels, a press on the header travels before it becomes a
/// window move. A hand holding a mouse still jitters by a pixel or two
/// between press and release, and that click must stay a click.
const MOVE_THRESHOLD: f64 = 4.;

/// The click count of a double-click, which maximizes or restores.
const DOUBLE_CLICK: usize = 2;

/// Resize handles are targets for the pointer, physical like a hairline, so
/// they stay in pixels while the interface zooms: the edges this thick, the
/// corners this square.
const EDGE: Pixels = px(6.);
const CORNER: Pixels = px(14.);

/// What the compositor left for the app to draw, read from the window each
/// frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The edges held against a screen edge or another window, which do not
    /// resize.
    pub tiling: Tiling,
    /// The controls the compositor can carry out.
    pub controls: WindowControls,
    pub maximized: bool,
}

impl Frame {
    /// The chrome `window` needs drawn, or `None` when the compositor draws
    /// its own.
    pub fn of(window: &Window) -> Option<Self> {
        match window.window_decorations() {
            Decorations::Server => None,
            Decorations::Client { tiling } => Some(Self {
                tiling,
                controls: window.window_controls(),
                maximized: window.is_maximized(),
            }),
        }
    }

    /// The controls to draw, left to right. A tiling compositor may support
    /// neither minimize nor maximize; close is always the app's to offer.
    fn shown(self) -> Vec<Control> {
        let mut shown = Vec::new();
        if self.controls.minimize {
            shown.push(Control::Minimize);
        }
        if self.controls.maximize {
            shown.push(if self.maximized {
                Control::Restore
            } else {
                Control::Maximize
            });
        }
        shown.push(Control::Close);
        shown
    }

    /// The edges and corners free to resize, edges first so the corners are
    /// drawn over them. A maximized window has none; a tiled edge, and any
    /// corner touching one, is held in place.
    fn free_edges(self) -> Vec<ResizeEdge> {
        if self.maximized {
            return Vec::new();
        }
        let tiling = self.tiling;
        [
            (ResizeEdge::Top, tiling.top),
            (ResizeEdge::Bottom, tiling.bottom),
            (ResizeEdge::Left, tiling.left),
            (ResizeEdge::Right, tiling.right),
            (ResizeEdge::TopLeft, tiling.top || tiling.left),
            (ResizeEdge::TopRight, tiling.top || tiling.right),
            (ResizeEdge::BottomLeft, tiling.bottom || tiling.left),
            (ResizeEdge::BottomRight, tiling.bottom || tiling.right),
        ]
        .into_iter()
        .filter(|&(_, held)| !held)
        .map(|(edge, _)| edge)
        .collect()
    }
}

/// A window control's job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Minimize,
    Maximize,
    Restore,
    Close,
}

impl Control {
    /// The element id, which the tests find the control by.
    const fn id(self) -> &'static str {
        match self {
            Self::Minimize => "window-minimize",
            Self::Maximize => "window-maximize",
            Self::Restore => "window-restore",
            Self::Close => "window-close",
        }
    }

    const fn icon(self) -> IconName {
        match self {
            Self::Minimize => IconName::WindowMinimize,
            Self::Maximize => IconName::WindowMaximize,
            Self::Restore => IconName::WindowRestore,
            Self::Close => IconName::WindowClose,
        }
    }

    fn run(self, window: &mut Window) {
        match self {
            Self::Minimize => window.minimize_window(),
            Self::Maximize | Self::Restore => window.zoom_window(),
            Self::Close => window.remove_window(),
        }
    }
}

/// Whether a press at `from` that is now at `to` has become a drag.
fn travelled(from: Point<Pixels>, to: Point<Pixels>) -> bool {
    (to - from).magnitude() > MOVE_THRESHOLD
}

/// Makes `header` the window's title bar when the app draws its own chrome,
/// and ends it with the window controls. Under the compositor's title bar,
/// `header` keeps only its id.
///
/// The header's buttons go inside [`no_drag`], so a press on one stays the
/// button's: a double-click on a crumb must not maximize the window.
pub fn titlebar(
    header: Div,
    frame: Option<Frame>,
    cx: &Context<'_, Disktree>,
) -> Stateful<Div> {
    let header = header.id("titlebar").debug_selector(|| "titlebar".into());
    let Some(frame) = frame else {
        return header;
    };

    // A press arms a move, and the pointer travelling past the threshold
    // starts it, so a click or a double-click never becomes a move.
    header
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event: &MouseDownEvent, window, _| {
                if event.click_count == DOUBLE_CLICK {
                    this.titlebar_press = None;
                    window.zoom_window();
                    return;
                }
                this.titlebar_press = Some(event.position);
            }),
        )
        .on_mouse_down(MouseButton::Right, |event, window, _| {
            window.show_window_menu(event.position);
        })
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _, _, _| this.titlebar_press = None),
        )
        .on_mouse_move(cx.listener(
            |this, event: &MouseMoveEvent, window, _| {
                let Some(from) = this.titlebar_press else {
                    return;
                };

                // Released somewhere the header did not see: nothing to move.
                if !event.dragging() {
                    this.titlebar_press = None;
                    return;
                }
                if !travelled(from, event.position) {
                    return;
                }
                this.titlebar_press = None;
                window.start_window_move();
            },
        ))
        .child(controls(frame, cx))
}

/// Keeps presses inside `group` away from the title bar, when the app draws
/// one: the buttons in `group` still see them, the header does not.
pub fn no_drag(group: Div, frame: Option<Frame>) -> Div {
    if frame.is_none() {
        return group;
    }
    group
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
}

/// Minimize, maximize or restore, and close: square, like every Omarchy
/// surface, and close turns the danger colour under the pointer.
fn controls(frame: Frame, cx: &Context<'_, Disktree>) -> Div {
    let theme = cx.omarchy().clone();
    let row = div().flex().flex_row().flex_none().items_center();
    let row = no_drag(row, Some(frame));
    row.children(frame.shown().into_iter().map(|control| {
        let (hover_bg, hover_fg) = if control == Control::Close {
            (theme.danger, theme.background)
        } else {
            (theme.hover_fill(), theme.bright)
        };
        div()
            .id(control.id())
            .debug_selector(move || control.id().into())
            .size(size::WINDOW_CONTROL)
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_color(theme.secondary)
            .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                control.run(window);
            })
            .child(gpui_omarchy::icon(control.icon()).size(icon::MD))
    }))
}

/// Invisible handles along the window's free edges and corners that resize
/// it, when the app draws its own chrome. The root lays them over
/// everything else.
pub fn resize_edges(frame: Option<Frame>) -> Option<AnyElement> {
    let edges = frame?.free_edges();
    if edges.is_empty() {
        return None;
    }
    let handles = edges.into_iter().map(|edge| {
        let handle = div()
            .id(edge_id(edge))
            .debug_selector(move || edge_id(edge).into())
            .absolute()
            .occlude()
            .cursor(edge_cursor(edge))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                window.start_window_resize(edge);
            });
        match edge {
            ResizeEdge::Top => handle.top_0().left_0().w_full().h(EDGE),
            ResizeEdge::Bottom => handle.bottom_0().left_0().w_full().h(EDGE),
            ResizeEdge::Left => handle.top_0().left_0().h_full().w(EDGE),
            ResizeEdge::Right => handle.top_0().right_0().h_full().w(EDGE),
            ResizeEdge::TopLeft => handle.top_0().left_0().size(CORNER),
            ResizeEdge::TopRight => handle.top_0().right_0().size(CORNER),
            ResizeEdge::BottomLeft => handle.bottom_0().left_0().size(CORNER),
            ResizeEdge::BottomRight => handle.bottom_0().right_0().size(CORNER),
        }
    });
    Some(
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .children(handles)
            .into_any_element(),
    )
}

/// The element id of the resize handle on `edge`.
const fn edge_id(edge: ResizeEdge) -> &'static str {
    match edge {
        ResizeEdge::Top => "resize-top",
        ResizeEdge::TopRight => "resize-top-right",
        ResizeEdge::Right => "resize-right",
        ResizeEdge::BottomRight => "resize-bottom-right",
        ResizeEdge::Bottom => "resize-bottom",
        ResizeEdge::BottomLeft => "resize-bottom-left",
        ResizeEdge::Left => "resize-left",
        ResizeEdge::TopLeft => "resize-top-left",
    }
}

/// The pointer over the resize handle on `edge`.
const fn edge_cursor(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
            CursorStyle::ResizeUpLeftDownRight
        }
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
            CursorStyle::ResizeUpRightDownLeft
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free() -> Frame {
        Frame {
            tiling: Tiling::default(),
            controls: WindowControls::default(),
            maximized: false,
        }
    }

    /// The controls follow what the compositor offers and whether the
    /// window is maximized, by reading `shown` for each case.
    #[test]
    fn the_controls_follow_what_the_compositor_offers() {
        use Control::{Close, Maximize, Minimize, Restore};

        assert_eq!(free().shown(), [Minimize, Maximize, Close]);
        let maximized = Frame {
            maximized: true,
            ..free()
        };
        assert_eq!(maximized.shown(), [Minimize, Restore, Close]);
        let tiling_only = Frame {
            controls: WindowControls {
                minimize: false,
                maximize: false,
                ..WindowControls::default()
            },
            ..free()
        };
        assert_eq!(tiling_only.shown(), [Close]);
    }

    /// Only free edges resize: a window tiled on its left keeps its left
    /// edge and both left corners still, and a maximized one keeps all.
    #[test]
    fn only_free_edges_resize() {
        assert_eq!(free().free_edges().len(), 8);
        let left = Frame {
            tiling: Tiling {
                left: true,
                ..Tiling::default()
            },
            ..free()
        };
        assert_eq!(
            left.free_edges(),
            [
                ResizeEdge::Top,
                ResizeEdge::Bottom,
                ResizeEdge::Right,
                ResizeEdge::TopRight,
                ResizeEdge::BottomRight,
            ]
        );
        let maximized = Frame {
            maximized: true,
            ..free()
        };
        assert!(maximized.free_edges().is_empty());
    }

    /// A press becomes a move only past the threshold, so the jitter of a
    /// click does not drag the window.
    #[test]
    fn a_press_moves_the_window_only_past_the_threshold() {
        let from = Point::new(px(100.), px(10.));
        assert!(!travelled(from, Point::new(px(102.), px(11.))));
        assert!(travelled(from, Point::new(px(110.), px(10.))));
    }
}
