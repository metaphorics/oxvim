#!/bin/sh
# Oxvim greenfield acceptance: runs each criterion's test command in order and
# reports pass/fail per criterion. Exits nonzero if any criterion fails.
#
# Intended to fail NOW: no oxvim binary exists yet (built by later tasks).
set -u

cd "$(dirname "$0")/../../.." || exit 1

run() {
    criterion="$1"
    shift
    echo "== $criterion =="
    if "$@"; then
        echo "PASS: $criterion"
    else
        echo "FAIL: $criterion"
        failed=1
    fi
}

failed=0

run "1. workspace builds clean with strict lints" cargo build --workspace --release
run "2. unit/property tests pass" cargo nextest run --workspace
run "3. api-info schema matches upstream" just apidiff
run "4. functional suite passes" just functional
run "5. oldtests pass" just oldtest

if [ "$failed" -ne 0 ]; then
    echo "ACCEPTANCE FAILED"
    exit 1
fi
echo "ACCEPTANCE PASSED"
exit 0