# Functional-suite blockers, first census (task 75)

Measurement and triage only. No source changed. The only non-artifact edit is two `.gitignore`
negations so this file and `functional-census.tsv` are tracked the way the oldtest summaries are.

## 1. What ran, and the proof it was oxvim

`crates/` was clean at `5a2105f269257ebc2ee5676361321afdcb11a7a2` when the binary was built with
`cargo build --release -p oxvim`. `target/release/oxvim` was copied to `/tmp/oxfunc/oxvim`
(`md5 bff1907d131d442a1806f9be90b0a16a`) and `runtime/` to a pinned copy exported as
`$OXVIM_RUNTIME`. Relocating the binary alone breaks `runtime_root()`
(`crates/oxvim/src/runtime.rs:109-121`), so both were pinned. Later in the pass `target/release/oxvim`
changed under me — HEAD moved to `fa2870d` and `crates/ox-tui` went dirty (Task74Tui) — and the pinned
copy is what every number below came from; its md5 was re-verified after the last pass.

Four independent proofs that oxvim, not the oracle, was under test:

1. **argv shim.** `NVIM_PRG` was pointed at a shell wrapper that appends its argv to a log and
   `exec`s the pinned binary. Running `core/exit_spec.lua` produced 5 log lines, each
   `SPAWN /tmp/oxfunc/shim.sh -> /tmp/oxfunc/build/bin/oxvim argv:-u NONE -i NONE --cmd set
   runtimepath^=…`. The suite does spawn `$NVIM_PRG`, once per `clear()`.
2. **Oracle control passes.** The same harness, same scratch tree, with `NVIM_PRG` = the oracle
   gives 9652 pass / 31 fail / 80 skip over 484 files, 475 of 484 files with at least one pass.
   A passing-everything result is what the control produces and what oxvim does not.
3. **Self-identification.** The pinned binary prints `OXVIM v0.13.0`.
4. **Failure texts are oxvim's.** `E5108: runtime error: Error(CallbackError { … })`,
   `Not implemented: <builtin>`, `oxvim: Ex command failed: …` have no upstream analogue.

`--api-info` is **not** a usable proof, and it misleads: oxvim's `--api-info` payload is
**byte-identical** to the oracle's (32701 bytes, 262 functions, 69 UI events). `tests/differential/apidiff.sh`
would report clean parity while 114 of those 262 methods are not dispatched at runtime (§5).

### Harness host, a chosen deviation from the recipe

`RunTests.cmake:85` runs the harness itself as `${NVIM_PRG} -l test/runner.lua`, so the `just functional`
recipe uses one binary for both the harness host and the instance under test. Two measurements were taken:

| pass | harness host | `$NVIM_PRG` | what it measures |
| --- | --- | --- | --- |
| **A** | oracle | oracle | control: is the scratch tree sound |
| **B** | oracle | **oxvim** | **primary**: oxvim's API/UI/RPC surface, isolated |
| **C** | **oxvim** | **oxvim** | the shipped `just functional` path |

B is the primary census because C destroys the diagnostics: with oxvim hosting the harness,
293 of 484 files render every failure as `<userdata 1>` (§7, defect P1), so the run produces counts
and no cause. Where B says `E121: Undefined variable: v:exiting`, C says `<userdata 1>`.

`RunTests.cmake:31-33` sets `ENV{NVIM_PRG}` from the cmake `-D` variable, so the recorded trap is real:
exporting `NVIM_PRG` alone silently tests the oracle. This census bypasses cmake and invokes
`runner.lua` directly, which is what lets host and subject be pinned separately.

### Method

484 `*_spec.lua` files under `test/functional` (the brief's ~752 counts `test/` as a whole:
484 functional + 37 unit + 12 benchmark). Each file ran alone, 20-way parallel, in its own throwaway
tree: private copy of `root/` (`test`, `src`, `runtime`, `contrib`, `cmake`, top-level files) and
`build/` with a per-run `paths.lua`, a fresh `$HOME`, `$TMPDIR`, `XDG_*`, `NVIM_LOG_FILE`,
stdin from `/dev/null`, 90 s deadline, killed by process group (killing only the direct child leaks
grandchildren that hold the stdout pipe open forever). Env matches `RunTests.cmake`: `NVIM_TEST=1`,
`LC_ALL=en_US.UTF-8`, `HISTFILE=/dev/null`, `SHELL=sh`, `SYSTEM_NAME=Linux`, and `XDG_DATA_DIRS`,
`NVIM`, `TMUX`, `VIM`, `VIMRUNTIME` unset.

