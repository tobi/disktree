//! Owner previews belong to the selected scan and marks. Probes and cache
//! measurement run off the UI thread, and preparation never executes cleanup.

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use disktree_core::cleanup::Prepared;
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
}

impl Review {
    fn matches(&self, app: &Disktree) -> bool {
        self.root == app.root_path
            && self.marks == app.marks.items()
            && self.created.elapsed() < Duration::from_mins(5)
            && app.remote.is_none()
            && app.scan.is_none()
            && app.tree.as_ref().is_some_and(|tree| {
                Weak::ptr_eq(&self.tree, &Arc::downgrade(tree))
            })
    }
}

impl Disktree {
    pub(crate) fn cleanup_ready(&self) -> Result<&Prepared, String> {
        let review = self.cleanup_review.as_ref().filter(|review| review.matches(self))
            .ok_or("Prepare a new owner cleanup preview for this scan and selection")?;
        review.result.as_ref().ok_or("Owner cleanup preview is preparing; retry the read-only preview shortly")?
            .as_ref().map_err(Clone::clone)
    }

    pub(crate) fn cleanup_description(&self) -> String {
        match self.cleanup_ready() {
            Ok(prepared) => prepared
                .operations
                .iter()
                .map(disktree_core::cleanup::Operation::description)
                .collect::<Vec<_>>()
                .join("\n\n"),
            Err(reason) => reason,
        }
    }

    pub(crate) fn prepare_cleanup(&mut self, cx: &mut Context<'_, Self>) {
        if self.remote.is_some()
            || self.scan.is_some()
            || self.screen == Screen::Running
            || self.marks.is_empty()
        {
            self.say("Owner cleanup needs a complete, idle local scan and selected cache paths", Status::Warning, cx);
            cx.notify();
            return;
        }
        // Do not start another probe while this exact request is pending.
        if self.cleanup_review.as_ref().is_some_and(|review| {
            review.matches(self) && review.result.is_none()
        }) {
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
        });
        let task = cx.background_executor().spawn(async move {
            disktree_core::cleanup::prepare(&marks, &root)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(review) = &mut this.cleanup_review
                    && review.created == created
                {
                    review.result = Some(result);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn take_cleanup(&mut self) -> Result<Prepared, String> {
        self.cleanup_ready()?;
        self.cleanup_review
            .take()
            .and_then(|review| review.result)
            .ok_or_else(|| "Missing owner preview".to_string())?
    }
}
