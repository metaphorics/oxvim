# Task 62: E15, "Invalid expression"

Scope: the E15 surface identified as the largest untouched blocker by the pass-2 census
(`.outline/sdd/reports/task-60.md`, `.outline/sdd/oldtest-blockers-2.md`). Owned crate:
`crates/ox-eval/`. One cross-crate commit in `crates/ox-editor/src/excmd_exec.rs` was handed over
explicitly by Task61Regressions, who owns that directory; it was staged hunk by hunk so that peer's
concurrent retab work stayed out of it.

Oracle for every claim below: `.references/neovim/build/bin/nvim`, v0.13.0-dev-1390, API level 15.

**Suite-measurement status.** All oldtest measurements in §3 were taken *before* the suite ban
announced after the `let $VAR` export defect was traced. They complied with both rules later made
mandatory: a throwaway `HOME` per run alongside isolated `XDG_*`/`TMPDIR`, and a copy of `testdir`
under `/tmp` so nothing ran inside `.references`. No oldtest has been run from this task since the
ban, and the acceptance criterion "re-run one census file per construct" is therefore reported as
already-collected evidence rather than re-verified.

## 1. Evidence: what the 46 E15 files actually die on

Extraction: for each of the 236 per-file logs in `.outline/sdd/census-2/`, the first `E15: <text>`
line and its throwpoint; the throwpoint's line number resolved against
`.references/neovim/test/old/testdir/<file>`. 46 files mention E15; 38 have it as first blocker, and
in all 38 it fires at script level, so the file contributes zero executed tests.

Distinct E15 texts across the 46 files:

| count | text |
| --- | --- |
| 38 | `expression expected` |
| 4 | `trailing characters after expression` |
| 1 | `invalid character 0x3d in expression` |
| 1 | `invalid character 0x7c in expression` |
| 1 | `but got E114: missing double quote` |
| 1 | `but got E720: invalid literal dictionary key` |
| 1 | `but got E274: white space is not allowed after '->'` |
| 1 | `but got E117: not implemented: searchpair` |

All 38 `expression expected` files fail on one of `CheckFeature`, `CheckOption`, `CheckExecutable`
or `CheckFunction`. Those are not built-in commands: `check.vim` defines each as
`command -nargs=1 CheckX call CheckX(<f-args>)`. So the E15 group is dominated by a single missing
substitution, not by missing expression syntax.

### Ranked construct table

"Files gated" counts files where the construct is *observed in the census logs*, not every file that
could contain it.

| rank | construct | files gated | disposition |
| --- | --- | --- | --- |
| 1 | `<f-args>` left unexpanded in a user command body | 38 (all setup-blocked; 103 files use `Check*` at all) | landed `53c5ce5` |
| 2 | white space between a bare name and its `(` | 4 | landed `f4558a8` |
| 3 | trailing input reported as E15 instead of E488 | 4 (same census signature as rank 2) | landed `2fc5a44` |
| 4 | E15 message text is oxvim's own wording, not `e_invexpr2` | 45 (text only; gates nothing by itself) | landed `4cdaeee` |
| 5 | white space around `->` collapsed into one E274 | 1 file, 2 assertions | landed `e5a3ffe` |
| 6 | `#{...}` literal key validation | 1 file, 1 assertion | landed `ba9070b` |
| 7 | `foreach(x, "string")` must run as an Ex command line | 1 file, 4 assertions | declined, §4 |
| 8 | `\|` bar splitting inside `:let` in an autocmd body | 1 file | declined, Ex layer |
| 9 | curly-brace names, `"xxxx"->str{s}()` | 1 file, 1 assertion | declined, §4 |
| 10 | `echo "\<C-">` gives E114 where upstream gives E15 | 1 file, 1 assertion | declined, §4 |

Rank 1 was not on the brief's candidate list (lambdas, method chains, dictionary-function references,
float and blob literals, string interpolation, falsy-coalescing). Every one of those already parses.
The ranking from the logs governed instead, as instructed.

## 2. Commits

