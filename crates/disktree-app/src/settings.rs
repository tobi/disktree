//! The Settings window: its own window, one at a time, as in PwrRoute,
//! PwrGit and PwrSnap. A nav on the left and panes of cards: Updates,
//! Safety, Agents (MCP), Diagnostics and About. No General pane until
//! there is something to put in it: Power Efficiency and the view settings
//! stay in the strip, where they are used. Design: "Menus and Help
//! Windows" in the PwrDisk Claude Design project, turn 3; Agents (MCP) in
//! "Notices, Sticky Headers and Agents (MCP)", as PwrSnap's Local Agents.
//!
//! Updates is the panel the main window used to draw, moved here with its
//! rows, its names and its keys (↑↓ and Enter pick a channel, C checks, R
//! restarts). Restart to update goes through the main window, which knows
//! whether a removal is running and refuses to cut one short.

use crate::semantics::{AccessibleButton as _, text as readable};
use disktree_core::path_style::PathStyle;
use disktree_core::removal_limit::Limit;

use crate::safety::Confirm;
use std::path::PathBuf;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, ClipboardItem, Context, Div, ElementId, FocusHandle,
    FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Render, Role, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Window, WindowHandle, div,
    img,
};
use gpui_omarchy::{
    ActiveTheme as _, ButtonVariant, ChoiceItem, Theme, button, button_group,
};

use crate::aux_window::{self, Spec};
use crate::build_info::BuildInfo;
use crate::chrome::Drags;
use crate::diagnostics::{self, Diagnostics};
use crate::state::Disktree;
use crate::ui::{size, space, text};
use crate::update_channel::{Slot, UpdateSettings};
use crate::updater;

pub use settings_actions::OpenSettings;

#[allow(
    clippy::derive_partial_eq_without_eq,
    reason = "GPUI's actions! macro writes the derives, not us"
)]
mod settings_actions {
    use gpui_kit::actions;

    actions!(
        disktree,
        [
            /// Open the Settings window, or bring it forward.
            OpenSettings,
        ]
    );
}

/// The element id of the window's root, for tests.
pub const ROOT_ID: &str = "settings-root";
pub const CHECK_ID: &str = "updates-check";
pub const RESTART_ID: &str = "updates-restart";
pub const COPY_DIAGNOSTICS_ID: &str = "diagnostics-copy";
pub const COPY_BUILD_ID: &str = "about-copy-build";
pub const COPY_PROMPT_ID: &str = "agents-copy-prompt";
/// How long a copy button says "Copied": PwrSnap's and PwrAgnt's 1.5 s.
pub const COPY_FEEDBACK: std::time::Duration =
    std::time::Duration::from_millis(1500);
/// About's header: the icon, the name and version, the build line.
pub const ABOUT_HEADER_ID: &str = "about-header";
pub const ABOUT_BUILD_LINE_ID: &str = "about-build-line";

const SPEC: Spec = Spec {
    title: "PwrDisk Settings",
    size: (1000.0, 680.0),
    // PwrGit's Settings minimum.
    min: (760.0, 520.0),
};

/// The nav's sections, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Updates,
    Diagnostics,
    Safety,
    Agents,
    About,
}

impl Pane {
    pub const ALL: [Self; 5] = [
        Self::Updates,
        Self::Safety,
        Self::Agents,
        Self::Diagnostics,
        Self::About,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Updates => "Updates",
            Self::Diagnostics => "Diagnostics",
            Self::Safety => "Safety",
            Self::Agents => "Agents (MCP)",
            Self::About => "About",
        }
    }

    /// The nav's heading above this pane, as the family's Settings group
    /// them: what PwrDisk does, then help with it.
    pub const fn group(self) -> &'static str {
        match self {
            Self::Updates | Self::Safety | Self::Agents => "App",
            Self::Diagnostics | Self::About => "Support",
        }
    }

    /// The nav item's element id.
    pub const fn nav_id(self) -> &'static str {
        match self {
            Self::Updates => "settings-nav-updates",
            Self::Diagnostics => "settings-nav-diagnostics",
            Self::Safety => "settings-nav-safety",
            Self::Agents => "settings-nav-agents",
            Self::About => "settings-nav-about",
        }
    }
}

pub fn install(cx: &mut App) {
    cx.on_action(|_: &OpenSettings, cx| {
        // Settings… reopens on the pane last shown; a first open starts on
        // Updates, the one with something to set.
        cx.defer(|cx| {
            let pane = aux_window::find::<SettingsWindow>(cx, |_| true)
                .and_then(|handle| handle.read(cx).ok().map(|view| view.pane))
                .unwrap_or(Pane::Updates);
            open(pane, cx);
        });
    });
}

/// Opens Settings on `pane`, or brings it forward and switches there.
pub fn open(pane: Pane, cx: &mut App) {
    let existing = aux_window::find::<SettingsWindow>(cx, |_| true);
    if let Some(handle) = existing {
        let _ = handle.update(cx, |view, _, cx| view.show(pane, cx));
    }
    if aux_window::activate(existing, cx) {
        return;
    }
    open_window(pane, cx);
}

/// A new Settings window, for `open` and the tests.
pub fn open_window(
    pane: Pane,
    cx: &mut App,
) -> Option<WindowHandle<SettingsWindow>> {
    aux_window::open(&SPEC, cx, |window, cx| {
        let view = SettingsWindow::new(pane, cx);
        window.focus(&view.focus, cx);
        view
    })
}

/// Redraws Settings › Agents (MCP) when the agent connected to the main
/// window changes. Only that pane: nothing else there follows the agent.
pub fn agents_changed(cx: &mut App) {
    if let Some(handle) = aux_window::find::<SettingsWindow>(cx, |_| true) {
        let _ = handle.update(cx, |view, _, cx| {
            if view.pane == Pane::Agents {
                cx.notify();
            }
        });
    }
}

/// Redraws Settings when the updater's state moves (`updater::install`).
/// Never for a download's percent, which Settings does not show: this
/// collects the diagnostics again.
pub fn refresh(cx: &mut App) {
    #[cfg(test)]
    REFRESHES.set(REFRESHES.get() + 1);
    if let Some(handle) = aux_window::find::<SettingsWindow>(cx, |_| true) {
        let _ = handle.update(cx, |view, _, cx| {
            view.diagnostics = diagnostics::collect(cx);
            cx.notify();
        });
    }
}

