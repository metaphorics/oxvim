# Task 64: upstream undo-block grouping

Status: **DONE_WITH_CONCERNS**. The port groups undo the way upstream does, and
`:undo`, `:redo`, `:undojoin`, `undotree()` and `changenr()` are consistent with
the new grouping. 10 of 12 oracle cases match byte for byte; the two that do not
are named below with their causes, both outside the undo seam.

Base: `9e119d5`. Commits, local only (the push token is invalid; nothing was
pushed and no push was attempted):

| SHA | subject |
| --- | --- |
| `3ed6951` | `feat(ox-text): group every change between main-loop returns into one undo block` |
| `7d8ac97` | `feat(ox-editor): run :undojoin, undotree() and changenr()` |
| `a6f078f` | `fix(ox-editor): run a mapped Ex command under feedkeys()` |

## The upstream rule, from source

**What starts a new block.** `u_savecommon` allocates a header only when
`buf->b_u_synced` is true (`undo.c:389`, comment at `:388`: "If buf->b_u_synced
is true make a new header"), links it as the newest header
(`undo.c:445-499`), and clears the flag before returning (`undo.c:616`).

**What joins the current one.** With `b_u_synced` false the same function takes
the `else` branch at `undo.c:500` and pushes another `u_entry_T` onto the open
header's list (`uep->ue_next = buf->b_u_newhead->uh_entry; uh_entry = uep`,
`undo.c:610-611`). The list is therefore newest-first, which is also the order
the edits have to be undone in. There is no per-command bookkeeping anywhere:
the flag is the whole mechanism.

**What `b_u_synced` means.** `buffer_defs.h:499`, "entry lists are synced": the
newest header is closed and the next change starts a new one. `undotree()`
exposes it directly (`undo.c:3255`).

**Where `u_sync` is called from.** `u_sync(force)` (`undo.c:2704-2717`) is
idempotent — it returns immediately when already synced — and closes the header
through `u_getbot`, which sets `b_u_synced = true` (`undo.c:2922`). Call sites:

- `may_sync_undo` (`input.c:1300-1306`), reached from `gotchars`
  (`input.c:1255`) and from the main loop's `K_EVENT` handling
  (`state.c:92`). This is the boundary that matters. `gotchars` is called only
  for bytes past `typebuf.tb_maplen` (`input.c:2495-2497`), i.e. **typed**
  keys, never keys a mapping produced. `may_sync_undo` additionally skips
  Insert and Cmdline mode unless a cursor key moved the caret, and skips
  entirely while reading a `-s` script (`curscript >= 0`).
- `u_undo` and `u_undo_and_forget` sync before undoing and force the count to
  1 when a block was open (`undo.c:1825-1828`, `1855-1858`); `u_undoredo`
  leaves the tree synced (`undo.c:1665`); `undo_time` the same
  (`undo.c:1983`).
- leaving a buffer: `win_enter_ext` "sync undo before leaving the current
  buffer" (`window.c:5275-5279`), `do_buffer` (`buffer.c:1743-1750`),
  `do_ecmd` (`ex_cmds.c:2597`, `2726`), `buf_reload` (`fileio.c:3169`).
- `'undolevels'` changes (`option.c:2837-2849`), `CTRL-G u` in Insert mode
  (`insert.c:3044`), writing the undo file (`undo.c:1296`), `:earlier`/`ctx`
  restore (`undo.c:3350`), `setline()`/`append()` when coming from Insert mode
  via `u_sync_once` (`eval/buffer.c:192-197`, `636-638`), and command preview
  (`ex_getln.c:2605`).

**How the main loop's return participates.** It participates only through the
*next typed character*. `state_enter` does not sync on every iteration; the
sync happens when a typed key is fetched. So everything one command does —
however many mutations, however deep the call chain — lands in one block, and
the next key the user types opens the next one.

**Scripted mutations inside one command versus separate commands.** This is the
part the brief's case list assumed differently, and the oracle settles it: a
sourced script never returns to a typed-key read, so **every** change a script
makes joins one block until something explicitly syncs. Two separate
`:call setline()` *command lines* in a script are one undo step, not two
(case 2 below, both binaries). Two commands *typed* at the prompt — or queued
through `feedkeys()`, which is typed input as far as `gotchars` is concerned —
are two steps (case 2b). `:normal!`, a mapping's keys, `:g//d` and a function
call are all inside one command and group (cases 3, 5, 5b, 6).

