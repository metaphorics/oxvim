# Task 75, first functional-suite census

Measurement and triage only. No source changed. The only non-artifact edit is two `.gitignore`
negations so `functional-census.tsv` and `functional-blockers.md` are tracked the way the oldtest
summaries are.

## What was measured

`cargo build --release -p oxvim` with `crates/` clean at
`5a2105f269257ebc2ee5676361321afdcb11a7a2`. The binary was pinned to `/tmp/oxfunc/oxvim`
(`md5 bff1907d131d442a1806f9be90b0a16a`) together with a copy of `runtime/` exported as
`$OXVIM_RUNTIME`, because relocating the binary alone breaks `runtime_root()`
(`crates/oxvim/src/runtime.rs:109-121`). HEAD moved to `fa2870d` and `crates/ox-tui` went dirty
during the pass (Task74Tui); the pin excludes that and its md5 was re-checked after the last run.

All 484 `*_spec.lua` files under `.references/neovim/test/functional`, each alone, 20-way parallel, in
its own throwaway copy of the upstream tree with a fresh `$HOME`, `$TMPDIR`, `XDG_*` and
`NVIM_LOG_FILE`, stdin from `/dev/null`, a 90 s deadline, and process-group kill. The brief's ~752 is
`test/` as a whole: 484 functional, 37 unit, 12 benchmark.

Six passes: `A` oracle host and oracle subject (control), `B` oracle host and oxvim subject
(primary), `C` oxvim host and oxvim subject (the `just functional` shape), `D`/`E`/`F` shim probes,
plus a neutrality control for the shims.

## Which binary ran, and how that was proved

Four proofs, since the brief warns that a passing-everything result is a lie.

1. `NVIM_PRG` was pointed at a wrapper that logs its argv and `exec`s the pinned binary. One spec
   produced 5 spawn lines, each naming the oxvim path.
2. The same harness with the oracle as subject scores 9652 pass / 31 fail over the same 484 files,
   475 of them with at least one pass. The control is what "everything passes" looks like.
3. The pinned binary self-identifies as `OXVIM v0.13.0`.
4. Failure texts have no upstream analogue: `E5108: runtime error: Error(CallbackError { … })`,
   `Not implemented: <builtin>`, `oxvim: Ex command failed: …`.

`--api-info` is worthless as a proof and misleads: oxvim's payload is byte-identical to the oracle's
(32701 bytes, 262 functions, 69 UI events) while 114 of those methods answer
`API function is not implemented` at runtime. `tests/differential/apidiff.sh` would report clean.

The recorded trap is real. `RunTests.cmake:31-33` sets `ENV{NVIM_PRG}` from the cmake `-D` variable,
so an env var alone tests the oracle. This census bypasses cmake and invokes `runner.lua` directly,
which is also what allows the harness host and the subject to be pinned separately.

## Results

| | A, oracle control | B, oxvim (primary) | C, `just functional` shape |
| --- | --- | --- | --- |
| pass | 9652 | 930 | 312 |
| fail | 31 | 7842 | 7670 |
| error | 0 | 27 | 66 |
| pending | 80 | 71 | 58 |
| executed | 9683 | 8799 | 8048 |
| files with a pass | 475 | 100 | 15 |
| unfinished files | 0 | 7 | 6 |

oxvim reaches 91 % of the control's executions and passes 9.6 % of its passing tests. Outcome
distribution in B: 373 zero-pass, 84 partial, 15 clean, 7 hang, 5 no-tests. Most of the 15 clean files
are host-side Lua (`luacats_grammar` 92, `iter` 38, `harness` 48) and hardly touch the binary.

Protocol versus editor, over all 486 distinct failure signatures: protocol 47 signatures / 5281
failing tests / 320 files, editor 439 / 2588 / 301. 163 files fail only at the protocol boundary and
can express no editor opinion at all, which is why protocol comes first.

## Two findings the file counts hide

**Opening the biggest gate does not move the pass rate.** `nvim_get_color_map` gates
`Screen.new()` in 171 files (2804 tests). Pass D re-ran those 171 with the color map substituted from
the oracle: 168 → 172 passes, executions 4684 → 3941, unfinished files 3 → 14. Pass F did the same for
`nvim_exec2` over its 100 files: 152 → 177. Pass E did both: 181, with 326 executions lost to new
hangs. The shims were validated against the oracle, where 97 of 100 files reproduce their control
counts exactly. So the two largest gates convert 4 and 25 failures into passes, and the layer behind
them (screen redraw content, `Row N did not match`, 71 files / 478 tests) is the real cost.

