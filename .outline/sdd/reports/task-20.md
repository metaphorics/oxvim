# Task 20 — Lua Ex commands

## Status

BLOCKED after completing the Lua Ex-command seam and server wiring. The functional runner advances past the original `not implemented: lua` failure and the nested current-editor `RefCell` panic, but cannot reach a terminal summary because the next startup command, `vim.cmd.highlight('clear')` in `runtime/colors/vim.lua:12`, routes through the general structured `nvim_cmd` API, whose registry entry is still an unimplemented stub. Correctly fixing that requires a reentrant structured-Ex dispatch seam between `ox-api` and the active `ExExecutor`; a startup-only `highlight clear` special case was rejected.

## Changes

- Added the typed `LuaExec` / `LuaExecError` host seam to `ox-editor` and injected it into the persistent Ex interpreter.
- Implemented `:lua`, `:luafile`, and `:luado`, including `:lua =expr`, ranged buffer Lua source, whole-buffer default `:luado`, `(line, linenr)` arguments, string/number line replacement, typed/catchable Lua error codes, and stopping after a callback switches buffers.
- Propagated the Lua host through nested Ex execution: control blocks, sourced scripts, user functions, `:execute`, `:global`, and user commands.
- Wired the server-owned real `LuaHost` into `ExExecutor`.
- Added scoped current-editor bindings for `vim.api` and `vim.g`/`b`/`w`/`t`/`v` access during Ex Lua execution, avoiding the nested `RefCell` panic while preserving the shared Lua state.
- Added fake-host executor coverage and a real embedded-server smoke covering persistent Lua state, `vim.api.nvim_get_current_buf()`, and `vim.g` mutation from `:lua`.

## Commits

- `a32a394 feat(ox-editor): execute Lua Ex commands`
- `1f5e8cd feat(oxvim): wire Ex commands to Lua host`
- `594b84b fix(oxvim): bind Ex Lua to current editor`
- `dc59709 fix(ox-editor): stop luado after buffer switch`

## Verification

- `cargo nextest run -p ox-editor -p oxvim`: **469 passed, 0 skipped** after the final source edit.
- Focused real-host smoke `lua_integration_smoke`: passed; `:lua` called `vim.api.nvim_get_current_buf()` and mutated/read `vim.g` without a nested editor borrow panic.
- Fresh release build: `cargo build -p oxvim --release` passed before the final functional run.
- `just functional` after the initial wiring reproduced the nested editor `RefCell` panic; the scoped current-editor binding removed it.
- Final `just functional`: T1 reached `runtime/colors/vim.lua:12` and failed with `E5108 ... API function is not implemented` from `vim.cmd.highlight('clear')`; T2 then waited, and the command hit its 1,200-second deadline. **No terminal pass/fail counts exist and none are fabricated.**

## Suite end state

BLOCKED: general `nvim_cmd` structured-command execution has no implementation/reentrant bridge to the active Ex executor. The original `:lua dofile("runtime/colors/vim.lua")` dispatch and editor-context ownership blockers are resolved; startup now enters that Lua file and stops specifically at its first structured Ex command.

## Concerns

- Lua code can cache one of the temporary scoped `vim.api` callback values and call it after the Ex command returns; mlua correctly invalidates that escaped scoped callback. A durable solution should install stable API functions backed by a switchable current-editor target rather than publishing scoped functions. This is coupled to the same reentrant editor/API ownership architecture needed for `nvim_cmd`.
- `:luafile` intentionally resolves the mutable global `loadfile`, matching upstream `nlua_exec_file` documentation in `lua/executor.c:2052-2055`; a review suggestion to bypass the global loader was declined as contrary to upstream semantics.
