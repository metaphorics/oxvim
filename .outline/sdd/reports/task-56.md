# Task 56: the Ex command surface, continuation

Status: **DONE_WITH_CONCERNS**. All six brief command groups landed, plus the two
audit findings against task 55 and four defects the oracle comparisons exposed.
Nothing is stubbed. Every named gap is documented on the code and listed below.

The concerns are not unfinished work: they are three upstream-model gaps and one
missing builtin family that this task uncovered and could not close inside its
own scope. They are itemised in "Found, not fixed".

Base: `ca0ef4a`. Work commits, local only — the push token is invalid, so nothing
was pushed and no push was attempted:

| SHA | subject |
| --- | --- |
| `57a0212` | `fix(ox-excmd): reject an out-of-domain Ex address instead of clamping it` |
| `a9f3a21` | `fix(ox-editor): fire readfile's autocommands for :read and its filter forms` |
| `89b99ab` | `docs(outline): report task 56 verification` (superseded by this file) |
| `7f76e94` | `feat(ox-editor): run :tabnew, :tabedit, :tabonly, and :vnew` |
| `3743c22` | `fix(ox-excmd): match upstream's E481 message casing` |
| `0260f23` | `feat(ox-editor): run :undo and :redo` |
| `83deee2` | `fix(ox-editor): scope the window builtins to the current tabpage` |
| `1e685e5` | `feat(ox-editor): run :retab` |
| `f946c19` | `feat(ox-editor): run :hide, :sleep, :z, :scriptencoding, and :argdelete` |
| `a5ca06e` | `fix(ox-excmd): report upstream's parser error codes and texts` |
| `7b507a1` | `feat(ox-editor): run :lockvar and :unlockvar` |
| `bebb704` | `feat(ox-editor): run :fold, :foldopen, and :foldclose` |

