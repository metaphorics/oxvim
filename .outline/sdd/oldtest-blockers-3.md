# oldtest suite-wide blocker census, pass 3

Source: `.outline/sdd/oldtest-census-3.tsv` (236 files) and the per-file logs in `.outline/sdd/census-3/`
(untracked). Measurement only; no source changed in this leaf. The only non-artifact edit is two
negation lines in `.gitignore`, exactly as pass 2 did for its own summaries.

Binary measured: `target/debug/oxvim` built at git SHA `8ff70b1341533a26e7976104eb0030d7d14a2239`
(`docs(outline): report task 69 items 1-3 and the maparg shortfall`), with `crates/` verified clean at
that SHA immediately after the copy, then copied to `/tmp/oxvim-c3-bin` (157 651 976 bytes,
sha256 head `337e5ad51c67e2a5`). `runtime/` was copied to `/tmp/oxvim-c3-runtime` and exported as both
`$OXVIM_RUNTIME` and `$VIMRUNTIME`; without that, `runtime_root()`
(`crates/oxvim/src/runtime.rs:109-121`) cannot resolve a relocated binary's runtime and every file dies
at `setup.vim:121` (`colorscheme vim`). Peers were committing to `crates/` throughout the pass; the pin
excludes that by design.

Invocation, per file, each in its own throwaway copy of `testdir` under `/tmp` with a fresh `HOME` and
isolated `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` / `XDG_CACHE_HOME` / `TMPDIR`, 8-way
parallel:

```
timeout 150 /tmp/oxvim-c3-bin -u NONE -i NONE --noplugin --headless \
  --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim <FILE> < /dev/null
```

The scratch `testdir` was seeded from a pristine base copy with the committed stale `test.log`
(71 387 bytes) stripped, so no run could read a prior pass's failures. Results were read from the
per-run `messages` file, never from `test.log`. Total oxvim CPU wall time for the 236 files was 277 s,
of which 155 s is one hung file (see D3).

TSV columns (no header row, matching passes 1 and 2): `name`, `outcome`, `executed`, `failed`,
`skipped`, `first_blocker`.

## Outcome distribution

| outcome | pass 3 | pass 2 | pass 1 | Δ3-2 | Δ2-1 |
| --- | --- | --- | --- | --- | --- |
| partial | 180 | 163 | 167 | **+17** | -4 |
| setup-blocked | 54 | 70 | 60 | **-16** | +10 |
| timeout | 1 | 3 | 6 | -2 | -3 |
| crash | 1 | 0 | 3 | **+1** | -3 |
| **total** | **236** | **236** | **236** | 0 | 0 |

| totals | pass 3 | pass 2 | pass 1 | Δ3-2 | Δ2-1 |
| --- | --- | --- | --- | --- | --- |
| tests executed | 3077 | 2510 | 2556 | **+567** | -46 |
| tests with errors | 2267 | 2314 | 2339 | -47 | -25 |
| tests self-skipped | 579 | 72 | 77 | **+507** | -5 |
| `partial` files with `failed = 0` | 3 | 2 | 2 | +1 | 0 |

The three clean files are `test_options_all.vim`, `test_plugin_matchparen.vim` and `test_syn_attr.vim`.

`full-pass` remains structurally unreachable under this invocation: upstream `runtest.vim` never writes
`test.res` (it is a Makefile marker), so `res_exists` is false for every file. The census records that
rather than inventing a pass class.

### `setup-blocked` now overstates the damage

33 of the 54 `setup-blocked` files are **honest whole-file skips**: the file's own `CheckFeature` /
`CheckUnix` guard fires, the harness records `NO tests executed` plus a `SKIPPED <file>: <reason>` line,
and the run exits 0. 31 of those 33 were `setup-blocked` with first blocker `E15` in pass 2, i.e. they
died *inside the guard itself*. `check.vim`'s `CheckFeature` is

```vim
if !has(a:name)
  throw 'Skipped: ' .. a:name .. ' feature missing'
endif
```

and pass 2's expression parser could not evaluate that concatenation, so `test_arabic.vim` reported
`Caught exception: E15: expression expected @ script …/test_arabic.vim[6]` (pass 2 log, line 6 is
`CheckFeature arabic`). Pass 3 evaluates it and the file skips truthfully. Nothing was lost: those
files executed 0 tests in pass 2 as well. Only **21** files are genuinely blocked before their first
test, listed in "Files still blocked before their first test" below.