## Where the boundary lives in this port

The undo seam is `BufferState::replace_lines`/`append_lines`
(`crates/ox-editor/src/buffer.rs`), which splice marks and derived state and
then call `UndoTree::record`. The synchronisation point could have been a flag
each of those callers sets, which is the same class of defect as the one being
fixed, so it is not:

- **`UndoTree` owns `synced`** (`crates/ox-text/src/undo.rs`). `record` joins
  the open header when the flag is clear and allocates one when it is set;
  `undo`, `redo`, `redo_branch` and `undo_to_seq` set it, mirroring
  `u_undoredo`. A caller cannot forget any of this because it is not a caller's
  job — recording *is* the flag transition.
- **One `sync` entry point.** `UndoTree::sync` → `BufferState::sync_undo` →
  `Editor::sync_buffer_undo`/`sync_current_undo`. Two callers, both mirroring
  an upstream site: `ModeMachine::may_sync_undo` for a consumed typed key, and
  `Editor::set_window_buffer` for the buffer a window leaves.
- **`ModeMachine::check`** is this port's `gotchars`: it already carries
  `TypeaheadFlags::mapped`, so the typed-versus-mapped distinction upstream
  gets from `tb_maplen` was already available. The mode gate skips Insert and
  Cmdline.
- **`'modified'`** compares `(header sequence, edits in that header)` against
  the saved pair. The sequence alone cannot distinguish a saved state from a
  later edit that joined the same still-open block, and that would have made
  `:w` followed by another scripted change report an unmodified buffer.

Named gap: upstream also syncs inside Insert mode once a cursor key has moved
the caret (`Ins.moved != kInsNone`). This port's insert mode has no cursor-key
handling to set that state, so there is nothing to read; `may_sync_undo` is
where it goes when there is.

## The oracle table

Identical scripts, both binaries, each case in its own process with a fresh
throwaway `HOME` and a file loaded with `:edit` so the buffer starts with no
history. `Rec` prints `changenr()`, `undotree().seq_cur`, `.seq_last`,
`.synced`, `len(.entries)` and the buffer.

**10 of 12 match byte for byte.** The two that do not are `4b` and `7b`, and in
both the undo numbers agree — the divergence is elsewhere.

### 1 setline over three lines, then undo — MATCH

```
nvim :
after setline x3       chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['A', 'B', 'C']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']

oxvim:
after setline x3       chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['A', 'B', 'C']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']
```

### 2 two :call setline() commands in a script, then undo twice — MATCH

```
nvim :
after setline 1        chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['A', 'b', 'c']
after setline 2        chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['A', 'B', 'c']
after undo 1           chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']
after undo 2           chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']

oxvim:
after setline 1        chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['A', 'b', 'c']
after setline 2        chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['A', 'B', 'c']
after undo 1           chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']
after undo 2           chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']
```

### 2b two typed commands, then undo twice — MATCH

```
nvim :
after typed x          chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['a', 'bb', 'cc']
after typed dd         chg=2   seq_cur=2   seq_last=2   synced=0 entries=2  ['bb', 'cc']
after undo 1           chg=1   seq_cur=1   seq_last=2   synced=1 entries=2  ['a', 'bb', 'cc']
after undo 2           chg=0   seq_cur=0   seq_last=2   synced=1 entries=2  ['aa', 'bb', 'cc']

oxvim:
after typed x          chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['a', 'bb', 'cc']
after typed dd         chg=2   seq_cur=2   seq_last=2   synced=0 entries=2  ['bb', 'cc']
after undo 1           chg=1   seq_cur=1   seq_last=2   synced=1 entries=2  ['a', 'bb', 'cc']
after undo 2           chg=0   seq_cur=0   seq_last=2   synced=1 entries=2  ['aa', 'bb', 'cc']
```

