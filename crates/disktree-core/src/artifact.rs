//! Artifact identity is evidence about contents, never removal permission.
//!
//! Layout rules are shared with `DiskHound`. Only recognized roots receive
//! an assessment; a matching child never promotes its parent. VM ownership
//! takes precedence over cache names, including inside VM bundles.

use std::path::Path;

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::classify::{Category, Reclaim};
use crate::path_style::PathStyle;
use crate::tree::{Node, Tree};

mod rules;

#[cfg(test)]
thread_local! {
    static WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn step() {
    WORK.set(WORK.get() + 1);
}

#[cfg(not(test))]
const fn step() {}

/// What was recognized, independently of whether cleanup is suggested.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Artifact {
    Worktree,
    NodeModules,
    PackageCache,
    RustTarget,
    CargoRegistry,
    JsBuild,
    Python,
    GoModule,
    Jvm,
    Dotnet,
    CompilerCache,
    CmakeBuild,
    Terraform,
    GitRepo,
    DiagLogs,
    VirtualMachine,
    VmDisk,
    VmSnapshot,
    VmSuspend,
}

impl Artifact {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::NodeModules => "Node dependencies",
            Self::PackageCache => "package cache",
            Self::RustTarget => "Rust build cache",
            Self::CargoRegistry => "Cargo registry",
            Self::JsBuild => "frontend output",
            Self::Python => "Python packages / cache",
            Self::GoModule => "Go modules / cache",
            Self::Jvm => "Gradle / JVM cache",
            Self::Dotnet => ".NET packages / intermediates",
            Self::CompilerCache => "compiler cache",
            Self::CmakeBuild => "CMake intermediates",
            Self::Terraform => "Terraform providers",
            Self::GitRepo => "Git history",
            Self::DiagLogs => "diagnostic logs",
            Self::VirtualMachine => "virtual machine data",
            Self::VmDisk => "VM disk image",
            Self::VmSnapshot => "VM snapshot",
            Self::VmSuspend => "VM saved memory / suspend state",
        }
    }

    pub const fn is_vm(self) -> bool {
        matches!(
            self,
            Self::VirtualMachine
                | Self::VmDisk
                | Self::VmSnapshot
                | Self::VmSuspend
        )
    }
}

/// Strength of identification, not a confidence score for deletion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Evidence {
    /// A distinctive tool name or file extension can still be a lookalike.
    Name,
    /// Multiple path components follow a documented tool layout.
    Layout,
}

/// Whether the recognized contents are a plausible cleanup candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    Detected,
    Suggested,
}

/// A path's identity and the independent assessment of cleanup suitability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detection {
    pub kind: Artifact,
    pub evidence: Evidence,
    pub disposition: Disposition,
}

impl Detection {
    /// A protected descendant can make the whole directory unsuitable for
    /// suggestion even though its path still identifies an artifact.
    #[must_use]
    pub fn assessed(mut self, node: Node<'_>) -> Self {
        if node.reclaim.is_none() {
            self.disposition = Disposition::Detected;
        }
        self
    }

    const fn new(kind: Artifact, evidence: Evidence) -> Self {
        let disposition = match kind {
            Artifact::Worktree
            | Artifact::GitRepo
            | Artifact::DiagLogs
            | Artifact::VirtualMachine
            | Artifact::VmDisk
            | Artifact::VmSnapshot
            | Artifact::VmSuspend => Disposition::Detected,
            _ => Disposition::Suggested,
        };
        Self {
            kind,
            evidence,
            disposition,
        }
    }

