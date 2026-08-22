# Task 18 — Functional harness child spawn

## Status

Complete for the assigned spawn contract. The harness now creates child processes and starts individual functional cases instead of rejecting `stdio` at `test/client/uv_stream.lua:204`. The complete corpus reached its terminal summary, with the intervention documented below.

## Root cause and changes

- Minimal pre-fix reproduction: `target/release/oxvim -l /tmp/oxvim-spawn-repro.lua` rejected the harness shape `{ pipe, pipe, 1, nil }` with `stdio entries must be pipe handles or nil`.
- `vim.uv.spawn` now accepts uninitialized pipe handles as create-pipe requests, `nil`/`false` as ignored descriptors, and integers 0, 1, or 2 as exact inherited parent descriptors.
- Numeric inheritance duplicates the selected parent standard descriptor safely before configuring `std::process::Command`; this preserves luv's “same zero-indexed fd” semantics when the entry appears in another stdio position.
- `args` retain sequence order. Non-nil `env` is parsed as `NAME=VALUE` entries and replaces the parent environment through the existing `ox-uv` `env_clear` path. Exit delivery remains `(code, signal)`.
- The last Lua process or process-pipe wrapper now closes its loop handle on collection. This prevents failed session construction from leaving most child processes and active handles alive.

## Commits

- `c1b2b30 fix(ox-lua): honor luv spawn options`
- `b6636cf fix(ox-lua): close abandoned process handles`

## Verification

- Post-fix minimal reproduction: `target/release/oxvim -l /tmp/oxvim-spawn-repro.lua` printed `spawn-ok`; it checked the four-entry stdio shape, ordered arguments, replacement environment, pipe output, and numeric `(code, signal)` callback.
- Focused regression: `spawn_accepts_luv_stdio_environment_args_and_exit_callback` passed.
- Focused functional iteration: `autocmd_spec.lua` ran cases `T1` onward and completed with **1 skipped, 88 failed**; failures moved from `uv_stream.lua:204` spawn rejection to the next missing surface, `vim.mpack.Packer` at `rpc_stream.lua:41`.
- `cargo nextest run -p ox-lua -p ox-uv -p oxvim`: **123 passed, 0 skipped** after the final source edit.
- Full `just functional`: terminal summary reported **0 passed, 41 skipped, 7713 failed, 141 errors**. Against the assigned baseline **0 passed, 7713 failed, 141 errors**, the numerical delta is **0 / 0 / 0**, while the common failure root moved beyond spawn.

## Concerns

- The full run executed all 9,338 indexed cases but retained one child created by `shada_spec.lua` with `--cmd qall`. Because the next missing `vim.mpack.Packer` surface aborts `Session.new` after spawn, its Lua exit callback retains the `ProcStream` table and its stdin pipe. After all cases had run, the verified lingering child required `SIGTERM`; the runner then printed the terminal summary and exited in 328.37 seconds. Thus the counts are complete and honest, but `just functional` is not yet unattended-clean until streaming mpack support lets session construction finish or the harness/session lifecycle gains failure cleanup.
