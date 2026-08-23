# Task 61, the four census regressions

Five fixes, one commit each, all in `crates/ox-editor` except the two authorized call sites in
`crates/oxvim` noted below. `cargo test -p ox-editor -- --test-threads=1` is **758 passed, 0 failed**
(baseline 742); `cargo test -p oxvim` is 61 passed, 0 failed.

| commit | subject |
| --- | --- |
| `7c50398` | fix(ox-editor): close the tabpage when quitting its last window |
| `ed72f7e` | fix(ox-editor): bound :retab at MAXCOL and raise E1240 |
| `8a21872` | fix(ox-editor): run VimLeavePre and VimLeave on the way out |
| `77140a6` | fix(ox-editor): let :cd leave a directory that no longer exists |
| `4edc886` | fix(ox-editor): export let $VAR to the process environment |
| `205c5f4` | fix(ox-editor): resolve ~ and $VAR in expand() |

Every fix carries a regression test that was mutation-checked: the fix was reverted in place from a
byte copy of the single file (never `git checkout`), the test observed failing, and the copy restored.
The failing assertions are quoted per defect below.

## Harness

Suite measurements use a fresh `shutil.copytree` of `testdir` per file under `/tmp/t61/run-<tag>-<file>`,
with `HOME`, `TMPDIR`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` and
`OXVIM_RUNTIME` all pointed inside that throwaway root, `cwd` at the copy, stdin `/dev/null`:

```
timeout 150 <binary> -u NONE -i NONE --noplugin --headless \
  --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim <FILE> < /dev/null
