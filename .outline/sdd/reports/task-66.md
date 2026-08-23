# Task 66: mappings do not execute, and `v:exception` has no `Vim({cmd}):` prefix

Status: **DONE_WITH_CONCERNS**. Both items are fixed at their seam rather than
per command. 78 of the 106 matrix cells now match the oracle, up from 44; every
remaining divergence is named below with the subsystem that causes it, and none
of them is in the mapping-execution or exception-formatting path this task owns.

Oracle for every string in this report: `.references/neovim/build/bin/nvim`,
v0.13.0-dev-1390, API level 15. Base: `1f781ad`.

| SHA | subject |
| --- | --- |
| `4162479` | `feat(ox-editor): run mappings from :normal, and decode key notation` |
| `a653f6e` | `fix(ox-editor): prefix v:exception with Vim({cmdname}) and echo the command line` |
| `6d9ea71` | `fix(ox-editor): stop recursive mapping expansion at 'maxmapdepth'` |

`6d9ea71` is a third commit rather than part of `4162479` because it fixes a
defect that only *became reachable* once `:normal` started applying mappings:
`nmap ,x ,x` hung the process. It is named separately in §4.

One accounting note: the commit that became `6d9ea71` was first made with
`git add -A crates` and swallowed six `crates/ox-eval/` files belonging to
Task68FloatCoercion. That was my error; Task68 split them back out with my
agreement, which is why `6d9ea71` is not the SHA I originally created.

---

## 1. Item 1: the before inventory

Every cell below is the *same* script run through both binaries, one process
each, fresh throwaway `HOME` and `XDG_*` per process, `-u NONE -i NONE
--noplugin --headless -S probe.vim`, output written with `writefile()` so
nothing depends on message rendering. Nothing ran inside `.references`.

The inventory answers the brief's first question — *what executes today* —
before anything was changed:

| invocation | mapping executes before this task? |
| --- | --- |
| `feedkeys(k, 't')` then `feedkeys('', 'x')` | **yes** |
| `feedkeys(k, 'tx')` | **yes** |
| typed input through the interactive path | **yes** |
| `:normal` | **no** — never, for any mode or right-hand side |
| `:normal!` | no (correct: `:normal!` must not remap) |

So task 63's "mappings never execute" was precisely one seam:
`command_normal` called `ModeMachine::feed_keys`, which loops over `chars()`
and calls `execute_key` directly. That path never touches the typeahead buffer,
and mapping lookup lives in `ModeMachine::check`, which only runs on typeahead.
`:normal` was therefore *identical to* `:normal!`, and task 64's `a6f078f` had
fixed only the `feedkeys()` half.

Three further mapping defects showed up in the same inventory, all of which
break a mapping on **every** invocation path including the ones that worked:

- **`:map` modifiers were not a prefix.** `command_map` scanned the whole
  argument with `contains("<buffer>")` and split the left-hand side off
  whatever was left, so `nnoremap <silent> ,x :cmd<CR>` registered `<silent>`
  as the left-hand side and `,x` did nothing. Same for `<nowait>`,
  `<unique>`, `<script>`, `<special>` and `<expr>`.
- **No key-notation decoding.** `nnoremap ,q ix<Esc>` stored the literal seven
  characters `ix<Esc>` and inserted `x<Esc>` into the buffer. `<Leader>`,
  `<CR>`, `<Tab>`, `<BS>`, `<Space>`, `<lt>`, `<Bar>` and `<C-x>` were all
  literal text on both sides.
- **`<expr>` was unreachable.** `MappingAction::Expr(u64)` held a callback
  identity nothing ever registered.

### The typed interactive path

`-s {scriptin}` is unimplemented in this port (`normal-mode script mode is not
yet wired`), so the typed path was measured the way a UI delivers keys instead:
`--embed --headless` over msgpack-rpc, `nvim_input` with the keys, then a
`nvim_command` that writes the observations to a file. That is the real typed
path — `nvim_input` enters the typeahead as *typed* input, not as a mapping —
and it exercises `Server::drive_input`, which the fix rewired.

---

## 2. Item 1: the oracle comparison, before and after

`M` = matches the oracle byte for byte, `D` = diverges. 106 cells.

### A–C: right-hand-side kind x invocation (normal mode)

