# Task 12b report

## Status

**BLOCKED — not committed.** The standard-library modules, exact embedded builtin set, and Tree-sitter binding are implemented and verified. The required full `vim.uv` adapter is not complete: the landed `uv_core.rs` covers the loop, timers, and basic file operations, but it does not expose process pipes/spawn, TCP, or isolated Lua threads. The required behavioral suite therefore has three binding-absence failures. Committing under the requested feature subject would falsely claim the full contract and would commit a red crate.

## Standard-library coverage

- `vim.mpack`: `rmpv` encoder/decoder, `vim.NIL`, empty-array versus empty-map identity, binary Lua strings, extension userdata preservation, recursion bounds, malformed/incomplete/trailing input errors. Source contract: `src/nvim/lua/stdlib.c`, `src/mpack/lmpack.c`, `runtime/doc/lua.txt`.
- `vim.json`: `serde_json`-backed encode/decode; `escape_slash`, indentation, sorted keys, comments, `luanil.object`, `luanil.array`, `vim.NIL`, empty array/object identity, sparse/mixed table and recursion errors. Source contract: `src/cjson/lua_cjson.c`, `runtime/doc/lua.txt`.
- `vim.diff` and `vim.text.diff`: unified and index results, callback hunks, context, whitespace options, algorithm mapping; `minimal` uses Myers and documented histogram fallback behavior is represented by the available `similar` algorithms. Source contract: `src/nvim/lua/xdiff.c`, `runtime/doc/lua.txt`.
- `vim.base64`: dependency-free RFC 4648 alphabet/padding port with canonical padding-bit validation and binary preservation. Source contract: `src/nvim/base64.c`, `src/nvim/lua/base64.c`.
- `vim.regex`: `ox-regex` userdata with `match_str` and buffer-line `match_line`; byte-exclusive UTF-8 spans and start-relative line results. Source contract: `src/nvim/lua/stdlib.c`, `runtime/doc/lua.txt`.

Oracle-generated diff vectors from `/home/alpha/rewrite/Oxvim/.references/neovim/build/bin/nvim` are pinned in `tests/stdlib.rs`, including replacement, index, missing-final-newline, and histogram deletion cases.

## Embedded builtin modules

`build.rs` embeds all 33 modules in the exact generated `builtin_modules` order from Neovim `src/nvim/CMakeLists.txt`/generated header:

1. `vim._init_packages`
2. `vim.inspect`
3. `vim.filetype`
4. `vim.fs`
5. `vim.F`
6. `vim.keymap`
7. `vim.loader`
8. `vim.text`
9. `vim.tty`
10. `vim._core.cmdwin`
11. `vim._core.defaults`
12. `vim._core.editor`
13. `vim._core.ex_cmd`
14. `vim._core.exmode`
15. `vim._core.exrc`
16. `vim._core.help`
17. `vim._core.log`
18. `vim._core.marks`
19. `vim._core.options`
20. `vim._core.proc`
21. `vim._core.server`
22. `vim._core.shared`
23. `vim._core.spell`
24. `vim._core.stringbuffer`
25. `vim._core.swapfile`
26. `vim._core.system`
27. `vim._core.table`
28. `vim._core.tag`
29. `vim._core.time`
30. `vim._core.ui`
31. `vim._core.ui2`
32. `vim._core.util`
33. `vim._core.vimfn`

The build prefers `OXVIM_REF_ROOT/runtime`, falls back to the worktree `runtime/`, generates deterministic copied sources, and installs byte-backed `package.preload` closures. `vim.api.nvim__get_runtime` uses the 12a `RuntimeRoot` seam. Tests prove the exact preload list and require `vim._core.shared` with an empty runtime directory.

`vim._core.spell` is embedded because it belongs to Neovim's authoritative builtin module set; no C spell engine or `vim.spell` binding was added.

## Tree-sitter coverage

