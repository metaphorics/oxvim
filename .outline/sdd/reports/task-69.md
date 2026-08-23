# Task 69: user commands inside a function body, the trailing CR, and `:normal`'s abort

Status: **INCOMPLETE — items 1, 2 and 3 done; item 4 (`maparg()`) not started.**
The budget ran out at the end of item 3. §6 says exactly what item 4 needs, including a model
gap found while measuring item 2 that must be closed before `maparg()` can answer honestly.

Oracle for every measurement: `.references/neovim/build/bin/nvim`, v0.13.0-dev-1390, API level 15.
`before` is `/tmp/oxvim-t69-before`, built from `f2f83ac` in a detached worktree with its own
`CARGO_TARGET_DIR`, so a peer's concurrent `ox-eval` edits are not in it. Every oldtest and probe
run used a freshly created throwaway `HOME` with isolated `XDG_*`/`TMPDIR`, its own copy of
`testdir` under `/tmp` with the committed stale `test.log`/`messages` removed, and
`VIMRUNTIME` set explicitly (without it the harness dies at `setup.vim:121`, as task 65 §5 noted).
Nothing ran inside `.references`.

| sha | subject |
| --- | --- |
| `9791ee7` | fix(ox-editor): resolve a user command when its line runs, and reuse a script's SID |
| `dccd747` | fix(ox-excmd): trim an Ex argument only where del_trailing_spaces runs |
| `b70d036` | fix(ox-editor): abort the rest of a :normal argument on a Normal-mode error |

`cargo test -p ox-editor -p ox-excmd -p ox-text -- --test-threads=1`:
**790 → 797, 160 → 162, 23 → 23, zero failures.**

---

## 1. Item 1: two defects, not one

The brief named parse-time resolution as the likely culprit. It is real, but it is not what made
the corpus guards inert. Both were fixed.

**(a) A user command is resolved when its line is *parsed*.** `parse_program` compiles a whole
script or function body up front against the registry as it stands at that moment, so a
`:command` created later is invisible. Measured, one script, `before`:

```
command! -nargs=1 T69A call writefile(['A got '.<f-args>],'out1.txt')
T69A hello
  nvim  : A got hello
  before: E492: Not an editor command: T69A hello   (script aborts at line 2)
```

Upstream resolves inside `do_one_cmd`, at execution, through `find_ex_command`. The fix is at the
one place a stale parse becomes visible — `run_instructions`' parse-error arm re-resolves the line
against the live table and runs it. Nothing else in a parse depends on state that can change, so
the retry is on the error path only and costs nothing on the hot path.

**(b) A fresh SID per sourcing event, which is the one that mattered.** `push_source` allocated a
new SID every time, so a re-sourced script started with an empty `s:` scope. `setup.vim:50-53` is

```
if exists('s:did_load')
  finish
endif
```

and `runtest.vim`'s `RunTheTest` sources `setup.vim` **before every test**. With the guard dead,
`setup.vim:71` `comclear` ran every time and wiped every user command — including all of
`check.vim`'s — between the test file being sourced and the test function being called. That is why
`exists(':CheckFunction')` was 2 at script level and 0 inside a test body, and why `:command`
listed nothing there. Measured:

| probe | nvim | before | after |
| --- | --- | --- | --- |
| `source setup.vim` twice, is a command defined between them still there? | yes | **no** | yes |
| under `runtest.vim`: in-function `exists(':CheckFunction')` | 2 | **0** | 2 |
| under `runtest.vim`: in-function `:command` listing | 30 entries | **empty** | 30 entries |
| `CheckFunction sha256` inside a test body | skips | **E492** | skips |

`do_source` looks the file up with `find_script_by_name` and reuses its SID; only the sequence
number is fresh (`runtime.c:2226,2333,2335`). `SourceFrame` now carries that sequence, and both
redefinition exemptions moved onto `(sid, seq)` exactly as `usercmd.c:940-948` and
`eval/userfunc.c:2856-2863` write them — replacing a name-comparison heuristic that only worked
*because* SIDs were unique per sourcing. Same SID, new sequence: silent replace. Anything else:
E174/E122, including two definitions inside one sourcing, which share a sequence.

