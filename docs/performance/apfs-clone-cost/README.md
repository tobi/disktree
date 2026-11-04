# What APFS clone accounting costs a scan

Measured 2026-11-04 on an Apple M5 Max (18 cores, 64 GiB, APFS, macOS
27), against `fce13fb` (v0.17.0). The machine was never quiet: load
averaged 12 to 17 through every run, so runs alternate between builds,
each in a fresh process.

## Where the time went

On a directory of agent worktrees (8.95 million files, 1.33 million
directories, 7.8 million of the files APFS clones) clone accounting made
a scan 17 s slower: 44 s against 27 s without it. Nearly all of it was
system time (455 s against 277 s), and none of it was the per-file
private-size reads: the per-scan cache already held those to 206 thousand
for 7.8 million clones, and skipping them altogether saved under 2 s.

The cost was in the directory listing. `getattrlistbulk` was asked for
three clone attributes; asking for each alone over the same tree (wall
time of the whole `tree_memory` run):

| asked for | run |
| --- | --- |
| none | 26.8 s |
| `ATTR_CMNEXT_REALDEVID` | 27.0 s |
| `ATTR_CMNEXT_CLONEID` | 28.0 s |
| `ATTR_CMNEXT_EXT_FLAGS` | 42.8 s |
| all three | 45.0 s |

`ATTR_CMNEXT_CLONE_REFCNT` costs what the flags do. Both cost only for
files that were ever cloned (about 1.5 us of wall time each), and the
cost grows with the threads listing at once: 3 us of system time a clone
on 4 threads, 7.6 us on 8, 20 us on 16. A lookup the kernel serializes,
which listing with fewer threads does not avoid.

## Why the flags can be read once a group

Apple documents the grouping. getattrlist(2) says `ATTR_CMNEXT_CLONEID`
"uniquely identifies the data stream associated with the file system
object. Useful for finding which files are pure clones of each other (as
they will have the same clone-id)", and `EF_SHARES_ALL_BLOCKS` means the
file "is a full clone". The Apple File System Reference (2020-06-22)
gives the stream's side: an inode's `private_id` is "the unique
identifier used by this file's data stream", extents belong to that
identifier, and the reference count lives in the stream's own record
(`j_dstream_id_val_t.refcnt`). "May share" is per inode
(`INODE_WAS_CLONED`, `INODE_WAS_EVER_CLONED`), but a file whose stream
another file uses was either cloned or made by cloning.

No document says the flags agree across a group in so many words, so a
standalone probe checked it. It listed every file with the group, the
flags and the reference count:

| tree | files | groups | groups whose files disagree |
| --- | ---: | ---: | ---: |
| agent worktrees | 8,946,147 | 1,055,645 | 0 |
| a home Library | 2,180,829 | 1,625,470 | 0 |
| a folder of checkouts | 371,930 | 300,332 | 0 |

"Shares all blocks" was set on exactly the files whose stream is shared
(reference count of two or more), and every such file also had "may
share". A modified clone gets a stream, and a group, of its own.

## What changed

* **Listing.** A directory whose parent's files were clones two to one or
  more is listed with the group only. A file then takes its flags from
  its group when the scan has sampled one of its files (the cache keeps
  a sample of every group that shares all its blocks, and only of
  those), or reads its own in the read that measures its private size,
  which most clones made anyway. A file never cloned costs one such read
  where the flags cost it nothing, about twice what the flags cost a
  clone, hence two to one. Other directories list the flags as before.
* **Records.** Files of a group whose record says the same share it: the
  tree kept 8.1 million 16-byte records for 129 thousand groups.
* **Sharing pass.** Near the top of the tree each worker folded runs of
  subdirectories into maps of its own, and with the same clones in every
  worktree each held nearly every group: 18 copies of 129 thousand.
  Subdirectories are now merged into their parent as each finishes.

## Results

Scan time and the scan's peak physical footprint (`tree_memory`), three
interleaved rounds:

| tree | build | scan | peak | tree |
| --- | --- | --- | --- | --- |
| worktrees, 8.95 M files | before | 43.9 / 44.0 / 43.9 s | 1594 / 1587 / 1536 MiB | 1049 MiB |
| | after | 31.5 / 31.2 / 31.4 s | 1279 / 1280 / 1310 MiB | 932 MiB |
| | clone accounting off | 26.6 / 26.2 / 26.4 s | 969 / 971 / 967 MiB | 854 MiB |
| home Library, 2.18 M files | before | 13.0 / 13.1 / 13.1 s | 508 / 508 / 513 MiB | 270 MiB |
| | after | 11.6 / 12.4 / 12.2 s | 510 / 507 / 502 MiB | 262 MiB |
| checkouts, 372 k files | before | 0.97 / 0.97 s | 116 / 116 MiB | 41 MiB |
| | after | 0.95 / 0.95 s | 117 / 114 MiB | 40 MiB |

On the worktrees clone accounting now adds 5 s instead of 17.5 s, and
320 MiB instead of 590 MiB.

## Same figures

`tree_digest` now hashes every sharing figure (private and duplicate
bytes, unknown files, both estimate bounds, truncation) and each file's
allocation record. On a contrived tree of 880 thousand files (a source
of 20 thousand, 40 `cp -c` copies of it, modified clones, hardlinks and
files never cloned) it gives the same digest before and after, with
hardlink de-duplication on and off. The real trees change while they
are scanned, so they are not compared this way.

Each change also has a test against a reference:

* **Listing without flags, against listing with them** (a model): 2,000
  random sets of clone groups, each file resolved both ways with one
  cache. Every file reads the same, and files that may share are read no
  more often (`macos::resolve_tests`).
* **The same, on real APFS** (a scenario): worktrees cloned with `cp -c`,
  with a modified clone, a file of a worktree's own, a hardlink and a
  resource fork, scanned as listings choose and with flags in every
  listing. Every node's figures agree, and some listing left the flags
  out (`worktrees_scan_the_same_with_flags_in_every_listing`).
* **The sharing pass and shared records** (oracles): 300 random trees
  deeper than the parallel levels, with clones across directories,
  hardlinks and followed links. Every node's figures and every estimate
  of one or two nodes agree on 2, 4 and 8 workers with one worker, and
  with every record kept whole. Every directory also reads as one flat
  directory of all its files would, an answer that never goes through
  merging subdirectories (`sharing::tests::random_trees_*`).

Each was checked by breaking what it guards: treating a sampled group
as only maybe shared, re-reading a file already read, keying shared
records without the reference count, keeping a group's last size
instead of its smallest, losing a subdirectory's figures, and a scan
whose listings all keep the flags each fail one.

Two differences are possible, neither on an unchanging tree:

* A file that cannot be read again by path (removed since it was listed)
  in a directory listed without flags counts as a clone of unknown
  private size; with flags, only a file flagged "may share" would. That
  side never overstates a saving.
* Past one million clone groups, which groups the sharing pass keeps
  depended on how its work was split, and now on which subdirectory
  finishes first. The figures are marked partial either way.

## Reproduce

```sh
cargo run --release -p disktree-core --example tree_memory -- PATH
cargo run --release -p disktree-core --example tree_memory -- --no-clones PATH
NODEDUP=1 cargo run --release -p disktree-core --example tree_digest -- PATH
cargo test -p disktree-core a_listing_without_flags
```
