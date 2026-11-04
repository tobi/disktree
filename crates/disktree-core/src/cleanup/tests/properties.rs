//! Rules of cache cleanup checked against many generated cases rather
//! than a few chosen ones. Each test states the promise it holds, builds
//! cases from a seed, and names the seed when a case breaks the rule:
//! `PWRDISK_SEED=<seed> cargo test -p disktree-core <test>` replays it.

use std::collections::BTreeSet;

use super::*;
use crate::test_rng::Rng;

/// This run's seeds, `runs` of them. They start at `PWRDISK_SEED` to
/// replay a failure, at the clock otherwise, so every run tries cases no
/// earlier run tried.
fn seeds(runs: u64) -> impl Iterator<Item = u64> {
    let first = std::env::var("PWRDISK_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or_else(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            now.as_secs().wrapping_mul(1_000_003)
                ^ u64::from(now.subsec_nanos())
        });
    (0..runs).map(move |run| first.wrapping_add(run))
}

fn pick<'a, T>(rng: &mut Rng, items: &'a [T]) -> &'a T {
    &items[usize::try_from(rng.below(items.len() as u64)).expect("index")]
}

fn count(rng: &mut Rng, low: u64, high: u64) -> usize {
    usize::try_from(low + rng.below(high - low + 1)).expect("count")
}

/// A directory name that can hold the characters a cache root's name may
/// legally have, the ones a careless parser strips or splits on included.
#[cfg(unix)]
fn awkward_name(rng: &mut Rng) -> String {
    const PLAIN: [char; 8] = ['a', 'b', '-', '_', '.', 'é', ' ', '\t'];
    const ENDINGS: [&str; 7] = ["", "", "", " ", "\t", "\r", " \r"];
    loop {
        let mut name: String =
            (0..count(rng, 1, 6)).map(|_| *pick(rng, &PLAIN)).collect();
        if rng.percent(15) {
            name.insert(0, *pick(rng, &['\r', '\n']));
        }
        name.push_str(pick(rng, &ENDINGS));
        let trimmed = name.trim_end();
        if !matches!(name.as_str(), "." | "..")
            && !matches!(trimmed, "" | "." | "..")
        {
            return name;
        }
    }
}

/// An owner that prints exactly `output` for its cache root and the
/// given version, and refuses anything that would change something.
#[cfg(unix)]
struct Printed {
    executable: PathBuf,
    version: String,
    output: String,
}

#[cfg(unix)]
impl Probe for Printed {
    fn executable(
        &mut self,
        _: &str,
        _: &Environment,
    ) -> Result<PathBuf, Issue> {
        Ok(self.executable.clone())
    }

    fn output(
        &mut self,
        command: &Command,
        _: &Environment,
    ) -> Result<String, String> {
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
                Ok(self.output.clone())
            }
            _ => panic!("Preview attempted a write: {command:?}"),
        }
    }
}

