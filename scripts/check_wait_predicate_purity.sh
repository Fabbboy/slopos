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
# The allowlist is empty and meant to stay that way. `napi::kick` is
# not a function this tree has; the pattern is the tripwire.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_wait_predicate_purity "$@"

# Files allow-listed to contain a kick / sleep / force-poll inside
# a wait predicate. Each entry must carry a documented incident
# reference and a removal condition.
#
# Empty. An entry here says a predicate drains the NIC itself, which
# only helps when the netpoll kthread is not running — a marker for a
# bug elsewhere, where the fix belongs.
ALLOWLIST=()

PATTERNS=(
    "napi::kick"
    "napi::wake_napi"
    "force_napi_poll"
    "sleep_current_task_ms"
)

# Patterns whose appearance inside an `Epoch::enter` / `NET_EPOCH.enter`
# scope is forbidden. Acquiring a SpinLock or sleeping while an epoch
# guard is live regresses the atomic-publish invariant — lockdep panics
# at runtime, but this fast-feedback grep keeps the source-level pattern
# out of code review entirely.
EPOCH_PATTERNS=(
    "\\.lock\\(\\)"
    "sleep_current_task_ms"
    "yield_with_deadline"
    "\\.wait\\(\\)"
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

# Search the kernel-side crates only; userland / tests are out of
# scope (they call `force_napi_poll` legitimately as a synchronous
# poll, not inside a wait predicate).
KERNEL_DIRS=(
    abi acpi boot core drivers font fs gfx hermetic karch keymap-core
    kernel-services ktesting mm net pidfd ring sched service-core signalfd
    slopos-ostd slopos-ostd-derive video vt windowing
)

# `grep -r` on a missing directory is swallowed by the `2>/dev/null || true`
# each scan needs for the no-match case, so a root with none of these would
# scan nothing and report OK.
require_kernel_dirs() {
    local root="$1"
    shift
    local dir present=0
    for dir in "$@"; do
        [ -d "$root/$dir" ] && present=$((present + 1))
    done
    if [ "$present" -eq 0 ]; then
        echo "check_wait_predicate_purity: none of the kernel crate directories exist" >&2
        echo "  under $root — the scan would be a no-op, so refusing to report OK." >&2
        exit 2
    fi
}

# Both passes run to completion and report together: pass 1 used to `exit 1`
# before pass 2 ever ran, hiding every Epoch-scope violation behind it.
scan_wait_predicates() {
    local root="$1"
    shift
    local dirs=("$@")
    cd "$root"
    require_kernel_dirs "$root" "${dirs[@]}"
    local pat hit file rest lineno dir in_kernel start context
    for pat in "${PATTERNS[@]}"; do
    # Find every file:line where the pattern occurs.
    while IFS= read -r hit; do
        [[ -z "$hit" ]] && continue
        file="${hit%%:*}"
        rest="${hit#*:}"
        lineno="${rest%%:*}"

        # Skip files outside the kernel-crate set.
        in_kernel=0
        for dir in "${dirs[@]}"; do
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
        # `[[:space:]]`, not `\s`: BSD grep -E reads `\s` as a literal `s`,
        # so the escape silently narrows this to `wait_events(`/`wait_event(`
        # on macOS.
        if echo "$context" | grep -qE 'wait_event(_timeout|_until)?([[:space:]]|\()'; then
            # Hit inside (or just after) a wait-predicate context.
            if is_allowlisted "$file"; then
                continue
            fi
            printf '1\t%s:%s: `%s` inside a wait-predicate\n' "$file" "$lineno" "$pat"
        fi
    done < <(grep -rn "$pat" "${dirs[@]}" --include="*.rs" 2>/dev/null || true)
    done
}

# ------------------------------------------------------------------------------
# Pass 2: Epoch-scope ban.
# Same 20-line lookback heuristic. A `.lock()` / `sleep` / `wait`
# appearing inside an `Epoch::enter` / `NET_EPOCH.enter` scope is
# forbidden — it risks holding a real lock or yielding across an RCU
# grace period and regresses the atomic-publish invariant.
# Runtime lockdep (slopos-ostd/src/sync/lock_graph.rs) is the load-bearing
# enforcement; this gate catches the source pattern earlier.
# ------------------------------------------------------------------------------

scan_epoch_scopes() {
    local root="$1"
    shift
    local dirs=("$@")
    cd "$root"
    require_kernel_dirs "$root" "${dirs[@]}"
    local pat hit file rest lineno dir in_kernel start context
    for pat in "${EPOCH_PATTERNS[@]}"; do
    while IFS= read -r hit; do
        [[ -z "$hit" ]] && continue
        file="${hit%%:*}"
        rest="${hit#*:}"
        lineno="${rest%%:*}"

        in_kernel=0
        for dir in "${dirs[@]}"; do
            if [[ "$file" == "$dir"/* ]]; then
                in_kernel=1
                break
            fi
        done
        [[ "$in_kernel" -eq 0 ]] && continue

        # `Epoch::enter` lives in slopos-ostd itself; the type's own
        # source file is exempt.
        if [[ "$file" == "slopos-ostd/src/sync/epoch.rs" ]]; then
            continue
        fi

        start=$((lineno > 20 ? lineno - 20 : 1))
        context=$(sed -n "${start},${lineno}p" "$file")
        # `[[:space:]]`, not `\s` — see the note in pass 1. This one was
        # strictly broken on BSD grep: the third alternative never matched.
        if echo "$context" | grep -qE '(NET_EPOCH|[A-Z_]*EPOCH)\.enter\(\)|Epoch::enter|\.enter\(\)[[:space:]]*;'; then
            printf '2\t%s:%s: `%s` inside an Epoch::enter scope\n' "$file" "$lineno" "$pat"
        fi
    done < <(grep -rEn "$pat" "${dirs[@]}" --include="*.rs" 2>/dev/null || true)
    done
}

run_scan() {
    local root="$1"
    shift
    (scan_wait_predicates "$root" "$@")
    (scan_epoch_scopes "$root" "$@")
}

# Print the 20-line lookback that made the call.
report() {
    local findings="$1" root="$2" line loc file lineno start
    printf '%s\n' "$findings" | while IFS= read -r line; do
        [ -z "$line" ] && continue
        loc="${line#*	}"
        file="${loc%%:*}"
        lineno="${loc#*:}"
        lineno="${lineno%%:*}"
        echo "  VIOLATION: ${loc}" >&2
        echo "    20-line context:" >&2
        start=$((lineno > 20 ? lineno - 20 : 1))
        sed -n "${start},${lineno}p" "$root/$file" | sed 's/^/      /' >&2
    done
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------
if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_wait_predicate_purity

    # Fixtures must live under a real KERNEL_DIRS entry. The two positive
    # files stay apart because `sleep_current_task_ms` is in both pattern
    # lists, and mixing them would make neither count interpretable.
    cat > "$(gate_fixture net/src/wait_positives.rs)" <<'FIXTURE'
fn a() { let _ = q.wait_event(|| { crate::napi::kick(); true }); }
fn b() { let _ = q.wait_event_timeout(|| { crate::napi::wake_napi(); true }, 10); }
fn c() { let _ = q.wait_event_until(|| { force_napi_poll(); true }); }
fn d() { let _ = q.wait_event(|| { sleep_current_task_ms(1); true }); }
FIXTURE

    cat > "$(gate_fixture net/src/epoch_positives.rs)" <<'FIXTURE'
fn g() {
    let _guard = NET_EPOCH.enter();
    let x = m.lock();
    sleep_current_task_ms(1);
    yield_with_deadline(0);
    let y = w.wait();
}
FIXTURE

    # No production entry to exercise, so the self-test supplies its own.
    # Pass 1 must honour it, pass 2 must ignore it.
    cat > "$(gate_fixture net/src/allowlisted_fixture.rs)" <<'FIXTURE'
fn allowlisted() { let _ = q.wait_event(|| { crate::napi::kick(); true }); }
fn not_allowlisted_for_epoch() {
    let _guard = NET_EPOCH.enter();
    let x = m.lock();
}
FIXTURE
    ALLOWLIST=("net/src/allowlisted_fixture.rs")

    # The 20-line lookback *is* the gate; both bounds are pinned here.
    cat > "$(gate_fixture net/src/negatives.rs)" <<'FIXTURE'
fn bare_kick_with_no_wait_anywhere() {
    crate::napi::kick();
}
fn far_below_a_wait_event() {
    let _ = q.wait_event(|| true);
    // 1
    // 2
    // 3
    // 4
    // 5
    // 6
    // 7
    // 8
    // 9
    // 10
    // 11
    // 12
    // 13
    // 14
    // 15
    // 16
    // 17
    // 18
    // 19
    // 20
    // 21
    crate::napi::kick();
}
FIXTURE

    cat > "$(gate_fixture slopos-ostd/src/sync/epoch.rs)" <<'FIXTURE'
fn own_source_is_exempt() {
    let _guard = Epoch::enter();
    let x = m.lock();
}
FIXTURE

    cat > "$(gate_fixture userland/src/x.rs)" <<'FIXTURE'
fn outside_the_kernel_dirs() { let _ = q.wait_event(|| { force_napi_poll(); true }); }
FIXTURE

    GATE_FINDINGS="$(run_scan "$GATE_FIXTURE_ROOT" "${KERNEL_DIRS[@]}")"

    ALLOWLIST=()

    gate_expect 1 4 "kick, wake_napi, force_napi_poll and a sleep inside the three wait_event spellings"
    gate_expect 2 5 "lock, sleep, yield and wait inside an Epoch scope, plus the allowlisted file which pass 2 does not exempt"
    gate_expect_silent 'negatives\.rs|slopos-ostd/src/sync/epoch\.rs|userland/' \
        "a kick with no wait_event above it, one 21 lines below a wait_event, the Epoch type's own source, and a directory outside the kernel crate set all stay silent"

    # A root with none of the kernel directories must fail, not report clean.
    empty_root="$(mktemp -d)"
    set +e
    (scan_wait_predicates "$empty_root" "${KERNEL_DIRS[@]}") >/dev/null 2>&1
    status=$?
    set -e
    rm -rf "$empty_root"
    if [ "$status" -ne 2 ]; then
        echo "check_wait_predicate_purity --self-test: an empty root exited $status, want 2" >&2
        GATE_SELF_TEST_FAIL=1
    else
        echo "  an empty scan root fails closed: ok"
    fi

    gate_selftest_end
fi

# ---------------------------------------------------------------------------
# Real run
# ---------------------------------------------------------------------------
wait_findings="$(scan_wait_predicates "$REPO_ROOT" "${KERNEL_DIRS[@]}")"
epoch_findings="$(scan_epoch_scopes "$REPO_ROOT" "${KERNEL_DIRS[@]}")"

fail=0
if [ -n "$wait_findings" ]; then
    report "$wait_findings" "$REPO_ROOT"
    echo "" >&2
    echo "FAIL: $(printf '%s\n' "$wait_findings" | grep -c .) wait-predicate purity violation(s) found." >&2
    echo "      A predicate passed to WaitQueue::wait_event{,_timeout,_until} must" >&2
    echo "      observe state, not paper over scheduler/NAPI starvation by" >&2
    echo "      kicking the NIC or sleeping the current task." >&2
    echo "      Move the side effect outside the predicate, or add a justified" >&2
    echo "      file entry to ALLOWLIST in scripts/check_wait_predicate_purity.sh." >&2
    fail=1
fi

if [ -n "$epoch_findings" ]; then
    report "$epoch_findings" "$REPO_ROOT"
    echo "" >&2
    echo "FAIL: $(printf '%s\n' "$epoch_findings" | grep -c .) Epoch-scope violation(s) found." >&2
    echo "      Holding a SpinLock or yielding inside an Epoch::enter scope" >&2
    echo "      delays RCU grace periods globally and risks the atomic-publish" >&2
    echo "      hazard that broke SCM_RIGHTS in unix_sendmsg. Restructure so" >&2
    echo "      the Epoch guard scope ends before the lock acquire / yield." >&2
    fail=1
fi

[ "$fail" -ne 0 ] && exit 1

echo "check_wait_predicate_purity: OK — no wait-predicate or Epoch-scope violations"
