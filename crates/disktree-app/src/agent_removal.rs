//! Explicit, single-use MCP removal intents. UI automation is never approval.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use disktree_core::removal::{Plan, Target, detect_trash_backend};
use disktree_core::tree::Tree;
use disktree_link::{
    ConfirmedRemoval, PathDecision, RemovalChoice, RemovalPreview, Response,
};
use gpui_kit::Context;

use crate::state::{Disktree, Screen};

const LIFETIME: Duration = Duration::from_mins(5);
static NEXT_PREVIEW: AtomicU64 = AtomicU64::new(0);

/// A bounded recursive inspection can outlast a link request. Its result
/// belongs to this exact scan/selection, never to a later set of marks.
#[derive(Debug)]
pub struct RawInspection {
    root: std::path::PathBuf,
    marks: Vec<Target>,
    targets: Vec<Target>,
    tree: Weak<Tree>,
    created: Instant,
    result: Option<Result<(), String>>,
}

impl RawInspection {
    fn matches(&self, app: &Disktree, plan: &Plan) -> bool {
        self.root == app.root_path
            && self.marks == app.marks.items()
            && self.targets == plan.targets
            && self.created.elapsed() < LIFETIME
            && app.tree.as_ref().is_some_and(|tree| {
                Weak::ptr_eq(&self.tree, &Arc::downgrade(tree))
            })
    }
}