/// The cache root an owner command acts on is exactly the one the owner
/// printed, never a similar path found by trimming or splitting its
/// output; and a root printed plainly, with one LF, is always accepted.
///
/// The oracle is the generator: it knows the real root, and puts the
/// marked path either inside it or inside a look-alike next to it.
#[cfg(unix)]
#[test]
fn an_owner_command_acts_only_on_the_root_the_owner_printed() {
    const OWNERS: [(Owner, &str); 5] = [
        (Owner::Uv, "uv 0.11.0"),
        (Owner::Pip, "pip 24.0 from /fixture (python 3.12)"),
        (Owner::Cypress, "14.0.0"),
        (Owner::Npm, "10.8.0"),
        (Owner::Npx, "11.6.2"),
    ];
    let scope_of = |owner: Owner, root: &Path| match owner {
        Owner::Npm => root.join("_cacache"),
        Owner::Npx => root.join("_npx"),
        _ => root.to_owned(),
    };
    for seed in seeds(150) {
        let rng = &mut Rng::new(seed);
        let fixture = Fixture::new();
        let (owner, version) = *pick(rng, &OWNERS);
        let mut parent = fixture.env.home.clone();
        if rng.percent(40) {
            parent.push(awkward_name(rng));
        }
        let name = awkward_name(rng);
        let root = parent.join(&name);
        // What a parser that trims or drops a CR would read instead.
        let look_alike = parent.join(name.trim_end());
        let marked_root = if look_alike != root && rng.percent(50) {
            &look_alike
        } else {
            &root
        };
        let mut selected = scope_of(owner, marked_root);
        if rng.percent(50) {
            selected.push("entries");
        }
        for directory in [scope_of(owner, &root), selected.clone()] {
            std::fs::create_dir_all(&directory).expect("cache fixture");
        }
        let printed = root.to_str().expect("UTF-8 root");
        let terminator = *pick(rng, &["\n", "\n", "\n", "\r\n", "", "\n\n"]);
        let mut probe = Printed {
            executable: fixture.executable.clone(),
            version: version.into(),
            output: format!("{printed}{terminator}"),
        };
        let case = format!(
            "seed {seed}: {owner:?} printed {:?}, marked {selected:?}",
            probe.output
        );
        let resolved =
            resolve_owner(owner, &selected, &fixture.env, &mut probe);
        let plain = terminator == "\n"
            && !printed.contains(['\n', '\r'])
            && marked_root == &root;
        match &resolved {
            Ok(Some(operation)) => {
                assert_eq!(operation.cache_root, root, "{case}");
                assert_eq!(operation.scope, scope_of(owner, &root), "{case}");
                let named = operation
                    .command
                    .arguments
                    .iter()
                    .chain(operation.command.environment.values())
                    .any(|argument| argument == printed);
                assert!(named, "{case}: {:?}", operation.command);
            }
            Ok(None) | Err(_) => {
                assert!(!plain, "{case}: refused a plain root: {resolved:?}");
            }
        }
        if let Err(issue) = &resolved {
            assert_eq!(issue.problem, Problem::ConfigCheckFailed, "{case}");
        }
    }
}

/// Signs of a cache that no name gives away. Each is put on a directory
/// in the generated tree; the decoys look close but are not caches.
#[derive(Clone, Copy, Debug)]
enum Sign {
    Tag,
    Pip,
    Pnpm,
    Cypress,
    TagDirectory,
    WheelsOnly,
    EmptyPnpmVersion,
    ShortCypressVersion,
}

const SIGNS: [Sign; 8] = [
    Sign::Tag,
    Sign::Pip,
    Sign::Pnpm,
    Sign::Cypress,
    Sign::TagDirectory,
    Sign::WheelsOnly,
    Sign::EmptyPnpmVersion,
    Sign::ShortCypressVersion,
];

fn put(sign: Sign, at: &Path, rng: &mut Rng) {
    let dir = |path: PathBuf| std::fs::create_dir_all(path).expect("sign");
    match sign {
        Sign::Tag => {
            std::fs::write(
                at.join("CACHEDIR.TAG"),
                "Signature: 8a477f597d28d172789f06886806bc55",
            )
            .expect("tag");
        }
        Sign::Pip => {
            dir(at.join("wheels"));
            dir(at.join(pick(rng, &["http", "http-v2"])));
        }
        Sign::Pnpm => dir(at.join(format!("v{}/files", rng.below(20)))),
        Sign::Cypress => {
            let version = format!("{}.{}.0", 10 + rng.below(6), rng.below(9));
            if rng.percent(50) {
                dir(at.join(version).join("Cypress.app"));
            } else {
                dir(at.join(&version).join("Cypress"));
                std::fs::write(at.join(version).join("Cypress/Cypress"), "")
                    .expect("binary");
            }
        }
        Sign::TagDirectory => dir(at.join("CACHEDIR.TAG")),
        Sign::WheelsOnly => dir(at.join("wheels")),
        Sign::EmptyPnpmVersion => dir(at.join("v7")),
        Sign::ShortCypressVersion => dir(at.join("14.0/Cypress.app")),
    }
}

/// The oracle: does this directory itself carry a cache's sign? Reads
/// every entry, where the code under test reads a bounded few.
fn signed(directory: &Path) -> bool {
    let is_dir = |name: &str| directory.join(name).is_dir();
    if directory.join("CACHEDIR.TAG").is_file()
        || (is_dir("wheels") && (is_dir("http") || is_dir("http-v2")))
    {
        return true;
    }
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let pnpm = name.strip_prefix('v').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            }) && path.join("files").is_dir();
            let parts: Vec<_> = name.split('.').collect();
            let cypress = parts.len() == 3
                && parts.iter().all(|part| {
                    !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())
                })
                && (path.join("Cypress.app").is_dir()
                    || path.join("Cypress/Cypress").is_file());
            pnpm || cypress
        })
}

