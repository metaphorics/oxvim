# oldtest suite-wide blocker census, pass 2

Source: `.outline/sdd/oldtest-census-2.tsv` (236 files) and the per-file logs in `.outline/sdd/census-2/`
(untracked). Measurement only; no source changed in this leaf.

Binary measured: `target/debug/oxvim` built at git SHA `ed44788ec370988d80ee5783e84d06bb04b5e25f`
(`docs(outline): report task 57 command-line flag parity`), copied to `/tmp/oxvim-census-pinned` before
the run so that peer commits landing during the pass could not change what was measured. The runtime
tree was pinned the same way (`runtime/` copied to `/tmp/oxvim-census-runtime`, exported as
`$OXVIM_RUNTIME`); without that the relocated binary cannot find its runtime and every file dies at
`setup.vim:121` with `E185: Cannot find color scheme 'vim'`.

Invocation, per file, each in its own throwaway copy of `testdir` under `/tmp` with isolated
`XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` / `TMPDIR` / `HOME`, 8-way parallel:

```
timeout 150 /tmp/oxvim-census-pinned -u NONE -i NONE --noplugin --headless \
  --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim <FILE> < /dev/null
```

TSV columns (no header row, matching pass 1): `name`, `outcome`, `executed`, `failed`, `skipped`,
`first_blocker`.

## Outcome distribution

| outcome | pass 2 | pass 1 | delta |
| --- | --- | --- | --- |
| partial | 163 | 167 | -4 |
| setup-blocked | 70 | 60 | +10 |
| timeout | 3 | 6 | -3 |
| crash | 0 | 3 | **-3** |
| **total** | **236** | **236** | 0 |

| totals | pass 2 | pass 1 | delta |
| --- | --- | --- | --- |
| tests executed | 2510 | 2556 | -46 |
| tests with errors | 2314 | 2339 | -25 |
| tests self-skipped | 72 | 77 | -5 |
| `partial` files with `failed = 0` | 2 | 2 | 0 |

The two clean files are `test_options_all.vim` and `test_syn_attr.vim`.

`full-pass` remains structurally unreachable under this invocation: upstream `runtest.vim` never writes
`test.res` (it is a Makefile marker), so `res_exists` is false for every file. The census records that
rather than inventing a pass class.

### Reading the delta honestly

Every panic is gone and 53 previously-missing builtins/commands are now present, yet *executed* fell by
46. The two movements are not in tension: the new Ex commands (`:tabnew`, `:retab`) reach code paths
that abort or hang the whole file, and an aborted file contributes 0 executed tests no matter how far it
got. The three abort mechanisms are itemised under "Defects, not statistics" below; they account for
15 files, and 12 of those were productive in pass 1.

## Blockers ranked by file count

Count = number of distinct test files in which the diagnostic appears at least once (a file is gated by
every blocker it hits, so the column does not sum to 236). Extraction is byte-identical to pass 1
(`\bE\d+\b` for codes, `not implemented: <sym>` for missing symbols) so the two columns are directly
comparable.

| rank | blocker | pass 2 | pass 1 | delta |
| --- | --- | --- | --- | --- |
| 1 | `E117` | 115 | 131 | -16 |
| 2 | `E492` | 83 | 87 | -4 |
| 3 | `E15` | 46 | 43 | +3 |
| 4 | `E605` | 45 | 41 | +4 |
| 5 | `E121` | 36 | 30 | +6 |
| 6 | `E5060` | 29 | 29 | 0 |
| 7 | `E474` | 24 | 23 | +1 |
| 8 | `E488` | 22 | 22 | 0 |
| 9 | `E484` | 16 | 16 | 0 |
| 10 | `E523` | 13 | 14 | -1 |
| 11 | `E444` | 12 | 0 | **+12** |
| 12 | `E475` | 11 | 12 | -1 |
| 13 | `E16` | 10 | 7 | +3 |
| 14 | `not implemented: assert_beeps` | 10 | 9 | +1 |
| 15 | `E745` | 8 | 8 | 0 |
| 16 | `not implemented: setreg` | 8 | 8 | 0 |
| 17 | `not implemented: search` | 7 | 4 | +3 |
| 18 | `not implemented: getreg` | 7 | 6 | +1 |
| 19 | `E216` | 6 | 2 | +4 |
| 20 | `E5113` | 6 | 0 | +6 |

