# Compatibility statement

Oxvim is a Rust rewrite of the Neovim core. This page states what matches upstream today, the command that checks each claim, and what remains open.

## Target specification

Oxvim targets functional compatibility with Neovim 0.13.0-dev (API level 15): running existing Lua plugins unmodified, serving external MessagePack-RPC user interfaces, and matching upstream behavior for text manipulation, options, expressions, and event loops.

## Verification commands

- `just test`: workspace unit, property, and integration suite (`cargo nextest run --workspace`).
- `just apidiff`: compares the `--api-info` schema against the compiled Neovim oracle.
- `just replay`: replays recorded YAML MessagePack-RPC sessions against both binaries.
- `just differential`: release-binary smoke and PTY checks (`cargo nextest run -p differential`).
- `just functional`: upstream Busted functional suite against Oxvim via `NVIM_PRG`.
- `just oldtest`: upstream legacy Vimscript suite against Oxvim via `NVIM_PRG`, run on a copy of the testdir with a sandboxed `HOME`.

## Current compatibility

### API metadata

`just apidiff` normalizes both `--api-info` schemas and requires them equal: every public function with its parameter types, optionality flags, return types, deprecation markers, and method flags, plus error type enumerations, UI event definitions, `ui_options`, and the Ext handle `types` map, matches the oracle. The `version.build` field is ignored and is the only permitted difference.

### MessagePack-RPC replay

`just replay` runs each session against Oxvim and the oracle and requires matching response streams and notification ordering:

- `core.yaml`: API info, buffer creation, line updates, Lua feature detection, `normal! ggdd`, and final buffer contents.
- `options.yaml`: boolean and integer option assignment plus type validation of rejected values.
- `eval.yaml`: arithmetic, string functions, list indexing, and dictionary lookups.
- `channels.yaml`: terminal channel allocation, `nvim_chan_send` routing, and unknown-method error events. Socket channels are not covered.
- `ui_attach.yaml`: initial redraw events ordered before the attach response, negotiated extension options, default colors, highlight tables, and mode info.

Known divergences are recorded in `tests/differential/SKIPS.md` and admitted only through `replay --bless --reason`. Two exist today:

- `core.yaml`: Oxvim's live `nvim_get_api_info` builder assembles an incomplete metadata surface (the canonical `--api-info` schema is complete and passes `apidiff`); all subsequent core smoke responses match upstream.
- `ui_attach.yaml`: the deterministic compositor/highlight snapshot omits runtime-specific title, cwd, the full default highlight corpus, and secondary mode/mouse frame.

### Release binary differential

`just differential` checks the release binary:

- An embedded smoke check spawns `oxvim --embed` and verifies the MessagePack handshake, channel identity, and API level 15.
- PTY checks run Oxvim in an 80x24 terminal with `TERM=xterm-256color`: they wait for terminal setup, insert text in Insert mode, drive the `:edit x` command-line overlay with a nested `Ctrl-R =1+1` expression level and cancel both, send `:q!`, and assert exit code 0, cursor-shape restore, and OSC 104 palette reset. Further PTY cases cover command-line overlay nesting and wildmenu protocol levels, message sticky-expiry versus same-id replacement, and colorscheme re-theming at a single batch boundary.

### Options and mappings

- Composite option mutation (`append`, `prepend`, `remove`, the `:set +=`, `:set ^=`, `:set -=` merges, including comma-list, flag-list, `key:value` item, and `$VAR` expansion rules) is implemented in the ox-api option merge and covered by the workspace suite.
- Key mappings, including `<Nop>`, parsed Ex-command mappings, and mappings backed by Vimscript expressions (`Expr`) or Lua callbacks, execute through the interactive input loop.

### Architecture invariants

- Buffer text is a Ropey-backed rope with transactional undo (ox-text).
- The event loop uses `mio` to poll file descriptors and drains the hierarchical `MultiQueue` at deferred safe points; `WorkQueues` classifies each posted item as Fast or Deferred work (ox-loop).

## Known gaps

- Terminal channels connect and route data, but parsing terminal VT escape sequences into buffer cells is not implemented.
- Unsetting a local option value with `nil` is not yet supported in the editor options store.
- Advanced `vim.uv` features (custom polling priorities, non-standard file descriptor redirection, and platform system metrics) return typed `Unsupported` errors.
- Windows is untested; verification runs on Linux and POSIX-compliant systems.

## In-progress upstream suites

`just functional` (Busted) and `just oldtest` (legacy Vimscript) run Neovim's own suites against the Oxvim binary through `NVIM_PRG`. Both are being enabled test by test.
