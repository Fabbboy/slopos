#!/usr/bin/env bash
# Fail the build if any kernel crate other than slopos-ostd declares a
# bare `alloc` dependency in its Cargo.toml.
#
# Kernel crates must route heap access through `slopos_ostd::mm::heap`,
# which is the only kernel module permitted to depend on `alloc`
# directly. Userland crates are exempt — they run on larger stacks and
# the constraint does not apply there.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Crate names allowed to own an `alloc` dep line. Userland runs on big
# stacks; slopos-ostd is the sanctioned allocation surface.
USERLAND_RE='^(userland|terminal-core|slibc|slop-protocol|ktesting|slopos-ostd)$'

bad=0
while IFS= read -r -d '' manifest; do
    crate_dir="$(dirname "$manifest")"
    rel_dir="${crate_dir#"$REPO_ROOT/"}"
    crate_name="$(basename "$crate_dir")"

    # Skip the workspace root itself.
    if [ "$manifest" = "$REPO_ROOT/Cargo.toml" ]; then
        continue
    fi
    # Skip third_party, build outputs, and the userland carve-out.
    case "$rel_dir" in
        third_party/*|builddir/*|target/*|*/target/*) continue ;;
    esac
    if [[ "$crate_name" =~ $USERLAND_RE ]]; then
        continue
    fi

    # Section-aware scan: only flag bare `alloc = ...` / `alloc.workspace`
    # entries inside `[dependencies]`, `[dev-dependencies]`,
    # `[build-dependencies]`, or `[target.*.*dependencies]` sections.
    # `[features]` stanzas legitimately use `alloc = [...]` to forward a
    # feature flag and are ignored.
    match="$(awk '
        /^[[:space:]]*\[/ {
            section = $0
            sub(/^[[:space:]]*\[/, "", section)
            sub(/\].*$/, "", section)
            next
        }
        /^[[:space:]]*alloc[[:space:]]*(=|\.workspace)/ {
            if (section == "dependencies" \
                || section == "dev-dependencies" \
                || section == "build-dependencies" \
                || section ~ /^target\.[^.]+\.(dev-|build-)?dependencies$/) {
                printf "%d: %s\n", NR, $0
            }
        }
    ' "$manifest")"
    if [ -n "$match" ]; then
        echo "check_alloc_dep: $manifest declares an 'alloc' dependency:" >&2
        echo "$match" | sed 's/^/    /' >&2
        echo "  kernel crates must route heap allocation through slopos_ostd::mm::heap instead" >&2
        bad=1
    fi
done < <(find "$REPO_ROOT" -maxdepth 3 -name Cargo.toml \
             -not -path "$REPO_ROOT/builddir/*" \
             -not -path "$REPO_ROOT/third_party/*" \
             -not -path "$REPO_ROOT/target/*" \
             -print0)

# -----------------------------------------------------------------------
# Source-level pass: catch `extern crate alloc;` / `use alloc::` / `use
# ::alloc::` patterns that a Cargo.toml scan would miss. The only kernel
# file allowed to name `alloc` from source is `kernel/src/main.rs`,
# which needs `extern crate alloc;` for its `#[global_allocator]` /
# `#[alloc_error_handler]` declarations. Userland + slopos-ostd itself
# are exempt per the Cargo-level whitelist above.
# -----------------------------------------------------------------------

SOURCE_WHITELIST="kernel/src/main.rs"

# A minimal awk scan of each `.rs` that flags `extern crate alloc;` or
# `use alloc::` / `use ::alloc::` lines *only* if the preceding line is
# not a `#[cfg(...)]` attribute. The cfg-gate cases live in `gfx/` and
# compile out of the kernel build; they're safe and we don't want to
# touch them.
#
# Both `use alloc::` and `use ::alloc::` (path-absolute form) are
# matched by the regex below.
#
# `git ls-files` respects `.gitignore` and skips third_party / builddir
# / target automatically.
source_offenders="$(
    cd "$REPO_ROOT"
    if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        git ls-files '*.rs'
    else
        find . -type f -name '*.rs' \
            -not -path './builddir/*' \
            -not -path './third_party/*' \
            -not -path './target/*'
    fi \
      | grep -Ev '^(userland|terminal-core|slibc|slop-protocol|ktesting|slopos-ostd)/' \
      | grep -vxF "$SOURCE_WHITELIST" \
      | while IFS= read -r file; do
            awk '
                BEGIN { bad = 0; n = 0 }
                {
                    lines[NR] = $0
                    if (n < NR) n = NR
                }
                /^[[:space:]]*(extern crate alloc;|use alloc::|use ::alloc::)/ {
                    # Accept if line N-1 is `#[cfg(...)]` (applies to
                    # this line directly) OR line N-2 is `#[cfg(...)]`
                    # AND line N-1 is a `mod ... {` declaration (the
                    # cfg then gates the enclosing mod block, making
                    # the inner `extern crate alloc;` safe).
                    gated = 0
                    if (NR - 1 >= 1 && lines[NR - 1] ~ /^[[:space:]]*#\[cfg\(/) {
                        gated = 1
                    } else if (NR - 2 >= 1 \
                               && lines[NR - 2] ~ /^[[:space:]]*#\[cfg\(/ \
                               && lines[NR - 1] ~ /mod[[:space:]]+[A-Za-z0-9_]+[[:space:]]*\{/) {
                        gated = 1
                    }
                    if (!gated) bad = 1
                }
                END { if (bad) exit 1 }
            ' "$file" || echo "$file"
        done \
      || true
)"

if [ -n "$source_offenders" ]; then
    echo "check_alloc_dep: source-level 'alloc' usage detected:" >&2
    echo "$source_offenders" | sed 's/^/    /' >&2
    echo "  only kernel/src/main.rs and slopos-ostd may name 'alloc' directly — everything" >&2
    echo "  else must go through slopos_ostd::*" >&2
    bad=1
fi

if [ "$bad" -eq 0 ]; then
    echo "check_alloc_dep: OK — no kernel crate or source file outside slopos-ostd names 'alloc' directly"
fi
exit "$bad"