## Skip analysis

This is the headline movement of pass 3 and the one most easily misread: `skipped` went 72 → 579. Per
*test* counting is only possible on pass 3 (pass 1 and 2 recorded skip counts, not skip reasons), so the
split below is derived from per-file arithmetic against `oldtest-census-2.tsv`. A skipped test still
increments `s:done`, so a test moving failure→skip leaves `executed` unchanged, drops `failed` by one
and raises `skipped` by one.

| movement | tests | derivation |
| --- | --- | --- |
| failure → skip (progress) | **384** | files with `Δexecuted = 0`: `min(Δskipped, −Δfailed)` |
| newly executing tests | **620** | sum of positive `Δexecuted` |
| **lost executions (regression)** | **53** | sum of negative `Δexecuted`, 3 files |
| other skip growth in stable files | 4 | `Δskipped > 0`, `Δfailed = 0` (skip replaced a pass) |
| skip growth inside newly executing files | 88 | honest skips in files that could not run before |
| skip growth inside the lost-execution files | 31 | all of it `test_diffmode.vim` |

`388 + 88 + 31 = 507`, reconciling the `skipped` delta exactly.

So the failure-to-skip conversion is **384 tests**, against **53 lost executions**. Every lost
execution is accounted for by name in D1, D2 and D4 below; none of it is unexplained attrition.

### Honest skips versus hidden coverage

All 579 skip reasons, classified. `oracle-parity` means the oracle
(`.references/neovim/build/bin/nvim`, v0.13.0-dev-1390) reports the same `has()` value or takes the same
environmental guard, so the file skips upstream too and oxvim is not losing anything upstream keeps.

| class | skips | meaning |
| --- | --- | --- |
| oracle-parity | 403 | oracle skips these too: `has()` is 0 on both, or the guard is environmental |
| oxvim-absent | 99 | oracle has the feature, oxvim genuinely does not: real remaining work |
| **under-claim** | **53** | oxvim exposes the surface but `has()` returns 0: coverage hidden, not absent |
| other | 24 | missing single functions, Nvim-specific N/A, non-Unix, GUI-only |

Largest `oracle-parity` reasons: `cannot make screendumps` 200, `cannot run Vim in a terminal window`
65, `Nvim supports cmdwin freedom #40312` 15, `terminal feature missing` 12, `cannot start the GUI` 9.
The first two both route through `CanRunVimInTerminal()`, which needs `has('terminal')`, and the oracle
reports `has('terminal') = 0`, so those 265 skips are structural for Nvim itself.

Largest `oxvim-absent` reasons: `quickfix feature missing` 67, `timers` 6, `menu` 5, `spell` 3, `diff`
3. Quickfix alone is 67 skipped tests plus 3 whole files, the single largest unbuilt subsystem in the
suite.

The `under-claim` bucket is the one worth a ticket. For each of these, `has(<feat>)` returns 0 on oxvim
while the corresponding option/command/function surface answers `exists()` with 1, so the test skips
against a capability oxvim has at least partly built:

| feature | skipped tests | surface that already exists |
| --- | --- | --- |
| `rightleft` | 11 | `&rightleft` |
| `vartabs` | 10 | `&vartabstop`, `&varsofttabstop` |
| `persistent_undo` | 6 | `&undofile`, `&undodir` |
| `folding` | 6 | `&foldmethod`, `:foldopen`, `foldclosed()` |
| `conceal` | 5 | `&conceallevel`, `&concealcursor` |
| `multi_lang` | 3 | `:language`, `&langmenu` |
| `linebreak` | 3 | `&linebreak`, `&breakindent` |
| `syntax` | 2 | `:syntax`, `&syntax` |
| `nanotime` | 2 | `getftime()` |
| `autocmd` | 2 | `:autocmd`, `##BufRead` |
| `langmap` | 1 | `&langmap` |
| `cmdline_compl` | 1 | `&wildmenu` |
| `arabic` | 1 | `&arabic` |

