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

# Run upstream Neovim functional tests against oxvim. The make target's cmake
# wrapper hardcodes the oracle binary, so invoke RunTests.cmake directly with
# our -D NVIM_PRG.
functional: _guard_binary
    cd "{{justfile_directory()}}/.references/neovim/build/test" && cmake -D TEST_TYPE=functional -D BUILD_DIR="{{justfile_directory()}}/.references/neovim/build" -D CI_BUILD=OFF -D NVIM_PRG="{{justfile_directory()}}/target/release/oxvim" -D TEST_DIR="{{justfile_directory()}}/.references/neovim/test" -D ROOT_DIR="{{justfile_directory()}}/.references/neovim" -P "{{justfile_directory()}}/.references/neovim/cmake/RunTests.cmake"

# Run upstream Neovim oldtests against oxvim via NVIM_PRG.
#
# The suite deletes whatever $HOME points to. setup.vim sandboxes it with
# `let $HOME = .../XfakeHOME`, and runtest.vim cleans up with `rm -rf` over
# names that the shell word-splits, one of which expands `~`. Running with an
# inherited HOME once destroyed this checkout, ~/.cargo, ~/.rustup and
# ~/.local. So: refuse to start unless HOME is a throwaway directory, and run
# against a copy of testdir so the reference tree stays untouched.
oldtest: _guard_binary
    #!/usr/bin/env bash
    set -euo pipefail
    case "${HOME:?}" in
      /tmp/*|/var/tmp/*) ;;
      *) echo "refusing to run: HOME is ${HOME}, which this suite can delete." >&2
         echo "run as: HOME=\$(mktemp -d) just oldtest" >&2
         exit 1 ;;
    esac
    scratch="$(mktemp -d)"
    cp -a "{{justfile_directory()}}/.references/neovim/test/old/testdir" "${scratch:?}/testdir"
    make -C "${scratch:?}/testdir" NVIM_PRG="{{justfile_directory()}}/target/release/oxvim"

# Diff oxvim --api-info schema against upstream.
apidiff: _guard_binary
    tests/differential/apidiff.sh

# Replay all semantic RPC seed sessions against upstream and oxvim.
replay: _guard_binary
    cargo run --quiet -p differential --bin replay

# Run the release-binary smoke and PTY differential checks.
differential: _guard_binary
    cargo nextest run -p differential