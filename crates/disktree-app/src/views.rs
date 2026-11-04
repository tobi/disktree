//! The screens: explore, review, running and done.
//!
//! Each function is a pure reading of [`Disktree`], so what a screen shows is
//! always exactly what the state says — there is no second copy of anything to
//! keep in sync.

use crate::semantics::AccessibleButton as _;

use std::path::Path;

use disktree_core::classify::Category;
use disktree_core::filter::Keep;
use disktree_core::insights::{Candidate, Finding, STALE_DAYS};
use disktree_core::path_style::PathStyle;
use disktree_core::removal::{Plan, RemovalMode, Target};
use disktree_core::size::human_bytes;
use disktree_core::tree::Metric;
use gpui_kit::base::CheckboxState;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Div, DragMoveEvent,
    ElementId, Focusable as _, FontWeight, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseDownEvent, ParentElement, Rems,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled, Window,
    anchored, deferred, div, pattern_slash, px, relative,
};
use gpui_omarchy::{
    ActiveTheme, ButtonVariant, ChoiceItem, Theme, alert_dialog, button,
    button_group, checkbox, dialog_button, dialog_description, dialog_popup,
    dialog_title, with_tooltip,
};

use gpui_kit::prelude::FluentBuilder as _;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::chrome::{self, Frame};
use crate::git_store::{Probe, Shown};
use crate::ledger::Cohort;
use crate::mark_guard::{Gate, MarkCheck, Over, Share};
use crate::palette;
use crate::safety::Confirm;
use crate::state::{
    ColorMode, Crumb, Disktree, PANEL_REMS, Screen, panel_width,
};
use crate::treemap_view::{self, Mosaic};
use crate::ui::{icon, size, space, text};
use crate::widgets;

/// Marks per review page: every mark stays reachable while bounding layout
/// and semantic work independently of the full marked list.
const LIST_LIMIT: usize = 1200;

/// Row decoration borrows this render's freshly evaluated plan. It must
/// not cache removal authorization, and each displayed row needs constant
/// expected hash lookups rather than scans of all blocked/covered targets.
#[derive(Debug)]
struct ReviewStatus<'a> {
    covered: FxHashSet<&'a Path>,
    blocked: FxHashMap<&'a Path, &'a str>,
}

impl<'a> ReviewStatus<'a> {
    fn new(plan: &'a Plan) -> Self {
        let mut status = Self {
            covered: FxHashSet::default(),
            blocked: FxHashMap::default(),
        };
        for target in &plan.covered {
            #[cfg(test)]
            crate::marks::work::REVIEW
                .set(crate::marks::work::REVIEW.get() + 1);
            status.covered.insert(target.path.as_path());
        }
        for target in &plan.blocked {
            #[cfg(test)]
            crate::marks::work::REVIEW
                .set(crate::marks::work::REVIEW.get() + 1);
            // A raw queue may repeat a path. Keep the first explanation,
            // matching the original linear find's behavior.
            status
                .blocked
                .entry(target.path.as_path())
                .or_insert(target.reason.as_str());
        }
        status
    }

    fn covered(&self, path: &Path) -> bool {
        #[cfg(test)]
        crate::marks::work::REVIEW.set(crate::marks::work::REVIEW.get() + 1);
        self.covered.contains(path)
    }

    fn blocked(&self, path: &Path) -> Option<&str> {
        #[cfg(test)]
        crate::marks::work::REVIEW.set(crate::marks::work::REVIEW.get() + 1);
        self.blocked.get(path).copied()
    }
}

/// Below the list-plus-explanation width budget, review becomes one column.
pub const REVIEW_COLUMNS_REMS: f32 = 65.0;

/// The whole window.
pub fn root(
    app: &mut Disktree,
    frame: &Frame,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Stateful<Div> {
    if app.volumes_open {
        app.prepare_picker_inputs(window, cx);
    }
    if app.screen == Screen::Review {
        app.prepare_over_phrase(window, cx);
    }
    app.apply_focus(window, cx);
    let fit = crate::toolbar::BarFit::for_width(
        window.viewport_size().width.as_f32() / app.rem,
    );
    // What ⋮ held at the old width is not what it holds at the new one.
    if fit != app.bar_fit {
        app.more_menu = None;
        // ⋮ may vanish under the pointer, which then never reports leaving.
        app.more_hover = false;
    }
    app.bar_fit = fit;
    // A dialog owns the keys; a menu left open behind it would take them
    // first (see `dispatch_key`).
    if app.has_modal() {
        app.more_menu = None;
        app.power_menu = None;
    }
    let theme = cx.omarchy().clone();
    let readable_status = app.accessibility_status();
    let strip = title_strip(app, frame, window, cx);
    // Each screen anchors the toasts in its content, above its footer, so
    // they never cover the footer's buttons. Dialogs draw over them.
    let toasts = crate::toasts::stack(app, window, cx);
    let body = match app.screen {
        Screen::Explore => explore(app, toasts, window, cx),
        Screen::Review => review(app, toasts, window, cx),
        Screen::Running => crate::run_view::running(app, toasts, window, cx),
        Screen::Done => crate::run_view::done(app, toasts, window, cx),
    };
    let modal = app.has_modal() || app.app_menu_open;
    let background = div().id("application-content")
        .role(gpui_kit::Role::Group).aria_label("Application content")
        .a11y_synthetic_children(move |b| { if modal { b.parent_node().set_hidden(); } })
        .flex().flex_col().flex_1().min_h_0().w_full()
        .child(strip)
        .child(body)
        .children(app.is_indexing().then(|| div().id("scan-progress")
            .role(gpui_kit::Role::ProgressIndicator)
            .aria_label(if app.screen == Screen::Explore { "Scan in progress; indeterminate; overview is incomplete; Escape cancels" } else { "Background scan in progress; indeterminate; overview is incomplete" })
            .a11y_synthetic_children(|b| b.parent_node().set_busy())))
        .child(crate::semantics::status("current-status", readable_status));

    let mut root = div()
        .id("disktree-root")
        .role(gpui_kit::Role::Group)
        .aria_label("Disk usage explorer")
        .debug_selector(|| "disktree-root".into())
        .track_focus(&app.focus)
        .tab_group().tab_stop(false)
        .key_context("Disktree")
        .on_action(cx.listener(|this, _: &crate::app_menu::Rescan, _, cx| {
            // Where `r` would: not behind the confirmation, and not under
            // the review list or a removal that is still running.
            if this.can_start_over() {
                this.start_scan(cx);
            }
        }))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::OpenFolder, _, cx| {
                if this.can_start_over() {
                    this.open_folder(cx);
                }
            },
        ))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::ShowInFinder, _, cx| {
                if !this.has_modal() { this.reveal_target(cx); }
            },
        ))
        // Only where the history buttons are: the review screen has none.
        .on_action(cx.listener(|this, _: &crate::app_menu::GoBack, _, cx| {
            if this.screen == Screen::Explore && !this.has_modal() {
                this.go_back(cx);
            }
        }))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::GoForward, _, cx| {
                if this.screen == Screen::Explore && !this.has_modal() {
                    this.go_forward(cx);
                }
            },
        ))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::ChooseSource, _, cx| {
                this.open_volumes(cx);
            },
        ))
        .on_action(cx.listener(|this, _: &crate::app_menu::Undo, _, cx| {
            this.undo_unmark(cx);
        }))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::ShowExplore, _, cx| {
                if this.screen == Screen::Review && !this.has_modal() {
                    this.screen = Screen::Explore;
                    cx.notify();
                }
            },
        ))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::ShowReview, _, cx| {
                if this.screen == Screen::Explore && !this.has_modal() {
                    this.open_review(cx);
                }
            },
        ))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::ShowTreemap, window, cx| {
                choose_view(this, false, window, cx);
            },
        ))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::ShowFileList, window, cx| {
                choose_view(this, true, window, cx);
            },
        ))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::ShowKeyboardShortcuts, _, cx| {
                this.open_help(cx);
            },
        ))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::ToggleAppMenu, _, cx| {
                if frame_has_app_menu(this.chrome_pin.as_ref()) {
                    this.toggle_app_menu(cx);
                }
            },
        ))
        .on_action(cx.listener(
            |_, _: &crate::app_menu::ConnectAgent, _, cx| {
                open_agents_settings(cx);
            },
        ))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            if this.zoom_interface(event, window, cx) {
                cx.notify();
                return;
            }
            // An open menu owns the arrows before the page behind it scrolls.
            if this.app_menu_open && app_menu_key(this, event, window, cx) { return; }
            if !this.has_modal() && this.screen == Screen::Review && !this.editing_phrase(window, cx) && crate::ui::scroll_key(&event.keystroke.key, &this.review_scroll, window.rem_size()) { cx.stop_propagation(); cx.notify(); return; }
            if this.show_help && crate::ui::scroll_key(&event.keystroke.key, &this.help_scroll, window.rem_size()) { cx.stop_propagation(); cx.notify(); return; }
            if event.keystroke.key == "tab" && !event.keystroke.modifiers.control && !event.keystroke.modifiers.platform {
                if !this.has_modal() { this.find_open = false; }
                let scope = (this.has_modal() || this.app_menu_open).then(|| this.confirm_focus.clone());
                crate::semantics::advance_focus(window, cx, event.keystroke.modifiers.shift, scope.as_ref());
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if this.volumes_open {
                let editing = [&this.picker_host, &this.picker_path].into_iter()
                    .flatten().any(|input| input.read(cx).focus_handle(cx).is_focused(window));
                if editing && event.keystroke.key != "escape" { return; }
            }
            if event.keystroke.key == "?" && !this.has_modal() && !this.find_open && !this.editing_phrase(window, cx) {
                this.open_help(cx); this.apply_focus(window, cx); return;
            }
            if event.keystroke.key == "escape" && this.screen == Screen::Explore && !this.has_modal() && this.is_indexing() {
                // Search and menus own Escape before scan cancellation,
                // including when a toolbar control currently has focus.
                this.on_key_down(event, cx); this.apply_focus(window, cx); return;
            }
            if !this.has_modal() && !this.app_menu_open && this.screen != Screen::Running && !this.focus.is_focused(window) && !this.file_focus.is_focused(window) {
                if event.keystroke.key == "escape" { window.focus(&this.focus, cx); cx.notify(); }
                return;
            }
            this.on_key_down(event, cx);
            this.apply_focus(window, cx);
        }))
        .relative()
        .flex()
        .flex_col()
        .size_full()
        .bg(theme.background)
        .text_color(theme.foreground)
        .font_family(theme.font)
        .text_size(text::BODY)
        .child(background);
    // A modal hides the mosaic's hover card: the pointer last seen on the
    // treemap is stale while the backdrop takes the mouse, so the card would
    // otherwise sit, or drift, behind the dialog.
    if !modal && let Some(tip) = cursor_tooltip(app, window, cx) {
        root = root.child(tip);
    }
    if app.show_help {
        root = root.child(help_overlay(app, cx));
    }
    if app.app_menu_open {
        root = root.child(app_menu_dropdown(app, cx));
    }
    if app.volumes_open {
        root = root.child(volumes_dialog(app, window, cx));
    }
    if app.confirm_open {
        root = root.child(delete_dialog(app, cx));
    }
    root
}

