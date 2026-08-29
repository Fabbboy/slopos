#!/usr/bin/env bash
set -euo pipefail

# Single-writer gate for the BSP's SafeStack bootstrap data-stack pointer.
#
# The BSP runs the entire boot call chain — `kernel_main` through
# `boot_init_run_phase` and every init step — on `BOOTSTRAP_UNSAFE_STACK`.
# Its allocation pointer lives in `BSP_BOOTSTRAP_TASK.unsafe_stack_sp`, seeded
# once by `boot/limine_entry.s` and thereafter *decremented* by each
# instrumented prologue and restored by the matching epilogue. That slot is a
# live stack pointer, not a configuration value.
#
# Storing the buffer *top* back into it re-hands the span of every live caller
# frame to the allocator. The frames opened afterwards are laid over the frames
# still owned by the callers, and the first caller to read a clobbered local
# takes the consequence. That shipped: `init_bootstrap_tasks`, called from
# `smp_init` at drivers-phase priority 45, re-seeded the BSP slot while
# `boot_init_run_phase` held ~0x950 bytes below the top. Its `ordered[64]`
# array of `&BootInitStep` was overwritten with lockdep `HeldLock` records, and
# the indirect `call *0x10(%r15)` dispatched through a `poison_fn` field —
# #GP(0) on a non-canonical target, two steps into the drivers phase.
#
# Why a gate rather than a kernel test: in the dev and tests builds
# `init_bootstrap_tasks` is *itself* SafeStack-instrumented, so its own
# epilogue restores the slot and the store is invisible. In the release build
# it has no such frame and the store persists. A `just test` run therefore
# cannot observe this bug at all — only the shipped ELF can, which is why the
# fault reproduced on real hardware and never in the test suite.
#
# The check is a name-and-target check on the disassembly: no store to
# `BSP_BOOTSTRAP_TASK` may originate anywhere but the boot trampoline, which is
# hand-written assembly and holds no Rust symbol.
#
# Usage:
#     scripts/check_bootstrap_stack_rewind.sh --variant release builddir/kernel-release.elf
#     scripts/check_bootstrap_stack_rewind.sh --self-test

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# The disassembly of a debug kernel is hundreds of MiB; the awk filter consumes
# all of it. `set -o pipefail` would otherwise turn objdump's SIGPIPE into a
# gate failure that has nothing to do with the check.
set +o pipefail

OBJDUMP="${OBJDUMP:-objdump}"
NM="${NM:-nm}"

usage() {
    echo "usage: $0 --variant <dev|release|tests> <kernel.elf>" >&2
    echo "       $0 --self-test" >&2
    exit 2
}

# Functions permitted to store to the BSP bootstrap stub. Empty by design:
# `limine_entry.s` is the only writer and carries no Rust symbol, so any
# named function reaching that address is the regression this gate exists for.
ALLOWED_WRITERS=()

fail=0
report() {
    echo "check_bootstrap_stack_rewind: FAIL — $1" >&2
    fail=1
}

# Print every function that stores to the symbol at $addr, one per line.
#
# objdump annotates a rip-relative operand with "# <hex> <SYMBOL>", where the
# hex is unpadded. Match on the resolved symbol name in that comment rather
# than on an address spelling, so the check is independent of padding and of
# where the linker placed the object.
writers_of() {
    local elf="$1" sym="$2"

    "$OBJDUMP" -d --no-show-raw-insn "$elf" 2>/dev/null | awk -v sym="$sym" '
        /^[0-9a-f]+ <.*>:$/ {
            fn = $0
            sub(/^[0-9a-f]+ </, "", fn)
            sub(/>:$/, "", fn)
            next
        }
        {
            ci = index($0, "#")
            if (ci == 0) next
            comment = substr($0, ci)
            if (comment !~ ("<" sym "(\\+0x0)?>")) next

            insn = substr($0, 1, ci - 1)
            # Destination side only: a store leaves the rip-relative term as
            # the final operand, whereas a load or `lea` reads it.
            if (insn ~ /\ylea\y/) next
            if (insn ~ /,[ \t]*%[a-z0-9]+[ \t]*$/) next
            if (insn ~ /\ymov[a-z]*\y/ && insn ~ /\(%rip\)[ \t]*$/) print fn
        }
    ' | sort -u
}

