#!/usr/bin/env bash
# Fail the build if any kernel crate other than slopos-ostd contains a
# bare `unsafe` block or `unsafe fn`. slopos-ostd is the kernel's
# Operating System Trusted Domain — the single crate that owns every
# line of first-party `unsafe` in the kernel. Every
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
#   - vendor/unwinding/             named TCB annex: a pinned third-party
#                                   unwinder used only through OSTD's
#                                   unwind surface.
#   - vendor/gimli/                 named TCB annex: the pinned DWARF
#                                   reader consumed by vendor/unwinding.
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
#
# Userland-side crates (userland, slibc, slop-protocol, appkit) are out of
# scope. `ktesting` is not one of them: it is an unconditional dependency of
# nine kernel crates and ships in kernel.elf, so it is scanned like any other.
#
# Comment-line and `#[cfg(...)]`-gated occurrences are skipped using the
# same lookback pattern as scripts/check_alloc_dep.sh so cfg-stubs that
# compile out of the kernel build are accepted.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_unsafe_outside_ostd "$@"

# Crate-name allowlist — everything userland-side, plus the trusted
# core and its proc-macro support. Matches the leading directory
# component of the path (relative to REPO_ROOT). The named TCB annexes
# (vendor/unwinding, vendor/gimli) are handled separately below; every
# other vendor crate is scanned, including untracked vendor/**/*.rs.
# slopos-rt = the userland async runtime; userland-side, identical role to
# userland/appkit which are already exempt and already carry unsafe.
USERLAND_RE='^(userland|slibc|slop-protocol|appkit|image|slopos-rt|slopos-ostd|slopos-ostd-derive)/'
TCB_ANNEX_RE='^vendor/(unwinding|gimli)/'

# Explicit file-level allowlist. Each entry is a repo-relative path.
SOURCE_WHITELIST=(
    "kernel/src/main.rs"
)

# Filter out userland-side crates and explicit-file exemptions. Built
# imperatively rather than with a piped grep-of-greps because the
# nested-pipe form silently drops stdin in some shell versions and
# would no-op the gate.
filter_files() {
    local path exempt skip
    while IFS= read -r path; do
        [ -z "$path" ] && continue
        [[ "$path" =~ $USERLAND_RE ]] && continue
        # Named TCB annexes. Other vendor crates are deliberately scanned.
        [[ "$path" =~ $TCB_ANNEX_RE ]] && continue
        skip=0
        for exempt in "${SOURCE_WHITELIST[@]}"; do
            if [ "$path" = "$exempt" ]; then
                skip=1
                break
            fi
        done
        [ "$skip" -eq 1 ] && continue
        printf '%s\n' "$path"
    done
}

# awk pass per file: flag any `unsafe`-keyword line that is not a
# comment and is not preceded by a `#[cfg(...)]` attribute (direct or
# via an enclosing `mod ... {` declaration two lines back).
#
# Findings carry a `<tag>\t` prefix so the self-test can count each check;
# the reports strip it back off.
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
                sub(/\/\/.*/, "", stripped)
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
                    printf "1\t%s:%d: %s\n", fname, NR, $0
                }
            }
        ' "$file" || true
    done
}

# ---------------------------------------------------------------------------
# Every kernel crate carries the lint attribute.
#
# The scan above finds `unsafe` that was written; this finds the crate that
# could write it without anyone noticing. A new workspace member linked into
# `kernel` starts with no lint attribute at all, and until it acquires one
# the scan is the only thing standing between it and an `unsafe` block.
#
# The crate set is the kernel binary's own dependency closure, so a crate
# joining the kernel image is covered the moment it is linked — there is no
# list here to forget to update.
# ---------------------------------------------------------------------------

# Crates that legitimately author unsafe, and why:
#   slopos-ostd         the OSTD itself
#   slopos-ostd-derive  proc-macro emitting `unsafe impl` token text
#   kernel              global allocator + alloc error handler
# The named vendor TCB annexes are covered by TCB_ANNEX_RE as above.
LINT_EXEMPT_RE='^(slopos-ostd|slopos-ostd-derive|kernel)$'

