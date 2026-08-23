# Task 68: the four float coercion defects

Scope: the four defects `.outline/sdd/reports/task-67.md` §5 measured, named worst-first in the
brief. Owned crate: `crates/ox-eval/`. Two E806 call sites were in `crates/ox-editor/`; they were
handed over by `Task66Mappings` over `hub` mid-task and are fixed here rather than left as a work
item — see §7.

Oracle for every claim below: `.references/neovim/build/bin/nvim`, v0.13.0-dev-1390, API level 15.
Binary comparisons use `/tmp/oxvim-t68-before`, built in a detached worktree at `6d9ea71` — the
commit immediately before this task's first, so the peer's three `ox-editor` commits are in both
binaries and cannot be mistaken for this task's effect — and `/tmp/oxvim-t68-after`, built at
`fb2b931`. Every oldtest run used a freshly created throwaway `HOME` with isolated `XDG_*`/`TMPDIR`
and its own copy of `testdir` under `/tmp`; nothing ran inside `.references`.

## 1. Commits

| sha | subject |
| --- | --- |
| `513c4a9` | fix(ox-eval): raise E806 only where upstream does and render a Float as %g |
| `6d1e64b` | fix(ox-editor): render a Float as a String at the last two E806 sites |
| `7d2da6c` | fix(ox-eval): parse str2float() the way string2float does |
| `3f34f31` | fix(ox-eval): read a String in a Number context through vim_str2nr |
| `9eaaa2e` | fix(ox-eval): saturate float2nr() at VARNUMBER_MAX, and give trunc() a Float |
| `fb2b931` | fix(ox-eval): negate in abs() the way f_abs does, not saturating |

`6d9ea71` also carries this task's fingerprints and is not this task's work: see §8.

## 2. Defect 1 — E806

**The rule.** `E806: Using a Float as a String` exists in exactly one place upstream:
`check_can_index` (`eval.c:3225-3229`), reached from `eval_index` and from `f_slice`. Nowhere else.
`tv_check_str` (`typval.c:4237-4258`) lists `VAR_FLOAT` among the types that *pass*, and
`tv_get_string_buf_chk` (`typval.c:4684-4685`) renders it with `vim_snprintf("%g")` and cannot fail
— `VAR_FLOAT` is not in the `str_errors` table at all. So a Float is a String everywhere a String
is wanted, and indexing is the single exception. `f_printf`'s `%s`, the concatenation operator, and
every builtin that takes a String go through that same coercion.

`format_float` was already in the tree from task 67, so the rendering was one call away. It is now
`ox_eval::float_as_string`, `pub` because `ox-editor` needs it too.

**Oracle comparison**, 20 probes, both binaries, same script:

| expression | oracle | before | after |
| --- | --- | --- | --- |
| `1.0 . ''` | `'1.0'` | `E806` | `'1.0'` |
| `1.5 .. 'x'` | `'1.5x'` | `E806` | `'1.5x'` |
| `strlen(1.0)` | `3` | `E806` | `3` |
| `'a' . (1.0)` | `'a1.0'` | `E806` | `'a1.0'` |
| `2.5 . ''` | `'2.5'` | `E806` | `'2.5'` |
| `1.0e20 . ''` | `'1.0e20'` | `E806` | `'1.0e20'` |
| `1.0e-5 . ''` | `'1.0e-5'` | `E806` | `'1.0e-5'` |
| `0.001 . ''` | `'0.001'` | `E806` | `'0.001'` |
| `9999999.9 . ''` | `'9999999.9'` | `E806` | `'9999999.9'` |
| `-0.0 . ''` | `'-0.0'` | `E806` | `'-0.0'` |
| `(1.0/0.0) . ''` | `'inf'` | `E806` | `'inf'` |
| `(-1.0/0.0) . ''` | `'-inf'` | `E806` | `'-inf'` |
| `(0.0/0.0) . ''` | `'nan'` | `E806` | `'nan'` |
| `toupper(1.5)` | `'1.5'` | `E806` | `'1.5'` |
| `substitute(1.5, '5', 'x', '')` | `'1.x'` | `E806` | `'1.x'` |
| `split(1.5, '\.')` | `['1', '5']` | `E806` | `['1', '5']` |
| `str2nr(1.5)` | `1` | `E806` | `1` |
| `strchars(1.0)` | `3` | `E806` | `3` |
| `matchstr(1.5, '5')` | `'5'` | `E806` | `'5'` |
| `join([1.0, 2.5], ',')` | `'1.0,2.5'` | `E806` | `'1.0,2.5'` |