Pass 1's ranks 7 and 10 (`not implemented: cursor`, 25 files; `not implemented: redraw`, 21 files) have
left the table entirely: both are implemented.

## Delta table: what moved

### Blockers that disappeared (pass 1 count >= 3, pass 2 count 0)

| blocker | pass 1 | pass 2 | landed by |
| --- | --- | --- | --- |
| `not implemented: cursor` | 25 | 0 | `ca0ef4a` position builtins |
| `not implemented: redraw` | 21 | 0 | Ex command work |
| `not implemented: tabnew` | 9 | 0 | `7f76e94` |
| `not implemented: col` | 8 | 0 | `ca0ef4a` |
| `not implemented: getpos` | 8 | 0 | `ca0ef4a` |
| `not implemented: scriptencoding` | 8 | 0 | `f946c19` |
| `not implemented: filetype` | 7 | 0 | Ex command work |
| `not implemented: fold` | 5 | 0 | `bebb704` |
| `not implemented: vnew` | 5 | 0 | `7f76e94` |
| `not implemented: read` | 4 | 0 | `a9f3a21` |
| `not implemented: sleep` | 4 | 0 | `f946c19` |
| `not implemented: tempname` | 4 | 0 | builtin work |
| `not implemented: undo` | 4 | 0 | `0260f23` |
| `not implemented: hide` | 3 | 0 | `f946c19` |
| `not implemented: lockvar` | 3 | 0 | `7b507a1` |

53 distinct `not implemented:` symbols went to zero in total; the long tail of single-file resolutions
is `argdelete`, `charcol`, `copy`, `dlist`, `drop`, `finddir`, `findfile`, `foldclose`, `foldopen`,
`getcharpos`, `getcursorcharpos`, `goto`, `histadd`, `matchfuzzy`, `matchfuzzypos`, `mode`,
`nvim_get_hl`, `readdir`, `redrawstatus`, `redrawtabline`, `retab`, `setcharpos`,
`setcursorcharpos`, `settabvar`, `settabwinvar`, `stop`, `suspend`, `tab`, `tabedit`, `tabonly`,
`tabpagewinnr`, `timer_start`, `unlockvar`, `winlayout`, `winsize`, `z` and two netrw helpers.

Distinct missing-symbol names fell from 194 to 162 under this extraction (pass 1's published figure of
200 used a looser count). 21 names are newly visible because execution now reaches them: `wincol` (3
files), then `argdedupe`, `argedit`, `ball`, `checktime`, `diffget`, `diffsplit`, `environ`,
`folddoopen`, `getregion`, `getregtype`, `lfile`, `nohlsearch`, `nvim_get_option_info2`, `preserve`,
`recover`, `screenattr`, `screenpos`, `screenstring`, `stopinsert`, `TestIdx` at one file each.

### Blockers that grew

| blocker | pass 1 | pass 2 | delta | why |
| --- | --- | --- | --- | --- |
| `E444` | 0 | 12 | +12 | regression, see below |
| `E121` | 30 | 36 | +6 | deeper execution reaches more undefined-variable sites |
| `E5113` | 0 | 6 | +6 | newly reachable Lua-chunk error path |
| `E605` | 41 | 45 | +4 | secondary symptom of the above |
| `E216` | 2 | 6 | +4 | autocommand group/event validation now reached |
| `E15` | 43 | 46 | +3 | expression parser reached in more files |
| `E16` | 7 | 10 | +3 | invalid range now reached |

### Per-file outcome moves (22 files)

Improved (7):

| file | pass 1 | pass 2 |
| --- | --- | --- |
| `test_visual.vim` | crash | partial |
| `test_file_size.vim` | timeout | partial |
| `test_recover.vim` | timeout | partial |
| `test_cjk_linebreak.vim` | setup-blocked | partial |
| `test_diffmode.vim` | setup-blocked | partial |
| `test_environ.vim` | setup-blocked | partial |
| `test_listdict.vim` | setup-blocked | partial |

