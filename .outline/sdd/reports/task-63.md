# Task 63: `has()` feature reporting

Scope: concern (1) of `.outline/sdd/reports/task-62.md`. Task 62 unblocked 38 setup-blocked oldtest
files by expanding `<f-args>`; 31 of them then self-skipped because `has()` answered 0 for names
Neovim always compiles in. Owned crate: `crates/ox-eval/`. One test file outside it,
`crates/oxvim/tests/cli.rs`, carries the process-level capability proofs, because `ox-eval` has no
editor to exercise them against; `crates/ox-editor/` was not touched (Task61Regressions owns it, and
Task64UndoBlocks was editing it and `ox-text` concurrently).

Oracle for every claim below: `.references/neovim/build/bin/nvim`, v0.13.0-dev-1390, API level 15.

## 1. Method

`has()` cannot be answered by copying upstream's `has_list` (`eval/funcs.c:2532-2667`). That list is a
statement about Neovim's build, and reporting a feature present that this rewrite does not have
converts an honest skip into a wall of failures. So each name was decided by exercising the
capability it names.

1. **Target set.** 89 names — every unconditional and platform-guarded entry of `has_list`, plus the
   `f_has` fast-path names (`vim_starting`, `multi_byte_encoding`, …) — were run through the oracle.
   99 of the 172 names probed answer 1 there.
2. **Answer set.** The same names were run through `oxvim`. Before this task, 96 of them differed:
   oracle 1, oxvim 0.
3. **Verdict per name.** For each divergence, the subsystem it names was exercised in both binaries
   through one probe per capability, one process per probe. This matters: **`exists('*name')` answers
   1 in oxvim for every generated builtin name, implemented or not.** `exists('*setqflist')` is 1
   while `setqflist(...)` raises `E117: not implemented: setqflist`. Any inventory built from
   `exists()` would have flipped roughly seventy names on a false premise.

## 2. Commits

| sha | subject |
| --- | --- |
| `45dc187` | feat(ox-eval): answer has() for the expression features this build has |
| `fcbc629` | feat(ox-eval): answer has() for the editor features this build has |

Both add to `FEATURES` in `crates/ox-eval/src/builtins.rs`, a sorted table with the answering module
named on each line. `f_has` compares with `STRICMP`, so the lookup now lowercases its argument:
`has("EVAL")` was 0 and is 1.

## 3. Features set to 1 (20), with the module behind each

| feature | module | capability proven by |
| --- | --- | --- |
| `eval` | `ox-eval/eval.rs` | `eval('1 + 2')` == 3 |
| `lambda` | `ox-eval/parser.rs` | `{a, b -> a * b}(6, 7)` == 42 |
| `vimscript-1` | `ox-eval` | legacy Vimscript is the dialect implemented |
| `float` | `ox-eval` `Typval::Float` | `1.5 * 2.0`, `str2float`, `float2nr`, `sqrt`, `floor`, `ceil`, `pow` |
| `num64` | `ox-eval` `Typval::Number` (i64) | `4611686018427387904 + 1` does not wrap |
| `multi_byte` | `ox-eval` | `strchars`/`strlen` disagree correctly on UTF-8 |
| `multi_byte_encoding` | `ox-eval` | `char2nr`/`nr2char` round-trip; unconditional upstream |
| `modify_fname` | `ox-eval/path_builtins.rs` | `fnamemodify(':t:r' / ':h' / ':e')` |
| `file_in_path` | `ox-eval/find_file.rs` | `findfile()` honours comma-separated 'path' |
| `path_extra` | `ox-eval/find_file.rs` | `**`, `**{count}` descent and `dir;` upward search |
| `user_commands` | `ox-editor/excmd_exec.rs` | `:command! -nargs=1` + `<f-args>` dispatches |
| `user-commands` | as above | the spelling upstream keeps for 5.4 |
| `windows` | `ox-editor/layout.rs` | `:split` then `winnr('$')` == 2 |
| `vertsplit` | `ox-editor/layout.rs` | `:vsplit` then `winnr('$')` == 2 |
| `visual` | `ox-editor` | `v2ld` leaves `def` from `abcdef` |
| `textobjects` | `ox-editor` | `daw` leaves `one three` from `one two three` |
| `startuptime` | `oxvim/cli.rs` | `--startuptime` writes a timing log |
| `fork` | `ox-uv/process.rs` | `system()` forks and returns output |
| `nvim` | build target | Neovim 0.13, `ox_rpc` API level 15 |
| `linux`, `fname_case` | platform | `#ifdef __linux__` and `#ifndef CASE_INSENSITIVE_FILENAME`; case-sensitive lookup verified |

