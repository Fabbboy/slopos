#!/usr/bin/env bash
set -euo pipefail

# Count-regression CI guard.
#
# Boots the test ISO via `builddir/run_tests --raw`, sums the
# `KTAP\t1..N` plan lines emitted by every phase (kernel + userland), and
# fails if the total drops below the baseline.
#
# Tests sometimes get accidentally deleted by mass refactors; this guard
# turns that into a build failure rather than a silent regression.
#
# Override the baseline via the TEST_COUNT_BASELINE env var. Bump it in
# the same commit that intentionally adds tests; do NOT lower it without
# explaining why in the commit message.
#
# Usage:
#     scripts/check_test_count.sh
#     scripts/check_test_count.sh --log captured-raw.log
#     TEST_COUNT_BASELINE=2500 scripts/check_test_count.sh
#
# `--log` parses a capture instead of booting, so CI can take one raw run and
# feed it to every boot-based ratchet rather than paying for a QEMU boot each.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BASELINE="${TEST_COUNT_BASELINE:-2885}"

LOG=""
while [ $# -gt 0 ]; do
    case "$1" in
        --log) LOG="$2"; shift 2 ;;
        -h|--help) sed -n '4,24p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

cd "$REPO_ROOT"

if [ -n "$LOG" ]; then
    TMP="$LOG"
else
    TMP="$(mktemp)"
    trap 'rm -f "$TMP"' EXIT INT TERM
    # --raw passes through QEMU stdout; --no-color keeps the stream parseable
    # (no ANSI escapes from the wrapper itself).
    builddir/run_tests --raw --no-color > "$TMP" 2>&1 || true
fi

# Sum the plan numbers across all phases.
total=0
plans=0
while read -r n; do
    total=$(( total + n ))
    plans=$(( plans + 1 ))
done < <(grep -E $'^KTAP\t1\\.\\.[0-9]+$' "$TMP" | sed -E 's/.*1\.\.([0-9]+)/\1/')

# A run that emitted no plan line at all did not measure anything. Without
# this, "sum of nothing" is 0, which passes against any baseline <= 0 — so the
# `TEST_COUNT_BASELINE=0` idiom used to read the current count would report 0
# for a kernel that failed to boot, and the number would then be written into
# the baseline as if it were real.
if [ "$plans" -eq 0 ]; then
    echo "FAIL: no KTAP plan line was emitted — nothing was measured." >&2
    echo "      The kernel did not reach the harness (boot panic, missing ISO," >&2
    echo "      or builddir/run_tests not built). Run 'just check-test-count'," >&2
    echo "      which builds the wrapper first, and read the output above." >&2
    exit 1
fi

if [ "$total" -lt "$BASELINE" ]; then
    echo "FAIL: observed $total tests, baseline is $BASELINE." >&2
    echo "      Did a refactor accidentally delete tests?" >&2
    echo "      If the drop is intentional, lower TEST_COUNT_BASELINE in CI." >&2
    exit 1
fi

echo "OK: $total tests planned across all phases (>= baseline $BASELINE)."