The scratch tree needed two things the oracle build does not ship: the six fixture binaries
(`tty-test`, `shell-test`, `pwsh-test`, `printargs-test`, `printenv-test`, `streams-test`, compiled
by hand against `.deps/usr/lib/libuv.a` — CMake marks them `EXCLUDE_FROM_ALL` and nobody built them)
and `build/lib/nvim/parser/*.so`. Without them the control loses 459 tests to
`'…/tty-test' is not executable` and `No parser for language "vim"`, and those failures are not oxvim's.
Adding them took the control from 490 failures to 31.

## 2. Headline numbers

| | pass A, oracle control | pass B, oxvim (primary) | pass C, `just functional` shape |
| --- | --- | --- | --- |
| files | 484 | 484 | 484 |
| pass | **9652** | **930** | **312** |
| fail | 31 | 7842 | 7670 |
| error | 0 | 27 | 66 |
| pending (skip) | 80 | 71 | 58 |
| executed | 9683 | 8799 | 8048 |
| files with ≥1 pass | 475 | **100** | **15** |
| files that did not finish | 0 | 7 | 6 |

oxvim reaches **91 %** of the control's executions (8799 / 9683) and passes **9.6 %** of the
control's passing tests (930 / 9652). Depth is not the problem; correctness is.

Outcome distribution, pass B:

| outcome | files |
| --- | --- |
| zero-pass (ran, nothing passed) | **373** |
| partial (some pass, some fail) | 84 |
| clean (no failures) | 15 |
| hang (deadline, file truncated) | 7 |
| no-tests (whole file skipped/empty) | 5 |

The 15 clean files are almost all host-side: `script/luacats_grammar_spec.lua` (92),
`lua/iter_spec.lua` (38), `lua/glob_spec.lua` (10), `lua/text_spec.lua` (6),
`lua/func_memoize_spec.lua` (6), `harness/harness_spec.lua` (48) — these exercise the harness or pure
Lua and hardly touch the binary. Files that drive oxvim and pass everything:
`ex_cmds/quit_spec.lua` (1), `options/modified_spec.lua` (1), `options/tabstop_spec.lua` (1),
`vimscript/modeline_spec.lua` (1), `vimscript/operators_spec.lua` (3), `lua/list_spec.lua` (3),
`lua/ssh_spec.lua` (3), `harness/assert_spec.lua` (3).

Control contamination is negligible: 7 signatures appear in both A and B, worth 31 tests over 17
files, and `functional-census.tsv` carries `ctl_pass`/`ctl_fail` per row so any claim can be checked
against the oracle on the same file.

## 3. Protocol-level versus editor-level: the split that decides ordering

Classification rule, applied mechanically to all 486 distinct failure signatures:

**protocol** = the RPC/API/UI-event boundary itself misbehaves — `API function is not implemented`,
`Wrong number of arguments`/`Wrong type for argument` on an `nvim_*` method, missing
`nvim_get_color_map` inside `Screen`, the `CallbackError`/`<userdata>` error-marshalling leaks,
`Nvim EOF (crash?)`, `Expected screen height … differs`, and the RPC-semantics builtins
(`serverstart`, `serverlist`, `rpcrequest`, `rpcnotify`, `jobstart`, `chansend`, `sockconnect`,
`api_info`).

**editor** = the boundary works and the answer is wrong — Vim `E###` errors, missing Vimscript
builtins and Ex commands, `assert eq`/pattern mismatches, `expected failure, but got success`,
screen *content* mismatches (`Row N did not match`).

| class | distinct signatures | failing tests | files touched | files touched *only* by this class |
| --- | --- | --- | --- | --- |
| **protocol** | 47 | **5281** | 320 | **163** |
| **editor** | 439 | 2588 | 301 | 144 |