/// Enter resolves to the dialog's Confirm action before button key events.
/// Consume it at the focused button so Cancel and every peer keep their
/// own click behavior instead of falling through to the dialog default.
fn modal_action(
    button: gpui_omarchy::Button,
    action: fn(&mut Disktree, &mut Window, &mut Context<'_, Disktree>),
    cx: &Context<'_, Disktree>,
) -> gpui_omarchy::Button {
    button
        .on_click(cx.listener(move |this, _, window, cx| {
            action(this, window, cx);
        }))
        .on_action(cx.listener(
            move |this, _: &gpui_kit::base::actions::Confirm, window, cx| {
                action(this, window, cx);
                cx.stop_propagation();
            },
        ))
}

/// Discovery is lightweight; a separate, explicit action starts the walk.
fn volumes_dialog(
    app: &Disktree,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> impl IntoElement {
    let theme = cx.omarchy().clone();
    let cancel = cx.entity().downgrade();
    let source_error = app.picker_error.as_ref().or(app.volumes_error.as_ref());
    let host_invalid = app.volumes_error.is_some();
    let path_invalid = app.picker_error.is_some();
    let source_ready = app.picker_source_matches(cx);
    let listable = source_ready
        && !app.volumes_loading
        && app.volumes_error.is_none()
        && !(app.picker_ssh && app.volumes_source.is_none());
    let mut rows = div()
        .id("volume-rows")
        .role(gpui_kit::Role::ListBox)
        .track_focus(&app.source_roots_focus).tab_index(0).tab_stop(true)
        .border_2().border_color(theme.background.opacity(0.))
        .focus_visible(|style| style.border_color(theme.accent))
        .aria_description("Up and Down select a root. Enter explicitly accepts the selected root and starts its scan.")
        .aria_label(
            "Available roots in the selected machine; listing is not a scan",
        )
        .debug_selector(|| "volume-rows".into())
        .flex()
        .flex_col()
        .max_h(size::SOURCE_LIST_HEIGHT)
        .overflow_y_scroll()
        .border_1()
        .border_color(theme.border)
        .bg(theme.inset);
    let quiet = |line: String| {
        div()
            .px(space::MD)
            .py(space::SM)
            .text_size(text::CAPTION)
            .text_color(theme.secondary)
            .child(line)
    };
    if app.picker_ssh && app.volumes_source.is_none() && !app.volumes_loading {
        rows = rows.child(quiet(
            "Enter an SSH alias or user@host, then Connect.".into(),
        ));
    } else if app.volumes_loading {
        rows = rows.child(quiet("Listing mounts, not scanning files…".into()));
    } else if let Some(error) = &app.volumes_error {
        rows = rows.child(
            div()
                .id("discovery-error")
                .role(gpui_kit::Role::Alert)
                .aria_label(error.clone())
                .px(space::MD)
                .py(space::SM)
                .text_color(theme.danger)
                .child(error.clone()),
        );
    }
    let row = |id: ElementId, index: usize, label: String| {
        let highlighted = index == app.volume_highlight;
        div()
            .id(id)
            .role(gpui_kit::Role::ListBoxOption)
            .aria_label(label)
            .aria_selected(highlighted)
            .when(
                highlighted,
                gpui_kit::StatefulInteractiveElement::aria_active_descendant,
            )
            .debug_selector(move || format!("volume-{index}"))
            .flex()
            .flex_row()
            .items_center()
            .gap(space::SM)
            .px(space::MD)
            .py(space::SM)
            .text_size(text::BODY)
            .text_color(if highlighted {
                theme.bright
            } else {
                theme.foreground
            })
            .when(highlighted, |row| row.bg(theme.accent.opacity(0.18)))
            .hover(|row| row.bg(theme.accent.opacity(0.1)))
    };
    for (index, volume) in app.volumes.iter().enumerate() {
        let label = format!(
            "{}  ·  {}",
            volume.point.display(),
            volume.device.as_deref().unwrap_or("Mounted volume")
        );
        let id = ElementId::Name(crate::file_list::path_id(&volume.point));
        rows = rows.child(row(id, index, label.clone()).child(label).on_click(
            cx.listener(move |this, _, window, cx| {
                this.volume_highlight = index;
                window.focus(&this.confirm_focus, cx);
                cx.notify();
            }),
        ));
    }
    let host = app
        .picker_host
        .as_ref()
        .expect("picker prepared before rendering");
    let path = app
        .picker_path
        .as_ref()
        .expect("picker prepared before rendering");
    // The typed path is the list's last row, so one Scan acts on whichever
    // row is highlighted and Enter never depends on which field has focus.
    let directory = app.volumes.len();
    if listable {
        rows = rows.child(
            row(
                "source-directory".into(),
                directory,
                "Directory to scan".into(),
            )
            .child("Directory")
            .child(
                gpui_omarchy::input("source-path", path, window, cx)
                    .aria_label("Root directory to scan")
                    .aria_value(path.read(cx).value())
                    .a11y_synthetic_children(move |b| {
                        if path_invalid {
                            b.parent_node().set_invalid(
                                gpui_kit::accesskit::Invalid::True,
                            );
                        }
                    })
                    .aria_description(format!(
                        "Absolute path in the selected machine. Scan starts \
                         only after explicit acceptance. {}",
                        source_error.map_or("", String::as_str)
                    ))
                    .debug_selector(|| "source-path".into())
                    .flex_1(),
            )
            .when(app.volumes_source.is_none(), |row| {
                row.child(
                    button(
                        "source-browse",
                        "Choose\u{2026}",
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .debug_selector(|| "source-browse".into())
                    .map(|button| {
                        modal_action(
                            button,
                            |this, _, cx| {
                                this.browse_picker_path(cx);
                            },
                            cx,
                        )
                    }),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.volume_highlight = directory;
                cx.notify();
            })),
        );
    }
    let entity = cx.entity().downgrade();
    let switch = button_group(
        "source-switch",
        vec![
            ChoiceItem::new("local", "This computer"),
            ChoiceItem::new("ssh", "SSH host"),
        ],
        Some(usize::from(app.picker_ssh)),
        move |index, window, cx| {
            let _ = entity.update(cx, |this, cx| {
                if index == 0 {
                    this.choose_local_source(window, cx);
                } else {
                    this.choose_ssh_source(window, cx);
                }
            });
        },
        window,
        cx,
    );
    let scan_ready = listable
        && (app.volume_highlight < app.volumes.len()
            || !path.read(cx).value().trim().is_empty());
    let cancel_button =
        dialog_button("source-cancel", "Cancel", ButtonVariant::Secondary, cx)
            .debug_selector(|| "source-cancel".into())
            .map(|button| {
                modal_action(
                    button,
                    |this, window, cx| {
                        this.close_volumes(cx);
                        this.apply_focus(window, cx);
                    },
                    cx,
                )
            });
    let scan_button =
        dialog_button("source-scan", "Scan", ButtonVariant::Primary, cx)
            .debug_selector(|| "source-scan".into())
            .semantic_disabled(!scan_ready)
            .map(|button| {
                modal_action(
                    button,
                    |this, window, cx| {
                        this.choose_volume(cx);
                        this.apply_focus(window, cx);
                    },
                    cx,
                )
            });
    let popup = dialog_popup(cx)
        .w_full()
        .child(crate::semantics::text(
            "source-cautions",
            "Choose this computer or an SSH host, then select a mount or \
             directory. No scan starts until Scan is accepted. Nested mounts \
             and symlinks are not traversed; remote sources are read-only. \
             Space is measured only after selection.",
        ))
        .child(dialog_title("Choose what to scan", cx))
        .child(
            div()
                .debug_selector(|| "source-switch".into())
                .child(switch),
        )
        .when(app.picker_ssh, |popup| {
            popup.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(space::SM)
                    .child(
                        gpui_omarchy::input("source-host", host, window, cx)
                            .aria_label("SSH destination")
                            .aria_value(host.read(cx).value())
                            .a11y_synthetic_children(move |b| {
                                if host_invalid {
                                    b.parent_node().set_invalid(
                                        gpui_kit::accesskit::Invalid::True,
                                    );
                                }
                            })
                            .aria_description(format!(
                                "SSH alias or user@host. {}",
                                source_error.map_or("", String::as_str)
                            ))
                            .debug_selector(|| "source-host".into())
                            .flex_1(),
                    )
                    .child(
                        button(
                            "source-connect",
                            "Connect",
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .debug_selector(|| "source-connect".into())
                        .map(|button| {
                            modal_action(
                                button,
                                |this, _, cx| {
                                    this.connect_picker(cx);
                                },
                                cx,
                            )
                        }),
                    ),
            )
        })
        .children(app.volumes_source.as_ref().map(|host| {
            let source = format!("{host} \u{b7} SSH \u{b7} read-only");
            crate::semantics::text("selected-source", source.clone())
                .text_color(theme.bright)
                .child(source)
        }))
        .child(rows)
        .child(dialog_description(
            if app.picker_ssh {
                "Read-only over SSH. Nested mounts and symlinks are not \
                 followed. Nothing is read until you press Scan."
            } else {
                "Nested mounts and symlinks are not followed. Nothing is \
                 read until you press Scan."
            },
            cx,
        ))
        .children(app.picker_error.as_ref().map(|error| {
            div()
                .id("source-error")
                .role(gpui_kit::Role::Alert)
                .aria_label(error.clone())
                .text_color(theme.danger)
                .child(error.clone())
        }))
        .child(dialog_footer(
            [],
            Some(cancel_button.into_any_element()),
            Some(scan_button.into_any_element()),
        ));
    let popup = div()
        .id("source-picker-scroll")
        .role(gpui_kit::Role::Dialog)
        .aria_label("Choose what to scan")
        .a11y_synthetic_children(|b| b.parent_node().set_modal())
        .tab_group()
        .debug_selector(|| "source-picker".into())
        .w(size::SOURCE_PICKER)
        .max_w(Rems(
            window.viewport_size().width.as_f32() / app.rem
                * size::SOURCE_VIEWPORT_FRACTION,
        ))
        .max_h(Rems(
            window.viewport_size().height.as_f32() / app.rem
                * size::SOURCE_VIEWPORT_FRACTION,
        ))
        .overflow_y_scroll()
        .child(popup);
    let centred = centred_popup(popup);
    let accept = cx.entity().downgrade();
    alert_dialog(&app.confirm_focus, cx)
        .open(true)
        .on_ok(move |_, window, cx| {
            let _ = accept.update(cx, |this, cx| {
                let host_focused =
                    this.picker_host.as_ref().is_some_and(|input| {
                        input.read(cx).focus_handle(cx).is_focused(window)
                    });
                let path_focused =
                    this.picker_path.as_ref().is_some_and(|input| {
                        input.read(cx).focus_handle(cx).is_focused(window)
                    });
                if this.volumes_open {
                    if host_focused {
                        this.connect_picker(cx);
                    } else if path_focused {
                        this.choose_picker_path(cx);
                    } else if this.confirm_focus.is_focused(window)
                        || this.source_roots_focus.is_focused(window)
                    {
                        this.choose_volume(cx);
                    }
                }
                this.apply_focus(window, cx);
            });
            false
        })
        .on_cancel(move |_, window, cx| {
            let _ = cancel.update(cx, |this, cx| {
                this.close_volumes(cx);
                this.apply_focus(window, cx);
            });
            false
        })
        .popup(centred)
}

/// Put a dialog's popup in the middle of the window, where a click outside
/// it does nothing.
///
/// The base dialog hosts its popup as an ordinary child of a full-window box,
/// so a popup lands in the top-left corner unless something centres it. A
/// dialog asks something or holds typing, so a stray click must not answer
/// or discard it, as macOS sheets and Windows dialogs ignore one: Cancel and
/// Escape close it. Overlays, which only show, close on one instead (see
/// `overlay_backdrop`).
fn centred_popup(card: impl IntoElement) -> Stateful<Div> {
    div()
        .id("dialog-outside")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        // Taken here, so nothing behind the dialog sees it either.
        .on_any_mouse_down(|_: &MouseDownEvent, _: &mut Window, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .id("dialog-card")
                .on_any_mouse_down(
                    |_: &MouseDownEvent, _: &mut Window, cx: &mut App| {
                        cx.stop_propagation();
                    },
                )
                .child(card),
        )
}

/// The footer every dialog and screen ends with. What does not end it sits
/// on the left; what does sits on the right, in the order the system
/// expects: Cancel then the default on macOS, GNOME and Omarchy, so the
/// default is in the corner; the default then Cancel on Windows.
pub fn dialog_footer(
    left: impl IntoIterator<Item = AnyElement>,
    cancel: Option<AnyElement>,
    primary: Option<AnyElement>,
) -> Div {
    let (first, second) =
        if chrome::Platform::current() == chrome::Platform::Windows {
            (primary, cancel)
        } else {
            (cancel, primary)
        };
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(space::SM)
        .children(left)
        .child(div().flex_1())
        .children(first)
        .children(second)
}

/// Clears an overlay's flag; one value serves its Done and its backdrop.
type CloseOverlay = fn(&mut Disktree);

/// The backdrop of an overlay that only shows something: it dims the
/// window, takes every mouse event so nothing behind it hovers or clicks,
/// and closes the overlay on a click outside the card. The card is never
/// taller than the window: give it `min_h_0` and let its middle scroll.
fn overlay_backdrop(
    id: &'static str,
    card: impl IntoElement,
    close: CloseOverlay,
    cx: &Context<'_, Disktree>,
) -> Stateful<Div> {
    let theme = cx.omarchy();
    div()
        .id(id)
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.background.opacity(0.86))
        // Room on every side, so a card taller than a short window still
        // reads as a dialog instead of a page.
        .p(space::LG)
        .on_any_mouse_down(cx.listener(move |this, _, window, cx| {
            // The click only dismisses; nothing behind or above sees it.
            cx.stop_propagation();
            close(this);
            this.focus_request = Some(crate::state::FocusTarget::Root);
            this.apply_focus(window, cx);
            cx.notify();
        }))
        .child(
            div()
                .id("overlay-card")
                .occlude()
                .flex()
                .flex_col()
                .max_w_full()
                .max_h_full()
                .on_any_mouse_down(
                    |_: &MouseDownEvent, _: &mut Window, cx: &mut App| {
                        cx.stop_propagation();
                    },
                )
                .child(card),
        )
}

/// An overlay's footer: what it says on the left, Done where a dialog's
/// default button sits, on the right.
fn overlay_footer(
    note: impl IntoElement,
    close: CloseOverlay,
    cx: &Context<'_, Disktree>,
) -> Div {
    let theme = cx.omarchy();
    let done = div().debug_selector(|| "overlay-done".into()).child(
        dialog_button("overlay-done", "Done", ButtonVariant::Primary, cx)
            .on_click(cx.listener(move |this, _, window, cx| {
                close(this);
                this.focus_request = Some(crate::state::FocusTarget::Root);
                this.apply_focus(window, cx);
                cx.notify();
            })),
    );
    dialog_footer(
        [div()
            .flex_1()
            .min_w_0()
            .text_size(text::CAPTION)
            .text_color(theme.secondary)
            .child(note)
            .into_any_element()],
        None,
        Some(done.into_any_element()),
    )
}

/// The one question disktree asks: a permanent deletion cannot be undone, so
/// it is an alert dialog that names what goes and what comes back. The trash is
/// reversible and needs no dialog.
fn delete_dialog(
    app: &Disktree,
    cx: &mut Context<'_, Disktree>,
) -> impl IntoElement {
    let plan = app.plan();
    let title = match plan.targets.as_slice() {
        [only] => format!(
            "Delete \u{201c}{}\u{201d} permanently?",
            short_name(&only.path, app.path_style())
        ),
        targets => format!("Delete {} items permanently?", targets.len()),
    };
    let body = format!(
        "Estimated to free ≈ {}. Shared blocks and snapshots may retain space. Deleted files can\u{2019}t be recovered; move them to the trash if you might need them again.",
        savings(plan.reclaimable_bytes(), plan.possible_bytes())
    );
    let body = if app.rescan_after_removal {
        format!("{body} PwrDisk rescans afterwards.")
    } else {
        body
    };
    let confirm = cx.entity().downgrade();
    let cancel = confirm.clone();
    let cancel_button =
        dialog_button("delete-cancel", "Cancel", ButtonVariant::Secondary, cx)
            .debug_selector(|| "delete-cancel".into())
            .map(|button| {
                modal_action(
                    button,
                    |this, window, cx| {
                        this.cancel_delete(cx);
                        this.apply_focus(window, cx);
                    },
                    cx,
                )
            });
    let delete_label = if app.rescan_after_removal {
        "Delete & rescan"
    } else {
        "Delete"
    };
    let delete_button = dialog_button(
        "delete-confirm",
        delete_label,
        ButtonVariant::Danger,
        cx,
    )
    .debug_selector(|| "delete-confirm".into())
    .map(|button| {
        modal_action(
            button,
            |this, window, cx| {
                this.confirm_delete(cx);
                this.apply_focus(window, cx);
            },
            cx,
        )
    });
    let popup = dialog_popup(cx)
        .child(dialog_title(title.clone(), cx))
        .child(dialog_description(body.clone(), cx))
        .child(dialog_footer(
            [],
            Some(cancel_button.into_any_element()),
            Some(delete_button.into_any_element()),
        ));
    let centred = centred_popup(popup)
        .role(gpui_kit::Role::AlertDialog)
        .aria_label(title)
        .aria_description(body)
        .a11y_synthetic_children(|b| b.parent_node().set_modal());
    alert_dialog(&app.confirm_focus, cx)
        .open(true)
        .on_ok(move |_, window, cx| {
            let _ = confirm.update(cx, |this, cx| {
                if this.confirm_open && this.confirm_focus.is_focused(window) {
                    this.confirm_delete(cx);
                    this.apply_focus(window, cx);
                }
            });
            false
        })
        .on_cancel(move |_, window, cx| {
            let _ = cancel.update(cx, |this, cx| {
                this.cancel_delete(cx);
                this.apply_focus(window, cx);
            });
            false
        })
        .popup(centred)
}

// ── explore ─────────────────────────────────────────────────────────────

/// The volume's space, for the row that stands in for the hidden panel.
fn compact_space(app: &Disktree) -> String {
    app.space.map_or_else(
        || "Volume space unavailable".to_owned(),
        |space| {
            format!(
                "{} available of {}; {} marked",
                human_bytes(space.available),
                human_bytes(space.total),
                app.marks.len()
            )
        },
    )
}

fn explore(
    app: &mut Disktree,
    toasts: Option<AnyElement>,
    window: &Window,
    cx: &Context<'_, Disktree>,
) -> Div {
    let theme = cx.omarchy().clone();
    if app.awaiting_root {
        return div()
            .debug_selector(|| "choose-root-idle".into())
            .relative()
            .children(toasts)
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(space::LG)
            .p(space::XXL)
            .child(
                div()
                    .text_size(text::HEADING)
                    .child("Choose a source. Then choose a root."),
            )
            .child(div().text_color(theme.secondary).child(
                "Local or SSH · one mount at a time · no background scan",
            ))
            .child(
                button(
                    "choose-source",
                    "Choose source…",
                    ButtonVariant::Primary,
                    cx,
                )
                .on_click(cx.listener(
                    |this, _, window, cx| {
                        this.open_volumes(cx);
                        this.apply_focus(window, cx);
                    },
                )),
            );
    }
    let mosaic: Mosaic = app.prepare();
    let width_rems = window.viewport_size().width.as_f32() / app.rem;
    // The panel is where the selection, the marks and the disk live; it
    // only gives way when the mosaic would be too narrow to read.
    let panel = (app.show_selection || app.is_indexing())
        && width_rems >= PANEL_SHOWN_REMS;
    if panel && let Some(path) = panel_checkout(app) {
        app.want_git(&path, window.is_window_active(), cx);
    }

    let viewport = if app.tree().is_none() {
        // The first walk of a home directory takes long enough that an
        // empty viewport would look broken; count the work instead.
        if app.scan_error.is_none()
            && let Some(preview) = &app.scan_preview
        {
            treemap_view::preview(preview, app, window, cx).into_any_element()
        } else {
            scanning_panel(app, &theme, cx).into_any_element()
        }
    } else {
        let view = if app.file_list {
            crate::file_list::list(app, window, cx).into_any_element()
        } else {
            treemap_view::mosaic(mosaic, app, window, cx).into_any_element()
        };
        // A filter that keeps nothing here says so over the empty view,
        // rather than leave a blank mosaic or list to puzzle over.
        match app.legend_empty_here() {
            Some(kept) => div()
                .relative()
                .flex()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .child(view)
                .child(legend_empty(app, kept, &theme, cx))
                .into_any_element(),
            None => view,
        }
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(
            div()
                .id("explore-body")
                .flex()
                .flex_row()
                .flex_1()
                .min_h_0()
                // The panel's handle starts the drag; the width follows the
                // pointer from here, wherever it goes in the window.
                .on_drag_move(cx.listener(
                    |this, event: &DragMoveEvent<PanelDrag>, window, cx| {
                        // The pointer is in window coordinates; the
                        // panel's right edge is the content's, inset by a
                        // client-side frame.
                        let inset = &this.content_edges;
                        this.panel_rems = panel_width(
                            (event.event.position.x - inset.left).as_f32(),
                            (window.viewport_size().width
                                - inset.left
                                - inset.right)
                                .as_f32(),
                            this.rem,
                        );
                        cx.notify();
                    },
                ))
                .child(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .when(!app.file_list || !crate::file_list::compact(window, app.rem), |view| view.child(trail_and_legend(app, &theme, cx)))
                        .child(
                            div()
                                .id("file-navigation")
                                .when(app.tree().is_some() && !app.file_list, |view| view
                                    .role(gpui_kit::Role::Group).aria_label("Treemap navigation; arrows move, Space marks, n selects the next sibling; choose List in the View switch for file details")
                                    .track_focus(&app.file_focus).tab_index(0).tab_stop(true)
                                    .focus_visible(|style| style.border_2().border_color(theme.accent)))
                                .flex()
                                .flex_1()
                                .min_h_0()
                                .px(space::LG)
                                .pb(space::SM)
                                .child(viewport),
                        )
                        // Over the mosaic's lower left, clear of the side
                        // panel and the bands below.
                        .children(toasts),
                )
                .children(panel.then(|| side_panel(app, &theme, cx))),
        )
        .children((app.is_indexing() && !panel).then(|| {
            div()
                .px(space::LG)
                .py(space::SM)
                .child(indexing_compact(app, &theme, cx))
        }))
        .children((!panel).then(|| div().flex().flex_row().flex_wrap().items_center().gap(space::SM).px(space::LG).py(space::XS)
            .child({ let space = compact_space(app); crate::semantics::text("compact-space", space.clone()).child(space) })
            .child(button("compact-source", "Source…", ButtonVariant::Secondary, cx).on_click(cx.listener(|this, _, window, cx| { this.open_volumes(cx); this.apply_focus(window, cx); })))))
        .child(key_bar(app, &theme, cx))
}

/// The selected tile, when it is a checkout git can say something about;
/// `selection_section` reads the answer back with `known_checkout`.
///
/// Only a selected tile: with nothing selected the panel describes the
/// directory on screen, often the scan root, and `git status` there can
/// walk a whole home directory tracked as a dotfiles checkout.
fn panel_checkout(app: &mut Disktree) -> Option<std::path::PathBuf> {
    if app.remote.is_some() {
        return None;
    }
    let target = git_target(app)?;
    let node = app.node_at(&target)?;
    if !node.is_dir() {
        return None;
    }
    let path = app.path_at(&target)?;
    app.checkout_at(&path).then_some(path)
}

/// The tile git may be asked about: a selected one, never the scan root.
fn git_target(app: &Disktree) -> Option<Vec<usize>> {
    app.action_target().filter(|target| !target.is_empty())
}

/// Width, in rem, below which the side panel gives the mosaic its room.
const PANEL_SHOWN_REMS: f32 = 52.0;

/// What the panel handle drags. The width itself lives in the app state.
#[derive(Clone, Copy, Debug)]
struct PanelDrag;

/// A drag needs a view to draw under the pointer; resizing draws nothing.
struct NoGhost;

impl gpui_kit::Render for NoGhost {
    fn render(
        &mut self,
        _: &mut Window,
        _: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        div()
    }
}

// ── title strip ─────────────────────────────────────────────────────────

/// Width, in rem, below which the wordmark gives the trail its room: the
/// settings do not shrink, and at the minimum window the stoplights' zone
/// would otherwise squeeze the trail to nothing. The mark stays.
const WORDMARK_SHOWN_REMS: f32 = 70.0;

/// The window's title strip. On explore it carries what the whole window
/// is about — where you are and how it is measured — after the brand; the
/// other screens have a heading of their own, so it is only the brand there.
fn title_strip(
    app: &Disktree,
    frame: &Frame,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Stateful<Div> {
    let theme = cx.omarchy().clone();
    let explore = app.screen == Screen::Explore;
    let width_rems = window.viewport_size().width.as_f32() / app.rem;
    let wordmark = !explore || width_rems >= WORDMARK_SHOWN_REMS;
    let menu =
        frame_has_app_menu(Some(frame)).then(|| app_menu_button(app, cx));
    let content = if explore {
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .min_w_0()
            .children(menu)
            // Where you are is navigation, and it belongs to the whole window.
            .child(trail(app, &theme, cx))
            // The space between the trail and the settings is all title
            // bar: a gap there would look like it and not move the window.
            .child(
                chrome::drag_area(frame, "strip-drag", cx)
                    .flex_1()
                    .min_w(space::XXL)
                    .self_stretch(),
            )
            .children(agent_pill(app, &theme))
            .children(update_chip(app, &theme, cx))
            .child(view_settings(app, window, cx))
    } else {
        div()
            .flex()
            .flex_1()
            .items_center()
            .self_stretch()
            .children(menu)
            .child(
                chrome::drag_area(frame, "strip-drag", cx)
                    .flex_1()
                    .self_stretch(),
            )
            .children(update_chip(app, &theme, cx))
    };
    chrome::strip(frame, wordmark, content, window, cx)
}

/// The trail, from `/`. Above the scanned root, a crumb widens the scan;
/// in the tree, it goes there, and its ▾ lists its siblings to jump to.
/// A deep trail keeps its first two steps and its last four.
fn trail(app: &Disktree, theme: &Theme, cx: &Context<'_, Disktree>) -> Div {
    let steps = app.breadcrumbs();
    let last = steps.len().saturating_sub(1);
    let widening = app.scan.is_some() && app.scan_root != app.root_path;
    let hidden = if steps.len() > TRAIL_STEPS {
        2..steps.len() - (TRAIL_STEPS - 3)
    } else {
        0..0
    };
    // Short of room, the trail gives up its start, never the crumb you are
    // in and its menu: content packed to the end overflows to the left.
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(space::XXS)
        .min_w_0()
        .overflow_hidden();
    for (index, (label, step)) in steps.into_iter().enumerate() {
        if hidden.contains(&index) {
            if index == hidden.start {
                row =
                    row.child(separator_glyph(theme, app.path_style())).child(
                        div()
                            .px(space::XS)
                            .text_color(theme.secondary.opacity(0.6))
                            .child("…"),
                    );
            }
            continue;
        }
        // The root is its own separator: "/" then "home", not "/ / home".
        if index > 1 {
            row = row.child(separator_glyph(theme, app.path_style()));
        }
        let id = ElementId::Name(SharedString::from(format!("crumb-{index}")));
        let crumb = match step {
            Crumb::Above(path) => {
                let pending = widening && app.scan_root == path;
                with_tooltip(
                    widgets::crumb(id, label, false, cx)
                        .text_color(if pending {
                            palette::highlight(theme)
                        } else {
                            theme.secondary.opacity(0.7)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.widen_to(path.clone(), cx);
                        })),
                    "Scan from here · what is below is reused",
                )
                .into_any_element()
            }
            Crumb::Tree(path) => {
                tree_crumb(app, theme, id, label, path, index == last, cx)
            }
        };
        row = row.child(crumb);
    }
    row
}

/// Steps a trail shows before it folds its middle into an ellipsis.
const TRAIL_STEPS: usize = 7;

/// The scanned machine's path separator, so the trail reads like the paths
/// shown elsewhere: `/` here and on SSH hosts, `\` on Windows.
fn separator_glyph(theme: &Theme, style: PathStyle) -> Div {
    div()
        .text_color(theme.secondary.opacity(0.5))
        .text_size(text::BODY)
        .child(style.separator().to_string())
}

/// A crumb in the tree. Its label goes there (the current one opens the
/// menu instead, being there already); its ▾ opens the sibling menu.
fn tree_crumb(
    app: &Disktree,
    theme: &Theme,
    id: ElementId,
    label: String,
    path: Vec<usize>,
    current: bool,
    cx: &Context<'_, Disktree>,
) -> gpui_kit::AnyElement {
    let Some((&index, parent)) = path.split_last() else {
        // The scanned root: no siblings in the tree to offer.
        return widgets::crumb(id, label, current, cx)
            .on_click(
                cx.listener(move |this, _, _, cx| this.go_to(Vec::new(), cx)),
            )
            .into_any_element();
    };
    let parent = parent.to_vec();
    // Inside a mark, the trail says so the way the tiles do: struck through.
    // Every tile below it is struck too, but the trail is what still shows
    // the mark itself once you have gone into it.
    let struck = app.goes_with_a_mark(&path);
    let open = app
        .crumb_menu
        .as_ref()
        .is_some_and(|menu| menu.parent == parent && menu.current == index);
    let chevron_path = path.clone();
    let label_path = path;
    let chip = div()
        .flex()
        .flex_row()
        .items_center()
        .when(current || open, |this| this.bg(theme.hover_fill()))
        .child(
            widgets::crumb(id.clone(), label, current, cx)
                .when(struck, Styled::line_through)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if current {
                        this.open_crumb_menu(&label_path, cx);
                    } else {
                        this.go_to(label_path.clone(), cx);
                    }
                })),
        )
        .child(
            div()
                .id(ElementId::Name(format!("{id}-menu").into()))
                .debug_selector({
                    let name = format!("{id}-menu");
                    move || name
                })
                .px(space::XS)
                .py(space::XXS)
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .hover(|style| {
                    style.bg(theme.hover_fill()).text_color(theme.bright)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.crumb_menu.is_some() {
                        this.crumb_menu = None;
                        cx.notify();
                    } else {
                        this.open_crumb_menu(&chevron_path, cx);
                    }
                }))
                .child("▾"),
        );
    div()
        .flex()
        .flex_col()
        .child(chip)
        .when(open, |this| {
            this.child(
                deferred(
                    anchored()
                        .snap_to_window_with_margin(px(8.))
                        .offset(gpui_kit::point(px(0.), px(4.)))
                        .child(sibling_menu(app, theme, &parent, cx)),
                )
                .with_priority(2),
            )
        })
        .into_any_element()
}

/// The siblings of a crumb, largest first, with a share bar in each one's
/// colour and its size: a sideways jump without going up first.
fn sibling_menu(
    app: &Disktree,
    theme: &Theme,
    parent: &[usize],
    cx: &Context<'_, Disktree>,
) -> impl IntoElement {
    let (rows, more) = app.siblings(parent);
    let menu = app.crumb_menu.clone();
    let largest = rows.first().map_or(1, |row| row.value.max(1));
    let parent_name = app.node_at(parent).map_or_else(String::new, |node| {
        if parent.is_empty() {
            app.display_path(&app.root_path)
        } else {
            node.name().to_string()
        }
    });
    let metric = app.options.metric;
    let mut panel = div()
        .id("sibling-menu")
        .debug_selector(|| "sibling-menu".into())
        .occlude()
        .flex()
        .flex_col()
        .w(size::SIBLING_MENU)
        .max_h(size::SIBLING_MENU_HEIGHT)
        .overflow_y_scroll()
        .p(space::XS)
        .bg(theme.surface)
        .border_1()
        .border_color(theme.control_border())
        .shadow_lg()
        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
            this.crumb_menu = None;
            cx.notify();
        }))
        .child(
            div()
                .px(space::SM)
                .py(space::XS)
                .text_size(text::CAPTION)
                .text_color(theme.secondary.opacity(0.8))
                .child(format!("Siblings in {parent_name}")),
        );
    for (row_index, row) in rows.into_iter().enumerate() {
        let current =
            menu.as_ref().is_some_and(|menu| menu.current == row.index);
        let highlighted = menu
            .as_ref()
            .is_some_and(|menu| menu.highlighted == row_index);
        let parent = parent.to_vec();
        let accent = palette::category_accent(theme, row.category);
        let value = match metric {
            Metric::Bytes => human_bytes(row.value),
            Metric::Files => widgets::human_count(row.value),
        };
        panel = panel.child(
            div()
                .id(ElementId::Name(format!("sibling-{row_index}").into()))
                .flex()
                .flex_row()
                .items_center()
                .gap(space::MD)
                .px(space::SM)
                .py(space::XS)
                .when(highlighted, |this| this.bg(theme.hover_fill()))
                .hover(|style| style.bg(theme.hover_fill()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.choose_sibling(&parent, row.index, cx);
                    window.focus(&this.focus, cx);
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(text::BODY)
                        .text_color(if row.is_dir {
                            theme.bright
                        } else {
                            theme.foreground
                        })
                        .when(current, |this| {
                            this.font_weight(FontWeight::BOLD)
                        })
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(row.name),
                )
                .child(div().flex_shrink_0().w(size::ROW_BAR).child(
                    widgets::bar(row.value as f32 / largest as f32, accent, cx),
                ))
                .child(
                    div()
                        .flex_shrink_0()
                        .w(size::SIZE_LANE)
                        .flex()
                        .justify_end()
                        .text_size(text::BODY)
                        .text_color(theme.secondary)
                        .child(value),
                ),
        );
    }
    if more > 0 {
        panel = panel.child(
            div()
                .px(space::SM)
                .py(space::XS)
                .text_size(text::CAPTION)
                .text_color(theme.secondary.opacity(0.7))
                .child(format!("+{more} smaller")),
        );
    }
    panel
}

/// How the mosaic is measured and drawn, as real controls with visible state.
///
/// Every control hands the keyboard straight back to the treemap: arrows,
/// Space and Enter belong to the tiles, and a settings click must not quietly
/// capture them.
fn view_settings(
    app: &Disktree,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Div {
    let focus = app.focus.clone();
    let entity = cx.entity().downgrade();

    // What does not fit goes in ⋮, least used first; the view switch
    // never does. See `toolbar::BarFit`. Folded controls are not built.
    let fit = app.bar_fit;
    let mode = fit.shows_mode().then(|| {
        let entity = entity.clone();
        let focus = focus.clone();
        button_group(
            "mode",
            vec![
                ChoiceItem::new("size", "Size"),
                ChoiceItem::new("files", "Files"),
                ChoiceItem::new("age", "Age"),
            ],
            Some(app.mode_index()),
            move |index, window, cx| {
                let _ = entity.update(cx, |this, cx| this.set_mode(index, cx));
                window.focus(&focus, cx);
            },
            window,
            cx,
        )
        .debug_selector(|| "measure-mode".into())
        .w(size::RANKING_CHOICE)
        // Tighter than a standalone group, so the segmented control shares
        // the checkboxes' height and centre line.
        .p(space::XXS)
    });

    let check = |on: bool| {
        if on {
            CheckboxState::Checked
        } else {
            CheckboxState::Unchecked
        }
    };
    let hidden = fit.shows_scan_options().then(|| {
        let entity = entity.clone();
        let focus = focus.clone();
        checkbox(
            "hidden",
            "Hidden files",
            check(app.options.include_hidden),
            cx,
        )
        // gpui-omarchy's checkbox is `self_start`, made to hug its label in
        // a column; in the strip that pins it to the top edge instead of
        // the centre line the other controls share.
        .self_center()
        .tab_stop(true)
        .on_change(move |_, _, window, cx| {
            let _ = entity.update(cx, Disktree::toggle_hidden);
            window.focus(&focus, cx);
        })
    });
    let apparent = fit.shows_scan_options().then(|| {
        let entity = entity.clone();
        let focus = focus.clone();
        checkbox(
            "apparent",
            "Apparent size",
            check(app.options.apparent_size),
            cx,
        )
        .self_center()
        .tab_stop(true)
        .on_change(move |_, _, window, cx| {
            let _ = entity.update(cx, Disktree::toggle_apparent);
            window.focus(&focus, cx);
        })
    });

    let theme = cx.omarchy().clone();
    // Back and forward through the directories visited, beside the choices
    // that change how the one on screen is drawn.
    let travel = |id: &'static str,
                  label: &'static str,
                  enabled: bool,
                  back: bool,
                  go: fn(&mut Disktree, &mut Context<'_, Disktree>),
                  cx: &mut Context<'_, Disktree>| {
        let shown = app.history_hover == Some(back);
        div()
            .id(ElementId::Name(format!("{id}-hover").into()))
            .debug_selector(move || id.into())
            .relative()
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                // Leaving one button can be reported after entering the
                // other; only the button that owns the card takes it away.
                if *hovered {
                    this.history_hover = Some(back);
                } else if this.history_hover == Some(back) {
                    this.history_hover = None;
                }
                cx.notify();
            }))
            .child(
                button(id, label, ButtonVariant::Secondary, cx)
                    .accessibility_label(if back { "Go back in navigation history" } else { "Go forward in navigation history" })
                    .tab_stop(true)
                    .semantic_disabled(!enabled)
                    // Borderless: glyphs on the bar, not controls in a
                    // frame. Hover keeps the fill but not the outline it
                    // would add.
                    .hover(|style| {
                        style
                            .bg(theme.hover_fill())
                            .border_color(theme.foreground.opacity(0.))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        go(this, cx);
                        window.focus(&this.focus, cx);
                    })),
            )
            .when(shown, |this| {
                // Anchored to a holder pinned at the button's bottom edge, so
                // the card hangs below it and never covers it; deferred so it
                // paints over the mosaic.
                this.child(
                    div().absolute().top_full().left_0().child(
                        deferred(
                            anchored()
                                .snap_to_window_with_margin(px(8.))
                                .offset(gpui_kit::point(px(0.), px(4.)))
                                .child(history_card(app, back, cx)),
                        )
                        .with_priority(2),
                    ),
                )
            })
    };
    let history = fit.shows_history_and_depth().then(|| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .child(travel(
                "history-back",
                "<",
                app.can_go_back(),
                true,
                Disktree::go_back,
                cx,
            ))
            .child(travel(
                "history-forward",
                ">",
                app.can_go_forward(),
                false,
                Disktree::go_forward,
                cx,
            ))
    });

    let depth = app.layout_options.max_depth;
    let stepper = |id: &'static str,
                   label: &'static str,
                   step: i32,
                   cx: &mut Context<'_, Disktree>| {
        button(id, label, ButtonVariant::Secondary, cx)
            .debug_selector(move || format!("{id}-target"))
            .min_w(size::POINTER_TARGET)
            .min_h(size::POINTER_TARGET)
            .accessibility_label(if step < 0 {
                "Decrease treemap depth"
            } else {
                "Increase treemap depth"
            })
            .tab_stop(true)
            .semantic_disabled(if step < 0 { depth <= 1 } else { depth >= 6 })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.adjust_depth(step, cx);
                window.focus(&this.focus, cx);
            }))
    };
    let depth_control = fit.shows_history_and_depth().then(|| {
        with_tooltip(
            div()
                .id("depth")
                .flex()
                .flex_row()
                .items_center()
                .border_1()
                .border_color(theme.control_border())
                .child(
                    div()
                        .px(space::SM)
                        .text_size(text::BODY)
                        .text_color(theme.foreground)
                        .child(format!("Depth {depth}")),
                )
                .child(stepper("depth-less", "\u{2212}", -1, cx))
                .child(stepper("depth-more", "+", 1, cx)),
            "Levels drawn at once \u{00b7} [ and ]",
        )
    });

    let rems = window.viewport_size().width.as_f32() / app.rem;
    let switch =
        view_switch(app, crate::toolbar::BarFit::labelled_switch(rems), cx);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .flex_shrink_0()
        .children(history)
        .child(switch)
        .children(mode)
        .children(hidden)
        .children(apparent)
        .children(depth_control)
        .map(|row| {
            if fit.shows_scan_options() {
                row.child(power_efficiency(app, cx))
            } else {
                row.child(more_menu(app, cx))
            }
        })
}

