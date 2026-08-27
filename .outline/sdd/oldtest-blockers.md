# oldtest suite-wide blocker census

Source: `.outline/sdd/oldtest-census.tsv` (236 files) and the per-file logs in `.outline/sdd/census/`. Measurement only; no fixes in this leaf.

## Outcome distribution

| outcome | files |
| --- | --- |
| partial | 167 |
| setup-blocked | 60 |
| timeout | 6 |
| crash | 3 |
| **total** | **236** |

Tests actually executed across the suite: **2556**, of which **2339** reported errors and **77** were skipped by the test itself.

`full-pass` is structurally unreachable under this invocation: upstream `runtest.vim` never writes `test.res` (it is a Makefile marker), so `res_exists` is false everywhere. The nearest thing to a pass is a `partial` row with `failed = 0`, of which there are **2**.

## Blockers ranked by file count

Count = number of distinct test files in which the diagnostic appears at least once (a file is gated by every blocker it hits, so columns do not sum to 236).

| rank | blocker | files gated |
| --- | --- | --- |
| 1 | `E117` | 131 |
| 2 | `E492` | 87 |
| 3 | `E15` | 43 |
| 4 | `E605` | 41 |
| 5 | `E121` | 30 |
| 6 | `E5060` | 29 |
| 7 | `not implemented: cursor` | 25 |
| 8 | `E474` | 23 |
| 9 | `E488` | 22 |
| 10 | `not implemented: redraw` | 21 |
| 11 | `E484` | 16 |
| 12 | `E523` | 14 |
| 13 | `E475` | 12 |
| 14 | `not implemented: tabnew` | 9 |
| 15 | `not implemented: assert_beeps` | 9 |
| 16 | `not implemented: setreg` | 8 |
| 17 | `not implemented: scriptencoding` | 8 |
| 18 | `not implemented: getpos` | 8 |
| 19 | `not implemented: col` | 8 |
| 20 | `E745` | 8 |

## Top 10 expanded

### 1. `E117` (131 files)

Unimplemented builtin function (oxvim reports `E117: not implemented: <fn>`)

Upstream surface: `.references/neovim/src/nvim/eval/funcs.c` (the `f_*` implementations, e.g. `f_cursor` at :920, `f_getpos` at :2034, `f_setreg` at :6452) plus the generated dispatch table in `.references/neovim/src/nvim/eval/funcs.h`. 200 distinct function/command names are missing across the suite.

### 2. `E492` (87 files)

`E492: Not an editor command`: Ex command missing from the command table

Upstream surface: `.references/neovim/src/nvim/ex_cmds.lua` (command table) resolved by `find_ex_command()` in `.references/neovim/src/nvim/ex_docmd.c:1445`. oxvim does not name the command, so per-command attribution needs a harness change.

### 3. `E15` (43 files)

`E15: Invalid expression`: expression parser rejects valid VimL

Upstream surface: `.references/neovim/src/nvim/errors.h:38` (`e_invexpr2`), produced by the expression evaluator `.references/neovim/src/nvim/eval.c` and `.references/neovim/src/nvim/viml/parser/expressions.c`.

### 4. `E605` (41 files)

`E605: Exception not caught`: uncaught throw escaping to top level

Upstream surface: `.references/neovim/src/nvim/ex_docmd.c:917`. Almost always a *secondary* symptom: the primary failure (E117/E492) is thrown inside a test and oxvim's `:try`/`:catch` propagation surfaces it here.

### 5. `E121` (30 files)

`E121: Undefined variable`

Upstream surface: `.references/neovim/src/nvim/eval/vars.c:2386`. Covers script-local/global scope resolution (`s:`, `g:`, `v:`) that the tests rely on.

### 6. `E5060` (29 files)

`E5060: Unknown flag`

Upstream surface: `.references/neovim/src/nvim/eval/fs.c:1856` — flag parsing in the file/glob builtins.

### 7. `not implemented: cursor` (25 files)

`cursor()` builtin missing

Upstream surface: `.references/neovim/src/nvim/eval/funcs.c:920` (`f_cursor`) — needs window cursor positioning incl. the list-arg and curswant forms.

### 8. `E474` (23 files)

`E474: Invalid argument`: argument validation in builtins/commands

Upstream surface: `.references/neovim/src/nvim/errors.h:33` (`e_invarg`), used from `.references/neovim/src/nvim/match.c` and `.references/neovim/src/nvim/eval/decode.c`.

### 9. `E488` (22 files)

`E488: Trailing characters`: command-line parser stops short of the full argument

Upstream surface: `.references/neovim/src/nvim/errors.h:122-123`, raised out of `.references/neovim/src/nvim/ex_docmd.c` command parsing.

### 10. `not implemented: redraw` (21 files)

`:redraw` Ex command missing

Upstream surface: `.references/neovim/src/nvim/ex_cmds.lua:2281-2284` dispatching to `ex_redraw()` at `.references/neovim/src/nvim/ex_docmd.c:6906`.

## Missing symbols behind E117/E492 (top 25 by file count)

| symbol | files |
| --- | --- |
| `cursor` | 25 |
| `redraw` | 21 |
| `tabnew` | 9 |
| `assert_beeps` | 9 |
| `setreg` | 8 |
| `scriptencoding` | 8 |
| `getpos` | 8 |
| `col` | 8 |
| `filetype` | 7 |
| `getreg` | 6 |
| `vnew` | 5 |
| `fold` | 5 |
| `undo` | 4 |
| `tempname` | 4 |
| `taglist` | 4 |
| `sleep` | 4 |
| `search` | 4 |
| `read` | 4 |
| `help` | 4 |
| `file` | 4 |
| `defer` | 4 |
| `append` | 4 |
| `winsaveview` | 3 |
| `tag` | 3 |
| `shellescape` | 3 |

200 distinct `not implemented: <symbol>` names appear across the suite; the head of that distribution is flat, so no single builtin unlocks a large block of files on its own.

## Harness caveat

An earlier pass of this census recorded 202/236 files as `timeout`. That was an artifact of the runner, not of oxvim: with an inherited (open) stdin the headless process blocks instead of exiting at `qall!`. Re-running every file with stdin redirected to `/dev/null` reduced timeouts to 6. All 236 logs in `.outline/sdd/census/` are from the corrected pass.

## Crashes (3 files)

| file | panic site | message |
| --- | --- | --- |
| `test_assert.vim` | `crates/ox-editor/src/excmd_exec.rs:1786:31` | index out of bounds: the len is 1 but the index is 1 |
| `test_visual.vim` | `crates/ox-editor/src/mode.rs:353:150` | index out of bounds: the len is 1 but the index is 2 |
| `test_window_cmd.vim` | `crates/ox-editor/src/layout.rs:1376:31` | internal error: entered unreachable code: split path descends only through containers |

These are the only hard aborts in the suite (returncode 101); every other non-pass is a diagnostic-level gap.

## Timeouts (6 files)

Still exceeding the 120 s per-file budget after the stdin fix: `test_alot.vim`, `test_expand.vim`, `test_file_size.vim`, `test_mapping.vim`, `test_recover.vim`, `test_system.vim`.
