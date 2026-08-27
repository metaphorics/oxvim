# Task 65: `type()` constants and the `v:t_*` variables

Scope: concern (2) of `.outline/sdd/reports/task-63.md`. Owned crate: `crates/ox-eval/`.
`crates/ox-editor/` and `crates/ox-text/` were untouched (Task64UndoBlocks owns them); no
coordination was needed, for the reason in §3.

Oracle for every number below: `.references/neovim/build/bin/nvim`, v0.13.0-dev-1390, API level 15.
Upstream reading: `src/nvim/eval/typval_defs.h:123-133` (the `VAR_TYPE_*` enum),
`src/nvim/eval/funcs.c:7570-7597` (`f_type`), `src/nvim/eval/vars.c:324-331` (the `v:t_*` table).

## 1. Commits

| sha | subject |
| --- | --- |
| `1f781ad` | `fix(ox-eval): return upstream's type() constants and define v:t_*` |

## 2. The constants

`type()` was returning `Typval::vartype()`, which is `ox_types`' internal `VAR_*` discriminant
(`VAR_UNKNOWN=0, VAR_NUMBER=1, …`). Upstream keeps two separate numberings: that internal enum, and
the public `VAR_TYPE_*` values `f_type` translates it into. They disagree for every type but Blob,
which is why the defect read as a plausible off-by-one rather than a category error — and why
`Blob` was the one value that happened to be right.

| type | upstream (oracle) | oxvim before | oxvim after |
| --- | --- | --- | --- |
| Number | 0 | 1 | **0** |
| String | 1 | 2 | **1** |
| Funcref | 2 | 3 | **2** |
| Partial / lambda | 2 | 9 | **2** |
| List | 3 | 4 | **3** |
| Dict | 4 | 5 | **4** |
| Float | 5 | 6 | **5** |
| Bool | 6 | 7 | **6** |
| Special (`v:null`) | 7 | 8 | **7** |
| Blob | 10 | 10 | **10** |

Two entries were not in the task-63 measurement and are worth naming. **Partial** was `9`
(`VAR_PARTIAL`) where upstream's `case VAR_PARTIAL:` falls through to `case VAR_FUNC:` and answers
`2` — so `type({-> 1})` was off by seven, not one, and a lambda did not even share a type with a
Funcref. **Channel and Job** are Numbers upstream and answer `VAR_TYPE_NUMBER`; `vartype()` already
folded them into `VAR_NUMBER`, and `type_constant()` keeps that.

The `v:t_*` variables were all undefined (`E121`). After: each equals its constant, and
`exists('v:t_number')` is 1 where it was 0.

`VAR_TYPE_SPECIAL = 7` has **no `v:t_` name upstream** — `exists('v:t_special')` is 0 on the oracle —
so none was invented. `v:t_job` and `v:t_channel` exist in Vim and not in Neovim; the oracle does not
define them, so neither does this port. Every type the port supports has a constant, and no constant
exists for a type it does not.

## 3. Where the code lives, and why not in the `v:` table

`VAR_TYPE_*`, the `v:t_*` name table, `type_constant()` and `vim_type_var()` are together in
`crates/ox-eval/src/builtins.rs`, so the two numberings cannot drift and there is one reader of each.

The `v:t_*` variables resolve in `eval_variable` (`eval.rs:417-430`), beside `v:true`, `v:false` and
`v:null`, rather than being seeded into `Scope::vim`. That is not a shortcut, it is the only place
that works: `crates/ox-editor/src/excmd_exec.rs:5264` does
`scope.vim = dict_to_scope(editor.vvars())` before every execution and writes it back after, so
`Editor::vvars` is the single source of truth for that map and anything seeded in `Scope::new()`
would be discarded on the first Ex command. Resolving ahead of the table keeps one owner in the crate
that owns `type()` and needs no edit to `ox-editor`.

