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

# Userland crates are out of the kernel framekernel discipline. The pinned
# TCB annexes are covered by scripts/check_vendor_pin.sh instead of this
# first-party destructor-policy scan.
OUT_OF_SCOPE_RE='^(userland|terminal-core|slibc|slop-protocol|appkit|image|slopos-rt|vendor/unwinding|vendor/gimli)/'

file_list="$(
    cd "$REPO_ROOT"
    {
        if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
            git ls-files '*.rs'
            git ls-files --others --exclude-standard '*.rs'
        else
            find . -type f -name '*.rs' \
                -not -path './builddir/*' \
                -not -path './third_party/*' \
                -not -path './target/*'
        fi
        find vendor -type f -name '*.rs' 2>/dev/null || true
    } | sed 's|^\./||' | LC_ALL=C sort -u
)"

offenders="$(
    cd "$REPO_ROOT"
    while IFS= read -r file; do
        [ -z "$file" ] && continue
        [[ "$file" =~ $OUT_OF_SCOPE_RE ]] && continue
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
                    printf "%s:%d: %s\n", fname, NR, $0
                }
                count_braces(stripped)
                if (depth <= 0 && stripped ~ /}/) {
                    in_drop = 0
                    in_impl = 0
                    depth = 0
                }
            }
        ' "$file"
    done <<< "$file_list"
)"

if [ -n "$offenders" ]; then
    echo "check_drop_panic_free: panic-capable operation found inside Drop:" >&2
    echo "$offenders" | sed 's/^/    /' >&2
    echo "  Drop implementations must not panic; return errors before ownership reaches Drop or make cleanup best-effort." >&2
    exit 1
fi

echo "check_drop_panic_free: OK — kernel-side Drop implementations contain no unreviewed panic triggers"
