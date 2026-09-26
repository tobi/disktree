//! The screens: explore, review, running and done.
//!
//! Each function is a pure reading of [`Disktree`], so what a screen shows is
//! always exactly what the state says — there is no second copy of anything to
//! keep in sync.

use disktree_core::classify::Category;
use disktree_core::insights::{Candidate, Finding, STALE_DAYS};
use disktree_core::removal::{RemovalMode, Target};
use disktree_core::size::human_bytes;
use disktree_core::tree::Metric;
use gpui_kit::base::CheckboxState;
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Div, DragMoveEvent, ElementId,
    FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement, Rems, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, Window, anchored, deferred, div,
    pattern_slash, px, relative,
};
use gpui_omarchy::{
    ActiveTheme, ButtonVariant, ChoiceItem, Theme, alert_dialog, button,
    button_group, checkbox, dialog_button, dialog_description, dialog_popup,
    dialog_title, separator, with_tooltip,
};

use gpui_kit::prelude::FluentBuilder as _;

use crate::palette;
use crate::state::{
    ColorMode, Crumb, Disktree, PANEL_REMS, Screen, panel_width,
};
use crate::treemap_view::{self, Mosaic};
use crate::ui::{icon, size, space, text};
use crate::widgets;

/// How many marks the review screen lists. Everything above the cap is still
/// removed; the list only stops being exhaustive, which it says out loud.
const LIST_LIMIT: usize = 1200;

