# Task 48: window, screen, and process builtin surface

## Status

Complete for leaf-C gates G1-G5. The evaluator now implements `matchstrlist()`. The editor host implements `systemlist()` through the existing reactor-backed job channels, including shell-string and argv-list command forms, `keepempty`, buffered stdout, and `v:shell_error`. Window IDs/dimensions, headless screen-cell queries, `getbufvar()`, `fullcommand()`, `eventhandler()`, `:resize`, `:wincmd`, and `:echohl` are recognized and backed by existing editor state.

G6 remains Main's publication gate, as assigned.

## Commits

- `782ce3e fix(eval): implement matchstrlist builtin`
- `a44d107 fix(editor): capture systemlist through job channels`
- `6931d26 fix(editor): add window and screen builtin surface`
- `ade028b fix(editor): align list target and comment semantics`

## Verification

- `cargo nextest run -p ox-editor -p ox-eval`: **954 passed, 0 skipped**.
- `cargo nextest run -p ox-editor -E 'test(/systemlist|system/)'`: **4 passed, 565 skipped**.
- Final fresh unfiltered `test_functions.vim`: **110 executed, 34 passed, 74 failed, 2 skipped**.
- Scoped E117 not-implemented matches: **0**.
- Process-level `Ex command failed` aborts: **0**.
- G1-G5: **PASS**.

## Final gate checker output

```text
  FAIL G6: all commits for this leaf pushed to origin/main
       5
.outline/GATES.md: 6 gates
UNMET: 1 (met: 5)
```

## Concerns

- The valid-cell branch of `Test_screen_functions` remains blocked by the pre-existing missing `v:progpath`; direct unit coverage proves Unicode cell/attribute/string shapes and invalid-coordinate behavior.
- `Test_getbufvar` reaches the new builtin but later stops at the out-of-scope missing `setbufvar()` builtin. Its current-buffer variable and option paths have focused coverage.
- `matchstrlist()` now passes its basic list/result contract; the aggregate oldtest still reports two regex-engine limitations (`\k` Unicode matching and malformed `\@=` rejection).
- `:echohl` is accepted and preserves message output, but the current editor `Message` model has no highlight-attribute field; carrying the resolved group into UI chunks requires a cross-crate `ox-editor`/`oxvim` message contract change outside this leaf's ownership.
- List-target destructuring and inline expression comments now close the measured E121/E114 evaluator gaps (E121 occurrences fell from 8 to 6; E114 from 3 to 1). Remaining E739 entries are stale fixture directories left by earlier failing tests; remaining E121/E114 entries are downstream input-command/startup-variable and unrelated garbage-collection-function gaps.
