# Task 26: job control

## Status

Implemented reactor-driven `jobstart()`, `jobstop()`, `jobwait()`, `jobpid()`, `chansend()`, and the requested `jobsend()` alias. Jobs use editor-shared dynamic channel IDs, `ox-uv` process/PTY handles, deferred stdout/stderr/exit events, stdin writes, SIGTERM stop, finite/infinite waits, cwd/environment/detach/stdin/pty/term/rpc options, buffered stream delivery, Vimscript options-dictionary `self`, and Lua callback references.

## Commits

- `423a26f feat(ox-editor): add reactor-driven job control`
- `413f4bb feat(oxvim): wire shared job channels and callbacks`

## Verification

- `cargo nextest run -p ox-eval -p ox-editor -p oxvim`: 848 passed, 0 skipped.
- Vimscript smoke: `jobstart(['sh', '-c', 'exit 7'])`, `jobpid()`, and `jobwait()` returned a live PID and `[7]`.
- Dictionary callback smoke: `s:logger` received stdout and exit events with `self.d_events` mutation; focused function module passed 31/31.
- Lua smoke: `vim.fn.jobstart()` with Lua `on_stdout`/`on_exit` callbacks and `vim.fn.jobwait()` completed with status 0 and both callbacks observed.
- Oldtest: unchanged `make test_arabic NVIM_PRG=/home/alpha/rewrite/Oxvim/target/debug/oxvim` executed the selected test body and produced a genuine failed child result: exit code 1, a one-line screen snapshot, and two callback events (`stdout`, `exit`). The outer runner then exited on its still-unimplemented `:cquit`; job creation, callbacks, waiting, and result collection all completed.

## Concerns

- The oldtest body/result gate is met, but Oxvim still reports `not implemented: cquit` after the runner has formatted the failed result, so the Make target itself ends with the wrapper error and does not create a `.res` target file.
- `rpc` is registered as channel metadata/state but this slice does not add msgpack-RPC decoding of job stdout.
- Lua calls nested inside `:lua` use a dedicated executor to avoid re-borrowing the active Ex executor; it shares channel IDs but not the Vimscript executor's job lookup table.