/// The whole window.
pub fn root(
    app: &mut Disktree,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Stateful<Div> {
    let theme = cx.omarchy().clone();
    let body = match app.screen {
        Screen::Explore => explore(app, window, cx),
        Screen::Review => review(app, window, cx),
        Screen::Running => running(app, cx),
        Screen::Done => done(app, cx),
    };

    let mut root = div()
        .id("disktree-root")
        .debug_selector(|| "disktree-root".into())
        .track_focus(&app.focus)
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
                    Disktree::open_folder(cx);
                }
            },
        ))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::ShowInFinder, _, cx| {
                this.reveal_target(cx);
            },
        ))
        // Only where the history buttons are: the review screen has none.
        .on_action(cx.listener(|this, _: &crate::app_menu::GoBack, _, cx| {
            if this.screen == Screen::Explore {
                this.go_back(cx);
            }
        }))
        .on_action(cx.listener(
            |this, _: &crate::app_menu::GoForward, _, cx| {
                if this.screen == Screen::Explore {
                    this.go_forward(cx);
                }
            },
        ))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            if this.zoom_interface(event, window) {
                cx.notify();
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
        .child(body);
    if let Some(tip) = cursor_tooltip(app, window, cx) {
        root = root.child(tip);
    }
    if app.show_help {
        root = root.child(help_overlay(app, cx));
    }
    if app.confirm_open {
        root = root.child(delete_dialog(app, cx));
    }
    root
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
            short_name(&only.path)
        ),
        targets => format!("Delete {} items permanently?", targets.len()),
    };
    let body = format!(
        "This frees {}. Deleted files can\u{2019}t be recovered; move them to the trash if you might need them again.",
        human_bytes(plan.bytes())
    );
    let confirm = cx.entity().downgrade();
    let cancel = confirm.clone();
    let actions = div()
        .flex()
        .flex_row()
        .justify_end()
        .gap(space::SM)
        .child(
            dialog_button(
                "delete-cancel",
                "Cancel",
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.cancel_delete(cx);
                this.apply_focus(window, cx);
            })),
        )
        .child(
            dialog_button(
                "delete-confirm",
                "Delete",
                ButtonVariant::Danger,
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.confirm_delete(cx);
                this.apply_focus(window, cx);
            })),
        );
    let popup = dialog_popup(cx)
        .child(dialog_title(title, cx))
        .child(dialog_description(body, cx))
        .child(actions);
    alert_dialog(&app.confirm_focus, cx)
        .open(true)
        .on_ok(move |_, window, cx| {
            let _ = confirm.update(cx, |this, cx| {
                this.confirm_delete(cx);
                this.apply_focus(window, cx);
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
        .popup(popup)
}

// ── explore ─────────────────────────────────────────────────────────────

fn explore(
    app: &mut Disktree,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Div {
    let theme = cx.omarchy().clone();
    let mosaic: Mosaic = app.prepare();
    let width_rems = window.viewport_size().width.as_f32() / app.rem;
    // The panel is where the selection, the marks and the disk live; it
    // only gives way when the mosaic would be too narrow to read.
    let panel = app.show_selection && width_rems >= PANEL_SHOWN_REMS;
    if panel && let Some(path) = selection_checkout(app) {
        app.ensure_git(&path, cx);
    }

    let viewport = if app.tree().is_none() {
        // The first walk of a home directory takes long enough that an
        // empty viewport would look broken; count the work instead.
        scanning_panel(app, &theme, cx).into_any_element()
    } else {
        treemap_view::mosaic(mosaic, app, window, cx).into_any_element()
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(top_bar(app, &theme, window, cx))
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
                        this.panel_rems = panel_width(
                            event.event.position.x.as_f32(),
                            window.viewport_size().width.as_f32(),
                            this.rem,
                        );
                        cx.notify();
                    },
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .child(trail_and_legend(app, &theme, cx))
                        .child(
                            div()
                                .flex()
                                .flex_1()
                                .min_h_0()
                                .px(space::LG)
                                .pb(space::SM)
                                .child(viewport),
                        ),
                )
                .children(panel.then(|| side_panel(app, &theme, cx))),
        )
        .child(key_bar(app, &theme, cx))
}

/// The selection, when it is a checkout git can say something about.
fn selection_checkout(app: &Disktree) -> Option<std::path::PathBuf> {
    let target = app.action_target()?;
    let node = app.node_at(&target)?;
    if !node.is_dir() {
        return None;
    }
    let path = app.path_at(&target)?;
    crate::git::is_checkout(&path).then_some(path)
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

// ── top bar ─────────────────────────────────────────────────────────────

/// The app, what the whole scan found, and the controls that decide what
/// is measured, on the edge they own.
fn top_bar(
    app: &Disktree,
    theme: &Theme,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::LG)
        .px(space::LG)
        .py(space::MD)
        .border_b_1()
        .border_color(theme.divider())
        .child(logo(theme))
        // Where you are is navigation, and it belongs to the whole window.
        .child(trail(app, theme, cx))
        .child(div().flex_1())
        .child(view_settings(app, window, cx))
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
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::XXS)
        .min_w_0()
        .overflow_hidden();
    for (index, (label, step)) in steps.into_iter().enumerate() {
        if hidden.contains(&index) {
            if index == hidden.start {
                row = row.child(separator_glyph(theme)).child(
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
            row = row.child(separator_glyph(theme));
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

/// The platform's path separator, so the trail reads like the paths shown
/// elsewhere: `/` here, `\` on Windows.
fn separator_glyph(theme: &Theme) -> Div {
    div()
        .text_color(theme.secondary.opacity(0.5))
        .text_size(text::BODY)
        .child(std::path::MAIN_SEPARATOR_STR)
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
        .child(widgets::crumb(id.clone(), label, current, cx).on_click(
            cx.listener(move |this, _, _, cx| {
                if current {
                    this.open_crumb_menu(&label_path, cx);
                } else {
                    this.go_to(label_path.clone(), cx);
                }
            }),
        ))
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
            crate::marks::display_path(&app.root_path, app.home.as_deref())
        } else {
            node.name.to_string()
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

/// Four tiles in category colours, and the name.
fn logo(theme: &Theme) -> Div {
    let tile = |category| {
        div()
            .size(space::SM)
            .bg(palette::category_accent(theme, category))
    };
    let column = |top, bottom| {
        div()
            .flex()
            .flex_col()
            .gap(space::XXS)
            .child(tile(top))
            .child(tile(bottom))
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .flex_shrink_0()
        .child(
            div()
                .flex()
                .flex_row()
                .gap(space::XXS)
                .child(column(Category::Code, Category::Synced))
                .child(column(Category::AgentScratch, Category::Toolchain)),
        )
        .child(
            div()
                .text_size(text::HEADING)
                .font_weight(FontWeight::BOLD)
                .text_color(theme.bright)
                .child("disktree"),
        )
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

    let mode = {
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
        .w(size::RANKING_CHOICE)
        // Tighter than a standalone group, so the segmented control shares
        // the checkboxes' height and centre line.
        .p(space::XXS)
    };

    let check = |on: bool| {
        if on {
            CheckboxState::Checked
        } else {
            CheckboxState::Unchecked
        }
    };
    let hidden = {
        let entity = entity.clone();
        let focus = focus.clone();
        checkbox(
            "hidden",
            "Hidden files",
            check(app.options.include_hidden),
            cx,
        )
        .tab_stop(false)
        .on_change(move |_, _, window, cx| {
            let _ = entity.update(cx, |this, cx| {
                this.options.include_hidden = !this.options.include_hidden;
                this.start_scan(cx);
            });
            window.focus(&focus, cx);
        })
    };
    let apparent = {
        checkbox(
            "apparent",
            "Apparent size",
            check(app.options.apparent_size),
            cx,
        )
        .tab_stop(false)
        .on_change(move |_, _, window, cx| {
            let _ = entity.update(cx, |this, cx| {
                this.options.apparent_size = !this.options.apparent_size;
                this.start_scan(cx);
            });
            window.focus(&focus, cx);
        })
    };

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
                    .tab_stop(false)
                    .disabled(!enabled)
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
    let history = div()
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
        ));

    let depth = app.layout_options.max_depth;
    let stepper = |id: &'static str,
                   label: &'static str,
                   step: i32,
                   cx: &mut Context<'_, Disktree>| {
        button(id, label, ButtonVariant::Secondary, cx)
            .tab_stop(false)
            .disabled(if step < 0 { depth <= 1 } else { depth >= 6 })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.adjust_depth(step, cx);
                window.focus(&this.focus, cx);
            }))
    };
    let depth_control = with_tooltip(
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
    );

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .flex_shrink_0()
        .child(history)
        .child(mode)
        .child(hidden)
        .child(apparent)
        .child(depth_control)
}

// ── trail and legend ────────────────────────────────────────────────────

/// Where you are, as a clickable trail, and what the colours mean, on one
/// row over the mosaic.
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
        .px(space::LG)
        .py(space::SM)
        .child(scan_totals(app, theme));
    if app.find_open || !app.find.is_empty() {
        row = row.child(find_field(app, theme));
    }
    row.child(div().flex_1()).child(legend(app, theme, cx))
}

/// What the whole scan found, as one quiet line; unreadable paths are
/// flagged in the warning colour when there are any.
fn scan_totals(app: &Disktree, theme: &Theme) -> Div {
    let tree = app.tree();
    let bytes = tree.map_or(app.progress.bytes, |node| node.bytes);
    let files = tree.map_or(app.progress.files, |node| node.files);
    let dirs = tree.map_or(app.progress.dirs, |node| node.dirs);
    let errors = app.progress.errors;
    div()
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
        .when(errors > 0, |this| {
            this.child(div().text_color(theme.warning).child(format!(
                "· {} unreadable",
                widgets::human_count(errors)
            )))
        })
}

/// The key to the colours: the categories, or the age ramp in age mode.
/// Clipped from the trailing end when the row runs out of room.
fn legend(app: &Disktree, theme: &Theme, cx: &App) -> Div {
    let item = |swatch: Div, label: &'static str| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::XS)
            .flex_shrink_0()
            .child(swatch)
            .child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary)
                    .child(label),
            )
    };
    let mut lane = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::MD)
        .min_w_0()
        .overflow_hidden();
    if app.color_mode == ColorMode::Age {
        for (bucket, (_, label)) in palette::AGE_BUCKETS.iter().enumerate() {
            lane = lane.child(item(
                widgets::swatch(palette::age_accent(theme, bucket)),
                label,
            ));
        }
    } else {
        for category in Category::LEGEND {
            lane = lane.child(item(
                widgets::swatch(palette::category_accent(theme, category)),
                category.label(),
            ));
        }
    }
    let ground = palette::category_fill(theme, Category::Other, 0);
    let hatch = cx.omarchy().bright.opacity(0.5);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::MD)
        .min_w_0()
        .overflow_hidden()
        .child(item(widgets::hatch_swatch(hatch, ground), "Reclaimable"))
        .child(lane)
}

