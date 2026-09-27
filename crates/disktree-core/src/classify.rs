//! What a directory *is*, and whether its space can be had back.
//!
//! Colour in the treemap means a kind of data, and a hatch means reclaimable
//! space, so the two questions a cleanup tool exists to answer — "what is it"
//! and "can I delete it" — can be read off a tile at a glance.
//!
//! Both come from names. A short lookup of well-known directory names covers
//! most of a home directory; anything unmatched takes its parent's kind, and a
//! top-level directory with an unknown name takes the kind of its largest
//! recognisable child (`~/world` is mostly `.git`, so it is git). Reclaimable
//! space is the same idea, plus a sibling check where a name alone is too
//! common to trust: `target` is only a build directory beside a `Cargo.toml`.

use std::borrow::Cow;

use rayon::prelude::*;

use crate::tree::{Dir, Item, Node, Tree, code, decode, name_in};

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
    /// Nothing recognisable.
    #[default]
    Other,
}

impl Category {
    /// The categories the legend lists, in its order.
    pub const LEGEND: [Self; 8] = [
        Self::Code,
        Self::AgentScratch,
        Self::Toolchain,
        Self::Synced,
        Self::Git,
        Self::Media,
        Self::Documents,
        Self::Cache,
    ];

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
            Self::Other => "Other",
        }
    }
}

/// Why a directory's space can be had back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reclaim {
    /// A cache: whatever wrote it will write it again.
    Regenerable,
    /// A sync client's old versions of files.
    SyncHistory,
    /// A package manager's content store.
    PackageStore,
    /// Compiler or bundler output beside its sources.
    BuildOutput,
    /// Installed dependencies beside their manifest.
    Reinstallable,
    /// Container or sandbox image layers.
    SandboxLayers,
    /// Sandbox or VM snapshots.
    Snapshots,
    /// Already deleted, still on disk.
    Trash,
    /// Scratch space meant to be thrown away.
    Temporary,
}

impl Reclaim {
    /// Every reason, in declaration order: what a tree numbers them by.
    pub(crate) const ALL: [Self; 9] = [
        Self::Regenerable,
        Self::SyncHistory,
        Self::PackageStore,
        Self::BuildOutput,
        Self::Reinstallable,
        Self::SandboxLayers,
        Self::Snapshots,
        Self::Trash,
        Self::Temporary,
    ];

