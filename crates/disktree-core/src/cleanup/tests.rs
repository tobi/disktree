use std::collections::BTreeMap;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::time::{Duration, Instant};

use super::*;
use Decision::{DeleteFiles, Owner as Run, Skip};

mod properties;

// These tests deliberately create live processes named uv. Serialize them
// so one fixture cannot trip another fixture's active-owner guard.
#[cfg(unix)]
static EXECUTION: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    env: Environment,
    executable: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        #[cfg(unix)]
        let temp = tempfile::tempdir_in("/tmp").expect("scratch fixture");
        #[cfg(not(unix))]
        let temp = tempfile::tempdir().expect("fixture");
        let root = std::fs::canonicalize(temp.path()).expect("real root");
        let home = root.join("home");
        std::fs::create_dir(&home).expect("home");
        let executable = root.join("fixture-cli");
        std::fs::write(&executable, "mock identity")
            .expect("executable identity");
        let env = Environment {
            home,
            variables: BTreeMap::new(),
        };
        Self {
            _temp: temp,
            root,
            env,
            executable,
        }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.env.home.join(name);
        std::fs::create_dir_all(&path).expect("cache");
        std::fs::write(path.join("payload"), vec![0; 8192]).expect("payload");
        path
    }

    fn probe(&self, name: &str, version: &str, cache: &Path) -> MockProbe {
        MockProbe {
            executable: self.executable.clone(),
            name: name.into(),
            version: version.into(),
            cache: cache.to_owned(),
            calls: vec![],
        }
    }
}

struct MockProbe {
    executable: PathBuf,
    name: String,
    version: String,
    cache: PathBuf,
    calls: Vec<Command>,
}

impl Probe for MockProbe {
    fn executable(
        &mut self,
        name: &str,
        _: &Environment,
    ) -> Result<PathBuf, Issue> {
        if name == self.name {
            Ok(self.executable.clone())
        } else {
            Err(Issue::new(
                Problem::NotFound,
                format!(
                    "{name} unavailable on PATH; software may still be installed"
                ),
            ))
        }
    }

    fn output(
        &mut self,
        command: &Command,
        _: &Environment,
    ) -> Result<String, String> {
        self.calls.push(command.clone());
        match command
            .arguments
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            ["--version"] | ["version", "--component", "package"] => {
                Ok(self.version.clone())
            }
            ["config", "get", "cache"] | ["cache", "dir" | "path"] => {
                Ok(self.cache.to_str().expect("UTF-8").into())
            }
            _ => panic!("Preview attempted a write: {command:?}"),
        }
    }
}

/// Why the only marked path has no owner command. Preparation is best
/// effort, so a refusal is that proposal's status, never an error.
#[cfg(unix)]
fn owner_error(prepared: Result<Prepared, String>) -> String {
    owner_issue(prepared).detail
}

fn owner_issue(prepared: Result<Prepared, String>) -> Issue {
    let prepared = prepared.expect("preparation reports per path");
    let proposal = &prepared.proposals[0];
    assert!(proposal.operations.is_empty(), "no command: {proposal:?}");
    proposal.issue.clone().expect("the proposal says why")
}

/// The person chose the owner command for every marked path.
#[cfg(unix)]
fn owners(prepared: &Prepared) -> Approved {
    prepared
        .approve(
            &vec![Decision::Owner; prepared.proposals.len()],
            Action::Skip,
        )
        .expect("owner commands approved")
}

fn target(path: &Path) -> Target {
    Target {
        path: path.to_owned(),
        bytes: 1,
        is_dir: true,
        hidden: false,
    }
}

#[test]
fn npm_package_and_npx_have_separate_commands_scopes_and_versions() {
    let fixture = Fixture::new();
    let cache = fixture.env.home.join(".npm");
    let package = fixture.dir(".npm/_cacache");
    let npx = fixture.dir(".npm/_npx");
    for (path, owner, expected) in [
        (
            &package,
            Owner::Npm,
            strings(&["cache", "clean", "--force", "--cache"]),
        ),
        (
            &npx,
            Owner::Npx,
            strings(&["cache", "npx", "rm", "--force", "--cache"]),
        ),
    ] {
        let mut probe = fixture.probe("npm", "11.6.2", &cache);
        let prepared = prepare_with(
            &[target(path)],
            &fixture.root,
            fixture.env.clone(),
            &mut probe,
        )
        .expect("preview");
        let operation = &prepared.operations[0];
        assert_eq!(operation.owner, owner);
        assert_eq!(&operation.scope, path);
        let mut expected = expected;
        expected.push(cache.to_str().expect("cache").into());
        assert_eq!(operation.command.arguments, expected);
        assert_eq!(
            operation.command.environment["npm_config_logs_dir"],
            "/dev/null"
        );
        assert_eq!(
            probe.calls.len(),
            2,
            "only version and configuration probes"
        );
        assert!(path.join("payload").exists());
    }
    let mut probe = fixture.probe("npm", "11.6.2", &cache);
    let prepared = prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut probe,
    )
    .expect("both caches explicitly previewed");
    assert_eq!(prepared.operations.len(), 2);
    assert_eq!(prepared.scope_bytes.len(), 2);
    assert_eq!(prepared.proposals[0].operations, [0, 1]);
    assert_eq!(prepared.operations[0].scope, package);
    assert_eq!(prepared.operations[1].scope, npx);
}

#[test]
fn custom_roots_are_queried_and_subtrees_expand_only_to_the_verified_scope() {
    let fixture = Fixture::new();
    let cache = fixture.dir("custom data; $(touch sentinel)");
    let selected = cache.join("one-package");
    std::fs::create_dir(&selected).expect("selected");
    let mut probe = fixture.probe("uv", "uv 0.11.0 (fixture)", &cache);
    let prepared = prepare_with(
        &[target(&selected)],
        &fixture.root,
        fixture.env.clone(),
        &mut probe,
    )
    .expect("configured root");
    let operation = &prepared.operations[0];
    assert_eq!(operation.scope, cache);
    assert_eq!(operation.selected, [selected]);
    assert_eq!(
        operation.command.arguments,
        [
            "cache",
            "clean",
            "--cache-dir",
            cache.to_str().expect("path")
        ]
    );
    assert!(
        prepared.scope_bytes[0] >= 8192,
        "includes unselected siblings"
    );
    assert!(operation.description().contains("WHOLE scope"));
    assert!(operation.description().contains("APFS"));
    assert!(!fixture.env.home.join("sentinel").exists());
    let mut probe = fixture.probe("uv", "uv 0.11.0", &cache);
    let other = fixture.dir(".cache/uv");
    assert_eq!(
        owner_issue(prepare_with(
            &[target(&other)],
            &fixture.root,
            fixture.env.clone(),
            &mut probe
        ))
        .problem,
        Problem::NotConfiguredCache,
        "do not clean the configured cache instead of a stale default selection"
    );
}