#[derive(Debug)]
pub struct PreparedRemoval {
    preview: RemovalPreview,
    /// A cache cleanup's confirmed decisions, run as they were previewed.
    cleanup: Option<disktree_core::cleanup::Approved>,
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
            && !preview.confirmation.is_empty()
            && request.confirmation == preview.confirmation
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
        decisions: Option<&[PathDecision]>,
        files: Option<RemovalChoice>,
        cx: &mut Context<'_, Self>,
    ) -> Response {
        // A new preview revokes the previous one even if preparation fails.
        if let Some(agent) = &mut self.agent {
            agent.removal_preview = None;
        }
        if mode == RemovalChoice::OwnerCleanup && self.cleanup_ready().is_err()
        {
            // A failed check says why once; the next request checks again,
            // rather than reporting "preparing" for good.
            if let Some(reason) = self.cleanup_failed() {
                self.cleanup_review = None;
                return Response::error(reason);
            }
            if !self.cleanup_preparing() {
                self.prepare_cleanup(cx);
            }
            return Response::error(self.cleanup_description());
        }
        if mode != RemovalChoice::OwnerCleanup {
            self.prepare_raw_inspection(cx);
        }
        match self.prepare_removal_intent(mode, decisions, files) {
            Ok(prepared) => {
                let preview = prepared.preview.clone();
                let Some(agent) = &mut self.agent else {
                    return Response::error("No agent link is attached");
                };
                // Proposals without decisions inform; they cannot be
                // confirmed, so nothing is kept to confirm against.
                if !preview.confirmation.is_empty() {
                    agent.removal_preview = Some(prepared);
                }
                Response::RemovalPreview(preview)
            }
            Err(reason) => Response::error(reason),
        }
    }

    fn prepare_raw_inspection(&mut self, cx: &Context<'_, Self>) {
        if self.remote.is_some()
            || self.scan.is_some()
            || self.screen == Screen::Running
            || self.restarting
            || self.marks.is_empty()
        {
            return;
        }
        let Some(tree) = &self.tree else {
            return;
        };
        let plan =
            disktree_core::removal::plan(self.marks.items(), &self.root_path);
        if !plan.blocked.is_empty() || plan.is_empty() {
            return;
        }
        if self
            .agent
            .as_ref()
            .and_then(|agent| agent.raw_inspection.as_ref())
            .is_some_and(|inspection| inspection.matches(self, &plan))
        {
            return;
        }
        let Some(agent) = &mut self.agent else {
            return;
        };
        let created = Instant::now();
        agent.raw_inspection = Some(RawInspection {
            root: self.root_path.clone(),
            marks: self.marks.items().to_vec(),
            targets: plan.targets.clone(),
            tree: Arc::downgrade(tree),
            created,
            result: None,
        });
        let task = cx.background_executor().spawn(async move {
            disktree_core::cleanup::check_raw(&plan.targets)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(inspection) = this
                    .agent
                    .as_mut()
                    .and_then(|agent| agent.raw_inspection.as_mut())
                    && inspection.created == created
                {
                    inspection.result = Some(result);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn raw_inspection_ready(&self, plan: &Plan) -> Result<(), String> {
        let inspection = self
            .agent
            .as_ref()
            .and_then(|agent| agent.raw_inspection.as_ref())
            .filter(|inspection| inspection.matches(self, plan))
            .ok_or(
                "Prepare a new raw-removal preview for this scan and selection",
            )?;
        inspection.result.as_ref()
            .ok_or("Raw removal inspection is preparing; retry only the read-only preview shortly")?
            .clone()
    }

    fn prepare_removal_intent(
        &self,
        mode: RemovalChoice,
        decisions: Option<&[PathDecision]>,
        files: Option<RemovalChoice>,
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
        if mode == RemovalChoice::OwnerCleanup {
            return self.prepare_cleanup_intent(decisions, files, tree);
        }
        if decisions.is_some() || files.is_some() {
            return Err("Decisions apply only to owner_cleanup".into());
        }
        let plan = {
            // Do not let a previously selected UI mode change the meaning
            // of an MCP raw-removal request.
            disktree_core::removal::plan(self.marks.items(), &self.root_path)
        };
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
        self.raw_inspection_ready(&plan)?;
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
                cleanup: vec![],
                proposals: vec![],
                decisions: vec![],
                files: None,
                confirmation: mode.confirmation().into(),
                consent: String::new(),
            },
            cleanup: None,
            marks: self.marks.items().to_vec(),
            tree: Arc::downgrade(tree),
            created: Instant::now(),
        })
    }

    /// A cache cleanup preview. Without decisions it lists what each path
    /// offers and cannot be confirmed; with one decision per marked path it
    /// is a single-use preview of exactly those steps and their phrase.
    fn prepare_cleanup_intent(
        &self,
        decisions: Option<&[PathDecision]>,
        files: Option<RemovalChoice>,
        tree: &Arc<Tree>,
    ) -> Result<PreparedRemoval, String> {
        use disktree_core::cleanup::Action;
        let prepared = self.cleanup_ready()?;
        let limit = crate::safety::load(self.removal_settings_dir.as_deref())?;
        let files_action = match files {
            None => Action::Skip,
            Some(RemovalChoice::Trash) => {
                if !detect_trash_backend().is_available() {
                    return Err("Trash is unavailable; choose permanent deletion for Delete files, which needs its own confirmation".into());
                }
                Action::Trash
            }
            Some(RemovalChoice::Permanent) => Action::Permanent,
            Some(RemovalChoice::OwnerCleanup) => {
                return Err("Delete files uses trash or permanent".into());
            }
        };
        let approved = match decisions {
            None => None,
            Some(named) => {
                let named: Vec<_> = named
                    .iter()
                    .map(|decision| (decision.path.clone(), decision.action))
                    .collect();
                let decisions = prepared.decisions_for(&named)?;
                Some(prepared.approve(&decisions, files_action)?)
            }
        };
        let (plan, volumes) = match &approved {
            Some(approved) => {
                (approved.plan.clone(), limit.check(&approved.plan.targets)?)
            }
            None => (disktree_core::removal::Plan::default(), vec![]),
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let preview = RemovalPreview {
            preview_id: if approved.is_some() {
                format!(
                    "{}-{nonce:x}-{}",
                    std::process::id(),
                    NEXT_PREVIEW.fetch_add(1, Ordering::Relaxed)
                )
            } else {
                String::new()
            },
            root: self.root_path.clone(),
            mode: RemovalChoice::OwnerCleanup,
            paths: plan
                .targets
                .iter()
                .map(|target| target.path.clone())
                .collect(),
            bytes: plan.bytes(),
            estimate: disktree_link::SelectionEstimate::unknown(&plan),
            limit_percent: limit.percent(),
            volumes,
            expires_in_seconds: LIFETIME.as_secs(),
            cleanup: approved.as_ref().map_or_else(Vec::new, |approved| {
                approved
                    .steps
                    .iter()
                    .filter_map(|step| match step {
                        disktree_core::cleanup::Step::Owner(index) => {
                            approved.prepared.operations.get(*index).cloned()
                        }
                        disktree_core::cleanup::Step::Files { .. } => None,
                    })
                    .collect()
            }),
            proposals: prepared.proposals.clone(),
            decisions: approved
                .as_ref()
                .map_or_else(Vec::new, |approved| approved.decisions.clone()),
            files: approved
                .as_ref()
                .filter(|approved| approved.file_steps() > 0)
                .and(files),
            confirmation: approved
                .as_ref()
                .map_or("", |approved| approved.confirmation())
                .into(),
            consent: approved.as_ref().map_or_else(
                || prepared.description(),
                disktree_core::cleanup::Approved::description,
            ),
        };
        Ok(PreparedRemoval {
            preview,
            cleanup: approved,
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
        let decisions: Vec<PathDecision> = preview
            .proposals
            .iter()
            .zip(&preview.decisions)
            .map(|(proposal, action)| PathDecision {
                path: proposal.selected.clone(),
                action: *action,
            })
            .collect();
        let fresh = match self.prepare_removal_intent(
            request.mode,
            (request.mode == RemovalChoice::OwnerCleanup)
                .then_some(decisions.as_slice()),
            preview.files,
        ) {
            Ok(fresh) => fresh,
            Err(reason) => return Response::error(reason),
        };
        // Confirmation uses the prepared read-only result. The removal
        // worker independently inspects again before any filesystem write.
        // No later preview may reuse this operation's inspection.
        if let Some(agent) = &mut self.agent {
            agent.raw_inspection = None;
        }
        if fresh.preview.limit_percent != preview.limit_percent
            || fresh.preview.volumes != preview.volumes
            || fresh.preview.cleanup != preview.cleanup
            || fresh.preview.paths != preview.paths
            || fresh.preview.decisions != preview.decisions
            || fresh.preview.confirmation != preview.confirmation
        {
            return Response::error(
                "Removal policy or volume capacity changed; obtain a new preview and user confirmation",
            );
        }
        self.removal_mode = request.mode.mode();
        self.confirm_open = false;
        self.rescan_after_removal = request.rescan;
        let cleanup = prepared.cleanup.clone();
        // Starting a run associates both UI and MCP runs with this preview.
        if let Some(agent) = &mut self.agent {
            agent.removal_preview = Some(prepared);
        }
        // Held to the agent limit, not the window's: with the window's Off
        // the preview was checked at the agent limit, and growth since must
        // be refused at the same limit.
        self.begin_agent_removal(cleanup, cx);
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
                cleanup: vec![],
                proposals: vec![],
                decisions: vec![],
                files: None,
                confirmation: RemovalChoice::Permanent.confirmation().into(),
                consent: String::new(),
            },
            cleanup: None,
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
