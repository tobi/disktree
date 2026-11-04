//! What a directory is, and whether it is a cleanup candidate to review.
//!
//! Colour identifies a kind of data; a hatch suggests review of potentially
//! regenerable contents. Neither names nor layouts prove that data is safe
//! to lose or that removing it will recover its listed bytes. Structural
//! developer-artifact rules live in [`crate::artifact`].
//!
//! Unmatched names inherit their parent's kind without guessing from
//! unrelated descendants. VM and Git ownership override cleanup heuristics.

use std::borrow::Cow;
use std::path::Path;

use rayon::prelude::*;

use crate::artifact::Roots;
use crate::path_style::PathStyle;
use crate::tree::{Block, Node, Tree};

/// A kind of data, for colour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Category {
    /// Source code and checkouts.
    Code,
    /// Space agents write into: worktrees, sandboxes, experiments.
    AgentScratch,
    /// Compilers, package managers and their installs.
    Toolchain,
    /// Folders a sync client owns.
    Synced,
    /// Version-control object stores.
    Git,
    /// Pictures, music, video, games and models.
    Media,
    /// Documents, downloads and the desktop.
    Documents,
    /// Caches and other regenerable state.
    Cache,
    /// macOS's persistent record of filesystem changes.
    FilesystemEvents,
    /// macOS's on-volume search database.
    Spotlight,
    /// VM/container disks and state; detection never suggests raw removal.
    VirtualMachine,
    /// Nothing recognisable.
    #[default]
    Other,
}

impl Category {
    /// The categories the legend lists, in its order.
    pub const LEGEND: [Self; 11] = [
        Self::Code,
        Self::AgentScratch,
        Self::Toolchain,
        Self::Synced,
        Self::Git,
        Self::Media,
        Self::Documents,
        Self::Cache,
        Self::FilesystemEvents,
        Self::Spotlight,
        Self::VirtualMachine,
    ];

    /// The label lowercased, as an agent reads and writes it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::AgentScratch => "agent scratch",
            Self::Toolchain => "toolchain",
            Self::Synced => "synced",
            Self::Git => "git",
            Self::Media => "media",
            Self::Documents => "documents",
            Self::Cache => "cache",
            Self::FilesystemEvents => "fsevents",
            Self::Spotlight => "spotlight",
            Self::VirtualMachine => "virtual machines",
            Self::Other => "other",
        }
    }

    /// The category with this [`Self::name`], if any.
    pub fn named(name: &str) -> Option<Self> {
        Self::LEGEND
            .iter()
            .copied()
            .chain(std::iter::once(Self::Other))
            .find(|category| category.name() == name)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Code => "Code",
            Self::AgentScratch => "Agent scratch",
            Self::Toolchain => "Toolchains",
            Self::Synced => "Synced",
            Self::Git => "Git",
            Self::Media => "Media",
            Self::Documents => "Documents",
            Self::Cache => "Cache",
            Self::FilesystemEvents => "FSEvents history",
            Self::Spotlight => "Spotlight index",
            Self::VirtualMachine => "Virtual machines",
            Self::Other => "Other",
        }
    }
}

/// Why contents may be a relatively lower-risk cleanup candidate.
/// This is a heuristic assessment, independent of removal authorization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reclaim {
    /// A cache inspection candidate; its name is not deletion permission.
    Regenerable,
    /// A sync client's old versions of files.
    SyncHistory,
    /// A package manager's content store.
    PackageStore,
    /// Compiler or bundler output beside its sources.
    BuildOutput,
    /// Installed dependencies beside their manifest.
    Reinstallable,
    /// Already deleted, still on disk.
    Trash,
    /// Scratch space meant to be thrown away.
    Temporary,
}

impl Reclaim {
    /// The reason, as the "Worth a look" list says it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Regenerable => "regenerable",
            Self::SyncHistory => "sync history",
            Self::PackageStore => "package store",
            Self::BuildOutput => "build output",
            Self::Reinstallable => "reinstallable",
            Self::Trash => "trash",
            Self::Temporary => "temporary",
        }
    }
}

/// Bytes of a name lowercased on the stack.
const LOWERED: usize = 32;

