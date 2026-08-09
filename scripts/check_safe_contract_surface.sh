#!/usr/bin/env bash
# Ratchet on OSTD's contract-bearing safe API surface.
#
# A framekernel's load-bearing property is not "unsafe lives in one crate" —
# it is that the crate's *safe* API is sound, meaning no possible safe caller
# can cause UB. Folding `unsafe` behind a safe wrapper is the design working
# as intended; several hundred OSTD functions do exactly that and are fine.
#
# A `# Safety` section on a function that is *not* `unsafe fn` is the
# opposite: a written admission that the caller must uphold something the
# compiler does not check. When one of those is wrong, the fault lands in
# OSTD's code but the cause is an ordinary safe call in a service crate —
# the debugging cost of a TCB bug without the containment the TCB buys.
#
# So this counts the contract-bearing subset, not the unsafe-containing one,
# and fails if it grows. Shrinking it is free; each removal should lower
# BASELINE in the same commit.
#
# The baseline is zero, and that is a measurement rather than a target: every
# contract this gate once counted turned out to be expressible. What retired
# them was a small set of shapes, and a new one should reach for the same set
# before asking for an exemption — a capability witness (`&IrqDisabled`,
# `&BspToken`, `Osxsave`), a validated newtype (`Xcr0Mask`), a linear handle
# (`OneShotBuf`), an owning reference (`KArc`), a sealed trait
# (`ApTrampolineAbi`), a runtime-checked borrow (`PerCpuSlot`), or simply
# taking a slice instead of a pointer and a length.
#
# Raising it is still allowed, because a contract that genuinely cannot be
# typed on the pinned toolchain is better documented than pushed into an
# undocumented function. But it is now an argument to make in the commit
# message, not a budget to spend.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/gate_common.sh
. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_safe_contract_surface "$@"

# Measured with this script.
BASELINE="${SAFE_CONTRACT_BASELINE:-0}"

count_file() {
    awk '
        # Track the doc block immediately preceding an item.
        /^[[:space:]]*\/\/\/[[:space:]]*#[[:space:]]*Safety/ { doc_safety = 1; next }
        /^[[:space:]]*\/\/\// { next }
        /^[[:space:]]*#\[/ { next }
        /^[[:space:]]*$/ { doc_safety = 0; next }
        {
            if (doc_safety) {
                # `unsafe fn` / `unsafe trait` carry the marker in the type
                # system; a `# Safety` section on them is required, not a
                # finding. Everything else with one is the surface we track.
                if ($0 ~ /(^|[^A-Za-z0-9_])unsafe([^A-Za-z0-9_])/) {
                    doc_safety = 0
                    next
                }
                if ($0 ~ /(^|[[:space:]])(pub|pub\(crate\))[[:space:]]/ && $0 ~ /fn[[:space:]]/) {
                    printf "%s:%d: %s\n", FILENAME, NR, $0
                }
            }
            doc_safety = 0
        }
    ' "$1"
}

# One tagged line per finding, so the self-test can count this check alone.
scan_tree() {
    local root="$1" file list
    list="$(find "$root" -name '*.rs' | LC_ALL=C sort)"
    gate_require_nonempty check_safe_contract_surface "$root" "$list"
    while IFS= read -r file; do
        [ -f "$file" ] || continue
        count_file "$file"
    done <<< "$list"
}

if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_safe_contract_surface
    root="$GATE_FIXTURE_ROOT/src"
    mkdir -p "$root"

    # Must fire: the whole point of the gate.
    cat > "$(gate_fixture src/positives.rs)" <<'RS'
/// Does a thing.
///
/// # Safety
///
/// The caller must not do the other thing.
pub fn safe_fn_with_contract(x: usize) -> usize { x }

/// # Safety
///
/// Caller obligation.
pub(crate) fn crate_visible_with_contract() {}
RS

    # Must not fire.
    cat > "$(gate_fixture src/negatives.rs)" <<'RS'
/// # Safety
///
/// Documented obligation, taken on in the type system.
pub unsafe fn properly_unsafe() {}

/// # Correctness
///
/// A deliberate blind spot: this gate tracks `# Safety` only.
pub fn correctness_section() {}

/// # Safety
///
/// Blank line below resets the tracker, so this fn is not attributed.

pub fn separated_from_the_block() {}

/// Ordinary documentation with no section at all.
pub fn plain_fn() {}

/// # Safety
///
/// An obligation on an unsafe trait implementation.
pub unsafe trait UnsafeTrait {}
RS

    GATE_FINDINGS="$(scan_tree "$root" | sed 's/^/1	/')"
    gate_expect 1 2 "a safe pub fn and a pub(crate) fn, each carrying a '# Safety' section"
    gate_expect_silent 'negatives\.rs' \
        "an unsafe fn, a '# Correctness' section, a blank-line-separated block, and an undocumented fn"
    gate_selftest_end
fi

findings="$(scan_tree "$REPO_ROOT/slopos-ostd/src")"
findings="$(grep -c . <<< "$findings" || true)"

if [ "$findings" -gt "$BASELINE" ]; then
    echo "check_safe_contract_surface: $findings safe fns carry a '# Safety' section (baseline $BASELINE)" >&2
    echo "  A '# Safety' section on a fn that is not 'unsafe fn' says the caller must" >&2
    echo "  uphold something the compiler will not check. Express it — a guard, a" >&2
    echo "  capability token, a closure-scoped borrow, a witness type — or mark the" >&2
    echo "  function 'unsafe fn' so the obligation is visible where it is taken on." >&2
    echo "  Re-run with SAFE_CONTRACT_BASELINE=0 to list them." >&2
    exit 1
fi

if [ "$findings" -lt "$BASELINE" ]; then
    echo "check_safe_contract_surface: OK — $findings (below the $BASELINE baseline;"
    echo "check_safe_contract_surface: lower BASELINE in this script to lock the win in)"
    exit 0
fi

echo "check_safe_contract_surface: OK — $findings safe fns carry a '# Safety' section"
