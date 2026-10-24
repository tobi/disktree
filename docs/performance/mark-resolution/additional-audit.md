# Additional rendering and scanning complexity audit

**Historical audit:** queue construction, parent absorption and Review status
joins below are now fixed by the [expanded PR #9](expanded/README.md). The
remaining depth, mount-topology and classification findings stay outside
that queue-focused scope. Counts in this file describe the audited snapshot,
not the final combined implementation.

Audited on 2026-10-14 at `ab11b1fd884e950faa9902c0b3e0e870eb6815d9`:
main `6c8d4ce` plus PR #9's fixes. APFS feature code is outside this audit.
The cases below remain after the original three fixes. This follow-up records
findings; it adds no further runtime or API change.

[Raw probe output](additional-audit.log) and the
[reproducible test-only probe patch](additional-audit.patch) preserve the
measurements. The probes drive production functions; Review and breadcrumb
measurements draw the real GPUI window. Fixtures are synthetic in-memory trees
or real private temporary trees. No real removal or whole-home scan ran.

M = marks, B = blocked marks, C = covered marks, D = path depth,
F = entries in one directory, K = mount-table entries. All operation counts
below compare 128 with 1,024 of the named variable, an 8x increase. Counts are
deterministic; recorded wall times are single instrumented test-profile runs,
not latency budgets or production timing guarantees.

## Confirmed follow-ups

| Operation | Variable / measured scope | Work at 128 | Work at 1,024 | Growth |
| --- | --- | ---: | ---: | ---: |
| Build a queue by calling `toggle_mark` repeatedly | M, no renders | 16,384 mark visits | 1,048,576 | 64.0x |
| Mark a parent over marked children | M children, one action | 8,384 retain visits | 525,824 | 62.7x |
| Render Review with all paths blocked | M, per actual render | 8,256 comparisons | 524,800 | 63.6x |
| Render Review with children covered by one parent | M children, per actual render | 8,384 comparisons | 525,824 | 62.7x |
| Build deep breadcrumbs | D, per actual render | 8,128 prefix index steps | 523,776 | 64.4x |
| Detect repeated Linux mount views | K, one startup calculation | 8,128 mount-pair checks | 523,776 | 64.4x |
| Classify case variants of `node_modules` | F, one finishing pass | 16,384 sibling checks | 1,048,576 | 64.0x |

### Queue construction and parent absorption: highest MCP relevance

[`Disktree::toggle_mark`](../../../crates/disktree-app/src/state.rs) invokes
`marked_ancestor`, which scans all existing marks, then scans marks again for
paths inside the new target. Repeating the operation M times costs O(M^2)
even if every mark is a disjoint file and rendering is suppressed. The probe
calls the real state transition without drawing between insertions. At 1,024
marks it measured about 200 ms with probes, versus 3 ms at 128.

Marking a parent over K marked descendants then calls `Marks::remove` K
times. Each call compacts the ordered target vector with `retain`:
O(K*M), or O(M^2) when most marks are descendants. This is one user action,
not the cost of the parent's scan. The 1,024-child probe measured ~15 ms;
operation counts are the stronger scaling evidence.

A future MCP batch API should normalize the entire queue once, preserving
mark ordering and path semantics. Merely suppressing redraws while looping
over the scalar marking API does not remove this cost. Parent absorption
should remove the whole descendant set with one compaction. Ancestor
membership can use indexed ancestor paths instead of scanning every mark.

### Review: row-status joins still use repeated linear lookup

[`views::review`](../../../crates/disktree-app/src/views.rs) runs
`covered.iter().any(...)` and `blocked.iter().find(...)` for every displayed
item. This is O(min(M, 1200) * (B+C)) in the general case. It behaves
quadratically below the 1,200-row cap when blocked/covered counts grow with
the queue; above the cap it has a bounded displayed-row factor. It is not an
unbounded quadratic pass over every file on disk.

The ordinary accepted-path fixture in the original budget had B=C=0, so it
could not expose these joins. The new blocked fixture queues paths outside
the scanned root; guards refuse them, with no deletion attempt. The covered
fixture supplies an overlapping raw queue. Normal manual marking absorbs
such overlaps, so the latter represents a raw/programmatic queue or other
path-alias situations, rather than an ordinary manual selection sequence.

Each test forces four Review draws and observes eight real renders. Logs
report both totals and comparisons per render. The ~1.10 s blocked and
~1.24 s covered wall samples at 1,024 are those **four-draw totals**, not
single clicks. A per-plan lookup map/set for row status would avoid repeated
searches while leaving removal guards freshly evaluated.

### Breadcrumbs and deep filtering: depth-dependent costs

[`Disktree::breadcrumbs`](../../../crates/disktree-app/src/state.rs) starts at
the scan root again for every successive prefix, producing D(D-1)/2 indexed
walk steps. It also clones crumb prefixes. The view later hides most of the
trail, after the full trail has already been materialized. Above-root path
ancestors are similarly copied before this display truncation.

The probe sets current depth to 128 and 1,024 in memory and draws the actual
Explore window. Deep chains like this are an edge workload, not evidence
that a common shallow home directory suffers this cost. Walking the node
chain once and building only displayed breadcrumb targets would bound it.

Separately, `Matches::keep` and marked-ancestor coloring hash successive crumb
prefixes. Hashing each prefix depends on its length, so these can also cost
O(D^2) per tile when the absolute path is deep and the filter/marks require
checking all ancestors. Those hash costs were source-audited, not separately
measured by this breadcrumb counter. They depend on path depth, not the
number of siblings in one folder.

### Linux mount setup: topology-dependent, not file-count-dependent

[`space::repeated_mounts_excluding`](../../../crates/disktree-core/src/space.rs)
compares each visible mount with previously kept mounts. The probe uses
independent synthetic devices so no early match masks K(K-1)/2 comparisons.
This runs while preparing a scan on systems with `/proc/self/mountinfo`.
The algorithm was measured on synthetic Linux mount metadata on macOS;
there was no actual Linux mount manipulation or Linux wall-time benchmark.

Other code in that routine can scan the full mount table or exclusion list
inside these checks, so more complex topologies deserve separate bounds.
The volume picker has a similar linear search through already seen devices.
Removal guards and volume attribution also retain O(M*K) checks per plan.
None of these is quadratic in the number of files with a fixed mount table.
The simple 1,024-mount synthetic case took ~3 ms here, so this ranks below
queue construction and Review for the proposed MCP flow.

### Directory classification: a real but unusual within-folder case

[`classify::reclaim_of`](../../../crates/disktree-core/src/classify.rs) requests
a sibling search only for particular names: `target`, `node_modules`, and
`logs`. Ordinary folders do not trigger a sibling scan for every arbitrary
entry. That apparent generic F^2 pattern is therefore usually a false alarm.

However, the directory-name match is ASCII case-insensitive. A case-sensitive
filesystem can contain many distinct spellings of `node_modules`, each of
which searches all siblings for an absent `package.json`. The probe creates
128/1,024 such names in memory and calls full `classify`, observing F^2
comparisons. There are only 2^11 case variants of this particular name, so
this is a quadratic window up to a finite namespace bound, not an unbounded
quadratic algorithm for arbitrary folder sizes. These variants cannot all
coexist on a normal case-insensitive APFS volume; this is chiefly a Linux or
case-sensitive-volume edge case. It is scan finishing, not per-frame work.

Precomputing the few manifest/sibling flags once per parent would remove the
case without adding fields to every scanned node.

## Normal scanning and bounded layout did not show this problem

The regular walk appends each directory entry once. Directory completion
moves completed children into their parent, and aggregation visits each node
once before sorting each folder's children. Hardlink de-duplication and
followed-directory detection use a bitmap/hash sets, not a search through all
previous files. The admission queue removes an admitted tail, avoiding a
front-removal shift per job.

Actual fixed-one-thread scans of private temporary trees produced:

| Fixture | Listed entries at 128 / 1,024 items | Aggregate node visits |
| --- | ---: | ---: |
| One directory of files | 128 / 1,024 | 129 / 1,025 |
| One directory of folders, one file in each | 256 / 2,048 | 257 / 2,049 |

Those counts are linear. Sorting is O(sum(F_i log F_i)) over directories,
with additional filename/path-length and filesystem costs. This does not
prove every input or OS path is linear; it rules out a general sibling-pair
loop in the normal walk/aggregation paths exercised here.

`treemap::squarify` recomputes worst aspect ratios across a growing row, which
is quadratic within a row. With unbounded equal-weight input, ratio-element
visits rose from 682 to 12,525 (18.4x at 8x input): superlinear, not a measured
64x file-count quadratic. The real layout caller keeps only 96 children plus
one merged tail per directory before this step. Its measured ratio work was
482 versus 319 at 128/1,024 source entries. Ranking all source entries still
costs O(F log F) on a layout cache miss; normal unchanged frames reuse layout.
The internal row rescan therefore is not currently an unbounded wide-folder
rendering blocker.

Windows MFT enumeration builds parent indexes and sorts names rather than
repeatedly searching every sibling. One additional **unmeasured** metadata
candidate is `mft::table_layout`'s vector `numbers.contains` while collecting
extension records from the MFT's own attribute list. That can be O(E^2) in
extension-record count E on a fragmented MFT; E is not the total file count.
Windows runtime behavior was not tested in this macOS audit.

## Reproduce without changing runtime code

The commit IDs below are historical pre-consolidation snapshots; see the
[fetch instructions](expanded/README.md#red-before-green).

From clean source at `ab11b1f` (or later evidence-only descendants):

```sh
git apply docs/performance/mark-resolution/additional-audit.patch
cargo test --workspace additional_ -- --ignored --nocapture --test-threads=1
git apply -R docs/performance/mark-resolution/additional-audit.patch
```

The patch adds counters gated by `cfg(test)` and ignored diagnostic tests.
All five diagnostic tests passed their correctness/call-path assertions. It
records counts rather than adding passing assertions that would enshrine
quadratic behavior. The patch was then removed: source remains byte-identical
to `ab11b1f`. The original lint/full-test result therefore still applies to
the unchanged runtime implementation. These findings need separate fixes
and corresponding failing budgets before their scaling can be called bounded.