    pub const fn status(self) -> &'static str {
        match self.disposition {
            Disposition::Detected => "Detected",
            Disposition::Suggested => "Suggested for review",
        }
    }

    pub const fn evidence_label(self) -> &'static str {
        match self.evidence {
            Evidence::Name => "name convention",
            Evidence::Layout => "tool layout",
        }
    }

    pub const fn guidance(self) -> &'static str {
        match self.kind {
            Artifact::VmDisk | Artifact::VirtualMachine => {
                "May contain unique work. Manage this VM or container in its owner app. Listed bytes are not guaranteed recoverable space."
            }
            Artifact::VmSnapshot => {
                "Review and consolidate snapshots in the VM app. Disks can depend on snapshot chains; do not remove individual snapshot files."
            }
            Artifact::VmSuspend => {
                "Saved memory may belong to a suspended VM or a checkpoint. Resume the VM and shut it down normally; let the VM app release the state."
            }
            Artifact::GitRepo => {
                "Git history may contain unpushed commits and recovery data. Inspect it with Git; detection is not a cleanup suggestion."
            }
            Artifact::Worktree => {
                "Check uncommitted and unpushed work, then manage the worktree with Git. Age and size do not establish disposability."
            }
            Artifact::DiagLogs => {
                "Review whether these logs are needed for diagnosis and whether their owner is still writing them."
            }
            Artifact::NodeModules | Artifact::Python => {
                "Review local edits and stop builds or running environments first. Reinstalling packages or rebuilding may require network access."
            }
            Artifact::RustTarget
            | Artifact::JsBuild
            | Artifact::CompilerCache
            | Artifact::CmakeBuild => {
                "Recognized build or cache layout. Review local changes and stop builds first; use the build tool's clean operation. The next build can be slower."
            }
            _ => {
                "Recognized dependency or cache layout. Review local edits and use the owner's cleanup operation. Downloads or rebuilding may be needed again."
            }
        }
    }

    pub const fn reclaim(self) -> Option<Reclaim> {
        if matches!(self.disposition, Disposition::Detected) {
            return None;
        }
        Some(match self.kind {
            Artifact::NodeModules => Reclaim::Reinstallable,
            Artifact::Python
            | Artifact::GoModule
            | Artifact::Jvm
            | Artifact::Dotnet => Reclaim::Regenerable,
            Artifact::RustTarget
            | Artifact::JsBuild
            | Artifact::CompilerCache
            | Artifact::CmakeBuild => Reclaim::BuildOutput,
            _ => Reclaim::PackageStore,
        })
    }

    pub const fn category(self) -> Category {
        if self.kind.is_vm() {
            Category::VirtualMachine
        } else {
            match self.kind {
                Artifact::GitRepo => Category::Git,
                Artifact::Worktree => Category::AgentScratch,
                _ => Category::Cache,
            }
        }
    }
}

/// Inspect a path using the scanned machine's path rules, without I/O.
pub fn detect(
    path: &Path,
    style: PathStyle,
    is_dir: bool,
) -> Option<Detection> {
    let names = style.names(path);
    let parts: Vec<&str> = names.iter().map(AsRef::as_ref).collect();
    detect_parts(&parts, is_dir)
}

/// Assess a displayed node. Ownership in the scanned tree wins when the
/// display path is an alias that does not name the owning VM or Git store.
pub fn assess(
    path: &Path,
    style: PathStyle,
    node: Node<'_>,
) -> Option<Detection> {
    let detection = detect(path, style, node.is_dir());
    let detection = match node.category {
        Category::VirtualMachine => {
            Some(detection.filter(|d| d.kind.is_vm()).unwrap_or_else(|| {
                Detection::new(Artifact::VirtualMachine, Evidence::Layout)
            }))
        }
        Category::Git => Some(
            detection
                .filter(|d| d.kind == Artifact::GitRepo)
                .unwrap_or_else(|| {
                    Detection::new(Artifact::GitRepo, Evidence::Name)
                }),
        ),
        _ => detection,
    };
    detection.map(|mut d| {
        if d.kind == Artifact::PackageCache
            && crate::classify::is_pnpm_store(node)
        {
            d.evidence = Evidence::Layout;
        }
        d.assessed(node)
    })
}

fn detect_parts(parts: &[&str], is_dir: bool) -> Option<Detection> {
    if let Some(detection) = vm(parts, is_dir) {
        return Some(detection);
    }
    // A path ending in a regular file named `node_modules` is not an
    // installed dependency tree. All developer roots are directories.
    let directories = if is_dir {
        parts
    } else {
        parts.get(..parts.len().saturating_sub(1))?
    };
    if directories
        .iter()
        .any(|name| name.eq_ignore_ascii_case(".git"))
    {
        return Some(Detection::new(Artifact::GitRepo, Evidence::Name));
    }
    if let Some(end) = pnpm_store_end(directories) {
        return Some(Detection::new(
            Artifact::PackageCache,
            evidence(&directories[..end]),
        ));
    }
    let (end, kind) = developer(directories)?;
    Some(Detection::new(kind, evidence(&directories[..end])))
}

/// The owning store, rather than any cache that happens to contain it.
fn pnpm_store_end(parts: &[&str]) -> Option<usize> {
    parts.iter().enumerate().find_map(|(i, name)| {
        step();
        if name.eq_ignore_ascii_case(".pnpm-store") {
            Some(i + 1)
        } else if name.eq_ignore_ascii_case("pnpm")
            && parts
                .get(i + 1)
                .is_some_and(|s| s.eq_ignore_ascii_case("store"))
        {
            Some(i + 2)
        } else {
            None
        }
    })
}

