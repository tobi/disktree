# Property, model and oracle tests

Most PwrDisk tests are **example tests**: build one tree, mark one path,
write down the number you expect. They check only the cases someone thought
of, and they check what the author *believed*. In this repo's history the
expected output was sometimes the bug, and the test kept it in place (table
below).

The three ideas here let the computer choose the inputs. We use
[proptest](https://docs.rs/proptest) inside `cargo test`.

## The three words

**Property.** A rule that must hold for *every* input, written as code.
proptest generates hundreds of inputs. When one breaks the rule, it
*shrinks* that input to the smallest one that still fails and prints it.

> *"Every directory a search keeps is reachable from the root through kept
> parents."* Against the code before #34, this shrank to one empty
> directory, `A/b`, searched for `b`: the count said 1 and the treemap
> showed nothing.

**Model.** For code with state, a small stand-in that is obviously correct.
The test generates random sequences of commands, such as mark, mark anyway,
open Review, type the phrase, commit, agent preview. It runs each sequence
on the real code and on the model, and checks that they agree after every
step.

> Rule: *no disk loses more than the limit unless the person confirmed that
> disk's share.* Against the code before #103, it shrank to four commands:
> mark 101 bytes on a USB disk anyway, open Review, type the phrase, commit.
> The USB disk lost 101 bytes of 2,000 against a 100-byte limit. Review
> asked for one share, "trash 11% of my disk", for both disks.

**Oracle.** Whatever tells the test what the right answer is. It can be one
of three things:

- **The filesystem itself.** Build the tree in a temp dir and compare
  PwrDisk's answer with `du`, `stat`, `find`, `df` before and after a real
  deletion, or `cp -c` clones and `hdiutil` disk images on macOS.
- **A slow, obviously correct version** of the function, such as a
  recursive walk with an inode set, or a guard that judges by `(dev, ino)`
  instead of by spelling.
- **A second code path** that must give the same answer, such as one worker
  against twelve, or the canonical path against a symlink to it. Comparing
  two paths is called a *differential* test.

> *"Deleting the selection frees at least the reclaim estimate's lower
> bound."* Checked with real `cp -c` clones and `df` on an APFS image, HEAD
> promises 4 MiB, exact, and deleting frees 2 MiB.

The three work together. A property needs an oracle to say what "correct"
means, and a model test usually uses the filesystem as its oracle.

## What the history showed

Each test below ran against the code from just before the fix, extracted
with `git archive` or `git show` into a scratch directory, and passed on
the fix and on `main`. "Runs" is the number of generated cases to the first
failure (min/median/max over 20 seeds).

| Fix | Bug | Test | Found in |
| --- | --- | --- | --- |
| #76, #97 | The removal guard refused everything under `/private/tmp` (38 days, v0.10.0–v0.14.0), then allowed `/Users/<other>` (v0.10.0–v0.16.0). Invariant 4 has had 14 fixes in under six weeks, 4 of them undoing an over-refusal an earlier fix caused. | Oracle: a guard judged by `(dev, ino)` on generated trees with links | Runs 1/1/4 and 1/3/8 |
| #103, #104 | #97 introduced both: one disk's confirmed share applied to every disk, and an agent's worker ran at 100% when the window's limit was Off. Fixed 2.5 hours later. A test asserted one share for one disk, so it could never fail. | Model: 17 kinds of command over three disks | #103 median 440 runs; #104 shrank to 5 commands |
| #92 | A removal worker that could not start left the window on Removing forever (40 days, v0.9.0–v0.16.0). #90's test asserted the silent empty-path item 53 minutes earlier. | Model on real files, with a real thread limit | Runs 1/3/10 |
| #10 | Files in a folder the scan could not read were dropped, and the root still said its totals were complete (35 days). The test checked the flag on the locked folder only. | Oracle: a slow walk, `find`, `stat` and `du` on generated trees | Runs 1/2/11; shrunk to `chmod 000 a` |
| #34 | A search matching only an empty directory counted 1 and showed nothing (37 days, eight releases). | Property with a slow oracle | Runs 2/26/117; shrunk to `A/b` |
| #5 | Two marks differing only in case both became targets, and the child was counted twice. | Differential against an O(n²) nesting check | Shrunk to `["a", "A/a"]` |
| #19 | Linking a window to earlier MCP marks moved the window's root. The test fake asserted "the fake has one tree", so it couldn't see the move. | Model with a real socket and a fake window | Runs 1/1/3 |
| #58, #29 | Clone accounting claimed more than it could measure: repeated bytes above shared bytes, and MCP saying "Frees 1.0 MiB" when deletion freed 0. | Property; oracle with `cp -c` and `df` | Median 246 runs; median 1 run |
| #70, #72 | The main window opened 48 points off a 1512-point screen, and new windows opened on the primary display, not the one in use. | Property over random display layouts | Runs 1/2/5 and 1/3/22 |
| #87 → #88 | Unbounded `git status` probes, then a cut probe that left `git-lfs` running. #88 fixed #87's cut 95 minutes later. | Model; oracle with `kill -0` | Runs 1/1/2; 5 of 5 runs |
| #31 | Mount sources printed as `My\040Share`. The mount *point* half was fixed earlier by a separate commit. | Property: decode(encode(x)) == x | Runs 1/1/3 |

Still on `main` when this was written, found by running the same tests
against it: a scan root spelled through a symlink (`/var`,
`/Volumes/Macintosh HD`, `/tmp`) gets past the system-tree, profile and
mount guards, and crosses into a disk mounted inside it. The MCP tests
mark files under `/var/folders/…`, which the guards refuse under its real
name, `/private/var/folders/…`. They pass because of the bug. The same
run found an APFS reclaim estimate that promises 4 MiB where deleting frees
2 MiB, and marks that forget an earlier removal inside them.

## When to write one

- **Fixing a bug.** Write the rule the bug broke as a property or model
  invariant, and confirm it fails on the pre-fix code before you fix it.
  Also keep the shrunk counterexample as a plain `#[test]`, so the
  regression has a name.
- **Anything that deletes, or refuses to** (`removal.rs`,
  `removal_limit.rs`, `mark_guard.rs`). Write an oracle that judges by
  identity: `(dev, ino)` from `stat`, the real mount table, the real home.
  Generate the same directory under every spelling it can have: through a
  symlink, `/tmp` and `/private/tmp`, the Data volume, a trailing `/`,
  another case, NFD. The verdict must not change.
- **Sizes and totals** (`scan.rs`, `tree.rs`, `sharing.rs`, `space.rs`).
  The filesystem is the oracle. Generate the shapes fixtures rarely build:
  - hardlinks inside and across directories, and to a file outside the root;
  - symlinks to files, to directories, in loops, and dangling;
  - sparse and zero-byte files;
  - APFS clones (`cp -c`), then one copy truncated, appended to or
    overwritten;
  - a disk image mounted inside the tree (`hdiutil` on macOS);
  - deep and wide directories, and odd names: `\`, newlines, emoji, NFC
    and NFD.
- **State over time:** marks across removals and rescans, the limit and
  Review's confirmation, the removal run, the git probe store, the link
  between `pwrdisk mcp` and the window. Write a model: a `Vec` of commands
  from proptest, applied to the real code and to a few lines of plain Rust.
- **Pure logic with rules:** treemap layout, filters, `PathStyle`, size
  formatting, window placement, mount table parsing. Write a property.
- **Two places answer the same question,** such as the canonical path and
  a link to it, one worker and twelve, `Draft` and `Tree::build`, Review's
  phrase and the worker's limit. Write a differential test that they agree.
- **Before an `assert_eq!` pins a refusal, a zero or a count,** say the
  rule in words. If the rule and the output disagree, the output is the
  bug.

## How to write them

- **Put the property beside the promise it checks,** in the module's
  `#[cfg(test)] mod tests` or a `tests/` file of the crate, as the house
  rules ask. `cargo xtask test` runs `cargo test --workspace`, so CI runs it
  on Linux, macOS and Windows with no extra wiring.
- **proptest is a dev-dependency** of the crate that uses it:
  `proptest = "1.12"` under `[dev-dependencies]`. 1.12.0 is already in
  `Cargo.lock` through gpui's test support, so adding it pulls no new crate.
  `licenses/about.toml` ignores dev-dependencies, so `THIRD_PARTY_LICENSES`
  does not change, but the `licenses` CI job still runs because a
  `Cargo.toml` changed.
- **Test code is linted.** `cargo xtask lint` runs clippy with
  `--all-targets`, so pedantic lints apply to properties and generators
  too.
- **Build inputs the way the filesystem does.** Generate a list of
  operations (make directory, write file, hardlink, symlink, `cp -c`,
  mount, mark, remove, rescan) and apply them to a `tempfile::tempdir()`,
  not random structs. Don't generate states the system can't produce:
  APFS never reports a clone group member with private bytes, for example.
  Impossible states give false failures.
- **Test both spellings of a temp root.** macOS temp dirs live under
  `/var`, a link to `/private/var`. A fixture that uses only one spelling
  sees only one verdict: the MCP fixtures pass under `/var/folders` because
  the guards compare spelling, and #58 moved its fixture into the checkout
  to get away from `/private`. Run the property under both.
- **Take the property from a promise:** the invariant in `AGENTS.md`, a
  doc comment, `docs/guide.md`, or what the user wants. Never take it from
  today's output.
- **Narrow, don't weaken.** A counterexample may be a case the code handles
  on purpose, such as PiB being the last unit or a hardlink whose other
  name survives freeing nothing. If so, restrict the generator or add
  `prop_assume!`, with a comment saying why. Don't loosen the assertion
  until it passes.
- **Budget the runs.**
  - Pure properties: 256 cases, and a test under about a second in debug.
  - Real-filesystem oracles: 32 to 64 cases. Disk images (`hdiutil`) cost
    about a second each, so gate them to macOS and keep them few.
  - Raise `PROPTEST_CASES` locally when hunting.
- **Don't commit replay files.** Set `failure_persistence: None` in the
  `ProptestConfig` so a failure writes no `proptest-regressions/` file.
  Copy the shrunk input from the failure message into a named `#[test]`.
- **A property failure on an unrelated PR is a real bug.** Reproduce it from
  the printed input and fix it or file it. Never re-run CI to get green.
- **Removal tests remove only inside their temp dir.** The guards apply to
  tests too; use permanent mode so nothing reaches the real Trash.