**Where E806 IS correct**, so the fix is not a deletion. All three were `E909`/`E709` before, which
is the same defect from the other side — the error existed everywhere except where it belonged:

| expression | oracle | before | after |
| --- | --- | --- | --- |
| `1.0[0]` | `E806` | **`E909`** | `E806` |
| `1.0[1:2]` | `E806` | **`E709`** | `E806` |
| `1.0[:]` | `E806` | **`E709`** | `E806` |

**One collateral fix was mandatory.** `len()` and `strlen()` were one function here. With a Float
rendering, `len(1.0)` would have answered 3, which is worse than the E806 it answered before: `f_len`
(`funcs.c:3793-3819`) refuses a Float with `E701: Invalid type for len()`. They are two different
questions — `f_strlen` is only `strlen(tv_get_string(...))` — so they are two functions now, and the
split corrects five more answers that had nothing to do with Floats:

| expression | oracle | before | after |
| --- | --- | --- | --- |
| `len(1.0)` | `E701` | `E806` | `E701` |
| `len(v:true)` | `E701` | `6` | `E701` |
| `len(v:null)` | `E701` | `6` | `E701` |
| `len(function('abs'))` | `E701` | `E729` | `E701` |
| `strlen([1,2])` | `E730` | `2` | `E730` |
| `strlen(0z1020)` | `E976` | `2` | `E976` |
| `strlen(function('abs'))` | `E729` | `E729` | `E729` |
| `strlen(v:true)` | `6` | `6` | `6` |
| `len(12)` / `strlen(12)` | `2` / `2` | `2` / `2` | `2` / `2` |
| `len([1,2])` / `len(0z1020)` / `len({'a':1})` | `2` / `2` / `1` | same | same |

### The inverted test

`crates/ox-eval/src/tests.rs:231`, in the `error_cases!` table:

```
(error_float_string_concat, b"1.5 .. 'x'", "E806", "vimeval.txt:1121-1131"),
```

What it asserted, and what it asserts now:

- Old: `1.5 .. 'x'` raises `E806`.
- New: `float_concatenates_as_a_string_and_only_indexing_is_e806`, which asserts `1.5 .. 'x'` is
  `'1.5x'`, plus eight more renderings (`1.0 . ''`, `1 . 90 * 90.0`, the two infinities, NaN,
  `1.0e20`, `-0.0`) **and** that `1.0[0]`, `1.0[1:2]` and `1.0[:]` are still `E806`.

Both halves are in one test on purpose: the rule is not "E806 is wrong", it is "E806 belongs to
indexing", and a test that dropped the error entirely would have passed against a fix that deleted
it.

**Why the citation misled it.** `vimeval.txt:1121-1131` reads, verbatim:

> Since '.' has lower precedence than "\*". This does NOT work, since this attempts to concatenate a
> Float and a String.

about the example `1 . 90 * 90.0`. That is a statement about a Vim old enough to predate
`tv_get_string_buf_chk`'s `%g` arm, and the doc was never updated. Run the doc's own example on the
oracle and it works:

```
1 . 90 * 90.0   => '18100.0'     (oracle)
1 . 90 * 90.0   => E806          (before)
1 . 90 * 90.0   => '18100.0'     (after)
```

