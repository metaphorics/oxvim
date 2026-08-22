# Task 24 — functional runner between-case lifecycle

## Status

PARTIAL: the original T1→T2 stall is fixed and a focused functional run prints its terminal summary. The requested three-crate verification is green. The unfiltered compatibility inventory progresses through thousands of cases without the old stall, but did not finish within the upstream 1,200-second default timeout; an extended run reached T7410 before its 3,000-second timeout, and a final longer inventory run was stopped when the session was directed to wrap up. Therefore no honest unfiltered aggregate counts are available.

## Diagnosis

The harness already pumps after each test through `run_test()` → `vim.wait(0)` → `vim._core.loop_poll(0, false)`, and ox-lua routes that poll through `LoopAccess::poll`. The stall happened before the next useful pump:

1. T1 failed on the real autocmd diagnostic mismatch.
2. T2's inherited `before_each(clear)` attempted to replace the current RPC session.
3. `Session:close()` checked `_timer:is_closing()` and then `_prepare:is_closing()`.
4. ox-lua phase userdata exposed `start`, `stop`, `is_active`, and `close`, but not luv's `is_closing` method.
5. The T2 before hook therefore failed before `ProcStream:close()` could close T1's stdin and before T2 could spawn.
6. The harness still entered its after hook, which called `session:next_message(0)` on retained T1; that entered `uv.run('default')` with the child stdin still open and no terminal event, producing the apparent between-case pump stall.

A syscall trace corroborated the causal chain: the stalled runner retained all three T1 child descriptors, issued no second child `execve`, and blocked in its poller after announcing T2.

## Change

`PhaseHandle` now forwards `is_closing` for idle, prepare, and check handles, and `LuaPhase` exposes it to Lua. This matches luv's shared handle contract and lets session cleanup reach the existing pipe-close and loop-pump paths instead of failing before them.

Regression `phase_handles_support_between_case_cleanup` exercises all three public constructors, checks the state before and after `close`, and calls `vim.wait(0)` to cover the between-case close pump. It failed before the fix with `attempt to call method 'is_closing' (a nil value)` and passes after it.

## Commit

- `c0aa0fb fix(ox-lua): expose phase handle closing state`

## Verification

- Pre-fix focused regression: **0 passed, 1 failed** at missing `is_closing`.
- Post-fix focused regression: **1 passed, 0 failed**.
- Full `cargo test -p ox-lua --test uv_core`: **23 passed, 0 failed**.
- `cargo nextest run -p ox-lua -p ox-uv -p oxvim`: **132 passed, 0 skipped**.
- Focused functional command (`TEST_FILE=.../api/autocmd_spec.lua`, `TEST_FILTER=nvim_create_autocmd`, `just functional`) printed a terminal summary: **18 total; 1 passed, 1 skipped, 16 failed**.
- Unfiltered `just functional`: default 1,200-second run reached T7101; extended 3,000-second run reached T7410. Both timed out because the current compatibility inventory takes roughly 224 ms for most cases, not because of the former T1→T2 lifecycle stall.

## Concerns

- The functional compatibility worklist is too slow to finish under Neovim's default 1,200-second CMake timeout, so its full aggregate remains unknown.
- The individual functional failures are the intended compatibility backlog and were not changed or suppressed.
