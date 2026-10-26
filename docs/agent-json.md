# disktree JSON reports

`disktree scan PATH --json`, `disktree summary PATH --json` and every tool of
`disktree mcp` print the objects described here. The MCP tools return the same
object twice: as `structuredContent`, and pretty-printed as the text content.

The report is built for a reader that should not have to do arithmetic.
Every size comes as exact `bytes` and human `size`, with its `percent` of the
scanned total; every time also comes as `age_days`; and every breakdown is
already summed. Lists that overlap say so (`nested_in`), and lists that are
meant to be added up are built not to overlap.

The format is versioned by `schema_version`. It is raised only when a field is
removed or changes meaning; new fields can appear without a bump, so ignore
fields you do not know.

Conventions:

- `bytes` is exact; `size` is binary units: `1.5 GiB`, `523 MiB`, `12 B`;
- `percent` is of `total.bytes` unless the field name says otherwise, rounded
  to one decimal, so a column of them can sum to 99.9 or 100.1;
- times are RFC 3339 in UTC (`2026-10-11T09:30:00Z`); `age_days` is whole
  days before the report was made. Both are `null` when unknown;
- for a directory, `modified` and `age_days` describe the newest write
  anywhere beneath it: a directory 200 days old has had nothing changed
  inside it for 200 days;
- paths are absolute, with `/` on macOS and Linux. A name that is not valid
  Unicode is shown lossily (`�`), so treat such a path as a label, not
  something to open.

## Scan report

Keys appear in this order: the short answers first, the long lists last.

| key | present | what it holds |
| --- | --- | --- |
| `schema_version`, `generator` | always | format version; `{name, version}` of disktree |
| `host` | SSH scans | the server the tree was scanned on, for a report exported from a window showing an SSH scan. Absent for this computer |
| `root` | always | the scanned directory, canonical (on `host` when present) |
| `scanned_at`, `scan_seconds` | always | when the walk finished, and how long it took. The MCP server reuses a scan for five minutes, so `scanned_at` can be older than the call |
| `options` | always | `size_mode` (`on_disk` like `du`, or `apparent` like `ls -l`), `one_filesystem`, `include_hidden`, `follow_links`, `exclude`, `top`, `depth` |
| `filters` | always | the filters the lists were made with; `older_than` is the resolved cutoff time |
| `total` | always | `bytes`, `size`, `files`, `directories` beneath the root |
| `errors` | always | `unreadable_paths` (count), up to 10 `examples`, and a `note` when the count is not zero. Those paths are counted nowhere |
| `volume` | overview, reclaim | the disk the root is on |
| `breakdown` | overview | three non-overlapping views of `total` |
| `reclaimable` | overview, reclaim | space that is likely safe to free, counted once |
| `largest_dirs` | dirs | biggest directories, filtered |
| `largest_files` | files | biggest files, filtered |
| `matched` | always | how many entries passed the filters, listed or not |

Which parts are present is chosen with `--only overview,reclaim,dirs,files`
(default: all). The MCP tools choose for you: `scan_path` is everything,
`reclaimable_space` is `volume` + `reclaimable`, `largest_dirs` is the
directory list, and `largest_files` and `list_files` are the file list.

### `volume`

```json
"volume": {
  "capacity": { "bytes": 994610155520, "size": "926 GiB" },
  "used": { "bytes": 608242950144, "size": "566 GiB" },
  "available": { "bytes": 386367205376, "size": "360 GiB" },
  "used_percent": 61.2,
  "available_percent": 38.8,
  "root_percent_of_used": 22.6
}
```

`root_percent_of_used` is how much of the volume's used space this tree is.
`null` if the volume cannot be measured, and always for an SSH scan: the
server's disk is not this computer's to measure.

### `breakdown`

Three ways of dividing `total.bytes`. In each, the parts do not overlap and
their `bytes` add up to `total.bytes` exactly. They describe the whole tree:
`depth` and the filters do not apply.

```json
"breakdown": {
  "note": "Each list divides total.bytes into parts that do not overlap; …",
  "children": [ { …entry… }, … ],
  "children_rest": { "count": 61, "bytes": 2040109056, "size": "1.9 GiB", "percent": 0.5, "files": 40112 },
  "by_category": [
    { "category": "other", "bytes": 214748364800, "size": "200 GiB", "percent": 51.6, "files": 2210345 },
    …
  ],
  "by_age": [
    { "age": "under_30_days", … },
    { "age": "30_days_to_6_months", … },
    { "age": "6_months_to_1_year", … },
    { "age": "over_1_year", … },
    { "age": "unknown", … }
  ]
}
```

- `children`: the root's direct children, the largest `top` of them, as
  entries (below). `children_rest` totals the ones not listed, so
  `children` + `children_rest` is the whole root.
- `by_category`: bytes and files by kind of data, largest first, kinds with
  nothing left out. A folder's kind is what its own name says (`node_modules`
  and `Caches` are `cache`, `.git` is `git`, `Movies` is `media`), or else
  the kind of the folder holding it; files take their folder's kind. An
  unrecognised top-level folder is `other`. (The window goes further and
  colours, say, `~/Library` by its largest recognisable child; that makes a
  good colour and a wrong total, so reports do not.)
- `by_age`: bytes and files by last write, always these five rows in this
  order.

### `reclaimable`

