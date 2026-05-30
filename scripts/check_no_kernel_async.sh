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
# Userland-side crates (userland, slibc, slop-protocol, ktesting, appkit)
# are *out of scope* — userland async is the whole point of the ring edge.
#
# Comment lines and `#[cfg(...)]`-gated occurrences are skipped using the
# same lookback pattern as scripts/check_unsafe_outside_ostd.sh so that
# cfg-stubs compiled out of the kernel build are accepted.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Userland-side crates are exempt (their whole job is to host async).
USERLAND_RE='^(userland|slibc|slop-protocol|ktesting|appkit|verification)/'

file_list="$(
    cd "$REPO_ROOT"
    if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        git ls-files '*.rs'
    else
        find . -type f -name '*.rs' \
            -not -path './builddir/*' \
            -not -path './third_party/*' \
            -not -path './target/*' \
          | sed 's|^\./||'
    fi
)"

filtered=""
while IFS= read -r path; do
    [ -z "$path" ] && continue
    [[ "$path" =~ $USERLAND_RE ]] && continue
    filtered+="$path"$'\n'
done <<< "$file_list"

# Flag any line introducing async in a kernel crate:
#   - `async fn ...`
#   - `async {` / `async move {` blocks
# while skipping comment lines and `#[cfg(...)]`-gated lines.
offenders="$(
    cd "$REPO_ROOT"
    printf '%s' "$filtered" | while IFS= read -r file; do
        [ -z "$file" ] && continue
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
                    printf "%s:%d: %s\n", fname, NR, $0
                }
            }
        ' "$file" || true
    done
)"

if [ -n "$offenders" ]; then
    echo "check_no_kernel_async: 'async' detected in a kernel crate:" >&2
    echo "$offenders" | sed 's/^/    /' >&2
    echo "  SlopOS is sync core, async edge (AD-8 / AD-9). No kernel crate —" >&2
    echo "  OSTD, services, or the ring/ crate — may contain async. Async lives" >&2
    echo "  in userspace on top of the ring surface. Move the async to userland." >&2
    exit 1
fi

echo "check_no_kernel_async: OK — no kernel crate contains async (sync core, async edge)"
