# Task 17c — Lua variable and option tables

## Status

Implemented the upstream `vim._getvar(scope, handle, name)` and `vim._setvar(scope, handle, name, value)` host seam and connected the runtime-created `vim.g`, `vim.b`, `vim.w`, `vim.t`, and `vim.v` tables to editor-owned variable dictionaries. Missing reads return Lua `nil`; assigning Lua `nil` deletes existing writable variables; buffer, window, and tabpage handles resolve through `Editor`; all `v:` writes fail with `E46` before mutation.

`vim.o`, `vim.go`, `vim.bo`, and `vim.wo` execute through `nvim_get_option_value` and `nvim_set_option_value` in the concrete API `Registry`. The Lua binding supplies the omitted trailing options dictionary accepted by the upstream Lua accessors.

Standalone `-l` scripts now receive a real `Editor`, core API registry, and variable host rather than a Lua-only builtin context. `v:servername` is initialized to the empty string for standalone and stdio modes. Listener mode replaces it after a successful bind with the actual TCP local address (including a dynamically selected port) or Unix-domain pipe path. Lua cannot overwrite the seeded value.

The runner also required `vim.uv.os_environ()` after variable access was unblocked. The binding now returns the process environment using the same portable lossy string policy as the existing `os_getenv` binding.

## Commits

- `01efccc8d044553e7a6dd076ecec6378c6b4d3cd feat(ox-lua,oxvim): wire editor variable tables`
- `51119863e756a313948c630be2f006a57eb318a6 feat(ox-lua): expose uv environment table`
- `546c2a0f90e95bf3260e242e1f9f25b826b5f393 fix(oxvim): keep pipe test stream writable`

## Verification

- `cargo nextest run -p oxvim --test smoke`: **10 passed, 0 skipped** after the table, E46, option routing, and actual listener-name tests.
- `cargo nextest run -p ox-lua -p oxvim`: **74 passed, 0 skipped** final required gate.
- The embedded-server Lua test round-trips global and compound values, current and explicit buffer/window/tabpage handles, deletes with `nil`, proves failed `v:servername` mutation preserves the value, and mutates global/buffer/window option tables through the registry.

## Runner end state

Command:

```text
target/debug/oxvim -l .references/neovim/test/runner.lua .references/neovim/test/functional/lua/option_and_var_spec.lua
```

The runner now completes global setup, selects and begins loading the requested spec, and prints counts. End state: **0 tests ran, 0 passed, 1 load error**. The precisely named blocker is the missing generated build module `test.cmakeconfig.paths`, required at `.references/neovim/test/testutil.lua:5`. The checked-in upstream reference tree contains no `test/cmakeconfig/paths.lua`; it is a Neovim CMake build artifact, so resolving it requires the larger upstream test-build integration step rather than another Lua primitive.

## Concerns

- Lua tables use the editor's canonical API-facing per-scope dictionaries. `ExExecutor` still owns a separate `ox_eval::Scope`; cross-language coherence between `:let g:x` and `vim.g.x` is not yet established by the current server architecture.
- Environment enumeration follows the existing portable `to_string_lossy` policy, so non-Unicode environment bytes are not preserved byte-for-byte.
- Upstream spec assertions have not run because the generated CMake test configuration module is absent; the reported zero-test count is harness progress, not spec success.