`autocmd` and `folding` are the clearest: oxvim has `crates/ox-editor/src/autocmd.rs` and Task 59's fold
work, yet `has('autocmd')` and `has('folding')` are 0, so `test_autocmd`-family and fold tests skip
themselves. `exists('&opt') == 1` proves the surface is accepted, not that it behaves, so these are
candidates to investigate rather than proven silent losses — but every one of them is a test the suite
would run if `has()` told the truth. Upstream's table is
`.references/neovim/src/nvim/eval/funcs.c` `f_has` against the `features[]` list in
`.references/neovim/src/nvim/version.c`.

## Blockers ranked by file count

Count = number of distinct test files in which the diagnostic appears at least once (a file is gated by
every blocker it hits, so the column does not sum to 236). Extraction is byte-identical to passes 1 and
2: two independent passes over `stdout+stderr+messages`, `\bE\d+\b` for codes and
`not implemented: <sym>` for missing symbols. Verified by recomputing passes 1 and 2 from their retained
logs in `.outline/sdd/census/` and `.outline/sdd/census-2/`, which reproduces their published figures
exactly (`E117` 131/115, `E492` 87/83, `E15` 43/46, `E605` 41/45, `E121` 30/36, `E444` 0/12).

| rank | blocker | pass 3 | pass 2 | pass 1 | Δ3-2 | Δ2-1 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | `E117` | 135 | 115 | 131 | +20 | -16 |
| 2 | `E605` | 60 | 45 | 41 | +15 | +4 |
| 3 | `E5060` | 39 | 29 | 29 | +10 | +0 |
| 4 | `E121` | 35 | 36 | 30 | -1 | +6 |
| 5 | `E492` | 34 | 83 | 87 | **-49** | -4 |
| 6 | `E474` | 29 | 24 | 23 | +5 | +1 |
| 7 | `E488` | 26 | 22 | 22 | +4 | +0 |
| 8 | `E484` | 21 | 16 | 16 | +5 | +0 |
| 9 | `E523` | 17 | 13 | 14 | +4 | -1 |
| 10 | `not implemented: assert_beeps` | 15 | 10 | 9 | +5 | +1 |
| 11 | `E16` | 14 | 10 | 7 | +4 | +3 |
| 12 | `E475` | 11 | 11 | 12 | +0 | -1 |
| 13 | `E518` | 10 | 6 | 5 | +4 | +1 |
| 14 | `E745` | 9 | 8 | 8 | +1 | +0 |
| 15 | `not implemented: search` | 9 | 7 | 4 | +2 | +3 |
| 16 | `not implemented: setreg` | 9 | 8 | 8 | +1 | +0 |
| 17 | `E1280` | 8 | 6 | 4 | +2 | +2 |
| 18 | `E471` | 8 | 4 | 6 | +4 | -2 |
| 19 | `not implemented: getreg` | 8 | 7 | 6 | +1 | +1 |
| 20 | `E15` | 7 | 46 | 43 | **-39** | +3 |
| 21 | `E216` | 7 | 6 | 2 | +1 | +4 |
| 22 | `E715` | 7 | 6 | 5 | +1 | +1 |
| 23 | `not implemented: screenstring` | 7 | 1 | 0 | +6 | +1 |
| 24 | `not implemented: winsaveview` | 7 | 4 | 3 | +3 | +1 |
| 25 | `E354` | 6 | 6 | 6 | +0 | +0 |

Almost every rise in this table is a consequence of the two collapses. `E15` (−39) and `E492` (−49)
were the gates; with them open, files run further and meet the next wall, which is why `E117` (+20),
`E605` (+15), `E5060` (+10) and `E484` (+5) all grew while `executed` grew by 567. Read rises here as
depth reached, not as new defects, with the four named exceptions in "Defects, not statistics".

### Blockers that collapsed

| blocker | pass 1 | pass 2 | pass 3 | why |
| --- | --- | --- | --- | --- |
| `E492` | 87 | 83 | 34 | Ex command table filled in by tasks 55-57, 65, 69 |
| `E15` | 43 | 46 | 7 | Task 62's expression parser work; 31 files now reach their own skip guard |
| `E444` | 0 | 12 | 1 | pass 2's D1 (`:quit` on a tabpage) fixed |
| `E274` | 3 | 3 | 0 | only blocker with pass-2 count ≥ 3 that reached zero |

### Blockers that are new at ≥ 3 files