Protocol failures are 2× editor failures by test count out of 1/9th as many distinct causes. 163
files fail *exclusively* at the protocol boundary and cannot express an editor opinion at all.
So protocol-level work comes first — but §6 shows the leverage is not where the file counts suggest.

`Row N did not match` (71 files / 478 tests once the color map is shimmed) is deliberately filed as
editor-level: the UI events arrive and a grid is assembled, the cells are wrong. It is the one
ambiguous bucket in the table.

## 4. Ranked blockers

Ranked by how many spec files carry at least one failure with the signature; `tests` is the number of
individual test cases with it. 486 distinct signatures in all; the top 25:

| files | tests | class | signature |
| --- | --- | --- | --- |
| 171 | 2804 | protocol | `screen: nvim_get_color_map not implemented` |
| 119 | 692 | editor | `assert eq: values differ` |
| 100 | 427 | protocol | `API not implemented via nvim_exec2` |
| 90 | 1232 | protocol | `Error(CallbackError { … })` leaked as error text (any position) |
| 34 | 579 | protocol | `API not implemented via nvim_set_var` |
| 32 | 158 | protocol | `API not implemented via exec_lua` (nested API inside `nvim_exec_lua`) |
| 32 | 61 | editor | `expected failure, but got success` |
| 24 | 48 | editor | `assert matches: pattern mismatch` |
| 14 | 114 | editor | `E117: Function is not implemented: stdpath` |
| 10 | 18 | editor | `not implemented: terminal` (`:terminal`) |
| 10 | 13 | editor | `E126: Missing :endfunction` |
| 9 | 26 | editor | `Not implemented: system` |
| 8 | 17 | editor | `E54: regular-expression engine is not installed` |
| 8 | 14 | editor | `E37: No write since last change` |
| 8 | 11 | editor | `retry() attempts: N` (condition never became true) |
| 7 | 31 | editor | `Not implemented: luaeval` |
| 7 | 23 | editor | `Not implemented: setline` |
| 7 | 15 | protocol | `API not implemented via nvim_set_current_dir` |
| 7 | 14 | editor | `Not implemented: bufnr` |
| 7 | 12 | editor | `Not implemented: execute` |
| 6 | 21 | editor | `Not implemented: tabpagenr` |
| 6 | 13 | editor | `not implemented: file` (`:file`) |
| 6 | 8 | editor | `not implemented: !` (`:!`) |
| 6 | 6 | protocol | `Not implemented: jobstart` |
| 5 | 210 | protocol | `API not implemented via nvim_set_keymap` |

The `CallbackError` row is counted by "appears anywhere in the failure body" (90 files / 1232 tests)
rather than "is the first line" (50 / 762), because it also corrupts *matching*: 144 spec files use
`pcall_err`, which compares the error string, so a wrong string fails the test even when the
underlying behaviour is right.

Long tails behind the single-signature rows, aggregated:

| class | distinct symbols | tests | files |
| --- | --- | --- | --- |
| missing Vimscript builtins (`Not implemented: X`) | 93 | 576 | 102 |
| missing Ex commands (`not implemented: X`) | 75 | 421 | 89 |
| Vim `E###` errors | 33 | 621 | 146 |
| `API function is not implemented` at 33 distinct call sites | 33 | 1618 | 160 |

Top missing builtins by tests: `hlID` 59, `serverstart` 52, `luaeval` 31, `stdpath` 29, `system` 26,
`setline` 23, `tabpagenr` 21, `bufnr` 14, `serverlist` 14, `msgpack` 14, `execute` 12, `expand` 12,
`systemlist` 12, `msgpackparse` 11.
Top missing Ex commands: `packadd` 44, `getreg` 34, `windo` 34, `tabnext` 32, `sort` 29, `diffthis` 19,
`terminal` 18, `tabprevious` 13, `file` 13, `move` 11, `checkhealth` 11, `clearjumps` 10, `!` 8.
Top `E###`: `E117` 200 tests / 38 files, `E121` 150 / 39, `E216` 45 / 4, `E484` 27 / 7, `E488` 26 / 10,
`E474` 22 / 14, `E523` 17 / 8, `E54` 17 / 8, `E492` 16 / 11, `E37` 14 / 8, `E126` 13 / 10.

## 5. The runtime API inventory (protocol-level, exact)

