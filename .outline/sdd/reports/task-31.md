# Task 31: oldtest lazy errors and harness continuation

## Status

Stopped at a genuine architectural blocker after clearing the misdiagnosed `:echoconsole` failure and four subsequent harness blockers. Neovim intentionally raises E492 for the Vim-only `:echoconsole`; Oxvim was incorrectly resolving every function-body command before control-flow selection. E492 is now deferred until the instruction is actually executed, so the inactive `_terminal_` branch matches the Neovim oracle.

The harness then advanced through empty menu cleanup, the core `nvim.popupmenu` augroup cleanup, the macro-backed `grepformat` default, `system()`, and the harness-critical `expand()` forms. The next failure is `not implemented: mkdir` while `setup.vim` creates `XfakeHOME`.

## Commits

- `f4a9975 fix(editor): defer inactive Ex command errors`
- `a71808b fix(editor): accept empty global menu cleanup`
- `d25d42f fix(editor): clear named autocmd groups`
- `99f086c fix(editor): resolve grepformat default`
- `b357d9e feat(editor): execute system builtin`
- `9cb9033 feat(editor): expand current buffer paths`

## Verification

- Oracle: `make test_functions.res NVIM_PRG=.references/neovim/build/bin/nvim` completed; its passing `.res` is empty by oldtest convention.
- Focused modules: control-flow 29 passed; state commands 43 passed; editor commands 40 passed; function/source 38 passed.
- `cargo nextest run -p ox-editor -p oxvim`: 514 passed, 0 skipped.
- Fresh `make test_functions.res NVIM_PRG=/home/alpha/rewrite/Oxvim/target/debug/oxvim`: runner exits 1 and produces no `.res`; direct batch diagnosis reaches `not implemented: mkdir`.

## Oldtest end state

The prior eager E492 at `runtest.vim:354` is gone: unresolved commands in inactive function branches no longer abort definition or invocation. `setup.vim` now passes `aunmenu *`, `tlunmenu *`, `autocmd! nvim.popupmenu`, and `set grepprg& grepformat&`; cleanup can call `system()`, and `expand('%')`/`expand($BUILD_DIR)` execute. The first remaining blocker is `mkdir()` at `setup.vim:116`.

## Architectural blocker

Oxvim has no filesystem-builtin host contract. `FileIO` only exposes whole-file read/write, regular-file existence, and canonicalization; implementing `mkdir()` honestly immediately requires directory metadata and mutation, followed on this same setup path by `isdirectory()` and later oldtest cleanup/discovery operations such as recursive `delete()`, `glob()`, `readfile()`, and `writefile()`. Adding isolated `std::fs` calls inside individual evaluator branches would bypass the deterministic `FileIO` test seam and create a second filesystem convention. The next task should extend the filesystem host as one coherent capability and route these builtins through it.

## Concerns

- `system()` currently uses the platform shell and captures stdout/status, but its optional stdin argument is not implemented because the reached harness call only required the one-argument form.
- `expand()` implements `%`, `<SID>`, and pass-through paths required by the reached harness path; other special keys such as `<afile>` remain outside the exercised path.