#[test]
fn pip_and_cypress_pin_the_resolved_root_in_arguments_or_environment() {
    let fixture = Fixture::new();
    for (name, version, relative, owner) in [
        (
            "pip",
            "pip 26.0.1 from /fixture/pip (python 3.14)",
            "Library/Caches/pip",
            Owner::Pip,
        ),
        (
            "cypress",
            "15.5.0",
            "Library/Caches/Cypress",
            Owner::Cypress,
        ),
    ] {
        let cache = fixture.dir(relative);
        let mut probe = fixture.probe(name, version, &cache);
        let prepared = prepare_with(
            &[target(&cache)],
            &fixture.root,
            fixture.env.clone(),
            &mut probe,
        )
        .expect("preview");
        let operation = &prepared.operations[0];
        assert_eq!(operation.owner, owner);
        if owner == Owner::Pip {
            assert_eq!(
                operation.command.arguments,
                [
                    "--cache-dir",
                    cache.to_str().expect("path"),
                    "cache",
                    "purge"
                ]
            );
        } else {
            assert_eq!(operation.command.arguments, ["cache", "clear"]);
            assert_eq!(
                operation.command.environment["CYPRESS_CACHE_FOLDER"],
                cache.to_str().expect("path")
            );
            assert!(operation.effects.contains("other projects"));
        }
    }
}

#[test]
fn unknown_unsupported_and_missing_owners_do_not_produce_a_write() {
    let fixture = Fixture::new();
    let uv = fixture.dir(".cache/uv");
    let mut missing = fixture.probe("absent", "", &uv);
    let issue = owner_issue(prepare_with(
        &[target(&uv)],
        &fixture.root,
        fixture.env.clone(),
        &mut missing,
    ));
    assert_eq!(issue.problem, Problem::NotFound);
    assert!(issue.detail.contains("software may still be installed"));
    for name in [
        "Library/pnpm/store",
        "Library/Caches/Homebrew",
        "Library/Caches/ms-playwright",
        ".cache/puppeteer",
        ".cache/codex-runtimes",
        ".cache/gh",
        "Library/Caches/Electron",
        "Library/Caches/vscode-cpptools",
    ] {
        let path = fixture.dir(name);
        let mut probe = fixture.probe("absent", "", &path);
        let prepared = prepare_with(
            &[target(&path)],
            &fixture.root,
            fixture.env.clone(),
            &mut probe,
        )
        .expect("per-path status");
        let proposal = &prepared.proposals[0];
        assert!(proposal.operations.is_empty(), "{name}");
        let problem = proposal.issue.as_ref().expect("why").problem;
        // Known owners without an audited command, or no verified owner.
        assert!(
            matches!(problem, Problem::NoCommand | Problem::UnknownOwner),
            "{name}: {problem:?}"
        );
        let warning = &proposal.delete.as_ref().expect("offered").warning;
        if problem == Problem::UnknownOwner {
            assert!(
                warning.contains("does not know that this is disposable"),
                "{name}: unknown caches are never labelled safe"
            );
        }
        assert!(path.join("payload").exists());
    }
}

#[test]
fn version_bounds_and_prereleases_are_explicit() {
    for (owner, version) in [
        (Owner::Npx, "11.1.0"),
        (Owner::Npm, "12.0.0"),
        (Owner::Uv, "uv 0.10.0"),
        (Owner::Pip, "pip 27.0 from /fixture"),
        (Owner::Cypress, "16.0.0"),
        (Owner::Npm, "11.6.2-beta.1"),
        (Owner::Pip, "unknown"),
    ] {
        assert!(
            supported_version(owner, version).is_err(),
            "{owner:?} {version}"
        );
    }
    for (owner, version) in [
        (Owner::Npx, "11.2.0"),
        (Owner::Npm, "9.0.0"),
        (Owner::Uv, "uv 0.11.0"),
        (Owner::Pip, "pip 23.0 from /fixture"),
        (Owner::Cypress, "13.0.0"),
    ] {
        supported_version(owner, version).expect("audited family");
    }
}

#[test]
fn actual_scope_must_be_beneath_scan_and_must_not_be_the_scan_root() {
    let fixture = Fixture::new();
    let cache = fixture.dir(".cache/uv");
    let selected = cache.join("selected");
    std::fs::create_dir(&selected).expect("selected");
    for root in [&selected, &cache] {
        let mut probe = fixture.probe("uv", "uv 0.11.0", &cache);
        let issue = owner_issue(prepare_with(
            &[target(&selected)],
            root,
            fixture.env.clone(),
            &mut probe,
        ));
        assert_eq!(issue.problem, Problem::Refused);
    }
    let mut probe =
        fixture.probe("uv", "uv 0.11.0", Path::new("relative-cache"));
    let issue = owner_issue(prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut probe,
    ));
    assert_eq!(issue.problem, Problem::ConfigCheckFailed);
    assert!(issue.detail.contains("relative"));
}

#[test]
fn raw_removal_refuses_cache_wrappers_and_descendants_but_accepts_build_output()
{
    let fixture = Fixture::new();
    let cache = fixture.dir("wrapper/.cache/unknown-runtime");
    assert!(check_raw(&[target(&cache)]).is_err());
    assert!(check_raw(&[target(&fixture.env.home.join("wrapper"))]).is_err());
    let build = fixture.dir("project/target/debug");
    check_raw(&[target(&build)]).expect("ordinary build output");
    assert!(cache.join("payload").exists());
}

