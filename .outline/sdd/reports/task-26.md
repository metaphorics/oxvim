# Task 26: job control

## Status

Implemented reactor-driven `jobstart()`, `jobstop()`, `jobwait()`, `jobpid()`, `chansend()`, and the requested `jobsend()` alias. Jobs use editor-shared dynamic channel IDs, `ox-uv` process/PTY handles, deferred stdout/stderr/exit events, stdin writes, SIGTERM stop, finite/infinite waits, cwd/environment/detach/stdin/pty/term/rpc options, buffered stream delivery, Vimscript options-dictionary `self`, and Lua callback references.

## Commits

- `423a26f feat(ox-editor): add reactor-driven job control`
- `413f4bb feat(oxvim): wire shared job channels and callbacks`

## Verification

- `cargo nextest run -p ox-editor -p oxvim`: 487 passed, 0 skipped.
- Vimscript smoke: `jobstart(['sh', '-c', 'exit 7'])`, `jobpid()`, and `jobwait()` returned a live PID and `[7]`.
- Dictionary callback smoke: `s:logger` received stdout and exit events with `self.d_events` mutation; focused function module passed 31/31.
- Lua smoke: `vim.fn.jobstart()` with Lua `on_stdout`/`on_exit` callbacks and `vim.fn.jobwait()` completed with status 0 and both callbacks observed.
- Oldtest: `make test_arabic NVIM_PRG=.../target/debug/oxvim` spawned and waited for the child instead of failing at `jobstart`; it then failed in outer `runnvim.vim` at `Main[12]` (`getline(1, '$')`, E488), so the selected test did not produce a `.res` result.

## Concerns

- The required oldtest body/result gate remains blocked by the pre-existing post-job `getline(1, '$')` parsing failure in the outer runner; job creation and waiting are no longer the blocker.
- `rpc` is registered as channel metadata/state but this slice does not add msgpack-RPC decoding of job stdout.
- Lua calls nested inside `:lua` use a dedicated executor to avoid re-borrowing the active Ex executor; it shares channel IDs but not the Vimscript executor's job lookup table.