| sha | subject |
| --- | --- |
| `53c5ce5` | feat(ox-editor): expand `<f-args>` in user command bodies |
| `2fc5a44` | fix(ox-eval): report unconsumed input as E488 with the remainder |
| `f4558a8` | feat(ox-eval): allow white space between a bare name and its call parens |
| `ba9070b` | fix(ox-eval): match upstream literal dictionary key rules |
| `e5a3ffe` | fix(ox-eval): match upstream errors for white space around `->` |
| `4cdaeee` | fix(ox-eval): quote the whole expression in E15 messages |

### `53c5ce5` — `<f-args>`

`command_invoke_user` substituted `<args>`, `<q-args>`, `<bang>`, `<line1>`, `<line2>`, `<count>` and
`<reg>`. The literal `<f-args>` survived into the body and reached the expression parser, where `<`
cannot start an expression. `split_command_arguments` follows `uc_split_args`
(`usercmd.c:1189-1302`): split on unescaped white space, emit each piece as a double-quoted string,
keep `\\`, let a backslash-escaped space or tab join two words, escape an embedded `"`. An empty
argument list expands to nothing rather than `""`, matching the early return for `quote == 2` at
`usercmd.c:1503-1512`.

### `2fc5a44` — E488 for trailing input

`e_trailing_arg` is `E488: Trailing characters: %s` (`errors.h:123`), raised from `eval.c:1251` once
`eval0` stops short of the end, quoting the remainder from the first unconsumed token. `eval('5 a')`
is E488, not E15. The existing `error_chained_comparison` case pinned E15 for `1 < 2 < 3`; the oracle
gives `E488: Trailing characters: < 3` there, so that expectation was wrong and moved with the code.

### `f4558a8` — white space before call parens

`eval.c:2783-2786` skips white space before testing for `(` when the name sits at the head of an
expression, so `substitute ( 'some text' , 't' , 'T' , 'g' )` and `call setline (1, ...)` are legal.
The relaxation is confined to that position: `handle_subscript` (`eval.c:6022-6026`) requires
`!ascii_iswhite(*(*arg - 1))`, so `d.Fn ()`, `l[0] ()` and `Fn() ()` stay errors and now report
upstream's E488 naming the detached parenthesis. Verified in both directions: three accepting forms
return values, three rejecting forms error.

### `ba9070b` — literal dictionary keys

`get_literal_key` (`eval.c:4458-4472`) scans raw bytes: a run of ASCII alphanumerics, `_` and `-`,
then skipwhite. A first byte that cannot start a key makes `eval_dict` abandon the dictionary
(`eval.c:4512-4514`) and the caller reports the whole expression via `e_invexpr2`; a valid key with
no colon is `E720: Missing colon in Dictionary: %s` (`eval.c:4517`). Ten probes now agree exactly,
message text included, including the three that previously produced the wrong code.

### `e5a3ffe` — white space around `->`

Three positions were collapsed into one E274 carrying oxvim's own wording. `eval_method`
(`eval.c:2990-3104`) separates them, and a gap after `->` is not an arrow complaint at all: the name
is read with no skipwhite, so the remainder is left unparsed and quoted whole.

| input | upstream |
| --- | --- |
| `->name()` | ok |
| `-> name()` | `E15: Invalid expression: " name()"` |
| `->name ()` | `E274: No white space allowed before parenthesis` |
| `->name` | `E107: Missing parentheses: name` |
| `->{x -> x}` | `E107: Missing parentheses: lambda` |
| `->` | `E260: Missing name after ->` |

13 of 14 probes now agree exactly. The 14th, `[1,2,3]-> ` typed at `:let`, differs only because
oxvim's Ex layer trims the trailing space before the parser sees it; the parser itself gives E15
there, which the unit test pins.

### `4cdaeee` — E15 message text

`e_invexpr2` (`errors.h:38`) is `E15: Invalid expression: "%s"` and is handed the whole expression,
not the token that failed. Constructs with their own upstream diagnostic keep it: `1 ? 2` stays E109,
pinned by a boundary test so this did not become a catch-all.

## 3. Measured effect

Test suites, `-- --test-threads=1`, zero failures everywhere:

| crate | baseline | now |
| --- | --- | --- |
| `ox-eval` | 394 | **410** |
| `ox-editor` | 742 | 758 (includes Task61's landings; 5 of the new tests are mine) |
| `ox-excmd` | 160 | 160 |
| `ox-text` | 18 | 18 |

Every construct carries the four tests the brief asked for: normal case, one boundary, the documented
error, and a malformed variant that must still fail with upstream's code.

Census recovery, measured with the brief's invocation, pre-ban. "Before" is the pinned census binary
`/tmp/oxvim-census-pinned` at `ed44788` re-run through the identical harness, not the TSV, so the two
columns are directly comparable. "Errors" counts `Found errors in` lines, which reproduces the census
`failed` column exactly on every file cross-checked.

| construct | file | executed before → after | with errors before → after |
| --- | --- | --- | --- |
| `<f-args>` | `test_breakindent.vim` | 0 → **52** | 1 → 32 |
| white space before parens | `test_expr.vim` | 33 → 33 | 26 → **25** |
| E488 trailing | `test_functions.vim` | 110 → 110 | 72 → **71** |
| `->` white space | `test_method.vim` | 10 → 10 | 8 → 8 |
| literal dict keys | `test_listdict.vim` | 54 → 54 | 54 → 54 |

The last two show no movement in the file-level counters because those functions fail for several
reasons at once, so the honest unit is the assertion:

- `test_method.vim` `Test_method_syntax`: both `Expected E15: but got E274:` failures are gone; the
  function now reaches statement 7 and dies on `str{s}()`, the curly-brace name declined in §4.
- `test_listdict.vim` `Test_dict`: `Expected E15: but got E720:` at statement 16 is gone; the
  function now reaches statement 21.
- `test_cursor_func.vim` `Test_screenpos_number`: was dying at statement 3 on
  `call setline (1, ...)`; now reaches statement 7 and dies on `win_screenpos` not being implemented.
- `test_expr.vim` `Test_white_in_function_call` and `test_functions.vim` `Test_eval` pass outright.

Across all 46 E15 files: executed **355 → 408**, files with errors **332 → 326**.

### The honest reading of the 38

All 38 setup-blocked files now parse, but only two go on to run tests (`test_breakindent.vim` 52,
`test_sha256.vim` 1). 33 of the rest reach the `Check*` throw and self-skip. That is the
upstream-shaped outcome and it replaces a bogus E15 failure with a correct skip, but it adds no
coverage. Their gate is now `has()`, not the parser: for 17 feature names oxvim answers 0 where nvim
answers 1.

| oxvim 0, nvim 1 (17) | agree at 0 (4) |
| --- | --- |
| `conceal` `quickfix` `spell` `arabic` `digraphs` `float` `cmdline_hist` `langmap` `menu` `mksession` `profile` `reltime` `signs` `syntax` `timers` `vartabs` `linebreak` | `python3` `perl` `ruby` `clipboard_working` |

`has()` lives in `crates/ox-eval/src/builtins.rs`, so it is in my crate, and answering 0 for a feature
nvim always compiles in is a parity defect on its own terms. I did not change it: it is not an E15
construct, and flipping 17 names would un-skip roughly 29 files at once and convert them from clean
skips into mass failures. That is a deliberate, separately-measurable decision deserving its own task
rather than a silent rider on this one. It is the single highest-leverage follow-up here.

## 4. Constructs declined, with reasons

- **`foreach(x, "string")` runs an Ex command line, not an expression** (`test_filter_map.vim`, 4
  assertions). `filter_map_one` (`eval/list.c:47-53`) calls `do_cmdline_cmd` for the string form of
  `foreach`, with the comment "foreach() is not limited to an expression". The oracle confirms it:
  `foreach([1], "xyzzy")` gives E492 and `foreach([1], "let a = foo")` gives E121. oxvim routes
  `foreach` through `filter_or_map` in `ox-eval`, which evaluates the string as an expression, so the
  E15 here is a symptom, not an expression-parser bug. Fixing it needs an Ex executor, which
  `ox-eval` deliberately does not have; the builtin has to move to the layer owning the Ex runtime.
  Cross-crate, and not an E15 construct.
- **`|` splitting inside `:let` in an autocmd body** (`test_gui.vim`). `au ColorScheme * let g:n += 1
  | let g:m = g:n` hands the whole tail to the expression parser, which reports
  `E15: invalid character 0x7c`. Command-line bar splitting is the Ex layer, not `ox-eval`.
- **Curly-brace names, `"xxxx"->str{s}()`** (`test_method.vim`). Not implemented anywhere in oxvim: a
  new name-resolution feature rather than an E15 fix. It now blocks `Test_method_syntax` at
  statement 7 with a correct E107.
- **`echo "\<C-">` gives E114 where upstream gives E15** (`test_expr.vim`). The oracle gives E114 for
  the same bytes reached through `eval()`, so the divergence is in how `:echo` hands its argument
  over, not in the expression lexer.
- **Evaluate-before-trailing ordering.** Upstream's `eval0` evaluates the parsed expression and only
  then complains about leftovers, so `eval("a = 1")` is `E121: Undefined variable: a` while oxvim
  rejects the trailing text at parse time and its lexer additionally refuses a bare `=`
  (`E15: invalid character 0x3d`). Matching this means `Parser::parse` must return the trailing
  offset and let callers report it after evaluation, changing a signature used across several
  crates. Too wide to land safely beside concurrent peers, and it fixes one assertion. Recorded here
  as the correct shape of the fix rather than papered over: changing only the lexer would move E15 to
  E488 and still not reach upstream's E121.

## 5. Concerns

- **User commands resolve at parse time.** A `:command` defined and invoked inside the same program
  never dispatches; it reports E492. It works only when the definition is in a separately sourced
  script, which is why `check.vim` works under `runtest.vim` but not in a single-file repro. This
  inflates the E492 census rank (83 files) by an unknown amount and is worth measuring before anyone
  attacks E492. Reported to Task61Regressions, who owns `crates/ox-editor/`.
- **`setenv()` was half-wrong, and is now fixed** (`ab2b9b2`). Checking it against the `let $VAR`
  sandbox defect at Main's request showed the process-environment side was already correct — both
  branches route through `ox_sys::set_env`/`ox_sys::unset_env`, so children always inherited the
  value and the `$HOME` sandbox was never at risk from this path. The read side was not:
  `Scope::env` is a snapshot of `std::env::vars_os()` taken once at startup
  (`excmd_exec.rs:345-346`) and every `$VAR` read comes from it, so `call setenv('X', 'v')` followed
  by `echo $X` returned the empty string where upstream prints `v`. Upstream has no snapshot:
  `f_setenv` is `os_setenv` and each read is an `os_getenv`. `:let $VAR` already wrote both sides;
  `setenv()` wrote one. Both are updated now, with the `v:null` branch removing the snapshot entry
  through a new `Scope::unset_env` so a stale value cannot outlive `os_unsetenv`. Found by
  Task61Regressions while tracing the sandbox escape. The pre-existing test asserted only through
  `std::env::var_os` and still passes with the fix reverted, so a new test drives a parsed `$NAME`
  expression through the same scope; it was mutation-checked by reverting the mirror and confirming
  it fails. Worth someone confirming `crates/ox-uv/src/misc.rs:199-207` is the libuv-facing env API
  and not a third path that can diverge.
- **`#{a: 1` gives E723 where upstream gives E722** ("Missing comma in Dictionary"). Noticed while
  probing literal dictionaries; left alone because it is the unterminated-dictionary construct, not
  the key construct, and `test_listdict.vim` already tracks it at `Test_dict[14]`.
- **`->99()` is a valid method call upstream** (`E117: Unknown function: 99`, because `get_name_len`
  accepts digits) and is E260 in oxvim. Left alone: accepting it would change what reaches the
  evaluator for a case no census file exercises.
- **`ox-editor`'s count is not all mine.** Task61Regressions was landing commits in the same tree
  throughout. `ox-eval` 394 → 410 is entirely this task.
