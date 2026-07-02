#!/usr/bin/env bash
set -euo pipefail

# Build the SlopOS kernel ELF binary.
#
# Usage: build_kernel.sh <build_dir> <cargo_target_dir> [features]
#
# Environment:
#   CARGO             - cargo binary (default: cargo)
#   RUST_CHANNEL      - toolchain channel (parsed from rust-toolchain.toml if unset)
#   RUST_TARGET       - custom target JSON (default: targets/x86_64-slos.json)
#   KERNEL_RUSTFLAGS  - extra RUSTFLAGS for the kernel build
#   KERNEL_RELEASE    - set to 1 for optimized (release) kernel build

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BUILD_DIR="${1:?Usage: build_kernel.sh <build_dir> <cargo_target_dir> [features]}"
CARGO_TARGET_DIR="${2:?Usage: build_kernel.sh <build_dir> <cargo_target_dir> [features]}"
FEATURES="${3:-}"

CARGO="${CARGO:-cargo}"
RUST_CHANNEL="${RUST_CHANNEL:-$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "${REPO_ROOT}/rust-toolchain.toml")}"
RUST_TARGET="${RUST_TARGET:-${REPO_ROOT}/targets/x86_64-slos.json}"
KERNEL_RUSTFLAGS="${KERNEL_RUSTFLAGS:--C force-frame-pointers=yes}"

# SafeStack dual-stack sanitizer — on by default.  The kernel's
# `karch::safestack_rt` supplies `__safestack_pointer_address`
# (via `-C llvm-args=-safestack-use-pointer-address`), its
# `_start` trampoline primes BSP_PCR + GS_BASE before any
# instrumented Rust runs, and `scripts/safestack_stub.sh`
# materialises an empty `librustc-nightly_rt.safestack.a` stub
# that rustc auto-links.  Set `KERNEL_SAFESTACK=0` to disable.
KERNEL_SAFESTACK="${KERNEL_SAFESTACK:-1}"
if [ "$KERNEL_SAFESTACK" = "1" ]; then
    KERNEL_RUSTFLAGS="$KERNEL_RUSTFLAGS -Z sanitizer=safestack -C llvm-args=-safestack-use-pointer-address"
fi

# Ensure toolchain is available
"$SCRIPT_DIR/ensure_toolchain.sh"

# Ensure the safestack runtime stub archive exists at rustc's expected path.
if [ "$KERNEL_SAFESTACK" = "1" ]; then
    RUST_CHANNEL="$RUST_CHANNEL" RUST_TARGET="$RUST_TARGET" "$SCRIPT_DIR/safestack_stub.sh"
fi

mkdir -p "$BUILD_DIR"
rm -f "$BUILD_DIR/kernel" "$BUILD_DIR/kernel.elf"

KERNEL_RELEASE="${KERNEL_RELEASE:-0}"

# Persistent kernel symbol table embedded for symbolized panic backtraces.
# Both build phases point `slopos-ostd`'s build script at this same file, and
# the second phase only recompiles when its content changes (build.rs tracks
# it via `rerun-if-changed`) — so a rebuild with unchanged symbols is a cache
# hit instead of a forced two-phase recompile of the whole kernel. The file is
# keyed by build variant so dev/release/tests kernels (with different symbol
# sets) do not invalidate each other's table.
KSYMS_TAG="dev"
[ "$KERNEL_RELEASE" = "1" ] && KSYMS_TAG="release"
[[ "$FEATURES" == *"kernel/tests"* ]] && KSYMS_TAG="${KSYMS_TAG}-tests"
KSYMS_RS="$(cd "$BUILD_DIR" && pwd)/kallsyms-${KSYMS_TAG}.rs"
if [ ! -f "$KSYMS_RS" ]; then
    printf 'pub static KERNEL_SYMBOLS: &[crate::ksym::KernelSymbol] = &[];\n' > "$KSYMS_RS"
fi

CARGO_ARGS=(
    +"$RUST_CHANNEL" build
    -Zbuild-std=core,alloc
    -Zbuild-std-features=compiler-builtins-mem
    -Zunstable-options
    --target "$RUST_TARGET"
    --package kernel
    --bin kernel
)
if [ -n "$FEATURES" ]; then
    CARGO_ARGS+=(--features "$FEATURES")
fi
if [ "$KERNEL_RELEASE" = "1" ]; then
    CARGO_ARGS+=(--release)
fi
CARGO_ARGS+=(--artifact-dir "$BUILD_DIR")