/// `name` lowercased, on the stack when it fits in [`LOWERED`] bytes: a
/// directory on a whole disk is looked up twice for each of hundreds of
/// thousands, and a string allocated each time was most of what
/// classifying cost. A longer name is rare enough to allocate.
fn lowered<'a>(name: &str, buffer: &'a mut [u8; LOWERED]) -> Cow<'a, str> {
    let Some(bytes) = buffer.get_mut(..name.len()) else {
        return Cow::Owned(name.to_ascii_lowercase());
    };
    bytes.copy_from_slice(name.as_bytes());
    bytes.make_ascii_lowercase();
    // ASCII lowercasing keeps UTF-8.
    Cow::Borrowed(std::str::from_utf8(bytes).unwrap_or_default())
}

/// The kind a directory name announces on its own, if any.
pub fn category_of_name(name: &str) -> Option<Category> {
    let mut buffer = [0; LOWERED];
    let lower = lowered(name, &mut buffer);
    // Windows: a work or school account's folder carries the organisation,
    // `OneDrive - Contoso`, and Dropbox's does the same, `Dropbox (Contoso)`.
    if lower.starts_with("onedrive - ")
        || lower.starts_with("dropbox (")
        // Legacy Google Drive roots predate File Provider. Match the wrapper,
        // never the account name inside it.
        || ["my drive (", "google drive ("]
            .iter()
            .any(|prefix| lower.starts_with(prefix) && lower.ends_with(')'))
    {
        return Some(Category::Synced);
    }
    let category = match &*lower {
        "src" | "code" | "projects" | "repos" | "dev" | "work"
        | "workspace" | "workspaces" | "github.com" | "gitlab.com"
        | "sites" | "development" => Category::Code,
        ".codex" | ".claude" | ".herdr" | ".pi" | ".cursor" | ".aider"
        | ".gemini" | ".continue" | ".windsurf" | ".microsandbox" | ".omp"
        | ".agents" | ".openai" | "tries" | "worktrees" | "experiments"
        | "scratch" | "playground" => Category::AgentScratch,
        ".cargo" | ".rustup" | ".local" | ".npm" | ".pnpm-store" | "pnpm"
        | ".bun" | ".deno" | "go" | ".gradle" | ".m2" | ".platformio"
        | "mise" | ".mise" | ".pyenv" | ".nvm" | ".gem" | "gem" | ".rbenv"
        | ".espressif" | ".arduino15" | ".config" | ".vscode" | ".zig"
        | ".rye" | ".conda" | "anaconda3" | "miniconda3" | ".opam"
        | ".ghcup" | ".stack" | ".julia" | ".dotnet" | ".android"
        | ".sdkman" | ".volta" | ".yarn" | ".java" | ".nuget"
        // macOS: Xcode's and the simulators' state in ~/Library/Developer.
        // Not `Developer` itself: ~/Developer is where Apple puts projects.
        | "xcode" | "coresimulator" => Category::Toolchain,
        "sync" | "dropbox" | "nextcloud" | "google drive" | "my drive" | "onedrive"
        | "pclouddrive" | "mega" | ".stversions"
        // macOS: iCloud Drive, and the File Provider clients (Dropbox,
        // Google Drive, OneDrive) since macOS 12.
        | "mobile documents" | "cloudstorage"
        // Windows: iCloud for Windows.
        | "iclouddrive" => Category::Synced,
        ".git" => Category::Git,
        ".fseventsd" => Category::FilesystemEvents,
        ".spotlight-v100" => Category::Spotlight,
        "pictures" | "photos" | "music" | "videos" | "movies" | "steam"
        | "steamlibrary" | "steamapps" | "emulation"
        | "models" | ".ollama" | ".lmstudio" | "games" | "wineprefix" => {
            Category::Media
        }
        "documents" | "desktop" | "downloads" | "books" | "notes"
        | "obsidian" | "public" | "templates" => Category::Documents,
        ".cache" | "cache" | "caches" | ".ccache" | ".sccache" | "_cacache"
        | "__pycache__" | "node_modules" | "trash" | ".trash" | "tmp"
        | ".tmp" | "deriveddata" | "ios devicesupport"
        | "watchos devicesupport"
        // Windows: see `reclaim_of`.
        | "temp" | "$recycle.bin" | "npm-cache" | "v3-cache" | "inetcache"
        | "d3dscache" | "dxcache" | "glcache" | "crashdumps" => Category::Cache,
        _ => return None,
    };
    Some(category)
}

