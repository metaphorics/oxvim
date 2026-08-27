# Task 70, third suite-wide oldtest census

Measurement only. No source changed in this leaf; the only non-artifact edit is two negation lines in
`.gitignore` so the pass 3 summaries are tracked the way passes 1 and 2 are.

## What was measured

`cargo build -p oxvim` against a working tree with `crates/` clean at
`8ff70b1341533a26e7976104eb0030d7d14a2239` (`docs(outline): report task 69 items 1-3 and the maparg
shortfall`). `target/debug/oxvim` was copied to `/tmp/oxvim-c3-bin` and `runtime/` to
`/tmp/oxvim-c3-runtime`, exported as both `$OXVIM_RUNTIME` and `$VIMRUNTIME`. `git status` was
re-checked immediately after the copy and `crates/` was unmodified; Task71Maparg was editing
`crates/ox-editor`, `crates/ox-excmd` and `crates/ox-eval` throughout the pass and the pin excludes that
by design.

All 236 `test_*.vim` files, 8-way parallel, each in its own throwaway copy of `testdir` with a fresh
`HOME` and isolated `XDG_*`/`TMPDIR`, stdin from `/dev/null`, 150 s timeout. The base `testdir` copy had
the committed stale `test.log` stripped; every number below comes from the per-run `messages` file.

## Results against both prior censuses

| outcome | pass 3 | pass 2 | pass 1 | Δ3-2 | Δ2-1 |
| --- | --- | --- | --- | --- | --- |
| partial | 180 | 163 | 167 | **+17** | -4 |
| setup-blocked | 54 | 70 | 60 | **-16** | +10 |
| timeout | 1 | 3 | 6 | -2 | -3 |
| crash | 1 | 0 | 3 | **+1** | -3 |

| totals | pass 3 | pass 2 | pass 1 | Δ3-2 | Δ2-1 |
| --- | --- | --- | --- | --- | --- |
| executed | 3077 | 2510 | 2556 | **+567** | -46 |
| failed | 2267 | 2314 | 2339 | -47 | -25 |
| skipped | 579 | 72 | 77 | **+507** | -5 |

Tasks 61-69 bought 567 executions and cost 53, the first pass where executed moved decisively in the
right direction. Two gates collapsed: `E492` 83 → 34 files (Ex command table) and `E15` 46 → 7 files
(expression parser). Nineteen files moved out of `setup-blocked`/`timeout` into `partial`, led by
`test_normal.vim` (0 → 121 executed), `test_options.vim` (0 → 89) and `test_window_cmd.vim` (0 → 73).
Pass 2's D1 (`E444`, `:quit` on a tabpage) fell 12 files → 1 and its D3 (`test_assert.vim` silent exit)
is gone.

Every top-10 blocker except `E121` and `E492` *grew*. That is depth reached, not new breakage: with the
two gates open, files run further and meet the next wall. The four exceptions are itemised as D1-D4 in
`oldtest-blockers-3.md`.

## Skip analysis

`skipped` went 72 → 579. Split by per-file arithmetic against `oldtest-census-2.tsv` (a skipped test
still increments `s:done`, so failure → skip leaves `executed` flat):

| movement | tests |
| --- | --- |
| failure → skip | **384** |
| newly executing | 620 |
| **lost executions** | **53** (3 files, all named) |
| skips inside newly executing files | 88 |
| skips inside the lost-execution files | 31 |
| skip replacing a pass | 4 |

`388 + 88 + 31 = 507`, reconciling exactly. So 384 tests converted failure → skip against 53 lost
executions, and no attrition is unexplained.

Classifying all 579 skip reasons: 403 are **oracle-parity** (the oracle nvim skips them too — 265 of
those route through `CanRunVimInTerminal()`, and the oracle reports `has('terminal') = 0`), 99 are
capabilities oxvim genuinely lacks (quickfix alone is 67), 24 are single missing functions or
Nvim-specific N/A, and **53 are under-claims**: `has(<feat>)` returns 0 while the option/command/function
surface answers `exists()` with 1. The clearest are `autocmd` (2 skips, oxvim has
`crates/ox-editor/src/autocmd.rs`) and `folding` (6 skips, Task 59 built folds); then `rightleft` 11,
`vartabs` 10, `persistent_undo` 6, `conceal` 5. Those are tests the suite would run if `has()` told the
truth — hidden coverage, not absent capability.

At file level the same effect is larger. 33 of the 54 `setup-blocked` files are honest whole-file skips:
31 of them were `E15` in pass 2 because `CheckFeature`'s own `throw 'Skipped: ' .. a:name .. …` could not
be evaluated, so the file died inside its skip guard. Only **21** files are genuinely blocked before
their first test.

## The two regressions

