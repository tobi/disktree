//! What the windows other than the main one share: Settings, Help's
//! documents and Logs. Their options (the same frame as the main window:
//! no native title bar, the stoplights in the strip on macOS), one window
//! of each kind (a second open brings it forward), and the frame around
//! their content.

use gpui_kit::{
    AnyElement, App, AppContext as _, Bounds, Context, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Styled as _, TitlebarOptions,
    Window, WindowBounds, WindowHandle, WindowOptions, div, point, px, size,
};
use gpui_omarchy::ActiveTheme as _;

use crate::chrome::{self, Drags, Frame};
use crate::semantics;
use crate::ui;
use gpui_kit::KeyDownEvent;
use gpui_kit::base::input::{IndentInline, OutdentInline};

/// A window's title (the Window menu, the taskbar and Waybar read it), its
/// size and its least size.
pub struct Spec {
    pub title: &'static str,
    pub size: (f32, f32),
    pub min: (f32, f32),
}

/// Brings `existing` forward if it is still open; `true` when it was.
pub fn activate<V: Render + 'static>(
    existing: Option<WindowHandle<V>>,
    cx: &mut App,
) -> bool {
    let Some(handle) = existing else {
        return false;
    };
    let shown = handle
        .update(cx, |_, window, _| window.activate_window())
        .is_ok();
    if shown {
        cx.activate(true);
    }
    shown
}

/// The open window of kind `V` that `matches`, if any.
pub fn find<V: Render + 'static>(
    cx: &App,
    matches: impl Fn(&V) -> bool,
) -> Option<WindowHandle<V>> {
    cx.windows().into_iter().find_map(|window| {
        let handle = window.downcast::<V>()?;
        let view = handle.read(cx).ok()?;
        matches(view).then_some(handle)
    })
}

/// Opens a window in front, centred on the display the user is working
/// on: the main window's, when it is in front.
pub fn open<V: Render + 'static>(
    spec: &Spec,
    cx: &mut App,
    build: impl FnOnce(&mut Window, &mut Context<'_, V>) -> V,
) -> Option<WindowHandle<V>> {
    let display = crate::placement::display_for_new_window(cx);
    let bounds =
        Bounds::centered(display, size(px(spec.size.0), px(spec.size.1)), cx);
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // GPUI reads the bounds relative to this display.
            display_id: display,
            titlebar: Some(TitlebarOptions {
                title: Some(spec.title.into()),
                // As the main window: the strip is the title bar.
                appears_transparent: true,
                traffic_light_position: Some(point(
                    px(ui::chrome::STOPLIGHTS.0),
                    px(ui::chrome::STOPLIGHTS.1),
                )),
            }),
            app_owns_titlebar_drag: true,
            app_id: Some("pwrdisk".to_owned()),
            window_min_size: Some(size(px(spec.min.0), px(spec.min.1))),
            ..Default::default()
        },
        |window, cx| {
            ui::sync_scale(window, cx);
            if crate::appearance::follows_system(
                std::env::home_dir().as_deref(),
            ) {
                crate::appearance::follow(window);
            }
            cx.new(|cx| {
                cx.observe_button_layout_changed(window, |_, _, cx| {
                    cx.notify();
                })
                .detach();
                build(window, cx)
            })
        },
    );
    match handle {
        Ok(handle) => {
            cx.activate(true);
            Some(handle)
        }
        Err(error) => {
            log::warn!(
                target: "pwrdisk",
                "could not open the {} window: {error}",
                spec.title
            );
            None
        }
    }
}

/// The window: the strip with `crumbs` and `actions`, `body` under it, in
/// the frame the platform needs.
pub fn frame<V: Drags>(
    crumbs: &[&'static str],
    actions: impl IntoElement,
    body: impl IntoElement,
    window: &mut Window,
    cx: &Context<'_, V>,
) -> AnyElement {
    let frame = Frame::detect(window, cx);
    let theme = cx.omarchy().clone();
    let content = div()
        .id("aux-root")
        // Tab walks every control, the title strip's buttons included, so
        // it is taken here, above them all, rather than by each window's
        // body. A text field binds Tab to indenting, and bindings run
        // before key handlers, so its actions are caught too. Interface
        // zoom is taken here for the same reason: every window gets it.
        .capture_key_down(|event: &KeyDownEvent, window, cx| {
            if ui::zoom_interface(event, window, cx) {
                cx.stop_propagation();
                return;
            }
            let keys = &event.keystroke;
            if keys.key == "tab"
                && !keys.modifiers.control
                && !keys.modifiers.platform
            {
                semantics::advance_focus(window, cx, keys.modifiers.shift, None);
                cx.stop_propagation();
            }
        })
        .capture_action(|_: &IndentInline, window, cx| {
            semantics::advance_focus(window, cx, false, None);
            cx.stop_propagation();
        })
        .capture_action(|_: &OutdentInline, window, cx| {
            semantics::advance_focus(window, cx, true, None);
            cx.stop_propagation();
        })
        .flex()
        .flex_col()
        .size_full()
        .bg(theme.background)
        .text_color(theme.foreground)
        .font_family(theme.font.clone())
        .text_size(ui::text::BODY)
        .child(chrome::aux_strip(&frame, crumbs, actions, window, cx))
        .child(div().flex().flex_col().flex_1().min_h_0().child(body));
    chrome::client_frame(&frame, content, window, &theme)
}
