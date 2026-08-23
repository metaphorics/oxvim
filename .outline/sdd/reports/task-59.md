# Task 59: fold observability report

## Method

The spec is read-only upstream source: `f_foldclosed`, `f_foldclosedend`,
`f_foldlevel`, `f_foldtext` and `f_foldtextresult` in
`.references/neovim/src/nvim/fold.c:3178-3301`, the searches they call
(`hasFoldingWin` at `fold.c:173-263`, `foldLevel`/`foldLevelWin` at
`fold.c:265-285` and `fold.c:1088-1107`, `checkupdate` at `fold.c:1113-1122`,
`hasAnyFolding` at `fold.h:23-25`), `tv_get_lnum` at
`eval/typval.c:4348-4369`, and the arity in `eval.lua:3043-3136`
(`args = 1, base = 1` for the three `{lnum}` functions, no `args` for
`foldtext()`).

Every expectation in the tests, and every value in the comparison below, was
first measured on the oracle binary `.references/neovim/build/bin/nvim`
(NVIM v0.13.0-dev-1390, API level 15) run as
`nvim --clean --headless -u NONE -i NONE -S <script>` with the results written
through `writefile()`. Nothing was inferred from documentation.

## Landed

| Builtin | Semantics served |
|---|---|
| `foldclosed({lnum})` | first line of the closed fold containing `{lnum}`, `-1` otherwise |
| `foldclosedend({lnum})` | last line of that fold, capped at the last buffer line, `-1` otherwise |
| `foldlevel({lnum})` | nesting level at `{lnum}` whatever the fold's state, `0` when none |

Behaviour ported deliberately, each one measured on the oracle:

* `tv_get_lnum` runs **before** the range check, so a `List` raises `E745` and a
  `Dictionary` raises `E728` whatever the folds hold; the address forms `.`,
  `$`, `'m` and a numeric string all resolve.
* A line outside `[1, ml_line_count]` answers `-1`/`0` without consulting folds
  (`fold.c:3182`, `fold.c:3209`).
* `hasFoldingWin` descends from the outermost fold and stops at the first closed
  one, so a closed fold nested inside a closed one is invisible until the outer
  one opens, while `foldlevel` counts both either way.
* `foldclosedend` caps at `ml_line_count` (`fold.c:250`).
* `'foldenable'` off makes `hasAnyFolding` false, so both queries answer as
  though the buffer had no folds at all — and the folds are still there when
  `'fen'` comes back.
* `checkupdate` runs before the query, so an `'foldmethod'` the user only set
  (never `:fold`ed) is computed on demand: an indent fold is observable with no
  Ex fold command at all. `'shiftwidth'`, falling back to `'tabstop'` when zero,
  is read the way `get_sw_value_col` (`indent.c:362-366`) reads it.

Model support added to `fold.rs`: `FoldRange::covers_row`,
`FoldRange::last_row`, `Folds::closed_rows_at`, `Folds::level_at_row`, and
`FoldMethod::from_option_value`. The last one replaces the `'foldmethod'`
name-to-method `match` that was written out inside the Ex host, so the option is
mapped in exactly one place.

## Declined, with the missing piece named

* **`foldtext()`** — renders the fold from `v:foldstart`, `v:foldend` and
  `v:folddashes` (`fold.c:3215-3258`). This port has none of those three `v:`
  variables, and there is no fold-display path that would set them. Missing
  subsystem: the `v:fold*` context Neovim installs around a `'foldtext'`
  evaluation (`fold.c:1681-1724`).
* **`foldtextresult({lnum})`** — calls `get_foldtext`, which evaluates the
  `'foldtext'` option in the fold's context and then strips `'foldmarker'` and
  `'commentstring'` in `foldtext_cleanup` (`fold.c:3261-3301`,
  `fold.c:1726-1750`). This port has no `'foldtext'` option and no evaluation
  path for it. Oracle answer for the nested fold used below is
  `+---  2 lines: c`; producing that string from a hand-written formatter
  instead of a `'foldtext'` evaluation would be a plausible-looking fake, so
  both builtins stay unimplemented and unrouted. `exists('*foldtextresult')`
  still answers 1, because the generated `eval.lua` table lists the name.

`Folds::fold_text_request` and `FoldText*` in `fold.rs` remain the typed host
seam these two would use once a `'foldtext'` evaluator exists.

## Remaining gaps in the query path (named, not worked around)

* `'foldmethod'` of `expr`, `syntax` or `diff` needs a host computation
  `Folds::refresh` can only request, so those methods answer from whatever a
  host last applied — nothing, by default.
* `'foldnestmax'` does not cap computed levels and `'foldignore'` does not
  exclude lines, so an indent fold deeper than `'foldnestmax'`, or one starting
  at an ignored line, is reported where upstream would not report it. Both
  default to values that make no observable difference (`fdn=20`, and `fdi=#`
  only matters for `#`-led lines).
* `'foldlevel'` does not close computed folds by level; a computed fold starts
  closed, which is what `'foldlevel'` at its default of zero produces.