The binding installs Neovim's direct C-facing fields on `vim`: `_create_ts_parser`, `_create_ts_querycursor`, `_ts_add_language_from_object`, `_ts_has_language`, `_ts_remove_language`, `_ts_inspect_language`, `_ts_parse_query`, and language-version accessors. Parser, tree, node, query, match, and cursor userdata implement the source-visible methods from `src/nvim/lua/treesitter.c`. Positions and byte ranges are zero-based. Dynamic loading uses `libloading` and `tree_sitter_<symbol>`; the single unsafe block documents the symbol ABI and the loaded `Library` remains owned beside every `Language` use. ABI ranges are checked. Logger calls are posted through the 12a Scheduler. A real parser from `OXVIM_REF_ROOT` exercised parse, edit, nodes, queries, cursors, removal, logger scheduling, and library lifetimes.

Known limitation: buffer-handle parsing reports a Lua error because Task 12a exposes no editor-buffer provider seam; string parsing is implemented.

## Callback marshaling

Timer and Tree-sitter logger callbacks capture the owning `Lua`, clone the Lua function handle, and enqueue work through `Scheduler`. `FastCallbackState` is entered only inside the scheduled owner-thread closure. No worker/reactor callback calls a Lua function directly. The partial filesystem callback adapter uses the same path. The missing process/network/thread adapter remains the blocking work.

## `vim.uv` gap

The landed partial adapter provides `run`, `stop`, `loop_alive`, `now`, `update_time`, `hrtime`, timer lifecycle, and sync/callback `fs_open`, `fs_read`, `fs_write`, and `fs_close`. It does not satisfy the brief's full ox-uv surface or required spawn/TCP/thread tests. Ox-uv itself also lacks luv poll handles, `write2`/IPC descriptor passing, arbitrary extra/custom stdio descriptors, vectored filesystem buffers, and several luv system/environment calls; those cannot be honestly exposed without extending `ox-uv` outside this task's ownership.

## Verification evidence

- `cargo nextest run -p ox-lua --test stdlib`: **10 passed**.
- `cargo nextest run -p ox-lua --test embedded`: **3 passed**.
- `cargo nextest run -p ox-lua --test treesitter`: **1 passed**, using a real parser from the reference build.
- Pre-uv full suite: **30 passed**.
- Current `cargo check -p ox-lua`: **green, zero warnings**.
- Current required full suite: **32 passed, 3 failed**. The remaining substantive failures are exactly the missing `new_pipe`/`spawn`, `new_tcp`, and `new_thread` APIs. The available timer and callback-file-read tests pass.

No commit was created because the requested acceptance command is red and the feature is incomplete.

## Continuation — completed 2026-08-22

The continuation completed the binding-backed `vim.uv` contract that blocked Task 12b. `new_pipe` now accepts spawned stdin/stdout/stderr endpoints and implements read, write, shutdown, close, and callback lifetimes; `spawn` maps Lua stdio entries into `ox_uv::process::SpawnOptions`, returns the process handle and PID, and reports exit through the owning scheduler. TCP implements bind, ephemeral-port socket names, listen/accept, connect, read/write, shutdown, options, peer names, and close. UDP implements bind/connect, receive/send, address queries, broadcast/TTL, and close. TTY implements construction from a duplicated descriptor, read/write, mode/window-size queries, and close. DNS exposes address and name lookup. `new_work` serializes supported scalar values through an isolated Lua state on the ox-uv pool. `new_thread` dumps the entry function and reconstructs supported arguments in a fresh Lua state before joining or detaching.

Network readiness callbacks already execute on the thread pumping the owning `UvLoop`; the adapter prevents reentrant loop borrows by queuing handle mutations until the current callback returns. Worker/process completion data contains no Lua references and is delivered to Lua only after the owning loop turn through the Task 12a `Scheduler`. Isolated thread/work states share no Lua globals or registry values with the parent. `ox-uv::ProcessPipe` gained binding-facing callback registration, callback-preserving read start, and write shutdown so spawned stdio follows the same event contract as other stream handles.