Regressed (15):

| file | pass 1 | pass 2 | pass 2 first blocker |
| --- | --- | --- | --- |
| `test_cd.vim` | partial | setup-blocked | `E444` |
| `test_excmd.vim` | partial | setup-blocked | `E444` |
| `test_execute_func.vim` | partial | setup-blocked | `E444` |
| `test_normal.vim` | partial | setup-blocked | `E444` |
| `test_plugin_netrw.vim` | partial | setup-blocked | `E444` |
| `test_tabline.vim` | partial | setup-blocked | `E444` |
| `test_tabpage.vim` | partial | setup-blocked | `E444` |
| `test_termcodes.vim` | partial | setup-blocked | `E444` |
| `test_winbuf_close.vim` | partial | setup-blocked | `E444` |
| `test_window_id.vim` | partial | setup-blocked | `E444` |
| `test_window_cmd.vim` | crash | setup-blocked | `E444` |
| `test_options.vim` | setup-blocked | setup-blocked | `E444` (was a different blocker) |
| `test_retab.vim` | partial | timeout | `timeout` |
| `test_alot.vim` | timeout | setup-blocked | `E212` |
| `test_expand.vim` | timeout | setup-blocked | `E212` |
| `test_assert.vim` | crash | setup-blocked | `silent-exit` |

## Top 10 expanded, with the upstream surface each needs

### 1. `E117` (115 files, was 131)

Unimplemented builtin function or Ex command; oxvim reports `E117: not implemented: <name>`.

Upstream surface: `.references/neovim/src/nvim/eval/funcs.c` (`f_*` bodies) with the generated dispatch
table in `.references/neovim/src/nvim/eval/funcs.h`, plus `.references/neovim/src/nvim/ex_cmds.lua`
for the command half. The head of the remaining distribution is flat (`assert_beeps` 10, `setreg` 8,
`search` 7, `getreg` 7, `append` 5, `help` 4, `winsaveview` 4, `defer` 4, `taglist` 4), so no single
symbol unlocks a block of files. `assert_beeps` is the cheapest: it needs only the beep counter that
`f_assert_beeps` (`funcs.c`) reads through `Ntest_override('ui_delay', ...)`.

### 2. `E492` (83 files, was 87)

`E492: Not an editor command` means the command is absent from the Ex command table.

Upstream surface: `.references/neovim/src/nvim/ex_cmds.lua`, resolved by `find_ex_command()` at
`.references/neovim/src/nvim/ex_docmd.c:1445`. oxvim still does not name the rejected command in the
message, so per-command attribution needs a harness change before this rank can be decomposed.

### 3. `E15` (46 files, was 43)

`E15: Invalid expression`: the expression parser rejects valid VimL. This is the largest *setup*
blocker: 38 of the 70 setup-blocked files die here at script level before any test runs, so it gates
more untouched surface than its rank suggests.

Upstream surface: `.references/neovim/src/nvim/errors.h:38` (`e_invexpr2`), produced by
`.references/neovim/src/nvim/eval.c` and `.references/neovim/src/nvim/viml/parser/expressions.c`.

### 4. `E605` (45 files, was 41)

`E605: Exception not caught`: an uncaught throw reaching top level. Almost always secondary: the
primary `E117`/`E492` is thrown inside a test and oxvim's `:try`/`:catch` propagation surfaces it here.

Upstream surface: `.references/neovim/src/nvim/ex_docmd.c:917`.

### 5. `E121` (36 files, was 30)

`E121: Undefined variable` covers script-local/global/`v:` scope resolution.

Upstream surface: `.references/neovim/src/nvim/eval/vars.c:2386`.

### 6. `E5060` (29 files, unchanged)

`E5060: Unknown flag`: flag parsing in the file/glob builtins. Unmoved by tasks 51–59.

Upstream surface: `.references/neovim/src/nvim/eval/fs.c:1856`.

### 7. `E474` (24 files, was 23)

