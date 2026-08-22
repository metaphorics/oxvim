# Task 17d — Functional harness signal and callback unblocking

## Status

Complete. The upstream functional harness now passes global setup, loads the functional corpus, executes its specs, and prints the terminal pass/fail summary.

## Changes

- Completed the luv signal surface in `vim.uv`: `new_signal`, module-form `signal_start`, `signal_start_oneshot`, and `signal_stop`, plus method-form `start`, `start_oneshot`, and `stop`.
- Accepted luv signal names or integers and delivered canonical signal names to callbacks.
- Added signal handle lifecycle inspection needed by functional cleanup.
- Bound the `vim.wait()` host primitives `vim._core.ui_flush`, `check_interrupt`, and bounded `loop_poll` to the owned UV loop.
- Reused callback-aware deferred loop access for timers so an immediate callback can inspect, stop, and close its own handle without re-borrowing the UV loop.
- Cached process/process-pipe closing state so read and exit callbacks can safely perform the same cleanup.

## Commits

- `943b508 fix(ox-lua): complete uv signal binding`
- `e94f319 fix(ox-lua): unblock functional wait loop`
- `4d24e85 fix(ox-lua): defer process cleanup in callbacks`

## Verification

- `cargo nextest run -p ox-lua --test uv_core`: **19 passed, 0 skipped** after the final callback-cleanup regression.
- `cargo nextest run -p ox-lua -p ox-uv`: **98 passed, 0 skipped**.
- `TEST_FILTER='api' just functional`: reached the summary with **1 skipped, 336 failed, 30 errors**.
- `just functional`: reached the complete corpus summary in 8.17 seconds with **0 passed, 41 skipped, 7713 failed, 141 errors**. Its non-zero exit is the expected compatibility worklist, not a harness load failure.

## Concerns

- The headless Lua host has no pending UI transport or interrupt state, so `ui_flush` is currently an empty flush and `check_interrupt` reports false. Adding interactive interrupt delivery or a Lua-driven attached UI requires an explicit host seam.
- Signal strings cover the portable Unix set exported by `signal-hook`, including luv aliases `sigiot` and `sigpoll`; platform-only `sigstkflt`, `sigpwr`, `sigbreak`, and `siglost` are not exposed as strings.
- Most functional failures currently originate at child-session spawn and represent the expected server/CLI compatibility worklist. Load errors also expose later missing surfaces such as `vim.text.indent` and LPeg; neither blocks harness completion.
