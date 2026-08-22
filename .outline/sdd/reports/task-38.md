# Task 38: `luaeval()` and oldtest iteration

## Status

Complete. `luaeval(expr[, arg])` is implemented end to end with upstream `_A` binding, conversion, and error semantics. The oldtest harness passes `runtest.vim:173` and stops at the next named blocker: missing default arguments in Vimscript function definitions (`E125` at `runtest.vim:220`).

## Change

### ox-editor — `luaeval` builtin + Lua seam threading

- `LuaExec` gained `eval_expression(editor, expression, arg: Option<&Typval>) -> Result<Typval, LuaExecError>`; the default reports a missing host so existing fakes compile unchanged.
- `EvalHost::call` dispatches `luaeval` before the filesystem/buffer/user-function fallbacks:
  - Arity from the generated spec (1..2): E119/E118.
  - First argument coerced `tv_get_string_chk`-style (String/Number/Bool/v:null accepted; Float→E806, List→E730, Dict→E731, else E729).
  - `sync_scope_into_editor` before and `sync_editor_into_scope` after, matching the Task 37 `:lua` discipline so `vim.g` inside the expression sees live Ex variables and mutations survive.
  - `LuaExecError::Load` → E5107, Runtime/Conversion → E5108, both with the upstream `Lua:` message prefix (`nlua_error` renders `E5107: Lua: <chunk error>`); chunk is named `luaeval()`.
  - No Lua host installed → `EvalError::not_implemented("luaeval")`, preserving the old flow.
- To make the builtin reachable mid-expression, the Lua seam is now threaded into the previously `None` call sites: `command_let` (`:let`/`:const`), `command_echo` (echo/echomsg/echon/echoerr), and `eval_condition` (`:if`/`elseif`/`:while`). `:throw`, `:return`, `:for`, `:call`, `:put =`, and `:echo`-inside-cmdline already passed it.

### oxvim — real host implementation

`ServerLuaExec::eval_expression` mirrors upstream `nlua_exec_typval_fmt("local _A=select(1,...) return (%.*s)", …, "luaeval()", arg, 1, special=true, ret_tv)`:

- Loads `local _A=select(1,...) return (<expr>)` named `luaeval()` inside `with_scoped_editor_api`.
- `_A` pushed via `typval_to_lua` (the `nlua_call`/`nlua_push_typval` convention); an omitted second argument is pushed as Lua nil (upstream lowers `VAR_UNKNOWN` with `lua_pushnil`).
- Called through `call_with_traceback`; the single result converts via `lua_to_typval` (`nlua_pop_typval` rules): booleans stay Bool, nil → v:null, integer-valued Lua numbers → Number, others Float, array-part tables → List, string-keyed tables → Dict, empty table with `vim.empty_dict()` metatable → Dict.

## Commits

- `55335e3 feat(editor): implement luaeval through the LuaExec seam`
- `715c657 feat(oxvim): evaluate luaeval expressions with typval conversion`

## Test summary

- New ox-editor unit tests (5): host expression/argument passing incl. `_A` list shape, no-argument → None, missing host → `not implemented: luaeval`, arity E119/E118, E5107/E5108 message mapping — all pass.
- Gate: `cargo nextest run -p ox-editor` — 510 passed, 0 skipped.
- New oxvim smoke test `luaeval_evaluates_expressions_with_upstream_conversion` (real LuaJIT host): runtest.vim:173 pcall boolean, `_A[1] + _A[2]` = 42, `string.match(_A, "[a-z]+")` = "foo", `3.0`→3, `math.pi`→float, `nil`→v:null, `{1,2,3}`→list, `{x=40,y=2}`→dict, E5107/E5108 with `Lua:` prefix — passes.
- Full `cargo nextest run -p oxvim --test smoke` — 19 passed, 0 skipped.
- Binary: `cargo build -p oxvim` — succeeded.

## Oldtest end state

Invocation from `.references/neovim/test/old/testdir` (unchanged from Task 37):

`/home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim`

`let s:has_ffi = luaeval('pcall(require, "ffi")')` (runtest.vim:173) now succeeds (returns v:true with the vendored LuaJIT host). The harness exits at the next deterministic named blocker:

**`E125: Illegal argument: time = 10` — `runtest.vim:220`, `func Nterm_wait(buf, time = 10)` — default arguments in Vimscript function definitions are not parsed.**

No `.res` is produced before this setup-time blocker. Manual verification beyond the harness: a headless `-S` script confirmed `has_ffi=v:true`, `sum=42`, `pi=3.141592653589793`, `nothing=v:null`, `row=[1, 2, 3]`, `whole=3`, and correct E5107 surfacing for an invalid expression.

## Concerns

- The `_A` push uses the codebase's `typval_to_lua` (the `nlua_call` non-special convention). Upstream `luaeval` pushes with `kNluaPushSpecial`, whose only observable differences are float args arriving as `vim.types.float` special tables and `v:null` arriving as Lua nil rather than `vim.NIL`; no oldtest file in the current iteration exercises that distinction.
- A luaeval result containing Lua functions keeps its registry refs alive for the typval's lifetime; with no typval destructor the slots pin (same accepted trade as other funcref-returning paths).
- `nvim_eval` (ox-api) evaluates against a fresh empty `Scope` and cannot read `g:` variables set by the executor — pre-existing, worked around in the smoke test via `vim.g` readback.
- The next blocker (default function arguments, E125 at runtest.vim:220) blocks setup before `test_functions.vim` tests run; `Nterm_wait` is only the first of several helpers with default args.