#[cfg(test)]
thread_local! {
    /// How many times [`refresh`] ran, for the test that a download's
    /// percent does not rebuild Settings.
    pub static REFRESHES: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// The window's view.
pub struct SettingsWindow {
    pub pane: Pane,
    /// The channel row the keys are on.
    pub updates_row: usize,
    /// Where the channel pick is saved: the settings folder; tests point
    /// it at a temporary one.
    pub updates_dir: Option<PathBuf>,
    pub diagnostics: Diagnostics,
    /// The copy button that last copied, which says "Copied" for
    /// [`COPY_FEEDBACK`].
    copied: Option<&'static str>,
    /// Puts "Copied" back to "Copy"; a newer copy replaces it, so an older
    /// click cannot cut the newer one's short.
    copied_reset: Option<gpui_kit::Task<()>>,
    /// Agents (MCP)'s commands: the executable does not move while it
    /// runs, so they are built once rather than on every redraw.
    connect: Vec<crate::agent::Connect>,
    /// Why the last pick could not be saved.
    pub note: Option<String>,
    /// The saved removal limit, as Safety shows it: read when the window
    /// opens, whenever Safety is shown and after a pick is saved, never
    /// while drawing, which runs on every frame.
    pub safety: Result<crate::safety::Policy, String>,
    focus: FocusHandle,
    pub pane_scroll: gpui_kit::ScrollHandle,
    strip_drag: bool,
}

impl Drags for SettingsWindow {
    fn strip_drag(&mut self) -> &mut bool {
        &mut self.strip_drag
    }
}

impl SettingsWindow {
    pub fn new(pane: Pane, cx: &Context<'_, Self>) -> Self {
        let installed = updater::installed_version();
        let updates_dir = updater::settings_dir();
        Self {
            pane,
            updates_row: updater::selection().resolve(&installed).index(),
            safety: crate::safety::load_policy(updates_dir.as_deref()),
            updates_dir,
            diagnostics: diagnostics::collect(cx),
            copied: None,
            copied_reset: None,
            connect: crate::agent::connect_commands(
                &std::env::current_exe()
                    .unwrap_or_else(|_| PathBuf::from("pwrdisk")),
            ),
            note: None,
            focus: cx.focus_handle(),
            pane_scroll: gpui_kit::ScrollHandle::new(),
            strip_drag: false,
        }
    }

    fn show(&mut self, pane: Pane, cx: &mut Context<'_, Self>) {
        self.pane = pane;
        self.copied = None;
        self.diagnostics = diagnostics::collect(cx);
        if pane == Pane::Safety {
            self.reload_safety();
        }
        cx.notify();
    }

    /// The copy button that says "Copied" now, if any.
    #[cfg(test)]
    pub const fn copied(&self) -> Option<&'static str> {
        self.copied
    }

    /// Read the saved removal limit again.
    fn reload_safety(&mut self) {
        self.safety = crate::safety::load_policy(self.updates_dir.as_deref());
        #[cfg(test)]
        work::SAFETY_LOADS.set(work::SAFETY_LOADS.get() + 1);
    }

    fn copy(
        &mut self,
        id: &'static str,
        text: String,
        cx: &mut Context<'_, Self>,
    ) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied = Some(id);
        self.copied_reset = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPY_FEEDBACK).await;
            let _ = this.update(cx, |this, cx| {
                this.copied = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Follow `slot` for updates from now on, and save the pick.
    pub fn pick(&mut self, slot: Slot, cx: &mut Context<'_, Self>) {
        self.updates_row = slot.index();
        self.note = updater::set_selection(
            UpdateSettings::picked(slot),
            self.updates_dir.as_deref(),
        )
        .err()
        .map(|error| {
            format!("Updates follow {slot} until PwrDisk quits: {error}")
        });
        self.diagnostics = diagnostics::collect(cx);
        cx.notify();
    }

    /// Whether the main window would let a restart through now.
    fn can_restart(cx: &App) -> bool {
        cx.windows()
            .into_iter()
            .filter_map(|window| window.downcast::<Disktree>())
            .filter_map(|main| main.read(cx).ok())
            .all(Disktree::can_restart_to_update)
    }

    /// Restart into the download, through the main window: it refuses
    /// while a removal runs, and says so in a toast.
    fn restart(cx: &mut App) {
        let main = cx
            .windows()
            .into_iter()
            .find_map(|window| window.downcast::<Disktree>());
        match main {
            Some(main) => {
                let _ = main.update(cx, |app, window, cx| {
                    window.activate_window();
                    app.restart_to_update(cx);
                });
            }
            None => updater::restart_in_background(),
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<'_, Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.control
            && matches!(keystroke.key.as_str(), "pageup" | "pagedown")
        {
            let index = Pane::ALL
                .iter()
                .position(|pane| *pane == self.pane)
                .unwrap_or(0);
            let step = if keystroke.key == "pageup" {
                Pane::ALL.len() - 1
            } else {
                1
            };
            self.show(Pane::ALL[(index + step) % Pane::ALL.len()], cx);
            return;
        }
        if keystroke.modifiers.platform || keystroke.modifiers.control {
            return;
        }
        let rows = Slot::ALL.len();
        match keystroke.key.as_str() {
            _ if self.pane != Pane::Updates => {}
            // The channels are a grid, Stable over Beta and Latest beside
            // Prerelease: up and down change the train, left and right the
            // track.
            "down" | "j" | "up" | "k" => {
                self.updates_row = (self.updates_row + 2) % rows;
                cx.notify();
            }
            "right" | "l" | "left" | "h" => {
                self.updates_row ^= 1;
                cx.notify();
            }
            "enter" | "space" => self.pick(Slot::ALL[self.updates_row], cx),
            "c" if !updater::state().checking => {
                updater::check_in_background(updater::Trigger::Manual);
                cx.notify();
            }
            "r" => {
                let installed = updater::installed_version();
                let slot = updater::selection().resolve(&installed);
                let state = updater::state();
                if !state.restarting
                    && updater::ready(&state, &installed, slot).is_some()
                {
                    cx.defer(Self::restart);
                }
            }
            _ => {}
        }
    }
}

impl Render for SettingsWindow {
    fn render(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        crate::ui::sync_scale(window, cx);
        let theme = cx.omarchy().clone();
        // When compact the panes wrap in a row above the content instead of
        // taking a column beside it, and the group headings, which only
        // make sense in a column, step out.
        let compact = window.viewport_size().width.as_f32()
            / window.rem_size().as_f32()
            < crate::ui::COMPACT_WINDOW_REMS;
        // Grouped, with the pane shown marked by an accent bar, as the
        // family's Settings windows have it; square, as Omarchy draws.
        let mut nav = div()
            .id("settings-panes")
            .role(gpui_kit::Role::TabList)
            .aria_label("Settings panes")
            .flex()
            .flex_col()
            .gap(space::XXS)
            .w(size::SETTINGS_NAV)
            .flex_shrink_0()
            .px(space::SM)
            .pb(space::SM)
            .border_r_1()
            .border_color(theme.divider())
            .bg(theme.inset)
            .when(compact, |nav| {
                nav.pt(space::SM).flex_row().flex_wrap().w_full()
            });
        let mut group = "";
        for &pane in &Pane::ALL {
            if pane.group() != group && !compact {
                group = pane.group();
                nav = nav.child(
                    div()
                        .px(space::SM)
                        .pt(space::MD)
                        .pb(space::XXS)
                        .font_family(theme.mono_font.clone())
                        .text_size(text::CAPTION)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.secondary)
                        .child(SharedString::from(group.to_uppercase())),
                );
            }
            let on = pane == self.pane;
            // The accent bar marks the pane shown, so focus gets the
            // app's full outline on a wrapper instead: recolouring the
            // bar would hide focus on the shown pane and make any other
            // focused row look shown too.
            nav = nav.child(
                div()
                    .id(pane.nav_id())
                    .role(gpui_kit::Role::Tab)
                    .aria_label(pane.label())
                    .aria_selected(on)
                    .focusable()
                    .tab_stop(true)
                    .h(size::SETTINGS_NAV_ROW)
                    .border_1()
                    .border_color(theme.inset)
                    .focus_visible(|style| style.border_color(theme.accent))
                    .debug_selector(move || pane.nav_id().into())
                    .cursor_pointer()
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .px(space::SM)
                            .border_l_2()
                            .border_color(if on {
                                theme.accent
                            } else {
                                theme.inset
                            })
                            .when(on, |item| {
                                item.bg(theme.accent.opacity(0.14))
                                    .text_color(theme.bright)
                            })
                            .hover(|item| item.bg(theme.hover_fill()))
                            .child(pane.label()),
                    )
                    .on_key_down(cx.listener(
                        move |this, event: &KeyDownEvent, _, cx| {
                            if matches!(
                                event.keystroke.key.as_str(),
                                "enter" | "space"
                            ) {
                                this.show(pane, cx);
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.show(pane, cx);
                    })),
            );
        }
        let pane = match self.pane {
            Pane::Updates => self.updates(&theme, cx),
            Pane::Diagnostics => self.diagnostics_pane(&theme, cx),
            Pane::Safety => self.safety_pane(&theme, window, cx),
            Pane::Agents => self.agents_pane(&theme, cx),
            Pane::About => self.about(&theme, cx),
        };
        let body = div()
            .id(ROOT_ID)
            .debug_selector(|| ROOT_ID.into())
            .track_focus(&self.focus)
            .tab_group()
            .tab_stop(false)
            .on_key_down(cx.listener(
                |this, event: &KeyDownEvent, window, cx| {
                    // Tab is the frame's; see `aux_window::frame`.
                    if !event.keystroke.modifiers.control
                        && matches!(
                            event.keystroke.key.as_str(),
                            "pageup" | "pagedown" | "home" | "end"
                        )
                        && crate::ui::scroll_key(
                            &event.keystroke.key,
                            &this.pane_scroll,
                            window.rem_size(),
                        )
                    {
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    this.on_key(event, cx);
                },
            ))
            .flex()
            .flex_row()
            .when(compact, gpui_kit::Styled::flex_col)
            .flex_1()
            .min_h_0()
            .child(nav)
            .child(
                div()
                    .id("settings-scroll")
                    .debug_selector(|| "settings-scroll".into())
                    .min_h_0()
                    .track_scroll(&self.pane_scroll)
                    .flex_1()
                    .min_w_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::LG)
                            .px(space::XL)
                            .py(space::XL)
                            .max_w(size::SETTINGS_PANE)
                            .child(pane),
                    ),
            );
        aux_window::frame(&["Settings"], div(), body, window, cx)
    }
}