/// Treemap or file list, from the menu bar or the view switch. A drawn
/// list takes the keyboard, so a screen reader lands in it; until a scan
/// finishes there is no list yet, and the root keeps it.
fn choose_view(
    app: &mut Disktree,
    on: bool,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) {
    if app.screen != Screen::Explore || app.has_modal() {
        return;
    }
    app.show_file_list(on, cx);
    let list = on && app.tree().is_some();
    window.focus(if list { &app.file_focus } else { &app.focus }, cx);
}

/// Treemap | List: how the directory on screen is drawn. It is a radio
/// group like Size | Files | Age, with a drawn glyph on each option and
/// its word when the bar has room. Its spoken name says what the list is
/// for, which the word on screen does not need to.
fn view_switch(
    app: &Disktree,
    labelled: bool,
    cx: &Context<'_, Disktree>,
) -> impl IntoElement {
    use gpui_kit::base::{Radio, RadioGroup};

    let theme = cx.omarchy().clone();
    let entity = cx.entity().downgrade();
    let shortcut = if cfg!(target_os = "macos") {
        "\u{2318}"
    } else {
        "ctrl-"
    };
    let option = |list: bool| {
        let checked = app.file_list == list;
        let (id, word, spoken, about) = if list {
            (
                "view-list",
                "List",
                "File list, for screen readers",
                "Every entry in this folder as a row, with its size and \
                 share. Works with screen readers.",
            )
        } else {
            (
                "view-treemap",
                "Treemap",
                "Treemap",
                "Each entry's share of this folder, drawn as nested tiles.",
            )
        };
        let ink = if checked {
            theme.bright
        } else {
            theme.foreground
        };
        let glyph = if list {
            widgets::list_glyph(ink)
        } else {
            widgets::treemap_glyph(ink)
        };
        let entity = entity.clone();
        let keys = entity.clone();
        let radio = Radio::new(id)
            .debug_selector(move || id.into())
            .checked(checked)
            .accessibility_label(spoken)
            .set_position(usize::from(list) + 1, 2)
            // One stop for the group: the option that is on, as a radio
            // group's roving focus goes.
            .tab_stop(checked)
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(space::SM)
            .min_w(size::POINTER_TARGET)
            .min_h(size::POINTER_TARGET)
            .px(space::SM)
            .border_1()
            .border_color(theme.foreground.opacity(0.))
            .text_size(text::BODY)
            .text_color(ink)
            .when(checked, |this| this.bg(theme.selected_fill()))
            .when(!checked, |this| {
                this.hover(|style| style.bg(theme.hover_fill()))
            })
            .focus_visible(|style| style.border_color(theme.focus_border()))
            .child(glyph)
            .when(labelled, |this| this.child(word))
            .on_change(move |_, _, window, cx| {
                let _ = entity.update(cx, |this, cx| {
                    choose_view(this, list, window, cx);
                });
            })
            // The arrows choose, as in any radio group. Focus moves into
            // the view chosen, as a click does, so the list is read out.
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                let to = match event.keystroke.key.as_str() {
                    "left" | "up" => false,
                    "right" | "down" => true,
                    _ => return,
                };
                cx.stop_propagation();
                let _ = keys.update(cx, |this, cx| {
                    choose_view(this, to, window, cx);
                });
            });
        with_tooltip(
            radio,
            format!(
                "{} \u{00b7} {shortcut}{}\n{about}",
                if list { "File list" } else { "Treemap" },
                if list { 2 } else { 1 }
            ),
        )
    };
    RadioGroup::new("view-switch")
        .axis(gpui_kit::Axis::Horizontal)
        .aria_label("View")
        .flex()
        .flex_row()
        .flex_shrink_0()
        .gap(space::XXS)
        // Framed and padded as Size | Files | Age is, so both groups share
        // a height and a centre line.
        .p(space::XXS)
        .border_1()
        .border_color(theme.divider())
        .child(option(false))
        .child(option(true))
}

/// ⋮: the bar's controls that did not fit, in the sibling and power menus'
/// grammar. When the scan workers are among them, their menu opens here.
fn more_menu(app: &Disktree, cx: &Context<'_, Disktree>) -> Div {
    use crate::toolbar::MoreItem;

    let theme = cx.omarchy().clone();
    let open = app.more_menu.is_some();
    let trigger = div()
        .id("more-hover")
        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
            this.more_hover = *hovered;
            cx.notify();
        }))
        .child(
            button("more-toggle", "", ButtonVariant::Secondary, cx)
                .debug_selector(|| "more-toggle".into())
                .accessibility_label("More view options")
                .selected(open)
                .tab_stop(true)
                .min_w(size::POINTER_TARGET)
                .min_h(size::POINTER_TARGET)
                .border_1()
                .border_color(theme.control_border())
                .child(
                    gpui_omarchy::icon(
                        gpui_omarchy::IconName::EllipsisVertical,
                    )
                    .size(icon::SM)
                    .text_color(theme.bright),
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.toggle_more_menu(cx);
                    // The menu's keys reach it through the root, as the
                    // power menu's do.
                    window.focus(&this.focus, cx);
                })),
        );
    let trigger = if open {
        trigger
    } else {
        with_tooltip(trigger, "More view options")
    };
    let mut control = div().relative().flex_shrink_0().child(trigger);
    if app.power_menu.is_some() {
        if let Some(popup) = power_popup(app, cx) {
            control = control.child(popup);
        }
        return control;
    }
    let Some(highlighted) = app.more_menu else {
        return control;
    };

    let items = app.bar_fit.more_items();
    let mut menu = div()
        .id("more-menu")
        .debug_selector(|| "more-menu".into())
        .occlude()
        .flex()
        .flex_col()
        .w(size::MORE_MENU)
        .p(space::XS)
        .bg(theme.surface)
        .border_1()
        .border_color(theme.control_border())
        .shadow_lg()
        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
            if !this.more_hover {
                this.more_menu = None;
                cx.notify();
            }
        }));
    let mut section = "";
    for (index, item) in items.into_iter().enumerate() {
        if item.section() != section {
            if !section.is_empty() {
                menu = menu
                    .child(div().h(px(1.)).my(space::XS).bg(theme.divider()));
            }
            section = item.section();
            menu = menu.child(
                div()
                    .px(space::SM)
                    .py(space::XS)
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary.opacity(0.8))
                    .child(section),
            );
        }
        let enabled = app.more_item_enabled(item);
        let check = |on: bool| {
            div().flex_shrink_0().w(icon::SM).when(on, |this| {
                this.child(
                    gpui_omarchy::icon(gpui_omarchy::IconName::Check)
                        .size(icon::SM)
                        .text_color(theme.accent),
                )
            })
        };
        let (lead, name, trail): (Div, String, Option<AnyElement>) = match item
        {
            MoreItem::Mode(mode) => (
                check(app.mode_index() == mode),
                ["Size", "Files", "Age"][mode].to_owned(),
                None,
            ),
            MoreItem::Back => (check(false), "Back".into(), None),
            MoreItem::Forward => (check(false), "Forward".into(), None),
            MoreItem::Depth => {
                let depth = app.layout_options.max_depth;
                let step = |id: &'static str, label: &'static str, by: i32| {
                    button(id, label, ButtonVariant::Secondary, cx)
                        .debug_selector(move || id.into())
                        .accessibility_label(if by < 0 {
                            "Decrease treemap depth"
                        } else {
                            "Increase treemap depth"
                        })
                        .min_w(size::POINTER_TARGET)
                        .min_h(size::POINTER_TARGET)
                        .semantic_disabled(if by < 0 {
                            depth <= 1
                        } else {
                            depth >= 6
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.adjust_depth(by, cx);
                            window.focus(&this.focus, cx);
                        }))
                };
                (
                    check(false),
                    format!("Depth {depth}"),
                    Some(
                        div()
                            .flex()
                            .flex_row()
                            .border_1()
                            .border_color(theme.control_border())
                            .child(step("more-depth-less", "\u{2212}", -1))
                            .child(step("more-depth-more", "+", 1))
                            .into_any_element(),
                    ),
                )
            }
            MoreItem::Hidden => (
                check(app.options.include_hidden),
                "Hidden files".into(),
                None,
            ),
            MoreItem::Apparent => (
                check(app.options.apparent_size),
                "Apparent size".into(),
                None,
            ),
            MoreItem::Workers => (
                check(false),
                "Scan workers".into(),
                Some(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .child(format!(
                            "{} \u{203a}",
                            app.power_choice.map_or(
                                "Custom",
                                crate::power::PowerEfficiency::name
                            )
                        ))
                        .into_any_element(),
                ),
            ),
        };
        let key = match item {
            MoreItem::Back => Some(if cfg!(target_os = "macos") {
                "\u{2318}["
            } else {
                "Alt+\u{2190}"
            }),
            MoreItem::Forward => Some(if cfg!(target_os = "macos") {
                "\u{2318}]"
            } else {
                "Alt+\u{2192}"
            }),
            _ => None,
        };
        let row = div()
            .id(ElementId::Name(format!("more-item-{index}").into()))
            .debug_selector(move || format!("more-item-{index}"))
            .flex()
            .flex_row()
            .items_center()
            .gap(space::MD)
            .min_h(size::POINTER_TARGET)
            .px(space::SM)
            .when(index == highlighted && enabled, |this| {
                this.bg(theme.hover_fill())
            })
            .when(!enabled, |this| this.opacity(0.45))
            .child(lead)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(text::BODY)
                    .text_color(theme.bright)
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(name),
            )
            .children(key.map(|key| {
                div()
                    .flex_shrink_0()
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary)
                    .child(key)
            }))
            .children(trail)
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered && this.more_menu.is_some() {
                    this.more_menu = Some(index);
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_more(item, cx);
                window.focus(&this.focus, cx);
            }));
        menu = menu.child(row);
    }
    control.child(
        div().absolute().top_full().right_0().child(
            deferred(
                anchored()
                    .anchor(gpui_kit::Anchor::TopRight)
                    .snap_to_window_with_margin(px(8.))
                    .offset(gpui_kit::point(px(0.), px(4.)))
                    .child(menu),
            )
            .with_priority(2),
        ),
    )
}

/// What the tooltip says the choice will do, where the gauge alone cannot.
/// Measured on an 18-core Apple silicon Mac scanning its whole Data volume
/// (medians of three): 4 workers 209 s and 433 CPU-seconds, 8 workers 133 s
/// and 580, 18 workers 137 s and 1,314. Elsewhere the shape is expected, not
/// measured, so it gives no numbers.
const POWER_TRADEOFF: &str = if cfg!(target_os = "macos") {
    "Up to about 8 workers a scan finishes sooner: a whole drive took a \
     third less time with 8 than with Balanced. Past that it is no faster, \
     but uses over twice the CPU and runs the fans at full speed."
} else {
    "More workers finish a scan sooner only up to a point. Past it they \
     use more CPU for no gain, and can run the fans at full speed."
};

/// How hard a scan may work, as a signal gauge framed like Depth; the
/// presets are in a menu built like the sibling menu, so the control costs
/// the bar no more than one button. Like the other settings, focus returns
/// to the treemap.
fn power_efficiency(app: &Disktree, cx: &Context<'_, Disktree>) -> Div {
    use crate::power::{self, PowerEfficiency};

    let theme = cx.omarchy().clone();
    let cpus = app.cpu_threads;
    let threads = app.options.threads.max_threads.min(cpus);
    let name = app.power_choice.map_or("Custom", PowerEfficiency::name);
    let open = app.power_menu.is_some();
    let trigger = div()
        .id("power-efficiency")
        .debug_selector(|| "power-efficiency".into())
        .flex()
        .flex_row()
        .items_center()
        .border_1()
        .border_color(theme.control_border())
        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
            this.power_hover = *hovered;
            cx.notify();
        }))
        .child(
            button("power-toggle", "", ButtonVariant::Secondary, cx)
                .accessibility_label(format!("Power Efficiency: {name}"))
                .selected(open)
                .tab_stop(true)
                .child(widgets::signal(
                    PowerEfficiency::signal(threads, cpus),
                    cx,
                ))
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .child("\u{25be}"),
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.toggle_power_menu(cx);
                    window.focus(&this.focus, cx);
                })),
        );
    // An open menu says it all; the tooltip would only cover it.
    let trigger = if open {
        trigger
    } else {
        with_tooltip(
            trigger,
            format!(
                "Power Efficiency: {name} \u{00b7} {} of {cpus} CPUs\n\
                 {POWER_TRADEOFF}\nUsed from the next scan, and remembered.",
                power::workers(threads)
            ),
        )
    };
    div()
        .relative()
        .child(trigger)
        .children(power_popup(app, cx))
}

/// The Power Efficiency menu, hung from the right edge of whichever control
/// opened it: the gauge, or ⋮ once the gauge has folded into it.
fn power_popup(
    app: &Disktree,
    cx: &Context<'_, Disktree>,
) -> Option<impl IntoElement> {
    use crate::power::{self, PowerEfficiency};

    let theme = cx.omarchy().clone();
    let cpus = app.cpu_threads;
    let highlighted = app.power_menu?;

    let mut menu = div()
        .id("power-menu")
        .debug_selector(|| "power-menu".into())
        .occlude()
        .flex()
        .flex_col()
        .w(size::POWER_MENU)
        .p(space::XS)
        .bg(theme.surface)
        .border_1()
        .border_color(theme.control_border())
        .shadow_lg()
        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
            // A press on its button (the gauge, or ⋮) is the button's.
            if !this.power_hover && !this.more_hover {
                this.power_menu = None;
                cx.notify();
            }
        }))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .px(space::SM)
                .py(space::XS)
                .text_size(text::CAPTION)
                .text_color(theme.secondary.opacity(0.8))
                .child("Scan workers")
                .child(format!("{cpus} CPUs")),
        );
    for (index, preset) in PowerEfficiency::ALL.into_iter().enumerate() {
        let offered = preset.available(cpus);
        let current = app.power_choice == Some(preset);
        let count = preset.threads(cpus);
        let row = div()
            .id(preset.key())
            .debug_selector(move || preset.key().into())
            .flex()
            .flex_row()
            .items_center()
            .gap(space::MD)
            .px(space::SM)
            .py(space::XS)
            .when(index == highlighted && offered, |this| {
                this.bg(theme.hover_fill())
            })
            // The saved preset keeps its check wherever the highlight goes;
            // bold alone read as losing to the fill under the pointer.
            .child(div().flex_shrink_0().w(icon::SM).when(current, |this| {
                this.debug_selector(|| "power-check".into()).child(
                    gpui_omarchy::icon(gpui_omarchy::IconName::Check)
                        .size(icon::SM)
                        .text_color(theme.accent),
                )
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(text::BODY)
                    .text_color(theme.bright)
                    .when(current, |this| this.font_weight(FontWeight::BOLD))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(preset.name()),
            )
            .child(div().flex_shrink_0().w(size::ROW_BAR).child(widgets::bar(
                count as f32 / cpus as f32,
                theme.accent,
                cx,
            )))
            .child(
                div()
                    .flex_shrink_0()
                    .w(size::SIZE_LANE)
                    .flex()
                    .justify_end()
                    .text_size(text::BODY)
                    .text_color(theme.secondary)
                    .child(power::workers(count)),
            );
        // A preset this machine cannot tell apart from the one below it is
        // shown, so the scale stays the same everywhere, but not offered.
        let row = if offered {
            // The pointer moves the one highlight the arrows move, so two
            // filled rows never compete with the check.
            row.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered && this.power_menu.is_some() {
                    this.power_menu = Some(index);
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.set_power_efficiency(preset, cx);
                window.focus(&this.focus, cx);
            }))
        } else {
            let below = PowerEfficiency::ALL[index - 1].name();
            with_tooltip(
                row.opacity(0.45),
                format!("Same as {below} with {cpus} CPUs"),
            )
        };
        menu = menu.child(row);
    }
    Some(
        div().absolute().top_full().right_0().child(
            deferred(
                // Hung from the right edge: the bar's last control, so the
                // menu opens inward rather than off the window.
                anchored()
                    .anchor(gpui_kit::Anchor::TopRight)
                    .snap_to_window_with_margin(px(8.))
                    .offset(gpui_kit::point(px(0.), px(4.)))
                    .child(menu),
            )
            .with_priority(2),
        ),
    )
}

// ── trail and legend ────────────────────────────────────────────────────

/// What the scan holds and the search, on one row over the mosaic, and
/// what the colours mean on the row under it.
fn trail_and_legend(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::LG)
        .child(scan_totals(app, theme));
    if app.find_open || !app.find.is_empty() {
        row = row.child(find_field(app, theme));
    }
    // The legend has a row of its own: its chips are filters, so each
    // must stay reachable, and sharing the row with the search clipped
    // the last of them whenever the window was not wide.
    div()
        .flex()
        .flex_col()
        .gap(space::XS)
        .px(space::LG)
        .py(space::SM)
        .child(row)
        .child(legend(app, theme, cx))
}

/// What the whole scan found, as one quiet line; unreadable paths are
/// flagged in the warning colour when there are any.
fn scan_totals(app: &Disktree, theme: &Theme) -> Div {
    if app.is_indexing() {
        return div()
            .text_size(text::CAPTION)
            .text_color(theme.secondary)
            .child("Live overview · incomplete · top-level contributions");
    }
    let tree = app.tree();
    let bytes = tree.map_or(app.progress.bytes, |node| node.bytes);
    let files = tree.map_or(app.progress.files, |node| node.files);
    let dirs = tree.map_or(app.progress.dirs, |node| u64::from(node.dirs));
    let errors = app.progress.errors;
    // Filtered, the line says what share of the scan is on screen, in the
    // bright colour, so a filtered mosaic is never taken for the whole.
    if tree.is_some() && app.legend.is_active() {
        let shown = match app.legend_matches.as_deref() {
            Some(matches) if !app.legend_pending => format!(
                "Showing {} of {}",
                human_bytes(matches.bytes),
                human_bytes(bytes)
            ),
            _ => "Filtering\u{2026}".to_owned(),
        };
        return div()
            .debug_selector(|| "scan-totals".into())
            .flex()
            .flex_row()
            .items_center()
            .gap(space::SM)
            .flex_shrink_0()
            .text_size(text::CAPTION)
            .text_color(theme.secondary)
            .child(div().text_color(theme.bright).child(shown))
            .child(format!("\u{00b7} {}", app.legend.summary()));
    }
    div()
        .debug_selector(|| "scan-totals".into())
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .flex_shrink_0()
        .text_size(text::CAPTION)
        .text_color(theme.secondary)
        .child(div().text_color(theme.foreground).child(human_bytes(bytes)))
        .child(format!(
            "· {} files · {} dirs",
            widgets::human_count(files),
            widgets::human_count(dirs)
        ))
        .when(tree.is_some_and(|node| node.sharing().files > 0), |this| {
            let sharing = tree.expect("checked above").sharing();
            this.child(format!(
                "· APFS clones: {} · ≈ {} counted again{}",
                human_bytes(sharing.bytes),
                human_bytes(sharing.duplicate_bytes),
                if sharing.groups_truncated {
                    " (partial)"
                } else {
                    ""
                }
            ))
        })
        .when(errors > 0, |this| {
            this.child(div().text_color(theme.warning).child(format!(
                "· {} unreadable",
                widgets::human_count(errors)
            )))
        })
}

/// The key to the colours, and the filter: the categories, or the age
/// ramp in age mode, each a chip that cycles shown → only → hidden (see
/// `legend.rs`). It wraps onto another line rather than clip a chip.
fn legend(app: &Disktree, theme: &Theme, cx: &Context<'_, Disktree>) -> Div {
    use crate::legend::ChipId;
    let ground = palette::category_fill(theme, Category::Other, 0);
    let hatch = cx.omarchy().bright.opacity(0.5);
    let mut chips = vec![legend_chip(
        app,
        theme,
        ChipId::Reclaimable,
        widgets::hatch_swatch(hatch, ground),
        cx,
    )];
    // The set of chips that is not on screen still filters, so it shows as
    // one pill that clears it.
    let (off_screen, pill) = if app.color_mode == ColorMode::Age {
        for (index, _) in palette::AGE_BUCKETS.iter().enumerate() {
            let color = palette::age_accent(theme, index);
            chips.push(legend_chip(
                app,
                theme,
                ChipId::Age(index),
                legend_swatch(app.legend.get(ChipId::Age(index)), color),
                cx,
            ));
        }
        (ChipId::Kind(0), app.legend.kinds_summary())
    } else {
        for (index, category) in Category::LEGEND.iter().enumerate() {
            let color = palette::category_accent(theme, *category);
            chips.push(legend_chip(
                app,
                theme,
                ChipId::Kind(index),
                legend_swatch(app.legend.get(ChipId::Kind(index)), color),
                cx,
            ));
        }
        (ChipId::Age(0), app.legend.ages_summary())
    };
    let pill = pill.map(|summary| {
        legend_button(
            "legend-other",
            summary,
            true,
            "Click to clear this filter",
            theme,
            cx,
            move |this, cx| this.clear_legend(Some(off_screen), cx),
        )
    });
    let clear = app
        .legend
        .is_active()
        .then(|| show_all_button("legend-clear", theme, cx));
    // One wrapping row: Show all and the pill lead, so they are never the
    // part that moves to another line.
    div()
        .debug_selector(|| "legend".into())
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(space::XS)
        .min_w_0()
        .children(clear)
        .children(pill)
        .children(chips)
}