// ── side panel ──────────────────────────────────────────────────────────

/// Selection, worth a look, marked, and the disk: everything a decision
/// needs, next to the mosaic rather than under it.
fn side_panel(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
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
        .child(selection_section(app, theme, cx))
        .child(rule())
        // The lists scroll; the selection above and the disk below stay put,
        // so the two numbers that matter never leave the screen.
        .child(
            div()
                .id("panel-lists")
                .flex()
                .flex_col()
                .gap(space::LG)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(worth_section(app, theme, cx))
                .child(rule())
                .child(marked_section(app, theme, cx)),
        )
        .children(notice_line(app, theme, cx))
        .children(privacy_line(app, theme, cx))
        .child(disk_section(app, theme, cx))
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
    let marked = path.as_deref().is_some_and(|path| app.marks.contains(path));
    let covered_by = marks_ancestor(app, path.as_deref());
    let is_current_root = target == app.crumbs;
    let highlight = palette::highlight(theme);

    let identity = div()
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
                        .child(node.name.to_string()),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(space::XS)
                .min_w_0()
                .child(
                    div()
                        .min_w_0()
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(path.as_deref().map_or_else(
                            String::new,
                            |path| {
                                crate::marks::display_path(
                                    path,
                                    app.home.as_deref(),
                                )
                            },
                        )),
                )
                .when(node.is_dir(), |row| {
                    row.children(path.clone().map(|path| {
                        with_tooltip(
                            gpui_omarchy::icon_button(
                                "search-folder",
                                gpui_omarchy::IconName::Search,
                                "Look up this folder on Google",
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .debug_selector(|| "search-folder".into())
                            .tab_stop(false)
                            .flex_shrink_0()
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.search_folder(&path, cx);
                                    window.focus(&this.focus, cx);
                                },
                            )),
                            "Search Google for this folder's purpose, \
                             application, and deletion risks",
                        )
                    }))
                }),
        );

    let (number, unit) = match app.options.metric {
        Metric::Bytes => widgets::split_size(&human_bytes(node.bytes)),
        Metric::Files => (widgets::human_count(node.files), "files".into()),
    };
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
        .child(widgets::bar(share, highlight, cx));

    let fourth = if node.is_dir()
        && let Some(path) = &path
        && crate::git::is_checkout(path)
    {
        let value = match app.git.get(path) {
            Some(Some(state)) => state.summary(),
            Some(None) => "not readable".to_string(),
            None => "asking\u{2026}".to_string(),
        };
        let clean =
            matches!(app.git.get(path), Some(Some(state)) if state.is_clean());
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
    if !is_current_root {
        if node.is_dir() {
            let crumbs = target.clone();
            actions = actions.child(
                button("open", "Open", ButtonVariant::Outline, cx)
                    .tab_stop(false)
                    .flex_1()
                    .justify_center()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.go_to(crumbs.clone(), cx);
                        window.focus(&this.focus, cx);
                    })),
            );
        }
        // Inside a marked directory there is nothing to mark on its own: it
        // goes with that directory, so the button offers to keep it by
        // unmarking the directory instead.
        let ancestor =
            path.as_deref().and_then(|path| app.marked_ancestor(path));
        let crumbs = target;
        let label = match &ancestor {
            Some(ancestor) if !marked => {
                format!("Unmark {}", short_name(ancestor))
            }
            _ if marked => "Unmark".to_string(),
            _ => "Mark for removal".to_string(),
        };
        let mark = button("mark", label, ButtonVariant::Primary, cx)
            .tab_stop(false)
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
            mark.bg(highlight)
                .border_color(highlight)
                .text_color(on)
                .font_weight(FontWeight::SEMIBOLD)
                .hover(move |style| {
                    style.bg(highlight.opacity(0.85)).border_color(highlight)
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
        .child(actions)
}

/// The biggest things that could plausibly go, with their total.
fn worth_section(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let total: u64 = app.insights.iter().map(|candidate| candidate.bytes).sum();
    let largest = app.insights.first().map_or(1, |candidate| candidate.bytes);
    let highlight = palette::highlight(theme);
    let mut section = div().flex().flex_col().gap(space::XS).child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .pb(space::XS)
            .child(widgets::eyebrow("Worth a look", cx))
            .when(total > 0, |this| {
                this.child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(highlight)
                        .child(human_bytes(total)),
                )
            }),
    );
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
        chain.iter().skip(1).map(|node| &*node.name).collect();
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
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let plan = app.plan();
    let count = app.marks.len();
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(widgets::eyebrow(
            if count == 0 {
                "Marked".to_string()
            } else {
                format!("Marked · {count}")
            },
            cx,
        ))
        .when(count > 0, |this| {
            this.child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary)
                    .child(human_bytes(plan.bytes())),
            )
        });
    let mut section = div().flex().flex_col().gap(space::XS).child(header);
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
                        .child(crate::marks::display_path(
                            &item.path,
                            app.home.as_deref(),
                        )),
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

