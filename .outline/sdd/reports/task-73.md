# Task 73: the trailing-garbage class, closed at the shared seam

Status: **done.** All 12 constructs task 71 measured now answer the oracle's error code, plus the
four (`:if`/`:while`/`:for`/`:return`) that could not be measured at all before, plus three
neighbouring seams found while closing them.

Oracle for every measurement: `.references/neovim/build/bin/nvim`, v0.13.0-dev-1390, API level 15.
`before` is `/tmp/oxvim-t73-before`, a detached worktree at `58182c8` with its own
`CARGO_TARGET_DIR`; `after` is `/tmp/oxvim-t73-after`, detached at `d94388d`, likewise isolated.
Every probe and oldtest run used a freshly created throwaway `HOME`, isolated `XDG_*`/`TMPDIR`,
`VIMRUNTIME` set explicitly, stdin from `/dev/null`, and its own copy of `testdir` under `/tmp`
with the committed stale `test.log` removed. Nothing ran inside `.references`.

| sha | subject |
| --- | --- |
| `afdc39f` | fix(ox-eval): answer E488 for a refused byte after a complete expression |
| `c6d7a23` | fix(ox-editor): keep a sourced line's trailing CR |
| `5eee4e3` | fix(ox-editor): run execute()'s List argument line by line |
| `d94388d` | fix(ox-editor): trim only the white space skipwhite trims before eval0 |

`cargo test --workspace -- --test-threads=1` at `d94388d`, zero failures:
**ox-editor 806 → 821, ox-eval 474 → 478, ox-excmd 162 → 162, ox-text 23 → 23, oxvim 61 → 62.**
(`ox-editor`'s 806 → 821 includes two tests from `Task72Crashes`'s `f322fa9`/`280352e`, which
interleave with these commits; twelve of the fifteen are mine.)

---

## 1. The fix, and why it is two halves

Task 71 §5 located it and named both halves. Both were needed, and neither is sufficient.

**Half one — `str::trim` is not `skipwhite`.** Rust's `trim` removes CR, VT, FF, NL and every
Unicode space. `skipwhite` and `del_trailing_spaces` (`strings.c:429-446`, `ascii_defs.h:84-87`)
remove ASCII space and tab, and nothing else. A new `skipwhite_trim` replaces `str::trim` at every
site that hands an expression to `eval_text`: `:while`, `:eval`, `:throw`, `:return` (both the
emptiness test and the evaluation), `:call`, the `:if` and `:elseif` conditions,
`split_assignment` (`:let`/`:const`), `split_for` and `strip_expression_comment`.

**Half two — eager tokenizing turns E488 into E15.** With the trims narrowed, `4\r` reaches
`Lexer::tokenize`, which failed on the unknown byte *before* `parse_expr1` finished, so the answer
was E15 where `eval0` (`eval.c:1234-1252`) parses `4`, stops, and reports the remainder. Upstream
never lexes past the expression it is parsing. `Lexer::tokenize_tolerant` now stops at the first
byte it cannot start a token from, keeps the error, and returns the tokens so far with an `Eof`
sitting on the refused offset. `Parser::parse` treats that `Eof` as remainder — E488 with the
bytes from there — when the expression completed in front of it, and surfaces the lexer's own
error when the expression actually needed the byte. `parse_many` keeps answering E15, because
`:echo`/`:echomsg`/`:execute` loop `eval1` until the line is spent and so do reach the refused
byte.

One detail cost a debugging round and is worth recording: the choice between the two answers must
be made against **the offset the lexer stopped at**, not the offset inside the error it produced.
`$"moo}"` refuses at byte 0 but reports E1278 against byte 5; comparing against the error's own
offset silently reinstated the parser's E15 and broke
`malformed_interpolated_strings_report_typed_errors`. `refusal_is_resolved_against_the_stop_offset_not_the_error_offset`
now pins it.

### Three neighbouring seams, all in the same class

