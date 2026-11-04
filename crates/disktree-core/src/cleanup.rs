//! Cache cleanup, decided item by item.
//!
//! Classification is an inspection hint, never permission to delete. For
//! each marked path the person picks Skip, the owner's exact allowlisted
//! command, or deliberately deleting the files; nothing else ever builds a
//! write, and one never stands in for another. See `docs/owner-cleanup.md`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

use crate::removal::{Plan, Target};

mod process;
mod recognize;
#[cfg(test)]
mod tests;

pub use recognize::inspection_reason;

// Preflight must bound queued paths as well as visited entries so one
// enormous flat directory cannot exhaust memory before the limit is seen.
const MAX_PREFLIGHT_ENTRIES: usize = 2_000_000;

/// Cache classifications never silently reach the recursive remover. Check
/// descendants too: deleting a wrapper must not bypass an owner's policy.
#[allow(
    clippy::unnecessary_debug_formatting,
    reason = "Quote paths and expose controls in safety refusals"
)]
pub fn check_raw(targets: &[Target]) -> Result<(), String> {
    let environment = Environment::current()?;
    for target in targets {
        if let Some(reason) =
            recognize::inspection_with(&target.path, &environment)
        {
            return Err(format!("{:?}: {reason}", target.path));
        }
    }
    let mut pending: Vec<_> =
        targets.iter().map(|target| target.path.clone()).collect();
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        visited += 1;
        if visited > MAX_PREFLIGHT_ENTRIES {
            return Err("Raw removal inspection exceeded its bound; inspect a smaller selection".into());
        }
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        if meta.is_dir() {
            if let Some(reason) =
                recognize::inspection_with(&path, &environment)
            {
                return Err(format!("{path:?}: {reason}"));
            }
            for entry in
                std::fs::read_dir(&path).map_err(|error| error.to_string())?
            {
                if visited + pending.len() >= MAX_PREFLIGHT_ENTRIES {
                    return Err("Raw removal inspection exceeded its bound; inspect a smaller selection".into());
                }
                pending.push(entry.map_err(|error| error.to_string())?.path());
            }
        }
    }
    Ok(())
}

/// Deliberately separate npm's two independently managed caches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    Npm,
    Npx,
    Uv,
    Pip,
    Cypress,
    Pnpm,
    Homebrew,
    Playwright,
    Puppeteer,
}

impl Owner {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Npm => "npm package cache",
            Self::Npx => "npm npx cache",
            Self::Uv => "uv cache",
            Self::Pip => "pip cache",
            Self::Cypress => "Cypress binary cache",
            Self::Pnpm => "pnpm store",
            Self::Homebrew => "Homebrew cache",
            Self::Playwright => "Playwright browser cache",
            Self::Puppeteer => "Puppeteer browser cache",
        }
    }

    const fn executable(self) -> &'static str {
        match self {
            Self::Npm | Self::Npx => "npm",
            Self::Uv => "uv",
            Self::Pip => "pip",
            Self::Cypress => "cypress",
            Self::Pnpm => "pnpm",
            Self::Homebrew => "brew",
            Self::Playwright => "playwright",
            Self::Puppeteer => "puppeteer",
        }
    }

    /// The command a person would recognize, for a button's label.
    pub const fn command_label(self) -> &'static str {
        match self {
            Self::Npm => "npm cache clean",
            Self::Npx => "npm cache npx rm",
            Self::Uv => "uv cache clean",
            Self::Pip => "pip cache purge",
            Self::Cypress => "cypress cache clear",
            Self::Pnpm
            | Self::Homebrew
            | Self::Playwright
            | Self::Puppeteer => "owner command",
        }
    }

    pub const fn unsupported(self) -> Option<&'static str> {
        match self {
            Self::Pnpm => Some(
                "pnpm prune can also change metadata, dlx/runtime caches and global virtual-store state outside the selected store; inspect with pnpm. This scope is not supported.",
            ),
            Self::Homebrew => Some(
                "brew cleanup also removes old installed formula versions and lock files outside the cache; cache-only cleanup is not supported.",
            ),
            Self::Playwright => Some(
                "playwright uninstall changes browser installations and package-reference state; selected-cache-only cleanup is not supported.",
            ),
            Self::Puppeteer => Some(
                "Puppeteer has a browser cleanup API, but its installed API, JavaScript configuration and interactive CLI scope are not resolved by this version of PwrDisk. Inspect with the owning installation.",
            ),
            _ => None,
        }
    }
}

/// The exact invocation shown before consent. No shell, executable or
/// argument can be supplied by an MCP caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: PathBuf,
}

/// One actual scope, which can be wider than the selected subtree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub owner: Owner,
    pub version: String,
    pub selected: Vec<PathBuf>,
    pub cache_root: PathBuf,
    pub scope: PathBuf,
    pub command: Command,
    pub effects: String,
    pub limitations: String,
}

impl Operation {
    /// The command acts on more than was marked: unselected siblings too.
    pub fn wider(&self) -> bool {
        self.selected.iter().any(|selected| *selected != self.scope)
    }

    #[allow(
        clippy::unnecessary_debug_formatting,
        reason = "Consent must expose argument boundaries and control characters"
    )]
    pub fn description(&self) -> String {
        // Debug quoting exposes control characters and argument boundaries.
        let mut text = format!(
            "{} ({})\nSelected: {:?}\nConfigured cache root: {:?}\nActual scope: {:?}\nExecutable: {:?}\nArguments: {:?}\nEnvironment overrides: {:?}\nWorking directory: {:?}\n",
            self.owner.label(),
            self.version,
            self.selected,
            self.cache_root,
            self.scope,
            self.command.executable,
            self.command.arguments,
            self.command.environment,
            self.command.working_directory,
        );
        if self.wider() {
            text.push_str("Wider than selected: the command acts on the whole scope, including unselected siblings.\n");
        }
        text.push_str(&self.effects);
        text.push('\n');
        text.push_str(&self.limitations);
        text.push('\n');
        text.push_str(OWNER_RECOVERABILITY);
        text
    }
}

/// What an owner command leaves to recover from, stated before consent.
pub const OWNER_RECOVERABILITY: &str = "Recoverability: none through PwrDisk or Trash. The owner downloads or rebuilds entries when they are next needed.";