| blocker | pass 2 | pass 3 |
| --- | --- | --- |
| `not implemented: tabnext` | 0 | 3 |
| `not implemented: tabfirst` | 0 | 3 |
| `not implemented: tabclose` | 0 | 3 |

### Correction to pass 2's "53 symbols resolved" claim

`oldtest-blockers-2.md` credited 53 `not implemented:` symbols as resolved between passes 1 and 2. At
least 16 of those were never implemented; they went to zero because the only files that reached them
aborted earlier at `E444` or timed out, and they are back in pass 3 now that those files run:

`mode`, `settabvar`, `settabwinvar`, `tabpagewinnr`, `timer_start`, `histadd`, `winlayout`, `winsize`,
`readdir`, `stop`, `suspend`, `copy`, `dlist`, `drop`, `tab`, `nvim_get_hl` — each 1 file in pass 1,
0 in pass 2, 1-2 files in pass 3.

Direct check against the pinned binary: `call mode()` returns
`oxvim: Ex command failed: not implemented: mode`. Nothing removed `mode()` between the passes; pass 2's
zero was an artifact of the abort. A blocker count of zero means "not observed", and a file that aborts
early makes every later symbol unobservable. Pass 3's own zeros carry the same caveat.

Distinct missing-symbol names: 194 (pass 1) → 162 (pass 2) → **200** (pass 3). 6 names went to zero
since pass 2 (`changenr`, `foldclosedend`, `getcompletiontype`, `startgreplace`, `undojoin`,
`undotree`); 44 are newly visible, of which the 16 above are recoveries rather than discoveries.

Head of the remaining missing-symbol distribution is still flat: `assert_beeps` 15, `search` 9,
`setreg` 9, `getreg` 8, `winsaveview` 7, `screenstring` 7, `append` 6, `file` 6, `help` 6, `winline` 6,
`win_execute` 5, `maparg` 4.

## Per-file outcome moves since pass 2 (21 files)

Improved (19):

| file | pass 2 | pass 3 | executed |
| --- | --- | --- | --- |
| `test_normal.vim` | setup-blocked | partial | 0 → 121 |
| `test_options.vim` | setup-blocked | partial | 0 → 89 |
| `test_window_cmd.vim` | setup-blocked | partial | 0 → 73 |
| `test_breakindent.vim` | setup-blocked | partial | 0 → 52 |
| `test_mapping.vim` | timeout | partial | 0 → 50 |
| `test_plugin_netrw.vim` | setup-blocked | partial | 0 → 39 |
| `test_excmd.vim` | setup-blocked | partial | 0 → 36 |
| `test_tabpage.vim` | setup-blocked | partial | 0 → 27 |
| `test_float_func.vim` | setup-blocked | partial | 0 → 26 |
| `test_assert.vim` | setup-blocked | partial | 0 → 25 |
| `test_termcodes.vim` | setup-blocked | partial | 0 → 19 |
| `test_cd.vim` | setup-blocked | partial | 0 → 15 |
| `test_execute_func.vim` | setup-blocked | partial | 0 → 11 |
| `test_tabline.vim` | setup-blocked | partial | 0 → 11 |
| `test_expand.vim` | setup-blocked | partial | 0 → 9 |
| `test_winbuf_close.vim` | setup-blocked | partial | 0 → 7 |
| `test_retab.vim` | timeout | partial | 0 → 4 |
| `test_gui.vim` | setup-blocked | partial | 0 → 3 |
| `test_window_id.vim` | setup-blocked | partial | 0 → 3 |

Regressed (2):

| file | pass 2 | pass 3 | executed | first blocker |
| --- | --- | --- | --- | --- |
| `test_cmdline.vim` | partial | setup-blocked | 45 → 0 | `E677` (D1) |
| `test_unlet.vim` | partial | crash | 7 → 0 | panic (D2) |

`test_assert.vim` also closes pass 2's D3: its silent exit is gone and it now records 25 executed.
`test_alot.vim` and `test_expand.vim` no longer die at `E212`; `test_expand.vim` runs, `test_alot.vim`
still aborts at `E212` inside `FinishTesting()` (see "Files still blocked").

## Files still blocked before their first test (21)