fn directories_under(path: &Path) -> Vec<PathBuf> {
    let mut found = vec![];
    let mut pending = vec![path.to_owned()];
    while let Some(directory) = pending.pop() {
        if !directory.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&directory).expect("listing") {
            pending.push(entry.expect("entry").path());
        }
        found.push(directory);
    }
    found
}

/// A cache's sign on any directory protects everything under it: Review
/// treats the path as a cache, and raw removal refuses it, however deep
/// the path is. Raw removal also refuses a path with a cache inside it.
/// Anything with no sign above or inside it stays removable.
///
/// The oracle walks every ancestor up to the fixture and every directory
/// below the path, reading each one whole.
#[test]
fn a_cache_sign_protects_everything_under_it_and_nothing_else() {
    for seed in seeds(120) {
        let rng = &mut Rng::new(seed);
        let fixture = Fixture::new();
        let mut chain = vec![fixture.env.home.join(format!("n{seed:x}"))];
        for level in 0..count(rng, 1, 5) {
            let next = chain.last().expect("level").join(format!("n{level}"));
            chain.push(next);
        }
        let deepest = chain.last().expect("deepest");
        std::fs::create_dir_all(deepest).expect("chain");
        std::fs::write(deepest.join("payload"), [0; 64]).expect("payload");
        for (level, directory) in chain.iter().enumerate() {
            if rng.percent(30) {
                std::fs::create_dir(directory.join(format!("m{level}")))
                    .expect("neighbour");
            }
            if rng.percent(20) {
                put(*pick(rng, &SIGNS), directory, rng);
            }
        }
        let selected = if rng.percent(25) {
            deepest.join("payload")
        } else {
            pick(rng, &chain).clone()
        };
        let above = selected
            .ancestors()
            .take_while(|ancestor| ancestor.starts_with(&fixture.root))
            .any(signed);
        let inside = directories_under(&selected).iter().any(|d| signed(d));
        let case = format!("seed {seed}: {selected:?}");
        let hint = recognize::inspection_with(&selected, &fixture.env);
        assert_eq!(hint.is_some(), above, "{case}: {hint:?}");
        let raw = check_raw(&[target(&selected)]);
        assert_eq!(raw.is_err(), above || inside, "{case}: {raw:?}");
        assert!(selected.exists(), "{case}: checking cannot delete");
    }
}

/// How the owner's executable is reached on PATH.
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
enum Reach {
    /// A file in an ordinary directory.
    Direct,
    /// A link into an installation, as npm's own `npm -> npm-cli.js`.
    Installed,
    /// A file in a `shims` directory on PATH.
    InShims,
    /// A link into some `shims` directory.
    LinkToShims,
    /// A link to Volta's dispatcher.
    VoltaShim,
    /// A file in `~/.volta/bin`.
    VoltaBin,
}

/// What runs the script, read from its first line.
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
enum Interpreter {
    Shell,
    /// `env node`, with node on PATH reached as given.
    Env(Reach),
    /// `env node --flag`: arguments env would pass on.
    EnvWithArguments,
    /// A shim named directly on the first line.
    ShimPath,
}

#[cfg(unix)]
const fn installs(reach: Reach) -> bool {
    matches!(
        reach,
        Reach::InShims
            | Reach::LinkToShims
            | Reach::VoltaShim
            | Reach::VoltaBin
    )
}

