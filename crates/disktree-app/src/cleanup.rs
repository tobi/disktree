//! Cache cleanup belongs to the selected scan and marks. Owners are
//! resolved off the UI thread, and preparing never runs or deletes
//! anything. The person decides each marked path: Skip (the default), the
//! owner's exact command, or deleting the files, which needs its own tick.

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Instant;

use disktree_core::cleanup::{Action, Approved, Decision, Prepared};
use disktree_core::removal::Target;
use disktree_core::tree::Tree;
use gpui_kit::Context;
use gpui_omarchy::Status;

use crate::state::{Disktree, Screen};

#[derive(Debug)]
pub struct Review {
    root: PathBuf,
    marks: Vec<Target>,
    tree: Weak<Tree>,
    created: Instant,
    result: Option<Result<Prepared, String>>,
    /// One per proposal, in its order. Skip until the person chooses.
    decisions: Vec<Decision>,
    /// How Delete files removes: Trash or Permanent.
    files: Action,
    /// The confirmation's separate consent to deleting files directly.
    files_ticked: bool,
    /// The decisions turned into steps, kept until they change: Review
    /// reads it several times a frame, and building it plans the steps
    /// against the mount table each time.
    approved: Option<Result<Approved, String>>,
}

impl Review {
    fn approve(&mut self) {
        self.approved = match &self.result {
            Some(Ok(prepared)) => {
                Some(prepared.approve(&self.decisions, self.files))
            }
            _ => None,
        };
    }

    /// No age limit: a person may take their time deciding, and an
    /// agent's preview carries its own lifetime. What could go stale is
    /// checked again on the worker right before each step: the owner's
    /// executable, version, configured root and scope, the guards, the
    /// capacity limit and whether the path is in use.
    fn matches(&self, app: &Disktree) -> bool {
        self.root == app.root_path
            && self.marks == app.marks.items()
            && app.remote.is_none()
            && app.scan.is_none()
            && app.tree.as_ref().is_some_and(|tree| {
                Weak::ptr_eq(&self.tree, &Arc::downgrade(tree))
            })
    }
}

impl Disktree {
    fn cleanup_review(&self) -> Result<&Review, String> {
        self.cleanup_review
            .as_ref()
            .filter(|review| review.matches(self))
            .ok_or_else(|| {
                "Check owners for this scan and selection first".to_string()
            })
    }

    /// The read-only proposals for this exact scan and selection.
    pub(crate) fn cleanup_ready(&self) -> Result<&Prepared, String> {
        self.cleanup_review()?
            .result
            .as_ref()
            .ok_or(
                "Owner check is preparing; retry the read-only preview shortly",
            )?
            .as_ref()
            .map_err(Clone::clone)
    }

    /// A finished check that failed as a whole, such as an environment
    /// that cannot be captured. Per-path failures are proposals instead.
    pub(crate) fn cleanup_failed(&self) -> Option<String> {
        match &self.cleanup_review().ok()?.result {
            Some(Err(reason)) => Some(reason.clone()),
            _ => None,
        }
    }

    pub(crate) fn cleanup_preparing(&self) -> bool {
        self.cleanup_review()
            .is_ok_and(|review| review.result.is_none())
    }

    /// The person's decisions in the window, as steps to run.
    pub(crate) fn cleanup_approved(&self) -> Result<&Approved, String> {
        self.cleanup_ready()?;
        self.cleanup_review()?
            .approved
            .as_ref()
            .ok_or("Choose an action for every marked path")?
            .as_ref()
            .map_err(Clone::clone)
    }

    pub(crate) fn cleanup_decision(&self, index: usize) -> Decision {
        self.cleanup_review().map_or(Decision::Skip, |review| {
            review.decisions.get(index).copied().unwrap_or_default()
        })
    }

    pub(crate) fn cleanup_files(&self) -> Action {
        self.cleanup_review()
            .map_or_else(|_| self.default_files(), |review| review.files)
    }

    pub(crate) fn cleanup_files_ticked(&self) -> bool {
        self.cleanup_review()
            .is_ok_and(|review| review.files_ticked)
    }

    /// Trash when it exists, as everywhere else in PwrDisk: the safer
    /// default, even though a cache's space returns only once emptied.
    const fn default_files(&self) -> Action {
        if self.trash_backend.is_available() {
            Action::Trash
        } else {
            Action::Permanent
        }
    }

    pub(crate) fn decide_cleanup(
        &mut self,
        index: usize,
        decision: Decision,
        cx: &mut Context<'_, Self>,
    ) {
        let allowed = self.cleanup_ready().is_ok_and(|prepared| {
            prepared
                .proposals
                .get(index)
                .is_some_and(|proposal| proposal.allows(decision))
        });
        if !allowed {
            return;
        }
        if let Some(review) = &mut self.cleanup_review
            && let Some(slot) = review.decisions.get_mut(index)
        {
            *slot = decision;
            // A change to what is deleted asks for the tick again.
            review.files_ticked = false;
            review.approve();
        }
        self.confirm_open = false;
        cx.notify();
    }

    pub(crate) fn set_cleanup_files(
        &mut self,
        files: Action,
        cx: &mut Context<'_, Self>,
    ) {
        if files == Action::Trash && !self.trash_backend.is_available() {
            return;
        }
        if let Some(review) = &mut self.cleanup_review {
            review.files = files;
            review.files_ticked = false;
            review.approve();
        }
        self.confirm_open = false;
        cx.notify();
    }

    pub(crate) fn tick_cleanup_files(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(review) = &mut self.cleanup_review {
            review.files_ticked = !review.files_ticked;
        }
        cx.notify();
    }

    /// Every proposal, or why there are none yet.
    pub(crate) fn cleanup_description(&self) -> String {
        match self.cleanup_ready() {
            Ok(prepared) => prepared.description(),
            Err(reason) => reason,
        }
    }

    pub(crate) fn prepare_cleanup(&mut self, cx: &mut Context<'_, Self>) {
        if self.remote.is_some()
            || self.scan.is_some()
            || self.screen == Screen::Running
            || self.marks.is_empty()
        {
            self.say(
                "Cache cleanup needs a complete, idle local scan and marked paths",
                Status::Warning,
                cx,
            );
            cx.notify();
            return;
        }
        // Do not start another probe while this exact request is pending.
        if self.cleanup_preparing() {
            return;
        }
        let Some(tree) = &self.tree else {
            return;
        };
        let root = self.root_path.clone();
        let marks = self.marks.items().to_vec();
        let tree = Arc::downgrade(tree);
        let created = Instant::now();
        self.cleanup_review = Some(Review {
            root: root.clone(),
            marks: marks.clone(),
            tree,
            created,
            result: None,
            decisions: vec![Decision::Skip; marks.len()],
            files: self.default_files(),
            files_ticked: false,
            approved: None,
        });
        self.confirm_open = false;
        let path = self.cleanup_path.clone();
        let task = cx.background_executor().spawn(async move {
            match path {
                Some(path) => disktree_core::cleanup::prepare_on_path(
                    &marks, &root, &path,
                ),
                None => disktree_core::cleanup::prepare(&marks, &root),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(review) = &mut this.cleanup_review
                    && review.created == created
                {
                    if let Ok(prepared) = &result {
                        review.decisions =
                            vec![Decision::Skip; prepared.proposals.len()];
                    }
                    review.result = Some(result);
                    review.approve();
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
