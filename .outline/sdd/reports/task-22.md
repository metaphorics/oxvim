# Task 22 — listen address lifecycle

## Status

BLOCKED after completing and verifying the listener lifecycle. Unix listeners now bind the exact requested path, recover dead stale paths without replacing live listeners, remove only the socket inode they own, and report the actual bound address through `vim.v.servername`. Combined `--embed --headless --listen` sessions now serve stdio and stop immediately when stdio reaches EOF.

`just functional` advances through T1 instead of crashing with `EADDRINUSE`, but still cannot print its terminal counts: T1 now reaches the next API-parity failure (`"Conflict: 'pattern' not allowed with 'buf'"` expected, `"Cannot use both 'pattern' and 'buffer'"` actual), and T2 blocks while the runner closes the T1 session.

## Changes

- Added dead-socket probing before Unix bind retry. A successful probe preserves an active listener; a failed probe permits stale-path removal and exact-path rebinding.
- Recorded the bound socket's device and inode, and remove the path on close only while it still names that owned socket.
- Closed the top-level TCP/pipe listener on every server-loop return path.
- Added poll-driven stdio RPC handling to the listener event loop for the functional harness's `--embed --headless --listen` child shape.
- Propagated stdio EOF and read/dispatch failures by stopping the listener loop; added direct process tests for graceful quit and raw stdin EOF.
- Covered exact servername reporting, stale-path replacement, active-listener preservation, sequential same-address reuse, cleanup, combined stdio/listener operation, and TCP behavior through the existing smoke suite.

## Commits

- `2e971d7 fix(ox-uv): reclaim stale Unix listener sockets`
- `000e43e fix(oxvim): serve embedded stdio while listening`

## Verification

- Pre-fix red signal: `target/debug/oxvim --headless --listen /tmp/oxvim-task22-stale.sock` exited 1 with `Address already in use (os error 98)` when the path pre-existed.
- Focused Unix pipe module: **3 passed, 0 failed**.
- Full oxvim smoke file after the combined transport change: **12 passed, 0 failed**; the subsequently added exact EOF regression also passed.
- Final required command, `cargo nextest run -p oxvim -p ox-uv`: **71 passed, 0 skipped**.
- Release build: `cargo build -p oxvim --release` passed before the final functional iteration; production source was unchanged afterward except formatting by the commit hook.
- `just functional`: T1 reached an ordinary assertion mismatch in 228.57 ms; T2 then waited indefinitely, so no suite counts were emitted.

## Suite end state

BLOCKED — `ox-lua` process-pipe closure is the next named blocker, outside this task's `crates/oxvim/` and `crates/ox-uv/` ownership. During T2 setup, the runner retains the T1 child-stdin writer (`fd 13`, pipe inode `36453216`) and the T1 child remains alive. The direct `embedded_listener_exits_when_stdio_reaches_eof` regression proves oxvim exits and removes its socket when EOF actually arrives. Source inspection identifies the likely deadlock: `LoopAccess::apply` defers pipe close while inside a uv callback (`crates/ox-lua/src/uv_handles.rs:100-104`), while the harness immediately enters nested `vim.uv.run('once')` waiting for child exit; deferred operations drain only after the outer callback returns (`uv_handles.rs:109-120`).

## Concerns

- The functional suite cannot reach totals until the `ox-lua` deferred-close/nested-run deadlock is fixed; changing the next autocmd diagnostic would only move the assertion and would not resolve T2 teardown.
- Windows named-pipe support remains unavailable in the existing ox-uv implementation; this task preserves the existing explicit unsupported result rather than pretending Unix path logic applies to Windows.
