//! Owner-managed cache cleanup. Classification is an inspection hint, never
//! permission to recursively delete a cache. Only this allowlist builds writes.

use std::collections::BTreeMap;
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
    #[allow(
        clippy::unnecessary_debug_formatting,
        reason = "Consent must expose argument boundaries and control characters"
    )]
    pub fn description(&self) -> String {
        // Debug quoting exposes control characters and argument boundaries.
        format!(
            "{} ({})\nSelected: {:?}\nActual scope: {:?}\nExecutable: {:?}\nArguments: {:?}\nEnvironment overrides: {:?}\nWorking directory: {:?}\n{}\n{}",
            self.owner.label(),
            self.version,
            self.selected,
            self.scope,
            self.command.executable,
            self.command.arguments,
            self.command.environment,
            self.command.working_directory,
            self.effects,
            self.limitations,
        )
    }
}

/// Command outcome and volume observations are independent of allocation.
/// Even a failed/cancelled command can have changed the cache already.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub owner: Owner,
    pub scope: PathBuf,
    pub success: bool,
    pub error: Option<String>,
    pub scope_allocation_at_preview: u64,
    pub available_before: Option<u64>,
    pub available_after: Option<u64>,
    pub observed_change: Option<i64>,
}

impl Report {
    #[allow(
        clippy::unnecessary_debug_formatting,
        reason = "Reports must expose path control characters"
    )]
    pub fn description(&self) -> String {
        format!(
            "{} scope {:?}: command {}. Error: {:?}. Scope allocation at preview: {} bytes (not savings). Volume available before: {:?}; after: {:?}; observed change: {:?} bytes. Other activity can change this measurement; open files, hardlinks, APFS clones and snapshots can retain blocks. No raw deletion fallback.",
            self.owner.label(),
            self.scope,
            if self.success {
                "succeeded"
            } else {
                "failed or cancelled"
            },
            self.error,
            self.scope_allocation_at_preview,
            self.available_before,
            self.available_after,
            self.observed_change
        )
    }

    pub(crate) fn of(
        operation: &Operation,
        bytes: u64,
        before: Option<u64>,
        after: Option<u64>,
        outcome: &Result<(), String>,
    ) -> Self {
        let observed_change = before.zip(after).map(|(before, after)| {
            i64::try_from(i128::from(after) - i128::from(before)).unwrap_or({
                if after >= before { i64::MAX } else { i64::MIN }
            })
        });
        Self {
            owner: operation.owner,
            scope: operation.scope.clone(),
            success: outcome.is_ok(),
            error: outcome.as_ref().err().cloned(),
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
    pub operations: Vec<Operation>,
    pub plan: Plan,
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
    ) -> Result<PathBuf, String>;
    fn output(
        &mut self,
        command: &Command,
        env: &Environment,
    ) -> Result<String, String>;
}

/// Read-only resolution. No cleanup command runs until the separately
/// confirmed worker starts. Unknown owners and failed probes fail closed.
pub fn prepare(targets: &[Target], root: &Path) -> Result<Prepared, String> {
    let environment = Environment::current()?;
    prepare_with(targets, root, environment, &mut process::SystemProbe)
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
    let selected = crate::removal::plan(targets, root);
    if !selected.blocked.is_empty() {
        return Err(format!(
            "Selected paths fail removal guards: {:?}",
            selected.blocked
        ));
    }
    let mut operations: Vec<Operation> = Vec::new();
    for target in targets {
        for operation in resolve(&target.path, &environment, probe)? {
            if let Some(existing) = operations
                .iter_mut()
                .find(|existing| existing.scope == operation.scope)
            {
                existing.selected.push(target.path.clone());
            } else {
                operations.push(operation);
            }
        }
    }
    operations.sort_by(|left, right| left.scope.cmp(&right.scope));
    // Overlapping commands would make both consent and byte accounting
    // ambiguous. npm's sibling scopes intentionally do not overlap.
    for pair in operations.windows(2) {
        if pair[1].scope.starts_with(&pair[0].scope) {
            return Err(
                "Overlapping owner cleanup scopes; review them separately"
                    .into(),
            );
        }
    }
    let scopes: Vec<_> = operations
        .iter()
        .map(|operation| Target {
            path: operation.scope.clone(),
            bytes: 0,
            is_dir: true,
            hidden: false,
        })
        .collect();
    let plan = crate::removal::plan(&scopes, root);
    if !plan.blocked.is_empty() || plan.targets.len() != scopes.len() {
        return Err(format!(
            "Actual owner scope fails removal guards: {:?}",
            plan.blocked
        ));
    }
    for scope in &scopes {
        process::check_tree(&scope.path)?;
    }
    let measured = crate::removal_limit::measured(&scopes)?;
    let plan = crate::removal::plan(&measured, root);
    let identities = operations
        .iter()
        .map(|operation| process::Identity::read(&operation.command.executable))
        .collect::<Result<_, _>>()?;
    Ok(Prepared {
        operations,
        plan,
        environment,
        identities,
    })
}

