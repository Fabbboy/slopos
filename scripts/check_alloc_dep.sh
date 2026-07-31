#!/usr/bin/env bash
# Fail the build if any kernel crate other than slopos-ostd declares a
# bare `alloc` dependency in its Cargo.toml.
#
# Kernel crates must route heap access through `slopos_ostd::mm::heap`,
# which is the only kernel module permitted to depend on `alloc`
# directly. Userland crates are exempt — they run on larger stacks and
# the constraint does not apply there.
#
# vendor/unwinding and vendor/gimli are named TCB annexes. They are
# third-party code pinned by scripts/check_vendor_pin.sh and reached only
# through OSTD's unwind surface, so this allocation gate skips those
# directories only. Other vendor crates are scanned like first-party
# kernel code.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_alloc_dep "$@"

# Crate names allowed to own an `alloc` dep line. Userland runs on big
# stacks; slopos-ostd is the sanctioned allocation surface.
USERLAND_RE='^(userland|terminal-core|slibc|slop-protocol|image|slopos-ostd)$'

# Findings carry a `<tag>\t` prefix so the self-test can count each pass
# independently; the reports strip it back off.
scan_manifests() {
    local root="$1"
    local manifest crate_dir rel_dir crate_name match
    while IFS= read -r -d '' manifest; do
    crate_dir="$(dirname "$manifest")"
    rel_dir="${crate_dir#"$root/"}"
    crate_name="$(basename "$crate_dir")"

    # Skip the workspace root itself.
    if [ "$manifest" = "$root/Cargo.toml" ]; then
        continue
    fi
    # Skip third_party, build outputs, and the userland carve-out.
    # The named vendor TCB annexes are the only vendored crates allowed to
    # live outside the framekernel allocation discipline.
    case "$rel_dir" in
        third_party/*|vendor/unwinding|vendor/unwinding/*|vendor/gimli|vendor/gimli/*|builddir/*|target/*|*/target/*) continue ;;
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
        # One finding per offending line, not per manifest: two alloc deps in
        # one file are two violations, and a count that collapses them makes
        # the self-test's exact-count assertion meaningless.
        printf '%s\n' "$match" | while IFS= read -r hit; do
            [ -z "$hit" ] && continue
            printf '1\t%s:%s\n' "$rel_dir/Cargo.toml" "$hit"
        done
    fi
    done < <(find "$root" -maxdepth 3 -name Cargo.toml \
                 -not -path "$root/builddir/*" \
                 -not -path "$root/third_party/*" \
                 -not -path "$root/target/*" \
                 -print0)
}

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
# The source scan includes untracked files and an explicit vendor sweep.
# Only the named TCB annexes are skipped; any other vendored Rust source
# that directly names alloc is a gate failure.
filter_files() {
    grep -Ev '^(userland|terminal-core|slibc|slop-protocol|image|slopos-ostd)/' \
      | grep -Ev '^vendor/(unwinding|gimli)/' \
      | grep -vxF "$SOURCE_WHITELIST" \
      || true
}

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
                    if (!gated) printf "2\t%s:%d: %s\n", fname, NR, $0
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
    gate_selftest_begin check_alloc_dep

    # Manifest positives. `net/sub` reproduces the real nesting depth the
    # `-maxdepth 3` walk has to reach, and the target section is spelled with
    # a triple rather than a quoted cfg: the section regex uses `[^.]+`, so a
    # `[target.'cfg(unix)'.dependencies]` header would not match. The triple
    # form is the one this tree uses.
    cat > "$(gate_fixture mm/Cargo.toml)" <<'FIXTURE'
[package]
name = "slopos-mm"

[dependencies]
alloc = "1"

[target.x86_64-slos.dependencies]
alloc = "1"
FIXTURE
    cat > "$(gate_fixture sched/Cargo.toml)" <<'FIXTURE'
[dependencies]
alloc.workspace = true
FIXTURE
    cat > "$(gate_fixture net/sub/Cargo.toml)" <<'FIXTURE'
[dev-dependencies]
alloc = "1"
FIXTURE

    # Manifest negatives.
    cat > "$(gate_fixture Cargo.toml)" <<'FIXTURE'
[workspace]
members = ["mm"]

[dependencies]
alloc = "1"
FIXTURE
    cat > "$(gate_fixture gfx/Cargo.toml)" <<'FIXTURE'
[features]
alloc = ["dep:foo"]
FIXTURE
    cat > "$(gate_fixture userland/Cargo.toml)" <<'FIXTURE'
[dependencies]
alloc = "1"
FIXTURE
    cat > "$(gate_fixture slopos-ostd/Cargo.toml)" <<'FIXTURE'
[dependencies]
alloc = "1"
FIXTURE
    cat > "$(gate_fixture vendor/unwinding/Cargo.toml)" <<'FIXTURE'
[dependencies]
alloc = "1"
FIXTURE
    cat > "$(gate_fixture a/b/c/Cargo.toml)" <<'FIXTURE'
[dependencies]
alloc = "1"
FIXTURE

    GATE_FINDINGS="$(scan_manifests "$GATE_FIXTURE_ROOT")"
    gate_expect 1 4 "a plain dep, a workspace dep, a target-section dep, and a dev-dep three levels down"
    gate_expect_silent '	(Cargo\.toml|gfx/|userland/|slopos-ostd/|vendor/unwinding/|a/b/c/)' \
        "the workspace root, a [features] alloc stanza, the userland and OSTD carve-outs, the pinned annex, and a manifest past -maxdepth 3 all stay silent"

    # Source positives and negatives.
    cat > "$(gate_fixture mm/src/lib.rs)" <<'FIXTURE'
extern crate alloc;
use alloc::vec::Vec;
FIXTURE
    cat > "$(gate_fixture mm/src/b.rs)" <<'FIXTURE'
use ::alloc::boxed::Box;
FIXTURE
    cat > "$(gate_fixture mm/src/negatives.rs)" <<'FIXTURE'
#[cfg(feature = "std")]
use alloc::vec::Vec;
#[cfg(test)]
mod tests {
    extern crate alloc;
}
fn quoted() { let s = "use alloc::vec::Vec;"; let _ = s; }
FIXTURE
    cat > "$(gate_fixture kernel/src/main.rs)" <<'FIXTURE'
extern crate alloc;
FIXTURE
    cat > "$(gate_fixture terminal-core/src/x.rs)" <<'FIXTURE'
use alloc::vec::Vec;
FIXTURE
    cat > "$(gate_fixture slopos-ostd/src/x.rs)" <<'FIXTURE'
use alloc::vec::Vec;
FIXTURE

    fixture_files="$(gate_collect_rs_files "$GATE_FIXTURE_ROOT")"
    gate_expect_enumerator "$GATE_FIXTURE_ROOT" "$fixture_files"
    scanned="$(printf '%s\n' "$fixture_files" | filter_files)"
    GATE_FINDINGS="$(run_scan "$GATE_FIXTURE_ROOT" $scanned)"

    gate_expect 2 3 "extern crate alloc, use alloc::, and the path-absolute use ::alloc::"
    gate_expect_silent 'negatives\.rs|kernel/src/main\.rs|terminal-core/|slopos-ostd/' \
        "both cfg lookbacks, a string literal that merely contains the text, the whole-line source whitelist, and the crate carve-outs all stay silent"

    gate_selftest_end
fi

# ---------------------------------------------------------------------------
# Real run
# ---------------------------------------------------------------------------
bad=0

manifest_offenders="$(scan_manifests "$REPO_ROOT" | cut -f2-)"
if [ -n "$manifest_offenders" ]; then
    echo "check_alloc_dep: crate manifest declares an 'alloc' dependency:" >&2
    echo "$manifest_offenders" | sed 's/^/    /' >&2
    echo "  kernel crates must route heap allocation through slopos_ostd::mm::heap instead" >&2
    bad=1
fi

file_list="$(gate_collect_rs_files "$REPO_ROOT")"
gate_require_nonempty check_alloc_dep "$REPO_ROOT" "$file_list"
filtered="$(printf '%s\n' "$file_list" | filter_files)"

source_offenders="$(run_scan "$REPO_ROOT" $filtered | cut -f2-)"
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