/// Over a mosaic or list the legend has emptied: that nothing here
/// passes, whether anything elsewhere in the scan does, and Show all.
fn legend_empty(
    app: &Disktree,
    (bytes, files): (u64, u64),
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let headline =
        format!("Nothing here passes the filter: {}", app.legend.summary());
    // The scanned root sums everything that passed, so it is only empty
    // when nothing did; anywhere else, what passed is in other folders.
    let elsewhere = if files == 0 {
        "Nothing in the scan passes it.".to_owned()
    } else {
        format!(
            "{} in {} {} elsewhere in the scan passes it; \u{232b} goes up.",
            human_bytes(bytes),
            widgets::human_count(files),
            if files == 1 { "file" } else { "files" }
        )
    };
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .debug_selector(|| "legend-empty".into())
                // It sits over the mosaic; without this a press on it
                // selected the tile beneath.
                .occlude()
                .flex()
                .flex_col()
                .items_center()
                .gap(space::SM)
                .p(space::LG)
                .bg(theme.surface)
                .border_1()
                .border_color(theme.divider())
                // Announced as it appears: the mosaic emptying says
                // nothing to someone who cannot see it.
                .child(
                    crate::semantics::status(
                        "legend-empty-status",
                        format!("{headline}. {elsewhere}"),
                    )
                    .debug_selector(|| "legend-empty-status".into()),
                )
                .child(
                    div()
                        .text_size(text::BODY)
                        .text_color(theme.bright)
                        .child(headline),
                )
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .child(elsewhere),
                )
                .child(show_all_button("legend-empty-clear", theme, cx)),
        )
}

/// Show all: clears every chip of the legend.
fn show_all_button(
    id: &'static str,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Stateful<Div> {
    legend_button(
        id,
        "Show all \u{00d7}".into(),
        false,
        "Show everything again · esc",
        theme,
        cx,
        |this, cx| this.clear_legend(None, cx),
    )
}

/// A chip's square: filled while it shows, hollow once hidden.
fn legend_swatch(state: crate::legend::Chip, color: gpui_kit::Hsla) -> Div {
    if state == crate::legend::Chip::Hidden {
        div()
            .flex_shrink_0()
            .size(size::SWATCH)
            .border_1()
            .border_color(color)
    } else {
        widgets::swatch(color)
    }
}

/// One legend chip. Filled while set to only, faded while another chip
/// of its set is, and struck through while hidden.
fn legend_chip(
    app: &Disktree,
    theme: &Theme,
    id: crate::legend::ChipId,
    swatch: Div,
    cx: &Context<'_, Disktree>,
) -> Stateful<Div> {
    use crate::legend::Chip;
    let state = app.legend.get(id);
    let only = state == Chip::Only;
    let faded = state == Chip::Shown && app.legend.narrowed(id);
    let hover = theme.foreground.opacity(0.06);
    let key = id.key();
    with_tooltip(
        div()
            .id(ElementId::Name(key.clone().into()))
            .debug_selector(move || key)
            .role(gpui_kit::Role::Button)
            .aria_label(id.label())
            .aria_value(state.spoken())
            .focusable()
            .tab_stop(true)
            .flex()
            .flex_row()
            .items_center()
            .gap(space::XS)
            .flex_shrink_0()
            .min_h(size::POINTER_TARGET)
            .px(space::XS)
            .border_1()
            .border_color(if only {
                theme.foreground.opacity(0.35)
            } else {
                gpui_kit::transparent_black()
            })
            .when(only, |chip| chip.bg(theme.foreground.opacity(0.14)))
            .when(!only, |chip| chip.hover(move |style| style.bg(hover)))
            .when(faded, |chip| chip.opacity(0.42))
            .when(state == Chip::Hidden, |chip| chip.opacity(0.6))
            .cursor_pointer()
            .on_key_down(cx.listener(
                move |this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        this.cycle_legend(id, cx);
                        cx.stop_propagation();
                    }
                },
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.cycle_legend(id, cx);
                window.focus(&this.focus, cx);
            }))
            .child(swatch)
            .child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(if only {
                        theme.bright
                    } else {
                        theme.secondary
                    })
                    .when(state == Chip::Hidden, Styled::line_through)
                    .child(id.label()),
            ),
        app.legend.hint(id),
    )
}

/// Show all, or the pill that stands for the chips not on screen. A tab
/// stop that Space and Enter press, like the chips beside it.
fn legend_button(
    id: &'static str,
    label: String,
    filled: bool,
    hint: &'static str,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
    press: impl Fn(&mut Disktree, &mut Context<'_, Disktree>) + Clone + 'static,
) -> Stateful<Div> {
    let hover = theme.foreground.opacity(0.1);
    let on_key = press.clone();
    with_tooltip(
        div()
            .id(id)
            .debug_selector(move || id.into())
            .role(gpui_kit::Role::Button)
            .aria_label(label.clone())
            .focusable()
            .tab_stop(true)
            .on_key_down(cx.listener(
                move |this, event: &KeyDownEvent, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        on_key(this, cx);
                        window.focus(&this.focus, cx);
                        cx.stop_propagation();
                    }
                },
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                press(this, cx);
                window.focus(&this.focus, cx);
            }))
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .min_h(size::POINTER_TARGET)
            .px(space::SM)
            .text_size(text::CAPTION)
            .text_color(theme.bright)
            .when(filled, |pill| pill.bg(theme.foreground.opacity(0.14)))
            .when(!filled, |button| {
                button
                    .border_1()
                    .border_color(theme.foreground.opacity(0.4))
            })
            .hover(move |style| style.bg(hover))
            .cursor_pointer()
            .child(label),
        hint,
    )
}

// ── side panel ──────────────────────────────────────────────────────────

/// Selection, worth a look, marked, and the disk: everything a decision
/// needs, next to the mosaic rather than under it.
fn side_panel(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let indexing = app.is_indexing();
    // Once per frame: the marked list, the marked total and the disk all
    // read it, and building it reads the mount table.
    let plan = app.plan();
    let rule = || div().h(px(1.)).bg(theme.divider());
    // A hairline to look at, a wider strip to grab; double-click resets.
    let handle = div()
        .id("panel-handle")
        .debug_selector(|| "panel-handle".into())
        .absolute()
        .top_0()
        .bottom_0()
        .left(px(-3.))
        .w(px(6.))
        .cursor_col_resize()
        .hover(|style| style.bg(theme.accent.opacity(0.35)))
        .on_drag(PanelDrag, |_, _, _, cx| cx.new(|_| NoGhost))
        .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
            if event.click_count() >= 2 {
                this.panel_rems = PANEL_REMS;
                cx.notify();
            }
        }));
    div()
        .relative()
        .flex()
        .flex_col()
        .gap(space::LG)
        .w(Rems(app.panel_rems))
        .flex_shrink_0()
        .min_h_0()
        .px(space::LG)
        .py(space::LG)
        .border_l_1()
        .border_color(theme.divider())
        .bg(theme.surface)
        .child(handle)
        .children((!indexing).then(|| selection_section(app, theme, cx)))
        .children((!indexing).then(rule))
        // Progress can exceed a short, zoomed window. Let it scroll with the
        // lists while the volume's free space stays visible below them.
        .child(panel_lists(app, &plan, theme, cx))
        .children(scan_error_line(app, theme))
        .children(privacy_line(app, theme, cx))
        .children(administrator_line(app, theme, cx))
        .children(marked_total(app, &plan, theme, cx))
        .child(disk_section(app, &plan, theme, cx).flex_shrink_0())
        .child(agent_line(app, theme))
}

/// The panel's scrolling lists, each under a header that stays in sight
/// while its list scrolls ([`pinned_head`]).
fn panel_lists(
    app: &Disktree,
    plan: &Plan,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let rule = || div().h(px(1.)).bg(theme.divider()).into_any_element();
    // Each child of the scroll, with the header it starts with, if any: the
    // index is what the scroll handle measures by.
    let mut lists: Vec<(AnyElement, Option<Div>)> = Vec::new();
    if app.is_indexing() {
        lists.push((
            indexing_progress(app, theme, cx)
                .flex_shrink_0()
                .into_any_element(),
            Some(section_head(widgets::eyebrow("Indexing", cx))),
        ));
        lists.push((rule(), None));
    }
    // The marked list leads: the pinned total below takes room from this
    // scroll, and on a short window what comes last scrolls out of sight.
    // Suggestions can; the cart cannot.
    lists.push((
        marked_section(app, plan, theme, cx).into_any_element(),
        Some(marked_head(app, cx)),
    ));
    lists.push((rule(), None));
    lists.push((
        worth_section(app, theme, cx).into_any_element(),
        Some(worth_head(app, theme, cx)),
    ));
    if let Some(services) = macos_services_section(app, theme, cx) {
        lists.push((rule(), None));
        lists.push((
            services.into_any_element(),
            Some(section_head(widgets::eyebrow("macOS service stores", cx))),
        ));
    }
    let pinned = pinned_head(
        &app.panel_scroll,
        lists.iter().map(|(_, head)| head.is_some()),
        px(size::SECTION_HEAD.0 * app.rem),
    );
    // `pinned_head` measures the last layout. When the lists change without
    // a scroll (a mark, the insights, the end of indexing) that layout is
    // a frame old, so draw again once it settles; an unchanged layout asks
    // for nothing, so this never loops.
    let scroll_handle = app.panel_scroll.clone();
    let seen = app.panel_layout.clone();
    let mut scroll = div()
        .on_children_prepainted(move |_, window, _| {
            let now: Vec<_> = (0..)
                .map_while(|index| scroll_handle.bounds_for_item(index))
                .collect();
            if *seen.borrow() != now {
                *seen.borrow_mut() = now;
                window.refresh();
            }
        })
        .id("panel-lists")
        .debug_selector(|| "side-panel-scroll".into())
        .track_scroll(&app.panel_scroll)
        .flex()
        .flex_col()
        .gap(space::LG)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll();
    let mut heads = Vec::new();
    for (list, head) in lists {
        scroll = scroll.child(list);
        heads.push(head);
    }
    div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(scroll)
        .children(pinned.and_then(|(index, top)| {
            let head = heads.get_mut(index)?.take()?;
            Some(
                div()
                    .id("pinned-head")
                    .debug_selector(|| "pinned-head".into())
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(top)
                    // Opaque, with a hairline and never a shadow, as
                    // PwrGit's and PwrAgnt's pinned headers are; the rows
                    // slide under it, and a click must not reach them
                    // through it, though a wheel over it still scrolls
                    // the list it heads.
                    .bg(theme.surface)
                    .border_b_1()
                    .border_color(theme.divider())
                    .block_mouse_except_scroll()
                    // A copy of the header in the list: read once.
                    .a11y_synthetic_children(|b| b.parent_node().set_hidden())
                    .child(head),
            )
        }))
}

/// Which header to pin over the panel's lists, and how far up the next
/// section has pushed it: the section whose top has scrolled past the top
/// of the list while some of it is still in sight. `heads` says which of
/// the scroll's children start with a header of height `head`. Measured
/// from the last layout, which only a change of content moves, against the
/// current offset, which every wheel tick moves and redraws.
fn pinned_head(
    scroll: &gpui_kit::ScrollHandle,
    heads: impl Iterator<Item = bool>,
    head: gpui_kit::Pixels,
) -> Option<(usize, gpui_kit::Pixels)> {
    let viewport = scroll.bounds();
    let offset = scroll.offset().y;
    heads
        .enumerate()
        .filter(|(_, has_head)| *has_head)
        .find_map(|(index, _)| {
            let bounds = scroll.bounds_for_item(index)?;
            let top = bounds.top() - viewport.top() + offset;
            let bottom = bounds.bottom() - viewport.top() + offset;
            // Pinned once its own header starts to leave; gone once the
            // section has. Before then the next one pushes it up: the
            // pinned header never overlaps the section after it.
            (top < px(0.) && bottom > px(0.))
                .then(|| (index, (bottom - head).min(px(0.))))
        })
}

/// A section's header row, at the one height [`pinned_head`] pushes by.
fn section_head(content: Div) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_shrink_0()
        .h(size::SECTION_HEAD)
        .child(content)
}

fn marked_head(app: &Disktree, cx: &Context<'_, Disktree>) -> Div {
    let count = app.marks.len();
    section_head(widgets::eyebrow(
        if count == 0 {
            "Marked".to_string()
        } else {
            format!("Marked · {count}")
        },
        cx,
    ))
}

fn worth_head(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let total: u64 = app.insights.iter().map(|candidate| candidate.bytes).sum();
    section_head(widgets::eyebrow("Worth a look", cx)).when(total > 0, |head| {
        head.child(
            div()
                .text_size(text::CAPTION)
                .text_color(palette::highlight(theme))
                .child(human_bytes(total)),
        )
    })
}

/// What the keys act on: its name and place, its size set large with its
/// share of the scan, when it was last written, what git says, and the two
/// things to do with it.
fn selection_section(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let section = div()
        .flex()
        .flex_col()
        .gap(space::MD)
        .child(widgets::eyebrow("Selection", cx));
    let target = app.action_target().unwrap_or_else(|| app.crumbs.clone());
    let Some(node) = app.node_at(&target) else {
        return section.child(
            div()
                .text_color(theme.secondary)
                .child("Point at a tile or select one with the arrows"),
        );
    };
    let path = app.path_at(&target);
    let root_value = app.tree().map_or(0, |tree| tree.bytes);
    let marked = path
        .as_deref()
        .is_some_and(|path| app.marks.contains(path, app.path_style()));
    let covered_by = marks_ancestor(app, path.as_deref());
    let is_current_root = target == app.crumbs;
    let highlight = palette::highlight(theme);

    let identity =
        div()
            .flex()
            .flex_col()
            .gap(space::XS)
            .min_w_0()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::SM)
                    .min_w_0()
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(space::XS)
                            .h(text::HEADING)
                            .bg(palette::category_accent(theme, node.category)),
                    )
                    .child(
                        div()
                            .text_size(text::HEADING)
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.bright)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(node.name().to_string()),
                    ),
            )
            .child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary)
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(path.as_deref().map_or_else(String::new, |path| {
                        app.display_path(path)
                    })),
            );

    let (mut number, mut unit) = match app.options.metric {
        Metric::Bytes => widgets::split_size(&human_bytes(node.bytes)),
        Metric::Files => (widgets::human_count(node.files), "files".into()),
    };
    if disktree_core::macos_services::is_service_name(node.name())
        && let (Some(tree), Some(path)) = (app.tree(), path.as_deref())
    {
        use disktree_core::macos_services::{Usage, usage};
        match usage(&app.root_path, tree, &app.options, path) {
            Usage::Unavailable | Usage::OutsideScan => {
                number = "Unavailable".into();
                unit.clear();
            }
            Usage::Incomplete(_) => {
                number = format!("≥ {number}");
                unit.push_str(" measured");
            }
            Usage::Measured(_) => {}
        }
    }
    let share = node.bytes as f32 / root_value.max(1) as f32;
    let measure = div()
        .flex()
        .flex_col()
        .gap(space::SM)
        .child(widgets::measure(
            number,
            text::DISPLAY,
            unit,
            text::TITLE,
            cx,
        ))
        .child(widgets::bar(share, highlight, cx))
        .when_some(clone_split(node), |this, split| {
            this.child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary)
                    .child(split),
            )
        })
        .when(node.sharing().estimate.is_some(), |this| {
            // Said as this entry's own estimate, so it is never read as
            // what the marks free together (that is the Marked block's).
            this.child(
                div()
                    .debug_selector(|| "selection-frees".into())
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary)
                    .child(format!(
                        "Removed on its own, frees ≈ {}",
                        savings(
                            node.sharing().reclaimable(node.bytes),
                            node.sharing().possible(node.bytes)
                        )
                    )),
            )
            .when(node.sharing().unknown_files > 0, |this| {
                this.child("Some clone private sizes are unavailable")
            })
        });

    // A filter changes what is shown, never what a mark takes: say so
    // before Space marks a folder that holds more than is on screen.
    let partly_shown = app
        .legend_matches
        .as_deref()
        .filter(|_| !app.legend_pending)
        .and_then(|matches| match matches.keep_at(&target, node) {
            Some(Keep::Partial { bytes, .. }) => Some(bytes),
            _ => None,
        });
    let measure = measure.when_some(partly_shown, |this, shown| {
        this.child(
            div()
                .debug_selector(|| "selection-partly-shown".into())
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(format!(
                    "{} passes the filter. Marking takes all {}, including \
                     what is filtered out.",
                    human_bytes(shown),
                    human_bytes(node.bytes)
                )),
        )
    });

    let fourth = if app.remote.is_none()
        && node.is_dir()
        && let Some(path) = &path
        // What `panel_checkout` asked about: a selected tile, never the
        // directory on screen.
        && git_target(app).is_some()
        && app.known_checkout(path)
    {
        let shown = app.git.shown(path);
        let value = match shown {
            Shown::Answer(probe) => probe.summary(),
            Shown::Asking => "asking\u{2026}".to_string(),
        };
        let clean = matches!(
            shown,
            Shown::Answer(Probe::Known(state)) if state.is_clean()
        );
        widgets::figure(
            "Git",
            value,
            if clean { theme.success } else { theme.bright },
            cx,
        )
    } else {
        let kind = node.reclaim.map_or_else(
            || node.category.label().to_string(),
            |reason| {
                format!("{} \u{00b7} {}", node.category.label(), reason.label())
            },
        );
        widgets::figure("Kind", kind, theme.bright, cx)
    };
    let grid = div()
        .flex()
        .flex_col()
        .gap(space::MD)
        .child(
            div()
                .flex()
                .flex_row()
                .child(div().flex_1().min_w_0().child(widgets::figure(
                    "Of scan",
                    widgets::percent(node.bytes, root_value),
                    theme.bright,
                    cx,
                )))
                .child(div().flex_1().min_w_0().child(widgets::figure(
                    "Files",
                    widgets::human_count(node.files),
                    theme.bright,
                    cx,
                ))),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .child(div().flex_1().min_w_0().child(widgets::figure(
                    "Last write",
                    widgets::ago(crate::state::now_seconds(), node.modified),
                    theme.bright,
                    cx,
                )))
                .child(div().flex_1().min_w_0().child(fourth)),
        );

    // Only states that change the decision earn a badge.
    let mut chips = Vec::new();
    // Gone from the disk, still in the scan: nothing to mark until a rescan.
    let removed = path.as_deref().is_some_and(|path| app.was_removed(path));
    if removed {
        chips.push(widgets::chip(
            "Removed since the scan",
            theme.secondary,
            cx,
        ));
    }
    if marked {
        chips.push(widgets::chip("Marked", theme.danger, cx));
    }
    if let Some(ancestor) = &covered_by {
        chips.push(widgets::chip(
            format!("Goes with {ancestor}"),
            theme.danger,
            cx,
        ));
    }
    if node.read_error {
        chips.push(widgets::chip("Partly unreadable", theme.warning, cx));
    }
    let badges = (!chips.is_empty()).then(|| {
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(space::XS)
            .children(chips)
    });

    // Open is secondary: Enter already does it. Marking is the action this
    // tool exists for, so it takes the highlight.
    let mut actions = div().flex().flex_row().gap(space::SM);
    if !is_current_root && node.is_dir() {
        let crumbs = target.clone();
        actions = actions.child(
            button("open", "Open", ButtonVariant::Outline, cx)
                .tab_stop(true)
                .flex_1()
                .justify_center()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.go_to(crumbs.clone(), cx);
                    window.focus(&this.focus, cx);
                })),
        );
    }
    // A new mark is judged before it is offered: what the guards refuse is
    // never offered at all, and past the limit only through Mark anyway.
    let check = if is_current_root || removed || marked || covered_by.is_some()
    {
        MarkCheck::Allowed
    } else {
        app.target_at(&target)
            .map_or(MarkCheck::Allowed, |candidate| app.mark_check(&candidate))
    };
    let guard = mark_guard_note(app, &check, node.is_dir(), theme, cx);
    let mut menu = None;
    if let MarkCheck::Protected(_) = &check {
        actions = actions.child(
            button("mark", "Can\u{2019}t remove", ButtonVariant::Outline, cx)
                .debug_selector(|| "inspector-mark".into())
                .tab_stop(true)
                .flex_1()
                .justify_center()
                .semantic_disabled(true),
        );
    } else if let (MarkCheck::OverLimit(share), Some(path)) = (&check, &path) {
        let open = app.mark_menu.as_deref() == Some(path.as_path());
        actions = actions.child(over_limit_mark(
            share,
            path.clone(),
            open,
            theme,
            cx,
        ));
        menu = open.then(|| mark_menu(share, target.clone(), theme, cx));
    } else if !is_current_root && !removed {
        // Inside a marked directory there is nothing to mark on its own: it
        // goes with that directory, so the button offers to keep it by
        // unmarking the directory instead.
        let ancestor =
            path.as_deref().and_then(|path| app.marked_ancestor(path));
        let crumbs = target;
        let label = match &ancestor {
            Some(ancestor) if !marked => {
                format!("Unmark {}", short_name(ancestor, app.path_style()))
            }
            _ if marked => "Unmark".to_string(),
            _ => "Mark for removal".to_string(),
        };
        let mark = button("mark", label, ButtonVariant::Primary, cx)
            .debug_selector(|| "inspector-mark".into())
            .tab_stop(true)
            .flex_1()
            .justify_center()
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(ancestor) = ancestor.as_deref().filter(|_| !marked)
                {
                    this.unmark(ancestor, cx);
                } else {
                    this.toggle_mark(&crumbs.clone(), cx);
                }
                window.focus(&this.focus, cx);
            }));
        let filled = !marked && covered_by.is_none();
        actions = actions.child(if filled {
            let on = palette::on_highlight(theme);
            let hover_fill = palette::mix(theme.background, highlight, 0.85);
            let hover_ink = palette::ink_on(on, hover_fill, 4.5);
            mark.bg(highlight)
                .border_color(highlight)
                .text_color(on)
                .font_weight(FontWeight::SEMIBOLD)
                .hover(move |style| {
                    style
                        .bg(hover_fill)
                        .text_color(hover_ink)
                        .border_color(highlight)
                })
        } else {
            mark
        });
    }

    section
        .child(identity)
        .child(measure)
        .child(grid)
        .children(badges)
        .children(guard)
        .child(actions)
        .children(menu)
        .children(match node.category {
            Category::FilesystemEvents => Some(service_guidance(
                disktree_core::macos_services::Service::Fsevents,
                theme,
                cx,
            )),
            Category::Spotlight => Some(service_guidance(
                disktree_core::macos_services::Service::Spotlight,
                theme,
                cx,
            )),
            _ => None,
        })
}

/// Why Mark is not the plain button: a rule in the warning colour, a chip
/// and a line, above the actions so it is read before them.
fn mark_guard_note(
    app: &Disktree,
    check: &MarkCheck,
    is_dir: bool,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Option<Div> {
    let (chip, title, body, hint) = match check {
        MarkCheck::Allowed => return None,
        MarkCheck::Protected(reason) => {
            // A home folder holds what is worth removing; the system trees
            // and mount points do not, so only there is Open the way on.
            let home = ["home folder", "user profile", "home directory"]
                .iter()
                .any(|word| reason.contains(word));
            (
                "Protected",
                "PwrDisk never removes this".to_string(),
                sentence(reason),
                (home && is_dir)
                    .then_some("Open it to mark what is inside.".to_string()),
            )
        }
        MarkCheck::OverLimit(share) => {
            let space = app.space.map_or(0, |space| space.total);
            let allowed = u64::try_from(
                u128::from(space) * u128::from(share.limit) / 100,
            )
            .unwrap_or(u64::MAX);
            (
                "Over limit",
                format!("{}% of this disk", share.percent),
                format!(
                    "One removal may take up to {}% of a disk ({} of {}). \
                     Mark something smaller, or use \u{25be} to mark it \
                     anyway.",
                    share.limit,
                    human_bytes(allowed),
                    human_bytes(space)
                ),
                Some("Change the limit in Settings \u{203a} Safety".into()),
            )
        }
    };
    Some(
        div()
            .debug_selector(|| "mark-guard".into())
            .flex()
            .flex_row()
            .gap(space::SM)
            .child(div().flex_shrink_0().w(space::XXS).bg(theme.warning))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::XS)
                    .min_w_0()
                    .py(space::XS)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap(space::SM)
                            .child(widgets::chip(chip, theme.warning, cx))
                            .child(
                                div()
                                    .text_color(theme.bright)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            ),
                    )
                    .child(
                        div()
                            .text_size(text::CAPTION)
                            .text_color(theme.foreground)
                            .child(body),
                    )
                    .children(hint.map(|hint| {
                        div()
                            .text_size(text::CAPTION)
                            .text_color(theme.secondary)
                            .child(hint)
                    })),
            ),
    )
}

