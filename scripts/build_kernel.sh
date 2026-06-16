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

CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
RUSTFLAGS="${RUSTFLAGS:-} $KERNEL_RUSTFLAGS -Zunstable-options -Zemit-stack-sizes" \
"$CARGO" "${CARGO_ARGS[@]}"

if [ -f "$BUILD_DIR/kernel" ]; then
    if [ ! -e "$BUILD_DIR/kernel.elf" ] || [ ! "$BUILD_DIR/kernel" -ef "$BUILD_DIR/kernel.elf" ]; then
        mv "$BUILD_DIR/kernel" "$BUILD_DIR/kernel.elf"
    fi
fi

# Kernel allocation + stack-frame invariant gates.
"$SCRIPT_DIR/check_alloc_dep.sh"

# The stack-sizes and soft-float gates apply to the production kernel only.
# Test builds (`kernel/tests` feature) compile in per-subsystem regression
# tests whose large stack frames are irrelevant to the real kernel image,
# and `test_support/cpu_state.rs` carries deliberate XMM/AVX asm for the
# xsave conformance tests.
if [[ "$FEATURES" != *"kernel/tests"* ]]; then
    "$SCRIPT_DIR/check_stack_sizes.sh" "$BUILD_DIR/kernel.elf"
    "$SCRIPT_DIR/check_kernel_softfloat.sh" "$BUILD_DIR/kernel.elf"
else
    echo "check_stack_sizes: skipped (kernel/tests feature enabled)"
    echo "check_kernel_softfloat: skipped (kernel/tests feature enabled)"
fi
