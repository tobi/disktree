# Expanded PR #9: construction, absorption and Review status

This is the final evidence for the three queue issues discovered after the
original PR #9 fixes. The scope is queue construction, marking a parent over
marked descendants, and Review's blocked/covered row lookups. The existing
removal nesting, mark-refresh and cached-crumb fixes remain included.

A mark is one independently queued file or folder. Marking a folder counts
once regardless of its descendant file count. Long queues are relevant to
programmatic/MCP control; no usage evidence establishes 1,024 independent
marks as a frequent manual workload. This PR does not add an MCP endpoint.

## Trigger and denominator

| Operation | What is timed | When the cost is paid |
| --- | --- | --- |
| Queue construction | All 1,024 state-level additions, before any drawing | Building a queue of disjoint paths; files and folders have separate budgets |
| Parent absorption | One call marking a parent over 1,024 child marks, plus an unrelated mark | That marking action, before rendering |
| Blocked/covered Review | Four forced draws, eight observed renders | Repeated Review rendering, including opening/updating the screen |
| Space, hover, C input | One input plus harness rendering/settling | Ordinary interaction with an already populated queue |

The earlier 6.60 s number was four forced Explore draws/eight prepares on
main, not a single click, one scan, or the time to select all 1,024 paths.
Neither queue construction nor parent absorption here includes painting.
The Review wall measurements below do include row layout/painting and fresh
plan evaluation. Comparisons per render are reported separately from total
wall time. Timed input tests assert zero `Marks::refresh` calls: no rescan is
included in those numbers.

## Red before green

