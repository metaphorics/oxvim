# Task 74: the bundled TUI client

## What already existed

Almost all of it. `crates/ox-tui/` was ~7 000 lines carrying the whole client:
`theme.rs` (design tokens, contrast arithmetic, colorscheme mapping),
`chrome.rs` (command-line levels, wildmenu, completion popup and its
documentation, the message stack with kinds/lifetimes/history, responsive
layout with cursor-collision handling), `screen.rs` (grid model, multigrid,
composition), `terminal.rs` (capability probes, session lifecycle, OSC 4
palette programming, damage writer, control-byte sanitising), `lib.rs` (run
loop, frame rendering, key and mouse encoding), plus `ox-ui`'s server-side
emitter and compositor. 90 tests passed (74 ox-tui unit, 16 ox-ui contract).
`tests/differential/tests/tui_smoke.rs` held one end-to-end PTY test and three
headless ones.

The `msg_show` contract was already honoured in full, including the 7-argument
0.13 shape with `trigger`, `replace_last`, `append`, per-id replacement,
streaming kinds and `msg_history_show`. The cmdline collision mechanism already
existed (the overlay moves from the top third to the lower third when the
server cursor is inside it). Narrow-terminal degradation existed for the
command line (<60 columns: full width, no padding) and the message stack (<80
columns: full width).

## What I built

**Signal restore** (`fa2870d`). A terminating signal bypassed `Drop`, so
`TerminalSession::restore` never ran: the programmed palette, the hidden cursor
and raw mode all outlived the client. SIGHUP/SIGINT/SIGQUIT/SIGTERM are now
registered as flags, observed by the main loop, restored in order, and only then
resumed to their default action. Flags rather than handler-side writes, so
nothing async-signal-unsafe runs in a handler.

**A non-colour channel for diagnostics** (`82c5f86`). The stack painted every
entry in `MsgArea`: an error and an `:echo` were indistinguishable, and any
severity added later would have been colour-only. Kinds now classify to
Error/Warning/Plain per `runtime/doc/api-ui-events.txt` (unknown kinds are
plain, as upstream requires), and a diagnostic gets `E`/`W` in the first inner
column coloured by `ErrorMsg`/`WarningMsg`. Server bytes still pass through
verbatim; the letter is client chrome beside them. `paint_surface` split into
`surface_style` + `fill_surface` + `flow_text` so the stack places one entry per
row band.

**Narrow-terminal degradation for the completion preview** (`6990ec9`). The
preview was sized from leftover columns with no floor, so at 20 columns it
became a rect of width 0, and at 2-3 columns a strip of border with no text.
It now takes the roomier side and needs `MIN_FLOAT_COLUMNS` (frame plus six
columns) there, otherwise it is dropped and the menu keeps its columns.

**The selected completion row, and the contrast floor** (`837ede1`). Three
findings, below.

**PTY verification** (`0341ba5`). Eight tests that assert on composed cells.

Cleanup: the two test modules that lacked it picked up the crate's own
`#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]`; `cargo
clippy -p ox-tui -p ox-ui --all-targets` now reports zero errors.

## Findings

**The selected popupmenu row was never rendered.** The menu painted all rows in
`Pmenu` and ignored `selected`, so the design system's one sanctioned use of the
accent surface reached no cell. It is now painted, along with the item's kind
and menu columns.

**Client chrome quantised its foreground and background independently.** On a
256-colour terminal that put muted text on a float at 4.38:1. Client pairs now
resolve the surface first and take the nearest text colour that still clears
4.5:1 against it. Server colours keep passing through unchanged.

**A 16-colour terminal was being sent xterm-256 indices.** It now receives one
of its sixteen colours, chosen under the same floor.

**The contrast gate was testing dead code.** It ran over
`ThemeTokens::quantized()`, which nothing rendered with; the shipped path used a
cube/gray approximation with no floor. The aggregate helpers are deleted and the
audit now iterates every painted group, in both variants, at all three colour
depths, with the text/surface partition checked against the enum so a new group
cannot slip in unaudited.

