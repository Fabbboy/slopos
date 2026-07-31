#!/usr/bin/env bash
# Fail if kernel-side Drop implementations contain direct panic triggers.
#
# Destructors run in cleanup paths and may execute during unwinding. A panic
# from Drop can turn a recoverable unwind into an abort or mask the original
# fault. This gate is intentionally syntactic: it catches direct panic/assert
# macros and obvious unwrap/expect calls inside `fn drop`.
#
# Existing reviewed exception:
#   slopos-ostd/src/cpu/preempt.rs: PreemptGuard::drop keeps its always-on
#   `preempt_count underflow` invariant assert. This script prevents that
#   single checked invariant from expanding into a broader Drop-panic surface.
#   drivers/src/tty/mod.rs and fs/src/ext2/inode.rs keep debug-only Drop
#   assertions that catch missing explicit cleanup during development.
#   boot/src/early_init.rs: the `panic.nested_drop_smoke` boot hook's guard
#   Drop panics BY DESIGN — it is the fault injection that proves a Drop
#   panic mid-unwind lands on the fatal path.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_drop_panic_free "$@"

# Userland crates are out of the kernel framekernel discipline. The pinned
# TCB annexes are covered by scripts/check_vendor_pin.sh instead of this
# first-party destructor-policy scan.
OUT_OF_SCOPE_RE='^(userland|terminal-core|slibc|slop-protocol|appkit|image|slopos-rt|vendor/unwinding|vendor/gimli)/'

filter_files() {
    local path
    while IFS= read -r path; do
        [ -z "$path" ] && continue
        [[ "$path" =~ $OUT_OF_SCOPE_RE ]] && continue
        printf '%s\n' "$path"
    done
}

