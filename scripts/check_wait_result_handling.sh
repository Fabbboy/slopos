#!/usr/bin/env bash
# Wait-result handling gate.
#
# Every blocking entry point returns `WaitResult<R>`, and a `Result` is
# `#[must_use]` under `warnings = "deny"` — so discarding one as a bare
# statement is already a build error. The compiler cannot see the silencers,
# though: `let _ =` and `.ok()` / `.unwrap_or*()` each turn four distinct ways
# a wait can end into one, and the one they erase is `Killed`. A site that
# erases it is a site where a dying task keeps waiting.
#
# Scope, stated so it is not mistaken for more: this catches the one-token
# silencers. A written-out `match r { Ok(x) => x, Err(_) => default }` is not
# flagged, deliberately — the abort variants genuinely collapse at some sites
# (a driver ring quarantines its chain the same way on a timeout as on a kill)
# and a gate that forbade it would only be routed around. The compile error
# from `Result` is the enforcement; this is the tripwire against un-doing it.
#
# Portability: `[[:space:]]` not `\s` — BSD grep reads `\s` as a literal `s`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_wait_result_handling "$@"

# Empty, and meant to stay that way. An entry needs a documented incident and
# a removal condition, the same rule check_wait_predicate_purity.sh states.
ALLOWLIST=()

# Same crate set the sibling gate scans. `userland/` is out of scope: it has
# no kernel wait queues.
KERNEL_DIRS=(
    abi acpi boot core drivers font fs gfx hermetic karch keymap-core
    kernel-services ktesting mm net pidfd ring sched service-core signalfd
    slopos-ostd slopos-ostd-derive video vt
)

# The token that makes a line a wait site. The trailing class is what keeps
# `handle.wait_events(timeout_ms)` — an unrelated userland poll helper — from
# matching.
WAIT_RE='wait_event(_interruptible)?(_timeout)?(_until)?([[:space:]]|\()'

# How far above a wait token a `let _ =` may sit and still bind it. A
# multi-line `BUS\n .subscribe(..)\n .wait_event(..)` is 3; 6 is headroom.
LOOKBACK=6

# Everything chained onto a wait result, and nothing inside its arguments.
#
# Depth matters and grep cannot count: `wait_event(|| x.unwrap_or(true))` has a
# silencer inside the predicate — the closure's own default, not the wait's —
# while `wait_event(..).ok()` has one chained on. Both look identical to a
# line-oriented match, so this walks the statement tracking paren depth and
# only reports a silencer at depth zero.
SILENCER_SCANNER='
import re, sys
WAIT = re.compile(r"wait_event(_interruptible)?(_timeout)?(_until)?(?=[\s(])")
SILENCER = re.compile(r"\.(ok|unwrap_or|unwrap_or_default|unwrap_or_else)\s*\(")
path = sys.argv[1]
try:
    src = open(path, errors="replace").read()
except OSError:
    sys.exit(0)
starts = [0]
for ch in src:
    starts.append(starts[-1] + 1)
line_of = []
n = 1
for ch in src:
    line_of.append(n)
    if ch == "\n":
        n += 1
line_of.append(n)
for m in WAIT.finditer(src):
    i = m.end()
    depth = 0
    while i < len(src):
        c = src[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
            if depth < 0:
                break
        elif c == ";" and depth == 0:
            break
        elif c == "." and depth == 0:
            sm = SILENCER.match(src, i)
            if sm:
                print(line_of[m.start()])
                break
        i += 1
'

is_allowlisted() {
    local file="$1" entry
    for entry in ${ALLOWLIST+"${ALLOWLIST[@]}"}; do
        case "$file" in
            "$entry") return 0 ;;
        esac
    done
    return 1
}

# Fails closed rather than reporting OK on an empty scan: `grep -r ... || true`
# over a directory set that does not exist is indistinguishable from a clean
# tree.
require_kernel_dirs() {
    local root="$1" dir
    shift
    for dir in "$@"; do
        [ -d "$root/$dir" ] && return 0
    done
    echo "check_wait_result_handling: none of the kernel directories exist under $root —" >&2
    echo "  the scan would be a no-op, so refusing to report OK." >&2
    exit 2
}