| cell | before | after |
| --- | --- | --- |
| A1 `nnoremap ,x :let g:r='EX'<CR>` via `:normal` | D | **M** |
| A2 same via `:normal!` | D | D (§5.1) |
| A3 same via `feedkeys(',x','t')` + flush | M | M |
| A4 same via `feedkeys(',x','tx')` | M | M |
| A5 same typed (`-s`) | D | D (§5.2) |
| A5' same typed (rpc `nvim_input`) | M | M |
| B1 `nnoremap ,d dd` via `:normal` | D | **M** |
| B2 same via `:normal!` | M | M |
| B3 same via `feedkeys 't'` + flush | M | M |
| B4 same via `feedkeys 'tx'` | M | M |
| B5 same typed (`-s`) | D | D (§5.2) |
| B5' same typed (rpc) | M | M |
| C1 `nnoremap ,c <Cmd>let g:r='CMD'<CR>` via `:normal` | D | **M** |
| C2 same via `:normal!` | M | M |
| C3 same via `feedkeys 't'` + flush | M | M |
| C4 same via `feedkeys 'tx'` | M | M |
| C5 same typed (`-s`) | D | D (§5.2) |
| C5' same typed (rpc) | M | M |

```
A1  nvim : r='EX' ; buf=['aaa', 'bbb', 'ccc']
    before: r='none' ; buf=['aa', 'bbb', 'ccc']
    after : r='EX' ; buf=['aaa', 'bbb', 'ccc']

B1  nvim : r='none' ; buf=['bbb', 'ccc']
    before: r='none' ; buf=['aaa', 'bbb', 'ccc']
    after : r='none' ; buf=['bbb', 'ccc']

C1  nvim : r='CMD' ; buf=['aaa', 'bbb', 'ccc']
    before: r='none' ; buf=['aaa', 'bbb', 'ccc']
    after : r='CMD' ; buf=['aaa', 'bbb', 'ccc']
```

### D: recursive versus non-recursive

| cell | before | after |
| --- | --- | --- |
| D1 `nmap ,n ,m` via `:normal` | D | **M** |
| D2 `nnoremap ,n ,m` via `:normal` | D | **M** |
| D3 `nmap` via `feedkeys 'tx'` | M | M |
| D4 `nnoremap` via `feedkeys 'tx'` | M | M |
| D5' `nmap` typed (rpc) | M | M |
| D6' `nnoremap` typed (rpc) | M | M |
| K14 three-deep `nmap ,1 ,2 ,3` via `:normal` | D | **M** |

```
D1  nvim : r='M'    before: E523: no previous search pattern (process aborted)
    after: r='M'
D2  nvim : r='none' before: E523: no previous search pattern (process aborted)
    after: r='none'
```

### E–H: the other modes

| cell | before | after |
| --- | --- | --- |
| E1 `inoremap ,i XYZ` via `:normal i,i<Esc>` | D | **M** |
| E2 same via `feedkeys 'tx'` | M | M |
| E3 `inoremap ,j <Cmd>...<CR>` via `feedkeys 'tx'` | M | M |
| E4 same typed (`-s`) | D | D (§5.2) |
| E5' same typed (rpc) | M | M |
| F1 `xnoremap ,v :<C-u>let ...<CR>` via `:normal v,v` | D | D (§5.3) |
| F2 same via `feedkeys 'tx'` | D | D (§5.3) |
| F3 `vnoremap ,w d` via `feedkeys 'tx'` | M | M |
| F4 `xnoremap ,w d` via `:normal Vj,w` | D | **M** |
| F5' `xnoremap ,w d` typed (rpc) | M | M |
| G1 `onoremap ,o 2j` via `:normal d,o` | D | **M** |
| G2 same via `feedkeys 'tx'` | M | M |
| G3' same typed (rpc) | M | M |
| H1 `cnoremap ,k 42` via `:normal :let g:r=,k<CR>` | D | D (§5.4) |
| H2 same via `feedkeys 'tx'` | M | M |
| H3 same typed (`-s`) | D | D (§5.2) |
| H4' same typed (rpc) | M | M |
| K8 `snoremap` via `gh` | D | D (§5.5) |

```
E1  nvim : buf=['XYZaaa', 'bbb', 'ccc']
    before: buf=[',iaaa', 'bbb', 'ccc']
    after : buf=['XYZaaa', 'bbb', 'ccc']
G1  nvim : buf=['']   before: buf=['aaa', '', 'bbb', 'ccc']   after: buf=['']
```

