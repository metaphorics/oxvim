# Task 47: feedkeys and builtin remainder

## Status

Complete for leaf-B gates G1-G5. `feedkeys()` now uses the editor's single typeahead buffer with `i`, `n`, `x`, `L`, and `!` mode behavior; the evaluator host supplies `getchar()`/`getcharstr()` consumption. The scoped pure and stateful builtins are implemented, and the measured `simplify()`, `slice()`, `str2nr()`, `strpart()`, and string `reverse()` mismatches were corrected. The unfiltered `test_functions.vim` run improved from 22 to 29 passing tests.

G6 remains Main's publication gate, as assigned.

## Commits

- `c1ae152 fix(eval): add remaining pure builtins`
- `4bb3c13 fix(editor): route feedkeys through typeahead`
- `a0b59fb fix(editor): add directory and funcref builtins`
- `ac78970 fix(editor): add cursor and highlight builtins`
- `f18b9e4 fix(eval): align measured string semantics`
- `7a215e7 fix(editor): admit insert command path`

## Verification

- `cargo nextest run -p ox-editor -p ox-eval`: **949 passed, 0 skipped**.
- `cargo nextest run -p ox-editor -E 'test(/feedkeys/)'`: **5 passed**.
- Final unfiltered `test_functions.vim`: **110 executed, 29 passed, 79 failed, 2 skipped**.
- G1-G5: PASS.

## Final gate checker output

```text
  PASS G1: no E117 'not implemented' for the scoped builtins in a fresh harness run
  PASS G2: passed count improves over the 22-passed baseline
  PASS G3: crate gate green (ox-editor + ox-eval)
  PASS G4: feedkeys unit tests pin mode semantics (i inserts into typeahead consumed as input; n does not remap; x executes; ! appends after pending input)
  PASS G5: no per-test process-level abort on the unfiltered run
  FAIL G6: all commits for this leaf pushed to origin/main
       6
.outline/GATES.md: 6 gates
UNMET: 1 (met: 5)
```

## Concerns

- `lcd` is modeled over process cwd with restoration when the temporary split buffer is wiped; the current editor model does not yet expose a fully independent cwd per window/tab/buffer, so wider `test_cd.vim` parity remains incomplete.
- `feedkeys()` preserves the existing internal keycode encoding and uses `K_EVENT` for `L`, but the wider special-key simplification surface in `getchar()` remains incomplete; `Test_getchar` still reports modified-function-key shape mismatches.
- The remaining scoped aggregate failures are downstream blockers rather than missing leaf-B builtins: `systemlist`, `win_getid`, `winwidth`, translated loop variables, full multiline `:insert` body execution, showbreak-aware `virtcol`, and the previously documented trim aggregate construction issue.
- `strftime()` exists, but `Test_strftime`/`Test_strptime` still stop in their `CheckFunction` command path before exercising the time conversion semantics.
