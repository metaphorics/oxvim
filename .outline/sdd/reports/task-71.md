# Task 71: `maparg()`, `:map {lhs}`, and the trailing-garbage class

Status: **item 1 done; item 2 measured and located, not implemented.**
The budget ran out after item 1's mutation round. §5 says exactly what item 2 needs, including
the two seams that have to move together and the reason the naive fix reports the wrong error code.

Oracle for every measurement: `.references/neovim/build/bin/nvim`, v0.13.0-dev-1390, API level 15.
`before` is `/tmp/oxvim-t71-before`, a detached worktree at `fe7b999` with its own
`CARGO_TARGET_DIR`. Every probe and oldtest ran with a freshly created throwaway `HOME`,
isolated `XDG_*`/`TMPDIR`, `VIMRUNTIME` set explicitly, and its own copy of `testdir` under
`/tmp` with the committed stale `test.log` removed. Nothing ran inside `.references`.

| sha | subject |
| --- | --- |
| `ed67c6b` | feat(ox-editor): answer maparg() and list one mapping with :map {lhs} |

`cargo test -p ox-editor -p ox-excmd -p ox-eval -p ox-text -- --test-threads=1`:
**797 → 806, 162 → 162, 474 → 474, 23 → 23, zero failures.**

---

## 1. The model gap task 69 named, and three more like it

Task 69 §4 found that `MappingAction::ExCommands(Vec<ExCommand>)` discards the right-hand side
as written, so nothing could answer `maparg()`'s `rhs`. Measuring the oracle showed the port was
short of **two** strings, not one, because upstream keeps two:

| upstream field | what reads it | what this port had |
| --- | --- | --- |
| `m_orig_str` — rhs as typed | `maparg()`'s compatible `rhs` | nothing |
| `m_str` — rhs after `replace_termcodes` | `maparg()`'s string form, `showmap`'s rhs column | only for `Keys`/`Expr`/`Nop`, lost for `ExCommands` |

`ExCommands` now carries `keys: Keys` beside `commands`, which is exactly the variant that
loses it — a `Vec<ExCommand>` does not print back to its source text. `MappingOptions` carries
`orig_rhs: String`. `MappingAction::replaced_keys()` answers `m_str` uniformly and returns
`None` only for `Callback`, which is upstream's `m_luaref != LUA_NOREF`.

Three further keys turned out to be *recordable*, not absent, which is why they are implemented
rather than reported below:

- **`mode_bits`.** `MapMode`'s discriminants were a private numbering (`Select = 1<<2`,
  `OperatorPending = 1<<3`, `CommandLine = 1<<5`, `LangArg = 1<<6`). Upstream's `MODE_*`
  (`state_defs.h:21-28`) are `OP_PENDING = 0x04`, `CMDLINE = 0x08`, `LANGMAP = 0x20`,
  `SELECT = 0x40`. Since `mapblock_fill_dict` reports the raw bits (`mapping.c:2143`), the
  choice was renumbering or a translation table beside the enum that can drift. Renumbered.
  Nothing depended on the old values: `MapModes::MAP`/`MAP_BANG`/`ALL` are built from the
  discriminants and no literal mode number exists anywhere in the tree.
- **`script`.** `command_map` computed `remap = !nore && !flags.script` and threw `flags.script`
  away. Execution still folds `<script>` into no-remap — there are no script-local mappings
  here, so the reachable `<SID>` set is empty — but `MappingOptions.script` is now recorded,
  because `script = 1, noremap = 1` is the only pair that distinguishes `<script>` from
  `:noremap` in the compatible dict.
