#!/usr/bin/env bash
# Compute the kernel's TCB ratio: lines of `unsafe` in slopos-ostd /
# total kernel Rust LoC. By default informational (always exits 0); the
# gate that *unconditionally* fails on a new unsafe outside OSTD is
# scripts/check_unsafe_outside_ostd.sh.
#
# Pass `--max <pct>` to make the ratio a hard gate — the script exits 1
# if the measured ratio exceeds the threshold. Used in CI to assert the
# Phase 1 OSTD-trust-base ratio obligation (≤ 1.5 %).
#
# Usage:
#   scripts/tcb_ratio.sh                # informational
#   scripts/tcb_ratio.sh --max 1.0      # gate at 1.0 % (Phase 2 bound)
#
# Definitions:
#   - "unsafe tokens": lines under slopos-ostd/src/ containing the
#     `unsafe` keyword (matched via explicit non-word-char boundaries
#     since POSIX awk has no `\b`) after stripping pure comment lines
#     (//, ///, //!, /*). The count is line-based, not token-based; a
#     line with multiple `unsafe` keywords counts once.
#   - "kernel LoC": non-blank, non-comment lines across every kernel
#     crate, where the kernel-crate set is derived from the `kernel`
#     binary's normal-dependency closure (scripts/kernel_crates.sh) rather
#     than a hand-maintained list. Userland-side crates (userland, slibc,
#     slop-protocol, appkit) and the workspace tooling fall out
#     automatically because the kernel image does not link them.
#
# Phase 1 target: ≤ 1.5 %.  Phase 2 target: ≤ 1.0 %.

set -euo pipefail

max_pct=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --max)
            shift
            max_pct="$1"
            ;;
        --max=*)
            max_pct="${1#--max=}"
            ;;
        --help|-h)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "tcb_ratio: unknown argument: $1" >&2
            exit 2
            ;;
    esac
    shift || true
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Kernel crate directories — the TCB-ratio denominator. Derived from
# ground truth (the `kernel` binary's normal-dependency closure) by
# scripts/kernel_crates.sh, never hand-maintained, so a new kernel crate
# is counted automatically and a userland-only crate is never miscounted.
KERNEL_CRATES=()
while IFS= read -r crate; do
    [ -n "$crate" ] && KERNEL_CRATES+=("$crate")
done < <("$SCRIPT_DIR/kernel_crates.sh")

if [ "${#KERNEL_CRATES[@]}" -eq 0 ]; then
    echo "tcb_ratio: kernel_crates.sh produced no crates — cannot measure" >&2
    exit 2
fi

# ---- Unsafe-token count (slopos-ostd only) --------------------------------

unsafe_lines=0
while IFS= read -r -d '' file; do
    n=$(awk '
        /^[[:space:]]*(\/\/|\/\*)/ { next }
        /(^|[^A-Za-z0-9_])unsafe([^A-Za-z0-9_]|$)/ { c++ }
        END { print c+0 }
    ' "$file")
    unsafe_lines=$(( unsafe_lines + n ))
done < <(find "$REPO_ROOT/slopos-ostd/src" -type f -name '*.rs' -print0)

# ---- Kernel LoC count (every kernel crate) --------------------------------

loc=0
for crate in "${KERNEL_CRATES[@]}"; do
    dir="$REPO_ROOT/$crate"
    [ -d "$dir/src" ] || continue
    while IFS= read -r -d '' file; do
        n=$(awk '
            /^[[:space:]]*$/ { next }
            /^[[:space:]]*(\/\/|\/\*)/ { next }
            { c++ }
            END { print c+0 }
        ' "$file")
        loc=$(( loc + n ))
    done < <(find "$dir/src" -type f -name '*.rs' -print0)
done

# ---- Ratio ----------------------------------------------------------------

if [ "$loc" -eq 0 ]; then
    echo "tcb_ratio: no kernel LoC found — check KERNEL_CRATES list" >&2
    exit 2
fi

ratio=$(awk -v u="$unsafe_lines" -v l="$loc" 'BEGIN { printf "%.3f", (u * 100.0) / l }')

printf 'TCB unsafe lines (slopos-ostd): %d\n' "$unsafe_lines"
printf 'Kernel Rust LoC (all crates):   %d\n' "$loc"
printf 'TCB ratio:                      %s %%  (target Phase 1: <= 1.5 %%, Phase 2: <= 1.0 %%)\n' "$ratio"

if [ -n "$max_pct" ]; then
    over=$(awk -v r="$ratio" -v m="$max_pct" 'BEGIN { print (r+0 > m+0) ? 1 : 0 }')
    if [ "$over" = "1" ]; then
        printf 'tcb_ratio: FAIL — ratio %s %% exceeds --max %s %%\n' "$ratio" "$max_pct" >&2
        exit 1
    fi
    printf 'tcb_ratio: OK — ratio %s %% is within --max %s %%\n' "$ratio" "$max_pct"
fi