| file | first blocker | pass 2 |
| --- | --- | --- |
| `test_alot.vim` | `E212` | `E212` |
| `test_autochdir.vim` | `E484` | `E484` |
| `test_autocmd.vim` | `E605` | `E605` |
| `test_buffer.vim` | `E948` | `E948` |
| `test_cmdline.vim` | `E677` | partial, 45 executed |
| `test_eval_stuff.vim` | `not implemented: ScreenLine` | same |
| `test_increment_dbcs.vim` | `E484` | `E484` |
| `test_ins_complete.vim` | `E948` | `E948` |
| `test_let.vim` | `E221` | `E221` |
| `test_mksession.vim` | `E484` | `E484` |
| `test_plugin_tar.vim` | `not implemented: runtime` | `E15` |
| `test_plugin_termdebug.vim` | `not implemented: packadd` | `E15` |
| `test_plugin_zip.vim` | `E114` | `E15` |
| `test_regex_char_classes.vim` | `E484` | `E484` |
| `test_regexp_latin.vim` | `E484` | `E484` |
| `test_regexp_utf8.vim` | `E484` | `E484` |
| `test_spell.vim` | `E484` | `E484` |
| `test_substitute.vim` | `E605` | `E605` |
| `test_swap.vim` | `E948` | `E948` |
| `test_vimscript.vim` | `E471` | `E492` |
| `test_writefile.vim` | `E484` | `E484` |

`E484` heads this list at 8 files: the file cannot open a fixture during setup.

## Top 10 expanded, with the upstream surface each needs

### 1. `E117` (135 files, was 115)

Unimplemented builtin function or Ex command; oxvim reports `E117: not implemented: <name>`. Upstream
surface: `.references/neovim/src/nvim/eval/funcs.c` (`f_*` bodies) with the generated dispatch table in
`.references/neovim/src/nvim/eval/funcs.h`, plus `.references/neovim/src/nvim/ex_cmds.lua` for the
command half. Still flat at the head; `assert_beeps` (15 files) remains the cheapest, needing only the
beep counter `f_assert_beeps` reads.

### 2. `E605` (60 files, was 45)

`E605: Exception not caught`: an uncaught throw reaching top level. Almost always secondary to an
`E117`/`E492` raised inside a test. Upstream surface:
`.references/neovim/src/nvim/ex_docmd.c:917`. Its rise tracks depth reached, not a new defect.

### 3. `E5060` (39 files, was 29): the cheapest large win in the census

`E5060: Unknown flag` from `writefile()` flag parsing. oxvim accepts only `b a s S`
(`crates/ox-editor/src/fs_builtins.rs:204-205`); upstream's `f_writefile`
(`.references/neovim/src/nvim/eval/fs.c:1840-1859`) accepts `b a D s S p`. The two missing letters are
`D` (defer delete, `add_defer("delete", …)` at `fs.c:1882-1889`) and `p` (`kFileMkDir` at `fs.c:1878`).
Every one of the 39 files hits it as `Unknown flag: D`, e.g.
`Test_backupskip()` in `test_options.vim` and `Test_abort_in_wincmd_f()` in `test_window_cmd.vim`.
One flag letter, 39 files.

### 4. `E121` (35 files, was 36)

`E121: Undefined variable`: script-local/global/`v:` scope resolution. Upstream surface:
`.references/neovim/src/nvim/eval/vars.c:2386`. The only top-10 blocker that did not grow.

### 5. `E492` (34 files, was 83)

`E492: Not an editor command`: the command is absent from the Ex command table. Upstream surface:
`.references/neovim/src/nvim/ex_cmds.lua`, resolved by `find_ex_command()` at
`.references/neovim/src/nvim/ex_docmd.c:1445`. Halved by tasks 55-57, 65 and 69. oxvim still does not
name the rejected command in the message, so per-command attribution of the remaining 34 needs a
harness change.

### 6. `E474` (29 files, was 24)

`E474: Invalid argument`: argument validation in builtins and commands. Upstream surface:
`.references/neovim/src/nvim/errors.h:33` (`e_invarg`). One concrete instance found while chasing D1:
`set cpo&` raises `E474: No literal default for cpo` (`test_cmdline.vim:1678`), where upstream resets
the option to its default.

### 7. `E488` (26 files, was 22)