# Findings carry a `<tag>\t` prefix so the self-test can count the check;
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
            function scan_line(line) {
                sub(/\/\/.*/, "", line)
                return line
            }
            function forbidden(line) {
                return line ~ /(^|[^A-Za-z0-9_])(panic|todo|unimplemented|unreachable|assert|assert_eq|assert_ne|debug_assert|debug_assert_eq|debug_assert_ne)![[:space:]]*\(/ \
                    || line ~ /(^|[^A-Za-z0-9_])(panic|todo|unimplemented|unreachable|assert|assert_eq|assert_ne|debug_assert|debug_assert_eq|debug_assert_ne)![[:space:]]*\{/ \
                    || line ~ /(^|[^A-Za-z0-9_])(panic|todo|unimplemented|unreachable|assert|assert_eq|assert_ne|debug_assert|debug_assert_eq|debug_assert_ne)![[:space:]]*\[/ \
                    || line ~ /\.(unwrap|expect)(_err)?[[:space:]]*\(/
            }
            function reviewed_exception(line) {
                return fname == "slopos-ostd/src/cpu/preempt.rs" \
                    && line ~ /preempt_count underflow/ \
                    || fname == "drivers/src/tty/mod.rs" \
                    && line ~ /^[[:space:]]*debug_assert![[:space:]]*\(/ \
                    || fname == "fs/src/ext2/inode.rs" \
                    && line ~ /^[[:space:]]*debug_assert![[:space:]]*\(/ \
                    || fname == "boot/src/early_init.rs" \
                    && line ~ /panic\.nested_drop_smoke: Drop panic during unwind/
            }
            function count_braces(line,    i, c) {
                for (i = 1; i <= length(line); i++) {
                    c = substr(line, i, 1)
                    if (c == "{") depth++
                    else if (c == "}") depth--
                }
            }

            /^[[:space:]]*(\/\/|\/\*)/ { next }

            !in_drop {
                if ($0 ~ /^[[:space:]]*impl([^A-Za-z0-9_]|<|$).*Drop[[:space:]]+for[[:space:]]/) {
                    in_impl = 1
                }
                if (in_impl && $0 ~ /fn[[:space:]]+drop[[:space:]]*\(/) {
                    in_drop = 1
                    depth = 0
                }
            }

            in_drop {
                stripped = scan_line($0)
                if (forbidden(stripped) && !reviewed_exception(stripped)) {
                    printf "1\t%s:%d: %s\n", fname, NR, $0
                }
                count_braces(stripped)
                if (depth <= 0 && stripped ~ /}/) {
                    in_drop = 0
                    in_impl = 0
                    depth = 0
                }
            }
        ' "$file"
    done
}

run_scan() {
    local root="$1"
    shift
    (scan_sources "$root" "$@")
}

# ---------------------------------------------------------------------------
# Self-test. The reviewed exceptions compare against the repo-relative path
# inside the awk program, so those fixtures live at exactly those paths —
# making the fixture tree a literal transcript of the exception list.
# ---------------------------------------------------------------------------
if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_drop_panic_free

    cat > "$(gate_fixture mm/src/positives.rs)" <<'FIXTURE'
impl Drop for Foo {
    fn drop(&mut self) {
        panic!("boom");
        assert!(self.ok);
        debug_assert_eq!(self.a, self.b);
        let _ = self.slot.unwrap();
        let _ = self.slot.expect("gone");
        let _ = self.res.unwrap_err();
        todo!();
        unreachable!();
    }
}
impl<T> Drop for Bar<T> {
    fn drop(&mut self) {
        panic!("generic impl header must still enter the state machine");
    }
}
FIXTURE

    # The brace counter is the load-bearing part: broken, it turns every
    # later panic in a file into a false positive.
    cat > "$(gate_fixture mm/src/negatives.rs)" <<'FIXTURE'
fn before_any_drop_impl() {
    panic!("not a destructor");
    let _ = opt.unwrap();
}
impl Drop for Quiet {
    fn drop(&mut self) {
        // panic!("commented out")
        self.release();
    }
}
fn after_the_drop_body_closed() {
    panic!("the brace counter must have left the destructor");
    let _ = opt.expect("still not a destructor");
}
FIXTURE

    # Line-scoped: each exception excuses one assertion, not the file.
    cat > "$(gate_fixture slopos-ostd/src/cpu/preempt.rs)" <<'FIXTURE'
impl Drop for PreemptGuard {
    fn drop(&mut self) {
        assert!(self.count > 0, "preempt_count underflow");
        panic!("a second, unreviewed panic in the same destructor");
    }
}
FIXTURE
    cat > "$(gate_fixture drivers/src/tty/mod.rs)" <<'FIXTURE'
impl Drop for Tty {
    fn drop(&mut self) {
        debug_assert!(self.drained);
        assert!(self.drained);
    }
}
FIXTURE
    cat > "$(gate_fixture fs/src/ext2/inode.rs)" <<'FIXTURE'
impl Drop for Inode {
    fn drop(&mut self) {
        debug_assert!(self.flushed);
    }
}
FIXTURE
    cat > "$(gate_fixture boot/src/early_init.rs)" <<'FIXTURE'
impl Drop for NestedDropSmoke {
    fn drop(&mut self) {
        panic!("panic.nested_drop_smoke: Drop panic during unwind");
    }
}
FIXTURE

    cat > "$(gate_fixture userland/src/x.rs)" <<'FIXTURE'
impl Drop for U {
    fn drop(&mut self) { panic!("userland is out of scope"); }
}
FIXTURE
    cat > "$(gate_fixture vendor/unwinding/src/x.rs)" <<'FIXTURE'
impl Drop for A {
    fn drop(&mut self) { panic!("pinned annex, covered by check_vendor_pin"); }
}
FIXTURE
    cat > "$(gate_fixture vendor/othercrate/src/x.rs)" <<'FIXTURE'
impl Drop for V {
    fn drop(&mut self) { panic!("a non-annex vendor crate is in scope"); }
}
FIXTURE

    fixture_files="$(gate_collect_rs_files "$GATE_FIXTURE_ROOT")"
    gate_expect_enumerator "$GATE_FIXTURE_ROOT" "$fixture_files"

    scanned="$(printf '%s\n' "$fixture_files" | filter_files)"
    GATE_FINDINGS="$(run_scan "$GATE_FIXTURE_ROOT" $scanned)"

    # 9 in positives.rs, 1 unreviewed panic beside the preempt exception,
    # 1 non-debug assert beside the tty exception, 1 non-annex vendor crate.
    gate_expect 1 12 "8 trigger forms, a generic impl header, and the unreviewed line beside each reviewed exception"
    gate_expect_silent 'negatives\.rs|userland/|vendor/unwinding/' \
        "a panic outside any destructor, a commented one, a panic after the drop body closes, userland, and the pinned annex all stay silent"

    gate_selftest_end
fi

# ---------------------------------------------------------------------------
# Real run
# ---------------------------------------------------------------------------
file_list="$(gate_collect_rs_files "$REPO_ROOT")"
gate_require_nonempty check_drop_panic_free "$REPO_ROOT" "$file_list"
filtered="$(printf '%s\n' "$file_list" | filter_files)"

offenders="$(run_scan "$REPO_ROOT" $filtered | cut -f2-)"

if [ -n "$offenders" ]; then
    echo "check_drop_panic_free: panic-capable operation found inside Drop:" >&2
    echo "$offenders" | sed 's/^/    /' >&2
    echo "  Drop implementations must not panic; return errors before ownership reaches Drop or make cleanup best-effort." >&2
    exit 1
fi

echo "check_drop_panic_free: OK — kernel-side Drop implementations contain no unreviewed panic triggers"
