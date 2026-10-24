# What is a mark, and when does the delay happen?

This file preserves the first set of input measurements at the original
PR #9 fix. For the final combined scope, current validation and freshly
replayed main-versus-final inputs, see [expanded evidence](expanded/README.md).

A **mark** is one file or directory queued for removal. Marking a directory
marks that path once, regardless of how many files it contains. For example,
marking a cache directory containing a million files is M=1, not M=1,000,000.
Marking 1,024 unrelated paths means 1,024 separate queued targets. Main has no
usage telemetry establishing that this is a frequent manual workload; it is
a large-queue stress case, also relevant to programmatic/bulk control.

The earlier **6.60 seconds** measured **four forced window draws, which caused
eight `prepare` calls** in the GPUI harness. It was not a scan time, a single
click time, or a measured delay for every item. The ~359 ms fixed result has
the same denominator. These draws were of the Explore screen with many marks
already present; they were not the act of selecting 1,024 items.

## Where it repeats

- Explore calls `prepare` whenever the app renders, resolving marked paths on
  main. Its Marked section and disk projection each call `app.plan()` when
  those panels are shown, evaluating the entire queue again.
- Marking/unmarking, keyboard selection, zoom and pointer movement over a
  tile can request another render. Pointer movement within the same hovered
  tile also requests rendering because the tooltip follows the pointer.
- Opening Review calls `app.plan()` for the queue again. Review does not use
  the treemap's `prepare`, but it still pays the original quadratic removal
  nesting cost, plus constructing the list rows.
- `Marks::refresh` is different: it runs when a scan result arrives and when
  the metric changes. It is not run by every mark action or hover.
- An unchanged idle window need not redraw. The free-space ticker checks
  every 1.2 seconds outside removal, but only requests rendering if the value
  changes. This is not an unconditional six-second periodic stall.

## Individual user actions, with smaller queues

An additional opt-in real-window test measures actual Space, pointer-move and
C inputs. The fixture exists on disk: 1,025 sibling directories, each with one
64-byte file, scanned before timing. The tree size stays fixed at 2,050
non-root nodes while the queued count changes. The window is 1,400 by 900,
with the selection/Marked panel and disk projection visible. No files are
deleted or trashed.

Each row seeds M folder marks, then measures adding one more folder with
Space. Hover and Review are measured with M+1 marks afterward. Values are
medians of three input repetitions in milliseconds; each cell is
**main → fixed**. Both snapshots use the same fixture and test-only counters.

| Already queued M | Add one mark (Space) | Move pointer over a tile | Open Review (C) |
| ---: | ---: | ---: | ---: |
| 0 | 2.44 → 2.51 | 2.80 → 2.55 | 2.21 → 2.18 |
| 1 | 3.28 → 2.59 | 2.78 → 2.67 | 2.45 → 2.57 |
| 8 | 3.61 → 3.82 | 3.62 → 3.61 | 4.16 → 4.04 |
| 32 | 6.26 → 5.45 | 6.35 → 5.40 | 9.74 → 8.76 |
| 128 | 29.22 → 14.72 | 27.68 → 13.02 | 44.19 → 30.69 |
| 1,024 | 1,154.41 → 77.31 | 1,099.36 → 74.57 | 1,229.52 → 242.71 |

For a handful of marks, these operations were already a few milliseconds on
main; this measurement does not establish a substantial common-case speedup.
At 128 marks the repeated work is more noticeable, and the large queue causes
roughly a one-second delay for ordinary interactions in this fixture. The
problem repeats while interacting with that queue, rather than being paid
once when the scan finishes. Opening a large Review list remains more costly
than hovering because it constructs many rows as well as evaluating the plan.

The measured Space input caused two renders in repetitions 2–3, and three in
the first repetition. Hover caused two renders. Review caused three renders
(one Explore prepare plus the switch to Review). With panels visible, these
inputs evaluated four plans, or six for the first Space repetition. Framework
render scheduling can vary, so these are end-to-end harness input timings,
not a universal mapping of one physical event to one frame.

Every timed input asserts that real rendering and removal projections ran,
and that **zero `Marks::refresh` calls** occurred. No scan is part of these
individual-action numbers. Raw output includes exact render, prepare, plan
and refresh counts:

- [Main interaction log](interactions-red.log): main algorithms from the
  red commit `5a3c92e`, with the new test/counters temporarily applied.
- [Fixed interaction log](interactions-green.log): fixed algorithms with
  the identical test/counters.

These are synthetic macOS arm64 test-profile measurements, not release-app
latency guarantees or evidence of how often users select a given queue size.
The previous deterministic N/8N budgets remain necessary to protect scaling;
this additional test is intentionally ignored by normal CI because elapsed
latency is recorded as evidence, not asserted as a portable threshold.

Reproduce on the final branch:

```sh
cargo test -p disktree-app marked_interaction_timings -- \
  --ignored --nocapture --test-threads=1
```
