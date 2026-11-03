# Using PwrDisk

Everything the [README](../README.md) leaves out: each screen, every key,
how sizes are measured, scanning servers over SSH, working with an AI
agent, and what is different on macOS and Windows. Building from source is
in [building.md](building.md).

- [Starting a scan](#starting-a-scan)
- [Choosing sources and roots](#choosing-sources-and-roots)
- [The screen](#the-screen)
- [Marking](#marking)
- [Zooming and going in](#zooming-and-going-in)
- [Removing](#removing)
- [Keys](#keys)
- [What it measures](#what-it-measures)
- [The whole disk](#the-whole-disk)
- [Servers over SSH (read-only)](#servers-over-ssh-read-only)
- [With an AI agent](#with-an-ai-agent)
- [Updates](#updates)
- [On macOS](#on-macos)
- [On Windows](#on-windows)
- [What it refuses to do](#what-it-refuses-to-do)
- [On Hyprland](#on-hyprland)

## Starting a scan

```sh
pwrdisk            # choose a local/SSH source and root
pwrdisk ~          # explicitly scan home
pwrdisk --disk     # the whole disk it lives on
pwrdisk ~/src      # or any directory
pwrdisk --help     # options: apparent size, follow links, skip hidden, …
```

PwrDisk is built with [GPUI](https://gpui-kit.com/) through
[gpui-omarchy](https://github.com/huacnlee/gpui-omarchy), so on Omarchy it
follows your theme and behaves like the rest of the desktop. Elsewhere it
follows the system's light or dark setting.

## Choosing sources and roots

Launch without a path and the chooser lists **This computer**'s mounts.
Switch to **SSH host**, enter an SSH alias or `user@host` and press
**Connect** to list that machine's instead. Discovery reads that machine's
mount namespace, not file contents. It never starts a scan or probes
mounted network filesystems just to fill the list.

The list ends with a **Directory** row: type an absolute path there, or
choose **Choose…** for the system's folder picker on this computer. Select
a mount or the Directory row and press **Scan**; Enter does the same, and
in the host field it connects. Escape or **Cancel** closes the chooser and
returns to the current scan, or the idle welcome screen at startup; a click
outside it does not.

During exploration, press `v`, click **Source…** beside the disk name,
or click the PwrDisk logo to reopen the same chooser. The source being
browsed is shown explicitly, and connecting never retargets an existing
scan. Changing machines on acceptance clears the old machine's marks,
identity and navigation history; an absolute path is not an identity
across hosts. A rescan that detects a changed or lost remote machine
identity also clears marks, even if the SSH alias is unchanged.

Pseudo-filesystems, snapshots and automount triggers are filtered out.
Linux bind mounts and btrfs subvolumes remain individually selectable,
even when they share a device. Free space is measured only after choosing
a root. Explicit command-line paths and `--disk` still start directly;
`ssh HOST --check [PATH]` remains the headless scan command.

## The screen

- **Top:** the trail from `/`, then **Treemap | List**, how the directory is
  drawn, and what is measured — **Size**, **Files** or **Age**, **Hidden
  files**, **Apparent size**, and the depth drawn. The list shows the same
  directory one entry per row and is the view for screen readers. When the
  window is too narrow for every control, the ones used least move into
  **⋮** at the end of the bar: the scan options first, then back, forward
  and depth, then Size | Files | Age. In the
  tree a crumb goes there, and its ▾ lists its siblings, largest first with
  their share and size, to jump sideways (arrows and Enter work too). Above
  the scanned root a crumb is dimmer, and clicking it widens the scan to
  there (see [The whole disk](#the-whole-disk)).
- **Under it:** the scan totals and the filter when one is typed, and on
  its own row the legend, which wraps onto another line rather than hide
  an entry.
- **Mosaic:** colour is the *kind* of data — code, agent scratch,
  toolchains, synced files, git, media, documents, caches — at one muted
  level, lighter with depth. A diagonal hatch is space that can be had back
  (caches, sync history, package stores, build output), independent of
  colour. Top-level directories carry a strip of their colour and a name
  band; deeper open directories a slim label row. In **Age** mode colour is
  the last write instead, from this week to older.
- **Legend:** each entry is also a filter. Click it once to show only that
  kind (or age), again to hide it, and a third time to show it again.
  Kinds set to only add up ("Code" and "Cache" show both), while
  **Reclaimable** narrows them to their reclaimable part. The mosaic lays
  out only what passes, at its own size, and a folder that only partly
  passes says so: `48G of 196G`. Kind and age filters apply together, so
  "old code" is a click in each mode. **Show all** or `esc` clears them.
  When nothing in the folder on screen passes, the mosaic or list says
  so, and whether anything elsewhere in the scan does.
  A filter changes what is shown, never what a mark takes: marking a
  folder that is only partly shown marks all of it, and the panel says so.
- **Panel:** the selection (its size set large, share of the scan, files,
  last write, and for a checkout what git says — changes, stashes, unpushed
  commits); what is marked; *Worth a look*, the largest things that could
  plausibly go; then, pinned above the disk, the marks taken together —
  how much is marked, what removing all of it frees, and the way to the
  review screen; and the disk, free now and after the marks. On a short
  window *Worth a look* is what scrolls. Drag its left edge to resize it;
  double-click the edge to reset.

One colour is kept apart: amber marks the selection, the main action, and
what can be had back.

The kinds come from directory names and a few shapes (a bare git repository,
`target` beside a `Cargo.toml`). Some of the names are specific to one
machine; see `crates/disktree-core/src/classify.rs`.

## Marking

Space, X, Enter and the arrows act on the tile under the mouse if the mouse
moved last, and on the keyboard selection after you use an arrow or n.

A click selects the tile under the pointer. A second click on the
selection, slow or as a double-click, opens one level toward it: the
directory on screen's child that holds it, however deep the tile is
drawn, with the tile still selected so the next click opens the next
level. One double- or triple-click never opens more than one level. The
pointer turns into a hand over a selection a click would open, and the
tile's hover card names the directory a click or a double-click opens.

A marked tile takes the danger colour, and so does everything inside it:
removing a directory takes its contents with it. Marking a directory absorbs
any marks already inside it, and something inside a marked directory cannot be
marked or kept on its own; its panel offers to unmark the directory instead.
Marking is reversible — press it again — and the saving is never counted twice.

## Zooming and going in

Scroll to magnify toward the pointer. The wheel magnifies until the directory
under the pointer fills the view, and the next notch goes into it — one
continuous motion, with the directory's contents growing into place. Scroll the
other way to come back out. Enter goes into the selected directory at any
depth, and Backspace or Escape goes up one level. `<` and `>`, beside the
Size / Files / Age switch, go back and forward through the directories visited,
as do `alt ←` `alt →` (also `⌘[` `⌘]` on macOS) and the mouse's side
buttons. `+` and `-` magnify without going in; `0` resets.

## Removing

`c` (or **Review…**) opens the list of everything marked. Unmark anything
there, then choose:

- **Move to trash** — the default when a trash is available. On macOS that is
  the system Trash, the same move as Finder's (on another disk, that disk's
  own Trash; network shares often have none). Elsewhere it is `trash-put`
  from trash-cli, then `gio trash`, then a built-in XDG trash. Recoverable
  until the trash is emptied, so it commits directly.
- **Delete permanently** — `rm -rf` semantics. It always asks first, in a dialog
  that names what goes and how much comes back.

The commit is the footer's right-hand button; beside it, **Move & rescan**
(**Delete & rescan…** in permanent mode, `shift enter`) does the same and
then scans again. **Back** is beside them; **Export** and **Unmark all** sit
on the left. While a removal runs, **Stop after this item** (or `esc`) ends
it once the item in hand is done.

Without a rescan, the tree stays as the scan found it, so the next batch
can be marked straight away; a rescan of a whole disk takes minutes. What
was removed stays where it was, greyed out, with its hover card saying
**Removed since the scan**, and cannot be marked again. Every size on
screen is still the scan's; a directory marked around something already
removed counts only what is left in it. The next scan clears all of it.

The Done screen lists every removal since the last scan three ways:
**reported**, the listed size of what went, a clone counted in every place
it appears; **private**, what the scan saw as theirs alone; and
**observed**, the volume's free space after minus before, which other
programs change too. With more than one removal, a total row adds the
first two and measures the third from the first removal's start to the
last one's end. The Disk section keeps the total as **Freed since scan**
until the next scan.

## Keys

| key | does |
| --- | --- |
| `space` / `x` | mark or unmark the tile you point at |
| `ctrl`-click (`⌘`-click on macOS) | mark without moving the selection |
| click, then click again (or double-click) | select a tile, then open one level toward it |
| `enter` | open that directory, at any depth |
| `⌫` / `esc` | go up one directory |
| `alt ←` `alt →` | back and forward through where you have been |
| `←` `↑` `↓` `→` | move between tiles at this level |
| `tab` / `shift-tab` | next / previous control; Escape returns to files |
| `n` / `shift-n` | next / previous largest sibling |
| scroll | zoom toward a directory, then go into it |
| `shift`-scroll | pan the magnified view |
| `[` `]` | draw fewer or more levels at once |
| `-` `=` `0` | magnify, shrink, reset the view |
| `ctrl =` `ctrl -` `ctrl 0` (`⌘` on macOS) | interface zoom, 75–200%, shared by all windows |
| `/` | filter by name: only matches keep their colour; `enter` shows only them, `esc` clears |
| click a legend entry | show only that kind or age, then hide it, then show it again; `esc` clears |
| `c` | review the marked list |
| `t` | size, files or age: what areas and colours say |
| `ctrl 1` `ctrl 2` (`⌘` on macOS) | treemap or file list; the list is read by screen readers |
| `d` | disk usage or apparent size |
| `i` | include or skip hidden entries |
| `r` | scan again |
| `esc` while scanning | stop the scan |
| `v` | choose a local/SSH source and root |
| `ctrl o` (`⌘O` on macOS) | choose another directory to scan |
| `g` | the whole disk |
| `p` | show or hide the selection line |
| `o` | show it in Finder, File Explorer or the file manager |
| right-click | show that tile in Finder, File Explorer or the file manager |
| `shift u` | updates: the version, the release channel, check now |
| `?` | every key |
| `q` | quit, on Linux; ⌘Q on macOS and Alt+F4 on Windows |

On macOS the menu bar also has ⌘⇧R to show the selection in Finder, ⌘R to
rescan, ⌘[ and ⌘] for back and forward, and ⌘Q, ⌘H and ⌘W (closing the
window quits); other ⌘ chords are left to the system. On Linux and Windows
the same work with ctrl, with F5 to rescan too.

On the review screen: `m` trash, `p` permanent, `!` or **Unmark all**
clears the list, `u` or Edit › Undo (⌘Z, Ctrl+Z) puts it back, `enter`
commits, `esc` goes back. Or hand the list on instead of acting on it from
**Export**: `s` saves it as a text file, one path per line, `e` and `y`
save or copy a delete script, and `a` copies a prompt for a
coding agent: free the space by removing what you picked, checking each path
first (git work that exists nowhere else, a tool's own clean command) and
touching nothing else. A name holding a newline is left out of the list and
escaped in the prompt, so it cannot pass for another path.

Help keeps its heading and Close button visible while the shortcut list
scrolls. Use arrows, PageUp/PageDown and Home/End to read it. Settings panes
scroll with PageUp/PageDown and Home/End; Ctrl+PageUp/PageDown still switches
panes. Documents wrap to the window and scale. Logs uses PageUp/PageDown to
scroll within long wrapped lines, while Up/Down selects lines for copying.

## What it measures

- **Disk usage** by default: `st_blocks × 512`, the number `du` reports and the
  space that actually comes back when a file is deleted. Apparent size (what
  `ls -l` shows) is one toggle away.
- **Hardlinks once.** Two names for one inode cost one file.
- **Hidden entries included**, because `~/.cache` is often the biggest thing in
  a home directory. Symlinks are not followed.
- **The projection counts one volume.** The meter shows the scanned volume's
  free space, so it adds up only the marks whose bytes come back to that
  volume, by the same rule the scan bounds the volume with. With `-X`, a mark
  on a disk mounted inside the tree, on a network share or in a snapshot
  subvolume is left out of the projection — removing it gives its space back
  somewhere else, or, in a snapshot, shares its blocks with the live files —
  and the panel and the review screen say what was left out. A mark whose
  volume cannot be told is not claimed either.

The scan follows [dust](https://github.com/bootandy/dust)'s approach: one rayon
scope per root, a completion counter per directory so no directory is built
before its last subdirectory lands, and one bottom-up pass that aggregates sizes
and removes duplicate hardlinks.

## The whole disk

Click `/` (or any directory above the scanned root) in the trail, press
`g`, run `pwrdisk --disk`, or use the launcher's *Scan the whole disk*
action. `g` and `--disk` scan the disk your home directory lives on — `/`
on Omarchy and on macOS. On macOS the Data volume's second mount,
`/System/Volumes/Data`, exposes only its private `.fseventsd` and
`.Spotlight-V100` stores: `/Users`, `/Applications` and the rest would be
the same files again under other names.

Widening is memoized: the tree already measured is handed to the wider walk
and reused where it is reached, so going from `~` to `/` reads only what is
outside `~` (on this machine, seconds instead of a full rescan). The current
view stays on screen until the wider tree lands, which then opens with the
directory you came from selected. Going back down is just navigation.

By default a scan stops at **every nested mount point**, including bind
mounts and same-device btrfs subvolumes. Boundaries are checked by path,
before metadata or directory reads, so an automounted NAS is not activated
just to measure its parent. Choose `/home`, `/var/log`, or another mount as
its own root rather than implicitly traversing it from `/`.

Local `-X` explicitly crosses mounts; remote streaming scans reject it.
A local mark on another volume is still removed, but is not added to the
metered volume's free-space projection.

Without root, some system directories cannot be read; they are counted as
unreadable in the top bar rather than guessed at.

## Servers over SSH (read-only)

```sh
pwrdisk ssh dev                 # connect and choose a remote root
pwrdisk ssh user@server /var    # an explicit remote path
pwrdisk ssh dev --disk          # the disk containing the remote home
pwrdisk ssh dev -a /srv         # apparent size instead of allocated blocks
```

Linux builds made with `make build` / `cargo xtask portable` include static
**x86-64 and ARM64 Linux scanners appended to the executable**. SSH detects
the server's OS/CPU and uploads the matching scanner only when its checked
cache is missing or damaged. The cache is private and content-addressed under
`~/.cache/disktree/ssh/<sha256>/` (or the server's `XDG_CACHE_HOME`).
Uploads are checksum-verified and atomically installed; there is no sudo,
system-package change or dependency on the server's glibc version.

This uses stable Rust's musl targets rather than Cosmopolitan: the existing
Rust/rayon/rustix scanner has no supported stable Cosmopolitan target. These
are Linux binaries, not a single executable for every OS; ARM64 is built and
ELF-validated, while x86-64 was also executed against a real server.

You can extract a helper without opening a window, or test the entire
SSH/deployment/scan path headlessly:

```sh
pwrdisk extract-scanner x86_64 ./disktree-scan
pwrdisk extract-scanner aarch64 ./disktree-scan-arm64
pwrdisk ssh dev --check /var/log
```

The appended manifest stores architecture, byte ranges and SHA-256 hashes.
Do not run `strip` after packaging: it discards the trailer. The Linux release
pipeline strips first, embeds last, and ships that single file. Appending to
signed macOS/Windows executables needs platform-specific signature handling;
those builds do not yet embed helpers.

For a development or non-Linux desktop build without the trailer, install
the small **disktree-scan** helper on the server first:

```sh
# On the server, inside the checkout:
make install-scan               # ~/.local/bin/disktree-scan; no GUI deps
# Or:
cargo install --path crates/disktree-scan --locked
```

The helper can be on the server's **non-interactive SSH PATH** or at
`~/.local/bin/disktree-scan` (the default install location); PwrDisk tries
both. A binary copied from another machine must match the server's OS,
architecture and runtime. The manually built native helper may need the
server's libc; the embedded Linux helpers are static. PwrDisk never runs
sudo or changes the server's configuration.
SSH keeps your normal authentication settings. When launched from a
terminal, it can ask for a password or key passphrase there; GUI launches
need key/agent access or a working SSH askpass helper. Connect once with
`ssh dev` to verify access and accept its host key.

The protocol also supports lightweight root discovery, separate from a scan:
it reads the target's mount namespace without walking or probing the listed
paths. Same-device mounts remain distinct choices; capacity is unknown
until a root is explicitly selected. Discovery requests omit scan fields,
so outdated helpers reject them instead of silently scanning their home.

The Linux/macOS helper streams a depth-first walk directly to stdout. It
never constructs a tree or retains a directory's children: memory scales
with open ancestors, not file count. Deep directories are not rejected
at an arbitrary depth; filesystem errors are reported as unreadable.
Hardlink identities are sent to the desktop, which uses the local scanner's
compact identity index to count each inode once. The helper keeps no
per-file identity table in negotiated desktop-de-duplication mode; links
outside the chosen root cannot fill one. Older clients retain bounded
helper-side accounting for wire compatibility. A resource limit fails
explicitly instead of returning a partial tree. Symlinks are not followed,
and every nested mount is skipped, even a bind mount or same-device
subvolume. Select those as separate roots.

While indexing, the sidebar shows a progress bar and estimated percentage
for whole-volume scans, using measured file bytes versus the drive's used
space at scan start. Shared blocks, snapshots and unreadable paths affect
this estimate; it stays below 100% while scanning and does not predict
time remaining. Folder, filtered, apparent-size and cross-volume scans
have no known total. Live file and directory counts, measured bytes, read
errors, elapsed time and average files per second remain visible. After
enumeration, the status changes to **Building treemap** while totals and
categories finish. Counters and paths use monospace type in fixed lanes;
changing values do not re-center or reflow the screen. Narrow windows use
a compact progress footer instead.

Ordinary local walks and current SSH helpers paint a **live top-level
overview** before the full tree arrives. These are measured contributions,
not complete directory sizes. Updates are limited to once a second, in
arrival order, with at most 128 named buckets plus an overflow bucket. The
preview is kept separate from the real tree: it cannot be navigated,
marked or removed. On completion the full, actionable treemap replaces it;
on error or cancellation the preview is discarded. The Windows file-table
fast path still waits for its complete tree. Remote scans also show a
sampled recent directory and connection / helper setup / indexing / treemap
stages. Progress uses a single replaceable snapshot, not a growing event
queue; path updates are throttled to five per second. There is no guessed
percentage: the total is unknown until the walk finishes. Older helpers
still provide counters from their streamed records, without path samples.

One compressed SSH connection carries a bounded JSON request on stdin and
compact postorder records on stdout. The client keeps stdin open so the
helper exits on disconnect, including while scanning or blocked on output.
Linux helpers also enforce a 1 GiB address-space ceiling, disable core dumps,
and stop after five minutes. File contents never cross the connection and
paths are never interpolated into remote shell commands. Only the desktop
retains the complete tree for offline navigation; its memory still scales
with entry count. The native Windows helper currently retains the original
tree-building scan path.

Remote mode is **export-only**. Mark with Space, open review with `c`, then:

- **Copy as prompt** (`a`): includes the SSH host, remote OS, scan root,
  selected paths, sizes and cleanup precautions. It requires verifying the
  actual scanned hostname and machine ID before any destructive operation,
  and stopping if either check fails or cannot be performed.
- **Save list…** (`s`): remote paths, one per line, saved locally.
- **Copy delete script** (`y`), or **Save delete script…** (`e`): a
  GNU/Linux shell script, copied to the clipboard or saved locally.
  Running it without arguments only previews the selection. After reviewing
  it, `sh pwrdisk-delete.sh --delete` connects to the named SSH host and
  runs the exported commands there, never against the local paths.

There are no trash/delete buttons or removal-mode choices in remote review.
Deletion shortcuts and removal entry points are disabled too. Git inspection,
local folder selection and file-manager reveal are disabled so remote names
cannot accidentally act on this computer. Free space is sampled when the
scan starts, **not live**; press `r` to refresh.

Before deleting, scripts compare the server's actual kernel hostname and
machine ID against the identity captured in the scan—not the SSH alias.
Missing identity or any mismatch aborts. The check runs on the SSH target,
not the computer launching the wrapper. Pasting the script previews it by
default and runs in a subshell, so its exit cannot close your terminal.

Exports are not an automatic cleanup recommendation. Scripts refuse system
paths, the scan root, home or its ancestors, symlinked/changed canonical paths,
and paths containing mount points. Control-character names are omitted.
These checks do not eliminate filesystem races or tell you whether git work
is backed up; review the script and prefer a tool's own cleanup command for
tool-owned data. The remote helper itself has no delete operation.

## With an AI agent

PwrDisk is also an [MCP](https://modelcontextprotocol.io) server, so an
agent such as Claude Code or Codex can do the looking while you keep the
deciding. One command connects it, and there is nothing to configure, sign
in to or approve: the agent runs `pwrdisk mcp` as you, over stdio, and can
do no more than your shell can.

```sh
claude mcp add --scope user pwrdisk -- pwrdisk mcp
```

(Codex: `codex mcp add pwrdisk -- pwrdisk mcp`. Any other client: a
server named `pwrdisk` with the command `pwrdisk` and the argument `mcp`.
On macOS the command lives inside the app, at
`PwrDisk.app/Contents/MacOS/pwrdisk`. The window's **Work with an AI
agent** line, under the disk, and **Help › Work with an AI Agent…** on
macOS, show each command with the binary's full path and a Copy button.)

Then ask it what is eating your disk. The agent walks the tree largest
first, lists every cache, build directory and package store at once, and
proposes what can go. It marks what you agree to, and marking is all it
does by default. `pwrdisk_review` opens the marked list for you to approve;
the agent must never approve it through computer use. For requested MCP
removal, `pwrdisk_removal_preview` returns the exact paths and an explicit
Trash or permanent mode. After you confirm that operation, the destructive
`pwrdisk_remove` tool requires interactive confirmation through the MCP
client. Clients without form elicitation use human review instead. Trash
may be recoverable until emptied; permanent deletion cannot be undone
through PwrDisk. Settings > Safety sets the per-operation limit: 5% of each
volume's capacity by default, for both UI and MCP removal. The app rechecks
the guards and re-measures the complete selection before removing anything.

When a PwrDisk window is open, its tree is what the agent reads. `ls`,
`find` and `scan` are answered from the mosaic on screen, a mark is
measured from it, and the two of you see the same numbers. The agent's
marks appear in the mosaic and in the free-space meter as it makes them,
`ls` and `find` never navigate; `show` moves the view only when you ask, a scan it asks for runs in
the window, and the header names the agent and its last action while it is
connected. With no window open the agent scans on its own, in seconds for a
home directory, and can open a window for the review. Without a display it
works headless and says so. A window showing an SSH server is not followed:
the agent's tools measure this computer, and PwrDisk does not remove files
over SSH.

`pwrdisk_remove` does not rescan unless asked (`rescan: true`), so the
agent can find and mark the next batch at once. Removed paths are still
listed by `pwrdisk_ls` and `pwrdisk_find`, with the scan's sizes, until a
rescan; marking one is refused. `pwrdisk_status` lists every removal since the scan,
reported, private and observed, with their total.

The tools: `pwrdisk_status`, `pwrdisk_scan`, `pwrdisk_ls`,
`pwrdisk_find`, `pwrdisk_show`, `pwrdisk_mark`, `pwrdisk_unmark` and
`pwrdisk_review`, `pwrdisk_removal_preview` and `pwrdisk_remove`. Each `mark` answers with the real plan: what the guards
below keep back and why, what another mark already covers, and the free
space it gives back on the volume, counted the way the meter counts it.

The server carries its own skills, so the agent knows how to use the tools
and what to be careful of before it starts:

| skill | for |
| --- | --- |
| `pwrdisk-explain-usage` | where the space went, read-only: never marks anything |
| `pwrdisk-free-space` | freeing space: agree the list with you first, unless you gave a standing instruction, then mark and hand over the review |
| `pwrdisk-free-space/references/` | macOS, Linux and Windows: what each hides from a scan (snapshots, clones, cloud placeholders), what PwrDisk refuses, and what never to propose |

They are served as `skill://` resources, through the skills extension's
`skills/list`, and as the prompts `free-space` and `explain-usage` (in
Claude Code, `/mcp__pwrdisk__free-space`), which add the reference for the
system the server runs on. The files are in `skills/`.

The window listens on an endpoint only you can reach: a Unix socket in
`$XDG_RUNTIME_DIR` or `$TMPDIR`, or a local Windows named pipe,
`\\.\pipe\pwrdisk-<user SID>`. The pipe grants access only to your Windows
account and rejects remote clients. The agent verifies the connected pipe
is owned by your account before exchanging messages. Set `PWRDISK_LINK` to
choose another socket path or local pipe name. On macOS, Linux and Windows,
the agent's marks appear in the open window and `pwrdisk_review` opens its
review screen.

## Updates

A copy installed from the DMG, Setup.exe or the MSI checks the project's
GitHub releases when it starts and every six hours, and downloads a newer
release in the background. The download installs when PwrDisk quits, or at
once from **Restart to update**. Nothing is sent but the request for the
release list.

**Check for Updates…** (in the PwrDisk menu on macOS, the strip's menu
elsewhere) checks at once and answers in a toast at the bottom left of the
main window: up to date, an update ready to install with **Restart to
update**, or why the check failed. A check on its own says nothing unless it
has downloaded an update, and then says so once for each version.

**Shift-U** opens the Updates panel: the version, what the last check found,
**Check now**, and the release channel:

| Channel | Gets |
| --- | --- |
| Stable · Latest | Releases promoted after a check. The default. |
| Stable · Prerelease | Candidates for the next stable release. |
| Beta · Latest | Beta builds (`-beta` tags). |
| Beta · Prerelease | The newest alpha (`-alpha` tags); may not work. |

Until you pick one, a copy follows the channel its own version implies, so
a beta keeps getting betas. Picking an older line downloads its release on
**Check now**, and it installs only from **Restart to switch**.

Copies that did not come from an installer — the Linux tarball, `make
install`, `cargo build` — do not update themselves; download the next
release.

## On macOS

To see privacy-protected files, give PwrDisk **Full Disk Access** in System
Settings › Privacy & Security (the panel offers a button when it is missing), then
reopen it. Without it macOS hides Mail, Messages, Safari, other apps' data
and the Trash, and PwrDisk counts them as unreadable. Started from a
terminal, it is the terminal that needs the access. macOS also asks once
each for Desktop, Documents and Downloads.

Full Disk Access does not override filesystem ownership and permissions.
Administrator-owned service stores can still be unreadable. Unknown contents
are never reported as zero bytes or as absent software. To inspect a store
explicitly, include hidden entries, remove any scan depth limit, and use the
directory chooser to scan it directly. Read access must be arranged outside
PwrDisk; the app does not request elevation or install a privileged helper.

What is different from Linux:

- **Free space** is what `df` reports. Finder's figure is larger: it counts
  purgeable space (caches and local snapshots macOS will clear on its own).
- **Cloned files** (copies APFS shares blocks between, as Finder's Duplicate
  makes) remain counted in full in the treemap. The scan line separately
  reports bytes in cloned files and estimated repeated data-fork allocation.
  The selection panel and a tile's hover card split the listed bytes into
  private and shared: shared bytes are clone blocks another clone, inside
  the scan or outside it, may still hold. In `pwrdisk mcp`, every
  `pwrdisk_scan`, `pwrdisk_ls` and `pwrdisk_find` row with clone blocks
  reads `0 B private · 11 MiB shared with other APFS clones · frees ≈ 0 B
  alone`, and the scan and listing headers say how much is counted again
  and what the files take on disk. Rows without clones, and every row off
  APFS or in an apparent-size scan, read as before.
  The panel's marked total, the review and the permanent-delete
  confirmation estimate savings from the accepted set of marks together;
  the selection's own line says what that entry frees removed on its own,
  which for one of several clones can be far less. Complete clone groups credit
  shared data once; a remaining hardlink retains its file data. Unknown or
  modified sharing and allocation outside the data fork supply a range,
  with only its lower bound used by the volume meter. Unknown private sizes
  contribute zero to that bound. Snapshots and open files can retain blocks,
  and changing files can stale the estimate; rescan to refresh it. The final
  result still measures the volume's actual free-space change.
  Clone hints come from bulk directory reads; only potentially shared files
  need a private-size query. A bounded, sharded cache reuses confirmed full
  clones within their observed reference count, separately on each real
  APFS volume and each scan. Modified clones are measured individually.
  Unsupported filesystems or kernels fall back to
  the ordinary listing without clone detection. Apparent-size scans do not
  collect clone metadata.
- **Time Machine's local snapshots** are not files and do not appear; they
  are part of the gap between the scan and the disk's used space.
- **Cloud-only folders** (iCloud Drive, Dropbox and the like, evicted to the
  server) are not opened, so a scan never downloads them.

### FSEvents history and Spotlight indexes

Local macOS scans categorize **FSEvents history** (`.fseventsd`) and
**Spotlight index** (`.Spotlight-V100`) when their files are readable. The
**macOS service stores** panel appears only when the scan measures at least
one store. If both are unreadable or outside the scan, it stays hidden and
adds no permission request or Full Disk Access hint. Other unreadable files
can still produce the existing privacy hint.

When one store is measurable, the other retains an honest **Unavailable** or
**Outside scan** label; partial results say **measured · incomplete**. Select
a service tile for Apple's guidance. Access is determined from the scan,
rather than assuming Full Disk Access overrides ownership permissions.
On a startup disk the stores live in `/System/Volumes/Data`; external volumes
have their own stores. Home-directory and SSH scans do not show this panel.

The figures are allocated file bytes (`st_blocks × 512`), unless apparent size
is selected, and are already part of the scan total. They are not additional
APFS volume, container or snapshot figures. Hardlinks are counted once across
the tree; shared APFS clone blocks and snapshots mean measured bytes are not
a promise of space freed. The rest of the second Data mount stays excluded,
so firmlinked user files are not counted again.

FSEvents stores persistent filesystem change history that backup and sync
apps can use. At an administrator's explicit discretion, Apple's supported,
root-only [`FSEventsPurgeEventsForDeviceUpToEventId` API](https://developer.apple.com/documentation/coreservices/1447985-fseventspurgeeventsfordeviceupto)
can shorten old history. It destroys history for **every consumer on that
volume**, and those consumers may need a full rescan. Apple's
[FSEvents guide](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/UsingtheFSEventsFramework/UsingtheFSEventsFramework.html)
advises against routine purging and requires an explicit administrator request
and confirmation. PwrDisk only explains this option; it does not call it or
suggest raw deletion of `.fseventsd`.

An administrator implementing a chosen retention cutoff would resolve the
volume's current `st_dev`, obtain an event ID with
`FSEventsGetLastEventIdForDeviceBeforeTime`, and only then use the purge API.
The installed macOS SDK's `FSEvents.h` documents this function's time argument
as **seconds since January 1, 1970**, despite its `CFAbsoluteTime` type. The
archived guide describes the usual 2001 epoch instead; check the target SDK
contract rather than applying that conversion. No retention cutoff or space
saving is guaranteed by PwrDisk.

Spotlight's index supports file and content search. Keep indexing for that
functionality, exclude selected folders through
[Spotlight Search Privacy](https://support.apple.com/guide/mac-help/prevent-spotlight-searches-in-files-mchlp2811/mac),
or have an administrator choose whether a volume should be indexed. The
installed `mdutil(1)` manual describes `-s` as a read-only status query,
`-i on|off` as the indexing choice, and `-X` as removing the index directory:
**`-X` does not itself disable indexing**. An erased index may be rebuilt when
indexing is enabled, consuming space and work again; disabling indexing reduces
search coverage. A failed status query is unknown, not evidence of disabled

## On Windows

The same program, with Windows' answers to the questions above:

- **Disk usage** is the allocation NTFS reports for each file: whole
  clusters, less for a compressed or sparse file, nothing for one small
  enough to live in its file record. It arrives with the directory listing
  itself (`FileIdExtdDirectoryInfo`), so it costs no more than the walk.
  Hardlinks count once on NTFS.
- **The whole disk** is the drive your profile is on, usually `C:\`. A
  folder another volume is mounted on is a link, like a junction, and is
  not entered, so a scan stays on one volume; `-l` follows links, and with
  them mounted folders.
- **Run as administrator** and the whole disk is read from NTFS's master
  file table instead of walked, as WizTree does: one pass over the table in
  large reads. On a 4-million-file `C:\` that took about 3.4 s against 11.2
  s for WizTree and about 20 s for the walk. It also sees what the walk is
  refused, such as System Volume Information. It is used only for a whole
  NTFS drive, since a folder would still cost the whole table, and not with
  `-l`, whose links the table does not follow. PwrDisk flushes the volume
  after its own removals so the rescan shows them; changes other programs
  made seconds before may not show yet.
  Started without admin rights, PwrDisk walks as before. On a whole NTFS
  drive without `-l`, or once Windows refuses the walk a folder, the side
  panel offers **Restart as Administrator**, which reopens the same folder
  and options through the UAC prompt; during a widening scan, the wider
  folder being scanned.
- **Move to trash** is the Recycle Bin, through the shell, which asks
  before destroying anything it cannot recycle.
- **Refused besides the rules below:** Windows, Program Files and
  ProgramData, what Windows keeps at the top of its drive (System Volume
  Information, Recovery, Boot, and the page and hibernation files, which
  Settings turns off), any folder holding your profile, such as
  `C:\Users`, and every profile in the folder Windows keeps them in
  (`C:\Users\*`, `Public` and `Default` included), since an elevated
  PwrDisk may run as another account and can reach them all. What is
  inside a profile can still be removed, as under your own. Names compare
  without regard to case, as Windows compares them.
- **Hidden** means a name starting with a dot, or the hidden attribute, so
  `-H` drops `AppData` as Explorer hides it.
- **The theme** follows Windows' light or dark setting, since there is no
  Omarchy theme to follow. It does on macOS too, and on GNOME and KDE.

## What it refuses to do

The removal rules live in `crates/disktree-core/src/removal.rs`, and each one is
tested:

- only paths under the scanned root can be removed;
- the filesystem root, the scanned root and your home directory are refused;
- a mount point is refused, and so is anything with a mount point inside it,
  since removing it would reach into another filesystem; permanent deletion
  also stops at a mount boundary rather than descending into one (a btrfs
  subvolume that is not mounted goes with its directory, as the scan shows
  it);
- a directory holding your home directory or a system tree is refused (on
  macOS `/Users` is on the same volume as `/`, and `/opt` holds
  `/opt/homebrew`);
- system trees (`/usr`, `/etc`, `/boot`, `/var/lib`, `/nix/store`,
  `/gnu/store`, Homebrew's prefix on macOS and Linux, …) are
  refused even where permissions would allow it: packages own them, and
  pacman, paccache or `journalctl --vacuum` are the tools;
- a symlink is unlinked, never followed;
- nothing is passed through a shell — a file called `-rf` is just a file;
- selecting a checkout never runs a program it names: git is asked with its
  fsmonitor, hooks and pager off, and a checkout that defines its own filter
  drivers is not asked for its status at all ("changes unknown").

## On Hyprland

Hyprland tiles new windows, so PwrDisk opens into whatever tile it is given.
It is designed for a roomy window; float it, or give it a rule. The window
class is `pwrdisk`.

```
windowrule = float, class:^(pwrdisk)$
windowrule = size 1400 900, class:^(pwrdisk)$
```