#[test]
fn relocated_cache_ancestors_protect_subtrees_and_files() {
    let fixture = Fixture::new();
    let tagged = fixture.dir("uv-data/archive-v0/one/two");
    std::fs::write(
        fixture.env.home.join("uv-data/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55",
    )
    .expect("cache tag");
    let pip = fixture.dir("pip-data/wheels/deep/entries");
    fixture.dir("pip-data/http-v2");
    let pnpm = fixture.dir("pnpm-data/v10/files/deep/entries");
    let cypress = fixture.dir("browser-data/15.0.0/Cypress.app/deep/entries");
    for path in [tagged, pip, pnpm, cypress] {
        for selected in [path.clone(), path.join("payload")] {
            assert!(
                recognize::inspection_with(&selected, &fixture.env).is_some(),
                "relocated ancestor must protect {selected:?}"
            );
            assert!(check_raw(&[target(&selected)]).is_err());
            assert!(selected.exists(), "inspection cannot delete");
        }
    }
    let ordinary = fixture.dir("ordinary/project/target");
    check_raw(&[target(&ordinary)]).expect("unrelated build output");
}

#[cfg(unix)]
#[test]
fn external_cache_links_are_refused_while_internal_layout_links_are_allowed() {
    let fixture = Fixture::new();
    let cache = fixture.dir(".cache/uv");
    std::os::unix::fs::symlink(cache.join("payload"), cache.join("internal"))
        .expect("internal link");
    process::check_tree(&cache).expect("internal link");
    let outside = fixture.dir("outside");
    std::os::unix::fs::symlink(&outside, cache.join("external"))
        .expect("external link");
    assert!(
        process::check_tree(&cache)
            .expect_err("external link")
            .contains("external symlink")
    );
    assert!(outside.join("payload").exists());
}

#[cfg(unix)]
fn executable_fixture() -> Fixture {
    use std::os::unix::fs::PermissionsExt as _;
    let mut fixture = Fixture::new();
    let cache = fixture.dir(".cache/uv");
    let bin = fixture.root.join("bin");
    std::fs::create_dir(&bin).expect("bin");
    fixture.executable = bin.join("uv");
    std::fs::write(&fixture.executable, r#"#!/bin/sh
case "$1 $2" in
  '--version ') printf '%s\n' 'uv 0.11.0';;
  'cache dir') if [ -f "$HOME/cache-root" ]; then /bin/cat "$HOME/cache-root"; else printf '%s\n' "$UV_CACHE_DIR"; fi;;
  'cache clean')
    printf '%s\n' "$@" > "$HOME/invocation"
    if [ "$FIXTURE_WAIT" = 1 ]; then /bin/sleep 20; fi
    if [ "$FIXTURE_FAIL" = 1 ]; then printf '%s\n' 'fixture owner failure' >&2; exit 17; fi
    printf '%s\n' 'fixture success; root remains';;
  'hold ') /bin/sleep 20;;
  *) exit 97;;
