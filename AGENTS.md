# PwrDisk — agent guide

PwrDisk is a maintained fork of [disktree](https://github.com/pwrdrvr/PwrDisk).
The user-facing name is PwrDisk (binary `pwrdisk`, bundle ID
`com.pwrdrvr.pwrdisk`); crate, module and type names and the SSH helper
(`disktree-scan`) keep the upstream names so upstream merges stay clean.

A GPUI + gpui-omarchy treemap explorer for disk usage on Omarchy. Read
`README.md` for the product; this file is the working contract.

Opening a PR? Read [Pull requests](#pull-requests) first: screenshots are
attached with `gh --attach`, never hosted on a branch.

## What this is

Find what is eating a volume, mark paths for removal, review the list, and
remove it — with the volume's free space live on screen the whole time. Two
phases after choosing a local/SSH source and root: explore (treemap,
breadcrumbs, selection and status) and review (the marked list,
the removal mode, the confirmation). Marking is never destructive.

`pwrdisk mcp` is the same thing for an AI agent: an MCP server over stdio
that scans with `disktree-core`, marks, and opens the review screen in a
window over a per-user socket or pipe (`disktree-link`). Removal uses either
human review or an explicit destructive MCP write after a single-use preview
and user confirmation of the exact paths and mode. Computer use must never
approve removal. Both routes enforce the configured per-operation capacity
limit (5% per volume by default), including a fresh measurement on the worker.

## Commands

```sh
make build                      # release build
make run                        # opens a window: see Testing on a desktop
make install                    # ~/.local: binary, desktop entry, icon
make install PREFIX=/usr/local  # system-wide (needs root)
make uninstall
make bundle                     # macOS: target/bundle/PwrDisk.app, zip and DMG
make dock-icons                 # macOS 26: redraw the Dock icon of an unbundled run
make lint                       # rustfmt --check, then clippy --all-targets -D warnings
make test                       # core and window-harness tests
make ci                         # lint, then test
make fmt                        # format in place
cargo xtask licenses            # rewrite THIRD_PARTY_LICENSES after a dependency change
```

`make install` is the supported way to put this on a machine: it installs the
release binary, `packaging/pwrdisk.desktop.in` (rendered with the real install
prefix and the crate version) and `assets/disktree.svg`. Keep the desktop
entry's `Categories` to a single main category plus additional ones, or
`desktop-file-validate` complains.

On macOS `make install` installs `target/bundle/PwrDisk.app` into
`~/Applications` instead, built by `cargo xtask bundle` from
`packaging/macos/` (the `Info.plist` template and the icon's SVG). The bundle
identifier there keys every macOS permission grant; do not change it lightly.

The macOS icon has two sources in `packaging/macos/`, and each macOS reads
the one made for it. `AppIcon.svg` is the mark on Apple's legacy 824-in-1024
tile; the bundle draws `AppIcon.icns` from it, which macOS 15 and earlier
draw literally. `AppIcon.icon` with `AppGlyph.svg` is the Icon Composer
package: when Xcode 26's actool is at hand `cargo xtask bundle` compiles it
into `Assets.car` and names it with `CFBundleIconName`, and macOS 26 draws
that with its own shape, glass edge and Dark, Clear and Tinted looks. Handed
only the `.icns`, macOS 26 normalizes it instead, and 26.6.2 has drawn such a
padded icns on a light plate, so a bundle without the catalog says so. A run
outside the bundle (`make run`, `cargo run`) has no icon for macOS to draw,
so `dock_icon.rs` paints one: `DockIcon.png`, or on macOS 26
`DockIcon-glass.png`, the shipped icon as that macOS itself draws it, since
Liquid Glass is never applied to an image set at run time. `make dock-icons`
redraws both on a macOS 26 with Xcode 26; run it after either SVG changes.

Releases are cut by `release.yml` from a `vX.Y.Z` tag that matches the
workspace version and a `## vX.Y.Z` section of `CHANGELOG.md` (`cargo xtask
release-check`); `docs/releasing.md` is the procedure and
`.github/workflows/README.md` the design. macOS ships a DMG and Windows a
Velopack Setup.exe, MSI and portable zip, both of which update themselves
from GitHub Releases. The `velopack` crate pin and `VPK_VERSION` in
`scripts/release/install-vpk.sh` move together.

`cargo xtask lint` is the gate. It must be green before anything is called
done, and it must not fix anything: a red local run is the same signal CI gives.
On Windows, where there is no make, run `cargo xtask lint`, `cargo xtask test`
and `cargo build --release` directly; CI runs the gate on both systems.

When Sandboxer MCP tools are available, run Linux builds, lint, tests, SSH
integration and other Linux-only checks there. Start from
`pwrdisk-base-us-east` in `us-east`; read
[remote verification](docs/sandboxer.md) first. Use local checks when
Sandboxer is unavailable. Sandboxer is Linux only. macOS and Windows changes
are verified on those systems with the headless gate (`cargo xtask lint`,
`cargo xtask test`) and CI; anything that needs a window follows
[Testing on a desktop](#testing-on-a-desktop).

## Testing on a desktop

Never launch PwrDisk, or send synthetic input, on the operator's desktop.
Keys and clicks from `osascript` or System Events, `cliclick`, AX actions,
computer use, and moving or resizing windows land wherever the operator is
working; they have landed in another app. That holds for screenshots,
native accessibility checks and every other GUI check, and for `make run`
or `cargo run` to look at a change. The window-harness tests (`make test`,
`cargo xtask test`) are fine: they draw headless.

* **An on-screen macOS window** (screenshots, VoiceOver, native AX checks) runs
  in the PwrSuiteLab headed E2E Tart VM through the PwrSuiteLab Control
  MCP. Read [docs/headed-e2e-pwrsuitelab.md](docs/headed-e2e-pwrsuitelab.md)
  before the first job. Without that MCP, stop and ask the operator: there
  is no local fallback and no script fallback.
* **Any other window**, such as a Linux or Windows GUI the lab cannot show:
  ask the operator. Never the local desktop.
* **Files, not the clipboard.** Lab VMs start with clipboard sharing off.
  Inputs travel in the committed source a job stages and outputs come back
  as collected artifacts; never ask the operator to paste into or copy
  from the guest.
* **Keep the lab out of this repository.** PwrDisk is public: no lab
  detail goes in code, docs, commits or PRs. The last section of
  [docs/headed-e2e-pwrsuitelab.md](docs/headed-e2e-pwrsuitelab.md) lists
  what counts. Targets come from `lab_status` at run time.

## House rules

* **Strict lints from omatrack's style.** `clippy::all` and `clippy::pedantic`
  are errors, `clippy::nursery` warns, and `-D warnings` promotes the rest. Every
  deliberate exception is listed with its reason in the workspace `Cargo.toml`
  or as an `#[allow(..., reason = "...")]` on the item — never silently.
* **80 columns**, 4-space indent, by `rustfmt.toml`. Only stable rustfmt
  options, because the gate runs on stable; unstable options would be ignored
  with a warning instead of applying.
* **Comments say why.** The code says what. Any non-obvious number, ordering or
  boundary deserves the reason next to it.
* **Tests live beside the promise they make.** `disktree-core` tests size
  accounting, layout and deletion guards against real temporary trees;
  `disktree-app/src/tests.rs` drives the real window harness — draw a frame,
  press keys — so a screen that panics while painting fails a test.
* **Never delete anything outside a marked path.** See `removal.rs`; the guards
  are load-bearing and are covered by tests.

## Invariants

1. **Sizes come from `st_blocks * 512` unless apparent size was asked for.**
   APFS clones can share those blocks; private-byte estimates are reported
   separately without changing tile weights. On macOS the directory listing
   reports every fork's allocation and the data fork's length in bulk; see
   `macos.rs`. On Windows it is the allocation the directory listing reports;
   see `windows.rs`. An
   elevated scan of a whole NTFS drive reads it from the master file table
   instead, keeping the walk's rules for hidden entries, links, cloud
   folders and depth; see `mft.rs`. That path counts every stream's
   allocation, alternate data streams included since they go when the file
   goes, so for a file with alternate streams it can exceed the walk's
   number.
2. **Totals are derived, never tracked.** A leaf's `bytes` is the only
   input: `tree::aggregate` computes every directory's `bytes`, `files` and
   `dirs` from its children, and `own_bytes()`/`own_files()` read the direct
   children when asked. Hardlink de-duplication zeroes a duplicate leaf's
   `bytes` while that pass runs; anything that patches a directory's `bytes`
   directly will be overwritten. `Node` is paid for tens of millions of
   times, so `a_node_stays_small` in `index.rs` holds its size.
3. **A directory is only built when its own scan *and* every subdirectory task
   has finished.** That is the `+1` sentinel in `PendingDir::pending`. Building
   early silently drops whole subtrees — it has happened once.
4. **Only paths under the scanned root may be removed**, and mount points, the
   root, the home directory, any directory holding it, and symlink targets are
   refused. So are the profiles folder and every directory directly in it:
   on Windows the one Windows reports (`FOLDERID_UserProfiles`, usually
   `C:\Users`), on macOS `/Users`. What is inside a profile stays
   removable. The window asks the guards before a mark is made
   (`removal::refusal`), so a refused path is never offered for marking.
5. **Marks are keyed by absolute path**, not tree position, so they survive a
   re-scan; `Marks::refresh` re-reads their sizes and drops what is gone.
6. **The treemap is painted, not composed of elements.** Thousands of
   rectangles belong in one canvas callback; labels are shaped there too so they
   clip to their own tile.
7. **Tile crumbs are absolute.** `treemap::layout` takes the drawn node's
   crumbs and every tile extends them, so a tile resolves from the scanned
   root at any depth. Relative crumbs look right at `~` and silently point
   at other directories after descending — including for marks. Any code that
   turns a path into crumbs walks from the scanned root, too.
8. **The view transform is the only thing zoom changes.** Layout runs in
   base-space pixels and is cached; `screen = (base - origin) * scale`.
9. **The status bar never claims a saving it cannot measure.** Projections come
   from marked bytes *on the volume the meter measures*, which is the scan's own
   notion of a volume — the mount *source* — so a mark on a disk mounted inside
   the tree, which `-X` and the media exception allow, is reported and never
   added to this volume's free space; see `space::attribution`. The final number
   comes from `statvfs` before and after.
10. **Discovery is not a scan.** No implicit home walk at startup. List
    mounts in the selected machine's namespace without probing them; scans
    stop at nested mounts, including bind mounts and same-device subvolumes.
    Switching machines clears marks and identity. The Unix SSH helper streams
    records and never retains the tree. SSH hosts are Unix, so remote paths
    are POSIX on every client: join, split and compare them through
    `PathStyle`, never `std::path`, which uses `\` on Windows.

11. **A live overview is not an actionable tree.** Keep provisional,
    bounded display summaries separate from `Disktree::tree` and all path,
    mark and removal APIs. Only a complete scan may replace the real tree.
    Counters and sampled paths belong in fixed, monospace progress lanes.

12. **The user confirms removal.** Marking never authorizes deleting.
    `review` only opens the review screen; agents must never click its
    confirmation or press its approval keys through computer use. MCP removal
    is a destructive write: prepare an expiring, single-use preview, show the
    exact paths and mode and explain recoverability, then obtain explicit
    confirmation for that operation. An expired or changed preview requires
    new confirmation. The app validates the intent and rechecks existing
    guards and its configured capacity limit for both UI and MCP removals.
    Past the limit only the person can go: in the window, through Mark
    anyway and then Review's second confirmation (a typed phrase by
    default), with the worker held to the share confirmed. An agent's mark
    past the limit is refused, and agents keep the default limit when the
    window's is Off. `mark_guard.rs` holds the rule.
    Trash never falls back to permanent deletion. No headless or SSH removal.
    A linked window's tree and marks remain the source of truth; `ls` and
    `find` read it without navigating. Only an explicit `show` request moves
    the view, and guidance requires the user to request that behavior.
    `scan` explicitly changes the scan root; linking never does so implicitly.
13. **The link is a same-user socket or pipe, nothing more.** `disktree-link`
    binds `0600` in a per-user directory on Unix. On Windows it uses a
    named pipe keyed by the user's SID, with a protected DACL granting only
    that user access and `PIPE_REJECT_REMOTE_CLIENTS`. Before exchanging
    messages the client verifies the connected pipe's owner is that SID.
    It has no token: the agent already runs as the user and `pwrdisk mcp`
    scans on its own.
    Do not add a port.
    A window showing an SSH host answers only `hello` and `state`: an
    agent's paths are this computer's.
14. **The skills are the agent's safety brief.** `skills/` is compiled
    into `pwrdisk mcp` (`disktree-mcp/src/skills.rs` lists every file;
    a URI is looked up, never turned into a path). They tell the agent
    to agree the list before marking unless the user gave a standing
    instruction, and never to delete by other means. A change to what
    the guards refuse, what removal does, or what a system hides from a
    scan updates the matching `references/<system>.md` in the same PR.

15. **A tree owns a handful of buffers, by index.** Nodes live in one
    vector, each directory's children a contiguous run of it, names in one
    string and clone records in side tables, so dropping a tree frees eight
    buffers instead of tens of millions of allocations. An allocation record
    keeps only what its node does not say (`index/records.rs`): a clone with
    one name reads its size from the node, so a placed leaf's `bytes` and
    `inode` change only by hardlink de-duplication. A node is read
    through a `Node<'_>` cursor from its tree and written through `NodeMut`
    or a whole-tree pass. Build with `Tree::build` (a `Draft` for tests and
    small trees); a subtree moves to another tree only as a copy. A
    finished tree keeps every subtree one contiguous run, which is what
    lets `par_bottom_up` split it across workers without `unsafe`: only
    the order *within* a block may change after it is built.

## Where changes belong

| change | where |
| --- | --- |
| bounded Unix SSH traversal | `crates/disktree-core/src/remote_walk.rs` |
| SSH protocol, discovery and transport | `crates/disktree-core/src/remote.rs` |
| joining, splitting or comparing a scanned path | `crates/disktree-core/src/path_style.rs` |
| measurement, filtering, parallelism | `crates/disktree-core/src/scan.rs` |
| what a node is, or a derived total | `crates/disktree-core/src/tree.rs` |
| how a tree is held in memory, its buffers and parallel passes | `crates/disktree-core/src/index.rs` |
| tile geometry, nesting, the merged tail | `crates/disktree-core/src/treemap.rs` |
| anything that deletes, or refuses to | `crates/disktree-core/src/removal.rs` |
| free space and projections | `crates/disktree-core/src/space.rs` |
| what macOS lists and measures in bulk | `crates/disktree-core/src/macos.rs` — `unsafe` for `getattrlistbulk` |
| what Windows lists, measures and compares differently | `crates/disktree-core/src/windows.rs` — `unsafe` for Win32 |
| reading a whole NTFS drive from its file table | `crates/disktree-core/src/mft.rs` |
| APFS clone hints and private-size system calls | `crates/disktree-core/src/macos.rs` — isolated `unsafe` |
| clone sharing totals and conservative savings | `crates/disktree-core/src/sharing.rs` |
| what git is asked about a checkout, when, and how many at once | `crates/disktree-app/src/git.rs` (the probe and its time limit), `crates/disktree-app/src/git_store.rs` (the queue, the bucket, the cache) |
| a key, a screen transition, a mark | `crates/disktree-app/src/state.rs` |
| what may be marked, Mark anyway, Review's over-limit confirmation | `crates/disktree-app/src/mark_guard.rs`, the settings in `safety.rs` |
| Removing and Done: the run's list, its rows, following and running figures; the row anatomy Review shares | `crates/disktree-app/src/run_view.rs`, the model in `run_list.rs`, Review's `mark_row` in `views.rs` |
| what the explore bar folds into ⋮ at each width, and ⋮'s keys | `crates/disktree-app/src/toolbar.rs` |
| the legend as a filter: its chips and what they keep | `crates/disktree-app/src/legend.rs`, the rule in `crates/disktree-core/src/filter.rs` |
| spacing, type and size | `crates/disktree-app/src/ui.rs` — tokens only, no `px` in layout |
| the mosaic's painting or labels | `crates/disktree-app/src/treemap_view.rs` |
| layout of a screen | `crates/disktree-app/src/views.rs` |
| colours derived from the theme | `crates/disktree-app/src/palette.rs` |
| the title strip, window controls, the client-side frame | `crates/disktree-app/src/chrome.rs` |
| where the main window opens, and remembering it | `crates/disktree-app/src/window_frame.rs` |
| which display a new window opens on | `crates/disktree-app/src/placement.rs` |
| the menu bar, its shortcuts, the strip's menu off macOS | `crates/disktree-app/src/app_menu.rs`, the dropdown in `views.rs` |
| Help's rows, Copy Diagnostics Info | `crates/disktree-app/src/help.rs`, the report in `diagnostics.rs` |
| Settings (Updates, Safety, Agents (MCP), Diagnostics, About) | `crates/disktree-app/src/settings.rs` |
| Changelog, License, Third-Party Notices, Logs windows | `doc_window.rs`, `logs_window.rs`, framed by `aux_window.rs` |
| what goes in `main.log`, and where it lives | `crates/disktree-app/src/diag_log.rs` — never a scanned path |
| `THIRD_PARTY_LICENSES` | `cargo xtask licenses` (cargo-about), then commit it; non-crate parts in `licenses/bundled.txt` |
| the macOS icon, in any of its looks | `packaging/macos/` — the SVGs and `AppIcon.icon`, then `make dock-icons` |
| checking, downloading and applying updates | `crates/disktree-app/src/updater.rs` |
| what the window says about updates, and its notices: the toasts, the strip's ready chip | `crates/disktree-app/src/toasts.rs`, the chip in `views.rs` |
| the MCP connect commands and the connected agent's status line | `crates/disktree-app/src/agent.rs` |
| which release a release channel picks | `crates/disktree-app/src/update_channel.rs` |
| release packaging, the DMG, `vpk` | `scripts/release/`, `xtask/src/bundle.rs`, `.github/workflows/release.yml` |
| what an agent can ask of the window, and the answer | `crates/disktree-link/src/lib.rs` (messages), `crates/disktree-app/src/agent.rs` (handling) |
| a tool, its parameters or its wording | `crates/disktree-mcp/src/server.rs` (the MCP surface), `crates/disktree-mcp/src/session.rs` (what it does) |
| how a listing or a plan reads to an agent | `crates/disktree-mcp/src/report.rs` |
| what an agent is told to do, and to avoid | `skills/` (each system's cautions in `skills/pwrdisk-free-space/references/`), listed in `crates/disktree-mcp/src/skills.rs` |

## Verification expectations

* Size accounting, hardlinks, symlinks, hidden entries, depth limits, the
  removal guards and squarified layout are covered by `disktree-core` tests
  against real temporary trees.
* The screens are covered by window-harness tests that draw frames and press
  keys, including one that marks a directory, confirms the removal and checks
  the files are gone while unmarked neighbours are untouched, and one that
  drives the window over the real socket or pipe as `pwrdisk mcp` does.
  Initialize them through `tests::init`, which applies a fixed theme without
  native filesystem watching. Keep its control bindings aligned with
  gpui-omarchy when updating that dependency; its normal `init` starts a
  watcher that cannot wake GPUI's deterministic test executor safely.
* `disktree-mcp` tests every tool as a function against a temporary tree,
  with a stand-in window on the socket or pipe for the linked cases;
  `disktree-link` tests the transport itself.
* Rendering is verified by those tests and, where an on-screen macOS window
  is needed, in the PwrSuiteLab VM against a contrived tree (see
  [Testing on a desktop](#testing-on-a-desktop)); it has not been eyeballed
  in every theme and font size.

## Pull requests

* **Screenshots and videos go on the PR with `gh --attach`.** GitHub CLI
  2.99+ uploads them: `gh pr create/edit/comment --attach ./after.png`, with
  the same `./after.png` in the Markdown body, rewrites that link to a
  `user-attachments` URL. Never push images to a side branch, a gist or the
  repository to host them. `gh` exits 0 even when the link does not match, so
  check every image link before calling a PR done. The procedure, and the
  three ways it fails silently, are in
  [docs/pr-screenshots.md](docs/pr-screenshots.md); read it before the first
  `--attach`, and before any later body edit.
* **A PR that changes what the window shows has before/after screenshots**,
  of a contrived tree rather than a real disk;
  [docs/pr-screenshots.md](docs/pr-screenshots.md) says how to make one in
  the lab VM, never on the operator's desktop. If nothing can show the
  change, say why in the body and ask the operator.
* **Scans are private data.** A real scan shows user names in paths,
  project and client names, volume names and sizes, and the chooser lists
  mounts and SSH hosts. Show any screenshot that is not 100% contrived to the
  operator and wait for approval before attaching it.
* **Titles are Conventional Commits:** `type(scope): what changed`, in the
  imperative, as the Pwr family's other repositories do. Scopes follow the
  table above: `core`, `scan`, `ssh`, `removal`, `macos`, `windows`, `app`,
  `ci`, `release`, `docs`. A port of an upstream PR says so in the title,
  `(port of tobi/disktree#N)`, and keeps the upstream commits and their
  authorship.
* **Don't edit `CHANGELOG.md` in feature or fix PRs.** Every open PR that
  adds a line conflicts with every other one. Put the user-facing summary in
  the PR body; one release commit (`chore(release): vX.Y.Z`) writes the
  version's section from changes since the last tag and is pushed directly
  to `main` as a fast-forward update, without a release-prep PR. Follow
  `.agents/skills/release/SKILL.md` when cutting a release.
* **PRs go to the fork, `pwrdrvr/PwrDisk`.** In a checkout with several
  remotes, push only to the one pointing there; `tobi/disktree` and personal
  forks are read-only from here.

## PwrAgent threads

* Before opening or attaching a PR, check that the thread's workspace holds
  its head commit. Attaching a PR does not move the workspace.
* Attach the thread's PR and watch it with `notifyOn: ["merged", "closed"]`,
  so the thread hears when it lands or is dropped.
* Work worth doing that this change leaves out goes on a PwrAgent to-do card
  (`add_todo`, with a `start_thread` action whose prompt stands alone), not a
  note in the PR body, a commit message or a code comment. The operator does
  not read those notes; a card can be tracked and started.
* A PwrDisk window open on this machine belongs to the operator or another
  thread: several checkouts and builds run here at once. Leave it alone, and
  do not start another to look at a change; that runs in the lab, as
  [Testing on a desktop](#testing-on-a-desktop) says. That includes the
  link: do not point a local `pwrdisk mcp` at it to test a change; the
  `disktree-mcp` tests use a stand-in window.

## Agent instruction files

Agent guidance lives in `AGENTS.md`, not in the README or code comments, and
in the smallest scope that needs it. Long procedures go in `docs/` with a
one-line pointer here that says when to read them. Every `AGENTS.md` has a
sibling `CLAUDE.md` symlink (`ln -s AGENTS.md CLAUDE.md`), so Codex and
Claude read one file.
