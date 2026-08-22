# Task 42: The argument-list subsystem

## Status

Complete for this iteration. The global argument list now exists as editor state, seeded from oxvim's positional file arguments before any startup command runs. `argc()`/`argv()`/`argidx()`/`arglistid()` are implemented with upstream window-argument shapes, and `:args` (listing and redefinition), `:next`, `:previous`/`:Next`, and `:argdo` follow `arglist.c`/`ex_cmds2.c` semantics including the E163/E164/E165 error shapes.

The oldtest harness advances past the Task 41 blocker (`argc()` at `runtest.vim:610`), executes the whole setup and test-dispatch path, and stops at the next named blocker: `E121: Undefined variable: v:errors` raised in `FinishTesting()` → `AfterTheTest()` (`runtest.vim:479`).

## Commits

- `11bbed9 feat(editor): implement the argument list subsystem`

## Change

- New `crates/ox-editor/src/arglist.rs`:
  - `ArgList` state (`global_alist` names + current entry index) with `set` (AL_SET resets the index), `check_target` implementing `do_argfile`'s bounds logic (≤1 entry → E163 "There is only one file to edit", before-first → E164, beyond-last → E165), and index clamping for shrunken lists.
  - Builtins `argc`/`argv`/`argidx`/`arglistid` (`f_argc` 1201, `f_argv` 1249, `f_argidx` 1221, `f_arglistid` 1228): no-argument/-1 forms report the global list; resolvable window arguments report the shared list (id 0); unresolvable ones report -1 (or an empty list for `argv`); out-of-range `argv` indexes return empty strings. Arity comes from the generated `eval.lua` metadata.
  - `split_file_list` implementing `do_one_arg` backslash-escaped whitespace splitting.
- `Editor` carries the arglist behind `arglist()`/`arglist_mut()`; `EvalHost::call` intercepts the four builtins before the typval dispatcher, alongside `swapfilelist`.
- Ex commands in `excmd_exec.rs`:
  - `:args` — with file arguments redefines the list (wildcards expand sorted via the glob seam with EW_NOTFOUND literal fallback, E479 when nothing remains) and edits the first entry like `:next`; bare `:args` lists entries on one line with the current entry bracketed (upstream uses `list_in_columns`; single-line is the documented divergence).
  - `:next`/`:previous`/`:Next` — counts honored in both spellings (`:3next`, `:3 next`) including the leading-number-as-count conversion; the changed-buffer E37 guard runs before redefinition (ex_next order) and before each edit.
  - `do_argfile` — bounds-checked entry edit that reuses the buffer already carrying the argument's name (`alist_name`) before loading the file like `:edit` (missing files open as empty named buffers); the index advances only on success.
  - `:argdo` — range over the argument list itself (default whole list), skipping the re-edit when the entry is already displayed (`editing_arg_idx`), executing the command tail per entry, aborting on a failed switch or failing command (E471 without an argument).
- `oxvim` seeds the arglist from `cli.files` at the top of `run_startup`, before `--cmd`/`-S` scripts, matching main.c's early `global_alist` fill; `open_startup_files` buffer names already match the seeded names exactly.
- `fs_builtins::expand_glob` is now `pub(crate)` for the EW_NOTFOUND expansion.

## Test summary

- `cargo nextest run -p ox-editor arglist` — 12 passed, 526 skipped (state/bounds shapes, all four builtins including arity E118 and window -1 forms, listing with brackets, movement and E163/E164/E165, previous count overflow, argdo whole-list and `2,3argdo` range, E471 and empty-list tolerance, buffer reuse without duplication, wildcard expansion with literal fallback, splitter escapes).
- `cargo nextest run -p ox-editor -p oxvim` — 574 passed, 0 skipped, against committed `11bbed9`.
- Binary-level smoke: `-S script test_functions.vim` reports `argc()=1`, `argv(0)=test_functions.vim`, `argidx()=0`, `arglistid()=0`; `:args f1 f2` + `:next`/`:previous` move between the named buffers; `argdo call add(g:seen, 'saw ' . expand('%'))` records each entry.

## Oldtest end state

Invocation from `.references/neovim/test/old/testdir`:

`/home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim`

Result:

```
oxvim: Ex command failed: E121: Undefined variable: v:errors
function FinishTesting[1]..function AfterTheTest[1]..script .../runtest.vim[1]
```

The `argc()`/`argv(1)` filter branch at `runtest.vim:610-612` passes (argc() == 1 skips the filter). The harness proceeds through test discovery and the execution loop to `FinishTesting()` → `AfterTheTest('')` → `len(v:errors)` at `runtest.vim:479`. No `.res` is produced before this blocker.

Two named blockers for the next task, verified with isolated probes:

1. **`v:errors` is undefined** (the visible E121): the `v:` variable dictionary lacks `errors` (and possibly other standard `v:` entries), which `runtest.vim` touches first in `AfterTheTest`.
2. **`:source %` does not expand `%`** (the hidden prerequisite): `source %` at `runtest.vim:590` fails with `E484: %: No such file or directory` inside the `try`/`catch`, so the test file is never sourced, zero `Test_` functions are discovered (`silent function /^Test_` lists nothing), and the execution loop runs zero iterations. Upstream `:source` carries EX_XFILE filename expansion including `%`. Fixing only `v:errors` would produce a "NO tests executed" result rather than running tests.

Additionally noted while smoking the binary (pre-existing, outside this task's scope): `let g:x += [...]` on a list corrupts it to the number `0` instead of extending (`runtest.vim` uses `let g:out +=` patterns? — not yet on the harness path, but test bodies do).

## Concerns

- `:args` listing is single-line (`a  [b]  c`) rather than upstream's columnar `list_in_columns` layout; greppable but visually different for long lists.
- Window-local argument lists (`:arglocal`/`:argglobal`, per-window `w_arg_idx`) are not modeled — the contract allowed skipping them when not cheap; every window shares the global list, which matches the harness and default upstream startup state. `argc({win})`-style builtins resolve windows but always report the shared list.
- `argc("string")` returns E745 where upstream would number-coerce the string; untested by the harness.
- The working tree still carries the unrelated `.outline/sdd/reports/task-12b.md` modification from before this task; Task 42 did not stage or alter it.