Because `--api-info` is identical to the oracle's, the real surface was measured by dispatch probe:
the oracle spawns `oxvim --embed --headless`, calls all 262 declared methods over RPC with no
arguments, and classifies the reply. `API function is not implemented` means the dispatch table has
no entry; `Wrong number of arguments`/`expects (…)` means the entry exists and argument checking was
reached; a value means it ran.

| probe result | methods |
| --- | --- |
| dispatched (arity/type check reached) | 128 |
| dispatched and returned a value with no args | 11 |
| dispatched, custom signature message | 9 |
| **`API function is not implemented`** | **114** |

Of the 114, **47 are `nvim_*`** and 67 are deprecated `buffer_*`/`window_*`/`tabpage_*`/`vim_*`
aliases. The fallback is `crates/ox-api/src/registry.rs:133`.

The 47 missing `nvim_*` methods:

```
nvim_buf_del_keymap        nvim_buf_del_mark         nvim_buf_del_user_command
nvim_buf_get_commands      nvim_buf_get_keymap       nvim_buf_get_mark
nvim_buf_set_keymap        nvim_buf_set_mark         nvim_call_dict_function
nvim_create_user_command   nvim_del_current_line     nvim_del_keymap
nvim_del_mark              nvim_del_user_command     nvim_del_var
nvim_eval_statusline       nvim_exec2                nvim_get_all_options_info
nvim_get_color_by_name     nvim_get_color_map        nvim_get_commands
nvim_get_current_line      nvim_get_keymap           nvim_get_mark
nvim_get_mode              nvim_get_option_info2     nvim_get_proc
nvim_get_proc_children     nvim_get_var              nvim_input_mouse
nvim_open_tabpage          nvim_parse_cmd            nvim_parse_expression
nvim_set_current_dir       nvim_set_current_line     nvim_set_decoration_provider
nvim_set_keymap            nvim_set_var              nvim_ui_pum_set_bounds
nvim_ui_pum_set_height     nvim_ui_send              nvim_ui_set_focus
nvim_ui_set_option         nvim_ui_term_event        nvim_ui_try_resize_grid
nvim_win_resize            nvim_win_text_height
```

Separately, 10 files / 15 tests fail on *shape*, not absence — a dispatched method rejects arguments
the oracle accepts. Most frequent: `nvim_create_augroup` (5 tests, `Wrong number of arguments:
expecting 2 but got 1`). That one is load-bearing: `runtime/lua/vim/_core/ui2.lua:60` calls
`nvim_create_augroup` at require time, so the default UI Lua layer fails to load. Others:
`nvim_buf_set_keymap`, `nvim_get_context`, `nvim_put`, `nvim_set_current_dir`, `nvim_win_set_cursor`,
`nvim_buf_get_lines` (`expecting Boolean`), `nvim_echo`, `nvim_create_buf` (`expecting Boolean`).

## 6. Top 10 expanded, with the upstream surface each needs — and the leverage evidence

### 6.1 `nvim_get_color_map` (171 files, 2804 tests, protocol)

`test/functional/ui/screen.lua:107-112` calls `nvim_get_color_map` in `_init_colors()`, which runs on
the first `Screen.new()`. It fails, `error('failed to get color map')`, and every `Screen`-based test
in the file dies before attaching a UI. Upstream surface: `src/nvim/api/vim.c:1486`
`DictOf(Integer) nvim_get_color_map(Arena *)`, which just walks
`color_name_table[]` (`src/nvim/highlight_group.c:2457`, ~700 entries). Sibling
`nvim_get_color_by_name` (`src/nvim/api/vim.c:1461`) is missing too.

**Measured leverage, not assumed.** Pass D re-ran the same 171 files against oxvim with one line of
`screen.lua` changed: on failure, substitute the oracle's color map (dumped from
`nvim_get_color_map`) instead of raising.

| the 171 color-map-gated files | oracle (A) | oxvim (B) | oxvim + color map (D) |
| --- | --- | --- | --- |
| pass | 5018 | 168 | **172** |
| fail | 12 | 4508 | 3764 |
| executed | 5030 | 4684 | **3941** |
| files with ≥1 pass | 171 | 26 | 35 |
| files that did not finish | 0 | 3 | **14** |