/// The removal limits Settings › Safety offers, in percent of a volume;
/// `None` is Off, for the window only. 100% was the last choice before Off
/// existed, and said the same less plainly.
const LIMITS: [Option<u8>; 6] =
    [Some(1), Some(5), Some(10), Some(25), Some(50), None];

const APPLIES_TO: &str = "Trash may be recoverable until it is emptied; \
     restoring is not guaranteed. A permanent deletion cannot be undone \
     through PwrDisk. An agent must have the exact paths and mode confirmed \
     every time.";

/// What no setting lifts, said where the settings are so a refusal is
/// never a surprise.
#[cfg(target_os = "macos")]
const PROTECTED: &str = "/Users and each folder in it, your home and any \
     folder holding it; /System, /Library, /private, /usr, /opt/homebrew \
     and the other system trees; mount points and the scanned root.";
#[cfg(windows)]
const PROTECTED: &str = "The profiles folder and each profile in it, your \
     home and any folder holding it; Windows and Program Files; mount \
     points, drive roots and the scanned root.";
#[cfg(target_os = "linux")]
const PROTECTED: &str = "/home and each folder in it, your home and any \
     folder holding it; /usr, /etc, /boot and the other system trees; \
     mount points and the scanned root.";
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
const PROTECTED: &str = "Your home and any folder holding it; /usr, /etc, \
     /boot and the other system trees; mount points and the scanned root.";

const AGENTS: &str = "Agents over MCP stop at the limit, at 5% when it is \
     Off, and can never mark a protected folder or confirm for you.";

// ── panes ───────────────────────────────────────────────────────────────

