# PwrDisk accessibility audit — 2026-10-31

Tracking: [PWR-27](https://linear.app/pwrdrvr/issue/PWR-27).

## Result and scope

**PwrDisk's core graphical workflow is not independently usable through
the inspected macOS accessibility tree.** Controls from GPUI Base expose
some useful semantics, but the file tree, selected-path details, marked
list, document text and much of the application state are missing. This
audit records failures and explicit validation gaps; it is not a claim of
WCAG conformance or a successful three-platform screen-reader test.

The audited production source is **v0.14.0,
`4d68f6cb3ca6742529faf3f4436278da11d486d2`**. The audit PR adds only this
report, a fixture generator, test-only contrast measurements and keyboard
regressions. It does not change the production UI or port upstream UI.
Re-run against a new commit after remediation; do not carry these outcomes
forward as proof about a later build.

Repository guidance and the full PWR-27 description were read first. Its
comments were empty. The earlier history investigation, PwrRoute audits
and PwrDisk PR #26's cleanup-advice inspection are not accessibility
evidence for this audit.

## Environments and evidence limits

| Environment | Build and tools | Result and limitation |
| --- | --- | --- |
| macOS | 27.0.1, build 26A434, Apple Silicon; local `cargo build --locked -p disktree-app`; debug v0.14.0; native CUA AX inspection and keyboard input | Native AX and selected keyboard/visual scenarios tested. The isolated `/tmp/PwrDisk-A11y.app` launcher executes this worktree's binary. An already-running installed PwrDisk was inspected only to identify it, then left untouched; its tree is not baseline evidence. |
| VoiceOver | System VoiceOver app; version not obtained | **BLOCKED.** Cmd-F5 did not produce a running VoiceOver. Direct launch through computer use timed out with `-10005: timeoutReached`; subsequent app inventory reported `isRunning: false`. No spoken output, rotor, speech cadence or VoiceOver navigation result is claimed. |
| Windows | Discovered WindowsVM peer: Windows 11 build 26200, x64 | **BLOCKED.** Peer was disconnected. No audited Windows binary, UIA snapshot, Narrator/NVDA version or runtime result. A toolkit Windows adapter in source is not Windows verification. |
| Linux | Dedicated Sandboxer `sb_ypcbvzmzum`, Ubuntu 24.04.5 LTS, x86_64, kernel 6.18.56-sandboxer; base `pwrdisk-base-us-east`, us-east | Build, lint, harness tests and isolated SSH integration ran at the exact baseline commit. These are not native AT tests. |
| Orca / AT-SPI | Same Linux VM | **BLOCKED.** `DISPLAY` and `WAYLAND_DISPLAY` unset, no X11 socket, `command -v orca` empty, no installed Orca/pyatspi version. Xvfb and dbus-run-session exist, but there is no usable desktop/AT session in the available controller. Neither X11 nor Wayland screen-reader behavior was verified. |

Native AX is direct semantic evidence, not a substitute for speech testing.
Harness focus is GPUI focus, not VoiceOver focus. The controller consistently
reported the native window as AX focused even after application keyboard
selection changed. A missing AX focused item is a finding; it does not prove
that no internal focus handle exists.

Live inspection used a 2880×1800 capture (1440×900 logical surface), default
dark appearance, 100% UI zoom and the supported maximum 175% UI zoom.
Light appearance and small/zoomed layouts have harness/source coverage as
specified below. OS display/text scaling and OS high-contrast modes were
not changed or verified. Screenshots include the real volume meter even
with a contrived scan, so live captures were not published as wholly
contrived evidence. No scan of the user's home was initiated.

## Reproduce

Use a fresh worktree/destination, with no real data in the fixture. Python
3 is required. The generator refuses an existing root and never removes
or overwrites an existing tree.

```sh
# Start on the audit PR's checkout to use its fixture generator.
python3 scripts/a11y-fixture.py /tmp/pwrdisk-a11y-new
git checkout --detach 4d68f6cb3ca6742529faf3f4436278da11d486d2
cargo build --locked -p disktree-app
target/debug/pwrdisk --apparent-size
```

On Windows choose an unused temporary path and run
`target\debug\pwrdisk.exe --apparent-size`. On macOS `/tmp` resolves to
`/private/tmp`; compare canonical paths. The default fixture has 135 files,
including hidden data, an empty file/directory, nested cache data, 128 small
files, a long name and `Résumé 日本語.txt`. Payloads and timestamps are fixed;
directory timestamps and allocated block sizes are not. Use apparent size
for comparable figures. The live root was `/tmp/pwrdisk-a11y-20261031`.
Use `--entries 50000` in a separate fresh fixture for a longer scan; the
default fixture completes too quickly for a live progress announcement
test. Permission-denied and successful SSH discovery require separate
controlled fixtures; do not infer their AT behavior from this local scan.

For each native run record the commit, binary/build profile, OS/AT version,
theme, logical dimensions, display scale and UI/text scale. Inspect with
VoiceOver/native AX, Narrator or NVDA/UIA, and Orca/AT-SPI respectively:

1. Launch without a path. Traverse forward/backward with Tab/Shift-Tab;
   inspect host/path field names, values, root rows, selected source,
   disabled states and instructions. Try arrows and Escape. Reopen with
   `v`. Enter `-audit-invalid` in the host field and Return: validation
   must occur without an SSH connection. Compare the visible error with
   the accessibility tree and announcements.
2. Enter only the fresh fixture path in the path input and Return. In the
   observed build this required a pointer click to seed field focus;
   keyboard-only startup did not provide a working Tab route. Observe
   scan progress, indeterminate/busy state, Escape cancellation and retry
   using a larger fixture. Read progress/error/completion without a stream
   of sampled-path announcements.
3. Explore with Tab/Shift-Tab, arrows, Enter and Backspace; filter with `/`.
   Compare names, sizes, hierarchy, current root, selection and active
   descendant with the visual result. Mark using Space, review with `c`,
   change modes with `m`/`p`, return with Escape. Check marked/blocked
   reasons and free-space projections. **Do not press Enter or an approval
   key in live Review; do not activate its removal control.**
4. Open Help with `?`, inspect all shortcuts and modal boundaries, try
   background navigation keys, dismiss with Escape. Open Settings with
   Cmd+, on macOS (Ctrl+, elsewhere); inspect every pane and each copy
   action. Open Changelog, License, Third-Party Notices and Logs. Read,
   search, select, scroll and copy without a pointer. Close auxiliary
   windows and check focus restoration. Do not change release channels or
   removal limits as part of reading Settings.
5. Check native menus on macOS; on Windows/Linux test F10/Alt, arrows,
   activation and Escape. Check disabled items, submenu order and focus
   return. Repeat relevant screens in light/dark/high-contrast appearance,
   at minimum window sizes and large UI/OS text/display scale, with long
   paths, and with reduced motion enabled.

Removal confirmation and destructive outcomes are **harness-only** here.
Existing tests operate on temporary trees and verify unmarked neighbors.
Any future interactive destructive test needs a human operator; never use
computer use, AX, UIA or AT-SPI to approve removal.

## Audit matrix

`PASS` means only the stated assertion passed. `FAIL` means an observed or
source-demonstrated defect. `BLOCKED` means an unavailable runtime or
controller capability. `NOT TESTED` identifies an unexercised case.
Evidence: **M** = live macOS native AX/keyboard/visual observation;
**H** = GPUI window harness (macOS and Linux); **S** = source inspection;
**C** = computed production-color diagnostic. H/S/C do not establish native
screen-reader interoperability. **Every native Windows and Linux row below
is BLOCKED** for the environment reasons above, including menu, contrast,
scale and motion runtime checks. Spoken VoiceOver results are also BLOCKED
for every row. See [PWR-36](https://linear.app/pwrdrvr/issue/PWR-36).

| Area / promise | macOS native semantics / visual result | Keyboard / additional evidence | Finding |
| --- | --- | --- | --- |
| Startup has no implicit scan | NOT TESTED through AT | PASS H: `startup_chooser_never_scans_before_explicit_acceptance` | Preserve |
| Source input names, values and relationships | FAIL M: two unnamed text fields; entered host value not observable in AX snapshot | FAIL M: repeated Tab/Shift-Tab left native focus on window; pointer needed for path input | F2, F3 |
| Mount/source choices and reading order | FAIL M: visible mount rows and source explanation absent; no selected root row exposed | PASS H: arrows and explicit acceptance; full keyboard reachability FAIL M | F2, F3 |
| Source modal containment/restoration | FAIL M: background toolbar still exposed, no dialog in AX snapshot | PASS H: background mark/review/filter/rescan keys contained; Escape restores application root focus | F3 |
| SSH invalid destination/error | FAIL M: visible `invalid SSH destination`, unchanged AX | PASS H: invalid host cannot start scan/change existing source; M field activation worked after pointer focus | F2 |
| Successful SSH chooser and authentication failures | NOT TESTED M/AT | PASS H: simulated remote source/error cases; PASS Linux SSH integration, not native AT | PWR-36 |
| Scan progress, busy, cancel and completion | NOT TESTED native progress: default tree completed before capture; FAIL S: progress/notice strings have no semantic status/progress nodes | PASS H: initial/widening scan Escape cancel and retry; preview/progress remain bounded and non-actionable | F2 |
| Explore file hierarchy and selection | FAIL M: no file/tree nodes, sizes, hierarchy or selected-item state | PASS M/H: Tab/arrows move visual selection; Enter/Backspace handled in harness | F1 |
| Filter / search | FAIL S: explore filter is custom key-fed text without input/result semantics | PASS H: typing, Enter and Escape; no native speech result | F1, F2 |
| Marks and review rows | FAIL M: marked paths, row reasons and summary text absent; Unmark button alone does not name the row | PASS M/H: Space and c open review; Escape returns | F1 |
| Review mode names/selected state | PASS M: `Move to trash` / `Delete permanently` radio buttons and selected state track m/p | PASS M/H: mode keys; no live approval attempted | Preserve |
| Removal alert names/description/actions | NOT TESTED native AX or speech | PASS H: open, contain background shortcuts, Tab/Shift-Tab remain in application dialog, Escape restores Review/root focus, files retained | F3; PWR-36 |
| Destructive completion and unmarked neighbors | NOT TESTED interactively by design | PASS H: existing permanent-removal and link tests operate on temporary trees | Preserve |
| Toolbar/control names and actions | FAIL M: `<`, `>`, `−`, `+` names lack purpose/context; radio/checkbox names and state present | PASS M: field/button activation where exercised; all safe control actions not individually tested | F2, F3 |
| Disabled-state metadata | FAIL M/S: visually disabled Scan controls still ordinary AX buttons; Base test records missing disabled flag | Click action removal in Base is not a disabled-state announcement; native disabled-action invocation NOT TESTED | F3 |
| Native macOS menus | PASS M: named File/Edit/View/Go/Window/Help headings exposed | BLOCKED: native Help menu click did not complete; controller required refresh, Ctrl-F2 made no observable change. Popup keyboard operation not verified | PWR-36 |
| Off-macOS app/menu popovers | BLOCKED native | FAIL S: custom clickable Div rows without roles; root menu handling only dismisses on Escape; full platform keyboard behavior NOT TESTED | F3 |
| Settings pane order/content | FAIL M: pane selectors/titles/descriptions absent; Tab changes panes with AX focus at window | PASS M/H: pane cycling, updates shortcuts; control-by-control traversal FAIL M | F3, F4 |
| About / copy names | FAIL M: two indistinguishable `Copy` buttons, no surrounding identity text | Safe actions exposed; complete keyboard-only copy path NOT TESTED | F4 |
| Help shortcut overlay | FAIL M: no added AX content or modal role; underlying controls remain | PASS H: background navigation/action keys contained, Escape restores operation; FAIL M: sheet clipped vertically | F3, F4, F6 |
| Changelog / License / Notices | FAIL M: only Copy and native window controls; no text, headings or document reading order | PASS M: close returns to Settings; Tab/PageDown did not expose document navigation; scrolling outcome NOT TESTED | F4 |
| Logs | FAIL M: unnamed search, no log text; Wrap/Mark/Live exposed as buttons without observed state semantics | PASS H: filtering/rotation/selection/copy; Cmd-F exposed no AX focused search item in live check | F3, F4 |
| Progress/error/selection/mark/free-space announcements | FAIL M/S for missing current readable state; speech/cadence BLOCKED | No measured announcement frequency; cannot claim throttling or silence is correct | F1, F2 |
| Light/dark text and non-text contrast | FAIL C: ratios below thresholds, table below | PASS H only means both appearances draw; light native appearance NOT TESTED | F5 |
| Color-independent meaning | PASS M/S: reclaimable hatch, visible Marked text, sizes, selection outline; FAIL S: color/fading carries age/filter state without equivalent semantic file content | Combined selected+marked, unreadable and all color-vision simulations NOT TESTED | F1, F5 |
| Main UI zoom / clipping | PASS M: zoom grows UI through 175%; FAIL M: Help clipped at 100% and 175%; 200% unsupported by configured steps | PASS H: chooser/progress bounds at small dimensions/large rem; all-screen minimum-size sweep NOT TESTED | F6 |
| Auxiliary zoom / OS scaling | FAIL S: main zoom handler not shared with auxiliary windows | OS text/display scale and high-contrast runtime checks NOT TESTED on macOS; BLOCKED Windows/Linux | F6; PWR-36 |
| Long/Unicode names and tiny targets | PASS M: Unicode name drawn; long tile name clipped; FAIL for accessible full-path alternative (F1) | Target pixel/spacing sweep NOT TESTED; small controls/tiles require measurement | F1, F6 |
| Reduced motion | FAIL S: custom LayoutTransition creation/sampling does not read `cx.reduce_motion()` | OS preference runtime toggle NOT TESTED; no claim that Base preference import disables custom animation | F7 |

### Native AX evidence excerpts

These are transcribed relevant portions, not complete raw dumps. Window
controls and menu headings are omitted below. Indices are transient.

```text
Source chooser:
  container
    button <
    button >
    radio button (selected, settable, boolean) Size
    radio button (selectable, settable, boolean) Files
    radio button (selectable, settable, boolean) Age
    checkbox (settable, boolean) Hidden files, Value: on
    checkbox (settable, boolean) Apparent size, Value: on
    button −
    button +
    button Power Efficiency: Balanced
    button This computer
    text field
    button Connect SSH
    text field
    button Scan directory
    button Cancel
    button Scan selected root
  focused UI element: standard window

Explore after Tab:
  toolbar controls as above
  button Open
  button Mark for removal
  button Source…
  no file nodes or selected path/size; focused UI element: standard window

Review after p:
  button Unmark
  radio button (selectable, settable, boolean) Move to trash
  radio button (selected, settable, boolean) Delete permanently
  button Save list…
  button Copy as prompt
  button Save delete script…
  button Copy delete script
  button Back
  button Delete 0 items…

License / Third-Party Notices / Changelog after frame settles:
  button Copy
  no document text; focused UI element: standard window

Logs:
  button Copy path
  button Reveal
  text field
  button Wrap
  button Mark
  button Live
  no log text; focused UI element: standard window
```

The live review mark was a hidden fixture directory held back by the plan;
the zero executable item count is not evidence of a deletion-count bug.
The held-back explanation was visible but missing from AX, which is the
finding. The harness separately marks a removable `junk` directory.

## Severity-ranked findings and remediation

High means a required task or essential information is unavailable to an
affected user. Medium means a serious preference/usability failure with a
potential alternative. Severity is task impact, not a conformance ruling.
Each issue is in the **PwrDisk** Linear project, linked under PWR-27.

| ID | Severity | Observed failure / reproduction | Remediation |
| --- | --- | --- | --- |
| F1 | High | Scan fixture, Tab/arrows, Space, c: visual files/selection/marks/review exist without corresponding accessible data | [PWR-29](https://linear.app/pwrdrvr/issue/PWR-29): semantic scalable file navigation, selection, marks and review; stable IDs and safe actions |
| F2 | High | Startup two unnamed inputs; `-audit-invalid` + Return shows an error absent from AX; progress/status lack semantic nodes | [PWR-30](https://linear.app/pwrdrvr/issue/PWR-30): named inputs/values/relationships, selected source, progress/errors/free-space status and measured announcement throttling |
| F3 | High | Tab does not traverse startup controls; overlays expose background controls; application focus is not an AX item; disabled metadata incomplete | [PWR-31](https://linear.app/pwrdrvr/issue/PWR-31): traversal, visible focus, modal/menu containment, useful restoration and disabled semantics |
| F4 | High | ? / Settings / documents / Logs expose action fragments but omit readable content and context | [PWR-32](https://linear.app/pwrdrvr/issue/PWR-32): structured text/panes/documents/logs, contextual names and keyboard reading/scrolling |
| F5 | High | Run contrast diagnostic: default palettes fail text/state thresholds; semantic alternatives to color-carried age/filter meaning absent | [PWR-33](https://linear.app/pwrdrvr/issue/PWR-33): production contrast thresholds and non-color state equivalents |
| F6 | High | Help overflows at 100% and 175%; main zoom capped at 175%, auxiliary windows lack shared zoom route | [PWR-34](https://linear.app/pwrdrvr/issue/PWR-34): scrolling/reflow/scaling, long paths and target-size measurements; verify capture-specific sidebar clipping independently |
| F7 | Medium | Source: descend/history transitions always create/sample LayoutTransition irrespective of reduced motion | [PWR-35](https://linear.app/pwrdrvr/issue/PWR-35): honor preference in custom navigation animation and test final geometry/history |
| G1 | High validation gap | VoiceOver unavailable, Windows peer disconnected, Linux without desktop/Orca; native scaling/menu/AT scenarios incomplete | [PWR-36](https://linear.app/pwrdrvr/issue/PWR-36): finish native platform and AT matrix, keep unobserved cells explicit |

### Contrast measurements

Run against the audit test code (using the unchanged production palettes):

```sh
cargo test --locked -p disktree-app audit_contrast_report -- --ignored --nocapture
```

The diagnostic computes sRGB relative luminance after alpha compositing
the text over an opaque fill. It samples all **11** categories, all **5**
age buckets and depths **0–4**. Numbers are minima per pair. Text uses a
4.5:1 reference; selection ring versus adjacent fill uses 3:1. Exit 0 means
the report ran, **not** that contrast passed. The output intentionally
reports known failures outside the normal regression gate.

References: W3C's [contrast minimum](https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum.html)
and [non-text contrast](https://www.w3.org/WAI/WCAG22/Understanding/non-text-contrast.html)
explain these thresholds. [Reflow](https://www.w3.org/WAI/WCAG22/Understanding/reflow.html)
and [target size](https://www.w3.org/WAI/WCAG22/Understanding/target-size-minimum.html)
inform the outstanding scaling/target checks. These are evaluation
references, not a claim that this native-app audit determines conformance.

| Pair | Tokyo Night | Flexoki Light | Required |
| --- | ---: | ---: | ---: |
| Normal tile name | **3.856 FAIL** | 6.162 PASS | 4.5 |
| Dim tile size | **2.083 FAIL** | **2.393 FAIL** | 4.5 |
| Selected ring versus tile fill | 3.610 PASS | **2.285 FAIL** | 3.0 |
| Marked name on marked fill | 5.460 PASS | **3.967 FAIL** | 4.5 |
| Secondary caption at 75% on background | 5.131 PASS | **3.872 FAIL** | 4.5 |
| Text on highlighted action | 8.547 PASS | **3.184 FAIL** | 4.5 |

These are flat-fill calculations, not raster measurements of glyph edges
or hatch intersections. Not every view uses every pair. Hover, filtered,
unreadable, combined marked/selected, disabled, borders, focus indicators,
custom Omarchy themes and OS high-contrast modes need additional rendered
checks. Distinct hue tests in the existing palette suite do not establish
contrast or color-independent meaning.

## Toolkit bridge investigation

The lockfile selects GPUI Kit/Base **0.6.6**, GPUI pre **0.3.6**, Omarchy
**0.1.3**, and AccessKit **0.24.1**. Adapter implementations exist for
macOS (`SubclassingAdapter`), Windows (`Adapter`, WM_GETOBJECT handling),
and Linux X11/Wayland (`accesskit_unix::Adapter`). macOS buttons/radio
buttons/checkboxes observed above demonstrate that the bridge is active.
It is incorrect to describe GPUI as having no accessibility support.

The GPUI accessibility guide requires IDs and roles; ordinary string
children are not automatically a readable document. PwrDisk chiefly uses
plain Div/string content and a painted canvas; inspection found only one
explicit app accessibility label override (Power Efficiency). The canvas
needs a semantic navigation path. GPUI supports synthetic accessibility
children, but their existence does not make this canvas accessible.
`debug_selector` identifiers used by harness tests are not accessible
names or roles.

There is also a concrete toolkit gap: GPUI Base's
`button::tests::accessibility_role_label_and_disabled_action_surface`
explicitly asserts `!disabled.is_disabled()` and explains that the GPUI
interface lacks a disabled-state setter even though AccessKit supports
one. Removing the Click action is not equivalent to conveying disabled
state. Input names/values/focus and modal semantics need investigation in
both the app and toolkit, without attributing every missing app role to
the platform bridge. Windows and Linux adapter correctness remains
unverified here. PWR-31 tracks the owning-layer work.

Relevant baseline source: [views](../crates/disktree-app/src/views.rs),
[state](../crates/disktree-app/src/state.rs),
[treemap](../crates/disktree-app/src/treemap_view.rs),
[Settings](../crates/disktree-app/src/settings.rs),
[documents](../crates/disktree-app/src/doc_window.rs),
[Logs](../crates/disktree-app/src/logs_window.rs),
[appearance](../crates/disktree-app/src/appearance.rs),
[palette](../crates/disktree-app/src/palette.rs), and
[scale tokens](../crates/disktree-app/src/ui.rs).

## Upstream remediation candidates

Inspected PR descriptions and full diffs on 2026-10-31; both were open.
Neither was built or run in this audit, and neither was imported.

* [tobi/disktree #51](https://github.com/tobi/disktree/pull/51), head
  `b7879541c4dfc709009725b376bc0d138a8f2e3c`: virtualized row view,
  expansion, arrows, shared selection/marks/filter and offscreen-row
  composition test. Useful groundwork for a scalable alternative to
  spatial tiles. The reviewed diff has no tree/list roles, accessibility
  item states/relationships or AT results. Virtualization must not make
  offscreen files unreachable to AT. Evaluate selected pieces in PWR-29,
  coordinated with [PWR-28](https://linear.app/pwrdrvr/issue/PWR-28).
* [tobi/disktree #58](https://github.com/tobi/disktree/pull/58), head
  `c9f454d5e8cac841be265c01f11eba18984060d5`: fixed themes, persistence,
  and alpha-composited contrast tests. Useful calculation/test patterns.
  Its `tile_names_stay_readable_on_every_fill` covers nine categories,
  normal labels, category/age/marked fills and theme variants. It does not
  cover PwrDisk's two extra categories, 50%-opacity size text, all status
  captions/actions, focus/state boundaries, high-contrast OS behavior or
  screen-reader usability. Evaluate narrowly in PWR-33; theme picker and
  persistence are independent product work.

## Regression coverage and verification

New `audit_` regressions in [tests.rs](../crates/disktree-app/src/tests.rs)
verify Help background-key containment and resumed marking, source-picker
background-key containment and focus restoration, and removal-dialog
background-key containment plus Tab/Shift-Tab containment and Escape
cancellation. The confirmation test never sends the final approval action.
Files and marks remain after cancellation. These preserve existing safety
and keyboard promises; they do not encode missing semantics as acceptable
behavior.

The palette suite adds a regression for alpha-composited contrast math
(black/white 21:1, transparent 1:1, half-white on black about 5.281:1) and
an explicitly ignored diagnostic for the table above. Future color fixes
need threshold assertions over corrected production colors. No production
fix is claimed by passing the math or keyboard tests.

Existing harness tests cover scan/cancel/retry, explicit chooser
acceptance, invalid SSH input, metric/selection preservation, filtering,
marks/review, removal guards, Settings, documents, Logs, both appearances,
and bounded chooser/progress layouts. They draw frames or inspect state,
not native AX/UIA/AT-SPI. The headless platform does not activate native AT;
native semantic regressions still need to be added with the remediation.

```sh
cargo test --locked -p disktree-app audit_ -- --include-ignored --nocapture
cargo xtask lint
cargo xtask test
```

Baseline Linux verification in the dedicated VM used
`scripts/sandboxer/verify.sh all`:

| Step | Exit | Seconds |
| --- | ---: | ---: |
| setup | 0 | 2 |
| lint | 0 | 16 |
| test | 0 | 30 |
| portable build | 0 | 84 |
| SSH integration | 0 | 7 |
| all | 0 | — |

Final audit-commit verification is recorded in the PR, including the
commit SHA and platform result. Documentation/test-only changes do not
have a before/after production window change. The audit publishes text
and numeric evidence; future visible remediations require contrived
before/after PR attachments under [the screenshot procedure](pr-screenshots.md).

## Remediation observations (separate from the v0.14.0 baseline)

### PWR-29: file navigation and review

Source: `0568e97cff3c29beebd3a70556e8f3de0765f93d`, debug macOS build,
2026-10-31. Binary SHA-256:
`98dfba4cde4a3c79b79c71cb86e0a2f7ae9d3918280855fdbd32cc0074e09a22`.
The original matrix above remains a record of the production baseline.

The new **Show accessible file list** control opens a directory-at-a-time
list over the completed scan. Right/Enter opens a directory, Left/Backspace
returns to its parent, Up/Down selects, Space marks and c reviews. Explicit
100-entry pages bound layout/AX work without silently dropping later entries.
Rows use lossless absolute-path identifiers; category, size, age, unreadable,
filter and mark context have text equivalents. Review pages expose all marks,
row explanations and the selected removal mode/recoverability summary.
This is a navigation alternative, not an expansion of the painted canvas.

| Promise | Native macOS AX / keyboard observation | Harness / source evidence |
| --- | --- | --- |
| File data and hierarchy | PASS for inspected fixture: list, full paths, directory/file kind, byte and formatted sizes, category, age and selected state exposed; Right/Left changed the named directory | PASS: nested absolute paths and filtering regression |
| Long / Unicode paths | PASS: full `Résumé 日本語.txt` and long-name path exposed under `documents` | PASS: 235 Unicode-named fixture files |
| Offscreen entries and counts | PASS: `many-small-files` reports 128 entries; PageDown exposes 101–128, selects `item-000100.txt`, and includes `item-000127.txt` | PASS: page boundaries at 99/100/200; bounded row composition; source supplies set position/count |
| Marks and review | PASS for inspected fixture: marked path, exact byte count, included/blocked/covered counts, mode and recoverability exposed; no live approval | PASS: existing removal guards and destructive harness tests; new safe-mark/navigation tests |
| Accessible identity / state | Stable path author IDs and selection updates observed | PASS: role, name, selected flag, author ID and non-UTF-8 collision regressions |
| Native focused item / speech | NOT VERIFIED: controller still reports the standard window as focused despite selected-state updates; no spoken-output result | Source provides a focusable list and active-descendant metadata; this is not native focus or speech evidence. PWR-31/PWR-36 remain open |
| Windows UIA / Narrator / NVDA | BLOCKED: WindowsVM still disconnected | No Windows runtime result |
| Linux AT-SPI / Orca | BLOCKED: dedicated VM has neither DISPLAY/WAYLAND_DISPLAY nor Orca | Linux checks are recorded separately in the PR |

Fixture: `scripts/a11y-fixture.py`, 135 invented files on a disposable APFS
disk image named **A11yFixtures**, root `/Volumes/A11yFixtures/Projects`.
macOS 27.0.1 / 26A434, Apple Silicon, dark appearance, 1440×900 logical,
100% UI zoom. The isolated Before/After launchers leave installed PwrDisk
untouched. Before uses audit head `12d2bef` (unchanged production UI);
contrived window-only before/after images are attached to the remediation PR.
OS scaling, high contrast and spoken screen-reader output were not tested.

macOS `cargo xtask lint`, `cargo xtask test` and debug build passed. Linux
Sandboxer `sb_uihqpdf98b`, Ubuntu 24.04.5 x86_64, verified the implementation
commit; final build/integration status is recorded in the PR. No native Linux
result is inferred from those checks.

Upstream #51 was re-evaluated narrowly: its virtual row composition and
shared selection/marks are useful patterns, but its row-index IDs, row cap
and absence of accessibility roles do not satisfy this issue. No upstream
commits or theme/list redesign were imported.

### PWR-30: source fields and current status

Source `9729798`, debug macOS binary SHA-256
`40b43a26cea199b307cf0b8e543de628918da262acf634b2c43aadea6e821809`,
same contrived Projects fixture, OS and scale as PWR-29 above.

| Promise | Observed native macOS AX | Harness / source |
| --- | --- | --- |
| Persistent source names and values | PASS: `SSH destination` and `Root directory to scan` exposed; entered `-audit-invalid` observable as the host value | Field semantics include current values and descriptions |
| Validation context | PASS: `invalid SSH destination` exposed and included in field descriptions after Return; no SSH connection attempted | Invalid flags distinguish connection and path errors; existing invalid-host/path and discovery regressions remain green |
| Source instructions | PASS: explicit Scan acceptance, nested-mount/symlink cautions and remote read-only context exposed | Source list has selected-root metadata and stable mount-path IDs; native mount traversal remains unverified |
| Current state and volume | PASS: completed scan, fixture root, marked count and 506 MiB available of 512 MiB exposed | Marked plan byte totals and estimates are independently readable; no extra per-frame mark scan |
| Progress/cancel/errors | Live progress not exercised: small fixture completes before inspection | PASS: two-second progress cache, error bypass, immediate cancel and completion regression; indeterminate progress/busy metadata; sampled paths omitted |
| Announcements | BLOCKED for speech/cadence; exposing a polite live region is not a spoken-output pass | Source live metadata and throttling only |

macOS lint, full workspace tests (including 219 app tests) and debug build
passed. Dedicated Linux VM `sb_uihqpdf98b` at `9729798`: setup 0 (2s), lint
0 (5s), test 0 (14s), portable build 0 (44s), isolated SSH 0 (3s), all 0.
These are build/harness/integration results, not AT-SPI or Orca evidence.
Native focus/modal containment, Windows UIA/AT, speech, OS scaling and high
contrast remain open under PWR-31/PWR-36. Changes add semantic metadata;
the visible source/status presentation is unchanged, so screenshots do not
show the repaired names/values. No real scan or live removal was performed.

### PWR-31: focus, modal boundaries and menus

Source `08a2b7f865d76db375b38cb5693ac1727cfa6186`, debug macOS binary
SHA-256 `d4e7b7c11f5b21314dff6a9edfc7f5ea6c08769d2655bad04b5ae866ddbf6d1a`.
Tab/Shift-Tab traverse controls and composite navigation; n/Shift-n retain
sibling stepping. Escape returns to file navigation. Modal traversal stays
inside its scope and dismissal restores the invoking focus handle. Explicit
tracked handles carry their own tab metadata: element styles alone did not
make the root/file lists reachable; strengthened regressions caught this.

| Promise | Native macOS result | Harness / source |
| --- | --- | --- |
| Visible traversal | PASS inspected case: Tab adds a visible focus edge to Size while retaining file selection | PASS: Tab reaches file navigation without changing selection; 50,000 empty files remain reachable in bounded pages |
| Source fields / root list | PASS keyboard: Tab reaches host and path inputs; keystrokes appear as values `a` and `b`; no acceptance or SSH connection | PASS: forward/reverse traversal reaches both inputs and the root list, stays in modal and restores originating control |
| Help / source AX boundaries | PASS inspected trees: named dialogs exposed and application background controls absent from full AX snapshots | PASS: modal background shortcuts contained, safe Escape cancellation; opaque overlays contain pointer input |
| Disabled actions | PASS: history and unconnected Scan buttons report disabled in native AX | Supported synthetic parent metadata supplies the flag omitted by the pinned button; Click remains removed by the base control |
| Settings navigation | PASS: named Settings panes tab group and four tab states; Tab leaves Updates selected; Ctrl+PageDown selects Safety without changing a preference | PASS: pane navigation and control traversal regressions; Ctrl+PageUp/PageDown replace pane-cycling Tab |
| Custom menu | NOT TESTED natively on Windows/Linux | PASS: arrows, Escape and Enter on the safe Keyboard Shortcuts action; menu/item/shortcut metadata and scroll-follow |
| Removal alert | NOT TESTED natively by design | PASS: named alert metadata, keyboard containment and Escape retain files; destructive coverage remains harness-only |
| AT focus / speech | Controller still reports the standard window as focused; no speech/navigation result | GPUI focus and active-descendant metadata do not establish VoiceOver/UIA/AT-SPI behavior |

macOS lint, full workspace tests (225 app tests) and debug build passed.
Linux `sb_uihqpdf98b` at this commit: setup 0 (1s), lint 0 (5s), test 0
(12s), portable build 0 (43s), isolated SSH 0 (2s), all 0. Native Windows,
Linux desktop/Orca, speech, native menu operation and OS scaling/high-contrast
remain unverified under PWR-36. Help content/reflow findings remain tracked
under PWR-32/PWR-34.

Contrived before/after focus images use PWR-30 head `21ae8fe` and this commit,
the same Projects/APFS fixture, default dark appearance and 100% UI zoom.
Both windows were fitted with macOS's window Zoom action: 2992×1878 pixel
captures (1496×939 logical at 2×). The initial offscreen capture had white
clipped edges and was replaced; that capture artifact is not a layout fix.
Before, Tab changes the selected tile; after, it focuses Size. No removal
confirmation was activated through computer use or accessibility APIs.

### PWR-31 review corrections: modal intent and foreground keys

Production source `d256dff65b5f95c8cd8d02b39f9a57daac982679` corrects the
three findings from PR #42's review at `e0f643c`. All six new
`focus_review_` window-harness tests fail on the reviewed production code
and pass after the correction. They use generated temporary trees only.

| Promise | Reviewed behavior | Observed harness result after correction |
| --- | --- | --- |
| Focused modal actions | Enter on Cancel starts removal or accepts a scan root | Enter/Space on removal Cancel leave marks and files intact; source Cancel starts no scan; focused Delete removes only its marked temporary directory |
| Foreground Escape | An indexing interception consumes Escape ahead of foreground screen commands | Running requests a removal stop while preserving the background scan; Review/Done return to Explore with the refresh intact |
| Settings pane traversal | Focusable pane selectors are omitted from the tab order | Forward and reverse traversal reach each of the four pane selectors |
| Application menu trigger | Focusable trigger is omitted from the tab order | Forward and reverse traversal reach the trigger; Enter opens its menu and Escape restores focus |

The modal's Confirm action is handled by the focused button using the same
callback as pointer activation. Dialog defaults apply only to their
container, root list or input context. Scan-specific Escape interception is
limited to Explore. Settings panes and the custom menu trigger explicitly
register as tab stops. These corrections change keyboard behavior, without
adding a visible layout or style; the existing contrived focus images keep
their original build provenance.

macOS 27.0.1 arm64: `cargo xtask lint`, full workspace tests (251 app passed,
2 ignored; 233 core passed, 7 ignored), and the normal debug app build pass.
Ubuntu 24.04.5 x86_64 Sandboxer `sb_uihqpdf98b`, same production source:
setup 0 (1s), lint 0 (7s), tests 0 (16s), portable build 0 (45s), isolated
SSH 0 (2s), all 0.

Native AX/UIA/AT-SPI and VoiceOver/Narrator/NVDA/Orca were not rerun for
these corrections. The Windows peer is disconnected; Windows verification
is pending CI. Native keyboard/AT containment and OS display/text scaling,
high contrast and speech remain separate acceptance gaps. The Running
cancellation-routing test uses an empty removal plan to avoid a deletion
race. Actual removal and confirmation coverage is confined to the
contrived-filesystem harness; no computer-use or accessibility API approved
removal.

### PWR-31 review corrections: foreground Escape and Help menus

Production source `471fbf02fe0bbc07e9a5b48782c65c4c22818975` corrects the
two keyboard-routing findings from PR #42's review at `040e68a`. Three new
`routing_review_` window-harness tests fail on that reviewed source and
pass after the correction. Every scan uses a generated temporary tree.

| Promise | Reviewed behavior | Observed harness result after correction |
| --- | --- | --- |
| Search Escape during widening | Cancels the background scan and leaves search open | Clears search in treemap and file-list views without changing the scan epoch or cancelling; the next Escape cancels the scan |
| Menu Escape during widening | Cancels the scan before dismissing the foreground menu | Breadcrumb, power and application menus dismiss while the scan continues; a subsequent Escape cancels |
| Help owns keyboard navigation | Existing menus remain active behind Help | Both `?` and the keyboard-shortcuts action close all three menu types before Help opens; Down/Enter cannot navigate the underlying directory or change marks |

The root handler delegates scan-time Escape to the existing dispatcher,
which already gives menus, search and applied filters precedence over scan
cancellation. `open_help` clears menu state for both shortcut and action
entry points. The focused tests retain the existing tree and verify the
contrived files remain present. No removal is requested by these tests.

macOS 27.0.1 arm64: `cargo xtask lint`, full workspace tests (254 app passed,
2 ignored; 233 core passed, 7 ignored), and the normal debug app build pass.
Ubuntu 24.04.5 x86_64 Sandboxer `sb_uihqpdf98b`, same production source:
setup 0 (1s), lint 0 (14s), tests 0 (23s), portable build 0 (43s), isolated
SSH 0 (2s), all 0. The sandbox was stopped after verification.

These are harness/source results. Native AX/UIA/AT-SPI and
VoiceOver/Narrator/NVDA/Orca were not rerun; Windows verification awaits CI.
Native menu/AT containment, OS display/text scaling, high contrast and
speech remain separate acceptance gaps. No native removal confirmation or
accessibility action was performed. These routing corrections add no new
layout or styling; the earlier contrived focus images keep their original
build provenance.

### PWR-31 integration with main's dialog, toast and Settings changes

Merge `4c71496866c9162b363c305eafdacf21dac3ec05` brings PR #42 onto main at
`cb3565d` (#50 platform footers and source switch, #51 update toasts, #52
grouped Settings). It keeps main's layout and reapplies this PR's modal
actions, conditional dialog defaults, disabled metadata, tab stops and
focus restoration to it. Two follow-ups apply the PR's own rules to what
main added:

| Promise | Integrated behavior before follow-up | After |
| --- | --- | --- |
| Disabled metadata | The update toast's held "Restart after the removal" used the pinned button's `disabled`, which omits the native flag | `semantic_disabled`, like every other disabled button (`133183a`) |
| Visible Settings focus | Main marks the shown pane with a left accent bar; focus recoloured that bar, so focus on the shown pane was invisible and a focused pane looked shown | Focus draws the app's full accent outline on a wrapper; the bar stays the selection (`0ea7fe8`) |

Contrived images: `pwrdisk` debug builds of main `cb3565d` and `0ea7fe8`,
scanning Projects on the APFS image A11yFixtures, macOS 27.0.1, Settings
window 1000×681 logical (2000×1362 capture), dark, 100% UI zoom. Before,
one Tab in a fresh Settings switches to Safety with no focus mark; after,
two Tabs focus the Safety tab with an outline while Updates stays shown.
Keys were sent with System Events; nothing was removed or confirmed.

macOS 27.0.1 arm64 at `0ea7fe8`: `cargo xtask lint`, full workspace tests
(268 app passed, 2 ignored; 233 core passed, 7 ignored) and the debug app
build pass. One test run aborted when gpui-omarchy's theme-file watcher
thread woke inside `an_agent_marks_shows_and_reviews_over_the_link`; the
unchanged rerun passed. Ubuntu 24.04.5 x86_64 Sandboxer `sb_uihqpdf98b`,
same source: setup 0 (1s), lint 0 (5s), tests 0 (12s; 264 app, 221 core),
portable build 0 (44s), isolated SSH 0 (2s), all 0; stopped afterward.

The watcher failure was resolved by the November 2 follow-up below.

The Settings outline was observed in the native macOS window only. Native
AX/UIA/AT-SPI and VoiceOver/Narrator/NVDA/Orca were not rerun for the
integration; Windows is verified by CI. The native acceptance gaps above
remain open.

### PWR-32: auxiliary text and keyboard reading

Implementation `e47e994`, Copy-name correction `0cb8d3e`, rebased on main
`cece3b9`. Final macOS debug binary SHA-256:
`0495f8e49c8cea040fa9d67c8182137ec7f769b34f205449bf30b9b78313917d`.
The native log example at `e47e994` reuses the production views with an
explicitly supplied invented log and no preference file; it never opens the
production log, scans, or removal UI.

| Surface | Observed native macOS AX / keyboard | Harness / source |
| --- | --- | --- |
| Help | PASS inspected sheet: heading and all shortcut descriptions exposed; background absent | Existing modal/keyboard tests remain green |
| Settings | PASS About: pane/card headings, descriptions, version/build/source values and project URLs exposed | Named groups scope repeated field/value IDs; channels expose radio state and retain keyboard activation |
| Contextual Copy | Initial native run found that the base button overwrote `aria_label`; corrected through its supported `accessibility_label` API. PASS final native names: Copy checkout path, Copy branch or tag, Copy commit, Copy complete License | Copy actions retain their existing payloads; complete-document clipboard regression passes |
| Documents | PASS: all 22 license lines; End reaches notices line 25,779 and exposes its final rows | PASS: long-notices Home/End brings virtual rows into view and keyboard Copy copies the complete source. Changelog contributes heading/paragraph semantics and block navigation |
| Logs | PASS invented 300-line log: named search, level/mode states, Unicode rows, selected lines 1 and 300, paused/live summary. Search for invented line 299 exposes one match | PASS keyboard Home/End, selected-line copying and existing search/rotation/empty-log tests. Input Escape action is captured to return to the log reader |
| Announcements / focus | No spoken-output result; native controller still reports the standard window as focused | Appending lines/counts deliberately have no live-region metadata; this does not prove AT announcement behavior |

macOS 27.0.1 / 26A434 arm64, default dark, 100% UI zoom. Native fixture log:
`/private/tmp/pwrdisk-a11y-invented.log`, 300 invented INFO lines with Unicode.
No real logs, diagnostics, removal approval or deletion were used. Closing
document and Settings windows returned to the fixture main window; useful
AT focus restoration and speech still require an actual screen reader.

Final `0cb8d3e`: macOS lint, full workspace tests (234 app tests) and debug
main/example builds passed. Dedicated Ubuntu 24.04.5 x86_64 Sandboxer:
setup 0 (1s), lint 0 (4s), tests 0 (11s), portable build 0 (44s), isolated
SSH 0 (2s), all 0. The earlier integrated `e47e994` run also passed all
five checks. These results cover the rebased PWR-29/30/31 implementation;
their historical native observations above retain their original SHAs.

Windows UIA/Narrator/NVDA, Linux AT-SPI/Orca, actual VoiceOver reading,
OS scaling/high contrast and document reflow are not verified here.
Semantic text/names do not produce a meaningful pixel comparison; operator
review of the native evidence in lieu of before/after images is required
before merging. Visible reflow/scaling changes belong to PWR-34.

### PWR-32 on main's Settings and About redesign

PR #43 moves onto main `a63a726` (after #42, #52 grouped Settings and #55
About header) as one implementation commit plus this record; #42's merged
commits are omitted. Main had replaced the Settings panes this work named,
so its semantics are applied to the new ones rather than restoring the old
layout:

| Surface on main | Applied |
| --- | --- |
| Pane head, cards, fields | Pane title and card titles are headings; heads, cards and fields are named groups; labels, descriptions and values are readable text |
| Safety | The "Applies to" statement is readable; load errors and save notes are alerts |
| Updates grid | Each channel tile is a radio button named for its slot and purpose, with its toggled state; Enter/Space pick, arrows move as the window's keys do; the channel caption, follow/picked line and status are readable |
| About header (#55) | Version is a heading; the build line keeps its whole text as its label when the painted line is cut short |
| Copy buttons | Diagnostics, checkout, branch or tag and commit Copy buttons carry distinct names, prefixed "Copied:" after use |

The native fixture example has since become a debug build's `pwrdisk
logs-fixture LOG` (`crates/disktree-app/src/logs_fixture.rs`), which opens
the same Logs window with no module list to keep in step. The earlier
native macOS AX observations above were made on the old Settings layout;
they were not repeated on main's panes. The Settings/About part of this
record is harness/source evidence only.

Contrived Logs images: debug builds of main `a63a726` and this PR, with
`HOME` set to a temporary folder holding 300 invented INFO lines (plus the
three lines each run writes), macOS 27.0.1, Logs window 1120×721 logical
(2240×1442 capture), dark, 100% UI zoom. Help › Logs was opened with
System Events, then Escape and End: main selects nothing; this PR selects
the last line and shows its copy bar. Nothing was scanned beyond a 4 MiB
invented folder, and nothing was removed.

### PWR-32 review corrections: Tab order, roles and first step

A high-effort review of PR #43 at `dd9f2f9` found that Tab could not
reach several of the controls this work exposes. In GPUI `focusable()`
alone is not a tab stop, and Tab from the auxiliary windows' title-strip
buttons had no handler at all. Four new `aux_review_` window-harness tests
cover the corrections; every log and document is invented.

| Promise | Reviewed behavior | After |
| --- | --- | --- |
| Logs controls reachable | Mode and level segments, context-line items and Include diagnostics were focusable but not tab stops; Tab from Copy path/Reveal stayed put | All are tab stops; the shared frame takes Tab (and the text field's indent actions) above the strip, so Tab cycles both ways and Space toggles a level |
| Document Copy | Tab from the title strip's Copy stayed put | Tab reaches the reader from Copy and back |
| Channel radios | Focusable but not a tab stop | One stop for the group, on the picked channel |
| Mode semantics | Filter/Highlight exposed as independent checkboxes | Radio buttons; levels stay checkboxes |
| First keyboard step | Down with no selection selected line 2 | Down starts at line 1, Up at the last line |
| Changelog layout | The scroll area had shrunk to the column width | Window-wide scroll area again; each block keeps the column's width and padding |

Tab handling moved from the Logs, document and Settings bodies to
`aux_window::frame`. Page Up/Down share one named step. macOS 27.0.1:
lint passes; workspace tests pass (275 app, 233 core) with
`an_agent_marks_shows_and_reviews_over_the_link` skipped. That test passes
alone three times. In full runs on this Mac it now aborts when
gpui-omarchy's theme-file watcher wakes inside it, and does so on the
unreviewed `dd9f2f9` too, which passed earlier the same day. Native AX and
screen readers were not rerun for these corrections.

The watcher failure was resolved by the November 2 follow-up below.

### PWR-33: contrast and non-color state cues

Source `08402c3`, macOS binary SHA-256
`5f963423d6a1d56f17f4d451f2906a4cecffa990f1faa3af65f43cf80fa3d782`.
Production label colors now use the exact category/age/depth/filter/marked
fill. A passing theme ink is retained; otherwise an opaque black or white
ink is chosen. Name and value runs carry the same opaque tile background,
so hatch cannot become a different text surface. Marked/covered, filtered
and unreadable labels also have text cues; selection has a thicker ring.
Age and category retain readable equivalents in the file list/sidebar.

| Check | Observed result |
| --- | --- |
| Production tile-state regression | PASS: 8,910 combinations across all 11 categories, six age choices (category plus five buckets), depths 0–4, three filter states and three marked/covered states, using default dark/light and deliberately low-contrast synthetic custom colors. Names/values >=4.5:1; selection/hover/mark/unreadable boundary inks >=3:1 against the actual fill |
| Captions / highlighted action | PASS numeric regression: opaque caption ink and highlighted-action text >=4.5:1; hover computes its own composited ground and ink |
| Alpha behavior | Regression initially failed: GPUI `opacity(1)` multiplies the existing alpha. Final code explicitly resets alpha; the opacity/contrast regression passes |
| Native default dark | PASS inspected rendering: matched contrived cache mark, explicit `[marked]` labels, brighter size values and thicker selected ring, fitted 2992×1878 captures at 100% UI zoom |
| Native light / custom / high contrast | NOT VERIFIED here. The synthetic custom-color regression is not a native custom-theme or OS high-contrast result |
| Hatch / filter / unreadable rendering | Production color and background paths covered by source/threshold checks; native intersections and all combined visual states still need the PWR-36 matrix |

macOS lint, full workspace tests (235 app tests) and debug build passed.
Ubuntu 24.04.5 x86_64 Sandboxer at this source: setup 0 (1s), lint 0 (4s),
test 0 (11s), portable build 0 (43s), isolated SSH 0 (3s), all 0. An earlier
run at `944d042` correctly failed the alpha regression; it is superseded.

The ignored audit diagnostic retains the legacy formulas to reproduce the
baseline. The production state regression is the threshold gate, not an AT
or WCAG certification. Upstream #58 was used narrowly to compare the
contrast calculation/test pattern; no commits, theme picker, persistence,
font or independent layout redesign were imported. Windows/Linux native
rendering, high contrast and actual AT results remain explicit gaps.

### PWR-33 review corrections: theme rings and highlight ink

Source `e02dd98`, rebased onto main `7cfb099`. A review found that the
first pass changed more of main's look than contrast required:

| Finding | Correction and evidence |
| --- | --- |
| Highlighted text ink | `on_highlight` keeps main's choice (background ink on dark themes, bright on light) and falls back only when that fails 4.5:1. Native dark capture: the highlighted Mark for removal button keeps its dark text |
| Ring translucency | Rings went through the text rule, which forces alpha to 1. `palette::boundary_on` keeps the themed ring, translucency included, when it already meets 3:1 over the fill, and falls back to `ink_on` otherwise. The 8,910-combination regression checks the colour actually painted |
| Text cue coverage | Harness test: marking `junk` labels it `[marked] junk`, and every covered tile's label starts with `[marked] ` |
| Label prefix density | Not changed. Repeating `[marked]`/`[filtered]` on covered and filtered tiles is a design question; the operator holds a to-do card to mock alternatives in Claude Design |

macOS 27 arm64: lint, workspace tests (276 app passed, 2 ignored; the
link test passed alone, as above) and debug build passed. Native default
dark before (`7cfb099`) and after (`e02dd98`) captures use the same
135-file contrived tree with `cache` marked and `.hidden` selected; no
removal was approved or performed. Light, custom and high-contrast native
rendering and AT results remain the PWR-36 gaps listed above.
Ubuntu 24.04.5 x86_64 Sandboxer at `e02dd98`: setup 0 (2s), lint 0
(12s), test 0 (24s), portable build 0 (83s), isolated SSH 0 (3s), all 0.

### PWR-34: reflow and shared UI scaling

Final implementation `16f7570`; macOS main binary SHA-256
`f61fa6b61bf17643e810408e9ca568fa7f533ebd6cb4ce469fb3ebdcc6f5b77d`;
native fixture example SHA-256
`b0cad63bb67174d6965838d66e4ca7ef07ec3e69a3f5186fc62779309b9ccf79`.
Interface zoom now spans 75–200% and is shared by existing/new main,
Settings, document and Logs windows. This is application zoom, separate
from OS display/text scaling. Help keeps a bounded reading lane, heading,
Close action and visible scroll-key guidance. Narrow controls wrap below
the strip; free-space/source information remains visible when the sidebar
is hidden. Short file lists retain a complete entry above compact actions,
with full path/action names in AX. Scrolled file/review rows retain their
content height. Documents rewrap using measured glyph widths and preserve
relative reading position; complete Copy always uses the original source.

| Surface | Native macOS observation | Harness / source |
| --- | --- | --- |
| Help | PASS inspected 100% and 200%: heading/Close remain in the window, End reaches final review shortcuts, Home returns to the start | PASS bounds, usable lane height and keyboard scroll at 900×600 / 16 and 32 px rem |
| Shared scale | PASS: Settings and License inherit the main window's 200%; reset in an auxiliary window restores normal main layout | Session-wide scale plus redraw/sync of existing and newly opened views |
| Settings | PASS inspected Safety: text reflows, End reaches the final option; selected 5% retained without preference edits | PASS 640×480 / 200% pane/navigation bounds and scroll; labels/values wrap |
| Documents / native menu | PASS: native Help → View License opens the document at 200%; 35 wrapped lines, final text reachable. Reset reflows to 22 lines | PASS 640×480 / 200%, end navigation and full-source keyboard Copy; Changelog PageUp/Down scrolls within long blocks |
| Minimum file reader | PASS final light-preset fixture: 900×600 requested content, 200%, one complete selected entry, full action/path AX names, Down selects the next entry; volume/source remain visible | Regression first failed with a 4 px viewport and shrinking rows. Final bounds require a complete line plus padding and retain intrinsic row heights |
| Logs | Native 200% log reading not exercised in this pass | PASS minimum-size Unicode wrapping and pixel paging inside one oversized line. Earlier paging regression was 0 → 0; final paging progresses without changing selection |
| Review | Native destructive route not exercised | PASS 900×600 / 200% safe End scrolling; files retained. Explanation/list stack, rows/footer wrap and retain content height |
| Targets | Native minimum reader supplies the larger target alternative to tiny painted tiles | PASS depth controls >=24 logical pixels at minimum/maximum UI zoom; file rows remain >=24 at 75% |
| Short progress sidebar | Existing regression caught an overly broad toolbar wrap; final wide layout preserves its scroll room | PASS free-space/progress/stop reachability regression and full workspace suite |

The light rendering used the shipped Flexoki Light preset explicitly in
`a11y-fixture --tree /Volumes/A11yFixtures/Projects --light`, under the
unchanged macOS appearance. It is native GPUI rendering, not proof of an
OS light/high-contrast setting or a custom theme. The requested 900×600
fixture yields a 1800×1202 window capture at 2× backing scale. Help's matched
100% pair uses `08402c3` and `8b612ee` (Help code retained at the final head),
2992×1878 fitted window captures; the 200% reader pair uses `8b612ee` and
`16f7570`, 1800×1202. All filesystem data is the invented APFS fixture.

A controller click on the offscreen About license action did not open it
at 200%; the native Help-menu action did. Actual AT activation/scroll-to-
focus still needs verification. The controller still reports window focus;
no VoiceOver speech result is inferred from these observations.

Final macOS lint, full workspace tests (243 app tests), main and fixture
builds passed. Ubuntu 24.04.5 x86_64 Sandboxer at `16f7570`: setup 0 (1s),
lint 0 (5s), tests 0 (14s), portable build 0 (44s), isolated SSH 0 (3s),
all 0. Earlier failed reflow runs are superseded; they exposed the progress
and reading-lane regressions fixed here. Native Windows/Linux desktop,
VoiceOver/Narrator/NVDA/Orca, OS display/text scaling and high contrast remain
unverified. UI zoom and a 2× capture backing scale do not establish those
OS/AT results. No removal approval was performed through native APIs or
computer use.

### PWR-34 rebase and review corrections

Source `d5990e3` and later, rebased onto main after #45 (contrast), #60 and
#61. Main's Settings redesign (grouped panes with an accent bar) landed
after this work was written, so the narrow-window rules were carried onto
it: below `ui::COMPACT_WINDOW_REMS` (50 rem) the panes wrap in a row above
the content and the group headings step out; Tab stays with
`aux_window::frame`, which now also takes interface zoom for every
auxiliary window. The fixed 20-line Page Up/Down step (`PAGE_STEP`) is gone:
documents page by the measured view and Logs scrolls within wrapped lines.

#60 replaced the `a11y-fixture` example with a debug build's
`pwrdisk logs-fixture LOG`, so the example's `--tree`/`--light` mode cited
above no longer exists. Its earlier observations stand as recorded at
`16f7570`; a contrived tree now opens with plain `pwrdisk ROOT`. After #61,
on-screen macOS checks run in the PwrSuiteLab headed E2E VM, never on the
operator's desktop.

| Review finding | Correction |
| --- | --- |
| Review scroll keys ran before the strip's app menu | An open menu takes its keys first, so its arrows move through rows instead of scrolling the review behind it (Linux, Windows) |
| Compact "Mark / unmark" not in its accessible name | Visible text is "Mark or unmark", inside "Mark or unmark selected path" (WCAG 2.5.3) |
| Notices re-wrapped on every pixel of a resize | Wrapping is keyed on whole monospace columns |
| Scrolling guidance appended under Hyprland | Moved under Keys in `docs/guide.md` |
| Duplicate compact threshold and checks | One named token; view settings decide inline once; the compact space line is built only when shown |

macOS arm64 (headless gate): lint, workspace tests (289 app passed, 2
ignored; 234 core passed, 7 ignored). The GPUI theme-watcher abort described
above also struck `the_agent_overlay_opens_and_closes`; it and the link test
pass alone. Ubuntu 24.04.5 x86_64 Sandboxer at `d5990e3`: setup 0 (8s),
lint 0 (17s), test 0 (17s), portable build 0 (49s), isolated SSH 0 (2s),
all 0. Native AT, OS scaling and high contrast remain the PWR-36 gaps above.

#### November 2 follow-up: headless theme watching

gpui-omarchy 0.1.3 watches `$HOME` and theme ancestors even when no Omarchy
theme exists. Any non-access event sends on an async channel, waking a
GPUI foreground task from the FSEvents thread. The deterministic scheduler
rejects that wake; a late wake can panic while async-task unwinds and abort
the process. PR #74 routed both agent tests through the shared initializer,
which cancels following immediately. The initializer now registers the
same base/control bindings and applies Tokyo Night without starting a
native watcher. Production theme following is unchanged.

On macOS 27.0.1 arm64, `de5fbb0`, `305f69c` and post-#74 `00da039` each
passed five ordinary full app-suite runs. With writes every 2 ms in an
isolated temporary `$HOME`, both older revisions failed five of five runs
with the FSEvents scheduler violation; `00da039` passed five of five. These
controlled failures exited 101 rather than reproducing SIGABRT. The bug
predates #68, whose ordinary suite median was 6.26 s versus 8.57 s before
it; these samples do not support a slowdown explanation. The historical
change in ambient filesystem traffic cannot be reconstructed from them.

### PWR-35 remediation: reduced motion

Production source `24cc28afe058f3893fa6805c7198570c1f090fa2` refreshes the
SDK's system motion preference before directory navigation and during
transition frames. The pinned base SDK samples macOS/Windows at startup
but does not follow subsequent native changes on those platforms. Enter,
ascend and history now land directly when reduced motion is enabled; an
active transition is discarded on the next frame. User-driven zoom stays
available. No upstream UI redesign or dependency upgrade is included.

| Layer | Observed result |
| --- | --- |
| Harness | PASS: two regressions failed before the change and pass after it. Immediate enter/ascend/history/wheel navigation, final layout, marks and user zoom retained; enabling reduced motion clears an active transition, disabling it allows subsequent transitions |
| Native macOS preference | PASS subset on macOS 27.0.1 arm64: System Settings AX reports Reduce motion off → on → off, restored to its initial value. One running contrived-tree fixture reports normal descent `reduced=false transitioning=true depth=1`, return with the OS preference on `reduced=true transitioning=false depth=0`, then forward with the restored preference `reduced=false transitioning=true depth=1` |
| Native process | Fixture SHA-256 `cb19f7a034b46df12cb95f4299dd56fa80f86426bd533464d625c728617d9899`, `--tree /Volumes/A11yFixtures/Projects --trace-motion`; only policy/transition/depth/scale flags are traced, no scanned paths |
| Gate | macOS lint, full workspace tests (245 app tests), main and fixture builds PASS. Ubuntu 24.04.5 x86_64 Sandboxer at `24cc28a`: setup 0 (1s), lint 0 (5s), tests 0 (15s), portable build 0 (43s), isolated SSH 0 (3s), all 0 |

Native preference switching establishes the macOS policy bridge and
navigation state, not perceptual motion sensitivity or AT speech. Native
mid-transition toggling is covered only by the harness. Native Windows
and Linux desktop motion preferences remain unverified. Still images of
the final destination do not distinguish the removed transition; operator
acceptance of the native trace plus failing-before/passing-after harness
evidence is requested before merge. No removal was approved through
computer use or accessibility APIs.

### PWR-35 rebase and review corrections

Rebased onto main after #46. #60 retired the `a11y-fixture` example, so the
`--tree … --trace-motion` invocation and the fixture hash cited above no
longer exist; that trace stands as recorded at `24cc28a`, and the motion
policy and code paths it exercised are unchanged. The review moved the
platform re-read inside a running transition: an idle frame no longer asks
macOS or Windows for the preference, while starting a transition and each of
its frames still do. The harness cannot reach that re-read (GPUI's test
scheduler never asks the platform), so live OS toggling stays native
evidence.

Headed macOS evidence after the rebase, in a macOS virtual machine against
the contrived tree (`scripts/a11y-fixture.py`), with Reduce motion turned on
before launch and confirmed through `NSWorkspace`, then restored. Entering
`cache` with Return was recorded at 60 frames per second:

| Build | Distinct frames | Result |
| --- | --- | --- |
| main `a05da0d` | 7 | Five in-between frames: the folder zooms despite the preference |
| this change `e7fe166` | 2 | The start and the destination: navigation lands directly |

The same runs tried to toggle the preference while the app was running.
Writing it with `defaults` reached new processes but not the running app,
which re-reads only when the system announces the change, so those clips
show nothing about live following and are not counted. Live toggling rests on
the earlier native trace above and the harness's mid-flight cancel.

### PWR-36 remediation: native evidence and remaining acceptance

The baseline above remains a historical result for `4d68f6cb`. The
following matrix records the remediation stack separately from that
baseline. A passing harness assertion does not establish a native API or
screen-reader result. All eight remediation issues remain In Progress and
the PRs remain drafts while native acceptance is incomplete.

| Issue / focused PR | Source and harness | Native evidence | Remaining acceptance |
| --- | --- | --- | --- |
| PWR-29 / [#38](https://github.com/pwrdrvr/PwrDisk/pull/38) | Paged file reader, raw path IDs, full metadata, selection, marks, review summaries; large-list, guard and PR #38 focus/pagination regressions | macOS AX file/page/Unicode/selected/review subset observed. Linux X11 AT-SPI file names, roles, metadata, selection and advertised actions observed in this pass | Actual AT reading/action/focus, review on Windows/Linux; removal approval remains human/harness only |
| PWR-30 / [#40](https://github.com/pwrdrvr/PwrDisk/pull/40) | Source inputs, validation, value-backed polite status, foreground phases, in-flight roots and sampled counters; five PR #40 review regressions | Earlier macOS AX host/root value, error/completion/volume subset; earlier Linux final status and volume names observed | Native live announcements/throttling, SSH success/failure and cancellation across readers; corrected announcement values have source/harness evidence only |
| PWR-31 / [#42](https://github.com/pwrdrvr/PwrDisk/pull/42) | Tab order, modal containment/restoration, disabled controls, menu keys | macOS AX modal boundaries and keyboard subset; Linux advertised menu/button actions read without invoking them | AT focus/activation; Windows/Linux custom menus; Linux disabled-state mapping discrepancy below |
| PWR-32 / [#43](https://github.com/pwrdrvr/PwrDisk/pull/43) | Settings/Help/document text, original-source Copy, log navigation/search/selection | macOS AX readable auxiliary subset; native Logs End/search/selection at 200% in this pass | AT reading order/actions and native platforms; final native search value `299` observed after controller recovery |
| PWR-33 / [#45](https://github.com/pwrdrvr/PwrDisk/pull/45) | Production tile-state contrast thresholds across 8,910 combinations; text backing and non-color cues | Contrived default dark rendering and later explicit light preset inspected | OS high contrast, real custom themes, remaining control/caption contexts and Windows/Linux appearance; no whole-window contrast certification |
| PWR-34 / [#46](https://github.com/pwrdrvr/PwrDisk/pull/46) | Shared 75–200% UI scale; minimum reading lanes, reflow and target checks | macOS Help/Settings/License/file reader subset; native Logs check found the additional search collapse fixed here | OS display/text scaling, all supported desktop/AT combinations and scroll-to-focus |
| PWR-35 / [#47](https://github.com/pwrdrvr/PwrDisk/pull/47) | Immediate navigation and active-transition cancellation regressions | macOS OS preference off/on/off in one running fixture; initial value restored | Native Windows/Linux motion policy and perceptual/AT evaluation |
| PWR-36 / [#48](https://github.com/pwrdrvr/PwrDisk/pull/48) | Read-only AT-SPI diagnostic, direct file-reader fixture, native-discovered Logs/label corrections | Results below; unavailable cases stay explicit | Windows UIA/Narrator/NVDA; VoiceOver; Orca, real X11/Wayland desktop, OS scaling/high contrast |

#### Native-discovered corrections

At 200% in a 1120×720 requested Logs window, the search field collapsed
to its icon and overlapped the Filter control. The search wrapper used a
zero flex basis, letting neighboring controls consume its width. It now
retains the existing `LOG_FILTER` minimum and lets the controls wrap. The
footer truncates the visual path while retaining its complete accessible
text. The strengthened regression fails before (275 px input at the
640×480 stress size) and passes after at widths 640, 880 and 1120, 32 px
rem. The wrapped line remains present.

The matched native images use only the invented 300-line Unicode log,
macOS 27.0.1 arm64, 200% application zoom, 2240×1442 window captures at 2×
backing scale. Before: source `24cc28a`, fixture SHA-256
`cb19f7a034b46df12cb95f4299dd56fa80f86426bd533464d625c728617d9899`.
After: source `4364396`, SHA-256
`d6facbbecb1923e2185b8c45066cfe9aefb31f5c0f7d205b03f05a9b96066fd4`.
The final semantic-only corrections do not alter this layout. End selects
line 300 with its complete Unicode accessible name; clearing selection
removes the copy overlay. Typing `299` leaves one matching line. The search
control initially omitted its typed value in AX; production now supplies
`aria_value`. The native recheck initially timed out repeatedly. After controller
recovery at `2cef5c5`, AX exposes the typed value `299` and one matching
line (300), establishing the corrected value export. An owned fixture
sample during the timeout showed AppKit event-loop/AX Raise work without
establishing a cause. No reader speech is inferred.

Linux also exposed an empty static-label name despite an application
`aria_label`. The pinned AT-SPI adapter derives a static label's name from
its value (`label_comes_from_value`); the shared text helper now supplies
both on Linux. The file and review summaries use that shared helper.
A macOS comparison showed that supplying static values there made the
controller report static text as settable even with the read-only flag;
this workaround is therefore scoped to Linux, preserving the original
macOS/Windows label metadata. This is a focused export correction with
the native result below.

#### Native platform/AT matrix

| Layer | Result and conditions |
| --- | --- |
| macOS AX | PASS subsets documented per issue, under macOS 27.0.1 build 26A434 arm64. The controller continues to identify the window as focused; this does not establish VoiceOver focus. Final Logs value `299` and one matching line observed at `2cef5c5` after controller recovery; static-label values are scoped to Linux to preserve macOS metadata |
| VoiceOver | NOT RUN: baseline launch failed. Temporary enablement was subsequently authorized, but the native UI tool reports its application session stopped. The setting was left unchanged; no speech or announcement-cadence result is claimed |
| Windows native build/UIA | BLOCKED: Windows 11 build 26200 x64 peer remains disconnected on recheck |
| Narrator/NVDA | BLOCKED with the Windows peer; neither version nor speech was observed |
| Linux native AT-SPI | Native GPUI X11 fixture under Xvfb and an isolated session/AT-SPI bus; Mesa llvmpipe software Vulkan. File-list/status observations and remaining state issue below. This is not the window harness |
| Orca | BLOCKED: not installed and no usable real desktop/audio reader session supplied. No speech or reader navigation claimed from the AT-SPI query |
| Linux real X11 / Wayland | NOT RUN: Xvfb has no desktop window manager/compositor. Real Omarchy/GNOME/KDE focus, menus, Wayland and rendering remain unverified |
| UI zoom | macOS native 200% subsets observed; 75–200% harness checks. UI zoom is distinct from OS scaling |
| OS display/text scaling and high contrast | NOT RUN across platforms; 2× backing scale, explicit light preset and synthetic low-contrast theme are not substitutes |
| Destructive confirmation | Never invoked through computer use, AX, UIA or AT-SPI. Destructive behavior is tested only by the temporary-tree harness or left to a human |

The Linux probe is `scripts/a11y-atspi-read.py --pid PID`, executed inside
the fixture's own desktop/session bus. It reads roles, names, descriptions,
states, relations, values/text where supported, and available action names;
it never invokes an action or starts a reader. `a11y-fixture --tree ROOT
--files` opens the production reader directly without simulated input.
The isolated bus enables `org.a11y.Status.IsEnabled` at `/org/a11y/bus`;
that flag is test setup, not proof of a running screen reader. Initial
wrong-path setup attempts did not find an application and are superseded
by the successful bus configuration.

Ubuntu 24.04.5 x86_64 versions: at-spi2-core 2.52.0-1build1, pyatspi
2.46.1-1, Xvfb 21.1.12-1ubuntu1.9, Mesa 25.2.8. Preliminary native reads
at `2956ca4`/`4364396` established list box/list item names, raw complete
Unicode/long paths, sizes, categories, ages, selected state, checked radio
and checkbox states, final scan/volume status, and advertised click actions.
The settled root snapshot at `0e7b57a` contained 35 nodes, seven file items,
one selected item and no focused node. No action was invoked; no
relationships were reported for the sampled entries. These reads use only
`a11y-fixture` PIDs, never another application's tree.

One remaining native FAIL is concrete: disabled Back/Previous/Next
controls advertise no action or focus stop but AT-SPI still reports
Enabled/Sensitive. Source/harness and macOS AX expose disabled correctly.
The pinned `accesskit_atspi_common` 0.19.1 state mapping adds
Enabled/Sensitive for controls without read-only support. An application
claim of a correct native disabled state would be false; a platform fix
and reader verification are tracked as follow-up work.

At `89c6a26`, native AT-SPI now reports the full static file-summary name
(`/tmp/pwrdisk-atspi-volume/Projects · 7 entries · showing 1–7`), rather
than an empty label. The native fixture SHA-256 is
`4eb8a9690668736b8194187e635b1540e8a51c499466c230a904f2e7ed8f5258`.
The final fixtures are on a dedicated 512 MiB tmpfs with 135 invented files;
status reports 508 MiB available of 512 MiB. Unicode and the complete
long-name file were observed separately under its documents root. The
initial root-capacity values from the disposable VM are superseded by
this bounded contrived-volume check.

Final macOS static-text check at production `6af7f1a` PASS subset: the
file-summary text and mark/volume summaries remain readable in AX and are
no longer reported as settable after scoping static values to Linux. The
fixture SHA-256 is
`fa205a03eddec466e3f3335d1a39d4a88fa8ba8bd836499d5dd6f49b3c3a032b`.
The successful native Logs query-value check used `2cef5c5`, SHA-256
`30e8b2b8c0fdd93dd4b05c5da0b8103909011dcdeaa504eb75899e594d4393a1`;
its input-value code is retained at the final production head. Earlier
controller timeouts are superseded for these specific export checks, not
for actual VoiceOver or OS scaling.

Final production source `6af7f1a8282f0e8b1e94be75a72de436f0d1ace2`:
macOS lint, full workspace tests (245 app tests; 232 core tests), normal
app and fixture builds PASS. Main/portable builds and embedded SSH helpers were also
verified throughout the stack. Ubuntu 24.04.5 x86_64 Sandboxer: setup 0
(0s), lint 0 (5s), tests 0 (14s), portable build 0 (57s), isolated SSH 0
(3s), all 0; final native fixture build 0 (53.65s). Final Linux fixture
SHA-256 `41ee44b27489f61d7b127e827e104822e1641f95638139d623a6923fa32301b8`.
The final AT-SPI query exits 0 with 35 nodes, seven file items, one selected
item, the complete static summary name and final contrived-volume status.
The disabled-state discrepancy remains observed.

The task-owned native windows were closed and the APFS fixture detached.
Reduce Motion was restored to its initial off value; VoiceOver was left
off. Remaining native acceptance is on the PwrAgent to-do card
“Complete native accessibility acceptance before merging the PR stack.”
No PR was merged and no removal confirmation was automated.


### PR #38 review corrections: focus and pagination

The four findings were reproduced against `3c92a33` using only temporary
contrived filesystem fixtures. Fix commit
`0613879e46fa46e78809c291834f219d975bb662` adds five window-harness
regressions, each failing before and passing after:

| Finding | Before | Observed correction |
| --- | --- | --- |
| Focused file list, rescan, Escape | The list's absent focus owner prevents cancellation | Focus moves to the persistent root before the list disappears; Escape cancels; a subsequent completed scan restores list focus when appropriate |
| Entry 99, Tab/Shift-Tab | Selection reaches entry 100 while page 0 remains rendered | Shared sibling navigation updates page, scroll and selection; Space safely marks the visible selected row |
| File Previous after filtering 450 entries to 150 | Page counter 4 becomes 3, still rendering effective page 1 | Previous uses the effective page and reaches page 0 in one click |
| Review Previous after marks shrink from 4,801 to 1,201 | The same stale-counter failure | One click reaches page 0; all 4,801 fixture files remain |
| Enter/Right on a file; Left/Backspace/u at the scanned root | No-op navigation resets the page and hides selection | Page and selection remain visible; an actual directory change resets pagination |

At `0613879`, macOS 27.0.1 arm64 lint, full workspace tests (229 app
passed, two ignored; 232 core passed, seven ignored) and normal app build
PASS. Ubuntu 24.04.5 x86_64 Sandboxer `sb_uihqpdf98b`: setup 0 (1s),
lint 0 (6s), tests 0 (15s), portable build 0 (56s), isolated SSH 0 (3s),
all 0.

Separate native macOS keyboard/AX observation: 128 invented files on a
disposable APFS image named A11yReviewFixtures. PageDown, Up, Tab selects
`item-000100.txt`. Before, the accessible list still reports entries 1–100
without a selected row; after, it reports entries 101–128 with the selected
row. Return, Left and Backspace preserve that page and selection. Matched
2992×1878 window captures, dark at 100% application zoom, are attached in
[PR #38](https://github.com/pwrdrvr/PwrDisk/pull/38). Normal app SHA-256:

- Before `3c92a33`:
  `843d85a2c4b4908bc56d02a43680e8c36713943de98cacb225da0b98ef227930`.
- After `0613879`:
  `606c6a9fbc97a5eaecef43d2e28dbbfe5c2ee765ea014468bfa6eb0d1ecafe13`.

The dependent focus PR also assigns the navigation focus handle to the
painted treemap. Its integration now tests visibility of either navigation
view before transferring focus. Its existing Tab-order regression caught
and verifies that distinction. The later control-traversal policy remains:
Tab moves between controls, while n/Shift-n exercise shared sibling
navigation. PR #38 retains its original Tab/Shift-Tab regression.

The restacked production source
`ec5847b292cdbd05c54215b3aad836d7a3a03d58` passes macOS lint, full workspace
tests (250 app passed, two ignored; 232 core passed, seven ignored), and
normal app build. Ubuntu 24.04.5 x86_64 Sandboxer at the same source:
setup 0 (0s), lint 0 (5s), tests 0 (12s), portable build 0 (43s), isolated
SSH 0 (3s), all 0. The first integration run's Tab-order failure is
superseded by the visibility correction and these passing gates.

No screen-reader speech was tested in this review pass. The Windows peer
remains disconnected. VoiceOver, UIA/Narrator/NVDA, Orca, real Linux
desktops, OS display/text scaling and high-contrast acceptance gaps in the
native matrix remain open; no harness result replaces them. No removal
confirmation was activated through computer use or accessibility APIs.
The task-owned test windows were closed and the disposable APFS image was
detached. Existing native acceptance work remains tracked separately.

### PR #40 review corrections: live values and active status

Reviewed source `d3941b1` supplied labels without values on both live
status nodes. The pinned `accesskit_macos` 0.26.3 event generator queues
live announcements only when the consumer node has a value, and reads the
announcement text from that value. This was confirmed in the local
dependency source; it is not a native speech observation.

Fix `3640f6821162780a441559e6941161590cf267bb` routes both
`current-status` and `marked-summary` through one read-only polite status
helper with matching labels and values. It resolves the foreground screen
before applying scan context. Widening names the worker's `scan_root` and
clears sampled text immediately on a new root, while file/byte counters
remain sampled at two seconds. Background scan metadata does not advertise
the foreground Escape-cancels instruction.

| Contrived regression | Before | Observed source/harness correction |
| --- | --- | --- |
| Current progress, error and completion status | Live value absent | Value matches the label |
| Marked-path summary | Live value absent | Same shared value-backed status helper |
| Cancel widening, then Review/Running/Done, including a previous scan error | Old scan state hides the active phase | Active phase leads; previous scan state remains separate |
| Harness removal completes and immediately starts a refresh | Indexing hides removal completion and advertises Escape cancellation | Removal completion remains primary; refresh is background context |
| Consecutive widening roots within two seconds | Old subtree or cached root is announced | Each in-flight root is named immediately; counters remain sampled |

All five regressions fail before and pass after. At `3640f68`, macOS
27.0.1 arm64 lint, full workspace tests (236 app passed, two ignored;
232 core passed, seven ignored) and normal app build PASS. Ubuntu
24.04.5 x86_64 Sandboxer `sb_uihqpdf98b`: setup 0 (1s), lint 0 (7s),
tests 0 (17s), portable build 0 (45s), isolated SSH 0 (2s), all 0.

The complete restacked production source
`10cff5159935359b9ba04539c7715398a18aebac` passes macOS lint, full
workspace tests (255 app passed, two ignored; 232 core passed, seven
ignored) and normal app build. Final Linux verification: setup 0 (1s), lint 0 (8s), tests 0 (19s), portable build 0 (49s), isolated SSH 0 (3s), all 0.

These are semantic metadata and status-string changes; the visible layout
does not change. No new before/after image can establish an announcement
value or spoken result.

Native evidence remains separate. The operator subsequently authorized
temporary VoiceOver enablement, but the native UI tool reports the
application session stopped, so no setting was changed and no VoiceOver
delivery/cadence result is claimed. The Windows peer remains disconnected
on recheck. Native AX live notifications, UIA, AT-SPI, VoiceOver,
Narrator/NVDA/Orca, OS display/text scaling and high-contrast checks remain
explicitly unverified for this correction. No removal was approved through
computer use or accessibility APIs; the destructive regression operates
only on contrived temporary-tree harness files.

### PWR-36 rebase and review corrections

Rebased onto main after #47. #60 retired the `a11y-fixture` example, so the
commit that taught it `--files` was dropped, and the invocations above
(`a11y-fixture --tree ROOT --files`, its PIDs) no longer exist on main. The
native reads stand as recorded at their cited sources; a contrived tree now
opens with plain `pwrdisk ROOT`, and the file reader is one key away.
`scripts/a11y-atspi-read.py` says so. On-screen macOS checks run in the
PwrSuiteLab headed E2E VM, never on the operator's desktop.

| Review finding | Correction |
| --- | --- |
| Folding the footer spacer into the log path left none while searching | A spacer stands in when the path is hidden, so the note and search hint stay at the right edge |
| The diagnostic script named the retired example | It names `pwrdisk ROOT` |
| No harness assertion for the search field's exposed value | Not added: the harness has no reader for accessibility values, so the value stays native evidence |

### PWR-33 follow-up: tile state without repeated words

The "Label prefix density" row above left a design question open: a
`[marked] ` or `[filtered] ` prefix repeats on every tile inside a mark or
a search, and on a narrow tile it is all that fits, so the name is lost.
Five treatments were mocked in Claude Design on one contrived tree (a mark
at the root, inside the mark, typing in Find); the operator chose
zero-width text styling.

| State | Cue now | Was |
| --- | --- | --- |
| Marked, or inside a mark | Name struck through, in the marked ink; the mark keeps its 2 px ring; the trail strikes a crumb that is marked or inside a mark | `[marked] ` on every label |
| Typing in Find | The matched part of a match's name is underlined, by the same ASCII-case rule the filter uses (`filter::find_ignoring_case`); non-matches are dimmed, unchanged, with no word added | `[filtered] ` on every non-match |
| Unreadable | `! ` prefix and the warning corner, unchanged: rare, and two characters | same |

Neither line takes any width from the name. Both are at least 1 px, in
the label's own ink, so the 4.5:1 label regression covers them. The accessible file list and the
selection status keep the words ("marked", "inside a marked directory",
"unreadable", "does not match filter") for assistive technology, unchanged.

| Check | Observed result |
| --- | --- |
| Marked and covered labels | Harness test: marking `junk` labels it `junk`, struck; every covered label is struck and carries no prefix; nothing else is struck; `goes_with_a_mark` is true for `junk` and `junk/deeper`, false for `keep` and the root |
| Find matches | Harness test: typing `BLOB` underlines exactly `blob` in each labelled `blob.bin`; no label starts with a bracketed word |
| Underline placement | A lab capture showed GPUI's run underline clipped away in a deeper directory's 1 rem band. The underline is now painted at GPUI's position, raised to stay inside the label's region; a unit test covers the clamp, and `find_ignoring_case` returns byte ranges on character boundaries (`Résumé cache.txt`) |