    /// The reason, as the "Worth a look" list says it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Regenerable => "regenerable",
            Self::SyncHistory => "sync history",
            Self::PackageStore => "package store",
            Self::BuildOutput => "build output",
            Self::Reinstallable => "reinstallable",
            Self::SandboxLayers => "sandbox layers",
            Self::Snapshots => "snapshots",
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
    if lower.starts_with("onedrive - ") || lower.starts_with("dropbox (") {
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
        "sync" | "dropbox" | "nextcloud" | "google drive" | "onedrive"
        | "pclouddrive" | "mega" | ".stversions"
        // macOS: iCloud Drive, and the File Provider clients (Dropbox,
        // Google Drive, OneDrive) since macOS 12.
        | "mobile documents" | "cloudstorage"
        // Windows: iCloud for Windows.
        | "iclouddrive" => Category::Synced,
        ".git" => Category::Git,
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

/// Whether a directory's space can be had back, judged from its name, the
/// kind of the directory holding it, and its siblings' names.
pub fn reclaim_of(
    name: &str,
    parent: Category,
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
        ".pnpm-store" | "pnpm" => Reclaim::PackageStore,
        "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache"
        | ".next" | ".turbo" | ".parcel-cache"
        // Xcode's build products and indexes, rebuilt on the next build.
        | "deriveddata" => Reclaim::BuildOutput,
        // ~/Library/Logs, told apart from a project's logs by its neighbour.
        "logs" if has_sibling("Application Support") => Reclaim::Temporary,
        // Too common to trust alone: only a build directory beside a manifest.
        "target" if has_sibling("Cargo.toml") => Reclaim::BuildOutput,
        "node_modules" if has_sibling("package.json") => Reclaim::Reinstallable,
        // Layers and snapshots are only disposable inside sandbox state.
        "layers" if parent == Category::AgentScratch => Reclaim::SandboxLayers,
        "snapshots" if parent == Category::AgentScratch => Reclaim::Snapshots,
        "trash" | ".trash" | "$recycle.bin" => Reclaim::Trash,
        // `Temp` is where Windows puts temporary files, in AppData\Local;
        // `CrashDumps` beside it holds dumps of programs that crashed.
        "tmp" | ".tmp" | "temp" | "crashdumps" => Reclaim::Temporary,
        _ => return None,
    };
    Some(reclaim)
}

/// Assign a category and a reclaim reason to every directory of `tree`;
/// a file is of the kind of what holds it, or beneath the root of what
/// its name says: see [`Node::kinds`].
///
/// Top-down: a directory's own name wins, otherwise it inherits.
/// Reclaimable space is inherited too, so everything under a cache is
/// hatched.
pub fn classify(tree: &mut Tree) {
    classify_where(tree, None);
}

/// [`classify`], deciding again only where a change can reach when
/// `touched` says where changes are: a directory it marks (its entries
/// changed, or some beneath it did), whose entries' kinds may follow
/// their new siblings, and every directory whose kind came out other than
/// before, whose entries inherit it. Beneath any other, nothing the kinds
/// depend on changed. The root's entries are always decided again: one
/// takes the kind of its largest child, and sizes anywhere beneath can
/// change which that is.
pub(crate) fn classify_where(tree: &mut Tree, touched: Option<&[bool]>) {
    let Some(root) = tree.dirs.first().copied() else {
        return;
    };
    let shared: &Tree = tree;
    let run = shared.run(&root);
    let text = shared
        .segs
        .get(root.seg as usize)
        .map_or("", |seg| &seg.text);
    let mut kinds: Vec<(u64, (u8, u8))> = run
        .par_iter()
        .filter(|item| item.is_dir())
        .flat_map_iter(|item| {
            let (category, reclaim) = top_level_kind(
                name_in(text, item),
                true,
                || is_git_store_at(shared, item.value),
                || dominant(shared, item.value),
                |wanted| has(run, text, wanted),
            );
            let mut kinds = Vec::new();
            decide(
                shared,
                item.value,
                code(category, reclaim),
                touched,
                1,
                &mut kinds,
            );
            kinds
        })
        .collect();
    kinds.push((0, code(Category::Other, None)));
    for (index, (category, reclaim)) in kinds {
        if let Some(dir) = usize::try_from(index)
            .ok()
            .and_then(|index| tree.dirs.get_mut(index))
        {
            dir.category = category;
            dir.reclaim = reclaim;
        }
    }
}

/// Entries, all levels down, past which a directory's entries are decided
/// across threads. By size rather than by depth: a disk's weight sits
/// unevenly, one user's folder four levels down holding half of it, and
/// below this a task costs more than it saves.
const SPLIT: u64 = 1 << 15;

/// Levels a tree is decided to: a real one is far shallower, and one kept
/// on disk that loops must still end.
const MOST_LEVELS: usize = 1024;

/// Directory `index` is of `kind` now: note it in `out` if that is new,
/// and go on beneath it where [`classify_where`] says to.
fn decide(
    tree: &Tree,
    index: u64,
    kind: (u8, u8),
    touched: Option<&[bool]>,
    depth: usize,
    out: &mut Vec<(u64, (u8, u8))>,
) {
    let Some(dir) = tree.dir(index) else {
        return;
    };
    let changed = (dir.category, dir.reclaim) != kind;
    if changed {
        out.push((index, kind));
    }
    let here = touched.is_none_or(|touched| {
        usize::try_from(index)
            .ok()
            .and_then(|index| touched.get(index))
            .copied()
            .unwrap_or(false)
    });
    if !changed && !here || depth >= MOST_LEVELS {
        return;
    }
    let (category, reclaim) = decode(kind);
    let run = tree.run(dir);
    let text = tree.segs.get(dir.seg as usize).map_or("", |seg| &seg.text);
    let child = |item: &Item, out: &mut Vec<(u64, (u8, u8))>| {
        if !item.is_dir() {
            return;
        }
        let (category, reclaim) = directory_kind(
            name_in(text, item),
            || is_git_store_at(tree, item.value),
            |wanted| has(run, text, wanted),
            category,
            reclaim,
        );
        decide(
            tree,
            item.value,
            code(category, reclaim),
            touched,
            depth + 1,
            out,
        );
    };
    if dir.files.saturating_add(u64::from(dir.dirs)) > SPLIT {
        out.par_extend(run.par_iter().flat_map_iter(|item| {
            let mut kinds = Vec::new();
            child(item, &mut kinds);
            kinds
        }));
    } else {
        for item in run {
            child(item, out);
        }
    }
}

/// What an entry directly beneath the scanned root is: its own name, the
/// shape of a git store, or, for a directory with an unknown name, the
/// kind of its largest recognisable child (`~/world` is mostly `.git`).
pub(crate) fn top_level_kind(
    name: &str,
    is_dir: bool,
    is_git: impl FnOnce() -> bool,
    dominant: impl FnOnce() -> Option<Category>,
    has_sibling: impl Fn(&str) -> bool,
) -> (Category, Option<Reclaim>) {
    let category = category_of_name(name)
        .or_else(|| is_git().then_some(Category::Git))
        .or_else(dominant)
        .unwrap_or(Category::Other);
    let reclaim = is_dir
        .then(|| reclaim_of(name, Category::Other, has_sibling))
        .flatten();
    (category, reclaim)
}

/// What a directory deeper down is, given what holds it: its own name
/// wins, then the shape of a git store, otherwise it inherits. Reclaimable
/// space is inherited, or judged from its name and its siblings'.
pub(crate) fn directory_kind(
    name: &str,
    is_git: impl FnOnce() -> bool,
    has_sibling: impl Fn(&str) -> bool,
    category: Category,
    reclaim: Option<Reclaim>,
) -> (Category, Option<Reclaim>) {
    let child_category = category_of_name(name)
        .or_else(|| is_git().then_some(Category::Git))
        .unwrap_or(category);
    let child_reclaim =
        reclaim.or_else(|| reclaim_of(name, category, has_sibling));
    (child_category, child_reclaim)
}

/// Whether a run has an entry named `wanted`.
fn has(run: &[Item], text: &str, wanted: &str) -> bool {
    run.iter().any(|item| name_in(text, item) == wanted)
}

/// The kind of an unknown directory, from what fills it: the first
/// recognisable name down the largest children, a few levels deep.
fn dominant(tree: &Tree, index: u64) -> Option<Category> {
    let mut dir: &Dir = tree.dir(index)?;
    for _ in 0..3 {
        let run = tree.run(dir);
        let text = tree.segs.get(dir.seg as usize).map_or("", |seg| &seg.text);
        let mut subdirectories = run.iter().filter(|item| item.is_dir());
        if let Some(category) = subdirectories.clone().find_map(|item| {
            category_of_name(name_in(text, item)).or_else(|| {
                is_git_store_at(tree, item.value).then_some(Category::Git)
            })
        }) {
            return Some(category);
        }
        dir = tree.dir(subdirectories.next()?.value)?;
    }
    None
}

/// The entries a git object store holds, whatever it is called: a bare
/// repository, or a `.git` directory, has `objects`, `refs` and `HEAD`.
pub(crate) const GIT_STORE: [&str; 3] = ["objects", "refs", "HEAD"];

/// A git object store by its shape: see [`GIT_STORE`].
pub fn is_git_store(node: Node<'_>) -> bool {
    node.is_dir()
        && GIT_STORE
            .iter()
            .all(|wanted| node.children().any(|child| child.name() == *wanted))
}

/// [`is_git_store`] for directory `index`.
fn is_git_store_at(tree: &Tree, index: u64) -> bool {
    tree.dir(index).is_some_and(|dir| {
        let run = tree.run(dir);
        let text = tree.segs.get(dir.seg as usize).map_or("", |seg| &seg.text);
        GIT_STORE.iter().all(|wanted| has(run, text, wanted))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Draft, Metric, NodeKind};

    fn file(name: &str, bytes: u64) -> Draft {
        Draft::entry(name, NodeKind::File, bytes)
    }

    fn dir(name: &str, children: Vec<Draft>) -> Draft {
        Draft {
            children,
            ..Draft::directory(name)
        }
    }

    fn classified(root: Draft) -> Tree {
        let mut tree = Tree::from_draft(root, Metric::Bytes);
        classify(&mut tree);
        tree
    }

    fn home() -> Tree {
        classified(dir(
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
        ))
    }

    fn named<'a>(tree: &'a Tree, path: &[&str]) -> Node<'a> {
        let mut node = tree.root();
        for part in path {
            node = node
                .child_named(part)
                .unwrap_or_else(|| panic!("no {part}"));
        }
        node
    }

