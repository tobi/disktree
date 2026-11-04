//! Explicit, single-use MCP removal intents. UI automation is never approval.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use disktree_core::removal::{Target, detect_trash_backend};
use disktree_core::tree::Tree;
use disktree_link::{
    ConfirmedRemoval, RemovalChoice, RemovalPreview, Response,
};
use gpui_kit::Context;

use crate::state::{Disktree, Screen};

const LIFETIME: Duration = Duration::from_mins(5);
static NEXT_PREVIEW: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct PreparedRemoval {
    preview: RemovalPreview,
    marks: Vec<Target>,
    tree: Weak<Tree>,
    created: Instant,
}

impl PreparedRemoval {
    fn accepts(&self, request: &ConfirmedRemoval) -> bool {
        let preview = &self.preview;
        self.created.elapsed() <= LIFETIME
            && request.preview_id == preview.preview_id
            && request.mode == preview.mode
            && request.paths == preview.paths
            && request.confirmation == preview.mode.confirmation()
    }
}

impl Disktree {
    /// Associate a native UI run with a preview only for the same selection,
    /// mode and scan. This records an outcome; it never authorizes a run.
    pub(crate) fn track_agent_removal(&mut self) {
        let matches = self
            .agent
            .as_ref()
            .and_then(|a| a.removal_preview.as_ref())
            .is_some_and(|prepared| {
                prepared.preview.root == self.root_path
                    && prepared.preview.mode.mode() == self.removal_mode
                    && prepared.marks == self.marks.items()
                    && self.tree.as_ref().is_some_and(|tree| {
                        Weak::ptr_eq(&Arc::downgrade(tree), &prepared.tree)
                    })
                    && prepared.preview.paths.iter().eq(self
                        .run_list
                        .items
                        .iter()
                        .map(|item| &item.target.path))
            });
        if matches && let Some(agent) = &mut self.agent {
            let prepared =
                agent.removal_preview.take().expect("matched preview");
            agent.removal_operation = Some(disktree_link::RemovalOperation {
                preview_id: prepared.preview.preview_id,
                mode: prepared.preview.mode,
                paths: prepared.preview.paths,
                finished: false,
                removed: 0,
                failed: 0,
                pending: self.run_list.items.len(),
            });
            agent.removal_operation_epoch = self.run_epoch;
        }
    }

    pub(crate) fn update_agent_removal(&mut self, finished: bool) {
        if let Some(agent) = &mut self.agent
            && agent.removal_operation_epoch == self.run_epoch
            && let Some(operation) = &mut agent.removal_operation
        {
            let totals = self.run_list.totals();
            operation.finished = finished;
            operation.removed = totals.removed;
            operation.failed = totals.failed_all();
            operation.pending = totals.total - totals.removed - totals.failed;
        }
    }

    pub(crate) fn agent_prepare_removal(
        &mut self,
        mode: RemovalChoice,
    ) -> Response {
        // A new preview revokes the previous one even if preparation fails.
        if let Some(agent) = &mut self.agent {
            agent.removal_preview = None;
        }
        match self.prepare_removal_intent(mode) {
            Ok(prepared) => {
                let preview = prepared.preview.clone();
                let Some(agent) = &mut self.agent else {
                    return Response::error("No agent link is attached");
                };
                agent.removal_preview = Some(prepared);
                Response::RemovalPreview(preview)
            }
            Err(reason) => Response::error(reason),
        }
    }