esac
"#).expect("fixture CLI");
    std::fs::set_permissions(
        &fixture.executable,
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("executable");
    fixture.env.variables.extend([
        ("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())),
        (
            "HOME".into(),
            fixture.env.home.to_str().expect("home").into(),
        ),
        ("UV_CACHE_DIR".into(), cache.to_str().expect("cache").into()),
    ]);
    fixture
}

#[cfg(unix)]
fn collect(
    handle: &crate::removal::RemovalHandle,
) -> Vec<crate::removal::RemovalEvent> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut events = Vec::new();
    loop {
        if let Some(event) = handle.poll() {
            let done =
                matches!(event, crate::removal::RemovalEvent::Done { .. });
            events.push(event);
            if done {
                return events;
            }
        }
        assert!(Instant::now() < deadline, "fixture worker timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn owner_failure_reports_error_without_deletion_and_success_does_not_mean_root_removed()
 {
    let _execution = EXECUTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for fail in [true, false] {
        let mut fixture = executable_fixture();
        if fail {
            fixture
                .env
                .variables
                .insert("FIXTURE_FAIL".into(), "1".into());
        }
        let cache = fixture.env.home.join(".cache/uv");
        let prepared = prepare_with(
            &[target(&cache)],
            &fixture.root,
            fixture.env.clone(),
            &mut process::SystemProbe,
        )
        .expect("fixture preview");
        assert!(
            !fixture.env.home.join("invocation").exists(),
            "preview is read-only"
        );
        let events = collect(&crate::removal::spawn_cleanup(
            owners(&prepared),
            crate::removal_limit::Limit::default().into(),
        ));
        let outcome = events
            .iter()
            .find_map(|event| {
                if let crate::removal::RemovalEvent::Item { outcome, .. } =
                    event
                {
                    Some(outcome)
                } else {
                    None
                }
            })
            .expect("item result");
        assert_eq!(outcome.is_err(), fail, "{events:?}");
        if fail {
            assert!(
                outcome
                    .as_ref()
                    .expect_err("failed")
                    .contains("Nothing else was deleted"),
                "{events:?}"
            );
        }
        assert!(
            cache.join("payload").exists(),
            "owner result never substitutes raw removal"
        );
        assert!(
            fixture.env.home.join("invocation").exists(),
            "mock owner was invoked"
        );
    }
}

#[cfg(unix)]
#[test]
fn owner_worker_applies_confirmed_ceilings_only_to_the_named_volume() {
    use crate::removal::{RemovalEvent, spawn_cleanup};
    use crate::removal_limit::{Ceilings, Limit, volume_of};

    let _execution = EXECUTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = executable_fixture();
    let cache = fixture.env.home.join(".cache/uv");
    let prepared = prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    )
    .expect("fixture preview");
    let mut prepared = owners(&prepared);
    let (volume, capacity) = volume_of(&cache).expect("fixture volume");
    // Charge a share just past 5% without allocating gigabytes of data.
    // Worker measurement must preserve this conservative preview charge.
    prepared.plan.targets[0].bytes = capacity / 20 + 1;
    let other = Ceilings::from(Limit::default()).raise(
        format!("{volume}-other-fixture"),
        Limit::new(100).expect("100%"),
    );
    let refused = collect(&spawn_cleanup(prepared.clone(), other));
    assert!(refused.iter().any(|event| matches!(event,
        RemovalEvent::Item { outcome: Err(reason), .. }
            if reason.contains("5% per-operation"))));
    assert!(!fixture.env.home.join("invocation").exists());

    let approved = Ceilings::from(Limit::default())
        .raise(volume, Limit::new(10).expect("10%"));
    let completed = collect(&spawn_cleanup(prepared, approved));
    assert!(completed.iter().any(|event| matches!(
        event,
        RemovalEvent::Item {
            outcome: Ok(()),
            ..
        }
    )));
    assert!(fixture.env.home.join("invocation").exists());
    assert!(
        cache.join("payload").exists(),
        "mock never deletes the cache"
    );
}

#[cfg(unix)]
#[test]
fn cancellation_stops_the_owner_process_group_without_raw_deletion() {
    let _execution = EXECUTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut fixture = executable_fixture();
    fixture
        .env
        .variables
        .insert("FIXTURE_WAIT".into(), "1".into());
    let cache = fixture.env.home.join(".cache/uv");
    let prepared = prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    )
    .expect("preview");
    let handle = crate::removal::spawn_cleanup(
        owners(&prepared),
        crate::removal_limit::Limit::default().into(),
    );
    let start = Instant::now();
    while !fixture.env.home.join("invocation").exists() {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    handle.cancel();
    let events = collect(&handle);
    assert!(start.elapsed() < Duration::from_secs(10));
    assert!(events.iter().any(|event| matches!(event, crate::removal::RemovalEvent::Item { outcome: Err(reason), .. } if reason.contains("cancelled"))));
    assert!(cache.join("payload").exists());
}

#[cfg(unix)]
#[test]
fn changed_executable_and_active_owner_revoke_execution() {
    let _execution = EXECUTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = executable_fixture();
    let cache = fixture.env.home.join(".cache/uv");
    let prepared = prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    )
    .expect("preview");
    let mut active = std::process::Command::new(&fixture.executable)
        .arg("hold")
        .spawn()
        .expect("fixture active owner");
    std::thread::sleep(Duration::from_millis(100));
    let active_check = prepared.preflight_operation(0);
    active.kill().expect("stop fixture");
    active.wait().expect("wait fixture");
    assert_eq!(
        active_check.expect_err("active owner").problem,
        Problem::InUse
    );
    std::fs::write(&fixture.executable, "changed fixture binary")
        .expect("replace executable");
    assert_eq!(
        prepared
            .preflight_operation(0)
            .expect_err("changed executable")
            .problem,
        Problem::Changed
    );
    assert!(!fixture.env.home.join("invocation").exists());
}

#[cfg(unix)]
#[test]
fn cancelled_before_start_and_unavailable_commands_do_not_execute() {
    let fixture = executable_fixture();
    let command = Command {
        executable: fixture.executable.clone(),
        arguments: strings(&["cache", "clean"]),
        environment: BTreeMap::new(),
        working_directory: fixture.env.home.clone(),
    };
    let cancel = AtomicBool::new(true);
    assert!(
        process::run(&command, &fixture.env, &cancel, Duration::from_secs(1))
            .expect_err("cancelled")
            .detail
            .contains("before command")
    );
    cancel.store(false, Ordering::Relaxed);
    let mut unavailable = command;
    unavailable.executable = fixture.root.join("missing");
    assert!(
        process::run(
            &unavailable,
            &fixture.env,
            &cancel,
            Duration::from_secs(1)
        )
        .expect_err("unavailable")
        .detail
        .contains("could not start")
    );
    assert!(!fixture.env.home.join("invocation").exists());
}

#[cfg(unix)]
#[test]
fn changed_configuration_and_capacity_failure_do_not_invoke_cleanup() {
    let _execution = EXECUTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = executable_fixture();
    let cache = fixture.env.home.join(".cache/uv");
    let prepared = prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    )
    .expect("preview");
    let mut approved = owners(&prepared);
    approved.plan.targets[0].bytes = u64::MAX;
    assert!(
        approved
            .admit(&crate::removal_limit::Limit::default().into())
            .is_err(),
        "the capacity limit covers the entire actual scope"
    );
    let other = fixture.dir("other-cache");
    std::fs::write(
        fixture.env.home.join("cache-root"),
        format!("{}\n", other.display()),
    )
    .expect("changed config");
    assert_eq!(
        prepared
            .preflight_operation(0)
            .expect_err("changed root")
            .problem,
        Problem::Changed
    );
    assert!(!fixture.env.home.join("invocation").exists());
    assert!(cache.join("payload").exists());
    assert!(other.join("payload").exists());
}

#[test]
fn observed_space_can_decrease_and_never_equals_scope_allocation_by_assumption()
{
    let path = Path::new("/fixture/cache");
    let success = Report::of(
        Action::Owner,
        Some(Owner::Uv),
        path,
        50_000,
        Some(100),
        Some(90),
        Ok(()),
    );
    assert_eq!(success.outcome, Outcome::Succeeded);
    assert_eq!(success.scope_allocation_at_preview, 50_000);
    assert_eq!(success.observed_change, Some(-10));
    assert!(success.description().contains("not savings"));
    let failure = Report::of(
        Action::Owner,
        Some(Owner::Uv),
        path,
        50_000,
        Some(100),
        Some(105),
        Err(&Issue::new(Problem::CommandFailed, "owner exit 17")),
    );
    assert_eq!(failure.outcome, Outcome::Failed);
    assert_eq!(failure.problem, Some(Problem::CommandFailed));
    assert_eq!(
        failure.observed_change,
        Some(5),
        "partial failure can still change free space"
    );
    assert_eq!(failure.error.as_deref(), Some("owner exit 17"));
    assert!(failure.description().contains("Nothing else was deleted"));
    let cancelled = Report::of(
        Action::Owner,
        Some(Owner::Uv),
        path,
        50_000,
        None,
        None,
        Err(&Issue::new(Problem::Cancelled, "stopped")),
    );
    assert_eq!(cancelled.outcome, Outcome::Cancelled);
}

