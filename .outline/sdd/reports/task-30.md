# Task 30: sourced-script heredocs

## Status

Complete for the assigned heredoc contract. Sourced `:lua << [trim] {marker}` commands now collect their bodies before Ex parsing and execute one exact Lua chunk. `:let` and `:const` `=<< [trim] {marker}` forms assign the collected lines as a Vim List. Collection is resolved through the existing Ex parser, so parser-accepted prefixes, ranges, modifiers, and command abbreviations use the same command identity as execution.

The implementation follows `.references/neovim/src/nvim/eval/vars.c:722-912` and `.references/neovim/src/nvim/ex_getln.c:4550-4598`: `trim` derives its exact byte prefix from the first nonempty body line, marker matching optionally removes the source command's indentation, embedded scripts default an omitted marker to `.`, and non-script heredocs require a marker and report a missing terminator.

## Commits

- `5be39fb feat(editor): execute sourced heredocs`

## Verification

- `cargo nextest run -p ox-editor -p ox-excmd`: 628 passed, 0 failed, 0 skipped.
- `cargo build -p oxvim`: clean dev build from the final implementation.
- `make -C .references/neovim/test/old/testdir NVIM_PRG=/home/alpha/rewrite/Oxvim/target/debug/oxvim test_functions.res`: no `.res`; inner process exits 1 after the heredoc blocker is cleared.

## Oldtest end state

The `runtest.vim:175-180` `:lua << trim EOF` body is no longer parsed as Ex. A diagnostic run against the implemented heredoc behavior reached the next first parse blocker at `runtest.vim:354`: `echoconsole 'After executing ' .. a:test` is not registered/resolved by the Ex parser and raises E492. Temporary diagnostic output was removed before the final build and verification.

## Concerns

- The next oldtest blocker is independent command-surface work: `:echoconsole` support or resolution must land before `test_functions.res` can be produced.
- Upstream's separate `eval` heredoc modifier is outside this task's `<< [trim] {marker}` contract and remains unsupported; it must not be confused with the implemented literal-line and trim forms.
