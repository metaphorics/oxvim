# Task 72, the four census-3 defects: the panic, `writefile()`'s flags, the lost record, the hang

Four commits, one per defect. Every parity claim below was checked against
`.references/neovim/build/bin/nvim` (v0.13.0-dev-1390, API level 15) with the exact input quoted.

| # | commit | subject |
| --- | --- | --- |
| 1 | `f322fa9` | refuse invalid env names instead of aborting on `unlet $` |
| 2 | `280352e` | accept every documented `writefile()` flag, including `D` and `p` |
| 3 | `fef8828` | report a failed shell through `v:shell_error`, not a fatal `E677` |
| 4 | `097b33a` | close the child's standard input so a reading child exits |

## 1. The panic on `unlet $`

`unlet $` reached `remove_target` → `ox_sys::unset_env("")` → `std::env::remove_var("")`, which
panics (`failed to remove environment variable ``: Invalid argument`). rc 101, the editor gone, and
`test_unlet.vim`'s whole record with it.

**Which layer validates, and why both do.** Upstream defends this twice and the port now matches
both. `os_setenv`/`os_unsetenv` (`os/env.c` 175-223) return -1 for an empty name and log a libuv
`EINVAL` for the rest without aborting; `ox-sys` is that boundary here — the only place that sees a
name before `std::env` turns it into a dead process — so the refusal lives there and both functions
report whether they applied. That alone removes the crash from every caller, including `setenv()` in
`ox-eval` (whose crate this task does not own), and it matches upstream: `setenv('', 'x')` on the
oracle succeeds silently, and `setenv('a=b', 'x')` is a no-op in a release build.

But the boundary cannot produce the message. Upstream's `E475` comes from `get_env_len` in
`ex_unletlock` (`eval/vars.c` 1587-1600) and `ex_let_env` (1323-1330), *before* the mutation is
attempted, and it names the whole remaining argument rather than the token. So `:unlet` and `:let`
measure a `$` target themselves. Validation at only one of the two layers would be wrong in a
different way each: only in `ox-sys`, `unlet $` becomes a silent success; only in the commands,
`setenv('', …)` still kills the process.

Oracle comparison, all twelve identical:

| input | oracle | oxvim |
| --- | --- | --- |
| `unlet $` | `E475: Invalid argument: $` | same |
| `unlet $ ` | `E475: Invalid argument: $ ` | same |
| `unlet $=x` | `E475: Invalid argument: $=x` | same |
| `unlet $FOO=BAR` | `E488: Trailing characters: =BAR` | same |
| `unlet! $` | `E475: Invalid argument: $` | same |
| `unlet $ trailing` | `E475: Invalid argument: $ trailing` | same |
| `unlet $ x` | `E475: Invalid argument: $ x` | same |
| `unlet $FOO $` | `E475: Invalid argument: $` | same |
| `let $=1` | `E475: Invalid argument: $=1` | same |
| `let ${}=1` | `E475: Invalid argument: ${}=1` | same |
| `let $ = 'x'` | `E475: Invalid argument: $ = 'x'` | same |
| `let $ = g:nosuchvar` | `E121: Undefined variable: g:nosuchvar` | same |

The last row is the ordering: upstream fills `tv` before `ex_let_one`, so the value is evaluated
first and a bad expression wins over `E475`. The guard sits after the evaluation to keep that.

`rc 0` now, and the process survives `setenv('')` and `setenv('a=b')`.

**Audit of every other `ox-sys` entry point.** `locale::set_locale` already rejects its own hazard —
`CString::new(name).ok()?` refuses an interior NUL and returns `None`, which is exactly the
`setlocale`-returned-NULL shape the caller reports as `E197`. `locale::current_locale` takes no
input. `set_env`/`unset_env` were the only unvalidated ones, and the panic set is now closed for
both: empty name, `=` in the name, NUL in the name, NUL in the value.

## 2. `writefile()`'s flags

Upstream accepts `b a D s S p` in any combination (`eval/fs.c` 1835-1860); the port accepted `b a s
S`. All 39 files the census counted failed as `Unknown flag: D`.

Every flag, oracle vs oxvim, identical on all 20 probes (14 flag strings, the `p` parent chain, the
`D` deferred delete at two nesting levels, and the `a`/`b`/`ab` byte-level contracts):