/// The last thing that happened, or a scan error, above the disk.
fn notice_line(app: &Disktree, theme: &Theme, cx: &App) -> Option<Div> {
    let (message, color) = if let Some(error) = &app.scan_error {
        (error.clone(), theme.danger)
    } else {
        let (message, status) = app.notice.as_ref()?;
        (message.clone(), widgets::alert_color(*status, cx))
    };
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
    if app.progress.errors == 0 || app.full_disk_access != Some(false) {
        return None;
    }
    let color = theme.warning;
    let errors = app.progress.errors;
    let noun = if errors == 1 { "item" } else { "items" };
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
            .child(div().text_color(color).child(format!(
                "macOS kept {} {noun} unreadable. Give disktree Full Disk \
                 Access, or your terminal if you started it there, then \
                 reopen it.",
                widgets::human_count(errors)
            )))
            .child(
                button(
                    "privacy",
                    "Open Privacy Settings",
                    ButtonVariant::Outline,
                    cx,
                )
                .tab_stop(false)
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

/// Free space on the volume now, and after the marks go.
fn disk_section(
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let device = app.device.clone().unwrap_or_default();
    let mut section = div().flex().flex_col().gap(space::SM).child(
        div()
            .flex()
            .flex_row()
            .gap(space::SM)
            .child(widgets::eyebrow("Disk", cx))
            .child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary.opacity(0.6))
                    .child(device),
            ),
    );
    let Some(space_info) = app.space else {
        return section.child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child("Free space is not available here"),
        );
    };
    let reclaiming = app.plan().bytes();
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
                            "→ {} free",
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
        .children(
            (!app.marks.is_empty())
                .then(|| review_button(app, reclaiming, theme, cx)),
        )
}

/// The way to the review screen, where the saving is: it says what is
/// marked and what it frees, so the number that matters is the thing to
/// press. Outlined in the highlight, so it does not compete with the filled
/// "Mark for removal" above it.
fn review_button(
    app: &Disktree,
    reclaiming: u64,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> impl IntoElement {
    let highlight = palette::highlight(theme);
    let count = app.marks.len();
    div()
        .id("review")
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
                .child(format!(
                    "Review {count} marked · frees {}…",
                    human_bytes(reclaiming)
                )),
        )
        .child(gpui_omarchy::keycap("c", cx))
}

// ── key bar ─────────────────────────────────────────────────────────────

