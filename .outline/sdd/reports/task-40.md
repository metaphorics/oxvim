# Task 40: swapfilelist() builtin; oldtest unblock iteration

## Status

Complete for this iteration. `swapfilelist()` is implemented per upstream
naming rules, and the iteration bundled one more harness-level unblock
(`indexof()`). The oldtest harness passes both Task 39 blockers
(`swapfilelist` at runtest.vim:243, `indexof` at runtest.vim:247) and stops
at the next named blocker: the missing `:redir` Ex command at
runtest.vim:604.

## Change

### Commit `ed3a1a9` — `swapfilelist()` (ox-editor)

- `fs_builtins.rs`: `swapfilelist(io, arg_count, directory)` following
  upstream `f_swapfilelist` (eval/funcs.c 7200) → `recover_names(NULL,
  false, list)` (memline.c 1303-1429, `fname == NULL` path):
  - Every entry of the `'directory'` option (comma-split, backslash
    escapes, empty parts skipped) is scanned.
  - `.` entry expands the bare relative patterns `*.sw?`, `.*.sw?`,
    `.sw?`; other entries expand `dir/*.sw?` etc. with one separator
    (`concat_fnames` parity, memline.c 1350-1354).
  - Matches append per pattern in pattern order, duplicates kept —
    `EW_KEEPALL` only skips 'wildignore'/'suffixes' filtering (path.c
    2129-2141); there is no cross-pattern dedup.
  - Relative results carry no `./` prefix, matching upstream's bare
    patterns for the `.` branch (our globber anchors at the cwd instead).
  - Hidden files only match the dot-prefixed patterns, per the existing
    glob hider rule (Unix dot-file convention upstream relies on).
- `excmd_exec.rs` `EvalHost::call`: `swapfilelist` intercept reads the
  `'directory'` option from the editor option store; unset reads as `.`
  (the option has no static default upstream — it is computed at
  startup). Arity via the generated spec (E118/E119).

### Commit `a4a3104` — `indexof()` (ox-eval)

- `builtins.rs`: `Builtins::indexof` following upstream `f_indexof`
  (eval/funcs.c 2961-3002) over `indexof_list`/`indexof_blob`:
  - Callback is a `v:`-expression string, funcref, or partial; empty or
    null-string callback returns -1 without iterating (funcs.c 2972-2975).
  - `opts.startidx` (dict arg, `v:_null_dict` allowed): negative counts
    from the end (`tv_list_uidx` parity via the existing
    `normalize_index`), out-of-range finds nothing.
  - A match is the first callback result converting to a nonzero number
    (`tv_get_bool_chk` = `tv_get_number_chk`, funcs.c 2872): string
    results parse as numbers, so `{_, v -> "v == 2"}` never truth-matches
    (test_listdict.vim asserts exactly that).
  - Callback errors abort the search and propagate (upstream `did_emsg`
    check stops iteration and surfaces the error).
  - Type errors keep upstream codes: E1226 (container), E1256
    (callback), E1206 (opts).

## Test summary

- New ox-editor tests (3): swap file collection across multiple
  `'directory'` entries with per-pattern sort order; `.` entry yielding
  relative (`./`-free) names under a cwd-guarded temp root; E118 arity
  plus end-to-end `let &directory = …` / `swapfilelist()` option wiring
  through the Ex executor.
- New ox-eval test (1): `indexof` string-callback matches, `startidx`
  0/-2/4/-4 and `{}`-default/`v:_null_dict`, empty and null-string
  callbacks, blob scanning with negative start, E1226/E1256/E1206.
- Gates: `cargo nextest run -p ox-editor` — 521 passed, 0 skipped;
  `cargo nextest run -p ox-eval` — 360 passed, 0 skipped.
- `cargo build -p oxvim` succeeded.

## Oldtest end state

Invocation unchanged (from `.references/neovim/test/old/testdir`):

`/home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim`

- runtest.vim:243 `swapfilelist()` inside `s:GetSwapFileList()` now
  returns the swap-file list (cleanup loop at :257 proceeds).
- runtest.vim:247 `indexof(files, 'v:val =~ "runtest.vim."')` now
  evaluates.
- New deterministic named blocker:

**`Ex command failed: not implemented: redir` — runtest.vim:604,
`redir @q` wrapping `silent function /^Test_` (runtest.vim:605): the
`:redir` Ex command is not implemented, and the wrapped
`:function /pattern` listing output (task-39 concern:
`list_func_head`/function-name listing) is its immediate consumer.**

No `.res` is produced before this setup-time blocker. `:redir` is an Ex
command with output-capture plumbing (variable/register/file targets,
`redir END`), a larger unit than a small builtin, so it was not bundled
into this iteration's ~3-small-unblocks budget.

## Concerns

- `:redir` will also need `silent` interaction and register capture
  (`@q`), plus the `:function /pattern` listing form, to clear the next
  blocker — the two should land together.
- `swapfilelist` skips empty `'directory'` parts (upstream's
  `skip_to_option_part` collapses them); a pathological `",,"` value
  behaves sanely instead of expanding `/*.sw?`.
- Our `glob('*…')` results generally carry a `./` prefix for relative
  patterns; `swapfilelist` strips it locally for upstream parity rather
  than changing `glob()` behavior (existing glob tests pin absolute
  paths only).
- `indexof` result coercion follows the existing `number_arg` (String →
  numeric prefix, Float → E805-style error path shared with `sort`'s
  callback); upstream emits E805 for a Float result via
  `tv_get_number_chk` — same shape.
- During development an `is_implemented` list edit briefly dropped the
  `"match"…"resolve"` names; restored and verified by the full 360-test
  ox-eval suite (all green) before commit.