### I: `<buffer>`-local

| cell | before | after |
| --- | --- | --- |
| I1 `nnoremap <buffer> ,b` via `:normal` | D | **M** |
| I2 the same mapping is invisible in another buffer | M | M |
| I3 `<buffer>` beats a global with the same lhs | D | **M** |
| I4' `<buffer>` typed (rpc) | M | M |
| L4 `<buffer> <silent>` combined | D | **M** |

### J: ambiguity and timeout

`'timeout'`, `'timeoutlen'`, `'ttimeout'` and `'ttimeoutlen'` all read the same
as upstream (`1 / 1000 / 1 / 50`, cell J3 M before and after), but nothing in
this port is driven by a timer. What is modelled is the *end-of-input* half of
`vgetorpeek`'s rule: with no further key possible, an incomplete mapping
"behaves like it timed out".

| cell | before | after |
| --- | --- | --- |
| J1 `,x` and `,xy` mapped, `:normal ,x` → shorter wins | D | **M** |
| J2 `:normal ,xy` → longer wins | D | **M** |
| J5 only `,xy` mapped, `:normal ,x` → keys run literally | D | D (§5.1) |
| J6 same via `feedkeys 'tx'` | M | M |
| J7 shorter wins via `feedkeys 'tx'` | M | M |
| J8' shorter wins typed (rpc) | M | M |
| J4 `<nowait>` prefers the complete match | M | M |

```
J1  nvim : r='SHORT'   before: r='none'   after: r='SHORT'
J2  nvim : r='LONG'    before: r='none'   after: r='LONG'
```

### K–M: `:normal`'s own contract, and the modifiers

| cell | before | after |
| --- | --- | --- |
| K5 `:normal!` leaves already-queued input alone | M | M |
| K6 mapping whose rhs is `:normal ddx<CR>` | D | **M** |
| K7 the same through `feedkeys 'tx'` | M | M |
| K9 `2,3normal! Ax` repeats per addressed line | D | **M** |
| K10 `:normal!` ignores a mapping to keys | M | M |
| K11/K12 `<expr>` with a `\<CR>` string escape | D | D (§5.6) |
| K15 a mapping in `:normal` is one undo block | D | **M** |
| L1 `<silent>` | D | **M** |
| L2 `<nowait>` | D | **M** |
| L3 `<unique>` | D | **M** |
| L5 `maparg()` | D | D — `maparg()` is unimplemented |
| L7 `:normal ihello` cursor after the implicit ESC | D | **M** |
| L8 `:normal ihello` then `:normal! x` | D | **M** |
| L9 `:normal cw` then `x` | M | M |
| L10 `:normal vl` then `:normal! x` | D | D (§5.7) |
| M1/M2 `:normal` skips leading whitespace in its argument | M | M |
| M3 `<unique>` duplicate raises E227 | D | **M** (via item 2) |
| O1/O3 `:normal!` argument ending in a bare CR | D | D (§5.4) |
| O2 the same with a key after the CR | M | M |
| O4 the same through `feedkeys` | M | M |
| P1 `nnoremap ,q ix<Esc>` | D | **M** |
| P2 `nnoremap ,q ox<CR>y<Esc>` | D | **M** |
| P3 `nnoremap <F2> ...` | D | D (§5.8) |
| P4 `nnoremap <C-x> ...` fed as `"\<C-x>"` | D | D (§5.6) |
| P4b/P4c the same lhs fed as the raw byte | M | M |
| P5 `:<C-u>` inside an Ex-command rhs | D | D (§5.3) |
| P6 `<Space>` | D | **M** |
| P7 `<Leader>` | D | **M** |
| P8 `<Tab>`/`<BS>` | D | **M** |
| P9 `<lt>`/`<Bar>` | D | **M** |
| P10/P11 `<expr>` | D | **M** |
| P12 `<Nop>` | M | M |
| R1 `nmap ,x ,x` terminates | D (hung) | **partial** (§4) |

```
L7  nvim : col=5   before: col=6   after: col=5
L8  nvim : buf=['hellaaa',...]  before: ['helloaa',...]  after: ['hellaaa',...]
K9  nvim : buf=['aaa','bbbx','cccx']
    before: buf=['aaax','bbb','ccc']
    after : buf=['aaa','bbbx','cccx']
P1  nvim : buf=['xaaa',...]  before: ['x<Esc>aaa',...]  after: ['xaaa',...]
P7  nvim : r='LDR'  before: r='none'  after: r='LDR'
```