Opening the gate buys **+4 passing tests** and **loses 743 executions** to 11 new hangs. It is a
prerequisite, not a win: behind it sit `nvim_exec2` (79 of the 171 files), `Row N did not match`
(71 files / 478 tests — the redraw content layer), `assert eq` (51 files), `Not implemented: jobstart`
(19 files). Anyone who reads the 171 as "one function unblocks 35 % of the suite" will be wrong.

### 6.2 `assert eq: values differ` (119 files, 692 tests, editor)

Not a blocker; the residue of everything else. `t.eq()` mismatches with no shared cause, spread over
119 files. It also occurs 7 times in the oracle control, so a few are harness/environment noise.
Needs per-case triage, has no single upstream surface, and is listed only so the ranking is honest
about what the second-largest bucket is.

### 6.3 `nvim_exec2` (100 files, 427 tests, protocol)

`testnvim.lua:900-908`: `M.exec()` and `M.exec_capture()` are `nvim_exec2(code, {})` and
`nvim_exec2(code, {output=true}).output`, and `M.source()` (line 713) funnels into `M.exec`. Every
spec that sets up state with a multi-line Vimscript block goes through it. Upstream surface:
`src/nvim/api/vimscript.c:54` `Dict nvim_exec2(uint64_t channel_id, String src, Dict(exec_opts) *opts,
Error *err)` — source a string, optionally capturing output.

**Measured leverage.** Pass F re-ran the same 100 files with `exec`/`exec_capture` rerouted through
`nvim_command('source <tmpfile>')` (and `:redir` for the capturing form). Pass E did that *and* the
color-map shim, so each part is isolated: D = colour map only, F = exec2 only, E = both.

| the 100 `nvim_exec2` files | oracle (A) | oxvim (B) | + exec2 only (F) | + exec2 and colour map (E) |
| --- | --- | --- | --- | --- |
| pass | 2902 | 152 | **177** | **181** |
| fail | 6 | 2562 | 2591 | 2208 |
| executed | 2908 | 2727 | 2780 | 2401 |
| files with ≥1 pass | 99 | 30 | 34 | 33 |
| did not finish | 0 | 0 | 1 | 5 |

The shim is behaviour-neutral where the real API exists: run against the **oracle**, 97 of 100 files
reproduce their unshimmed pass/fail counts exactly. The 3 that do not (`ex_cmds/echo_spec.lua` 41→33,
`ex_cmds/verbose_spec.lua` 80→79, `legacy/assert_spec.lua` 17→16, −10 tests total) are output-capture
tests where `:redir` is not equivalent to `nvim_exec2`'s capture, so the F/E columns understate by at
most ~10.

`nvim_exec2` gates 100 files and, once open, converts **25** of 2756 possible failures into passes.

### 6.4 Lua error marshalling (90 files / 1232 tests in B, 293 files / 3602 tests in C, protocol)

Two linked defects, both reproducible in one command.

**P1, the error object is userdata, not a string.**
`oxvim --headless -c 'lua local ok,e = pcall(vim.api.nvim_get_mode); print(type(e))'` prints
`userdata`. The oracle prints `string`. 144 spec files use `pcall_err`, which asserts on the error
*string*; all of them are wrong by construction. And when oxvim hosts the harness (pass C), the
reporter has nothing to print: **293 of 484 files** emit `<userdata 1>` for **3602** failures.
Upstream converts API errors to Lua strings before they reach `pcall`.

**P2, non-string error values are Debug-formatted.**
`oxvim --headless -c 'lua vim.api.nvim_get_mode()'` prints
`Vim(lua):E5108: runtime error: Error(CallbackError { traceback: "…", cause: CallbackError { …,
cause: ExternalError(Exception("API function is not implemented")) } })`.
The innermost message is right there and is thrown away. Site:
`crates/ox-lua/src/vim.rs:428-433`, `lua_error_text` — `Value::String` is decoded, everything else is
`format!("{other:?}")`. The result flows through `ExecError::Runtime`
(`crates/ox-lua/src/host.rs:59`, `70`) into `lua_error_flow`
(`crates/ox-editor/src/excmd_exec.rs:7156-7163`) as `E5108`. A second Debug leak of the same shape is
`crates/ox-editor/src/builtins/process.rs:330`, `format!("{error:?}")` on a job-callback error.

