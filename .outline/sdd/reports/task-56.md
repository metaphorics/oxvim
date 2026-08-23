# Task 56: the Ex command surface, continuation

Status: **DONE_WITH_CONCERNS**. The two audit findings against task 55 are landed
and verified. **None of the brief's six command groups landed**: the checkout was
destroyed mid-task and the parent's post-recovery instruction scoped this run to
restoring the lost fix plus finishing the audit work. The six groups are
researched and unblocked but unwritten. Nothing is stubbed.

Base: `c41dab3` (the recovery commit). Work commits, local only — the push token
is invalid, so nothing was pushed and no push was attempted:

| SHA | subject |
| --- | --- |
| `57a0212` | `fix(ox-excmd): reject an out-of-domain Ex address instead of clamping it` |
| `a9f3a21` | `fix(ox-editor): fire readfile's autocommands for :read and its filter forms` |

## Test counts

| crate | before (`c41dab3`) | after (`a9f3a21`) |
| --- | --- | --- |
| ox-editor | 600 passed | 608 passed |
| ox-excmd | 158 passed | 158 passed |

`cargo test -p ox-editor -p ox-excmd -- --test-threads=1`, 0 failed in both.
Single-threaded because a pre-existing env-mutating test flakes in parallel.

The `a9f3a21` numbers were measured in a detached worktree pinned to `57a0212`
with only my two files copied in (`git worktree add /tmp/ox56-verify 57a0212`).
The shared checkout does not compile right now: peer `FixTask54` is mid-edit on
`crates/ox-editor/src/builtins/position.rs`, `editor.rs` and `layout.rs`
(unresolved `unicode_width` import, missing `buflist_findnr`). Those files are
not mine and were left untouched and unstaged. `57a0212` itself was measured in
the shared tree while it was still clean.

The one pre-existing warning, `unused_must_use` on `arglist.rs:401`, predates
this task and is untouched.

## Landed

### 1. Out-of-domain Ex addresses are rejected, not clamped (`57a0212`)

Audit finding 2. An address past the end of its domain was clamped onto the
domain's last element, so `:99read in.txt` in a three-line buffer inserted the
file at line 3, and `:5,6delete` resolved to the *reversed* range `(5, 3)`
because `resolve_range` clamped only the upper end. Upstream rejects the
address: `invalid_range` (`ex_docmd.c:3735-3820`) runs inside `do_one_cmd`
before the command function and bounds `line2` against a per-domain limit,
returning `E16: Invalid range`.

The clamp was shared by twelve `resolve_range` call sites, so per the finding's
own instruction the fix went to the shared site rather than into the `:read`
arm. It went one level further out than that, to the `dispatch` entry, because
that is where upstream puts it: `:close` and `:buffer` never resolve a range at
all, so a check inside `resolve_range` would still have missed them. That is
exactly what the first draft of this fix did, and the `:99close` assertion
caught it.

Applying the rule needs the address domain, which the generated command table
did not carry. `addr_type` now comes from the same `ex_cmds.lua` entries the
flags come from, so the two cannot drift. The build script had to change shape
for it: it used to emit an entry the moment its `flags` completed, but
`addr_type` *follows* `flags` in every upstream entry, so it now accumulates
fields until the entry's closing `},`. All 564 entries carry an `addr_type`, and
a missing one is a hard build error rather than a silent default.

Domains implemented with upstream's limits: `Lines` (line count), `Arguments`
(`ARGCOUNT + (!ARGCOUNT)`, so `:0argdelete` on an empty list is not an error),
`Windows`, `Tabs`, `Buffers` (plus its `line1 < 1` guard). `Other`,
`TabsRelative` and `Unsigned` accept any range, as upstream does.

Named gap: `LoadedBuffers`, `QuickFix` and `QuickFixValid` are not checked. This
port has neither a buffer load state ordered by number nor a quickfix list, so
there is no limit to compare against. Documented on the function rather than
faked with a wrong bound.

A post-command *count* still clamps silently. That is deliberate and is
upstream's ordering: `invalid_range` runs at `ex_docmd.c:2209`, before
`parse_count(..., validate=true)` at `2321`, and `set_cmd_count`
(`ex_docmd.c:1372-1393`) clamps a count with the comment "be vi compatible: no
error message for out of range". So `:99print` is an error and `:1print 99`
still prints to the end.