/// Whether a directory is a cleanup candidate, judged from its name, the
/// kind of the directory holding it, and its siblings' names.
pub fn reclaim_of(
    name: &str,
    _parent: Category,
    has_sibling: impl Fn(&str) -> bool,
) -> Option<Reclaim> {
    let mut buffer = [0; LOWERED];
    let lower = lowered(name, &mut buffer);
    let reclaim = match &*lower {
        ".cache" | "cache" | "caches" | ".ccache" | ".sccache" | "_cacache"
        // Windows' own caches in AppData\Local: npm's, NuGet's downloads,
        // the browser engine's, and compiled shaders, Direct3D's and the
        // graphics driver's, all rebuilt as they are needed.
        | "npm-cache" | "v3-cache" | "inetcache" | "d3dscache" | "dxcache"
        | "glcache"
        // Symbols Xcode copies off a device it meets, and copies again the
        // next time that device is plugged in.
        | "ios devicesupport" | "watchos devicesupport" => Reclaim::Regenerable,
        ".stversions" => Reclaim::SyncHistory,
        "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache"
        | ".next" | ".turbo" | ".parcel-cache"
        // Xcode's build products and indexes, rebuilt on the next build.
        | "deriveddata" => Reclaim::BuildOutput,
        // ~/Library/Logs, told apart from a project's logs by its neighbour.
        "logs" if has_sibling("Application Support") => Reclaim::Temporary,
        "node_modules" if has_sibling("package.json") => Reclaim::Reinstallable,
        "trash" | ".trash" | "$recycle.bin" => Reclaim::Trash,
        // `Temp` is where Windows puts temporary files, in AppData\Local;
        // `CrashDumps` beside it holds dumps of programs that crashed.
        "tmp" | ".tmp" | "temp" | "crashdumps" => Reclaim::Temporary,
        _ => return None,
    };
    Some(reclaim)
}

/// Assign a category and a reclaim reason to every node beneath `root`.
///
/// Top-down: a node's own name wins, otherwise it inherits. Reclaimable
/// space is inherited too, so everything under a cache is hatched.
///
/// Two passes, so that both can spread across workers. Writing kinds sees
/// one directory's children at a time, but a git or pnpm store shows what
/// it is two levels down, so a pass that only reads finds those first.
pub fn classify(tree: &mut Tree) {
    let name = tree.root().name().to_owned();
    classify_at(tree, Path::new(&name), PathStyle::Native);
}

/// Classify using the scan root's full path, including tool ownership above
/// that root. SSH callers pass POSIX rules even on Windows.
pub fn classify_at(tree: &mut Tree, path: &Path, style: PathStyle) {
    let artifacts = Roots::find(tree, path, style);
    let stores = Stores::find(tree, artifacts);
    let root = tree.root();
    // Protected stores win over any cache identity inferred from a parent
    // above this scan root. Narrow scans retain the same ownership rules.
    let own = category_of_name(root.name())
        .filter(|category| {
            matches!(category, Category::FilesystemEvents | Category::Spotlight)
        })
        .map(|category| (category, None))
        .or_else(|| stores.is_git(root.id()).then_some((Category::Git, None)))
        .or_else(|| {
            stores
                .artifacts
                .get(root.id())
                .map(|d| (d.category(), d.reclaim()))
        })
        .or_else(|| {
            is_pnpm_store(root)
                .then_some((Category::Toolchain, Some(Reclaim::PackageStore)))
        });
    tree.par_top_down(
        |facts, mut block| {
            if let Some((category, reclaim)) = own {
                facts.category = category;
                facts.reclaim = reclaim;
                decide(&mut block, category, reclaim, &stores);
            } else {
                facts.category = Category::Other;
                facts.reclaim = None;
                decide_top(&mut block, &stores);
            }
        },
        |facts, mut block| {
            decide(&mut block, facts.category, facts.reclaim, &stores);
        },
    );
    // A generic cache can contain protected data. Never suggest the whole
    // parent when a descendant overrides its inherited cleanup assessment.
    tree.par_bottom_up(
        |_| {},
        |facts, block| {
            if facts.reclaim.is_some()
                && block.iter().any(|child| child.reclaim.is_none())
            {
                facts.reclaim = None;
            }
        },
    );
}