#[test]
fn active_owner_detection_recognizes_node_launchers_and_pip3() {
    let owned = |operation: &Operation, argument: &str| {
        process::owner_named(
            operation.owner,
            Some(&operation.command.executable),
            argument,
        )
    };
    let mut operation = Operation {
        owner: Owner::Npm,
        version: "11.6.2".into(),
        selected: vec![],
        cache_root: "/fixture/.npm".into(),
        scope: "/fixture/.npm/_cacache".into(),
        command: Command {
            executable: "/fixture/npm-cli.js".into(),
            arguments: vec![],
            environment: BTreeMap::new(),
            working_directory: "/fixture".into(),
        },
        effects: String::new(),
        limitations: String::new(),
    };
    assert!(owned(&operation, "/other-install/npm-cli.js"));
    assert!(owned(&operation, "/other-install/npx-cli.js"));
    assert!(!owned(&operation, "/fixture/unrelated.js"));
    operation.owner = Owner::Pip;
    assert!(owned(&operation, "/fixture/bin/pip3"));
    assert!(owned(&operation, "pip"));
    // Found by a_running_owner_is_recognized_in_every_launcher_form.
    assert!(owned(&operation, "/opt/bin/pip3.12"));
    assert!(!owned(&operation, "/opt/bin/pip3.x"));
    operation.owner = Owner::Uv;
    assert!(owned(&operation, "/opt/bin/uvx"));
}

#[cfg(unix)]
#[test]
fn executable_resolution_uses_pip3_and_refuses_nonexecutable_files() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = executable_fixture();
    let bin = fixture.executable.parent().expect("bin");
    let pip3 = bin.join("pip3");
    std::fs::write(&pip3, "fixture executable identity").expect("pip3");
    std::fs::set_permissions(&pip3, std::fs::Permissions::from_mode(0o755))
        .expect("executable");
    let mut env = fixture.env.clone();
    env.variables
        .insert("PATH".into(), format!("relative:{}", bin.display()));
    assert_eq!(
        process::SystemProbe
            .executable("pip", &env)
            .expect("pip3 fallback"),
        pip3
    );
    std::fs::set_permissions(
        &fixture.executable,
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("not executable");
    assert!(
        process::SystemProbe
            .executable("uv", &env)
            .expect_err("unavailable command")
            .detail
            .contains("not executable")
    );
}

#[cfg(unix)]
#[test]
fn preparation_refuses_installing_shims_and_shimmed_env_interpreters() {
    use std::os::unix::fs::PermissionsExt as _;
    let mut fixture = executable_fixture();
    let cache = fixture.env.home.join(".cache/uv");
    let shims = fixture.root.join("shims");
    std::fs::create_dir(&shims).expect("shim directory");
    let shim = shims.join("uv");
    std::fs::write(
        &shim,
        "#!/bin/sh\nprintf installed > \"$HOME/unwanted-install\"\n",
    )
    .expect("installing shim fixture");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
        .expect("executable shim");
    fixture
        .env
        .variables
        .insert("PATH".into(), shims.to_str().expect("path").into());
    let issue = owner_issue(prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    ));
    assert_eq!(issue.problem, Problem::UnsupportedLauncher);
    assert!(issue.detail.contains("shim"));
    assert!(!fixture.env.home.join("unwanted-install").exists());

    let bin = fixture.executable.parent().expect("bin");
    std::fs::write(
        &fixture.executable,
        "#!/usr/bin/env node\nprintf installed > \"$HOME/unwanted-install\"\n",
    )
    .expect("env runtime fixture");
    std::os::unix::fs::symlink(&shim, bin.join("node")).expect("shimmed node");
    fixture
        .env
        .variables
        .insert("PATH".into(), bin.to_str().expect("bin").into());
    assert!(
        owner_error(prepare_with(
            &[target(&cache)],
            &fixture.root,
            fixture.env.clone(),
            &mut process::SystemProbe
        ))
        .contains("shim"),
        "do not dispatch env node through a shim"
    );
    assert!(!fixture.env.home.join("unwanted-install").exists());
}

#[cfg(unix)]
#[test]
fn npm_installing_proxy_is_not_invoked_even_for_version_discovery() {
    use std::os::unix::fs::PermissionsExt as _;
    let mut fixture = executable_fixture();
    let cache = fixture.dir(".npm/_cacache");
    let bin = fixture.executable.parent().expect("bin");
    let proxy = bin.join("npm");
    std::fs::write(
        &proxy,
        "#!/bin/sh\nprintf installed > \"$HOME/unwanted-install\"\n",
    )
    .expect("proxy fixture");
    std::fs::set_permissions(&proxy, std::fs::Permissions::from_mode(0o755))
        .expect("executable proxy");
    fixture
        .env
        .variables
        .insert("PATH".into(), bin.to_str().expect("bin").into());
    let error = owner_error(prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    ));
    assert!(error.contains("proxy/wrapper"));
    assert!(!fixture.env.home.join("unwanted-install").exists());
}

#[cfg(unix)]
#[test]
fn configured_root_whitespace_cannot_redirect_cleanup_to_a_similar_path() {
    let fixture = Fixture::new();
    let selected = fixture.dir("custom-cache");
    let configured = fixture.dir("custom-cache ");
    for suffix in ["\r\n", "\n\n"] {
        let ambiguous =
            PathBuf::from(format!("{}{suffix}", selected.display()));
        let mut probe = fixture.probe("uv", "uv 0.11.0", &ambiguous);
        owner_issue(prepare_with(
            &[target(&selected)],
            &fixture.root,
            fixture.env.clone(),
            &mut probe,
        ));
    }
    let mut probe = fixture.probe("uv", "uv 0.11.0", &configured);
    // A trailing space is part of the configured path, not output padding:
    // no supported owner claims the selection, so none is offered.
    assert_eq!(
        owner_issue(prepare_with(
            &[target(&selected)],
            &fixture.root,
            fixture.env.clone(),
            &mut probe,
        ))
        .problem,
        Problem::UnknownOwner
    );
    let mut probe = fixture.probe("uv", "uv 0.11.0", &configured);
    let prepared = prepare_with(
        &[target(&configured)],
        &fixture.root,
        fixture.env.clone(),
        &mut probe,
    )
    .expect("exact configured path");
    assert_eq!(prepared.operations[0].scope, configured);
    assert_eq!(
        prepared.operations[0]
            .command
            .arguments
            .last()
            .expect("cache argument"),
        configured.to_str().expect("path")
    );
    assert!(selected.join("payload").exists());
    assert!(configured.join("payload").exists());
}