/// Why a proposal or a step could not do what was asked. One vocabulary
/// for the window, the agent and the results, so a failure reads the same
/// wherever it is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Problem {
    /// No executable of that name on PwrDisk's PATH, or it cannot start.
    NotFound,
    /// A shim, proxy or launcher PwrDisk will not invoke.
    UnsupportedLauncher,
    UnsupportedVersion,
    VersionCheckFailed,
    ConfigCheckFailed,
    /// A recognized owner with no audited cache-only command.
    NoCommand,
    /// No supported owner resolves to this path.
    UnknownOwner,
    /// The owner's configured cache is somewhere else.
    NotConfiguredCache,
    /// Removal guards, scope checks or bounds refuse it.
    Refused,
    /// The whole run is past the capacity limit.
    OverLimit,
    /// Owner, version, executable or configured root changed since preview.
    Changed,
    InUse,
    CommandFailed,
    /// Stopped or timed out; it may have done part of its work.
    Cancelled,
    /// Deleting or trashing the files failed.
    RemovalFailed,
}

impl Problem {
    pub const fn label(self) -> &'static str {
        match self {
            Self::NotFound => "not found on PATH",
            Self::UnsupportedLauncher => "unsupported launcher",
            Self::UnsupportedVersion => "unsupported version",
            Self::VersionCheckFailed => "version check failed",
            Self::ConfigCheckFailed => "configuration check failed",
            Self::NoCommand => "no cleanup command",
            Self::UnknownOwner => "unknown owner",
            Self::NotConfiguredCache => "not the configured cache",
            Self::Refused => "refused by guards",
            Self::OverLimit => "over the capacity limit",
            Self::Changed => "changed since preview",
            Self::InUse => "in use",
            Self::CommandFailed => "command failed",
            Self::Cancelled => "cancelled or timed out",
            Self::RemovalFailed => "removal failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub problem: Problem,
    pub detail: String,
}

impl Issue {
    pub fn new(problem: Problem, detail: impl Into<String>) -> Self {
        Self {
            problem,
            detail: detail.into(),
        }
    }

    pub fn description(&self) -> String {
        format!("{}: {}", self.problem.label(), self.detail)
    }
}

/// The person's choice for one marked path. Skip is the default: nothing
/// runs for a path nobody chose an action for.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    #[default]
    Skip,
    /// Run the exact owner command shown for it.
    Owner,
    /// Delete exactly this path directly, without its owner.
    DeleteFiles,
}

/// What a step does, or did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Skip,
    Owner,
    Trash,
    Permanent,
}

impl Action {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Owner => "owner command",
            Self::Trash => "move files to Trash",
            Self::Permanent => "delete files permanently",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    /// Stopped while running; it may have done part of its work.
    Cancelled,
    /// The person chose Skip; nothing ran.
    Skipped,
    /// Stopped before this step started.
    NotReached,
}

/// What Delete files means for one path, said before it is chosen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disclosure {
    pub path: PathBuf,
    /// Recognized caches inside it, whose owners are bypassed as well.
    pub contains: Vec<String>,
    pub warning: String,
}

impl Disclosure {
    #[allow(
        clippy::unnecessary_debug_formatting,
        reason = "Consent must expose path control characters"
    )]
    pub fn description(&self, action: Action) -> String {
        let mut text = format!("{:?}: {}", self.path, self.warning);
        if !self.contains.is_empty() {
            text.push_str(" It contains ");
            text.push_str(&self.contains.join("; "));
            text.push_str(", whose owners are not involved either.");
        }
        let recovery = files_recoverability(action);
        if !recovery.is_empty() {
            text.push(' ');
            text.push_str(recovery);
        }
        text
    }
}

/// What deleting files directly leaves to recover from.
pub const fn files_recoverability(action: Action) -> &'static str {
    match action {
        Action::Trash => {
            "Moved to Trash: recoverable until the Trash is emptied, and its space returns only then."
        }
        Action::Permanent => {
            "Deleted permanently: cannot be undone through PwrDisk or Trash."
        }
        Action::Skip | Action::Owner => "",
    }
}

/// One marked path and what can be done with it. A failure to resolve its
/// owner is this proposal's status, never the whole preview's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub selected: PathBuf,
    pub bytes: u64,
    pub is_dir: bool,
    /// Why it is treated as a cache. None for an ordinary path.
    pub hint: Option<String>,
    pub owner: Option<Owner>,
    /// Indices into [`Prepared::operations`]; empty when none can run.
    pub operations: Vec<usize>,
    /// Why the owner's command is not offered, or is only partly offered.
    pub issue: Option<Issue>,
    /// What Delete files means, when it is offered.
    pub delete: Option<Disclosure>,
    /// Why Delete files is not offered.
    pub delete_issue: Option<Issue>,
}

impl Proposal {
    pub const fn allows(&self, decision: Decision) -> bool {
        match decision {
            Decision::Skip => true,
            Decision::Owner => !self.operations.is_empty(),
            Decision::DeleteFiles => self.delete.is_some(),
        }
    }
}

/// Command outcome and volume observations are independent of allocation.
/// Even a failed/cancelled command can have changed the cache already.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub action: Action,
    pub owner: Option<Owner>,
    /// An owner step's actual scope; otherwise the selected path.
    pub path: PathBuf,
    pub outcome: Outcome,
    pub problem: Option<Problem>,
    pub error: Option<String>,
    pub scope_allocation_at_preview: u64,
    pub available_before: Option<u64>,
    pub available_after: Option<u64>,
    pub observed_change: Option<i64>,
}

