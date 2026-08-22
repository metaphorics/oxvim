# Task 19 — vim.mpack streaming sessions

## Status

Blocked on the next startup-level failure root after completing the assigned `vim.mpack` session surface. `Packer`, `Unpacker`, and the harness-required `Session` now load and pass focused and crate-wide verification. The release functional run enters real cases, but it cannot reach a terminal count because the next child startup failure makes the first case observe EOF and the second case wait indefinitely.

## Changes

- `vim.mpack.Packer([options])` is callable and encodes one object per call. It supports shallow-copied `ext` handlers and boolean/function `is_bin` options.
- `vim.mpack.Unpacker([options])` is callable with `(string, [startpos])`, retains fragmented MessagePack input, returns `(object, next_position)`, decodes concatenated objects by position, shallow-copies EXT handlers, invokes matching handlers, and returns raw payload strings for unhandled EXT values.
- `vim.mpack.Session({ unpack = unpacker })` supplies the functional client's request/reply/notification headers, response callback correlation, fragmented RPC receive state, five-value receive protocol, and safe request-id reuse. `package.loaded.mpack` aliases `vim.mpack`.
- All Lua uv surfaces now share one `LoopAccess`; `vim.uv.stop()` invoked from a timer/stream/process callback is applied reentrantly instead of panicking on a second `RefCell` mutable borrow.
- `:set` comma-list mutations now operate on complete escape-aware items, preserve empty items only for upstream list kinds that allow them, handle comma-colon keys, and let the harness apply `wildoptions-=pum` and prepend its runtime path. `:comclear` now clears the user-command registry during startup.

## Commits

- `cd424bd fix(ox-lua): add mpack streaming sessions`
- `950e4aa fix(ox-lua): frame functional RPC sessions`
- `b599071 fix(ox-lua): allow stopping uv from callbacks`
- `51e9874 fix(ox-editor): honor list option mutations`

## Verification

- Pre-fix focused regression failed at `vim.mpack.Packer` being nil.
- `cargo nextest run -p ox-lua`: **59 passed, 0 skipped** after all source changes.
- `cargo nextest run -p ox-editor`: **441 passed, 0 skipped**.
- `cargo build -p oxvim --release`: passed after each functional-root fix; `target/release/oxvim -l /tmp/oxvim-mpack-smoke.lua` exercised callable Packer/Unpacker and reported `vim.mpack.Session` as a function.
- The first post-build `just functional` exposed and reproduced the `RefCell already borrowed` panic at `uv_core.rs:217`; the focused `immediate_timer_callback_can_close_its_handle` regression passes after the fix.
- Final `just functional`: the release binary advanced beyond missing `Packer`, missing `Session`, and the uv-stop panic. T1 ran and failed after about 1.02 seconds with `Nvim EOF (crash?)`; T2 then hung. The command was terminated by its 1,200-second execution deadline, so **no terminal after-count exists**.

## Suite counts

- Before: **0 passed / 7,713 failed / 141 errors** (plus 41 skipped in Task 18's terminal report).
- After: **unavailable, not fabricated** — the suite did not reach its summary because T2 hung. Therefore a numeric delta cannot be reported honestly.

## Next failure root

The sanctioned option work moved startup through `wildoptions-=pum`, `runtimepath^=...`, and `comclear`. The next shared root is the harness startup command `lua dofile("runtime/colors/vim.lua")`, which exits because Ex dispatch reports `not implemented: lua`. Focused T1 consequently observes child EOF. The earlier T2 hang follows the same startup-exit path; separately, teardown calls `_prepare:is_closing()`, but the current `LuaPhase` surface has no `is_closing` method, producing a named cleanup error instead of closing cleanly.

## Concerns

- Full after-counts remain blocked: the final full run reached T1 and then T2 but hit its 1,200-second deadline without a terminal summary. The supplied baseline remains **0 / 7,713 / 141**; no after delta is fabricated.
- The next load-level work is Ex `:lua` command dispatch; the separate small lifecycle gap is `LuaPhase:is_closing`.
- The harness-required `Session({ unpack = ... })` path is implemented; upstream's separate unpacker-free, three-return Session mode is not part of this compatibility increment.
