use std::collections::BTreeMap;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::time::{Duration, Instant};

use super::*;

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
    ) -> Result<PathBuf, String> {
        if name == self.name {
            Ok(self.executable.clone())
        } else {
            Err(format!(
                "{name} unavailable on PATH; software may still be installed"
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
    assert_eq!(prepared.plan.targets.len(), 2);
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
        prepared.plan.bytes() >= 8192,
        "includes unselected siblings"
    );
    assert!(operation.description().contains("WHOLE scope"));
    assert!(operation.description().contains("APFS"));
    assert!(!fixture.env.home.join("sentinel").exists());
    let mut probe = fixture.probe("uv", "uv 0.11.0", &cache);
    let other = fixture.dir(".cache/uv");
    assert!(
        prepare_with(
            &[target(&other)],
            &fixture.root,
            fixture.env.clone(),
            &mut probe
        )
        .is_err(),
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
    let error = prepare_with(
        &[target(&uv)],
        &fixture.root,
        fixture.env.clone(),
        &mut missing,
    )
    .expect_err("missing");
    assert!(error.contains("unavailable"));
    assert!(error.contains("does not prove it is absent"));
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
        let error = prepare_with(
            &[target(&path)],
            &fixture.root,
            fixture.env.clone(),
            &mut probe,
        )
        .expect_err("inspection only");
        assert!(error.contains("fallback"), "{error}");
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
        let error = prepare_with(
            &[target(&selected)],
            root,
            fixture.env.clone(),
            &mut probe,
        )
        .expect_err("scope exceeds scan");
        assert!(error.contains("guards"));
    }
    let mut probe =
        fixture.probe("uv", "uv 0.11.0", Path::new("relative-cache"));
    assert!(
        prepare_with(
            &[target(&cache)],
            &fixture.root,
            fixture.env.clone(),
            &mut probe
        )
        .expect_err("relative root")
        .contains("relative")
    );
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
    let _execution = EXECUTION.lock().expect("fixture serialization");
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
            prepared,
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
                    .contains("no raw deletion fallback"),
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

    let _execution = EXECUTION.lock().expect("fixture serialization");
    let fixture = executable_fixture();
    let cache = fixture.env.home.join(".cache/uv");
    let mut prepared = prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    )
    .expect("fixture preview");
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
    let _execution = EXECUTION.lock().expect("fixture serialization");
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
        prepared,
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
    let _execution = EXECUTION.lock().expect("fixture serialization");
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
    let active_check =
        prepared.preflight(&crate::removal_limit::Limit::default().into());
    active.kill().expect("stop fixture");
    active.wait().expect("wait fixture");
    assert!(active_check.expect_err("active owner").contains("active"));
    std::fs::write(&fixture.executable, "changed fixture binary")
        .expect("replace executable");
    assert!(
        prepared
            .preflight(&crate::removal_limit::Limit::default().into())
            .expect_err("changed executable")
            .contains("changed")
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
        .contains("could not start")
    );
    assert!(!fixture.env.home.join("invocation").exists());
}

#[cfg(unix)]
#[test]
fn changed_configuration_and_capacity_failure_do_not_invoke_cleanup() {
    let _execution = EXECUTION.lock().expect("fixture serialization");
    let fixture = executable_fixture();
    let cache = fixture.env.home.join(".cache/uv");
    let mut prepared = prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    )
    .expect("preview");
    prepared.plan.targets[0].bytes = u64::MAX;
    assert!(
        prepared
            .preflight(&crate::removal_limit::Limit::default().into())
            .is_err(),
        "the capacity limit covers the entire actual scope"
    );
    prepared.plan.targets[0].bytes = 0;
    let other = fixture.dir("other-cache");
    std::fs::write(
        fixture.env.home.join("cache-root"),
        format!("{}\n", other.display()),
    )
    .expect("changed config");
    assert!(
        prepared
            .preflight(&crate::removal_limit::Limit::default().into())
            .expect_err("changed root")
            .contains("changed")
    );
    assert!(!fixture.env.home.join("invocation").exists());
    assert!(cache.join("payload").exists());
    assert!(other.join("payload").exists());
}

#[test]
fn observed_space_can_decrease_and_never_equals_scope_allocation_by_assumption()
{
    let operation = Operation {
        owner: Owner::Uv,
        version: "uv 0.11.0".into(),
        selected: vec![],
        cache_root: "/fixture/cache".into(),
        scope: "/fixture/cache".into(),
        command: Command {
            executable: "/fixture/uv".into(),
            arguments: vec![],
            environment: BTreeMap::new(),
            working_directory: "/fixture".into(),
        },
        effects: String::new(),
        limitations: String::new(),
    };
    let success = Report::of(&operation, 50_000, Some(100), Some(90), &Ok(()));
    assert!(success.success);
    assert_eq!(success.scope_allocation_at_preview, 50_000);
    assert_eq!(success.observed_change, Some(-10));
    assert!(success.description().contains("not savings"));
    let failure = Report::of(
        &operation,
        50_000,
        Some(100),
        Some(105),
        &Err("owner exit 17".into()),
    );
    assert!(!failure.success);
    assert_eq!(
        failure.observed_change,
        Some(5),
        "partial failure can still change free space"
    );
    assert_eq!(failure.error.as_deref(), Some("owner exit 17"));
}

#[test]
fn active_owner_detection_recognizes_node_launchers_and_pip3() {
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
    assert!(process::owner_argument(
        &operation,
        "/other-install/npm-cli.js"
    ));
    assert!(process::owner_argument(
        &operation,
        "/other-install/npx-cli.js"
    ));
    assert!(!process::owner_argument(
        &operation,
        "/fixture/unrelated.js"
    ));
    operation.owner = Owner::Pip;
    assert!(process::owner_argument(&operation, "/fixture/bin/pip3"));
    assert!(process::owner_argument(&operation, "pip"));
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
    let error = prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    )
    .expect_err("do not probe an installing shim");
    assert!(error.contains("shim"));
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
        prepare_with(
            &[target(&cache)],
            &fixture.root,
            fixture.env.clone(),
            &mut process::SystemProbe
        )
        .expect_err("do not dispatch env node through a shim")
        .contains("shim")
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
    let error = prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    )
    .expect_err("no automatic npm install");
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
        assert!(
            prepare_with(
                &[target(&selected)],
                &fixture.root,
                fixture.env.clone(),
                &mut probe
            )
            .is_err(),
            "ambiguous line endings must not select another directory"
        );
    }
    let mut probe = fixture.probe("uv", "uv 0.11.0", &configured);
    assert!(
        prepare_with(
            &[target(&selected)],
            &fixture.root,
            fixture.env.clone(),
            &mut probe
        )
        .is_err(),
        "a trailing space is part of the configured path, not output padding"
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
    let error = prepare_with(
        &[target(&cache)],
        &fixture.root,
        fixture.env.clone(),
        &mut process::SystemProbe,
    )
    .expect_err("unambiguous probe output required");
    assert!(error.contains("non-UTF-8"));
    assert!(cache.join("payload").exists());
    assert!(!fixture.env.home.join("invocation").exists());
}
