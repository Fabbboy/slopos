#!/usr/bin/env bash
# Fail the build if an under-classified syscall can reach a terminal power
# primitive.
#
# ---------------------------------------------------------------------------
# What this exists to catch
# ---------------------------------------------------------------------------
#
# The classification gate in `core/src/syscall/handlers.rs` is a `rustc` error
# and needs no script: every dispatch-table slot names one capability, and a
# `const` histogram asserts totality. But it covers *the table*, not
# *reachability*. `roulette_result` is the proof: it was classified, the gate
# was green, and its loss arm called `kernel_reboot` two syscalls away from an
# unprivileged caller. A slot-level gate cannot see that, by construction.
#
# So this walks the actual call graph in the linked ELF, from every syscall
# handler to the power primitives, and requires that each handler which can
# reach one either
#
#   (a) is classified `Power` itself, or
#   (b) appears in the tracked allowlist below with a stated reason.
#
# The ELF is the right input rather than the source: inlining, generic
# instantiation and trait-object dispatch all change who really calls whom, and
# a source scan answers for none of them.
#
# ---------------------------------------------------------------------------
# The seam this does not close
# ---------------------------------------------------------------------------
#
# Indirect calls. A `call *%rax` through a function pointer or a vtable has no
# callee in the disassembly, so a path that goes through one is invisible here.
# That is why the *kernel-initiated* callers -- which reach power through the
# `PowerOps` indirection deliberately -- are an explicit tracked list rather
# than something this discovers. The list being short and reviewed is the
# control; this gate keeps it from growing silently.
#
# Usage:
#     scripts/check_authority_reachability.sh --variant dev builddir/kernel-dev.elf
#     scripts/check_authority_reachability.sh --variant dev --emit-allowlist builddir/kernel-dev.elf
#     scripts/check_authority_reachability.sh --self-test

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
GATE_DATA_DIR="$SCRIPT_DIR/gates/authority"

VARIANT=""
ELF=""
EMIT_ALLOWLIST=0
SELF_TEST=0

while [ $# -gt 0 ]; do
    case "$1" in
        --variant) VARIANT="${2:?--variant needs a value}"; shift 2 ;;
        --emit-allowlist) EMIT_ALLOWLIST=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        -*) echo "usage: check_authority_reachability.sh [--variant V] [--emit-allowlist] ELF" >&2; exit 2 ;;
        *) ELF="$1"; shift ;;
    esac
done

# ---------------------------------------------------------------------------
# The terminal primitives. A handler reaching any of these acts on the whole
# machine irreversibly, which is what `Power` classifies.
# ---------------------------------------------------------------------------
# `slopos_ostd::platform::power::{shutdown,reboot}` -- the choke point every
# caller funnels through, rather than boot's implementation. Naming the ostd
# functions is what makes the walk see *authority* rather than one machine's
# reset sequence, and it survives the implementation moving.
SINK_RE='slopos_ostd8platform5power(8shutdown|6reboot)'

# The handler symbols to walk from. `define_syscall!` names every one
# `syscall_*` inside a `slopos_core` module, so the set is discoverable rather
# than listed -- a new handler is walked the moment it is linked.
HANDLER_RE='slopos_core.*7syscall.*(syscall_[a-z0-9_]+)'

# ---------------------------------------------------------------------------
# Build the call graph and find every handler that reaches a sink.
#
# One awk pass over the disassembly: track the enclosing symbol, record each
# direct `call <target>` edge, then BFS backwards from the sinks.
# ---------------------------------------------------------------------------
reachable_handlers() {
    local elf="$1" objdump
    objdump="$(cd "$REPO_ROOT" && scripts/llvm_tool.sh llvm-objdump)"
    "$objdump" -d --no-show-raw-insn "$elf" 2>/dev/null | awk -v sink_re="$SINK_RE" -v handler_re="$HANDLER_RE" '
        # Symbol header: "ffffffff80019060 <_RNv...>:"
        /^[0-9a-f]+ <.*>:$/ {
            cur = $2
            gsub(/^</, "", cur); gsub(/>:$/, "", cur)
            next
        }
        # A direct call names its target in angle brackets.
        /[[:space:]]call(q)?[[:space:]]/ && /<.*>/ {
            if (cur == "") next
            line = $0
            # Take the last <...> on the line: a call with a displacement
            # comment can carry an earlier one.
            n = split(line, parts, "<")
            target = parts[n]
            sub(/>.*$/, "", target)
            # Skip PLT-ish and offset forms ("sym+0x10").
            sub(/\+0x[0-9a-f]+$/, "", target)
            if (target == "" || target == cur) next
            key = cur SUBSEP target
            if (!(key in seen)) {
                seen[key] = 1
                callers[target] = callers[target] " " cur
            }
            next
        }
        END {
            # Seed the frontier with every sink symbol.
            for (k in callers) {}
            nfront = 0
            for (key in callers) {
                if (key ~ sink_re) { front[++nfront] = key; hit[key] = 1 }
            }
            # Backwards BFS. Bounded by the edge count, so it terminates.
            i = 1
            while (i <= nfront) {
                node = front[i++]
                m = split(callers[node], preds, " ")
                for (j = 1; j <= m; j++) {
                    p = preds[j]
                    if (p == "" || p in hit) continue
                    hit[p] = 1
                    front[++nfront] = p
                }
            }
            for (sym in hit) {
                if (match(sym, handler_re)) print sym
            }
        }
    ' | sort -u
}

