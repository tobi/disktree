# PwrDisk on Windows

What Windows keeps outside a file scan, what PwrDisk refuses, and what to
leave alone. Read with `skill://pwrdisk-free-space/SKILL.md`.

## Working with the window

On Windows `pwrdisk mcp` connects to an open PwrDisk window over a local
named pipe restricted to the current Windows account. The window is the
source of truth: scans, listings, searches and marks use its tree, and the
agent's marks appear there. `pwrdisk_show` moves its view and
`pwrdisk_review` opens its review screen; only the person confirms removal.
Without a window the agent scans and marks in its own session and can
launch a window for the handover. Agree the list before marking unless the
user gave a standing instruction. Never remove files yourself.

When the window shows an SSH source, its paths belong to a Unix host and
use `/` on every client; backslashes and colons in names stay literal.
SSH windows answer only link identity and state requests. Switch to a local
source before using local scan, listing, marking or review tools. Windows
SSH servers are not supported.

## Reading the numbers

- **Sizes are allocation.** PwrDisk counts what each file occupies on disk,
  which is less than its length for a compressed or sparse file and more for
  a small one.
- **Cloud files take no space until downloaded.** OneDrive, iCloud for
  Windows and Dropbox use placeholders; an online-only file costs nothing
  locally. Deleting a synced file deletes it in the cloud and on every
  device. To free local space without deleting, the user right-clicks it in
  File Explorer and chooses "Free up space".
- **Space the files do not explain**: System Restore and shadow copies
  (`vssadmin list shadowstorage`, from an elevated prompt), the page file,
  `hiberfil.sys`, and the Recycle Bin of each drive. Changing any of these
  is the user's decision in Settings or System Properties.
- **An elevated whole-drive scan** in the PwrDisk window reads the NTFS
  master file table. It also counts alternate data streams, so a file's
  size can be larger there than in a normal scan. The agent's scans are not
  elevated; do not ask for elevation to look.

## Refused by PwrDisk

`pwrdisk_mark` keeps these back and says why: `Windows`, `Program Files`,
`Program Files (x86)`, `ProgramData`, `System Volume Information`,
`$Recycle.Bin`, `Recovery`, `Boot`, `bootmgr`, `pagefile.sys`,
`hiberfil.sys`, `swapfile.sys`, every user profile directly under
`C:\Users`, the home directory and anything holding it, mount points, and
names Windows would misread (trailing dots or spaces, device names such as
`NUL` or `COM1`, a colon).

## Never propose

- **WinSxS, the component store, and installers' caches** under Windows:
  Disk Cleanup, Storage Sense or `DISM /Online /Cleanup-Image
  /StartComponentCleanup`, which the user runs.
- **Installed programs**: uninstall from Settings › Apps, which the user
  does.
- **`AppData\Roaming`, `AppData\Local\Packages`**: application settings and
  Store apps' data. Only a specific, known cache inside `AppData\Local`,
  with the app closed.
- **WSL and Docker Desktop disks** (`ext4.vhdx`, `docker_data.vhdx`):
  deleting one destroys every distribution or container in it. Use
  `wsl --unregister` (after `wsl --export`) or Docker's prune commands.
- **Synced folders** (`OneDrive`, `OneDrive - <organization>`,
  `iCloudDrive`, `Dropbox`): see above.

## Usually safe, with the owning tool first

