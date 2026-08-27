# Task 67: `exists()` over-claiming, and the float family

Scope: concerns (1) and (6)/§6 of `.outline/sdd/reports/task-63.md`. Owned crate: `crates/ox-eval/`.
One narrow edit outside it, agreed with `Task66Mappings` over `hub` before and after landing:
`crates/ox-editor/src/builtins/eval.rs`, confined to `exists_with_editor` plus two new private
predicates appended after it. Because that crate was being edited concurrently, the committed tree
was validated on its own, in a detached worktree at `db95ea7` with its own `CARGO_TARGET_DIR`:
`cargo test -p ox-eval -p ox-editor -- --test-threads=1` gives 470 and 767 passed, zero failures.

Oracle for every claim: `.references/neovim/build/bin/nvim`, v0.13.0-dev-1390, API level 15.
Every binary comparison used `/tmp/oxvim-t67-before` (built from `fcd02d7`, the commit before this
task), `/tmp/oxvim-t67-after1` (item 1 only) and `/tmp/oxvim-t67-after2` (both items). Every oldtest
run used a freshly created throwaway `HOME` with isolated `XDG_*`/`TMPDIR` and its own copy of
`testdir` under `/tmp`; nothing ran inside `.references`.

## 1. Commits

| sha | subject |
| --- | --- |
| `5744718` | fix(ox-eval): answer exists() for what this port can call and execute |
| `a4ec17b` | feat(ox-eval): render floats like vim_snprintf and add the float builtins |

## 2. Item 1: what was wrong, and what the answer is keyed on now

`exists('*name')` asked `builtin_spec()`, which is the generated `eval.lua` inventory — 452 entries,
one per builtin Neovim has. `exists(':cmd')` asked `resolve_command()` over the generated command
table — 564 entries. Both tables are statements about *upstream*. Neither is a statement about what
this port runs, so both answers were true of Neovim and false here.

That matters more than an isolated wrong answer, because `check.vim` is built on exactly these two
calls (`check.vim:41-53`): `CheckFunction` is `exists('*' .. name)` and `CheckCommand` is
`exists(':' .. name)`. Both guards were therefore inert, and an inert guard is worse than a missing
one — the file proceeds into code that cannot work, and the failures it produces are noise on top of
the real ones.

The distinguishing fact was already in the tree, unnamed and private: `Builtins::dispatch` gated on
an `is_implemented(name)` predicate before doing any work, and returned `E117` for everything else.
That predicate is the answer to "can this be called", so it is now `pub fn is_builtin_implemented`,
named for the question it answers rather than for where it sat.

Three predicates, each derived from the place that actually serves the name:

| predicate | where | covers |
| --- | --- | --- |
| `ox_eval::is_builtin_implemented` | `builtins.rs` `dispatch` | 118 typval-only builtins |
| `builtins::route` | `ox-editor/src/builtins/mod.rs` | the editor-stateful families |
| `is_executed_command` | `ox-editor/src/builtins/eval.rs` | 179 of 564 Ex command names |

`exists('*name')` is the union of the first two, plus user functions as before. `is_executed_command`
has three arms because a command name is served in three places: `dispatch`'s 142 match arms
(`excmd_exec.rs:903-1034`), the control-flow openers and closers `run_program` interprets before
`dispatch` is reached (`if`/`while`/`for`/`try`/`function` and their closers), and
`ox_excmd::ModifierKind`. Modifiers are in because upstream's `cmd_exists` answers for them out of
`cmdmods` *before* it consults the command table, and a modifier has no execution separate from the
command it decorates — the Ex parser recognising it is the whole question.

### The probe table

35 probes, one script, three binaries. `after` is `/tmp/oxvim-t67-after1`.