`let v:t_number = 5` correctly gives `E46`: `vim_variable_is_writable` is a whitelist and `t_number`
is not on it.

## 4. Tests corrected

Every one of these pinned the internal discriminant that `type()` was leaking, so each asserted the
defect. `case!` compares against the oracle's documented value, and the old values were never that.

| test | old expected | new expected | why the old value was wrong |
| --- | --- | --- | --- |
| `type_number` | 1 | **0** | `VAR_NUMBER`, the internal tag, not `VAR_TYPE_NUMBER` |
| `type_string` | 2 | **1** | `VAR_STRING`, not `VAR_TYPE_STRING` |
| `type_list` | 4 | **3** | `VAR_LIST`, not `VAR_TYPE_LIST` |
| `type_dict` | 5 | **4** | `VAR_DICT`, not `VAR_TYPE_DICT` |
| `type_float` | 6 | **5** | `VAR_FLOAT`, not `VAR_TYPE_FLOAT` |
| `type_bool` | 7 | **6** | `VAR_BOOL`, not `VAR_TYPE_BOOL` |
| `type_null` | 8 | **7** | `VAR_SPECIAL`, not `VAR_TYPE_SPECIAL` |

Seven tests corrected. Two cases were **added** rather than corrected, because the numbering left
them uncovered: `type_func` (2) and `type_blob` (10). `type_blob` is the one value that was already
right, and it had no test — which is the shape of the whole defect: the numbering was never compared
to the oracle anywhere.

`crates/ox-types/src/typval.rs` `vartype_and_truthiness_follow_upstream` was **not** changed and
still passes. It pins the internal `VAR_*` tags, which are correct and are not what `type()` returns.
Nothing outside `crates/ox-eval` pinned a `type()` value: `type(` and `v:t_` appear nowhere in
`ox-editor`, `oxvim` or `tests/` as a Vimscript expectation.

## 5. The round-trip test

`type_matches_its_vim_type_variable_for_every_supported_type` asserts, for each of the nine supported
values, all four of: `type(x) == v:t_<name>` evaluates to 1, `type(x)` is the oracle's number,
`v:t_<name>` is the same number, and `exists('v:t_<name>')` is 1. It closes with
`type(v:null) == 7` and `exists('v:t_special') == 0`, so the missing name is pinned as missing rather
than left unstated. The Funcref case arrives as a seeded scope variable because `function()` is a
host-layer builtin that `Builtins::without_regex()` answers with `E117`.

### Mutation checks

| mutation | caught by |
| --- | --- |
| `VAR_TYPE_LIST` 3 → 4 | `type_list` **and** the round-trip test (2 failures) |
| `v:t_blob` → `v:t_bloc` in the table | the round-trip test |
| `Typval::Partial` mapped to `VAR_TYPE_SPECIAL` instead of `VAR_TYPE_FUNC` | the round-trip test |

The third is the one that matters: it proves the lambda arm is load-bearing and not covered by a
Funcref-only assertion. The file was copied to `/tmp`, mutated, restored from the copy and `touch`ed
between runs, so no run served a stale binary; the restored tree is green at 465.

## 6. Suite counts

`cargo test -p ox-eval -- --test-threads=1`:

| | count |
| --- | --- |
| task-63 baseline | 462 |
| after | **465**, 0 failures |

`+3`: `type_func`, `type_blob`, and the round-trip test. No test was deleted or disabled.

Clippy on `ox-eval --all-targets` reports 348 `unwrap_used` errors, all in `builtins_tests.rs`; 345
predate this task (296 `unwrap()` calls at `HEAD~1`), and the 3 added follow the file's existing
convention. `cargo fmt -p ox-eval --check` drift exists throughout the crate and touches none of the
edited regions; no formatter was run.

## 7. Oldtest: `test_method.vim`

