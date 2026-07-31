#!/usr/bin/env bash
# Print the absolute path to an `llvm-tools-preview` binary, or exit 2.
#
#     OBJDUMP="$(scripts/llvm_tool.sh llvm-objdump)"
#
# Pinned sysroot first, then PATH. No fallback to a bare tool name: a host
# `objdump` is a different program with different output, and a gate that
# accepts it reports OK on a disassembly it never produced.

set -euo pipefail

TOOL="${1:?usage: llvm_tool.sh <llvm-objdump|llvm-readobj|llvm-nm|llc|...>}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

RUST_CHANNEL="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$REPO_ROOT/rust-toolchain.toml")"
if [ -z "$RUST_CHANNEL" ]; then
    echo "llvm_tool: could not read the channel from $REPO_ROOT/rust-toolchain.toml" >&2
    exit 2
fi

SYSROOT="$(rustc +"$RUST_CHANNEL" --print sysroot)"
HOST="$(rustc +"$RUST_CHANNEL" -vV | sed -n 's/^host: //p')"

CANDIDATE="$SYSROOT/lib/rustlib/$HOST/bin/$TOOL"
if [ -x "$CANDIDATE" ]; then
    printf '%s\n' "$CANDIDATE"
    exit 0
fi

CANDIDATE="$(command -v "$TOOL" 2>/dev/null || true)"
if [ -n "$CANDIDATE" ]; then
    printf '%s\n' "$CANDIDATE"
    exit 0
fi

echo "llvm_tool: $TOOL not found in the $RUST_CHANNEL sysroot or on PATH" >&2
echo "  Install it with:" >&2
echo "      rustup component add llvm-tools-preview --toolchain $RUST_CHANNEL" >&2
exit 2