The lesson is narrow and worth stating: the `error_cases!` table takes a citation string per row and
nothing checks it, so a row citing prose is as trusted as a row citing `typval.c`. Prose in
`runtime/doc` describes some Vim; only the binary describes *this* Vim. Every row this task touched
now cites a source file and a measurement.

## 3. Defect 2 — `str2float()`

`f_str2float` (`funcs.c:7042-7056`) skips white space, takes an optional sign, then **skips white
space again**, and only `-` sets the sign. `string2float` (`eval.c:4611-4630`) matches `inf`,
`-inf` and `nan` case-insensitively as three- and four-byte *prefixes* ahead of `strtod`, then falls
back to `strtod`, whose grammar includes a `0x` significand with a binary exponent. The old
implementation scanned a prefix of digits, sign, dot and `e` and handed it to Rust's parser, which
knows none of those shapes. All three layers are now here, `strtod` included, because Rust's parser
accepts neither a hexadecimal float nor a trailing garbage tail.

| expression | oracle | before | after |
| --- | --- | --- | --- |
| `str2float('inf')` | `inf` | `0.0` | `inf` |
| `str2float('-inf')` | `-inf` | `0.0` | `-inf` |
| `str2float('+inf')` | `inf` | `0.0` | `inf` |
| `str2float('Inf')` / `str2float('INF')` | `inf` | `0.0` | `inf` |
| `str2float('  -inf')` | `-inf` | `0.0` | `-inf` |
| `str2float('infinity')` | `inf` | `0.0` | `inf` |
| `str2float('inf3')` | `inf` | `0.0` | `inf` |
| `str2float('nan')` / `str2float('NaN')` | `nan` | `0.0` | `nan` |
| `str2float('-nan')` | `nan` | `0.0` | `nan` |
| `str2float('nanx')` | `nan` | `0.0` | `nan` |
| `str2float('- 1.5')` | `-1.5` | `0.0` | `-1.5` |
| `str2float('+ 2.5')` | `2.5` | `0.0` | `2.5` |
| `str2float(' + 1e2 ')` | `100.0` | `0.0` | `100.0` |
| `str2float('0x10')` | `16.0` | `0.0` | `16.0` |
| `str2float('-')` | `-0.0` | `0.0` | `-0.0` |
| `str2float('  \t 3.5')` | `3.5` | `3.5` | `3.5` |
| `str2float('1.5abc')` | `1.5` | `1.5` | `1.5` |
| `str2float('.5')` / `str2float('12.')` | `0.5` / `12.0` | same | same |
| `str2float('abc')` / `str2float('')` | `0.0` | `0.0` | `0.0` |
| `str2float(12)` | `12.0` | `12.0` | `12.0` |

Four details that are easy to get wrong and were each measured rather than assumed. The `inf`/`nan`
match is a **prefix**, so `'infinity'` and `'inf3'` are infinity — a whole-word match fails these.
`-nan` stays a NaN because upstream multiplies by `-1` instead of branching. `str2float('-')` is
`-0.0`, not `0.0`, for the same reason: `strtod` converts nothing, returns `+0.0`, and the sign is
applied afterwards. And `str2float('  ')` is `0.0`, not `-0.0`, because there is no sign to apply.

## 4. Defect 3 — the coercion, and why the brief's diagnosis was wrong

The brief asked for "String-to-Float coercion", citing `abs('-12')` answering 0 against the oracle's
12. **There is no String-to-Float coercion in upstream.** `tv_get_float`
(`typval.c:4413-4415`) answers `E892: Using a String as a Float` for a String and `tv_get_float_chk`
(`typval.h:393-406`) answers `E808: Number or Float required`, and the oracle agrees:

```
sqrt('12')      !! E808: Number or Float required     (oracle, before and after)
cos('a')        !! E808: Number or Float required     (oracle, before and after)
float2nr('12')  !! E808: Number or Float required     (oracle, before and after)
```

