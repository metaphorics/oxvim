# Task 21 — structured `nvim_cmd`

## Status

BLOCKED after implementing and verifying structured command execution. The runtime now advances through `vim.cmd.highlight('clear')`, all of `runtime/colors/vim.lua`, named default highlight colors, and the prior `g:colors_name` scope-loss point. `just functional` still cannot reach its terminal count summary because the functional child-session listener lifecycle reuses an occupied address: T1 reports `RPC server failed: network I/O failed: Address already in use (os error 98)`, then T2 waits indefinitely. This is a harness/session-address architectural blocker rather than an `nvim_cmd` execution failure.

## Changes

- Added structured `nvim_cmd` decoding for command name, arguments, bang, count, ranges, register, magic-bar handling, and supported command modifiers, with validation for malformed/unknown fields.
- Executed decoded commands through the 8e `ExExecutor` seam and captured/removes newly emitted echo messages when `opts.output` is true; returns an empty string otherwise.
- Routed external RPC `nvim_cmd` and scoped runtime `vim.cmd.*` calls to real editors; covered `vim.cmd.highlight('clear')` and captured `:echo` output end to end.
- Synchronized editor global variables into and out of the persistent Ex scope, including after Lua execution, so Lua-set `vim.g` values remain visible to following Ex commands.
- Accepted named and `#RRGGBB` highlight colors used by the default runtime colorscheme.
- Propagated `:quit` from RPC `nvim_command` and structured `nvim_cmd` into the stdio server exit state.
- Aligned the callback/command autocmd conflict diagnostic reached by the first functional assertion.

## Commit

- `ff25d9f feat: execute structured Ex commands from Lua`

## Verification

- `cargo nextest run -p ox-api -p ox-editor -p oxvim`: **522 passed, 0 skipped** after the final source changes.
- `cargo build -p oxvim --release`: passed after the final commit.
- Focused real embedded smoke: dynamic `vim.cmd.highlight('clear')`, external structured `nvim_cmd` output capture, and the first functional autocmd validation request all return without a child crash.
- Final `just functional`: T1 reaches functional execution but the spawned RPC child fails with `Address already in use (os error 98)`; T2 then waits. The bounded 150-second run was terminated, so no suite counts exist.

## Suite end state

BLOCKED: the functional harness/session layer does not allocate or release a fresh child RPC listen address after prior child termination. The earlier colors/`vim.lua`, structured-command stub, Lua/Ex global-store drift, and RPC quit propagation blockers are resolved.

## Concerns

- Direct top-level `nvim_exec_lua` still uses the long-lived API bindings; the verified dynamic `vim.cmd.*` path is the scoped Ex-Lua path used by runtime startup. A durable switchable editor target would unify both paths and remove the remaining scoped-callback lifetime limitation noted in Task 20.
- `magic.file=false` is validated but the current 8e command representation has no per-argument filename-expansion flag; `magic.bar=false` is preserved by escaping bars.
- `mods.filter` is rejected rather than silently ignored because the current executor has no filter-modifier representation.