/// Write a script that records that it ran and prints `version`, and put
/// it on a PATH directory as `name`, reached as `reach`. Returns the PATH
/// directory it is reachable from.
#[cfg(unix)]
fn place(
    root: &Path,
    name: &str,
    target_name: &str,
    reach: Reach,
    first_line: &str,
    version: &str,
) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let ran = root.join("ran");
    let script = |path: &Path| {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(
            path,
            format!(
                concat!(
                    "{first_line}\n",
                    "printf '%s\\n' \"$0\" >> '{ran}'\n",
                    "printf '%s\\n' '{version}'\n",
                ),
                first_line = first_line,
                ran = ran.display(),
                version = version,
            ),
        )
        .expect("script");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
    };
    let link = |from: &Path, to: &Path| {
        std::fs::create_dir_all(from.parent().expect("parent")).expect("dir");
        std::os::unix::fs::symlink(to, from).expect("link");
    };
    let path_dir = |dir: &str| root.join(format!("{dir}-{name}"));
    match reach {
        Reach::Direct => {
            let dir = path_dir("bin");
            script(&dir.join(name));
            dir
        }
        Reach::Installed => {
            let dir = path_dir("bin");
            let installed = path_dir("lib").join("dist").join(target_name);
            script(&installed);
            link(&dir.join(name), &installed);
            dir
        }
        Reach::InShims => {
            let dir = path_dir("version-manager").join("shims");
            script(&dir.join(name));
            dir
        }
        Reach::LinkToShims => {
            let dir = path_dir("bin");
            let shim = path_dir("version-manager").join("shims").join(name);
            script(&shim);
            link(&dir.join(name), &shim);
            dir
        }
        Reach::VoltaShim => {
            let dir = path_dir("bin");
            let shim = path_dir("volta").join("volta-shim");
            script(&shim);
            link(&dir.join(name), &shim);
            dir
        }
        Reach::VoltaBin => {
            let dir = path_dir("home").join(".volta").join("bin");
            script(&dir.join(name));
            dir
        }
    }
}

/// Finding an owner's executable never runs one that could install
/// software on demand: a version manager's shim, Volta's dispatcher, an
/// npm that is not npm's own `npm-cli.js`, or a script whose interpreter
/// is any of those. Everything else is found, and its version probed.
///
/// The oracle is the generator's own record of what each layout is, and
/// every generated script writes down that it ran.
#[cfg(unix)]
#[test]
fn probing_never_runs_a_launcher_that_could_install() {
    const REACHES: [Reach; 6] = [
        Reach::Direct,
        Reach::Installed,
        Reach::InShims,
        Reach::LinkToShims,
        Reach::VoltaShim,
        Reach::VoltaBin,
    ];
    const OWNERS: [(Owner, &str, &str); 5] = [
        (Owner::Uv, "uv", "uv 0.11.0"),
        (Owner::Pip, "pip", "pip 24.0 from /fixture (python 3.12)"),
        (Owner::Pip, "pip3", "pip 24.0 from /fixture (python 3.12)"),
        (Owner::Cypress, "cypress", "14.0.0"),
        (Owner::Npm, "npm", "10.8.0"),
    ];
    // Generated owners are real processes named like real owners, which
    // another test's in-use check would see.
    let _execution = EXECUTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for seed in seeds(60) {
        let rng = &mut Rng::new(seed);
        let fixture = Fixture::new();
        let (owner, name, version) = *pick(rng, &OWNERS);
        let reach = *pick(rng, &REACHES);
        let interpreter = match rng.below(4) {
            0 => Interpreter::Env(*pick(rng, &REACHES)),
            1 if rng.percent(50) => Interpreter::EnvWithArguments,
            1 => Interpreter::ShimPath,
            _ => Interpreter::Shell,
        };
        let root = &fixture.root;
        let mut path = vec![];
        let first_line = match interpreter {
            Interpreter::Shell => "#!/bin/sh".to_owned(),
            Interpreter::Env(node) => {
                path.push(place(
                    root,
                    "node",
                    "node",
                    node,
                    "#!/bin/sh",
                    version,
                ));
                "#!/usr/bin/env node".to_owned()
            }
            Interpreter::EnvWithArguments => {
                "#!/usr/bin/env node --flag".into()
            }
            Interpreter::ShimPath => {
                let shims = place(
                    root,
                    "node",
                    "node",
                    Reach::InShims,
                    "#!/bin/sh",
                    version,
                );
                format!("#!{}", shims.join("node").display())
            }
        };
        // npm's distribution names its entry point npm-cli.js; a link
        // to anything else is a proxy such as Corepack.
        let target_name = if owner == Owner::Npm && rng.percent(60) {
            "npm-cli.js"
        } else {
            name
        };
        path.insert(
            0,
            place(root, name, target_name, reach, &first_line, version),
        );
        let reached_npm_cli =
            matches!(reach, Reach::Installed) && target_name == "npm-cli.js";
        let unsafe_launcher = installs(reach)
            || (owner == Owner::Npm && !reached_npm_cli)
            || match interpreter {
                Interpreter::Shell => false,
                Interpreter::Env(node) => installs(node),
                Interpreter::EnvWithArguments | Interpreter::ShimPath => true,
            };
        let mut env = fixture.env.clone();
        env.variables.extend([
            (
                "PATH".into(),
                std::env::join_paths(&path)
                    .expect("PATH")
                    .into_string()
                    .expect("UTF-8"),
            ),
            ("HOME".into(), env.home.to_str().expect("home").into()),
        ]);
        let cache = env.home.join("cache");
        let case = format!(
            "seed {seed}: {name} reached {reach:?}, run by {interpreter:?}, \
             linked to {target_name}"
        );
        let resolved =
            resolve_owner(owner, &cache, &env, &mut process::SystemProbe);
        let ran = std::fs::read_to_string(root.join("ran")).unwrap_or_default();
        let issue = resolved.expect_err(&case);
        if unsafe_launcher {
            assert_eq!(issue.problem, Problem::UnsupportedLauncher, "{case}");
            assert_eq!(ran, "", "{case}: ran before it was refused");
        } else {
            // Our scripts print a version for a cache root, so the root
            // check fails after the probe: proof that the probe ran.
            assert_eq!(issue.problem, Problem::ConfigCheckFailed, "{case}");
            assert_ne!(ran, "", "{case}: an ordinary launcher was not run");
        }
    }
}