impl SettingsWindow {
    fn safety_pane(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Div {
        let current = &self.safety;
        // One choice of six, so one control: the segments the review
        // screen uses for Trash or permanent, saved as soon as one is
        // picked. Six buttons read as six commands.
        let picked = current.as_ref().ok().and_then(|policy| {
            LIMITS.iter().position(|&percent| {
                percent == policy.limit.map(Limit::percent)
            })
        });
        let entity = cx.entity().downgrade();
        let rows =
            div()
                .debug_selector(|| "safety-limit".into())
                .child(button_group(
                    "safety-limit",
                    LIMITS
                        .iter()
                        .map(|percent| {
                            percent.map_or_else(
                                || ChoiceItem::new("off", "Off"),
                                |percent| {
                                    ChoiceItem::new(
                                        percent.to_string(),
                                        format!("{percent}%"),
                                    )
                                },
                            )
                        })
                        .collect(),
                    picked,
                    move |index, window, cx| {
                        let _ = entity.update(cx, |this, cx| {
                            this.note = LIMITS[index]
                                .map(Limit::new)
                                .transpose()
                                .and_then(|limit| {
                                    crate::safety::save_limit(
                                        this.updates_dir.as_deref(),
                                        limit,
                                    )
                                })
                                .err();
                            // Read back what was written, so the pane
                            // shows the file and not the click.
                            this.reload_safety();
                            cx.set_global(crate::safety::Saved);
                            // Back to the window, as the other panes'
                            // controls do, so Tab keeps walking panes.
                            window.focus(&this.focus, cx);
                            cx.notify();
                        });
                    },
                    window,
                    cx,
                ));
        let confirmed = current.as_ref().ok().and_then(|policy| {
            Confirm::ALL
                .iter()
                .position(|&confirm| confirm == policy.confirm)
        });
        let entity = cx.entity().downgrade();
        let confirm = div().debug_selector(|| "safety-confirm".into()).child(
            button_group(
                "safety-confirm",
                Confirm::ALL
                    .iter()
                    .map(|confirm| {
                        ChoiceItem::new(confirm.label(), confirm.label())
                    })
                    .collect(),
                confirmed,
                move |index, window, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        this.note = crate::safety::save_confirm(
                            this.updates_dir.as_deref(),
                            Confirm::ALL[index],
                        )
                        .err();
                        this.reload_safety();
                        cx.set_global(crate::safety::Saved);
                        window.focus(&this.focus, cx);
                        cx.notify();
                    });
                },
                window,
                cx,
            ),
        );
        let caption = |id: &'static str, text: &'static str| {
            readable(id, text)
                .text_color(theme.foreground)
                .child(text)
                .into_any_element()
        };
        let mut body = div()
            .flex()
            .flex_col()
            .gap(space::LG)
            .child(head(
                "Safety",
                "Removal limit",
                "Each operation is re-measured before anything is removed, \
                 from the window and from an agent over MCP.",
                None,
                theme,
            ))
            .child(
                card_plain(theme)
                    .child(field(
                        "Per operation",
                        Some(
                            "Measured data, as a share of each volume's \
                             capacity. The default is 5%. Past it, Mark \
                             needs Mark anyway.",
                        ),
                        vec![rows.into_any_element()],
                        theme,
                    ))
                    .child(field(
                        "Over the limit",
                        Some(
                            "What Review asks after Mark anyway, before \
                             anything is removed.",
                        ),
                        vec![confirm.into_any_element()],
                        theme,
                    ))
                    .child(field(
                        "Always protected",
                        Some("Never offered for removal. Not a setting."),
                        vec![caption("safety-protected", PROTECTED)],
                        theme,
                    ))
                    .child(field(
                        "Agents",
                        Some("Marks and removals over MCP."),
                        vec![caption("safety-agents", AGENTS)],
                        theme,
                    ))
                    .child(field(
                        "Applies to",
                        Some("Trash and permanent deletion."),
                        vec![caption("safety-applies", APPLIES_TO)],
                        theme,
                    )),
            );
        if let Err(reason) = current {
            body = body.child(
                readable("safety-error", reason.clone())
                    .role(Role::Alert)
                    .text_color(theme.warning)
                    .child(reason.clone()),
            );
        }
        if let Some(note) = &self.note {
            body = body.child(alert_note(note, theme));
        }
        body
    }

    /// How to connect an agent over MCP, who is connected, and what an
    /// agent may do. The main window's link answers the agent; this pane
    /// only reads who it is.
    fn agents_pane(&self, theme: &Theme, cx: &Context<'_, Self>) -> Div {
        let (connected, status) = cx
            .windows()
            .into_iter()
            .filter_map(|window| window.downcast::<Disktree>())
            .find_map(|main| main.read(cx).ok().map(Disktree::agent_status))
            .unwrap_or_else(|| {
                (false, "No PwrDisk window is open.".to_owned())
            });
        let mut connect = card("Connect", None, theme);
        for way in &self.connect {
            connect = connect.child(field(
                way.label,
                None,
                vec![self.copy_row(way.id, &way.text, theme, cx)],
                theme,
            ));
        }
        div()
            .flex()
            .flex_col()
            .gap(space::LG)
            .child(head(
                "App",
                "Agents (MCP)",
                "Let Claude Code, Codex or any MCP client scan, explore and \
                 mark with PwrDisk, and this window follows along. Removing \
                 stays yours.",
                None,
                theme,
            ))
            .child(card("Connection", None, theme).child(field(
                "Status",
                None,
                vec![
                        readable("agents-status", status.clone())
                            .debug_selector(|| "agents-status".into())
                            .flex()
                            .flex_row()
                            .gap(space::XS)
                            .child(
                                div()
                                    .text_color(if connected {
                                        theme.success
                                    } else {
                                        theme.secondary
                                    })
                                    .child("\u{25cf}"),
                            )
                            .child(
                                div()
                                    .text_color(theme.foreground)
                                    .child(status),
                            )
                            .into_any_element(),
                    ],
                theme,
            )))
            .child(connect)
            .child(card("First prompt", None, theme).child(field(
                "Ask it",
                Some("A proposal first: it marks only once you agree."),
                vec![self.copy_row(
                    COPY_PROMPT_ID,
                    crate::agent::FIRST_PROMPT,
                    theme,
                    cx,
                )],
                theme,
            )))
            .child(
                card("What an agent may do", None, theme)
                    .child(field(
                        "Look and mark",
                        None,
                        vec![value_text(
                            "Scan, walk the tree, mark what could go, and \
                             open the review screen."
                                .to_owned(),
                        )],
                        theme,
                    ))
                    .child(field(
                        "Remove",
                        None,
                        vec![value_text(
                            "Only after you confirm the exact paths and \
                             mode, within the limit in Safety. Removing \
                             from the review screen stays yours."
                                .to_owned(),
                        )],
                        theme,
                    )),
            )
    }

    fn updates(&self, theme: &Theme, cx: &Context<'_, Self>) -> Div {
        use crate::updater::Installation;

        let installed = updater::installed_version();
        let selection = updater::selection();
        let slot = selection.resolve(&installed);
        let state = updater::state();
        let ready = updater::ready(&state, &installed, slot);
        let installation = updater::installation();
        // Only a copy Velopack installed checks on its own.
        let on_its_own = match installation {
            Installation::Velopack { .. } => "PwrDisk also checks on its own.",
            Installation::NotInstalled { .. } => {
                "This copy never checks on its own."
            }
        };
        let lede = match installation {
            Installation::Velopack { .. } => {
                "Checks GitHub Releases at startup and every six hours. A \
                 download installs when you restart or quit."
            }
            Installation::NotInstalled { .. } => {
                "Not installed from the DMG, Setup.exe or the MSI, so this \
                 copy does not update itself. The channel still says which \
                 releases a check reads."
            }
        };

        // Stable over Beta, Latest beside Prerelease: the grid the family's
        // Updates panes use.
        let mut grid = div()
            .flex()
            .flex_col()
            .gap(space::XS)
            .px(space::MD)
            .pt(space::SM)
            .pb(space::SM)
            .border_t_1()
            .border_color(theme.divider());
        for (train, pair) in Slot::ALL.chunks(2).enumerate() {
            let mut row = div().flex().flex_row().gap(space::XS);
            for (track, &choice) in pair.iter().enumerate() {
                let index = train * 2 + track;
                row = row.child(self.slot_tile(index, choice, slot, theme, cx));
            }
            grid = grid.child(row);
        }
        let follows = if selection.is_inferred() {
            "Follows this build until you pick one."
        } else {
            "Picked, and saved with the other settings."
        };
        let channel = card(
            "Release channel",
            Some(
                readable("channel-purpose", "Which releases a check reads")
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary)
                    .child("Which releases a check reads")
                    .into_any_element(),
            ),
            theme,
        )
        .child(grid)
        .child(
            readable("channel-follows", follows)
                .px(space::MD)
                .pb(space::SM)
                .text_size(text::CAPTION)
                .text_color(theme.secondary)
                .child(follows),
        );

        let chip = ready.as_ref().map(|held| {
            chip(format!("{} ready", held.version), theme.warning, theme)
                .into_any_element()
        });
        let checked = if state.checking {
            "Checking now".to_owned()
        } else {
            state.last.as_ref().map_or_else(
                || "Not checked yet this session".to_owned(),
                |(when, _)| {
                    let then = when
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |since| {
                            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
                        });
                    format!(
                        "Checked {}",
                        crate::widgets::ago(crate::state::now_seconds(), then)
                    )
                },
            )
        };
        let mut actions = div().flex().flex_row().gap(space::SM).child(
            button(CHECK_ID, "Check now", ButtonVariant::Outline, cx)
                .debug_selector(|| CHECK_ID.into())
                .semantic_disabled(state.checking)
                .on_click(cx.listener(|_, _, _, cx| {
                    updater::check_in_background(updater::Trigger::Manual);
                    cx.notify();
                })),
        );
        if let Some(held) = &ready {
            let verb = if held.switch_back { "switch" } else { "update" };
            let label = if state.restarting {
                "Restarting\u{2026}".to_owned()
            } else {
                format!("Restart to {verb} to {}", held.version)
            };
            actions = actions.child(
                button(RESTART_ID, label, ButtonVariant::Primary, cx)
                .debug_selector(|| RESTART_ID.into())
                // As the panel had it: never while a removal runs, which an
                // exit would stop partway through the marked list; and once
                // pressed, until it fails.
                .semantic_disabled(state.restarting || !Self::can_restart(cx))
                .on_click(|_, _, cx| cx.defer(Self::restart)),
            );
        }
        let status = status_line(&state, ready.as_ref());
        let this_copy = card("This copy", chip, theme)
            .child(field_with(
                "Status",
                Some(checked.into()),
                vec![
                    readable("updates-status", status.clone())
                        .debug_selector(|| "updates-status".into())
                        .child(status)
                        .into_any_element(),
                ],
                theme,
            ))
            .child(field(
                "Check now",
                Some(on_its_own),
                vec![actions.into_any_element()],
                theme,
            ));

        let mut pane = div()
            .flex()
            .flex_col()
            .gap(space::LG)
            .child(head(
                "Updates",
                &format!("PwrDisk {installed}"),
                lede,
                None,
                theme,
            ))
            .child(channel)
            .child(this_copy);
        if let Some(note) = &self.note {
            pane = pane.child(alert_note(note, theme));
        }
        // Omarchy's apps list their keys; on macOS and Windows the keys
        // still work, and the help overlay names them.
        if crate::chrome::Platform::current() == crate::chrome::Platform::Linux
        {
            pane = pane.child(hint(
                "\u{2191}\u{2193}\u{2190}\u{2192} and enter pick a channel \
                 \u{b7} c checks \u{b7} r restarts into a download \u{b7} \
                 Ctrl+PageUp/PageDown switches panes",
                theme,
            ));
        }
        pane
    }

    /// One channel in the grid: picked, under the keys, or neither.
    fn slot_tile(
        &self,
        index: usize,
        choice: Slot,
        current: Slot,
        theme: &Theme,
        cx: &Context<'_, Self>,
    ) -> gpui_kit::Stateful<Div> {
        let picked = choice == current;
        let keyed = index == self.updates_row;
        div()
            .id(ElementId::Name(format!("update-slot-{index}").into()))
            .debug_selector(move || format!("update-slot-{index}"))
            .role(Role::RadioButton)
            .aria_label(format!("{choice}: {}", slot_purpose(choice)))
            .aria_toggled(if picked {
                gpui_kit::accesskit::Toggled::True
            } else {
                gpui_kit::accesskit::Toggled::False
            })
            .focusable()
            // One stop for the group, on the channel picked, as a radio
            // group takes Tab; the arrows move within it.
            .tab_stop(picked)
            .on_key_down(cx.listener(
                move |this, event: &KeyDownEvent, window, cx| {
                    match event.keystroke.key.as_str() {
                        "enter" | "space" => {
                            this.updates_row = index;
                            this.pick(choice, cx);
                            cx.stop_propagation();
                        }
                        "up" | "down" | "left" | "right" => {
                            this.updates_row = index;
                            this.on_key(event, cx);
                            window.focus(&this.focus, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                },
            ))
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(space::XXS)
            .px(space::MD)
            .py(space::SM)
            .border_1()
            .border_color(if picked {
                theme.accent
            } else if keyed {
                theme.border
            } else {
                theme.divider()
            })
            .when(picked, |tile| tile.bg(theme.accent.opacity(0.10)))
            .when(keyed && !picked, |tile| tile.bg(theme.hover_fill()))
            .hover(|tile| tile.bg(theme.hover_fill()))
            .cursor_pointer()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::SM)
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.bright)
                            .child(choice.to_string()),
                    )
                    .when(picked, |name| {
                        name.child(chip("Selected".into(), theme.accent, theme))
                    }),
            )
            .child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(theme.secondary)
                    .child(slot_purpose(choice)),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.updates_row = index;
                this.pick(choice, cx);
            }))
    }

    fn diagnostics_pane(&self, theme: &Theme, cx: &Context<'_, Self>) -> Div {
        let model = &self.diagnostics;
        let report = model.report();
        let copy = self.copy_button(COPY_DIAGNOSTICS_ID, "Copy", report, cx);
        let mut pane = div().flex().flex_col().gap(space::LG).child(head(
            "Support",
            "Diagnostics",
            "What this copy runs with. Copy carries these lines and \
             nothing else: no scanned path ever appears here. Help \u{203a} \
             Copy Diagnostics Info copies the same.",
            Some(copy.into_any_element()),
            theme,
        ));
        let mut this_copy = card("This copy", None, theme).child(field(
            "Version",
            None,
            vec![value_text(model.version_line())],
            theme,
        ));
        // Paths and Session have their own cards below.
        for section in model
            .sections
            .iter()
            .filter(|section| !matches!(section.title, "Paths" | "Session"))
        {
            for fact in &section.facts {
                this_copy = this_copy.child(field(
                    fact.label,
                    None,
                    vec![value_text(fact.value.clone())],
                    theme,
                ));
            }
        }
        pane = pane.child(this_copy);

        let home = std::env::home_dir();
        let shown = |path: &Option<PathBuf>| {
            path.as_deref().map_or_else(
                || "None".to_owned(),
                |path| {
                    crate::marks::display_path(
                        path,
                        home.as_deref(),
                        PathStyle::Native,
                    )
                },
            )
        };
        let settings_dir = model.settings_dir.clone();
        let log = model.log.clone();
        pane = pane.child(
            card("Files", None, theme)
                .child(field(
                    "Settings folder",
                    Some("The release channel and the power preset."),
                    vec![
                        command_box(shown(&settings_dir), theme)
                            .into_any_element(),
                        button(
                            "diagnostics-open-settings",
                            "Open settings folder",
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .debug_selector(|| "diagnostics-open-settings".into())
                        .semantic_disabled(
                            !settings_dir
                                .as_deref()
                                .is_some_and(std::path::Path::exists),
                        )
                        .on_click(move |_, _, cx| {
                            if let Some(dir) = &settings_dir {
                                cx.open_with_system(dir);
                            }
                        })
                        .into_any_element(),
                    ],
                    theme,
                ))
                .child(field(
                    "Log",
                    Some(
                        "Scans, removals and updates, never a scanned path. \
                         Help \u{203a} Logs follows it live.",
                    ),
                    vec![
                        command_box(shown(&log), theme).into_any_element(),
                        button(
                            "diagnostics-open-logs",
                            "Open Logs",
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .debug_selector(|| "diagnostics-open-logs".into())
                        .semantic_disabled(log.is_none())
                        .on_click(|_, _, cx| {
                            cx.defer(crate::logs_window::open);
                        })
                        .into_any_element(),
                    ],
                    theme,
                )),
        );
        if let Some(session) = model
            .sections
            .iter()
            .find(|section| section.title == "Session")
        {
            pane = pane.child(session.facts.iter().fold(
                card("This session", None, theme),
                |card, fact| {
                    card.child(field(
                        fact.label,
                        None,
                        vec![value_text(fact.value.clone())],
                        theme,
                    ))
                },
            ));
        }
        pane
    }

    fn about(&self, theme: &Theme, cx: &Context<'_, Self>) -> Div {
        let build = BuildInfo::current();
        let channel = updater::selection()
            .resolve(&updater::installed_version())
            .to_string();
        let header = card_frame(theme)
            .debug_selector(|| ABOUT_HEADER_ID.into())
            .flex_row()
            .items_center()
            .gap(space::MD)
            .p(space::MD)
            .child(
                img(crate::chrome::MARK.clone())
                    .size(size::ABOUT_ICON)
                    .flex_shrink_0(),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(space::XXS)
                    .child(
                        readable("version", format!("PwrDisk {}", build.version))
                            .role(Role::Heading)
                            .text_size(text::HEADING)
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.bright)
                            .child(format!("PwrDisk {}", build.version)),
                    )
                    // One line, as the design draws it: a long branch
                    // name ends in an ellipsis rather than wrapping.
                    // The label keeps the whole line for assistive
                    // technology when the painted one is cut short.
                    .child(
                        readable("build", build.header_line(&channel))
                            .debug_selector(|| ABOUT_BUILD_LINE_ID.into())
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(text::CAPTION)
                            .text_color(theme.secondary)
                            .child(build.header_line(&channel)),
                    ),
            )
            .when(!build.is_release(), |row| {
                row.child(chip("Dev build".into(), theme.warning, theme))
            })
            .child(self.copy_button(
                COPY_BUILD_ID,
                "Copy build info",
                build.report(),
                cx,
            ));
        let mut source = Vec::new();
        if let Some(checkout) = &build.checkout {
            source.push(self.copy_row(
                "about-copy-checkout",
                checkout,
                theme,
                cx,
            ));
        }
        match (build.is_release(), &build.tag, &build.branch) {
            (true, Some(tag), _) => {
                source.push(self.copy_row("about-copy-branch", tag, theme, cx));
            }
            (_, _, Some(branch)) => {
                source.push(self.copy_row(
                    "about-copy-branch",
                    branch,
                    theme,
                    cx,
                ));
            }
            _ => source.push(value_text(
                "Detached HEAD: the commit names it.".to_owned(),
            )),
        }
        match &build.commit {
            Some(commit) => {
                source.push(self.copy_row(
                    "about-copy-commit",
                    commit,
                    theme,
                    cx,
                ));
            }
            None => source.push(value_text(
                "No commit recorded: this copy was built outside a git \
                 checkout."
                    .to_owned(),
            )),
        }
        if let Some(line) = build.commit_line() {
            source.push(
                readable("commit-details", line.clone())
                    .text_color(theme.secondary)
                    .child(line)
                    .into_any_element(),
            );
        }
        if let Some(line) = build.changes_line() {
            source.push(
                readable("build-changes", line.clone())
                    .text_color(theme.warning)
                    .child(line)
                    .into_any_element(),
            );
        }
        let docs = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(space::SM)
            .child(
                button(
                    "about-license",
                    "View license",
                    ButtonVariant::Secondary,
                    cx,
                )
                .debug_selector(|| "about-license".into())
                .on_click(|_, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::help::ViewLicense),
                        cx,
                    );
                }),
            )
            .child(
                button(
                    "about-notices",
                    "Third-party notices",
                    ButtonVariant::Secondary,
                    cx,
                )
                .debug_selector(|| "about-notices".into())
                .on_click(|_, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::help::ThirdPartyNotices),
                        cx,
                    );
                }),
            )
            .child(
                button(
                    "about-changelog",
                    "Changelog",
                    ButtonVariant::Secondary,
                    cx,
                )
                .debug_selector(|| "about-changelog".into())
                .on_click(|_, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::help::OpenChangelog),
                        cx,
                    );
                }),
            );
        let repository = crate::help::REPOSITORY
            .trim_start_matches("https://")
            .to_owned();
        div()
            .flex()
            .flex_col()
            .gap(space::LG)
            .child(head(
                "PwrDisk",
                "About",
                "Finds what is eating a volume and removes what you mark, \
                 with the free space live on screen. A fork of disktree, in \
                 the Pwr family with PwrRoute, PwrGit and PwrAgnt.",
                None,
                theme,
            ))
            .child(header)
            .child(
                card("This copy", None, theme)
                    .child(field(
                        "Source",
                        Some("Read by git when this copy was built."),
                        source,
                        theme,
                    ))
                    .child(field(
                        "License",
                        Some("MIT. Includes disktree, by Tobi L\u{fc}tke."),
                        vec![docs.into_any_element()],
                        theme,
                    )),
            )
            .child(
                card("Project", None, theme)
                    .child(field(
                        "Source",
                        None,
                        vec![
                            mono(repository.clone(), theme).into_any_element(),
                        ],
                        theme,
                    ))
                    .child(field(
                        "Issues",
                        None,
                        vec![
                            mono(format!("{repository}/issues"), theme)
                                .into_any_element(),
                        ],
                        theme,
                    )),
            )
    }

    fn copy_button(
        &self,
        id: &'static str,
        label: &'static str,
        text: String,
        cx: &Context<'_, Self>,
    ) -> impl IntoElement {
        let label = if self.copied == Some(id) {
            "Copied"
        } else {
            label
        };
        // Several buttons read "Copy", so each is named for what it
        // copies, and "Copied" says so too.
        let name = match id {
            COPY_DIAGNOSTICS_ID => "Copy diagnostics report",
            "about-copy-checkout" => "Copy checkout path",
            "about-copy-branch" => "Copy branch or tag",
            "about-copy-commit" => "Copy commit",
            "agents-copy-command" => "Copy the command line",
            "agents-copy-claude" => "Copy the Claude Code command",
            "agents-copy-codex" => "Copy the Codex command",
            "agents-copy-json" => "Copy the MCP client JSON",
            COPY_PROMPT_ID => "Copy the first prompt",
            _ => label,
        };
        let name = if self.copied == Some(id) {
            format!("Copied: {name}")
        } else {
            name.to_owned()
        };
        button(id, label, ButtonVariant::Secondary, cx)
            .accessibility_label(name)
            .debug_selector(move || id.into())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.copy(id, text.clone(), cx);
            }))
    }

    /// A value in a command box with its own Copy.
    fn copy_row(
        &self,
        id: &'static str,
        value: &str,
        theme: &Theme,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        command_box(value.to_owned(), theme)
            .id(ElementId::Name(format!("{id}-row").into()))
            .child(self.copy_button(id, "Copy", value.to_owned(), cx))
            .into_any_element()
    }
}