**The shipped `just functional` path is much worse than the number suggests.** With oxvim hosting the
harness, 293 of 484 files render every failure as `<userdata 1>`: oxvim's Lua error objects are
userdata where upstream's are strings, so the reporter has nothing to print. Separately,
`vim.text.indent` is missing from oxvim's Lua stdlib, so `test/testutil.lua:728` `dedent()` throws in
99 files / 485 tests. Those 99 files score 29 passes in C against 198 in B.

## Panics and hangs

One panic. `crates/ox-editor/src/builtins/process.rs:112:14`,
`internal error: entered unreachable code`, the `_ => unreachable!()` arm of `call_job_builtin`.
Trigger: `test/functional/lua/vim_spec.lua`, `T47 lua stdlib vim.rpcrequest and vim.rpcnotify`. The
process dies and the rest of the file is lost. Same shape as the `test_unlet.vim` panic in oldtest
pass 3: unvalidated input reaching `unreachable!` where upstream raises a Vim error.

Seven hangs in B: `core/exit_spec.lua` (`:cquit`), `core/main_spec.lua` (`-s` with Ex-mode
completion), `core/startup_spec.lua` (`--startuptime`), `ex_cmds/write_spec.lua` (FIFO write),
`lua/vim_spec.lua` (the panic), `plugin/lsp_spec.lua` (LSP runtime fails to load, session never
settles), `terminal/tui_spec.lua` (hung with no test open). C adds `api/keymap_spec.lua` and
`ui/float_spec.lua`. Shimming the color map raised unfinished files from 3 to 14 among the 171 gated
specs, so every UI gate that opens exposes a hang.

## Recommendations

1. **Make Lua and API error values plain strings, and stop Debug-formatting them.** Unblocks
   diagnosis for 293 files / 3602 failures on the shipped path, removes the Rust struct dump from
   90 files / 1232 failures in the primary path, and makes the 144 spec files that assert through
   `pcall_err` able to pass at all. Reproduced in one command each:
   `pcall(vim.api.nvim_get_mode)` returns `userdata` (oracle: `string`), and
   `:lua vim.api.nvim_get_mode()` prints `E5108: runtime error: Error(CallbackError { … cause:
   ExternalError(Exception("API function is not implemented")) })`. Sites:
   `crates/ox-lua/src/vim.rs:428-433` (`lua_error_text` Debug-formats any non-string value),
   `crates/ox-lua/src/host.rs:59,70`, `crates/ox-editor/src/builtins/process.rs:330`. It converts
   about zero failures to passes by itself, and it is first because nothing else in this corpus can
   be trusted or matched until it lands.
2. **Ship `vim.text.indent`.** 99 files / 485 failures on the shipped path, worth at least the 169
   passes those files already earn when the oracle hosts the harness. `vim.text` exists and
   `.indent` is `nil`; the only consumers are `test/testutil.lua:188` and `:728`, and `dedent` is on
   the setup path of most specs through `testnvim.lua:714`. One function.
3. **Fill the 47 missing `nvim_*` dispatch entries, color map and `nvim_exec2` first.** Gating:
   171 files stop dying at `Screen.new()`, 100 at `exec()`, 37 at the `g:` variable API, 5 at
   `nvim_set_keymap` (210 tests); 160 files carry an `API function is not implemented` failure across
   33 call sites. The 47 is a count from an exhaustive dispatch probe over all 262 declared methods,
   not an estimate. Conversion is small and measured (+4, +25, +29 in D/F/E), so expect the next
   census to show executions fall and hangs rise as these land.

Ranking by file count alone would put the color map first and call it a 171-file win; pass D
falsifies that. Unlike the oldtest corpus, which was gated by `E492` and `E15`, this suite is broadly
short of behaviour: 373 of 484 files execute tests and pass none.

## Artifacts

| path | contents |
| --- | --- |
| `.outline/sdd/functional-census.tsv` | 484 rows: spec, outcome, pass/fail/error/pending, executed, oracle-control pass/fail, blocker class, first blocker, hang site, `just functional`-shape pass/fail |
| `.outline/sdd/functional-blockers.md` | ranked blockers, protocol versus editor split, top 10 expanded with upstream surfaces, the 114-method runtime API inventory, shim-probe evidence, panics and hangs, recommendations |
| `.outline/sdd/reports/task-75.md` | this report |

Per-file logs for all six passes were kept under `/tmp/t75func/logs/` and are not committed.