/// A guard's reason as a sentence: capital first, full stop last.
fn sentence(reason: &str) -> String {
    let mut chars = reason.chars();
    chars.next().map_or_else(String::new, |first| {
        let mut text: String = first.to_uppercase().chain(chars).collect();
        if !text.ends_with('.') {
            text.push('.');
        }
        text
    })
}

/// Mark past the limit: the main part says why it does nothing, and the
/// chevron beside it, the one live part, opens the menu with Mark anyway.
fn over_limit_mark(
    share: &Share,
    path: std::path::PathBuf,
    open: bool,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    div()
        .flex()
        .flex_row()
        .flex_1()
        .child(
            button(
                "mark",
                format!("Over {}% limit", share.limit),
                ButtonVariant::Outline,
                cx,
            )
            .debug_selector(|| "inspector-mark".into())
            .flex_1()
            .justify_center()
            .semantic_disabled(true),
        )
        .child(
            button("mark-more", "\u{25be}", ButtonVariant::Outline, cx)
                .debug_selector(|| "inspector-mark-more".into())
                .aria_label("More ways to mark")
                .tab_stop(true)
                // At body size the glyph reads as a dot beside the label.
                .text_size(text::HEADING)
                .border_color(theme.warning)
                .text_color(theme.warning)
                .when(open, |button| button.bg(theme.warning.opacity(0.16)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.mark_menu = (!open).then(|| path.clone());
                    window.focus(&this.focus, cx);
                    cx.notify();
                })),
        )
}

/// What the chevron holds: Mark anyway, which names its cost, and the
/// way to the setting. Drawn in the panel's flow, below the actions, so
/// the panel's scroll never clips it; its rows are buttons, so Tab and a
/// screen reader reach them as they reach Mark.
fn mark_menu(
    share: &Share,
    crumbs: Vec<usize>,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Stateful<Div> {
    let cost = format!(
        "{}% of this disk \u{00b7} Review will ask you to confirm",
        share.percent
    );
    div()
        .id("mark-menu")
        .debug_selector(|| "mark-menu".into())
        .occlude()
        .flex()
        .flex_col()
        .gap(space::SM)
        .p(space::SM)
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .shadow_lg()
        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
            this.mark_menu = None;
            cx.notify();
        }))
        .child(
            button("mark-anyway", "Mark anyway", ButtonVariant::Outline, cx)
                .debug_selector(|| "mark-anyway".into())
                .aria_description(cost.clone())
                .tab_stop(true)
                .justify_center()
                .border_color(theme.warning)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.mark_anyway(&crumbs, cx);
                    window.focus(&this.focus, cx);
                })),
        )
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(cost),
        )
        .child(
            button(
                "mark-safety",
                "Safety settings\u{2026}",
                ButtonVariant::Outline,
                cx,
            )
            .debug_selector(|| "mark-safety".into())
            .tab_stop(true)
            .justify_center()
            .on_click(cx.listener(|this, _, window, cx| {
                this.mark_menu = None;
                crate::settings::open(crate::settings::Pane::Safety, cx);
                window.focus(&this.focus, cx);
            })),
        )
}

fn service_guidance(
    service: disktree_core::macos_services::Service,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(space::SM)
        .text_size(text::CAPTION)
        .text_color(theme.secondary)
        .child(service.guidance())
        .child(
            button(
                ElementId::Name(
                    format!("service-docs-{}", service.name()).into(),
                ),
                "Apple's guidance",
                ButtonVariant::Outline,
                cx,
            )
            .tab_stop(true)
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.open_url(service.documentation());
                window.focus(&this.focus, cx);
            })),
        )
}

/// Offer service figures only when this local scan measured a store.
fn macos_services_section(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Option<impl IntoElement> {
    use disktree_core::macos_services::{Service, Usage, stores_volume, usage};
    if !cfg!(target_os = "macos") || app.remote.is_some() {
        return None;
    }
    let volume = stores_volume(&app.root_path)?;
    let tree = app.tree()?;
    let stores = Service::ALL.map(|service| {
        let path = volume.join(service.name());
        let state = usage(&app.root_path, tree, &app.options, &path);
        (service, path, state)
    });
    if !stores.iter().any(|(_, _, state)| {
        matches!(state, Usage::Measured(_) | Usage::Incomplete(_))
    }) {
        return None;
    }
    let mut section = div()
        .id("macos-services")
        .debug_selector(|| "macos-services".into())
        .flex()
        .flex_col()
        .gap(space::SM)
        .child(section_head(widgets::eyebrow("macOS service stores", cx)));
    let mut incomplete = false;
    for (service, path, state) in stores {
        let measured = match state {
            Usage::Measured(bytes) => human_bytes(bytes),
            Usage::Incomplete(bytes) => {
                incomplete = true;
                format!("{} measured · incomplete", human_bytes(bytes))
            }
            Usage::Unavailable => {
                incomplete = true;
                "Unavailable".into()
            }
            Usage::OutsideScan => {
                incomplete = true;
                "Outside scan".into()
            }
        };
        section = section.child(
            div()
                .flex()
                .flex_col()
                .gap(space::XS)
                .child(widgets::figure(
                    service.label(),
                    measured,
                    theme.bright,
                    cx,
                ))
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .child(path.display().to_string()),
                ),
        );
    }
    if incomplete {
        section = section.child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(
                    "Unknown contents are not zero. Figures marked incomplete \
                     include only the files this scan could measure.",
                ),
        );
    }
    Some(
        section.child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(if app.options.apparent_size {
                    "Apparent file sizes, already included in the scan. Switch \
                 to disk usage for allocated bytes."
                } else {
                    "Allocated file bytes, already included in the scan. APFS \
                 shared blocks and snapshots can change actual space freed."
                }),
        ),
    )
}

/// The biggest things that could plausibly go, with their total.
fn worth_section(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let largest = app.insights.first().map_or(1, |candidate| candidate.bytes);
    let mut section = div()
        .debug_selector(|| "worth-section".into())
        .flex()
        .flex_col()
        .gap(space::XS)
        .child(worth_head(app, theme, cx));
    if app.insights.is_empty() {
        return section.child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(if app.tree().is_some() {
                    "Nothing obviously disposable"
                } else {
                    "Waiting for the scan"
                }),
        );
    }
    let selected = app.action_target();
    for (index, candidate) in app.insights.iter().enumerate() {
        let Some(node) = app.node_at(&candidate.crumbs) else {
            continue;
        };
        let (title, detail) = insight_text(app, candidate);
        let accent = palette::category_accent(theme, node.category);
        let active = selected.as_deref() == Some(candidate.crumbs.as_slice());
        let crumbs = candidate.crumbs.clone();
        section = section.child(
            div()
                .id(ElementId::Name(format!("insight-{index}").into()))
                .flex()
                .flex_row()
                .items_center()
                .gap(space::SM)
                .px(space::SM)
                .py(space::XS)
                .when(active, |this| this.bg(theme.hover_fill()))
                .hover(|style| style.bg(theme.hover_fill()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.reveal(crumbs.clone(), cx);
                    window.focus(&this.focus, cx);
                }))
                .child(div().flex_shrink_0().w(px(2.)).h(space::XL).bg(accent))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(text::BODY)
                                .text_color(theme.bright)
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(text::CAPTION)
                                .text_color(theme.secondary.opacity(0.8))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(detail),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_end()
                        .gap(space::XS)
                        .flex_shrink_0()
                        .w(size::ROW_BAR)
                        .child(
                            div()
                                .text_size(text::BODY)
                                .text_color(theme.bright)
                                .child(human_bytes(candidate.bytes)),
                        )
                        .child(widgets::bar(
                            candidate.bytes as f32 / largest.max(1) as f32,
                            accent,
                            cx,
                        )),
                ),
        );
    }
    section
}

/// A finding's title, as the last two parts of its path, and why it is on
/// the list.
fn insight_text(app: &Disktree, candidate: &Candidate) -> (String, String) {
    let tree = app.tree();
    let chain = tree
        .map_or_else(Vec::new, |tree| tree.resolve_chain(&candidate.crumbs));
    let names: Vec<&str> =
        chain.iter().skip(1).map(|node| node.name()).collect();
    let tail = names[names.len().saturating_sub(2)..].join("/");
    match &candidate.finding {
        Finding::Reclaimable(reason) => (tail, reason.label().to_string()),
        Finding::Worktrees { count, oldest_days } => (
            tail,
            format!(
                "{count} worktree{} \u{00b7} oldest {oldest_days} d",
                if *count == 1 { "" } else { "s" }
            ),
        ),
        Finding::StaleExperiments { count } => (
            format!("{tail} > {STALE_DAYS} days"),
            format!(
                "{count} experiment{} untouched",
                if *count == 1 { "" } else { "s" }
            ),
        ),
    }
}

/// What is queued for removal, each with a way off the list.
fn marked_section(
    app: &Disktree,
    plan: &Plan,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let count = app.marks.len();
    // The header leads, so it is the section's top edge: that is where
    // `pinned_head` takes over from it.
    let mut section = div().flex().flex_col().gap(space::XS)
        .child(marked_head(app, cx))
        .child(crate::semantics::status("marked-summary", format!("{} marked paths; {} listed bytes; {} included, {} blocked, {} covered. Estimated reclaimable: {}. Projections use only the measured volume.",
            count, plan.bytes(), plan.targets.len(), plan.blocked.len(), plan.covered.len(), savings(plan.reclaimable_bytes(), plan.possible_bytes()))));
    if count == 0 {
        return section.child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child("Space marks the tile you point at"),
        );
    }
    for (index, item) in app.marks.items().iter().take(MARKED_ROWS).enumerate()
    {
        let category = app
            .crumbs_for_path(&item.path)
            .and_then(|crumbs| app.node_at(&crumbs))
            .map_or(Category::Other, |node| node.category);
        let path = item.path.clone();
        section = section.child(
            div()
                .debug_selector(move || format!("marked-row-{index}"))
                .flex()
                .flex_row()
                .items_center()
                .gap(space::SM)
                .text_size(text::CAPTION)
                .child(
                    div()
                        .flex_shrink_0()
                        .size(space::SM)
                        .rounded_full()
                        .bg(palette::category_accent(theme, category)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.foreground)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(app.display_path(&item.path)),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.secondary)
                        .child(human_bytes(item.bytes)),
                )
                .child(
                    div()
                        .id(ElementId::Name(format!("unmark-{index}").into()))
                        .flex_shrink_0()
                        .px(space::XS)
                        .text_color(theme.secondary.opacity(0.7))
                        .hover(|style| style.text_color(theme.danger))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.unmark(&path, cx);
                            window.focus(&this.focus, cx);
                        }))
                        .child("\u{00d7}"),
                ),
        );
    }
    if count > MARKED_ROWS {
        section = section.child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(format!(
                    "+{} more on the review screen",
                    count - MARKED_ROWS
                )),
        );
    }
    if !plan.covered.is_empty() || !plan.blocked.is_empty() {
        section = section.child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(format!(
                    "{} nested \u{00b7} {} kept back",
                    plan.covered.len(),
                    plan.blocked.len()
                )),
        );
    }
    section
}

/// Marked rows the panel lists before pointing at the review screen.
const MARKED_ROWS: usize = 6;

/// Why the scan failed, above the disk. It stays while it holds: it
/// describes the scan, not something that just happened, which a toast
/// says (`toasts::notice`).
fn scan_error_line(app: &Disktree, theme: &Theme) -> Option<Div> {
    let message = app.scan_error.clone()?;
    let color = theme.danger;
    Some(
        div()
            .px(space::SM)
            .py(space::XS)
            .border_1()
            .border_color(color.opacity(0.5))
            .text_color(color)
            .text_size(text::CAPTION)
            .child(message),
    )
}

/// Why folders were unreadable on macOS, and the one place to fix it.
///
/// Only once the scan has hit something it could not read and the process is
/// known to lack Full Disk Access: an unreadable folder has other causes, and
/// the hint must not nag when it would not help. A grant needs a relaunch, and
/// started from a terminal it is the terminal that needs it.
fn privacy_line(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Option<Div> {
    let errors = app
        .progress
        .errors
        .saturating_sub(app.progress.service_errors);
    if errors == 0 || app.full_disk_access != Some(false) {
        return None;
    }
    let color = theme.warning;
    let noun = if errors == 1 { "item" } else { "items" };
    Some(
        div()
            .debug_selector(|| "privacy-hint".into())
            .flex()
            .flex_col()
            .gap(space::SM)
            .px(space::SM)
            .py(space::SM)
            .border_1()
            .border_color(color.opacity(0.5))
            .text_size(text::CAPTION)
            .child(div().text_color(color).child(format!(
                "macOS kept {} {noun} unreadable. Give PwrDisk Full Disk \
                 Access, or your terminal if you started it there, then \
                 reopen it. Filesystem permissions can still restrict \
                 administrator-owned service stores.",
                widgets::human_count(errors)
            )))
            .child(
                button(
                    "privacy",
                    "Open Privacy Settings",
                    ButtonVariant::Outline,
                    cx,
                )
                .tab_stop(true)
                .justify_center()
                .on_click(cx.listener(
                    |this, _, window, cx| {
                        cx.open_url(
                            disktree_core::access::FULL_DISK_ACCESS_SETTINGS,
                        );
                        window.focus(&this.focus, cx);
                    },
                )),
            ),
    )
}

/// The faster, fuller scan an administrator gets on Windows, and the way to
/// it. Only where it would help: on a whole NTFS drive walked without `-l`,
/// which is then read from its file table instead, or once the walk was
/// refused something. Hidden once asked, and while a removal runs, which a
/// restart would cut short.
fn administrator_line(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Option<Div> {
    if app.remote.is_some()
        || app.administrator != Some(false)
        || app.restarting
        || app.run.is_some()
    {
        return None;
    }
    let errors = app.progress.errors;
    let message = if errors > 0 {
        let noun = if errors == 1 { "item" } else { "items" };
        // With `-l` the elevated copy walks too: only the reading is gained.
        let faster = if app.options.follow_links {
            ""
        } else {
            ", and a whole drive several times faster"
        };
        format!(
            "Windows kept {} {noun} unreadable. Run as administrator to read \
             them{faster}.",
            widgets::human_count(errors)
        )
    } else if app.file_table && !app.options.follow_links {
        "Run as administrator to read the whole drive from its file table: \
         several times faster than this walk."
            .to_owned()
    } else {
        return None;
    };
    // Marks live only in this process; the new one starts without them.
    let message = if app.marks.is_empty() {
        message
    } else {
        format!("{message} Restarting drops the marks.")
    };
    let color = theme.warning;
    Some(
        div()
            .flex()
            .flex_col()
            .gap(space::SM)
            .px(space::SM)
            .py(space::SM)
            .border_1()
            .border_color(color.opacity(0.5))
            .text_size(text::CAPTION)
            .child(div().text_color(color).child(message))
            .child(
                button(
                    "administrator",
                    "Restart as Administrator",
                    ButtonVariant::Outline,
                    cx,
                )
                .tab_stop(true)
                .justify_center()
                .on_click(cx.listener(
                    |this, _, window, cx| {
                        window.focus(&this.focus, cx);
                        this.restart_as_administrator(cx);
                    },
                )),
            ),
    )
}

/// What the removals since the scan freed, measured, with the estimates
/// beside it: removing no longer rescans, so the tree on screen is the
/// last scan's minus what went, and this is the running account.
fn freed_since_scan(app: &Disktree, theme: &Theme) -> Option<Div> {
    let totals =
        crate::ledger::Ledger::totals(app.ledger.since(app.scans_done))?;
    let noun = if totals.count == 1 {
        "removal"
    } else {
        "removals"
    };
    let estimates = match totals.private {
        Some(private) => format!(
            "{} reported \u{b7} {} private",
            human_bytes(totals.reported),
            human_bytes(private)
        ),
        None => format!("{} reported", human_bytes(totals.reported)),
    };
    Some(
        div()
            .debug_selector(|| "freed-since-scan".into())
            .flex()
            .flex_col()
            .text_size(text::CAPTION)
            .text_color(theme.secondary)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(space::SM)
                    .child(format!(
                        "Freed since scan \u{b7} {} {noun}",
                        totals.count
                    ))
                    .child(div().flex_1())
                    .children(totals.observed.map(|delta| {
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if delta >= 0 {
                                theme.success
                            } else {
                                theme.warning
                            })
                            .child(signed_bytes(delta))
                    })),
            )
            .child(estimates),
    )
}

/// Free space on the volume now, and after the marks go.
fn disk_section(
    app: &Disktree,
    plan: &Plan,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let device = app.device.clone().unwrap_or_default();
    // The disk header doubles as the way to another volume: it opens the
    // picker, like `V` does. A plain label would hide that the scan can move.
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .child(widgets::eyebrow("Disk", cx))
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary.opacity(0.6))
                .child(device),
        )
        .child(div().flex_1())
        .child(
            button("volumes", "Source…", ButtonVariant::Secondary, cx)
                .tab_stop(true)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_volumes(cx);
                    this.apply_focus(window, cx);
                })),
        );
    let mut section = div()
        .debug_selector(|| "disk-section".into())
        .flex()
        .flex_col()
        .gap(space::SM)
        .child(header);
    let Some(space_info) = app.space else {
        return section.child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child("Free space is not available here"),
        );
    };
    // The meter measures this volume, so it projects only the marks whose
    // bytes come back to it; the rest is said, never added.
    let reclaiming = plan.reclaim();
    let after = space_info.after_removing(reclaiming);
    let highlight = palette::highlight(theme);
    let (number, unit) =
        widgets::split_size(&human_bytes(space_info.available));
    let total = space_info.total.max(1) as f32;
    let used_now = (space_info.used() as f32 / total).clamp(0.0, 1.0);
    let used_after = (after.used() as f32 / total).clamp(0.0, used_now);

    section = section.child(
        div()
            .flex()
            .flex_row()
            .items_end()
            .child(widgets::measure(
                number,
                text::FIGURE,
                format!("{unit} free"),
                text::BODY,
                cx,
            ))
            .child(div().flex_1())
            .when(reclaiming > 0, |this| {
                // Lifted like the unit beside the figure, onto its baseline.
                this.child(
                    div()
                        .text_size(text::TITLE)
                        .line_height(text::TITLE)
                        .pb(gpui_kit::Rems(
                            (text::FIGURE.0 - text::TITLE.0) * 0.2,
                        ))
                        .text_color(highlight)
                        .child(format!(
                            "→ ≈ {} free",
                            human_bytes(after.available)
                        )),
                )
            }),
    );
    // Used space from the left; the slice the marks give back is the hatched
    // end of it, so the gap it leaves is exactly what comes back.
    section = section.child(
        div()
            .relative()
            .w_full()
            .h(space::SM)
            .bg(theme.foreground.opacity(0.08))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .h_full()
                    .w(relative(used_after))
                    .bg(theme.foreground.opacity(0.28)),
            )
            .child(
                div()
                    .absolute()
                    .left(relative(used_after))
                    .top_0()
                    .h_full()
                    .w(relative(used_now - used_after))
                    .bg(highlight.opacity(0.25))
                    .child(
                        div()
                            .size_full()
                            .bg(pattern_slash(highlight, 1.0, 3.0)),
                    ),
            ),
    );
    if let Some(left_out) = not_counted(plan) {
        section = section.child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(format!("not counted here: {left_out}")),
        );
    }
    section
        .child(
            div()
                .flex()
                .flex_row()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(format!("{} used", human_bytes(space_info.used())))
                .child(div().flex_1())
                .child(format!("{} total", human_bytes(space_info.total))),
        )
        .children(freed_since_scan(app, theme))
}

/// The marks taken together, pinned above Disk so it never scrolls away:
/// how much is marked, what removing all of it frees, and the way to the
/// review. Marked clones free together what each frees little of alone,
/// so this is the number to decide by, not the selection's.
fn marked_total(
    app: &Disktree,
    plan: &Plan,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Option<Div> {
    if app.marks.is_empty() {
        return None;
    }
    let total = plan.bytes();
    // An SSH host's marks are an export, and its scan says nothing of what
    // removal would free: no estimate is claimed.
    let estimate = app.remote.is_none().then(|| {
        let lower = plan.reclaimable_bytes().min(total);
        (lower, plan.possible_bytes().clamp(lower, total))
    });
    let highlight = palette::highlight(theme);
    let (number, unit) = widgets::split_size(&human_bytes(total));
    let figure = div()
        .flex()
        .flex_row()
        .items_end()
        .gap(space::SM)
        .child(widgets::measure(
            number,
            text::FIGURE,
            format!("{unit} marked"),
            text::BODY,
            cx,
        ))
        .child(div().flex_1())
        .when_some(estimate, |this, (lower, upper)| {
            this.child(
                div()
                    .debug_selector(|| "marked-frees".into())
                    .text_size(text::TITLE)
                    .line_height(text::TITLE)
                    // Lifts the smaller line onto the figure's baseline: a
                    // fifth of the size gap is about the descender space the
                    // figure's line keeps below its digits.
                    .pb(gpui_kit::Rems((text::FIGURE.0 - text::TITLE.0) * 0.2))
                    .text_color(highlight)
                    .whitespace_nowrap()
                    .child(if upper == total && lower == total {
                        format!("frees {}", human_bytes(total))
                    } else {
                        format!("frees ≈ {}", savings(lower, upper))
                    }),
            )
        });
    let mut block = div()
        .debug_selector(|| "marked-total".into())
        .flex()
        .flex_col()
        .gap(space::SM)
        .flex_shrink_0()
        .px(space::MD)
        .py(space::SM)
        .bg(theme.background)
        .border_t_2()
        .border_color(highlight)
        .child(figure);
    if let Some((lower, upper)) = estimate {
        let share =
            |bytes: u64| (bytes as f32 / total.max(1) as f32).clamp(0.0, 1.0);
        let (frees, may) = (share(lower), share(upper) - share(lower));
        let stays = theme.foreground.opacity(0.16);
        // What comes back is hatched, as on the Disk meter; what may is a
        // wash of the same colour; the rest stays, held by clones elsewhere.
        block = block.child(
            div()
                .relative()
                .w_full()
                .h(size::METER)
                .bg(stays)
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(relative(frees))
                        .bg(highlight.opacity(0.25))
                        .child(
                            div()
                                .size_full()
                                .bg(pattern_slash(highlight, 1.0, 3.0)),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .left(relative(frees))
                        .top_0()
                        .h_full()
                        .w(relative(may))
                        .bg(highlight.opacity(0.18)),
                ),
        );
        if upper < total {
            let key = |swatch: Div, label: &'static str| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::XS)
                    .child(swatch)
                    .child(label)
            };
            block = block.child(
                div()
                    .debug_selector(|| "marked-split".into())
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_x(space::MD)
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary)
                    .child(key(
                        widgets::hatch_swatch(
                            highlight,
                            highlight.opacity(0.25),
                        ),
                        "frees",
                    ))
                    .when(upper > lower, |this| {
                        this.child(key(
                            widgets::swatch(highlight.opacity(0.18)),
                            "may free",
                        ))
                    })
                    .child(key(
                        widgets::swatch(stays),
                        "stays: clone data used elsewhere",
                    )),
            );
        }
    }
    Some(block.child(review_button(app, theme, cx)))
}

