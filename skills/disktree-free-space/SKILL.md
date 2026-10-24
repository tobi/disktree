---
name: disktree-free-space
description: Find what is filling a disk and free space with disktree's MCP tools. Use when the user asks what is eating their disk, wants to free up space, or wants to decide what to delete. The agent scans and marks; the person removes, on disktree's review screen.
license: MIT
---

# Free up space with disktree

disktree measures a tree the way the disk does: allocated blocks, hardlinks
once, hidden directories included. Your job is to turn one scan into a short
list of things the user can let go of, mark them, and hand the decision back
on the review screen. You never remove anything yourself.

## Ground rules

- **Mark, don't delete.** `disktree_mark` only marks. `disktree_review`
  opens the review screen in the disktree window, where the user presses
  Enter to move the marks to the trash, or chooses permanent deletion, which
  always asks first. Do not delete marked paths with shell commands instead;
  the window keeps the free-space meter honest and the user in control.
- **Prefer the owning tool for tool-owned data.** Package stores, container
  images, simulators, toolchains and build caches have their own clean
  commands (`cargo clean`, `pnpm store prune`, `docker system prune`,
  `xcrun simctl delete unavailable`, `uv cache clean`). Mention them as the
  alternative; marking the directory is still fine when the user wants the
  space now.
- **Check before marking a checkout.** For a git repository or worktree, run
  `git status --short`, `git stash list` and look for unpushed commits. Work
  that exists nowhere else is the user's call, not yours.
- **Sizes are bytes on disk.** A sparse disk image or a VM disk shows what it
  really occupies. On macOS, APFS clones (Finder duplicates, `cp -c`, some
  package managers) share blocks: two copies cost one, and removing one
  frees nothing. On Linux, pnpm stores and other hardlinked trees free their
  space only when the last name goes.
- **Say what you cannot see.** `disktree_scan` compares the scan's total
  with the volume's used space. A big gap is space files do not explain:
  Time Machine local snapshots or other APFS volumes on macOS, btrfs or ZFS
  snapshots, swap, deleted-but-open files. Deleting files will not fix that
  part; name the likely cause and the read-only command that confirms it
  (`tmutil listlocalsnapshots /`, `diskutil apfs list`, `btrfs subvolume
  list`, `zfs list -t snapshot`, `lsof +L1`).

## Procedure

1. **Measure the gap.** `disktree_status` shows the volumes, their free
   space, and whether a disktree window is open. When one is, its tree is
   what every tool reads: you and the user look at the same numbers, and
   a path outside its root needs a `disktree_scan` from higher up, which
   the window follows. Note what the user wants: a number, or "whatever
   is safe".
2. **Scan.** `disktree_scan` with no path scans the home directory in a
   few seconds; `disk: true` scans the whole volume. With a window open it
   does the scanning and shows the result. The result lists the top
   level, largest first, and *worth a look*: the biggest directories that
   could plausibly go (caches, build output, package stores, stale agent
   worktrees and experiments).
3. **Walk, biggest first.** `disktree_ls` on the largest directories, a few
   levels down, until a directory clearly belongs to one project or one
   tool. Pass `show: true` so the window follows you; the user then sees
   what you see.
4. **Sweep.** `disktree_find` with `reclaimable: true` lists every cache,
   build directory and package store at once. `kind: "file"` with a
   `min_bytes` finds the huge files. `older_than_days` finds what nobody
   has touched. `name` matches a substring of the name.
5. **Classify each candidate**, in one line each: regenerable (caches,
   build output, installed dependencies), tool-owned (stores, images,
   simulators), user data (media, downloads, archives, VM disks: ask), or
   hands-off (anything the OS or an app owns).
6. **Mark.** `disktree_mark` with the paths. Read the plan it returns: what
   is kept back and why (the home directory, a mount point, a system tree,
   a path outside the scan), what is covered by another mark, and the
   projected free space, which counts only bytes on the measured volume.
7. **Hand it over.** `disktree_review` opens the review screen. Tell the
   user what is marked and what it frees, and that Enter there moves it to
   the trash. Afterwards `disktree_status` reports what was actually freed.

A question about where the space went can end with the explanation. It is
not a request to delete anything.