# Map a mangled symbol to the bare handler name the allowlist keys on.
handler_name() {
    sed -E 's/.*[0-9]+(syscall_[a-z0-9_]+).*/\1/'
}

# ---------------------------------------------------------------------------
# Self-test: a synthetic disassembly with a known-reachable handler, so the
# walker is proven to find a two-hop path and to stay silent on an unrelated
# one. A gate never observed to reject has not been observed to work.
# ---------------------------------------------------------------------------
if [ "$SELF_TEST" -eq 1 ]; then
    echo "check_authority_reachability: self-test against a synthetic call graph"
    fixture="$(mktemp)"
    trap 'rm -f "$fixture"' EXIT INT TERM
    cat > "$fixture" <<'DIS'
0000000000001000 <_RNvNtCs1_11slopos_core7syscall13core_handlers13syscall_reboot>:
    1000: callq  0x3000 <_RNvNtCs1_11slopos_ostd8platform5power6reboot>
0000000000001100 <_RNvNtCs1_11slopos_core7syscall12ui_handlers22syscall_roulette_result>:
    1100: callq  0x2000 <_RNvNtCs1_11slopos_core7syscall12ui_handlers11fate_unwind>
0000000000001200 <_RNvNtCs1_11slopos_core7syscall12core_handlers12syscall_yield>:
    1200: callq  0x4000 <_RNvNtCs1_11slopos_sched9scheduler8schedule>
0000000000002000 <_RNvNtCs1_11slopos_core7syscall12ui_handlers11fate_unwind>:
    2000: callq  0x3000 <_RNvNtCs1_11slopos_ostd8platform5power6reboot>
0000000000003000 <_RNvNtCs1_11slopos_ostd8platform5power6reboot>:
    3000: ret
0000000000004000 <_RNvNtCs1_11slopos_sched9scheduler8schedule>:
    4000: ret
DIS
    found="$(awk -v sink_re="$SINK_RE" -v handler_re="$HANDLER_RE" '
        /^[0-9a-f]+ <.*>:$/ { cur = $2; gsub(/^</, "", cur); gsub(/>:$/, "", cur); next }
        /[[:space:]]call(q)?[[:space:]]/ && /<.*>/ {
            if (cur == "") next
            n = split($0, parts, "<"); target = parts[n]; sub(/>.*$/, "", target)
            sub(/\+0x[0-9a-f]+$/, "", target)
            if (target == "" || target == cur) next
            key = cur SUBSEP target
            if (!(key in seen)) { seen[key] = 1; callers[target] = callers[target] " " cur }
            next
        }
        END {
            nfront = 0
            for (key in callers) if (key ~ sink_re) { front[++nfront] = key; hit[key] = 1 }
            i = 1
            while (i <= nfront) {
                node = front[i++]
                m = split(callers[node], preds, " ")
                for (j = 1; j <= m; j++) { p = preds[j]; if (p == "" || p in hit) continue; hit[p] = 1; front[++nfront] = p }
            }
            for (sym in hit) if (match(sym, handler_re)) print sym
        }
    ' "$fixture" | handler_name | sort -u)"

    fail=0
    # The direct caller must be found.
    if ! printf '%s\n' "$found" | grep -qx 'syscall_reboot'; then
        echo "  FAIL: the direct caller was not found" >&2; fail=1
    else
        echo "  direct call: syscall_reboot found"
    fi
    # The two-hop caller must be found -- this is the roulette_result shape,
    # the whole reason the gate exists.
    if ! printf '%s\n' "$found" | grep -qx 'syscall_roulette_result'; then
        echo "  FAIL: the two-hop path was not found (the roulette_result shape)" >&2; fail=1
    else
        echo "  two-hop call: syscall_roulette_result found"
    fi
    # An unrelated handler must NOT be reported.
    if printf '%s\n' "$found" | grep -qx 'syscall_yield'; then
        echo "  FAIL: false positive on an unrelated handler" >&2; fail=1
    else
        echo "  negatives: syscall_yield not reported"
    fi
    rm -f "$fixture"; trap - EXIT INT TERM
    if [ "$fail" -ne 0 ]; then
        echo "check_authority_reachability: SELF-TEST FAILED" >&2
        exit 1
    fi
    echo "check_authority_reachability: self-test OK"
    exit 0