/// What the volume meter leaves out, and why, when it leaves anything out:
/// bytes marked on another volume come back there, and bytes no volume can
/// be found for are not claimed at all. `None` when every marked byte is
/// this volume's, which is the usual case.
fn not_counted(plan: &Plan) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if plan.foreign() > 0 {
        parts.push(format!("{} on other volumes", human_bytes(plan.foreign())));
    }
    if plan.unattributed() > 0 {
        parts.push(format!(
            "{} could not be placed",
            human_bytes(plan.unattributed())
        ));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The way to the review screen, where the saving is: it says what is
/// marked and what it frees, so the number that matters is the thing to
/// press. Outlined in the highlight, so it does not compete with the filled
/// "Mark for removal" above it.
fn review_button(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> impl IntoElement {
    let highlight = palette::highlight(theme);
    let count = app.marks.len();
    div()
        .id("review")
        .debug_selector(|| "review".into())
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .mt(space::XS)
        .px(space::MD)
        .py(space::SM)
        .border_1()
        .border_color(highlight)
        .bg(highlight.opacity(0.1))
        .hover(move |style| style.bg(highlight.opacity(0.18)))
        .active(move |style| style.bg(highlight.opacity(0.26)))
        .on_click(cx.listener(|this, _, window, cx| {
            this.reread_disks();
            this.screen = Screen::Review;
            cx.notify();
            window.focus(&this.focus, cx);
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(text::BODY)
                .text_color(highlight)
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(format!("Review {count} marked\u{2026}")),
        )
        .child(gpui_omarchy::keycap("c", cx))
}

// ── key bar ─────────────────────────────────────────────────────────────

/// The keys, quietly: outlines and light labels, there when needed. The key
/// to every other key and the scan's own numbers hold the trailing edge.
fn key_bar(app: &Disktree, theme: &Theme, cx: &App) -> Div {
    // Most useful first, so a narrow window clips the least useful.
    let hints: [(&str, &str); 11] = [
        ("space", "mark"),
        ("enter", "open"),
        ("\u{232b}", "up"),
        ("c", "review"),
        ("hjkl", "move"),
        ("/", "filter"),
        ("[ ]", "depth"),
        ("t", "mode"),
        ("0", "reset"),
        ("v", "volumes"),
        ("r", "rescan"),
    ];
    let mut lane = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::LG)
        .flex_1()
        .min_w_0()
        .overflow_hidden();
    for (keys, label) in hints {
        lane = lane.child(widgets::hint(keys, label, cx).flex_shrink_0());
    }

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::LG)
        .px(space::LG)
        .py(space::XS)
        .border_t_1()
        .border_color(theme.divider())
        .child(lane);
    if (app.view.scale - 1.0).abs() > 0.01 {
        row = row.child(
            div()
                .flex_shrink_0()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(format!("{:.1}\u{00d7}", app.view.scale)),
        );
    }
    let scan = if app.scan.is_some() || app.remote_scan.is_some() {
        app.scan_fraction().map_or_else(
            || {
                if app.scan_finalizing() {
                    "Building the treemap…".into()
                } else {
                    "Indexing · incomplete".into()
                }
            },
            |fraction| format!("Indexing · ≈{:.0}% scanned", fraction * 100.0),
        )
    } else if app.progress.cancelled {
        format!(
            "scan cancelled \u{00b7} {} entries \u{00b7} r scans again",
            widgets::human_count(app.progress.files)
        )
    } else {
        let elapsed = app.scan_elapsed.map_or_else(String::new, |time| {
            format!(" \u{00b7} {:.1} s", time.as_secs_f32())
        });
        format!(
            "scan {} entries{elapsed}",
            widgets::human_count(app.progress.files)
        )
    };
    row.child(widgets::hint("?", "all keys", cx).flex_shrink_0())
        .child(
            div()
                .flex_shrink_0()
                .text_size(text::CAPTION)
                .text_color(theme.secondary.opacity(0.7))
                .child(scan),
        )
}

/// What the viewport shows while the first scan is running.
fn scanning_panel(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let error = app.scan_error.as_ref();
    div()
        .debug_selector(move || {
            if error.is_some() {
                "scan-error"
            } else {
                "scan-reading"
            }
            .into()
        })
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .items_start()
        .justify_start()
        .p(space::XL)
        .gap(space::MD)
        .bg(theme.inset)
        .text_size(text::BODY)
        .text_color(theme.secondary)
        .child(if error.is_some() {
            "Scan failed"
        } else if app.progress.cancelled {
            "Indexing stopped"
        } else {
            "Waiting for the first indexed entries…"
        })
        .children(error.map(|error| {
            div()
                .max_w(size::HELP)
                .text_color(theme.danger)
                .child(error.clone())
        }))
        .when(!app.is_indexing(), |panel| {
            panel.child(
                button("scan-again", "Scan again", ButtonVariant::Outline, cx)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_scan(cx);
                        window.focus(&this.focus, cx);
                    })),
            )
        })
}

/// Narrow windows retain the essential counters in a compact footer.
fn indexing_compact(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    div()
        .debug_selector(|| "indexing-compact".into())
        .flex()
        .items_center()
        .gap(space::MD)
        .min_w_0()
        .text_size(text::BODY)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.mono_font.clone())
                .child(format!(
                    "{} files",
                    widgets::human_count(app.progress.files)
                )),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.mono_font.clone())
                .child(app.scan_fraction().map_or_else(
                    || {
                        if app.scan_finalizing() {
                            "Building treemap…".into()
                        } else {
                            format!(
                                "{} measured",
                                human_bytes(app.progress.bytes)
                            )
                        }
                    },
                    |fraction| format!("≈{:.0}% scanned", fraction * 100.0),
                )),
        )
        .child(
            button("cancel-scan", "Stop", ButtonVariant::Outline, cx).on_click(
                cx.listener(|this, _, window, cx| {
                    this.cancel_scan(cx);
                    window.focus(&this.focus, cx);
                }),
            ),
        )
}

/// Progress has one owner. Stable rows and mono values prevent digit,
// unit and path changes from pushing the rest of the screen around.
fn indexing_progress(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let elapsed = app.scan_started.map_or(0, |at| at.elapsed().as_secs());
    let fraction = app.scan_fraction();
    let row = |label: &'static str, value: String, id: &'static str| {
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(size::PROGRESS_ROW)
            .child(div().text_color(theme.secondary).child(label))
            .child(
                div()
                    .debug_selector(move || id.into())
                    .w(size::PROGRESS_VALUE)
                    .text_right()
                    .font_family(theme.mono_font.clone())
                    .child(value),
            )
    };
    let path = if app.remote.is_some() {
        app.remote_progress.directory.as_ref()
    } else {
        app.scan_preview
            .as_ref()
            .and_then(|preview| preview.directory.as_ref())
    };
    let full_path = path.map_or_else(
        || "Directory not reported yet".into(),
        |path| path.display().to_string(),
    );
    // Split by the scanned machine's rules: a Unix host's `a\b` is one name.
    let names = path.map(|path| app.path_style().names(path));
    let names = names.as_deref().unwrap_or_default();
    let short_path = if names.len() > 3 {
        format!("…/{}", names[names.len() - 3..].join("/"))
    } else {
        full_path.clone()
    };
    div()
        .debug_selector(|| "indexing-progress".into())
        .flex()
        .flex_col()
        .min_w_0()
        .gap(space::XS)
        .text_size(text::BODY)
        .text_color(theme.foreground)
        .child(section_head(widgets::eyebrow("Indexing", cx)))
        .child(
            div()
                .debug_selector(|| "indexing-percent".into())
                .h(size::SCAN_STATUS)
                .min_w_0()
                .truncate()
                .font_family(theme.mono_font.clone())
                .text_size(text::FIGURE)
                // The default line height is ~1.6x the type, taller than
                // this box, and `truncate` clips the overflow: descenders
                // were cut off. A line the box's height fits it exactly.
                .line_height(size::SCAN_STATUS)
                .font_weight(FontWeight::SEMIBOLD)
                .child(fraction.map_or_else(
                    || {
                        if app.scan_finalizing() {
                            "Finalizing…".into()
                        } else {
                            "Scanning…".into()
                        }
                    },
                    |fraction| format!("≈{:.0}% scanned", fraction * 100.0),
                )),
        )
        .child(div().h(size::METER).child(widgets::bar(
            fraction.unwrap_or(0.0),
            theme.accent,
            cx,
        )))
        .child(with_tooltip(
            div()
                .id("indexing-estimate")
                .h(size::PROGRESS_ROW)
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(if fraction.is_some() {
                    "Used-space estimate"
                } else if app.scan_finalizing() {
                    "Finishing totals and categories"
                } else {
                    "Total scan size is unknown"
                }),
            "Measured file bytes ÷ drive used space at scan start. \
             Shared blocks, snapshots and unreadable paths affect this \
             estimate; it does not predict time remaining. \
             Folder and filtered scans have no known total.",
        ))
        .child(div().h(size::PROGRESS_ROW).truncate().child(
            if app.remote.is_some() {
                app.remote_progress.stage.label()
            } else if app.scan_finalizing() {
                "Building the treemap"
            } else {
                "Reading this computer"
            },
        ))
        .child(row(
            "Files",
            widgets::human_count(app.progress.files),
            "indexing-files",
        ))
        .child(row(
            "Directories",
            widgets::human_count(app.progress.dirs),
            "indexing-dirs",
        ))
        .child(row(
            "Measured",
            human_bytes(app.progress.bytes),
            "indexing-bytes",
        ))
        .child(row(
            "Read errors",
            widgets::human_count(app.progress.errors),
            "indexing-errors",
        ))
        .child(row("Elapsed", format!("{elapsed} s"), "indexing-elapsed"))
        .child(row(
            "Average rate",
            format!(
                "{} files/s",
                widgets::human_count(app.progress.files / elapsed.max(1))
            ),
            "indexing-rate",
        ))
        .child(
            div()
                .h(size::PROGRESS_ROW)
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child("Recent directory"),
        )
        .child(with_tooltip(
            div()
                .id("indexing-directory")
                .debug_selector(|| "scan-directory".into())
                .h(size::PROGRESS_ROW)
                .min_w_0()
                .truncate()
                .font_family(theme.mono_font.clone())
                .child(short_path),
            full_path,
        ))
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child("Preview only · provisional totals"),
        )
        .child(
            button("cancel-scan", "Stop indexing", ButtonVariant::Outline, cx)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.cancel_scan(cx);
                    window.focus(&this.focus, cx);
                })),
        )
}

fn find_field(app: &Disktree, theme: &Theme) -> Div {
    // What the text matches, said beside it as it is typed, and what Enter
    // and Escape will do with it.
    let (summary, hint) = match app.matches.as_deref() {
        _ if app.finding && app.matches.is_none() => {
            ("searching…".to_string(), "")
        }
        None => (String::new(), "type to filter"),
        Some(matches) if matches.count == 0 => {
            ("no matches".to_string(), "esc clears")
        }
        Some(matches) => (
            format!(
                "{} match{} · {}",
                widgets::human_count(matches.count as u64),
                if matches.count == 1 { "" } else { "es" },
                human_bytes(matches.bytes)
            ),
            if app.filter_applied {
                "esc clears"
            } else {
                "enter shows only these"
            },
        ),
    };
    let highlight = palette::highlight(theme);
    div()
        .child(crate::semantics::text("filter-status", format!("Filter by name: {}. {summary}. {hint}. Slash starts editing; Escape clears.", app.find)))
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .px(space::SM)
        .py(space::XS)
        .debug_selector(|| "find-field".into())
        // Narrows with the window, and then clips its own content rather
        // than drawing past its border.
        .min_w_0()
        .overflow_hidden()
        .border_1()
        .border_color(if app.find_open {
            theme.accent
        } else if app.filter_applied {
            highlight
        } else {
            theme.control_border()
        })
        .bg(theme.normal_fill())
        .text_size(text::BODY)
        .child(
            gpui_omarchy::icon(gpui_omarchy::IconName::Search)
                .flex_shrink_0()
                .size(icon::SM)
                .text_color(if app.filter_applied {
                    highlight
                } else {
                    theme.secondary
                }),
        )
        .child(if app.find.is_empty() {
            div()
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_color(theme.secondary.opacity(0.7))
                .child("Filter by name")
        } else {
            div()
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_color(theme.bright)
                .child(app.find.clone())
        })
        .when(!summary.is_empty(), |this| {
            this.child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(if app.filter_applied {
                        highlight
                    } else {
                        theme.secondary
                    })
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .child(summary),
            )
        })
        // Last in the field, so it is what the field's clip takes when the
        // row is too short even without the legend. No ellipsis: layout
        // rounding can leave the hint half a pixel under its own width, and
        // an ellipsis then cut it although the field had room.
        .child(
            div()
                .debug_selector(|| "find-hint".into())
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_size(text::CAPTION)
                .text_color(theme.secondary.opacity(0.6))
                .child(hint),
        )
}

fn short_name(path: &std::path::Path, style: PathStyle) -> String {
    style.file_name(path).map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The marked ancestor of a path, if any, so the UI can explain nesting.
fn marks_ancestor(
    app: &Disktree,
    path: Option<&std::path::Path>,
) -> Option<String> {
    let path = path?;
    app.marks
        .items()
        .iter()
        .filter(|item| app.path_style().is_below(path, &item.path))
        .map(|item| short_name(&item.path, app.path_style()))
        .next()
}

// ── review ──────────────────────────────────────────────────────────────

fn review(
    app: &Disktree,
    toasts: Option<AnyElement>,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Div {
    let theme = cx.omarchy().clone();
    let compact = window.viewport_size().width.as_f32()
        / window.rem_size().as_f32()
        < REVIEW_COLUMNS_REMS;
    let plan = app.plan();
    let items = app.marks.items();
    let status = ReviewStatus::new(&plan);

    let mut list = div()
        .id("marked-list")
        .role(gpui_kit::Role::List)
        .aria_label(format!(
            "{} marked paths; page {} of {}",
            items.len(),
            app.review_page
                .min(items.len().saturating_sub(1) / LIST_LIMIT)
                + 1,
            items.len().div_ceil(LIST_LIMIT).max(1)
        ))
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .overflow_y_scroll()
        .border_1()
        .border_color(theme.border)
        .bg(theme.inset)
        .when(compact, gpui_kit::Styled::flex_none);

    if items.is_empty() {
        list = list.child(
            div()
                .p(space::XXL)
                .text_color(theme.secondary)
                .child("Nothing is marked. Go back and mark what should go."),
        );
    }

    let page = app
        .review_page
        .min(items.len().saturating_sub(1) / LIST_LIMIT);
    // Each row says what the scan made of it, read from a cache that
    // lives as long as the tree: resolving indexes whole directories.
    let kinds = app.kinds_of(
        items
            .iter()
            .skip(page * LIST_LIMIT)
            .take(LIST_LIMIT)
            .map(|item| item.path.as_path()),
    );
    let budget = crate::run_view::folder_budget(
        window,
        crate::run_view::REVIEW_ROW_FIXED_REMS,
    );
    for ((index, item), (category, reclaim)) in items
        .iter()
        .enumerate()
        .skip(page * LIST_LIMIT)
        .take(LIST_LIMIT)
        .zip(kinds)
    {
        let is_covered = status.covered(&item.path);
        let blocked_reason = status.blocked(&item.path).map(str::to_owned);
        #[cfg(test)]
        {
            use crate::marks::work;
            work::REVIEW_ROWS.set(work::REVIEW_ROWS.get() + 1);
            work::COVERED_ROWS
                .set(work::COVERED_ROWS.get() + usize::from(is_covered));
            work::BLOCKED_ROWS.set(
                work::BLOCKED_ROWS.get()
                    + usize::from(blocked_reason.is_some()),
            );
        }
        list = list.child(mark_row(
            index,
            item,
            MarkRow {
                covered: is_covered,
                blocked: blocked_reason,
                category,
                reclaim,
                folder: crate::run_view::folder_line(app, &item.path, budget),
            },
            app,
            &theme,
            cx,
        ));
    }
    if items.len() > LIST_LIMIT {
        list = list.child(
            div()
                .px(space::MD)
                .py(space::SM)
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(
                    button(
                        "review-previous",
                        "Previous marked paths",
                        ButtonVariant::Outline,
                        cx,
                    )
                    .debug_selector(|| "review-previous".into())
                    .semantic_disabled(page == 0)
                    .on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.review_page = page.saturating_sub(1);
                            window.focus(&this.focus, cx);
                            cx.notify();
                        },
                    )),
                )
                .child(
                    button(
                        "review-next",
                        "Next marked paths",
                        ButtonVariant::Outline,
                        cx,
                    )
                    .debug_selector(|| "review-next".into())
                    .semantic_disabled((page + 1) * LIST_LIMIT >= items.len())
                    .on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.review_page = page + 1;
                            window.focus(&this.focus, cx);
                            cx.notify();
                        },
                    )),
                ),
        );
    }

    let summary = review_summary(app, &plan, &theme, window, cx);
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(screen_header(
            "Review",
            &format!(
                "{} marked \u{00b7} ≈ {} to free",
                app.marks.len(),
                savings(plan.reclaimable_bytes(), plan.possible_bytes())
            ),
            &theme,
            cx,
        ))
        .child(crate::semantics::text("review-readable-summary", format!("Review: {} marked paths; {} included; {} blocked; {} covered by another mark. Mode: {}. Estimated reclaimable: {}. Marking does not authorize removal.",
                items.len(), plan.targets.len(), plan.blocked.len(), plan.covered.len(),
                if app.removal_mode == RemovalMode::Trash { "Move to trash; recoverable until emptied" } else { "Permanent deletion; not recoverable" },
                savings(plan.reclaimable_bytes(), plan.possible_bytes()))))
        .child(
            // The toasts sit over the list's lower left, outside the
            // scroll so they stay put, above the hints and the footer.
            div().relative().flex().flex_col().flex_1().min_h_0()
                .child(div().id("review-scroll")
                    .debug_selector(|| "review-scroll".into())
                    .overflow_y_scroll().track_scroll(&app.review_scroll)
                    .flex()
                    .flex_row().when(compact, gpui_kit::Styled::flex_col)
                    .gap(space::LG)
                    .p(space::LG)
                    .flex_1()
                    .min_h_0()
                    .child(list)
                    .child(summary))
                .children(toasts),
        )
        .when(
            chrome::Platform::current() == chrome::Platform::Linux,
            |screen| screen.child(review_hints(app, &theme, cx)),
        )
        .child(
            screen_footer(review_actions(app, &plan, cx), &theme)
                .debug_selector(|| "review-footer".into()),
        )
}

/// What a review row says beyond the mark itself.
struct MarkRow {
    covered: bool,
    blocked: Option<String>,
    category: Category,
    reclaim: Option<disktree_core::classify::Reclaim>,
    /// The folder it is in, already shortened to fit.
    folder: String,
}

/// One marked path, drawn as Removing draws it so the list approved is
/// the list watched: the icon where Removing puts its checkbox, the kind's
/// colour, the name over its folder, why it is listed, then Review's own
/// lanes. Lanes are fixed, so names, bars and sizes line up down the list
/// and sizes can be compared by eye.
fn mark_row(
    index: usize,
    item: &Target,
    row: MarkRow,
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Stateful<Div> {
    let MarkRow {
        covered,
        blocked,
        category,
        reclaim,
        folder,
    } = row;
    let root_value = app.tree().map_or(0, |tree| tree.bytes);
    let full = app.display_path(&item.path);
    let accent = palette::category_accent(theme, category);
    let kind = crate::run_list::kind(category, reclaim);
    let color = if blocked.is_some() || covered {
        theme.secondary
    } else {
        theme.bright
    };
    let explanation = blocked.as_deref().unwrap_or(if covered {
        "inside a marked directory"
    } else {
        "included in removal plan"
    });
    let row = div()
        .id(crate::file_list::path_id(&item.path))
        .debug_selector(move || format!("review-row-{index}"))
        .role(gpui_kit::Role::ListItem)
        .aria_label(format!(
            "{}; {} bytes; {}marked; {explanation}",
            item.path.display(),
            item.bytes,
            kind.map_or_else(String::new, |kind| format!("{kind}; "))
        ))
        .flex()
        .flex_row()
        .flex_wrap()
        .flex_shrink_0()
        .items_center()
        .gap(space::MD)
        .px(space::MD)
        .py(space::SM)
        .border_b_1()
        .border_color(theme.divider())
        .when(covered, |row| row.opacity(0.6))
        .child(
            // As wide as Removing's checkbox, which takes its place.
            div()
                .flex_shrink_0()
                .size(icon::MD)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    gpui_omarchy::icon(if item.is_dir {
                        gpui_omarchy::IconName::FolderOpen
                    } else {
                        gpui_omarchy::IconName::File
                    })
                    .size(icon::SM)
                    .text_color(theme.secondary),
                ),
        )
        .child(div().flex_shrink_0().w(px(2.)).h(space::XL).bg(accent))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w(relative(0.4))
                .flex_1()
                .child(
                    div()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(color)
                        .child(short_name(&item.path, app.path_style())),
                )
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(folder),
                ),
        )
        .children(kind.map(|kind| {
            widgets::chip(kind, accent, cx)
                .flex_shrink_0()
                .debug_selector(move || format!("review-kind-{index}"))
        }))
        .children(covered.then(|| {
            widgets::chip("Inside a marked directory", theme.secondary, cx)
        }))
        .children(
            blocked.map(|reason| widgets::chip(reason, theme.warning, cx)),
        )
        .child(div().w(size::SHARE_LANE).flex_shrink_0().child(
            widgets::glyph_bar(
                item.bytes,
                root_value.max(1),
                8,
                theme.secondary,
                cx,
            ),
        ))
        .child(
            // Comparable numbers right-align.
            div()
                .w(size::SIZE_LANE)
                .flex_shrink_0()
                .flex()
                .justify_end()
                .text_color(theme.bright)
                .child(human_bytes(item.bytes)),
        )
        .child(
            button(
                ElementId::Name(SharedString::from(format!("unmark-{index}"))),
                "Unmark",
                ButtonVariant::Outline,
                cx,
            )
            .accessibility_label(format!("Unmark {}", item.path.display()))
            .on_click(cx.listener({
                let path = item.path.clone();
                move |this, _, _, cx| this.unmark(&path.clone(), cx)
            })),
        );
    with_tooltip(row, full)
}