**D1, `test_cmdline.vim`, 45 executed → 0, the largest single loss.**
`Test_shellcmd_completion()` sets `$PATH` to `<cwd>/Xpathdir` (`test_cmdline.vim:860`), throws at
`getcompletion()` (line 866, `E117`), and never reaches its restore at line 871. `let $PATH` now mutates
the process environment — pass 2's D4 export defect was fixed, which is what makes this reachable — so
`Delete_Xtest_Files()`'s `call system('rm -rf  ' .. file)` (`runtest.vim:472`) cannot find a shell,
`Command::output()` fails `ENOENT`, and `crates/ox-editor/src/builtins/process.rs:106` raises a fatal
`E677`. `messages` is never written and oxvim drops buffered stdout on the fatal exit path, so the row
reads `0 0 0` with an empty log. With `Delete_Xtest_Files()` wrapped in `try`/`catch` in a scratch
`runtest.vim`, the same invocation reports `PROBE PATH=…/Xpathdir`, `Executed 45 tests`, `39 FAILED:` —
the file actually *improved* on pass 2's 45/46/0 and the census records nothing. A fix made a
pre-existing hazard fatal.

**D2, `test_unlet.vim`, the only panic.** `rc = 101`,
`panicked at library/std/src/env.rs:433:29: failed to remove environment variable ``: Invalid argument`.
`remove_target()` (`crates/ox-editor/src/excmd_exec.rs:5905`) passes the unvalidated name to
`ox_sys::unset_env` (`crates/ox-sys/src/lib.rs:35-37`) → `std::env::remove_var("")`. Trigger is
`unlet $` at `test_unlet.vim:23`; upstream `do_unlet_var` raises `E475`. Same panic is reachable for any
name containing `=` or NUL, and the assignment path
(`crates/ox-eval/src/builtins.rs:1275-1279`) shares it. 7 executed → 0.

## Other findings

- **One flag letter gates 39 files.** `E5060` rose to rank 3 because `writefile()` in oxvim accepts only
  `b a s S` (`crates/ox-editor/src/fs_builtins.rs:204-205`) where upstream accepts `b a D s S p`
  (`.references/neovim/src/nvim/eval/fs.c:1840-1859`). All 39 files fail as `Unknown flag: D`. Cheapest
  large win in the census.
- **`E523` is a label, not a defect class.** `error_flow(runtime, "E523", error.to_string())` at
  `crates/ox-editor/src/excmd_exec.rs:496`, `507`, `559`, `2590`, `2606` wraps any error escaping the
  Normal-mode/typeahead machinery. Observed texts include `E523: no previous search pattern` (upstream
  `E35`) and `E523: register text must be valid UTF-8` (no upstream analogue). Rank 9 hides at least two
  upstream error identities, and each leaked internal string is its own parity defect.
- **Correction to pass 2.** `oldtest-blockers-2.md` credited 53 `not implemented:` symbols as resolved.
  At least 16 were never implemented — `mode`, `settabvar`, `settabwinvar`, `tabpagewinnr`,
  `timer_start`, `histadd`, `winlayout`, `winsize`, `readdir`, `stop`, `suspend`, `copy`, `dlist`,
  `drop`, `tab`, `nvim_get_hl` — they read zero because the only files reaching them aborted at `E444`,
  and they are back now that those files run. `call mode()` on the pinned binary still returns
  `not implemented: mode`. Pass 3's own zeros carry the same caveat: a blocker count of zero means "not
  observed".
- **`test_system.vim` hang localised.** The only timeout, and 155 s of the census's 277 s.
  `system('cat', '123')` (`test_system.vim:20`) leaves `/bin/sh -c cat` blocked on an unclosed stdin
  pipe; `timeout` kills oxvim but not the grandchild, so a runner reading stdout through a pipe hangs
  past the deadline. The job branch does write and close input
  (`crates/ox-editor/src/builtins/process.rs:139-141`); the plain `Command::output()` branch at 105-106
  ignores the input argument.

## Method notes worth keeping

- Relocating the binary without the runtime tree breaks `runtime_root()`
  (`crates/oxvim/src/runtime.rs:109-121`). Pass 2 lost a whole pass to this; pass 3 pinned both.
- Blocker counts were validated by recomputing passes 1 and 2 from their retained logs with the pass 3
  extractor, reproducing their published figures exactly (`E117` 131/115, `E492` 87/83, `E15` 43/46,
  `E605` 41/45, `E121` 30/36, `E444` 0/12). The three columns are directly comparable.
- oxvim's throwpoint line numbers are wrong by a constant offset inside function bodies: D1's `E677` is
  reported at `Delete_Xtest_Files[9]` where the raising call is body line 12. Do not trust `[n]`.
- Reading a file's record through a pipe is unsafe for `test_system.vim`; that one file needs
  `start_new_session` plus a process-group kill.

## Artifacts

| path | contents |
| --- | --- |
| `.outline/sdd/oldtest-census-3.tsv` | 236 rows: name, outcome, executed, failed, skipped, first_blocker |
| `.outline/sdd/oldtest-blockers-3.md` | ranked blockers with a three-way delta, top 10 expanded, skip analysis, D1-D4 |
| `.outline/sdd/census-3/*.log` | 236 per-file logs, ignored by `.gitignore`, not force-added |
