//! Review's cache cleanup: under each marked path, what Check owners found
//! and exactly what would run, then the person's choice for that path; and
//! in the summary, the counts and how Delete files removes.

use std::fmt::Write as _;
use std::path::Path;

use disktree_core::cleanup::{
    Action, Decision, OWNER_RECOVERABILITY, Operation, Problem, Proposal,
};
use disktree_core::removal::RemovalMode;
use disktree_core::size::human_bytes;
use gpui_kit::{
    AnyElement, Context, Div, InteractiveElement as _, IntoElement,
    ParentElement, SharedString, Styled, Window, div,
};
use gpui_omarchy::{ButtonVariant, ChoiceItem, Theme, button, button_group};

use crate::semantics::AccessibleButton as _;
use crate::state::Disktree;
use crate::ui::{space, text};
use crate::widgets;

fn caption(text_: impl Into<SharedString>, color: gpui_kit::Hsla) -> Div {
    div()
        .text_size(text::CAPTION)
        .text_color(color)
        .child(text_.into())
}

/// Monospace, so argument boundaries and paths line up and read exactly.
fn exact(text_: impl Into<SharedString>, theme: &Theme) -> Div {
    caption(text_, theme.foreground).font_family(theme.mono_font.clone())
}

/// What the proposal says first, and the colour it is ruled in.
fn status(
    proposal: &Proposal,
    operations: &[Operation],
    theme: &Theme,
) -> (String, gpui_kit::Hsla) {
    let owner = proposal.owner.map_or("", |owner| owner.label());
    match (&proposal.issue, proposal.operations.first()) {
        (None, Some(first)) => (
            format!(
                "Ready \u{b7} {} {}",
                operations[*first].owner.label(),
                operations[*first].version
            ),
            theme.success,
        ),
        (Some(_), Some(_)) => (
            "Partly ready: some of its commands are unavailable".into(),
            theme.warning,
        ),
        (Some(issue), None) if issue.problem == Problem::Refused => {
            (format!("Refused \u{b7} {owner}"), theme.danger)
        }
        (Some(issue), None) if issue.problem == Problem::UnknownOwner => (
            "Unknown owner \u{b7} not known to be disposable".into(),
            theme.warning,
        ),
        (Some(issue), None) => {
            let label = issue.problem.label();
            let mut first = label.chars();
            let label = first.next().map_or_else(String::new, |head| {
                head.to_uppercase().chain(first).collect()
            });
            (format!("{label} \u{b7} {owner}"), theme.warning)
        }
        (None, None) => ("No owner command".into(), theme.warning),
    }
}

#[allow(
    clippy::unnecessary_debug_formatting,
    reason = "Consent must expose argument boundaries and control characters"
)]
fn command_lines(operation: &Operation) -> String {
    let mut lines = format!(
        "Executable  {:?}\nArguments   {:?}\n",
        operation.command.executable, operation.command.arguments
    );
    if !operation.command.environment.is_empty() {
        let _ =
            writeln!(lines, "Environment {:?}", operation.command.environment);
    }
    let _ = write!(
        lines,
        "Directory   {:?}\nCache root  {:?}\nScope       {:?}",
        operation.command.working_directory,
        operation.cache_root,
        operation.scope
    );
    lines
}