`abs()` is not a float context for a non-Float argument. `f_abs` (`funcs.c:424-441`) takes the
`fabs` path *only* when `argvars[0].v_type == VAR_FLOAT` and hands everything else to
`tv_get_number_chk`. So `abs('-12')` is the **Number** 12, and `type(abs('-12'))` is 0 on the
oracle, not 5. The defect is the String-to-**Number** coercion, and it was three separate parsers:
a decimal-only prefix scan in `builtins.rs` (the one `abs` used), a second in `path_builtins.rs`,
and a base-detecting one in `eval.rs` that had three divergences of its own. One
`eval::string_to_number`, faithful to `vim_str2nr(…, STR2NR_ALL, …)` (`charset.c:1219-1406`), now
serves all three.

What upstream does with the two cases the brief asked about: a **non-numeric string** is 0
(`abs('abc')`, `abs('')`), and a **numeric prefix followed by garbage** keeps the prefix
(`abs('12abc')` is 12, `abs('-12abc')` is 12 — both are `Test_abs` assertions).

| expression | oracle | before | after |
| --- | --- | --- | --- |
| `abs('-12')` | `12` | **`0`** | `12` |
| `type(abs('-12'))` | `0` | `0` | `0` |
| `abs('12abc')` / `abs('-12abc')` | `12` | `12` / `12` | `12` / `12` |
| `abs('abc')` / `abs('')` | `0` | `0` | `0` |
| `abs('0x10')` | `16` | **`0`** | `16` |
| `abs('0X1f')` | `31` | `31` | `31` |
| `abs('0b11')` | `3` | **`0`** | `3` |
| `abs('010')` | `8` | **`10`** | `8` |
| `abs('-9223372036854775808')` | `-9223372036854775808` | **`0`** | `-9223372036854775808` |
| `abs(function('abs'))` | `E703` | **`E745`** | `E703` |
| `abs(0z10)` | `E974` | **`E745`** | `E974` |
| `abs([])` / `abs({})` | `E745` / `E728` | same | same |
| `abs(v:true)` / `abs(v:null)` / `abs(-4)` | `1` / `0` / `4` | same | same |
| `abs(-1.23)` (the Float path) | `1.23` | `1.23` | `1.23` |

The `E703` row is the one task 67 flagged as "`Test_abs` also wants E703 where we give E745"; it
falls out of using `num_errors` (`typval.c:4171-4181`) verbatim, which also supplies the `E974`.

`abs('-9223372036854775808')` needed a second fix (`fb2b931`), found by re-probing the fixed binary:
`f_abs` is `n > 0 ? n : -n` and that negation is plain, so `VARNUMBER_MIN` negates back to itself.
`saturating_abs` clamped it to `VARNUMBER_MAX`. The wrong value was unreachable before `3f34f31`,
because the old parser answered 0 for any string with a sign in it.

**Three divergences in the evaluator's own coercion, fixed by the same unification.** These are not
`abs()`; they are `'…' + 0` and every other Number context. They are recorded here because they
change observable behavior and no one asked for them — the alternative was leaving a second,
divergent parser next to the correct one:

| expression | oracle | before | after |
| --- | --- | --- | --- |
| `' 12' + 0` | `0` | **`12`** | `0` |
| `' -12 ' + 0` | `0` | **`-12`** | `0` |
| `'+12' + 0` | `0` | **`12`** | `0` |
| `'08' + 0` | `8` | **`0`** | `8` |
| `'-12' + 0` / `'12 ' + 0` | `-12` / `12` | same | same |
| `'0x10' + 0` / `'0o17' + 0` / `'0X1f' + 0` | `16` / `15` / `31` | same | same |

No white space is skipped (`vim_str2nr` has no `skipwhite`), `+` is not a sign, and a leading zero
selects octal only while every digit stays octal — `ptr[1]` being `8` or `9` skips base detection
outright (`charset.c:1276-1277`), which is why `'08'` is decimal 8 and `'019'` is decimal 19 while
`'010'` is octal 8. The unsigned accumulator saturates at `UVARNUMBER_MAX`
(`charset.c:1338-1347`), verified at both ends and past `u64`:

| expression | oracle | after |
| --- | --- | --- |
| `and('9223372036854775808', -1)` | `9223372036854775807` | `9223372036854775807` |
| `and('-9223372036854775808', -1)` | `-9223372036854775808` | `-9223372036854775808` |
| `and('18446744073709551616', -1)` | `9223372036854775807` | `9223372036854775807` |
| `and('99999999999999999999999', -1)` | `9223372036854775807` | `9223372036854775807` |
| `and('-99999999999999999999999', -1)` | `-9223372036854775808` | `-9223372036854775808` |
| `and('0xffffffffffffffffff', -1)` | `9223372036854775807` | `9223372036854775807` |

## 5. Defect 4 — the `float2nr()` boundary

`f_float2nr` (`funcs.c:1484-1500`) is

```c
if (f <= (float_T)(-VARNUMBER_MAX) + DBL_EPSILON) { rettv = -VARNUMBER_MAX; }
else if (f >= (float_T)VARNUMBER_MAX - DBL_EPSILON) { rettv = VARNUMBER_MAX; }
else { rettv = (varnumber_T)f; }
```

Two things in that are traps. The `DBL_EPSILON` terms **do nothing**: `(float_T)VARNUMBER_MAX` is
exactly 2^63, whose neighbouring doubles are 1024 apart, so 2.2e-16 is absorbed and both
comparisons are against ±2^63. And the clamp is `±VARNUMBER_MAX`, so the low end is
-9223372036854775807 — the off-by-one this fixes — not `VARNUMBER_MIN`.

**The boundary, established from both sides on the oracle.** 9223372036854774784.0 is the largest
double strictly below 2^63; it passes both comparisons and comes out of the cast unchanged.
9223372036854775296.0 looks smaller but is not representable and rounds *to* 2^63 on the way in, so
it saturates. Both are in the table, because only the first pins the boundary rather than the clamp:

| expression | oracle | before | after |
| --- | --- | --- | --- |
| `float2nr(9223372036854774784.0)` | `9223372036854774784` | `9223372036854774784` | `9223372036854774784` |
| `float2nr(-9223372036854774784.0)` | `-9223372036854774784` | `-9223372036854774784` | `-9223372036854774784` |
| `float2nr(9223372036854775296.0)` | `9223372036854775807` | `9223372036854775807` | `9223372036854775807` |
| `float2nr(-9223372036854775296.0)` | `-9223372036854775807` | **`-9223372036854775808`** | `-9223372036854775807` |
| `float2nr(pow(2,63))` | `9223372036854775807` | `9223372036854775807` | `9223372036854775807` |
| `float2nr(-pow(2,63))` | `-9223372036854775807` | **`-9223372036854775808`** | `-9223372036854775807` |
| `float2nr(pow(2,64))` | `9223372036854775807` | `9223372036854775807` | `9223372036854775807` |
| `float2nr(-pow(2,64))` | `-9223372036854775807` | **`-9223372036854775808`** | `-9223372036854775807` |
| `float2nr(pow(2,62))` | `4611686018427387904` | `4611686018427387904` | `4611686018427387904` |
| `float2nr(-pow(2,62))` | `-4611686018427387904` | `-4611686018427387904` | `-4611686018427387904` |
| `float2nr(1.0/0.0)` | `9223372036854775807` | `9223372036854775807` | `9223372036854775807` |
| `float2nr(-1.0/0.0)` | `-9223372036854775807` | **`-9223372036854775808`** | `-9223372036854775807` |
| `float2nr(0.0/0.0)` | `-9223372036854775808` | **`0`** | `-9223372036854775808` |
| `float2nr(1.5)` / `float2nr(-1.5)` / `float2nr(-0.5)` | `1` / `-1` / `0` | same | same |