```

Counts come from `Executed (\d+) tests?`, `^(\d+) FAILED:` and the `SKIPPED ` lines in `messages`. The
"before" binary is `cargo build -p oxvim` at `548c09b`, the tree as it stood when this leaf started;
the "after" binary is the tree with all six commits. `.references` was never run in and `HOME` never
pointed at a real home directory.

## D1, `:quit` raised a fatal E444 on the last window of a non-last tabpage

`command_close` tested for "last window" with the editor-global `editor.windows().len() == 1`, so with
several tabpages of one window each the guard never fired, `close_window` reached a layout that refuses
to empty a tabpage, and the abort landed inside `runtest.vim`'s unguarded
`while tabpagenr('$') > 1 … quit!` cleanup loop.

Upstream's `last_window` (`window.c`:2798) is one window in the *current* tabpage **and** one tabpage in
the editor; only that is E444, and `:quit` there is `getout(0)`. Every other case goes to
`close_last_window_tabpage` (`window.c`:2678-2725), which enters `alt_tabpage()` (`window.c`:3719) and
removes the tabpage. The port now does the same, including the alternate-tabpage choice: the next
tabpage, or the previous one when the closing tabpage is last.

The three `Err(error)` arms that leaked `LayoutError`'s Display string
`cannot close the last tiled window` now carry upstream's `Cannot close last window`
(`:tabonly`, `:close`/`:quit`, `:hide`).

Tests: `quit_on_the_last_window_of_a_tabpage_closes_the_tabpage`,
`close_of_a_tabpages_last_window_enters_the_alternate_tabpage`. Restoring the old guard fails both with
`Err value: Vim(VimException { kind: Error("E444"), value: String(OxStr(Cannot close last window)) })`.

Four of the twelve E444 files, before → after:

| file | executed | failed | skipped | outcome |
| --- | --- | --- | --- | --- |
| `test_tabpage.vim` | 0 → 27 | 0 → 26 | 0 → 1 | abort rc=1 → completes |
| `test_window_cmd.vim` | 0 → 73 | 0 → 64 | 0 → 5 | abort rc=1 → completes |
| `test_cd.vim` | 0 → 15 | 0 → 13 | 0 → 0 | abort rc=1 → completes |
| `test_winbuf_close.vim` | 0 → 7 | 0 → 6 | 0 → 0 | abort rc=1 → completes |

The pre-fix stderr is `oxvim: Ex command failed: E444: cannot close the last tiled window`, which shows
both halves of the defect in one line.

## D2, `:retab` spun instead of raising E1240

`:retab` carried neither of `ex_retab`'s ceilings. Columns are measured with the old `'tabstop'` and
rebuilt with the new one, so each pass over a tab run against a larger `'tabstop'` multiplies the
whitespace written: `test_retab.vim`'s `RetabLoop()` (`while 1 / set ts=4000 / retab 4`) grew the line a
thousandfold per pass, and the file died on the census timeout with nothing recorded.

Both guards now sit where upstream puts them, so the loop is bounded rather than guarded from outside:
`vcol >= MAXCOL` while scanning (`indent.c`:1563-1567) and a `new_len >= MAXCOL` test on the line the
rewrite would produce (`indent.c`:1522-1526). Either abandons the rest of the line, keeps the runs
already rebuilt, and reports `E1240: Resulting text too long` (`indent.c`:1425-1433).

Tests: `retab_past_maxcol_is_e1240_and_leaves_the_line_alone` (536_871 tabs at `'tabstop'` 4000 is
2_147_484_000 columns, one tab past the ceiling) and `retab_just_below_maxcol_still_rebuilds` (536_870
tabs, still rewritten). Removing the two `vcol` ceilings fails the first with
`expected ExecError::Vim(E1240), got Ok(Completed)`.

| file | executed | failed | outcome |
| --- | --- | --- | --- |
| `test_retab.vim` | 0 → 4 | 0 → 2 | timeout at 150.7 s → completes in 1.0 s |

Named gap: upstream also sets `got_int` outside a `:try` so "Interrupted" follows E1240. This port has
no interrupt state, so the raised error is what leaves the loop; `Test_nocatch_retab_endless` asserts
both messages and is one of the two remaining failures in the file.

## D3, `VimLeavePre` and `VimLeave` were never dispatched

Both existed as event names and nothing fired them, so a bare `quit` left the executor directly.
`test_assert.vim`'s `Test_zz_quit_detected` is exactly that, and `runtest.vim`:324 recovers through
`au VimLeavePre * call EarlyExit(g:testfunc)`: without the events the file wrote no `messages` at all
and every result already collected was lost in silence.

`getout` (`main.c`:753-882) fires `VimLeavePre` (:828), writes ShaDa, fires `VimLeave` (:851), then
exits. The executor now runs that sequence whenever a flow ends the process — `:quit` on the last
window, `:qall`, `:cquit`, a quit from inside a function or a sourced script — once per process, before
the scope is synced back so a handler sees the state the quitting command left. An autocmd that fails
does not cancel the exit: upstream reports it through `emsg` and carries on to `os_exit`, so the text is
recorded as a message.

Exits the *host* decides on take the same path through the new public `ExExecutor::run_exit_sequence`:
`run_batch` when its input is exhausted, and `run_stdio`/`run_listener` when the peer goes away. Those
two call sites are in `crates/oxvim`, which Task58Messages owns; they were added with its explicit
authorization and at the placement it specified, and `cargo test -p oxvim` (61 tests, which pin exact
`-es` and `--headless` bytes) is green.

Tests: `quit_runs_vimleavepre_then_vimleave` (order pinned by reading `g:pre` from the `VimLeave`
handler), `the_exit_sequence_runs_once_for_qall_and_cquit`,
`a_quit_inside_a_sourced_script_runs_the_exit_sequence`. Two mutations were checked: never dispatching
fails three tests, and swapping the event order fails the first.

| file | executed | failed | skipped | outcome |
| --- | --- | --- | --- | --- |
| `test_assert.vim` | 0 → 25 | 0 → 15 | 0 → 3 | silent exit 0, no `messages` → completes |

## D4, re-attributed: the export defect, not a `:cd` restore

The brief's D4 read the `E212: Can't open file for writing` at `FinishTesting` as a working directory
that `runtest.vim` failed to restore. That attribution was wrong, and Task60Census corrected the census
artifacts at `aac03ab`/`f9b1a26` while this leaf was running. Neither `test_alot.vim` nor
`test_expand.vim` reproduces the E212 in this harness, with the current binary **or** with a fresh build
of the census pin `ed44788` in a detached worktree: both produce full runs
(`test_alot` 110 executed / 88 failed, `test_expand` 9 / 9). The difference was the harness, not the
binary, and the real chain is:

1. `setup.vim`:115 sandboxes the home directory with `let $HOME = expand(getcwd() . '/XfakeHOME')`.
2. This port recorded that assignment in the script scope only, so children inherited the environment
   oxvim started with.
3. `test_expand.vim`:37 creates a directory named `Xdir ~ dir`.
4. `runtest.vim`:472 cleans leftovers with `call system('rm -rf  ' .. file)`; the shell word-splits that
   name and expands `~` against **the child's** `HOME`.

So the `rm -rf` hit the real home directory instead of the sandbox. That is what destroyed this
session's checkout, `~/.cargo`, `~/.rustup` and `~/.local`, and with them 21 unpushed commits. Where the
census author's `HOME` sat inside the run root, it took the run root with it and the later relative
`test.log` write failed as E212 — the reported symptom, two steps downstream of the cause.

Two real defects, both plugin-observable, both fixed.

**`let $VAR` must reach the process environment** (`4edc886`). `ex_let_env`
(`eval/vars.c`:1349-1351) assigns through `vim_setenv_ext`, which is `os_setenv`: the assignment *is* a
change to the process environment. `unlet $VAR` is the matching process-wide unset (`do_unlet_var`,
`eval/vars.c`:1653-1654), and a `$VAR` read with nothing in the scope copy now falls back to the process
environment, which is what `vim_getenv` reads, so a value set only through `setenv()` is visible.