    fn prepare_removal_intent(
        &self,
        mode: RemovalChoice,
    ) -> Result<PreparedRemoval, String> {
        if self.remote.is_some()
            || self.scan.is_some()
            || self.screen == Screen::Running
            || self.restarting
        {
            return Err("Removal requires a complete, idle local scan".into());
        }
        if mode == RemovalChoice::Trash
            && !detect_trash_backend().is_available()
        {
            return Err("Trash is unavailable; permanent deletion requires a new preview and separate user confirmation".into());
        }
        let tree = self.tree.as_ref().ok_or("No complete scan is available")?;
        let plan = self.plan();
        if !plan.blocked.is_empty() {
            return Err(format!(
                "Removal guards refuse: {}",
                plan.blocked
                    .iter()
                    .map(|blocked| format!(
                        "{}: {}",
                        blocked.path.display(),
                        blocked.reason
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        if plan.is_empty() {
            return Err("Nothing is marked".into());
        }
        let limit = crate::safety::load(self.removal_settings_dir.as_deref())?;
        let volumes = limit.check(&plan.targets)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        Ok(PreparedRemoval {
            preview: RemovalPreview {
                preview_id: format!(
                    "{}-{nonce:x}-{}",
                    std::process::id(),
                    NEXT_PREVIEW.fetch_add(1, Ordering::Relaxed)
                ),
                root: self.root_path.clone(),
                mode,
                paths: plan
                    .targets
                    .iter()
                    .map(|target| target.path.clone())
                    .collect(),
                bytes: plan.bytes(),
                estimate: self.agent_selection_estimate(&plan),
                limit_percent: limit.percent(),
                volumes,
                expires_in_seconds: LIFETIME.as_secs(),
            },
            marks: self.marks.items().to_vec(),
            tree: Arc::downgrade(tree),
            created: Instant::now(),
        })
    }

    pub(crate) fn agent_remove(
        &mut self,
        request: &ConfirmedRemoval,
        cx: &mut Context<'_, Self>,
    ) -> Response {
        if self
            .agent
            .as_ref()
            .and_then(|a| a.removal_operation.as_ref())
            .is_some_and(|operation| {
                operation.preview_id == request.preview_id
                    && operation.mode == request.mode
                    && operation.paths == request.paths
            })
        {
            return Response::State(self.agent_state());
        }
        // Consume before validation: failures and retries need a new preview.
        let Some(prepared) = self
            .agent
            .as_mut()
            .and_then(|agent| agent.removal_preview.take())
        else {
            return Response::error(
                "No unused removal preview; prepare a new preview and obtain user confirmation",
            );
        };
        let preview = &prepared.preview;
        if !prepared.accepts(request) {
            return Response::error(
                "Removal confirmation does not match the unexpired preview; obtain a new preview and explicit user confirmation",
            );
        }
        if self.root_path != preview.root
            || self.marks.items() != prepared.marks
            || self.tree.as_ref().is_none_or(|tree| {
                !Weak::ptr_eq(&Arc::downgrade(tree), &prepared.tree)
            })
        {
            return Response::error(
                "Scan or marked paths changed after preview; obtain a new preview and user confirmation",
            );
        }
        let fresh = match self.prepare_removal_intent(request.mode) {
            Ok(fresh) => fresh,
            Err(reason) => return Response::error(reason),
        };
        if fresh.preview.limit_percent != preview.limit_percent
            || fresh.preview.volumes != preview.volumes
        {
            return Response::error(
                "Removal policy or volume capacity changed; obtain a new preview and user confirmation",
            );
        }
        self.removal_mode = request.mode.mode();
        self.confirm_open = false;
        self.rescan_after_removal = request.rescan;
        // begin_removal associates both UI and MCP runs with this preview.
        if let Some(agent) = &mut self.agent {
            agent.removal_preview = Some(prepared);
        }
        self.begin_removal(cx);
        if self.screen != Screen::Running {
            if let Some(agent) = &mut self.agent {
                agent.removal_preview = None;
            }
            return Response::error(
                "Removal did not start; inspect status before preparing another preview",
            );
        }
        self.note_agent(None, "started confirmed removal".into());
        Response::State(self.agent_state())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_or_different_intents_are_not_accepted() {
        let mut prepared = PreparedRemoval {
            preview: RemovalPreview {
                preview_id: "fixture-id".into(),
                root: "/fixture".into(),
                mode: RemovalChoice::Permanent,
                paths: vec!["/fixture/junk".into()],
                bytes: 123,
                estimate: disktree_link::SelectionEstimate::unknown(
                    &disktree_core::removal::Plan::default(),
                ),
                limit_percent: 5,
                volumes: vec![],
                expires_in_seconds: LIFETIME.as_secs(),
            },
            marks: vec![],
            tree: Weak::new(),
            created: Instant::now(),
        };
        let request = ConfirmedRemoval {
            rescan: false,
            preview_id: "fixture-id".into(),
            mode: RemovalChoice::Permanent,
            paths: vec!["/fixture/junk".into()],
            confirmation: "PERMANENTLY DELETE".into(),
        };
        assert!(prepared.accepts(&request));
        let mut trash = request.clone();
        trash.mode = RemovalChoice::Trash;
        assert!(!prepared.accepts(&trash));
        let mut extra = request.clone();
        extra.paths.push("/fixture/keep".into());
        assert!(!prepared.accepts(&extra));
        let mut no_confirmation = request.clone();
        no_confirmation.confirmation.clear();
        assert!(!prepared.accepts(&no_confirmation));
        prepared.created = Instant::now()
            .checked_sub(LIFETIME + Duration::from_secs(1))
            .expect("expiration before now");
        assert!(!prepared.accepts(&request));
    }
}
