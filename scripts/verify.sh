#!/usr/bin/env bash
set -euo pipefail

# Run the pinned Verus toolchain over every proof in verification/proofs/.
# A verify failure behaves like any other framekernel-discipline gate:
# non-zero exit, offending file named.
#
# Each *.rs directly under verification/proofs/ is a self-contained Verus
# crate-of-one (compiled with `verus <file>`), mirroring a slice of OSTD
# under `verus! { ... }`. Files under verification/proofs/ whose name
# starts with `_` are treated as shared helper modules and skipped as
# top-level entry points (they are `include!`d by the proofs that need
# them).
#
# Usage:
#   scripts/verify.sh                 # verify every proof
#   scripts/verify.sh frame_refcount  # verify a single proof by stem
#
# Environment:
#   VERUS_BIN   path to the verus launcher (default: resolved via
#               scripts/ensure_verus.sh, which downloads + pins it)
#   VERUS_EXTRA extra args appended to every verus invocation

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROOFS_DIR="${PROOFS_DIR:-${REPO_ROOT}/verification/proofs}"

filter="${1:-}"

if [ ! -d "$PROOFS_DIR" ]; then
    echo "verify: proofs directory not found: $PROOFS_DIR" >&2
    exit 1
fi

# Resolve the Verus launcher, fetching + pinning on demand.
VERUS_BIN="${VERUS_BIN:-}"
if [ -z "$VERUS_BIN" ]; then
    VERUS_BIN="$("$SCRIPT_DIR/ensure_verus.sh" | tail -n1)"
fi
if [ ! -x "$VERUS_BIN" ]; then
    echo "verify: verus launcher not executable: $VERUS_BIN" >&2
    exit 1
fi

shopt -s nullglob
proofs=()
for f in "$PROOFS_DIR"/*.rs; do
    base="$(basename "$f")"
    # Skip shared helper modules (leading underscore) — they are include!d.
    case "$base" in
        _*) continue ;;
    esac
    stem="${base%.rs}"
    if [ -n "$filter" ] && [ "$stem" != "$filter" ]; then
        continue
    fi
    proofs+=("$f")
done

if [ "${#proofs[@]}" -eq 0 ]; then
    if [ -n "$filter" ]; then
        echo "verify: no proof matching '$filter' under $PROOFS_DIR" >&2
        exit 1
    fi
    # No proofs authored yet — this is a green no-op so the gate can be
    # wired into CI immediately, before the first proof lands.
    echo "verify: no proofs under verification/proofs/ yet — nothing to check (OK)"
    exit 0
fi

# Emit any rustc artifacts into a scratch dir so the repo root stays
# clean — `--crate-type=lib` otherwise drops `lib<stem>.rlib` in cwd.
OUT_DIR="$(mktemp -d)"
trap 'rm -rf "$OUT_DIR"' EXIT

failures=0
verified_total=0
for f in "${proofs[@]}"; do
    stem="$(basename "${f%.rs}")"
    echo "verify: ${stem} ..."
    # `--crate-type=lib` so a proof file need not carry `fn main`. Verus
    # prints a `verification results:: N verified, M errors` summary line.
    if out="$("$VERUS_BIN" --crate-type=lib --out-dir "$OUT_DIR" ${VERUS_EXTRA:-} "$f" 2>&1)"; then
        echo "$out" | sed 's/^/    /'
        n="$(printf '%s\n' "$out" | sed -n -E 's/.*: ([0-9]+) verified, .*/\1/p' | tail -n1)"
        verified_total=$(( verified_total + ${n:-0} ))
    else
        echo "$out" | sed 's/^/    /' >&2
        echo "verify: FAILED on ${stem}" >&2
        failures=$(( failures + 1 ))
    fi
done

if [ "$failures" -ne 0 ]; then
    echo "verify: ${failures} proof file(s) failed Verus verification." >&2
    exit 1
fi

echo "verify: OK — ${#proofs[@]} proof file(s), ${verified_total} obligation(s) verified on pinned Verus."