/// The children of a root that is nothing in particular: each takes only
/// its own evidence, as `kind_of` gives it for a parent of no kind, except
/// that a file directly in the root takes a kind from its own name too.
fn decide_top(block: &mut Block<'_>, stores: &Stores) {
    for index in 0..block.len() {
        let child = block.at(index);
        let (category, reclaim) =
            if let Some(d) = stores.artifacts.get(block.id(index)) {
                (d.category(), d.reclaim())
            } else if child.kind.is_dir() {
                kind_of(block, index, Category::Other, None, stores)
            } else {
                (
                    category_of_name(child.name).unwrap_or(Category::Other),
                    None,
                )
            };
        let facts = block.facts_mut(index);
        facts.category = category;
        facts.reclaim = reclaim;
    }
}

/// The children of a directory that is `category`, and `reclaim`.
fn decide(
    block: &mut Block<'_>,
    category: Category,
    reclaim: Option<Reclaim>,
    stores: &Stores,
) {
    // Most directories hold only files, which take this one's kind as
    // they are: no call per file.
    if !block.iter().enumerate().any(|(index, child)| {
        child.kind.is_dir() || stores.artifacts.get(block.id(index)).is_some()
    }) {
        block.for_each_facts_mut(|facts| {
            facts.category = category;
            facts.reclaim = reclaim;
        });
        return;
    }
    for index in 0..block.len() {
        let (category, reclaim) =
            kind_of(block, index, category, reclaim, stores);
        let facts = block.facts_mut(index);
        facts.category = category;
        facts.reclaim = reclaim;
    }
}

/// What the child at `index` is, given what its parent is: its own name
/// wins, otherwise it inherits. A file always inherits.
fn kind_of(
    siblings: &Block<'_>,
    index: usize,
    category: Category,
    reclaim: Option<Reclaim>,
    stores: &Stores,
) -> (Category, Option<Reclaim>) {
    let child = siblings.at(index);
    // These stores belong to macOS even when an internal folder is named
    // `cache` or `tmp`; raw deletion is not an appropriate cleanup method.
    if matches!(
        category,
        Category::FilesystemEvents
            | Category::Spotlight
            | Category::VirtualMachine
            | Category::Git
    ) {
        return (category, None);
    }
    let id = siblings.id(index);
    if stores.is_git(id) {
        return (Category::Git, None);
    }
    if let Some(detection) = stores.artifacts.get(id) {
        return (detection.category(), detection.reclaim());
    }
    if !child.kind.is_dir() {
        return (category, reclaim);
    }
    let has_sibling =
        |wanted: &str| siblings.iter().any(|sibling| sibling.name == wanted);
    let child_category = category_of_name(child.name)
        .or_else(|| stores.is_git(id).then_some(Category::Git))
        .unwrap_or(category);
    let child_reclaim = if matches!(
        child_category,
        Category::FilesystemEvents
            | Category::Spotlight
            | Category::Git
            | Category::VirtualMachine
    ) {
        None
    } else {
        reclaim
            .or_else(|| reclaim_of(child.name, category, has_sibling))
            .or_else(|| stores.is_pnpm(id).then_some(Reclaim::PackageStore))
    };
    (child_category, child_reclaim)
}

/// The directories whose kind is in their shape rather than their name,
/// by [`Node::id`], in order. There are few: a store per repository.
struct Stores {
    shapes: Vec<Shape>,
    artifacts: Roots,
}

#[derive(Clone, Copy)]
struct Shape {
    id: u32,
    git: bool,
    pnpm: bool,
}

impl Stores {
    fn find(tree: &Tree, artifacts: Roots) -> Self {
        // An ordered parallel collect keeps the ids ascending.
        Self {
            artifacts,
            shapes: tree
                .par_nodes()
                .filter(|node| node.is_dir())
                .filter_map(|node| {
                    let git = is_git_store(node);
                    let pnpm = is_pnpm_store(node);
                    (git || pnpm).then(|| Shape {
                        id: node.id(),
                        git,
                        pnpm,
                    })
                })
                .collect(),
        }
    }

    fn get(&self, id: u32) -> Option<Shape> {
        let found = self
            .shapes
            .binary_search_by_key(&id, |shape| shape.id)
            .ok()?;
        Some(self.shapes[found])
    }

    fn is_git(&self, id: u32) -> bool {
        self.get(id).is_some_and(|shape| shape.git)
    }

    fn is_pnpm(&self, id: u32) -> bool {
        self.get(id).is_some_and(|shape| shape.pnpm)
    }
}