fn developer(parts: &[&str]) -> Option<(usize, Artifact)> {
    let mut found = rules::classify_parts(parts, 0)?;
    // A worktree is inventory, but its installed dependencies can still be
    // reviewed separately without suggesting the checkout itself.
    while found.1 == Artifact::Worktree && found.0 < parts.len() {
        let Some(inner) = rules::classify_parts(parts, found.0) else {
            break;
        };
        found = inner;
    }
    Some(found)
}

fn excluded_tool_contents(parts: &[&str]) -> bool {
    parts.iter().enumerate().any(|(i, part)| {
        step();
        let Some(next) = parts.get(i + 1) else {
            return false;
        };
        let next = next.to_ascii_lowercase();
        let after = parts.get(i + 2);
        match part.to_ascii_lowercase().as_str() {
            ".yarn" => match next.as_str() {
                "cache" | "unplugged" => false,
                "berry" => {
                    after.is_some_and(|s| !s.eq_ignore_ascii_case("cache"))
                }
                _ => true,
            },
            ".bun" => {
                next != "install"
                    || after.is_some_and(|s| !s.eq_ignore_ascii_case("cache"))
            }
            ".gradle" => {
                next != "caches"
                    && !rules::numeric_version(&next, 2, 3)
                    && (next != "wrapper"
                        || after
                            .is_some_and(|s| !s.eq_ignore_ascii_case("dists")))
            }
            ".m2" => next != "repository",
            ".nuget" => next != "packages",
            ".vercel" => !matches!(next.as_str(), "cache" | "output"),
            ".netlify" => next != "cache",
            ".output" => !matches!(next.as_str(), "public" | "server"),
            _ => false,
        }
    })
}

fn evidence(parts: &[&str]) -> Evidence {
    let last = parts.last().copied().unwrap_or_default();
    if matches!(
        last.to_ascii_lowercase().as_str(),
        "node_modules"
            | ".pnpm-store"
            | ".next"
            | ".nuxt"
            | ".turbo"
            | ".parcel-cache"
            | ".svelte-kit"
            | "__pycache__"
            | ".mypy_cache"
            | ".pytest_cache"
            | ".ruff_cache"
            | "cmakefiles"
            | ".zig-cache"
            | ".git"
            | "diagoutputdir"
            | "rdclientautotrace"
    ) {
        Evidence::Name
    } else {
        Evidence::Layout
    }
}

fn vm(parts: &[&str], is_dir: bool) -> Option<Detection> {
    let name = parts.last()?.to_ascii_lowercase();
    let extension = name.rsplit_once('.').map_or("", |(_, ext)| ext);
    let kind =
        match extension {
            "avhd" | "avhdx" | "vmsn" => Some(Artifact::VmSnapshot),
            "vmss" | "vmem" | "vmrs" | "sav" => Some(Artifact::VmSuspend),
            "vdi" | "vmdk" | "vhd" | "vhdx" | "qcow" | "qcow2" | "qed"
            | "hds" => Some(Artifact::VmDisk),
            "vbox" | "vbox-prev" | "vmx" | "vmxf" | "vmsd" | "vmcx"
            | "vmgs" | "hdd" | "pvm" | "vmwarevm" | "utm" | "ova" | "ovf"
            | "xva" => Some(Artifact::VirtualMachine),
            _ => None,
        };
    let owned = parts.iter().enumerate().any(|(i, part)| {
        step();
        let at = |offset: usize, wanted: &str| {
            parts
                .get(i + offset)
                .is_some_and(|s| s.eq_ignore_ascii_case(wanted))
        };
        let part = part.to_ascii_lowercase();
        part.rsplit_once('.').is_some_and(|(_, extension)| {
            matches!(extension, "vmwarevm" | "pvm" | "utm" | "hdd")
        }) || (part == ".tart" && at(1, "vms") && i + 2 < parts.len())
            || (part == "virtualbox vms" && i + 1 < parts.len())
            || (part == ".minikube" && at(1, "machines") && i + 2 < parts.len())
            || (part == "com.docker.docker"
                && at(1, "data")
                && at(2, "vms")
                && i + 3 < parts.len())
            || (!is_dir
                && matches!(
                    name.as_str(),
                    "diffdisk" | "basedisk" | "cidata.iso"
                )
                && ((part == ".lima"
                    && i + 3 == parts.len()
                    && !parts[i + 1].starts_with('_'))
                    || (part == ".colima"
                        && at(1, "_lima")
                        && i + 4 == parts.len())))
    });
    // .sav is also used for game saves. Only VM ownership establishes it.
    let kind = if extension == "sav" && !owned {
        None
    } else if (extension == "vmdk"
        && name.strip_suffix(".vmdk").is_some_and(|stem| {
            let stem = stem
                .strip_suffix("-delta")
                .or_else(|| stem.strip_suffix("-sesparse"))
                .unwrap_or(stem);
            stem.rsplit_once('-').is_some_and(|(_, number)| {
                number.len() == 6 && number.bytes().all(|b| b.is_ascii_digit())
            })
        }))
        || (owned && is_dir && name == "snapshots")
    {
        Some(Artifact::VmSnapshot)
    } else {
        kind
    };
    if owned || (!is_dir && kind.is_some()) {
        Some(Detection::new(
            kind.unwrap_or(Artifact::VirtualMachine),
            if owned {
                Evidence::Layout
            } else {
                Evidence::Name
            },
        ))
    } else {
        None
    }
}

