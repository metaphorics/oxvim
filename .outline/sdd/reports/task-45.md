# Task 45: catch-frame transfer and unfiltered oldtest

## Status

**ABANDON proposal for the `.res`/gate-ledger acceptance, not for the implemented fix.** Evaluator errors escaping a called user function now enter the caller's Vimscript catch frame before the function frame is popped. The caught exception has `v:exception = "E117: not implemented: exepath"`; `v:throwpoint` retains the active function frame; `:finally` executes. The original process-level abort is gone.

The corrected unfiltered harness enumerates and runs 110 distinct `Test_` functions. It records 104 failed and 2 skipped, hence 4 passed. This is a real unfiltered run, but upstream `runtest.vim` deliberately creates `test_functions.res` only when `s:fail == 0 && s:fail_expected == 0`; with 104 genuine feature/parity failures it therefore leaves no `.res`. When it does create a success marker, upstream writes a new empty buffer, so G3's `EXPECT: Test` also contradicts the upstream marker format. Making G3 pass requires either implementing the 104 reported oldtest failures or changing the read-only upstream harness/expected gate, neither of which is the assigned catch-transfer fix.

G4's exact command also cannot observe the real summary: the headless executable records `:echo` messages in the `messages` file rather than process stdout. The unchanged harness evidence in `messages` is `Executed 110 tests`, `104 FAILED`, and two `SKIPPED` lines, while the mandated stdout pipeline produces no output.

G2's crate suite is green, but `gate_check.py` marks it failed because the current `cargo-nextest` zero-failure summary is `912 tests run: 912 passed, 0 skipped` and omits the literal `0 failed` required by the ledger.

## Commits

- `3a33295 fix(editor): catch evaluator errors from user functions`
- `e813bbf fix(editor): advance multiline regex cursors`
- `4fa6ab7 fix(editor): preserve tab geometry after closing splits`

The multiline regex fix makes the oldtest `redir`/`substitute(..., 'g')` enumeration produce one entry per function. The layout fix prevents repeated split/close cleanup from shrinking the tab root from 24 rows to one row and aborting `FinishTesting()` with E36.

## Verification

- Catch regression before fix: 0 passed, 1 failed with `NotImplemented("exepath")` escaping the script.
- Catch regression after fix: 1 passed, 0 failed.
- Function execution module: 63 passed, 0 failed.
- Editor/evaluator crate gate: 912 passed, 0 skipped; no failures.
- Exact G1 output: 19 tests run, 19 passed, 532 skipped.
- Unfiltered oldtest: exit 0; 110 executed, 4 passed, 104 failed, 2 skipped; no `Ex command failed`; no `.res` by upstream failure policy.

## Final gate checker output (verbatim)

```text
  PASS G1: evaluator errors raised inside user functions are catchable by Vimscript try/catch (regression test)
  FAIL G2: crate gate green after the change
       ──────────── | Summary [   0.227s] 912 tests run: 912 passed, 0 skipped
  FAIL G3: unfiltered oldtest run of test_functions.vim writes test_functions.res with result lines
       (no output)
  FAIL G4: the harness summary reports a non-zero number of Test_ functions executed
       (no output)
  PASS G5: no per-test escape of the harness error channel (process-level abort) on the unfiltered run
  FAIL G6: all commits for this leaf pushed to origin/main
       23
.outline/GATES.md: 6 gates
UNMET: 4 (met: 2)
```

## Concerns

- The unfiltered oldtest exposes broad parity work (missing builtins, commands, special variables, parser cases, and behavior mismatches) well beyond the task-44 catch-frame blocker.
- G2, G3, and G4 must be corrected to test observable current behavior before the ledger can represent this work faithfully. G6 remains Main's responsibility.