`E474: Invalid argument`: argument validation in builtins and commands.

Upstream surface: `.references/neovim/src/nvim/errors.h:33` (`e_invarg`), used from
`.references/neovim/src/nvim/match.c` and `.references/neovim/src/nvim/eval/decode.c`.

### 8. `E488` (22 files, unchanged)

`E488: Trailing characters`: the command-line parser stops short of the full argument.

Upstream surface: `.references/neovim/src/nvim/errors.h:122-123`, raised out of command parsing in
`.references/neovim/src/nvim/ex_docmd.c`.

### 9. `E484` (16 files, unchanged)

`E484: Can't open file`, and 9 of these are the *first* blocker, i.e. the file dies during setup because a
fixture cannot be opened.

Upstream surface: `.references/neovim/src/nvim/errors.h` (`e_notopen`) via
`.references/neovim/src/nvim/ex_cmds.c` `:read`/`:source` file handling.

### 10. `E444` (12 files, was 0): the largest single regression

`E444: cannot close the last tiled window`, raised fatally so the whole file aborts. Root cause and
upstream surface are in the next section; it is listed here because by file count it now outranks
`E475`, `E16` and every remaining missing builtin.

## Defects, not statistics

No panic occurred anywhere in the suite. Pass 1's three hard aborts are all resolved:

| file | pass 1 panic site | pass 2 |
| --- | --- | --- |
| `test_assert.vim` | `crates/ox-editor/src/excmd_exec.rs:1786:31` index out of bounds | no panic (now a silent exit, below) |
| `test_visual.vim` | `crates/ox-editor/src/mode.rs:353:150` index out of bounds | no panic, `partial`, 47 executed |
| `test_window_cmd.vim` | `crates/ox-editor/src/layout.rs:1376:31` unreachable | no panic (now `E444`, below) |

Three defects replaced them. Each one destroys a whole file's results, which is why `executed` fell
even though coverage grew.

### D1. `:quit` cannot close a tabpage (12 files, fatal)

`command_close` (`crates/ox-editor/src/excmd_exec.rs:3090-3117`) decides "is this the last window" with
`editor.windows().len() == 1` at line 3110. `Editor::windows()` (`crates/ox-editor/src/editor.rs:287`)
returns every window in the *editor*, not in the current tabpage. With three tabpages of one window
each the guard sees 3, falls through to `editor.close_window(tab, window, true)` at line 3113, and the
tabpage layout refuses because its own tiled count is 1, giving `LayoutError::LastWindow`
(`crates/ox-editor/src/layout.rs:186-188`), which surfaces as `E444` at line 3115.

Minimal reproduction against the pinned binary:

```vim
edit Xtest
tabnew tab1
tabnew tab2
quit!
" oxvim: E444: cannot close the last tiled window
```

Upstream closes the tabpage instead: `ex_quit` (`.references/neovim/src/nvim/ex_docmd.c`) calls
`win_close()`, which routes the last window of a non-last tabpage into
`close_last_window_tabpage()` (`.references/neovim/src/nvim/window.c`). `E444` is correct only for the
last window of the last tabpage.

Why it is new: before `7f76e94` (`:tabnew`, `:tabedit`, `:tabonly`, `:vnew`) a second tabpage was
unreachable, so the global window count and the per-tabpage count were always equal and the bug was
unobservable. The 12 files hit it inside `RunTheTest`'s window-cleanup loop
(`.references/neovim/test/old/testdir/runtest.vim`, the `while tabpagenr('$') > 1 … quit!` block),
which upstream leaves unguarded, so the failure aborts the script and discards every result already
collected, including files that had run most of their tests.

Secondary defect on the same path: the message text. oxvim prints
`E444: cannot close the last tiled window` (the `LayoutError` Display string leaking through
`error.to_string()`); upstream's text is `E444: Cannot close last window`. Error text is
plugin-observable, so the leaked internal wording is itself a parity defect. Note lines 3093 and 3111
already carry the correct upstream string, so only the `Err(error)` arms at 3035, 3115 and 3613 leak.