Space disktree recognises as regenerable or disposable: caches, build
output, dependencies beside their manifest, package stores, trash, temporary
files and logs. Each place is counted once, at its outermost folder, so
nothing is counted twice and `by_reason` adds up to `bytes`. It describes the
whole tree: `depth` and the filters do not apply.

```json
"reclaimable": {
  "bytes": 90301702144,
  "size": "84 GiB",
  "percent": 65.8,
  "places": 785,
  "available_after": { "bytes": 476668907520, "size": "444 GiB", "percent_of_capacity": 47.9 },
  "note": "… A hint to weigh, not a promise: check before removing anything.",
  "by_reason": [
    {
      "reason": "reinstallable",
      "bytes": 78734090240,
      "size": "73 GiB",
      "percent": 57.4,
      "places": 103,
      "description": "Installed dependencies beside their manifest; reinstall with the project's package manager."
    }
  ],
  "largest": [ { …entry… }, … ]
}
```

- `places`: how many outermost folders.
- `available_after`: the volume's available space if all of it were freed.
  `null` unless the bytes are what freeing would give back to this one
  volume: it needs on-disk sizes (not `apparent`, which overstates sparse
  files), one filesystem, and links not followed (a followed link can lead
  to another volume).
- `by_reason`: largest first, with one plain-language `description` per
  reason. Reasons: `regenerable`, `sync-history`, `package-store`,
  `build-output`, `reinstallable`, `sandbox-layers`, `snapshots`, `trash`,
  `temporary`.
- `largest`: the biggest `top` places, as entries.

### Entries

`largest_dirs`, `largest_files`, `breakdown.children` and
`reclaimable.largest` all hold entries:

```json
{
  "path": "/Users/me/Library/Developer/Xcode/DerivedData",
  "name": "DerivedData",
  "type": "directory",
  "bytes": 55834574848,
  "size": "52 GiB",
  "percent": 13.4,
  "modified": "2026-10-10T18:02:11Z",
  "age_days": 1,
  "depth": 4,
  "category": "toolchain",
  "reclaim": "build-output",
  "files": 210554,
  "unreadable": false,
  "nested_in": null
}
```

| field | meaning |
| --- | --- |
| `path`, `name` | where it is |
| `type` | `directory` or `file` |
| `bytes`, `size`, `percent` | for a directory, everything beneath it. A hardlinked file is counted once, so a second name of it has `0` |
| `modified`, `age_days` | last write; for a directory, the newest write anywhere beneath it |
| `depth` | levels below `root`; its children are `1` |
| `category` | kind of data, as in `breakdown.by_category` |
| `reclaim` | the reason it is reclaimable, or `null`. Inherited: everything inside a cache is reclaimable too |
| `files` | directories only: files beneath it |
| `unreadable` | directories only: it could not be listed, so its size is a lower bound |
| `nested_in` | `largest_dirs` only: the nearest directory **also in this list** that contains it, or `null`. Entries with a `nested_in` are already counted in that directory: never add them to it |

### `largest_dirs` and `largest_files`

The biggest `top` matching entries, largest first. The root itself is never
listed; `largest_files` holds regular files only. `depth` limits both lists to
entries at most that many levels below the root, files included, so a small
depth can hide large files deeper down. The filters apply to both:

| CLI | MCP argument | passes when |
| --- | --- | --- |
| `--min-size 1G` | `min_size: "1G"` or bytes | at least that size. `K`, `M`, `G`, `T` with or without `B` or `iB` are all binary: `1GB` is 1024³ bytes |
| `--ext iso,dmg` | `extensions: ["iso", "dmg"]` | files only: the name ends in `.iso` or `.dmg` (any case; `tar.gz` works) |
| `--older-than 6mo` | `older_than: "6mo"` | last written before then. `s`, `min`, `h`, `d`, `w`, `mo` (30 days), `y` (365 days), or a date `2025-01-31`. An unknown time never passes |
| `--name '*backup*'` | `name: ["*backup*"]` | the name matches, or the path if the pattern has a `/` |
| `--category cache` | `category: "cache"` | of that kind. Directories: only the outermost match is listed |
| `--reclaim any` | `reclaim: "any"` | reclaimable for that reason, or any. Directories: only the outermost match is listed |

Patterns, for `--name` and `--exclude`: `*` matches within one path component,
`**` across them, `?` one character. Case-sensitive.

### `matched`

```json
"matched": { "directories": 5541, "files": 17, "file_bytes": 70228426752, "file_size": "65 GiB", "file_percent": 16.9 }
```

Every entry that passed the filters and `depth`, listed or not.
`directories` is present when directories were listed, the `file_*` fields
when files were. `file_bytes` totals the matching files, which never
overlap; directory bytes are not summed, since nested directories would be
counted more than once.

## Summary

`disktree summary PATH --json` and the MCP tool `disk_summary`. Nothing is
scanned; this asks the volume.

```json
{
  "schema_version": 1,
  "generator": { "name": "disktree", "version": "0.11.0" },
  "path": "/Users/me",
  "volume_root": "/",
  "device": "/dev/disk3s5",
  "capacity": { "bytes": 994610155520, "size": "926 GiB" },
  "used": { "bytes": 588366356480, "size": "548 GiB" },
  "free": { "bytes": 406243799040, "size": "378 GiB" },
  "available": { "bytes": 406243799040, "size": "378 GiB" },
  "used_percent": 59.2,
  "available_percent": 40.8
}
```

`available` is what an unprivileged user can still write; `free` can be larger
where the filesystem reserves blocks for root. `device` is the mount source
where the platform reports one.