Tests: `let_env_assignment_reaches_child_processes` compares `$HOME` against a real child
(`system('printf %s "$HOME"')`) **and** against the shell's own `~` (`system('printf %s ~')`), with
`HOME` pointed at a throwaway `/tmp` path and nothing written to it;
`unlet_env_removes_the_variable_from_child_processes` does the same for the unset. Dropping the two
`ox_sys` calls fails both, and the first pre-fix failure is exactly the escape:

```
assertion `left == right` failed
  left: Some("/home/alpha")
 right: Some("/tmp/ox-editor-fakehome-1287959")
```

Main verified the repaired chain against the oracle at `.references/neovim/build/bin/nvim`: upstream
also deletes the *sandbox* `HOME` and leaves `Xdir ~ dir` behind, so the suite is self-inflicted on its
own sandbox and oxvim now matches it. The operational invariant that follows is that the suite will
delete whatever `$HOME` points at, so every run needs a fresh throwaway `HOME` and a scratch copy of
`testdir`.

**`expand()` must resolve `~` and `$NAME`** (`205c5f4`). Anything that was not `%` or `<SID>` came back
verbatim, so `expand('~')` was the literal `~`. Upstream hands that path to `ExpandOne`, which resolves
it through `expand_env_esc` (`os/env.c`) first. An unexpanded `~` is not a smaller answer: a caller that
hands the result to a shell lets the shell expand it against its own environment, which is the other
half of the same chain. The `:set` value expansion already implemented exactly this, so it is now one
shared `expand_env_esc` rather than a second copy.

Test: `expand_builtin_resolves_home_and_environment_variables` (`~`, `~/path`, `$NAME`, `${NAME}`, an
unset name that stays literal, and an interior `~` that is not a home reference). Restoring the verbatim
arm fails it with `left: Some("~")`.

Named gap: the wildcard half of `ExpandOne` is still absent, so a pattern with `*` or `?` comes back as
itself; `glob()` is where this port matches files.

The `:cd` commit (`77140a6`) predates the re-attribution and is kept on its own merits, with its own
test: `changedir_func` (`ex_docmd.c`:6308-6312) reads the directory it leaves purely to record it and
carries on when `os_dirname` fails, while this port turned that failure into E472 and refused the move,
so a script standing in a deleted directory could never `:cd` back out. Restoring the hard error fails
`cd_out_of_a_deleted_directory_still_moves` with E472. It does not fix the E212, and the commit message
should not be read as claiming it does.

## Suite totals for the six files measured

| file | before executed / failed | after executed / failed |
| --- | --- | --- |
| `test_tabpage.vim` | 0 / 0 (abort) | 27 / 26 |
| `test_window_cmd.vim` | 0 / 0 (abort) | 73 / 64 |
| `test_cd.vim` | 0 / 0 (abort) | 15 / 13 |
| `test_winbuf_close.vim` | 0 / 0 (abort) | 7 / 6 |
| `test_retab.vim` | 0 / 0 (timeout) | 4 / 2 |
| `test_assert.vim` | 0 / 0 (silent exit) | 25 / 15 |
| `test_expand.vim` | 9 / 9 | 9 / 9 |
| `test_alot.vim` | 110 / 88 | 110 / 87 |

126 executed tests recovered across the four D1 files sampled and D2/D3, with eight more E444 files
unmeasured here.

## Left for someone else

- `has()` returns 0 for features Neovim always compiles in; already dispatched by Main.
- `$VAR` reads came from `Scope::env`, a snapshot of `std::env::vars_os()` taken once at startup
  (`excmd_exec.rs`:345-346), so `call setenv('X', 'v')` then `echo $X` read empty. Reported to
  Task62ExprParse and fixed by it in `ab2b9b2`: `setenv()` now writes both sides the way `let $VAR`
  does, and its `v:null` branch drops the snapshot entry. Children were never affected, since
  `setenv()` always called `ox_sys::set_env`, so the `$HOME` sandbox was never at risk through that
  path.
- `getcwd()` raises E472 when the working directory has been deleted; upstream's `f_getcwd` returns the
  empty string. Same crate boundary as above.
- `~` and `$VAR` expansion now exists in four places: `expand_env_esc` (`ox-editor`), `expand_env`
  (`ox-eval/src/find_file.rs`), and `expand_home` in both `ox-eval/src/path_builtins.rs` and
  `ox-editor/src/script.rs`.
- Throwpoint line numbers inside functions count executable lines rather than all lines, so the D1
  abort was reported at `RunTheTest[71]` where the `quit!` is body line 106, and the D4 E212 at
  `FinishTesting[13]` where the `write` is body line 19. Plugin-observable through `v:throwpoint`.
