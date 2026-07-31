#!/usr/bin/env bash
# Fail the build if any kernel crate contains an `async fn` (or an
# `async` block / `async move`). SlopOS is a *sync core, async edge*
# framekernel: the kernel — OSTD and every service crate, including the
# future io_uring-style `ring/` crate — is synchronous; async lives
# entirely in userspace on top of the ring surface (AD-8 / AD-9).
#
# This is the async sibling of scripts/check_unsafe_outside_ostd.sh: the
# discipline is load-bearing, so it is enforced in CI rather than left to
# code review. The `forbid(unsafe_code)` lint has no `async`-forbidding
# equivalent, so this script is the gate.
#
# Userland-side crates (userland, slibc, slop-protocol, appkit)
# are *out of scope* — userland async is the whole point of the ring edge.
#
# Comment lines and `#[cfg(...)]`-gated occurrences are skipped using the
# same lookback pattern as scripts/check_unsafe_outside_ostd.sh so that
# cfg-stubs compiled out of the kernel build are accepted.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_no_kernel_async "$@"

# Userland-side crates are exempt (their whole job is to host async).
# slopos-rt = the userland async runtime; userland-side, identical role to
# userland/appkit which are already exempt.
USERLAND_RE='^(userland|slibc|slop-protocol|appkit|image|slopos-rt|verification)/'
TCB_ANNEX_RE='^vendor/(unwinding|gimli)/'

filter_files() {
    local path
    while IFS= read -r path; do
        [ -z "$path" ] && continue
        [[ "$path" =~ $USERLAND_RE ]] && continue
        # Named TCB annexes. Other vendor crates are deliberately scanned.
        [[ "$path" =~ $TCB_ANNEX_RE ]] && continue
        printf '%s\n' "$path"
    done
}

# Flag any line introducing async in a kernel crate:
#   - `async fn ...`
#   - `async {` / `async move {` blocks
# while skipping comment lines and `#[cfg(...)]`-gated lines.
#
# Findings carry a `<tag>\t` prefix so the self-test can count each check;
# the report strips it back off.
scan_sources() {
    local root="$1"
    shift
    cd "$root"
    local file
    for file in "$@"; do
        [ -z "$file" ] && continue
        [ -f "$file" ] || continue
        awk -v fname="$file" '
            BEGIN { n = 0 }
            {
                lines[NR] = $0
                if (n < NR) n = NR
            }
            # Skip pure line/block comments.
            /^[[:space:]]*(\/\/|\/\*|\*)/ { next }
            # Match a real async *construct*, not the bare word: the
            # keyword must be followed by `fn`, `move`, `gen`, an opening
            # brace, or a closure pipe. This catches `async fn`, `pub
            # async fn`, `async move {`, `async {`, `async || ...`, while
            # ignoring rejections-of-async like `.asyncness.is_some()` or
            # an error string that merely mentions `async` in backticks.
            {
                if ($0 !~ /(^|[^A-Za-z0-9_])async[[:space:]]+(fn|move|gen)([^A-Za-z0-9_]|$)/ \
                   && $0 !~ /(^|[^A-Za-z0-9_])async[[:space:]]*[{|]/) next
            }
            {
                gated = 0
                if (NR - 1 >= 1 && lines[NR - 1] ~ /^[[:space:]]*#\[cfg\(/) {
                    gated = 1
                } else if (NR - 2 >= 1 \
                           && lines[NR - 2] ~ /^[[:space:]]*#\[cfg\(/ \
                           && lines[NR - 1] ~ /mod[[:space:]]+[A-Za-z0-9_]+[[:space:]]*\{/) {
                    gated = 1
                }
                if (!gated) {
                    printf "1\t%s:%d: %s\n", fname, NR, $0
                }
            }
        ' "$file" || true
    done
}

run_scan() {
    local root="$1"
    shift
    (scan_sources "$root" "$@")
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------
if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_no_kernel_async

    cat > "$(gate_fixture sched/src/positives.rs)" <<'FIXTURE'
async fn a() {}
pub async fn b() {}
fn c() { let _f = async move { 1 }; }
fn d() { let _g = async { 1 }; }
fn e() { let _h = async || {}; }
async gen fn f() {}
FIXTURE

    cat > "$(gate_fixture vendor/othercrate/src/lib.rs)" <<'FIXTURE'
async fn vendored() {}
FIXTURE

    # Forms the gate deliberately accepts. Each is a live regression risk:
    # awk has no `\b`, so the word-boundary emulation is hand-rolled.
    cat > "$(gate_fixture sched/src/negatives.rs)" <<'FIXTURE'
// async fn commented() {}
/* async fn block_commented() {} */
 * async fn continuation() {}
fn asyncness_probe() { if sig.asyncness.is_some() { } }
fn message() { klog_info!("no `async` here"); }
fn ident() { let asynchronous = 1; let _ = asynchronous; }
#[cfg(feature = "host")]
async fn cfg_gated() {}
#[cfg(test)]
mod tests {
    async fn inside_gated_mod() {}
}
FIXTURE

    cat > "$(gate_fixture userland/src/lib.rs)" <<'FIXTURE'
async fn userland_is_the_point() {}
FIXTURE
    cat > "$(gate_fixture verification/proofs/x.rs)" <<'FIXTURE'
async fn proof_harness() {}
FIXTURE
    cat > "$(gate_fixture vendor/gimli/src/x.rs)" <<'FIXTURE'
async fn annex() {}
FIXTURE

    fixture_files="$(gate_collect_rs_files "$GATE_FIXTURE_ROOT")"
    gate_expect_enumerator "$GATE_FIXTURE_ROOT" "$fixture_files"

    # The filter decides scope: a widened regex would exempt kernel crates.
    scanned="$(printf '%s\n' "$fixture_files" | filter_files)"
    GATE_FINDINGS="$(run_scan "$GATE_FIXTURE_ROOT" $scanned)"

    gate_expect 1 7 "async fn, pub async fn, async move, async block, async closure, async gen, and a non-annex vendor crate"
    gate_expect_silent 'negatives\.rs|userland/|verification/|vendor/gimli/' \
        "comments, .asyncness, a backticked mention, an identifier, both cfg lookbacks, userland, verification, and the named TCB annexes all stay silent"

    gate_selftest_end
fi

# ---------------------------------------------------------------------------
# Real run
# ---------------------------------------------------------------------------
file_list="$(gate_collect_rs_files "$REPO_ROOT")"
gate_require_nonempty check_no_kernel_async "$REPO_ROOT" "$file_list"
filtered="$(printf '%s\n' "$file_list" | filter_files)"

offenders="$(run_scan "$REPO_ROOT" $filtered | cut -f2-)"

if [ -n "$offenders" ]; then
    echo "check_no_kernel_async: 'async' detected in a kernel crate:" >&2
    echo "$offenders" | sed 's/^/    /' >&2
    echo "  SlopOS is sync core, async edge (AD-8 / AD-9). No kernel crate —" >&2
    echo "  OSTD, services, or the ring/ crate — may contain async. Async lives" >&2
    echo "  in userspace on top of the ring surface. Move the async to userland." >&2
    exit 1
fi

echo "check_no_kernel_async: OK — no kernel crate contains async (sync core, async edge)"