#[allow(
    clippy::unnecessary_debug_formatting,
    reason = "Safety errors quote selected paths including controls"
)]
fn resolve(
    path: &Path,
    env: &Environment,
    probe: &mut impl Probe,
) -> Result<Vec<Operation>, String> {
    let hint = recognize::owner(path, env).or_else(|| {
        path.ancestors().take(4).find_map(|root| {
            recognize::shape_owner(root).map(|owner| (owner, root.to_owned()))
        })
    });
    if let Some(owner) = hint
        .as_ref()
        .and_then(|(owner, _)| owner.unsupported().map(|_| *owner))
    {
        return Err(format!(
            "{}: {} No raw deletion fallback.",
            owner.label(),
            owner.unsupported().unwrap_or_default()
        ));
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
        |(owner, _)| vec![owner],
    );
    let mut failures = Vec::new();
    let mut found = Vec::new();
    for owner in owners {
        match resolve_owner(owner, path, env, probe) {
            Ok(Some(operation)) => found.push(operation),
            Ok(None) => {}
            Err(reason) => {
                failures.push(format!("{}: {reason}", owner.label()));
            }
        }
    }
    if !found.is_empty() {
        if found.iter().any(|operation| {
            operation.owner == Owner::Npm
                && operation.cache_root == path
                && operation.cache_root.join("_npx").is_dir()
        }) && !found.iter().any(|operation| operation.owner == Owner::Npx)
        {
            return Err(format!(
                "The npm wrapper also contains an npx cache without a supported cleanup command. Select _cacache explicitly or inspect with a supported npm version. No raw deletion fallback. {}",
                failures.join("; ")
            ));
        }
        return Ok(found);
    }
    Err(format!(
        "No supported owner cleanup resolves to {:?}. Inspect the cache and its owning installation. Software may be installed outside PwrDisk's PATH; an unavailable command does not prove it is absent. No raw deletion fallback. {}",
        path,
        failures.join("; ")
    ))
}

fn resolve_owner(
    owner: Owner,
    selected: &Path,
    env: &Environment,
    probe: &mut impl Probe,
) -> Result<Option<Operation>, String> {
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
        return Err("UV_LINK_MODE=symlink couples installed environments to this cache; inspect with uv before cleanup".into());
    }
    let version_args = if owner == Owner::Cypress {
        vec!["version", "--component", "package"]
    } else {
        vec!["--version"]
    };
    command.arguments = strings(&version_args);
    let version = probe.output(&command, env)?.trim().to_owned();
    supported_version(owner, &version)?;
    command.arguments = strings(match owner {
        Owner::Npm | Owner::Npx => &["config", "get", "cache"],
        Owner::Uv | Owner::Pip => &["cache", "dir"],
        Owner::Cypress => &["cache", "path"],
        _ => return Err("Owner is inspection-only".into()),
    });
    let output = probe.output(&command, env)?;
    // Remove the CLI's line terminator, never whitespace that can be part
    // of a real directory name. Guessing here can select a different tree.
    // The supported Unix adapters emit LF. A CR is ambiguous with a
    // literal path character, so refuse it rather than guess at CRLF.
    let value = output.strip_suffix('\n').unwrap_or(&output);
    let cache_root = PathBuf::from(value);
    if !cache_root.is_absolute() || value.contains('\n') || value.contains('\r')
    {
        return Err("Owner returned an ambiguous or relative cache root".into());
    }
    if cache_root
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("Owner cache root contains parent traversal; inspect its absolute configured path".into());
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
        .ok_or("Non-UTF-8 cache roots are unsupported")?
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
        _ => return Err("Owner is inspection-only".into()),
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
        owner, version, selected: vec![selected.to_owned()], cache_root, scope, command,
        effects: effects.into(),
        limitations: "The command acts on the WHOLE scope, including unselected siblings. Permanent owner cleanup has no Trash recovery. Listed allocation is an upper bound, not promised savings. Other activity, open files, hardlinks, APFS clones and snapshots can retain blocks. Active-use detection is best effort; stop the owning tools first. PwrDisk rescans afterwards. No raw deletion fallback.".into(),
    }))
}

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
    if supported {
        Ok(())
    } else {
        Err(format!(
            "Unsupported {} version {text}; inspect with the owner, no deletion fallback",
            owner.label()
        ))
    }
}

impl Prepared {
    /// Re-probe against the captured environment before any write, then
    /// recheck the complete actual scopes and confirmed per-volume ceilings.
    pub(crate) fn preflight(
        &self,
        limit: &crate::removal_limit::Ceilings,
    ) -> Result<(), String> {
        for (operation, identity) in
            self.operations.iter().zip(&self.identities)
        {
            if process::Identity::read(&operation.command.executable)?
                != *identity
            {
                return Err("Owner executable changed after preview; obtain new approval".into());
            }
            let fresh = resolve_owner(operation.owner, &operation.scope, &self.environment, &mut process::SystemProbe)?.ok_or("Configured cache root changed after preview; obtain new approval")?;
            if fresh.command != operation.command
                || fresh.version != operation.version
                || fresh.scope != operation.scope
            {
                return Err("Owner command, version or cache scope changed; obtain new approval".into());
            }
            process::check_tree(&operation.scope)?;
            process::check_active(operation)?;
        }
        let fresh = crate::removal::plan(&self.plan.targets, &self.plan.root);
        if !fresh.blocked.is_empty()
            || fresh.targets.len() != self.plan.targets.len()
        {
            return Err(
                "Owner cleanup scopes no longer pass removal guards".into()
            );
        }
        limit.check(&crate::removal_limit::measured(&fresh.targets)?)?;
        Ok(())
    }

    pub(crate) fn execute(
        &self,
        index: usize,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let operation = self
            .operations
            .get(index)
            .ok_or("Missing owner operation")?;
        // Recheck active use between commands, too. Failure never calls a
        // generic remover, even when the owner partially changed the cache.
        process::check_active(operation)?;
        process::run(
            &operation.command,
            &self.environment,
            cancel,
            std::time::Duration::from_mins(10),
        )
        .map(|_| ())
    }
}
