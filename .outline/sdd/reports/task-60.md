# Task 60, fresh suite-wide oldtest census (pass 2)

Measurement only. No source changed in this leaf; the only non-artifact edit is two negation lines in
`.gitignore` so the pass 2 summaries are tracked the way the pass 1 summaries are.

## What was measured

The binary was pinned before the run because peers were committing to this tree throughout it.
`cargo build -p oxvim` was run against a clean working tree at
`ed44788ec370988d80ee5783e84d06bb04b5e25f` (`docs(outline): report task 57 command-line flag parity`),
and `target/debug/oxvim` was copied to `/tmp/oxvim-census-pinned`. Everything below is that copy.
HEAD has since moved (`e3abe00`, Task 59's fold work), which the pin excludes by design.

The runtime tree had to be pinned too. `runtime_root()` (`crates/oxvim/src/runtime.rs:75-87`) resolves
`$OXVIM_RUNTIME`, else `<exe dir>/../../runtime`, else `./runtime`, so a binary copied to `/tmp` finds
no runtime at all and every file dies at `setup.vim:121` with `E185: Cannot find color scheme 'vim'`.
The first attempt recorded 226 of 236 files that way and was discarded; `runtime/` was copied to
`/tmp/oxvim-census-runtime` and exported as `$OXVIM_RUNTIME` for the real pass.

All 236 `test_*.vim` files in `.references/neovim/test/old/testdir` were run 8-way parallel, each in its
own throwaway copy of `testdir` under `/tmp` with isolated `XDG_CONFIG_HOME`, `XDG_DATA_HOME`,
`XDG_STATE_HOME`, `TMPDIR` and `HOME`:

```
timeout 150 /tmp/oxvim-census-pinned -u NONE -i NONE --noplugin --headless \
  --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim <FILE> < /dev/null
```

## Results against the pass 1 baseline

| outcome | pass 2 | pass 1 | delta |
| --- | --- | --- | --- |
| partial | 163 | 167 | -4 |
| setup-blocked | 70 | 60 | +10 |
| timeout | 3 | 6 | -3 |
| crash | 0 | 3 | -3 |

| totals | pass 2 | pass 1 | delta |
| --- | --- | --- | --- |
| executed | 2510 | 2556 | -46 |
| failed | 2314 | 2339 | -25 |
| skipped | 72 | 77 | -5 |

Zero panics anywhere in the suite. All three pass 1 hard aborts are gone, and 53 distinct
`not implemented:` symbols went to zero, including the two that headed the pass 1 blocker table
(`cursor` 25 files, `redraw` 21 files). `E117` fell 131 to 115 and `E492` fell 87 to 83.

Executed nevertheless fell by 46, and that is the finding of this census rather than a rounding error.
Three new whole-file abort mechanisms cost more executed tests than the new builtins bought, because an
aborted file contributes zero regardless of how far it got. They are itemised in
`.outline/sdd/oldtest-blockers-2.md` as D1 to D4; the load-bearing one is D1.

## D1, the regression worth a ticket

`command_close` (`crates/ox-editor/src/excmd_exec.rs:3090-3117`) tests for "last window" with
`editor.windows().len() == 1` at line 3110, but `Editor::windows()`
(`crates/ox-editor/src/editor.rs:287`) returns every window in the editor rather than in the current
tabpage. With three tabpages of one window each the guard sees 3, falls through to `close_window` at
line 3113, and the tabpage's own layout refuses with `LayoutError::LastWindow`
(`crates/ox-editor/src/layout.rs:186-188`), surfacing as a fatal `E444`:

```vim
edit Xtest
tabnew tab1
tabnew tab2
quit!
" oxvim: E444: cannot close the last tiled window
```

Upstream closes the tabpage instead: `ex_quit` routes the last window of a non-last tabpage into
`close_last_window_tabpage()` (`.references/neovim/src/nvim/window.c`), and reserves `E444` for the last
window of the last tabpage. The bug was unobservable before `7f76e94` added `:tabnew`, because a second
tabpage could not exist and the two window counts were always equal.

Cost: 12 files, `E444` moving from 0 files in pass 1 to 12, every one of them a fatal abort inside
`RunTheTest`'s unguarded `while tabpagenr('$') > 1 … quit!` cleanup loop, which discards results the
file had already collected. Eleven of the twelve produced results in pass 1.

Two smaller parity defects ride along. The message text is the internal `LayoutError` Display string
(`cannot close the last tiled window`) where upstream says `Cannot close last window`; lines 3093 and
3111 already carry the correct string, so only the `Err(error)` arms at 3035, 3115 and 3613 leak. And
the throwpoint line numbers are wrong: the abort is reported at `function RunTheTest[71]` when the
`quit!` that raises it is body line 106.

## The other three

- D2. `:retab` (added in `1e685e5`) never raises `E1240` and never yields an interrupt, so
  `test_retab.vim`'s `RetabLoop()` (`while 1 / set ts=4000 / retab 4`) spins until the census timeout.
  The file moved from `partial` to `timeout`. `E1240` appears nowhere in the suite.
- D3. `VimLeavePre` and `VimLeave` exist as event names (`crates/ox-editor/src/autocmd.rs:197-198`) but
  nothing dispatches them. `test_assert.vim`'s `Test_zz_quit_detected` is a bare `quit`, and upstream
  recovers through `au VimLeavePre * call EarlyExit(g:testfunc)`. oxvim exits 0 with no `messages` file
  and no output: total silent loss of the record, replacing a pass 1 panic with something less visible.
  The missing subsystem is the exit sequence in `getout()`
  (`.references/neovim/src/nvim/main.c`).
- D4, and the one that matters most. `test_alot.vim` (after 16.8 s of work) and `test_expand.vim` abort
  in `FinishTesting()` with `E212: Can't open file for writing: No such file or directory` while
  writing `test.log`, because by then the process working directory has been recursively deleted.
  `setup.vim:115` sandboxes the home directory with `let $HOME = expand(getcwd() . '/XfakeHOME')`, but
  oxvim does not export a vim-level `let $VAR` into child environments: `$HOME` reads back as
  `<cwd>/XfakeHOME` inside oxvim while `system('printf %s "$HOME"')` returns the inherited value, where
  upstream's env branch in `.references/neovim/src/nvim/eval/vars.c` calls `vim_setenv`/`os_setenv`.
  `test_expand.vim:37` then creates a directory named literally `Xdir ~ dir`, and
  `Delete_Xtest_Files()` cleans up with `call system('rm -rf  ' .. file)`
  (`.references/neovim/test/old/testdir/runtest.vim:472`); the shell word-splits the name and expands
  `~` against the child's `HOME`, which the defect left as the real inherited one. The suite therefore
  `rm -rf`s the inherited home directory. Only `test_expand.vim:37` creates such a name and only
  `test_alot.vim:10` sources that file, which is exactly the two files showing `E212`. A second defect
  from the same probe: `expand('~')` returns the literal `~` where upstream resolves it through
  `expand_env_esc`/`home_replace` in `.references/neovim/src/nvim/os/env.c`. This is a hazard rather
  than a statistic, and it is the mechanism behind the deletion that cost this session its checkout.
  An earlier draft of this entry blamed a `:cd` restore failure; Task 61's non-reproduction disproved
  that, and the corrected chain is measured end to end in `oldtest-blockers-2.md` D4.

## Blocker movement

Full ranked table with a per-blocker delta, the top 10 expanded with upstream surfaces, and the
per-file outcome moves are in `.outline/sdd/oldtest-blockers-2.md`. Headlines:

| blocker | pass 1 | pass 2 |
| --- | --- | --- |
| `E117` | 131 | 115 |
| `E492` | 87 | 83 |
| `E15` | 43 | 46 |
| `E605` | 41 | 45 |
| `E121` | 30 | 36 |
| `not implemented: cursor` | 25 | 0 |
| `not implemented: redraw` | 21 | 0 |
| `E444` | 0 | 12 |

`E15` is now the largest untouched surface: 38 of the 70 setup-blocked files die at script level in the
expression parser before a single test runs, so it gates more unexplored behavior than its rank
suggests. `E5060` (29 files) did not move at all.

## Artifacts

| path | contents |
| --- | --- |
| `.outline/sdd/oldtest-census-2.tsv` | 236 rows: name, outcome, executed, failed, skipped, first_blocker |
| `.outline/sdd/oldtest-blockers-2.md` | ranked blockers with pass 1 delta, top 10 expanded, defect write-ups |
| `.outline/sdd/census-2/*.log` | 236 per-file logs, ignored by `.gitignore`, not force-added |

`first_blocker` uses two labels beyond the four requested: `no diagnostic` (18 files, inherited from
pass 1, the file self-skips or ends emitting no code) and `silent-exit` (1 file, D3). Blocker counts use
the pass 1 extraction (`\bE\d+\b` plus `not implemented: <sym>`) so the two censuses are directly
comparable; a stricter `E<nnn>:` extraction gives lower absolute counts on both.

## Harness caveats

- No oldtest run of any kind is permitted until the `let $VAR` export defect (D4) is fixed and its
  regression test is green; that covers `runtest.vim` invocations, single-file measurements and quick
  checks, and it suspends any brief's acceptance criterion that asks for a census file to be re-run.
  When the ban lifts, two parts are mandatory together, not either: `HOME` set to a fresh throwaway
  directory created per run, and the testdir copied to a scratch location so nothing runs inside
  `.references`. `.references/neovim/test/old/testdir` currently holds a stale `test.log` recording
  in-repo paths, so the suite has already been run in place at least once.
- The timeout budget was 150 s here against pass 1's 120 s, so the timeout delta slightly favours
  pass 2 and should not be read as a pure speed win.
- stdin must be `/dev/null`, as pass 1 established: with an inherited open stdin the headless process
  blocks at `runtest.vim`'s final `qall!`.
- `$TEST_FILTER` matches names that carry `()`, so a `$`-anchored pattern matches nothing and the run
  reports success having executed no tests. Bisection here used `^\(…\)(` after that trap produced two
  false negatives.