/// The proposal for marked path `index`, drawn under its row in Review.
pub fn proposal(
    app: &Disktree,
    index: usize,
    path: &Path,
    theme: &Theme,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Option<AnyElement> {
    if app.removal_mode != RemovalMode::OwnerCleanup || app.remote.is_some() {
        return None;
    }
    let block = |color| {
        div()
            .id(SharedString::from(format!("cleanup-proposal-{index}")))
            .debug_selector(move || format!("cleanup-proposal-{index}"))
            .flex()
            .flex_col()
            .gap(space::XS)
            .ml(space::XL)
            .mr(space::MD)
            .mb(space::SM)
            .pl(space::MD)
            .border_l_2()
            .border_color(color)
    };
    if app.cleanup_preparing() {
        return Some(
            block(theme.secondary)
                .child(caption("Checking owner\u{2026}", theme.secondary))
                .into_any_element(),
        );
    }
    let prepared = app.cleanup_ready().ok()?;
    let proposal = prepared
        .proposals
        .get(index)
        .filter(|proposal| proposal.selected == path)?;
    let decision = app.cleanup_decision(index);
    let (headline, color) = status(proposal, &prepared.operations, theme);
    let mut element = block(color).child(
        div()
            .text_size(text::CAPTION)
            .text_color(color)
            .child(headline),
    );
    for operation in &proposal.operations {
        let operation = &prepared.operations[*operation];
        element = element.child(exact(command_lines(operation), theme));
        if operation.wider() {
            element = element.child(caption(
                "Wider than selected: the command acts on the whole scope, \
                 including unselected siblings.",
                theme.warning,
            ));
        }
        element = element.child(caption(
            format!(
                "{} {} {OWNER_RECOVERABILITY}",
                operation.effects, operation.limitations
            ),
            theme.secondary,
        ));
    }
    if let Some(issue) = &proposal.issue {
        element =
            element.child(caption(issue.detail.clone(), theme.foreground));
    }

    let run = match proposal.operations.as_slice() {
        [] => format!(
            "Run {}",
            proposal
                .owner
                .map_or("owner command", |owner| owner.command_label())
        ),
        [only] => {
            format!("Run {}", prepared.operations[*only].owner.command_label())
        }
        many => format!("Run {} owner commands", many.len()),
    };
    let entity = cx.entity().downgrade();
    let choice = button_group(
        SharedString::from(format!("cleanup-decision-{index}")),
        vec![
            ChoiceItem::new("skip", "Skip"),
            ChoiceItem::new("owner", run)
                .disabled(!proposal.allows(Decision::Owner)),
            ChoiceItem::new("delete", "Delete files")
                .disabled(!proposal.allows(Decision::DeleteFiles)),
        ],
        Some(match decision {
            Decision::Skip => 0,
            Decision::Owner => 1,
            Decision::DeleteFiles => 2,
        }),
        move |choice, _, cx| {
            let decision = match choice {
                1 => Decision::Owner,
                2 => Decision::DeleteFiles,
                _ => Decision::Skip,
            };
            let _ = entity.update(cx, |this, cx| {
                this.decide_cleanup(index, decision, cx);
            });
        },
        window,
        cx,
    );
    element = element.child(div().flex().child(choice));

    match (&proposal.delete, &proposal.delete_issue) {
        (Some(disclosure), _) if decision == Decision::DeleteFiles => {
            element = element.child(
                div()
                    .debug_selector(move || format!("cleanup-delete-{index}"))
                    .flex()
                    .flex_col()
                    .gap(space::XS)
                    .p(space::SM)
                    .border_1()
                    .border_color(theme.danger)
                    .child(
                        div()
                            .text_size(text::CAPTION)
                            .text_color(theme.bright)
                            .child(format!(
                                "Delete files directly: {}, {}",
                                app.display_path(&proposal.selected),
                                human_bytes(proposal.bytes)
                            )),
                    )
                    .child(caption(
                        disclosure.description(app.cleanup_files()),
                        theme.foreground,
                    )),
            );
        }
        (None, Some(issue)) => {
            element = element.child(caption(
                format!("Delete files unavailable: {}", issue.description()),
                theme.secondary,
            ));
        }
        _ => {}
    }
    Some(element.into_any_element())
}

/// The summary's cleanup section: what Check owners does, the counts of
/// what was chosen, and how Delete files removes.
pub fn summary(
    app: &Disktree,
    theme: &Theme,
    window: &mut Window,
    cx: &mut Context<'_, Disktree>,
) -> Div {
    let mut section = div().flex().flex_col().gap(space::MD);
    let preparing = app.cleanup_preparing();
    let ready = app.cleanup_ready();
    let check = button(
        "check-owners",
        if preparing {
            "Checking owners\u{2026}"
        } else if ready.is_ok() {
            "Check owners again"
        } else {
            "Check owners"
        },
        ButtonVariant::Outline,
        cx,
    )
    .debug_selector(|| "check-owners".into())
    .semantic_disabled(preparing)
    .on_click(cx.listener(|this, _, _, cx| this.prepare_cleanup(cx)));
    section = section.child(check);
    let prepared = match ready {
        Ok(prepared) => prepared,
        Err(reason) => {
            if !preparing && app.cleanup_review.is_some() {
                section = section.child(caption(reason, theme.warning));
            }
            return section;
        }
    };
    let decisions: Vec<_> = (0..prepared.proposals.len())
        .map(|index| app.cleanup_decision(index))
        .collect();
    let count = |wanted: Decision| {
        decisions
            .iter()
            .filter(|decision| **decision == wanted)
            .count()
    };
    section = section
        .child(widgets::row(
            "Owner commands",
            count(Decision::Owner).to_string(),
            cx,
        ))
        .child(widgets::row(
            "Delete files",
            count(Decision::DeleteFiles).to_string(),
            cx,
        ))
        .child(widgets::row(
            "Skipped",
            count(Decision::Skip).to_string(),
            cx,
        ));
    if count(Decision::DeleteFiles) > 0 {
        let entity = cx.entity().downgrade();
        let files = button_group(
            "cleanup-files-mode",
            vec![
                ChoiceItem::new("trash", "Trash")
                    .disabled(!app.trash_backend.is_available()),
                ChoiceItem::new("permanent", "Permanently"),
            ],
            Some(usize::from(app.cleanup_files() == Action::Permanent)),
            move |index, _, cx| {
                let _ = entity.update(cx, |this, cx| {
                    this.set_cleanup_files(
                        if index == 0 {
                            Action::Trash
                        } else {
                            Action::Permanent
                        },
                        cx,
                    );
                });
            },
            window,
            cx,
        );
        section = section
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::XS)
                    .child(caption("Delete files by", theme.secondary))
                    .child(div().flex().child(files)),
            )
            .child(caption(
                disktree_core::cleanup::files_recoverability(
                    app.cleanup_files(),
                ),
                theme.secondary,
            ));
    }
    if let Err(reason) = app.cleanup_approved()
        && decisions.iter().any(|decision| *decision != Decision::Skip)
    {
        section = section.child(caption(reason, theme.warning));
    }
    section
}

/// What Review's mode explains for a cache cleanup.
pub const EXPLANATION: &str = "Check owners finds each marked path's \
    installed owner, version and configured cache without changing \
    anything. Then choose for each path: Skip, the owner's exact command, \
    or Delete files. Nothing is chosen for you, and a failed owner command \
    never deletes files instead. Listed allocation is an upper bound; \
    freed space is measured afterwards, and cleanup rescans.";