### The corpus effect

All 29 files whose first blocker in `.outline/sdd/census-2/` is E492, same harness, both binaries.
`executed / failed / skipped`:

| file | before | after |
| --- | --- | --- |
| `test_display.vim` | 21 / 21 / 0 | 21 / **5** / **16** |
| `test_debugger.vim` | 16 / 16 / 0 | 16 / **4** / **12** |
| `test_messages.vim` | 29 / 26 / 0 | 29 / **10** / **16** |
| `test_diffmode.vim` | 85 / 82 / 0 | 84 / **51** / **31** |
| `test_search.vim` | 82 / 79 / 1 | 82 / **60** / **20** |
| `test_edit.vim` | 89 / 82 / 3 | 89 / **63** / **21** |
| `test_startup.vim` | 57 / 44 / 12 | 57 / **29** / **26** |
| `test_undo.vim` | 41 / 29 / 5 | 41 / **22** / **12** |
| `test_cursorline.vim` | 9 / 9 / 0 | 9 / **2** / **6** |
| `test_plugin_matchparen.vim` | 7 / 7 / 0 | 7 / **0** / **7** |
| `test_prompt_buffer.vim` | 10 / 10 / 0 | 10 / **4** / **6** |
| `test_statusline.vim` | 18 / 16 / 1 | 18 / **10** / **7** |
| `test_tagjump.vim` | 41 / 38 / 1 | 41 / **33** / **6** |
| `test_compiler.vim` | 8 / 8 / 0 | 8 / **6** / **2** |
| `test_stat.vim` | 9 / 8 / 0 | 9 / **6** / **2** |
| `test_signals.vim` | 5 / 4 / 1 | 5 / **1** / **4** |
| `test_ex_mode.vim` | 21 / 6 / 14 | 21 / **3** / **17** |
| `test_delete.vim` | 8 / 4 / 0 | 8 / **3** / **1** |
| `test_textformat.vim` | 29 / 27 / 0 | 29 / **26** / **1** |
| `test_startup_utf8.vim` | 4 / 4 / 0 | 4 / **3** / **1** |
| unchanged (9) | `test_charsearch`, `test_charsearch_utf8`, `test_filechanged`, `test_goto`, `test_join`, `test_nested_function`, `test_plus_arg_edit`, `test_scrollbind`, `test_vimscript` | |

**20 of 29 moved; 460 failures became 231.** Every file that moved converted failures into
skips: a `CheckFeature`/`CheckFunction`/`CheckOption` guard that had been dying on E492 now
reaches `exists()` and skips honestly. That is a win, not a regression — the failures those tests
were producing were noise generated *after* an inert guard let them into code that cannot work.
`test_plugin_matchparen.vim` is the clearest case: 7 failures → 7 honest skips, zero failures.

Nine files did not move; their E492 is a different name (not a `check.vim` command), which this
fix cannot reach. `test_vimscript.vim` still dies in setup for its own reason.

### Tests, and the mutations

Four new tests in `excmd_exec_function_tests.rs`, four mutations, **none survived**:

| mutation | caught by |
| --- | --- |
| `reusable_sid` always `None` (no SID reuse) | `re_sourcing_a_script_keeps_its_script_local_variables` (body ran 3× not 1×) |
| the re-resolution retry filtered to never succeed | `a_user_command_is_resolved_when_its_line_runs_not_when_it_is_parsed` (E492 on line 2) |
| command rule drops `existing.script.1 != script.1` | `a_reloaded_script_redefines_its_own_command_and_function_but_a_stranger_cannot` |
| function rule drops `existing.seq != seq` | the same test |

The last two are worth naming: they **survived the first version of that test**, which only
compared a stranger script against a reloaded one — a case the SID half already decides. The seq
half is only load-bearing for two definitions inside *one* sourcing, so the test gained
`/twice.vim` and `/twicefn.vim`. Oracle-checked first: two `:command`s for one name in one script
is `E174`, two `:func`s is `E122`.

---

## 2. Item 2: the trailing CR, and the rule it belongs to