/// pnpm's versioned content-addressable store, not its installation prefix.
/// `pnpm` also holds its executable, global tools and `.tools`; only a
/// store with a version directory containing `files` is a cleanup candidate.
pub(crate) fn is_pnpm_store(node: Node<'_>) -> bool {
    node.is_dir()
        && matches!(node.name(), "store" | ".pnpm-store")
        && node.children().iter().any(|version| {
            version.is_dir()
                && version.name().strip_prefix('v').is_some_and(|number| {
                    !number.is_empty()
                        && number.bytes().all(|c| c.is_ascii_digit())
                })
                && version
                    .children()
                    .iter()
                    .any(|child| child.is_dir() && child.name() == "files")
        })
}

/// A git object store by its shape, whatever it is called: a bare
/// repository, or a `.git` directory, has `objects`, `refs` and `HEAD`.
pub fn is_git_store(node: Node<'_>) -> bool {
    let has = |wanted: &str| {
        node.children().iter().any(|child| child.name() == wanted)
    };
    node.is_dir() && has("objects") && has("refs") && has("HEAD")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Draft, Tree};
    use crate::tree::{Metric, NodeKind, aggregate};

    fn file(name: &str, bytes: u64) -> Draft {
        Draft::entry(name, NodeKind::File, bytes)
    }

    fn dir(name: &str, children: Vec<Draft>) -> Draft {
        let mut node = Draft::directory(name);
        node.children = children;
        node
    }

    fn home() -> Tree {
        let home = dir(
            "tobi",
            vec![
                dir(
                    "src",
                    vec![dir(
                        "tries",
                        vec![dir("2026-09-01", vec![file("a", 1)])],
                    )],
                ),
                dir(
                    ".cache",
                    vec![dir("kache", vec![dir("store", vec![file("b", 1)])])],
                ),
                dir(
                    "world",
                    vec![
                        dir(".git", vec![dir("objects", vec![file("c", 9)])]),
                        file("README", 1),
                    ],
                ),
                dir(
                    "rust-thing",
                    vec![
                        file("Cargo.toml", 1),
                        dir("target", vec![file("d", 5)]),
                    ],
                ),
                dir("js-thing", vec![dir("target", vec![file("e", 5)])]),
                dir(
                    ".microsandbox",
                    vec![
                        dir("cache", vec![dir("layers", vec![file("f", 1)])]),
                        dir("snapshots", vec![file("g", 1)]),
                    ],
                ),
                dir("Sync", vec![dir(".stversions", vec![file("h", 1)])]),
                dir("mystery", vec![file("i", 1)]),
                dir(
                    "monorepo",
                    vec![dir(
                        "git",
                        vec![
                            file("HEAD", 1),
                            dir("refs", vec![]),
                            dir("objects", vec![file("pack", 50)]),
                        ],
                    )],
                ),
            ],
        );
        let mut home = Tree::from(home);
        aggregate(&mut home, Metric::Bytes);
        classify(&mut home);
        home
    }

    fn named<'a>(node: Node<'a>, path: &[&str]) -> Node<'a> {
        let mut node = node;
        for part in path {
            node = node
                .children()
                .iter()
                .find(|child| child.name() == *part)
                .unwrap_or_else(|| panic!("no {part}"));
        }
        node
    }

    /// Regression: a Steam library on a second drive was named by the
    /// `temp` folder Steam keeps in `steamapps`, so a disk of games showed
    /// as cache.
    #[test]
    fn a_steam_library_on_another_drive_is_media_not_its_temp_folder() {
        let root = dir(
            "data",
            vec![dir(
                "SteamLibrary",
                vec![dir(
                    "steamapps",
                    vec![
                        dir("temp", vec![file("partial", 1)]),
                        dir("common", vec![file("game.pak", 1_000)]),
                    ],
                )],
            )],
        );
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);
        classify(&mut root);
        let library = root.root().children().at(0);
        assert_eq!(library.category, Category::Media);
        let common = library
            .children()
            .at(0)
            .child_named("common")
            .expect("common");
        assert_eq!(common.category, Category::Media);
        assert_eq!(common.reclaim, None, "installed games are not hatched");
    }

    #[test]
    fn steam_libraries_and_emulation_are_media_not_disposable_caches() {
        for name in ["SteamLibrary", "steamapps", "Emulation"] {
            assert_eq!(category_of_name(name), Some(Category::Media));
            assert_eq!(reclaim_of(name, Category::Other, |_| false), None);
        }
    }

    #[test]
    fn names_announce_their_kind_and_children_inherit_it() {
        let home = home();
        assert_eq!(named(home.root(), &["src"]).category, Category::Code);
        // `tries` is agent scratch even inside code: it is what agents write.
        assert_eq!(
            named(home.root(), &["src", "tries"]).category,
            Category::AgentScratch
        );
        assert_eq!(
            named(home.root(), &["src", "tries", "2026-09-01"]).category,
            Category::AgentScratch,
            "an unknown name inherits"
        );
        assert_eq!(
            named(home.root(), &["src", "tries", "2026-09-01", "a"]).category,
            Category::AgentScratch,
            "files inherit too"
        );
    }

    #[test]
    fn unknown_aggregates_do_not_guess_from_descendants() {
        let home = home();
        assert_eq!(named(home.root(), &["world"]).category, Category::Other);
        assert_eq!(named(home.root(), &["mystery"]).category, Category::Other);
        assert_eq!(
            named(home.root(), &["world", "README"]).category,
            Category::Other
        );
        // A bare repository is git by its shape, whatever it is called.
        assert_eq!(named(home.root(), &["monorepo"]).category, Category::Other);
        assert_eq!(
            named(home.root(), &["monorepo", "git"]).category,
            Category::Git
        );
    }

    #[test]
    fn pnpm_installations_are_not_package_stores() {
        let store = || {
            dir(
                "store",
                vec![dir(
                    "v10",
                    vec![
                        dir(
                            "files",
                            vec![dir("ab", vec![file("package", 10)])],
                        ),
                        dir("index", vec![]),
                    ],
                )],
            )
        };
        let root = dir(
            "Library",
            vec![
                dir(
                    "pnpm",
                    vec![
                        file("pnpm", 1),
                        dir("global", vec![file("tool", 2)]),
                        dir(".tools", vec![file("pnpm", 3)]),
                        store(),
                    ],
                ),
                dir(".pnpm-store", vec![file("unverified", 4)]),
                dir(".PNPM-STORE", vec![file("unverified", 4)]),
            ],
        );
        let mut root = Tree::from(root);
        classify(&mut root);
        for path in [
            vec!["pnpm"],
            vec!["pnpm", "pnpm"],
            vec!["pnpm", "global", "tool"],
            vec!["pnpm", ".tools", "pnpm"],
            vec![".pnpm-store", "unverified"],
            vec![".PNPM-STORE", "unverified"],
        ] {
            assert_eq!(named(root.root(), &path).reclaim, None, "{path:?}");
        }
        assert_eq!(
            named(
                root.root(),
                &["pnpm", "store", "v10", "files", "ab", "package"]
            )
            .reclaim,
            Some(Reclaim::PackageStore)
        );
        let mut standalone = store();
        standalone.name = ".pnpm-store".into();
        let mut standalone = Tree::from(standalone);
        classify(&mut standalone);
        assert_eq!(standalone.root().reclaim, Some(Reclaim::PackageStore));
        assert_eq!(reclaim_of("pnpm", Category::Toolchain, |_| false), None);
        for contents in [
            dir("version", vec![dir("files", vec![])]),
            dir("v", vec![dir("files", vec![])]),
            dir("v3-backup", vec![dir("files", vec![])]),
            dir("v3", vec![file("files", 1)]),
        ] {
            let unverified = dir(".pnpm-store", vec![contents]);
            let mut unverified = Tree::from(unverified);
            classify(&mut unverified);
            assert_eq!(unverified.root().reclaim, None);
            assert_eq!(unverified.root().children().at(0).reclaim, None);
        }
    }

    #[test]
    fn a_whole_root_scan_keeps_categories_and_filters_local_to_evidence() {
        use crate::query::{Filter, find};
        use std::path::Path;
        let root = dir(
            "/",
            vec![dir(
                "Users",
                vec![dir(
                    "fixture",
                    vec![
                        dir(
                            ".codex",
                            vec![dir("scratch", vec![file("experiment", 100)])],
                        ),
                        dir(
                            "Library",
                            vec![
                                dir("VM data", vec![file("disk", 30)]),
                                dir("snapshots", vec![file("vm-snapshot", 20)]),
                            ],
                        ),
                        dir(
                            "My Drive (Fixture Account)",
                            vec![dir(
                                "client files",
                                vec![file("document", 10)],
                            )],
                        ),
                    ],
                )],
            )],
        );
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);
        classify(&mut root);
        for path in [
            vec!["Users"],
            vec!["Users", "fixture"],
            vec!["Users", "fixture", "Library", "VM data", "disk"],
            vec!["Users", "fixture", "Library", "snapshots"],
        ] {
            let node = named(root.root(), &path);
            assert_eq!(node.category, Category::Other, "{path:?}");
            assert_eq!(node.reclaim, None, "{path:?}");
        }
        for (category, expected) in [
            (Category::AgentScratch, "/Users/fixture/.codex"),
            (
                Category::Synced,
                "/Users/fixture/My Drive (Fixture Account)",
            ),
        ] {
            let found = find(
                Path::new("/"),
                root.root(),
                &Filter {
                    category: Some(category),
                    ..Filter::default()
                },
                20,
            );
            assert_eq!(found.count, 1);
            assert_eq!(found.matches[0].0, Path::new(expected));
        }
        assert_eq!(
            named(
                root.root(),
                &[
                    "Users",
                    "fixture",
                    "My Drive (Fixture Account)",
                    "client files",
                    "document"
                ]
            )
            .category,
            Category::Synced
        );
        for name in [
            "My Drive",
            "My Drive (Another Account)",
            "Google Drive (work)",
        ] {
            assert_eq!(category_of_name(name), Some(Category::Synced));
        }
        for name in ["My Driver", "My Drive (unfinished", "My Drive backups"] {
            assert_eq!(category_of_name(name), None);
        }
    }

    #[test]
    fn caches_are_reclaimable_all_the_way_down() {
        let home = home();
        assert_eq!(
            named(home.root(), &[".cache"]).reclaim,
            Some(Reclaim::Regenerable)
        );
        assert_eq!(
            named(home.root(), &[".cache", "kache", "store"]).reclaim,
            Some(Reclaim::Regenerable)
        );
        assert_eq!(
            named(home.root(), &["Sync", ".stversions"]).reclaim,
            Some(Reclaim::SyncHistory)
        );
        assert_eq!(named(home.root(), &["src"]).reclaim, None);
    }

    #[test]
    fn a_manifest_does_not_promote_a_whole_target_directory() {
        let home = home();
        assert_eq!(named(home.root(), &["rust-thing", "target"]).reclaim, None);
        assert_eq!(named(home.root(), &["js-thing", "target"]).reclaim, None);
    }

    #[test]
    fn snapshots_are_not_suggested_from_agent_scratch_ancestry() {
        let home = home();
        // .microsandbox/cache is already a regenerable cache, so its layers
        // inherit that; the snapshots are judged on their own.
        assert!(
            named(home.root(), &[".microsandbox", "cache", "layers"])
                .reclaim
                .is_some()
        );
        assert_eq!(
            named(home.root(), &[".microsandbox", "snapshots"]).reclaim,
            None
        );
        assert_eq!(
            reclaim_of("snapshots", Category::Documents, |_| false),
            None
        );
    }

    #[test]
    fn macos_developer_leftovers_are_reclaimable_and_its_risks_are_not() {
        let none = |_: &str| false;
        assert_eq!(
            reclaim_of("DerivedData", Category::Toolchain, none),
            Some(Reclaim::BuildOutput)
        );
        assert_eq!(
            reclaim_of("iOS DeviceSupport", Category::Toolchain, none),
            Some(Reclaim::Regenerable)
        );
        let library = |name: &str| name == "Application Support";
        assert_eq!(
            reclaim_of("Logs", Category::Other, library),
            Some(Reclaim::Temporary)
        );
        assert_eq!(reclaim_of("logs", Category::Code, none), None);
        // Big, but not safe to offer: Archives hold the symbols crash
        // reports need, and Backup is an iPhone's only backup.
        assert_eq!(reclaim_of("Archives", Category::Toolchain, none), None);
        assert_eq!(reclaim_of("Backup", Category::Other, none), None);
        assert_eq!(
            category_of_name("Mobile Documents"),
            Some(Category::Synced)
        );
        // ~/Developer holds a user's projects, not a toolchain.
        assert_eq!(category_of_name("Developer"), None);
        assert_eq!(category_of_name("Xcode"), Some(Category::Toolchain));
    }

    #[test]
    fn windows_caches_and_sync_folders_are_recognized() {
        let none = |_: &str| false;
        for (name, reclaim) in [
            ("Temp", Reclaim::Temporary),
            ("CrashDumps", Reclaim::Temporary),
            ("npm-cache", Reclaim::Regenerable),
            ("v3-cache", Reclaim::Regenerable),
            ("INetCache", Reclaim::Regenerable),
            ("D3DSCache", Reclaim::Regenerable),
            ("DXCache", Reclaim::Regenerable),
            ("$Recycle.Bin", Reclaim::Trash),
        ] {
            assert_eq!(
                reclaim_of(name, Category::Other, none),
                Some(reclaim),
                "{name}"
            );
            assert_eq!(category_of_name(name), Some(Category::Cache), "{name}");
        }
        for name in [
            "OneDrive - Contoso",
            // Past the stack buffer for lowercasing.
            "OneDrive - Contoso Pharmaceuticals International",
            "Dropbox (Contoso)",
            "iCloudDrive",
        ] {
            assert_eq!(
                category_of_name(name),
                Some(Category::Synced),
                "{name}"
            );
        }
        assert_eq!(category_of_name(".nuget"), Some(Category::Toolchain));
        assert_eq!(category_of_name("OneDriveSetup"), None);
    }

    #[test]
    fn macos_stores_are_named_without_suggesting_raw_deletion() {
        for (name, category) in [
            (".fseventsd", Category::FilesystemEvents),
            (".Spotlight-V100", Category::Spotlight),
        ] {
            let store = dir(name, vec![dir("cache", vec![file("blob", 123)])]);
            let mut store = Tree::from(store);
            aggregate(&mut store, Metric::Bytes);
            classify(&mut store);
            let root = store.root();
            assert_eq!(root.category, category);
            assert_eq!(root.children().at(0).category, category);
            assert_eq!(
                root.children().at(0).children().at(0).category,
                category
            );
            assert_eq!(root.children().at(0).reclaim, None);
            assert_eq!(root.bytes, 123);
            let volume =
                dir("volume", vec![root.to_draft(), dir("ordinary", vec![])]);
            let mut volume = Tree::from(volume);
            classify(&mut volume);
            let volume = volume.root();
            assert_eq!(volume.children().at(0).category, category);
            assert_eq!(volume.children().at(1).category, Category::Other);
            assert_eq!(volume.children().at(0).reclaim, None);
        }
    }

    /// What lies directly in the root is judged by its own evidence only:
    /// its name, or its shape for a store, never its parent's kind.
    #[test]
    fn the_roots_children_take_only_their_own_evidence() {
        let root = dir(
            "volume",
            vec![
                file("tmp", 1),
                file("README", 1),
                dir(
                    "bare",
                    vec![
                        dir("objects", vec![]),
                        dir("refs", vec![]),
                        file("HEAD", 1),
                    ],
                ),
                dir("store", vec![dir("v3", vec![dir("files", vec![])])]),
                dir("Caches", vec![file("blob", 1)]),
                dir("plain", vec![file("tmp", 1)]),
            ],
        );
        let mut root = Tree::from(root);
        classify(&mut root);
        let kind = |path: &[&str]| {
            let node = named(root.root(), path);
            (node.category, node.reclaim)
        };
        // A file directly in the root takes a kind from its name, but
        // never a reclaim reason; deeper, a file only inherits.
        assert_eq!(kind(&["tmp"]), (Category::Cache, None));
        assert_eq!(kind(&["README"]), (Category::Other, None));
        assert_eq!(kind(&["plain", "tmp"]), (Category::Other, None));
        assert_eq!(kind(&["bare"]), (Category::Git, None));
        assert_eq!(kind(&["bare", "HEAD"]), (Category::Git, None));
        assert_eq!(
            kind(&["store"]),
            (Category::Other, Some(Reclaim::PackageStore))
        );
        assert_eq!(
            kind(&["Caches", "blob"]),
            (Category::Cache, Some(Reclaim::Regenerable))
        );
    }

    #[test]
    fn the_legend_lists_every_named_category_once() {
        let mut seen = std::collections::HashSet::new();
        for category in Category::LEGEND {
            assert!(seen.insert(category));
            assert_ne!(category, Category::Other);
            assert!(!category.label().is_empty());
        }
    }
}