### 3 :normal! making several changes in one command — MATCH

```
nvim :
after normal ddx2      chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['3', '4', '5']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['1', '2', '3', '4', '5']

oxvim:
after normal ddx2      chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['3', '4', '5']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['1', '2', '3', '4', '5']
```

### 4 insert session with several inserted lines, then undo — MATCH

```
nvim :
after insert session   chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['a', 'x', 'y', 'z', 'b', 'c']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']

oxvim:
after insert session   chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['a', 'x', 'y', 'z', 'b', 'c']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']
```

### 4b :normal! with \<Esc>/\<CR> string escapes — DIVERGES

```
nvim :
after insert 3 lines   chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['x', 'y', 'z', 'a', 'b', 'c']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']

oxvim:
after insert 3 lines   chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['x��Ry��Rz��E', 'a', 'b', 'c']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']
```

### 5 change from a mapping (two changes in one mapping) — MATCH

```
nvim :
after mapped ddx       chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['b', 'cc']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['aa', 'bb', 'cc']

oxvim:
after mapped ddx       chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['b', 'cc']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['aa', 'bb', 'cc']
```

### 5b change from a function called by a mapping — MATCH

```
nvim :
after mapped func      chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['F1', 'F2', 'c']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']

oxvim:
after mapped func      chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['F1', 'F2', 'c']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']
```

### 6 :g/pattern/d over multiple lines, then undo — MATCH

```
nvim :
after g/x/d            chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['y', 'y']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['x1', 'y', 'x2', 'y', 'x3']

oxvim:
after g/x/d            chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['y', 'y']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['x1', 'y', 'x2', 'y', 'x3']
```

### 7 undojoin joins the next change into the closed block — MATCH

```
nvim :
after typed x          chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['a', 'bb', 'cc']
after typed dd         chg=2   seq_cur=2   seq_last=2   synced=0 entries=2  ['bb', 'cc']
after undojoin setline chg=2   seq_cur=2   seq_last=2   synced=0 entries=2  ['J', 'cc']
after undo             chg=1   seq_cur=1   seq_last=2   synced=1 entries=2  ['a', 'bb', 'cc']

oxvim:
after typed x          chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['a', 'bb', 'cc']
after typed dd         chg=2   seq_cur=2   seq_last=2   synced=0 entries=2  ['bb', 'cc']
after undojoin setline chg=2   seq_cur=2   seq_last=2   synced=0 entries=2  ['J', 'cc']
after undo             chg=1   seq_cur=1   seq_last=2   synced=1 entries=2  ['a', 'bb', 'cc']
```

### 7b undojoin after undo is E790 — DIVERGES

```
nvim :
after setline          chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['A', 'b', 'c']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']
undojoin caught: Vim(undojoin):E790: undojoin is not allowed after undo

oxvim:
after setline          chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['A', 'b', 'c']
after undo             chg=0   seq_cur=0   seq_last=1   synced=1 entries=1  ['a', 'b', 'c']
undojoin caught: E790: undojoin is not allowed after undo
```

### 8 undotree()/changenr() across redo, branch and :undo N — MATCH

```
nvim :
after typed x          chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['a', 'bb', 'cc']
after typed dd         chg=2   seq_cur=2   seq_last=2   synced=0 entries=2  ['bb', 'cc']
after undo             chg=1   seq_cur=1   seq_last=2   synced=1 entries=2  ['a', 'bb', 'cc']
after redo             chg=2   seq_cur=2   seq_last=2   synced=1 entries=2  ['bb', 'cc']
after undo again       chg=1   seq_cur=1   seq_last=2   synced=1 entries=2  ['a', 'bb', 'cc']
after branch change    chg=3   seq_cur=3   seq_last=3   synced=0 entries=2  ['', 'bb', 'cc']
after undo 1           chg=1   seq_cur=1   seq_last=3   synced=1 entries=2  ['a', 'bb', 'cc']
after undo 3           chg=3   seq_cur=3   seq_last=3   synced=1 entries=2  ['', 'bb', 'cc']
entries: [1, 3]
alt of last: [2]

oxvim:
after typed x          chg=1   seq_cur=1   seq_last=1   synced=0 entries=1  ['a', 'bb', 'cc']
after typed dd         chg=2   seq_cur=2   seq_last=2   synced=0 entries=2  ['bb', 'cc']
after undo             chg=1   seq_cur=1   seq_last=2   synced=1 entries=2  ['a', 'bb', 'cc']
after redo             chg=2   seq_cur=2   seq_last=2   synced=1 entries=2  ['bb', 'cc']
after undo again       chg=1   seq_cur=1   seq_last=2   synced=1 entries=2  ['a', 'bb', 'cc']
after branch change    chg=3   seq_cur=3   seq_last=3   synced=0 entries=2  ['', 'bb', 'cc']
after undo 1           chg=1   seq_cur=1   seq_last=3   synced=1 entries=2  ['a', 'bb', 'cc']
after undo 3           chg=3   seq_cur=3   seq_last=3   synced=1 entries=2  ['', 'bb', 'cc']
entries: [1, 3]
alt of last: [2]
```


