# Oxvim — workspace build/debug/test targets.

set shell := ["bash", "-uc"]

# Build the workspace in release mode.
build:
    cargo build --workspace --release

# Run the workspace unit/property test suite via nextest.
test:
    cargo nextest run --workspace

# Guard: the oxvim binary must exist before the upstream suites can run.
_guard_binary:
    @test -x target/release/oxvim || { echo "oxvim binary not built yet (later task)" >&2; exit 1; }

# Run upstream Neovim functional tests against oxvim via NVIM_PRG.
functional: _guard_binary
    NVIM_PRG=$PWD/target/release/oxvim make -C .references/neovim functionaltest

# Run upstream Neovim oldtests against oxvim via NVIM_PRG.
oldtest: _guard_binary
    make -C .references/neovim/test/old/testdir NVIM_PRG=$PWD/target/release/oxvim

# Diff oxvim --api-info schema against upstream.
apidiff: _guard_binary
    tests/differential/apidiff.sh

# Replay all semantic RPC seed sessions against upstream and oxvim.
replay: _guard_binary
    cargo run --quiet -p differential --bin replay

# Run the release-binary smoke and PTY differential checks.
differential: _guard_binary
    cargo nextest run -p differential