**Cells matching: 44 before → 78 after, of 106.**

---

## 3. Item 1: what changed, and why there

**`:normal` goes through the typeahead.** `ex_normal` stuffs its argument with
`ins_typebuf(cmd, forceit ? REMAP_NONE : REMAP_YES, 0, true, false)`
(`ex_docmd.c:7263-7268`) and then runs `exec_normal`'s loop. The remap argument
is the *only* difference between `:normal` and `:normal!`; nothing else in the
two paths differs. `command_normal` now does the same: save the typeahead
(`save_typeahead`, `ex_docmd.c:7096`), push the argument as **not typed**, drain
it, restore. The keys being not-typed is what preserves task 64's distinction —
they never reach `may_sync_undo`, so one `:normal` is still one undo block
(cell K15, and `a_mapping_run_by_normal_stays_one_undo_block`).

**One owner for "drain the queue and run what it produces."**
`ModeMachine::check` expands a mapping whose right-hand side is *keys* by
pushing them back onto typeahead, but an Ex-command, `<Cmd>` or `<expr>`
right-hand side can only be *parked* there — nothing below the host can run a
command. That handling was duplicated in `builtins/eval.rs` (feedkeys) and
`oxvim/src/server.rs` (the input loop), and absent from `:normal`, which is
exactly how a mapping came to execute on one path and be silently dropped on
another. It is now `excmd_exec::drain_typeahead`, reached by all three:
`:normal` and `feedkeys()` directly, and the host loop through the new
`ExExecutor::run_typeahead`. `Server::drive_input` is nine lines and holds no
mapping knowledge at all.

**The implicit ESC.** `vgetorpeek` returns ESC when the typeahead is empty and
`ex_normal_busy` is set, so an argument that ends half-way through an insert or
a command line cannot hang. Only Insert and Cmdline ask for another character;
`exec_normal`'s loop simply stops for the others, which is why upstream's
`:normal v` leaves a selection active (§5.7). The ESC is pushed
un-remappable, which is what makes the step provably terminate.

**Modifier parsing is a prefix loop**, `str_to_mapargs`
(`mapping.c:400-451`), and it consumes `<buffer>`, `<nowait>`, `<silent>`,
`<special>`, `<script>`, `<expr>` and `<unique>` in any order before the
left-hand side is read.

**Key notation.** `Keys::parse_notation` is `replace_termcodes`
(`keycodes.c`) for the notation that names a **byte**: the ASCII control and
named keys, `<C-x>`, and `<Leader>`/`<LocalLeader>`. The leaders are read from
the live `Scope`, not from the editor's `g:` dictionary, because the two are
only synced at the end of a program — a script that sets `g:mapleader` and
defines a `<Leader>` mapping on the next line would otherwise see the old
value (`leader_expands_from_the_value_set_earlier_in_the_same_script`).
Notation naming an internal three-byte key is deliberately left literal; see
§5.8.

**The end-of-queue timeout** is `ModeMachine::timeout_pending_mapping`.
`check` still *waits* on a prefix, because the interactive path really can
receive another key; the drain loop times it out when it sees no key ready and
input still queued. With a complete match behind the prefix that match wins;
with none, the front byte is marked un-remappable (upstream clears one byte of
`typebuf.tb_noremap`) and the keys run as themselves.

---

## 4. The defect the fix uncovered: recursive mapping