NaN is the fourth trap: every comparison with a NaN is false, so it fails both bounds and reaches
the cast, which on x86-64 gives `INT64_MIN`. The old code special-cased it to 0.

**Collateral, and load-bearing.** `trunc` shared `float2nr`'s dispatch arm and so answered a Number.
It is `float_op_wrapper` over libm's `trunc` (`eval.lua`) and answers a **Float**; `Test_trunc`
compares `string(trunc(2.1))` against `'2.0'`. Leaving it would have applied a Number saturation
rule to a Float function.

| expression | oracle | before | after |
| --- | --- | --- | --- |
| `string(trunc(4.8))` | `'4.0'` | **`'4'`** | `'4.0'` |
| `type(trunc(4.8))` | `5` | **`0`** | `5` |
| `string(trunc(-4.8))` | `'-4.0'` | **`'-4'`** | `'-4.0'` |
| `string(trunc(4))` | `'4.0'` | **`'4'`** | `'4.0'` |

## 6. Tests

`cargo test -p ox-eval -- --test-threads=1`: **470 → 474**, zero failures. `cargo test --workspace
-- --test-threads=1` is green as well, including `ox-editor` at 790 and `ox-excmd` at 160; the
number-coercion change in §4 crosses crate boundaries and needed that pass.

One test removed (`error_float_string_concat`, inverted into a named test), five added:

| test | pins |
| --- | --- |
| `float_concatenates_as_a_string_and_only_indexing_is_e806` | the inversion: nine renderings through concatenation, and the three index/slice forms that keep E806 |
| `builtins_coerce_a_float_to_its_percent_g_rendering` | the builtin side — `strlen`/`strchars`/`toupper`/`str2nr` of a Float, and the whole `len`-versus-`strlen` split including the E701 arms |
| `str2float_parses_what_string2float_parses` | 39 spellings plus the NaN shapes, the signed zeroes through `string()`, and `Test_str2float`'s four type cases |
| `a_string_reaches_a_number_context_through_vim_str2nr` | `Test_abs` verbatim, the three E808 rows proving there is no String-to-Float coercion, and 20 `vim_str2nr` inputs including both saturation ends |
| `float2nr_saturates_at_plus_and_minus_varnumber_max` | `Test_float2nr`'s six rows, both sides of the 2^63 boundary, NaN, and `trunc` as a Float |

### Mutations

17 run, each by copying the single file to `/tmp`, editing, running, restoring from the copy and
`touch`ing it so cargo could not serve a stale binary.

| mutation | caught by |
| --- | --- |
| `string_arg`'s Float arm back to `E806` | `builtins_coerce_…` |
| `to_string`'s Float arm back to `E806` | `float_concatenates_…` |
| drop `index`'s Float arm (fall through to E909) | `float_concatenates_…` |
| drop `slice`'s Float arm (fall through to E709) | `float_concatenates_…` |
| `len`'s E701 arm back to `string_arg` (the pre-split body) | `builtins_coerce_…` |
| `string2float`'s `inf` prefix check disabled | `str2float_…` |
| skip white space only before the sign, not after | `str2float_…` |
| a leading `+` also sets the sign | `str2float_…` |
| hexadecimal float grammar disabled | `str2float_…` |
| `inf` matched as a whole word instead of a prefix | `str2float_…` |
| `number_arg` back to the decimal-only prefix scan | `a_string_reaches_…` |
| `string_to_number` skips leading white space | `a_string_reaches_…` |
| `string_to_number` treats `+` as a sign | `a_string_reaches_…` |
| legacy octal without the all-digits-octal scan | `a_string_reaches_…` |
| Funcref as a Number back to E745 | `a_string_reaches_…` |
| `abs` back to `saturating_abs` | `a_string_reaches_…` |
| `float2nr`'s NaN arm removed | `float2nr_…` |
| `float2nr`'s low clamp back to `VARNUMBER_MIN` | `float2nr_…` |
| `float2nr`'s bound moved off 2^63 by one ulp (`- 2048.0`) | `float2nr_…` |
| `trunc` back on `float2nr`'s arm | `float2nr_…`, `trunc_positive`, `trunc_negative` |