| input | both |
| --- | --- |
| `''` `b` `a` `s` `S` `p` `ba` `bS` `ap` | `ret=0`, file written |
| `D`, `pD` at script level | `E193: defer not inside a function` |
| `x`, `bax` | `E5060: Unknown flag: x` |
| `bxa` | `E5060: Unknown flag: xa` |
| `writefile(…, 'sub/dir/deep.txt', 'p')` | `ret=0`, readable |
| same without `p` | `E482` |
| `D` inside a function | readable inside, gone after |
| `pD` inside a function | file gone after, directory kept |
| `['A']` then `['B']` with `a` | `['A', 'B']` |
| `['x','y']` with `b` / without / `ab` | 3 / 4 / 4 bytes |

Two things worth recording. `E5060` prints the *rest* of the flag string, not one character —
`semsg(_("E5060: Unknown flag: %s"), p)` — which is why `bxa` is `Unknown flag: xa`. And `s`/`S`
select the `fsync` in `file_close(&fp, do_fsync)` (1902), for which `FileIO` has no seam; they are
accepted and change nothing, because the bytes are in the file either way and rejecting the letters
would be the only observable difference.

`D` needed the frame machinery upstream keeps in `funccall_T.fc_defer`. `ExRuntime` now carries one
list per active user-function frame; `writefile` reports the absolutized path and the frame's list is
drained in reverse in `call_user_function_with_self`, after the body and **whatever its outcome was**
— upstream calls `handle_defer_one` from `call_user_func`'s cleanup (`userfunc.c` 1272) and
saves/restores the exception state around it precisely so an aborted function still gets its files
removed. `can_add_defer` is checked before the file is opened, so `E193` leaves nothing behind.

Effect, measured on five files (the census's 39 is the count of files where `E5060` *appears*; in all
of them it is a caught exception inside a test, so it costs failures rather than executions):

| file | executed/failed/skipped before → after | `E5060` occurrences before → after |
| --- | --- | --- |
| `test_filetype.vim` | 80/79/0 → 80/78/0 | **57 → 0** |
| `test_edit.vim` | 89/63/0 → 89/63/0 | 8 → 0 |
| `test_tagjump.vim` | 41/33/0 → 41/33/0 | 8 → 0 |
| `test_cpoptions.vim` | 45/38/0 → 45/38/0 | 7 → 0 |
| `test_modeline.vim` | 17/15/0 → 17/14/0 | 4 → 0 |

## 3. `test_cmdline.vim`'s lost 45-test record

The census chain was right, and both halves are fixed.

**`E677` had no upstream counterpart on this path at all.** nvim's `f_system` never touches a temp
file; a shell it cannot spawn is reported through `v:shell_error`. Oracle with `&shell` pointed at a
missing path: `''` and `v:shell_error == -1`, no exception. oxvim now answers exactly that. With
`$PATH` poisoned and a working `&shell`, the oracle answers the shell's `rm: command not found`
diagnostic with `v:shell_error == 127`; oxvim now answers the same text and the same 127.

**A poisoned `$PATH` no longer reaches the shell lookup.** `system()` hardcoded `sh`, `systemlist()`
read `$SHELL`, and neither read `'shell'` — so the two disagreed about the shell of the same editor
and both went through `$PATH`. Both now build their argv from `'shell'` + `'shellcmdflag'`
(`shell_build_argv`, `os/shell.c` 60-97), as does a String command to `jobstart()`; and startup seeds
`'shell'` from `$SHELL` the way `set_init_default_shell` (`option.c` 182-199) does, quoting a path
holding a space. The option holds an absolute path, so `$PATH` is irrelevant to finding it.

Two parity fixes fell out of routing both builtins through one core: `system()` had been ignoring its
input argument entirely (`system('cat', '123')` answered `''`; it is `'123'` now), and `os_system`
merges the child's standard error into the same buffer as its standard output.

`test_cmdline.vim` now reports **45 executed, 39 failed, 7 skipped** with no patch to the test — the
45/39/7 the census predicted from its patched cleanup, and an improvement on pass 2's 45/46/0.

## 4. `test_system.vim`'s hang and its orphan

