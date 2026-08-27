# Task 57: command-line flag parity report

## Method

The inventory came from upstream's own parser, `command_line_scan` in
`.references/neovim/src/nvim/main.c:1100-1545`, plus `mainerr`/`usage`/`version`
at `main.c:2287-2360`. Every row below was then measured against the oracle
binary `.references/neovim/build/bin/nvim` (NVIM v0.13.0-dev-1390, API level 15)
with stdin redirected from `/dev/null` and a hard `timeout -s KILL`; `-D`, `+`
alone, `-p x` and `-o 2` do hang a naive probe, which is why every run is
bounded. `oxvim` columns were measured the same way against
`target/debug/oxvim`.

The observation channel for option effects is `-es` batch mode, because it is
the only startup mode whose message stream currently reaches stdout in oxvim
(see *Divergences* below). Window and tab counts were read with
`winnr("$")`/`tabpagenr("$")` on both binaries; the oracle side used
`--headless` plus `writefile(..., "/dev/stderr")` since `-es` suppresses `:echo`
upstream.

## Measured flag table

Status legend: `match` is the same observable effect and status, `decline` is recognized and
rejected by name with the missing subsystem, and `divergent` is an observable
difference that remains.

| Flag | Oracle behavior (measured) | oxvim behavior (measured) | Status |
|---|---|---|---|
| `-c {cmd}` | runs after config and files, argv-ordered with `+cmd` | same | match |
| `-c{cmd}` | inline command, same array | same | match |
| `+{cmd}` | same array as `-c`; `+` alone is `$` | same | match |
| `--cmd {cmd}` | every one runs before config | same | match |
| `-c`/`--cmd` ordering | `pre1 pre2 c1 c2 plus` | `pre1 pre2 c1 c2 plus` | match |
| `-S [file]` | `so {file}`, default `Session.vim`, keeps argv order | same | match |
| `--help`, `-h`, `-?`, `--HELP` | usage on stdout, exit 0 | usage on stdout, exit 0 | match |
| `--version`, `-v` | version + build on stdout, exit 0 | `OXVIM v0.13.0` + `API level 15 (compatible: 0)`, exit 0 | match; text differs by design |
| `--version --bogus` | exit 0; scan never reaches the bad flag | exit 0 | match |
| `--api-info` | msgpack metadata, exit 0 | same | match |
| unknown long (`--bogus`) | `nvim: Unknown option argument: "--bogus"` + `More info with "nvim -h"`, exit 1 | same text with `oxvim`, exit 1 | match |
| unknown short (`-Q`) | `Unknown option argument: "-Q"`, exit 1 | same, exit 1 | match |
| garbage (`-uxx NONE`) | `Garbage after option argument: "-uxx"`, exit 1 | same, exit 1 | match |
| garbage (`--cmdfoo x`) | `Garbage after option argument: "--cmdfoo"`, exit 1 | same, exit 1 | match |
| missing arg (`-u`, `-c`, `--cmd`, `--listen`, `-l`, `-W`) | `Argument missing after: "-u"`, exit 1 | same, exit 1 | match |
| `-s a -s b` | `Attempt to open script file again: "-s b"`, exit **2**, no `-h` line | same, exit 2 | match |
| `--embed -es` | `--embed conflicts with -es/-Es/-l`, exit 1 | same, exit 1 | match |
| clustered (`-Rn`) | `ro=1 uc=0` (three options in one argument) | `readonly`, `updatecount=0`, `binary` for `-Rnb` | match |
| `-u NONE`/`NORC`/`{file}`/`""` | `""` accepted (E282 later, not a usage error) | accepted | match |
| `-i NONE`/`{file}` | shada selection | same | match |
| `-R` | `readonly` set, `updatecount=10000` | `readonly`, `updatecount=10000` | match |
| `-m` | `nowrite` | `nowrite` | match |
| `-M` | `nowrite` + `nomodifiable` | `nowrite` + `nomodifiable` | match |
| `-n` | `updatecount=0` | `updatecount=0` | match |
| `-b` | `binary` | `binary` | match |
| `-w{N}` | sets `'window'` (then clobbered by `'lines'` init under a UI) | `window=42` | match at scan time |
| `-e` | Ex mode, stdin is Ex commands | same | match |
| `-es` | silent Ex mode | same | match |
| `-E`, `-Es` | `input_istext`: stdin becomes buffer text, `+cmd` runs over it (`hello\nworld`) | `hello\nworld` | match |
| `-e -` | silent modifier, not a stdin file | same | match |
| bare `-` | edits stdin (`EDIT_STDIN`); `%print` yields the piped text | writes the piped text through `:w` | match |
| `-` in `-es` | stays the silent modifier | same | match |
| `-o` / `-o2` / `-o5` (3 files) | `3/1`, `2/1`, `5/1` windows/tabs, current window 1 | `3 1 1`, `2 1 1`, `5 1 1` | match |
| `-O` (3 files) | `3/1` | `3 1 1` | match |
| `-p` / `-p3` / `-p1` (3 files) | `1/3`, `1/3`, `1/1` | `1 3 1`, `1 3 1`, `1 1 1` | match |
| `-o` buffer mapping | `a.txt,b.txt,c.txt cur=1` | window 1 `a.txt`, window 2 `b.txt` | match |
| `-o5` past the file count | windows 4-5 hold new empty buffers | new empty buffers | match |
| `-p x`, `-o 2` | the operand is a file name, not a count | same | match |
| `--startuptime {file}` | writes `--- Startup times for process: ...` with `clock elapsed:` lines | same header, marks `OXVIM STARTING`/`parsing arguments`/`sourcing vimrc file(s)`/`opening buffers`/`OXVIM STARTED` | match |
| `--literal`, `--literalxyz` | accepted, no effect (#7679) | accepted, no effect | match |
| `-N`, `-X`, `-f` | accepted, no effect | accepted, no effect | match |
| `-U {gvimrc}` | argument consumed, never sourced | same | match |
| `--noplugin`, `--noplugins` | `'loadplugins'` off | same | match |
| `--clean` | `lpl=1 sd=NONE`, skips user config | same | match |
| `-u NONE` without `--clean` | `'loadplugins'` off | same | match |
| `--headless`, `--embed`, `--listen {addr}` | as before this task | unchanged | match |
| `-l {file} [args]` | headless, silent, no swap, no config, trailing argv to Lua | headless, no swap, no config, trailing argv | match |
| `-V`, `-V3`, `-Vlog`, `-V3log` | verbosity level and file | same | match |
| `--` | only file names after it | same | match |
| `-e -es` | accepted (sets exmode twice) | accepted | match |
| `-d` | diff mode; `&diff` set | `Option not supported: "-d": requires a diff engine`, exit 1 | decline, needs the diff engine |
| `-A` | `E544: Keymap file not found`, `rl=1` | decline, exit 1 | decline, needs the `'arabic'` option side effects (`did_set_arabic`) and keymap files |
| `-H` | `E544`, `rl=1`, `keymap=hebrew` attempted | decline, exit 1 | decline, needs the keymap file loading |
| `-D` | hangs on the debug prompt (probe TIMEOUT) | decline, exit 1 | decline, needs the Ex debugger (`:debug`) |
| `-q {ef}` | `1| error one` in the quickfix list | decline, exit 1 | decline, needs the quickfix list and `'errorformat'` |
| `-t {tag}` | `E433: No tags file` / `E426: Tag not found` | decline, exit 1 | decline, needs the tags subsystem |
| `-r`, `-L` | `Swap files found: -- none --` | decline, exit 1 | decline, needs the swap-file recovery |
| `-w {scriptout}`, `-W {file}` | creates the script-output file | decline, exit 1 | decline, needs the script recording of typed keys |
| `--remote`, `--remote-expr`, `--remote-send`, `--remote-ui` | `E247: Failed to connect ...`, exit 2 (or a missing-runtime Lua error) | decline, exit 1 | decline, needs the RPC client channels (`sockconnect`/`rpcrequest`) and the `vim._cs_remote` runtime module |
| `--server {addr}` | stored; only consumed by `--remote*` | decline, exit 1 | decline, needs the same as `--remote` |
| `--luamod-dev` | disables the Lua module preload table | decline, exit 1 | decline, needs the Lua module preload table (`nlua_disable_preload`) |
| `-s {scriptin}` | replays Normal-mode keys from the file | `normal-mode script mode is not yet wired`, exit 1 | divergent, needs Normal-mode script replay (pre-existing, out of scope) |

## Divergences that remain (3)

1. `-s {scriptin}` parses correctly and reports an explicit not-wired
   error at exit 1 instead of replaying keys. It needs the Normal-mode
   script-replay path, and a caller can detect the failure today.
2. Upstream prints `:echo` output to stderr under `--headless`; oxvim
   flushes nothing on that path, so `oxvim --headless -c 'echo x' -c 'qa!'`
   is silent. The `-c` command itself runs (proved through `-es` and
   through side effects such as `:w`), so this is a message-routing gap in
   the stdio server rather than a flag gap. It is why every effect
   assertion in the test suite uses `-es`.
3. Upstream's `silent_mode` swallows `:echo` under `-es`; oxvim prints it
   to stdout. That is the opposite direction from item 2, over the same
   message-routing seam.

Every declined flag is a fourth category rather than a divergence in the
sense the brief cares about: it is rejected with a nonzero status and a
message naming the requirement, so a script detects it. Nothing in this
change accepts a flag and then ignores it.

## Verification

`PATH="/home/alpha/.cargo/bin:$PATH" RUSTC_WRAPPER="" cargo test -p oxvim -- --test-threads=1`

* before: **36** passing (6 + 10 + 1 + 19)
* after: **56** passing (15 + 21 + 1 + 19), 0 failed

Integration coverage, one test per implemented group, each asserting the
observable effect rather than that parsing succeeded:
`help_and_version_print_and_exit_zero`,
`post_commands_keep_argv_order_after_every_pre_command`,
`batch_runs_pre_and_post_commands_before_reading_stdin`,
`usage_failures_match_upstream_text_and_status`,
`startup_option_flags_reach_their_options`,
`window_and_tab_openers_build_the_startup_layout`,
`improved_ex_mode_reads_stdin_as_buffer_text`,
`bare_dash_edits_standard_input`,
`startuptime_writes_a_timing_log`,
`window_height_flag_sets_the_window_option`,
`upstream_no_op_flags_are_accepted`,
`flags_without_their_subsystem_are_rejected_by_name`.

Three of those were mutation-checked against the source they defend, to
prove they are not tautological:

| mutation | test that failed | observed |
|---|---|---|
| drop the `-R` `'updatecount'` write | `startup_option_flags_reach_their_options` | `updatecount=200` vs `10000` |
| never call `create_startup_windows` | `window_and_tab_openers_build_the_startup_layout` | `1 1 1` vs `3 1 1` |
| run stdin before `+cmd` again | `batch_runs_pre_and_post_commands_before_reading_stdin` | `PRE` vs `PRE\nPLUS` |

## Commits

| SHA | Subject |
|---|---|
| `6072302` | `feat(oxvim): scan the command line the way main.c does` |
| `a4946ec` | `feat(oxvim): print help and version, and exit zero` |
| `c144ec6` | `feat(oxvim): apply the startup option flags, window openers and --startuptime` |

The first two commits were each built and run against the full `-p oxvim`
suite before being made. The scanner in `cli.rs` is a single indivisible
rewrite, so a flag's parse and its effect have to land in the same commit:
the third commit therefore carries three of the brief's flag groups
together, because they share the startup sequence that both the batch and
the RPC-server paths run. Splitting them would have left one path acting
on a flag while the other ignored it, which is the state the contract
forbids.

Not pushed: the push token is invalid, as the brief states.
