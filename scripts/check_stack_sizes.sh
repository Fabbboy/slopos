#!/usr/bin/env bash
# Fail the build if any function in the kernel ELF has a stack frame larger
# than STACK_SIZE_THRESHOLD bytes. Default: 2048 (2 KiB), matching
# Linux mainline's CONFIG_FRAME_WARN default on x86_64/arm64 but
# enforced as a hard failure here. SlopOS inspects
# the post-link ELF rather than a compile-time heuristic (so inline
# expansion, NRVO failures, and trait-object dispatch are all accounted
# for).
#
# A small measured allowlist permits known frames that exceed 2 KiB. Each
# entry is capped at its measured size, so growth still fails. Entries fall in
# two groups: vendored-unwinder frames that run only during a panic unwind
# (off every hot path), and a handful of SlopOS functions whose large
# temporaries are already heap-boxed but whose residual frame still sits
# between 2 KiB and the 4 KiB guard-page size.
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
#
# Allowlist entries are matched against rustc's mangled symbol names as
# reported by llvm-readobj. Patterns use only stable readable fragments where
# possible; the byte caps are measured from the current production kernel ELF.
allowed_stack_frame_max() {
    local fn="$1"

    case "$fn" in
        # Vendored-unwinder frames: reached only while a panic unwinds, never on
        # a hot path. The DWARF CFI evaluation is inherently frame-heavy.
        *CallFrameInstruction*E5parse*9unwinding*) echo 3864 ;;
        *unwinding9panicking12catch_unwind8do_catch*slopos_ostd6unwind11KernelPanic*) echo 2744 ;;
        *unwinding8unwinder5frame*Frame12from_context*) echo 2616 ;;
        *unwinding8unwinder19force_unwind_phase2*) echo 2072 ;;
        *UnwindTable*StoreInVec*E8evaluate*) echo 2072 ;;
        # SlopOS functions whose large temporaries are already heap-boxed; the
        # residual frame is inherent local/format-args state, still under the
        # 4 KiB guard page.
        *slopos_mm10process_vm20process_vm_clone_cow*) echo 2200 ;;
        *slopos_mm10process_vm17create_process_vm*) echo 2104 ;;
        *slopos_acpi3aml14process_device*) echo 2424 ;;
        *slopos_net4ipv49handle_rx*) echo 2344 ;;
        *slopos_fs4cpio19for_each_cpio_entry*unpack_cpio_into_root*) echo 2344 ;;
        *slopos_net3dns11dns_resolve*) echo 2200 ;;
        *slopos_acpi3aml6interp*Interp4eval*) echo 2200 ;;
        *slopos_core7syscall2fs19poll_ioctl_handlers14syscall_select*) echo 2200 ;;
        *slopos_core7syscall12net_handlers15syscall_sendmsg*) echo 2200 ;;
        # Library smallsort monomorphizations: scratch is the element size, not
        # reducible without changing the sorted element type.
        *sort4_stable*slopos_drivers3pci*MatchIndex14candidates_for*) echo 2248 ;;
        *sort4_stable*slopos_drivers12platform_bus9matchmake*) echo 2248 ;;
        *) echo "" ;;
    esac
}

candidates_file="$(mktemp)"
offenders_file="$(mktemp)"
trap 'rm -f "$candidates_file" "$offenders_file"' EXIT
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
    | sort -rn > "$candidates_file"

allowed_count=0
while IFS= read -r line; do
    [ -z "$line" ] && continue
    size="${line%%$'\t'*}"
    fn="${line#*$'\t'}"
    allowed_max="$(allowed_stack_frame_max "$fn")"
    if [ -n "$allowed_max" ] && [ "$size" -le "$allowed_max" ]; then
        allowed_count=$(( allowed_count + 1 ))
        continue
    fi
    printf '%s\n' "$line" >> "$offenders_file"
done < "$candidates_file"

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
    echo "  Known >${THRESHOLD} B frames must be added to allowed_stack_frame_max with a measured cap." >&2
    exit 1
fi

if [ "$allowed_count" -gt 0 ]; then
    echo "check_stack_sizes: OK — all unallowlisted frames <= ${THRESHOLD} bytes (${allowed_count} measured allowlist hit(s))"
else
    echo "check_stack_sizes: OK — all frames <= ${THRESHOLD} bytes"
fi