* Ex ranges are not expanded to whole closed folds
  (`ex_docmd.c:2225-2226` runs `hasFolding` on `line1`/`line2` for every
  command). Measured consequence: on the oracle, `:2,5fold` followed by
  `:3,4fold` while 2-5 is closed creates a *second* 2-5 fold, so `foldlevel(2)`
  answers 2; in oxvim the inner range is taken literally. This is an Ex-range
  behaviour in `excmd_exec`, not a fold-query one, and it is why the comparison
  script below builds the inner fold first. Out of scope for this task.
* One `Folds` per buffer, not per window, so two windows on one buffer cannot
  hold different fold state — the pre-existing gap task 56 named.

## Oracle comparison through the binary

Same script, same flags, both binaries. `:fold`, `:foldopen` and `:foldclose`
from task 56 are now readable back through the query builtins, which is the
point of the task.

```vim
let out = []
call setline(1, ['a','b','c','d','e','f'])
2,4fold
call add(out, '1 fc1=' .. foldclosed(1) .. ' fc2=' .. foldclosed(2) .. ' fc4=' .. foldclosed(4) .. ' fc5=' .. foldclosed(5))
call add(out, '2 fce2=' .. foldclosedend(2) .. ' fce4=' .. foldclosedend(4) .. ' fce1=' .. foldclosedend(1))
call add(out, '3 fl1=' .. foldlevel(1) .. ' fl2=' .. foldlevel(2) .. ' fl4=' .. foldlevel(4) .. ' fl5=' .. foldlevel(5))
call add(out, '4 fc0=' .. foldclosed(0) .. ' fc99=' .. foldclosed(99) .. ' fl0=' .. foldlevel(0) .. ' fl99=' .. foldlevel(99))
call cursor(3, 1)
call add(out, '5 dot=' .. foldclosed('.') .. ' dollar=' .. foldclosed('$') .. ' str=' .. foldclosed('4') .. ' fldot=' .. foldlevel('.'))
set nofoldenable
call add(out, '6 fc2=' .. foldclosed(2) .. ' fce2=' .. foldclosedend(2) .. ' fl2=' .. foldlevel(2))
set foldenable
2foldopen
call add(out, '7 open fc2=' .. foldclosed(2) .. ' fce2=' .. foldclosedend(2) .. ' fl2=' .. foldlevel(2))
2foldclose
call add(out, '8 close fc2=' .. foldclosed(2) .. ' fce2=' .. foldclosedend(2))
call writefile(out, OUTFILE)
qall!
```

`.references/neovim/build/bin/nvim --clean --headless -u NONE -i NONE -S ...`:

```
1 fc1=-1 fc2=2 fc4=2 fc5=-1
2 fce2=4 fce4=4 fce1=-1
3 fl1=0 fl2=1 fl4=1 fl5=0
4 fc0=-1 fc99=-1 fl0=0 fl99=0
5 dot=2 dollar=-1 str=2 fldot=1
6 fc2=-1 fce2=-1 fl2=0
7 open fc2=-1 fce2=-1 fl2=1
8 close fc2=2 fce2=4
```

`target/debug/oxvim --clean --headless -u NONE -i NONE -S ...`:

```
1 fc1=-1 fc2=2 fc4=2 fc5=-1
2 fce2=4 fce4=4 fce1=-1
3 fl1=0 fl2=1 fl4=1 fl5=0
4 fc0=-1 fc99=-1 fl0=0 fl99=0
5 dot=2 dollar=-1 str=2 fldot=1
6 fc2=-1 fce2=-1 fl2=0
7 open fc2=-1 fce2=-1 fl2=1
8 close fc2=2 fce2=4
```

`cmp` reports the two output files identical.

A second oracle run pinned the nested and error cases the unit tests assert:
`3,4fold` then `2,5fold` gives `fc3=2 fce3=5 fl3=2 fl2=1`, and after
`:2foldopen` `fc2=-1 fc3=3 fce3=4 fl3=2`; `foldclosed([])` raises
`Vim(call):E745: Using a List as a Number`, `foldlevel({})` raises
`E728: Using a Dictionary as a Number`, `foldclosed()` raises
`E119: Not enough arguments for function: foldclosed`, and `foldlevel(1, 2)`
raises `E118: Too many arguments for function: foldlevel`.

## Tests

`PATH="$HOME/.cargo/bin:$PATH" RUSTC_WRAPPER="" cargo test -p ox-editor -- --test-threads=1`

* before: 742 total is the new count; the baseline this task started from was
  **730 passed, 0 failed**.
* after: **742 passed, 0 failed, 0 ignored**.

Twelve tests in `crates/ox-editor/src/builtins/fold.rs`, each carrying the
oracle value it asserts: normal case, first-line and last-line boundaries, a
line outside every fold, a line outside the buffer, the address forms, nested
folds before and after `:foldopen`, `'nofoldenable'`, computed indent folds, and
the documented errors (`E745`, `E728`, `E119`, `E118`).

## Commits

* `e3abe00 feat(ox-editor): answer fold queries by row in the fold model`
* `49f46a1 feat(ox-editor): serve foldclosed, foldclosedend and foldlevel`

Not pushed: the push token is invalid, as the brief states.
