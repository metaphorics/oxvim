# Task 57: command-line flag parity

## Goal

A spot check against the freshly rebuilt oracle shows `oxvim` rejecting flags upstream accepts, including `-c {cmd}`, which is the most common scripting flag there is and which the functional suite leans on heavily. Close the gap for every flag upstream documents.

## Files

You own, and may edit:
- `crates/oxvim/src/cli.rs`, `crates/oxvim/src/main.rs`, `crates/oxvim/src/runtime.rs`, `crates/oxvim/src/server.rs`
- `crates/oxvim/tests/cli.rs` and the other test files under `crates/oxvim/tests/`

Do not edit `crates/ox-excmd/`, `crates/ox-editor/`, `crates/ox-eval/` (peers hold them). Never stage `.outline/GATES.md`.

## Method

1. Build the inventory from upstream rather than from a guess. `.references/neovim/src/nvim/main.c` holds the real parser (`command_line_scan`), and `runtime/doc/starting.txt` documents the surface. Produce the full list of flags upstream accepts, with their argument shapes.
2. For each, compare behavior against the oracle binary at `.references/neovim/build/bin/nvim`, which is built and working. Run each flag with a bounded timeout and stdin redirected from `/dev/null`: some flags (`--embed`, `--listen`, `--remote`, `-d`) wait on input or a socket and will hang a naive probe. That is what killed my own probe, so bound every run.
3. Implement what is missing, in this priority order:
   - `-c {cmd}` and `+{cmd}`, including ordering against `--cmd` (upstream runs every `--cmd` before loading files and every `-c` after)
   - `--version` and `-v`, `--help` and `-h`, matching upstream's exit codes; the text need not match byte for byte, but the exit status and the presence of the version and API level must
   - `-R`, `-m`, `-M`, `-n`, `-b`, `-A`, `-Z`, `-e`, `-E`, `-s`, `--literal`, `--clean`, `--noplugin`
   - `-o`, `-O`, `-p` window and tab openers, `-d` diff mode
   - `--startuptime`, `-w`, `-W`, `--remote` family
4. Anything whose honest implementation needs a subsystem that does not exist (diff mode without a diff engine, for instance) gets named with what it needs, and you move on. Do not accept a flag and then ignore it: a flag that parses and does nothing is worse than one that errors, because scripts cannot detect it.

## Constraints

Exit codes and error text are observable by every script and test harness that drives the binary. `-c` ordering relative to `--cmd` is observable. An unknown flag must still fail the way upstream fails, with upstream's status.

## Verification

- `PATH="/home/alpha/.cargo/bin:$PATH" RUSTC_WRAPPER="" cargo test -p oxvim -- --test-threads=1` green, and report the before and after counts.
- One integration test per implemented flag, asserting the observable effect rather than that parsing succeeded.
- A table in your report: flag, oracle behavior, oxvim behavior, status. Every row measured against the oracle, none inferred.

## Commits

One commit per flag group, prefix `feat(oxvim):`. No formatters, no project-wide suites. Commit locally only: the push token is invalid.
