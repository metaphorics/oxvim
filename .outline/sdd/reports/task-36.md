# Task 36: Lua colorscheme sourcing + oldtest unblock past E185 'vim'

## Status

Complete. The oldtest blocker **E185: Cannot find color scheme 'vim'** (setup.vim:121) is gone: the harness now sources setup.vim cleanly — `:colorscheme vim` resolves `colors/vim.lua` and executes it through the Lua host seam — and advances to a new named blocker, **E46: Cannot change read-only variable "testing"** at runtest.vim:171 (`let v:testing = 1`).

The colorscheme contract items were verification, not implementation: `runtime/colors/vim.lua` was already committed verbatim (fe17664, byte-identical to `.references/neovim/runtime/colors/vim.lua`), and `:colorscheme`'s `.vim`-then-`.lua` resolution through `LuaExec` landed in 8db3a20 (task 33) — the order matches upstream `runtime.c:370` `source_callback_vim_lua` (".vim files being sourced first"). The real E185 cause was upstream of the colorscheme command: setup.vim:85 rewrites `'runtimepath'` as `$VIM/vimfiles,$VIMRUNTIME,$VIM/vimfiles/after` before `colorscheme vim`, and three gaps made that rewrite destroy the search roots: `$VIM`/`$VIMRUNTIME` were never exported, `:set` did not expand environment variables in values (option.c `P_EXPAND`), and `:set` writes were not mirrored into the eval scope so same-script `&runtimepath` reads observed a stale snapshot.

## Commits

- `29cc0ec fix(editor): :set writes reach the eval scope and expand env vars` — `OptionMetadata::expand` codegen from the options.lua `expand` key (378 options; runtimepath, packpath, path, tags, directory family, …); `set_one` mirrors every write into the eval scope like `:let &opt` (`set_and_mirror`) so `&opt` reads in the same command batch see the new value; expand-flag string values expand `$NAME`/`${NAME}` through the process environment and a leading `~` through `$HOME` before list operators apply (`stropt_expand_envvar` → `expand_env_esc`); unset variables stay literal like upstream `vim_getenv` returning NULL.
- `ccc33ea feat(oxvim): derive and export $VIM/$VIMRUNTIME at startup` — `export_vim_environment()` (env.c `vim_getenv` derivation + `os_setenv` caching) seeds both variables from `runtime_root()` before any mode dispatch, honoring explicit exports; `$VIM` strips a trailing `runtime` component (`remove_tail RUNTIME_DIRNAME`). Wired in `main.rs` `run()` so every process mode (server, batch, `-l`, interactive child) sees them before any executor snapshots the environment.

## Test summary

- New: `set_write_is_scope_visible_and_expands_env_vars` — same-batch `&runtimepath` visibility after `:set`, `$VAR`/`${VAR}` expansion for the expanded value, unset-variable literal passthrough.
- Required gate: `cargo nextest run -p ox-editor -p oxvim` — **537 passed, 0 skipped** (ox-editor alone: 503).
- `cargo build -p ox-editor -p oxvim` warning-free.

## Oldtest end state

Invocation (direct, from `.references/neovim/test/old/testdir`):

`/home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim`

setup.vim completes end to end (rtp rewrite, `$HOME`→XfakeHOME, `colorscheme vim` executing `runtime/colors/vim.lua` — `vim.cmd.highlight('clear')`, `vim.g.colors_name`, and `vim.api.nvim_set_hl` all work through the server Lua seam). Exit is at the next named blocker:

**`E46: Cannot change read-only variable "testing"` — runtest.vim:171, `let v:testing = 1`.** Upstream `v:testing` is a writable internal variable (set by the test harness to enable `test_garbagecollect_now` and shorten completion delays); our `v:` model rejects writes. Next task: allow `v:testing` (and audit which other `v:` vars upstream marks settable — `vimvars` table in eval/vars.c).

## Concerns

- **Lua-boundary global corruption (reproducible, not yet blocking the harness):** after `let g:probe = 'before'` then `:lua vim.g.foo = 1`, reading `g:probe` yields a Dictionary (E715 on string concat). Bisected to any `:lua` execution (`vim.cmd`, `vim.g`, `nvim_set_hl` variants all trigger it); the `:lua`/`colorscheme` sync pair around `ServerLuaExec` is the suspect seam. `colorscheme vim` itself survived it in the harness, but test bodies that set globals before touching Lua will hit it.
- `vim.o.background = x` (write path) fails: the server's `with_scoped_editor_api` rebind of `nvim_set_option_value` lacks the 2-arg→3-arg opts default that `ox-lua`'s `bind_api` has (server.rs:1546 special-cases only `nvim_get_option_value`; vim.rs:370-374 handles both) — "Wrong number of arguments: expecting 3 but got 2". vim.lua only reads `vim.o.background`, so it did not block.
- `nvim_set_hl` silently ignores `link`, `ctermfg`, `ctermbg`, `force`, `default` keys (ox-api ui.rs `attrs`); vim.lua's link groups no-op. Harmless for the harness; semantic gap for real colorschemes.
- `:colorscheme` without an argument errors E471 where upstream prints `g:colors_name` or `default` (ex_docmd.c `ex_colorscheme`); `ColorSchemePre` is modeled (`Event::ColorSchemePre`) but never fired by `command_colorscheme`. Neither is harness-reachable yet; both belong with the next colorscheme parity pass.
- The workspace builds under `CARGO_BUILD_BUILD_DIR=/tmp/cargo_cache/...`; the leftover `target/debug/build/ox-editor-*` out dirs are stale decoys that never reflect codegen changes.