/// The keys, quietly: outlines and light labels, there when needed. The key
/// to every other key and the scan's own numbers hold the trailing edge.
fn key_bar(app: &Disktree, theme: &Theme, cx: &App) -> Div {
    // Most useful first, so a narrow window clips the least useful.
    let hints: [(&str, &str); 10] = [
        ("space", "mark"),
        ("enter", "open"),
        ("\u{232b}", "up"),
        ("c", "review"),
        ("hjkl", "move"),
        ("/", "filter"),
        ("[ ]", "depth"),
        ("t", "mode"),
        ("0", "reset"),
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
    let scan = if app.scan.is_some() {
        format!(
            "scanning \u{00b7} {} entries \u{00b7} {}",
            widgets::human_count(app.progress.files),
            human_bytes(app.progress.bytes)
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
fn scanning_panel(app: &Disktree, theme: &Theme, cx: &gpui_kit::App) -> Div {
    let progress = &app.progress;
    let mut panel = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .gap(space::LG)
        .bg(theme.inset)
        .child(
            gpui_omarchy::icon(gpui_omarchy::IconName::Loader)
                .size(icon::LG)
                .text_color(theme.accent),
        )
        .child(
            div()
                .text_size(text::TITLE)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.bright)
                .child(format!("Reading {}", widgets::display_root(app))),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(space::XL)
                .child(widgets::stat(
                    "files",
                    widgets::human_count(progress.files),
                    cx,
                ))
                .child(widgets::stat("directories", widgets::human_count(progress.dirs), cx))
                .child(widgets::stat("measured", human_bytes(progress.bytes), cx))
                .child(widgets::stat_colored(
                    "unreadable",
                    widgets::human_count(progress.errors),
                    if progress.errors > 0 {
                        theme.warning
                    } else {
                        theme.secondary
                    },
                    cx,
                )),
        )
        .child(
            div()
                .w(size::SCANNING_METER)
                .child(widgets::meter_row(
                    "",
                    "",
                    progress_estimate(progress.files),
                    theme.accent,
                    cx,
                )),
        )
        .child(
            div()
                .text_size(text::BODY)
                .text_color(theme.secondary)
                .child("Marking, zooming and the free-space meter all work as soon as it lands."),
        );

    if let Some(error) = &app.scan_error {
        panel = panel.child(
            div()
                .max_w(size::HELP)
                .p(space::MD)
                .border_1()
                .border_color(theme.danger)
                .text_color(theme.danger)
                .text_size(text::BODY)
                .child(error.clone()),
        );
    }
    panel
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
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .px(space::SM)
        .py(space::XS)
        .min_w_0()
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
                .size(icon::SM)
                .text_color(if app.filter_applied {
                    highlight
                } else {
                    theme.secondary
                }),
        )
        .child(if app.find.is_empty() {
            div()
                .text_color(theme.secondary.opacity(0.7))
                .child("Filter by name")
        } else {
            div().text_color(theme.bright).child(app.find.clone())
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
                    .whitespace_nowrap()
                    .child(summary),
            )
        })
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary.opacity(0.6))
                .whitespace_nowrap()
                .child(hint),
        )
}

fn short_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
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
        .filter(|item| item.path != path && path.starts_with(&item.path))
        .map(|item| short_name(&item.path))
        .next()
}

/// A scan has no total to measure against, so the meter shows the work done
/// rather than pretending to know how far along it is.
fn progress_estimate(files: u64) -> f32 {
    if files == 0 {
        0.06
    } else {
        // Asymptotic: the bar keeps moving while the walk continues.
        let scaled = files as f32 / (files as f32 + 20_000.0);
        (0.1 + scaled * 0.85).min(0.99)
    }
}

// ── review ──────────────────────────────────────────────────────────────

fn review(
    app: &Disktree,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Div {
    let theme = cx.omarchy().clone();
    let plan = app.plan();
    let items: Vec<Target> = app.marks.items().to_vec();
    let covered: Vec<Target> = plan.covered.clone();
    let blocked = plan.blocked.clone();

    let mut list = div()
        .id("marked-list")
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .overflow_y_scroll()
        .border_1()
        .border_color(theme.border)
        .bg(theme.inset);

    if items.is_empty() {
        list = list.child(
            div()
                .p(space::XXL)
                .text_color(theme.secondary)
                .child("Nothing is marked. Go back and mark what should go."),
        );
    }

    for (index, item) in items.iter().take(LIST_LIMIT).enumerate() {
        let is_covered =
            covered.iter().any(|covered| covered.path == item.path);
        let blocked_reason = blocked
            .iter()
            .find(|blocked| blocked.path == item.path)
            .map(|blocked| blocked.reason.clone());
        list = list.child(mark_row(
            index,
            item,
            is_covered,
            blocked_reason,
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
                .child(format!(
                    "{} more are marked and will be removed too. Unmark them in the treemap.",
                    items.len() - LIST_LIMIT
                )),
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
                "{} marked \u{00b7} {} to free",
                app.marks.len(),
                human_bytes(plan.bytes())
            ),
            &theme,
            cx,
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .gap(space::LG)
                .p(space::LG)
                .flex_1()
                .min_h_0()
                .child(list)
                .child(summary),
        )
        .child(review_footer(app, &theme, cx))
}

/// One marked path. Lanes are fixed, so names, bars and sizes line up down
/// the list and sizes can be compared by eye.
fn mark_row(
    index: usize,
    item: &Target,
    covered: bool,
    blocked: Option<String>,
    app: &Disktree,
    theme: &Theme,
    cx: &Context<'_, Disktree>,
) -> Div {
    let root_value = app.tree().map_or(0, |tree| tree.bytes);
    let path_text = crate::marks::display_path(&item.path, app.home.as_deref());
    let color = if blocked.is_some() || covered {
        theme.secondary
    } else {
        theme.foreground
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::MD)
        .px(space::MD)
        .py(space::SM)
        .border_b_1()
        .border_color(theme.divider())
        .child(
            gpui_omarchy::icon(if item.is_dir {
                gpui_omarchy::IconName::FolderOpen
            } else {
                gpui_omarchy::IconName::File
            })
            .size(icon::SM)
            .flex_shrink_0()
            .text_color(theme.secondary),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::XXS)
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .min_w_0()
                        .text_color(color)
                        .child(short_name(&item.path)),
                )
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .child(path_text),
                ),
        )
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
            .on_click(cx.listener({
                let path = item.path.clone();
                move |this, _, _, cx| this.unmark(&path.clone(), cx)
            })),
        )
}