build_kernel_once() {
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
    SLOPOS_KSYMS_RS="$KSYMS_RS" \
    RUSTFLAGS="${RUSTFLAGS:-} $KERNEL_RUSTFLAGS -Zunstable-options -Zemit-stack-sizes" \
    "$CARGO" "${CARGO_ARGS[@]}"

    if [ -f "$BUILD_DIR/kernel" ]; then
        if [ ! -e "$BUILD_DIR/kernel.elf" ] || [ ! "$BUILD_DIR/kernel" -ef "$BUILD_DIR/kernel.elf" ]; then
            mv "$BUILD_DIR/kernel" "$BUILD_DIR/kernel.elf"
        fi
    fi
}

# Phase 1: build against the previous (or empty) symbol table.
build_kernel_once

HOST_TRIPLE="$(rustc +"$RUST_CHANNEL" -vV | sed -n 's/^host: //p')"
SYSROOT="$(rustc +"$RUST_CHANNEL" --print sysroot)"
LLVM_NM="$SYSROOT/lib/rustlib/$HOST_TRIPLE/bin/llvm-nm"
if [ ! -x "$LLVM_NM" ]; then
    LLVM_NM="$(command -v llvm-nm || true)"
fi
if [ -z "$LLVM_NM" ]; then
    echo "gen_kernel_symbols: llvm-nm not found; building without embedded symbol names" >&2
else
    # Refresh the symbol table from the phase-1 ELF (rewrites only on change),
    # then rebuild. Phase 2 is a cache hit unless the symbols actually moved.
    python3 "$SCRIPT_DIR/gen_kernel_symbols.py" "$LLVM_NM" "$BUILD_DIR/kernel.elf" "$KSYMS_RS"
    build_kernel_once
fi

# Source-discipline gates (vendor pin, unsafe-outside-ostd, no-async, alloc
# dep, Drop-panic-free, TCB ratio) are NOT run here: they scan the whole tree
# (~14 s) and would tax every interactive boot. They run in `just
# check-framekernel` and CI, which is the canonical enforcement point. Set
# KERNEL_BUILD_GATES=1 to also run them from a build.
if [ "${KERNEL_BUILD_GATES:-0}" = "1" ]; then
    "$SCRIPT_DIR/check_vendor_pin.sh"
    "$SCRIPT_DIR/check_unsafe_outside_ostd.sh"
    "$SCRIPT_DIR/check_no_kernel_async.sh"
    "$SCRIPT_DIR/check_alloc_dep.sh"
    "$SCRIPT_DIR/check_drop_panic_free.sh"
    "$SCRIPT_DIR/tcb_ratio.sh" --max 1.0
fi

# The stack-sizes and soft-float gates inspect the produced ELF, so they stay
# on the build path. They apply to the production kernel only.
# Test builds (`kernel/tests` feature) compile in per-subsystem regression
# tests whose large stack frames are irrelevant to the real kernel image,
# and `test_support/cpu_state.rs` carries deliberate XMM/AVX asm for the
# xsave conformance tests.
if [[ "$FEATURES" == *"kernel/tests"* ]]; then
    echo "check_stack_sizes: skipped (kernel/tests feature enabled)"
    echo "check_kernel_softfloat: skipped (kernel/tests feature enabled)"
else
    # These gates depend only on the ELF bytes; skip when the binary is
    # unchanged since they last passed (soft-float disassembles the whole
    # image, so re-running it on an identical rebuild is pure latency).
    GATE_STAMP="$BUILD_DIR/.kernel-elf-gates.stamp"
    ELF_HASH="$(sha256sum "$BUILD_DIR/kernel.elf" 2>/dev/null | awk '{print $1}')"
    if [ -n "$ELF_HASH" ] && [ -f "$GATE_STAMP" ] && [ "$(cat "$GATE_STAMP" 2>/dev/null)" = "$ELF_HASH" ]; then
        echo "check_stack_sizes: skipped (kernel.elf unchanged since last pass)"
        echo "check_kernel_softfloat: skipped (kernel.elf unchanged since last pass)"
    else
        "$SCRIPT_DIR/check_stack_sizes.sh" "$BUILD_DIR/kernel.elf"
        "$SCRIPT_DIR/check_kernel_softfloat.sh" "$BUILD_DIR/kernel.elf"
        [ -n "$ELF_HASH" ] && printf '%s\n' "$ELF_HASH" > "$GATE_STAMP"
    fi
fi