### D2. `:retab` never raises `E1240` and loops forever (1 file, hang)

`test_retab.vim` moved from `partial` to `timeout`. Two of its four tests hang:

```
Test_retab_endless          rc 124 (killed at 30 s in isolation)
Test_nocatch_retab_endless  rc 124
```

Both call `RetabLoop()`, which is `while 1 / set ts=4000 / retab 4` on the line `"\t0\t"`. Upstream
breaks the loop by raising `E1240` once the retabbed length would exceed `INT_MAX`
(`.references/neovim/src/nvim/indent.c`, `ex_retab`), and outside a `try` it raises `Interrupted`.
oxvim's `:retab` (added in `1e685e5`) has neither guard, so it spins until the census timeout kills the
process and the file's results are lost. `E1240` is not emitted anywhere in the suite.

### D3. Exit autocommands never fire, so an intentional quit loses everything (1 file, silent)

`test_assert.vim` exits with status 0 after 4.1 s, writes no `messages` file and prints nothing: total
loss of the record, which is worse for observability than the pass 1 panic it replaced. The cause is
`Test_zz_quit_detected`, whose entire body is `quit`, so the test deliberately ends the editor, and
upstream recovers because `RunTheTest` installs `au VimLeavePre * call EarlyExit(g:testfunc)`, which
writes the results before the process leaves.

oxvim knows `VimLeavePre` and `VimLeave` as event names (`crates/ox-editor/src/autocmd.rs:197-198`) but
never dispatches either: a repo-wide search finds no producer for those variants. The missing subsystem
is upstream's exit sequence, `getout()` in `.references/neovim/src/nvim/main.c`, which fires
`VimLeavePre` and then `VimLeave` before `os_exit()`.

### D4. `let $VAR` never reaches child processes, so the suite `rm -rf`s its own HOME (2 files, fatal)

`test_alot.vim` (after 16.8 s of work) and `test_expand.vim` both abort in `FinishTesting()` with
`E212: Can't open file for writing: No such file or directory (os error 2)` while writing `test.log`,
so both files report 0 executed. In pass 1 both were timeouts, so this is newly visible rather than
newly broken.

The first write-up of this entry blamed a `:cd` restore failure. That was wrong, and Task 61's
non-reproduction is what exposed it. The real chain has four steps, every one measured against the
pinned binary:

1. `setup.vim:115` sandboxes the home directory with `let $HOME = expand(getcwd() . '/XfakeHOME')`.
2. oxvim does not export a vim-level `let $VAR` into the environment of child processes. Measured in
   one script: `$HOME` reads back as `<cwd>/XfakeHOME` inside oxvim, while
   `system('printf %s "$HOME"')` returns the inherited process value. Upstream's assignment path
   (`.references/neovim/src/nvim/eval/vars.c`, the env branch of `set_var_lval`) calls
   `vim_setenv`/`os_setenv`, so children inherit it.
3. `test_expand.vim:37` creates a directory named literally `Xdir ~ dir`.
4. `Delete_Xtest_Files()` (`.references/neovim/test/old/testdir/runtest.vim:461-475`) globs `X*` and,
   for anything plain `delete()` could not remove, falls back to
   `call system('rm -rf  ' .. file)` at line 472. The shell word-splits `rm -rf Xdir ~ dir` and
   expands `~` against the *child's* `HOME`, which step 2 left as the inherited value rather than
   `XfakeHOME`.

The result is `rm -rf` of the whole inherited `HOME` tree. In this census `HOME` was the per-file run
root, which contains `testdir`, so the process cwd vanished mid-run and every later write failed with
`ENOENT`. Direct proof, no harness involved:

```vim
call mkdir('Xdir ~ dir')
call writefile(['x'], 'Xdir ~ dir/inner.txt')
call delete('Xdir ~ dir')
call system('rm -rf  ' .. 'Xdir ~ dir')
call writefile(['probe'], 'rmprobe.txt')
" E482: Can't open file "rmprobe.txt" for writing: No such file or directory
```