**`FORBIDDEN_FG_ON_ACCENT_CONTRAST = 1.91` is not reproducible.** Foreground on
accent measures 1.78:1 dark and 2.05:1 light. The prohibition is unaffected —
both are far below 4.5:1 — but the number was an aggregate of neither variant.
Replaced with the two measured values.

## Contrast, recomputed

Every pair the renderer paints as text, measured from the shipped tokens and
from what each quantiser emits. Verdict: **no pair below 4.5:1**.

| variant | group | pair | truecolor | xterm-256 | ansi-16 |
|---|---|---|---|---|---|
| dark | Normal | fg on bg | 11.05 | 11.05 | 11.54 |
| dark | NormalFloat / Pmenu / MsgArea | fg on float | 10.16 | 9.81 | 11.54 |
| dark | FloatBorder / MsgSeparator | accent on float | 5.70 | 5.42 | 5.32 |
| dark | PmenuSel / WildMenu | bg on accent | 6.21 | 6.10 | 5.32 |
| dark | PmenuKind / PmenuExtra | muted on float | 4.85 | 4.85 | 5.32 |
| dark | ErrorMsg | error on float | 5.67 | 5.22 | 5.32 |
| dark | WarningMsg | warn on float | 7.05 | 7.34 | 5.32 |
| light | Normal | fg on bg | 13.17 | 11.38 | 21.00 |
| light | NormalFloat / Pmenu / MsgArea | fg on float | 11.82 | 11.38 | 21.00 |
| light | FloatBorder / MsgSeparator | accent on float | 5.78 | 5.05 | 10.95 |
| light | PmenuSel / WildMenu | bg on accent | 6.44 | 4.94 | 5.01 |
| light | PmenuKind / PmenuExtra | muted on float | 5.41 | 5.50 | 4.77 |
| light | ErrorMsg | error on float | 5.78 | 6.06 | 10.95 |
| light | WarningMsg | warn on float | 5.81 | 5.05 | 10.95 |

The two `bg`-on-accent rows are the brief's 6.21:1 and 6.44:1 exactly. The
xterm-256 and ansi-16 columns are the floored selections; nearest-colour
selection would ship 4.38:1 (dark muted on float, 256-colour) and 4.20:1 (light
accent on float, 16-colour), and both are pinned as counter-tests so the floor
cannot become vacuous.

Excluded, and stated in code rather than silently: `PmenuSbar` and `PmenuThumb`
describe a completion scrollbar this client does not draw. Their fallbacks would
not clear the floor — `visual` on `float_bg` is 1.26:1 dark, 1.28:1 light — so
if a scrollbar is ever drawn those two tokens need re-picking first. They are
retained only so a colorscheme's definitions for them are not discarded.

`fg_muted` on `visual` is 3.86:1 dark and 4.22:1 light. The brief scopes the
muted values to `bg` and `float` only, and no painted group pairs them, so it is
not a shipped pair; the audit's partition would fail if a group introduced it.

## PTY evidence

`crates/ox-tui/tests/pty.rs`, eight tests. A harness binary
(`src/bin/ox-tui-pty-harness.rs`) carries the client on the PTY slave, because
rendering needs a controlling terminal and libtest's own progress output would
otherwise land in the cells under test. It runs the shipping client against
itself as the embedded RPC server. Output is fed through a small terminal
emulator (cursor addressing, SGR colour, printable text) and the assertions read
composed cells.

Raw bytes cannot be searched for text: the damage writer addresses one cell at a
time and skips a cell whose value already matches, so `stream head` painting
over `second failure` arrives as `stram head`. This was mistaken for an emulator
bug before it was understood as the damage contract.

- **Cmdline overlay** — 80x24: `=1+1` renders in the top third on the float
  background while the grid around it keeps the terminal's own colours, and only
  the active level is painted. A second test cancels the nested level and finds
  `:edit alpha` restored with no trace of `1+1`.
