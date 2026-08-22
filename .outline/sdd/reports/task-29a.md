# Task 29a: oldtest evaluator and sourced-script unblock

## Status

Complete for the assigned contract. Implemented upstream-shaped `exists()` lookup across evaluator variables/environment/options/builtins and editor user functions, Ex commands, supported events, augroups, and registered autocommands. Implemented source-local `:finish` propagation, persistent `s:` function resolution against each function's defining SID, and `$"..."`/`$'...'` interpolation with brace escaping, nested expressions, Vim string coercion, and typed malformed-input errors.

The oldtest run also exposed the previously hidden `$put =error` parse failure in an unreachable screen-size error branch. The executor now parses and evaluates the expression-register form generally rather than special-casing that line.

## Commits

- `5949ddf feat(eval): unblock oldtest script setup`

## Verification

- `cargo nextest run -p ox-eval -p ox-editor`: 828 passed, 0 failed, 0 skipped.
- `cargo build -p oxvim`: clean dev build.
- `make -C .references/neovim/test/old/testdir NVIM_PRG=/home/alpha/rewrite/Oxvim/target/debug/oxvim test_functions.res`: no `.res`; inner process exits 1.

## Oldtest end state

The four named blockers are cleared, and `$put =error` is also cleared. Diagnostic execution against the same final behavior identifies the next first failure while parsing `runtest.vim:176`: `require('ffi').cdef([[` is treated as an Ex command because the executor does not yet consume the multiline `:lua << trim EOF` heredoc body at lines 175-180. This produces E492 (`not an editor command`), then the outer runner reports `Quit(1)`. Diagnostic logging was removed before the final build and verification.

## Concerns

- Multiline Ex heredocs are a distinct parser/execution feature outside this task's four-feature contract; oldtest cannot produce `test_functions.res` until the `:lua << [trim] {marker}` body is collected as one Lua command rather than parsed line-by-line as Ex.
- `:put =expr` now evaluates and inserts the result directly because the register store's `=` slot retains expression source and requires a separate evaluator provider; the observable insertion contract is covered, but expression-register replay through later `@=` use remains the register subsystem's existing seam.