/// What each release channel slot is for, in the family's words.
/// The answer to Check now, in the family's words: "You're up to date
/// (0.14.0).", "Update ready: 0.15.0. Restart to install."
fn status_line(
    state: &updater::State,
    ready: Option<&updater::Held>,
) -> String {
    use crate::updater::Outcome;
    if state.checking {
        return "Checking\u{2026}".to_owned();
    }
    match (&state.last, ready) {
        (None, _) => "Not checked yet.".to_owned(),
        (_, Some(held)) if held.switch_back => {
            format!("{} is downloaded. Restart to switch.", held.version)
        }
        (_, Some(held)) => {
            format!("Update ready: {}. Restart to install.", held.version)
        }
        (Some((_, outcome)), None) => match outcome {
            Outcome::UpToDate(version) => {
                format!("You're up to date ({version}).")
            }
            Outcome::Failed(why) => format!("Update check failed: {why}"),
            Outcome::NotInstalled(_) => {
                "This copy doesn't update itself.".to_owned()
            }
            Outcome::NothingPublished(slot) => {
                format!("Nothing is published on {slot} yet.")
            }
            other => other.to_string(),
        },
    }
}

const fn slot_purpose(slot: Slot) -> &'static str {
    use crate::update_channel::{Track, Train};
    match (slot.train, slot.track) {
        (Train::Stable, Track::Latest) => "Releases promoted after a check",
        (Train::Stable, Track::Prerelease) => "Candidates for the next one",
        (Train::Beta, Track::Latest) => "Beta builds off main",
        (Train::Beta, Track::Prerelease) => "The newest alpha; may not work",
    }
}

