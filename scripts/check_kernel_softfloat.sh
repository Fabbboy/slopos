#!/usr/bin/env bash
# Fail the build if the kernel ELF contains any x86 vector (SSE/AVX)
# instruction. The kernel MUST be built `+soft-float` so it never touches
# the XMM/YMM/ZMM register file in its own code.
#
# Why this is load-bearing: a syscall or exception (page fault, IRQ) that
# enters from userland does NOT save the caller's FPU/vector state — that
# only happens on a full context switch (xsave/xrstor in the scheduler).
# If the kernel emits even one vector instruction in such a path, it
# clobbers the interrupted user task's live XMM/YMM and the restarted
# user instruction reads garbage. The classic symptom is a userland AVX
# `vmovups` memset that demand-faults mid-fill and ends up with stale
# zeros (garbage glyphs after a terminal resize).
#
# The soft-float guarantee comes from `targets/x86_64-slos.json`
# (`features: ...,-sse,...,+soft-float` + `rustc-abi: x86-softfloat`).
# It is easy to silently lose: a `RUSTFLAGS` env var fully overrides
# `.cargo/config.toml` `target.*.rustflags`, so codegen flags must live
# in the target JSON. This gate is the belt-and-braces that catches any
# regression where the kernel re-acquires SSE.
#
# Skipped for the `kernel/tests` build: `slopos-ostd/src/test_support/
# cpu_state.rs` carries deliberate named-register XMM/AVX asm used by the
# xsave/fpu conformance tests (it saves+restores around its own use and
# never runs in a fault path).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

ELF="${1:-$REPO_ROOT/builddir/kernel.elf}"

if [ ! -f "$ELF" ]; then
    echo "check_kernel_softfloat: missing $ELF (run \`just build\` first)" >&2
    exit 2
fi

RUST_CHANNEL="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$REPO_ROOT/rust-toolchain.toml")"
HOST="$(rustc +"$RUST_CHANNEL" -vV | sed -n 's/^host: //p')"
SYSROOT="$(rustc +"$RUST_CHANNEL" --print sysroot)"
OBJDUMP="$SYSROOT/lib/rustlib/$HOST/bin/llvm-objdump"
if [ ! -x "$OBJDUMP" ]; then
    OBJDUMP="objdump"
fi

# Disassemble and count any instruction that references an XMM/YMM/ZMM
# register. Soft-float codegen emits none.
COUNT="$("$OBJDUMP" -d --no-show-raw-insn "$ELF" 2>/dev/null \
    | grep -cE '%?(x|y|z)mm[0-9]' || true)"

if [ "$COUNT" -ne 0 ]; then
    echo "check_kernel_softfloat: FAIL — kernel ELF has $COUNT vector (XMM/YMM/ZMM) instructions" >&2
    echo "  The kernel must be +soft-float; check targets/x86_64-slos.json features / rustc-abi." >&2
    "$OBJDUMP" -d --no-show-raw-insn "$ELF" 2>/dev/null \
        | grep -E '%?(x|y|z)mm[0-9]' | head -5 >&2
    exit 1
fi

echo "check_kernel_softfloat: OK — kernel ELF is vector-free (+soft-float)"