/// A running owner is recognized whatever form launched it: the friendly
/// name, a versioned name such as `pip3.12`, the companion runner such as
/// `uvx`, the node entry points npm runs through, or the exact executable
/// the command names. Programs that only share a prefix are not.
///
/// The oracle is the list of each owner's launcher names.
#[test]
fn a_running_owner_is_recognized_in_every_launcher_form() {
    const OTHERS: [&str; 10] = [
        "node",
        "python3",
        "python3.12",
        "bash",
        "unrelated.js",
        "npm-cli.json",
        "pipx",
        "uvicorn",
        "cypress.json",
        "pip3.x",
    ];
    const OWNERS: [Owner; 5] = [
        Owner::Npm,
        Owner::Npx,
        Owner::Uv,
        Owner::Pip,
        Owner::Cypress,
    ];
    const DIRECTORIES: [&str; 4] = [
        "",
        "/usr/local/bin/",
        "/opt/tool/lib/node_modules/npm/bin/",
        "/install/bin/",
    ];
    let launchers = |owner: Owner| -> Vec<String> {
        match owner {
            Owner::Npm | Owner::Npx => {
                ["npm", "npx", "npm-cli.js", "npx-cli.js"]
                    .map(String::from)
                    .to_vec()
            }
            Owner::Uv => vec!["uv".into(), "uvx".into()],
            Owner::Pip => vec!["pip".into(), "pip3".into(), "pip3.12".into()],
            _ => vec![owner.executable().into()],
        }
    };
    for seed in seeds(400) {
        let rng = &mut Rng::new(seed);
        let owner = *pick(rng, &OWNERS);
        let names = launchers(owner);
        let executable = rng
            .percent(50)
            .then(|| PathBuf::from("/install/bin").join(pick(rng, &names)));
        let launcher = rng.percent(60);
        let base = if launcher {
            pick(rng, &names).clone()
        } else {
            (*pick(rng, &OTHERS)).to_owned()
        };
        let argument = format!("{}{base}", pick(rng, &DIRECTORIES));
        let expected = launcher
            || executable
                .as_ref()
                .is_some_and(|executable| executable.ends_with(&base));
        assert_eq!(
            process::owner_named(owner, executable.as_deref(), &argument),
            expected,
            "seed {seed}: {owner:?} with executable {executable:?} \
             launched as {argument:?}"
        );
    }
}

/// A generated set of marked paths, owner commands and decisions, over
/// real directories so the removal guards see a real tree.
struct Cleanup {
    _fixture: Fixture,
    nodes: Vec<PathBuf>,
    prepared: Prepared,
    decisions: Vec<Decision>,
    files: Action,
}

