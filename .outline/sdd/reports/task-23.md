# Task 23 — nested deferred-close draining

## Status

BLOCKED after implementing and verifying callback-scoped nested-run draining. The exact ox-lua deadlock regression now completes, and both owned crate suites are green. The functional suite still stalls at T2 setup after the expected T1 autocmd diagnostic mismatch, so it does not print terminal counts.

## Cause and change

`LoopAccess::apply` correctly deferred close operations requested inside UV callbacks, preserving the 12b borrow-release guarantee, but `vim.uv.run` and `vim._core.loop_poll` attempted to reborrow the already-running `UvLoop`. Deferred operations and process-exit delivery were therefore unavailable to nested runs/waits.

`LoopAccess` now retains callback-scoped access to the exact active loop, drains deferred operations only through that access after Lua callback borrow sites have released, and routes both `vim.uv.run` and wait polling through the same seam. `UvLoop::run_nested` preserves an outer run mode. Completed process callbacks are delivered after every pump, including wait polling, rather than only after explicit `vim.uv.run` calls.

## Regression

`nested_run_drains_pipe_close_requested_by_write_callback` spawns `/bin/cat`, enters a pipe read callback, requests child-stdin close, and repeatedly calls nested `vim.uv.run('once')` until the child-exit callback fires. Before the fix it failed immediately at `uv_handles.rs:601` with `RefCell already borrowed`; after the fix it completes without timeout. The existing synchronous pipe-write borrow-release regression remains green.

## Commit

- `c57f264 fix(ox-lua): drain deferred closes in nested waits`

## Verification

- Focused pre-fix regression: **0 passed, 1 failed** (`RefCell already borrowed`).
- Focused post-fix regression: **1 passed, 0 failed**.
- Full `ox-lua` UV module: **22 passed, 0 skipped**.
- Required `cargo nextest run -p ox-lua -p ox-uv`: **105 passed, 0 skipped**.
- Release build: `cargo build -p oxvim --release` passed.
- `just functional`: T1 reports the known autocmd diagnostic mismatch; T2 then remains stuck until a bounded 120-second run is terminated, without terminal counts.

## Suite end state

BLOCKED — `functional/api/autocmd_spec.lua` T2 setup still retains the T1 child stdin and does not reach the nested `vim.uv.run`/wait seam exercised by the regression. This is the next named blocker: the functional runner executes T2 while still inside the long-lived callback/deferred-operation frame that queued T1 teardown, with no loop pump between cases.

## Concerns

- Callback-scoped loop access uses a narrowly documented `NonNull<UvLoop>` reborrow whose lifetime is bounded by synchronous `LoopAccess::callback`; it must never escape that callback.
- The exact nested-run contract is fixed and crate-tested, but the functional runner exposes an additional between-test safe-point problem: queued T1 teardown is not pumped before T2 begins.
