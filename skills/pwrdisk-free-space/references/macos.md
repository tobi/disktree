# PwrDisk on macOS

What macOS keeps outside a file scan, what PwrDisk refuses, and what to
leave alone. Read with `skill://pwrdisk-free-space/SKILL.md`.

## Reading the numbers

- **APFS clones share blocks.** Finder duplicates, `cp -c` and some package
  managers make clones: two copies can cost one copy's space, and removing
  one frees little or nothing. PwrDisk counts each file's allocation, so a
  cloned tree can exceed the volume's used space. Listed allocation is not
  guaranteed freed space; do not cap the scan total to volume usage. The
  free space measured after the review is the real figure.
- **Read a row's clone split.** A row with clone blocks adds
  `X private · Y shared with other APFS clones · frees ≈ Z alone`. Only the
  private part goes when that entry is removed by itself; the shared part
  stays while any other clone of it remains, and APFS counts clones outside
  the scanned root too. A header line `APFS clones: … counted again` says
  how much of the total is listed more than once. Do not present listed
  bytes, or the shared part, as space a clone-backed worktree or copy will
  free. A row without the clause has no clone files.
- **Use the selected-set reclaim bounds, not listed bytes.** PwrDisk
  evaluates accepted marks together: a complete clone group is counted
  once, while copies retained outside the selection keep shared blocks
  alive. All hardlink names must be selected before their allocation can
  be reclaimed. Clone groups are scoped to their physical APFS volume.
  Missing metadata widens the bounds; it does not justify projecting all
  listed bytes. Apparent-size scans and disabled clone measurements cannot
  establish physical reclaim estimates. Linked reports use the window's
  measurement context; an ongoing scan has no final estimate.
  Bounds are scan-derived, before snapshots, open files and
  later changes; even the lower estimate is not a guaranteed saving.
- **One container, several volumes.** The System, Data, Preboot, Recovery
  and VM volumes share one APFS container's space. A whole-disk scan of
  `/` explains the Data volume; the rest shows as space the scan does not
  explain. `diskutil apfs list` shows each volume's use.
- **Local snapshots hold deleted data.** Time Machine and system updates
  keep APFS snapshots; space freed while one exists may not come back until
  it expires. `tmutil listlocalsnapshots /` lists them. Deleting one
  (`tmutil deletelocalsnapshots`) is the user's decision; give the command,
  do not run it.
- **"Purgeable" is not free.** Finder counts purgeable space as available;
  PwrDisk and `df` do not. If the numbers disagree, that is usually why.
- **Swap and the sleep image** live on the VM volume. Leave them alone.

## Privacy and access

- macOS protects Desktop, Documents, Downloads, Mail, Messages, Safari,
  Photos and parts of `~/Library`. A scan started by the agent runs with the
  privacy grants of the app hosting the agent, such as a terminal or an
  editor. It may show these folders as unreadable or make macOS prompt for
  that app.
- A scan the PwrDisk window runs uses PwrDisk's own grants. If a protected
  folder matters, open the window (`pwrdisk_show`) and scan from there.
- Do not ask the user to give the agent's host Full Disk Access just to
  measure. Report what was unreadable and let them decide.

## Refused by PwrDisk

`pwrdisk_mark` keeps these back and says why: `/System`, `/Library`,
`/private`, `/cores`, `/opt/homebrew` and the other system trees, the home
directory and anything holding it, `/Users` and every account's folder in
it (`/Users/Shared`, `/Users/Guest`, another user's home; what is inside
one may be proposed), mount points, and the `.fseventsd` and
`.Spotlight-V100` stores (use `mdutil` for Spotlight, never delete either).
What is inside `/private/tmp` and `/private/var/tmp` (`/tmp` and
`/var/tmp`) may be marked, as on Linux; the two folders themselves stay
refused under `/private`, and so does `/private/var/folders`, where each
user's `$TMPDIR` and caches hold running apps' live state. Classification
is independent of removal eligibility: `pwrdisk_find` may list
`/private/tmp` as a temporary cleanup candidate and report the `/private`
guard refusal for the folder. Look inside it with `pwrdisk_ls`, propose
entries the user recognizes as their own leftovers, and leave files a
running program may still use. Do not bypass the guard.

## Never propose

- **Inside a library package**: the Photos Library, Music, Apple TV, iMovie,
  Final Cut and Logic libraries. Removing files inside breaks the library;
  the app manages its own media.
- **`~/Library/Mobile Documents`, `~/Library/CloudStorage`**: iCloud Drive
  and File Provider folders (Dropbox, Google Drive, OneDrive), and legacy
  Google Drive roots named `My Drive` or `My Drive (<account>)`. Deleting a
  file there deletes it in the cloud and on every device. An evicted file
  takes no local space; to free space without deleting, the user chooses
  "Remove Download" in Finder or turns on Optimize Mac Storage.
- **App data**: `~/Library/Containers`, `~/Library/Group Containers` and
  `~/Library/Application Support` hold apps' databases and settings. Only
  propose a specific, known cache inside them, with the app quit.