**One survived on the first pass and is worth recording, because it is exactly the failure mode
mutation testing exists to catch.** `string_to_number`'s `saturating_mul`/`saturating_add` changed
to `wrapping_mul`/`wrapping_add` was **not** caught. The reason: the largest input in the test was
`'9223372036854775808'`, which is 2^63 and fits in a `u64` without overflowing, so the accumulator
never reached its own limit and only the final signed conversion clamped. The test was measuring the
`i64::try_from`, not the accumulator. Adding `'99999999999999999999999'`, its negative, and
`'0xffffffffffffffffff'` — all three confirmed against the oracle first — made the accumulator
overflow inside the digit loop, and the mutation is caught. Two more table cases pin the two paths
separately now.

Two table cases were **corrected rather than deleted**, same discipline as the inverted test:
`trunc_positive` and `trunc_negative` asserted `number(4)` and `number(-4)`, which is what `trunc`
answered while it shared `float2nr`'s arm. They assert `Typval::Float(4.0)` and
`Typval::Float(-4.0)` now, with the `Test_trunc` citation in a comment above them.

## 7. Corpus effect

Same harness as the pass-2 census (`.outline/sdd/oldtest-blockers-2.md`), one throwaway `testdir`
and `HOME` per run, differing only in the binary.

| file | executed | failed | skipped |
| --- | --- | --- | --- |
| `test_float_func.vim` | 26 → 26 | **5 → 1** | 0 → 0 |
| `test_expr.vim` | 33 → 33 | 26 → 26 | 0 → 0 |
| `test_format.vim` | 6 → 6 | 6 → 6 | 0 → 0 |
| `test_functions.vim` (control) | 110 → 110 | **70 → 68** | 2 → 2 |
| `test_vimscript.vim` (control) | 0 → 0 | 0 → 0 | 0 → 0 |
| `test_eval_stuff.vim` (control) | 0 → 0 | 0 → 0 | 0 → 0 |
| `test_let.vim` (control) | 0 → 0 | 0 → 0 | 0 → 0 |

`test_float_func.vim` is the file task 67 left a work list against, and four of its five failing
tests closed: `Test_abs`, `Test_float2nr`, `Test_str2float` and `Test_trunc`. `Test_trunc` was
failing before this task and is closed by the collateral fix in §5, which is the evidence that the
collateral was load-bearing rather than tidy-up.

The one remaining failure is **not** a float coercion defect and is out of this crate:
`Test_float_misc` fails three assertions on compound assignment.

```
let v = 1.234 | let v += 6.543   " expected '7.777', got '0.0'
let v = 1.234 | let v += 5       " expected '6.234', got '5.0'
let v = 5     | let v += 3.333   " expected '8.333', got '5'
```

`:let +=` drops the left-hand side when either side is a Float. Its own §9 entry.

`test_expr.vim` and `test_format.vim` do not move, and the reason matters: both are blocked ahead of
their float assertions by `printf`, and the block is a **false E806** — see §9. `test_format.vim` is
the file the census recorded with `E806` as its first blocker, and that E806 was never the float one.

### The two `ox-editor` call sites