One defect was fixed on the way back in. The version recovered from backup
carried a duplicated `CommandFlags::RANGE` guard — dead code left by my own
edit, harmless but debris. It is gone; the restored commit is otherwise
byte-identical in substance to the lost `4e2ae81`.

Tests (4): an out-of-range address is E16 and leaves the buffer untouched; the
rule reaches `:99print`, `:5,6delete` and `:2,9yank`, not just `:read`; the last
line and `:0read`'s ZEROR line 0 still resolve; each domain gets its own limit
(`:99bnext` fine, `:99close` and `:99buffer` E16).

### 2. `:read` fires `readfile`'s autocommands (`a9f3a21`)

Audit finding 1. `:read {file}` went straight through the `FileIO` seam and the
filter forms returned as soon as the shell exited, so no plugin could observe
either.

- File form: `FileReadCmd` fires first and a matching definition **replaces**
  the read — the command then performs none of its own work, which is upstream's
  `apply_autocmds_exarg` returning true (`fileio.c:336-340`). Otherwise
  `FileReadPre` runs before the insert (`fileio.c:640`) and `FileReadPost` after
  it (`fileio.c:1925`). Both match against the file name with a null buffer,
  as upstream passes them.
- Filter form: reads with `READ_FILTER`, so `FilterReadPre`/`FilterReadPost`
  (`fileio.c:631,1914`) instead of the `FileRead` pair, then `ShellFilterPost`
  (`ex_cmds.c:1236`). These match against the current buffer's name, which is
  what upstream's null-name-plus-`curbuf` call resolves to and what
  `:help FilterReadPre` documents.
- `:write !cmd` reads nothing back, so `ShellFilterPost` alone.

The post events now fire even when the read produced no lines, because upstream
runs them on the way out of `readfile` regardless. That meant splitting the
early return the empty case used to take — the one structural change in the
arm.

`lua` is now threaded through `command_read`, `command_write` and
`command_write_filter`, since running an autocommand plan needs the Lua host for
callback definitions.

Tests (4): the file form's Pre/Post straddle the insert (asserted via
`line('$')` inside each handler: 1 then 2, so they cannot both be on one side);
`FileReadCmd` intercepts, suppressing both `FileReadPre` and the insert; the
filter form fires Filter+Shell and *not* `FileReadPost`; `:write !cmd` fires
`ShellFilterPost` and not `FilterReadPost`.

## Mutation check

Every new test was proved load-bearing by reverting the fix and confirming the
test fails.

`57a0212`, `check_address_domain` neutralized to `return Ok(())`:

```
test each_address_domain_gets_its_own_limit ... FAILED
test in_range_addresses_survive_the_domain_check ... ok
test out_of_range_address_raises_e16_for_every_line_addressed_command ... FAILED
test out_of_range_address_raises_e16_without_mutating_the_buffer ... FAILED
test result: FAILED. 601 passed; 3 failed
```

The in-range control passing is the point: it proves the three failures come
from the check firing, not from the harness breaking.

`a9f3a21`, mutation A — `fire_read_autocmd` and `fire_shell_filter_post`
neutralized:

```
test read_file_fires_the_file_read_events ... FAILED
test read_file_read_cmd_replaces_the_read ... ok
test read_filter_fires_the_filter_and_shell_events ... FAILED
test write_filter_fires_shell_filter_post_only ... FAILED
test result: FAILED. 605 passed; 3 failed
```

`read_file_read_cmd_replaces_the_read` survives A because interception is an
inline plan, not one of those two helpers, so it needed mutation B — the
`FileReadCmd` interception replaced by `let _ = plan;`:

```
test read_file_read_cmd_replaces_the_read ... FAILED
test result: FAILED. 607 passed; 1 failed
```

## Oracle comparison

Captured before the checkout was destroyed, against
`.references/neovim/build/bin/nvim` and `target/debug/oxvim` built at the E16
fix. Not re-run: the parent's instruction was explicit, and the oracle binary
does not exist yet after the fresh `.references` clone.

Script on `['a','b','c']` with `in.txt` = `x\ny\n`, running each command inside
`try`/`catch` and recording `v:exception`:

