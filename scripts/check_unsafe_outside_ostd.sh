#!/usr/bin/env bash
# Fail the build if any kernel crate other than slopos-ostd contains a
# bare `unsafe` block or `unsafe fn`. slopos-ostd is the kernel's
# Operating System Trusted Domain — the single crate that owns every
# line of `unsafe` in the kernel (Asterinas-paper AD-1 / AD-2). Every
# other kernel crate must compile under `#![forbid(unsafe_code)]`; the
# `forbid` attribute already rejects new `unsafe` at rustc time, but
# this script is the load-bearing belt-and-braces gate that catches:
#
#   - `#[allow(unsafe_code)]` exemptions slipping in,
#   - the (very rare) macros that bypass `forbid` via attribute-form
#     `unsafe`,
#   - new crates added to the workspace that forget the lint attribute.
#
# The trusted-core carve-outs:
#   - slopos-ostd/                  the OSTD itself.
#   - slopos-ostd-derive/           proc-macro support; emits literal
#                                   `unsafe impl Trait for T {}` token
#                                   text consumed inside OSTD. Listed
#                                   here so future drift (a new unsafe
#                                   block added elsewhere in the crate)
#                                   does *not* trip the gate, while
#                                   keeping the proc-macro's existing
#                                   output strings allowed.
#   - kernel/src/main.rs            global allocator + alloc error
#                                   handler declarations (must name
#                                   `alloc` directly; same exemption
#                                   pattern as check_alloc_dep.sh).
#   - hermetic/src/macros.rs        `macro_rules!` body containing the
#                                   Edition-2024 `#[unsafe(link_section
#                                   = "...")]` attribute used at
#                                   expansion sites elsewhere. The
#                                   keyword is required by the
#                                   attribute grammar, not a runtime
#                                   unsafe block. Allowlisted by file
#                                   so a *new* unsafe block added to
#                                   the same file still fails the gate.
#
# Userland-side crates (userland, slibc, slop-protocol, ktesting, appkit)
# are out of scope per Phase-1 plan § A.
#
# Comment-line and `#[cfg(...)]`-gated occurrences are skipped using the
# same lookback pattern as scripts/check_alloc_dep.sh so cfg-stubs that
# compile out of the kernel build are accepted.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Crate-name allowlist — everything userland-side, plus the trusted
# core and its proc-macro support. Matches the leading directory
# component of the path (relative to REPO_ROOT).
# slopos-rt = the userland async runtime; userland-side, identical role to
# userland/appkit which are already exempt and already carry unsafe.
USERLAND_RE='^(userland|slibc|slop-protocol|ktesting|appkit|image|slopos-rt|slopos-ostd|slopos-ostd-derive)/'

# Explicit file-level allowlist. Each entry is a repo-relative path.
SOURCE_WHITELIST=(
    "kernel/src/main.rs"
    "hermetic/src/macros.rs"
)

# git ls-files respects .gitignore and skips third_party / builddir /
# target. Mirror the find-fallback from check_alloc_dep.sh for environments
# without git.
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

# Filter out userland-side crates and explicit-file exemptions. Built
# imperatively rather than with a piped grep-of-greps because the
# nested-pipe form silently drops stdin in some shell versions and
# would no-op the gate.
filtered=""
while IFS= read -r path; do
    [ -z "$path" ] && continue
    [[ "$path" =~ $USERLAND_RE ]] && continue
    skip=0
    for exempt in "${SOURCE_WHITELIST[@]}"; do
        if [ "$path" = "$exempt" ]; then
            skip=1
            break
        fi
    done
    [ "$skip" -eq 1 ] && continue
    filtered+="$path"$'\n'
done <<< "$file_list"

# awk pass per file: flag any `unsafe`-keyword line that is not a
# comment and is not preceded by a `#[cfg(...)]` attribute (direct or
# via an enclosing `mod ... {` declaration two lines back).
source_offenders="$(
    cd "$REPO_ROOT"
    printf '%s' "$filtered" | while IFS= read -r file; do
        [ -z "$file" ] && continue
        [ -f "$file" ] || continue
        awk -v fname="$file" '
            BEGIN { n = 0 }
            {
                lines[NR] = $0
                if (n < NR) n = NR
            }
            # Skip pure line/block comments. Lines starting with bare
            # `*` are Rust dereference syntax, not a block-comment
            # continuation, so they are NOT skipped.
            /^[[:space:]]*(\/\/|\/\*)/ { next }
            # Skip the Edition-2024 attribute form `#[unsafe(...)]` —
            # it is not an unsafe block. Any *real* unsafe block on the
            # same line would still trip the regex below because the
            # word appears outside the attribute parens.
            #
            # awk does not support `\b`; emulate word boundaries with
            # an explicit non-word-character class on both sides
            # (`(^|[^A-Za-z0-9_])unsafe([^A-Za-z0-9_]|$)`), which
            # correctly rejects identifiers like `_unsafe_handle` while
            # still catching the keyword in any indentation.
            {
                stripped = $0
                gsub(/#\[unsafe\([^)]*\)\]/, "", stripped)
                if (stripped !~ /(^|[^A-Za-z0-9_])unsafe([^A-Za-z0-9_]|$)/) next
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

if [ -n "$source_offenders" ]; then
    echo "check_unsafe_outside_ostd: kernel-side 'unsafe' detected outside slopos-ostd:" >&2
    echo "$source_offenders" | sed 's/^/    /' >&2
    echo "  slopos-ostd is the only crate allowed to use unsafe." >&2
    echo "  If a new file legitimately needs an unsafe attribute (e.g. #[unsafe(link_section)] in a" >&2
    echo "  macro_rules! body) and you have audited it, add the file to SOURCE_WHITELIST in this script." >&2
    exit 1
fi

echo "check_unsafe_outside_ostd: OK — kernel crates outside slopos-ostd contain no executable unsafe"
