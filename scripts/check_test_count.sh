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
#     TEST_COUNT_BASELINE=2500 scripts/check_test_count.sh

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BASELINE="${TEST_COUNT_BASELINE:-2545}"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT INT TERM

cd "$REPO_ROOT"

# --raw passes through QEMU stdout; --no-color keeps the stream parseable
# (no ANSI escapes from the wrapper itself).
builddir/run_tests --raw --no-color > "$TMP" 2>&1 || true

# Sum the plan numbers across all phases.
total=0
while read -r n; do
    total=$(( total + n ))
done < <(grep -E $'^KTAP\t1\\.\\.[0-9]+$' "$TMP" | sed -E 's/.*1\.\.([0-9]+)/\1/')

if [ "$total" -lt "$BASELINE" ]; then
    echo "FAIL: observed $total tests, baseline is $BASELINE." >&2
    echo "      Did a refactor accidentally delete tests?" >&2
    echo "      If the drop is intentional, lower TEST_COUNT_BASELINE in CI." >&2
    exit 1
fi

echo "OK: $total tests planned across all phases (>= baseline $BASELINE)."