### Cases that could not be matched

**`4b` — `:normal!` with `\<Esc>`/`\<CR>` string escapes.** Every undo reading
matches (one block, `seq_last` 1, `undo` restores the original three lines);
the *text* does not, because `"\<Esc>"` and `"\<CR>"` reach the buffer as the
literal `K_SPECIAL` bytes instead of acting as keys. The cause is the
`\<Key>` string escape (ox-eval, owned by another leaf) plus the `:normal`
argument path, which lossily converts the `0x80` byte to U+FFFD before
`ModeMachine::feed_keys` sees it. Nothing to do with grouping: case 4 runs the
same insert session with real `ESC`/`CR` bytes through `feedkeys()` and matches
exactly. Not adjusted to fit — it is left in the table as a divergence.

**`7b` — the `v:exception` prefix.** The `E790` code and message body match;
upstream reports `Vim(undojoin):E790: ...` and this port reports
`E790: ...`. This is **pre-existing and general**, not specific to
`:undojoin`: `:foldopen` gives `Vim(foldopen):E490: No fold found` upstream
against `E490: No fold found` here, and `:99print` gives
`Vim(print):E16: Invalid range:   99print` against `E16: Invalid range`. It
affects every Ex command that raises, so it belongs to the exception-formatting
seam rather than to this task.

Two further observations from building the table, neither an undo defect and
neither fixed here:

- Normal-mode `J` and `D` are not implemented, so the first drafts of case 8
  recorded no change at all. The case was rewritten around `x` and `dd`, which
  both binaries agree on, and still exercises branch creation, `alt` and
  `:undo {N}`.
- `oxvim` refuses to start when a `'runtimepath'` entry contains a comma
  (`invalid value for option 'runtimepath': duplicate comma-list item`) where
  upstream starts fine; this only showed up because the harness put the case
  name in the directory path.

## Test counts

`PATH=/home/alpha/.cargo/bin:$PATH RUSTC_WRAPPER="" cargo test -p ox-editor -p
ox-text -- --test-threads=1`

| crate | before | after |
| --- | --- | --- |
| ox-editor | 758 | **767** |
| ox-text | 18 | **23** |

0 failed in both.

### Mutation checks

Every new assertion was proved load-bearing by breaking the code it covers,
from a byte copy of the single file, and restoring it afterwards.

