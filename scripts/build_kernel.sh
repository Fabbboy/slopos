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

FEATURE_ARGS=()
if [ -n "$FEATURES" ]; then
    FEATURE_ARGS=(--features "$FEATURES")
fi

CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
RUSTFLAGS="${RUSTFLAGS:-} $KERNEL_RUSTFLAGS" \
$CARGO +"$RUST_CHANNEL" build \
    -Zbuild-std=core,alloc \
    -Zbuild-std-features=compiler-builtins-mem \
    -Zunstable-options \
    --target "$RUST_TARGET" \
    --package kernel \
    --bin kernel \
    "${FEATURE_ARGS[@]}" \
    --artifact-dir "$BUILD_DIR"

if [ -f "$BUILD_DIR/kernel" ]; then
    if [ ! -e "$BUILD_DIR/kernel.elf" ] || [ ! "$BUILD_DIR/kernel" -ef "$BUILD_DIR/kernel.elf" ]; then
        mv "$BUILD_DIR/kernel" "$BUILD_DIR/kernel.elf"
    fi
fi
