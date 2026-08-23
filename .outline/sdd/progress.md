# Oxvim execution ledger

This file was lost once because `.outline/` was gitignored in full. It is tracked now, together with the reports and the census artifacts. Do not un-track it.

## Recovered state

Origin held only `b05106f` when the working checkout at `/home/alpha/rewrite` was deleted, because the push token had stopped working earlier in the session. Twenty-one local commits were lost. Recovery commit `c41dab3` restored their file content from a surviving git worktree; per-commit history could not be recovered. Details are in that commit message.

Lost with no backup: the task 51 through 55 reports (reports were tracked only through `task-50.md`), and this ledger, reconstructed below from the session record.

## Foundation through editor core

Workspace, toolchain, justfile, third-party fetch, upstream oracle build, then `ox-types`, `ox-rpc`, `ox-loop`, `ox-text`, `ox-regex`, `ox-editor`, `ox-eval`, `ox-excmd`, `ox-api`, `ox-lua`, `ox-ui`, `ox-tui`, and the `oxvim` binary. The differential harness landed at `4ef0f37`; API metadata parity reached zero unsanctioned diff at `aa530e2`; option setter returns, terminal job channels, and `ui_attach` ordering followed at `df5db96` and `c295695`.

## Runner and harness unblock chain (tasks 17 to 24)

Lua prelude, `vim.uv` filesystem binding, runner builtins, the `ox-sys` sanctioned-unsafe crate for environment mutation, variable tables, process spawn, mpack sessions, `:lua`, `nvim_cmd`, listen-address lifecycle, deferred-close draining, and the between-test loop pump. The functional suite ran cases from this point but produced no terminal counts until the pump fix at `c0aa0fb`.

## Oldtest campaign, gated leaves (tasks 29 to 49)

The harness became fully operational: it discovers `Test_` functions, dispatches them, and reports per-test results to the messages file. Upstream writes `.res` only on a full pass, so it is a make marker rather than a results file.

Measured progression on `test_functions.vim`, 110 executed with 2 skipped upstream-expected:

| leaf | passed | failed |
| --- | --- | --- |
| baseline | 4 | 104 |
| A | 22 | 86 |
| B | 29 | 79 |
| C | 34 | 74 |
| D | 36 | 72 |

Landed along the way: try/catch error transfer, feedkeys typeahead, `:redir`, the argument list, heredocs, `luaeval`, colorschemes including Lua ones, `:language`, runtimepath handling, job control, filesystem builtins, the string and Unicode builtin tail, `systemlist`, window and screen builtins, the `g:` variable Lua sync fix, and the LuaRef lifecycle (both the leak and the frees).

## Suite-wide census (task 50, commit lost, content restored)

All 236 oldtest files measured with stdin redirected from `/dev/null`: 167 partial, 60 setup-blocked, 6 timeout, 3 crash, and 2556 tests executed with 2339 failing and 77 skipped. A predecessor's run had misclassified 202 files as timeouts purely because inherited stdin makes headless oxvim block at `qall!`.

Blockers ranked by the number of files each gates: `E117` 131, `E492` 87, `E15` 43, `E605` 41, `E121` 30. By name: `cursor` 100, `filetype` 99, `setqflist` 97, `search` 68, `redraw` 65, `getpos` 43.

Artifacts: `.outline/sdd/oldtest-census.tsv`, `.outline/sdd/oldtest-blockers.md`, and the per-file logs under `.outline/sdd/census/`.

## Leaf waves after the census

- Task 51, review clean: the editor builtin dispatch split into family modules, `excmd_exec.rs` from 5886 to 4524 lines, plus three census panics fixed at their cause.
- Task 52, review found two Important issues, both fixed: `matchfuzzy`, `matchfuzzypos`, `tempname`, `findfile`, `finddir`, and the lockvar engine; `getcompletion` declined because it needs the cmdline expansion engine.
- Task 53, review clean: all four `ox-text` mutators were quadratic, materializing every line and rebuilding the rope. Now ranged splices. A 10000-line append went from 497.3s to 84ms, and `test_file_size.vim` from killed at a 900s cap to 23.2s. `test_window_cmd.vim` went from a census timeout to 73 tests executed.
- Task 54, review found four Important issues: real `curswant` and `coladd` window state plus a `var2fpos` port serving twelve position names. Lost in the deletion and restored from a single-file backup with its tests rewritten.
- Task 55, review found two Important issues: the `:redraw` family, `:filetype`, and `:read` with `:write !cmd`. `:help` declined because it needs the tag subsystem and a `doc/tags` index.
- Task 56: the `E16` address-domain fix at the range-resolution site with `addr_type` sourced from upstream's own command table, and the `:read` autocommand events. Its research corrected task 55 twice: `:redo` takes no count, so `:3redo` is `E481`.

## Open

The six remaining Ex command groups, the `E117` builtin tail (about 191 distinct names), the functional suite via `NVIM_PRG`, the plugin ecosystem probe, TUI theme and PTY checks, and bench sanity.

## Environment losses to repair

The deletion took `~/.cargo`, `~/.rustup`, `~/.local` and the global gitconfig with it. The toolchain was reinstalled and the identity restored from the repository's own history. `.references/neovim`, the read-only upstream specification and its built oracle binary, was destroyed and is being re-cloned; the oracle needs rebuilding before any oracle comparison or oldtest run.

The push token is still invalid. Nothing since `b05106f` is on the remote, so history is preserved locally as a bundle at `/home/alpha/oxvim-recovery/oxvim-full.bundle` with a copy at `/tmp/oxvim-full.bundle`, and the loose survivor files are archived at `/tmp/oxvim-recovery-survivors.tar.gz`. Run `gh auth login` and push to make this durable.