The census localised this to `system('cat', '123')`; the failing call is actually
`systemlist('cat', '123')` at `test_system.vim:21`. `system()` was reaching
`std::process::Command::output()`, whose default child stdin is `/dev/null`, so it returned `''`
immediately — wrong, but not a hang. `systemlist()` went through `JobManager`, and that is where the
open descriptor was. The census's `/bin/sh -c cat` observation fits: `job_command` wraps a String
command in a shell, and `system()`'s hardcoded path did not.

`JobManager::close_input` only dropped the `ProcessPipe` handle. A `ProcessPipe` shares its stream
state with the clone the loop's reactor holds, so dropping the handle closed nothing: the child's
standard-input descriptor stayed open, `cat` never saw EOF, and the `wait(-1)` behind
`system()`/`systemlist()` waited forever. Upstream shuts that stream down before waiting
(`os/shell.c` `do_os_system`).

`close_input` now closes the handle through `ox_uv::Handle::close`, which deregisters the descriptor
and drops it. A write the synchronous attempt could not finish — an input larger than one pipe
buffer — is still queued, so the loop is pumped until the queue drains first; the reactor clears the
queue on either a completed write or a closed peer, so the pump terminates. `shutdown` was the other
candidate and is the wrong one: it refuses while a write is queued, which would leave the descriptor
open on a stuck peer, and an open descriptor is the whole defect.

Second half, so a child can never outlive the parent that was waiting on it: dropping a `JobManager`
terminates every job it still holds that has not exited, and leaves a `detach`ed one alone —
upstream's `channel_close_on_exit`. Without it a child still blocked when the editor goes away
becomes an orphan holding the inherited standard output, which is what turned the census timeout from
a failure into a hang for anything reading that pipe.

Oracle comparison and the orphan proof:

| input | oracle | oxvim before | oxvim after |
| --- | --- | --- | --- |
| `system('echo 123')` | `'123\n'` | `'123\n'` | `'123\n'` |
| `system('cat', '123')` | `'123'` | `''` | `'123'` |
| `systemlist('cat', '123')` | `['123']` | **hang** | `['123']` |
| `systemlist('echo 123')` | `['123']` | `['123']` | `['123']` |

`test_system.vim` went from a 150 s timeout with an empty record to 4 executed / 2 failed / 2 skipped
in **0.4 s**. After the run `ps` shows no `sh -c cat` and no `cat` (checked on a clean run, and again
on four isolated single-call probes with a `pkill` between each). The only `cat` processes seen during
this task were the ones the J1 mutation below deliberately created by restoring the old behaviour.

## Oldtest, before and after

Fresh throwaway `HOME` per run, `VIMRUNTIME` and `OXVIM_RUNTIME` exported, a scratch copy of
`testdir` with the committed stale `test.log` stripped, stdin from `/dev/null`, 150 s timeout, process
group killed on expiry, numbers read from the per-run `messages`.

| file | before (exec/fail/skip) | after (exec/fail/skip) | note |
| --- | --- | --- | --- |
| `test_unlet.vim` | 0/0/0 | **7/4/0** | rc 101 panic → rc 0 |
| `test_cmdline.vim` | 0/0/0 | **45/39/7** | rc 1 fatal `E677` → rc 0 |
| `test_system.vim` | 0/0/0 | **4/2/2** | 150 s timeout → 0.4 s |
| `test_filetype.vim` | 80/79/0 | 80/78/0 | `E5060` 57 → 0 |
| `test_cpoptions.vim` | 45/38/0 | 45/38/0 | `E5060` 7 → 0 |
| `test_modeline.vim` | 17/15/0 | 17/14/0 | `E5060` 4 → 0 |

Three further `E5060` files measured while choosing the three above and left in for the record:
`test_edit.vim` 89/63/0 → 89/63/0 (8 → 0), `test_tagjump.vim` 41/33/0 → 41/33/0 (8 → 0), and
`test_file_perm.vim` / `test_scriptnames.vim` / `test_stacktrace.vim` unchanged at 1/1/0, 2/2/0 and
4/3/0 — the census recorded `E5060` as their *first* blocker, but all three already ran every test
they have and their remaining failures are unrelated (`setfperm`, `getscriptinfo`, `getstacktrace`).

## Unit tests