fn review_summary(
    app: &Disktree,
    plan: &disktree_core::removal::Plan,
    theme: &Theme,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Div {
    let reclaiming = plan.bytes();
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

    let explanation = match app.removal_mode {
        RemovalMode::Trash => format!(
            "Recoverable from the trash until it is emptied. Uses {}.",
            app.trash_backend.label()
        ),
        RemovalMode::Permanent => {
            "Deleted at once, like rm -rf. Nothing is recoverable.".to_string()
        }
    };

    let mut panel = div()
        .flex()
        .flex_col()
        .gap(space::XL)
        .w(size::REVIEW_SUMMARY)
        .flex_shrink_0()
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
                .child(mode)
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .child(explanation),
                ),
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
                    "Space freed",
                    human_bytes(reclaiming),
                    cx,
                )),
        );

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
                        short_name(&item.path),
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

    panel
        .child(div().flex_1())
        .children(notice_line(app, theme, cx))
        .child(export_controls(plan, cx))
        .child(commit_controls(app, plan, cx))
}

/// The list handed on instead of acted on: saved as paths, or copied as a
/// prompt for a coding agent to do the cleanup with care.
fn export_controls(
    plan: &disktree_core::removal::Plan,
    cx: &Context<'_, Disktree>,
) -> Div {
    div()
        .flex()
        .flex_row()
        .justify_end()
        .gap(space::SM)
        .child(
            button(
                "save-list",
                "Save list\u{2026}",
                ButtonVariant::Secondary,
                cx,
            )
            .disabled(plan.is_empty())
            .on_click(cx.listener(|this, _, _, cx| {
                this.save_delete_list(cx);
            })),
        )
        .child(
            button(
                "copy-prompt",
                "Copy as prompt",
                ButtonVariant::Secondary,
                cx,
            )
            .disabled(plan.is_empty())
            .on_click(cx.listener(|this, _, _, cx| {
                this.copy_agent_prompt(cx);
            })),
        )
}

/// The screen's one commitment. Moving to the trash is the default commit,
/// so it is the primary action; a permanent deletion is destructive, so it is
/// the danger action and asks first, which its ellipsis promises.
fn commit_controls(
    app: &Disktree,
    plan: &disktree_core::removal::Plan,
    cx: &Context<'_, Disktree>,
) -> Div {
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
    let unavailable = plan.is_empty()
        || (app.removal_mode == RemovalMode::Trash
            && !app.trash_backend.is_available());

    div()
        .flex()
        .flex_row()
        .justify_end()
        .gap(space::SM)
        .child(
            button("back", "Back", ButtonVariant::Secondary, cx).on_click(
                cx.listener(|this, _, window, cx| {
                    this.screen = Screen::Explore;
                    cx.notify();
                    window.focus(&this.focus, cx);
                }),
            ),
        )
        .child(
            button("commit", label, variant, cx)
                .disabled(unavailable)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.commit(cx);
                    this.apply_focus(window, cx);
                })),
        )
}

fn review_footer(app: &Disktree, theme: &Theme, cx: &App) -> Div {
    let commit = match app.removal_mode {
        RemovalMode::Trash => "move to trash",
        RemovalMode::Permanent => "delete\u{2026}",
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::LG)
        .px(space::LG)
        .py(space::SM)
        .border_t_1()
        .border_color(theme.divider())
        .child(widgets::hint("enter", commit, cx))
        .child(widgets::hint("m", "trash", cx))
        .child(widgets::hint("p", "permanent", cx))
        .child(widgets::hint("!", "unmark all", cx))
        .child(widgets::hint("s", "save list", cx))
        .child(widgets::hint("a", "copy as prompt", cx))
        .child(widgets::hint("esc", "back", cx))
        .child(div().flex_1())
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(format!(
                    "{} available",
                    app.space.map_or_else(
                        || "?".into(),
                        |space| human_bytes(space.available)
                    )
                )),
        )
}

// ── running ─────────────────────────────────────────────────────────────