#[cfg(unix)]
#[test]
fn non_utf8_probe_output_is_refused_without_lossy_path_conversion() {
    let fixture = executable_fixture();
    let cache = fixture.env.home.join(".cache/uv");
    std::fs::write(&fixture.executable, "#!/bin/sh\nprintf '\\377\\n'\n")
        .expect("non-UTF-8 fixture probe");
    let error = owner_error(prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    ));
    assert!(error.contains("non-UTF-8"));
    assert!(cache.join("payload").exists());
    assert!(!fixture.env.home.join("invocation").exists());
}

/// Three marked paths, three different answers: one owner resolves, one
/// owner has no audited command, one has no verified owner at all.
fn mixed(fixture: &Fixture) -> (Prepared, [PathBuf; 3]) {
    let uv = fixture.dir(".cache/uv");
    let pnpm = fixture.dir("Library/pnpm/store");
    let runtime = fixture.dir(".cache/codex-runtimes");
    let mut probe = fixture.probe("uv", "uv 0.11.0", &uv);
    let prepared = prepare_with(
        &[target(&uv), target(&pnpm), target(&runtime)],
        &fixture.root,
        fixture.env.clone(),
        &mut probe,
    )
    .expect("best-effort preview");
    (prepared, [uv, pnpm, runtime])
}

#[test]
fn one_failure_is_one_proposal_and_the_other_paths_stay_offered() {
    let fixture = Fixture::new();
    let (prepared, [uv, pnpm, runtime]) = mixed(&fixture);
    assert_eq!(prepared.proposals.len(), 3);
    assert_eq!(prepared.operations.len(), 1);
    assert_eq!(prepared.operations[0].scope, uv);
    let [ready, inspection, unknown] = &prepared.proposals[..] else {
        panic!("three proposals");
    };
    assert_eq!(ready.operations, [0]);
    assert!(ready.issue.is_none());
    assert_eq!(inspection.selected, pnpm);
    assert_eq!(
        inspection.issue.as_ref().map(|issue| issue.problem),
        Some(Problem::NoCommand)
    );
    assert_eq!(unknown.selected, runtime);
    assert_eq!(
        unknown.issue.as_ref().map(|issue| issue.problem),
        Some(Problem::UnknownOwner)
    );
    // Delete files is offered everywhere the guards allow it, and says
    // what it bypasses; an unknown runtime is never called disposable.
    for proposal in &prepared.proposals {
        assert!(proposal.allows(Decision::DeleteFiles));
        assert!(proposal.allows(Decision::Skip));
    }
    assert!(
        ready
            .delete
            .as_ref()
            .expect("offered")
            .warning
            .contains("uv cache is not involved")
    );
    assert!(
        unknown
            .delete
            .as_ref()
            .expect("offered")
            .warning
            .contains("does not know that this is disposable")
    );
    let text = prepared.description();
    assert!(text.contains("Owner command (choose owner)"));
    assert!(text.contains("no cleanup command"));
    assert!(text.contains("unknown owner"));
    for path in [uv, pnpm, runtime] {
        assert!(path.join("payload").exists(), "preview is read-only");
    }

    let unsupported = fixture.dir("other/.cache/uv");
    let mut env = fixture.env.clone();
    env.variables
        .insert("UV_CACHE_DIR".into(), unsupported.display().to_string());
    let mut probe = fixture.probe("uv", "uv 0.10.4", &unsupported);
    let issue = owner_issue(prepare_with(
        &[target(&unsupported)],
        &fixture.root,
        env,
        &mut probe,
    ));
    assert_eq!(issue.problem, Problem::UnsupportedVersion);
    assert!(issue.detail.contains("0.10.4"), "{issue:?}");
    assert!(issue.detail.contains("uv 0.11.x"), "says what runs");
}

#[test]
fn decisions_are_explicit_and_each_intent_has_its_own_confirmation() {
    let fixture = Fixture::new();
    let (prepared, [uv, pnpm, runtime]) = mixed(&fixture);
    assert!(
        prepared
            .approve(&[Skip, Skip, Skip], Action::Trash)
            .expect_err("nothing chosen")
            .contains("Nothing chosen")
    );
    assert!(prepared.approve(&[Run, Skip], Action::Trash).is_err());
    assert!(
        prepared
            .approve(&[Skip, Run, Skip], Action::Trash)
            .expect_err("pnpm offers no command")
            .contains("owner command")
    );
    assert!(
        prepared
            .approve(&[Skip, Skip, DeleteFiles], Action::Owner)
            .expect_err("delete needs a mode")
            .contains("Trash or permanent")
    );

    let owner = prepared
        .approve(&[Run, Skip, Skip], Action::Skip)
        .expect("owner");
    assert_eq!(owner.confirmation(), RUN_OWNER_CLEANUP);
    assert_eq!(owner.steps, [Step::Owner(0)]);
    let trash = prepared
        .approve(&[Skip, Skip, DeleteFiles], Action::Trash)
        .expect("trash files");
    assert_eq!(trash.confirmation(), TRASH_CACHE_FILES);
    assert_eq!(
        prepared
            .approve(&[Skip, DeleteFiles, Skip], Action::Permanent)
            .expect("delete files")
            .confirmation(),
        DELETE_CACHE_FILES
    );

    let both = prepared
        .approve(&[Run, Skip, DeleteFiles], Action::Permanent)
        .expect("both intents");
    assert_eq!(both.confirmation(), RUN_AND_DELETE);
    assert_eq!(both.owner_steps(), 1);
    assert_eq!(both.file_steps(), 1);
    assert_eq!(both.plan.targets.len(), 2);
    // Owner approval reaches the owner's scope; deletion reaches exactly
    // the selected path and nothing wider.
    let paths: Vec<_> = both
        .plan
        .targets
        .iter()
        .map(|target| &target.path)
        .collect();
    assert!(paths.contains(&&uv));
    assert!(paths.contains(&&runtime));
    assert!(!paths.contains(&&pnpm));
    assert_eq!(both.skipped.len(), 1);
    assert_eq!(both.skipped[0].path, pnpm);
    assert_eq!(both.skipped[0].outcome, Outcome::Skipped);
    assert_eq!(both.skipped[0].problem, Some(Problem::NoCommand));
    let text = both.description();
    assert!(text.contains("Run owner commands (1)"));
    assert!(
        text.contains("Delete files directly, not through their owner (1)")
    );
    assert!(text.contains("cannot be undone"));
    assert!(text.contains("Nothing happens to (1)"));
    assert_eq!(CONFIRMATIONS.len(), 5);
    for path in [uv, pnpm, runtime] {
        assert!(path.join("payload").exists(), "approval runs nothing");
    }
}