# check 1 — a discard binding on a wait call.
# check 2 — a silencer chained onto a wait result (see SILENCER_SCANNER).
# check 3 — a new `allow`/`expect` of unused_must_use, which would switch the
#           compiler's own half of this off.
scan() {
    local root="$1" dir file lineno present
    shift
    present=()
    for dir in "$@"; do
        [ -d "$root/$dir" ] && present+=("$dir")
    done
    [ ${#present[@]} -eq 0 ] && return 0

    (
        cd "$root"
        grep -rnE "$WAIT_RE" --include='*.rs' "${present[@]}" 2>/dev/null || true
    ) | while IFS=: read -r file lineno _rest; do
        [ -z "${lineno:-}" ] && continue
        case "$file" in
            *_tests.rs | */tests/* | */tests.rs) continue ;;
        esac
        is_allowlisted "$file" && continue

        # On the wait's own line, or as the unterminated head of the
        # expression it belongs to. The `;` rule is what distinguishes the
        # two: a `let _ = something();` above a wait is a complete statement
        # of its own and binds nothing below it.
        local discarded=0
        if sed -n "${lineno}p" "$root/$file" \
            | grep -qE 'let[[:space:]]+_[[:space:]]*='; then
            discarded=1
        elif [ "$lineno" -gt 1 ]; then
            local from=$((lineno - LOOKBACK))
            [ "$from" -lt 1 ] && from=1
            if sed -n "${from},$((lineno - 1))p" "$root/$file" \
                | grep -E 'let[[:space:]]+_[[:space:]]*=' \
                | grep -qvE ';[[:space:]]*$'; then
                discarded=1
            fi
        fi
        if [ "$discarded" -eq 1 ]; then
            printf '1\t%s:%s: a wait result bound to _\n' "$file" "$lineno"
        fi
    done

    # check 2, once per file rather than per hit: the scanner walks the whole
    # source and reports the line of every wait whose result is collapsed.
    (
        cd "$root"
        grep -rlE "$WAIT_RE" --include='*.rs' "${present[@]}" 2>/dev/null || true
    ) | while IFS= read -r file; do
        [ -z "$file" ] && continue
        case "$file" in
            *_tests.rs | */tests/* | */tests.rs) continue ;;
        esac
        is_allowlisted "$file" && continue
        python3 -c "$SILENCER_SCANNER" "$root/$file" | while IFS= read -r lineno; do
            printf '2\t%s:%s: a wait result collapsed by a silencer\n' "$file" "$lineno"
        done
    done

    (
        cd "$root"
        grep -rnE '#!?\[(allow|expect)\((.*,)?[[:space:]]*unused_must_use' \
            --include='*.rs' "${present[@]}" 2>/dev/null || true
    ) | while IFS=: read -r file lineno _rest; do
        [ -z "${lineno:-}" ] && continue
        printf '3\t%s:%s: unused_must_use silenced\n' "$file" "$lineno"
    done
}

run_scan() {
    local root="$1"
    shift
    require_kernel_dirs "$root" "$@"
    scan "$root" "$@"
}

report() {
    local findings="$1"
    echo "check_wait_result_handling: a wait result was discarded or collapsed:" >&2
    printf '%s\n' "$findings" | cut -f2- | sed 's/^/    /' >&2
    echo >&2
    echo '  WaitAbort distinguishes Killed from Interrupted, Timeout and' >&2
    echo '  NoRuntime. A silencer erases that, and the one it erases is the' >&2
    echo '  one a dying task needs: branch on the variant instead.' >&2
    exit 1
}

if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_wait_result_handling

    cat > "$(gate_fixture net/src/discards.rs)" <<'FIXTURE'