`$HOME` and the cwd beneath it are both gone at that point. Only `test_expand.vim:37` creates a
tilde-bearing `X*` name and only `test_alot.vim:10` sources that file, which is exactly the two files
that show `E212`, so the chain is closed.

This is a hazard, not just a census artifact: the same two files run with `HOME` pointing at a real
home directory will delete it. `.references/neovim/test/old/testdir` currently holds a stale
`test.log` whose recorded paths are the in-repo testdir, so the suite has been run in place at least
once.

A second defect fell out of the same probe: `expand('~')` returns the literal `~` instead of the home
directory. Upstream expands it in `.references/neovim/src/nvim/os/env.c` (`expand_env_esc`, with
`home_replace` for the reverse direction).

Scope of the defect, so this entry is not misread later. The `rm -rf` at `runtest.vim:472` is the
suite's own design and not an oxvim defect. Main verified the identical probe against the oracle at
`.references/neovim/build/bin/nvim`: upstream also deletes the sandbox `HOME` and leaves the literal
`Xdir ~ dir` directory behind. The defect was only step 2, the sandbox never reaching the child, which
redirected that deletion from `XfakeHOME` to the real home directory. Fixed in `4edc886`, after which
oxvim reproduces the oracle's behavior exactly.

The invariant this leaves behind outlives the fix: the suite will delete whatever `$HOME` points to.
Every oldtest run therefore needs a fresh throwaway `HOME` created for that run and a scratch copy of
the testdir, both parts, every run.

`expand('~')` is still open at the time of writing. Both defects belong to peers; this leaf is
measurement only and does not touch `crates/`.

### Line attribution is off in tracebacks

Both fatal aborts report a throwpoint whose line number does not match the source: the `E444` abort is
reported as `function RunTheTest[71]`, but the `quit!` that raises it is at body line 106 of
`RunTheTest`; the `E212` abort is reported as `function FinishTesting[13]`, but the failing `write` is
at body line 19. Throwpoints are plugin-observable (tests match on `v:throwpoint`), so this is worth a
ticket of its own.

## Timeouts (3 files, 150 s budget)

| file | pass 1 | note |
| --- | --- | --- |
| `test_mapping.vim` | timeout | pre-existing |
| `test_system.vim` | timeout | pre-existing |
| `test_retab.vim` | partial | new, D2 above |

Pass 1's other three timeouts (`test_alot.vim`, `test_expand.vim`, `test_file_size.vim`,
`test_recover.vim`) no longer time out; the budget here was 150 s against pass 1's 120 s, so the
comparison favours pass 2 slightly and the timeout delta should not be read as a pure speed win.

## Harness caveats

- Every oldtest run needs two things together, not either: `HOME` set to a fresh throwaway directory
  created for that run, and the testdir copied to a scratch location so nothing runs inside
  `.references`. The suite deletes whatever `$HOME` points to, by its own design and matching the
  oracle. `test_expand.vim` does it and `test_alot.vim` sources that file. The temporary ban on all
  oldtest runs was lifted once `4edc886` landed; this requirement is permanent.
- Relocating the binary breaks runtime discovery. `runtime_root()`
  (`crates/oxvim/src/runtime.rs:75-87`) resolves `$OXVIM_RUNTIME`, else `<exe dir>/../../runtime`, else
  `./runtime`. A copy under `/tmp` finds none of them, so every file dies at `setup.vim:121` with
  `E185`. The first attempt at this census recorded 226/236 files that way; it was discarded and rerun
  with `$OXVIM_RUNTIME` pointed at a pinned copy of `runtime/`.
- stdin must be `/dev/null`, as pass 1 established: with an inherited open stdin the headless process
  blocks at `runtest.vim`'s final `qall!` instead of exiting.
- `$TEST_FILTER` matches against names that carry `()`, so a pattern anchored with `$`
  (`^\(Test_a\|Test_b\)$`) silently matches nothing and the run reports success having executed no
  tests. Bisection here used `^\(…\)(`.
- `first_blocker` uses two labels beyond the four required: `no diagnostic` (18 files, inherited from
  pass 1: the file self-skips or ends without emitting any code) and `silent-exit` (1 file, D3).
