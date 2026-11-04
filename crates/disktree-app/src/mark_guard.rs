//! What may be marked, and what Review asks before a removal past the
//! limit.
//!
//! The guards in `removal.rs` decide what may be removed; asking them only
//! in Review let a filled Mark for removal sit on `/Users`. So a mark is
//! judged when it is made: a protected path is never marked, and a mark
//! that takes a volume past Settings › Safety's limit needs Mark anyway
//! here and a second confirmation in Review.

use std::path::{Path, PathBuf};

use disktree_core::removal::{Plan, Target};
use disktree_core::removal_limit::Limit;
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::{App, AppContext as _, Context, Focusable as _, Window};
use gpui_omarchy::Status;

use crate::safety::{Confirm, Policy, matches, phrase, share_percent};
use crate::state::Disktree;

/// Whether a new mark may be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkCheck {
    Allowed,
    /// The guards refuse it, with their reason. No setting lifts this.
    Protected(String),
    /// Marking it takes the volume's marked total past the limit.
    OverLimit(Share),
}

/// Marked bytes as a share of the scanned volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share {
    pub bytes: u64,
    pub capacity: u64,
    /// Rounded up; see [`share_percent`].
    pub percent: u8,
    pub limit: u8,
}

impl Share {
    fn over(bytes: u64, capacity: u64, limit: Limit) -> Option<Self> {
        // `bytes > floor(capacity * limit / 100)`, as the worker's check
        // counts it, without the rounding.
        (u128::from(bytes) * 100
            > u128::from(capacity) * u128::from(limit.percent()))
        .then(|| Self {
            bytes,
            capacity,
            percent: share_percent(bytes, capacity),
            limit: limit.percent(),
        })
    }
}

/// One path's answer from the guards, kept while the inspector stays on
/// it: judging reads the filesystem, and the inspector draws every frame.
pub type RefusalCache = Option<(PathBuf, PathBuf, Option<String>)>;

impl Disktree {
    /// Settings › Safety as last read. Read once and again before every
    /// removal, never per frame; Settings saves it, and an unreadable file
    /// is the default here, which is the stricter reading.
    pub fn policy(&self) -> Policy {
        *self.safety_policy.borrow_mut().get_or_insert_with(|| {
            crate::safety::load_policy(self.removal_settings_dir.as_deref())
                .unwrap_or_default()
        })
    }

    /// Read Settings › Safety again, as a removal must.
    pub fn reload_policy(&self) -> Result<Policy, String> {
        let policy =
            crate::safety::load_policy(self.removal_settings_dir.as_deref());
        *self.safety_policy.borrow_mut() =
            Some(policy.clone().unwrap_or_default());
        policy
    }

    /// Why the guards would refuse `path`. An SSH host's paths are never
    /// judged against this computer's filesystem: nothing is removed
    /// there, and its marks only export a list.
    pub fn refusal_of(&self, path: &Path) -> Option<String> {
        if self.remote.is_some() {
            return None;
        }
        if let Some((root, cached, reason)) = &*self.refusal_cache.borrow()
            && *root == self.root_path
            && cached == path
        {
            return reason.clone();
        }
        let reason = disktree_core::removal::refusal(path, &self.root_path);
        *self.refusal_cache.borrow_mut() =
            Some((self.root_path.clone(), path.to_path_buf(), reason.clone()));
        reason
    }

    /// The marked total with `target` marked too: marks inside it are
    /// absorbed by it, so they are not counted twice.
    fn marked_with(&self, target: &Target) -> u64 {
        let style = self.path_style();
        self.marks
            .items()
            .iter()
            .filter(|mark| {
                mark.path != target.path
                    && !style.is_below(&mark.path, &target.path)
            })
            .map(|mark| mark.bytes)
            .fold(target.bytes, u64::saturating_add)
    }

    /// Whether marking `target` passes `limit` on the scanned volume.
    /// Every mark counts against it, wherever it lives: the worker checks
    /// each volume again, and over-counting only asks sooner.
    fn over_limit(&self, target: &Target, limit: Limit) -> Option<Share> {
        Share::over(self.marked_with(target), self.capacity()?, limit)
    }

    /// The scanned volume's capacity: the ticker's, or measured now when
    /// it has no reading yet, so a mark made before the first reading is
    /// still judged.
    fn capacity(&self) -> Option<u64> {
        self.space
            .or_else(|| disktree_core::space::space_info(&self.root_path).ok())
            .map(|space| space.total)
    }

    /// Whether `target`, not yet marked, may be marked now.
    pub fn mark_check(&self, target: &Target) -> MarkCheck {
        if let Some(reason) = self.refusal_of(&target.path) {
            return MarkCheck::Protected(reason);
        }
        if self.remote.is_some() {
            return MarkCheck::Allowed;
        }
        self.policy()
            .limit
            .and_then(|limit| self.over_limit(target, limit))
            .map_or(MarkCheck::Allowed, MarkCheck::OverLimit)
    }

