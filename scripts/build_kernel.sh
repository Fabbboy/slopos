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

KERNEL_RELEASE="${KERNEL_RELEASE:-0}"

# The build variant, resolved once: symbol table, ELF path, gate stamp and
# the `--variant` the ELF gates are held to all key off it.
if [[ "$FEATURES" == *"kernel/tests"* ]]; then
    [ "$KERNEL_RELEASE" = "1" ] && VARIANT="release-tests" || VARIANT="tests"
else
    [ "$KERNEL_RELEASE" = "1" ] && VARIANT="release" || VARIANT="dev"
fi

# One ELF per variant: a shared path means whichever build ran last silently
# answers for all three, to the gates, to gdb and to the ISO builder.
KERNEL_ELF="$BUILD_DIR/kernel-${VARIANT}.elf"
rm -f "$BUILD_DIR/kernel" "$KERNEL_ELF"

# Persistent kernel symbol table embedded for symbolized panic backtraces.
# Both build phases point `slopos-ostd`'s build script at this same file, and
# the second phase only recompiles when its content changes (build.rs tracks
# it via `rerun-if-changed`) — so a rebuild with unchanged symbols is a cache
# hit instead of a forced two-phase recompile of the whole kernel. The file is
# keyed by build variant so dev/release/tests kernels (with different symbol
# sets) do not invalidate each other's table.
KSYMS_RS="$(cd "$BUILD_DIR" && pwd)/kallsyms-${VARIANT}.rs"
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
        if [ ! -e "$KERNEL_ELF" ] || [ ! "$BUILD_DIR/kernel" -ef "$KERNEL_ELF" ]; then
            mv "$BUILD_DIR/kernel" "$KERNEL_ELF"
        fi
    fi
}

# Phase 1: build against the previous (or empty) symbol table.
build_kernel_once

# Refresh the symbol table from the phase-1 ELF (rewrites only on change),
# then rebuild. Phase 2 is a cache hit unless the symbols actually moved.
# Fails closed: without llvm-nm the kernel's panic backtraces would carry
# addresses and no names.
LLVM_NM="$("$SCRIPT_DIR/llvm_tool.sh" llvm-nm)"
python3 "$SCRIPT_DIR/gen_kernel_symbols.py" "$LLVM_NM" "$KERNEL_ELF" "$KSYMS_RS"
build_kernel_once

echo "build_kernel: ${VARIANT} kernel -> $KERNEL_ELF"

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

# Single-writer gate for the kernel master PML4. A source scan, but a few
# greps rather than a tree walk, and the regression it catches is silent:
# a second raw writer over the master compiles, boots, and only loses a
# leaf when two CPUs happen to line up.
"$SCRIPT_DIR/check_kernel_pml4_writer.sh"

# All three ELF gates run for every variant, including tests — that is the
# image the whole suite executes on. The stamp key covers the gate scripts and
# their allowlists as well as the ELF, so an edited gate re-runs instead of
# waiting for the next unrelated kernel change.
GATE_STAMP="$BUILD_DIR/.kernel-elf-gates-${VARIANT}.stamp"
GATE_INPUTS=(
    "$KERNEL_ELF"
    "$SCRIPT_DIR/check_stack_sizes.sh"
    "$SCRIPT_DIR/check_kernel_softfloat.sh"
    "$SCRIPT_DIR/check_registry_sections.sh"
    "$SCRIPT_DIR/check_bootstrap_stack_rewind.sh"
    "$SCRIPT_DIR/llvm_tool.sh"
    "$SCRIPT_DIR/gates/stack/${VARIANT}.txt"
    "$SCRIPT_DIR/gates/vector/${VARIANT}.txt"
)

# GNU coreutils on Linux, BSD `shasum` on macOS. An empty digest degrades to
# always-run, never always-skip. The env line covers the two settings that
# change a verdict without changing a tracked file.
gate_input_digest() {
    local hash
    if command -v sha256sum >/dev/null 2>&1; then
        hash="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        hash="shasum -a 256"
    else
        return 0
    fi
    {
        $hash "$@" 2>/dev/null
        printf 'variant=%s threshold=%s channel=%s\n' \
            "$VARIANT" "${STACK_SIZE_THRESHOLD:-2048}" "$RUST_CHANNEL"
    } | $hash 2>/dev/null | awk '{print $1}'
}

GATE_KEY="$(gate_input_digest "${GATE_INPUTS[@]}")"
if [ -n "$GATE_KEY" ] && [ "$(cat "$GATE_STAMP" 2>/dev/null)" = "$GATE_KEY" ]; then
    echo "check_stack_sizes: skipped (${VARIANT} kernel + gates unchanged since last pass)"
    echo "check_kernel_softfloat: skipped (${VARIANT} kernel + gates unchanged since last pass)"
    echo "check_registry_sections: skipped (${VARIANT} kernel + gates unchanged since last pass)"
    echo "check_bootstrap_stack_rewind: skipped (${VARIANT} kernel + gates unchanged since last pass)"
else
    "$SCRIPT_DIR/check_stack_sizes.sh" --variant "$VARIANT" "$KERNEL_ELF"
    "$SCRIPT_DIR/check_kernel_softfloat.sh" --variant "$VARIANT" "$KERNEL_ELF"
    "$SCRIPT_DIR/check_registry_sections.sh" "$KERNEL_ELF"
    # Release-only in effect, but run for every variant: in dev/tests the
    # rewinding function is itself instrumented and its epilogue restores the
    # slot, so the bug is invisible at runtime there. The ELF still shows the
    # store, which is the whole reason this is a gate and not a kernel test.
    "$SCRIPT_DIR/check_bootstrap_stack_rewind.sh" --variant "$VARIANT" "$KERNEL_ELF"
    [ -n "$GATE_KEY" ] && printf '%s\n' "$GATE_KEY" > "$GATE_STAMP"
fi
