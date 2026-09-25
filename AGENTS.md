# disktree — agent guide

A GPUI + gpui-omarchy treemap explorer for disk usage on Omarchy. Read
`README.md` for the product; this file is the working contract.

## What this is

Find what is eating a volume, mark paths for removal, review the list, and
remove it — with the volume's free space live on screen the whole time. Two
phases: explore (treemap, breadcrumbs, the selection and status lines) and review (the marked list,
the removal mode, the confirmation). Marking is never destructive.

## Commands

```sh
make build                      # release build
make run                        # build and run, scanning $HOME
make install                    # ~/.local: binary, desktop entry, icon
make install PREFIX=/usr/local  # system-wide (needs root)
make uninstall
make bundle                     # macOS: target/bundle/disktree.app and its zip
make lint                       # rustfmt --check, then clippy --all-targets -D warnings
make test                       # core and window-harness tests
make ci                         # lint, then test
make fmt                        # format in place
```

`make install` is the supported way to put this on a machine: it installs the
release binary, `packaging/disktree.desktop.in` (rendered with the real install
prefix and the crate version) and `assets/disktree.svg`. Keep the desktop
entry's `Categories` to a single main category plus additional ones, or
`desktop-file-validate` complains.

On macOS `make install` installs `target/bundle/disktree.app` into
`~/Applications` instead, built by `cargo xtask bundle` from
`packaging/macos/` (the `Info.plist` template and the icon's SVG). The bundle
identifier there keys every macOS permission grant; do not change it lightly.

`cargo xtask lint` is the gate. It must be green before anything is called
done, and it must not fix anything: a red local run is the same signal CI gives.
On Windows, where there is no make, run `cargo xtask lint`, `cargo xtask test`
and `cargo build --release` directly; CI runs the gate on both systems.

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
   That is the number that comes back when a file is deleted. On Windows it
   is the allocation the directory listing reports; see `windows.rs`. An
   elevated scan of a whole NTFS drive reads it from the master file table
   instead, keeping the walk's rules for hidden entries, links, cloud
   folders and depth; see `mft.rs`. That path counts every stream's
   allocation, alternate data streams included since they go when the file
   goes, so for a file with alternate streams it can exceed the walk's
   number.
2. **`own_bytes`/`own_files` are derived, never tracked.** `tree::aggregate`
   computes the totals from the children. Hardlink de-duplication zeroes a
   duplicate leaf's weight while that pass runs; anything that patches
   `bytes` directly will be overwritten.
3. **A directory is only built when its own scan *and* every subdirectory task
   has finished.** That is the `+1` sentinel in `PendingDir::pending`. Building
   early silently drops whole subtrees — it has happened once.
4. **Only paths under the scanned root may be removed**, and mount points, the
   root, the home directory, any directory holding it, and symlink targets are
   refused. On Windows so is every directory directly in the profiles folder
   Windows reports (`FOLDERID_UserProfiles`, usually `C:\Users`); what is
   inside a profile stays removable.
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
   from marked bytes; the final number comes from `statvfs` before and after.
10. **Git is only read.** `git::Git` runs every command with no inherited
    `GIT_*` variable, optional locks off and every program a checkout's
    config could name overridden; nothing fetches, an fsmonitor daemon is
    used only if one is already running, and a checkout that defines its own
    filter drivers is not read. Removing a linked worktree's folder leaves
    its branches, commits and stashes in its repository, which is why
    `Checkout::loses_nothing` asks only what the folder itself holds.
11. **Details are found cheaply and read later.** `details::details` decides
    from the tree and a few `lstat`s what else an item is, since the panel
    asks on every pointer move; anything slower is a reading, which the
    window starts once it has been wanted for a moment, two at a time, and
    keeps until the next scan.

## Where changes belong

| change | where |
| --- | --- |
| measurement, filtering, parallelism | `crates/disktree-core/src/scan.rs` |
| what a node is, or a derived total | `crates/disktree-core/src/tree.rs` |
| tile geometry, nesting, the merged tail | `crates/disktree-core/src/treemap.rs` |
| anything that deletes, or refuses to | `crates/disktree-core/src/removal.rs` |
| what an item is beyond its size (a new kind is a `Detail` case, its reading, its card) | `crates/disktree-core/src/details.rs`, `crates/disktree-app/src/detail_cards.rs` |
| reading a git checkout, what removing it would lose | `crates/disktree-core/src/checkout.rs`, and `git.rs` for how git is run |
| free space and projections | `crates/disktree-core/src/space.rs` |
| what Windows lists, measures and compares differently | `crates/disktree-core/src/windows.rs` — the only `unsafe` |
| reading a whole NTFS drive from its file table | `crates/disktree-core/src/mft.rs` |
| a key, a screen transition, a mark | `crates/disktree-app/src/state.rs` |
| spacing, type and size | `crates/disktree-app/src/ui.rs` — tokens only, no `px` in layout |
| the mosaic's painting or labels | `crates/disktree-app/src/treemap_view.rs` |
| layout of a screen | `crates/disktree-app/src/views.rs` |
| colours derived from the theme | `crates/disktree-app/src/palette.rs` |

## Verification expectations

* Size accounting, hardlinks, symlinks, hidden entries, depth limits, the
  removal guards and squarified layout are covered by `disktree-core` tests
  against real temporary trees.
* Git checkouts are covered by `checkout.rs` tests that build a bare origin,
  a clone and worktrees with git itself and read them: merges, squashes found
  by content and by patch, detached heads, locks, nested checkouts, ignored
  `.env` files, submodules kept out of the worktree list, and a checkout's
  own fsmonitor, signature program and filter drivers never running. The
  merge base comes from `rev-list --boundary` and the squash search has a
  time budget because a monorepo with a hundred thousand remote refs makes
  the obvious commands take seconds.
* The screens are covered by window-harness tests that draw frames and press
  keys, including one that marks a directory, confirms the removal and checks
  the files are gone while unmarked neighbours are untouched.
* Rendering was verified by those tests and by running the app against a real
  home directory; it has not been eyeballed in every theme and font size.
