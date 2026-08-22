# Task 39: default function arguments + `...` refinements; oldtest iteration

## Status

Complete. Vimscript default function arguments (`func F(a, b = expr)`) are
implemented per upstream `userfunc.c`, with upstream a:-accounting and error
shapes. The oldtest harness passes the Task 38 blocker (`E125` at
`runtest.vim:220`) and stops at the next named blocker: the missing
`swapfilelist()` builtin at `runtest.vim:243`.

## Change

### ox-editor `userfunc.rs` — parse side (`get_function_args` parity)

- `FunctionSignature`/`UserFunc` gained `default_args: Vec<String>` — default
  expression source text aligned with the last `default_args.len()` entries of
  `args` (upstream `uf_def_args`).
- The argument list is now split with a depth/quote-aware scanner
  (`find_top_level`/`skip_quoted`/`split_argument_ranges`): commas inside
  strings, lists, dicts, lambdas, and nested calls no longer split arguments,
  and a `)` inside a string/nesting no longer ends the signature. Upstream
  gets this for free by walking the expression with `eval1`.
- Per-argument handling:
  - `name = expr` — name validated as identifier (`E125` for invalid names,
    now also for `firstline`/`lastline`), `E853` duplicates; the expression is
    syntax-checked at definition time with the expression parser (upstream
    `eval1` parse-without-evaluate) → `E475` on bad syntax.
  - Non-default after a default → `E989`.
  - White space before the separating comma → `E1068` (test_user_func.vim
    `fu F(a=1 ,) | endf`).
  - `...` followed by anything → `E475` (upstream `mustend`) — replaces the
    previous custom `E125 ... must be last` shape.
  - Trailing comma (`F(a,)`) tolerated, matching upstream.

### ox-editor `userfunc.rs`/`excmd_exec.rs` — call side (`call_user_func` parity)

- `begin_call` arity check is now `required = args.len() - default_args.len()`:
  `E119` below required, `E118` above total when not varargs.
- `call_user_function_with_self` fills omitted defaulted parameters by
  evaluating their expressions with `eval_text` **in the caller's scope**
  before `begin_call` swaps `l:`/`a:` — matching upstream, where defaults
  evaluate in the calling context and an error aborts the call before the
  body runs.
- a:-accounting is unchanged and now correct with defaults:
  `a:0 = max(call_args - declared_args, 0)` — defaults fill positionals and
  never count as varargs; `a:000`/`a:N` hold only call-time extras.

## Commits

- `1fddce4 feat(editor): parse and bind default function arguments`

## Test summary

- New ox-editor tests (8): defaults fill omitted positionals / supplied values
  win; `Args(mandatory, optional = v:null, ...)` a: dict shape for 1 and 3
  call args (`o` default `v:null` → supplied, `'0'` 0 → 1, `'1'` 3);
  E118 above total; E119 below required; E989; E1068; default expressions
  containing commas/strings-with-parens/nesting; default-eval error in caller
  scope caught as `E121: Undefined variable: s:undefined_variable` with the
  body never entered (upstream
  `Test_default_argument_expression_error_while_inside_of_a_try_block`).
- Gate: `cargo nextest run -p ox-editor` — 518 passed, 0 skipped (510 prior).
- `cargo build -p oxvim` succeeded; `cargo nextest run -p oxvim --test smoke`
  — 19 passed, 0 skipped.
- Binary end-to-end (`-S` script, real oxvim): `Sum(40)` → 42, `Sum(40,3)` →
  43, `Args(1)` → `{'m': 1, 'o': v:null, '0': 0, '000': []}`, `Args(1,2,3)` →
  `{'m': 1, 'o': 2, '0': 1, '000': [3], '1': 3}`, `E989`/`E118` shapes fire.

## Oldtest end state

Invocation unchanged (from `.references/neovim/test/old/testdir`):

`/home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim`

`func Nterm_wait(buf, time = 10)` at runtest.vim:220 now parses; the harness
advances past it and exits at the next deterministic named blocker:

**`Ex command failed: not implemented: swapfilelist` — runtest.vim:243,
`s:GetSwapFileList()` (top-level swap-file cleanup loop at runtest.vim:263):
the `swapfilelist()` builtin is not implemented.**

No `.res` is produced before this setup-time blocker.

## Concerns

- `:function Name` listing (`list_func_head`) is still unimplemented, so
  `execute('func Args2')` rendering (`a = 1, b = 2, c = 3`) will fail when
  test_user_func.vim eventually runs; separate blocker, not needed for setup.
- On default-evaluation error in a *non*-`abort` function, upstream still
  enters the body with the parameter unbound (producing a second `E121` for
  `a:x`); we abort the call before entering. The upstream tests only assert
  the first error and the body-not-entered behavior, both of which we match.
- Upstream `E475` messages print the full text from `(` onward; ours formats
  the argument-list text — code parity kept, message tails may differ
  slightly.
- Definition-time syntax checking uses our expression parser; an expression
  it cannot parse (but Vim can) would fail the definition. The same parser
  runs at call time, so the definition and call behavior stay consistent.
