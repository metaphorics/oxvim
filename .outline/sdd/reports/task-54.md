# Task 54 — Position builtins (`var2fpos` / `list2fpos` port)

## Scope

The twelve position builtins are served from one module,
`crates/ox-editor/src/builtins/position.rs`, because upstream resolves all of
them through `eval.c:var2fpos` and `eval.c:list2fpos`:

`cursor`, `setcursorcharpos`, `getpos`, `getcharpos`, `getcurpos`,
`getcursorcharpos`, `setpos`, `setcharpos`, `col`, `charcol`, `line`,
`virtcol`.

Before this change only `getcurpos`, `setpos` and `virtcol` existed (as
placeholders that echoed the byte column), and `line` was served from
`builtins/buffer.rs` as an Ex-address lookup rather than through `var2fpos`.

## What landed

| File | Change |
| --- | --- |
| `crates/ox-editor/src/builtins/position.rs` | Ported `var2fpos`, `list2fpos`, `getpos_both`, `get_col`, `f_line`, `f_virtcol`, `set_cursorpos`, `set_position`, `setmark_pos`, `check_cursor`/`mb_adjust_cursor`, `getvcol`, and the byte/char index conversions. 109 → 1018 lines. |
| `crates/ox-editor/src/builtins/mod.rs` | Routes all twelve names to `Family::Position`; `line` moved out of `Family::Buffer`. |
| `crates/ox-editor/src/builtins/buffer.rs` | Superseded `call_line_builtin` deleted (clean cutover, no alias). |
| `crates/ox-editor/src/layout.rs` | `WindowState` gains `curswant`, `set_curswant`, `coladd` (upstream `w_curswant`, `w_set_curswant`, `pos_T.coladd`). |
| `crates/ox-editor/src/editor.rs` | `Editor::window_mut`, the mutable counterpart of `Editor::window`. |
| `crates/ox-editor/Cargo.toml`, `Cargo.lock` | `unicode-width` for `plines.c:charsize_fast_impl` cell widths. |
| `crates/ox-editor/src/position_tests.rs` | New: 70 behavioral tests. |
| `crates/ox-editor/src/lib.rs` | Registers `position_tests`. |
| `crates/ox-editor/src/excmd_exec_state_tests.rs` | One expectation, see "Cross-file change" below. |

## The four audit findings

All four were raised by the reviewer against the pre-loss implementation and
are fixed here. Each was **mutation-verified**: the fix was reverted, the
scoped suite re-run, and the naming tests confirmed to fail.

### 1. `setpos('.', [0, lnum, col, off])` must not touch `w_set_curswant`

`funcs.c:set_position` only assigns `w_curswant` and `w_set_curswant` when the
list carried a fifth element. Passing `false` unconditionally froze a stale
wanted column into `getcurpos()[4]`. `place_cursor` now takes `Option<bool>`,
`None` for the four-element `'.'` form and `Some(flag)` for `set_cursorpos`,
which always writes the flag.

Mutating `None` to `Some(false)` fails three tests:
`getcurpos_refreshes_the_wanted_column_from_the_virtual_column` (9 becomes 1),
`setpos_four_element_list_leaves_the_wanted_column_live` (10 becomes 1), and
`getcurpos_puts_the_cursor_on_the_last_cell_of_a_tab`.

### 2. A lowercase mark is written into the buffer `fnum` names

`mark.c:setmark_pos` writes `buf->b_namedm` of the buffer it looked up, not
`curbuf`. `set_mark` now stores into that buffer.

Mutating back to `win.buffer` fails
`setpos_lowercase_mark_lands_in_the_buffer_fnum_names`, where the mark appears
in the current buffer as `[0, 1, 3, 0]` instead of `[0, 0, 0, 0]`.

### 3. A nonexistent `fnum` fails, with no phantom mark

`mark.c:setmark_pos` runs `buflist_findnr(fnum)` and returns `FAIL` when the
buffer does not exist. A new `buflist_findnr` helper gates both the global and
the lowercase branch. The previous-context marks `'` and `` ` `` are answered
*before* the lookup, exactly as upstream orders it, because they live in the
window (`w_pcmark`) rather than in a buffer.

Mutating the guard away fails
`setpos_global_mark_in_a_nonexistent_buffer_fails_and_stores_nothing`, where
the call reports 0 and `getpos("'A")` reads the phantom mark.

### 4. A character column converts against the `fnum` buffer's line

`eval.c:list2fpos` calls `buflist_findnr(fnum)` and
`buf_charidx_to_byteidx(buf, lnum == 0 ? cursor line : lnum, n)`. Reading
`win.line(...)` produced the wrong byte column for multibyte text in another
buffer, and did not fail for a buffer that does not exist. `list2fpos` now
takes `&Editor`, resolves the buffer, and reuses the already-snapshotted lines
when the buffer is the window's own, so the common case makes no second
full-buffer copy.

Mutating back to `win.line(...)` fails two tests:
`setcharpos_converts_the_character_index_against_the_named_buffer`, which
reads `[2,1,3,0]` instead of `[2,1,7,0]`, and
`setcharpos_does_not_use_the_current_buffer_line`.

## Oracle expectations

`.references/neovim` was destroyed and re-cloned during this task, so the
oracle binary was never rebuilt. Every recorded expectation below came from
the real binary before the loss, driven by the surviving scripts in
`/home/alpha/oxvim-recovery/t54/`, and each was re-derived from the upstream
C source (now readable again at `.references/neovim/`) before being asserted.

| Script | Recorded | Test |
| --- | --- | --- |
| `f1.vim` | `setpos('.',[0,1,5,0])` → `getcurpos()` `[0,1,5,0,9]` | `getcurpos_refreshes_the_wanted_column_from_the_virtual_column` |
| `f1.vim` | `setpos('.',[0,1,5,0,3])` → `[0,1,5,0,3]` | `setpos_five_element_list_pins_the_wanted_column` |
| `f1b.vim` | `cursor(2,1)`; `setpos('.',[0,2,3,0])` → `[0,2,3,0,10]` | `setpos_four_element_list_leaves_the_wanted_column_live` |
| `f2.vim` | cross-buffer `'a` → `[0,1,3,0]` there, `[0,0,0,0]` here | `setpos_lowercase_mark_lands_in_the_buffer_fnum_names` |
| `f3.vim` | `setpos("'A",[9999,1,1,0])` → `-1`, `getpos` `[0,0,0,0]` | `setpos_global_mark_in_a_nonexistent_buffer_fails_and_stores_nothing` |
| `f4.vim` | `setcharpos("'A",[2,1,3,0])` → `getpos` `[2,1,7,0]` | `setcharpos_converts_the_character_index_against_the_named_buffer` |