fn review_summary(
    app: &Disktree,
    plan: &disktree_core::removal::Plan,
    theme: &Theme,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Stateful<Div> {
    // What comes back to the volume on screen, which is all its meter can
    // project; the totals below say what the removal does in full.
    let reclaiming = plan.reclaim();
    // The part of the saving that lands elsewhere, said rather than added to
    // this volume's projection, so the two numbers cannot be confused.
    let left_out = not_counted(plan)
        .map(|left_out| widgets::row("Not counted here", left_out, cx));
    let trash = app.trash_backend.is_available();

    // One silhouette for one either-or choice, reversible option first.
    let entity = cx.entity().downgrade();
    let mode = button_group(
        "removal-mode",
        vec![
            ChoiceItem::new("trash", "Move to trash").disabled(!trash),
            ChoiceItem::new("permanent", "Delete permanently"),
        ],
        Some(usize::from(app.removal_mode == RemovalMode::Permanent)),
        move |index, _, cx| {
            let _ = entity.update(cx, |this, cx| {
                this.removal_mode = if index == 0 {
                    RemovalMode::Trash
                } else {
                    RemovalMode::Permanent
                };
                cx.notify();
            });
        },
        window,
        cx,
    );

    let explanation = if app.remote.is_some() {
        "Read-only SSH session. Export the selection; nothing is \
         deleted here. Free space is sampled at scan time."
            .to_string()
    } else {
        match app.removal_mode {
            RemovalMode::Trash => format!(
                "Recoverable from the trash until it is emptied. Uses {}.",
                app.trash_backend.label()
            ),
            RemovalMode::Permanent => {
                "Deleted at once, like rm -rf. Nothing is recoverable."
                    .to_string()
            }
        }
    };

    // Never taller than its space above the footer: in a short window, or
    // at a larger interface zoom, the panel scrolls instead of running
    // under the footer.
    let mut panel = div()
        .id("review-summary")
        .debug_selector(|| "review-summary".into())
        .flex()
        .flex_col()
        .gap(space::XL)
        .w(size::REVIEW_SUMMARY)
        .when(
            window.viewport_size().width.as_f32() / window.rem_size().as_f32()
                < REVIEW_COLUMNS_REMS,
            |panel| panel.w_full().max_w_full(),
        )
        .flex_shrink_0()
        .min_h_0()
        .overflow_y_scroll()
        .p(space::LG)
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::SM)
                .child(widgets::section("What happens", cx))
                .children(app.remote.is_none().then_some(mode))
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .child(explanation),
                ),
        )
        .children(
            app.removal_gate(plan)
                .map(|gate| over_limit_gate(app, &gate, theme, window, cx)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::SM)
                .child(widgets::section("Totals", cx))
                .child(widgets::row(
                    "Marked",
                    format!("{}", app.marks.len()),
                    cx,
                ))
                .child(widgets::row(
                    "Acted on",
                    format!("{}", plan.targets.len()),
                    cx,
                ))
                .child(widgets::row(
                    "Nested, go with a parent",
                    format!("{}", plan.covered.len()),
                    cx,
                ))
                .child(widgets::row(
                    "Kept back",
                    format!("{}", plan.blocked.len()),
                    cx,
                ))
                .child(widgets::row(
                    if app.removal_mode == RemovalMode::Trash {
                        "After emptying trash (est.)"
                    } else {
                        "Estimated space freed"
                    },
                    savings(plan.reclaimable_bytes(), plan.possible_bytes()),
                    cx,
                ))
                .children(left_out),
        );

    if cfg!(target_os = "macos") && app.remote.is_none() {
        panel = panel.child(div().text_size(text::CAPTION)
            .text_color(theme.secondary)
            .child("Complete clone groups count shared data once. Remaining hardlinks retain file data. Ranges show unmeasured sharing; snapshots and open files can retain space. Rescan to update estimates."));
    }
    if let Some(volume) = app.space {
        panel = panel.child(
            div()
                .flex()
                .flex_col()
                .gap(space::SM)
                .child(widgets::section("Volume", cx))
                .child(widgets::space_meter(volume, reclaiming, cx)),
        );
    }

    if !plan.blocked.is_empty() {
        let mut blocked = div().flex().flex_col().gap(space::XS);
        for item in plan.blocked.iter().take(6) {
            blocked = blocked.child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(theme.warning)
                    .child(format!(
                        "{}: {}",
                        short_name(&item.path, app.path_style()),
                        item.reason
                    )),
            );
        }
        panel = panel.child(
            div()
                .flex()
                .flex_col()
                .gap(space::XS)
                .child(widgets::section("Kept back", cx))
                .child(blocked),
        );
    }

    // The panel says what will happen; the footer below does it.
    panel
        // Named for the test that scrolls to the panel's end: with no
        // notice, nothing comes after it.
        .child(div().flex_1().debug_selector(|| "review-summary-end".into()))
        .children(scan_error_line(app, theme))
}

/// The list handed on instead of acted on: saved as paths, or copied as a
/// prompt or script for a person or a coding agent to act on with care.
/// One menu, so four ways of handing it on don't outweigh the commit.
fn export_menu(
    app: &Disktree,
    plan: &disktree_core::removal::Plan,
    cx: &Context<'_, Disktree>,
) -> impl IntoElement {
    type Export = fn(&mut Disktree, &mut Context<'_, Disktree>);
    let theme = cx.omarchy().clone();
    let rows: [(&'static str, &'static str, &'static str, Export); 4] = [
        ("save-list", "Save list\u{2026}", "s", |this, cx| {
            this.save_delete_list(cx);
        }),
        ("copy-prompt", "Copy as prompt", "a", |this, cx| {
            this.copy_agent_prompt(cx);
        }),
        (
            "save-script",
            "Save delete script\u{2026}",
            "e",
            |this, cx| {
                this.save_delete_script(cx);
            },
        ),
        ("copy-script", "Copy delete script", "y", |this, cx| {
            this.copy_delete_script(cx);
        }),
    ];
    let menu = rows.into_iter().fold(
        div()
            .id("export-menu")
            .debug_selector(|| "export-menu".into())
            // It opens over the marked list; without this the hit test
            // reaches through it, and the row beneath raised its path
            // tooltip over the menu.
            .occlude()
            .absolute()
            // Above the button: the footer is the window's bottom edge.
            .bottom_full()
            .left_0()
            .mb(space::XS)
            .w(size::EXPORT_MENU)
            .flex()
            .flex_col()
            .py(space::XS)
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.export_menu_open = false;
                cx.notify();
            })),
        |menu, (id, label, key, run)| {
            menu.child(
                div()
                    .id(id)
                    .debug_selector(move || id.into())
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::LG)
                    .px(space::MD)
                    .py(space::XS)
                    .cursor_pointer()
                    .hover(|row| row.bg(theme.hover_fill()))
                    .child(
                        div()
                            .flex_1()
                            .text_color(theme.foreground)
                            .child(label),
                    )
                    .child(
                        div()
                            .font_family(theme.mono_font.clone())
                            .text_size(text::CAPTION)
                            .text_color(theme.secondary)
                            .child(key),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.export_menu_open = false;
                        run(this, cx);
                        window.focus(&this.focus, cx);
                        cx.notify();
                    })),
            )
        },
    );
    div()
        .relative()
        .flex_none()
        .child(
            button("export", "Export \u{25be}", ButtonVariant::Outline, cx)
            .debug_selector(|| "export".into())
                .semantic_disabled(plan.is_empty())
                .selected(app.export_menu_open)
                // Capture runs before the open menu's press-outside handler
                // closes it, so a press on the button toggles.
                .capture_any_mouse_down(cx.listener(|this, _, _, _| {
                    this.export_menu_was_open = this.export_menu_open;
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.export_menu_open = !this.export_menu_was_open;
                    // The keys stay the window's: the button is disabled
                    // once nothing is marked, and a disabled button that
                    // kept focus would swallow every key after it.
                    window.focus(&this.focus, cx);
                    cx.notify();
                })),
        )
        .when(app.export_menu_open, |export| export.child(deferred(menu)))
}

/// Past the limit, Review asks again before the commit button wakes: each
/// disk past it with its share against the limit, then the phrase to type
/// or the box to tick that Settings › Safety picked.
fn over_limit_gate(
    app: &Disktree,
    gate: &Gate,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<'_, Disktree>,
) -> Stateful<Div> {
    let phrase = app.gate_phrase(gate);
    let policy = app.policy();
    let ask: AnyElement = match policy.confirm {
        Confirm::Nothing => div()
            .text_size(text::CAPTION)
            .text_color(theme.foreground)
            .child(format!(
                "Your limit is {}% of a disk per removal. Settings \u{203a} \
                 Safety asks nothing more past it.",
                gate.limit
            ))
            .into_any_element(),
        Confirm::Tick => {
            let key = gate.ticked(app.removal_mode);
            let ticked = app.over_ticked.as_ref() == Some(&key);
            let verb = match app.removal_mode {
                RemovalMode::Trash => "Move",
                RemovalMode::Permanent => "Delete",
            };
            let shares = crate::safety::series(
                gate.percents().iter().map(|percent| format!("{percent}%")),
            );
            let disks = if gate.over.len() > 1 {
                "these disks"
            } else {
                "this disk"
            };
            let entity = cx.entity().downgrade();
            div()
                .flex()
                .flex_col()
                .gap(space::SM)
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.foreground)
                        .child(format!(
                            "Your limit is {}% of a disk per removal.",
                            gate.limit
                        )),
                )
                .child(
                    checkbox(
                        "over-limit-tick",
                        format!(
                            "{verb} {shares} of {disks}{}",
                            if app.removal_mode == RemovalMode::Trash {
                                " to the trash"
                            } else {
                                ""
                            }
                        ),
                        if ticked {
                            CheckboxState::Checked
                        } else {
                            CheckboxState::Unchecked
                        },
                        cx,
                    )
                    .debug_selector(|| "over-limit-tick".into())
                    .tab_stop(true)
                    .on_change(move |_, _, _, cx| {
                        let key = key.clone();
                        let _ = entity.update(cx, |this, cx| {
                            this.over_ticked = (!ticked).then_some(key);
                            cx.notify();
                        });
                    }),
                )
                .into_any_element()
        }
        Confirm::Phrase => {
            let matched = app.gate_met(gate, cx);
            let field = app.over_phrase.as_ref().map(|input| {
                gpui_omarchy::input("over-limit-phrase", input, window, cx)
                    .debug_selector(|| "over-limit-phrase".into())
                    .aria_label(format!("Type {phrase} to confirm"))
            });
            div()
                .flex()
                .flex_col()
                .gap(space::SM)
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.foreground)
                        .child(format!(
                            "Your limit is {}% of a disk per removal. To go \
                             past it, type \u{201c}{phrase}\u{201d}.",
                            gate.limit
                        )),
                )
                .children(field)
                .when(matched, |this| {
                    this.child(
                        div()
                            .debug_selector(|| "over-limit-matched".into())
                            .text_size(text::CAPTION)
                            .text_color(theme.success)
                            .child("\u{2713} Matches"),
                    )
                })
                .into_any_element()
        }
    };
    let heading = match gate.over.as_slice() {
        [one] => format!("This removes {}% of a disk", one.share.percent),
        many => format!(
            "This removes more than {}% of {} disks",
            gate.limit,
            many.len()
        ),
    };
    div()
        .id("over-limit-gate")
        .debug_selector(|| "over-limit-gate".into())
        .role(gpui_kit::Role::Group)
        .aria_label(format!(
            "This removes {}, past the {}% limit",
            gate.summary(),
            gate.limit
        ))
        .flex()
        .flex_col()
        .gap(space::SM)
        .p(space::MD)
        .border_1()
        .border_color(theme.warning)
        .bg(theme.warning.opacity(0.07))
        .child(
            div()
                .text_color(theme.bright)
                .font_weight(FontWeight::SEMIBOLD)
                .child(heading),
        )
        .children(
            gate.over
                .iter()
                .enumerate()
                .map(|(index, over)| over_limit_disk(index, over, theme)),
        )
        .child(ask)
}

/// One disk in the over-limit gate: where it is mounted, its share
/// against its capacity, and a meter with the limit's tick on it, so the
/// gap reads without arithmetic.
fn over_limit_disk(index: usize, over: &Over, theme: &Theme) -> Div {
    let share = &over.share;
    let fraction =
        (share.bytes as f32 / share.capacity.max(1) as f32).clamp(0.0, 1.0);
    let limit_at = f32::from(share.limit) / 100.0;
    let meter = div()
        .relative()
        .h(size::METER)
        .bg(theme.divider())
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(gpui_kit::relative(fraction))
                .bg(theme.warning),
        )
        .child(
            div()
                .absolute()
                .top(-size::METER)
                .bottom(-size::METER)
                .left(gpui_kit::relative(limit_at))
                .w(px(1.))
                .bg(theme.bright),
        );
    div()
        .debug_selector(move || format!("over-limit-disk-{index}"))
        .flex()
        .flex_col()
        .gap(space::XS)
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .gap(space::SM)
                .text_size(text::CAPTION)
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.foreground)
                        .child(over.name.clone()),
                )
                .child(div().flex_none().child(format!(
                    "{} of {} \u{b7} {}%",
                    human_bytes(share.bytes),
                    human_bytes(share.capacity),
                    share.percent
                ))),
        )
        .child(meter)
}

/// The review screen's footer. Exporting and unmarking stay on the left,
/// since neither leaves the screen; Back and the commit end it, on the
/// right. Moving to the trash is the default commit, so it is the primary
/// action; a permanent deletion is the danger action and asks first, which
/// its ellipsis promises. Over SSH nothing is removed here: Back alone.
fn review_actions(
    app: &Disktree,
    plan: &disktree_core::removal::Plan,
    cx: &Context<'_, Disktree>,
) -> Div {
    let unmark = button("unmark-all", "Unmark all", ButtonVariant::Outline, cx)
        .debug_selector(|| "unmark-all".into())
        .semantic_disabled(app.marks.is_empty())
        .on_click(cx.listener(|this, _, window, cx| {
            this.clear_marks(cx);
            window.focus(&this.focus, cx);
        }));
    let back = button("back", "Back", ButtonVariant::Outline, cx)
        .debug_selector(|| "back".into())
        .on_click(cx.listener(|this, _, window, cx| {
            this.screen = Screen::Explore;
            cx.notify();
            window.focus(&this.focus, cx);
        }));
    let unavailable = plan.is_empty()
        || (app.removal_mode == RemovalMode::Trash
            && !app.trash_backend.is_available())
        || app
            .removal_gate(plan)
            .is_some_and(|gate| !app.gate_met(&gate, cx));
    let commit = app.remote.is_none().then(|| {
        let count = plan.targets.len();
        let noun = if count == 1 { "item" } else { "items" };
        let (label, variant) = match app.removal_mode {
            RemovalMode::Trash => (
                format!("Move {count} {noun} to trash"),
                ButtonVariant::Primary,
            ),
            RemovalMode::Permanent => (
                format!("Delete {count} {noun}\u{2026}"),
                ButtonVariant::Danger,
            ),
        };
        button("commit", label, variant, cx)
            .debug_selector(|| "commit".into())
            .semantic_disabled(unavailable)
            .on_click(cx.listener(|this, _, window, cx| {
                this.commit(false, cx);
                this.apply_focus(window, cx);
            }))
            .into_any_element()
    });
    // The same removal, then a full rescan, for exact figures afterwards.
    // Secondary: a rescan of a whole disk takes minutes, and the tree is
    // already right without one.
    let rescan = app.remote.is_none().then(|| {
        let label = match app.removal_mode {
            RemovalMode::Trash => "Move & rescan",
            RemovalMode::Permanent => "Delete & rescan\u{2026}",
        };
        button("commit-rescan", label, ButtonVariant::Outline, cx)
            .debug_selector(|| "commit-rescan".into())
            .semantic_disabled(unavailable)
            .on_click(cx.listener(|this, _, window, cx| {
                this.commit(true, cx);
                this.apply_focus(window, cx);
            }))
            .into_any_element()
    });
    let trailing = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(space::SM)
        .child(back)
        .children(rescan)
        .into_any_element();
    dialog_footer(
        [
            export_menu(app, plan, cx).into_any_element(),
            unmark.into_any_element(),
        ],
        Some(trailing),
        commit,
    )
}

/// Review's keys, on Linux, where Omarchy's apps keep them in view. macOS
/// and Windows have the buttons, their menus and `?` for the same keys.
fn review_hints(app: &Disktree, theme: &Theme, cx: &App) -> Div {
    let commit = match app.removal_mode {
        RemovalMode::Trash => "move to trash",
        RemovalMode::Permanent => "delete\u{2026}",
    };
    let local = app.remote.is_none();
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(space::LG)
        .px(space::LG)
        .py(space::XS)
        .border_t_1()
        .border_color(theme.divider())
        .children(local.then(|| widgets::hint("enter", commit, cx)))
        .children(local.then(|| widgets::hint("shift enter", "& rescan", cx)))
        .children(local.then(|| widgets::hint("m", "trash", cx)))
        .children(local.then(|| widgets::hint("p", "permanent", cx)))
        .child(widgets::hint("!", "unmark all", cx))
        .child(widgets::hint("u", "undo", cx))
        .child(widgets::hint("s e a y", "export", cx))
        .child(widgets::hint("esc", "back", cx))
}

/// A screen's footer band: [`dialog_footer`]'s row across the window's
/// bottom edge.
pub fn screen_footer(row: Div, theme: &Theme) -> Div {
    row.px(space::LG)
        .py(space::SM)
        .border_t_1()
        .border_color(theme.divider())
}

/// `+1.2 GB` or `-300 MB`: a change in free space.
pub fn signed_bytes(delta: i64) -> String {
    let sign = if delta >= 0 { '+' } else { '-' };
    format!("{sign}{}", human_bytes(delta.unsigned_abs()))
}

/// A wall-clock time, for the ledger's rows.
fn clock(at: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(at)
        .format("%H:%M")
        .to_string()
}

/// Every removal since the scan, three ways, and their total: the last
/// row's observed figure runs from the first removal's start to the last
/// one's end, so it is measured rather than added up.
pub fn ledger_table(
    cohorts: &[Cohort],
    theme: &Theme,
    cx: &gpui_kit::App,
) -> Div {
    let figure = |text: String, color: gpui_kit::Hsla| {
        div()
            .w(size::LEDGER_FIGURE)
            .flex_shrink_0()
            .flex()
            .justify_end()
            .text_color(color)
            .child(text)
    };
    let row = |label: Div,
               reported: String,
               private: String,
               observed: Option<i64>,
               color: gpui_kit::Hsla| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::SM)
            .py(space::XXS)
            .border_b_1()
            .border_color(theme.divider())
            .child(label.flex_1().min_w_0())
            .child(figure(reported, color))
            .child(figure(private, color))
            .child(figure(
                observed.map_or_else(|| "\u{2014}".into(), signed_bytes),
                match observed {
                    Some(delta) if delta < 0 => theme.warning,
                    Some(_) => theme.success,
                    None => theme.secondary,
                },
            ))
    };
    let private = |bytes: Option<u64>| {
        bytes.map_or_else(|| "unknown".into(), human_bytes)
    };
    let label = |name: String, detail: String| {
        div()
            .flex()
            .flex_row()
            .gap(space::SM)
            .overflow_hidden()
            .child(div().flex_shrink_0().child(name))
            .child(div().text_color(theme.secondary).truncate().child(detail))
    };
    let heading = |text: &'static str| {
        div()
            .w(size::LEDGER_FIGURE)
            .flex_shrink_0()
            .flex()
            .justify_end()
            .child(text)
    };
    let mut table = div()
        .debug_selector(|| "removal-ledger".into())
        .flex()
        .flex_col()
        .text_size(text::CAPTION)
        .child(
            div()
                .flex()
                .flex_row()
                .gap(space::SM)
                .pb(space::XXS)
                .border_b_1()
                .border_color(theme.divider())
                .text_color(theme.secondary)
                .child(div().flex_1().child("Since the last scan"))
                .child(heading("reported"))
                .child(heading("private"))
                .child(heading("observed")),
        );
    for (index, cohort) in cohorts.iter().enumerate() {
        let noun = if cohort.removed == 1 { "item" } else { "items" };
        let mode = match cohort.mode {
            RemovalMode::Trash => "trash",
            RemovalMode::Permanent => "permanent",
        };
        let latest = index + 1 == cohorts.len();
        table = table.child(row(
            label(
                format!("Removal {}", index + 1),
                format!(
                    "{} \u{b7} {} {noun} \u{b7} {mode}",
                    clock(cohort.finished),
                    cohort.removed
                ),
            ),
            human_bytes(cohort.reported),
            private(cohort.private),
            cohort.observed(),
            if latest {
                theme.bright
            } else {
                theme.foreground
            },
        ));
    }
    if let Some(totals) =
        crate::ledger::Ledger::totals(cohorts).filter(|totals| totals.count > 1)
    {
        table = table.child(
            row(
                label(
                    "Total".into(),
                    format!(
                        "{} \u{2192} {}",
                        clock(totals.started),
                        clock(totals.finished)
                    ),
                ),
                human_bytes(totals.reported),
                private(totals.private),
                totals.observed,
                theme.bright,
            )
            .font_weight(FontWeight::SEMIBOLD),
        );
    }
    let _ = cx;
    table
}

// ── shared ──────────────────────────────────────────────────────────────

pub fn screen_header(
    title: &str,
    subtitle: &str,
    theme: &Theme,
    cx: &gpui_kit::App,
) -> Div {
    let _ = cx;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::MD)
        .px(space::LG)
        .py(space::MD)
        .border_b_1()
        .border_color(theme.divider())
        .child(
            div()
                .text_size(text::TITLE)
                .font_weight(FontWeight::BOLD)
                .text_color(theme.bright)
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(subtitle.to_string()),
        )
}

/// A tooltip that follows the cursor above everything else in the window.
///
/// It is a layer of the root view rather than a child of the treemap, so it is
/// never clipped at the edge of the mosaic, and it tracks the pointer the
/// hover logic already maintains, so it cannot lag behind the tile it
/// describes.
pub fn cursor_tooltip(
    app: &Disktree,
    window: &Window,
    cx: &gpui_kit::App,
) -> Option<Div> {
    // Placement is in pixels because the pointer is; the card's own size and
    // gaps come from the rem scale so they follow interface zoom.
    const HEIGHT_REMS: f32 = 6.5;
    // A clone split adds a caption line: its text and the card's gap.
    const SPLIT_REMS: f32 = text::CAPTION.0 * 1.25 + space::XS.0;
    let rem = window.rem_size().as_f32();
    let width = size::TOOLTIP.0 * rem;
    let split = app
        .hovered
        .as_deref()
        .and_then(|crumbs| app.node_at(crumbs))
        .and_then(clone_split)
        .is_some();
    let height = (HEIGHT_REMS + if split { SPLIT_REMS } else { 0.0 }) * rem;
    let gap = space::MD.0 * rem;
    let edge = space::XS.0 * rem;

    let pointer = app.pointer?;
    let content = hover_tooltip(app, cx)?;
    let origin = app.treemap_origin.get();
    let window_size = window.bounds().size;
    // The tooltip is placed in the root's coordinates, which a client-side
    // frame insets from the window's by its resize ring and border.
    let inset = &app.content_edges;
    let content_width = (window_size.width - inset.left - inset.right).as_f32();
    let content_height =
        (window_size.height - inset.top - inset.bottom).as_f32();

    // Flip to the other side of the pointer rather than overflow the window.
    let anchor_x = (origin.x + pointer.x - inset.left).as_f32();
    let anchor_y = (origin.y + pointer.y - inset.top).as_f32();
    let x = if anchor_x + gap + width > content_width {
        (anchor_x - gap - width).max(edge)
    } else {
        anchor_x + gap
    };
    let y = if anchor_y + gap + height > content_height {
        (anchor_y - gap - height).max(edge)
    } else {
        anchor_y + gap
    };

    Some(
        card_surface(cx)
            .debug_selector(|| "cursor-tooltip".into())
            .absolute()
            .left(px(x))
            .top(px(y))
            .child(content),
    )
}

/// The same surface treatment as Omarchy's tooltip, square and bordered,
/// so an anchored surface of this app does not drift from the system's.
/// Translucent so the mosaic stays visible underneath it.
fn card_surface(cx: &gpui_kit::App) -> Div {
    let theme = cx.omarchy();
    div()
        .w(size::TOOLTIP)
        .flex()
        .flex_col()
        .gap(space::XS)
        .px(space::SM)
        .py(space::SM)
        .border_1()
        .border_color(theme.control_border())
        .bg(theme.background.opacity(0.93))
        .text_color(theme.foreground)
        .font_family(theme.font.clone())
        .text_size(text::CAPTION)
}

/// The card under `<` or `>` while it is hovered: what hovering a tile
/// shows, for the directory the button goes to, so where it leads is known
/// before going. Drawn by the app rather than as a tooltip, which GPUI puts
/// at the pointer, over the button itself.
fn history_card(app: &Disktree, back: bool, cx: &gpui_kit::App) -> Div {
    let (label, keys) = if back {
        ("Back", "alt \u{2190} \u{00b7} side button")
    } else {
        ("Forward", "alt \u{2192} \u{00b7} side button")
    };
    let card = app
        .history_target(back)
        .and_then(|(_, crumbs)| node_card(app, &crumbs, keys, cx));
    card_surface(cx)
        .debug_selector(|| "history-tip".into())
        .map(|surface| match card {
            Some(card) => surface.child(card),
            // Nowhere to go: the button is disabled; say what it is for.
            None => surface.child(format!("{label} \u{00b7} {keys}")),
        })
}

/// The tooltip content for the hovered tile: everything the tile cannot show.
pub fn hover_tooltip(app: &Disktree, cx: &gpui_kit::App) -> Option<Div> {
    let crumbs = app.hovered.as_deref()?;
    // Say what a click does here, naming what it opens: a click on a
    // tile that is not selected only selects it, which looked like a
    // click that failed to open.
    let opens = app
        .click_target(crumbs)
        .and_then(|target| app.node_at(&target))
        .map(|node| node.name().to_string());
    let selected = app.selected.as_deref() == Some(crumbs);
    // What was removed since the scan cannot be marked again.
    let mark = if app.path_at(crumbs).is_some_and(|p| app.was_removed(&p)) {
        "rescan to update"
    } else {
        "space mark"
    };
    let keys = match opens {
        Some(name) if selected => format!("click opens {name} · {mark}"),
        Some(name) => {
            format!("click select · double-click opens {name} · {mark}")
        }
        None => format!("click select · {mark}"),
    };
    node_card(app, crumbs, &keys, cx)
}