Every one of these carries **two** tests: the answer, and the capability. The eval-layer capabilities
are in `crates/ox-eval/src/builtins_tests.rs`; `file_in_path` and `path_extra` were already covered by
`findfile_and_finddir_match_upstream_over_the_oldtest_tree`, which pins `**`, `**{count}` and upward
`;` search against the oracle, so that test is cited rather than duplicated. The six editor-side names
are proven in `crates/oxvim/tests/cli.rs` `features_reported_present_have_their_capability`, which
sources a script through the real binary and requires `has()` and the capability to agree in one
output: `1|<result>`, or the process reports the command that could not run.

Both tests were mutation-checked: renaming the `"visual"` table entry fails
`has_visual` **and** `features_reported_present_have_their_capability` (`left: "0|def"`), so neither
passes vacuously.

## 4. Features deliberately left 0 (69), with what is missing

No name was left 0 by default. Each was probed; the probe is the entry.

### The 17 from the task 62 concern

| feature | files gated | what is missing |
| --- | --- | --- |
| `quickfix` | 4 | `setqflist()`/`getqflist()`/`getloclist()` all raise `not implemented` |
| `conceal` | 5 | `matchadd()` and `synconcealed()` raise `not implemented`; 'conceallevel' is stored only |
| `spell` | 3 | `spellbadword()`/`spellsuggest()` raise `not implemented` |
| `syntax` | 2 | `synID()`/`synstack()`/`hlID()` raise `not implemented`; `hlexists('Comment')` is 0, so there are no default groups either |
| `signs` | 2 | every `sign_*` builtin raises `not implemented` |
| `timers` | 1 | `timer_start()`/`timer_info()` raise `not implemented` |
| `reltime` | 1 | `reltime()`/`reltimestr()`/`reltimefloat()` raise `not implemented` |
| `profile` | 1 | `:profile start` raises `not implemented` |
| `menu` | 1 | `menu_get()`/`menu_info()` raise `not implemented` |
| `mksession` | 1 | `:mksession!` raises `not implemented` |
| `digraphs` | 1 | `digraph_get()`/`digraph_set()`/`digraph_getlist()` raise `not implemented` |
| `cmdline_hist` | 1 | `histadd()`/`histget()`/`histnr()` raise `not implemented` |
| `langmap` | 1 | `set langmap=qx` then `feedkeys('q','xt')` leaves the buffer untouched; the oracle deletes a character. The option is stored and never applied |
| `vartabs` | 1 | `set vartabstop=4,8` gives `virtcol([1,2])`/`virtcol([1,4])` of 9/17 against the oracle's 5/13 — 'tabstop' still governs. `:retab` is also unimplemented |
| `arabic` | 1 | `set arabic` leaves 'rightleft' and 'delcombine' at 0 and 'keymap' empty; the oracle sets all three |
| `linebreak` | 0 | **never reaches `has()`.** `check.vim:30-37` `CheckOption` tests `exists('&x')` then `exists('+x')`; both are already 1 in oxvim, so `test_listlbr.vim` clears that gate and skips on `CheckFeature conceal` instead. Same for `breakindent`, which is why `test_breakindent.vim` already ran |
| `float` | 2 | **set to 1** — see §6 |

`python3`, `perl`, `ruby`, `clipboard_working`, `terminal` and `cryptv` were already 0 and stay 0.

### The other 52