fi

if [ -z "$ELF" ] || [ ! -f "$ELF" ]; then
    echo "check_authority_reachability: need a kernel ELF (got '${ELF:-}')" >&2
    exit 2
fi
if [ -z "$VARIANT" ]; then
    echo "check_authority_reachability: --variant is required (dev|tests|release)" >&2
    exit 2
fi

ALLOWLIST="$GATE_DATA_DIR/$VARIANT.txt"
FOUND="$(reachable_handlers "$ELF" | handler_name | sort -u)"

if [ -z "$FOUND" ]; then
    echo "check_authority_reachability: no handler reaches a power primitive," >&2
    echo "  which cannot be right -- 'halt' calls one directly. The symbol" >&2
    echo "  patterns have rotted, or the ELF carries no disassembly." >&2
    exit 2
fi

if [ "$EMIT_ALLOWLIST" -eq 1 ]; then
    mkdir -p "$GATE_DATA_DIR"
    {
        echo "# check_authority_reachability allowlist — variant: $VARIANT"
        echo "#"
        echo "# Syscall handlers that can reach a terminal power primitive."
        echo "# Each must be classified Power, or carry a stated reason here."
        echo "#"
        echo "#     scripts/check_authority_reachability.sh --variant $VARIANT \\"
        echo "#         --emit-allowlist builddir/kernel-$VARIANT.elf"
        echo
        printf '%s\n' "$FOUND"
    } > "$ALLOWLIST"
    echo "check_authority_reachability: wrote $ALLOWLIST"
    exit 0
fi

if [ ! -f "$ALLOWLIST" ]; then
    echo "check_authority_reachability: no allowlist at $ALLOWLIST" >&2
    echo "  Generate one with --emit-allowlist and review every entry." >&2
    exit 2
fi

ALLOWED="$(grep -vE '^[[:space:]]*(#|$)' "$ALLOWLIST" | awk '{print $1}' | sort -u)"

UNEXPECTED="$(comm -23 <(printf '%s\n' "$FOUND") <(printf '%s\n' "$ALLOWED"))"
DEAD="$(comm -13 <(printf '%s\n' "$FOUND") <(printf '%s\n' "$ALLOWED"))"

status=0
if [ -n "$UNEXPECTED" ]; then
    echo "check_authority_reachability: these handlers reach a power primitive" >&2
    echo "  but are not accounted for:" >&2
    printf '%s\n' "$UNEXPECTED" | sed 's/^/    /' >&2
    cat >&2 <<'MSG'

  A slot-level classification cannot see this: the handler need not call the
  primitive itself, only reach it. Either classify the handler `Power`, or --
  if the path is genuinely gated by something else, as roulette_result's
  reboot arm is by BOOT_FLAG_FATE_REBOOT -- add it to the allowlist with the
  gate named in a comment.
MSG
    status=1
fi

# A dead entry is an exemption for a path that no longer exists; keeping it
# would let the real path grow back under its cover.
if [ -n "$DEAD" ]; then
    echo "check_authority_reachability: allowlist entries matching nothing:" >&2
    printf '%s\n' "$DEAD" | sed 's/^/    /' >&2
    echo "  Remove them: a dead exemption hides the path growing back." >&2
    status=1
fi

if [ "$status" -ne 0 ]; then
    exit 1
fi

echo "check_authority_reachability: OK — variant=$VARIANT, $(printf '%s\n' "$FOUND" | grep -c .) handler(s) reach a power primitive, all accounted for"