One sibling commit, `ca0ef4a` (Task54Position's position family), landed between
`89b99ab` and `7f76e94`.

## Test counts

| crate | before | after |
| --- | --- | --- |
| ox-editor | 600 (`c41dab3`) | **730** |
| ox-excmd | 158 (`c41dab3`) | **160** |

`PATH=/home/alpha/.cargo/bin:$PATH RUSTC_WRAPPER="" cargo test -p ox-editor
-p ox-excmd -- --test-threads=1`, 0 failed in both. Single-threaded because a
pre-existing env-mutating test flakes in parallel.

Every one of the 16 new commands dispatches rather than reporting
`NotImplemented`, checked through the built binary in one pass
(`ALL DISPATCHED`).

The one remaining warning, `unused_must_use` on `arglist.rs:401`, predates this
task and is untouched.

## The recovery

The shared checkout was destroyed mid-task: `/home/alpha/rewrite` and its `.git`
were deleted while the second audit fix was being edited. The remote held only
`b05106f`, so every local commit was lost. The parent rebuilt the tree as
`c41dab3` from `/tmp/ox56-base`, a detached worktree of mine that happened to
survive, and I re-emitted the E16 commit as `57a0212` from a disk backup plus the
five other file edits held in context. One defect was fixed on the way back in:
the recovered backup carried a duplicated `CommandFlags::RANGE` guard, dead code
from my own earlier edit, now gone.

`.outline/sdd/reports/task-55.md` did not survive and is unrecoverable. Reports
were tracked only through `task-50.md`, so 51–55 were never committed. Task 55's
*code* is intact in `c41dab3`; its write-up is not. The research half is
re-derived below; its `:help` decline rationale should be recovered from that
run's transcript before anyone re-attempts `:help`. The parent has since removed
`.outline/sdd/reports` from `.gitignore`.

## Audit findings against task 55

### 1. Out-of-domain addresses are rejected, not clamped (`57a0212`)

An address past the end of its domain was clamped onto the domain's last
element, so `:99read in.txt` in a three-line buffer inserted at line 3 and
`:5,6delete` resolved to the reversed range `(5, 3)`. Upstream rejects it:
`invalid_range` (`ex_docmd.c:3735-3820`) runs in `do_one_cmd` before the command
function and returns `E16: Invalid range`.

The clamp was shared by twelve `resolve_range` call sites, so the fix went to
the shared site as the finding instructed — and then one level further out, to
the `dispatch` entry, because that is where upstream puts it. `:close` and
`:buffer` never resolve a range at all, so a check inside `resolve_range` would
still have missed them. The first draft made exactly that mistake and the
`:99close` assertion caught it.

Applying the rule needed the address domain, which the generated table did not
carry. `addr_type` now comes from the same `ex_cmds.lua` entries the flags come
from, so it cannot drift; the build script accumulates each entry until its
closing brace because `addr_type` follows `flags` upstream. All 564 entries carry
one and a missing one is a hard build error.

Lines, Arguments, Windows, Tabs and Buffers get upstream's limits. Other,
TabsRelative and Unsigned accept any range. **Named gap:** LoadedBuffers and the
two QuickFix domains are unchecked — this port has neither a number-ordered
buffer load state nor a quickfix list, so there is no limit to compare against.

A post-command *count* still clamps silently, which is upstream's ordering:
`invalid_range` runs at `ex_docmd.c:2209`, before `parse_count(..., true)` at
`2321`, and `set_cmd_count` clamps with the comment "be vi compatible: no error
message for out of range". So `:99print` is an error and `:1print 99` still
prints to the end.

### 2. `:read` fires `readfile`'s autocommands (`a9f3a21`)

`:read {file}` went straight through the `FileIO` seam and the filter forms
returned as soon as the shell exited, so no plugin could observe either.

- File form: `FileReadCmd` fires first and a match **replaces** the read
  (`fileio.c:336-340`); otherwise `FileReadPre` before the insert
  (`fileio.c:640`) and `FileReadPost` after (`fileio.c:1925`), both matched
  against the file name with a null buffer.
- Filter form: reads with `READ_FILTER`, so `FilterReadPre`/`FilterReadPost`
  (`fileio.c:631,1914`), then `ShellFilterPost` (`ex_cmds.c:1236`). These match
  the current buffer's name, as `:help FilterReadPre` documents.
- `:write !cmd` reads nothing back, so `ShellFilterPost` alone.

The post events now fire even for an empty read, because upstream runs them on
the way out of `readfile` regardless.

## The six command groups

### 1. `:tabnew`, `:tabedit`, `:tabonly`, `:vnew` (`7f76e94`)

Tabpage order had to become part of the model first. `tabpages` is a BTreeMap
keyed by handle and `tabpages()` returned its key order, so position was a
function of allocation and nothing could express or later change anything else.
Upstream keeps tabpages in an ordered linked list (`tp_next`), walks it in
`tabpage_index` and reorders it with `:tabmove`. `Editor` now carries
`tab_order` as the order of record with the map as storage, so existing lookups
are untouched and `:tabmove` stops being permanently blocked. Peer
`Task54Position` reached the same conclusion independently.

`create_tabpage_at` implements `win_new_tabpage(after)`
(`window.c:4484-4539`). `close_tabpage` is new and is the **sole owner** of
tabpage removal, which the parent asked me to settle: the window path cannot
take that job here because `Layout::close` refuses `LastWindow`, so
`close_window` is structurally unable to empty a tabpage. Upstream puts removal
in `win_close`, which it can because its layout permits closing a tabpage's last
window.

Addresses now resolve in their own domain. `get_address`
(`ex_docmd.c:3435-3463`) picks `.` and `$` per `addr_type`, so `$` on an
ADDR_TABS command is the last tabpage, not the last buffer line — the bug the
oracle caught, where `:$tabnew` landed at position 2 in a one-line buffer.
`address_domain_bounds` is now the one owner of those values and
`invalid_range`'s limits read from it too.

`:tabonly` resolves its survivor through `get_tabpage_arg`
(`ex_docmd.c:4398-4488`). Its bang is upstream's `forceit`, which only matters
when a modified buffer would be unloaded; closing a tabpage leaves buffers
loaded and hidden, here and upstream under Neovim's default `'hidden'`, so both
forms close the same tabpages — checked against the oracle, not assumed.

### 2. `:undo` and `:redo` (`0260f23`)

**The brief was wrong about `:redo`** and the parent confirmed the correction:
upstream is `u_redo(1)` unconditionally and `redo`'s entry carries neither RANGE
nor COUNT, so `:3redo` is E481, not three redos.

`:undo`'s address is a *sequence number*, not a step count, and seeking may move
forward: `set_cmd_count` folds the COUNT form into the same `line2` the RANGE
form uses, so `:undo 2` and `:2undo` are one command reaching
`undo_time(step, absolute)`. `:undo 0` returns to the original state and an
absent sequence is E830.

That needed real tree navigation. `UndoTree::undo_to_seq` already picked a route
but nothing applied it, so `BufferState::undo_to_seq` and
`Editor::buffer_undo_to_seq` replay it through the same pipeline one-step undo
uses. While wiring it, `undo` and `redo` collapsed onto one `apply_undo_step`;
they had the direction-dependent parts written twice and `undo_to_seq` would have
been a third copy.

"Already at oldest change" and "Already at newest change" (`undo.c:1935,1948`)
did not exist in this port before.

**Named gap:** `:undo!` is `u_undo_and_forget`, which destroys the redo branch it
moves off. This port's `UndoTree` has no forget operation, so the bang reports
NotImplemented rather than behaving as a plain `:undo` and leaving a branch
upstream would have discarded.

### 3. `:retab` (`1e685e5`)

Widths are measured with the *old* `'tabstop'` and rebuilt with the new one,
which is why `:retab 4` turns one eight-column tab into two.
`tabstop_fromto` (`indent.c:220-243`) splits a run into tabs plus a spare-space
remainder; `'expandtab'` makes it all spaces.

Two guards decide whether a run is touched, and both matter: a run without a tab
is skipped unless `!` was given and it is more than one space
(`indent.c:1495`), and even then the rewrite is discarded unless it is no longer
than the original (`indent.c:1509`). That is why `:retab!` turns eight spaces
into a tab but leaves two spaces alone.

`win_chartabsize` already existed as `cell_width` in `builtins/position.rs`; it
is now `pub(crate)` and shared rather than recomputed, so the display-width rule
keeps one owner. The new `'tabstop'` goes through `set_and_mirror`, the dual
editor+eval-scope write `:set` uses — writing only the editor left `&tabstop`
reading the pre-command snapshot, which the oracle caught.

**Named gap:** upstream also accepts a comma list and writes `'vartabstop'`
(`indent.c:1597-1613`). This port has no `'vartabstop'` option at all, so that
form reports NotImplemented instead of silently keeping one value.

### 4. `:hide`, `:sleep`, `:z`, `:scriptencoding`, `:argdelete` (`f946c19`)

The shared count parse had to be fixed first, as the parent directed rather than
special-casing `:sleep`. `take_count` refused digits not followed by whitespace,
so `:sleep 100m` saw no count. Upstream takes them greedily and leaves the
suffix; only an EX_BUFNAME command insists the digits end at whitespace, so
`:buffer 123foo` stays a buffer name (`ex_docmd.c:1401-1403`). BUFNAME and ZEROR
are now exposed, and a zero count without ZEROR is E939 — shared by every COUNT
command.

`:hide` closes a window without freeing its buffer, with `win_find_nr`'s
fall back to the last window. `:sleep` pauses for the count, seconds by default
and milliseconds with `m`, and any other tail is E475 naming only the text after
the count. `:z` prints a window of lines around the address: the leading `-`,
`+`, `=`, `^` or `.` picks the side, repeated signs multiply the distance, and
the size defaults to `'scroll'` doubled for a lone window, the window height
less three otherwise, `'lines'` less one for `:z!`. The `=` form widens by two,
brackets the line with `'columns'`-wide rules and leaves the cursor on it.
`:argdelete` removes by position or by file pattern, reusing
`fs_builtins::wildcard_match` rather than adding a third globbing convention.

**Named gap:** `:scriptencoding` is E167 outside a sourced file; inside one,
upstream sets up a conversion via `convert_setup`, which needs an encoding
converter this port lacks. Every script here is already read as UTF-8, which is
what the only form in the runtime files asks for.

### 5. `:lockvar` and `:unlockvar` (`7b507a1`)

Only the argument parse and the call, as the brief scoped it. Depth defaults to
2, `!` means -1, a leading digit run overrides it, and the names that follow are
walked one per word.

The depth is pinned through the reassignment path rather than `add()`, because
that is where the difference is observable in both binaries: depth 0 locks the
*variable* (E1122) and deeper locks its *value* (E741). A test that could not
tell those apart would not have proved the digits reached the engine.

### 6. `:fold`, `:foldopen`, `:foldclose` (`bebb704`)

**Not declinable, and the parent's instruction here was the important one: do
not land a guard that cannot fail.** A fold model does exist in `fold.rs`, but
`'foldmethod'` was wired to nothing — `Folds::set_method` was called from
nowhere in the tree, so `require_manual()` always passed. The guard now reads
the current window's `'foldmethod'`, so `indent`, `expr`, `syntax` and `diff`
all report E350 and `manual`/`marker` are allowed, exactly as
`foldManualAllowed` (`fold.c:522-533`) decides. The commands also set the
buffer's fold method from that option, so the model follows the option.

Under `marker`, upstream writes the `'foldmarker'` pair into the text; that is
implemented, wrapped in `'commentstring'` when it carries a `%s`.
`:foldopen`/`:foldclose` walk the addressed lines with `!` selecting the
recursive form, and E490 fires only when *no* line had a fold — a fold already
in the requested state counts as found, because `setManualFoldWin` records
DONE_FOLD without DONE_ACTION.

**Named gaps:** `'foldmethod'` is a window option and upstream's fold tree is
per-window (`wp->w_folds`) while this port keeps one `Folds` per buffer, so two
windows on one buffer with different methods cannot both be honoured — the
current window wins. And `foldAddMarker` skips the `'commentstring'` wrap when
the line already ends inside a comment, which needs `skip_comment` and a comment
parser this port lacks.

## Defects the oracle comparisons exposed

Four fixes that were not in the brief. Each was found by running the group's
proof and reading a difference rather than assuming it was cosmetic.

**`83deee2` — window builtins counted every window in the editor.** `winnr()`
and `winnr('$')` should count within one tabpage (`get_winnr`,
`eval/window.c:278-292`); the function's own doc comment already said so.
`screen_cell` had the same fault and could return a character from a window in
another tabpage. Both were *latent* until group 1: before `:tabnew` there was no
way to create a second tabpage. The parent asked me to audit the whole file on
the theory that one such bug usually means several — that was right for
`screen_cell` and wrong for the rest: `tabpagenr`, `win_getid`, `winwidth` and
`winheight` already scope correctly, and `tabpagenr` is right to be editor-wide.
`nvim_list_wins` is also correctly editor-wide (upstream uses
`FOR_ALL_TAB_WINDOWS`).

**`3743c22` and `a5ca06e` — parser error codes and texts.** Five defects, all
plugin-observable through `v:exception`:

- A disallowed bang reported E488 "trailing characters". Upstream is `e_nobang`,
  **"E477: No ! allowed"** — a different *code*. Two tests pinned the wrong code
  (including one from task 55) and were corrected against the oracle.
- E471, E481, E488 and E492 were lower-cased where upstream capitalises.
- E488 for trailing input dropped the offending text; upstream uses
  `e_trailing_arg`, "E488: Trailing characters: %s". Carrying it needed
  `ParseError::message` to become `String`.

`parser_error_texts_match_upstream` pins all five codes and bodies together,
because the existing `error_case!` cases assert the code alone and so could not
have caught the wording — and asserted the wrong code for the bang.

## Mutation check

Every new assertion was proved load-bearing by breaking the code it covers and
confirming it fails. Controls that should keep passing did.

| commit | mutation | result |
| --- | --- | --- |
| `57a0212` | `check_address_domain` → `Ok(())` | 3 of 4 fail; in-range control passes |
| `a9f3a21` | fire helpers neutralised | 3 fail |
| `a9f3a21` | `FileReadCmd` interception removed | the 4th fails |
| `7f76e94` | `create_tabpage_at` ignores `after` | positional test fails |
| `7f76e94` | `address_domain_bounds` domain-blind | domain + positional tests fail |
| `3743c22` | E481 casing reverted | message test fails |
| `0260f23` | `:undo {N}` as N steps | 2 fail |
| `83deee2` | `winnr` global again | 2 fail |
| `1e685e5` | tab-run guard removed | 2 fail |
| `1e685e5` | widths measured with the new tabstop | 3 fail |
| `f946c19` | `take_count` must end at whitespace | 3 `:sleep` tests fail |
| `f946c19` | E939 guard removed | 1 fails |
| `f946c19` | `:z` kind character ignored | 2 fail |
| `7b507a1` | depth argument and bang ignored | depth test fails |
| `bebb704` | E350 guard removed | 2 fail |
| `bebb704` | E490 removed | 1 fails |
| `bebb704` | `:foldopen!` recursion ignored | 1 fails |

The `bebb704` E350 mutation is the one that matters most: it is precisely the
"guard that cannot fail" the parent warned against, and the tests detect it.

## Oracle comparisons

All against `.references/neovim/build/bin/nvim` and `target/debug/oxvim`, same
script, same working directory.

### `:tabnew` group — identical

```
start: tabs=1 cur=1                                    SAME
tabnew: tabs=2 cur=2                                   SAME
tabnew: tabs=3 cur=3                                   SAME
0tabnew: tabs=4 cur=1                                  SAME
tabnew(cur=1of4): tabs=5 cur=2                         SAME
dollar-tabnew: tabs=6 cur=6                            SAME
2tabnew: tabs=7 cur=3                                  SAME
newtab-buffer-name=[]                                  SAME
tabedit-file: line1=filetext cur=4 tabs=8              SAME
vnew: wins=1->2 name=[]                                SAME
tabonly: tabs=1 cur=1                                  SAME
tabonly xyz => E475: Invalid argument: xyz              prefix only
unchanged tabs=3                                       SAME
tabo: tabs=1                                           SAME
```

`vnew: wins=1->2` matched only after `83deee2`; before it this port said `8->9`.

### `:undo` group — identical on a single-header buffer

```
built: [x]              undo: []           redo: [x]
undo 0: []              undo 1: [x]        1undo: [x]
u abbrev: []            u at oldest: []
red abbrev: [x]         redo at newest: [x]
undo 99 => E830: Undo number 99 not found
3redo => E481: No range allowed
```

All twelve lines match; only the exception prefix differs.

### `:retab` — identical

```
retab noet ts8: [>one|        two|    three|a>>b] ts=8          SAME
retab 4: [>>one|        two|    three|a>>>>b] ts=4              SAME
retab with et: [        one|a               b|...] ts=8         SAME
retab!: [>eight|a> b|  x|   y] ts=8                             SAME
retab -indentonly 4: [>>one>two|a> b|  x|   y] ts=4             SAME
retab xyz => E475: Invalid argument: xyz                        prefix only
```

(`>` stands for a tab.) `ts=4` matched only after routing through
`set_and_mirror`.

### `:z`, `:sleep`, `:hide`, `:argdelete` — identical

```
5z   -> [l5/l6/l7/l8/l9/l10] cursor=10        5z3  -> [l5/l6/l7] cursor=7
5z-3 -> [l3/l4/l5] cursor=5                   5z.3 -> [l4/l5/l6] cursor=6
5z^3 -> [l1/l2] cursor=2                      5z+3 -> [l6/l7/l8] cursor=8
5z=3 -> [l3/l4/<rule>/l5/<rule>/l6/l7] cursor=5
sleep 5x => E475: Invalid argument: x          sleep 0m => E939: Positive count required
after split wins=2                             after hide wins=1
args=['a.txt','b.txt','c.txt'] idx=0
argdelete b.txt => ['a.txt','c.txt']           2,3argdelete => ['a.txt','d.txt']
argdelete zzz => E480: No match: zzz           argdelete *.txt => []
bare argdelete on empty => E610: No argument to delete
```

### `:lockvar` — identical

```
locked assign => E741: Value is locked: g:v v=1
after unlockvar v=3
depth 0: reassign=[E1122: Variable is locked: g:l0]
depth 1: reassign=[E741: Value is locked: g:l1]
depth 2: reassign=[E741: Value is locked: g:l2]
lockv abbrev => E741: Value is locked: g:w    unlo abbrev w=5
two names => E741: Value is locked: g:q       bare lockvar => E471: Argument required
```

### `:fold` group — identical on its observable surface

```
fold under indent => E350: Cannot create fold with current 'foldmethod'   SAME
text unchanged: a1|a2|a3|a4|a5|a6                                         SAME
fold under marker: a1{{{|a2|a3}}}|a4|a5|a6                                SAME
foldopen with no fold => E490: No fold found                              SAME
foldclose with no fold => E490: No fold found                             SAME
foldopen on a real fold => []                                             SAME
foldopen again (already open) => []                                       SAME
fold under expr => E350: Cannot create fold with current 'foldmethod'     SAME
```

### `:read` / E16 — identical

```
99read in.txt => E16: Invalid range     99print => E16: Invalid range
5,6delete => E16: Invalid range         2read in.txt => (ok)
buffer=a/b/x/y/c
99resize => (ok)   99close => E16   99buffer => E16   99argdelete => E16
```

### Parser errors — identical bodies

```
print!       => E477: No ! allowed
doesnotexist => E492: Not an editor command
print zz     => E488: Trailing characters: zz
badd         => E471: Argument required
```

## Found, not fixed

Each is outside this task's files or needs its own subsystem. All are recorded
here rather than worked around.

1. **Undo-block grouping (most serious).** This port records one undo header per
   buffer mutation; upstream groups every change made without returning to the
   main loop into one (`u_sync`, `b_u_synced`). A three-line `setline()` is one
   header upstream and three here, so buffer contents after `:undo` diverge for
   any multi-edit script. Measured directly: `undotree().seq_last` is 1 upstream
   where this port makes 3. No per-command fix can reach it, and it is
   plugin-observable. The parent has it in the ledger.
2. **No fold builtins.** `foldclosed()`, `foldlevel()` and `foldclosedend()` do
   not exist, so no plugin can observe a fold at all and the fold group's state
   is untestable through the binary. Those three are the obvious next leaf.
3. **`v:exception` is missing two decorations.** The `Vim({cmd}):` prefix and
   `append_command`'s `: {cmdline}` suffix (`ex_docmd.c:2375-2384,2993-3019`) are
   never emitted, for any command. This accounts for every remaining difference
   in every comparison above. Task 55 found it; any oldtest matching on
   `Vim(cmd):` will still miss.
4. **`redir` drops a leading newline.** The oracle's capture begins with one and
   this port's does not — for `:print` and `:echo` as much as `:z`, so it is
   message plumbing, not a command bug.
5. **Deep locks do not survive the scope-sync seam.** `add()` on a deep-locked
   container raises E741 through the binary but not across separate
   `ExExecutor::execute_line` calls. In `ox-eval`, outside this task's files.
   Upstream also names the argument ("Value is locked: add() argument") where
   this port does not.
6. **`:resize` raises E36 where upstream succeeds.** `:99resize` cannot grow a
   window past the screen upstream and simply does nothing; this port errors.
   Not an addressing bug — the domain test uses `:99bnext` to avoid it.

## Notes for the integrator

- `.outline/GATES.md` and `.outline/sdd/reports/task-12b.md` were never staged,
  nor was `.gitignore` while the parent was editing it.
- No formatter was run and no project-wide suite was run.
- Files touched outside the brief's original list, each with the parent's
  approval recorded above: `editor.rs` (`tab_order`, `create_tabpage_at`,
  `close_tabpage`, `buffer_undo_to_seq`, `buffer_undo_seq`, `tabpage_index`,
  `LastTabpage`), `buffer.rs` (`undo_to_seq`, `apply_undo_step`, `UndoError`),
  `builtins/window.rs` (the tabpage scoping fix), `builtins/position.rs` and
  `fs_builtins.rs` (one visibility change each, to share `cell_width` and
  `wildcard_match` instead of duplicating them).
- `bash` stayed broken for me after the recovery — its persistent shell cannot
  reinitialise once its cwd has been deleted — so every command here ran through
  `subprocess` in the eval kernel with
  `PATH=/home/alpha/.cargo/bin:$PATH RUSTC_WRAPPER=""`.
- `excmd_exec_state_tests.rs:1410` was changed by `Task54Position`, not me: their
  `update_curswant` work made `getcurpos()[4]` 8 rather than 4, and I ceded that
  edit so the expectation would land atomically with the behaviour it pins.