**Subsystem absent, the builtin or command raises `not implemented`:** `byte_offset`
(`line2byte`/`byte2line`), `iconv`, `libcall`, `diff` (`:diffthis`, `diff_hlID`), `persistent_undo`
(`undofile()`; `&undodir` also reads empty), `packages` (`:packadd`), `listcmds`
(`:buffers`, `:badd`), `ex_extra` (`:center`, `:left`), `filterpipe` (`:%!sort`; `:!` itself is
unimplemented), `jumplist` (`getjumplist`), `insert_expand` (`complete_info`), `cmdline_compl`
(`getcompletion`), `cindent`, `lispindent`, `tag_binary` (`taglist`), `shada` (`:wshada`),
`dialog_con` (`confirm()`).

**Registered but inert — the option or variable exists and changes nothing observable:**

- `smartindent`, `comments`: `o` after `if (x) {` or after a `# ` comment line produces an empty
  line; the oracle indents and continues the comment.
- `virtualedit`: `set virtualedit=all` then `5|ix` corrupts the line instead of padding it.
- `cursorbind`, `scrollbind`: `set cursorbind` in both windows then `3G` leaves the other window's
  cursor on line 1; the oracle moves it to 3.
- `wildignore`: `set wildignore=*.o` then `expand()` still returns the path; the oracle returns empty.
- `keymap`: `set keymap=accents` is stored verbatim. The oracle *rejects* it, because it tries to load
  a keymap file; nothing here loads keymap files at all.
- `cmdwin`: `&cedit` is empty where the oracle has `^F`.
- `termresponse`: `v:termresponse` is undefined.
- `multi_lang`: `v:lang` is undefined (`E121`); `:language` is accepted with no locale state behind it.
- `localmap`: mappings never execute. `nnoremap ,x :let g:mp=7<CR>` then `normal ,x` runs nothing,
  buffer-local or global. This is broader than `localmap` and worth its own task.
- `autocmd`: registration works (`exists('#BufWritePre')` is 1) but nothing fires — a `BufReadPost`
  or `BufWritePost` autocmd does not run on `:edit`/`:write`, and `:doautocmd` is unimplemented.
- `visualextra`, `vreplace`: `<C-v>jlrX` leaves `abc|def` where the oracle gives `XXc|XXf`, and `gR`
  leaves `bcdef` where the oracle gives `xycdef`.
- `gettext`: `gettext()` returns its msgid. Upstream does too when untranslated, but there is no
  message catalogue here at all, so 1 would claim a subsystem rather than a function.
- `nanotime`: `getftime()` returns whole seconds.
- `acl`, `xattr`: nothing preserves ACLs or extended attributes across a write.
- Display-only, unverifiable headless and unverified: `autochdir`, `cmdline_info`, `cursorshape`,
  `extra_search`, `mouse`, `rightleft`, `showcmd`, `statusline`, `tablineat`, `termguicolors`,
  `title`, `wildmenu`, `winaltkeys`, `writebackup`, `find_in_path`, `browsefilter` (0 in the oracle
  too).
- `vim_starting`: dynamic startup-phase state. `ox-eval` holds no editor state, so an honest answer
  needs the editor layer. The oracle returns 1 during a `-S` script and 0 afterwards; a constant here
  would be wrong half the time.

### `folding` — the partial case, decided 0

Folding is the closest call after `float`, and it went the other way.

Works: `:2,3fold` gives the oracle's `foldlevel` 1 and `foldclosed` 2; `foldmethod=marker` and
`foldmethod=indent` both give the oracle's `foldlevel`; `foldclosed`/`foldclosedend`/`foldlevel`
answer.

