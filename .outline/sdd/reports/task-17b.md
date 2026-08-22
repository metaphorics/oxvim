# Task 17b — sanctioned environment mutation and runner continuation

## Status

The environment-mutation blocker is resolved. `ox-sys` owns the workspace's two documented unsafe standard-library calls, and both `ox-uv` and `ox-eval` consume that boundary without relaxing their `forbid(unsafe_code)` policies.

The rebuilt upstream runner progresses past `vim.env.VIMRUNTIME = ...` (`runner.lua:48`) and reaches `vim.v.servername` (`runner.lua:53`). It still exits before busted loads or prints spec pass/fail counts because the embedded Lua core does not install `vim._getvar`.

## Commits

- `d760606c5b92cdfe3a164859e357363f67dc92e1 chore: add ox-sys`
- `8c55730c77454859dafe5003b662e993aa66bbb0 feat(ox-uv,ox-eval): env mutation through ox-sys`

## Implemented behavior

- Added `crates/ox-sys` to the workspace with only `set_env(name, value)` and `unset_env(name)` as its public operations.
- Each operation contains one documented unsafe call to `std::env::set_var` or `std::env::remove_var`.
- The shared safety contract requires callers to exclude concurrent process-environment reads and writes. It records Oxvim's main-thread initialization/script-execution use, the `ox-uv` worker pool's no-environment-read invariant, and Miri's inability to validate the process-wide concurrency condition.
- `ox-uv::misc::os_setenv` and `os_unsetenv` now perform their libuv-compatible mutations through `ox-sys` and return `Ok(())`.
- The `ox-eval` `setenv()` builtin stringifies ordinary typvals, treats `v:null` as deletion, returns numeric zero, and delegates mutation to `ox-sys`, matching `.references/neovim/src/nvim/eval/funcs.c:6362-6381`.

## Verification

- `cargo test -p ox-sys`: passed (0 unit tests, 0 doc tests).
- `cargo nextest run -p ox-uv -p ox-eval`: **397 passed, 0 skipped** after consumer wiring.
- `cargo build -p oxvim`: passed after both code commits.
- `cargo nextest run -p ox-sys -p ox-uv -p ox-eval`: **397 passed, 0 skipped**.
- `target/debug/oxvim -l .references/neovim/test/runner.lua`: exits 1 at `runner.lua:53`, after the former line-48 `setenv()` blocker, with `vim/_core/editor.lua:546: attempt to call field '_getvar' (a nil value)`; no spec counts print.

## Named runner blocker

`vim._getvar(scope, handle, name)` is not installed by `ox-lua::install_vim_core`, although `runtime/lua/vim/_core/editor.lua` uses it for every `vim.g`, `vim.b`, `vim.w`, `vim.t`, and `vim.v` lookup. The immediate lookup is the read-only `v:servername` value, but a special-case empty-string function would duplicate state and only suppress the symptom. An honest implementation needs a variable-access host seam backed by the editor's global, buffer, window, tabpage, and `v:` stores, plus ownership of the server lifecycle value. That cross-crate state bridge is not a quick runner-only addition, so iteration stops at this named blocker.

## Concerns

- `ox-sys` cannot enforce its process-wide concurrency precondition; callers must preserve the documented main-thread/worker-pool invariant.
- Environment-mutating tests use unique names and clean them up, but Rust's process environment remains global to each test process.
- Runner specs have not begun; passing the three-package gate proves the changed contracts, not broader upstream compatibility.