#[test]
fn skip_means_nothing_happens_even_inside_a_chosen_scope() {
    let fixture = Fixture::new();
    let uv = fixture.dir(".cache/uv");
    let archive = fixture.dir(".cache/uv/archive-v0");
    let wheels = fixture.dir(".cache/uv/wheels-v5");
    let mut probe = fixture.probe("uv", "uv 0.11.0", &uv);
    let prepared = prepare_with(
        &[target(&archive), target(&wheels)],
        &fixture.root,
        fixture.env.clone(),
        &mut probe,
    )
    .expect("preview");
    assert_eq!(prepared.operations.len(), 1, "one scope, asked twice");
    assert!(prepared.operations[0].wider());
    for refused in [[Run, Skip], [Run, DeleteFiles], [DeleteFiles, Run]] {
        assert!(
            prepared
                .approve(&refused, Action::Permanent)
                .expect_err("overlapping choices")
                .contains("choose the same action"),
            "{refused:?}"
        );
    }
    let approved = prepared
        .approve(&[Run, Run], Action::Skip)
        .expect("same choice");
    assert_eq!(approved.steps, [Step::Owner(0)], "the command runs once");

    let decisions = prepared
        .decisions_for(&[(wheels.clone(), Run), (archive.clone(), Run)])
        .expect("by path");
    assert_eq!(decisions, [Run, Run]);
    assert!(prepared.decisions_for(&[(archive.clone(), Run)]).is_err());
    assert!(
        prepared
            .decisions_for(&[
                (archive.clone(), Run),
                (archive.clone(), Skip),
                (wheels, Run)
            ])
            .is_err()
    );
    assert!(
        prepared
            .decisions_for(&[(archive, Run), (uv, Run)])
            .is_err(),
        "only marked paths"
    );
}

#[test]
fn delete_files_names_the_caches_it_would_bypass() {
    let fixture = Fixture::new();
    let wrapper = fixture.env.home.join(".cache");
    let uv = fixture.dir(".cache/uv");
    let tagged = fixture.dir(".cache/tool/data");
    std::fs::write(
        tagged.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55",
    )
    .expect("tag");
    let mut probe = fixture.probe("absent", "", &uv);
    let prepared = prepare_with(
        &[target(&wrapper)],
        &fixture.root,
        fixture.env.clone(),
        &mut probe,
    )
    .expect("preview");
    let disclosure = prepared.proposals[0].delete.as_ref().expect("offered");
    assert_eq!(disclosure.path, wrapper);
    assert!(
        disclosure
            .contains
            .iter()
            .any(|item| item.starts_with("uv cache")),
        "{disclosure:?}"
    );
    assert!(
        disclosure
            .contains
            .iter()
            .any(|item| item.starts_with("tagged cache")),
        "{disclosure:?}"
    );
    let text = disclosure.description(Action::Trash);
    assert!(text.contains("owners are not involved either"));
    assert!(text.contains("recoverable until the Trash is emptied"));
    assert!(uv.join("payload").exists());
}

#[test]
fn guarded_paths_offer_neither_command_nor_deletion() {
    let fixture = Fixture::new();
    let cache = fixture.dir(".cache/uv");
    let mut probe = fixture.probe("uv", "uv 0.11.0", &cache);
    // The scan root itself is never removable, whatever is chosen.
    let prepared =
        prepare_with(&[target(&cache)], &cache, fixture.env, &mut probe)
            .expect("preview");
    let proposal = &prepared.proposals[0];
    assert!(!proposal.allows(Decision::Owner));
    assert!(!proposal.allows(Decision::DeleteFiles));
    assert_eq!(
        proposal.delete_issue.as_ref().map(|issue| issue.problem),
        Some(Problem::Refused)
    );
    assert!(
        prepared
            .approve(&[Decision::DeleteFiles], Action::Permanent)
            .is_err()
    );
}

#[cfg(unix)]
fn run_fixture(
    fixture: &Fixture,
    marks: &[PathBuf],
    decisions: &[Decision],
    files: Action,
) -> (Approved, crate::removal::RemovalHandle) {
    // Only the fixture CLI is reachable, so no real tool is ever probed.
    let mut env = fixture.env.clone();
    let bin = fixture.executable.parent().expect("bin");
    env.variables
        .insert("PATH".into(), bin.to_str().expect("bin").into());
    let targets: Vec<_> = marks.iter().map(|path| target(path)).collect();
    let prepared =
        prepare_with(&targets, &fixture.root, env, &mut process::SystemProbe)
            .expect("preview");
    let approved = prepared.approve(decisions, files).expect("decisions");
    let handle = crate::removal::spawn_cleanup(
        approved.clone(),
        crate::removal_limit::Limit::default().into(),
    );
    (approved, handle)
}

#[cfg(unix)]
fn reports(
    events: &[crate::removal::RemovalEvent],
) -> Vec<(PathBuf, Action, Outcome, Option<Problem>)> {
    events
        .iter()
        .filter_map(|event| match event {
            crate::removal::RemovalEvent::Cleanup(report) => Some((
                report.path.clone(),
                report.action,
                report.outcome,
                report.problem,
            )),
            _ => None,
        })
        .collect()
}