fn same_line() {
    let _ = q.wait_event(|| true);
}
fn multi_line() {
    let _ = BUS
        .subscribe(ev)
        .wait_event(|| true);
}
FIXTURE

    cat > "$(gate_fixture net/src/silencers.rs)" <<'FIXTURE'
fn a() { let x = q.wait_event(|| true).ok(); }
fn b() { let x = q.wait_event_timeout(|| true, 1).unwrap_or(()); }
fn c() { let x = q.wait_event_until(|| None).unwrap_or_default(); }
fn d() { let x = q.wait_event_interruptible(|| true).unwrap_or_else(|_| ()); }
FIXTURE

    cat > "$(gate_fixture net/src/escape.rs)" <<'FIXTURE'
#[allow(unused_must_use)]
fn a() {}
#![expect(unused_must_use)]
FIXTURE

    cat > "$(gate_fixture net/src/negatives.rs)" <<'FIXTURE'
fn branches_on_the_variant() {
    match q.wait_event(|| true) {
        Ok(()) => {}
        Err(WaitAbort::Killed) => return,
        Err(_) => {}
    }
}
fn propagates() -> WaitResult<()> {
    q.wait_event(|| true)?;
    Ok(())
}
fn tests_the_outcome() {
    if q.wait_event(|| true).is_err() {
        return;
    }
}
fn a_terminated_discard_far_above_is_not_ours() {
    let _ = unrelated();
    // 1
    // 2
    // 3
    // 4
    // 5
    // 6
    q.wait_event(|| true).map_err(|e| e)?;
}
fn userland_poll_helper_is_not_a_wait_event() {
    let _ = handle.wait_events(timeout_ms);
}
fn a_predicates_own_default_is_not_the_waits() {
    let queued = || {
        table.get(i).map(|s| !s.is_empty()).unwrap_or(true)
    };
    match sub.wait_event_interruptible(queued) {
        Ok(()) => {}
        Err(_) => {}
    }
}
FIXTURE

    cat > "$(gate_fixture net/src/socket_tests.rs)" <<'FIXTURE'
fn t() { let _ = q.wait_event(|| true).ok(); }
FIXTURE

    cat > "$(gate_fixture core/src/tests/bus_tests.rs)" <<'FIXTURE'
fn t() { let _ = q.wait_event(|| true).ok(); }
FIXTURE

    cat > "$(gate_fixture userland/src/x.rs)" <<'FIXTURE'
fn outside_the_kernel_dirs() { let _ = q.wait_event(|| true).ok(); }
FIXTURE

    GATE_FINDINGS="$(run_scan "$GATE_FIXTURE_ROOT" "${KERNEL_DIRS[@]}")"

    gate_expect 1 2 "a same-line discard and one three lines up"
    gate_expect 2 4 "ok, unwrap_or, unwrap_or_default and unwrap_or_else"
    gate_expect 3 2 "an allow and an expect of unused_must_use"
    gate_expect_silent 'negatives\.rs|_tests\.rs|/tests/|userland/' \
        "an exhaustive match, a ?, an is_err branch, a discard seven lines up, \
wait_events(), a predicate's own unwrap_or, both test-file spellings, and a \
directory outside the kernel set"

    empty_root="$(mktemp -d)"
    set +e
    (run_scan "$empty_root" "${KERNEL_DIRS[@]}") >/dev/null 2>&1
    status=$?
    set -e
    rm -rf "$empty_root"
    if [ "$status" -ne 2 ]; then
        echo "check_wait_result_handling --self-test: an empty root exited $status, want 2" >&2
        GATE_SELF_TEST_FAIL=1
    else
        echo "  an empty scan root fails closed: ok"
    fi

    gate_selftest_end
fi

FINDINGS="$(run_scan "$REPO_ROOT" "${KERNEL_DIRS[@]}")"
if [ -n "$FINDINGS" ]; then
    report "$FINDINGS"
fi
echo "check_wait_result_handling: OK — no wait result is discarded or collapsed"
