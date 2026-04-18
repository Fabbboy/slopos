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

# Ensure toolchain is available
"$SCRIPT_DIR/ensure_toolchain.sh"

mkdir -p "$BUILD_DIR"
rm -f "$BUILD_DIR/kernel" "$BUILD_DIR/kernel.elf"

KERNEL_RELEASE="${KERNEL_RELEASE:-0}"

FEATURE_ARGS=()
if [ -n "$FEATURES" ]; then
    FEATURE_ARGS=(--features "$FEATURES")
fi

PROFILE_ARGS=()
if [ "$KERNEL_RELEASE" = "1" ]; then
    PROFILE_ARGS=(--release)
fi

CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
RUSTFLAGS="${RUSTFLAGS:-} $KERNEL_RUSTFLAGS -Zunstable-options -Zemit-stack-sizes" \
$CARGO +"$RUST_CHANNEL" build \
    -Zbuild-std=core,alloc \
    -Zbuild-std-features=compiler-builtins-mem \
    -Zunstable-options \
    --target "$RUST_TARGET" \
    --package kernel \
    --bin kernel \
    "${FEATURE_ARGS[@]}" \
    "${PROFILE_ARGS[@]}" \
    --artifact-dir "$BUILD_DIR"

if [ -f "$BUILD_DIR/kernel" ]; then
    if [ ! -e "$BUILD_DIR/kernel.elf" ] || [ ! "$BUILD_DIR/kernel" -ef "$BUILD_DIR/kernel.elf" ]; then
        mv "$BUILD_DIR/kernel" "$BUILD_DIR/kernel.elf"
    fi
fi

# Kernel allocation + stack-frame invariant gates.
"$SCRIPT_DIR/check_alloc_dep.sh"

# The stack-sizes gate applies to the production kernel only. Test builds
# (`kernel/builtin-tests` feature) compile in per-subsystem regression
# tests whose large stack frames are irrelevant to the real kernel image.
if [[ "$FEATURES" != *"builtin-tests"* ]]; then
    "$SCRIPT_DIR/check_stack_sizes.sh" "$BUILD_DIR/kernel.elf"
else
    echo "check_stack_sizes: skipped (builtin-tests feature enabled)"
fi