| probe | oracle | before | after |
| --- | --- | --- | --- |
| `*strlen` (implemented builtin) | 1 | 1 | 1 |
| `*winnr` (implemented editor builtin) | 1 | 1 | 1 |
| `*setqflist` (generated, unimplemented) | 1 | 1 | **0** |
| `*foldtextresult` (generated, unimplemented) | 1 | 1 | **0** |
| `*timer_start` (generated, unimplemented) | 1 | 1 | **0** |
| `*NoSuchFunc` (unknown name) | 0 | 0 | 0 |
| `*MyFunc` (user function) | 1 | 1 | 1 |
| `*v:lua.vim.trim` | 1 | 0 | 0 |
| `:write` (full match) | 2 | 2 | 2 |
| `:w` (unambiguous abbreviation) | 1 | 1 | 1 |
| `:wshada` (unimplemented command) | 2 | 2 | **0** |
| `:diffthis` (unimplemented command) | 2 | 2 | **0** |
| `:cscope` (absent from both tables) | 0 | 0 | 0 |
| `:Fo` (ambiguous user command) | 3 | 3 | 3 |
| `:Foo` (user command, full match) | 2 | 2 | 2 |
| `:NoSuchCmd` (unknown) | 0 | 0 | 0 |
| `:vertical` (modifier, full) | 2 | 2 | 2 |
| `:vert` (modifier, abbreviation) | 1 | 1 | 1 |
| `:sil` (modifier, abbreviation) | 1 | 1 | 1 |
| `:write foo` (trailing garbage) | 0 | 0 | 0 |
| `g:answer` (variable set) | 1 | 1 | 1 |
| `g:missing` (variable unset) | 0 | 0 | 0 |
| `strlen` (function name without the star) | 0 | 0 | 0 |
| `&tabstop` (option, `&` form) | 1 | 1 | 1 |
| `+tabstop` (option, `+` form) | 1 | 1 | 1 |
| `&nosuchopt` (unknown option) | 0 | 0 | 0 |
| `$HOME` (env var set) | 1 | 1 | 1 |
| `$OX_T67_NOPE` (env var unset) | 0 | 0 | 0 |
| `#OxT67Grp` (autocmd group) | 1 | 1 | 1 |
| `#OxT67Absent` (absent group) | 0 | 0 | 0 |
| `##BufWritePre` (supported event) | 1 | 1 | 1 |
| `##NoSuchEvent` (unknown event) | 0 | 0 | 0 |
| `v:true` | 1 | 1 | 1 |
| `v:t_number` | 1 | 1 | 1 |
| `v:nosuchvar` | 0 | 0 | 0 |

Five deliberate divergences from the oracle, all of the same shape: the name exists upstream and does
not work here, so 0 is the true answer about *this* binary. `*v:lua.vim.trim` diverges too, but it
diverged before this task and in the same honest direction.

**Which forms were already right: all six the brief named.** A variable (set and unset), a function
name without the star, an option in both the `&` and `+` forms, an unknown option, `$env` set and
unset, `#group` present and absent, and `v:` names including the `v:t_*` constants Task 65 added —
every one of those matched the oracle before this task and is untouched by it. So did four command
rules that had to survive the change: full match 2, unambiguous abbreviation 1, ambiguous user
command 3, trailing garbage 0.

Eight further forms probed while in there, none of which this task changes:

| probe | oracle | before | after |
| --- | --- | --- | --- |
| `*strlen()` (trailing parens) | 1 | 0 | 0 |
| `*strlen&6` (garbage after a valid name) | 0 | 0 | 0 |
| `:2match` (the digit-skipping rule) | 2 | 0 | 0 |
| `:3buffer` (a count is not a command) | 0 | 0 | 0 |
| `:edit/a` (trailing garbage) | 0 | 0 | 0 |
| `&nojoinspaces` (the `no` prefix) | 0 | 0 | 0 |
| `#OxT67Grp#BufWritePre` (group and event) | 1 | 1 | 1 |

`*strlen()` and `:2match` are pre-existing gaps and both fail conservatively (0 where upstream says
1 or 2), so neither can make a guard inert. `:2match` would also need `:match` implemented before
its answer could be anything but 0.

### The corpus effect

Three files, same harness, differing only in the binary.

| file | guard | executed | failed | skipped |
| --- | --- | --- | --- | --- |
| `test_sha256.vim` | `CheckFunction sha256`, **file level** | 1 → **0** | 1 → **0** | 0 → **1** |
| `test_functions.vim` | `CheckFunction strftime` / `strptime`, in-function | 110 → 110 | 70 → 70 | 2 → 2 |
| `test_exists.vim` | the dedicated `exists()` oracle file | 2 → 2 | 1 → 1 | 0 → 0 |

`test_sha256.vim` is the win the brief predicted: it now skips honestly on its own guard instead of
executing one test that dies on `E117: not implemented: sha256`. It is a one-test wall, not a large
one, which is the honest size of it.

`test_exists.vim` is the file that could most easily have been broken, and its `messages` output is
**byte-identical** before and after, path prefix aside. Its 11 remaining failures are autocmd
groups and events, options, env vars and dict variables — none of them the `*` or `:` forms this
task touched. Three more files were measured as controls and are unchanged in all three columns:
`test_usercommands.vim` (22/19/1), `test_options.vim` (89/74/9), `test_vimscript.vim` (0/0/0).