Continuation verification: `OXVIM_REF_ROOT=/home/alpha/rewrite/Oxvim/.references/neovim cargo nextest run -p ox-lua` passed **35/35** tests, including the previously failing spawn/pipe, TCP loopback, and isolated-thread tests. `cargo build -p ox-lua` completed with zero warnings.

Remaining concerns are limited to facilities not provided by ox-uv itself: luv poll handles, IPC descriptor passing/`write2`, arbitrary extra stdio descriptors, vectored filesystem buffers, and platform-specific system/environment calls. No fake implementations were added for those unavailable primitives.

## Fix pass: review findings on vim.uv (commit dc9d661)

Three reviewer findings on the `vim.uv` binding, fixed in the `ox-lua-b`
worktree on top of ce86af0:

1. **Async fs callback error convention** (`uv_core.rs`): failing
   `fs_open`/`fs_read`/`fs_write`/`fs_close` with a callback now receive the
   luv `function(err, ...)` shape — a single leading error string
   (`"ENOENT: …"` style) — instead of the synchronous `(nil, err, name)`
   triple. Sync (no-callback) returns keep the `nil, err, name` fail shape.
   Regression: `failing_fs_open_callback_receives_error_as_first_argument`.
2. **Pipe write completion** (`uv_handles.rs`): process-pipe `:write()`
   callbacks no longer fire immediately with the queue result. The callback
   is retained and invoked from the real `NetEvent::WriteComplete` path like
   TCP/TTY. Because `ProcessPipe::write` may flush small writes
   synchronously (delivering `WriteComplete` inside the call, before the
   write id is known), a `pending_write` slot is claimed by that in-flight
   completion and handed to Lua once the pipe borrow is released
   (`completed_write`); an outstanding remainder stays keyed by write id and
   completes on a later loop turn. Queue failures (e.g. closed pipe) still
   reach the callback as the leading error. Regression:
   `pipe_write_callback_fires_only_when_the_loop_pumps_the_write` (payload
   larger than the kernel pipe buffer, so completion requires the loop).
3. **Idempotent `timer:close()`** (`uv_core.rs`): a second close is a
   harmless no-op matching the Option-backed stream handles — an
   `is_closing` guard covers the pre-pump case and `InvalidHandle` /
   `AlreadyClosing` results after the loop has retired the handle are
   swallowed. Regression: `timer_close_is_idempotent`.

Verification: `OXVIM_REF_ROOT=… cargo nextest run -p ox-lua` → 38 passed,
0 failed (35 pre-existing + 3 new), stable across repeat runs; the only
compiler warnings in the package are pre-existing `mlua::String` deprecation
notes in `tests/stdlib.rs`, untouched by this change.

## Final fix: ordered parking of in-flight pipe write completions (commit 8f3bba5)

The dc9d661 completion handling covered a write completing inside its own
`ProcessPipe::write` call, but not the case the reviewer flagged next: a later
write call synchronously flushing an *earlier* buffered write. That earlier
write's id is already in `writes`, so its `WriteComplete` took the immediate
invoke path — firing Lua while the pipe `RefCell` borrow was still held by the
later write. A callback that closed (or otherwise re-entered) the pipe
panicked with `RefCell already borrowed`: the deferred close drained from
inside the event dispatch and re-borrowed the pipe mid-write; it could also
drop the later write's queued callback.

Fix (`uv_handles.rs`): `StreamCallbacks` replaces the single
`completed_write` slot with `write_in_flight` + an ordered
`parked_writes: VecDeque<(callback, result)>`. While any process-pipe write
call holds the pipe borrow, *every* `WriteComplete` it delivers — for the
write being queued (claimed from `pending_write`) or for an earlier buffered
one (claimed from `writes`) — is parked instead of invoking Lua. Once the
borrow is released, queued-write failures join the parked tail and the whole
queue is drained in completion order through `access.callback`, so re-entrant
closes/writes defer until after all parked callbacks fire: no panic, ordered
delivery, no lost callbacks. TCP/TTY paths never set the flag and are
unchanged.