// ── the pane's parts ────────────────────────────────────────────────────

/// The pane's head: an eyebrow, the title, what the pane is for, and an
/// action at the end.
fn head(
    eyebrow: &str,
    title: &str,
    lede: &'static str,
    action: Option<AnyElement>,
    theme: &Theme,
) -> Stateful<Div> {
    let title = SharedString::from(title.to_owned());
    div()
        .id("settings-heading")
        .role(Role::Group)
        .aria_label(title.clone())
        .flex()
        .flex_col()
        .gap(space::XS)
        .child(
            div()
                .flex()
                .flex_row()
                .items_end()
                .gap(space::MD)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap(space::XXS)
                        .child(
                            div()
                                .font_family(theme.mono_font.clone())
                                .text_size(text::CAPTION)
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.accent)
                                .child(SharedString::from(
                                    eyebrow.to_uppercase(),
                                )),
                        )
                        .child(
                            readable("pane-title", title.clone())
                                .role(Role::Heading)
                                .text_size(text::HEADING)
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.bright)
                                .child(title),
                        ),
                )
                .children(action),
        )
        .child(
            readable("pane-description", lede)
                .max_w(size::SETTINGS_LEDE)
                .text_color(theme.secondary)
                .child(lede),
        )
}

/// A card: a raised heading strip with something at its end, then rows,
/// each of which draws the rule above it.
fn card(
    title: &'static str,
    trailing: Option<AnyElement>,
    theme: &Theme,
) -> Stateful<Div> {
    card_frame(theme)
        .id(title)
        .role(Role::Group)
        .aria_label(title)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(space::MD)
                .px(space::MD)
                .py(space::SM)
                .bg(theme.surface)
                .child(
                    readable("card-heading", title)
                        .role(Role::Heading)
                        .flex_1()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.bright)
                        .child(title),
                )
                .children(trailing),
        )
}