/// What is known about the node at `crumbs`, as a card: name, path, size and
/// share, counts, and badges, with `keys` for what can be done from there.
fn node_card(
    app: &Disktree,
    crumbs: &[usize],
    keys: &str,
    cx: &gpui_kit::App,
) -> Option<Div> {
    let theme = cx.omarchy();
    let node = app.node_at(crumbs)?;
    let path = app.path_at(crumbs);
    let parent = crumbs[..crumbs.len().saturating_sub(1)].to_vec();
    let parent_value = app.node_at(&parent).map_or(0, |node| node.bytes);
    let marked = path
        .as_deref()
        .is_some_and(|path| app.marks.contains(path, app.path_style()));
    let hidden = node.name().starts_with('.');
    let covered = marks_ancestor(app, path.as_deref());
    let removed = path.as_deref().is_some_and(|path| app.was_removed(path));

    let mut tip =
        div()
            .flex()
            .flex_col()
            .gap(space::XS)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::XS)
                    .child(
                        gpui_omarchy::icon(if node.is_dir() {
                            gpui_omarchy::IconName::FolderOpen
                        } else {
                            gpui_omarchy::IconName::File
                        })
                        .size(icon::SM)
                        .text_color(theme.secondary),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.bright)
                            .child(node.name().to_string()),
                    ),
            )
            .child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary)
                    .child(path.as_deref().map_or_else(String::new, |path| {
                        app.display_path(path)
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::SM)
                    .child(
                        div()
                            .text_size(text::TITLE)
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.bright)
                            .child(human_bytes(node.bytes)),
                    )
                    .child(widgets::glyph_bar(
                        node.bytes,
                        parent_value.max(1),
                        10,
                        theme.secondary,
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(text::CAPTION)
                            .text_color(theme.secondary)
                            .child(widgets::percent(node.bytes, parent_value)),
                    ),
            )
            .child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary)
                    .child(format!(
                        "{} files · {} dirs · {} direct",
                        widgets::human_count(node.files),
                        widgets::human_count(
                            u64::from(node.dirs)
                                .saturating_sub(u64::from(node.is_dir()))
                        ),
                        human_bytes(node.own_bytes())
                    )),
            )
            .when_some(clone_split(node), |this, split| {
                this.child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .child(split),
                )
            });

    let mut badges = div().flex().flex_row().gap(space::XS).flex_wrap();
    if hidden {
        badges = badges.child(widgets::chip("Hidden", theme.secondary, cx));
    }
    if removed {
        badges = badges.child(widgets::chip(
            "Removed since the scan",
            theme.secondary,
            cx,
        ));
    }
    if marked {
        badges =
            badges.child(widgets::chip("Marked for removal", theme.danger, cx));
    }
    if let Some(ancestor) = covered {
        badges = badges.child(widgets::chip(
            format!("Inside marked {ancestor}"),
            theme.secondary,
            cx,
        ));
    }
    tip = tip.child(badges);
    tip = tip.child(
        div()
            .text_size(text::CAPTION)
            .text_color(theme.secondary.opacity(0.7))
            .child(keys.to_string()),
    );
    Some(tip)
}

/// The modifier the help names for clicks and interface zoom. Both are
/// accepted everywhere; this is the one each platform's users reach for, and
/// on macOS ctrl-click is a right-click.
const MODIFIER_CLICK: &str = if cfg!(target_os = "macos") {
    "\u{2318}-click"
} else {
    "ctrl-click"
};
const MODIFIER_ZOOM: &str = if cfg!(target_os = "macos") {
    "\u{2318} = / - / 0"
} else {
    "ctrl = / - / 0"
};
const MODIFIER_OPEN: &str = if cfg!(target_os = "macos") {
    "\u{2318}O"
} else {
    "ctrl-o"
};

const MODIFIER_VIEW: &str = if cfg!(target_os = "macos") {
    "\u{2318}1 / \u{2318}2"
} else {
    "ctrl-1 / ctrl-2"
};

fn help_overlay(app: &Disktree, cx: &Context<'_, Disktree>) -> Stateful<Div> {
    let theme = cx.omarchy();
    // Sentence case, and the tile a key acts on is always the one under the
    // pointer if the pointer moved last, else the keyboard selection.
    let rows = [
        ("space / x", "Mark or unmark the tile you point at"),
        (MODIFIER_CLICK, "Mark without moving the selection"),
        (
            "double-click",
            "Open one level toward a tile; so does a click on the selection",
        ),
        ("enter", "Open that directory, at any depth"),
        ("\u{232b} / esc", "Go up one directory"),
        (
            "alt \u{2190} / \u{2192}",
            "Back or forward, as do the mouse's side buttons",
        ),
        (
            "\u{2190} \u{2191} \u{2193} \u{2192}",
            "Move between tiles at this level",
        ),
        (
            "tab / shift-tab",
            "Next or previous control; Escape returns to files",
        ),
        ("n / shift-n", "Next or previous largest sibling"),
        ("scroll", "Zoom toward a directory, then go into it"),
        ("shift-scroll", "Pan the magnified view"),
        ("[ / ]", "Draw fewer or more levels at once"),
        ("- / = / 0", "Magnify, shrink, or reset the view"),
        (MODIFIER_ZOOM, "Interface zoom"),
        (
            "/",
            "Filter by name: only matches keep their colour; enter shows only them",
        ),
        (
            "click legend",
            "Show only a kind or age, then hide it; esc clears",
        ),
        ("c", "Review the marked list"),
        ("t", "Size, files or age: what areas and colours say"),
        (
            MODIFIER_VIEW,
            "Treemap or file list; the list is read by screen readers",
        ),
        ("r", "Scan again from the same root"),
        ("esc", "Stop a scan in progress"),
        ("v", "Scan another volume"),
        (MODIFIER_OPEN, "Choose another directory to scan"),
        ("g", "The whole disk; click any directory above to widen"),
        ("d", "Disk usage or apparent size"),
        ("i", "Include or skip hidden entries"),
        ("p", "Show or hide the selection line"),
        (
            "o",
            if cfg!(target_os = "macos") {
                "Show it in Finder"
            } else if cfg!(windows) {
                "Show it in File Explorer"
            } else {
                "Show it in the file manager"
            },
        ),
        (
            "shift-u",
            "Updates: the version, the release channel, check now",
        ),
        // A bare q only on Linux, where Omarchy's apps quit that way.
        (
            if cfg!(target_os = "macos") {
                "\u{2318}Q"
            } else if cfg!(windows) {
                "alt f4"
            } else {
                "q"
            },
            "Quit",
        ),
        ("", ""),
        (
            "Review screen",
            "m trash \u{00b7} p permanent \u{00b7} ! unmark all \u{00b7} u undo \u{00b7} s save list \u{00b7} a copy as prompt",
        ),
        ("", "enter commits \u{00b7} esc goes back"),
        ("", "A permanent deletion always asks first"),
    ];

    // The list scrolls inside the card when the window is too short for
    // it; the title and Done stay in view.
    let mut keys = div()
        .id("help-scroll")
        .debug_selector(|| "help-scroll".into())
        .role(gpui_kit::Role::Document)
        .aria_label("Shortcut list; arrows scroll, PageUp/PageDown scroll a page, Home/End reaches either end")
        .flex()
        .flex_col()
        .gap(space::SM)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .track_scroll(&app.help_scroll);
    for (index, (key, label)) in rows.into_iter().enumerate() {
        if key.is_empty() && label.is_empty() {
            continue;
        }
        keys = keys.child(
            crate::semantics::text(
                ("help-shortcut", index),
                format!("{key}: {label}"),
            )
            .flex()
            .flex_row()
            .flex_shrink_0()
            .gap(space::MD)
            .items_start()
            .child(
                div()
                    .w(size::KEY_LANE)
                    .max_w(relative(0.35))
                    .flex_shrink_0()
                    .text_size(text::CAPTION)
                    .text_color(theme.accent)
                    .child(key.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(text::BODY)
                    .text_color(theme.foreground)
                    .child(label.to_string()),
            ),
        );
    }

    let close: CloseOverlay = |this| this.show_help = false;
    let context = format!(
        "{} · {}",
        app.root_path.display(),
        app.options.metric.label()
    );
    let note = div()
        .flex()
        .flex_col()
        .min_w_0()
        .child("↑↓ / PageUp / PageDown scroll · ? / Esc closes")
        .child(
            crate::semantics::text("help-context", context.clone())
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(context),
        );
    overlay_backdrop(
        "help-backdrop",
        div()
            .debug_selector(|| "help-overlay".into())
            .flex()
            .flex_col()
            .gap(space::MD)
            .w(size::HELP)
            .max_w_full()
            .max_h_full()
            .min_h_0()
            .p(space::XL)
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::SM)
                    .child(
                        gpui_omarchy::icon(gpui_omarchy::IconName::Keyboard)
                            .size(icon::MD)
                            .text_color(theme.accent),
                    )
                    .child(
                        crate::semantics::text(
                            "help-heading",
                            "Keyboard and mouse",
                        )
                        .role(gpui_kit::Role::Heading)
                        .text_size(text::TITLE)
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.bright)
                        .child("Keyboard and mouse"),
                    ),
            )
            .child(keys)
            .child(overlay_footer(note, close, cx).flex_shrink_0()),
        close,
        cx,
    )
    .role(gpui_kit::Role::Dialog)
    .aria_label("Keyboard and mouse shortcuts")
    .aria_description("Escape closes and returns to the originating control")
    .track_focus(&app.confirm_focus)
    .tab_group()
    .tab_stop(false)
    .a11y_synthetic_children(|b| b.parent_node().set_modal())
}

/// A node's listed bytes split into private and APFS-clone-shared, as the
/// MCP rows read; `None` without clone blocks, so other systems and other
/// scans show nothing new. Tile weights stay the allocated bytes.
pub fn clone_split(node: disktree_core::tree::Node<'_>) -> Option<String> {
    disktree_link::Clones::of(node)?.split()
}

/// Both ends are scan-derived; the meter uses the lower bound only.
fn savings(lower: u64, upper: u64) -> String {
    disktree_core::size::human_bytes_range(lower, upper)
}

// ── the menu, off macOS ─────────────────────────────────────────────────

/// Whether the strip carries the menu button: off macOS, where there is no
/// global menu bar. Tests pin a frame to draw another system's strip.
fn frame_has_app_menu(pin: Option<&Frame>) -> bool {
    pin.map_or(crate::chrome::Platform::current(), |frame| frame.platform)
        != crate::chrome::Platform::MacOs
}

/// The strip's menu button: the menu bar's place off macOS.
fn app_menu_button(
    app: &Disktree,
    cx: &Context<'_, Disktree>,
) -> impl IntoElement {
    let theme = cx.omarchy().clone();
    with_tooltip(
        div()
            .id("app-menu-button")
            .role(gpui_kit::Role::Button)
            .aria_label("Application menu")
            .aria_expanded(app.app_menu_open)
            .focusable()
            .tab_stop(true)
            .focus_visible(|style| style.border_color(theme.accent))
            .debug_selector(|| "app-menu-button".into())
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .size(size::MENU_BUTTON)
            .mr(space::SM)
            .border_1()
            .border_color(if app.app_menu_open {
                theme.border
            } else {
                theme.background.opacity(0.)
            })
            .when(app.app_menu_open, |button| {
                button.bg(theme.accent.opacity(0.18))
            })
            .hover(|button| button.bg(theme.hover_fill()))
            .cursor_pointer()
            .text_color(theme.bright)
            .child(
                gpui_omarchy::icon(gpui_omarchy::IconName::Menu).size(icon::MD),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_app_menu(cx);
                this.apply_focus(window, cx);
            }))
            .on_key_down(cx.listener(
                |this, event: &KeyDownEvent, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        this.toggle_app_menu(cx);
                        this.apply_focus(window, cx);
                        cx.stop_propagation();
                    }
                },
            )),
        "Menu \u{b7} Alt",
    )
}

/// The menu bar's template (`app_menu::menus`) as one dropdown, its menus
/// as headed groups rather than cascades: one level, nothing to aim at
/// diagonally. A row runs its action as the menu bar's would.
fn app_menu_dropdown(
    app: &Disktree,
    cx: &Context<'_, Disktree>,
) -> impl IntoElement {
    use gpui_kit::MenuItem;
    let theme = cx.omarchy().clone();
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let mut list = div()
        .id("app-menu")
        .track_scroll(&app.menu_scroll)
        .role(gpui_kit::Role::Menu)
        .aria_label("Application menu")
        .track_focus(&app.confirm_focus)
        .tab_group()
        .tab_stop(false)
        .debug_selector(|| "app-menu".into())
        // It drops over the toolbar and the mosaic; without this a press
        // on a heading selected the tile beneath and a scroll zoomed it.
        // The list's own scroll still works: it is the hit element.
        .occlude()
        .flex()
        .flex_col()
        .w(size::APP_MENU)
        .max_h(size::APP_MENU_HEIGHT)
        .overflow_y_scroll()
        .py(space::XS)
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .shadow_lg()
        .on_mouse_down_out(cx.listener(|this, _, window, cx| {
            this.toggle_app_menu(cx);
            this.apply_focus(window, cx);
        }));
    let menus = crate::app_menu::menus(crate::app_menu::Platform::Other);
    let count = menus.len();
    let mut item_index = 0;
    for (group, menu) in menus.into_iter().enumerate() {
        list = list.child(
            div()
                .px(space::MD)
                .pt(space::SM)
                .pb(space::XXS)
                .font_family(theme.mono_font.clone())
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(menu.name.to_uppercase()),
        );
        for (index, item) in menu.items.into_iter().enumerate() {
            let MenuItem::Action { name, action, .. } = item else {
                continue;
            };
            let shortcut = keymap
                // The last binding wins, as the menu bar shows it; Edit's and
                // zoom's are in the menu-only context, and count.
                .bindings_for_action(action.as_ref())
                .next_back()
                .map(|binding| {
                    binding
                        .keystrokes()
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" ")
                });
            let id = format!("app-menu-{group}-{index}");
            let selected = item_index == app.app_menu_index;
            item_index += 1;
            list = list.child(
                div()
                    .id(ElementId::Name(id.clone().into()))
                    .role(gpui_kit::Role::MenuItem).aria_label(name.clone())
                    .when_some(shortcut.clone(), gpui_kit::StatefulInteractiveElement::aria_keyshortcuts)
                    .when(selected, gpui_kit::StatefulInteractiveElement::aria_active_descendant)
                    .debug_selector(move || id)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::LG)
                    .px(space::MD)
                    .py(space::XXS)
                    .hover(|row| row.bg(theme.accent.opacity(0.18)))
                    .when(selected, |row| row.bg(theme.hover_fill()))
                    .cursor_pointer()
                    .child(
                        div().flex_1().text_color(theme.foreground).child(name),
                    )
                    .children(shortcut.map(|keys| {
                        div()
                            .font_family(theme.mono_font.clone())
                            .text_size(text::CAPTION)
                            .text_color(theme.secondary)
                            .child(keys)
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.app_menu_open = false;
                        this.focus_request = Some(crate::state::FocusTarget::Root);
            this.apply_focus(window, cx);
                        window.dispatch_action(action.boxed_clone(), cx);
                        cx.notify();
                    })),
            );
        }
        if group + 1 < count {
            list =
                list.child(div().my(space::XS).h(px(1.)).bg(theme.divider()));
        }
    }
    deferred(
        anchored()
            .position(gpui_kit::point(
                crate::ui::chrome::STOPLIGHT_ZONE,
                crate::ui::chrome::STRIP,
            ))
            .child(list),
    )
}

/// Who is driving the window, in the top bar: the agent's name and what it
/// last did. Only while an agent is connected; a click opens the overlay.
/// The strip's id for the update chip, for tests.
pub const UPDATE_CHIP_ID: &str = "strip-update";

/// "0.15.0 ready · Restart": the release Restart to update installs, offered
/// quietly on every screen, Later or not (design: Update Notices 3a). A
/// press is Restart to update, which waits for a running removal. At the
/// tightest bar it keeps only the verb.
fn update_chip(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Option<Stateful<Div>> {
    let held = app.update_watch.ready()?;
    let restarting = app.update_watch.seen().restarting;
    let verb = match (restarting, held.switch_back) {
        (true, _) => "Restarting\u{2026}",
        (false, true) => "Switch",
        (false, false) => "Restart",
    };
    let action = if held.switch_back { "switch" } else { "update" };
    let label = format!("Restart to {action} to {}", held.version);
    let tight = app.bar_fit == crate::toolbar::BarFit::Tight;
    let lead = if tight {
        format!("{} \u{00b7}", held.version)
    } else {
        format!("{} ready \u{00b7}", held.version)
    };
    let hover = theme.accent.opacity(0.1);
    Some(with_tooltip(
        div()
            .id(UPDATE_CHIP_ID)
            .debug_selector(|| UPDATE_CHIP_ID.into())
            .role(gpui_kit::Role::Button)
            .aria_label(label.clone())
            .focusable()
            .tab_stop(true)
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            .gap(space::XS)
            .px(space::SM)
            .py(space::XXS)
            .mr(space::SM)
            .border_1()
            .border_color(theme.accent)
            .font_family(theme.mono_font.clone())
            .text_size(text::CAPTION)
            .whitespace_nowrap()
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .focus_visible(|style| style.border_color(theme.focus_border()))
            .on_key_down(cx.listener(
                move |this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        if !this.update_watch.seen().restarting {
                            this.restart_to_update(cx);
                        }
                        cx.stop_propagation();
                    }
                },
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.update_watch.seen().restarting {
                    this.restart_to_update(cx);
                }
            }))
            .child(
                div()
                    .size(size::SWATCH)
                    .rounded_full()
                    .flex_shrink_0()
                    .bg(theme.accent),
            )
            .child(div().text_color(theme.secondary).child(lead))
            .child(div().text_color(theme.accent).underline().child(verb)),
        label,
    ))
}

fn agent_pill(app: &Disktree, theme: &Theme) -> Option<Stateful<Div>> {
    let presence = app.agent.as_ref()?.presence.as_ref()?;
    Some(
        div()
            .id("agent-pill")
            .debug_selector(|| "agent-pill".into())
            .flex()
            .flex_row()
            .items_center()
            .gap(space::XS)
            .px(space::SM)
            .py(space::XXS)
            .border_1()
            .border_color(theme.success.opacity(0.5))
            .text_size(text::CAPTION)
            .cursor_pointer()
            .hover(|style| style.bg(theme.success.opacity(0.1)))
            .on_click(|_, _, cx| open_agents_settings(cx))
            .child(div().text_color(theme.success).child("\u{25cf}"))
            .child(
                div()
                    .text_color(theme.foreground)
                    .whitespace_nowrap()
                    .child(presence.client.clone()),
            )
            .child(
                div()
                    .text_color(theme.secondary)
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .max_w(size::TOOLTIP)
                    .child(format!("\u{00b7} {}", presence.action)),
            ),
    )
}

/// The way to an agent, under the disk: who is connected, or how to
/// connect one. Quiet, since most sessions never need it.
fn agent_line(app: &Disktree, theme: &Theme) -> Stateful<Div> {
    let presence = app.agent.as_ref().and_then(|agent| agent.presence.clone());
    let mut line = div()
        .id("agent-line")
        .debug_selector(|| "agent-line".into())
        .flex()
        .flex_row()
        .items_center()
        .gap(space::XS)
        .text_size(text::CAPTION)
        .cursor_pointer()
        .on_click(|_, _, cx| open_agents_settings(cx));
    match presence {
        Some(presence) => {
            line = line
                .child(div().text_color(theme.success).child("\u{25cf}"))
                .child(
                    div()
                        .text_color(theme.foreground)
                        .child(format!("{} is connected", presence.client)),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_color(theme.secondary)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(presence.action),
                );
        }
        None => {
            line = line
                .text_color(theme.secondary)
                .hover(|style| style.text_color(theme.foreground))
                .child("Connect an AI agent (MCP) \u{203a}");
        }
    }
    line
}

/// Settings › Agents (MCP): how to connect an agent, who is connected,
/// and what it may do. Deferred, since Settings may open a window.
fn open_agents_settings(cx: &mut App) {
    cx.defer(|cx| crate::settings::open(crate::settings::Pane::Agents, cx));
}

fn app_menu_key(
    app: &mut Disktree,
    event: &KeyDownEvent,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> bool {
    let actions: Vec<_> =
        crate::app_menu::menus(crate::app_menu::Platform::Other)
            .into_iter()
            .flat_map(|menu| menu.items)
            .filter_map(|item| {
                if let gpui_kit::MenuItem::Action { action, .. } = item {
                    Some(action)
                } else {
                    None
                }
            })
            .collect();
    let count = actions.len();
    if count == 0 {
        return false;
    }
    match event.keystroke.key.as_str() {
        "down" | "j" => app.app_menu_index = (app.app_menu_index + 1) % count,
        "up" | "k" => {
            app.app_menu_index = (app.app_menu_index + count - 1) % count;
        }
        "home" => app.app_menu_index = 0,
        "end" => app.app_menu_index = count - 1,
        "enter" | "space" => {
            let action =
                actions[app.app_menu_index.min(count - 1)].boxed_clone();
            app.app_menu_open = false;
            app.focus_request = Some(crate::state::FocusTarget::Root);
            app.apply_focus(window, cx);
            window.dispatch_action(action, cx);
        }
        _ => return false,
    }
    let mut remaining = app.app_menu_index;
    let mut row = app.app_menu_index + 1;
    for menu in crate::app_menu::menus(crate::app_menu::Platform::Other) {
        let count = menu
            .items
            .iter()
            .filter(|item| matches!(item, gpui_kit::MenuItem::Action { .. }))
            .count();
        if remaining < count {
            break;
        }
        remaining -= count;
        // Each preceding group contributes a heading and a separator.
        row += 2;
    }
    app.menu_scroll.scroll_to_item(row);
    cx.stop_propagation();
    cx.notify();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use disktree_core::removal::Blocked;
    use std::path::PathBuf;

    #[test]
    fn review_status_preserves_exact_paths_and_first_blocked_reason() {
        let path = PathBuf::from("/synthetic/a/child");
        let mut plan = Plan::default();
        plan.covered.push(Target {
            path: path.clone(),
            bytes: 1,
            is_dir: false,
            hidden: false,
        });
        plan.blocked.push(Blocked {
            path: path.clone(),
            reason: "first".into(),
        });
        plan.blocked.push(Blocked {
            path: path.clone(),
            reason: "second".into(),
        });
        let status = ReviewStatus::new(&plan);
        assert!(status.covered(&path));
        assert!(!status.covered(Path::new("/synthetic/a/childish")));
        assert!(!status.covered(Path::new("/synthetic/A/child")));
        assert_eq!(status.blocked(&path), Some("first"));
        assert_eq!(
            status.blocked(Path::new("/synthetic/a/./child")),
            Some("first")
        );
        assert_eq!(status.blocked(Path::new("/synthetic/A/child")), None);
        plan.blocked.clear();
        plan.covered.clear();
        let refreshed = ReviewStatus::new(&plan);
        assert_eq!(refreshed.blocked(&path), None);
        assert!(!refreshed.covered(&path));
    }

    #[cfg(unix)]
    #[test]
    fn review_status_keeps_non_utf8_paths_distinct() {
        use std::os::unix::ffi::OsStrExt as _;
        let path = Path::new("/synthetic")
            .join(std::ffi::OsStr::from_bytes(b"not-\xff"));
        let mut plan = Plan::default();
        plan.blocked.push(Blocked {
            path: path.clone(),
            reason: "blocked".into(),
        });
        let status = ReviewStatus::new(&plan);
        assert_eq!(status.blocked(&path), Some("blocked"));
        assert_eq!(
            status.blocked(Path::new(&path.to_string_lossy().into_owned())),
            None
        );
    }
}
