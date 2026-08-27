# Task 58 — the message-routing seam

## The upstream rule, from source

`message.c` `msg_puts_len` is the single funnel, and it decides in this order:

1. **Capture first, unconditionally.** `redir_write(str, len)`
   (`message.c:2406`) feeds `:redir`, a register, a variable and `execute()`'s
   `capture_ga` *before* any display decision, so a message that is never
   printed is still captured. Verified against the oracle: `nvim -es` with
   `call writefile([execute("echo 42")], …)` writes `42` even though nothing
   reaches a stream.
2. **`:silent` returns early.** `msg_silent != 0` (`message.c:2409`).
3. **printf or display.** `msg_use_printf()` (`message.c:3013-3016`) is
   `!embedded_mode && !ui_active() && !ui_has(kUIMessages)`: the text goes to a
   stream exactly when no `--embed` peer and no attached UI can display it.
   `main.c:332` starts a built-in UI only when a terminal exists and none of
   `--headless`, `--embed`, `-es`/`-Es` was given, which is why those three
   modes land in the printf branch. Otherwise `msg_puts_display`
   (`message.c:2448`) hands it to the UI.
4. **Inside printf** (`msg_puts_printf`, `message.c:3019-3063`): the text is
   dropped entirely while `silent_mode && p_verbose == 0` (`message.c:3038`),
   written to **stdout** while `info_message` is set (`message.c:3047`), and
   otherwise written to **stderr** (`message.c:3049`).
5. **`info_message` is set by the informative listing commands**, which also
   clear `silent_mode` for the duration: `print_line` (`ex_cmds.c:1701-1725`,
   `:print`/`:number`/`:list`, comment "Also in silent mode") and `showoneopt`
   (`option.c:4851-4882`) plus `do_set`'s trailing newline
   (`option.c:1676-1683`). So `:print` and `:set` display survive `-es` on
   stdout while a neighbouring `:echo` is dropped.

`silent_mode` is set by `-es`/`-Es` (`main.c:1296`), `-e -` (`main.c:1133`) and
`-l` (`main.c:1436`, together with `p_verbose = 1`, which is why `-l` still
prints). `headless_mode` is set by `--headless` (`main.c:1172`) and `-l`
(`main.c:1435`).

Separation, not termination: `msg_start` (`message.c:1770`) emits the newline
*before* the next message, in the stream the previous message used — measured
as `A\nB` with no trailing newline for two `--headless` `:echo`s, and as
`E\n` on stderr + `nonumber` on stdout for `:echo` followed by `:set number?`.
Batch mode is the exception: `print_line` (`ex_cmds.c:1721`) and `do_set`
(`option.c:1680`) add a newline after their output because a "batch mode
message should always end in newline".

## What changed

`Editor` now carries the process state the rule needs as a plain field,
`message_routing: MessageRouting { embedded, silent, ui_attached }`, and
records a `MessageDestination` (`Stderr`, `Stdout`, `Ui`, `Suppressed`) for
every message as it is pushed. Messages stay retained whatever the
destination, because upstream captures before it displays (point 1 above), so
`execute()`, `:redir` and `:silent` are untouched. `push_info_message` is the
`info_message` entry point; six call sites in `excmd_exec.rs` use it
(`:print`, `:z` output, bare `:set`, `:set opt?`).

`oxvim` sets the routing in `apply_startup_options`, sets `'verbose'` from
`-V{level}` (which is what defeats batch-mode suppression), tracks
`ui_active()` in `nvim_ui_attach`/`nvim_ui_detach`, and writes messages
through `messages.rs`'s `PrintfSink`, which reproduces upstream's separation.
`run_stdio` now exits before waiting on stdin when a startup command quit
(`main.c` `getout`), so `--headless -c … -c 'qall!'` no longer blocks.

## Oracle comparisons

Script and flags identical on both binaries; bytes are hex, `-u NONE -i NONE`.

| case | stream | `nvim` v0.13.0-dev-1390 | `oxvim` |
| --- | --- | --- | --- |
| `--headless -c 'echo "HELLO"' -c 'qall!'` | stdout | *(empty)* | *(empty)* |
| | stderr | `48454c4c4f` (`HELLO`) | `48454c4c4f` |
| `-es` ← `echo "HELLO"` | stdout | *(empty)* | *(empty)* |
| | stderr | *(empty)* | *(empty)* |
| `-es -V1` ← `echo "HELLO"` (third case) | stdout | *(empty)* | *(empty)* |
| | stderr | `48454c4c4f0a` (`HELLO\n`) | `48454c4c4f0a` |

The third case is the one the rule predicts and that no reading of the old
code would have got right: `-es` alone suppresses, but a nonzero `'verbose'`
sends the same message to stderr — where the old code sent every `:echo` to
stdout regardless of mode, and never set `'verbose'` from `-V` at all.

Four further comparisons, all byte-identical after the change:

- `--headless -c 'echo "E"' -c 'set number?'` → stdout `nonumber`, stderr `E\n`
  (the separator lands in the previous message's stream).
- `--headless` two `:echo`s → stderr `A\nB`, no trailing newline.
- `-es` ← `%print` over two lines → stdout `PRE\nPLUS\n`.
- `-es -V1` with `%print` then `echo g:pre` → stdout `one\ntwo\nthree\n`,
  stderr `first+second\n`.

## Tests

`cargo test -p ox-editor -p oxvim -- --test-threads=1`

| | brief baseline | after (peers' tests included) |
| --- | --- | --- |
| `ox-editor` | 730 | 749 |
| `oxvim` | 56 | 61 |

Zero failures. New tests, each mutation-checked by reverting the routing
decision and confirming the failure:

| test | mutation | result |
| --- | --- | --- |
| `message_output_follows_the_process_mode` | drop the `silent && verbose == 0` branch | fails: "-es :echo must not reach stderr" |
| `message_output_follows_the_process_mode` | drop batch mode's trailing newline | fails: `HELLO` vs `HELLO\n` |
| `message_output_follows_the_process_mode` | server path back to chrome-only | fails: `--headless :echo belongs on stderr` |
| `informative_listings_keep_stdout_in_batch_mode` | drop the `info_message` override | fails: stdout empty vs `one\nnonumber\n` |
| `default_fork_forwards_every_startup_flag` | stop forwarding `-R` | fails: flags never reach the child |
| `readonly_mode_reaches_every_loaded_startup_buffer` | drop the per-buffer `'readonly'` write | fails |
| `window_height_flag_sets_the_window_option` | reject every separate `-w` argument | fails |
| `a_startup_quit_skips_the_ex_input_loop` | let the quit fall through to the input loop | fails: "piped Ex input ran after a startup quit" |

Two existing tests changed, because they observed `:echo` under `-es`, which
upstream suppresses: they now use the `batch_verbose` (`-es -V1`) helper and
read stderr. Their expectations were cross-checked byte for byte against
`nvim` first.

Two verification hazards worth recording. Restoring a mutated file with a
copy that preserves its mtime leaves cargo's artifact looking newer than the
source, so the *mutated* binary answers the next test run: restore, then
touch. And `git checkout -- crates/` as a mutation undo reverts every dirty
file in the tree, not the mutated one; it destroyed a peer's uncommitted work
here. Copy the single owned file aside and restore from that copy.

## Audit findings (separate commits)

1. **Startup flags never reached the interactive editor.** The default mode
   re-execs `oxvim --embed`, and `interactive_child_arguments` forwarded only
   config, commands, verbosity, plugin state and files — so `-R`, `-m`, `-M`,
   `-n`, `-b`, `-o`/`-O`/`-p` and `-w` had no effect at all in the mode users
   run, while every batch and headless test passed. The whole parsed command
   line is now rebuilt for the child. `-` is deliberately still absent: the
   child's stdin is the RPC channel, so it has no buffer text to read, and
   forwarding it would feed msgpack into a buffer. The regression test drives
   the real TUI through a PTY and reads the options back out of the running
   child.
2. **`-R` reached one buffer.** It is `readonlymode` (`main.c:1286`), and
   `open_buffer` (`buffer.c:258`) marks every buffer it loads for a named
   file, so `-R -o a b` now leaves both read-only, while windows padded with
   fresh empty buffers stay writable (upstream requires `b_ffname != NULL`).
   Confirmed against `nvim`: `W1=1 W2=1 W4=0`.
3. **`-w 42` was rejected.** `main.c:1473` takes the separate `-w` argument as
   the `'window'` value whenever it starts with a digit; only a non-numeric
   argument is the script-recording file. Both forms are accepted now, and a
   non-numeric `-w`, like `-W`, still names the missing subsystem.
4. **A startup quit did not stop the batch loop.** `execute_lines` swallowed
   the quit, so `-es -c 'qa!'` went on to execute piped stdin. `run_batch` now
   ends at the first quit like `main.c` `getout`, and returns the requested
   status, which fixes a second half of the same defect: `-es -c 'cquit 7'`
   exited 0 where upstream exits 7.

## Known gaps, named rather than faked

- **No CR translation.** `msg_puts_printf` turns `\n` into `\r\n` while
  `!info_message && !silent_mode && !headless_mode` (`message.c:3041`). The
  only oxvim mode that reaches it is `-e` without `-s`, and upstream's `-e` is
  interactive Ex mode, which cannot be driven non-interactively to measure, so
  the translation is not implemented rather than guessed.
- **Separator stream for consecutive listings from different commands.**
  Upstream decides the separator's stream inside the producer: `print_line`
  sets `info_message` before emitting it (so consecutive printed lines
  separate on stdout), while `do_set` emits it before `showoneopt` runs (so
  `--headless -c 'set number?' -c 'set binary?'` puts a lone `\n` on
  **stderr**). The sink models this as "the previous message's stream", which
  matches every other measured case but puts that lone newline on stdout.
- **`--headless` still runs the stdio RPC server.** Upstream `--headless`
  opens no stdio channel; only `--embed` does. Unchanged here, out of scope.
- **`'verbosefile'` is inert.** `-V3log.txt` now sets `'verbose'`, so messages
  reach stderr as upstream does, but nothing is written to the file.
- **`AppError` text is not a sink message.** A failing Ex command in batch
  mode still prints `oxvim: Ex command failed: …` to stderr and exits 1, where
  upstream `-es` exits 1 silently. That is the harness error path, not
  `push_message`.
- **`:set opt?` resolves a buffer-local option differently.** For a buffer
  with no local value, `set readonly?` reports the wrong value where
  `&readonly` is right (visible as `set readonly?` saying `readonly` in a
  padded window). Pre-existing, in `options.rs`, and owned by nobody in this
  slice.
- **`-w42` keeps `'window'` at 42.** Upstream's `win_init_size()` later resets
  `p_window` to `Rows - 1` (23 under `-es`), so `nvim` reports 23. Pre-existing
  divergence, pinned by an existing test; untouched here.