Upstream removes trailing whitespace from an Ex argument in exactly one place:
`del_trailing_spaces` (`strings.c:429-436`), called only from `separate_nextcmd`
(`ex_docmd.c:4162-4164`), which `do_one_cmd` calls only for an `EX_TRLBAR` command that is not a
filter, and which skips the trim entirely when `EX_NOTRLCOM` is set. `del_trailing_spaces` takes
space and tab only (`ascii_iswhite`, `ascii_defs.h:84-87`), stops at one escaped by `\` or
CTRL-V, and never removes the argument's first byte. The leading side is `skipwhite` — space and
tab. **A CR is therefore part of the argument of every command, and there is no command class
that trims it.**

Flags read from `ex_cmds.lua`: `:normal`, `:execute`, `:let`, `:echo`, `:command`, `:call` have
`NOTRLCOM` and no `TRLBAR`; `:map`/`:nnoremap` and the vimgrep family have both; `:substitute`
has neither (it finds its own bar in `do_sub`); `:edit`, `:write`, `:print` have `TRLBAR` without
`NOTRLCOM` and are the only ones trimmed.

### The oracle spread

Every row is one `execute()` in one process on each binary, observed through a written file.

| # | probe | nvim | before | after |
| --- | --- | --- | --- | --- |
| 1 | `normal! :let g:a=1<CR>` | `g:a=1` | **`unset`** | `g:a=1` |
| 2 | `normal :let g:a2=2<CR>` | `g:a2=2` | **`unset`** | `g:a2=2` |
| 3 | `nnoremap ,x :let g:b=3<CR>` then `normal ,x` | `g:b=3` | **`unset`** | `g:b=3` |
| 4 | `let g:c = 4<CR>` | `E488: Trailing characters` | **ok, `g:c=4`** | **ok, `g:c=4`** (§5) |
| 5 | `echo 'z'<CR>` | `E15` | **ok** | `E15` (text differs, §5) |
| 6 | `execute "let g:d = 5"<CR>` | `E15` | **ok** | `E15` (text differs, §5) |
| 7 | `edit Xt69a<CR>` | buffer named `Xt69a<CR>` | **`Xt69a`** | `Xt69a<CR>` |
| 8 | `nnoremap ,y :let g:e=6␠␠␠` | rhs keeps 3 spaces | not measurable (§5) | rhs keeps them |
| 9 | `edit Xt69c␠␠␠` | `Xt69c` | `Xt69c` | `Xt69c` |
| 10 | `edit Xt69d\␠` | `Xt69d␠` | **`Xt69d\`** | **`Xt69d\␠`** (§5) |
| 11 | `T69C hi<CR>` (`-nargs=1`, `<q-args>`) | `E488` | **ok, `'hi'`** | **ok, `'hi'`** (§5) |
| 12 | `T69Q ␠<CR>hi` → `<q-args>` | `"<CR>hi"` | **`"hi"`** | `"<CR>hi"` |
| 13 | `T69R hi<CR>` → `<q-args>` | `"hi<CR>"` | **`"hi"`** | `"hi<CR>"` |

Rows 12 and 13 are what proves the *leading* half: a user command's `<q-args>` keeps a leading CR
upstream, so `skipwhite` really is space-and-tab only. (Row 7's leading twin, `edit <CR>Xfoo`,
gives `Xfoo` on **both** binaries — `:edit` drops it somewhere further along its own path, and
after this change we still match.)

Three downstream trims undid the same bytes and went with the parser change:

- `command_map` now splits lhs from rhs the way `str_to_mapargs` does (`mapping.c:463-475`): lhs
  to the next space or tab with CTRL-V and backslash escaping, `:unmap` taking the whole rest, rhs
  verbatim from `skipwhite(lhs_end)` to the end. It was splitting on `char::is_whitespace` — which
  includes CR — and then `.trim()`ing the rhs.
- `command_edit` and `command_invoke_user` stop re-trimming `ea.arg`. The parser has already done
  everything upstream does to it.

**Seven parser expectations moved**, and they were pinning the over-trim, not the oracle:
`:substitute`, `:echo` and the vimgrep family never reach `del_trailing_spaces`, so the space
before their separating bar is part of the argument upstream too. `write_splits_at_bar`, which
asserts `:write file | print` gives `"file"`, is the other side of the same rule and still passes —
that pair is the discriminator.

Five mutations, **none survived**:

| mutation | caught by |
| --- | --- |
| restore `args.trim_end()` in the parser | both new `ox-editor` behavioral tests |
| drop the `TRLBAR && !NOTRLCOM && !usefilter` gate (always trim) | `trailing_spaces_are_removed_only_where_del_trailing_spaces_runs`, `substitute_splits_at_trailing_bar` |
| `skip_ascii_space` back to `is_ascii_whitespace` | `a_trailing_cr_survives_into_normal_and_a_mapping_rhs` (the `<q-args>` leading-CR case) |
| `command_map` rhs back to `.trim()` | both `ox-editor` tests |
| `command_invoke_user` back to `.trim()` | `a_trailing_cr_survives_into_normal_and_a_mapping_rhs` |

---

## 3. Item 3: `:normal` aborts, and E223 is a message

The abort mechanism is not `did_emsg`. It is `clearopbeep` → `beep_flush` →
`flush_buffers(FLUSH_MINIMAL)` (`input.c:473-529`), which discards the *mapped* run at the front
of the typeahead. `ex_normal` stuffs its argument with `ins_typebuf(..., nottyped = true)`, and
`nottyped` is what counts the whole argument into `tb_maplen` (`input.c:964-966`) — so the
remainder of a `:normal` argument is exactly what a beep throws away.

`nv_csearch` beeps when `searchc` returns false, and `searchc` returns false both when the target
is absent and when `;`/`,` has no previous `f`/`t` to repeat. Here `;`/`,` with no previous find
was a silent no-op and a failed `f`/`t` reported nothing, so the keys behind them ran.
`move_find` now reports whether it moved, and `Typeahead::flush_mapped` is `FLUSH_MINIMAL`.

| case | nvim | before | after |
| --- | --- | --- | --- |
| A2 `:normal! ,x`, `,x` unmapped, buffer `aaa` | `aaa` | **`aa`** | `aaa` |
| J5 only `,xy` mapped, `:normal ,x` | `aaa` | **`aa`** | `aaa` |
| `:normal! x,x` | `aa` | **`a`** | `aa` |
| `:normal! fbx;x` on `abcabc` (a find that works) | `acac` | `acac` | `acac` |
| `:normal! fzx` on `abc` (target absent) | `abc` | — | `abc` |
| R1 `nmap ,x ,x` then `:normal ,x` inside `:try` | no exception, script continues | **`Vim(normal):E223` caught** | no exception, message `E223: recursive mapping` |

E223 was task 66's named divergence and closes here: `vgetorpeek` calls `emsg(e_recursive_mapping)`,
then `flush_buffers(FLUSH_MINIMAL)` and `return map_result_fail` (`input.c:2513-2518`) — a message
and a discarded queue. The two tests task 66 wrote against the raised form now assert the message,
that the chain never reached its target, and that the next `:normal` still works.

Three mutations, **none survived**:

| mutation | caught by |
| --- | --- |
| `beep_flush` a no-op | `an_error_in_normal_discards_the_rest_of_its_argument` (`aa` for `aaa`) |
| `flush_mapped` takes `!flags.mapped` (drops nothing) | the same test |
| E223 raised through `error_flow` again | `a_self_recursive_mapping_stops_at_maxmapdepth`, `maxmapdepth_bounds_how_far_a_mapping_chain_expands` |

---

## 4. What did not get done: item 4, `maparg()`

Not started. The budget ran out at the end of item 3, so this is a plain shortfall, not a
judgement that it should be skipped.

The measurement work for it exists and is in §2 row 8: `:map {lhs}` listing a *single* mapping is
also unimplemented (`E474: Invalid argument`), so `maparg()` is currently the only instrument that
could observe a mapping's stored rhs from script. That is why row 8 is proven at the parser and
through behavior instead.

`f_maparg` → `get_maparg` (`mapping.c:2148-2227`) and `mapblock_fill_dict`
(`mapping.c:2090-2146`) are the spec. The string form is `str2special` of the rhs, or `<Nop>` for
an empty rhs. The dict form is 17 keys: `lhs`, `lhsraw`, `lhsrawalt` (only when the lhs
simplified), `rhs` (`m_orig_str` in the compatible form `maparg()` uses), `noremap`, `script`,
`expr`, `silent`, `sid`, `scriptversion`, `lnum`, `buffer`, `nowait`, `replace_keycodes`, `mode`,
`abbr`, `mode_bits`, plus `desc` and `callback` when present.

**A model gap has to be closed first, and it is the reason this is not a mechanical job.** This
port's `Mapping` carries only `lhs`, `action` and `options`, and for a `:`- or `<Cmd>`-shaped
right-hand side `MappingAction::ExCommands(Vec<ExCommand>)` holds the *parsed* commands — the rhs
as written is discarded at registration. `maparg()`'s `rhs` is `m_orig_str`, the text as typed, so
`Mapping` needs an `orig_rhs: String` set in `command_map` before anything can answer for it.
Four further fields have no data behind them and must be named rather than invented: `sid` and
`lnum` (mappings record no script context here), `script` (`<script>` is folded into
no-remap — see task 66 §3), and `replace_keycodes` (no `nvim_set_keymap` option surface).
`expr` is recoverable from `MappingAction::Expr`, `abbr` from which table is consulted,
`mode`/`mode_bits` from `MappingOptions::modes`, and `buffer` from `MapScope`.

---

## 5. Concerns, and what still diverges

- **An Ex command's expression argument tolerates trailing garbage.** §2 rows 4 and 11:
  `let g:c = 4<CR>` and a user command whose expansion ends in a CR are `E488: Trailing
  characters` upstream and succeed here. The parser now hands the CR through correctly; what is
  missing is the check *after* an expression is parsed that the rest of the argument is empty.
  That is one rule in `command_let` and its siblings, and it is a whole class — the same tolerance
  will be hiding other accepted-but-invalid scripts. Worth its own task.
- **E15's message text differs.** §2 rows 5 and 6: the code matches, but ours reads
  `E15: invalid character 0x0d in expression` where upstream reads `E15: Invalid expression: "<CR>"`.
  `ox-eval`'s message format, not this seam.
- **`:edit` does not halve a backslash.** §2 row 10: `edit Xt69d\ ` now keeps the escaped space
  (it used to lose it) but keeps the backslash too, where upstream gives `Xt69d `. That is
  `backslash_halve` inside `expand_filename`, and it applies to every `EX_XFILE` command, so it
  belongs with a file-name-argument task rather than here.
- **User commands are granted `EX_TRLBAR` here and upstream does not grant it.**
  `parser::effective_flags` gives every user command `RANGE|BANG|EXTRA|TRLBAR`, but
  `uc_scan_attr` starts `argt` at 0 and only `-bar` adds `EX_TRLBAR` (`usercmd.c:1008,755-756`).
  So `:Foo a | echo b` splits at the bar here and does not upstream unless the command was
  defined `-bar`, and the same grant makes us trim trailing spaces from a non-`-bar` command's
  argument. Left alone deliberately: it changes the argument of every user command and wants its
  own before/after, which is precisely the shape of item 2.
- **`:map {lhs}` cannot list one mapping** (`E474`), which is what forced §2 row 8 to be proven
  indirectly. It is `showmap`/`map_clear` territory and pairs naturally with item 4.
- **`v:count` is not implemented** — `x . v:count` is `E121` here and `'a0'` upstream. Found while
  building probes; unrelated to these items but it silently breaks any probe or test that uses it.
- **`test_diffmode.vim` executes 84 after and 85 before.** The failure count drops 82 → 51, so the
  direction is right, but one test that used to be counted as executed no longer is. I did not
  isolate which; whoever revisits that file should check it is a skip and not a lost test.
- **The seq half of the reload rule had no test until the mutation caught it.** Recorded in §1
  because it is the second time in this series that a rule keyed on two fields was pinned by a
  case only one field decides. A test for a compound condition should exercise each conjunct.