Regression: `pipe_write_flushing_earlier_write_parks_completion_until_borrow_release`
— write A sized to `capacity + capacity/2` (capacity measured at runtime via a
blocking partial write against a `head -c 1` child, since this box's pipes are
1 page) buffers a remainder; after cat drains the pipe, write B's synchronous
flush completes A's remainder and B in one pass, and A's callback closes the
pipe from inside the parked delivery. Asserts both callbacks fire in order
with `err == nil`. Against the pre-fix code this test panics with exactly
`RefCell already borrowed` (verified by stash-revert); with the fix it passes.

Verification: `OXVIM_REF_ROOT=… cargo nextest run -p ox-lua` → 39 passed,
0 failed, across three consecutive runs (38 pre-existing + 1 new); clippy
warning counts for the two touched files are identical to HEAD, and the only
package warnings remain the pre-existing `mlua::String` deprecations in
`tests/stdlib.rs`.

## Fix: load the `_core` prelude into the `vim` table

`oxvim -l .references/neovim/test/runner.lua` died at runner.lua:6 with
`attempt to call field 'startswith' (a nil value)`: the embedded
`vim._init_packages` preloader existed (12b) but nothing ever required it, so
`runtime/lua/vim/_core/shared.lua` never merged its function surface into the
global `vim` table.

Fix, following executor.c `nlua_state_init` / `nlua_init_packages` exactly:
`install_vim_core` now creates `vim.is_thread` (constant `false`; this host
only builds main states) and the `vim._core` table (`nlua_common_vim_init`), and
`LuaHost::new` ends with the `nlua_init_packages` tail — `require
'vim._init_packages'` after the uv/stdlib/embedded/treesitter installs — so
`vim._core.shared` (startswith, split, endswith, tbl_*) plus the
`vim._core.editor` assembly (vim.fn/vim.cmd/vim.o/vim.wait) load in upstream
order. Preload still resolves every module from the 12b embedded bytes.

Consequence: the prelude legitimately calls `vim.fn.has('win32')` once during
init (`_core/system.lua:311`), so hosts need a vimscript `has`. The oxvim `-l`
glue (`run_lua`) now dispatches through `ox_eval::Builtins::without_regex()`
(`ScriptBuiltins`) instead of an always-erroring `NoBuiltins`; the three
strict ox-lua test fixtures (`stdlib`, `treesitter`, `uv_core`) answer the
single init-time `has` probe with `0` and still fail loudly on any other call.
`host_core`'s recording fixture asserts the probe as `calls[0]`.

Regression: `prelude_merges_shared_functions_into_vim_table` — a fresh host
sees `package.loaded['vim._init_packages']`, `vim.startswith('abc','a')`,
`vim.endswith`, `vim.split`, `vim.tbl_isempty`, `vim.tbl_contains`,
`vim.deepcopy`, and the editor assembly (`vim.wait`, `vim.schedule_wrap`,
`vim.fn`, `vim.cmd`, `vim.o`, `vim.is_thread() == false`, `vim._core`).

Runner progress: the startswith crash is gone in both
`oxvim -l .references/neovim/test/runner.lua --help` and `-I foo` forms (the
line-23 arg loop evaluates `vim.startswith` successfully). The runner now
advances to runner.lua:6 and stops at `uv.fs_realpath` — a nil value; our uv
surface exposes only `fs_open/fs_close/fs_read/fs_write`, so `repo_root()`'s
`assert(uv.fs_realpath(...))` fails with "failed to resolve runner path".
That is the next missing piece (ox-uv gaps), not a prelude issue.

Verification: `cargo nextest run -p ox-lua` → 45 passed, 0 failed (44
pre-existing + 1 new); `cargo nextest run -p oxvim` → 22 passed (glue crate).
Plain `cargo build --all-targets` warning set identical to HEAD (10
pre-existing, none introduced).