Fixing P2 alone would not make a single assertion pass, and fixing P1 might not either — but until
they are fixed the `just functional` path yields 293 files of undiagnosable output, and 1232 failures
in the primary path carry a Rust struct dump where the test expects an upstream message.

### 6.5 `nvim_set_var`, `nvim_get_var`, `nvim_del_var` (37 files, 592 tests, protocol)

`src/nvim/api/vim.c:757` `void nvim_set_var(String name, Object value, Error *err)`; the trio writes
`globvardict` through `dict_set_var`. oxvim dispatches none of them, though `nvim_eval` and
`nvim_call_function` are dispatched, so the underlying variable store is reachable. This is a
registry gap, not a missing subsystem.

### 6.6 `exec_lua` (32 files, 158 tests, protocol)

`nvim_exec_lua` *is* dispatched (it answers `expects (String, Array)`). These failures are nested:
Lua sent to the child calls a missing API and the failure surfaces at the `exec_lua` frame. The
innermost causes are `API function is not implemented` (18 files / 132 hits), `Not implemented:
nvim_buf_call` (5), `Not implemented: nvim_win_call` (2), `Not implemented: luaeval` (2). Cross-check
by call site: `nvim_set_option_value` 74 hits, `nvim_set_keymap` 54, `nvim_buf_call` 24,
`nvim_create_user_command` 22, `nvim_get_all_options_info` 16, `nvim_buf_set_keymap` 12,
`nvim_create_augroup` 11, `nvim_win_call` 8, `nvim_parse_cmd` 8. Fixing §5's list fixes this row.

### 6.7 `expected failure, but got success` (32 files, 61 tests, editor)

oxvim accepts input the oracle rejects: the test asserts an error and gets none. Error *parity*,
not error *presence*. Appears once in the control, so ~60 of 61 are real. No single surface; each
case names its own upstream validation.

### 6.8 `E117: Function is not implemented: stdpath` (14 files, 114 tests, editor)

Upstream `f_stdpath` at `src/nvim/eval/funcs.c:7011`, over `src/nvim/os/stdpaths.c`. A further 5
files / 29 tests hit the same gap as `Not implemented: stdpath` from the Ex path. Note
`vimscript/executable_spec.lua` asserts `stdpath('data')` matches
`build/Xtest_xdg[%w_]*/share/nvim%-data`, so the XDG plumbing has to be right, not just the name.
Task78Bridge has claimed `crates/ox-eval/` and named `stdpath` in its list.

### 6.9 `not implemented: terminal` (10 files, 18 tests, editor)

`:terminal`. Upstream: `src/nvim/terminal.c` plus `nvim_open_term`. Note also
`E216: No such group or event: nvim.terminal` (4 files / 45 tests): the runtime's terminal autocmd
group does not exist, a distinct symptom of the same hole. The 16 `terminal/*_spec.lua` files also
need the `tty-test` fixture, which no build in this checkout produces.

### 6.10 `E126: Missing :endfunction` (10 files, 13 tests, editor)

oxvim's `:function` parser loses the body when the definition arrives inside a sourced or `exec`'d
multi-line block. Since almost every spec sets up state that way, this is the parser, not the
command: the same text at top level works in the oldtest corpus.

## 7. Panics and hangs

**One panic**, and it kills the file.

`crates/ox-editor/src/builtins/process.rs:112:14`, `internal error: entered unreachable code`,
in `call_job_builtin`'s `_ => unreachable!()` arm — a name is routed to the job-builtin dispatcher
that the `match` above does not handle. Trigger: `test/functional/lua/vim_spec.lua`, test
`T47 lua stdlib vim.rpcrequest and vim.rpcnotify`. The process dies, the remaining tests in the file
are never run, and the file is recorded as a hang because the harness host then waits on a dead
channel. Same shape as the `test_unlet.vim` panic in oldtest pass 3: an unvalidated input reaching a
`panic!`/`unreachable!` where upstream raises a Vim error.

**Seven hangs in pass B** (90 s deadline, killed by process group; each loses the rest of its file):

