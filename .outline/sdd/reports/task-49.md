# Task 49: leaf-D function semantics

## Status

Complete for assigned gates G1-G4. `v:progpath` is seeded from the running executable; `setbufvar()` writes buffer variables and buffer-local options with E518 unknown-option reporting; `getchar()` consumes modifier-prefixed special keys atomically and honors `number`/`simplify`; `virtcol()` includes `showbreak` cells on wrapped continuation rows; the regex engine handles non-ASCII default keyword characters and rejects unattached lookaround suffixes with E866; queued input command lines execute without the prior downstream E121 failures. G5 remains Main's publication gate.

## Commits

- `fa32403 fix(editor): add progpath and setbufvar semantics`
- `9bb7116 fix(editor): preserve getchar special key semantics`
- `cdeb547 fix(editor): count showbreak in virtcol`
- `38ef0fd fix(regex): close matchstrlist engine gaps`
- `e3b2af8 fix(editor): execute queued input command lines`
- `ce07ff1 fix(editor): report misplaced regex lookaround`

## Verification

- `cargo nextest run -p ox-editor -p ox-eval`: **961 passed, 0 skipped**.
- `cargo nextest run -p ox-regex`: **121 passed, 0 skipped**.
- Fresh unfiltered `test_functions.vim`: **110 executed, 36 passed, 72 failed, 2 skipped** (improved from 34 passed).
- Process-level `Ex command failed` aborts: **0**.
- Assigned G1-G4: **PASS**.

## Final gate checker output

```text
  PASS G1: v:progpath set at startup and setbufvar functional (unit tests)
  PASS G2: passed count improves over the 34-passed baseline
  PASS G3: crate gate green (ox-editor + ox-eval)
  PASS G4: no per-test process-level abort on the unfiltered run
  FAIL G5: all commits for this leaf pushed to origin/main
       6
.outline/GATES.md: 5 gates
UNMET: 1 (met: 4)
```

## Concerns

- `Test_screen_functions` now passes `v:progpath` initialization and reaches the pre-existing unsupported `:!` command path.
- `Test_getchar` still exposes parser/keycode gaps outside this leaf: dictionary-literal parsing stops the later option matrix, and the internal Tab spelling differs in several assertions despite focused `number`/`simplify` coverage.
- `Test_getbufvar` now passes the former missing `setbufvar()` call but retains unrelated buffer creation, forced-edit option, and dictionary-member gaps.
- `Test_virtcol` no longer reports the showbreak continuation failures; its first three failures are caused by the pre-existing `:normal! 4|` cursor-motion gap.
- The only remaining E121 in the fresh harness is `Test_mode`'s mapping-driven `g:current_modes`, outside the scoped input-command residuals. No E114 remains.
