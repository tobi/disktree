# GitHub Actions

The release workflows are ported from PwrRoute's, which follow PwrGit's
shape: label-gated previews, and signing in environments whose jobs never
check out source. Every build step is a `cargo xtask` task or a script under
`scripts/release/`, so CI and a local run do the same thing.
`docs/releasing.md` is the procedure; this file is how the workflows do it.

| Workflow | Trigger | What it does |
|---|---|---|
| `ci.yml` | push to `main`, PRs | Lint and test on Linux, macOS and Windows; tests on the other architectures releases build for; an ad hoc signed bundle and DMG on macOS; check third-party notices when their inputs change. |
| `preview-build.yml` | `build-preview` PR label, manual dispatch | The release's packaging, unsigned: a macOS universal DMG cut from the vpk-packed app, and Windows x64 and ARM64 Setup.exe, MSI and portable zip, as workflow artifacts. |
| `release.yml` | `v*` tag push, manual dispatch with a tag | Checks the tag against `Cargo.toml` and `CHANGELOG.md`, lints, builds every platform in unprivileged jobs, packs and signs macOS and Windows with Velopack in credentialed jobs, then creates a GitHub Pre-release with every asset and the Velopack update feeds. |

Every job runs on a standard GitHub-hosted runner: free on a public
repository, macOS included, and discarded after the job. PwrRoute builds
macOS on self-hosted lab machines; PwrDisk does not, because it takes pull
requests from forks and those must never reach a persistent machine. Every
Rust job installs the version `rust-toolchain.toml` pins, read from the
checkout, so that file is the only place it is written.

## ci.yml

The Linux `licenses` job runs `cargo xtask licenses --check` on every push
to `main` and on PRs changing `Cargo.lock`, any `Cargo.toml`, `licenses/**`
or `THIRD_PARTY_LICENSES`. Changes to `ci.yml`, the `setup-rust` action or
`xtask/src/licenses.rs` also run it so edits to the gate or generator are
checked. It installs
`cargo-about@0.9.2` with `cargo install --locked --features cli` and caches
the binary by OS, architecture, generator version and Rust version. Like
PwrRoute, the generator is pinned and the notice check runs on Linux only.

The existing changed-file classifier selects this job, including both
paths of renames. Unrelated PRs skip it; a failed classifier or incomplete
file list runs it. The workflow trigger stays unfiltered so the job always
reports a result. A stale file fails with instructions to run
`cargo xtask licenses` and commit `THIRD_PARTY_LICENSES`.

### The Rust cache