## Tests

**70 tests landed** in `crates/ox-editor/src/position_tests.rs` (the pre-loss
suite had 60; coverage was prioritised over matching the old count). They
cover, per builtin family:

* `getcurpos`/`getcursorcharpos` — wanted-column refresh, pinned wanted
  column, tab last-cell rule, `0` window argument, unknown window, background
  window reading `w_curswant` raw.
* `setpos`/`setcharpos` — cursor writes, `fnum` ignored for `'.'`,
  `check_cursor` clamping, empty line, multibyte head-byte adjust, `coladd`,
  list length bounds (3..5), negative components, non-list argument, negative
  `off`, all four mark classes, `E474` for an unknown name.
* `cursor`/`setcursorcharpos` — byte and character forms, zero line, virtual
  offset, `'$'` line, `E474` shapes.
* `getpos`/`getcharpos` — `.`, `$`, `v`, unset mark, list form column bounds,
  `'$'` column, line outside the buffer, character-column counting.
* `col`/`charcol` — byte vs character column, `'$'`, unset mark, mark in
  another buffer answering 0, `E1210`/`E1222` argument types.
* `line` — `.`, `$`, `w0`, `w$`, mark, unset mark.
* `virtcol` — tab span, wide character, unset mark (scalar and list result),
  window argument, unknown window.
* Arity — all twelve names against the generated `eval.lua` table
  (`E118`/`E119`), plus the no-argument cursor readers.

## Cross-file change (coordinated)

`crates/ox-editor/src/excmd_exec_state_tests.rs:1410` is owned by
`Task56ExCommands`. Its `position_builtins_round_trip_and_expand_tabs`
expected `getcurpos()[4] == 4`, the byte column leaking through the
placeholder. With `update_curswant` ported, `"the\tquick"` with `'ts'=8` spans
the tab over virtual columns 4-8, Normal mode sits on a tab's last cell, so
`w_curswant` is 7 and `getcurpos()` answers 8. Task56 was asked and replied
"You take it — make that one edit yourself, in the same commit", because the
expectation and the behaviour it pins must land atomically. Only that one
assertion changed.

## Verification

```
PATH="/home/alpha/.cargo/bin:$PATH" RUSTC_WRAPPER="" cargo build --workspace
    Finished `dev` profile

PATH="/home/alpha/.cargo/bin:$PATH" RUSTC_WRAPPER="" \
  cargo test -p ox-editor --lib -- --test-threads=1
    test result: ok. 678 passed; 0 failed; 0 ignored
```

`--test-threads=1` is required: `excmd_exec_function_tests::
language_ctype_leaves_lang_and_messages_env_untouched` mutates process
environment and flakes under the parallel runner. That is pre-existing and
unrelated to this change.

`cargo clippy -p ox-editor --lib --all-targets` reports zero findings against
`builtins/position.rs`, `layout.rs`, `editor.rs` or `position_tests.rs`; the
269 `unwrap_used` errors it does report all sit in pre-existing files
(`input_tests.rs`, `mode_tests.rs`, `fs_builtins.rs`, `job.rs`,
`excmd_exec.rs`, `arglist.rs`, `excmd_exec_control_tests.rs`).

Not pushed: the repository token is invalid, so the work is committed locally
only.

## Honest gaps

Named rather than faked:

* **Visual marks.** `'<`, `'>`, `'[`, `']` and `'"` are not stored by
  ox-editor, so `setpos` on them answers `-1` — the failure upstream answers
  for a name it cannot set — instead of silently pretending to succeed.
  `setpos_unmodelled_mark_name_fails` pins that.
* **`w$`.** ox-editor tracks only `topline`, so the last displayed line is
  derived as `topline + height - 1` without wrap or fold accounting, the way
  the `H`/`L` motions derive it. Upstream reads `w_botline`.
* **`'v` in Visual mode.** Visual state lives in the mode machine, not in
  editor state, so the eval host only ever reaches upstream's
  Visual-inactive branch, where `'v'` answers the cursor.
* **Previous-context marks.** Upstream `w_pcmark` is window-local; ox-editor
  models `'` and `` ` `` as buffer-local marks of the current buffer. The
  observable `setpos`/`getpos` shape matches; the storage location differs.
* **`set_cursorpos` error reporting.** Upstream `semsg(e_invarg2)` on a
  negative line number, then falls through to return `-1`. This port raises
  `E475` as an exception instead. Kept as the reviewed implementation had it;
  flagged here rather than changed without an oracle to confirm the message
  and abort semantics.
* **`virtcol` `'showbreak'`.** Continuation-row cells are added arithmetically
  from window width and `'showbreak'` length; upstream measures them through
  the real line-wrap machinery, which folds and `'breakindent'` also feed.