impl Report {
    /// A few words on how this step ended, for a list of results.
    pub const fn outcome_label(&self) -> &'static str {
        match (self.action, self.outcome) {
            (_, Outcome::Skipped) => "skipped by you",
            (_, Outcome::NotReached) => "not reached",
            (_, Outcome::Cancelled) => "cancelled; may be partly done",
            (_, Outcome::Failed) => "failed",
            (Action::Owner, Outcome::Succeeded) => "command succeeded",
            (Action::Trash, Outcome::Succeeded) => "moved to Trash",
            (_, Outcome::Succeeded) => "deleted",
        }
    }

    #[allow(
        clippy::unnecessary_debug_formatting,
        reason = "Reports must expose path control characters"
    )]
    pub fn description(&self) -> String {
        let who = self.owner.map_or("", Owner::label);
        let what = match (self.action, self.outcome) {
            (_, Outcome::Skipped) => {
                "skipped by the person; nothing ran".to_string()
            }
            (_, Outcome::NotReached) => "not reached; nothing ran".to_string(),
            (Action::Owner, Outcome::Succeeded) => {
                "owner command exited successfully".to_string()
            }
            (Action::Trash, Outcome::Succeeded) => {
                "files moved to Trash".to_string()
            }
            (Action::Permanent, Outcome::Succeeded) => {
                "files deleted permanently".to_string()
            }
            (_, Outcome::Cancelled) => {
                "cancelled; it may have done part of its work".to_string()
            }
            (_, _) => "failed".to_string(),
        };
        let mut text =
            format!("{} {who} {:?}: {what}.", self.action.label(), self.path);
        if let Some(error) = &self.error {
            let problem = self.problem.map_or("error", Problem::label);
            let _ = write!(text, " {problem}: {error}.");
        }
        if matches!(self.outcome, Outcome::Skipped | Outcome::NotReached) {
            return text;
        }
        let _ = write!(
            text,
            " Scope allocation at preview: {} bytes (not savings). Volume available before: {:?}; after: {:?}; observed change: {:?} bytes. Other activity can change this measurement; open files, hardlinks, APFS clones and snapshots can retain blocks.",
            self.scope_allocation_at_preview,
            self.available_before,
            self.available_after,
            self.observed_change
        );
        if self.action == Action::Owner && self.outcome != Outcome::Succeeded {
            text.push_str(" Nothing else was deleted. Deleting these files instead needs its own choice in a new review.");
        }
        text
    }

    fn of(
        action: Action,
        owner: Option<Owner>,
        path: &Path,
        bytes: u64,
        before: Option<u64>,
        after: Option<u64>,
        outcome: Result<(), &Issue>,
    ) -> Self {
        let observed_change = before.zip(after).map(|(before, after)| {
            i64::try_from(i128::from(after) - i128::from(before)).unwrap_or({
                if after >= before { i64::MAX } else { i64::MIN }
            })
        });
        let (outcome, problem, error) = match outcome {
            Ok(()) => (Outcome::Succeeded, None, None),
            Err(issue) => (
                if issue.problem == Problem::Cancelled {
                    Outcome::Cancelled
                } else {
                    Outcome::Failed
                },
                Some(issue.problem),
                Some(issue.detail.clone()),
            ),
        };
        Self {
            action,
            owner,
            path: path.to_owned(),
            outcome,
            problem,
            error,
            scope_allocation_at_preview: bytes,
            available_before: before,
            available_after: after,
            observed_change,
        }
    }
}

/// Prepared on a worker. The environment is captured and replayed so an
/// approved invocation cannot silently acquire different configuration.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub proposals: Vec<Proposal>,
    /// Commands that resolved and passed every scope check, by scope.
    pub operations: Vec<Operation>,
    /// Each operation's whole scope, measured at preview.
    pub scope_bytes: Vec<u64>,
    pub root: PathBuf,
    environment: Environment,
    identities: Vec<process::Identity>,
}

#[derive(Clone, Debug)]
pub(crate) struct Environment {
    pub home: PathBuf,
    pub variables: BTreeMap<String, String>,
}

impl Environment {
    fn current() -> Result<Self, String> {
        let mut variables: BTreeMap<_, _> = std::env::vars_os()
            .map(|(key, value)| {
                Ok((key.into_string().map_err(|_| "Non-UTF-8 environment keys are unsupported for cleanup")?,
                    value.into_string().map_err(|_| "Non-UTF-8 environment values are unsupported for cleanup")?))
            }).collect::<Result<_, String>>()?;
        if let Some(path) = variables.get_mut("PATH") {
            *path = std::env::join_paths(
                std::env::split_paths(path)
                    .filter(|directory| directory.is_absolute()),
            )
            .map_err(|error| error.to_string())?
            .into_string()
            .map_err(|_| "Non-UTF-8 executable PATH is unsupported")?;
        }
        Ok(Self {
            home: std::env::home_dir()
                .ok_or("Cannot identify this user's home")?,
            variables,
        })
    }
}

pub(crate) trait Probe {
    fn executable(
        &mut self,
        name: &str,
        env: &Environment,
    ) -> Result<PathBuf, Issue>;
    fn output(
        &mut self,
        command: &Command,
        env: &Environment,
    ) -> Result<String, String>;
}

/// Read-only resolution, best effort per marked path. No cleanup command
/// runs and nothing is deleted until a separately confirmed worker starts.
pub fn prepare(targets: &[Target], root: &Path) -> Result<Prepared, String> {
    let environment = Environment::current()?;
    prepare_with(targets, root, environment, &mut process::SystemProbe)
        .map(where_checkable)
}

/// [`prepare`] with `path` as the only place owners are looked for, for
/// checking against a known set of executables, such as test fixtures.
/// Execution replays the same environment, so it uses them too.
pub fn prepare_on_path(
    targets: &[Target],
    root: &Path,
    path: &str,
) -> Result<Prepared, String> {
    let mut environment = Environment::current()?;
    environment.variables.insert("PATH".into(), path.into());
    prepare_with(targets, root, environment, &mut process::SystemProbe)
        .map(where_checkable)
}

/// Delete files is checked for programs using the path right before it
/// runs. Where that check is unavailable the step would always be refused,
/// so it is not offered at all rather than offered and then failed.
fn where_checkable(mut prepared: Prepared) -> Prepared {
    if !process::ACTIVE_USE_CHECKED {
        for proposal in &mut prepared.proposals {
            if proposal.delete.take().is_some() {
                proposal.delete_issue = Some(Issue::new(
                    Problem::Refused,
                    "Delete files needs PwrDisk's check for programs using \
                     the path, which runs only on macOS and Linux",
                ));
            }
        }
    }
    prepared
}

