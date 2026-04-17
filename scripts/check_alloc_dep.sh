#!/usr/bin/env bash
# Fail the build if any kernel crate other than slopos-alloc declares a
# bare `alloc` dependency in its Cargo.toml.
#
# Kernel crates must route heap access through `slopos-alloc`, which is
# the only kernel crate permitted to depend on `alloc` directly. Userland
# crates are exempt — they run on larger stacks and the constraint does
# not apply there.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Crate names allowed to own an `alloc` dep line. Userland runs on big
# stacks; slopos-alloc is the sanctioned allocation surface.
USERLAND_RE='^(userland|slibc|slop-protocol|ktesting|slopos-alloc)$'

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
        echo "  kernel crates must route heap allocation through slopos-alloc instead" >&2
        bad=1
    fi
done < <(find "$REPO_ROOT" -maxdepth 3 -name Cargo.toml \
             -not -path "$REPO_ROOT/builddir/*" \
             -not -path "$REPO_ROOT/third_party/*" \
             -not -path "$REPO_ROOT/target/*" \
             -print0)

if [ "$bad" -eq 0 ]; then
    echo "check_alloc_dep: OK — no kernel crate declares a direct 'alloc' dep"
fi
exit "$bad"