| spec | hung at |
| --- | --- |
| `core/exit_spec.lua` | `T5 :cquit exits with non-zero after :cquit` (11 of 15 tests lost) |
| `core/main_spec.lua` | `T3 command-line option -s does not crash when running completion in Ex mode` |
| `core/startup_spec.lua` | `T4 startup --startuptime does not crash on error #31125` |
| `ex_cmds/write_spec.lua` | `T3 :write appends FIFO file` |
| `lua/vim_spec.lua` | `T47 lua stdlib vim.rpcrequest and vim.rpcnotify` (the panic above) |
| `plugin/lsp_spec.lua` | `T246 LSP vim.lsp.config() and vim.lsp.enable() in first FileType event` |
| `terminal/tui_spec.lua` | no `RUN` line open — hung before or between tests |

`:cquit` and `--startuptime` are exit-path hangs: the test waits for the process to die and it does
not. `plugin/lsp_spec.lua` hangs immediately after a `CallbackError` from
`runtime/lua/vim/lsp/log.lua:36` → `vim.log.new` → `vim.call`, i.e. the LSP runtime fails to load and
the session never settles.

**Pass C adds two more** (`api/keymap_spec.lua` at `T269`, `ui/float_spec.lua` at `T268`) and drops
`ex_cmds/write_spec.lua` and `plugin/lsp_spec.lua`, which fail earlier there.

**Pass D warns about the future.** Shimming the color map took the 171 gated files from 3 unfinished
to **14**: `api/buffer_spec.lua`, `core/startup_spec.lua`, `editor/ctrl_c_spec.lua`,
`lua/vim_spec.lua`, `terminal/scrollback_spec.lua`, `terminal/tui_spec.lua`, `ui/cmdline_spec.lua`,
`ui/decorations_spec.lua`, `ui/float_spec.lua`, `ui/highlight_spec.lua`, `ui/inccommand_spec.lua`,
`ui/messages_spec.lua`, `ui/mouse_spec.lua`, `vimscript/system_spec.lua`. Every UI gate that opens
exposes a hang. Bound the suite before opening them, or a later census will spend its whole budget on
deadlines.

**The `just functional` host defect.** With oxvim hosting the harness, `vim.text.indent` is `nil`
(`type(vim.text)` is `table`, `type(vim.text.indent)` is `nil`; the oracle reports `function`), so
`test/testutil.lua:728` `dedent()` throws `attempt to call field 'indent' (a nil value)` in
**99 files / 485 tests**. Those 99 files score 29 passes in pass C against 198 in pass B — the host
gap costs 169 passes that oxvim already earns as a subject.

## 8. The three highest-leverage fixes

Ordered by evidence, not by file count. The file counts in §4 measure *gating*; the shim probes in
§6 measure *conversion*, and they disagree. Both are reported.

### R1. Make Lua/API error values plain strings, and stop Debug-formatting them

- **Unblocks:** 293 files / 3602 failures become diagnosable on the `just functional` path;
  90 files / 1232 failures in the primary path stop carrying a Rust struct dump; 144 spec files that
  assert through `pcall_err` become able to pass at all.
- **Evidence:** direct one-line reproduction, both halves. `pcall(vim.api.nvim_get_mode)` yields
  `userdata` in oxvim and `string` in the oracle. `:lua vim.api.nvim_get_mode()` yields
  `E5108: runtime error: Error(CallbackError { … cause: ExternalError(Exception("API function is not
  implemented")) })` where the oracle would print the message. Sites named:
  `crates/ox-lua/src/vim.rs:428-433` (`lua_error_text`), `crates/ox-lua/src/host.rs:59,70`,
  `crates/ox-editor/src/builtins/process.rs:330`.
- **Honest caveat:** this converts approximately zero failures to passes *by itself*. It is first
  because it is the only fix that makes the other 483 files' output trustworthy, and because
  `pcall_err` assertions cannot pass until the error is a string with upstream's text.

### R2. Ship `vim.text.indent` in the host Lua stdlib

- **Unblocks:** 99 files / 485 failures on the `just functional` path, worth at least the 169 passes
  those files already score when the oracle hosts the harness.