fn prepare_with(
    targets: &[Target],
    root: &Path,
    environment: Environment,
    probe: &mut impl Probe,
) -> Result<Prepared, String> {
    if targets.is_empty() {
        return Err("Nothing is marked".into());
    }
    let selection = crate::removal::plan(targets, root);
    let mut operations: Vec<Operation> = Vec::new();
    let mut proposals = Vec::with_capacity(targets.len());
    for target in targets {
        let hinted = recognize::hinted(&target.path, &environment);
        let mut proposal = Proposal {
            selected: target.path.clone(),
            bytes: target.bytes,
            is_dir: target.is_dir,
            hint: recognize::inspection_with(&target.path, &environment),
            owner: hinted.as_ref().map(|(owner, _)| *owner),
            operations: vec![],
            issue: None,
            delete: None,
            delete_issue: None,
        };
        if let Some(blocked) = selection
            .blocked
            .iter()
            .find(|blocked| blocked.path == target.path)
        {
            let issue = Issue::new(Problem::Refused, blocked.reason.clone());
            proposal.issue = Some(issue.clone());
            proposal.delete_issue = Some(issue);
            proposals.push(proposal);
            continue;
        }
        match resolve(&target.path, hinted.as_ref(), &environment, probe) {
            Ok(found) => {
                for operation in found {
                    proposal.owner.get_or_insert(operation.owner);
                    let index = if let Some(index) = operations
                        .iter()
                        .position(|existing| existing.scope == operation.scope)
                    {
                        operations[index].selected.push(target.path.clone());
                        index
                    } else {
                        operations.push(operation);
                        operations.len() - 1
                    };
                    proposal.operations.push(index);
                }
            }
            Err(issue) => proposal.issue = Some(issue),
        }
        proposals.push(proposal);
    }

    // Every operation is checked on its own; a failure is reported on the
    // proposals that asked for it and leaves the others offered.
    let mut failures: Vec<Option<Issue>> = vec![None; operations.len()];
    let mut order: Vec<usize> = (0..operations.len()).collect();
    order.sort_by(|left, right| {
        operations[*left].scope.cmp(&operations[*right].scope)
    });
    // Overlapping commands would make both consent and byte accounting
    // ambiguous. npm's sibling scopes intentionally do not overlap.
    for pair in order.windows(2) {
        if operations[pair[1]]
            .scope
            .starts_with(&operations[pair[0]].scope)
        {
            for index in pair {
                failures[*index] = Some(Issue::new(
                    Problem::Refused,
                    "Overlapping owner cleanup scopes; review them separately",
                ));
            }
        }
    }
    let mut checked = vec![None; operations.len()];
    for (index, operation) in operations.iter().enumerate() {
        if failures[index].is_none() {
            match check_scope(operation, root) {
                Ok(result) => checked[index] = Some(result),
                Err(issue) => failures[index] = Some(issue),
            }
        }
    }
    let mut remap = vec![None; operations.len()];
    let mut kept = Vec::new();
    let mut scope_bytes = Vec::new();
    let mut identities = Vec::new();
    for index in order {
        if let Some((bytes, identity)) = checked[index].take() {
            remap[index] = Some(kept.len());
            kept.push(operations[index].clone());
            scope_bytes.push(bytes);
            identities.push(identity);
        }
    }
    for proposal in &mut proposals {
        let failed = proposal
            .operations
            .iter()
            .find_map(|index| failures[*index].clone());
        proposal.operations = proposal
            .operations
            .iter()
            .filter_map(|index| remap[*index])
            .collect();
        if proposal.issue.is_none() {
            proposal.issue = failed;
        }
        if proposal.delete_issue.is_none() {
            match disclose(proposal, &environment) {
                Ok(disclosure) => proposal.delete = Some(disclosure),
                Err(issue) => proposal.delete_issue = Some(issue),
            }
        }
    }
    Ok(Prepared {
        proposals,
        operations: kept,
        scope_bytes,
        root: root.to_owned(),
        environment,
        identities,
    })
}

/// An owner's whole scope must pass the same guards as any removal, stay
/// on one volume without external links, and be measurable.
fn check_scope(
    operation: &Operation,
    root: &Path,
) -> Result<(u64, process::Identity), Issue> {
    let refused = |detail: String| Issue::new(Problem::Refused, detail);
    let scope = Target {
        path: operation.scope.clone(),
        bytes: 0,
        is_dir: true,
        hidden: false,
    };
    let plan = crate::removal::plan(std::slice::from_ref(&scope), root);
    if let Some(blocked) = plan.blocked.first() {
        return Err(refused(format!(
            "Actual owner scope fails removal guards: {}",
            blocked.reason
        )));
    }
    if plan.targets.len() != 1 {
        return Err(refused("Actual owner scope fails removal guards".into()));
    }
    process::check_tree(&operation.scope).map_err(refused)?;
    let bytes = crate::removal_limit::measured(&plan.targets)
        .map_err(refused)?
        .first()
        .map_or(0, |target| target.bytes);
    let identity = process::Identity::read(&operation.command.executable)
        .map_err(|detail| Issue::new(Problem::NotFound, detail))?;
    Ok((bytes, identity))
}

/// What Delete files means for this path. Never "safe": an unknown cache is
/// said to be unknown, and a recognized one is said to bypass its owner.
fn disclose(
    proposal: &Proposal,
    env: &Environment,
) -> Result<Disclosure, Issue> {
    let contains = recognize::contained(&proposal.selected, env)
        .map_err(|detail| Issue::new(Problem::Refused, detail))?;
    let warning = match (proposal.owner, &proposal.hint) {
        (Some(owner), _) => format!(
            "Deletes exactly this path and its contents directly; the {} is not involved. Anything it expects to find there must be downloaded or rebuilt, and installed environments that link into it can break until reinstalled.",
            owner.label()
        ),
        (None, Some(_)) => "No verified owner: PwrDisk does not know that this is disposable. It may be installed runtime or application state, and software that needs it can break. Inspect it before choosing this.".to_owned(),
        (None, None) => "Not recognized as a cache. Deletes exactly this path and its contents, as any other marked path.".to_owned(),
    };
    Ok(Disclosure {
        path: proposal.selected.clone(),
        contains,
        warning,
    })
}

