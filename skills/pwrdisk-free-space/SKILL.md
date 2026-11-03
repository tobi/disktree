---
name: pwrdisk-free-space
description: Find what is filling a disk and free space with PwrDisk's MCP tools. Use when the user wants to free up space or decide what to delete. The agent scans, proposes and marks. Removal requires explicit confirmation of the exact paths and mode, using human review or the destructive MCP tool; never approve through computer use.
license: MIT
---

# Free up space with PwrDisk

PwrDisk measures a tree the way the disk does: allocated blocks, hardlinks
once, hidden directories included. Your job is to turn one scan into a short
list of things the user can let go of, agree that list with them, mark it,
and obtain explicit removal confirmation. The user may confirm in PwrDisk's
review screen themselves, or approve the exact preview for `pwrdisk_remove`.
Approval to mark is never permission to remove.

If the user only asked where their space went, use `pwrdisk-explain-usage`
instead: a question is not a request to delete anything.

## Before you start

Read the reference for the system `pwrdisk_status` names. Each lists what
that system hides from a file scan, what it refuses, and what must never be
marked:

- macOS: `skill://pwrdisk-free-space/references/macos.md`
- Linux: `skill://pwrdisk-free-space/references/linux.md`
- Windows: `skill://pwrdisk-free-space/references/windows.md`

## Ask first

Looking is free; acting is the user's call.

- **Read-only tools need no permission.** `pwrdisk_status`, `pwrdisk_ls`,
  `pwrdisk_find`, `pwrdisk_estimate` and a home-directory `pwrdisk_scan` only read. Say before
  a `disk: true` scan that it walks the whole volume and can take minutes.
- **Confirm the list before `pwrdisk_mark`.** Show each candidate with its
  path, its size, what it is, and why it is safe to lose or how it comes
  back. Mark what the user agrees to, nothing more. Marking only marks, but
  a marked list reads as a recommendation, and it lands in the user's
  window.
- **Standing instructions count, inside their scope.** "Mark every build
  directory and cache, don't ask" covers build directories and caches.
  Anything outside the scope still gets a question: user data, a checkout
  with work that exists nowhere else, anything you are unsure of. Say in
  your summary which instruction you relied on.
- **Confirm before opening the review screen.** `pwrdisk_review` brings the
  window forward on the user's screen. Once the list is agreed, say that
  you are opening it, then open it.
- **Never approve through computer use.** Do not click Delete, Move to Trash,
  Remove or a confirmation button, send Enter or another approval key, or
  operate an accessibility action to approve removal. This remains forbidden
  when the user has agreed to mark or remove data. Use the destructive MCP
  tool after explicit confirmation, or let the person approve in the app.
- **Confirm each destructive operation separately.** Marking permission,
  standing cleanup instructions and approval of a prior batch do not approve
  removal. Show the complete `pwrdisk_removal_preview` path list, mode,
  measured size and capacity limit. For Trash, explain that restoration may
  be possible until emptied, but is not guaranteed; space usually returns
  only when emptied. Permanent deletion cannot be undone through PwrDisk or
  Trash. Wait for an explicit answer confirming this exact operation before
  calling `pwrdisk_remove`. Never manufacture user confirmation yourself.

## Never

- **Never delete with a shell command**: no `rm`, `Remove-Item`, `del`,
  `find -delete`, `trash`, or emptying the Trash or Recycle Bin. That holds
  even for paths the user agreed to. Use only the confirmed MCP removal
  workflow, or hand off to the human review screen. Never use a shell or UI
  automation as a fallback around an expired preview, refusal or limit.