- **`:unlet`** never reaches `eval_text`. `ex_unletlock` (`eval/vars.c:1600-1617`) separates
  targets with `skipwhite`/`skiptowhite` and then requires the byte after the name to be white
  space or `ends_excmd`. The port split on Rust white space, so a CR or VT after a name was
  dropped instead of being E488. Now split on space/tab, with `unlet_name_garbage` for the
  non-`$` branch. (`Task72Crashes`'s `$ENV`/E475 branch is untouched and still runs first.)
- **A `-nargs=0` user command** with any argument answered `E471: Argument required`, which is
  the opposite complaint. `-nargs=0` clears `EX_EXTRA`, so `do_one_cmd` raises E488 before the
  body runs (`ex_docmd.c:4542`). `-nargs=1` with no argument is still E471.
- **`:call`'s** own E488 did not name the remainder; `ex_call` does.

## 2. The oracle table, before and after

One `execute()` per probe, one process per probe, on each binary, the error read out of
`v:exception`. `<CR>` is a raw `0x0d`, `<VT>` `0x0b`, `<FF>` `0x0c`. Rows 1-12 are task 71 §5's
twelve; 13-15 are the ones it could not measure; 16-24 were added here.

| # | probe | nvim | ox before | ox after |
| --- | --- | --- | --- | --- |
| 1 | `let g:v = 4<CR>` | `E488: Trailing characters:` | **ok, `g:v` set** | `E488: Trailing characters:` |
| 2 | `const g:c = 4<CR>` | `E488` | **ok** | `E488` |
| 3 | `let g:v = 4<VT>` | `E488` | **ok** | `E488` |
| 4 | `eval 4<CR>` | `E488` | **ok** | `E488` |
| 5 | `unlet g:z<CR>` | `E488` | **ok** | `E488` |
| 6 | `call len('a')<CR>` | `E488` | **ok** | `E488` |
| 7 | `T73D<CR>`, `-nargs=0` | `E488` | **ok** | `E488` |
| 8 | `let g:v = 4 x` | `E488: … x` | `E488: … x` | `E488: … x` |
| 9 | `throw 'a'<CR>` | `E488` | **throws `a`** | `E488` |
| 10 | `echo 'z'<CR>` | `E15` | `E15` | `E15` |
| 11 | `execute '…'<CR>` | `E15` | `E15` | `E15` |
| 12 | `echomsg 'q'<CR>` | `E15` | `E15` | `E15` |
| 13 | `if 1<CR>` … `endif` | `Vim(if):E488` | **E492 — unmeasurable** | `Vim(if):E488` |
| 14 | `while 0<CR>` … `endwhile` | `Vim(while):E488` | **E492 — unmeasurable** | `Vim(while):E488` |
| 15 | `for i in [1]<CR>` … `endfor` | `Vim(for):E488` | **E492 — unmeasurable** | `Vim(for):E488` |
| 16 | `execute(['let g:a = 1', 'let g:b = 2'])` | ok, both set | **`E492: Not an editor command: ['let…`** | ok, both set |
| 17 | `call len('a') x` | `E488: … x` | **`E488` without the remainder** | `E488: … x` |
| 18 | `T73D x`, `-nargs=0` | `E488: … x` | **`E471: Argument required`** | `E488: … x` |
| 19 | `T73N`, `-nargs=1`, no arg | `E471` | `E471` | `E471` |
| 20 | `let g:v = 4 'ab` | `E488: … 'ab` | **`E115: missing single quote`** | `E488: … 'ab` |
| 21 | `let g:v = 1 + <CR>` | `E15` | `E15` | `E15` |
| 22 | `unlet g:z<VT>` | `E488` | **ok** | `E488` |
| 23 | `let g:v = 4<FF>` | `E488` | **ok** | `E488` |
| 24 | `let g:v = 4<TAB>` / trailing spaces | ok | ok | ok |
| 25 | sourced `let g:v = 4<CR>\n` (file) | `E488` | **ok, `g:v` = 4** | `E488` |
| 26 | sourced wholly-CRLF file | `E488` on line 1 | **ok, runs every line** | `E488` |
| 27 | sourced clean-LF file | ok | ok | ok |

Every error **code** now matches. Three message-text differences remain, none of them this
defect:

- Rows 7 and 18: nvim appends the echoed command line (`… x: T73D x`); this port does not
  (`append_command`).
- Rows 10-12 and 21: nvim's E15 reads `Invalid expression: "…"`, this port's reads
  `invalid character 0x0d in expression` — the `ox-eval` message-format concern task 69 §5 opened.
- Rows 13-15: an exception escaping `execute()` is wrapped `Vim(call):E605:` here where nvim
  reports `Vim(if):E488:` directly.

## 3. The two traps task 71 named

**The script-line reader** stripped a trailing CR from every physical line, then `trim_end`ed the
rest, so a sourced `let g:v = 4<CR>` arrived as `let g:v = 4`. It hid the whole class from any
file-based probe. It was **in scope and is fixed**: `get_one_sourceline` (`runtime.c:2891-2905`)
strips the CR only when the file is `EOL_DOS`, and that whole branch is inside `#ifdef USE_CRNL`,
a Windows-only define — so on this platform nvim never strips it, which rows 25-26 confirm
against the oracle directly. Trailing white space is still removed, but only the space and tab
`del_trailing_spaces` removes.

**`execute()` with a List** stringified its argument instead of running each item as a source
line, which is `E492` here and the reason `:if`/`:while`/`:for`/`:return` were unmeasurable
through the one instrument that bypasses the script reader. **Fixed, not worked around**:
`execute_common` (`eval/funcs.c:1206-1216`) hands `do_cmdline` a `get_list_line` cookie, so the
items are joined and read back through the same logical-line reader a sourced file uses,
continuations and all. The single-string form still goes through one command line, unchanged.

## 4. Tests corrected

Exactly one existing test pinned the permissive behaviour, and it is corrected rather than
deleted:

| test | old expectation | new expectation | why the old one was wrong |
| --- | --- | --- | --- |
| `excmd_exec_state_tests::e488_from_call_trailing_characters` | `exc.message() == "Vim(call):E488: Trailing characters"` | `… == "Vim(call):E488: Trailing characters: trailing"` | Its own comment already recorded the oracle as `Vim(call):E488: Trailing characters: trailing` and called the missing remainder "a separate gap in `:call`'s own argument check". It was pinning the gap, not the behaviour. |

Two `ox-eval` tests task 71 predicted would need to keep their expectations did:
`trailing_input_reports_e488_with_the_remainder` and
`white_space_before_call_parenthesis_stays_an_error_in_the_subscript_chain` both use printable
trailing text (row 8's shape) and are unchanged and still green.
`malformed_interpolated_strings_report_typed_errors` broke once during development and was **not**
adjusted — it was correct, and it caught the stop-offset bug described in §1.

Fifteen new tests: four in `ox-eval/src/tests.rs`, eleven in
`ox-editor/src/excmd_exec_control_tests.rs`.

## 5. Mutations

Seventeen mutations, run one at a time against a `/tmp` copy of the single file and restored from
that copy. `ox-eval` mutations ran in the working tree; `ox-editor` mutations ran in the isolated
`/tmp/oxvim-t73-after` worktree, because `Task72Crashes` was editing sibling files and the crate
did not always compile.

**The compound condition.** The rule has two parts, and the tests are arranged so each part has a
case the *other* part alone gets wrong:

- `let g:v = 4<CR>` must be **E488**. With only the white-space rule the CR reaches an eager lexer
  and the answer is **E15**; with only the tolerant lexer the CR never survives `str::trim` and
  there is **no error at all**. Mutations X1 (`skipwhite_trim` → `str::trim`) and M1b (eager
  `tokenize()` in `parse`) fail it from opposite sides.
- `let g:v = 4 'ab` isolates the **lexer** half: `'ab` is not white space under any spelling of
  the white-space rule, so only tolerance decides, and an eager lexer answers E115.
- `let g:v = 4<FF>` isolates the **white-space** half against the near-miss spelling: FF is
  `is_ascii_whitespace`, so a "trim ASCII white space" implementation (mutation X2) swallows it
  and passes everything else. `<VT>` does the same against `str::trim`.

| # | mutation | killed by |
| --- | --- | --- |
| M1a | tolerant lexer discards the refusal (silent truncation) | 6 tests |
| M1b | `parse` uses the eager `tokenize()` | `a_byte_the_lexer_refuses_after_a_complete_expression_is_e488` |
| M2 | `parse` drops `\|\| refused.is_some()` | same |
| M3 | `resolve_refusal` never prefers the lexer error | 4 tests |
| M4 | resolve against the error offset, not the stop offset | `refusal_is_resolved_against_the_stop_offset_not_the_error_offset` |
| M5 | `parse_many` swallows the refusal | `parse_many_reaches_the_refused_byte_and_reports_e15` |
| S1 | reader strips the trailing CR again | `a_sourced_line_keeps_its_trailing_carriage_return` |
| S2 | sourced line uses `trim_end()` | same |
| E1 | `execute()` stringifies its List again | `execute_runs_a_list_argument_line_by_line` |
| X1 | `skipwhite_trim` → `str::trim` | 5 tests |
| X2 | `skipwhite_trim` → ASCII white space | 5 tests |
| X3 | only `split_assignment`'s expression side reverts | 4 tests |
| X4/X11/X12/X13 | only the `:eval` / `:throw` / `:while` / `:return` arm reverts | one test each |
| X5 | `:unlet` name-garbage check removed | `every_expression_command_rejects_a_trailing_carriage_return` |
| X6 | `-nargs=0` E488 check removed | `a_nargs_zero_user_command_rejects_any_argument_with_e488` |
| X7/X8 | `:call` trailing check uses Unicode `trim_start` / drops the remainder | one test each |
| X9/X10 | only the `:if` condition / `split_for` reverts | `block_openers_…` |
| X14 | `:return`'s emptiness test reverts | *survived the first round* |
| X15 | `strip_expression_comment` reverts to `trim_end()` | *survived the first round* |

**Two survivors, and what they were.** X14 and X15 both live at sites that only *test* the
argument or *cut* it, so every row above left them free: under `str::trim` a bare `:return<CR>`
looks like a plain `:return` and quietly returns 0, and `4<CR> "c"` loses the CR along with the
comment. Both were measured against the oracle (`Vim(return):E15`, and
`Vim(let):E488: Trailing characters: <CR> "c"`) and pinned by a new test,
`the_emptiness_test_and_the_comment_cut_see_the_carriage_return_too`, which also holds the
negative rows — a genuinely bare `:return` still returns 0, and an ordinary trailing comment is
still a comment. Both mutations were re-run and both are now killed. Nothing survives.

## 6. Oldtest

Six files most exposed to this class, one run each on each binary, each in its own throwaway
`testdir` with a fresh `HOME`; numbers read from the per-run `messages` file, not the committed
stale `test.log`.

| file | before executed | before failed | after executed | after failed |
| --- | --- | --- | --- | --- |
| `test_let.vim` | 0 | 1 | 0 | 1 |
| `test_eval_stuff.vim` | 0 | 0 | 0 | 0 |
| `test_expr.vim` | 33 | 25 | 33 | 25 |
| `test_vimscript.vim` | 0 | 0 | 0 | 0 |
| `test_execute_func.vim` | 11 | 9 | 11 | **8** |
| `test_usercommands.vim` | 22 | 16 | 22 | 16 |
| **total** | **66** | **51** | **66** | **50** |

One test moved to passing and nothing regressed. The modest movement is expected: this change
makes the port *stricter*, and the three files that execute nothing are blocked before any test
runs — `test_let.vim` on `E221: Marker cannot start with lower case letter` from a heredoc in the
file's own setup, which is a separate defect and pre-existing on both binaries.

## 7. Coordination

`crates/ox-editor/src/excmd_exec.rs` belongs to `Task72Crashes`, who was editing it throughout.
Rather than a unified diff, the fifteen edits went over as an idempotent anchored-replacement
script (`/tmp/task73_excmd_patch.py`) that refuses to write unless every anchor matches exactly
once, so it survived their concurrent edits and their `command_unlet` rewrite. They applied it,
reverse-applied it so their own defect commit would not swallow it, and re-applied it after
committing; I committed `excmd_exec.rs` as `d94388d`. The one test correction in their
`excmd_exec_state_tests.rs` went the same way. `script.rs` and `builtins/eval.rs` were claimed
explicitly and unopposed. **No ox-editor site was left uncoordinated, and no construct remains
divergent for want of coordination.**

## 8. Concerns

- **`E15` message text still differs** (`invalid character 0x0d in expression` versus upstream's
  `Invalid expression: "…"`). Task 69 §5 opened this as an `ox-eval` formatting concern; rows
  10-12 and 21 agree on the code and differ only in wording.
- **An exception escaping `execute()` is wrapped in `E605`** where nvim reports the inner error
  directly (rows 13-15). Found while probing; it is `flow_to_eval_error`'s shape, not this class.
- **`append_command` is missing.** nvim appends `: <the command line>` to errors raised from
  `do_one_cmd`'s own checks (rows 7 and 18); this port does not.
- **`:let`'s E488 names less than upstream's** when a `" comment` follows the garbage: upstream
  reports `<CR> "c"`, this port reports `<CR>`, because `strip_expression_comment` cuts the
  comment before `eval0` sees it. The code matches; the remainder is short.
- **`test_let.vim` still executes nothing**, blocked on `E221` from a heredoc in its own setup —
  a separate defect, unchanged by this work, and the reason the oldtest movement here is one test
  rather than many.
- **The compound-condition lesson, fourth time.** This round's two survivors (X14, X15) were
  exactly the shape tasks 69, 70 and 71 recorded: a rule with N parts, pinned by cases that N-1
  parts already decide. They were found only because every site was mutated individually rather
  than the helper being mutated once. Mutating the shared helper alone would have shown ten kills
  and hidden both.