fn generate(rng: &mut Rng) -> Cleanup {
    let fixture = Fixture::new();
    let root = fixture.env.home.join("scan");
    let mut nodes = vec![root.clone()];
    for index in 0..count(rng, 4, 12) {
        let parent = pick(rng, &nodes).clone();
        nodes.push(parent.join(format!("d{index}")));
    }
    for node in &nodes {
        std::fs::create_dir_all(node).expect("tree");
    }
    let candidates = &nodes[1..];
    let mut scopes: Vec<PathBuf> = vec![];
    for _ in 0..count(rng, 0, 3) {
        let scope = pick(rng, candidates).clone();
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    let operations = scopes
        .iter()
        .map(|scope| Operation {
            owner: Owner::Uv,
            version: "uv 0.11.0".into(),
            selected: vec![],
            cache_root: scope.clone(),
            scope: scope.clone(),
            command: Command {
                executable: "/fixture/uv".into(),
                arguments: vec!["cache".into(), "clean".into()],
                environment: BTreeMap::new(),
                working_directory: fixture.env.home.clone(),
            },
            effects: String::new(),
            limitations: String::new(),
        })
        .collect::<Vec<_>>();
    let mut selected = candidates.to_vec();
    rng.shuffle(&mut selected);
    selected.truncate(count(rng, 1, 5));
    let proposals: Vec<Proposal> = selected
        .iter()
        .map(|path| Proposal {
            selected: path.clone(),
            bytes: 1,
            is_dir: true,
            hint: None,
            owner: None,
            // An owner's scope contains the marked path, or, for a wrapper
            // such as npm's, lies inside it.
            operations: scopes
                .iter()
                .enumerate()
                .filter(|(_, scope)| {
                    (path.starts_with(scope) || scope.starts_with(path))
                        && rng.percent(70)
                })
                .map(|(index, _)| index)
                .collect(),
            issue: None,
            delete: rng.percent(60).then(|| Disclosure {
                path: path.clone(),
                contains: vec![],
                warning: String::new(),
            }),
            delete_issue: None,
        })
        .collect();
    let decisions = proposals
        .iter()
        .map(|proposal| {
            let offered: Vec<_> =
                [Decision::Skip, Decision::Owner, Decision::DeleteFiles]
                    .into_iter()
                    .filter(|decision| proposal.allows(*decision))
                    .collect();
            *pick(rng, &offered)
        })
        .collect();
    let files = *pick(
        rng,
        &[
            Action::Trash,
            Action::Permanent,
            Action::Trash,
            Action::Skip,
        ],
    );
    let prepared = Prepared {
        proposals,
        scope_bytes: vec![0; operations.len()],
        operations,
        root,
        environment: fixture.env.clone(),
        identities: vec![],
    };
    Cleanup {
        _fixture: fixture,
        nodes,
        prepared,
        decisions,
        files,
    }
}

/// What a decision acts on when it runs.
fn reached(prepared: &Prepared, step: Step) -> &Path {
    match step {
        Step::Owner(index) => &prepared.operations[index].scope,
        Step::Files { proposal, .. } => &prepared.proposals[proposal].selected,
    }
}

/// Approval turns decisions into steps that keep each one:
///
/// * Skip means nothing happens to the path: no step acts inside it or
///   on anything around it.
/// * No step acts inside another, so nothing is acted on twice.
/// * A path chosen for its owner command is reached only by owner
///   commands, and one chosen for Delete files only by Delete files.
/// * The steps are exactly the chosen commands, and the deletions cover
///   exactly the paths chosen for Delete files.
/// * The confirmation names exactly the intents present.
///
/// Decisions that keep all of those are approved. The oracle is the set
/// of directories each step reaches, compared node by node.
#[test]
fn approval_keeps_every_decision_and_skip_means_nothing_happens() {
    for seed in seeds(300) {
        let rng = &mut Rng::new(seed);
        let cleanup = generate(rng);
        let prepared = &cleanup.prepared;
        let decisions = &cleanup.decisions;
        let under = |path: &Path| -> BTreeSet<&PathBuf> {
            cleanup
                .nodes
                .iter()
                .filter(|node| node.starts_with(path))
                .collect()
        };
        let chosen = |wanted: Decision| {
            prepared
                .proposals
                .iter()
                .zip(decisions)
                .filter(move |(_, decision)| **decision == wanted)
                .map(|(proposal, _)| proposal)
        };
        let commands: BTreeSet<usize> = chosen(Decision::Owner)
            .flat_map(|proposal| proposal.operations.iter().copied())
            .collect();
        // Every path some decision acts on, with the decision.
        let actions: Vec<(&Path, Decision)> = commands
            .iter()
            .map(|index| {
                (prepared.operations[*index].scope.as_path(), Decision::Owner)
            })
            .chain(chosen(Decision::DeleteFiles).map(|proposal| {
                (proposal.selected.as_path(), Decision::DeleteFiles)
            }))
            .collect();
        let consistent = prepared.proposals.iter().zip(decisions).all(
            |(proposal, decision)| {
                actions.iter().all(|(path, action)| {
                    action == decision
                        || under(path).is_disjoint(&under(&proposal.selected))
                })
            },
        ) && commands.iter().all(|left| {
            commands.iter().all(|right| {
                left == right
                    || under(&prepared.operations[*left].scope)
                        .is_disjoint(&under(&prepared.operations[*right].scope))
            })
        });
        let deletes = chosen(Decision::DeleteFiles).count() > 0;
        let acceptable = consistent
            && !actions.is_empty()
            && (!deletes
                || matches!(cleanup.files, Action::Trash | Action::Permanent));
        let case = format!(
            "seed {seed}: {:#?} decided {decisions:?} with {:?}",
            prepared.proposals, cleanup.files
        );
        let approved = match prepared.approve(decisions, cleanup.files) {
            Ok(approved) => approved,
            Err(error) => {
                assert!(!acceptable, "{case}: refused: {error}");
                continue;
            }
        };
        assert!(acceptable, "{case}: approved {:?}", approved.steps);
        let reach: Vec<_> = approved
            .steps
            .iter()
            .map(|step| (*step, under(reached(prepared, *step))))
            .collect();
        for (index, (step, nodes)) in reach.iter().enumerate() {
            for (other, other_nodes) in &reach[index + 1..] {
                assert!(
                    nodes.is_disjoint(other_nodes),
                    "{case}: {step:?} and {other:?} overlap"
                );
            }
        }
        for (proposal, decision) in prepared.proposals.iter().zip(decisions) {
            let own = under(&proposal.selected);
            for (step, nodes) in &reach {
                let action = match step {
                    Step::Owner(_) => Decision::Owner,
                    Step::Files { .. } => Decision::DeleteFiles,
                };
                assert!(
                    action == *decision || own.is_disjoint(nodes),
                    "{case}: {step:?} reaches {:?}, decided {decision:?}",
                    proposal.selected
                );
            }
        }
        let owners: BTreeSet<usize> = approved
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Owner(index) => Some(*index),
                Step::Files { .. } => None,
            })
            .collect();
        assert_eq!(owners, commands, "{case}");
        let deleted: Vec<&PathBuf> = approved
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Files { proposal, action } => {
                    assert_eq!(*action, cleanup.files, "{case}");
                    Some(&prepared.proposals[*proposal].selected)
                }
                Step::Owner(_) => None,
            })
            .collect();
        for proposal in chosen(Decision::DeleteFiles) {
            assert!(
                deleted
                    .iter()
                    .any(|path| proposal.selected.starts_with(path)),
                "{case}: {:?} is not deleted",
                proposal.selected
            );
        }
        for path in &deleted {
            assert!(
                chosen(Decision::DeleteFiles).any(|p| p.selected == **path),
                "{case}: {path:?} was not chosen for Delete files"
            );
        }
        let skipped: Vec<_> =
            approved.skipped.iter().map(|report| &report.path).collect();
        let skips: Vec<_> = chosen(Decision::Skip)
            .map(|proposal| &proposal.selected)
            .collect();
        assert_eq!(skipped, skips, "{case}");
        let phrase = approved.confirmation();
        assert_eq!(
            (
                phrase.contains("RUN OWNER CLEANUP"),
                phrase.contains("TRASH"),
                phrase.contains("DELETE"),
            ),
            (
                !owners.is_empty(),
                !deleted.is_empty() && cleanup.files == Action::Trash,
                !deleted.is_empty() && cleanup.files == Action::Permanent,
            ),
            "{case}: {phrase}"
        );
    }
}