/// A card with no heading strip: its first row's rule is the card's top
/// edge, so the card draws none of its own.
fn card_plain(theme: &Theme) -> Div {
    card_frame(theme).border_t_0()
}

fn card_frame(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .border_1()
        .border_color(theme.divider())
        .bg(theme.inset)
}

/// A labelled row: the label and what it is for on the left, the values
/// at the right, one under the other, as the family's Settings rows sit.
fn field(
    label: &'static str,
    sub: Option<&'static str>,
    values: Vec<AnyElement>,
    theme: &Theme,
) -> Stateful<Div> {
    field_with(label, sub.map(SharedString::from), values, theme)
}

fn field_with(
    label: &'static str,
    sub: Option<SharedString>,
    values: Vec<AnyElement>,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(label)
        .role(Role::Group)
        .aria_label(label)
        .flex()
        .flex_row()
        .items_center()
        .flex_wrap()
        .gap(space::LG)
        .px(space::MD)
        .py(space::SM)
        .border_t_1()
        .border_color(theme.divider())
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::XXS)
                .w(size::SETTINGS_LABEL)
                .flex_shrink_0()
                .child(
                    readable("field-label", label)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.bright)
                        .child(label),
                )
                .children(sub.map(|sub| {
                    readable("field-description", sub.clone())
                        .text_size(text::CAPTION)
                        .text_color(theme.secondary)
                        .child(sub)
                })),
        )
        .child(
            div()
                .flex()
                .flex_col()
                // Buttons keep their own width; a command box spans it.
                .items_end()
                .gap(space::SM)
                .flex_1()
                // At least half the row, so a narrow window wraps the
                // values below their label rather than crushing them.
                .min_w(gpui_kit::relative(0.5))
                .text_color(theme.bright)
                .children(values),
        )
}