| what | where | prefer |
| --- | --- | --- |
| temp files | `%LOCALAPPDATA%\Temp` | Settings › System › Storage › Temporary files |
| app cache inspection | `%LOCALAPPDATA%\<app>\Cache` | close the app; use its supported cleanup; unknown owners need inspection |
| browser caches | the browser's profile `Cache` folders | the browser's clear-cache setting |
| language caches | `%LOCALAPPDATA%\npm-cache`, `%USERPROFILE%\.cargo\registry`, `%LOCALAPPDATA%\pip\Cache` | the tool's clean command |
| build output | `target\`, `node_modules\`, `bin\`, `obj\` | check the checkout first (see the skill) |
| game libraries | `steamapps`, Epic and Xbox folders | uninstall in the launcher |

## Recycle Bin

Removal moves marks to the Recycle Bin of their drive. The space returns
when the user empties it. A network share has no Recycle Bin, so a removal
there may not be recoverable; tell the user before they mark one.

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

Capacity checks charge a symbolic link or junction to its containing
volume, not its destination. Individual sparse and compressed files use
the same allocated size as directory scans.

## Owner-managed caches

A cache name or regenerable classification is an inspection lead, never
permission to delete it. Cache cleanup is decided path by path. Call
`pwrdisk_removal_preview` with `owner_cleanup` and no decisions: it returns
one proposal per marked path with its status and problem, resolved owner,
version, executable, arguments, environment overrides, configured root, full
actual scope (including unselected siblings), effects, limitations,
recoverability, and whether Delete files is offered. Show every proposal and
let the user choose for each path `skip`, `owner` (only where offered) or
`delete_files`. Never choose for them and never suggest `delete_files`
yourself; inspect unknown runtime/install caches first, since nothing says
they are disposable. Then preview again with `decisions` naming every marked
path once, and `delete_files_mode` (`trash` or `permanent`, as the user
chose) when any path uses `delete_files`. Confirmation needs that preview's
exact paths and its own text: `RUN OWNER CLEANUP`, `DELETE CACHE FILES`,
`TRASH CACHE FILES`, or both intents joined with `AND`. Owner approval never
approves deleting files. Unavailable commands do not establish absent
software. An unsupported version, failed probe, or failed or cancelled owner
command never falls back to deleting files, and never to a shell command;
deleting them instead is a new preview the user decides.

Cache tags and owner layouts on ancestors protect contained files and
subtrees too. All removal modes may return a preparing response while
inspection runs on a worker; retry only the read-only preview. Exported
lists keep marked paths, and raw script exports cannot replace owner cleanup.

On macOS and Linux the initial adapters cover npm packages (9–11), npm's
separate npx cache (11.2–11.x), uv (0.11.x), pip (23–26) and Cypress (13–15),
only when their installed CLI resolves the selected cache. pnpm, Homebrew,
Playwright and Puppeteer remain explicit inspection-only owners because their
full command/configuration scopes are not supported. Windows command
launchers have no execution adapter yet, and on Windows Delete files is not
offered either: it needs PwrDisk's check for programs using the path, which
runs only on macOS and Linux. A Windows cache cleanup can therefore only be
inspected and skipped. Neither containers nor volumes are cleanup scopes.

Each path gets its own result: succeeded, failed with a reason, cancelled
(possibly part done), skipped, or not reached after Stop. A command's
success reports the owner's exit result, not guaranteed savings.
Report allocation separately from observed volume availability before/after;
other activity, hardlinks, open files, APFS clones and snapshots can retain
blocks. PwrDisk always rescans after owner cleanup, even partial failure.
Never alter snapshots to force the reported allocation to become free space.

## Developer artifacts and virtual machines

Artifact identity and evidence are separate from cleanup suitability.
`detected` is inventory only. `suggested` means a plausible, relatively
lower-risk candidate to review, with local edits, active tools and the
cost of reinstalling or rebuilding still to check. Name conventions are
weaker evidence than recognized tool layouts. Whole tool homes and generic
`target`, `build`, `dist`, `bin` or `obj` names do not establish disposability;
review the specific detected subtree instead.

VM/container disk images can contain unique work. Manage snapshots in the
owner app, never by removing individual files in a snapshot chain. Saved
memory or suspend state can usually be released by resuming the VM and
shutting it down normally; checkpoints may retain it. VM data stays a
detection even inside a directory named like a cache, and its displayed
size is not a promise of recovered space. Do not run an owner's destructive
cleanup command without explicit approval for that operation.
