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


# Build the upstream helper programs beside oxvim. testprg() resolves helpers
# relative to NVIM_PRG, not the Neovim reference build directory.
_functional_fixtures:
    #!/usr/bin/env bash
    set -euo pipefail
    src="{{justfile_directory()}}/.references/neovim/test/functional/fixtures"
    out="{{justfile_directory()}}/target/release"
    mkdir -p "${out}"
    for name in printenv-test printargs-test shell-test; do
      if [[ ! -x "${out}/${name}" || "${src}/${name}.c" -nt "${out}/${name}" ]]; then
        cc -std=c11 -D_DEFAULT_SOURCE -O2 "${src}/${name}.c" -o "${out}/${name}"
      fi
    done
    read -r -a uv_flags <<<"$(pkg-config --cflags --libs libuv)"
    for name in streams-test tty-test; do
      if [[ ! -x "${out}/${name}" || "${src}/${name}.c" -nt "${out}/${name}" ]]; then
        cc -std=c11 -D_DEFAULT_SOURCE -O2 "${src}/${name}.c" -o "${out}/${name}" "${uv_flags[@]}"
      fi
    done
# Run upstream Neovim functional tests against oxvim. Focused runs retain the
# single-file interface; full runs isolate top-level groups so one slow group
# cannot consume the whole suite's timeout or delete another group's XDG tree.
functional: _guard_binary _functional_fixtures
    #!/usr/bin/env bash
    set -euo pipefail
    root="{{justfile_directory()}}"
    run_group() {
      local group="$1"
      # TEST_PARALLEL_GROUP restricts the run to the group's spec dir and gives
      # it a private Xtest_xdg_<group>/Xtest_tmpdir_<group> tree; without it
      # every worker runs the whole suite on the shared tree and the first
      # finisher's REMOVE_RECURSE deletes it under the rest. TEST_FILE
      # (example_spec) forbids combining the two, so it is only passed for
      # directory groups. Per-test output goes to stdout, so capture it.
      local isolation=()
      if [[ -z "${TEST_FILE:-}" ]]; then
        isolation=(-D "TEST_PARALLEL_GROUP=${group}")
      fi
      cmake -D TEST_TYPE=functional \
        -D BUILD_DIR="${root}/.references/neovim/build" \
        -D CI_BUILD=OFF \
        -D NVIM_PRG="${root}/target/release/oxvim" \
        -D TEST_DIR="${root}/.references/neovim/test" \
        -D ROOT_DIR="${root}/.references/neovim" \
        "${isolation[@]}" \
        -P "${root}/.references/neovim/cmake/RunTests.cmake" \
        >"${root}/.outline/evidence/functional-${group}.log" 2>&1
    }
    cd "${root}/.references/neovim/build/test"
    if [[ -n "${TEST_FILE:-}${TEST_FILTER:-}${TEST_TAG:-}${TEST_FILTER_OUT:-}" ]]; then
      cmake -D TEST_TYPE=functional \
        -D BUILD_DIR="${root}/.references/neovim/build" \
        -D CI_BUILD=OFF \
        -D NVIM_PRG="${root}/target/release/oxvim" \
        -D TEST_DIR="${root}/.references/neovim/test" \
        -D ROOT_DIR="${root}/.references/neovim" \
        -P "${root}/.references/neovim/cmake/RunTests.cmake"
      exit
    fi
    export -f run_group
    export root
    # "example" is the loose example_spec.lua, not a directory: run it by
    # file after the directory groups.
    printf '%s\n' api autocmd core editor ex_cmds legacy lua options plugin provider script shada terminal testnvim treesitter ui vimscript |
      xargs -P 4 -n 1 bash -c 'run_group "$1"' _
    TEST_FILE=test/functional/example_spec.lua bash -c 'run_group example_spec'

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
    out="{{justfile_directory()}}/.outline/evidence/oldtest"
    scratch="$(mktemp -d)"
    mkdir -p "${scratch:?}/test/old" "${scratch:?}/home" "${out:?}"
    cp -a "${ref:?}/src" "${scratch:?}/src"
    ln -s "${ref:?}/runtime" "${scratch:?}/runtime"
    cp -a "${ref:?}/test/old/testdir" "${scratch:?}/test/old/testdir"
    rm -f -- "${scratch:?}/test/old/testdir/messages" \
             "${scratch:?}/test/old/testdir/test.log" \
             "${scratch:?}/test/old/testdir/test.res"
    # runnvim.sh:83 guards its `cp -a test.log messages` clobber with a typo
    # (`test -f message`, no s): without this sentinel any test whose runner
    # exits nonzero replaces the accumulated census with the errors-only log.
    touch "${scratch:?}/test/old/testdir/message"
    set +e
    HOME="${scratch:?}/home" make -k -C "${scratch:?}/test/old/testdir" \
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
    executed=$(grep -aoE '^Executed [0-9]+ tests?' "${out:?}/messages" | grep -oE '[0-9]+' | awk '{s+=$1} END{print s+0}' || true)
    failed=$(grep -aoE '^[0-9]+ FAILED:' "${out:?}/messages" | grep -oE '^[0-9]+' | awk '{s+=$1} END{print s+0}' || true)
    skipped=$(grep -ac '^SKIPPED' "${out:?}/messages" || true)
    echo "oldtest: executed=${executed:-0} failed=${failed:-0} skipped=${skipped:-0}"
    echo "results: ${out:?}/messages"
    if [[ "${executed:-0}" -eq 0 ]]; then
      printf '%s\n' "oldtest executed no tests: the harness result is invalid." >&2
      exit 1
    fi
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