fn possible_vm_file(name: &str) -> bool {
    // Most files need no path processing. Keep the common case allocation
    // free; the longest recognized extension is nine ASCII bytes.
    let extension = name.rsplit_once('.').map_or("", |(_, ext)| ext);
    let mut buffer = [0_u8; 10];
    let Some(lower) = buffer.get_mut(..extension.len()) else {
        return false;
    };
    lower.copy_from_slice(extension.as_bytes());
    lower.make_ascii_lowercase();
    matches!(
        &*lower,
        b"vdi"
            | b"vmdk"
            | b"vhd"
            | b"vhdx"
            | b"qcow"
            | b"qcow2"
            | b"qed"
            | b"avhd"
            | b"avhdx"
            | b"vmsn"
            | b"vmss"
            | b"vmem"
            | b"vmrs"
            | b"sav"
            | b"vbox"
            | b"vbox-prev"
            | b"vmx"
            | b"vmxf"
            | b"vmsd"
            | b"vmcx"
            | b"vmgs"
            | b"hds"
            | b"hdd"
            | b"pvm"
            | b"vmwarevm"
            | b"utm"
            | b"ova"
            | b"ovf"
            | b"xva"
            | b"iso"
    ) || name.eq_ignore_ascii_case("diffdisk")
        || name.eq_ignore_ascii_case("basedisk")
}

/// Temporary overrides by node id. No field is added to every stored node.
#[derive(Debug, Default)]
pub(crate) struct Roots(FxHashMap<u32, Detection>);

impl Roots {
    pub fn find(tree: &Tree, path: &Path, style: PathStyle) -> Self {
        let names = style.names(path);
        let mut parts: Vec<&str> = names.iter().map(AsRef::as_ref).collect();
        let mut roots = Self::default();
        if let Some(mut detection) = detect_parts(&parts, tree.root().is_dir())
        {
            // A narrow scan cannot verify a store above its root. Seeing a
            // versioned layout inside this subtree does not verify that owner.
            if detection.kind == Artifact::PackageCache
                && pnpm_store_end(&parts).is_some_and(|end| end < parts.len())
            {
                detection.disposition = Disposition::Detected;
            }
            roots.insert(tree.root(), detection);
            if detection.kind.is_vm() || detection.kind == Artifact::GitRepo {
                return roots;
            }
        }
        let excluded = excluded_tool_contents(&parts);
        // The root was judged with its full path. A bounded lookback here
        // could lose its enclosing ownership and overwrite that assessment.
        for child in tree.root().children() {
            parts.push(child.name());
            Self::visit(child, &mut parts, excluded, &mut roots);
            parts.pop();
        }
        roots
    }