fn running(app: &Disktree, cx: &gpui_kit::Context<'_, Disktree>) -> Div {
    let theme = cx.omarchy().clone();
    let summary = app.run_summary.clone();
    let done = summary.removed + summary.failed as u64;
    let progress = if summary.total == 0 {
        0.0
    } else {
        done as f32 / summary.total as f32
    };

    let mut log = div()
        .id("run-log")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .border_1()
        .border_color(theme.border)
        .bg(theme.inset);
    for (path, outcome) in app.run_log.iter().rev().take(200) {
        let ok = outcome.is_ok();
        log = log.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(space::SM)
                .px(space::MD)
                .py(space::XS)
                .text_size(text::CAPTION)
                .child(
                    gpui_omarchy::icon(if ok {
                        gpui_omarchy::IconName::Check
                    } else {
                        gpui_omarchy::IconName::X
                    })
                    .size(icon::SM)
                    .text_color(if ok {
                        theme.success
                    } else {
                        theme.danger
                    }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(if ok {
                            theme.foreground
                        } else {
                            theme.danger
                        })
                        .child(short_name(path)),
                )
                .children(outcome.as_ref().err().map(|error| {
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.warning)
                        .child(error.clone())
                })),
        );
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(screen_header(
            "Removing",
            &format!("{} of {} done", done, summary.total),
            &theme,
            cx,
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::MD)
                .p(space::LG)
                .flex_1()
                .min_h_0()
                .child(widgets::meter_row(
                    "progress",
                    format!(
                        "{} removed · {} to free",
                        summary.removed,
                        human_bytes(summary.bytes)
                    ),
                    progress,
                    if app.removal_mode == RemovalMode::Permanent {
                        theme.danger
                    } else {
                        theme.accent
                    },
                    cx,
                ))
                .children(
                    app.space.map(|space| widgets::space_meter(space, 0, cx)),
                )
                .child(log),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(space::LG)
                .px(space::LG)
                .py(space::SM)
                .border_t_1()
                .border_color(theme.divider())
                .child(widgets::hint("esc", "stop after the current item", cx)),
        )
}

// ── done ────────────────────────────────────────────────────────────────

fn done(app: &Disktree, cx: &gpui_kit::Context<'_, Disktree>) -> Div {
    let theme = cx.omarchy().clone();
    let summary = app.run_summary.clone();
    let measured = match (app.space_baseline, app.space) {
        (Some(before), Some(after)) => Some(
            after
                .available
                .cast_signed()
                .saturating_sub(before.available.cast_signed()),
        ),
        _ => None,
    };

    let mut failures = div().flex().flex_col().gap(space::XXS);
    for (path, outcome) in app
        .run_log
        .iter()
        .filter(|(_, outcome)| outcome.is_err())
        .take(40)
    {
        failures = failures.child(
            div()
                .flex()
                .flex_row()
                .gap(space::SM)
                .text_size(text::CAPTION)
                .child(div().text_color(theme.danger).child(short_name(path)))
                .child(div().text_color(theme.secondary).child(
                    outcome.as_ref().err().cloned().unwrap_or_default(),
                )),
        );
    }

    let mut body = div()
        .flex()
        .flex_col()
        .gap(space::MD)
        .p(space::LG)
        .flex_1()
        .min_h_0()
        .child(
            div()
                .flex()
                .flex_row()
                .gap(space::XXL)
                .child(widgets::stat_colored(
                    "removed",
                    format!("{} items", summary.removed),
                    theme.success,
                    cx,
                ))
                .child(widgets::stat("bytes claimed", human_bytes(summary.bytes), cx))
                .child(widgets::stat_colored(
                    "failed",
                    format!("{}", summary.failed),
                    if summary.failed > 0 {
                        theme.danger
                    } else {
                        theme.secondary
                    },
                    cx,
                ))
                .children(measured.map(|delta| {
                    widgets::stat_colored(
                        "volume freed",
                        if delta >= 0 {
                            format!("+{}", human_bytes(delta.unsigned_abs()))
                        } else {
                            format!("-{}", human_bytes(delta.unsigned_abs()))
                        },
                        if delta >= 0 { theme.success } else { theme.warning },
                        cx,
                    )
                })),
        )
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child("The treemap is being re-scanned so the numbers on screen match the disk again."),
        );

    if let Some(space) = app.space {
        body = body.child(widgets::space_meter(space, 0, cx));
    }
    if app.scan.is_some() {
        body = body.child(widgets::meter_row(
            "re-scanning",
            format!("{} files", widgets::human_count(app.progress.files)),
            progress_estimate(app.progress.files),
            theme.accent,
            cx,
        ));
    }
    if summary.failed > 0 {
        body = body
            .child(separator(cx))
            .child(widgets::section("what could not be removed", cx))
            .child(failures);
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(screen_header("Done", "removal finished", &theme, cx))
        .child(body)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(space::LG)
                .px(space::LG)
                .py(space::SM)
                .border_t_1()
                .border_color(theme.divider())
                .child(
                    // An acknowledgement: the result is already on screen.
                    button("continue", "Done", ButtonVariant::Primary, cx)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.screen = Screen::Explore;
                            cx.notify();
                            window.focus(&this.focus, cx);
                        })),
                )
                .child(widgets::hint("enter", "continue", cx)),
        )
}

// ── shared ──────────────────────────────────────────────────────────────