**`test_functions.vim` does not move, and the reason is a second defect that this fix cannot reach.**
Of the 22 `CheckFunction`/`CheckCommand` sites in the corpus, exactly one, `test_sha256.vim:5`, is
at file level. The other 21 sit inside a test function, and there the guard never gets as far as
`exists()`: it dies on `E492: Not an editor command` for `CheckFunction` itself. Isolated:

```
source check.vim
call writefile(['script-level exists(:CheckFunction)=' . exists(':CheckFunction')], 'x5a.txt')
func Test_probe()
  call writefile(['in-function exists(:CheckFunction)=' . exists(':CheckFunction')], 'x5b.txt')
endfunc
```

run through `runtest.vim` gives `script-level ... =2` and `in-function ... =0`. A user command
defined during `source` is visible at the sourcing script's own level and invisible inside a function
body invoked from another script — which is how `runtest.vim` calls every test. Proof that the
`exists()` half is nonetheless correct: the same probe with the command defined in the *calling*
script goes `in-function MyGuard ran` → `in-function MyGuard: Skipped: sha256` across the two
binaries. The guard works the moment it can be reached. See §5.

## 3. Item 2: float rendering

Rendering was fixed before the builtins, because it silently changed output that scripts compare:
`string(1.0)` was `'1'`.

Rust's `Display` and `LowerExp` are not C's `%g` and `%e` — and upstream's `%g` is not C's either.
`vim_snprintf` (`strings.c:2093-2101`) refuses to call `%g` at all, with the comment "can't use %g
directly, cause it prints 1.0 as 1": it rewrites `%g` to `%f` when `|x|` is in `[0.001, 1e7)` or
zero and to `%e` otherwise, then strips the superfluous zeroes itself while keeping the one directly
after the dot. That kept zero is the whole of `'1.0'`. `format_float` follows that path, including
the sign flags (`+` beats a space whichever order they appear in, `strings.c:1516,1585`), the
`infinity_str` table (`strings.c:800`, where a negative value ignores the sign flags), the unsigned
`nan`, the `TMP_LEN - 10` precision cap, and zero padding inserted after the sign.

`string()` goes through `TYPVAL_ENCODE_CONV_FLOAT` (`encode.c:351-372`), which is that same `%g`
plus a re-readable `str2float('inf')` / `str2float('nan')` for the two values `%g` cannot round-trip.
`printf`'s bad-argument error is E807, which is `tv_float`'s (`strings.c:716`), not the E808 that
`tv_get_float` gives `sqrt("a")`.

### Oracle comparison

42 probes through the real binaries. `before` is blank in the whole table because the script aborts
on the first `acos` call in that binary (`Ex command failed: not implemented: acos`), which is
itself the item-2 baseline. **41 of 42 match the oracle exactly**; the one that does not is a
different function and is recorded in §5.

| expression | oracle | after |
| --- | --- | --- |
| `string(1.0)` | `1.0` | `1.0` |
| `string(0.0)` | `0.0` | `0.0` |
| `string(-0.0)` | `-0.0` | `-0.0` |
| `string(1.23)` | `1.23` | `1.23` |
| `string(9999999.9)` | `9999999.9` | `9999999.9` |
| `string(1.0e20)` | `1.0e20` | `1.0e20` |
| `string(1.0/0.0)` | `str2float('inf')` | `str2float('inf')` |
| `string(-1.0/0.0)` | `-str2float('inf')` | `-str2float('inf')` |
| `string(0.0/0.0)` | `str2float('nan')` | `str2float('nan')` |
| `printf('%f',1.0)` | `1.000000` | `1.000000` |
| `printf('%g',9999999.9)` | `9999999.9` | `9999999.9` |
| `printf('%e',123.456)` | `1.234560e+02` | `1.234560e+02` |
| `printf('%.8g',10000000.1)` | `1.00000001e7` | `1.00000001e7` |
| `printf('%06.2f',-1.0/3.0)` | `-00.33` | `-00.33` |
| `printf('%010.2e',1.0/3.0)` | `003.33e-01` | `003.33e-01` |
| `printf('%f',1.0/0.0)` | `inf` | `inf` |
| `printf('%-6F',-1.0/0.0)` | `-INF` | `-INF` |
| `printf('%06f',0.0/0.0)` | `   nan` | `   nan` |
| `printf('%s',0.0/0.0)` | `str2float('nan')` | `str2float('nan')` |
| `1.0 . ''` | `1.0` | **`E806`** (§5) |

### Builtins landed (17), one oracle case each