    /// Why an agent may not mark `target`, if it may not. An agent is held
    /// to the limit with no way past it, and to the default one when the
    /// window's is off: going past it is the person's call, in the window.
    pub fn agent_mark_refusal(&self, target: &Target) -> Option<String> {
        if let Some(reason) = self.refusal_of(&target.path) {
            return Some(reason);
        }
        let limit = self.policy().agent_limit();
        self.over_limit(target, limit).map(|share| {
            format!(
                "marking it would put {}% of the volume in the list, over \
                 the {}% limit; only the person can mark past it, in the \
                 window",
                share.percent, share.limit
            )
        })
    }

    /// The notice for a mark refused here; `None` when it was allowed.
    pub fn mark_refusal_notice(
        &self,
        target: &Target,
        check: &MarkCheck,
    ) -> Option<String> {
        let name = self.display_path(&target.path);
        match check {
            MarkCheck::Allowed => None,
            MarkCheck::Protected(reason) => Some(format!("{name}: {reason}")),
            MarkCheck::OverLimit(share) => Some(format!(
                "{name} would make the marked list {}% of this disk, over \
                 the {}% limit; use \u{25be} beside Mark to mark it anyway",
                share.percent, share.limit
            )),
        }
    }

    /// The inspector's Mark anyway: the one way to mark past the limit.
    /// Protected paths stay refused.
    pub fn mark_anyway(
        &mut self,
        crumbs: &[usize],
        cx: &mut Context<'_, Self>,
    ) {
        self.mark_menu = None;
        self.toggle_mark_checked(crumbs, true, cx);
    }

    /// Marked bytes against the scanned volume when they pass the window's
    /// limit, for Review's second confirmation. Every target counts, as at
    /// marking. `None` with the limit off, over SSH, or with no capacity
    /// known, where the worker's own check still stands.
    pub fn removal_gate(&self, plan: &Plan) -> Option<Share> {
        if self.remote.is_some() {
            return None;
        }
        let limit = self.policy().limit?;
        Share::over(plan.bytes(), self.capacity()?, limit)
    }

    /// The phrase Review asks for `share`, in the current mode.
    pub fn gate_phrase(&self, share: &Share) -> String {
        phrase(share.percent, self.removal_mode)
    }

    /// Whether the person has confirmed `share` as Settings asks.
    pub fn gate_met(&self, share: &Share, cx: &App) -> bool {
        match self.policy().confirm {
            Confirm::Nothing => true,
            Confirm::Tick => {
                self.over_ticked == Some((share.percent, self.removal_mode))
            }
            Confirm::Phrase => self.over_phrase.as_ref().is_some_and(|input| {
                matches(&input.read(cx).value(), &self.gate_phrase(share))
            }),
        }
    }

    /// The limit the worker holds this removal to: what the person
    /// confirmed when past the window's limit, so a share that grew since
    /// is refused; otherwise the window's. Err with the notice to show.
    pub fn removal_limit(
        &self,
        plan: &Plan,
        cx: &App,
    ) -> Result<Limit, String> {
        let policy = self.reload_policy()?;
        let limit = match self.removal_gate(plan) {
            Some(share) if !self.gate_met(&share, cx) => {
                return Err(match policy.confirm {
                    Confirm::Tick => format!(
                        "this removes {}% of the disk; tick the box to go \
                         past the {}% limit",
                        share.percent, share.limit
                    ),
                    _ => format!(
                        "this removes {}% of the disk; type \u{201c}{}\
                         \u{201d} to go past the {}% limit",
                        share.percent,
                        self.gate_phrase(&share),
                        share.limit
                    ),
                });
            }
            Some(share) => Limit::new(share.percent.max(1))?,
            // Off still counts against a capacity, so an unknown one
            // fails closed; it just never says no for size.
            None => policy.limit.map_or_else(|| Limit::new(100), Ok)?,
        };
        limit.check(&plan.targets)?;
        Ok(limit)
    }

    /// Make Review's phrase field once there is a window to make it in.
    /// Enter in it commits, as Enter on the list does, once it matches.
    pub fn prepare_over_phrase(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.over_phrase.is_some() {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe(&input, |this, _, event, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.commit(false, cx);
            }
            cx.notify();
        })
        .detach();
        self.over_phrase = Some(input);
    }

    /// Whether the phrase field has the keys: Review's own keys, `?`
    /// included, are letters typed into it then.
    pub fn editing_phrase(&self, window: &Window, cx: &App) -> bool {
        self.over_phrase.as_ref().is_some_and(|input| {
            input.read(cx).focus_handle(cx).is_focused(window)
        })
    }

    /// Show `notice` as a warning.
    pub fn warn(&mut self, notice: String, cx: &mut Context<'_, Self>) {
        self.say(notice, Status::Warning, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_is_over_exactly_where_the_worker_refuses() {
        let five = Limit::default();
        // 5% of 1000 is 50: 50 passes, 51 does not, as `Limit::check`.
        assert_eq!(Share::over(50, 1000, five), None);
        let over = Share::over(51, 1000, five).expect("over");
        assert_eq!((over.percent, over.limit), (6, 5));
        assert_eq!(
            Share::over(u64::MAX, 1, five).map(|s| s.percent),
            Some(100)
        );
    }
}