    fn visit<'a>(
        node: Node<'a>,
        parts: &mut Vec<&'a str>,
        excluded: bool,
        roots: &mut Self,
    ) {
        step();
        // VM ownership is propagated once; the rule window is bounded even
        // in deeply nested dependency trees or repeated ambiguous segments.
        let tail = &parts[parts.len().saturating_sub(6)..];
        // Preserve tool-home exclusions after their names leave the bounded
        // window; installed plugins/settings are not dependency caches.
        let excluded =
            excluded || (node.is_dir() && excluded_tool_contents(tail));
        let machine = if node.is_dir() || possible_vm_file(node.name()) {
            vm(tail, node.is_dir())
        } else {
            None
        };
        let detection = machine.or_else(|| {
            if node.is_dir() && node.name().eq_ignore_ascii_case(".git") {
                return Some(Detection::new(Artifact::GitRepo, Evidence::Name));
            }
            if !node.is_dir() || excluded {
                return None;
            }
            if pnpm_store_end(tail) == Some(tail.len()) {
                return Some(Detection::new(
                    Artifact::PackageCache,
                    evidence(tail),
                ));
            }
            let (end, kind) = developer(tail)?;
            (end == tail.len()).then(|| Detection::new(kind, evidence(tail)))
        });
        if let Some(detection) = detection {
            roots.insert(node, detection);
        }
        // A VM subtree is protected by its inherited category. Its files
        // need no additional overrides or path checks.
        if machine.is_some()
            || detection.is_some_and(|d| d.kind == Artifact::GitRepo)
        {
            return;
        }
        for child in node.children() {
            parts.push(child.name());
            Self::visit(child, parts, excluded, roots);
            parts.pop();
        }
    }

    pub fn get(&self, id: u32) -> Option<Detection> {
        self.0.get(&id).copied()
    }

    fn insert(&mut self, node: Node<'_>, mut detection: Detection) {
        // PwrDisk can inspect the scanned entries, so retain its stronger
        // store check rather than promoting a name-only DiskHound match.
        if detection.kind == Artifact::PackageCache
            && (node.name().eq_ignore_ascii_case("store")
                || node.name().eq_ignore_ascii_case(".pnpm-store"))
            && !crate::classify::is_pnpm_store(node)
        {
            detection.disposition = Disposition::Detected;
        }
        self.0.insert(node.id(), detection);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::classify_at;
    use crate::tree::{Draft, NodeKind};

    #[derive(Deserialize)]
    struct Fixture {
        path: String,
        root: Option<String>,
        kind: Option<Artifact>,
    }

    #[test]
    fn diskhound_corpus_matches_across_case_and_separator_variants() {
        let fixtures: Vec<Fixture> =
            serde_json::from_str(include_str!("artifact/classification.json"))
                .unwrap();
        assert!(fixtures.len() >= 350);
        for fixture in fixtures {
            for path in [
                fixture.path.clone(),
                fixture.path.to_ascii_uppercase(),
                fixture.path.replace('/', "\\"),
            ] {
                let parts: Vec<&str> =
                    path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
                let actual = rules::classify_parts(&parts, 0);
                assert_eq!(
                    actual.map(|(_, kind)| kind),
                    fixture.kind,
                    "{path}"
                );
                if let Some((end, _)) = actual {
                    let expected: Vec<_> = fixture
                        .root
                        .as_ref()
                        .unwrap()
                        .split(['/', '\\'])
                        .filter(|s| !s.is_empty())
                        .map(str::to_ascii_lowercase)
                        .collect();
                    let root: Vec<_> = parts[..end]
                        .iter()
                        .map(|s| s.to_ascii_lowercase())
                        .collect();
                    assert_eq!(root, expected, "{path}");
                }
            }
        }
    }

    fn detection(path: &str, is_dir: bool) -> Option<Detection> {
        detect(Path::new(path), PathStyle::Posix, is_dir)
    }

    #[test]
    fn identity_evidence_and_cleanup_are_independent() {
        let dependencies = detection("/project/node_modules", true).unwrap();
        assert_eq!(dependencies.evidence, Evidence::Name);
        assert_eq!(dependencies.disposition, Disposition::Suggested);
        let rust = detection("/project/target/debug/deps", true).unwrap();
        assert_eq!(rust.evidence, Evidence::Layout);
        assert_eq!(rust.disposition, Disposition::Suggested);
        let git = detection("/project/.git/objects/pack", true).unwrap();
        assert_eq!(git.disposition, Disposition::Detected);
        assert_eq!(git.reclaim(), None);
        for path in [
            "/project/target",
            "/project/build",
            "/project/dist",
            "/project/node_modules",
        ] {
            assert_eq!(
                detection(path, false),
                None,
                "a name is not a directory: {path}"
            );
        }
        assert_eq!(detection("/project/target", true), None);
        assert_eq!(detection("/project/venv/bin", true), None);
        assert_eq!(detection("/project/.yarn/patches", true), None);
        assert_eq!(detection("/project/vendor/cache", true), None);
    }

    #[test]
    fn vm_disks_checkpoints_and_memory_are_detected_only() {
        for (path, kind) in [
            ("/VM/guest.qcow2", Artifact::VmDisk),
            ("/VM/guest.vhdx", Artifact::VmDisk),
            ("/VM/guest.avhdx", Artifact::VmSnapshot),
            ("/VM/guest-000001.vmdk", Artifact::VmSnapshot),
            ("/VM/guest-000001-delta.vmdk", Artifact::VmSnapshot),
            ("/VM/guest-s001.vmdk", Artifact::VmDisk),
            ("/VM/guest.vmsn", Artifact::VmSnapshot),
            ("/VM/guest.vmss", Artifact::VmSuspend),
            ("/VM/guest.vmem", Artifact::VmSuspend),
            ("/VM/guest.vmrs", Artifact::VmSuspend),
            ("/VM/guest.vmwarevm/cache/blob", Artifact::VirtualMachine),
            (
                "/home/test/.tart/vms/node_modules",
                Artifact::VirtualMachine,
            ),
            (
                "/home/test/VirtualBox VMs/node_modules",
                Artifact::VirtualMachine,
            ),
            (
                "/home/test/.lima/default/diffdisk",
                Artifact::VirtualMachine,
            ),
            (
                "/home/test/.colima/_lima/default/basedisk",
                Artifact::VirtualMachine,
            ),
        ] {
            let d = detection(path, false).unwrap();
            assert_eq!(d.kind, kind, "{path}");
            assert_eq!(d.disposition, Disposition::Detected);
            assert_eq!(d.reclaim(), None);
        }
        for path in [
            "/photos/photo.raw",
            "/images/disk.img",
            "/games/slot.sav",
            "/source/vms/image.img",
            "/VM/guest.vmwarevm-backup/disk.img",
            "/source/folder\\guest.vmdk/notes",
        ] {
            assert_eq!(detection(path, false), None, "{path}");
        }
        let memory = detection("/VM/guest.vmss", false).unwrap();
        assert!(memory.guidance().contains("shut it down normally"));
        assert!(
            detection("/VM/guest.avhdx", false)
                .unwrap()
                .guidance()
                .contains("snapshot chains")
        );
    }

    fn dir(name: &str, children: Vec<Draft>) -> Draft {
        let mut draft = Draft::directory(name);
        draft.children = children;
        draft
    }

    fn file(name: &str) -> Draft {
        Draft::entry(name, NodeKind::File, 100)
    }

    #[test]
    fn mixed_project_output_does_not_promote_parents_or_settings() {
        let mut tree = Tree::from(dir(
            "project",
            vec![
                file("Cargo.toml"),
                dir(
                    "target",
                    vec![
                        file("notes.txt"),
                        dir(
                            "debug",
                            vec![
                                file("my-program"),
                                dir("deps", vec![file("lib.rlib")]),
                            ],
                        ),
                        dir("scala-3.3", vec![file("App.class")]),
                    ],
                ),
                dir(
                    ".yarn",
                    vec![
                        dir("patches", vec![file("local.patch")]),
                        dir("cache", vec![file("package.zip")]),
                    ],
                ),
                dir(
                    ".terraform",
                    vec![
                        file("terraform.tfstate"),
                        dir("providers", vec![file("provider")]),
                    ],
                ),
            ],
        ));
        classify_at(&mut tree, Path::new("/project"), PathStyle::Posix);
        let root = tree.root();
        let target = root.child_named("target").unwrap();
        assert_eq!(target.reclaim, None);
        assert_eq!(target.child_named("notes.txt").unwrap().reclaim, None);
        let debug = target.child_named("debug").unwrap();
        assert_eq!(debug.reclaim, None);
        assert_eq!(debug.child_named("my-program").unwrap().reclaim, None);
        assert_eq!(
            debug.child_named("deps").unwrap().reclaim,
            Some(Reclaim::BuildOutput)
        );
        assert!(target.child_named("scala-3.3").unwrap().reclaim.is_some());
        let yarn = root.child_named(".yarn").unwrap();
        assert_eq!(yarn.reclaim, None);
        assert_eq!(yarn.child_named("patches").unwrap().reclaim, None);
        assert!(yarn.child_named("cache").unwrap().reclaim.is_some());
        let terraform = root.child_named(".terraform").unwrap();
        assert_eq!(
            terraform.child_named("terraform.tfstate").unwrap().reclaim,
            None
        );
        assert!(
            terraform
                .child_named("providers")
                .unwrap()
                .reclaim
                .is_some()
        );
    }

    #[test]
    fn vm_ownership_protects_nested_cache_names_and_enclosing_candidates() {
        let vm = dir(
            "guest.utm",
            vec![
                dir("node_modules", vec![file("local-work")]),
                dir("cache", vec![file("state")]),
            ],
        );
        let mut tree = Tree::from(dir(
            "home",
            vec![dir(".cache", vec![vm, file("other-cache")])],
        ));
        classify_at(&mut tree, Path::new("/home"), PathStyle::Posix);
        let cache = tree.root().child_named(".cache").unwrap();
        assert_eq!(
            cache.reclaim, None,
            "a parent cannot swallow protected data"
        );
        let vm = cache.child_named("guest.utm").unwrap();
        assert_eq!(vm.category, Category::VirtualMachine);
        assert_eq!(vm.reclaim, None);
        assert_eq!(vm.child_named("node_modules").unwrap().reclaim, None);
        assert_eq!(vm.child_named("cache").unwrap().reclaim, None);
        let mut narrow = Tree::from(vm.to_draft());
        classify_at(
            &mut narrow,
            Path::new("/home/guest.utm/deep/inside"),
            PathStyle::Posix,
        );
        assert_eq!(narrow.root().category, Category::VirtualMachine);
        assert_eq!(narrow.root().child_named("cache").unwrap().reclaim, None);
    }

    #[test]
    fn git_metadata_cannot_inherit_a_cleanup_suggestion_even_without_subdirectories()
     {
        let mut tree = Tree::from(dir(
            "home",
            vec![dir(
                ".cache",
                vec![
                    dir(".git", vec![file("HEAD"), file("index")]),
                    file("cache-data"),
                ],
            )],
        ));
        classify_at(&mut tree, Path::new("/home"), PathStyle::Posix);
        let cache = tree.root().child_named(".cache").unwrap();
        assert_eq!(cache.reclaim, None);
        let git = cache.child_named(".git").unwrap();
        assert_eq!(git.category, Category::Git);
        assert_eq!(git.reclaim, None);
        assert_eq!(git.child_named("HEAD").unwrap().reclaim, None);
        assert!(cache.child_named("cache-data").unwrap().reclaim.is_some());
        let mut narrow = Tree::from(git.to_draft());
        classify_at(&mut narrow, Path::new("/home/.git"), PathStyle::Posix);
        assert_eq!(narrow.root().category, Category::Git);
        assert_eq!(narrow.root().child_named("index").unwrap().reclaim, None);
        let mut bare = Tree::from(dir(
            "cache",
            vec![
                file("HEAD"),
                dir("refs", vec![]),
                dir("objects", vec![file("pack")]),
            ],
        ));
        classify_at(
            &mut bare,
            Path::new("/home/.cache/pip/cache"),
            PathStyle::Posix,
        );
        assert_eq!(bare.root().category, Category::Git);
        assert_eq!(bare.root().child_named("HEAD").unwrap().reclaim, None);
        let mut service =
            Tree::from(dir(".Spotlight-V100", vec![file("index")]));
        classify_at(
            &mut service,
            Path::new("/home/.cache/pip/.Spotlight-V100"),
            PathStyle::Posix,
        );
        assert_eq!(service.root().category, Category::Spotlight);
        assert_eq!(service.root().child_named("index").unwrap().reclaim, None);
    }

    #[test]
    fn narrow_git_scans_keep_enclosing_ownership_over_cache_names() {
        use crate::query::Filter;

        for path in [
            "/repo/node_modules/pkg/.git/objects",
            "/home/.cache/pip/pkg/.git/objects",
            "/repo/node_modules/pkg/.git/a/b/c/d/e/f/objects",
        ] {
            let path = Path::new(path);
            let mut tree = Tree::from(dir(
                "objects",
                vec![file("pack"), dir("node_modules", vec![file("index")])],
            ));
            classify_at(&mut tree, path, PathStyle::Posix);
            let root = tree.root();
            for node in [root, root.child_named("node_modules").unwrap()] {
                assert_eq!(node.category, Category::Git, "{path:?}");
                assert_eq!(node.reclaim, None, "{path:?}");
            }
            let assessment = assess(path, PathStyle::Posix, root).unwrap();
            assert_eq!(assessment.kind, Artifact::GitRepo);
            assert_eq!(assessment.disposition, Disposition::Detected);
            let mut candidates = Vec::new();
            Filter {
                reclaimable: true,
                ..Filter::default()
            }
            .collect(path, root, &mut candidates);
            assert!(candidates.is_empty(), "{path:?}: {candidates:?}");
        }
    }

    #[test]
    fn narrow_pnpm_scans_require_evidence_at_the_owning_store() {
        use crate::query::Filter;

        for owner in [
            "/home/me/.pnpm-store",
            "/home/me/pnpm/store",
            "/repo/node_modules/pkg/.pnpm-store",
        ] {
            let owner = Path::new(owner);
            for verified in [false, true] {
                let mut children = vec![dir("local-work", vec![file("notes")])];
                if verified {
                    children
                        .push(dir("v10", vec![dir("files", vec![file("ab")])]));
                }
                let mut tree = Tree::from(dir(
                    owner.file_name().unwrap().to_str().unwrap(),
                    children,
                ));
                classify_at(&mut tree, owner, PathStyle::Posix);
                let expected = verified.then_some(Reclaim::PackageStore);
                assert_eq!(tree.root().reclaim, expected, "{owner:?}");
                assert_eq!(
                    tree.root().child_named("local-work").unwrap().reclaim,
                    expected,
                    "{owner:?}"
                );
            }
            // Even a descendant named `store` with a versioned layout cannot
            // verify an owning store that lies outside the scanned tree.
            for relative in ["local-work", "a/b/c/d/e/f/local-work", "store"] {
                let path = PathStyle::Posix.join(owner, relative);
                let mut tree = Tree::from(dir(
                    relative.rsplit('/').next().unwrap(),
                    vec![
                        file("notes"),
                        dir("v10", vec![dir("files", vec![file("ab")])]),
                    ],
                ));
                classify_at(&mut tree, &path, PathStyle::Posix);
                let root = tree.root();
                assert_eq!(root.reclaim, None, "{path:?}");
                let assessment = assess(&path, PathStyle::Posix, root).unwrap();
                assert_eq!(assessment.kind, Artifact::PackageCache);
                assert_eq!(assessment.disposition, Disposition::Detected);
                let mut candidates = Vec::new();
                Filter {
                    reclaimable: true,
                    ..Filter::default()
                }
                .collect(&path, root, &mut candidates);
                assert!(candidates.is_empty(), "{path:?}: {candidates:?}");
            }
        }
        // Other recognized package caches do not require pnpm's layout.
        let mut tree = Tree::from(dir("subtree", vec![file("cache-data")]));
        classify_at(
            &mut tree,
            Path::new("/home/me/.cache/electron/subtree"),
            PathStyle::Posix,
        );
        assert_eq!(tree.root().reclaim, Some(Reclaim::PackageStore));
    }

    #[test]
    fn tool_home_exclusions_survive_deep_paths_and_worktree_caches_stay_separate()
     {
        let output = || {
            dir(
                "target",
                vec![dir("debug", vec![dir("deps", vec![file("lib.rlib")])])],
            )
        };
        let mut plugin = output();
        for name in ["f", "e", "d", "c", "b", "a"] {
            plugin = dir(name, vec![plugin]);
        }
        let mut tree = Tree::from(dir(
            "project",
            vec![
                dir(".yarn", vec![dir("plugins", vec![plugin])]),
                dir(".worktrees", vec![dir("feature", vec![output()])]),
            ],
        ));
        classify_at(&mut tree, Path::new("/project"), PathStyle::Posix);
        let root = tree.root();
        let (_, plugin) = root
            .descend(Path::new(".yarn/plugins/a/b/c/d/e/f/target/debug/deps"))
            .unwrap();
        assert_eq!(plugin.reclaim, None);
        let (_, cache) = root
            .descend(Path::new(".worktrees/feature/target/debug/deps"))
            .unwrap();
        assert_eq!(cache.reclaim, Some(Reclaim::BuildOutput));
        let detection = assess(
            Path::new("/project/.worktrees/feature/target/debug/deps"),
            PathStyle::Posix,
            cache,
        )
        .unwrap();
        assert_eq!(detection.kind, Artifact::RustTarget);
        assert_eq!(detection.disposition, Disposition::Suggested);
        assert_eq!(root.child_named(".worktrees").unwrap().reclaim, None);
    }

    #[test]
    fn finding_roots_has_bounded_work_per_node() {
        let run = |count| {
            let tree = Tree::from(dir(
                "projects",
                (0..count)
                    .map(|i| {
                        dir(
                            &format!("p{i}"),
                            vec![dir(
                                "target",
                                vec![dir(
                                    "debug",
                                    vec![dir("deps", vec![file("lib.rlib")])],
                                )],
                            )],
                        )
                    })
                    .collect(),
            ));
            WORK.set(0);
            let roots =
                Roots::find(&tree, Path::new("/projects"), PathStyle::Posix);
            assert_eq!(roots.0.len(), count);
            WORK.get()
        };
        let small = run(100);
        let large = run(800);
        assert!(large <= small * 16, "{small} -> {large}");
        assert!(large <= 800 * 100, "{large}");
    }
}