`cargo test -p ox-editor -p ox-uv -p ox-sys -- --test-threads=1`: **825 / 45 / 1, zero failures**
(ox-editor baseline 806; +19 is 7 of mine and 12 from task 73's commits, which landed in the same
tree). `cargo test --workspace -- --test-threads=1` is green throughout: ox-eval 478, ox-excmd 162,
ox-text 23, oxvim 62.

The suite needs `--test-threads=1`. At default parallelism three pre-existing tests fail on `HOME`
and cwd races (`expand_builtin_resolves_home_and_environment_variables`,
`let_env_assignment_reaches_child_processes`,
`swapfilelist_current_directory_entry_yields_relative_names`); that is not new here.

## Mutations

Every new test was mutation-checked. **23 mutations, 23 killed, 0 survivors.** Each mutation is a
restoration of the defect or the removal of one clause of a compound rule, so no clause is unpinned.

Defect 1, `ox-sys` (`env_mutation_refuses_only_the_names_the_platform_rejects` — one case per clause,
each failing only its own clause, plus an accepted name so a blanket `false` cannot pass):

| # | mutation | result |
| --- | --- | --- |
| M1 | drop the empty-name clause | killed |
| M2 | drop the `=`-in-name clause | killed |
| M3 | drop the NUL-in-name clause | killed |
| M4 | drop the NUL-in-value clause | killed |

Defect 1, the commands (`unlet_env_target_measures_the_name_before_unsetting`,
`let_env_target_reports_e475_after_evaluating_the_value`):

| # | mutation | result |
| --- | --- | --- |
| M5 | drop `:unlet`'s `E475` branch | killed |
| M6 | drop `:unlet`'s `E488` branch | killed |
| M7 | `E475` names the token instead of the remainder | killed |
| M8 | drop `:let`'s `E475` guard | killed |
| M9 | hoist `:let`'s guard above the value evaluation | killed (the `E121` ordering case) |

Defect 2 (`writefile_accepts_every_documented_flag`,
`writefile_defer_flag_deletes_per_frame_on_return_and_on_abort`):

| # | mutation | result |
| --- | --- | --- |
| W1 | drop the `D` letter | killed (both tests) |
| W2 | drop the `p` letter | killed |
| W3 | drop the `s`/`S` letters | killed |
| W4 | `E5060` names the character instead of the remainder | killed |
| W5 | drop the `E193` frame check | killed |
| W6 | run the `E193` check after the write | killed (the "leaves no file behind" case) |
| W7 | accept `p` but skip the parent-chain creation | killed |
| W8 | run the deferred deletes only on the success path | killed (the abort case) |
| W9 | one shared defer list instead of one per frame | killed (the nesting case) |
| W10 | never push a frame | killed |

Defect 3 (`system_uses_the_shell_options_feeds_input_and_never_raises_on_a_bad_shell`):

| # | mutation | result |
| --- | --- | --- |
| S1 | hardcode `sh` and ignore `'shell'` | killed |
| S2 | ignore the input argument | killed |
| S3 | raise `E677` on a spawn failure again | killed |
| S4 | drop the stderr merge | killed |

Defect 4 (`close_input_gives_a_reading_child_eof_after_its_input`,
`close_input_flushes_a_write_larger_than_one_pipe_buffer`,
`dropping_the_manager_terminates_a_live_child_but_not_a_detached_one`):

| # | mutation | result |
| --- | --- | --- |
| J1 | `close_input` drops the handle again — the exact defect | killed (both close tests; also reproduced the orphan `cat`) |
| J2 | drop the pending-write pump | killed (the >64 KiB case only, which is the point of it) |
| J3 | the drop guard terminates detached children too | killed |
| J4 | remove the drop guard | killed |

One mutation was **not run**: removing the `close_input` call from the shell path. It reinstates an
unbounded `wait(-1)` on a child that never sees EOF, so the test process hangs rather than fails. J1
covers the same clause from the other side by restoring the old drop-only close, and it killed both
tests in 26 s.

Two tests of mine were themselves wrong before the mutation round and are worth recording, because
they failed *green-looking*: with a buffered stream **and** an `on_stdout` callback, `drain_raw` hands
the accumulated bytes to the EOF callback, so `take_buffered_output` finds nothing. The tests were
asserting the shape of the wrong job configuration, not a real truncation. They now use the
callback-less shape `system()`/`systemlist()` actually build.