`E488: Trailing characters`: the command-line parser stops short of the full argument. Upstream surface:
`.references/neovim/src/nvim/errors.h:122-123`, raised out of command parsing in
`.references/neovim/src/nvim/ex_docmd.c`.

### 8. `E484` (21 files, was 16)

`E484: Can't open file`, and 8 of these are the *first* blocker, so the file dies during setup because a
fixture cannot be opened. Upstream surface: `.references/neovim/src/nvim/errors.h` (`e_notopen`) via
`.references/neovim/src/nvim/ex_cmds.c` `:read`/`:source` handling.

### 9. `E523` (17 files, was 13): a label, not a defect class

Upstream `E523` is `Not allowed here` (`.references/neovim/src/nvim/errors.h:111`, `e_secure`). oxvim
does not use it that way: `error_flow(runtime, "E523", error.to_string())` at
`crates/ox-editor/src/excmd_exec.rs:496`, `507`, `559`, `2590` and `2606` wraps *any* error escaping the
Normal-mode/typeahead machinery under that code. Observed texts include
`E523: no previous search pattern` (upstream is `E35`) and
`E523: register text must be valid UTF-8` (no upstream analogue). Rank 9 therefore hides at least two
distinct upstream error identities, and the error text is plugin-observable, so each leaked string is
its own parity defect.

### 10. `not implemented: assert_beeps` (15 files, was 10)

Highest-count single missing symbol. `f_assert_beeps`
(`.references/neovim/src/nvim/eval/funcs.c`) counts beeps through `Ntest_override('ui_delay', …)`;
oxvim has neither the counter nor the override.

## Defects, not statistics

Four items destroy or distort a whole file's record. Between them they account for all 53 lost
executions and the one panic.

### D1. `let $PATH` is now real, and an aborted test poisons every later `system()` (1 file, fatal)

`test_cmdline.vim` moved `partial` → `setup-blocked`, 45 executed → 0, and it is the largest single
regression in this pass.

Chain, measured end to end against the pinned binary:

1. `Test_shellcmd_completion()` (`test_cmdline.vim:852`) sets
   `let $PATH = getcwd() . '/Xpathdir'` at line 860 and restores it at line 871.
2. Line 866 calls `getcompletion('X', 'shellcmd')`, which oxvim does not implement, so the test throws
   `E117: not implemented: getcompletion` and line 871 never runs.
3. `let $PATH` now mutates the *process* environment (`ox_sys::set_env`), so `$PATH` stays
   `<cwd>/Xpathdir` for the rest of the run. Pass 2's D4 defect — vim-level `let $VAR` not reaching
   children — has been fixed, which is what makes this reachable.
4. At the end, `FinishTesting()` → `Delete_Xtest_Files()` (`runtest.vim:460-476`) reaches
   `call system('rm -rf  ' .. file)` at line 472. With `$PATH` pointing only at `Xpathdir`, the shell
   cannot be found, `Command::output()` fails `ENOENT`, and
   `crates/ox-editor/src/builtins/process.rs:106` raises `E677: Error writing temp file`.
5. `Delete_Xtest_Files()` is unguarded upstream, so the abort is fatal, `messages` is never written and
   the whole record is discarded. `rc = 1`, empty stdout — the buffered `Executing …` echoes are also
   lost on the fatal exit path.

Proof that the tests do run and only the record is lost: with `Delete_Xtest_Files()`'s body wrapped in
`try`/`catch` in a scratch copy of `runtest.vim`, the same invocation writes

```
PROBE PATH=/tmp/oxvim-c3-scratch-p3/testdir/Xpathdir
PROBE cleanup threw: Vim(call):E677: Error writing temp file: No such file or directory (os error 2)
Executed 45 tests
39 FAILED:
```

So the honest reading is 45 executed / 39 failed / 7 skipped, i.e. the file *improved* on pass 2's
45/46/0 and the census records 0/0/0. Two separate defects to fix: `getcompletion()` is missing, and
`system()` failing to spawn should not be `E677` (upstream's `E677` is `write_viminfo`'s temp file;
a shell that cannot be executed gives `v:shell_error` and an empty result, not a throw).

Generalisation worth a ticket: any oldtest that sets `$PATH`, `$HOME` or `$SHELL` and aborts before its
restore line now poisons every subsequent `system()` in that file. `test_cmdline.vim` is the only file
in the suite where that currently fires, but the hazard is structural.

