# Large removal-queue performance

The final combined scope and current validation are documented in
[expanded evidence](expanded/README.md): the original path-resolution and
removal-plan fixes plus efficient queue construction, one-pass parent
absorption and indexed Review status. The expanded regular budgets fail
before those additions and pass afterward, including 8,192-entry Review
queues. Current validation: 241 enabled tests and the required lint gate pass.

A mark is one queued file or folder, regardless of how many files a marked
folder contains. The original 6.60-second stress result was four forced draws
and eight prepares of Explore; it was not one scan or one click. The expanded
report separates whole-queue construction, one parent action, four Review
draws and individual UI inputs, and includes fresh combined-head timings.

The sections below preserve the original three fixes' measurements and
reproduction history. They are historical stage results; the expanded report
is the source for the final combined implementation and validation.

Measured on 2026-10-14, Darwin arm64, Rust 1.97.1, repository development/test
profile (application opt-level 1, dependency opt-level 2). All trees and paths
are synthetic or inside private temporary directories. No home/volume scan,
real Trash operation, APFS accounting change or parent-branch edit was used.

## What the timing means

A mark is one independently queued file or directory, not every file inside a
marked directory. The 1,024-mark case is a large-queue stress test, not an
established frequent manual workload. The 6.60-second result is **four forced
draws/eight prepares of Explore with that queue already present**. It is not
one scan or one click.

[Individual-action measurements and call-path explanation](interactions.md)
now cover 0, 1, 8, 32, 128 and 1,024 queued paths on a fixed real temporary
tree, using Space, pointer movement and Review inputs. Main already completes
those inputs in a few milliseconds with a handful of marks; the demonstrated
large improvement applies to large queues. At 1,024 queued folders, adding
one more or hovering takes about 1.1 seconds on main in that fixture. These
costs can recur on renders while interacting, rather than once per scan.

## Broader follow-up audit

[Additional rendering/scanning findings](additional-audit.md) document
the audit that identified queue construction, parent absorption and Review
status joins, now addressed in the expansion. Depth, mount-topology and the
case-sensitive classification edge case remain separate follow-ups.
Historical test-only probes and raw counts are included.

## Red before green

Base: `6c8d4ce6211bf135ff4e42890faebff1040e1846` (upstream/main and fork main).
Commit `5a3c92e` adds probes and budgets without changing the algorithms.
Both application budgets and the core removal budget fail there. Their exact
output is in [red.log](red.log) and [red-core.log](red-core.log).

Fix: `1206fda1437c288a53c4625e9ed2f0df05a8997e`. The same budgets pass;
[green.log](green.log) also includes the additional dimension and warm-frame
checks. The baseline dimension test was temporarily applied to the red
snapshot before restoring the fixed sources; see
[red-dimensions.log](red-dimensions.log).

The three red budgets grow marks from 128 to 1,024. Refresh and window fixtures
also grow sibling folders from 128 to 1,024, each containing one marked file:
N=256/2,048 non-root nodes, M=128/1,024 marks, D=2 path components. The removal
fixture uses disjoint paths in a deterministic permutation, so the sort does
real work and no early ancestor match hides the quadratic loop.

| Production path | Main work at 128 / 1,024 marks | Growth | Fixed work at 128 / 1,024 marks | Growth |
| --- | ---: | ---: | ---: | ---: |
| `plan_against`, one plan | 9,187 / 537,027 | 58.5x | 1,314 / 15,298 | 11.6x |
| `Marks::refresh`, four refreshes | 34,048 / 2,107,392 | 61.9x | 2,560 / 20,480 | 8.0x |
| Real window, four forced draws | 68,312 / 4,215,000 | 61.7x | 856 / 5,336 | 6.2x |

Every budget requires at most 16x work for 8x input, plus an absolute cap:
32 work units per mark for removal; 16 per mark per refresh for refresh; 16 per
mark per observed `prepare` for the window. No elapsed-time assertion is used.

Removal units count sort comparisons and nesting-candidate checks. Resolution
units count mark visits and sibling-name comparisons before the fix; after the
fix they count mark visits, indexed sibling entries and component lookups.
These are logical work budgets, not CPU instruction counts. Hashing/comparing
long names and paths still costs their length. All probes are test-only.

The GPUI harness forces notifications, draws the actual window and asserts
that `Disktree::prepare` ran at least once per draw: eight calls were observed
for four draws. This includes path lookups elsewhere in the screen, not just
the new resolver. It catches reintroducing a linear sibling search per mark
into `prepare`, even if a resolver microbenchmark still passes.

## Wall measurements

These are medians of three serial repetitions with `--test-threads=1` on the
same machine/profile. Fixture construction, compilation and mark insertion
are outside the timers. The window timer includes the real render path,
layout/painting work and freshly evaluated removal projections. Backing roots
are empty temporary directories, with the displayed trees constructed in
memory; these are scaling workloads, not a filesystem latency benchmark.

| Workload at 1,024 marks | Main median | Fixed median | Speedup |
| --- | ---: | ---: | ---: |
| One removal plan | 398.13 ms | 23.24 ms | 17.1x |
| Four mark refreshes | 9.07 ms | 1.53 ms | 5.9x |
| Four window draws, eight prepares | 6,602.00 ms | 358.68 ms | 18.4x |

Raw samples: [wall-red-repeats.log](wall-red-repeats.log) and
[wall-green-repeats.log](wall-green-repeats.log). Fixed warm draws have their
own timer and are excluded from the cold-window median above. Timings are
observations, not portable thresholds or a frame-rate guarantee.

## Which variables actually cause the growth