#[allow(
    clippy::unnecessary_debug_formatting,
    reason = "Safety errors quote selected paths including controls"
)]
fn resolve(
    path: &Path,
    hint: Option<&(Owner, PathBuf)>,
    env: &Environment,
    probe: &mut impl Probe,
) -> Result<Vec<Operation>, Issue> {
    if let Some(reason) = hint.and_then(|(owner, _)| owner.unsupported()) {
        return Err(Issue::new(Problem::NoCommand, reason));
    }
    // Probe all supported owners for nonstandard roots configured in user
    // settings. A name is insufficient, and a default path is not authority.
    let owners = hint.map_or_else(
        || {
            vec![
                Owner::Npm,
                Owner::Npx,
                Owner::Uv,
                Owner::Pip,
                Owner::Cypress,
            ]
        },
        |(owner, _)| vec![*owner],
    );
    let mut failures = Vec::new();
    let mut found = Vec::new();
    for owner in owners {
        match resolve_owner(owner, path, env, probe) {
            Ok(Some(operation)) => found.push(operation),
            Ok(None) => {}
            Err(issue) => failures.push(Issue::new(
                issue.problem,
                format!("{}: {}", owner.label(), issue.detail),
            )),
        }
    }
    if !found.is_empty() {
        if found.iter().any(|operation| {
            operation.owner == Owner::Npm
                && operation.cache_root == path
                && operation.cache_root.join("_npx").is_dir()
        }) && !found.iter().any(|operation| operation.owner == Owner::Npx)
        {
            return Err(Issue::new(
                Problem::NoCommand,
                format!(
                    "The npm wrapper also contains an npx cache without a supported cleanup command. Select _cacache explicitly or inspect with a supported npm version. {}",
                    failures
                        .iter()
                        .map(|issue| issue.detail.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            ));
        }
        return Ok(found);
    }
    if let Some((owner, _)) = hint {
        // The one owner asked either failed, which says why, or reported
        // a configured cache elsewhere, which is never redirected to.
        return Err(failures.into_iter().next().unwrap_or_else(|| {
            Issue::new(
                Problem::NotConfiguredCache,
                format!(
                    "The {}'s configured cache is elsewhere, so its command would not clean {path:?}. Inspect it.",
                    owner.label()
                ),
            )
        }));
    }
    Err(Issue::new(
        Problem::UnknownOwner,
        format!(
            "No supported owner cleanup resolves to {:?}. Inspect the cache and its owning installation. Software may be installed outside PwrDisk's PATH; an unavailable command does not prove it is absent. {}",
            path,
            failures
                .iter()
                .map(|issue| issue.detail.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        ),
    ))
}

fn resolve_owner(
    owner: Owner,
    selected: &Path,
    env: &Environment,
    probe: &mut impl Probe,
) -> Result<Option<Operation>, Issue> {
    let executable = probe.executable(owner.executable(), env)?;
    let mut command = Command {
        executable,
        arguments: vec![],
        environment: BTreeMap::new(),
        working_directory: env.home.clone(),
    };
    // npm rotates logs even with logs-max=0. Sending the log directory to
    // a non-directory prevents that unapproved side effect for probes and
    // cleanup alike. pip's configured log file is similarly overridden.
    if owner == Owner::Pip {
        command.environment.extend([
            ("PIP_LOG".into(), "/dev/null".into()),
            ("PIP_DISABLE_PIP_VERSION_CHECK".into(), "1".into()),
            ("PIP_NO_INPUT".into(), "1".into()),
        ]);
    }
    if matches!(owner, Owner::Npm | Owner::Npx) {
        command.environment.extend([
            ("npm_config_logs_dir".into(), "/dev/null".into()),
            ("npm_config_logs_max".into(), "0".into()),
            ("npm_config_update_notifier".into(), "false".into()),
            ("npm_config_timing".into(), "false".into()),
        ]);
    }
    if owner == Owner::Uv
        && env
            .variables
            .get("UV_LINK_MODE")
            .is_some_and(|mode| mode == "symlink")
    {
        return Err(Issue::new(
            Problem::Refused,
            "UV_LINK_MODE=symlink couples installed environments to this cache; inspect with uv before cleanup",
        ));
    }
    let version_args = if owner == Owner::Cypress {
        vec!["version", "--component", "package"]
    } else {
        vec!["--version"]
    };
    command.arguments = strings(&version_args);
    let version = probe
        .output(&command, env)
        .map_err(|detail| Issue::new(Problem::VersionCheckFailed, detail))?
        .trim()
        .to_owned();
    supported_version(owner, &version)
        .map_err(|detail| Issue::new(Problem::UnsupportedVersion, detail))?;
    let inspection_only =
        || Issue::new(Problem::NoCommand, "Owner is inspection-only");
    command.arguments = strings(match owner {
        Owner::Npm | Owner::Npx => &["config", "get", "cache"],
        Owner::Uv | Owner::Pip => &["cache", "dir"],
        Owner::Cypress => &["cache", "path"],
        _ => return Err(inspection_only()),
    });
    let output = probe
        .output(&command, env)
        .map_err(|detail| Issue::new(Problem::ConfigCheckFailed, detail))?;
    let config = |detail: &str| Issue::new(Problem::ConfigCheckFailed, detail);
    // Remove the CLI's line terminator, never whitespace that can be part
    // of a real directory name. Guessing here can select a different tree.
    // The supported Unix adapters emit LF. A CR is ambiguous with a
    // literal path character, so refuse it rather than guess at CRLF.
    let value = output.strip_suffix('\n').unwrap_or(&output);
    let cache_root = PathBuf::from(value);
    if !cache_root.is_absolute() || value.contains('\n') || value.contains('\r')
    {
        return Err(config(
            "Owner returned an ambiguous or relative cache root",
        ));
    }
    if cache_root
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(config(
            "Owner cache root contains parent traversal; inspect its absolute configured path",
        ));
    }
    let scope = match owner {
        Owner::Npm => cache_root.join("_cacache"),
        Owner::Npx => cache_root.join("_npx"),
        _ => cache_root.clone(),
    };
    // Never redirect an old/default cache selection into the active cache
    // elsewhere. Also never interpret the npm wrapper as just one cache.
    let npm_wrapper = matches!(owner, Owner::Npm | Owner::Npx)
        && selected == cache_root
        && scope.is_dir();
    if !selected.starts_with(&scope) && !npm_wrapper {
        return Ok(None);
    }
    let root_arg = cache_root
        .to_str()
        .ok_or_else(|| config("Non-UTF-8 cache roots are unsupported"))?
        .to_owned();
    command.arguments = match owner {
        Owner::Npm => vec![
            "cache".into(),
            "clean".into(),
            "--force".into(),
            "--cache".into(),
            root_arg,
        ],
        Owner::Npx => vec![
            "cache".into(),
            "npx".into(),
            "rm".into(),
            "--force".into(),
            "--cache".into(),
            root_arg,
        ],
        Owner::Uv => vec![
            "cache".into(),
            "clean".into(),
            "--cache-dir".into(),
            root_arg,
        ],
        Owner::Pip => vec![
            "--cache-dir".into(),
            root_arg,
            "cache".into(),
            "purge".into(),
        ],
        Owner::Cypress => {
            command
                .environment
                .insert("CYPRESS_CACHE_FOLDER".into(), root_arg);
            strings(&["cache", "clear"])
        }
        _ => return Err(inspection_only()),
    };
    let effects = match owner {
        Owner::Npm => {
            "Clears the entire npm package cache (_cacache); does not clear the separate npx cache. Later installs may download packages again."
        }
        Owner::Npx => {
            "Clears the entire npx execution cache (_npx); does not clear npm's package cache. Cached npx environments must be downloaded/recreated."
        }
        Owner::Uv => {
            "Clears all uv cache entries, preserving uv's lock protocol. Later operations may download/build again. Environments installed with symlink link mode can break; inspect that configuration before approval."
        }
        Owner::Pip => {
            "Purges pip's HTTP and wheel caches. Later installs may download/build again; other files in the cache root may remain."
        }
        Owner::Cypress => {
            "Removes ALL cached Cypress binary versions, including versions other projects use. Tests require reinstalling their matching binary."
        }
        _ => "",
    };
    Ok(Some(Operation {
        owner,
        version,
        selected: vec![selected.to_owned()],
        cache_root,
        scope,
        command,
        effects: effects.into(),
        limitations: OWNER_LIMITATIONS.into(),
    }))
}

/// What every owner command shares, whatever its owner.
const OWNER_LIMITATIONS: &str = "The command acts on the WHOLE scope, including unselected siblings. Listed allocation is an upper bound, not promised savings. Other activity, open files, hardlinks, APFS clones and snapshots can retain blocks. Active-use detection is best effort; stop the owning tools first. PwrDisk rescans afterwards. If it fails, nothing else is deleted.";

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn supported_version(owner: Owner, output: &str) -> Result<(), String> {
    let text = match owner {
        Owner::Uv => output.strip_prefix("uv "),
        Owner::Pip => output.strip_prefix("pip "),
        _ => Some(output),
    }
    .and_then(|text| text.split_whitespace().next())
    .ok_or("Unrecognized version output")?;
    let parts: Vec<_> = text
        .split('.')
        .map(str::parse::<u32>)
        .collect::<Result<_, _>>()
        .map_err(|_| "Prerelease or unrecognized version is unsupported")?;
    let supported = match (owner, parts.as_slice()) {
        (Owner::Npm, [major, _, _]) => (9..=11).contains(major),
        (Owner::Npx, [11, minor, _]) => *minor >= 2,
        (Owner::Uv, [0, minor, _]) => *minor == 11,
        (Owner::Pip, [major, ..]) => (23..=26).contains(major),
        (Owner::Cypress, [major, _, _]) => (13..=15).contains(major),
        _ => false,
    };
    let runs = match owner {
        Owner::Npm => "npm 9 to 11",
        Owner::Npx => "npm 11.2 or later 11.x",
        Owner::Uv => "uv 0.11.x",
        Owner::Pip => "pip 23 to 26",
        Owner::Cypress => "Cypress 13 to 15",
        _ => "no version",
    };
    if supported {
        Ok(())
    } else {
        Err(format!(
            "{} {text} is not a version PwrDisk runs ({runs}). Update it, or clean it with the owner yourself",
            owner.label()
        ))
    }
}

/// Confirmation phrases, one per combination of the two intents. Owner
/// cleanup and deleting files are confirmed separately and named apart.
pub const RUN_OWNER_CLEANUP: &str = "RUN OWNER CLEANUP";
pub const DELETE_CACHE_FILES: &str = "DELETE CACHE FILES";
pub const TRASH_CACHE_FILES: &str = "TRASH CACHE FILES";
pub const RUN_AND_DELETE: &str = "RUN OWNER CLEANUP AND DELETE CACHE FILES";
pub const RUN_AND_TRASH: &str = "RUN OWNER CLEANUP AND TRASH CACHE FILES";
pub const CONFIRMATIONS: [&str; 5] = [
    RUN_OWNER_CLEANUP,
    DELETE_CACHE_FILES,
    TRASH_CACHE_FILES,
    RUN_AND_DELETE,
    RUN_AND_TRASH,
];

impl Prepared {
    #[allow(
        clippy::unnecessary_debug_formatting,
        reason = "Consent must expose path control characters"
    )]
    pub fn proposal_description(&self, index: usize) -> String {
        let Some(proposal) = self.proposals.get(index) else {
            return String::new();
        };
        let mut text = format!(
            "{}. {:?} ({} bytes)",
            index + 1,
            proposal.selected,
            proposal.bytes
        );
        if let Some(hint) = &proposal.hint {
            let _ = write!(text, "\n   Treated as a cache: {hint}");
        }
        for operation in &proposal.operations {
            if let Some(operation) = self.operations.get(*operation) {
                text.push_str("\n   Owner command (choose owner):\n   ");
                text.push_str(&operation.description().replace('\n', "\n   "));
            }
        }
        if let Some(issue) = &proposal.issue {
            let _ = write!(
                text,
                "\n   Owner command {}: {}",
                if proposal.operations.is_empty() {
                    "unavailable"
                } else {
                    "partly unavailable"
                },
                issue.description()
            );
        }
        match (&proposal.delete, &proposal.delete_issue) {
            (Some(disclosure), _) => {
                let _ = write!(
                    text,
                    "\n   Delete files (choose delete_files): {}",
                    disclosure.description(Action::Skip)
                );
            }
            (None, Some(issue)) => {
                let _ = write!(
                    text,
                    "\n   Delete files unavailable: {}",
                    issue.description()
                );
            }
            (None, None) => {}
        }
        text
    }

    /// Every proposal, for the window's caption and an agent's preview.
    pub fn description(&self) -> String {
        (0..self.proposals.len())
            .map(|index| self.proposal_description(index))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// An agent names a decision per path; map them onto the proposals.
    /// Every marked path must be named exactly once, so nothing is decided
    /// by omission or by an ambiguous duplicate.
    pub fn decisions_for(
        &self,
        named: &[(PathBuf, Decision)],
    ) -> Result<Vec<Decision>, String> {
        let mut decisions = vec![None; self.proposals.len()];
        for (path, decision) in named {
            let index = self
                .proposals
                .iter()
                .position(|proposal| proposal.selected == *path)
                .ok_or_else(|| {
                    format!("{} is not a marked path", path.display())
                })?;
            if decisions[index].replace(*decision).is_some() {
                return Err(format!(
                    "{} is named more than once",
                    path.display()
                ));
            }
        }
        decisions
            .into_iter()
            .zip(&self.proposals)
            .map(|(decision, proposal)| {
                decision.ok_or_else(|| {
                    format!(
                        "Choose skip, owner or delete_files for {}",
                        proposal.selected.display()
                    )
                })
            })
            .collect()
    }

    /// Turn the person's decisions into the steps to run. Refuses any
    /// choice the proposal did not offer, and any pair of choices that
    /// would act on the same files twice or on a path left on Skip.
    pub fn approve(
        &self,
        decisions: &[Decision],
        files: Action,
    ) -> Result<Approved, String> {
        if decisions.len() != self.proposals.len() {
            return Err("Choose an action for every marked path".into());
        }
        if !matches!(files, Action::Trash | Action::Permanent)
            && decisions.contains(&Decision::DeleteFiles)
        {
            return Err(
                "Delete files needs Trash or permanent deletion chosen".into(),
            );
        }
        let mut owners: Vec<usize> = Vec::new();
        let mut raw = Vec::new();
        for (proposal, decision) in self.proposals.iter().zip(decisions) {
            if !proposal.allows(*decision) {
                return Err(format!(
                    "{} cannot be chosen for {}",
                    match decision {
                        Decision::Owner => "An owner command",
                        Decision::DeleteFiles => "Delete files",
                        Decision::Skip => "Skip",
                    },
                    proposal.selected.display()
                ));
            }
            match decision {
                Decision::Owner => {
                    for index in &proposal.operations {
                        if !owners.contains(index) {
                            owners.push(*index);
                        }
                    }
                }
                Decision::DeleteFiles => raw.push(Target {
                    path: proposal.selected.clone(),
                    bytes: proposal.bytes,
                    is_dir: proposal.is_dir,
                    hidden: false,
                }),
                Decision::Skip => {}
            }
        }
        owners.sort_unstable();
        // Paths each action reaches. A path left on Skip, or given another
        // action, must not overlap one: Skip means nothing happens to it.
        let reached: Vec<(&Path, Decision)> =
            owners
                .iter()
                .map(|index| {
                    (self.operations[*index].scope.as_path(), Decision::Owner)
                })
                .chain(raw.iter().map(|target| {
                    (target.path.as_path(), Decision::DeleteFiles)
                }))
                .collect();
        for (proposal, decision) in self.proposals.iter().zip(decisions) {
            for (path, action) in &reached {
                // Either way round: an action inside a skipped path changes
                // it too, and so does one around it.
                let overlaps = proposal.selected.starts_with(path)
                    || path.starts_with(&proposal.selected);
                if overlaps && *decision != *action {
                    return Err(format!(
                        "{} is inside what another choice acts on ({}); choose the same action for both",
                        proposal.selected.display(),
                        path.display()
                    ));
                }
            }
        }
        let raw_plan = crate::removal::plan(&raw, &self.root);
        if let Some(blocked) = raw_plan.blocked.first() {
            return Err(format!(
                "{}: {}",
                blocked.path.display(),
                blocked.reason
            ));
        }
        let mut steps = Vec::new();
        let mut targets = Vec::new();
        for index in &owners {
            steps.push(Step::Owner(*index));
            targets.push(Target {
                path: self.operations[*index].scope.clone(),
                bytes: self.scope_bytes[*index],
                is_dir: true,
                hidden: false,
            });
        }
        for target in raw_plan.targets {
            let proposal = self
                .proposals
                .iter()
                .position(|proposal| {
                    crate::removal::normalize(&proposal.selected) == target.path
                })
                .ok_or("Delete files lost track of a chosen path")?;
            steps.push(Step::Files {
                proposal,
                action: files,
            });
            targets.push(target);
        }
        if steps.is_empty() {
            return Err("Nothing chosen: every marked path is skipped".into());
        }
        let skipped = self
            .proposals
            .iter()
            .zip(decisions)
            .filter(|(_, decision)| **decision == Decision::Skip)
            .map(|(proposal, _)| Report {
                action: Action::Skip,
                owner: proposal.owner,
                path: proposal.selected.clone(),
                outcome: Outcome::Skipped,
                problem: proposal.issue.as_ref().map(|issue| issue.problem),
                error: proposal
                    .issue
                    .as_ref()
                    .map(|issue| issue.detail.clone()),
                scope_allocation_at_preview: 0,
                available_before: None,
                available_after: None,
                observed_change: None,
            })
            .collect();
        // Planned whole so the run shows guards and volumes as any removal
        // does; the steps above were already checked not to overlap.
        let plan = crate::removal::plan(&targets, &self.root);
        if plan.targets.len() != targets.len() || !plan.blocked.is_empty() {
            return Err("Chosen actions fail removal guards".into());
        }
        let steps = plan
            .targets
            .iter()
            .map(|target| {
                targets
                    .iter()
                    .position(|chosen| {
                        crate::removal::normalize(&chosen.path) == target.path
                    })
                    .map(|index| steps[index])
                    .ok_or("Chosen actions lost track of a path")
            })
            .collect::<Result<_, _>>()?;
        Ok(Approved {
            prepared: self.clone(),
            decisions: decisions.to_vec(),
            files,
            steps,
            plan,
            skipped,
        })
    }

    /// Re-probe against the captured environment right before this
    /// command, then recheck its scope and whether its owner is running.
    pub(crate) fn preflight_operation(
        &self,
        index: usize,
    ) -> Result<(), Issue> {
        let changed = |detail: &str| Issue::new(Problem::Changed, detail);
        let operation = self
            .operations
            .get(index)
            .ok_or_else(|| changed("Missing owner operation"))?;
        let identity =
            process::Identity::read(&operation.command.executable)
                .map_err(|detail| Issue::new(Problem::Changed, detail))?;
        if self.identities.get(index) != Some(&identity) {
            return Err(changed(
                "Owner executable changed after preview; obtain new approval",
            ));
        }
        let fresh = resolve_owner(
            operation.owner,
            &operation.scope,
            &self.environment,
            &mut process::SystemProbe,
        )
        .map_err(|issue| {
            Issue::new(issue.problem, format!("Since the preview: {}", issue.detail))
        })?
        .ok_or_else(|| {
            changed("Configured cache root changed after preview; obtain new approval")
        })?;
        if fresh.command != operation.command
            || fresh.version != operation.version
            || fresh.scope != operation.scope
        {
            return Err(changed(
                "Owner command, version or cache scope changed; obtain new approval",
            ));
        }
        process::check_tree(&operation.scope)
            .map_err(|detail| Issue::new(Problem::Refused, detail))?;
        process::check_active(operation)
            .map_err(|detail| Issue::new(Problem::InUse, detail))
    }

    /// Runs only the allowlisted command. A failure never calls a remover,
    /// even when the owner partially changed the cache.
    pub(crate) fn execute(
        &self,
        index: usize,
        cancel: &AtomicBool,
    ) -> Result<(), Issue> {
        let operation = self.operations.get(index).ok_or_else(|| {
            Issue::new(Problem::Changed, "Missing owner operation")
        })?;
        process::run(
            &operation.command,
            &self.environment,
            cancel,
            std::time::Duration::from_mins(10),
        )
        .map(|_| ())
    }
}

/// One step of an approved cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Index into [`Prepared::operations`].
    Owner(usize),
    /// Delete one proposal's exact selected path directly.
    Files { proposal: usize, action: Action },
}