| builtin | expression | oracle and after |
| --- | --- | --- |
| `acos` | `string(acos(0))` | `1.570796` |
| `asin` | `string(asin(1))` | `1.570796` |
| `atan` | `string(atan(1))` | `0.785398` |
| `atan2` | `string(atan2(-1,1))` | `-0.785398` |
| `cos` | `string(cos(0))` | `1.0` |
| `cosh` | `string(cosh(0.5))` | `1.127626` |
| `exp` | `string(exp(2))` | `7.389056` |
| `fmod` | `string(fmod(12.33,1.22))` | `0.13` |
| `isinf` | `isinf(1.0/0.0)`, `isinf(-1.0/0.0)`, `isinf(1)` | `1`, `-1`, `0` |
| `isnan` | `isnan(0.0/0.0)`, `isnan(1.0)` | `1`, `0` |
| `log` | `string(log(10))` | `2.302585` |
| `log10` | `string(log10(1000))` | `3.0` |
| `round` | `round(0.456)`, `round(4.5)`, `round(-4.5)` | `0.0`, `5.0`, `-5.0` |
| `sin` | `string(sin(0))` | `0.0` |
| `sinh` | `string(sinh(0.5))` | `0.521095` |
| `tan` | `string(tan(0.5))` | `0.546302` |
| `tanh` | `string(tanh(0.5))` | `0.462117` |

None declined. Twelve are `float_op_wrapper` (`funcs.c:344`) over the same-named libm function;
`atan2` and `fmod` are its two-argument form; `round` is half-away-from-zero, which Rust's
`f64::round` and C's `round` agree on and `floor(x + 0.5)` does not. `isinf` and `isnan` answer only
for a Float (`funcs.c:3141-3154`), so `isinf(1)` is 0 rather than an error — a Number never carries
an infinity.

### Corpus effect

`test_float_func.vim`, the file Task 63's §6 argued into running: **26 executed / 26 failed →
26 executed / 5 failed.** 21 of the 26 closed. `test_expr.vim` is unchanged at 33/26; its
`Test_printf_float` failure was already one of many in that file and its float assertions are inside
`CheckLegacyAndVim9Success`, which fails ahead of them for its own reasons.

The 5 residual failures in `test_float_func.vim` are a work list, and none of them is rendering:

- `Test_str2float` (8 assertions): `str2float()` does not parse `inf`, `-inf`, `nan` or a leading
  space — `str2float('inf')` is `0.0` where the oracle gives infinity. `vim_str2float` handles those
  spellings; our prefix scan takes only digits, sign, dot and `e`.
- `Test_float_misc`, `Test_abs`: String-to-Float coercion. `abs('-12')` is 0 against the oracle's 12.
- `Test_abs` also wants E703 where we give E745.
- `Test_float2nr` (2 assertions): the saturation boundary Task 63 already named —
  `-9223372036854775808` against the oracle's `-9223372036854775807`.

## 4. Tests

`cargo test -p ox-eval -- --test-threads=1`: **465 → 470**, zero failures. Five new tests.

| test | pins |
| --- | --- |
| `exists_star_answers_only_for_builtins_this_port_can_call` | 1 for callable, 0 for generated-but-unimplemented, 0 for unknown (and, so it is not tautological, that the three zeroes *are* in the generated table and *do* report `NotImplemented` when called) |
| `every_builtin_claimed_implemented_reaches_a_dispatch_arm` | the other direction: a name the predicate claims must reach a real arm, because the arm-less fallthrough is also `NotImplemented` and `exists()` would over-claim again. Passes a Dict for every argument so no builtin does any work |
| `string_renders_floats_the_way_upstream_encodes_them` | 12 values plus a Float inside a List |
| `printf_float_conversions_match_vim_snprintf` | `Test_printf_float` verbatim (66 conversions, the 330/340/350 precision cap, and E807) |
| `float_builtins_answer_like_libm` | one case per new builtin, `isinf`/`isnan` on a Number and a String, and E808 for `cos("a")` |

Six mutations run, each by copying the file to `/tmp`, editing, running, restoring and touching:

| mutation | caught by |
| --- | --- |
| `remove_trailing_zeroes = false` (do not strip `%g`'s zeroes) | both float-rendering tests (`9999999.900000`) |
| `text[mantissa_end - 2] != '.'` → `true` (drop the keep-the-dot-zero rule) | both (`0.` for `0.0`) |
| `exists`'s `*` arm back on `builtin_spec().is_some()` | `exists_star_...` (`exists('*setqflist')` 1) |
| add `"setqflist"` to `is_builtin_implemented` | `every_builtin_claimed_...` **and** `exists_star_...` |
| `isinf` sign always 1 | `float_builtins_...` |
| `round` as `(x + 0.5).floor()` | `float_builtins_...` (`round(-4.5)` −4.0) |

A seventh, `mantissa_end > 3` → `> 2`, was **not** caught — and should not have been, so the
constant went. Upstream's identical `tp > tmp + 2` bound is unreachable: `%f` and `%e` always emit a
dot ahead of the zeroes, so the dot rule always stops the loop first. The `> 2` that remains is index
safety and says so.

## 5. Concerns

- **`E806: Using a Float as a String` is wrong everywhere, and an existing test pins it that way.**
  Measured: `1.0 . ''` → oracle `1.0`, oxvim E806. `1.5 .. 'x'` → oracle `1.5x`, oxvim E806.
  `strlen(1.0)` → oracle `3`, oxvim E806. `tv_get_string_buf_chk` (`typval.c:4684-4685`) renders
  `VAR_FLOAT` with `%g` and never errors, so a Float coerces to a String in upstream everywhere a
  String is wanted. This is the same rendering defect as the one this task fixed, in a third place,
  and `format_float` is now sitting there ready to serve it. It was left alone because it is five
  call sites across two crates, two of them in `ox-editor`, and because
  `crates/ox-eval/src/tests.rs:231` `error_float_string_concat` **asserts the wrong behavior** and
  cites `vimeval.txt:1121-1131` for it. That test has to be inverted, not deleted, and whoever does
  it should re-read that doc range against the oracle first.
- **`is_executed_command` duplicates `dispatch`'s arm list, and nothing mechanical keeps them in
  step.** The 142 names were extracted from `excmd_exec.rs:903-1034` by parsing the match arms, and
  the doc comment names all three derivation sites, but a new `dispatch` arm will not update the
  predicate. The single-source design is a guard at the head of `dispatch` keyed on the predicate,
  which makes an under-listed name break its own command loudly; it was not done here because
  `ox-editor` was mid-edit by `Task66Mappings` for this whole task, and a four-line change to a
  6407-line executor someone else is holding open is the wrong trade. Do that when `ox-editor` is
  quiet. Two things bound the risk meanwhile: `ox-editor`'s 767 tests pass on the committed tree, so
  no name those tests exercise is under-listed, and the failure mode is conservative anyway. An
  under-listed name makes `exists()` answer 0 for a command that works, which skips a file that
  might have run, never the inert-guard direction this task removed.
- **User commands are invisible inside a function body invoked from another script.** Measured in §2:
  `exists(':CheckFunction')` is 2 at the sourcing script's level and 0 inside a function that
  `runtest.vim` calls. This keeps 21 of the corpus's 22 `CheckFunction`/`CheckCommand` sites inert
  regardless of `exists()`, and it is the single highest-leverage follow-up to this task — the
  `exists()` half is already correct and waiting for it. It is also the same shape as the defects
  Task 63 flagged: the introspection is right and the plumbing under it is absent.
- **`str2float()` does not parse `inf`/`nan`.** 8 assertions in `Test_str2float`; `vim_str2float`
  is the oracle. Small, self-contained, and the largest single remaining block in
  `test_float_func.vim`.
- **String-to-Float coercion.** `abs('-12')` is 0 against the oracle's 12, and `Test_float_misc`
  fails the same way. `tv_get_float` falls back to the numeric prefix of a String.
- **`exists('*name()')` and `exists(':2match')` still diverge**, 0 against the oracle's 1 and 2.
  Both fail conservatively so neither can make a guard inert; `:2match` also needs `:match`
  implemented before its answer could be anything else. `exists('*v:lua.…')` is 0 for the same
  reason and will stay 0 until Lua callables are reachable.
- **`test_float_func.vim` is now the argument Task 63 made, resolved.** Its §6 recorded that
  `string(3.0) == '3'` meant a Float was observably not a Vim Float in string context, and that the
  fix was the rendering rather than the `has('float')` answer. That is done: 26 failures to 5.
  `has('float')` stays 1 and is now less arguable than it was.
- **One shared-tree note.** `crates/ox-editor` was being edited by `Task66Mappings` throughout, so
  the `ox-editor` file in commit `5744718` was staged through `git hash-object` plus `update-index`
  to keep their in-progress work out of it. That makes the two commits atomic in content but never
  co-resident in the working tree, so they were validated separately: `cargo test -p ox-eval` and
  the binary probes during the work, then `cargo test -p ox-eval -p ox-editor` in a detached
  worktree at `db95ea7` afterwards (470 and 767 passed). `oxvim`'s own `--test cli` suite was not
  run and is worth one pass by whoever validates the tree.