Does not: `zf` creates no fold (`foldclosed(1)` is -1 against the oracle's 1), `foldmethod=expr`
yields level 0 for every line, and `foldtextresult()` raises `not implemented`.

`folding` gates no file at the top level; it gates 8 in-function `CheckFeature` calls inside
`test_normal.vim`, `test_display.vim`, `test_scroll_opt.vim`, `test_cursor_func.vim` and
`test_assert.vim`, all of which already run. Flipping it would run those specific functions, and the
normal-mode fold commands they use are exactly the half that does not work — so it would add
failures inside already-noisy files without adding a single pass. 0 is the honest answer while `zf`
does nothing, and this is the first name to revisit when Task59's fold work lands.

## 5. Measured effect

Test suites, `-- --test-threads=1`, zero failures:

| crate | task 62 baseline | now |
| --- | --- | --- |
| `ox-eval` | 409 | **462** |
| `oxvim` (`--test cli`) | 25 | **26** |

### The 37 files, before and after

Both columns come from the same harness and the same runtest invocation, differing only in the
binary: `/tmp/oxvim-t63-before` built from `ce29695` (the commit before this task) and
`/tmp/oxvim-t63-after` built from `fcbc629`. Every run used a freshly created throwaway `HOME` with
isolated `XDG_*`/`TMPDIR`, and its own copy of `testdir` under `/tmp/ox-t63`; nothing ran inside
`.references`.

| | before | after |
| --- | --- | --- |
| files running (≥1 test executed) | 2 | **3** |
| tests executed | 53 | **79** |
| test functions with errors | 33 | **59** |
| files self-skipped | 31 | **30** |

One file moved. Every other row is byte-identical between the two runs:

| file | executed | with errors | file skipped |
| --- | --- | --- | --- |
| `test_float_func.vim` | 0 → **26** | 0 → 26 | yes → no |
| `test_breakindent.vim` | 52 → 52 | 32 → 32 | no → no |
| `test_sha256.vim` | 1 → 1 | 1 → 1 | no → no |
| the other 34 | 0 → 0 | unchanged | unchanged |

**This is the headline the next census must not misread.** The task 62 concern predicted that
flipping the 17 names would un-skip roughly 29 files at once. It did not, and the reason is the point
of this task: 15 of those 17 subsystems genuinely do not exist here, so the honest answer is still 0
and those files still skip. `test_breakindent.vim` remains the shape of what a truthful 1 looks like —
0 executed to 52 executed with 32 erroring — and `test_float_func.vim` is now the second instance.

### Files outside the 37

31 further files reference a name whose answer changed, through an in-function `CheckFeature` or a
direct `has()`. Measured before and after with the same harness: executed 1029 → 1029, files running
26 → 26, files skipped 9 → 9, test functions with errors 900 → **901**. Three functions moved:

| file | function | before → after |
| --- | --- | --- |
| `test_lambda.vim` | `Test_lambda_feature` | failing → **passing** |
| `test_expr.vim` | `Test_printf_float` | skipped → failing |
| `test_startup.vim` | `Test_startuptime` | skipped → failing |

`Test_startuptime` is worth naming precisely, because it looks like an over-claim and is not: it fails
on `E117: not implemented: !`, the shell-filter command, which `filterpipe` correctly reports absent.
`--startuptime` itself works and is pinned by `startuptime_writes_a_timing_log`.

## 6. `float`: the judgment call, and why it stayed 1

`float` is the one name where a truthful 1 turned a clean skip into 26 executed tests, all 26
failing. Under the brief's rule — prefer 0 when a test that proceeds would fail on the missing part —
this is the case that has to be argued rather than assumed, so here is the argument.

Reverted or not, the question is what the 26 failing functions are made of:

- **17 are missing builtins**, each naming itself with `E117: not implemented`: `acos`, `asin`,
  `atan`, `atan2`, `cos`, `cosh`, `exp`, `fmod`, `isinf`, `isnan`, `log`, `log10`, `round`, `sin`,
  `sinh`, `tan`, `tanh`.
- **8 are one root cause**: `Test_abs`, `Test_ceil`, `Test_floor`, `Test_trunc`, `Test_sqrt`,
  `Test_pow`, `Test_str2float`, `Test_float_misc` all fail on float rendering —
  `Expected '0.0' but got '0'`, `Expected '2.0' but got '2'`, `Expected '1.234560e+02' but got
  '1.234560e2'`. `string()` and `printf('%g'/'%e')` drop the decimal point and the exponent padding
  upstream always prints. This is also the whole of the `test_expr.vim` `Test_printf_float` failure
  (`Expected '9999999.9' but got '9999999.900000'`).
- **1 is a saturation boundary**: `Test_float2nr` expects `-9223372036854775807` where oxvim gives
  `-9223372036854775808`.

So the 26 are not noise: they are a work list of 17 named functions, one formatting defect and one
boundary, and the skip was hiding all 19 items. The feature `has('float')` names — upstream's
`FEAT_FLOAT`, the existence of the Float type — is genuinely present: literals parse, arithmetic and
comparison are right, and `str2float`, `float2nr`, `sqrt`, `floor`, `ceil`, `pow`, `abs`, `trunc` all
work and are pinned by `float_capability_backs_its_feature_answer`.

Decision: **1**. Answering 0 would be false about the type, and would re-hide 19 actionable defects
behind a skip. Nothing was reverted after measuring.

The honest counter-argument, recorded because it is not weak: `string(3.0) == '3'` means a Float is
observably not a Vim Float in string context, which is a real sense in which the feature is partial.
If the next census wants `test_float_func.vim` quiet again, the fix is the rendering, not the answer —
that one defect closes 8 of the 26 failures here plus the `test_expr.vim` one.

## 7. Concerns

- **`exists('*name')` is the same lie, one layer down, and larger.** It answers 1 for every generated
  builtin name whether or not the function is implemented: `exists('*setqflist')` is 1,
  `setqflist(...)` is `not implemented`. `exists(':wshada')` is 2 for an unimplemented command as
  well. `check.vim` `CheckFunction` and `CheckCommand` are therefore both toothless, which is why
  files like `test_sha256.vim` execute a test and fail rather than skipping. This is a bigger parity
  defect than the one this task fixed and deserves its own task; it also means no future inventory
  should be built from `exists()`.
- **`type()` returns the wrong constants, across the board.** oxvim gives
  `1,2,4,5,6,7,8,10` for Number, String, List, Dict, Float, Bool, Null, Blob where upstream gives
  `0,1,3,4,5,6,7,10`, and `v:t_number`, `v:t_string`, `v:t_list`, `v:t_dict`, `v:t_float`, `v:t_bool`,
  `v:t_blob` are all undefined (`E121`). This is in `crates/ox-eval`, my crate, and was left alone
  deliberately: it is not a `has()` question, the current numbering is pinned by existing tests, and
  changing it touches every `type()` expectation in the tree. It will silently corrupt any test that
  compares `type(x)` to a literal, so it should be fixed before anyone writes more of those.
- **Mappings never execute.** `nnoremap ,x :let g:mp=7<CR>` then `normal ,x` runs nothing. That is
  what forced `localmap` to 0, but it is far wider than one `has()` name and will gate a large part of
  `test_mapping.vim`.
- **Autocmds register but never fire.** `exists('#BufWritePre')` is 1 while a `BufWritePost` autocmd
  does not run on `:write`, and `:doautocmd` is unimplemented. Same shape as the `has()` defect: the
  introspection agrees with upstream while the behavior is absent.
- **`persistent_undo` is pinned at 0 by agreement.** Task64UndoBlocks confirmed its scope is undo-block
  grouping plus `:undo`/`:redo`/`:undojoin`/`undotree()`/`changenr()`, and that it adds no
  `undofile()`, `&undodir` or `:wundo`/`:rundo`. Flip `persistent_undo` when those land — it gates 6
  `CheckFeature` calls in `test_undo.vim`.
- **`folding` is the next name to revisit**, once `zf` and `foldmethod=expr` work; see §4.
- **`vim_starting` needs the editor layer.** It is the only name left at 0 whose honest answer is not
  "absent" but "not visible from here".
- **One shared-tree note.** `crates/ox-editor` and `crates/ox-text` were mid-edit twice during this
  task, so the two commits were validated against `ox-eval` and `oxvim --test cli` only, and the
  oldtest measurements used pinned binaries built from `ce29695` and `fcbc629`. A workspace-wide
  build was green at the point `fcbc629` was committed.