The development history was consolidated into one upstream contribution.
Commit IDs below identify the original measured snapshots, archived in
[fork PR #9](https://github.com/huntharo/disktree/pull/9); they are not
separate commits in the final branch. To reproduce a historical stage,
fetch its full SHA from `https://github.com/huntharo/disktree.git` before
checking it out. For example:

```sh
git fetch https://github.com/huntharo/disktree.git \
  57d4efde372e7f08e0a2b91a906ff3b870abaddc
git switch --detach FETCH_HEAD
```

At `57d4efde372e7f08e0a2b91a906ff3b870abaddc`, the new probes and budgets run
against the original queue/Review algorithms, with the earlier PR #9 fixes
already present. Four regular budgets fail: queue construction, parent
absorption, blocked Review and covered Review. See [red.log](red.log).
The added folder-only variant was also replayed against that unchanged
algorithm and fails; see [red-folders.log](red-folders.log).

The original development commits were:

- `e88b9e1`: ordered mark index, indexed ancestor lookup and one-pass
  descendant absorption.
- `b71718f`: Review row indexes borrowed from each newly computed plan.
- `1c36fae`: the separate directory-insertion regression budget; it changes
  test coverage, not runtime behavior.
- `d886865`: compatibility coverage for raw paths with redundant separators,
  preserving the original shortest-stored-spelling ancestor choice. This
  does not affect the canonical paths in the timing fixtures.

The same regular budgets pass after the fixes. They grow input from 128 to
1,024 and require <=16x logical work, plus absolute caps. Queue mutation
allows <=32 units per mark; row status allows <=8 units per queued item per
observed render. Both Review variants also run at 8,192 entries and verify
that exactly 1,200 rows are built per render.

| Measured path | Before, 128 / 1,024 | Combined fix, 128 / 1,024 |
| --- | ---: | ---: |
| Queue construction, all additions | 16,384 / 1,048,576 | 1,152 / 9,216 |
| Folder queue construction, all additions | 16,384 / 1,048,576 | 1,152 / 9,216 |
| Parent absorption, one action | 8,771 / 528,899 | 266 / 2,058 |
| Blocked Review status, per render | 8,256 / 524,800 | 384 / 3,072 |
| Covered Review status, per render | 8,384 / 525,824 | 386 / 3,074 |

The blocked fixture queues paths outside the scanned root. The covered
fixture supplies a raw overlapping parent/children queue; normal manual
marking would absorb those overlaps. Both exercise status decoration through
planning and rendering only, with no removal attempt.

At 8,192 items, both Review cases use 10,592 status units per render, while
keeping the 1,200-row cap. See [green.log](green.log). The tests drive
`Disktree::toggle_mark` or actual GPUI window draws, not an isolated index
helper. They verify surviving mark order, counts, directory/file kinds,
covered/blocked decorations, render counts and the row cap.

The logical mutation units count mark visits before the fix; after the fix
they count ancestor/range requests, removals and compaction visits. They do
not count internal B-tree comparisons. The complexity guarantee below
includes that logarithmic factor. Review units count status construction and
row lookups after the fix, versus repeated comparison scans before it. These
are deterministic work budgets, not CPU instruction counts.

## Three-run wall measurements

Medians in milliseconds from three serial repetitions, macOS arm64, Rust
1.97.1, the repository test profile, `--test-threads=1`. Compilation and
fixture setup are excluded. These workloads use synthetic displayed trees
backed by empty private temporary roots; they establish scaling, not storage
throughput. Raw samples: [wall-before.log](wall-before.log) and
[wall-after.log](wall-after.log).

| Workload at 1,024 marks | Before expansion | Combined fix |
| --- | ---: | ---: |
| Queue all 1,024 files, no drawing | 176.13 ms | 6.41 ms |
| One parent-absorption action | 15.60 ms | 0.47 ms |
| Blocked Review, four draws/eight renders | 1,003.88 ms | 883.69 ms |
| Covered Review, four draws/eight renders | 1,110.66 ms | 1,009.63 ms |

Review's wall improvement is much smaller than its row-lookup work reduction:
the screen still lays out up to 1,200 rows and computes plans/guards.
Eliminating one quadratic join does not eliminate those linear and
O(M log M) costs. The larger 8,192-item capped cases took approximately
1.19 s blocked / 2.11 s covered for four draws in these instrumented runs.
No elapsed-time threshold or frame-rate claim is made.

## Implementation and guarantees

Let M be current marks, D path depth, K absorbed descendants, B blocked
entries, C covered entries and L displayed rows (L <= 1,200). Path lengths
add comparison/hashing/copying costs to the bounds below.

`Marks` keeps its ordered `Vec<Target>` for display and replaces the private
exact-path hash set with a `BTreeSet<PathBuf>`. Native `Path` ordering is by
components, so a strict descendant, if any, immediately follows a parent's
position in that ordered index. Prefix lookalikes such as `a-b`, `a.b` and
`ab` do not defeat that query.

- Ancestor lookup visits path ancestors and queries the index, costing
  O(D log M) index work rather than scanning M marks.
- A disjoint insertion proves that no descendants exist using an ordered
  range query. Building M disjoint marks therefore costs O(M*D*log M),
  instead of O(M^2). Both files and directories follow this path.
- A parent with K marked descendants compacts the item vector once and
  removes those K keys: O(M + K log M), instead of K full compactions.
- Exact mark membership changes from expected hash O(1) to O(log M). The
  index still stores one path per mark and adds no fields to scan nodes.
- Unmarking one item still compacts the ordered vector in O(M). Multiple
  separately invoked parent actions can each require their own compaction;
  this is a per-action bound, not a promise that arbitrary sequences of
  scalar edits cost one pass in total.

`Disktree::toggle_mark` and the existing public behavior stay intact: it
refuses a mark beneath an already marked ancestor, absorbs descendants,
keeps unrelated marks in their original order and reports the absorbed count.
Tree-to-crumb display caches are invalidated by the mutations, including the
new absorption operation. Refresh continues retaining missing paths at zero
bytes, and the ordered index stays consistent with those retained paths.

`ReviewStatus` borrows covered paths and blocked reasons from the `Plan`
computed for the current render. Construction plus all row lookups costs
expected O(B + C + L) hashing work and O(B + C) temporary index storage.
The first blocked reason wins for duplicate paths, preserving the original
linear lookup's behavior. Item order, chips and the display cap remain.
No plan/status authorization is retained across renders.

Tests preserve native path component/case semantics, exact non-UTF-8 paths,
first-reason behavior, parent mark retention, unrelated neighbours and cache
invalidation after absorption. Removal guards, measurement and execution
logic are unchanged; worker-side mount/link revalidation still occurs
immediately before deletion. Existing synthetic permanent-deletion tests
continue passing.

## End-to-end input evidence

The separate opt-in input benchmark uses real on-disk temporary directories:
1,025 sibling folders, one 64-byte file each, with the same 1,400x900 window
and both panels visible. Space queues one additional folder; hover and C
then run with that additional mark present. Three repetitions record the
actual render/prepare/plan counts and zero refreshes. These inputs cause two
or three renders in the harness, not universally one production frame.

[interactions-main.log](interactions-main.log) replays main algorithms
(`5a3c92e`, tests/probes over main `6c8d4ce`) with the input test applied.
[interactions-final.log](interactions-final.log) runs the combined fixes
immediately afterward. [interactions-before.log](interactions-before.log)
is a separate control with only the original PR #9 fixes, before this
expansion. A generated table of the latest input medians is kept below.
These test-profile samples are observations, not release-app per-click
guarantees; small queues do not support a substantial speedup claim.

| Already queued | Space: main → final | Hover: main → final | Review C: main → final |
| ---: | ---: | ---: | ---: |
| 0 | 3.52 → 3.52 ms | 3.64 → 3.58 ms | 3.10 → 3.07 ms |
| 1 | 3.79 → 3.76 ms | 3.94 → 3.87 ms | 3.47 → 3.43 ms |
| 8 | 5.14 → 5.01 ms | 5.28 → 5.14 ms | 5.82 → 5.74 ms |
| 32 | 8.70 → 7.23 ms | 8.85 → 7.43 ms | 13.53 → 12.19 ms |
| 128 | 41.54 → 16.13 ms | 40.85 → 16.26 ms | 62.99 → 38.19 ms |
| 1,024 | 1584.73 → 96.49 ms | 1575.83 → 97.78 ms | 1786.18 → 300.71 ms |

## Validation and reproduction

- `cargo xtask lint`: pass; [lint.log](lint.log).
- `cargo xtask test`: 88 app + 150 core + 3 xtask tests pass (241 total);
  [tests.log](tests.log). Two pre-existing ignored core tests and the manual
  timing test remain ignored by normal CI.
- `cargo test --workspace budget -- --nocapture --test-threads=1`: all
  original and expanded budgets pass, including 8,192-entry Review cases.
- The opt-in input timing test passes when explicitly run.
- Local validation is macOS arm64. Other operating systems remain for the
  repository's existing CI; no new cross-platform result is claimed.

The first four red failures can be reproduced at `57d4efd`:

```sh
cargo test -p disktree-app budget -- --nocapture --test-threads=1
```

At the final head:

```sh
cargo xtask lint
cargo xtask test
cargo test --workspace budget -- --nocapture --test-threads=1
cargo test -p disktree-app marked_interaction_timings -- \
  --ignored --nocapture --test-threads=1
```

These changes improve the existing state/render paths. Future programmatic
bulk control should still submit additions within one UI transaction rather
than require a redraw per tool call. Per-render plans continue processing
the queued items, and independently rendering R times still pays that cost
R times. No MCP transport/control implementation is included here.

Deep breadcrumbs, Linux mount-topology checks and the unusual classification
case from the [broader audit](../additional-audit.md) remain separate
follow-ups. This PR is one main-based review unit for large removal queues;
it does not depend on the APFS feature branch.