N is all scanned nodes, M is marked paths, D is path depth, R is the number
of refreshes/renders. F below is the number of first-level sibling folders.
The remaining single-child chains make depth independently controllable.

| Folders F | Marks M | Depth D | Refreshes R | Main work | Fixed work |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 128 | 8 | 2 | 1 | 1,012 | 160 |
| 1,024 | 8 | 2 | 1 | 8,180 | 1,056 |
| 1,024 | 64 | 2 | 1 | 63,648 | 1,280 |
| 128 | 8 | 16 | 1 | 1,124 | 384 |
| 128 | 8 | 2 | 8 | 8,096 | 1,280 |

Main is linear in each of F, M, D and R when the others are held fixed.
It becomes quadratic when siblings and marks grow together. Repeating an
unchanged linear pass R times is O(items * R); calling it quadratic solely
because R can grow would misidentify the problem. Unmarked file counts do
not affect removal nesting at all, but sibling counts affect path lookup.

On main:

- Disjoint accepted removal marks cause M(M-1)/2 ancestor checks after sorting.
- Refresh resolves each marked path separately. At a broad directory, each
  lookup scans siblings again: O(M*N) worst-case per refresh.
- `prepare` repeats analogous path-to-crumb searches for every mark on every
  render, even when the tree and marks are unchanged.

After the fix:

- Nesting costs O(M log M) path comparisons plus M prefix checks. Guard keys
  are computed once per accepted target, sorted using the guards' case and
  macOS Data-volume semantics, and compared with the last retained outer
  target. Display output retains the original path ordering. Including path
  length L, this stage costs O(M log M * L) time and O(M * L) storage.
- One `Resolver` batch indexes each visited directory's siblings once. Let S
  be those indexed entries (S <= N). Expected hash work is O(S + M*D) per
  batch, with O(S) temporary index storage; untouched subtrees cost nothing.
  `Marks::refresh` creates one batch per refresh, so R refreshes still cost
  O(R * (S + M*D)). Path/name lengths are additional hashing/copying costs.
- `Marks::crumbs` stores O(M*D) crumbs, shared by `Arc`. An unchanged frame
  reuses that set without resolving marked paths. The first frame after an
  invalidation costs one batch. Visible tiles still require processing each
  frame: O(T*R) at bounded display depth, where T is the laid-out tile count.
  Ancestor-mark checks depend on depth; this does not claim depth-independent
  work for arbitrarily deep crumb vectors.

The real-window warm budget allows only 64 resolution units per observed
prepare, independent of M. Both mark counts measured 216 units over eight
prepares (other screen lookups); marked-path resolution was reused. It also
counts actual tile visits and verifies proportional work across repeated
frames. Merging tiny tiles means T need not grow with N.

Fresh removal plans still evaluate guards, resolve filesystem paths and
attribute volumes for marks on every render. The sort remains O(M log M)
per plan, and mount-table/system-tree checks retain their existing costs.
These costs are visible in the remaining ~359 ms window workload. No removal
authorization is cached. The worker's fresh mount/link checks immediately
before deletion and descriptor-based deletion protections are unchanged.

## Invalidation and semantics

The display cache is invalidated by toggle, remove, clear and refresh, and
checks tree allocation identity, root path and metric. It stores a `Weak`
reference to the tree, so it cannot retain an old scan; the weak reference also
prevents address reuse from masquerading as the same tree. `Arc::make_mut`
dissociates that identity before in-place changes can affect a cached tree.

Tests cover every boundary, metric reordering and replacement through the
window, missing paths, component boundaries, exact case-sensitive scan names,
first-match behavior for equal lossy names, and non-UTF-8 Unix paths. Removal
nesting uses the platform guard keys, with coverage for case variants,
duplicates, component prefixes and stable output ordering. Existing safety
and synthetic permanent-deletion tests remain green.

Integration API:

```rust
let mut resolver = marks::Resolver::new(root_path, tree);
let resolved = resolver.resolve(path); // Option<(Vec<usize>, &Node)>
```

Reuse a resolver for the whole batch. The old standalone `marks::find` was
removed when its production caller moved to this API. Display callers use
`Marks::crumbs(&Path, &Arc<Node>, Metric) -> Arc<FxHashSet<Vec<usize>>>`;
callers have no revision-counter or manual invalidation obligation.

## Original-stage verification and reproduction

- `cargo xtask lint`: pass; [lint.log](lint.log).
- `cargo xtask test`: 78 app, 150 core and 3 xtask tests pass;
  [tests.log](tests.log). Two pre-existing ignored core tests remain ignored. The new opt-in
  interaction timing test is also ignored by the normal suite; it passes
  when explicitly run on both main and fixed snapshots.
- `cargo test --workspace budget -- --nocapture`: all budgets pass.
- Three serial red repetitions fail the intended budgets (exit 101), followed
  by three serial green repetitions passing them (exit 0).
- Local validation is macOS arm64. Other operating systems/architectures are
  left to the repository's existing CI; no cross-platform result is claimed.

The commit IDs in this report refer to pre-consolidation development
snapshots archived in [fork PR #9](https://github.com/huntharo/disktree/pull/9).
See the [historical fetch instructions](expanded/README.md#red-before-green).

From a clean checkout at `5a3c92e`, reproduce the failures with:

```sh
cargo test -p disktree-app budget -- --nocapture --test-threads=1
cargo test -p disktree-core removal_nesting_budget -- --nocapture
```

At `1206fda`, run the green budgets and gates:

```sh
cargo test --workspace budget -- --nocapture --test-threads=1
cargo xtask lint
cargo xtask test
```
