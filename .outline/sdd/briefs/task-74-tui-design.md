# Task 74: the bundled TUI client, per the design system

## Why this is safe to redesign at all

Upstream's TUI is already an out-of-process msgpack-RPC client (`.references/neovim/src/nvim/ui_client.c`), so client-side presentation is invisible to plugins. That freedom has a hard edge, established by an adversarial review earlier in this project and binding here.

## What the client may not own

There is no `ext_statusline`. The UI extension enum in `.references/neovim/src/nvim/ui_defs.h` is exactly: cmdline, popupmenu, tabline, wildmenu, messages, linegrid, multigrid, hlstate, termcolors. So a client cannot take over the statusline, the tabline, a mode indicator or the ruler: the server renders those into the grid. Suppressing them would mean changing `laststatus`, `statusline`, `showtabline` or `tabline`, which are plugin-visible options, and the upstream functional suite pins their default cell text (`test/functional/main_spec.lua:111`, `test/functional/editor/tabpage_spec.lua:116`). Any design that needs server-rendered chrome to disappear is rejected, not negotiated.

Git and LSP state are likewise not server-visible, so no chrome may claim to show them.

## What the client may own

The surfaces the protocol hands to clients, with the attach set `ext_messages`, `ext_cmdline`, `ext_multigrid`:

- the command line, as a top-third overlay
- the message area, as a stack, following the Emacs echo-area and `*Messages*` idiom
- the popup menu, including a documentation preview float built from the pum info field
- the terminal palette, via `ext_termcolors`

Server-originated bytes pass through untouched: the client never rewrites text the server produced.

## Contracts that the review found at risk

- **`msg_show` semantics.** The message stack must preserve the upstream contract exactly: message kinds, replacement of a message with the same id, streaming and nested output, and `msg_history_show`. Read `.references/neovim/src/nvim/api/ui_events.in.h` and `runtime/doc/api-ui-events.txt` for the event shapes, and honor them rather than a simplification that looks the same in the common case.
- **Cmdline collision.** The overlay occupies screen space the server also draws into. Define the collision mechanism explicitly, and make it degrade rather than overlap.
- **`ext_termcolors` restore.** On exit the terminal palette must be restored (OSC 104), including on an abnormal exit path.
- **Narrow terminals.** Specify and implement the degradation at 20 columns rather than letting it clip.

## The design system

Motion default is `reduced`, overridable with `OXVIM_TUI_MOTION`. The only sanctioned animation is a 150 ms opacity fade-in on notifications; everything else is instant. Diagnostics carry a non-color letter as well as a color, so color is never the sole channel.

Palette, measured and fixed. Dark: bg `#16181d`, float `#1d2026`, visual `#2b3140`, muted foreground `#878c99`, accent `#da834f`, error `#f26f74`. Light: bg `#f3f5fb`, float `#e6e9f1`, visual `#c7cfe4`, muted foreground `#595d69`. Every text pair clears 4.5:1 on bg and on float; those two muted values are the lowest that do, so do not darken them.

Foreground-on-accent is forbidden at 1.91:1. The selected popupmenu row uses background-colored text on accent: 6.21:1 dark, 6.44:1 light.

Verify contrast rather than trusting these numbers: recompute the ratio for every pair you ship and put the table in your report. If a pair falls below 4.5:1, that is a defect in the palette and I want to know.

## Files

You own `crates/ox-tui/` and `crates/ox-ui/`. Do not edit other crates; coordinate via hub if the server side genuinely lacks an event you need.

## Verification

A TUI claim is only as good as a rendered frame, so drive a real PTY rather than asserting on internal state:

- Render under a real PTY at 80x24, and at 20 columns for the degradation case. Assert on the visible cells.
- Prove the palette reaches the terminal, and prove the restore on both a clean exit and a signal.
- Prove one `msg_show` sequence with a replacement and one with nested output render per the contract.
- `PATH="/home/alpha/.cargo/bin:$PATH" RUSTC_WRAPPER="" cargo test -p ox-tui -p ox-ui -- --test-threads=1`, and report before and after counts.
- Do not regress the workspace: 2004 tests currently pass with zero failures.

## Commits

Atomic, one concern each. Report to `.outline/sdd/reports/task-74.md` and commit it.