Chosen by grepping the corpus for `type(` and `v:t_`. `test_vimscript.vim` has the most references
(30, including the canonical `Test_type()`) but aborts while sourcing at line 270 on `E492`, so it
executes nothing in either binary and cannot measure anything. Of the files that do run,
`test_method.vim` is the one whose `v:t_` assertions are reached.

Both columns come from the same harness and the same `runtest.vim` invocation, differing only in the
binary. Peers landed three `ox-editor`/`ox-text` commits during this task, so neither binary was
taken from the working tree: both were built in detached worktrees pinned to commits — `f2da5c3`
(this commit's parent) and `1f781ad` — with their own `CARGO_TARGET_DIR`, so the only difference
between them is this task's three files. Each run got a freshly created throwaway `HOME` with
isolated `XDG_*`/`TMPDIR` and its own copy of `testdir` under `/tmp/ox-t65-*`; nothing ran inside
`.references`. The counts are identical to an earlier unpinned pair, so the confound was absent
either way.

| | before | after |
| --- | --- | --- |
| tests executed | 10 | **10** |
| tests failed | 8 | **7** |

One function moved, and its before-state names the defect exactly:

| function | before | after |
| --- | --- | --- |
| `Test_list_method` | `Caught exception … E121: Undefined variable: v:t_list` | **passing** |

`Test_dict_method` still fails, on unrelated error-code mismatches (`Expected E897: but got E714`,
`Expected E731: but got E1294`), not on its `v:t_dict` line.

Two neighbouring files were measured the same way and are byte-identical before and after:
`test_expr.vim` 33 executed / 26 failed, `test_getvar.vim` 6 executed / 3 failed. `test_expr.vim` is
the brief's first candidate and it does not move: its two `v:t_string` references sit inside
`Test_funcref`, which fails earlier. It is reported here so the single-function move above is not
mistaken for a broad one.

## 8. Concerns

- **`E46` omits the `v:` prefix.** `let v:t_number = 5` reports
  `E46: Cannot change read-only variable "t_number"` where the oracle reports `"v:t_number"`.
  `scope.rs:388-392` formats the bare name. This affects **every** `v:` and `a:` read-only error, not
  just the new constants, and the oldtest corpus matches on the full text
  (`assert_fails('let v:count = 1', 'E46: … "v:count"')`). One line in `ox-eval`, deliberately left
  alone as outside this task's scope, and the next thing to fix in this file.
- **The `v:` scope dict does not list the constants.** `v:t_number` resolves, but the bare `v:`
  dictionary is built from the host's `vvars` and so contains neither `v:t_*` nor `v:true`/`v:null`.
  Upstream's `v:` dict contains all of them, so `has_key(v:, 't_number')` and iterating `v:` still
  diverge. This predates the task and applies equally to the three constants that were already
  special-cased; fixing it means giving `ox-editor`'s `Editor::vvars` the full startup table, which
  is a cross-crate change.
- **`test_vimscript.vim` executes nothing.** It aborts while sourcing at line 270 with
  `E492: Not an editor command`, which hides `Test_type()` — the file that would have proven all ten
  constants at once against the corpus rather than against my own test. That single sourcing failure
  gates 30 `type(`/`v:t_` references, the largest concentration in the corpus, and is worth a task.
- **The oldtest harness needs `VIMRUNTIME` set explicitly.** Running `runtest.vim` directly, rather
  than through `make`, fails at `setup.vim:121` `colorscheme vim` unless `VIMRUNTIME` points at
  `.references/neovim/runtime`; `runnvim.sh:33` exports it and a direct invocation does not. Worth
  recording because the `make` path itself reports only `Nvim exited with non-zero code`, with no
  hint at the cause.
- **The two numberings will invite this bug again.** `Typval::vartype()` and `type_constant()` now
  both exist and return different numbers for the same value. `vartype()` is correct and is the
  internal tag; anything that reaches for it to answer a user-visible question will be wrong in
  exactly the way `type()` was. It has two callers, both in `ox-types`' own tests.