## Not done

**The `E523` split** (`excmd_exec.rs` 496/507/559/2590/2606) is not done. It was the
budget-permitting item and the budget went to the four defects, the twenty-three mutations, and the
concurrent-edit protocol below. It is unstarted, not half-done: no source was touched. The shape of
the work is unchanged from the census — each site wraps any error escaping the Normal-mode/typeahead
machinery, `E523: no previous search pattern` is upstream's `E35`, and
`E523: register text must be valid UTF-8` is a leaked internal string with no upstream analogue.

## Concerns

- **`'a'.x` is `E715: Dictionary required`.** String concatenation with `.` and no surrounding spaces
  before an identifier or a function call is parsed as a dict subscript: `echo 'a='.filereadable('/tmp')`
  and `echo 'a'.x` both fail where the oracle answers `a=0` and `a1`. `'a'.'b'` and `'a' . x` are
  fine. Pre-existing — reproduced on the pinned pre-task binary — and not one of the four, but it is
  a large class: legacy Vimscript uses this spelling constantly, and every occurrence is a lost test.
  This is why the probes in this report are written with `..`.
- **`system()`'s List input is not NL→NUL converted.** `systemlist('cat', ["as\<NL>df"])` is
  `test_system.vim:22` and still fails: upstream writes a list item's NL as NUL and reads it back as
  NL, `channel_bytes` joins with NL. Same for a buffer-number input (`system('wc -l', bufnr('%'))`),
  which is written as the digits of the number rather than the buffer's text — `test_system.vim:26`
  and 41. Both are inside `Test_System`, which now aborts earlier at `E16: rope text must be valid
  UTF-8` on `setline(1, ['asdf', "pw\<NL>er", 'xxxx'])`.
- **`E482` text.** `Can't open file "sub2/dir/deep.txt" for writing: No such file or directory (os
  error 2)` where upstream is `Can't open file sub2/dir/deep.txt for writing: no such file or
  directory`: the path is `{path:?}`-quoted and the message is Rust's rather than `os_strerror`'s.
  Pre-existing and used consistently across `fs_builtins.rs`, so it is one change to that file's
  formatting convention rather than a line in this diff; left alone deliberately.
- **The census's own two misattributions**, for whoever reads it next. `system('cat','123')` was not
  the hang (`systemlist` was), and "39 files fail as `Unknown flag: D`" counts files where `E5060`
  appears, not files it gates — in all 39 it is a caught exception inside a test body, so it cost
  failures and not executions. Both are recorded above with the measurements.
- **`'shell'`'s static default is still `sh`.** The `$SHELL` seeding is in `oxvim`'s startup, where
  upstream's `set_init_default_shell` is, so an embedder that builds an `Editor` directly still gets
  the bare name. That is upstream's layering, but it means `ox-editor`'s own tests see `sh`.
- **`close_input`'s 30 s pump deadline.** If it ever expired the handle is still closed, so no
  descriptor leaks; the only loss would be an unflushed write remainder. The reactor drains the queue
  on a closed peer, so the only way to reach the deadline is a child that reads its input slower than
  30 s, which nothing in the suite does.

## Concurrent-edit protocol with task 73

Both tasks needed `crates/ox-editor/src/excmd_exec.rs`. Task 73's half was 15 anchored literal
edits interleaved with mine inside the same functions, so partial staging was not available. What
worked, and is worth reusing: their patch shipped as an idempotent script with an anchor list, which
made it mechanically **reversible** — reverse-applying the `new → old` substitutions and forward-
applying again reproduced the patched file byte-identically (asserted, not assumed). So each side
could take the file, commit its own change alone, and hand back a tree whose only delta was the
other's. Cost two round-trips; saved two commits with each other's work in them. Their commits
(`afdc39f`, `c6d7a23`, `5eee4e3`, `d94388d`, `a39a13c`) and mine interleave in the log and are
individually clean.

One cross-task edit was unavoidable and is in commit 1: `excmd_exec_function_tests.rs` 1144-1147 and
1522-1525 restore `$HOME` through a `match` tail expression, which stopped compiling once
`set_env`/`unset_env` began reporting a status. Each is now wrapped in `assert!`, which also pins
that the restore succeeded.