- **`sid` and `lnum`.** `map_add` receives sid 0 and lnum 0 and copies `current_sctx`, *adding*
  `SOURCING_LNUM` to its line (`mapping.c:530-537`). At script level `Scripts::current_line()`
  is the whole answer. Inside a function body it is not: upstream reports the `:function`'s own
  physical line plus the body-relative line. Oracle, two data points — a function defined on
  line 2 with a `:map` on body line 2 reports 4, one defined on line 6 with a `:map` on body
  line 1 reports 7. `UserFunc` recorded no definition line, so it gained one. Its `sid`/`seq`
  pair and the new line became `SourceContext` (upstream's `sctx_T`), which shortened
  `UserFunctions::define`'s parameter list rather than lengthening it.

## 2. `maparg()` against the oracle

Setup, sourced as a script so `sid`/`lnum` are observable:

```vim
nnoremap ,a :let g:hit=1<CR>
nmap ,b ,a
inoremap <silent> <buffer> ,c foo
nnoremap <expr> ,d 'x'
nnoremap <nowait> ,e yy
nnoremap ,f <Nop>
```

### String form — `maparg({lhs}, {mode})`

```
                nvim                          ox (after)
,a       ':let g:hit=1<CR>'            ':let g:hit=1<CR>'
,b       ',a'                          ',a'
,c       ''                            ''            (no Normal-mode mapping)
,d       '''x'''                       '''x'''
,e       'yy'                          'yy'
,f       '<Nop>'                       '<Nop>'
,zz      ''                            ''
,c (i)   'foo'                         'foo'
```

Byte identical. `before` answered `E117: not implemented: maparg` to every row.

### Dict form — `maparg({lhs}, {mode}, 0, 1)`

Sixteen keys, and `sort(keys(d))` is identical on both binaries:

```
['abbr','buffer','expr','lhs','lhsraw','lnum','mode','mode_bits','noremap',
 'nowait','replace_keycodes','rhs','script','scriptversion','sid','silent']
```

Every value is identical too. Four rows, nvim on the left of each pair and ox on the right —
they were compared line by line and no line differed:

| key | `,a` `:cmd<CR>` | `,c` `<silent><buffer>` insert | `,d` `<expr>` | `,f` `<Nop>` |
| --- | --- | --- | --- | --- |
| `rhs` | `':let g:hit=1<CR>'` | `'foo'` | `'''x'''` | `'<Nop>'` |
| `lhs` / `lhsraw` | `',a'` / `',a'` | `',c'` / `',c'` | `',d'` / `',d'` | `',f'` / `',f'` |
| `noremap` | 1 | 1 | 1 | 1 |
| `script` | 0 | 0 | 0 | 0 |
| `expr` | 0 | 0 | **1** | 0 |
| `silent` | 0 | **1** | 0 | 0 |
| `nowait` | 0 | 0 | 0 | 0 (`,e` is 1) |
| `buffer` | 0 | **1** | 0 | 0 |
| `mode` / `mode_bits` | `'n'` / 1 | `'i'` / 16 | `'n'` / 1 | `'n'` / 1 |
| `sid` / `lnum` | 1 / 2 | 1 / 4 | 1 / 5 | 1 / 7 |
| `scriptversion` | 1 | 1 | 1 | 1 |
| `abbr` / `replace_keycodes` | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |

`,b` (`nmap`, so remapping) reports `noremap = 0`, `rhs = ',a'`, `lnum = 3`.
`nmap <script> ,g` reports `script = 1` with `noremap = 1`.
`maparg(',zz','n',0,1)` is the empty dictionary on both; `maparg('','n')` is the empty string.

Inside a function body, oracle and ox both give `lnum = 4` for a `:map` on body line 2 of a
function defined on line 2, and `7` for body line 1 of a function defined on line 6.

### Keys named as absent, with what would have to exist

- **`callback`** — needs a Funcref. `MappingAction::Callback` is a `u64` host-callback identity,
  not a callable value; upstream's key holds the `LuaRef` itself (`mapping.c:2111-2112`).
  Nothing script-reachable creates one, since `:map` cannot, so such a mapping answers the empty
  dictionary instead of an invented Funcref. Would need a Funcref-valued callback registry
  reachable from the mapping table.
- **`lhsrawalt`** — needs key simplification. Upstream emits it only when `replace_termcodes`
  reported `did_simplify` (`mapping.c:2124-2127`), i.e. when the written lhs has a second
  simplified byte form (`<C-I>` versus `<Tab>`). This port has no `m_simplified` model and no
  `REPTERM_NO_SIMPLIFY` pass, so it never emits the key — which is also what upstream does for
  every lhs that does not simplify. Would need the two-keyround `do_map` and the `m_alt` pairing.
- **`abbr = 1`** — needs the abbreviation table to be script-reachable *and* to carry mapping
  flags. `:abbreviate` is not an executed Ex command here, so no script can add an entry, and
  `Abbreviation` records no mode set, `nowait`, `silent` or original right-hand side. The query
  answers nothing rather than filling a dictionary from data that does not exist. Would need
  `:abbreviate`/`:iabbrev`/`:cabbrev` executed and `Abbreviation` merged onto `MappingOptions`.
- **`<BS>`, `<Del>`, `<NL>`, `<Nul>` in `lhs`/`lhsraw` and in the string form.** Not a dict key
  but the same kind of absence. `Keys::parse_notation` decodes these four to plain bytes where
  upstream builds the special keys `K_BS`, `K_DEL`, `K_NL`, `K_ZERO`, so they render here as
  `<C-H>`, a raw `0x7f`, `<NL>` and `<Nul>` and their `lhsraw` is the plain byte rather than a
  `K_SPECIAL` triple. `<CR>`, `<Tab>`, `<Esc>` and every `<C-x>` agree with upstream because
  they *are* plain bytes there too. Would need the internal three-byte key encoding to be
  agreed between `parse_notation`, `ox-eval`'s `\<Key>` escape and the RPC input decoder, which
  `typeahead.rs` already documents as unreconciled.

`replace_keycodes` is **not** in this list: only `nvim_set_keymap`'s option table sets
`m_replace_keycodes`, so upstream reports 0 for every mapping a `:map` command can create,
which is every mapping this port can hold. Reporting 0 is the oracle's answer, not a guess.

## 3. `:map {lhs}` listing against the oracle

`before`: every form was `E474: Invalid argument`, and a bare `:nmap` was a silent no-op.
`after`, through `execute()` so the rows are capturable — `\n` shown as `\n`:

| command | nvim | ox (after) |
| --- | --- | --- |
| `map ,a` | `\n\nn  ,a          * :let g:hit=1<CR>` | identical |
| `nmap ,a` | `\n\nn  ,a          * :let g:hit=1<CR>` | identical |
| `nmap ,b` | `\n\nn  ,b            ,a` | identical |
| `imap ,c` | `\n\ni  ,c          *@foo` | identical |
| `nmap ,d` | `\n\nn  ,d          * 'x'` | identical |
| `nmap ,e` | `\n\nn  ,e          * yy` | identical |
| `nmap ,f` | `\n\nn  ,f          * <Nop>` | identical |
| `nmap ,g` (`<script>`) | `\n\nn  ,g          & :echo 1<CR>` | identical |
| `nmap ,zz` | `\n\nNo mapping found` | identical |
| `map ,` | five rows, `,g ,f ,e ,d ,b ,a` order | identical |
| `map ,` with `<buffer> ,h` and an in-function `,i` | `,h` (`*@`) then `,i` then `,g ,f ,e ,d ,b ,a` | identical |

The layout is `showmap` (`mapping.c:220-266`): mode chars padded to three, lhs padded past
twelve with one blank guaranteed, the `*`/`&`/blank remap marker, the `@`/blank buffer-local
marker, then the rhs. The leading blank row is real — `msg_start` emits one newline and the
first `showmap` emits another whenever `msg_col > 0 || msg_silent != 0`, which holds both
interactively and under `execute()`.

The order is three independent rules, all matched: the buffer-local table before the global one
(`mapping.c:698-726`), `maphash[]` buckets ascending (`MAP_HASH`, `mapping.c:75-78`), and
newest-first inside a bucket because `map_add` pushes onto its head (`mapping.c:545-547`).

The only listing difference found is `:imap` with no lhs, where nvim also prints its built-in
Lua default mappings (`<S-Tab>` from `vim/_core/defaults`) that this port does not ship. Every
row for a mapping both binaries hold is identical.

## 4. Tests and mutations for item 1

No existing test needed correcting. Two were extended rather than changed:
`mapping_rhs_parses_cmd_form_with_ex_parser` and `mapping_rhs_parses_colon_command_form` kept
their old expectation (`commands.len() == 1`) and gained an assertion on the retained `keys`.

Nine new tests. **Thirteen mutations, one survived the first version of a test and none
survived the second.**

| mutation | caught by |
| --- | --- |
| `matching()` drops the locals-first key | `map_lists_buffer_local_mappings_first_and_each_bucket_newest_first` |
| `matching()` sorts oldest-first | the same test |
| `matching()` drops the bucket key | **survived first**, then the same test |
| `fill_dict` reports `str2special(m_str)` as `rhs` | `maparg_answers_the_right_hand_side_as_written_and_as_replaced` |
| `fill_dict` reports `script` as always 0 | `maparg_reports_each_recorded_flag_independently` |
| `command_map` never consults the function frame | `maparg_lnum_inside_a_function_adds_the_body_line_to_the_definition_line` |
| `script_context()` drops the body line from the sum | the same test |
| `showmap_row` always prints `*` | `map_lists_matching_mappings_and_says_so_when_none_match` |
| `showmap_row` never prints `@` | `map_lists_buffer_local_mappings_first_...` |
| `list_mappings` omits the blank leading row | both listing tests |
| `find_exact` matches a prefix instead of an exact lhs | `exact_mapping_lookup_tests_mode_length_and_locality_separately` |
| `special_notation`'s `<C-x>` fallback loses the `@` offset | `special_notation_names_each_class_of_control_byte` |
| `to_chars` loses the `:map`-set collapse arm | `mode_chars_and_bits_cover_every_map_mode_to_chars_arm` and the flag test |

The survivor is task 69 §5's lesson repeating. The bucket key is a *third* sort component, and
the first version of the ordering test defined `,z` before `+z` — so newest-first alone already
produced the asserted order and the bucket key decided nothing. Swapping the definition order
made `+z` the older mapping, where only the bucket key can put it first. Every compound rule in
this change is now exercised one conjunct at a time: `find_exact`'s three conditions
(mode overlap, exact length, locality) each have a case the other two cannot decide, the eleven
`map_mode_to_chars` arms have one case each, and the ten flags `maparg()` reports are asserted
on ten mappings that differ in exactly one flag rather than on one mapping with all of them set.

## 5. Item 2: measured, located, **not implemented**

`before` and `after` are the same binary here — nothing in this task changed it. The class is
real and larger than task 69 §5 recorded.

### The oracle table

Every row is one `execute()` in one process on each binary, the error read back out of
`v:exception`. `<CR>` is a raw `0x0d`, `<VT>` a raw `0x0b`.

| # | probe | nvim | ox |
| --- | --- | --- | --- |
| 1 | `let g:v = 4<CR>` | `E488: Trailing characters: <CR>` | **ok, `g:v` set** |
| 2 | `const g:v = 4<CR>` | `E488: Trailing characters: <CR>` | **ok** |
| 3 | `let g:v = 4<VT>` | `E488: Trailing characters: <VT>` | **ok** |
| 4 | `eval 4<CR>` | `E488: Trailing characters: <CR>` | **ok** |
| 5 | `unlet g:z<CR>` | `E488: Trailing characters: <CR>` | **ok** |
| 6 | `call len('a')<CR>` | `E488: Trailing characters: <CR>` | **ok** |
| 7 | `T71D<CR>`, `:command! T71D let g:v = 4` | `E488: Trailing characters: <CR>` | **ok** |
| 8 | `let g:v = 4 x` | `E488: Trailing characters: x` | `E488: Trailing characters: x` |
| 9 | `throw 'a'<CR>` | `E488: Trailing characters: <CR>` | throws `a` (no E488) |
| 10 | `echo 'z'<CR>` | `E15: Invalid expression: "<CR>"` | `E15: invalid character 0x0d in expression` |
| 11 | `execute "let g:v = 5"<CR>` | `E15: Invalid expression: "<CR>"` | `E15: invalid character 0x0d in expression` |
| 12 | `echomsg 'q'<CR>` | `E15: Invalid expression: "<CR>"` | `E15: invalid character 0x0d in expression` |

Rows 10-12 are `:echo`/`:execute`/`:echomsg`, which loop `eval1` and so try to parse the
remainder as *another* expression — E15, not E488. Those three already error here; only the
message text differs, which is task 69 §5's separate `ox-eval`-format concern.

`:if`, `:while`, `:for` and `:return` could not be measured through `execute()` because
`execute()` with a **List** argument is broken here — `execute(["if 1\r", …])` gives
`E492: Not an editor command: ['if 1…`, i.e. the list is stringified instead of joined with
newlines. That is a third, separate defect, found while building these probes. Measured through
a sourced file instead, nvim raises `E488` for all four and ox accepts all four; but that route
also exposed a **fourth** divergence: this port's script-line reader strips a trailing `\r` from
every sourced line, where `get_one_sourceline` strips it only for a `EOL_DOS` file. So a
file-based probe understates the gap rather than overstating it.

### Where the fix goes, and why it is not one line

`eval_text` (`excmd_exec.rs:1356`) **is** the shared seam — every one of `:let`, `:if`,
`:while`, `:for`, `:return`, `:throw`, `:eval`, `:call`, `:echo`, `:execute` and a user
command's expansion reaches the parser through it or through `eval_condition` beside it. And
`ExprParser::parse` (`ox-eval/src/parser.rs:184-196`) **already** raises
`E488: Trailing characters: {rest}` for a leftover token. The check is not missing; the bytes
never reach it. Two things eat them:

1. **`str::trim` is not `skipwhite`.** `split_assignment` (`excmd_exec.rs:5990`) returns
   `args[index + 1..].trim()`, and four `dispatch` arms pass `command.args.trim()`
   (`:eval` 1232, `:throw` 1236, `:return` 1252, `:while` 1022). Rust's `trim` removes CR, VT,
   FF, NL and every Unicode space; upstream's `skipwhite`/`del_trailing_spaces`
   (`strings.c:429-436`, `ascii_defs.h:84-87`) remove space and tab only. The lexer's
   `skip_layout` already skips exactly space and tab, so the fix is to *delete* these
   pre-trims — not to add a check — and to make `split_assignment`'s and
   `strip_expression_comment`'s own trimming ASCII space/tab.
2. **Eager tokenizing turns E488 into E15.** With the trims gone, `4\r` reaches
   `Lexer::tokenize`, which fails on the unknown byte (`lexer.rs:228-234`) *before*
   `parse_expr1` finishes — so the answer would be `E15`, where upstream's `eval0` parses `4`,
   stops, and reports `E488` with the remainder. Upstream never lexes past the expression.
   The faithful shape is a tolerant tokenize: stop at the first unlexable byte, record the
   error and its offset, hand back the tokens so far with an `Eof` there. Then `parse` reports
   E488 with the remainder from that offset when the expression completed before it, and
   surfaces the recorded E15 when `parse_expr1` actually needed the poisoned token — which is
   also what keeps rows 10-12 on E15, since `parse_many` loops and does reach it.

Row 7 is a fifth seam, not this one: a `-nargs=0` user command with a non-empty argument is
`E488` from `do_ucmd`'s own check, so it needs the argument left untrimmed *and* that check.

Existing tests expected to move when this lands, all of them pinning the permissive behavior
rather than the oracle: `ox-eval`'s `trailing_input_reports_e488_with_the_remainder` and
`white_space_before_call_parenthesis_stays_an_error_in_the_subscript_chain` should keep their
expectations (they use printable trailing text, row 8's shape) but any `ox-editor` test that
feeds a trailing CR to `:let` will need its expectation inverted from success to `E488`. The
exact list was not enumerated, because the change was not made.

**No oldtest measurement for item 2**, since there is no change to measure.

## 6. Concerns

- **Item 2 is unimplemented.** §5 is a location and a plan, not a fix. It is a whole class:
  every construct in rows 1-7 accepts a script upstream rejects.
- **`execute()` with a List argument is broken** — `execute(['if 1', 'endif'])` stringifies the
  list instead of joining with newlines and running each line. Found while probing; it makes
  any multi-line construct unmeasurable through `execute()`, which is the only instrument that
  bypasses the script reader.
- **The script-line reader strips a trailing `\r` unconditionally.** Upstream strips it only for
  a `EOL_DOS` file (`get_one_sourceline`), so a sourced `let g:x = 4<CR>` is `E488` there and
  silently fine here even before item 2. This hid rows 1-7 from a file-based probe entirely.
- **`:imap` with no lhs lists fewer rows than nvim** because this port ships none of nvim's
  built-in Lua default mappings. Every row for a mapping both binaries hold matches; the
  absence is the defaults, not the listing.
- **`maparg()`'s `lhsraw` for `<BS>`/`<Del>`/`<NL>`/`<Nul>`** is the plain byte, not upstream's
  `K_SPECIAL` triple, and the string form renders `<BS>` as `<C-H>`. §2 names it; it is the
  pre-existing notation-encoding disagreement `typeahead.rs` documents, and closing it touches
  `ox-eval`'s `\<Key>` escape and the RPC decoder together.
- **`test_mapping.vim` still fails 40 of 50.** The two that moved are
  `Test_map_meta_multibyte` and `Test_map_super_multibyte`. `Test_map_listing` and
  `Test_list_mappings` are screendump-gated or still failing for reasons beyond the listing
  format, so the listing work is proven by the direct oracle comparison in §3 rather than by
  that file.
- **A third repeat of the compound-condition lesson.** §4's survivor was a three-part sort key
  pinned by a case two parts already decided. Task 69 §5 recorded the same shape for a
  two-field rule, and task 69 §1 before that. A rule with N parts needs N cases, each one
  arranged so the other N-1 give the wrong answer.
