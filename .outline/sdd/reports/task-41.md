# Task 41: `:redir` and patterned `:function` listing

## Status

Complete for this iteration. `:redir` now captures Ex output to registers, variables, and files with replacement and append forms, `:redir END`, active-redirection `E930`, and `:silent` capture without display. `:function /pattern` now lists matching user-function signatures, including default arguments as `name = expr` and the supported function flags.

The oldtest harness advances past the Task 40 blocker at `runtest.vim:604-607` and stops at the next named blocker: the missing `argc()` builtin at `runtest.vim:610`.

## Commits

- `340ee1d feat(editor): implement Ex output redirection`

## Change

- Added executor-owned redirection state and target parsing for:
  - `:redir @r`, `:redir @R`, `:redir @r>`, and `:redir @r>>` register replacement/append;
  - `:redir => var` and `:redir =>> var` variable replacement/append;
  - `:redir > file` and `:redir >> file` file truncation/append;
  - case-insensitive `:redir END` and `E930` when another redirect is active.
- Register and file targets stream captured output as messages are emitted. Variable output remains buffered and is assigned at `END`, matching upstream's `var_redir_start`/`var_redir_stop` lifecycle.
- `:silent` now suppresses newly emitted editor messages after redirection has captured them; unsilenced redirected messages remain visible.
- Message cursors prevent nested Ex execution from capturing the same message twice. Consecutive `:echon` commands concatenate without an inserted newline.
- Added `:function /pattern` and bare `:function` listing through the existing user-function registry and `ox-regex`. Signature rendering includes required/default arguments, `...`, and `abort`, `range`, `dict`, and `closure` flags.
- Extended the in-memory function-test `FileIO` seam with append writes and added five end-to-end redirection/listing tests.

## Test summary

- `cargo nextest run -p ox-editor redir` — 5 passed, 521 skipped.
- `cargo nextest run -p ox-editor excmd_exec_function_tests` — 61 passed, 465 skipped.
- `cargo nextest run -p ox-editor` — 526 passed, 0 skipped.
- `cargo build -p oxvim` — succeeded against committed `340ee1d`.
- Independent re-review after lifecycle fixes — no actionable findings.

## Oldtest end state

Invocation from `.references/neovim/test/old/testdir`:

`/home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim`

Result:

`oxvim: Ex command failed: not implemented: argc`

The harness passes `redir @q`, `silent function /^Test_`, `redir END`, and the subsequent transformation of `@q` at `runtest.vim:604-607`. It stops at `argc()` in the argument-filter setup at `runtest.vim:610`. No `.res` is produced before this setup-time blocker.

## Concerns

- The next blocker is `argc()` (with `argv(1)` immediately following on the same branch); these argument-list builtins are outside Task 41's redirection/function-listing scope.
- Redirection captures the editor's existing string message payloads. Future non-string message payloads will need an explicit textual rendering contract before they can participate in Vim-compatible redirection.
- The working tree already contained an unrelated modification to `.outline/sdd/reports/task-12b.md`; Task 41 did not stage or alter it.