#[cfg(unix)]
#[test]
fn a_failed_owner_command_stays_local_and_deletion_runs_only_where_chosen() {
    let _execution = EXECUTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut fixture = executable_fixture();
    fixture
        .env
        .variables
        .insert("FIXTURE_FAIL".into(), "1".into());
    let uv = fixture.env.home.join(".cache/uv");
    let stale = fixture.dir(".cache/stale-tool");
    let kept = fixture.dir(".cache/kept-tool");
    let (_, handle) = run_fixture(
        &fixture,
        &[uv.clone(), stale.clone(), kept.clone()],
        &[Run, DeleteFiles, Skip],
        Action::Permanent,
    );
    let events = collect(&handle);
    let reports = reports(&events);
    assert!(
        reports.contains(&(
            kept.clone(),
            Action::Skip,
            Outcome::Skipped,
            Some(Problem::UnknownOwner)
        )),
        "{reports:?}"
    );
    assert!(
        reports.contains(&(
            uv.clone(),
            Action::Owner,
            Outcome::Failed,
            Some(Problem::CommandFailed)
        )),
        "{reports:?}"
    );
    assert!(
        reports.contains(&(
            stale.clone(),
            Action::Permanent,
            Outcome::Succeeded,
            None
        )),
        "{reports:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        crate::removal::RemovalEvent::Done {
            removed: 1,
            failed: 1,
            ..
        }
    )));
    assert!(fixture.env.home.join("invocation").exists(), "owner ran");
    assert!(
        uv.join("payload").exists(),
        "a failed owner never falls back"
    );
    assert!(!stale.exists(), "the deliberate choice ran");
    assert!(kept.join("payload").exists(), "skip touched nothing");
}

#[cfg(unix)]
#[test]
fn stop_cancels_the_running_command_and_reports_the_rest_not_reached() {
    let _execution = EXECUTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut fixture = executable_fixture();
    fixture
        .env
        .variables
        .insert("FIXTURE_WAIT".into(), "1".into());
    let uv = fixture.env.home.join(".cache/uv");
    // Sorted after the cache, so it is still waiting when Stop arrives.
    let later = fixture.dir("zz-stale-output");
    let (_, handle) = run_fixture(
        &fixture,
        &[uv.clone(), later.clone()],
        &[Decision::Owner, Decision::DeleteFiles],
        Action::Permanent,
    );
    let start = Instant::now();
    while !fixture.env.home.join("invocation").exists() {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    handle.cancel();
    let reports = reports(&collect(&handle));
    assert!(
        reports.contains(&(
            uv.clone(),
            Action::Owner,
            Outcome::Cancelled,
            Some(Problem::Cancelled)
        )),
        "{reports:?}"
    );
    assert!(
        reports.contains(&(
            later.clone(),
            Action::Permanent,
            Outcome::NotReached,
            None
        )),
        "{reports:?}"
    );
    assert!(uv.join("payload").exists());
    assert!(
        later.join("payload").exists(),
        "not reached means untouched"
    );
}

#[cfg(unix)]
#[test]
fn deleting_files_in_use_is_refused_on_the_worker() {
    let _execution = EXECUTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = executable_fixture();
    let busy = fixture.dir("busy-output");
    let mut holder = std::process::Command::new("/bin/sh")
        // A trailing command keeps the shell, and its argument naming the
        // path, alive instead of exec-ing sleep in its place.
        .args(["-c", "/bin/sleep 20; :", "holder"])
        .arg(&busy)
        .current_dir(&busy)
        .spawn()
        .expect("fixture holder");
    std::thread::sleep(Duration::from_millis(100));
    let (_, handle) = run_fixture(
        &fixture,
        std::slice::from_ref(&busy),
        &[Decision::DeleteFiles],
        Action::Permanent,
    );
    let reports = reports(&collect(&handle));
    holder.kill().expect("stop holder");
    holder.wait().expect("wait holder");
    assert_eq!(
        reports,
        [(
            busy.clone(),
            Action::Permanent,
            Outcome::Failed,
            Some(Problem::InUse)
        )]
    );
    assert!(busy.join("payload").exists());
}

/// Deleting files by Trash in a cleanup needs the Trash found for it; an
/// owner-only cleanup never looks.
#[test]
fn a_cleanup_finds_the_trash_only_when_files_go_there() {
    use crate::removal::{RemovalMode, TrashBackend, backend_for};
    let fixture = Fixture::new();
    let (prepared, _) = mixed(&fixture);
    let trash = prepared
        .approve(&[Skip, Skip, DeleteFiles], Action::Trash)
        .expect("trash files");
    assert_eq!(
        backend_for(RemovalMode::OwnerCleanup, Some(&trash)),
        crate::removal::detect_trash_backend()
    );
    let owner = prepared
        .approve(&[Run, Skip, Skip], Action::Trash)
        .expect("owner only");
    assert_eq!(
        backend_for(RemovalMode::OwnerCleanup, Some(&owner)),
        TrashBackend::Unavailable
    );
    let permanent = prepared
        .approve(&[Skip, Skip, DeleteFiles], Action::Permanent)
        .expect("permanent files");
    assert_eq!(
        backend_for(RemovalMode::OwnerCleanup, Some(&permanent)),
        TrashBackend::Unavailable
    );
}

/// Skip on a parent means nothing happens inside it either: deleting a
/// marked child under a skipped parent is refused until both agree.
#[test]
fn a_skipped_parent_protects_a_marked_child() {
    let fixture = Fixture::new();
    let wrapper = fixture.dir("tools");
    let child = fixture.dir("tools/stale");
    let mut probe = fixture.probe("absent", "", &wrapper);
    let prepared = prepare_with(
        &[target(&wrapper), target(&child)],
        &fixture.root,
        fixture.env.clone(),
        &mut probe,
    )
    .expect("preview");
    assert!(
        prepared
            .approve(&[Skip, DeleteFiles], Action::Permanent)
            .expect_err("overlap")
            .contains("choose the same action")
    );
    let both = prepared
        .approve(&[DeleteFiles, DeleteFiles], Action::Permanent)
        .expect("same action");
    assert_eq!(both.steps.len(), 1, "the child is inside the parent");
    assert!(child.join("payload").exists());
}