```
=== ORACLE .references/neovim/build/bin/nvim ===
99read in.txt => Vim(read):E16: Invalid range: 99read in.txt
99print => Vim(print):E16: Invalid range: 99print
5,6delete => Vim(delete):E16: Invalid range: 5,6delete
2read in.txt =>
buffer=a/b/x/y/c

=== OXVIM target/debug/oxvim ===
99read in.txt => E16: Invalid range
99print => E16: Invalid range
5,6delete => E16: Invalid range
2read in.txt =>
buffer=a/b/x/y/c
```

Per-domain probe, one window and one buffer:

```
=== ORACLE ===
99resize =>
99close => Vim(close):E16: Invalid range: 99close
99buffer => Vim(buffer):E16: Invalid range: 99buffer
99argdelete => Vim(argdelete):E16: Invalid range: 99argdelete
```

Result: **the error code, the set of commands that fail, and the buffer content
all match.** `:99resize` is correctly *not* an E16 (ADDR_OTHER), and `:2read`
still inserts normally, so the check is not over-firing.

Two differences remain, both **pre-existing and command-independent**, and both
already recorded in task 55's report: this port never emits the `Vim({cmd}):`
prefix in `v:exception`, and never emits `append_command`'s `: {cmdline}` suffix
(`ex_docmd.c:2375-2384,2993-3019`). Neither is introduced here — task 55 proved
the prefix gap with `:edit` on both binaries. They are worth their own leaf,
since any oldtest matching on `Vim(cmd):` will miss.

`:99resize` diverges for an unrelated reason worth noting for whoever owns
`:resize`: upstream accepts it and simply cannot grow the window, while this
port raises `E36: requested window extent 1 exceeds 24 available cells`. That is
not an addressing bug, and the domain test uses `:99bnext` instead so it pins
the domain rule rather than that divergence.

## Not reached: the brief's six command groups

No code was written for any of them, so every one still reports
`NotImplemented`. No partial arms, no stubs. Ordering: the parent injected the
two audit findings as priority work at the start of the run, the checkout was
destroyed while the second one was mid-edit, and the post-recovery instruction
scoped this run to restoring `4e2ae81` and finishing that finding.

The research pass did complete, and it corrects two of task 55's assumptions:

| item | upstream entry point | finding |
| --- | --- | --- |
| `:tabnew`, `:tabedit`, `:tabonly`, `:vnew` | `ex_docmd.c ex_splitview:5637`, `ex_tabonly:5238`, `get_tabpage_arg:4398` | Needs one new `Editor` method. `win_new_tabpage` (`window.c:4484-4539`) inserts positionally — `after == 1` makes the new tab first, `after > 0` inserts *before* tab `after`, `after == 0` after the current one, and `:tabnew` passes `eap->line2 + 1`. `create_tabpage` only appends and `tabpages()` is BTreeMap key order, so tab position is inexpressible today. The parent approved adding `create_tabpage_at`. `:tabonly`'s "Already only one tab page" message and its `get_tabpage_arg` parse (`+N`/`-N`/`$`/`#`, E474 for `#` with no last-used tab, else E475) are otherwise the only new pieces. |
| `:undo`, `:redo` | `ex_docmd.c ex_undo:6729`, `ex_redo:6783` | `:redo` takes **no** count: upstream is `u_redo(1)` unconditionally, and `redo`'s flags are `TRLBAR\|BUFLOCK_OK\|LOCK_OK` with no RANGE or COUNT, so `:3redo` is E481, not three steps. Task 55's "with a count" applies to `:undo` only. At the ends, upstream messages `Already at oldest change` / `Already at newest change` (`undo.c:1935,1948`) rather than erroring; this port has neither string yet. `:undo {N}` is `undo_time(N, absolute)` and its not-found case is `E830: Undo number %ld not found` (`undo.c:2165`). `ox-text` exposes `UndoTree::undo_to_seq` and `current_seq`, but `Editor` has no wrapper for seq-targeted navigation, only one-step `buffer_undo`/`buffer_redo`; `BufferState.undo` visibility needs checking before choosing between a loop and a new wrapper. `:undo!` additionally needs the branch check and `E5767`. |
| `:retab` | `indent.c ex_retab:1436-1617` | Needs a `win_chartabsize` equivalent, which this port has only as local arithmetic in `builtins/position.rs`. Also `-indentonly`, and the `'vartabstop'`-vs-`'tabstop'` write-back at `1597-1613`. The `DFLALL` prerequisite is already in from task 55. |
| `:hide`, `:sleep`, `:z`, `:scriptencoding`, `:argdelete` | `ex_hide:5369`, `ex_sleep:6459`, `ex_cmds.c ex_z:3154`, `runtime.c ex_scriptencoding:2946`, `arglist.c ex_argdelete:759` | `:sleep 100m` exposes a real parser gap: `take_count` (`parser.rs:876-887`) refuses digits not followed by whitespace, but upstream takes them greedily unless `EX_BUFNAME` is set (`ex_docmd.c:1401-1403`), and the port's `CommandFlags` has no `BUFNAME` bit. Fixing that is shared across every COUNT command, so it belongs with `:sleep`, not after it. `:z` needs `'window'`/`'scroll'` and `Rows`, and its `=` form prints separator lines sized to `Columns`. `:scriptencoding` needs the E167 guard; its `convert_setup` conversion needs an encoding converter this port does not have. `:argdelete` is straightforward now that ADDR_ARGUMENTS is validated. |
| `:lockvar`, `:unlockvar` | `eval/vars.c ex_lockvar:1554` | Only the depth parse (`!` → -1, else optional leading digits, default 2) plus the `Scope::lockvar`/`unlockvar` call, as the brief said. |
| `:fold`, `:foldopen`, `:foldclose` | `ex_docmd.c ex_fold:8019`, `ex_foldopen:8028`, `fold.c:386-533` | A fold model **does** exist in `fold.rs`, so this item is not declined: `Folds::create_manual/open/close/open_recursive/close_recursive` are all there. But `Folds::set_method` is called from nowhere in the tree and `'foldmethod'` is wired to nothing, so the E350 guard has no source of truth yet and `require_manual()` currently always passes. Upstream's guard also *allows* `foldmethod=marker`, where `:fold` inserts `'foldmarker'` strings into the text (`foldCreateMarkers`) rather than recording a range — a second, text-mutating path. Deciding between wiring the option into `Folds` and scoping `:fold` to manual-only is the first real choice here, and it should be made deliberately rather than inside a command arm. |

## Notes for the integrator

- `.outline/GATES.md` and `.outline/sdd/reports/task-12b.md` were never staged.
  Neither was `Cargo.lock`, `crates/ox-editor/Cargo.toml`, or any of peer
  `FixTask54`'s in-flight files.
- No formatter was run and no project-wide suite was run.
- Verification ran in `/tmp/ox56-verify`, a detached worktree at `57a0212` and
  then at the committed `a9f3a21`. It has been removed; `git worktree list`
  shows only the main checkout. Recreate it with
  `git worktree add /tmp/ox56-verify a9f3a21` if you want to re-check these
  numbers while the shared tree still does not compile.
- The `bash` tool stayed broken for me after the recovery — its persistent shell
  could not reinitialise once its cwd had been deleted — so everything here ran
  through `subprocess` in the eval kernel with
  `PATH=/home/alpha/.cargo/bin:$PATH RUSTC_WRAPPER=""`.
- One line in `excmd_exec_state_tests.rs` (a file I own) needs to change as a
  consequence of `Task54Position`'s `update_curswant` work: line 1410's
  `Typval::Number(4)` becomes `8`. I ceded that edit to them so the expectation
  lands atomically with the behavior it pins, rather than as a separately-red
  commit I cannot currently run.
- **`.outline/sdd/reports/task-55.md` did not survive.** Reports through
  `task-50.md` are tracked, but 51-55 were never committed, so the recovery had
  nothing to restore them from and they are gone. Task 55's landed *code* is
  intact in `c41dab3`; what is lost is its write-up — the `:help` decline with
  its named missing subsystem (tag subsystem, `doc/tags` index, `help` buftype),
  the `:s` false-positive finding, the `:redraw` topline gap, the `:filetype`
  `do_modelines` gap, and the `Vim({cmd}):` prefix leaf. The research half is
  re-derived and extended in this report's "Not reached" table; the decline
  rationale and the named gaps are not, and should be recovered from that run's
  transcript before anyone re-attempts `:help` or re-audits those commands.