## Waves after recovery (tasks 56 to 62)

- Task 56: all six remaining Ex command groups, plus the two task-55 audit fixes. `:retab`; `:hide`, `:sleep`, `:z`, `:scriptencoding`, `:argdelete`; `:lockvar` and `:unlockvar`; `:fold`, `:foldopen`, `:foldclose`; `:tabnew`, `:tabedit`, `:tabonly`, `:vnew` with a real tab order vector and `close_tabpage`; `:undo` and `:redo`. Its research corrected the brief twice: `:redo` takes no count, so `:3redo` is `E481`. Also fixed `winnr` and the screen builtins counting windows globally rather than per tabpage, latent until tabpages could exist.
- Task 57: command-line flag parity, built from upstream's own `command_line_scan` rather than a guess. `-c` and `+cmd` with correct ordering against `--cmd`, `--version`, `--help`, clustered short options, the startup option flags, the window openers, `--startuptime`. Declined with named subsystems: `-d`, `-A`, `-H`, `-D`, `-q`, `-t`, `-r`, `-L`, `--remote`, `--server`. Audit found four gaps, all fixed in task 58.
- Task 58: the message-routing seam. Upstream decides in `msg_use_printf` (message.c:3013) as a function of embedded mode, whether a UI is attached, and `silent_mode` with `p_verbose`; `info_message` routes to stdout, everything else to stderr, and silent mode drops it. The editor's sink was a pure accumulator with no access to any of that. Nine oracle comparisons now byte-identical, seven of seven mutation checks killed their test.
- Task 59: fold observability. `foldclosed`, `foldclosedend`, `foldlevel`, byte-identical to the oracle, which makes task 56's fold commands observable through the binary for the first time. `foldtext` and `foldtextresult` declined: they need `v:foldstart`, `v:foldend`, `v:folddashes` and a `foldtext` evaluator that do not exist.
- Task 60: fresh 236-file census at `ed44788`, compared against the first. Crashes 3 to 0, timeouts 6 to 3, `cursor` from 100 files to 0, `redraw` from 65 to 0, `E117` 131 to 115, `E492` 87 to 83. But executed fell 2556 to 2510 and setup-blocked rose 60 to 70, entirely from four whole-file aborts the new features introduced. A feature that costs a whole file buys nothing.
- Task 61 (in flight): the four aborts. `:quit` raising a fatal `E444` on the last window of a non-last tabpage, 12 files, because `command_close` tested the editor-global window count instead of the tabpage's; `:retab` spinning to a timeout instead of raising `E1240`; `VimLeavePre` and `VimLeave` named but never dispatched, which cost `test_assert.vim` silently; an `E212` on a relative `test.log` write because the working directory was not restored.
- Task 62 (in flight): `E15`, now the largest untouched surface, gating 46 files with 38 of the 70 setup-blocked files dying at script level, so each fix can unlock a whole file rather than one test.

## Process lessons paid for in lost work

- `.outline/` was gitignored in full, so the reports and this ledger were untracked and a deleted checkout took them with it. Now tracked; do not un-track them.
- Three separate destructive operations cost real work: a `git stash` that swept every dirty file in a shared tree, the directory deletion itself, and `git checkout -- crates/` used as mutation-check undo, which reverted a peer's uncommitted edits twice. All three had a blast radius wider than the author's model of it. Mutation checks now copy the single owned file to /tmp and restore from that copy, then touch it so cargo does not serve a stale binary.
- Parallel workers sharing one tree need file-level ownership stated up front and a named owner for every shared file, or they overwrite each other silently.

## Measured state at the end of this arc

Workspace: 2004 tests passing, zero failures, verified independently at the top level rather than taken from worker reports.

Third census (`8ff70b1`) against the first two:

| metric | census 1 | census 2 | census 3 |
| --- | --- | --- | --- |
| executed | 2556 | 2510 | 3077 |
| failed | 2339 | 2314 | 2267 |
| skipped | 77 | 72 | 579 |
| setup-blocked files | 60 | 70 | 54 |
| timeouts | 6 | 3 | 1 |
| crashes | 3 | 0 | 1 |

Of the skip growth, 384 tests moved from failing to skipping honestly, because `has()` and `exists()` stopped over-claiming: a test that skips on a genuinely absent capability is correct, and the census separates that from the 53 lost executions it also found. Blockers: `E492` fell 49 files, `E15` went 46 to 7, `E444` 12 to 1, while `E117` rose 20 as newly running files reached further.

The single largest unblock was not a feature. `push_source` allocated a new script ID per source, so `s:` variables died between sources, `setup.vim`'s load guard never fired, and its `comclear` wiped every user command before every oldtest. Fixing that converted 460 failures into 231 across 20 files.

The crash count of 1 in census 3 was `unlet $` reaching `std::env::remove_var("")`; it is fixed, and the file it killed now runs 7 tests.

## The deletion, for the record

The suite deletes whatever `$HOME` points to, by design: `setup.vim` sandboxes it, and `runtest.vim` cleans up with `rm -rf` over names the shell word-splits, one of which expands `~`. Oxvim did not export a vim-level `let $VAR` to child processes, so the sandbox never reached the shell and `~` resolved to the real home. That is what destroyed this checkout, `~/.cargo`, `~/.rustup` and `~/.local`, and with them 21 unpushed commits. The export defect is fixed and the behavior now matches the oracle exactly, which I verified by running the same chain on both binaries. The `just oldtest` recipe now refuses to start unless `HOME` is a throwaway path, and copies the testdir rather than running in the reference tree.