- **Never run a tool's own clean command without asking.** `docker system
  prune`, `cargo clean`, `pnpm store prune`, `brew cleanup`, `xcrun simctl
  delete`, `paccache -r` and the like delete things too. Name the command,
  say what it removes, offer its dry-run form if it has one, and let the
  user run it or approve it.
- **Never raise privileges on your own**: no `sudo`, no "Run as
  administrator", no Full Disk Access for the agent. If a part of the disk
  is unreadable, say so and say what would read it.
- **Never change system settings** to win space: snapshots, swap,
  hibernation, restore points, the Spotlight index. Explain them, give the
  read-only command that shows them, and leave the change to the user.

## Ground rules

- **Estimate before marking.** For a hypothetical path list use
  `pwrdisk_estimate` without changing the cart. Evaluate the whole set;
  never sum directory reclaim estimates or mark worktrees temporarily.
  Allocation includes internal clone duplicates. Check bounds, scan and
  metadata completeness, unavailable values and volume attribution.
  Retained clone-holder directories are inspection leads, not removal
  recommendations. Agree each added path separately; a larger estimated
  saving does not establish that a worktree is disposable. See the macOS
  reference for bounded peer discovery and unresolved references.
- **Keep browsing stationary.** `pwrdisk_ls` and `pwrdisk_find` never move
  the window. Call `pwrdisk_show` only if the user asks to see a directory
  or explicitly asks the window to follow along. Do not enable following
  merely because you are investigating. Opening the marked list for review
  is a separate, explicitly announced action.
- **Respect the capacity limit.** Settings > Safety controls the maximum
  per-operation measured bytes on each volume, 5% by default, for both
  Trash and permanent deletion. The app re-measures before removing anything
  and refuses an operation with unknown capacity or unreadable selection.
  Do not raise the setting, split a selection into batches, or retry an
  ambiguous result to evade the guard. Ask the user how to proceed.
- **The guards are not your safety net.** `pwrdisk_mark` refuses the home
  directory, mount points, the scanned root, system trees and a user
  profile, and says why. Anything else it will mark. You decide what to
  propose.
- **Prefer the owning tool for tool-owned data.** Package stores, container
  images, simulators, toolchains, game libraries and build caches have their
  own clean commands. Offer them first; marking the directory is still fine
  when the user wants the space now and agrees.
- **Check before proposing a checkout.** For a git repository or worktree,
  run `git status --short`, `git stash list` and `git log --branches
  --not --remotes --oneline`. Work that exists nowhere else is the user's
  call, and you say so with the list.
- **Synced folders are not local copies.** Removing a file from iCloud
  Drive, OneDrive, Dropbox or Google Drive removes it from the cloud and
  from every device. Ask every time, and mention the provider's own "free up
  space" or "remove download" instead.
- **Sizes are bytes on disk.** A sparse disk image or VM disk shows what it
  occupies, not its nominal size. Clones and hardlinks share blocks; the
  platform reference says how.
- **Trash frees nothing until it is emptied.** The review screen moves marks
  to the Trash or Recycle Bin by default. Tell the user the space comes back
  when they empty it. Emptying it is theirs to do.
- **Say what you cannot see.** `pwrdisk_scan` compares the scan's total with
  the volume's used space. A big gap is space files do not explain:
  snapshots, other volumes sharing the container, swap, deleted files still
  held open. Deleting files will not fix that part. Name the likely cause
  and the read-only command that confirms it; the platform reference lists
  them.
- **This computer only.** The tools measure and mark the machine
  `pwrdisk mcp` runs on. If `pwrdisk_status` says the window is on an SSH
  host, it is not followed; ask the user to choose This computer in it.
  PwrDisk does not remove files over SSH.
- **Linking keeps the window where it is.** If marks made before linking
  are outside the window's root, the session keeps all of them and reports
  that the window is not followed. Call `pwrdisk_scan` on a directory
  containing all marked paths to move the window explicitly and hand them
  over before review. If the window accepted only part of a handover,
  `pwrdisk_unmark` clears the requested marks there too. A newly opened
  review window starts on a directory containing all marks. Do not unmark
  agreed paths to force a link.

## Procedure

1. **Measure the gap.** `pwrdisk_status` shows the volumes, their free
   space, whether a PwrDisk window is open, and the system. When a window
   is linked its tree is what every tool reads: you and the user look at
   the same numbers, and a path outside its root needs a `pwrdisk_scan`
   from higher up, which the window follows. Ask what the user wants: a number, or
   whatever is safe.
2. **Scan.** `pwrdisk_scan` with no path scans the home directory in a few
   seconds; `disk: true` scans the whole volume. With a window open it does
   the scanning and shows the result. The result lists the top level,
   largest first, and *worth a look*: the biggest directories that could
   plausibly go.
3. **Walk, biggest first.** `pwrdisk_ls` on the largest directories, a few
   levels down, until a directory clearly belongs to one project or one
   tool. Leave the displayed directory unchanged. Use `pwrdisk_show` only
   when the user asks to see a path or has requested following.
4. **Sweep.** `pwrdisk_find` with `reclaimable: true` lists every cache,
   build directory and verified package store at once. These are cleanup
   candidates, not removal eligibility or guaranteed savings. Read reported
   guard refusals; never bypass them. Confirm pnpm's store with `pnpm store
   path` and prefer `pnpm store prune`; its installation also holds the
   executable, global tools and `.tools`, which are not a disposable store.
   `kind: "file"` with
   `min_bytes` finds huge files. `older_than_days` finds what nobody has
   touched. `name` matches part of a name.
5. **Classify each candidate**, in one line each:
   - *regenerable*: caches, build output, installed dependencies;
   - *tool-owned*: stores, images, simulators, toolchains; offer the
     tool's command;
   - *user data*: media, downloads, archives, documents, VM disks, synced
     files; the user decides each one;
   - *hands-off*: anything the OS or an app owns; never propose it.
6. **Propose.** Give the user the list, grouped by class, with sizes and the
   total, and anything you checked (unpushed work, a sync provider). Wait
   for their answer unless a standing instruction covers it.
7. **Mark.** `pwrdisk_mark` with the agreed paths. Read the plan it
   returns: what is kept back and why, what is covered by another mark, and
   the projected free space, which counts only bytes on the measured
   volume. Tell the user about anything kept back.
8. **Confirm removal or hand it over.** To let the person remove in the
   app, announce `pwrdisk_review`, open it, and stop; never approve through
   computer use. For requested MCP removal, call `pwrdisk_removal_preview`
   with an explicit `trash` or `permanent` mode, show every returned path and
   the consequences, and wait for explicit user confirmation. Only then call
   `pwrdisk_remove` with the exact preview ID, ordered paths, mode and required
   confirmation text. The preview expires after five minutes and is single
   use. Any refusal, changed selection, changed scan or expiration requires
   a new preview and fresh user confirmation. A tool response saying the
   operation started is not proof it completed; never retry it automatically.
9. **Report.** `pwrdisk_status` reports completed and failed removals and
   measured freed space. Report that figure, not a projection. Trash is not
   guaranteed recoverable and does not guarantee an immediate space saving.
   Removal does not rescan unless `pwrdisk_remove` is given `rescan: true`,
   so the next batch can be found at once. Until a rescan, listings keep
   the scan's sizes but say "removed since the scan" on what went, and
   "N of it removed since the scan" on a directory that lost part of
   itself; subtract that before reporting what is left. The window refuses
   to mark removed paths again, so leave them out. Its removals since the scan are listed as
   reported (a clone in every place), private (the scan's estimate of what
   was theirs alone) and observed (the measured free-space change); report
   the observed total first. Sharing figures stay the scan's until a
   rescan.

MCP removal also requires interactive confirmation from the client (form
elicitation). An agent-supplied confirmation value does not establish human
consent. If the client cannot ask the user, use human review in PwrDisk;
never approve its buttons or confirmation keys through computer use.