/// The person's decisions, turned into steps aligned with `plan.targets`.
/// Carried intact to the removal worker, which checks each step again.
#[derive(Clone, Debug)]
pub struct Approved {
    pub prepared: Prepared,
    pub decisions: Vec<Decision>,
    /// How Delete files removes: Trash or permanently.
    pub files: Action,
    pub steps: Vec<Step>,
    pub plan: Plan,
    /// Proposals left on Skip, reported so every path has a result.
    pub skipped: Vec<Report>,
}

impl Approved {
    pub fn action(&self, index: usize) -> Action {
        match self.steps.get(index) {
            Some(Step::Owner(_)) => Action::Owner,
            Some(Step::Files { action, .. }) => *action,
            None => Action::Skip,
        }
    }

    pub fn owner_steps(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step, Step::Owner(_)))
            .count()
    }

    pub fn file_steps(&self) -> usize {
        self.steps.len() - self.owner_steps()
    }

    /// The text that confirms exactly these intents, and no others.
    pub fn confirmation(&self) -> &'static str {
        match (self.owner_steps() > 0, self.file_steps() > 0, self.files) {
            (true, false, _) => RUN_OWNER_CLEANUP,
            (false, true, Action::Trash) => TRASH_CACHE_FILES,
            (false, _, _) => DELETE_CACHE_FILES,
            (true, true, Action::Trash) => RUN_AND_TRASH,
            (true, true, _) => RUN_AND_DELETE,
        }
    }

    /// Everything the person confirms: each command verbatim, each path
    /// deleted directly with what that means, and what is left alone.
    #[allow(
        clippy::unnecessary_debug_formatting,
        reason = "Consent must expose path control characters"
    )]
    pub fn description(&self) -> String {
        let mut text = String::new();
        if self.owner_steps() > 0 {
            let _ = writeln!(
                text,
                "Run owner commands ({}), exactly as shown:",
                self.owner_steps()
            );
            for step in &self.steps {
                if let Step::Owner(index) = step
                    && let Some(operation) =
                        self.prepared.operations.get(*index)
                {
                    text.push_str(&operation.description());
                    text.push_str("\n\n");
                }
            }
        }
        if self.file_steps() > 0 {
            let _ = writeln!(
                text,
                "Delete files directly, not through their owner ({}) \u{2014} {}:",
                self.file_steps(),
                self.files.label()
            );
            for step in &self.steps {
                if let Step::Files { proposal, action } = step
                    && let Some(proposal) =
                        self.prepared.proposals.get(*proposal)
                    && let Some(disclosure) = &proposal.delete
                {
                    let _ = writeln!(
                        text,
                        "{} ({})",
                        disclosure.description(*action),
                        crate::size::human_bytes(proposal.bytes)
                    );
                }
            }
            text.push('\n');
        }
        if !self.skipped.is_empty() {
            let _ =
                writeln!(text, "Nothing happens to ({}):", self.skipped.len());
            for report in &self.skipped {
                let _ = writeln!(text, "{:?}", report.path);
            }
        }
        text.trim_end().to_owned()
    }

    /// Re-measure each step on the worker. A path that can no longer be
    /// measured is refused alone; past the capacity limit the whole run is.
    pub(crate) fn admit(
        &self,
        limit: &crate::removal_limit::Ceilings,
    ) -> Result<Vec<Option<Issue>>, String> {
        let mut refusals = vec![None; self.plan.targets.len()];
        let mut measured = Vec::new();
        for (index, target) in self.plan.targets.iter().enumerate() {
            match crate::removal_limit::measured(std::slice::from_ref(target)) {
                Ok(mut fresh) => measured.append(&mut fresh),
                Err(reason) => {
                    refusals[index] =
                        Some(Issue::new(Problem::Refused, reason));
                }
            }
        }
        limit.check(&measured)?;
        Ok(refusals)
    }

    /// Checked again right before the step: an owner's command, version,
    /// executable and scope; for direct deletion, whether it is in use.
    pub(crate) fn preflight(&self, index: usize) -> Result<(), Issue> {
        match self.steps.get(index) {
            Some(Step::Owner(operation)) => {
                self.prepared.preflight_operation(*operation)
            }
            Some(Step::Files { proposal, .. }) => {
                let proposal = &self.prepared.proposals[*proposal];
                process::check_path_active(&proposal.selected, proposal.owner)
                    .map_err(|detail| Issue::new(Problem::InUse, detail))
            }
            None => Err(Issue::new(Problem::Changed, "Missing cleanup step")),
        }
    }

    pub(crate) fn report(
        &self,
        index: usize,
        before: Option<u64>,
        after: Option<u64>,
        outcome: Result<(), &Issue>,
    ) -> Report {
        let (owner, path, bytes) = match self.steps.get(index) {
            Some(Step::Owner(operation)) => {
                let operation = &self.prepared.operations[*operation];
                (
                    Some(operation.owner),
                    operation.scope.clone(),
                    self.plan
                        .targets
                        .get(index)
                        .map_or(0, |target| target.bytes),
                )
            }
            Some(Step::Files { proposal, .. }) => {
                let proposal = &self.prepared.proposals[*proposal];
                (proposal.owner, proposal.selected.clone(), proposal.bytes)
            }
            None => (None, PathBuf::new(), 0),
        };
        Report::of(
            self.action(index),
            owner,
            &path,
            bytes,
            before,
            after,
            outcome,
        )
    }

    pub(crate) fn not_reached(&self, index: usize) -> Report {
        let mut report = self.report(index, None, None, Ok(()));
        report.outcome = Outcome::NotReached;
        report
    }
}
