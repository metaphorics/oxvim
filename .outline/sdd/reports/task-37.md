# Task 37: Lua variable synchronization and writable `v:` parity

## Status

Complete. The `:lua` global-variable corruption, scoped `vim.o` setter arity failure, and `v:testing` E46 oldtest blocker are fixed. The oldtest harness now passes `let v:testing = 1` and stops at the next named blocker: missing `luaeval()` at `runtest.vim:173`.

## Root cause and fixes

### Ex globals corrupted across `:lua`

`ExExecutor::execute_script()` keeps live Vimscript variables in its `Scope` until the command stream finishes. The direct Lua handlers called `LuaExec` against `Editor` first and synchronized `Editor` back into `Scope` afterward. A preceding `let g:probe = 'before'` therefore existed only in `Scope`; `vim.g.foo = 1` mutated the stale editor dictionary, and the post-Lua `sync_editor_into_scope()` replaced the complete live scope with that stale dictionary, deleting or changing unrelated values. The minimized red reproduction was a startup script containing:

```vim
let g:probe = 'before'
lua vim.g.foo = 1
let g:after_lua = g:probe . '!'
```

Before the fix it stopped at line 3 with `E121: Undefined variable: g:probe` (the prior Task 36 concatenation probe observed the same overwrite as E715). `command_lua`, `command_luafile`, `command_luado`, and the Lua colorscheme path now synchronize `Scope` into `Editor` before entering Lua, then synchronize Lua/editor mutations back afterward. The end-to-end regression verifies `probe`, Lua-created `foo`, and the post-Lua value all survive.

### Scoped `vim.o` writes

`with_scoped_editor_api()` defaulted the omitted opts dictionary for one-argument `nvim_get_option_value`, but not for two-argument `nvim_set_option_value`, unlike `ox_lua::bind_api`. The scoped binding now applies the shared arity rule to both operations. `:lua vim.o.background = 'light'` succeeds and the persistent Lua host reads the assigned value.

### Writable `v:` variables

The local Ex scope and both Lua variable-host paths rejected the entire `v:` namespace. A single `vim_variable_is_writable()` authority now mirrors the zero-flag entries in upstream `eval/vars.c::vimvars`: `errmsg`, `warningmsg`, `statusmsg`, `this_session`, `fcs_choice`, `scrollstart`, `swapchoice`, `char`, `mouse_win`, `mouse_winid`, `mouse_lnum`, `mouse_col`, `searchforward`, `hlsearch`, `oldfiles`, `completed_item`, `errors`, and `testing`.

The audit also confirmed that the suggested `count`, `count1`, `dying`, `register`, and `event` variables are upstream read-only; tests pin those names as non-writable, along with `servername`. `:let v:testing = 1` and `vim.v.testing = 1` work, while `:let v:count = 2` and writes to `v:servername` retain E46.

## Commits

- `8bda437 fix(editor): sync Ex variables before Lua execution`
- `c2e42e0 fix(oxvim): default scoped option setter opts`
- `dbd92e0 fix(editor): honor writable upstream v variables`

## Test summary

- Pre-fix red reproduction: `cargo nextest run -p oxvim --test smoke lua_script_preserves_vimscript_globals_across_lua_commands` — failed at startup script line 3 with `E121: Undefined variable: g:probe`.
- Focused regression set — 5 passed.
- Full changed integration file: `cargo nextest run -p oxvim --test smoke` — 18 passed, 0 skipped.
- Required gate: `cargo nextest run -p ox-editor -p oxvim -p ox-api` — 592 passed, 0 skipped.
- Oldtest binary: `cargo build -p oxvim` — succeeded.

## Oldtest end state

Invocation from `.references/neovim/test/old/testdir`:

`/home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim`

`v:testing` assignment at `runtest.vim:171` now succeeds. The harness exits at the next deterministic named blocker:

**`not implemented: luaeval` — `runtest.vim:173`, `let s:has_ffi = luaeval('pcall(require, "ffi")')`.**

No `.res` is produced before this setup-time blocker.

## Concerns

- `luaeval()` is required before oldtest can enter `test_functions.vim`; it is outside this task's three assigned defects.
- The writable table mirrors upstream binding mutability. Side effects associated with variables such as `v:searchforward`/`v:hlsearch` are not yet modeled beyond storing their values.
