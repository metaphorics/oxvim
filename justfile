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
# names the shell word-splits, one of which expands `~`. Running with an
# inherited HOME once destroyed this checkout, ~/.cargo, ~/.rustup and
# ~/.local, so this recipe allocates its own HOME and never trusts the
# caller's. It also runs against a copied testdir, because the suite writes
# into its own directory and .references is read-only. The copy needs the
# sibling src/ and runtime/ the Makefile reaches for, so they are symlinked.
oldtest *targets: _guard_binary
    #!/usr/bin/env bash
    set -euo pipefail
    ref="{{justfile_directory()}}/.references/neovim"
    out="{{justfile_directory()}}/target/oldtest"
    scratch="$(mktemp -d)"
    mkdir -p "${scratch:?}/test/old" "${scratch:?}/home" "${out:?}"
    cp -a "${ref:?}/src" "${scratch:?}/src"
    ln -s "${ref:?}/runtime" "${scratch:?}/runtime"
    cp -a "${ref:?}/test/old/testdir" "${scratch:?}/test/old/testdir"
    rm -f -- "${scratch:?}/test/old/testdir/messages" \
             "${scratch:?}/test/old/testdir/test.log" \
             "${scratch:?}/test/old/testdir/test.res"
    # make's exit status does not track per-test failures here: runtest.vim
    # writes .res as a pass marker and the results land in `messages`. So keep
    # going, then decide from the messages file itself.
    set +e
    HOME="${scratch:?}/home" make -C "${scratch:?}/test/old/testdir" \
        NVIM_PRG="{{justfile_directory()}}/target/release/oxvim" {{targets}}
    set -e
    msg="${scratch:?}/test/old/testdir/messages"
    if [[ ! -s "${msg}" ]]; then
      echo "oldtest produced no messages file: the harness never reported." >&2
      cp -a "${scratch:?}/test/old/testdir" "${out:?}/failed-run" 2>/dev/null || true
      rm -rf -- "${scratch:?}"
      exit 1
    fi
    cp -f "${msg}" "${out:?}/messages"
    cp -f "${scratch:?}/test/old/testdir/test.log" "${out:?}/test.log" 2>/dev/null || true
    rm -rf -- "${scratch:?}"
    # grep exits 1 when it finds nothing, and a clean run has no FAILED line,
    # so each capture must tolerate no match or `set -e` aborts the summary.
    executed=$(grep -aoE '^Executed [0-9]+ tests?' "${out:?}/messages" | grep -oE '[0-9]+' | tail -1 || true)
    failed=$(grep -aoE '^[0-9]+ FAILED:' "${out:?}/messages" | grep -oE '[0-9]+' | tail -1 || true)
    skipped=$(grep -ac '^SKIPPED' "${out:?}/messages" || true)
    echo "oldtest: executed=${executed:-0} failed=${failed:-0} skipped=${skipped:-0}"
    echo "results: ${out:?}/messages"
    [[ "${failed:-0}" -eq 0 ]] || exit 1

# Diff oxvim --api-info schema against upstream.
apidiff: _guard_binary
    tests/differential/apidiff.sh

# Replay all semantic RPC seed sessions against upstream and oxvim.
replay: _guard_binary
    cargo run --quiet -p differential --bin replay

# Run the release-binary smoke and PTY differential checks.
differential: _guard_binary
    cargo nextest run -p differential