    /// Regression: a Steam library on a second drive was named by the
    /// `temp` folder Steam keeps in `steamapps`, so a disk of games showed
    /// as cache.
    #[test]
    fn a_steam_library_on_another_drive_is_media_not_its_temp_folder() {
        let root = classified(dir(
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
        ));
        let library = named(&root, &["SteamLibrary"]);
        assert_eq!(library.category(), Category::Media);
        let common = named(&root, &["SteamLibrary", "steamapps", "common"]);
        assert_eq!(common.category(), Category::Media);
        assert_eq!(common.reclaim(), None, "installed games are not hatched");
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
        assert_eq!(named(&home, &["src"]).category(), Category::Code);
        // `tries` is agent scratch even inside code: it is what agents write.
        assert_eq!(
            named(&home, &["src", "tries"]).category(),
            Category::AgentScratch
        );
        assert_eq!(
            named(&home, &["src", "tries", "2026-09-01"]).category(),
            Category::AgentScratch,
            "an unknown name inherits"
        );
        assert_eq!(
            named(&home, &["src", "tries", "2026-09-01", "a"]).category(),
            Category::AgentScratch,
            "files inherit too"
        );
    }

    #[test]
    fn an_unknown_top_level_directory_takes_its_largest_known_child() {
        let home = home();
        assert_eq!(named(&home, &["world"]).category(), Category::Git);
        assert_eq!(named(&home, &["mystery"]).category(), Category::Other);
        // A bare repository is git by its shape, whatever it is called.
        assert_eq!(named(&home, &["monorepo"]).category(), Category::Git);
        assert_eq!(
            named(&home, &["monorepo", "git"]).category(),
            Category::Git
        );
    }