- **Mail and Messages attachments, Keychains, and iPhone and iPad backups**
  (`~/Library/Application Support/MobileSync/Backup`): user data and
  secrets. Old device backups are the user's to delete from Finder.
- **`/Applications`**: uninstall with the app's uninstaller or by dragging to
  the Trash in Finder, which the user does.

## Usually safe, with the owning tool first

| what | where | prefer |
| --- | --- | --- |
| app caches | `~/Library/Caches/<app>` | quit the app first; they regrow |
| Xcode build data | `~/Library/Developer/Xcode/DerivedData` | Xcode rebuilds it |
| old device support | `~/Library/Developer/Xcode/iOS DeviceSupport` | re-created when a device connects |
| simulators | `~/Library/Developer/CoreSimulator` | `xcrun simctl delete unavailable`; runtimes in Xcode › Settings › Components |
| Xcode archives | `~/Library/Developer/Xcode/Archives` | ask: needed to symbolicate shipped builds |
| Homebrew downloads | `~/Library/Caches/Homebrew` | `brew cleanup --dry-run`, then `brew cleanup` |
| Docker Desktop | `~/Library/Containers/com.docker.docker/.../Docker.raw` | never mark the disk image; `docker system df`, then prune |
| pnpm package store | `~/Library/pnpm/store/v*/files` (also older `.pnpm-store/v*/files`) | `pnpm store path` to confirm the location; `pnpm store prune` first; never treat the `pnpm` installation, executable, `global` or `.tools` as a disposable store |
| language caches | `~/.npm`, `~/.cargo/registry`, `~/Library/Caches/pip`, `~/.gradle/caches` | the tool's clean command |

## Trash

Marks go to the Trash on the volume that holds them. The space returns when
the user empties the Trash; on an external disk, that disk's `.Trashes`.

## Confirmed removal

Never approve a removal with computer use (clicks, keys or accessibility
actions). The user approves in the app, or explicitly confirms the exact
paths and mode from `pwrdisk_removal_preview` before the destructive
`pwrdisk_remove` call. Trash or Recycle Bin recovery may be possible until
emptied, but is not guaranteed; permanent deletion cannot be undone through
PwrDisk. Both modes use the existing removal guards and Settings > Safety's
per-operation limit, 5% of each volume's capacity by default. Never bypass a
refusal, increase the setting or split batches to evade it. A window
refuses a mark that would take the list past the limit, and marks made
without one are checked when they reach it: only the person can mark past
it, in the window. Say so, and let them decide.

MCP removal also requires interactive confirmation from the client (form
elicitation). An agent-supplied confirmation value does not establish human
consent. If the client cannot ask the user, use human review in PwrDisk;
never approve its buttons or confirmation keys through computer use.

Capacity checks charge a symbolic link to its containing volume, not its
destination.

## Read-only APFS path-set planning

Use `pwrdisk_estimate` with the complete hypothetical `paths` list before
marking. It leaves the cart, scan root, displayed directory and pending
removal previews unchanged. Do not temporarily mark worktrees to measure a
set, and do not add individual directory reclaim estimates: clone groups
and hardlink names can span directories.

`listed_bytes` counts directory allocation, even repeated clone blocks.
`shared_allocation` includes copies shared inside the selection and clone
allocation whose private size is unknown. `internal_duplicate_allocation`
reports repetitions within that whole set; neither is unique physical
usage or necessarily space retained outside it. Private allocation can
remain held by hardlink names outside the set. Reclaim bounds evaluate the
accepted union with existing guards; covered paths do not count twice.
Unavailable values are null, not measured zero. Apparent scans, disabled or
unsupported clone collection cannot supply physical bounds. Check metadata
and scan completeness, hidden filtering, depth, unreadable entries and
volume attribution. Cross-volume directory totals are not projected onto
the scanned volume's meter.

For four full 12 GB clones, selecting three can reclaim almost nothing.
Selecting all four can unlock the shared 12 GB once. The tool returns
bounded retained clone-holder **directories** and bounds for adding each,
plus a single estimate for adding all returned peers. Individual peer
savings are alternatives, not additive. `additional` uses interval
subtraction, so modified clones can have wide additional bounds. A peer is
an inspection lead, never permission or a recommendation to remove it.
Establish ownership, uncommitted work and the user's intent before proposing
any worktree for removal.

`peer_scope` groups peers by its direct child directories. Its default is
the selected paths' common parent; widen it within the current scan to look
elsewhere, then narrow it with another call if results are too broad. The
response bounds peer count and metadata visits and reports truncation.
Known unselected clone files are observations within the scan. Unresolved
reference counts have no owner paths; `references_not_seen_in_scan` is only
available when the relevant metadata walk and scan coverage are complete.
It does not identify external owners. Clone IDs cannot discover every
shared extent owner, especially after clones are modified. Snapshots, open
files, compression and changes after scanning can alter actual freed space;
never present these bounds as exact predictions. Trash usually frees space
only after the user empties it.
