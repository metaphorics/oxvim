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
