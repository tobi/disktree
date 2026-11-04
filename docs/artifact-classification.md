# Detected artifacts and cleanup suggestions

Artifact identity, identification evidence and cleanup suitability are
separate. A detection never authorizes marking or removal. A suggestion
means a plausible, relatively lower-risk candidate for human review; it
does not mean safe to delete. Size and age do not establish disposability.

The selection panel and MCP listings report the artifact, its evidence
(`name convention` or `tool layout`), and one of these assessments:

| Assessment | Examples | Next step |
| --- | --- | --- |
| Detected | VM disks, Git history, worktrees, diagnostic logs | Inspect ownership and whether the data is still needed. It may contain unique work. |
| Detected | VM snapshots / checkpoints | Review and consolidate through the VM app. Never remove individual files from a snapshot chain. |
| Detected | Saved VM memory / suspend state | Resume the VM and shut it down normally. Let the VM app release its saved state; memory can also belong to checkpoints. |
| Suggested for review | Recognized dependency caches, installed packages, compiler intermediates | Check local edits, stop builds and running environments, and prefer the owner's cleanup operation. Rebuilding, reinstalling or downloading may be needed. |

The **Virtual machines** legend chip filters this inventory. Agents can
use `pwrdisk_find` with `category: "virtual-machines"`; `reclaimable: true`
keeps cleanup candidates and excludes detected VM data.

A distinctive name is weaker identification evidence than a documented
multi-component layout. Neither proves that files have no local edits or
that the owner is idle. pnpm stores retain an additional check against the
scanned version-directory structure. Scans starting below the store remain
detection-only because they cannot verify the owning store's layout.
Incomplete scans may also omit that evidence.

VM ownership wins over cache names. Bundles and known Tart, VirtualBox,
Minikube and Docker VM directories cannot become suggestions because a
child happens to be named `cache`, `tmp` or `node_modules`. Lima and Colima
extensionless disk files are recognized without labeling their whole tool
homes as VMs. A candidate containing protected data loses its whole-parent
suggestion. A displayed size is not a promise of recovered bytes: VM disks,
snapshot chains, APFS clones and open files can retain or share storage.

## Imported developer layouts

The structural rules and 350-case positive/negative corpus were ported from
[DiskHound](https://github.com/huntharo/diskhound), revision
`0b0209b469ab5afd8d562f57460e668cae94a659`. Its MIT notice is included in
`licenses/bundled.txt` and the generated third-party notices.

Coverage includes JS/TypeScript dependencies and frontend output; Rust,
Python, Gradle/Maven/Scala, Go, .NET, C/C++ and CMake; Ruby/Bundler,
Composer, Dart/Flutter, Swift/Xcode, Elixir/Erlang, Haskell and Zig caches;
Terraform provider downloads; and compiler and package-manager caches.
Git metadata, worktree conventions and diagnostic logs are detections.
Git ownership still applies when a scan starts inside `.git`, including
repositories stored within dependencies or caches.

The boundary is the recognized subtree, never a parent inferred from it.
For example, `target/debug/deps` is Rust output and `target/scala-3.3` is
JVM output. Neither labels all of `target`, even beside `Cargo.toml`.
Likewise `.yarn/cache` excludes patches, `.terraform/providers` excludes
state and backend configuration, and Python `site-packages` excludes
environment executables and activation scripts. Generic `build`, `dist`,
`out`, `bin`, `obj`, whole tool homes and unknown custom layouts are not
added by these rules. Existing general cache/temporary-name heuristics
remain review candidates, with the same VM and Git protections.

Classification reads only the completed scan, with no new filesystem
probes. Detailed metadata is derived from paths on demand. The tree keeps
its existing node size, and a bounded six-component rule window and
operation-count tests guard classification cost. SSH paths use POSIX rules
on every client. Existing marking, confirmation, capacity and removal
guards apply to every assessment.
