# Task 25: oldtest reset and function-name unblock

## Status

Implemented `:set option&` / `:set option&vim` default restoration, upstream-compatible script-local and dictionary-member function definitions, and three further startup blockers reached while iterating the Neovim oldtest runner. The owned crates pass their required gate. The oldtest runner now reaches `jobstart()` in `runnvim.vim`; Oxvim has no job-control implementation anywhere in the repository, so executing the child test file is blocked on architecture outside this task's owned crates.

## Commits

- `179a9c7 fix(ox-editor): accept Vim option reset suffix`
- `45d4e19 fix(ox-editor): define functions on dictionaries`
- `548d329 fix(ox-editor): allow lowercase script functions`
- `fb76a77 fix(ox-editor): evaluate dynamic expressions`
- `fdf4c0c fix(ox-editor): create empty buffers with enew`
- `321124b fix(ox-editor): retain live script scope in calls`

## Behavior

- `:set background&` and `:set background&vim` restore the option metadata's declared default through the existing option mutation path.
- Dictionary function definitions resolve an existing dictionary path, register the function under its canonical name, and install a callable Funcref at the member key. `function! s:logger.on_stdout()` is accepted and callable; missing/non-dictionary paths remain errors.
- Lowercase `s:`, `<SID>`, and canonical `<SNR>` function names are accepted. A lowercase bare name still raises E128.
- Same-script calls retain the currently live `s:` scope rather than replacing it with a not-yet-stored script registry snapshot.
- The editor host now evaluates `eval({string})` in the current scope, and `:enew` creates and selects a distinct empty buffer with the existing modified-buffer guard.

## Verification

- `cargo nextest run -p ox-editor 'excmd_exec_state_tests::'`: 33 passed.
- `cargo nextest run -p ox-editor 'excmd_exec_function_tests::'`: 30 passed.
- `cargo nextest run -p ox-editor -p ox-excmd -p ox-eval`: 962 passed, 0 skipped.
- `cargo build -p oxvim`: passed after each harness-facing source change.

## Oldtest endpoint

Command:

```text
make -C .references/neovim/test/old/testdir NVIM_PRG=/home/alpha/rewrite/Oxvim/target/debug/oxvim
```

Progression observed from fresh rebuilt binaries:

1. E128 on `s:logger.on_stdout`.
2. E128 on lowercase `s:escape_non_printable`.
3. `not implemented: eval`.
4. `not implemented: enew`.
5. E121 on live `s:logger` inside `Main()`.
6. `not implemented: jobstart`.

The final run reports each selected oldtest file as failed because the outer `runnvim.vim` runner exits at `jobstart(args, s:logger)` before spawning the child test process. Repository-wide search under `crates/` finds no `jobstart`, `jobwait`, or `jobstop` implementation. This is an architectural job-control gap: completing it requires subprocess lifecycle integration, environment propagation, callback invocation with bound dictionary `self`, terminal-buffer capture, wait/stop semantics, and wiring to the executable/runtime layers outside `crates/ox-editor`, `crates/ox-excmd`, and `crates/ox-eval`.

## Concerns

- Dictionary Funcrefs are installed and callable, but bound `self` is not yet propagated through `ox-eval::Evaluator::call_value`; the oldtest job callbacks require this together with the missing job subsystem.
- The oldtest Make target enumerates test files, but no child test file can execute until job control exists; the final `Failed: test_arabic` line is a runner-startup failure, not a test-body result.
