#!/usr/bin/env bash
# Fail the build if any function in the kernel ELF has a stack frame larger
# than STACK_SIZE_THRESHOLD bytes. Default: 2048 (2 KiB) — matches Linux
# mainline's default `CONFIG_FRAME_WARN` on x86_64/arm64, but SlopOS
# fails the build rather than merely warning, and inspects the
# post-link ELF rather than a compile-time heuristic (so inline
# expansion, NRVO failures, and trait-object dispatch are all accounted
# for).
#
# Tightening below 2 KiB requires follow-up work on a handful of
# warm-path functions (`DataState::process_payload_fin_and_ack`,
# `syscall_getsockname`, `dns_resolve`, `ipv4::handle_rx`,
# `panic_handler_impl`, `tcp::input`, `tcp::close`,
# `virtio_net_probe`, ramfs insertion).
#
# Relies on `-Zemit-stack-sizes` populating the `.stack_sizes` ELF section;
# see scripts/build_kernel.sh for where the flag is injected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

ELF="${1:-$REPO_ROOT/builddir/kernel.elf}"
THRESHOLD="${STACK_SIZE_THRESHOLD:-2048}"

if [ ! -f "$ELF" ]; then
    echo "check_stack_sizes: missing $ELF (run \`just build\` first)" >&2
    exit 2
fi

RUST_CHANNEL="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$REPO_ROOT/rust-toolchain.toml")"
SYSROOT="$(rustc +"$RUST_CHANNEL" --print sysroot)"
READOBJ="$SYSROOT/lib/rustlib/$(rustc +"$RUST_CHANNEL" -vV | sed -n 's/^host: //p')/bin/llvm-readobj"

if [ ! -x "$READOBJ" ]; then
    READOBJ="$(command -v llvm-readobj || true)"
fi
if [ -z "$READOBJ" ]; then
    echo "check_stack_sizes: llvm-readobj not found (install llvm-tools-preview component)" >&2
    exit 2
fi

# Parse `llvm-readobj --stack-sizes` output:
#
#   Entry {
#     Functions: [NAME1, NAME2, ...]
#     Size: 0xNN
#   }
#
# Emit size<TAB>function pairs for everything exceeding the threshold.
mapfile -t offenders < <(
    "$READOBJ" --stack-sizes "$ELF" \
    | awk -v t="$THRESHOLD" '
        /Functions:/ {
            gsub(/.*\[/, ""); gsub(/\].*/, "");
            fns = $0;
        }
        /Size:/ {
            s = $2; sub(/^0x/, "", s);
            v = strtonum("0x" s);
            if (v > t) {
                n = split(fns, a, /, */);
                for (i = 1; i <= n; i++) printf "%d\t%s\n", v, a[i];
            }
        }' \
    | sort -rn
)

if [ "${#offenders[@]}" -gt 0 ]; then
    printf 'check_stack_sizes: %d function(s) exceed STACK_SIZE_THRESHOLD=%s bytes:\n' \
        "${#offenders[@]}" "$THRESHOLD" >&2
    for line in "${offenders[@]}"; do
        size="${line%%$'\t'*}"
        fn="${line#*$'\t'}"
        printf '  %10d B  %s\n' "$size" "$fn" >&2
    done
    echo >&2
    echo "  (decode names with: cargo install rustfilt; <name> | rustfilt)" >&2
    exit 1
fi

echo "check_stack_sizes: OK — all frames <= ${THRESHOLD} bytes"