The brief expected these to be left named. They are fixed, in `6d1e64b`, after `Task66Mappings`
handed them over unprompted over `hub` ("I am not touching them, they are yours to change once
ox-eval exports `float_as_string`"). Recorded precisely anyway, because the hand-over is the only
reason this is not a boundary violation:

- `crates/ox-editor/src/builtins/mod.rs:128` — `input_string_arg`, the input family's String
  coercion. Needed the `Err(E806)` arm replaced with `Ok(ox_eval::float_as_string(*number))`.
- `crates/ox-editor/src/builtins/eval.rs:136` — `luaeval`'s argument coercion, which is
  `f_luaeval`'s `tv_get_string_chk`. Same replacement, into an owned `String`.

`ox-editor`'s 790 tests pass with both changed.

## 8. One shared-tree note

`crates/ox-editor` was being edited by `Task66Mappings` throughout. Their commit `20b5bb3` was made
with `git add -A crates` while this task's six `ox-eval` files sat staged, and swallowed them. With
their explicit consent over `hub` — and after they confirmed they were between operations —
`20b5bb3` was split into `6d9ea71` (their three `ox-editor` files, their message byte-for-byte) and
`513c4a9` (this task's six `ox-eval` files). `git diff 20b5bb3 HEAD` was empty at that point, so
nothing moved but the boundary. `t68-backup-20b5bb3` still points at the pre-split commit. Their two
earlier commits were untouched. Every commit after that used an explicit pathspec
(`git commit -F … -- <paths>`) rather than the index, which is what makes it impossible to be swept
again.

## 9. Concerns

- **`printf` uses E806 for "Invalid format specifier", and that is a false E806.** Measured:
  `printf('%q', 1)` → oracle `E767: Too many arguments to printf()`, oxvim
  `E806: Invalid format specifier: %q`. `printf('%1$s', 'a')` → oracle `'a'`, oxvim E806.
  `printf('%S', 0.0/0.0)` → oracle `str2float('nan')`, oxvim E806. Upstream has **no**
  invalid-conversion error at all: `vim_snprintf` ignores an unknown conversion, consumes no
  argument, and `f_printf`'s leftover-argument check then produces E767. So E806 still does not mean
  one thing in this tree, which is the whole point of §2. It was left alone deliberately: fixing it
  honestly means implementing skip-and-continue, the `%S` conversion, and positional arguments
  (`%1$s`), which is a `printf` task with its own oracle work, and the cheap half-fix — inventing a
  substitute code — would be worse than the documented divergence. This is what blocks
  `test_expr.vim` and `test_format.vim` (§7), and it is the highest-leverage follow-up to this task.
- **`:let +=` drops a Float operand.** `let v = 1.234 | let v += 6.543` gives `0.0` against the
  oracle's `7.777`; `let v = 5 | let v += 3.333` gives `5`. Three assertions, the only remaining
  failure in `test_float_func.vim`, and the file's last wall. It is in the `:let` executor, not in
  `ox-eval`, so it was not touched here.
- **`1.0 == '1.0'` answers 1 where the oracle answers `E892: Using a String as a Float`.**
  `compare_values`' Float branch coerces the other side with `to_number` where upstream uses
  `tv_get_float`, which refuses a String. Pre-existing, unchanged by this task, and in this crate —
  it is the fourth member of the E805/E806/E808/E892 family and the only one still wrong. Small and
  self-contained; the reason it is not in this task is that it is a comparison defect rather than a
  coercion one, and inverting it will change the answer to expressions that currently succeed.
- **`string_to_number` is byte-oriented and upstream is NUL-terminated.** `vim_str2nr` stops at the
  C string's NUL; `OxStr` can carry an embedded NUL and this scans past it. No observable case was
  found (a digit run cannot contain a NUL), but the two are not identical and a `Blob`-derived
  String could expose it.
- **`hex_float_prefix` accumulates in `f64` rather than rounding correctly.** `strtod` rounds a
  hexadecimal significand once, at the end; this multiplies as it goes, so a hexadecimal float with
  more than 14 significant hex digits can land one ulp away. `str2float('0x10')` and
  `str2float('0x1.8p1')` are exact and pinned; nothing in the corpus reaches the imprecise range.
- **`printf`'s float conversions and `string()`'s were already correct and are untouched.** Worth
  saying explicitly: `float_as_string` is a third caller of `format_float`, not a replacement for
  either, and the 66-conversion `printf_float_conversions_match_vim_snprintf` test from task 67
  still passes unchanged.