fn screen_header(
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
    let rem = window.rem_size().as_f32();
    let width = size::TOOLTIP.0 * rem;
    let height = HEIGHT_REMS * rem;
    let gap = space::MD.0 * rem;
    let edge = space::XS.0 * rem;

    let pointer = app.pointer?;
    let content = hover_tooltip(app, cx)?;
    let origin = app.treemap_origin.get();
    let window_size = window.bounds().size;

    // Flip to the other side of the pointer rather than overflow the window.
    let anchor_x = origin.x.as_f32() + pointer.x.as_f32();
    let anchor_y = origin.y.as_f32() + pointer.y.as_f32();
    let x = if anchor_x + gap + width > window_size.width.as_f32() {
        (anchor_x - gap - width).max(edge)
    } else {
        anchor_x + gap
    };
    let y = if anchor_y + gap + height > window_size.height.as_f32() {
        (anchor_y - gap - height).max(edge)
    } else {
        anchor_y + gap
    };

    Some(
        card_surface(cx)
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
        ("Back", "alt \u{2190} back")
    } else {
        ("Forward", "alt \u{2192} forward")
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
    let is_dir = app.node_at(crumbs)?.is_dir();
    let keys = if is_dir {
        "space mark · enter open"
    } else {
        "space mark"
    };
    node_card(app, crumbs, keys, cx)
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
    let marked = path.as_deref().is_some_and(|path| app.marks.contains(path));
    let hidden = node.name.starts_with('.');
    let covered = marks_ancestor(app, path.as_deref());

    let mut tip = div()
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
                        .child(node.name.to_string()),
                ),
        )
        .child(
            div()
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(path.as_deref().map_or_else(String::new, |path| {
                    crate::marks::display_path(path, app.home.as_deref())
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
                        node.dirs.saturating_sub(u64::from(node.is_dir()))
                    ),
                    human_bytes(node.own_bytes)
                )),
        );

    let mut badges = div().flex().flex_row().gap(space::XS).flex_wrap();
    if hidden {
        badges = badges.child(widgets::chip("Hidden", theme.secondary, cx));
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

fn help_overlay(app: &Disktree, cx: &gpui_kit::App) -> Div {
    let theme = cx.omarchy();
    // Sentence case, and the tile a key acts on is always the one under the
    // pointer if the pointer moved last, else the keyboard selection.
    let rows: [(&str, &str); 27] = [
        ("space / x", "Mark or unmark the tile you point at"),
        (MODIFIER_CLICK, "Mark without moving the selection"),
        ("enter", "Open that directory, at any depth"),
        ("\u{232b} / esc", "Go up one directory"),
        (
            "alt \u{2190} / \u{2192}",
            "Back or forward through where you have been",
        ),
        (
            "\u{2190} \u{2191} \u{2193} \u{2192}",
            "Move between tiles at this level",
        ),
        ("tab", "Next largest sibling"),
        ("scroll", "Zoom toward a directory, then go into it"),
        ("shift-scroll", "Pan the magnified view"),
        ("[ / ]", "Draw fewer or more levels at once"),
        ("- / = / 0", "Magnify, shrink, or reset the view"),
        (MODIFIER_ZOOM, "Interface zoom"),
        (
            "/",
            "Filter by name: only matches keep their colour; enter shows only them",
        ),
        ("c", "Review the marked list"),
        ("t", "Size, files or age: what areas and colours say"),
        ("r", "Scan again from the same root"),
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
        ("q", "Quit"),
        ("", ""),
        (
            "Review screen",
            "m trash \u{00b7} p permanent \u{00b7} ! unmark all \u{00b7} s save list \u{00b7} a copy as prompt",
        ),
        ("", "enter commits \u{00b7} esc goes back"),
        ("", "A permanent deletion always asks first"),
    ];

    let mut keys = div().flex().flex_col().gap(space::SM);
    for (key, label) in rows {
        if key.is_empty() && label.is_empty() {
            continue;
        }
        keys = keys.child(
            div()
                .flex()
                .flex_row()
                .gap(space::MD)
                .items_center()
                .child(
                    div()
                        .w(size::KEY_LANE)
                        .flex_shrink_0()
                        .text_size(text::CAPTION)
                        .text_color(theme.accent)
                        .child(key.to_string()),
                )
                .child(
                    div()
                        .text_size(text::BODY)
                        .text_color(theme.foreground)
                        .child(label.to_string()),
                ),
        );
    }

    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.background.opacity(0.86))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::MD)
                .w(size::HELP)
                .p(space::XL)
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(space::SM)
                        .child(
                            gpui_omarchy::icon(
                                gpui_omarchy::IconName::Keyboard,
                            )
                            .size(icon::MD)
                            .text_color(theme.accent),
                        )
                        .child(
                            div()
                                .text_size(text::TITLE)
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.bright)
                                .child("Keyboard and mouse"),
                        ),
                )
                .child(keys)
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .child(format!(
                            "? or esc closes \u{00b7} {} \u{00b7} {}",
                            app.root_path.display(),
                            app.options.metric.label()
                        )),
                ),
        )
}
