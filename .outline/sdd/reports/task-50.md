# Task 50 — oldtest suite-wide census (leaf E)

Measurement only. No source changes.

## What was run

Every one of the 236 `test_*.vim` files in `.references/neovim/test/old/testdir` was executed against
`target/debug/oxvim` (HEAD `b05106f`), each in its own throwaway copy of `testdir` under `/tmp` with
isolated `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` / `TMPDIR`, 8-way parallel:

```
timeout 120 target/debug/oxvim -u NONE -i NONE --noplugin --headless \
  --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim <FILE>
```

## Correction to the predecessor's pass

The 152 logs inherited from the killed predecessor classified 128 files as `timeout`. That was an
artifact of the runner, not of oxvim: when the child inherits an open stdin, the headless process
blocks instead of exiting at `runtest.vim`'s final `qall!`. Proof (same file, same env, only stdin
differs):

```
inherit: (130.01, 124)     # rc 124 = killed by timeout
devnull: (1.65, 0)         # rc 0
```

Because that defect changed the recorded outcome class for a majority of files, the inherited logs
were not reusable. All 236 files were re-run with `stdin=/dev/null`; timeouts dropped from 202 to 6.
Every log in `.outline/sdd/census/` is from the corrected pass.

## Results

| outcome | files |
| --- | --- |
| partial | 167 |
| setup-blocked | 60 |
| timeout | 6 |
| crash | 3 |
| full-pass | 0 |

2556 tests executed suite-wide, 2339 with errors, 77 self-skipped. Two files ran to completion with
zero failures (`partial`, `failed = 0`).

`full-pass` is structurally unreachable under this invocation: upstream `runtest.vim` never writes
`test.res` — it is a Makefile marker — so `res_exists` is false for every file. The census records
this rather than inventing a pass class.

## Top blockers by file count

| blocker | files gated |
| --- | --- |
| `E117` (unimplemented builtin) | 131 |
| `E492` (not an editor command) | 87 |
| `E15` (invalid expression) | 43 |
| `E605` (exception not caught) | 41 |
| `E121` (undefined variable) | 30 |

200 distinct `not implemented: <symbol>` names appear across the suite; head of the distribution is
flat (`cursor` 25, `redraw` 21, `tabnew` 9, `assert_beeps` 9), so no single builtin unlocks a large
block of files.

Three hard aborts (returncode 101):

| file | panic site | failure |
| --- | --- | --- |
| `test_assert.vim` | `crates/ox-editor/src/excmd_exec.rs:1786:31` | index out of bounds |
| `test_visual.vim` | `crates/ox-editor/src/mode.rs:353:150` | index out of bounds |
| `test_window_cmd.vim` | `crates/ox-editor/src/layout.rs:1376:31` | unreachable: split path descends only through containers |

## Artifacts

| path | contents |
| --- | --- |
| `.outline/sdd/oldtest-census.tsv` | 236 rows (name, outcome, executed, failed, skipped, first_blocker) |
| `.outline/sdd/oldtest-blockers.md` | ranked blocker table, top 10 expanded with upstream surfaces |
| `.outline/sdd/census/*.log` | 236 per-file logs |

## Gate evidence

`gate_check.py` is not installed on this machine. The
`/home/alpha/.claude/plugins/cache/odin-marketplace/` tree does not exist, and
`find /home/alpha/.claude /home/alpha/.omp -name gate_check.py` returns nothing (a filesystem-wide
`find /` was also attempted and hit a 300 s timeout without a hit), so the script's output cannot be
produced. The GATES.md CHECK commands were run verbatim instead:

```
G1: 236
G2: COVERED
G3: 156      (grep -c '^|'; 61 real table rows, this grep treats '^|' as an alternation)
G4: 236
G5: 0        (before this commit; G5 is Main's push gate)
```

G1–G4 satisfy their EXPECT patterns. G5 will read 1 until Main pushes.