check_elf() {
    local elf="$1" variant="$2"

    [[ -f "$elf" ]] || { echo "check_bootstrap_stack_rewind: missing ELF $elf" >&2; exit 1; }

    local sym_addr
    sym_addr="$("$NM" "$elf" | awk '$3 == "BSP_BOOTSTRAP_TASK" { print $1; exit }')"
    if [[ -z "$sym_addr" ]]; then
        report "BSP_BOOTSTRAP_TASK not found in $elf (input sanity)"
        return
    fi
    local addr="0x${sym_addr}"

    # Input sanity: both symbols must exist, or the scan below is vacuous and
    # an ELF with the discipline removed would read as clean.
    local stack_addr
    stack_addr="$("$NM" "$elf" | awk '$3 == "BOOTSTRAP_UNSAFE_STACK" { print $1; exit }')"
    if [[ -z "$stack_addr" ]]; then
        report "BOOTSTRAP_UNSAFE_STACK not found in $elf (input sanity)"
        return
    fi

    local writers
    writers="$(writers_of "$elf" BSP_BOOTSTRAP_TASK)"

    local bad=0 w
    while IFS= read -r w; do
        [[ -n "$w" ]] || continue
        local ok=0
        for allowed in ${ALLOWED_WRITERS[@]+"${ALLOWED_WRITERS[@]}"}; do
            [[ "$w" == *"$allowed"* ]] && ok=1
        done
        if (( ! ok )); then
            report "$w stores to BSP_BOOTSTRAP_TASK ($addr) — that slot is the BSP's live data-stack pointer; rewinding it frees every live caller frame"
            bad=1
        fi
    done <<< "$writers"

    if (( ! bad )); then
        echo "check_bootstrap_stack_rewind: OK — variant=$variant, BSP_BOOTSTRAP_TASK ($addr) has no Rust writer"
    fi
}

self_test() {
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN

    command -v cc >/dev/null 2>&1 || { echo "check_bootstrap_stack_rewind: self-test needs cc" >&2; exit 1; }

    # A planted violation: a named function that stores to the tracked symbol.
    cat > "$tmp/bad.c" <<'EOF'
unsigned long BSP_BOOTSTRAP_TASK;
unsigned long BOOTSTRAP_UNSAFE_STACK;
void init_bootstrap_tasks(void) { BSP_BOOTSTRAP_TASK = (unsigned long)&BOOTSTRAP_UNSAFE_STACK; }
int main(void) { init_bootstrap_tasks(); return 0; }
EOF
    cc -O1 -no-pie -o "$tmp/bad.elf" "$tmp/bad.c" 2>/dev/null || {
        echo "check_bootstrap_stack_rewind: self-test could not build fixture" >&2; exit 1; }

    local out rc
    set +e
    out="$(fail=0; check_elf "$tmp/bad.elf" selftest 2>&1; echo "rc=$fail")"
    set -e
    rc="${out##*rc=}"
    if [[ "$rc" != "1" ]]; then
        echo "check_bootstrap_stack_rewind: SELF-TEST FAIL — gate accepted a planted writer" >&2
        echo "$out" >&2
        exit 1
    fi

    # And silence on the form it must accept: no writer at all.
    cat > "$tmp/good.c" <<'EOF'
unsigned long BSP_BOOTSTRAP_TASK;
unsigned long BOOTSTRAP_UNSAFE_STACK;
unsigned long read_only(void) { return BSP_BOOTSTRAP_TASK; }
int main(void) { return (int)read_only(); }
EOF
    cc -O1 -no-pie -o "$tmp/good.elf" "$tmp/good.c" 2>/dev/null || {
        echo "check_bootstrap_stack_rewind: self-test could not build fixture" >&2; exit 1; }

    set +e
    out="$(fail=0; check_elf "$tmp/good.elf" selftest 2>&1; echo "rc=$fail")"
    set -e
    rc="${out##*rc=}"
    if [[ "$rc" != "0" ]]; then
        echo "check_bootstrap_stack_rewind: SELF-TEST FAIL — gate rejected a read-only fixture" >&2
        echo "$out" >&2
        exit 1
    fi

    echo "check_bootstrap_stack_rewind: self-test OK — rejects a planted writer, accepts a reader"
}

[[ $# -ge 1 ]] || usage

if [[ "$1" == "--self-test" ]]; then
    self_test
    exit 0
fi

VARIANT=""
ELF=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --variant) VARIANT="${2:-}"; shift 2 ;;
        -*) usage ;;
        *) ELF="$1"; shift ;;
    esac
done

[[ -n "$VARIANT" && -n "$ELF" ]] || usage

check_elf "$ELF" "$VARIANT"
exit "$fail"