`nmap ,x ,x` expanded its own right-hand side forever. The port had no
`mapdepth` counter, so `test_mapping.vim` did not merely fail — it never
terminated, and the before-measurement of that file is a 300-second timeout
with no output at all. This was already reachable through `feedkeys()` before
this task (task 64's `a6f078f`); `:normal` made it reachable from a great deal
more code.

`vgetorpeek` counts mapping applications since the last character was
*returned* and gives up at `'maxmapdepth'`, default 1000. `apply_mapping`
increments, `check` clears the counter when it pops a key, so a long chain that
terminates is unaffected (`maxmapdepth_bounds_how_far_a_mapping_chain_expands`
pins both sides of the boundary with `set maxmapdepth=2`).

```
R1  nmap ,x ,x  then  :normal ,x
    nvim  : done=1
    before: <TIMEOUT 300s>
    after : exc='Vim(normal):E223: recursive mapping' ; done=1
```

Named divergence: upstream reports E223 as a message and lets the rest of the
script run; this raises it, so a `:catch` sees it. Terminating was the point;
matching the message-versus-exception behaviour needs the `:normal`
abort-on-error semantics of §5.1.

---

## 5. Item 1: cells still diverging, with the cause

Every one of these is outside the mapping-execution seam.

**5.1 `:normal` does not abort on an error (A2, J5).** `:normal! ,x` with `,x`
unmapped leaves the buffer untouched upstream, because `,` with no previous
`f`/`t` errors and `:normal` stops. Here `,` is a silent no-op, so `x` runs and
deletes a character. Two missing pieces: `;`/`,` must fail without a previous
find, and `:normal` must stop on `did_emsg`. `feedkeys()` does *not* abort
upstream either (J6 matches), so this is specific to `:normal`.

**5.2 `-s {scriptin}` is unimplemented (A5, B5, C5, E4, H3).** `oxvim` exits
with `normal-mode script mode is not yet wired`. This is a CLI feature, not a
mapping one; the typed path is proven instead through `--embed` +
`nvim_input`, where all eleven cells match.

**5.3 `:<C-u>cmd<CR>` as a right-hand side (F1, F2, P5, K13).**
`MappingAction::parse_rhs` recognizes a `:`-prefixed body and parses it as Ex
*text*, so `<C-u>` reaches the Ex parser and raises E488. Upstream stores the
whole thing as keys and lets command-line mode handle `<C-u>`. Closing it needs
three things this port does not have: dropping the Ex-text shortcut for the `:`
form, `<C-u>` in command-line mode, and visual-mode `:` (which upstream
prefills with `'<,'>` — the reason `<C-u>` is written there in the first place).
That is a command-line-mode task, not a mapping one.

**5.4 A trailing CR is trimmed off every Ex argument (H1, H3, O1, O3).**
`crates/ox-excmd/src/parser.rs:359` builds `ExCommand.args` with
`args.trim_end()`, so `:normal! :let g:r=7<CR>` loses the CR and the command
line is abandoned by the implicit ESC instead of being executed. Upstream never
trims: `ea.arg` is a pointer into the line. Proven to be the cause rather than
a cmdline-mapping problem by O2, which is byte-identical except that a key
follows the CR, and matches. Not changed here: it is one line in a crate this
task does not own, and it changes the argument of *every* command, several of
which trim it again themselves.

**5.5 Select mode does not exist (K8).** `gh` does nothing, so `snoremap`
cannot be reached. `map_mode` has no `MapMode::Select` arm either.

**5.6 `ox-eval`'s `\<Key>` string escape (K11, K12, P4).** `"\<CR>"` and
`"\<C-x>"` do not produce the bytes a mapping now matches — task 64 recorded
the same thing for `:normal`. P4b and P4c show the `<C-x>` left-hand side
matching correctly when the byte is delivered as itself, so the mapping side is
right and the escape is not.

**5.7 `ModeMachine` is not persistent across Ex commands (L10, K2).**
Upstream's `:normal vl` leaves Visual mode *active* — `restore_current_state`
restores `State` but not `Visual.active` — so a following `:normal! x` deletes
the selection. Here every `:normal` builds a fresh `ModeMachine`, so the
selection is dropped. `feedkeys()` has the same shape. Fixing it means moving
the mode machine into `Editor`, which reaches `oxvim/src/server.rs` and the
whole test corpus; it is a distinct ownership change.

**5.8 Notation for internal three-byte keys (P3).** `<F2>`, `<Up>` and friends
stay literal. Their encoding is produced independently by `ox-eval`'s `\<Key>`
escape and by the RPC input decoder, and the three do not agree; decoding it
here alone would make `nnoremap <F2>` match a sequence nothing produces, which
is worse than leaving it literal. `<M-x>`/`<A-x>` are absent for the same
reason — there is no meta-key input path to produce the byte.

---

## 6. Item 2: `Vim({cmd}):` and `append_command`

`get_exception_string` (`ex_eval.c:383-401`) builds an error exception's value
as `Vim({cmdname}):{message}`, where `cmdname` is
`cmdnames[ea.cmdidx].cmd_name` — the *canonical* name — and NULL for a user
command or an unresolvable one, which gives `Vim:`. `append_command`
(`ex_docmd.c:2993-3019`) appends `": "` and the command line, but only for the
errors `do_one_cmd` raises while *reading* it (`ex_docmd.c:2375-2384`).

`ExRuntime` now carries the command `do_one_cmd` is running, exactly as
upstream carries it in a global, and `ExRuntime::exception` reads it. Threading
it through the dispatcher instead would mean touching several hundred
`error_flow` call sites and would still miss the next one: an error can be
raised anywhere below the command — in expression evaluation, in a nested
function, in a buffer mutation — and every one has to produce the same prefix
without knowing it exists. `run_program` sets it per instruction and restores
it on the way out, so a nested `:normal` → `:let` failure keeps `let`.

### The oracle table

Ten commands, each caught with `:try`/`:catch` and `v:exception` written out.

```
:foldopen (no fold)
  nvim : Vim(foldopen):E490: No fold found
  before: E490: No fold found
  after : Vim(foldopen):E490: No fold found

:99print  (inside a :try, so the line is indented two spaces)
  nvim : Vim(print):E16: Invalid range:   99print
  before: E16: Invalid range
  after : Vim(print):E16: Invalid range:   99print

:undojoin after :undo
  nvim : Vim(undojoin):E790: undojoin is not allowed after undo
  before: E790: undojoin is not allowed after undo
  after : Vim(undojoin):E790: undojoin is not allowed after undo

:echo g:nope
  nvim : Vim(echo):E121: Undefined variable: g:nope
  before: E121: Undefined variable: g:nope
  after : Vim(echo):E121: Undefined variable: g:nope

:let x = g:nope
  nvim : Vim(let):E121: Undefined variable: g:nope
  before: E121: Undefined variable: g:nope
  after : Vim(let):E121: Undefined variable: g:nope

execute '99print " q | b"'      <-- a quote AND a bar in the command line
  nvim : Vim(print):E16: Invalid range: 99print " q | b"
  before: E16: Invalid range
  after : Vim(print):E16: Invalid range: 99print " q | b"

:definitelynotacommand          <-- no cmdidx, so upstream prefixes Vim:
  nvim : Vim:E492: Not an editor command:   definitelynotacommand
  before: E492: Not an editor command
  after : Vim:E492: Not an editor command:   definitelynotacommand

:nunmap ,zzz
  nvim : Vim(nunmap):E31: No such mapping
  before: (silent success -- E31 was not raised at all)
  after : Vim(nunmap):E31: No such mapping

:foldo                          <-- abbreviation reports the canonical name
  nvim : Vim(foldopen):E490: No fold found
  before: E490: No fold found
  after : Vim(foldopen):E490: No fold found

:call NoSuchFunc()
  nvim : Vim(call):E117: Unknown function: NoSuchFunc
  before: E117: Unknown function: NoSuchFunc
  after : Vim(call):E117: Unknown function: NoSuchFunc

:throw 'boom'                   <-- a user exception keeps no prefix
  nvim : boom      before: boom      after : boom
```

The quote-and-bar case is the one that shows `append_command` really is a byte
copy: neither the `"` nor the `|` is escaped, and the `|` is not treated as a
command separator inside the echoed text. The `:99print` case shows the same
for whitespace — the two spaces of `:try` indentation are part of the line
upstream echoes, which is why each instruction records the line *as written*
rather than the re-rendered form used to rebuild a `:function` body.

### The suffix is a set, not "always"

`:undojoin xyz` is `Vim(undojoin):E488: Trailing characters: xyz:   undojoin
xyz` while `:call Foo()trailing` is `Vim(call):E488: Trailing characters:
trailing` — same code, one appended and one not, because the first comes from
`do_one_cmd`'s own `EX_EXTRA` check and the second from `ex_call`. So the
suffix is decided in two named places: the address codes (E16, E493, which are
the only ones range resolution produces) and the parser's own failures. It is
*not* decided by the code alone, which the first draft did and which put a
spurious suffix on `:call`'s E488.

### Unterminated blocks report `Vim:`

Oracle-checked, because stamping them with the opener looked right and is not:

```
source a file containing "if 1"      nvim : Vim:E171: Missing :endif
source a file containing "while 1"   nvim : Vim:E170: Missing :endwhile
source a file containing "try"       nvim : Vim:E600: Missing :endtry
source a file containing "function F()"
                                     nvim : Vim(function):E126: Missing :endfunction
```

`do_cmdline` notices a missing closer after its loop, where no command is
current. `:function` is the exception and keeps its name, because it consumes
the rest of the input itself. All four now match.

### Six existing expectations moved

Every replacement string was read from the oracle first, not inferred from the
failure:

| test | new expectation | oracle |
| --- | --- | --- |
| `missing_endif_produces_e171_error` | `Vim:E171: Missing :endif` | `Vim:E171: Missing :endif` |
| `missing_endtry_produces_e600_error` | `Vim:E600: Missing :endtry` | `Vim:E600: Missing :endtry` |
| `sleep_rejects_an_unknown_suffix_with_e475` | `Vim(sleep):E475: Invalid argument: x` | same |
| `default_expression_evaluates_in_caller_scope_and_aborts_call` | `Vim(call):E121: ...` | `Vim(call):E121: Undefined variable: s:nope` |
| `evaluator_error_inside_user_function_enters_caller_catch_frame` | `Vim(call):E117: ...` | `Vim(call):` confirmed by the E121 case above |
| `e488_from_call_trailing_characters` | `Vim(call):E488: Trailing characters` | `Vim(call):E488: Trailing characters: trailing` |

The last one is a partial match recorded as such in the test: the prefix is
right and the absence of a suffix is right; naming the offending text in the
message is a separate gap in `:call`'s own argument check.

### Item 2 cells still diverging

- **A parse error loses the command name.** `:sleep 0m` is
  `Vim:E939: Positive count required:   sleep 0m` here against upstream's
  `Vim(sleep):E939: ...`, and `:undojoin xyz` likewise reports `Vim:`.
  Upstream still has `ea.cmdidx` when the failure is *after* the name resolved;
  `ox_excmd::ParseError` does not carry the command it had already resolved, so
  there is nothing to stamp. Fixing it means adding a field to `ParseError` in
  `crates/ox-excmd`, which this task does not own. The prefix and the suffix
  are both otherwise correct on this path.
- **`E580` for every unmatched closer.** `:endif` alone is
  `Vim(endif):E580: :endif without matching opener` here against
  `Vim(endif):E580: :endif without :if: endif`; `:else` should be E581 and
  `:catch` E603, and each appends the offending word. The prefix is right; the
  code and message text are a pre-existing defect.
- **`:execute` and `:command` do not split on `|`.** `execute "let g:ok=1 |
  99print"` raises `Vim(let):E15: invalid character 0x7c in expression`, and a
  `:command` body containing a bar is truncated. Both are unrelated to
  exception formatting and both were in the way of two of my probes.
- **`:set nosuchoption` reports `Vim(set):E518: Unknown option: suchoption`** —
  the `no` prefix is stripped before the message is built. Prefix right, text
  wrong, pre-existing.

---

## 7. Test counts

`PATH=/home/alpha/.cargo/bin:$PATH RUSTC_WRAPPER="" cargo test -p ox-editor -p
ox-text -- --test-threads=1`

| crate | before | after |
| --- | --- | --- |
| `ox-editor` | 767 | **790** |
| `ox-text` | 23 | **23** |

0 failed in both.

### Mutation checks

Each was made on a byte copy of the single file, run against the tests it
should break, and restored (`touch` afterwards so cargo could not serve a stale
binary).

| mutation | fails |
| --- | --- |
| `:normal` always inserts with `Remap::No` (bang ignored) | `normal_applies_a_mapping_and_normal_bang_does_not`, `normal_applies_a_mapping_to_keys_with_decoded_notation`, `normal_honors_the_recursion_flag_of_the_mapping_it_ran`, `a_mapping_run_by_normal_stays_one_undo_block` (4 of 11 in the filter) |
| `VimException::message` drops the `Vim(...)`/`Vim:` prefix | 8 tests, including all four new item-2 tests and all six moved expectations |
| `exception` never calls `append_command` | `a_command_line_error_echoes_the_line_it_could_not_read` only — so the prefix and the suffix are pinned independently |
| `drain_typeahead` never times out a pending mapping | `an_incomplete_mapping_times_out_at_the_end_of_the_queue` |
| `split_map_modifiers` returns the argument untouched | `map_modifiers_are_stripped_before_the_left_hand_side` |
| `Keys::parse_notation` decodes nothing | `mapping_rhs_decodes_key_notation_into_bytes`, `mapping_notation_expands_both_leaders`, `normal_applies_a_mapping_to_keys_with_decoded_notation` (3 of 6 in the filter) |

The recursion guard's own mutation is the state the code was in before
`6d9ea71`: with the counter removed, `a_self_recursive_mapping_stops_at_maxmapdepth`
does not fail, it hangs, which is how the defect was found in the first place.

The item-1 commit was checked out into a detached worktree with its own
`CARGO_TARGET_DIR` and `cargo check --workspace` is clean there, so neither
commit is a non-compiling intermediate.

---

## 8. Oldtest, before and after

Both columns come from the same harness and differ only in the binary:
`/tmp/t66-before-wt/target/debug/oxvim` built from `1f781ad` in a detached
worktree, and the working tree's binary after all three commits. Every run got
a freshly created throwaway `HOME` with `TMPDIR` and the four `XDG_*` roots
inside it, and its own `shutil.copytree` of `testdir` under `/tmp/t66-old`;
nothing ran in `.references`, and `HOME` never pointed at a real home
directory. `timeout 300`, stdin `/dev/null`.

```
<binary> -u NONE -i NONE --noplugin --headless \
  --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim <file>
```

| file | before | after |
| --- | --- | --- |
| `test_mapping.vim` | **timed out at 300 s, 0 executed** | **50 executed / 46 with errors / 1 skipped** |
| `test_undo.vim` | 41 executed / 29 with errors / 5 skipped | **41 executed / 29 with errors / 5 skipped** |

`test_undo.vim` is byte-identical before and after, which is the point: the
typed-versus-mapped distinction task 64 established survived `:normal` moving
onto the typeahead. Its numbers also reproduce task 64's exactly.

`test_mapping.vim` had no "before" numbers to compare against because it never
finished — the recursive-mapping hang of §4 killed the run. 50 executed with 46
erroring is the first time that file has produced a result at all.

Counting note: the committed `testdir` carries a stale `test.log` from earlier
runs, which is copied along with everything else, so the failure counts above
are read from the `messages` file only. Anyone repeating this should delete
`test.log` from the copy first.

---

## 9. Concerns

- **`:normal` does not stop on an error.** §5.1. It is the last structural
  piece of `:normal`, it is what makes E223 a message rather than an exception
  upstream, and it will change oldtest outcomes in both directions.
- **The mode machine is per-command.** §5.7. `:normal v` losing its selection
  is the visible symptom; the cause is that `ModeMachine` is constructed by
  `:normal` and by `feedkeys()` rather than owned by `Editor`. Anything
  stateful in normal mode — a pending operator, the last visual, a count —
  cannot survive a command boundary until that moves.
- **A trailing CR is trimmed off every Ex argument.** §5.4,
  `crates/ox-excmd/src/parser.rs:359`. One line, but it changes the argument of
  every command; it needs an owner who can sweep the commands that re-trim.
- **`ParseError` carries no command name.** §6. It costs `Vim(sleep):` and
  `Vim(undojoin):` on the parse-error path, and it is additive in
  `crates/ox-excmd`.
- **`replace_termcodes` is half-done.** §5.8. The three producers of internal
  key encodings — mapping notation, `ox-eval`'s `\<Key>` escape, and the RPC
  input decoder — need to agree before `<F2>` can be decoded anywhere. That
  disagreement is also task 64's case 4b.
- **`maparg()`/`mapset()` are unimplemented**, so `test_mapping.vim` cannot
  introspect a mapping even now that mappings run. This is likely a large share
  of its 46 erroring functions.
- **E223 is thrown, not reported.** §4. It terminates, which was the
  requirement, but a script that upstream would carry on running will abort.
- **`has('localmap')` can be revisited.** Task 63 pinned it at 0 because
  mappings never executed. `nnoremap <buffer>` now resolves local-first, beats
  a global with the same left-hand side, and is invisible from another buffer
  (I1, I2, I3, I4', L4 all match), so the capability the name describes is
  present. I did not flip it: `has()` lives in `crates/ox-eval`, which another
  leaf owns, and the honest flip should be made by whoever can also measure
  what it un-skips.
- **One shared-tree note.** `crates/ox-eval` was being edited by two peers
  throughout this task. The three commits here touch `crates/ox-editor` and
  `crates/oxvim` only; the commit that became `6d9ea71` initially swallowed six
  of Task68's `ox-eval` files through `git add -A crates`, and Task68 split them
  back out. Use an explicit pathspec.