- **Evidence:** `vim.text` exists in oxvim and `vim.text.indent` is `nil`; `test/testutil.lua:728`
  (`dedent`) and `:188` are the only consumers, and `dedent` is called by
  `testnvim.lua:714` (`source`), so it is on the setup path of most specs. Pass C attributes exactly
  99 files to it; pass B, where the oracle supplies `dedent`, shows those files scoring 198.
- **Cost:** one function. Cheapest measured fix in this census by an order of magnitude.

### R3. Fill the 47 missing `nvim_*` dispatch entries, colour map and `nvim_exec2` first

- **Unblocks (gating):** 171 files stop dying at `Screen.new()`; 100 stop dying at `exec()`;
  34 + 3 at the `g:` variable API; 5 at `nvim_set_keymap` (210 tests); 160 files in total carry an
  `API function is not implemented` failure at 33 distinct call sites.
- **Evidence:** the dispatch probe in §5 is exhaustive over all 262 declared methods, so the 47 is
  a count, not an estimate. The *conversion* is measured too, and it is small: colour map alone
  +4 passes over 171 files (pass D), `nvim_exec2` alone +25 over 100 files (pass F), both together
  +29 (pass E) — with 743 and 326 executions respectively lost to new hangs. The shims were validated
  against the oracle: 97 of 100 files reproduce their control counts exactly.
- **Why still third and still worth doing:** 163 files fail *only* at the protocol boundary and can
  produce no editor signal until it is open. The pass-rate payoff arrives after the layer behind the
  gate — screen redraw content, `Row N did not match`, 71 files / 478 tests — is fixed. Expect the
  next census to show executions *fall* and hangs *rise* as these land. That is progress, and it
  will look like regression in the totals.

**What is not recommended, and why.** Ranking by file count alone would put the colour map first and
call it a 171-file win. Pass D falsifies that. The functional suite is not gated by a short list of
missing entry points the way the oldtest corpus was gated by `E492` and `E15`; it is broadly short of
behaviour. 373 of 484 files execute tests and pass none. The realistic read is that this corpus needs
the redraw and UI-content layer, not a dozen registry entries.

## 9. Method notes worth keeping

- `RunTests.cmake:31-33` sets `ENV{NVIM_PRG}` from the cmake `-D` variable and `:85` uses the same
  binary as the harness host. Exporting `NVIM_PRG` without `-D` silently measures the oracle.
- `--api-info` parity is meaningless here: oxvim's payload is byte-identical to the oracle's while
  114 of 262 methods are undispatched. Any parity claim must probe dispatch.
- The reporter emits ANSI colour even when stdout is a file. Strip `\x1b\[[0-9;]*[A-Za-z]` before
  parsing or every count reads zero.
- Kill by process group. `subprocess.run(timeout=…)` kills the direct child and then blocks forever
  on a stdout pipe held open by a grandchild.
- Six fixture binaries (`tty-test` and friends) are `EXCLUDE_FROM_ALL` and absent from this build;
  `build/lib/nvim/parser/*.so` must be present for `runtimepath^=$BUILD/lib/nvim`. Without them the
  control loses 459 tests that are not oxvim's fault.
- Per-file isolation matters: `treesitter/highlight_spec.lua` renames files under
  `test_source_path/runtime/queries/c/`, `plugin/pack_spec.lua` sets `GIT_DIR` to
  `test_source_path/.git`, `legacy/normal_spec.lua` and `ui/messages_spec.lua` create directories
  under `test_build_dir/share/locale`. `test/` and `src/` cannot be symlinks
  (`vimscript/executable_spec.lua`, `vimscript/fnamemodify_spec.lua` say so in comments).
- A blocker count of zero means "not observed", never "implemented". 373 files pass nothing, so most
  of the surface is masked by whatever fails first.

## 10. Artifacts

| path | contents |
| --- | --- |
| `.outline/sdd/functional-census.tsv` | 484 rows: spec, outcome, pass/fail/error/pending, executed, oracle-control pass/fail, blocker class, first blocker, hang site, and the `just functional`-shape pass/fail |
| `.outline/sdd/functional-blockers.md` | this file |
| `.outline/sdd/reports/task-75.md` | task report |

Per-file logs for all six passes (A control, B primary, C `just functional` shape, D/E/F shim probes,
Fctl neutrality control) were kept under `/tmp/t75func/logs/` and are not committed.
