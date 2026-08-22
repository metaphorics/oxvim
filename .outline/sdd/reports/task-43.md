# Task 43: first real oldtest result progression

## Status

The three contracted defects are fixed: `:source %` resolves the current buffer filename through the `:source` EX_XFILE path, `v:errors` is initialized writable and the non-command assertion family records failures in both `v:errors` and the editor message sink, and List `+=` extends the existing List identity instead of coercing it to zero.

The oldtest harness now sources `test_functions.vim`, discovers its `Test_` functions, and enters `RunTheTest()`. It does not yet reach `FinishTesting()` or write `test_functions.res`: the first remaining architectural blocker is `E15: expression expected` during per-test teardown at `RunTheTest[88]`. This is after the former zero-discovery path and after the first test dispatch begins. The failing command is:

```
/home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim
```

Current terminal result:

```
oxvim: Ex command failed: E15: expression expected
function RunTheTest[88]..script /home/alpha/rewrite/Oxvim/.references/neovim/test/old/testdir/runtest.vim[88]
```

No `.res` file exists. The architectural blocker is the remaining expression/execution mismatch in the `RunTheTest()` teardown path; unlike Task 42, script loading, function discovery, and dispatch are no longer blocked.

## Commits

- `411698b fix(editor): expand current file in source commands`
- `4e6c222 fix(editor): extend lists with compound addition`
- `9a3fe67 feat(editor): integrate assertions with v:errors`
- `48f8e42 feat(editor): add oldtest line append builtins`
- `64ae289 fix(editor): advance oldtest function dispatch`

## Change summary

- `:source` expands an exact `%` argument to the current buffer name, including paths containing spaces. `#` remains unsupported because the editor has no alternate-buffer filename model; no false expansion was added.
- `Editor::new()` initializes `v:errors` as an empty List-compatible array. Writable assignment remains supported.
- `assert_equal`, `assert_notequal`, `assert_true`, `assert_false`, `assert_match`, `assert_notmatch`, `assert_inrange`, `assert_exception`, `assert_equalfile`, and `assert_report` honor generated arities, return `0`/`1`, and append failures to shared state plus messages.
- List/List `+=` snapshots the right side, extends the left List in place, preserves aliases, and safely handles self-extension.
- Harness-level progression added `line()`, `append()`, sourced-script command/function reload semantics, deferred line parse errors inside uncalled functions, `eval trim` heredoc recognition, the harness time format, `tabpagenr()`, and `winnr()`.

## Verification

- `cargo nextest run -p ox-editor -p ox-eval` — **903 passed, 0 skipped**.
- `cargo nextest run -p ox-editor excmd_exec_editor_tests` — **41 passed** after the source fix.
- `cargo nextest run -p ox-editor excmd_exec_state_tests` — **59 passed** after assertions, List `+=`, buffer logging, and heredoc recognition.
- `cargo nextest run -p ox-editor excmd_exec_function_tests` — **61 passed** after same-script reload handling.
- Oldtest harness — reaches `RunTheTest[88]`; **no `.res`**, blocked by the named E15 teardown expression path.

## Concerns

- `:source #` requires a real alternate-buffer state model and was intentionally not faked.
- `assert_fails`, `assert_beeps`, and `assert_nobeep` still require command/beep execution integration; the implemented assertion family covers value, regex, exception, range, and file assertions.
- The harness-only `strftime()` support is intentionally limited to `%H:%M:%S` and uses Unix-day time rather than a local timezone formatter; other formats return explicit not-implemented.
- The pre-existing `.outline/sdd/reports/task-12b.md` modification remains untouched.
