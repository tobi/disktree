# Cache cleanup

A cache name, category or `regenerable` hatch is an inspection lead, not
permission to delete. Review steers marked caches to **Cache cleanup**,
where the person decides each marked path on its own:

* **Skip** (the default): nothing happens to it.
* **Run the owner's command**: the exact allowlisted command shown for it,
  for its owner's whole actual scope. Offered only when that command
  resolved; see the allowlist below.
* **Delete files**: delete exactly that selected path directly, without
  its owner, by Trash or permanently. A separate destructive intent with
  its own disclosure and its own consent.

Ordinary Trash and Permanent removal still refuse recognized cache paths,
cache wrappers and opaque cache tags, including caches inside a selected
parent: deleting a cache's files happens only as a Delete files decision.

## How this reconciles with the earlier guard policy

| Before | Now | What still holds |
| --- | --- | --- |
| One Owner cleanup mode for the whole list; a missing CLI, unsupported version or failed probe refused the entire preview | Check owners returns one proposal per marked path; a failure is that proposal's status | Preview is read-only, off the UI thread, with the same probes, timeouts and shim/proxy refusals |
| Approval meant running every command shown | Each path is decided; Skip is the default and nothing is chosen for the person | An agent cannot decide for the person |
| Raw deletion of a recognized cache was always refused | Allowed only as an explicit Delete files decision for that exact path, never the owner's wider scope | Removal guards, protected folders, the capacity limit and Review's over-limit confirmation, permissions, and an in-use check |
| "No raw deletion fallback" | Unchanged: a missing, unsupported, refused, failed or cancelled owner command deletes nothing else | Deleting those files instead needs a new review and its own decision |
| Unknown caches were refused | Unknown owner, "not known to be disposable"; Delete files is offered with a runtime/installation warning | PwrDisk never labels an unknown cache disposable; agents inspect first and never suggest Delete files |
| Exports: marked paths only; raw scripts refuse caches | Unchanged | Owner scopes never widen an export |

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
versions, non-executable launchers and failed configuration queries withhold
the owner command for that path; the proposal says which. A command missing from PwrDisk's inherited PATH means unavailable,
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
remains available, but shell-based Windows launchers are unsupported. Delete
files is not offered on Windows either: it is checked for programs using the
path right before it runs, and that check exists only on macOS and Linux, so
it would always be refused.

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

## Proposals

**Check owners** (MCP: `pwrdisk_removal_preview` with `mode:
"owner_cleanup"` and no decisions) resolves each marked path on a worker
without cleaning. Each proposal carries:

* the path, its size, and why it is treated as a cache;
* each resolved operation: owner and version, absolute executable,
  argument list with boundaries, environment overrides, working directory,
  configured cache root and actual scope, whether that scope is wider than
  the selection, effects, limitations and recoverability (none through
  PwrDisk or Trash);
* otherwise the problem, in one vocabulary used everywhere: not found on
  PATH, unsupported launcher, unsupported version (naming the versions that
  run), version check failed, configuration check failed, no cleanup
  command, unknown owner, not the configured cache, refused by guards;
* whether Delete files is offered and what it means: that the owner is not
  involved, any recognized caches inside the path (a bounded look, three
  levels deep), and for an unknown owner that PwrDisk does not know the
  path is disposable.

A path the removal guards refuse offers neither action. A command whose
whole scope fails the guards, crosses a volume, contains an external link,
overlaps another command's scope or cannot be measured is withdrawn from
the proposals that asked for it, with the reason.

## Decisions and confirmation

Decisions that would act on the same files twice are refused: a path left
on Skip, or given another action, may not lie inside a chosen command's
scope or a path being deleted. Choose the same action for both. Paths
chosen for the same owner scope run its command once.

In the window, Review shows each proposal under its mark with Skip, Run
and Delete files; Delete files by Trash or Permanently is chosen once for
the run (Trash by default where it exists). The confirmation lists each
command verbatim, each path deleted directly with its consequence, and
what is skipped. Deleting files has its own tick; the button stays
disabled without it, even when the commands alone could be confirmed.
Changing a decision clears the tick.

Over MCP the agent shows every proposal and asks the person per path, then
previews again with `decisions` (every marked path once: `skip`, `owner` or
`delete_files`) and `delete_files_mode` (`trash` or `permanent`) when any
path deletes files. Without decisions nothing can be confirmed. A decided
preview is single-use, bound to the scan, marks and decisions, expires
within five minutes, and has its own confirmation text naming each
intent: `RUN OWNER CLEANUP`, `DELETE CACHE FILES`, `TRASH CACHE FILES`, or
`RUN OWNER CLEANUP AND DELETE CACHE FILES` / `... AND TRASH CACHE FILES`.
`pwrdisk_remove` needs the preview ID, its exact ordered paths, mode
`owner_cleanup`, that text and the client's interactive confirmation.
Owner approval never confirms deleting files. The agent's decisions run;
the window's own choices never stand in for them. Computer use cannot
approve.

## Execution and results

The usual root/home/system/mount/symlink protections apply to every
step. The configured capacity limit (5% per volume by default) charges each
command's full scope and each deleted path; the worker re-measures first,
and only that ceiling refuses the whole run. A step that can no longer be
measured is refused on its own.

Steps run one at a time. Before each owner command the worker re-reads the
executable's identity and re-probes version and configured root against
the captured environment, rechecks the scope, and checks whether the owner
is running; before each deletion it checks for a process running from,
inside or naming the path, or for a recognized cache any running instance
of its owner. Detection is best effort; close owning tools first. uv's own
lock adds coordination; known `UV_LINK_MODE=symlink` is refused because
installed environments can depend on cache files.

A failed step reports why and the run moves on to the next. Probes time
out after ten seconds; commands after ten minutes. Stop kills the current
owner process group; that step reads cancelled, may be partly done, and the
rest read not reached. Nothing is retried automatically. npm logging and
rotation and pip log writes are disabled for these invocations so files
outside the approved scope stay untouched.

Every marked path gets a result: succeeded, failed with its problem and
detail, cancelled, skipped, or not reached. A succeeded command means the
owner exited successfully, not that its root or all its allocation went.
Results keep scope allocation (the scan's size, an upper bound) separate
from the volume's measured availability before and after, which can fall.
Other activity, open files, hardlinks, APFS clones and snapshots can keep
blocks; PwrDisk never alters snapshots. Cache cleanup always rescans,
including after a partial failure, and never marks a retained cache root as
deleted.

## Exports

Exports remain limited to the marked paths, even when a command's scope
is wider or a path is chosen for Delete files. Lists and agent prompts
never inherit a wider scope. Save/copy raw deletion scripts refuse Cache
cleanup mode and recognized cache selections; an export cannot stand in
for an owner command or for a decision made in Review. Tags and owner
layouts on ancestors protect contained subdirectories and files, including
caches relocated through CLI settings.

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