/// A value to copy, in monospace on the window's ground.
fn command_box(value: String, theme: &Theme) -> Div {
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(space::SM)
        .pl(space::SM)
        .pr(space::XS)
        .py(space::XXS)
        .border_1()
        .border_color(theme.divider())
        .bg(theme.background)
        .child(
            readable("command-value", value.clone())
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .font_family(theme.mono_font.clone())
                .text_color(theme.bright)
                .child(value),
        )
}

fn mono(value: String, theme: &Theme) -> Stateful<Div> {
    readable("project-url", value.clone())
        .font_family(theme.mono_font.clone())
        .text_color(theme.foreground)
        .child(value)
}

fn chip(label: String, color: gpui_kit::Hsla, theme: &Theme) -> Div {
    div()
        .px(space::SM)
        .border_1()
        .border_color(color)
        .text_color(color)
        .font_family(theme.mono_font.clone())
        .text_size(text::CAPTION)
        .child(label)
}

fn hint(text_: &'static str, theme: &Theme) -> Stateful<Div> {
    readable("settings-hint", text_)
        .text_size(text::CAPTION)
        .text_color(theme.secondary)
        .child(text_)
}

/// A value read out as text, not just painted.
fn value_text(value: String) -> AnyElement {
    readable("value", value.clone())
        .child(value)
        .into_any_element()
}

/// A failed save or pick, announced as it appears.
fn alert_note(note: &str, theme: &Theme) -> Stateful<Div> {
    readable("settings-note", note.to_owned())
        .role(Role::Alert)
        .text_color(theme.warning)
        .child(note.to_owned())
}

/// What Settings did, counted for the tests that hold it to doing it once.
#[cfg(test)]
pub mod work {
    use std::cell::Cell;

    thread_local! {
        /// Reads of the saved removal limit.
        pub static SAFETY_LOADS: Cell<usize> = const { Cell::new(0) };
    }
}
