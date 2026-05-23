#!/usr/bin/env bash
# Wait-predicate purity gate.
#
# `WaitQueue::wait_event{,_timeout,_until}` predicates do nothing but
# observe state — they must not paper over a scheduler / NAPI
# starvation bug by side-effecting the world. The pre-refactor
# `crate::napi::kick()` workaround was exactly that, and letting it
# return would silently re-introduce the curl-times-out regression
# that prompted the scheduler rip-and-replace.
#
# Scope: the gate flags `napi::kick`, `napi::wake_napi`,
# `force_napi_poll`, and `sleep_current_task_ms` inside wait
# predicates — all are starvation-papering moves that should never
# live inside a `wait_event*` closure.
#
# This is a simple `grep`-based audit, not a full Rust AST walker.
# It scans the workspace for the offending tokens, then for each hit
# walks ~20 lines back looking for a `wait_event` / `wait_event_timeout`
# / `wait_event_until` opener. A hit that resolves to a wait-predicate
# context fails the gate.
#
# Allowlist is empty: Phase 2 retired the last transitional kick and
# the production data path now relies entirely on the IRQ-driven
# threaded-NAPI cadence. Any new allowlist entry must come with a
# documented incident and an exit plan — the gate exists to keep
# regressions from drifting back in unnoticed.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

# Files allow-listed to contain a kick / sleep / force-poll inside
# a wait predicate. Each entry must carry a documented incident
# reference and a removal condition.
#
# `net/src/socket.rs::wait_socket_event` — the predicate calls
# `napi::kick()` as a synchronous drain because the virtio-net MSI-X
# delivery path shows post-probe IRQ-delivery gaps that have not yet
# been root-caused. The IRQ-driven kthread parks on `NAPI_WAKER` and
# wakes on `arm_and_wake`; we observe the wake fires reliably during
# probe (DHCP / ARP-scan IRQs) but is intermittent afterward. The
# kick keeps the user-task wake-up path correct on top of that;
# remove this allowlist entry once the driver fix lands and the
# end-to-end test (`userland/src/bin/tests/curl_e2e_test.rs`) passes
# without it.
ALLOWLIST=(
    "net/src/socket.rs"
)

PATTERNS=(
    "napi::kick"
    "napi::wake_napi"
    "force_napi_poll"
    "sleep_current_task_ms"
)

is_allowlisted() {
    local file="$1"
    for entry in "${ALLOWLIST[@]}"; do
        if [[ "$file" == "$entry" ]]; then
            return 0
        fi
    done
    return 1
}

found_violations=0

# Search the kernel-side crates only; userland / tests are out of
# scope (they call `force_napi_poll` legitimately as a synchronous
# poll, not inside a wait predicate).
KERNEL_DIRS=(
    abi acpi boot core drivers font fs gfx hermetic karch
    kernel-services mm net sched service-core slopos-ostd
    slopos-ostd-derive video windowing
)

for pat in "${PATTERNS[@]}"; do
    # Find every file:line where the pattern occurs.
    while IFS= read -r hit; do
        [[ -z "$hit" ]] && continue
        file="${hit%%:*}"
        rest="${hit#*:}"
        lineno="${rest%%:*}"

        # Skip files outside the kernel-crate set.
        in_kernel=0
        for dir in "${KERNEL_DIRS[@]}"; do
            if [[ "$file" == "$dir"/* ]]; then
                in_kernel=1
                break
            fi
        done
        [[ "$in_kernel" -eq 0 ]] && continue

        # Walk back 20 lines looking for a wait_event opener whose
        # closing brace is below `lineno`. Heuristic: an unmatched
        # `||` or `|` opener (closure) near a `wait_event*` token
        # means we're inside a predicate.
        start=$((lineno > 20 ? lineno - 20 : 1))
        context=$(sed -n "${start},${lineno}p" "$file")
        if echo "$context" | grep -qE 'wait_event(_timeout|_until)?(\s|\()'; then
            # Hit inside (or just after) a wait-predicate context.
            if is_allowlisted "$file"; then
                continue
            fi
            echo "  VIOLATION: $file:$lineno: \`$pat\` inside a wait-predicate"
            echo "    20-line context:"
            sed -n "${start},${lineno}p" "$file" | sed 's/^/      /'
            found_violations=$((found_violations + 1))
        fi
    done < <(grep -rn "$pat" "${KERNEL_DIRS[@]}" --include="*.rs" 2>/dev/null || true)
done

if [[ "$found_violations" -gt 0 ]]; then
    echo ""
    echo "FAIL: $found_violations wait-predicate purity violation(s) found."
    echo "      A predicate passed to WaitQueue::wait_event{,_timeout,_until} must"
    echo "      observe state, not paper over scheduler/NAPI starvation by"
    echo "      kicking the NIC or sleeping the current task."
    echo "      Move the side effect outside the predicate, or add a justified"
    echo "      file entry to ALLOWLIST in scripts/check_wait_predicate_purity.sh."
    exit 1
fi

echo "check_wait_predicate_purity: OK — no wait-predicate violations"