Every Rust job in `ci.yml`, `preview-build.yml` and `release.yml` sets up
through one local composite action, `.github/actions/setup-rust`, so the
toolchain pin, the prune and the cache's save rule cannot drift apart
between jobs. It reads the `channel` from the checked-out
`rust-toolchain.toml`, installs it with `dtolnay/rust-toolchain`, runs
`scripts/ci/prune-rust-toolchains.sh`, then `Swatinem/rust-cache`, saving
only on `main`. The version is read from the file, not a repository
variable, so a release tag builds with the Rust it pinned however the
variable has moved since. Its inputs are what differ between jobs:
`components` (clippy and rustfmt for the lint jobs), `targets` (both Apple
architectures for the universal macOS build), `cache-bin` (off in the
`licenses` job, whose own cache owns `cargo-about`) and `save-if` (`false`
in `release.yml`, because a cache saved under a tag is readable by that
tag's runs alone). Its `toolchain` output is the version it read, which
keys the `cargo-about` cache. A job must check out the repository before
using it, as with any local action.

The cache saves only on `main`; a PR restores `main`'s entry for the same
job, by key prefix when its `Cargo.lock` differs. The Actions cache store
holds 10 GB and evicts the least recently used entry first. While PRs saved
too, each wrote about 6 GB across the jobs, `main`'s entries were evicted,
and the next PR compiled from scratch: the macOS job's lint went from under
a minute to six, and its tests from two minutes to eight. As in PwrRoute,
the prune script removes the runner image's own stable toolchain before the
cache step, because rust-cache hashes every installed toolchain and image
rollouts would otherwise change the key. A push to `main` never cancels the
`main` run in progress, since a cancelled job saves nothing.

Lint and test stay one job per platform. They compile separately anyway
(clippy builds dependencies as metadata, tests as full code), so splitting
them would only trade one runner for two and add a cache entry per
platform to the 10 GB budget.

## preview-build.yml

Applying the `build-preview` label starts a run, and later pushes to a
labeled PR re-run it. `workflow_dispatch` packages a branch without a PR,
from the default branch's copy of the workflow. PR builds are versioned
`<base>-pr.<number>`, dispatched builds `<base>-dev.<run number>`; the
version reaches `cargo xtask bundle` through `PWRDISK_VERSION` and `vpk`
through `--packVersion`, and `Cargo.toml` is never edited, so `--locked`
holds.

It runs release.yml's jobs minus credentials, release notes and deltas:
`macos-prepare` and `windows-prepare` build and hand over an archive and its
digest, and `macos-pack` and `windows-pack` check the digest and pack
without a checkout, with the same scripts. So a preview also proves the
hand-off, including the per-architecture outputs of the Windows matrix.
No secrets reach this workflow.

The macOS DMG is ad hoc signed and not notarized: a downloaded copy needs
**Open Anyway** once. Its app carries Velopack's `UpdateMac`, like a
release. The Windows installers are unsigned, so SmartScreen warns.

## release.yml

| Job | Runner | Environment | Does |
|---|---|---|---|
| `check` | ubuntu-24.04 | — | `cargo xtask release-check`, `cargo xtask lint`; hands the changelog section on as the release notes. |
| `linux` | ubuntu-22.04, ubuntu-22.04-arm | — | Tests, `cargo xtask portable`, the tarball and its `.sha256`. The oldest runners, for the oldest glibc floor. |
| `macos-prepare` | macos-26 | — | Tests; `cargo xtask bundle --universal --app-only`, with the Icon Composer catalog checked; archives `PwrDisk.app`, the entitlements, the DMG background, `macos-dmg.sh`, `dmg-layout.py`, `macos-signing.py`, `install-vpk.sh` and the notes. |
| `macos-sign` | macos-26 | `apple-signing` | `vpk pack`, signed, notarized and stapled; the DMG from the packed app, the same; verifies both. |
| `windows-prepare` | windows-2025 (x64), windows-11-arm (ARM64) | — | Tests; `cargo build --release` with the static C runtime; archives `pwrdisk.exe`, README, LICENSE, the Velopack notices, `install-vpk.sh`, `vpk-pack-windows.ps1`, the icon and the notes. |
| `windows-sign` | windows-2025 | `windows-signing` | `vpk pack` for each architecture, signed through Azure Artifact Signing; verifies every signature. |
| `publish` | ubuntu-24.04 | — (`contents: write`) | Creates the Pre-release with every platform's assets. |

### The hand-off

A prepare job has the checkout and the toolchain. It uploads one archive
holding everything its sign job needs, and passes the archive's SHA-256 as
a job output. The sign job, the only one in its signing environment,
downloads the archive, checks it against both the job output and the
archive's own `.sha256`, and works from the archive alone: it never checks
out source, runs cargo or restores a cache. The Windows prepare job is a
matrix, whose legs share one outputs map; each leg sets only
`sha256-<arch>`, and `windows-sign` refuses an empty digest.

`macos-sign` follows PwrRoute's isolated signing workspace: it clears only
its own `release-signing` directory, and the vpk tools, .NET, the decoded
certificate and API key and the keychain all live in a private `mktemp`
directory removed at the end. `macos-signing.py` snapshots the keychain
search list and default first and restores them on cleanup, which runs on
success, failure and cancellation; its tests (`macos_packaging_test.py`,
run by `cargo xtask test`) mock `security` and `xcrun`.

### Signing

Both environments accept only `v*` tags and wait for a reviewer's approval;
`docs/releasing.md` lists their secrets and variables and the scripts that
upload them from 1Password. PwrRoute packs unsigned, with a warning, when a
credential is missing. PwrDisk does not: each sign job checks its
credentials first and fails, so no release ships unsigned or ad hoc
signed.

### Velopack

Each sign job runs `vpk download github --pre` for its channel before
packing, so `vpk` can write a delta against the newest published release;
the first release, or a feed that cannot be read, packs full packages only.
That previous package stays in the release that owns it: the macOS job
copies this version's files by name, and the Windows job skips every
package that was there before packing.

| Channel | Built for | Feed |
|---|---|---|
| `osx` | macOS, universal | `releases.osx.json` |
| `win` | Windows x64 | `releases.win.json` |
| `win-arm64` | Windows ARM64 | `releases.win-arm64.json` |

An installed copy reads its own channel from Velopack's manifest, so an x64
install never takes the ARM64 build, or the reverse. The publish job fails
if two platforms produce the same file name rather than let one replace the
other.

### Not ported from PwrRoute

- **Self-hosted macOS runners**, for the fork-safety reason above. The
  hosted macos-26 builds both architectures, on Xcode 26 so the Icon
  Composer icon compiles; another macos-26 signs, as PwrRoute's does.
- **`--require-icon-catalog`.** The jobs check the bundle for `Assets.car`
  and `CFBundleIconName` instead.
- **Winget and Homebrew audit stubs.** There is no package in either yet.
- **The separate `typecheck`/`build`/`test` jobs.** PwrDisk's `ci.yml`
  lints and tests in one job per platform; the release lints once and tests
  on each platform it builds. PwrRoute's main-only Rust cache is ported.
- **Linux `.deb`.** PwrDisk keeps its tarball and `install.sh`.

## PR labels

| Label | Workflow | Effect |
|---|---|---|
| `build-preview` | `preview-build.yml` | Builds the unsigned DMG and Windows installers for the PR. Applying it starts a run; later pushes to a labeled PR re-run it. |
