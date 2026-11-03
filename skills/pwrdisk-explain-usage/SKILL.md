---
name: pwrdisk-explain-usage
description: Explain where a disk's space went with PwrDisk's read-only MCP tools. Use when the user asks what is eating their disk, why it is full, or why free space disagrees with what they see. Never marks or removes anything.
license: MIT
---

# Explain disk usage with PwrDisk

The user wants to understand their disk, not change it. Use only the tools
that read: `pwrdisk_status`, `pwrdisk_scan`, `pwrdisk_ls`, `pwrdisk_find`,
`pwrdisk_estimate`
and, only when the user asks to see a path, `pwrdisk_show`. Do not call
`pwrdisk_mark`, `pwrdisk_review` or `pwrdisk_remove`, and do
not run anything that deletes or changes settings. End with the
explanation; offering to free space is a question you ask, not a step you
take.

Read the reference for the system `pwrdisk_status` names. Each says what
that system keeps outside any file scan and which read-only command shows
it:

- macOS: `skill://pwrdisk-free-space/references/macos.md`
- Linux: `skill://pwrdisk-free-space/references/linux.md`
- Windows: `skill://pwrdisk-free-space/references/windows.md`

## Procedure

1. **Status.** `pwrdisk_status`: the volumes, their size and free space,
   and whether a PwrDisk window is linked. With a window linked, its tree
   is what you read. Status reports allocated versus apparent sizes, hidden
   entry inclusion and unreadable count; retain that context without a
   rescan. Browsing never moves the displayed directory. Use `pwrdisk_show`
   only when the user asks to see a path or requests following. If earlier
   marks are outside its root, the session keeps them and leaves the
   window where it is; status explains why it is not followed.
2. **Scan the right thing.** The home directory answers "what of mine is
   big" in seconds. `disk: true` answers "why is the disk full"; say first
   that it walks the whole volume and can take minutes.
3. **Account for the volume.** The scan reports how much of the volume's
   used space it explains. Name the rest rather than guess at it:
   snapshots, other volumes in the same container, swap and hibernation,
   files deleted while still open, and parts the scan could not read. Give
   the read-only command from the platform reference that confirms each.
4. **Walk the big branches.** `pwrdisk_ls` largest first, a few levels
   down, until each big directory has an owner: a project, a tool, an app,
   the OS, or the user's own files. `pwrdisk_find` with
   `reclaimable: true` totals classified cleanup candidates such as caches,
   build output and verified package stores, including guard refusals;
   `kind: "file"` with `min_bytes` finds the large single files.
5. **Explain.** A short table: what, where, how big, who owns it, and
   whether it comes back by itself. Say plainly where PwrDisk's number
   differs from another tool's and why: PwrDisk counts allocated blocks,
   counts a hardlinked file once, and includes hidden files unless skipped;
   apparent-size scans measure file lengths instead. On macOS summed
   allocation can exceed volume usage because APFS clones share blocks.
   Never cap the total or call it guaranteed freed space. Finder, Explorer
   and `du` may measure differently.
6. **Estimate a hypothetical set when asked.** Pass the full list to
   `pwrdisk_estimate`; never mark temporarily or add directory estimates.
   Report listed allocation, private/shared allocation and selected-set
   reclaim bounds separately. Shared bytes include duplicates inside the
   selection. Check completeness and unavailable measurements. Bounded
   clone-holder directory peers help find retained copies; inspect those
   directories, never recommend deleting a worktree just because it unlocks
   space. Change `peer_scope` within the scan to group other directories.
   Peer rows are alternatives; use the returned combined union or another
   full path-list estimate. Unresolved reference counts do not identify
   out-of-scan owners, and clone IDs do not enumerate modified shared extents.
7. **Offer, don't act.** If space can be had back, say roughly how much
   and from what, and ask whether the user wants to go through it. If they
   do, follow `skill://pwrdisk-free-space/SKILL.md`, which starts by agreeing
   the list with them.

## Keep in mind

- A scan can be unreadable in places: protected folders, other users'
  files, system directories. Report the count of unreadable entries and do
  not ask for elevated access to fill the gap.
- Synced folders may hold files that are only in the cloud and take no
  local space; the platform reference says how each provider shows them.
- Paths and names in a scan are the user's private data. Quote what
  answers the question; do not paste whole listings into places other than
  this conversation.

Never use computer use to approve removal, including Enter, clicks or
accessibility actions in the review screen. A read-only audit ends with an
explanation; approval to investigate or mark is not approval to remove.