### D2. Panic: `unlet $` calls `std::env::remove_var("")` (1 file, hard abort)

The only crash in the suite. `test_unlet.vim` `rc = 101`, 7 executed → 0:

```
thread 'main' (1840567) panicked at library/std/src/env.rs:433:29:
failed to remove environment variable ``: Invalid argument (os error 22)
```

Source: `remove_target()` at `crates/ox-editor/src/excmd_exec.rs:5899-5908` strips the `$` prefix and
calls `ox_sys::unset_env(environment)` at line **5905** with no name validation;
`crates/ox-sys/src/lib.rs:35-37` forwards straight to `std::env::remove_var`, which panics on an empty
name. Trigger is `test_unlet.vim:23`:

```vim
call assert_fails('unlet $', 'E475:')
```

Upstream `do_unlet_var` (`.references/neovim/src/nvim/eval/vars.c`) raises `E475: Invalid argument: $`
for an empty environment-variable name. The same panic is reachable from any name containing `=` or a
NUL, which `std::env::remove_var` and `set_var` both reject; the assignment path
(`crates/ox-eval/src/builtins.rs:1275-1279`) shares the hazard.

### D3. `system(cmd, input)` never closes the child's stdin (1 file, hang)

`test_system.vim` is the only timeout, and it costs 155 s of the census's 277 s. `test_system.vim:20`
is `call assert_equal('123', system('cat', '123'))`. The process tree at 30 s and at 150 s is

```
timeout 150 /tmp/oxvim-c3-bin … -S runtest.vim test_system.vim
  /tmp/oxvim-c3-bin … -S runtest.vim test_system.vim
    /bin/sh -c cat
      cat
```

so `cat` is still waiting on an unclosed stdin pipe. `timeout` kills oxvim but not the grandchild, which
keeps the pipe open; a runner that reads the child's stdout through a pipe hangs indefinitely rather
than at 150 s. This census used `start_new_session` plus a process-group kill for that one file. The
input-writing path exists for the job-based branch
(`crates/ox-editor/src/builtins/process.rs:139-141`, `manager.send` then `close_input`), so the defect
is in the plain `Command::output()` branch at line 105-106 which ignores the second argument entirely.
Upstream `f_system` writes the input to a temp file and redirects it (`.references/neovim/src/nvim/os/shell.c`).

### D4. `test_diffmode.vim`, one lost execution against 31 conversions

`test_diffmode.vim` went 85 executed / 82 failed / 0 skipped → 84 / 51 / 31. 31 tests moved
failure → skip (`diff feature missing`, honest: oxvim has no `:diffthis`) and one test stopped
executing. First blocker moved `E492` → `not implemented: diffthis`. Net direction is strongly positive;
the single lost execution is noted for completeness, not flagged as a defect.

## Harness caveats

- Every run needs `HOME` pointed at a fresh throwaway directory **and** the testdir copied out of
  `.references`, together. The suite deletes whatever `$HOME` points to.
- The committed `.references/neovim/test/old/testdir/test.log` (71 387 bytes, in-repo paths) is stale
  and inflates failure counts if read. This census stripped it from the base copy and read only the
  per-run `messages` file.
- oxvim drops buffered stdout on a fatal-error exit (`rc = 1`), so a file that aborts loses its
  `Executing …` echoes as well as its `messages`. A `0 0 0` row can therefore hide a full run; D1 is the
  proof.
- oxvim's throwpoint line numbers are wrong by a constant offset inside function bodies: D1's `E677` is
  reported at `Delete_Xtest_Files[9]` where the raising `call system(…)` is body line 12. Pass 2 saw the
  same skew. Do not trust `[n]` in a throwpoint when locating the raising line.
- `first_blocker` uses two labels beyond the four requested outcomes: `no diagnostic` (52 files: the
  file self-skips or ends emitting no code) and `timeout` (1 file). Pass 2's `silent-exit` label is no
  longer needed.
- Timeout budget was 150 s, same as pass 2 and higher than pass 1's 120 s.
- stdin must be `/dev/null`; with an inherited open stdin the headless process blocks at `runtest.vim`'s
  final `qall!`.