- **Message stack** — `second failure` replaces `first failure` under the same
  id (the replaced text is absent from the screen), and a later batch's
  `append` joins `stream head and tail` on one row rather than two. The error
  row carries `E` as a glyph in `#f26f74`; its body and the stream's body are
  both in `#c9ccd4`, so neither channel is coming from the surface style.
- **Popupmenu with doc preview** — the selected row's cells are
  `#da834f` background with `#16181d` text (the 6.21:1 pairing on a real
  terminal), the unselected row stays on the float background, and the preview
  built from the selected item's info field sits to the right of the menu. The
  unselected item's documentation is not shown.
- **Palette set and restore** — all seven programmed slots appear as exact OSC 4
  strings (`ESC]4;0;rgb:16/18/1d ST` … `ESC]4;9;rgb:da/83/4f ST`), and OSC 104
  plus the cursor-show follow a clean exit.
- **Palette restore on a signal** — with the client still running the test first
  proves OSC 104 is *absent*, then sends SIGTERM and finds OSC 104 and the
  cursor-show, with the process terminating from the signal rather than exiting
  successfully.
- **20-column degradation** — the menu keeps all 20 columns with its selection
  still marked in accent, no row exceeds the terminal width, and the preview is
  not painted. A second case at 25 columns, where three columns remain beside
  the menu, proves the preview is *dropped* rather than clipped: those columns
  keep the terminal's default background. The 20-column case alone cannot show
  this, because there the clipped strip would be zero columns wide.

Each case was mutation-checked. Removing the signal registration, the selection
band, the append, the id replacement, the float minimum, the palette
programming, or moving the overlay out of the top third each fails its own case
and no other.

## Tests

Before: 90 (`ox-tui` 74 unit, `ox-ui` 16 contract). After: 109 (`ox-tui` 85
unit + 8 PTY, `ox-ui` 16). Zero failures. `cargo check -p differential --tests`
still passes, so the removed theme helpers had no outside users.

## What the design demands that the server cannot support

Nothing is missing from the server. Every surface the brief assigns to the
client is reachable with the attach set already in use
(`ext_linegrid`, `ext_multigrid`, `ext_cmdline`, `ext_messages`,
`ext_popupmenu`, `ext_hlstate`, `rgb`, `ext_termcolors`), and the client already
decodes every event those extensions emit. The brief's own exclusion holds: with
no `ext_statusline` in `ui_defs.h`, the statusline, tabline, mode indicator and
ruler stay server-rendered, and this client does not attempt to own them.

One deliberate non-use worth recording rather than a gap: `default_colors_set`
is received and consumed as a no-op. Under `ext_termcolors` the server stops
applying those defaults to its own highlights but still sends them; since the
client owns the palette, taking the variant from them would be circular, so the
variant comes from `$COLORFGBG` until a `Normal` highlight arrives.

## Concerns

- The 150 ms notification fade is implemented and reachable, but motion default
  is `reduced`, so the PTY tests run with `OXVIM_TUI_MOTION=reduced` and none of
  them exercise a partially-faded frame. The fade is covered only by the unit
  test on `MotionPolicy`.
- On a monochrome terminal the only channels left are the severity letter and
  the selected row's reverse band. Float borders and surfaces have no colour to
  distinguish them, so a float is bounded by nothing visible. If mono matters,
  the border needs box-drawing glyphs rather than recoloured spaces.
- The PTY tests carry timing: the harness spaces batches by 120 ms and the tests
  wait on the composed screen with a 20 s ceiling. They are deterministic in the
  sense that each script's final frame is fixed — the nesting case was split into
  two scripts precisely to avoid asserting on a transient frame — but they are
  not instant, and the suite spends about 4 s.
- `PmenuSbar`/`PmenuThumb` remain mapped but unpainted. That is defensible while
  no scrollbar is drawn; it is a loose end if one ever is, and their tokens fail
  the floor today.