    #[test]
    fn caches_are_reclaimable_all_the_way_down() {
        let home = home();
        assert_eq!(
            named(&home, &[".cache"]).reclaim(),
            Some(Reclaim::Regenerable)
        );
        assert_eq!(
            named(&home, &[".cache", "kache", "store"]).reclaim(),
            Some(Reclaim::Regenerable)
        );
        assert_eq!(
            named(&home, &["Sync", ".stversions"]).reclaim(),
            Some(Reclaim::SyncHistory)
        );
        assert_eq!(named(&home, &["src"]).reclaim(), None);
    }

    #[test]
    fn target_is_build_output_only_beside_a_cargo_manifest() {
        let home = home();
        assert_eq!(
            named(&home, &["rust-thing", "target"]).reclaim(),
            Some(Reclaim::BuildOutput)
        );
        assert_eq!(named(&home, &["js-thing", "target"]).reclaim(), None);
    }

    #[test]
    fn sandbox_layers_and_snapshots_are_reclaimable_only_in_sandbox_state() {
        let home = home();
        // .microsandbox/cache is already a regenerable cache, so its layers
        // inherit that; the snapshots are judged on their own.
        assert!(
            named(&home, &[".microsandbox", "cache", "layers"])
                .reclaim()
                .is_some()
        );
        assert_eq!(
            named(&home, &[".microsandbox", "snapshots"]).reclaim(),
            Some(Reclaim::Snapshots)
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
    fn the_legend_lists_every_named_category_once() {
        let mut seen = std::collections::HashSet::new();
        for category in Category::LEGEND {
            assert!(seen.insert(category));
            assert_ne!(category, Category::Other);
            assert!(!category.label().is_empty());
        }
    }
}
