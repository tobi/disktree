# Owner-managed cache cleanup

A cache name, category or `regenerable` hatch is an inspection lead, not
permission to recursively delete its contents. Review steers recognized
caches to **Owner cleanup**. Raw removal refuses recognized cache paths,
cache wrappers and opaque cache tags, including caches inside a selected
parent. Unsupported and unknown caches stay inspection candidates. A failed
owner command never falls back to Trash or permanent deletion.

## Initial allowlist

Commands below are argument lists, executed directly with an absolute
resolved executable. `ROOT` is the owner's queried configured root; it is
never inferred solely from a directory name. PwrDisk does not use `npx`,
`uvx`, a package download, a project script or a shell to find a missing CLI.

| Owner | Accepted stable versions | Query | Cleanup and actual scope |
| --- | --- | --- | --- |
| npm packages | 9–11 | `npm config get cache` | `npm cache clean --force --cache ROOT`; entire `ROOT/_cacache` only |
| npm npx | 11.2–11.x | `npm config get cache` | `npm cache npx rm --force --cache ROOT`; entire `ROOT/_npx` only |
| uv | 0.11.x | `uv cache dir` | `uv cache clean --cache-dir ROOT`; whole cache, using uv's exclusive cache lock, without `--force` |
| pip | 23–26 | `pip cache dir` | `pip --cache-dir ROOT cache purge`; HTTP and wheel entries in that cache; other files can remain |
| Cypress | 13–15 | `cypress cache path` | `CYPRESS_CACHE_FOLDER=ROOT cypress cache clear`; ALL cached binary versions, including ones other projects use |

The version is queried first. Unknown output, prereleases, unsupported
versions, non-executable launchers and failed configuration queries refuse
cleanup. A command missing from PwrDisk's inherited PATH means unavailable,
not that the software is uninstalled. Relative PATH entries are ignored. pip resolution also accepts `pip3` when
`pip` is unavailable.
Known version-manager shims (including shimmed `env node` interpreters) are
refused before version probing, because a probe can otherwise install a
missing runtime. Expose the actual installed CLI/runtime directly on PATH.
npm must resolve to its installed `npm-cli.js`; downloading proxy launchers
such as Corepack are unsupported.
Relative PATH entries are removed from the captured execution environment;
non-UTF-8 environments fail closed instead of silently losing configuration.
Only macOS and Linux have execution adapters; Windows cache recognition
remains available, but shell-based Windows launchers are unsupported.

Queries run from the user's home with the app's environment, so user config
and environment overrides apply. Project-only configuration must be made
available explicitly to that environment; PwrDisk never searches projects or
evaluates Puppeteer JavaScript config. A stale/default selection that does
not match the queried root is refused, rather than redirected to a different
cache. Selecting a subtree expands to the owner's complete scope. Selecting
the npm wrapper previews its supported sibling caches separately.

| Inspection-only owner | Why automatic cleanup is unavailable |
| --- | --- |
| pnpm | `store prune` can also affect metadata, dlx/runtime caches and global virtual-store state outside a selected store; these scopes are not modeled here |
| Homebrew | `brew cleanup`, including `--prune=all`, also removes old installed formula versions and stale locks outside the selected cache |
| Playwright | `uninstall` manages browser installations and package references; no adapter models that wider scope |
| Puppeteer | The browser API and interactive `browsers clear --path` exist; resolving the correct installed API/configuration and its confirmation is not implemented |
| codex-runtimes, gh, Electron, VS Code C++ and unknown caches | No verified owner adapter; inspect the owning installation and its supported lifecycle |

## Approval and execution

**Preview owner cleanup** resolves and measures on a worker without cleaning.
It shows owner, version, executable, argument boundaries, environment
overrides, working directory, selected paths, actual whole-cache scopes,
effects and limitations. Wider scope includes unselected siblings and needs
approval as such. Cache cleanup has no Trash recovery.

MCP uses `pwrdisk_removal_preview` with `mode: "owner_cleanup"`. All removal
modes prepare recursive cache inspection on a worker. The first request can
return a preparing message; retry only that read-only preview. Confirmation
uses the scan/mark-bound result and execution independently inspects again
on its worker before writes. Show
all operations to the user. `pwrdisk_remove` requires the exact ordered
**actual-scope paths**, preview ID, same mode and `RUN OWNER CLEANUP`, plus
the existing interactive client confirmation. Computer use cannot approve.
The preview is single-use, bound to the scan and marks, and expires within
five minutes. Changed/expired previews require new approval.

Exports remain limited to the marked paths, even when an owner preview has
expanded its execution scope. Lists and agent prompts never inherit that
wider scope. Save/copy raw deletion scripts refuse Owner cleanup mode and
recognized cache selections; exporting cannot substitute `rm -rf` for an
owner command. Tags and owner layouts on ancestors protect contained
subdirectories and files, including caches relocated through CLI settings.

The usual root/home/system/mount/symlink protections apply to the full
actual scopes. Scopes outside the scan, or equal to its root, are refused;
explicitly scan a suitable parent and preview again. External symlinks and
unreadable or excessively large preflight walks are refused. The default
5%-per-volume capacity limit uses full scope allocation, with a worker
remeasurement before any command starts. Owner version, configuration and
executable identity are rechecked. Active owner processes are checked before
execution; detection is best effort, so close owning tools first. uv's own
lock provides additional coordination; known `UV_LINK_MODE=symlink` is
refused because installed environments can depend on cache files.

Probes time out after ten seconds; cleanup after ten minutes. Stop cancels
the current process group. Failure/cancellation may leave partial cleanup;
inspect the result and obtain a fresh preview, never retry automatically.
npm logging/rotation and pip log writes are disabled for these invocations
to keep diagnostic files outside the approved scope untouched.

Success means the owner exited successfully; it does not mean its root or
all allocation disappeared. Per-command results report success/error,
scope allocation and measured volume availability before/after, including
negative changes. Other activity affects that observation. Open files,
hardlinks, APFS clones and snapshots can retain blocks. PwrDisk never alters
snapshots. Cleanup always rescans, including after partial failure, and
never marks the retained cache root as deleted.

## Upstream scope evidence

- [npm cache documentation](https://docs.npmjs.com/cli/cache/),
  [npm 11.2 release notes](https://github.com/npm/cli/releases/tag/v11.2.0),
  [npm 11.6.2 cache implementation](https://github.com/npm/cli/blob/v11.6.2/lib/commands/cache.js)
  and [log rotation](https://github.com/npm/cli/blob/v11.6.2/lib/utils/log-file.js).
- [uv cache documentation](https://docs.astral.sh/uv/concepts/cache/) and
  [0.11.0 cleanup lock handling](https://github.com/astral-sh/uv/blob/0.11.0/crates/uv/src/commands/cache_clean.rs).
  Older uv releases do not provide that same cache-wide coordination.
- [pip cache](https://pip.pypa.io/en/stable/cli/pip_cache/) and
  [Cypress cache location and clearing](https://docs.cypress.io/app/references/advanced-installation).
- [pnpm store scope](https://pnpm.io/cli/store),
  [Homebrew cleanup](https://docs.brew.sh/Manpage#cleanup-options-formulacask-),
  [Playwright uninstall](https://playwright.dev/docs/browsers#uninstall-browsers),
  [Puppeteer browser CLI implementation](https://github.com/puppeteer/puppeteer/blob/puppeteer-v24.26.1/packages/browsers/src/CLI.ts).