| mutation | fails |
| --- | --- |
| `UndoTree::record` always allocates a header (drop the join branch) | ox-text: `unsynced_edits_join_one_undo_block` ("an open block must not allocate a sequence", left 2 right 1), `undojoin_reopens_the_newest_block_but_never_after_an_undo`, `ox_text_writes_a_grouped_block_real_nvim_undoes_it_in_one_step`; ox-editor: all five grouping tests |
| `may_sync_undo` never syncs | `a_typed_key_closes_the_block_so_two_commands_are_two_steps`, `undojoin_puts_the_next_change_in_the_previous_block`, `undotree_reports_the_branch_shape_and_the_sync_flag` (entries `[2]` instead of `[1, 3]`) |
| `may_sync_undo` syncs mapped keys too | `a_mapping_making_two_changes_is_one_undo_block` (left 2 right 1); the typed-key test still passes, so the two halves are distinguished |
| `'modified'` compares the sequence only | `a_change_joining_a_saved_block_still_sets_modified`; `undo_away_from_saved_point_sets_modified` and `undo_to_initial_saved_point_clears_modified` keep passing |
| drop `undojoin`'s `active_child` (curhead) rejection | ox-editor `undojoin_after_an_undo_raises_e790` ("expected E790, got Ok"), ox-text `undojoin_reopens_...`. The first draft of both tests only reached the *root* rejection and this mutation survived them; both now cover an undo that stopped on an earlier header as well |
| `apply_undo_step` replays a block forwards when undoing | `a_global_delete_is_one_undo_block` (`["x1","x2","x3","y","y"]`), `a_mapping_making_two_changes_is_one_undo_block` (`["bb","cc","cc"]`) |
| undo file writes grouped entries oldest-first | `ox_text_writes_a_grouped_block_real_nvim_undoes_it_in_one_step`: real Neovim produces `b\na\nc` instead of `a\nb\nc`. The first draft of this fixture used three independent single-line replacements, which are order-insensitive, and the mutation survived it; the fixture is now two deletes of line 1 |

## Oldtest

`test_undo.vim`, measured the same way both times: a fresh `shutil.copytree`
of `testdir` per run under `/tmp/t64/old-<tag>`, with `HOME`, `TMPDIR` and the
four `XDG_*` roots inside that throwaway root, `cwd` at the copy, stdin
`/dev/null`, `timeout 250`:

```
<binary> -u NONE -i NONE --noplugin --headless \
  --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_undo.vim
```

The "before" binary is `cargo build -p oxvim` in a detached worktree at `HEAD`
(`9e119d5`), so it carries the sibling commits that landed during this leaf but
none of this work. `.references` was never run in and `HOME` never pointed at a
real home directory.

| file | before | after |
| --- | --- | --- |
| `test_undo.vim` | 41 executed / 33 failed / 5 skipped | **41 executed / 29 failed / 5 skipped** |

Four fixed, none broken:

- `Test_undojoin_after_undo` — `:undojoin` existed only as `NotImplemented`.
- `Test_undo_line_backspace_after_insert_cmd_edit`
- `Test_undo_line_backspace_after_insert_cmd_cursor_movement`
- `Test_undo_line_backspace_after_insert_func_edit`

The last three are the grouping fix itself: each makes several changes inside
one command and then undoes once.

Of the 29 remaining failures, `Test_undotree`, `Test_undotree_bufnr` and
`Test_undojoin`/`Test_undojoin_noop` no longer raise `E117` for the builtins
this task added but still fail on other requirements in the same functions;
the bulk of the rest are persistent undo (`:wundo`, `:rundo`, `undofile()`,
`'undodir'`), `:undolist`, `:earlier`/`:later` and `test_settime()`, none of
which this task touched.

## Left for someone else

- `:undo!` is still `NotImplemented`: it is `u_undo_and_forget`, and
  `UndoTree` has no forget operation (unchanged from task 56).
- Persistent undo has no reader and no `:wundo`/`:rundo`/`undofile()`, so
  `has('persistent_undo')` stays 0. Confirmed with `Task63HasFeatures`.
- `:undolist` (`ex_undolist`, `undo.c:2720`) and `:earlier`/`:later`
  (`undo_time` with a relative or time target) are absent. The tree now
  exposes everything `:undolist` needs through `entries()`.
- `undotree()`'s `save_last`/`save_cur` are zero because the tree carries no
  `uh_save_nr`; that is the same missing per-header save counter `:undolist`
  would print.
- `v:exception` is missing upstream's `Vim({command}):` prefix for every Ex
  command that raises, and `E16` drops the offending text. Reproduced above
  with `:foldopen` and `:99print`.
- The `\<Key>` string escape reaches the buffer as literal `K_SPECIAL` bytes
  through `:normal`; see case `4b`.
- Normal-mode `J` and `D` are unimplemented.
- `'runtimepath'` rejects an entry containing a comma at startup where
  upstream accepts it.