# Takes the crate-dir list as arguments so the self-test can drive it over a
# fixture tree; the real run pipes `kernel_crates.sh` into it.
scan_crate_lints() {
    local root="$1"
    shift
    local dir candidate crate_root
    for dir in "$@"; do
        [ -z "$dir" ] && continue
        [[ "$dir" =~ $LINT_EXEMPT_RE ]] && continue
        [[ "$dir/" =~ $TCB_ANNEX_RE ]] && continue
        crate_root=""
        for candidate in "$root/$dir/src/lib.rs" "$root/$dir/src/main.rs"; do
            [ -f "$candidate" ] && crate_root="$candidate" && break
        done
        if [ -z "$crate_root" ]; then
            printf '2\t%s (no crate root found)\n' "$dir"
            continue
        fi
        if ! grep -qE '^#!\[forbid\(unsafe_code\)\]' "$crate_root"; then
            printf '2\t%s → %s\n' "$dir" "${crate_root#"$root"/}"
        fi
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
    gate_selftest_begin check_unsafe_outside_ostd

    cat > "$(gate_fixture sched/src/positives.rs)" <<'FIXTURE'
fn raw() { unsafe { core::ptr::read(p) } }
unsafe fn danger() {}
unsafe impl Send for Foo {}
unsafe extern "C" { fn ext(); }
fn block() { let x = unsafe { *p }; }
fn deref_assign() {
    *out = unsafe { *src };
}
FIXTURE

    cat > "$(gate_fixture vendor/othercrate/src/lib.rs)" <<'FIXTURE'
unsafe fn vendored() {}
FIXTURE

    # Forms the gate deliberately accepts. The bare-`*` deref line is
    # load-bearing in the other direction: unlike check_no_kernel_async, this
    # gate must NOT skip it as a block-comment continuation.
    cat > "$(gate_fixture sched/src/negatives.rs)" <<'FIXTURE'
// unsafe { never_executed() }
/* unsafe fn block_commented() {} */
fn ident() { let unsafe_count = 0; let _ = unsafe_count; }
fn make_unsafe_handle() {}
#[unsafe(link_section = ".driver_registry")]
static E: u8 = 0;
fn trailing() { let v = 1; } // unsafe
#[cfg(test)]
unsafe fn cfg_gated() {}
#[cfg(feature = "host")]
mod gated {
    unsafe fn inside() {}
}
FIXTURE

    # `SOURCE_WHITELIST` is an exact whole-path compare; a basename or
    # substring match would exempt every main.rs in the tree.
    cat > "$(gate_fixture kernel/src/main.rs)" <<'FIXTURE'
#[global_allocator]
static A: KernelHeap = KernelHeap;
unsafe fn allocator_support() {}
FIXTURE
    cat > "$(gate_fixture slopos-ostd/src/x.rs)" <<'FIXTURE'
unsafe fn the_trusted_core_owns_this() {}
FIXTURE
    cat > "$(gate_fixture userland/src/main.rs)" <<'FIXTURE'
unsafe fn userland_is_out_of_scope() {}
FIXTURE
    cat > "$(gate_fixture vendor/unwinding/src/lib.rs)" <<'FIXTURE'
unsafe fn named_annex() {}
FIXTURE

    fixture_files="$(gate_collect_rs_files "$GATE_FIXTURE_ROOT")"
    gate_expect_enumerator "$GATE_FIXTURE_ROOT" "$fixture_files"

    scanned="$(printf '%s\n' "$fixture_files" | filter_files)"
    GATE_FINDINGS="$(run_scan "$GATE_FIXTURE_ROOT" $scanned)"

    gate_expect 1 7 "unsafe block, unsafe fn, unsafe impl, unsafe extern, a block in a let, a deref assignment, and a non-annex vendor crate"
    gate_expect_silent 'negatives\.rs|kernel/src/main\.rs|slopos-ostd/|userland/|vendor/unwinding/' \
        "comments, the #[unsafe(...)] attribute form, identifiers containing the keyword, a trailing comment, both cfg lookbacks, the whole-path source whitelist, the trusted core, userland, and the named TCB annexes all stay silent"

    # Pass 2 over a synthetic crate list. The crate *set* resolution stays
    # uncovered — that is kernel_crates.sh's job.
    mkdir -p "$GATE_FIXTURE_ROOT/mm/src" "$GATE_FIXTURE_ROOT/newcrate"
    printf '#![forbid(unsafe_code)]\n' > "$GATE_FIXTURE_ROOT/mm/src/lib.rs"
    printf 'pub fn f() {}\n' > "$GATE_FIXTURE_ROOT/sched/src/lib.rs"
    GATE_FINDINGS="$(scan_crate_lints "$GATE_FIXTURE_ROOT" \
        mm sched newcrate slopos-ostd kernel vendor/unwinding)"
    gate_expect 2 2 "a crate with no forbid attribute, and a crate with no crate root at all"
    gate_expect_silent '	(mm|slopos-ostd|kernel|vendor/unwinding) ' \
        "the forbidding crate and the three exempt crates stay silent"

    gate_selftest_end
fi

# ---------------------------------------------------------------------------
# Real run
# ---------------------------------------------------------------------------
file_list="$(gate_collect_rs_files "$REPO_ROOT")"
gate_require_nonempty check_unsafe_outside_ostd "$REPO_ROOT" "$file_list"
filtered="$(printf '%s\n' "$file_list" | filter_files)"

source_offenders="$(run_scan "$REPO_ROOT" $filtered | cut -f2-)"

if [ -n "$source_offenders" ]; then
    echo "check_unsafe_outside_ostd: executable 'unsafe' detected outside slopos-ostd and the named TCB annexes:" >&2
    echo "$source_offenders" | sed 's/^/    /' >&2
    echo "  slopos-ostd is the kernel OSTD; vendor/unwinding and vendor/gimli are the named vendor TCB annexes." >&2
    echo "  If a new file legitimately needs an unsafe attribute (e.g. #[unsafe(link_section)] in a" >&2
    echo "  macro_rules! body) and you have audited it, add the file to SOURCE_WHITELIST in this script." >&2
    exit 1
fi

# Fails closed. This used to warn and skip, so a runner without jq passed a
# crate carrying no lint attribute at all; cargo and jq are already hard
# requirements of tcb_ratio.sh, which runs beside this gate.
if ! crate_dirs="$("$SCRIPT_DIR/kernel_crates.sh")"; then
    echo "check_unsafe_outside_ostd: could not resolve the kernel crate set" >&2
    echo "  (scripts/kernel_crates.sh needs cargo + jq). The lint-attribute scan is" >&2
    echo "  half this gate; skipping it silently would pass a crate carrying no" >&2
    echo "  #![forbid(unsafe_code)] at all." >&2
    exit 2
fi

missing_lint="$(scan_crate_lints "$REPO_ROOT" $crate_dirs | cut -f2-)"

if [ -n "$missing_lint" ]; then
    echo "check_unsafe_outside_ostd: kernel crate without #![forbid(unsafe_code)]:" >&2
    echo "$missing_lint" | sed 's/^/    /' >&2
    echo "  Every crate the kernel binary links must carry the attribute at its crate root." >&2
    exit 1
fi

echo "check_unsafe_outside_ostd: OK — no executable unsafe outside slopos-ostd and the named TCB annexes;"
echo "check_unsafe_outside_ostd: every kernel crate carries #![forbid(unsafe_code)]"
