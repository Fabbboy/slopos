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
# This 2 KiB ceiling is the load-bearing enforcement of **Inv. 5'**
# (framekernel soundness invariant): an OSTD client's stack frame
# cannot grow large enough to puncture the kernel guard page in a
# single function entry. Derived from Asterinas paper §4.3 Inv. 5 +
# the per-task stack guard frame requirement.
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
# Use a temp file instead of Bash 4 `mapfile`, and parse hex without GNU awk's
# `strtonum`, so this gate also runs on macOS's stock Bash 3.2 and awk.
offenders_file="$(mktemp)"
trap 'rm -f "$offenders_file"' EXIT
"$READOBJ" --stack-sizes "$ELF" \
    | awk -v t="$THRESHOLD" '
        function hex_digit(c) {
            c = tolower(c);
            return index("0123456789abcdef", c) - 1;
        }
        function hex_to_dec(s,    i, d, value) {
            sub(/^0[xX]/, "", s);
            value = 0;
            for (i = 1; i <= length(s); i++) {
                d = hex_digit(substr(s, i, 1));
                if (d < 0) {
                    return -1;
                }
                value = value * 16 + d;
            }
            return value;
        }
        /Functions:/ {
            fns = $0;
            sub(/.*\[/, "", fns);
            sub(/\].*/, "", fns);
        }
        /Size:/ {
            size = hex_to_dec($2);
            if (size > t) {
                n = split(fns, a, /, */);
                for (i = 1; i <= n; i++) {
                    if (a[i] != "") {
                        printf "%d\t%s\n", size, a[i];
                    }
                }
            }
        }' \
    | sort -rn > "$offenders_file"

offender_count="$(wc -l < "$offenders_file" | tr -d '[:space:]')"

if [ "$offender_count" -gt 0 ]; then
    printf 'check_stack_sizes: %d function(s) exceed STACK_SIZE_THRESHOLD=%s bytes:\n' \
        "$offender_count" "$THRESHOLD" >&2
    while IFS= read -r line; do
        size="${line%%$'\t'*}"
        fn="${line#*$'\t'}"
        printf '  %10d B  %s\n' "$size" "$fn" >&2
    done < "$offenders_file"
    echo >&2
    echo "  (decode names with: cargo install rustfilt; <name> | rustfilt)" >&2
    exit 1
fi

echo "check_stack_sizes: OK — all frames <= ${THRESHOLD} bytes"
