# PwrDisk on Linux

What Linux keeps outside a file scan, what PwrDisk refuses, and what to
leave alone. Read with `skill://pwrdisk-free-space/SKILL.md`.

## Reading the numbers

- **Hardlinks are counted once.** pnpm stores, Nix, ccache and backup tools
  hardlink files. Removing one name frees nothing until the last name goes;
  a hardlinked tree can give back far less than its size.
- **Snapshots hold deleted data.** On btrfs (snapper, Timeshift, btrbk) and
  ZFS, a snapshot keeps every block it references. Space freed while one
  exists may not come back. `btrfs subvolume list /`, `snapper list` and
  `zfs list -t snapshot` show them; removing one is the user's decision.
- **btrfs reports space its own way.** `df` can disagree with the sum of the
  files; `btrfs filesystem usage /` gives the real allocation.
- **Deleted files still open** keep their space until the process closes
  them. `lsof +L1` lists them; restarting that process frees it.
- **Swap files and the journal**: `swapon --show` and
  `journalctl --disk-usage` show them. Shrinking either is a system change
  (`journalctl --vacuum-size=...` needs root); give the command, do not run
  it.
- **Scans stop at other filesystems.** Mounts, bind mounts and btrfs
  subvolumes under the root are not counted unless the user asks for them.

## Root discovery

The root chooser omits mounts at or below `/proc`, `/sys` and `/dev`,
plus kernel pseudo filesystems including `binfmt_misc`, `bpf`, `efivarfs`
and `ramfs`. Discovery reads the mount table without probing candidates
or measuring their free space; an explicit scan root is still required.

## Access

`pwrdisk mcp` runs as the user. Root-owned directories show as unreadable;
do not use `sudo` to look inside them. Say what was unreadable and what
would read it.

## Refused by PwrDisk

`pwrdisk_mark` keeps these back and says why: `/bin`, `/boot`, `/dev`,
`/etc`, `/lib*`, `/proc`, `/run` (except removable media under it), `/sbin`,
`/sys`, `/usr`, `/var/lib`, `/efi`, `/nix/store`, `/gnu/store`, Homebrew's
`/home/linuxbrew/.linuxbrew`, the home directory and anything holding it,
and mount points.

## Never propose

- **Anything a package manager installed.** Uninstall with `pacman`,
  `apt`, `dnf`, `flatpak` or `snap`, which the user runs.
- **`~/.var/app/<app>`, `~/.local/share/<app>`, `~/.config`**: application
  data and settings. Only a specific, known cache inside them, with the app
  closed.
- **Keyrings, `~/.ssh`, `~/.gnupg`, password stores**: secrets.
- **Container storage**: `~/.local/share/containers`, `/var/lib/docker`.
  Use `docker system df` / `podman system df`, then the prune commands.
- **Virtual machine disks** (`*.qcow2`, `*.img` under libvirt or GNOME
  Boxes): user data; ask, and remove the VM in its manager instead.
- **Synced folders** (Nextcloud, Dropbox, Syncthing, rclone mounts):
  deleting there deletes on every device.

## Usually safe, with the owning tool first

| what | where | prefer |
| --- | --- | --- |
| user caches | `~/.cache/<app>` | close the app; they regrow |
| pacman packages | `/var/cache/pacman/pkg` | `paccache -dk1`, then `paccache -rk1` (root) |
| apt and dnf packages | `/var/cache/apt`, `/var/cache/dnf` | `apt clean`, `dnf clean all` (root) |
| unused Flatpak runtimes | `/var/lib/flatpak`, `~/.local/share/flatpak` | `flatpak uninstall --unused` |
| language caches | `~/.npm`, `~/.cargo/registry`, `~/.cache/pip`, `~/.gradle/caches` | the tool's clean command |
| pnpm store | `~/.local/share/pnpm/store` | `pnpm store prune` |
| build output | `target/`, `node_modules/`, `build/`, `dist/` | check the checkout first (see the skill) |
| Steam games | `~/.local/share/Steam/steamapps` | uninstall in Steam |

## Trash

PwrDisk moves marks with `trash-put`, then `gio trash`, then its own XDG
trash in `~/.local/share/Trash`; the review screen names the one it uses.
`trash-put` and `gio` put a mark on another filesystem in that
filesystem's `.Trash-<uid>`. The space returns when the user empties the
trash. With no trash at all, the review screen deletes permanently and says
so; tell the user before they confirm.

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
