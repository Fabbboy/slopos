#!/usr/bin/env bash
# Fail the build if a kernel crate's *expansion* contains `unsafe` that no
# source scan can see.
#
# `#![forbid(unsafe_code)]` and scripts/check_unsafe_outside_ostd.sh are
# blind to the same construct: rustc drops any `unsafe_code` diagnostic
# whose primary span satisfies `in_external_macro` unless the lint declares
# `report_in_external_macro`, and `UNSAFE_CODE` does not. So a macro defined
# in another crate — OSTD's, a proc macro, or a registry dependency's — can
# expand `unsafe` into a forbid crate with zero diagnostics, and the call
# site contains no keyword for a grep to find. `--force-warn unsafe_code`
# does not defeat it either; it fires in the defining crate and stays silent
# at the call site.
#
# This gate inspects the real artifact instead, the way check_stack_sizes.sh
# and check_kernel_softfloat.sh do. It expands each kernel crate and holds
# the result to a constant, not to a recorded per-crate count: a baseline
# table would need re-recording on every legitimate change and would stop
# meaning anything the first time someone bumped it to make CI pass.
#
# The constant is:
#
#   executable unsafe            0     `unsafe {`, `unsafe fn`,
#                                      `unsafe extern`, `unsafe trait`
#   unsafe impl                  only of a trait in TRAIT_ALLOWLIST
#   #[unsafe(link_section=…)]    only a section in SECTION_ALLOWLIST
#   #[unsafe(no_mangle)]         only a symbol in SYMBOL_ALLOWLIST
#
# Compiler-emitted `unsafe` is filtered by trait path, not by guesswork.
# `#[derive(Clone, Copy)]` in one derive list emits
# `unsafe impl ::core::clone::TrivialClone`, and `#[derive(PartialEq)]` on a
# multi-variant enum with fields emits
# `_ => unsafe { ::core::intrinsics::unreachable() }`. Those two are the
# entire footprint: panic!, assert!, assert_eq!, write!, todo! and the other
# common derives inject nothing.
#
# Feature matrix: the ~2 700 `stest!` sites all sit behind a crate's test
# feature, so a default-features-only run sees none of them. Each crate is
# expanded once per feature configuration it has.
#
# `-Zunpretty=expanded` is unstable and its output format is not guaranteed.
# GOLDEN_FIXTURE below is a canary: it pins the two compiler-emitted shapes
# this script filters on, so a toolchain bump that changes them fails here
# with a clear message instead of silently making the filters over-match.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

CARGO="${CARGO:-cargo}"
RUST_CHANNEL="${RUST_CHANNEL:-$(grep -oP 'channel = "\K[^"]+' rust-toolchain.toml)}"
RUST_TARGET="${RUST_TARGET:-targets/x86_64-slos.json}"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-builddir/target}"

# Where an expansion's stderr lands, so a failure can be reported instead of
# read as an empty expansion. See expand_first_error.
EXPAND_STDERR="$(mktemp)"
trap 'rm -f "$EXPAND_STDERR"' EXIT

# Crates that legitimately author unsafe. Same set as
# check_unsafe_outside_ostd.sh's lint-attribute scan.
EXPAND_EXEMPT_RE='^(slopos-ostd|slopos-ostd-derive|kernel|vendor/.*)$'

# `unsafe impl` is how an unsafe trait gets implemented; there is no safe
# spelling. Each entry is a trait whose obligation something mechanical
# discharges, so the impl is sanctioned rather than merely tolerated.
#
#   Pod / Zeroable        `#[derive]`d, and the derive checks repr and gives
#                         every field the same bound.
#   HermeticState         hand-written per impl. The obligation is real —
#                         `restore` republishes saved PCR pointers and MSRs —
#                         and belongs to the invoking crate, so it stays.
#   PcrStackTy            declares one of the two stack-handle types the live
#                         kernel builds tasks from.
#   TrivialClone          compiler-emitted, see header.
TRAIT_ALLOWLIST=(
    '::slopos_ostd::Pod'
    '::slopos_ostd::Zeroable'
    'slopos_ostd::Pod'
    'slopos_ostd::Zeroable'
    'crate::Pod'
    'crate::Zeroable'
    'HermeticState'
    'PcrStackTy'
    '::core::clone::TrivialClone'
)

# The ten sections link.ld brackets, plus the three the Limine boot
# protocol reads. OSTD's registry_entry! / limine_request! own every one of
# these labels; a crate cannot name a section that is not on this list.
SECTION_ALLOWLIST=(
    '.boot_init_early_hw' '.boot_init_memory' '.boot_init_drivers'
    '.boot_init_services' '.boot_init_optional'
    '.driver_registry' '.platform_driver_registry'
    '.test_registry' '.hermetic_state_registry' '.kconsole_registry'
    '.limine_requests' '.limine_requests_start_marker'
    '.limine_requests_end_marker'
)

# C-ABI entry points whose callers are assembly, so the symbol has to
# resolve at link time.
SYMBOL_ALLOWLIST=(
    'kernel_main'
    'common_exception_handler'
    'isr_iret_frame_corrupt'
)

# `unsafe fn` items that are a required method of an allowlisted unsafe
# trait. Implementing the trait means writing them; there is no safe
# spelling, and the trait's own entry above records why the obligation
# stays with the invoking crate.
#
#   restore   HermeticState::restore — republishes saved PCR pointers,
#             IST stack pointers and the syscall MSRs.
METHOD_ALLOWLIST=(
    'unsafe fn restore'
)

# Compiler-emitted expansion shapes this script's filters depend on.
GOLDEN_FIXTURE='unsafe impl ::core::clone::TrivialClone'

join_alt() {
    local IFS='|'
    echo "$*"
}

crate_configs() {
    # Echo one feature-flag string per configuration to expand this crate
    # under. An empty line means "default features".
    case "$1" in
        boot) echo ""; echo "--features tests" ;;
        fs) echo ""; echo "--features tests" ;;
        hermetic) echo ""; echo "--features tests" ;;
        ktesting) echo ""; echo "--features tests" ;;
        core | drivers | mm | net | ring | sched)
            echo ""; echo "--features test-hooks" ;;
        *) echo "" ;;
    esac
}

expand_crate() {
    # $1 = package dir, $2 = extra cargo flags
    local dir="$1" flags="$2" pkg
    pkg="$(python3 - "$dir" <<'PY'
import re, sys
src = open(f"{sys.argv[1]}/Cargo.toml").read()
print(re.search(r'^name\s*=\s*"([^"]+)"', src, re.M).group(1))
PY
)"
    # shellcheck disable=SC2086
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
    SLOPOS_KSYMS_RS="${SLOPOS_KSYMS_RS:-$REPO_ROOT/builddir/kallsyms-dev.rs}" \
        "$CARGO" "+$RUST_CHANNEL" rustc \
        -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem \
        -Zunstable-options --target "$RUST_TARGET" \
        -p "$pkg" $flags -- -Zunpretty=expanded 2>"$EXPAND_STDERR"
}

# The first compiler error from the most recent expand_crate, or empty.
#
# Expansion stderr is captured rather than discarded because an empty
# expansion has two very different causes that otherwise look identical: a
# crate that genuinely expands to nothing, and a crate that failed to compile
# — most often because something it depends on is mid-edit. Reporting only
# "expansion produced no output" sends the reader looking at the wrong crate,
# which is exactly what happened once: nine dependent configurations were
# reported and every one of them was a single unrelated crate failing to
# build. One line of the real error points straight at the cause.
expand_first_error() {
    [ -s "$EXPAND_STDERR" ] || return 0
    grep -m1 -E "^error" "$EXPAND_STDERR" 2>/dev/null || true
}

# Drop doc comments — they survive expansion verbatim and 40-odd of them in
# kernel crates contain the word — and the compiler-emitted shapes named in
# the header. String literals are left alone here because the attribute
# checks below read the section name out of one; they are blanked per line
# in the executable-unsafe fallback instead.
#
# `#[unsafe(no_mangle)]` is joined to the item it decorates: the attribute
# alone does not say which symbol it exports, and the allowlist is by name.
strip_noise() {
    sed -e 's;^[[:space:]]*//[/!].*$;;' \
        -e '/unsafe impl ::core::clone::TrivialClone/d' \
        -e 's;unsafe { ::core::intrinsics::unreachable() };;g' \
    | awk '
        /#\[unsafe\(no_mangle\)\]/ { pending = $0; next }
        pending != "" { print pending " " $0; pending = ""; next }
        { print }
        END { if (pending != "") print pending }
    '
}

if [ ! -f "$REPO_ROOT/builddir/kallsyms-dev.rs" ]; then
    mkdir -p "$REPO_ROOT/builddir"
    printf 'pub static KERNEL_SYMBOLS: &[crate::ksym::KernelSymbol] = &[];\n' \
        > "$REPO_ROOT/builddir/kallsyms-dev.rs"
fi

trait_alt="$(join_alt "${TRAIT_ALLOWLIST[@]}")"
section_alt="$(join_alt "${SECTION_ALLOWLIST[@]}")"
symbol_alt="$(join_alt "${SYMBOL_ALLOWLIST[@]}")"

golden_seen=0
offenders=""
crates_scanned=0

for dir in $("$SCRIPT_DIR/kernel_crates.sh"); do
    [[ "$dir" =~ $EXPAND_EXEMPT_RE ]] && continue
    while IFS= read -r flags; do
        expanded="$(expand_crate "$dir" "$flags" || true)"
        if [ -z "$expanded" ]; then
            why="$(expand_first_error)"
            offenders+="$dir${flags:+ [$flags]}: expansion produced no output${why:+ — $why}"$'\n'
            continue
        fi
        crates_scanned=$((crates_scanned + 1))
        if grep -qF "$GOLDEN_FIXTURE" <<< "$expanded"; then
            golden_seen=1
        fi
        cleaned="$(strip_noise <<< "$expanded")"

        while IFS= read -r line; do
            [ -z "$line" ] && continue
            case "$line" in *unsafe*) ;; *) continue ;; esac

            # Attribute forms first: an allowlisted one is not an offence.
            if [[ "$line" =~ \#\[unsafe\(link_section ]]; then
                if [[ "$line" =~ \"($section_alt)\" ]]; then continue; fi
                offenders+="$dir${flags:+ [$flags]}: unblessed link_section: $line"$'\n'
                continue
            fi
            if [[ "$line" =~ \#\[unsafe\((no_mangle|export_name) ]]; then
                offenders+="$dir${flags:+ [$flags]}: no_mangle/export_name: $line"$'\n'
                continue
            fi
            if [[ "$line" =~ unsafe[[:space:]]+impl ]]; then
                if [[ "$line" =~ ($trait_alt) ]]; then continue; fi
                offenders+="$dir${flags:+ [$flags]}: unblessed unsafe impl: $line"$'\n'
                continue
            fi
            allowed_method=0
            for method in "${METHOD_ALLOWLIST[@]}"; do
                if [[ "$line" == *"$method"* ]]; then allowed_method=1; break; fi
            done
            [ "$allowed_method" -eq 1 ] && continue
            # Anything else spelling the keyword is executable unsafe. Blank
            # string literals first: a `"… unsafe …"` in expanded output is
            # data, not code.
            bare="${line//\"*\"/}"
            bare="$(sed 's;"[^"]*";;g' <<< "$line")"
            if [[ "$bare" =~ (^|[^A-Za-z0-9_])unsafe([^A-Za-z0-9_]|$) ]]; then
                offenders+="$dir${flags:+ [$flags]}: executable unsafe: $line"$'\n'
            fi
        done <<< "$cleaned"
    done < <(crate_configs "$dir")
done

# The no_mangle allowlist is applied per symbol name, on the line following
# the attribute; the loop above rejects every occurrence, so re-admit the
# named ones here rather than carrying a two-line lookback through it.
if [ -n "$offenders" ]; then
    filtered=""
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        if [[ "$line" == *"no_mangle/export_name"* ]] && [[ "$line" =~ ($symbol_alt) ]]; then
            continue
        fi
        filtered+="$line"$'\n'
    done <<< "$offenders"
    offenders="$filtered"
fi

if [ "$crates_scanned" -eq 0 ]; then
    echo "check_unsafe_expansion: no crate expanded — refusing to report OK" >&2
    exit 2
fi

if [ "$golden_seen" -eq 0 ]; then
    echo "check_unsafe_expansion: golden fixture '$GOLDEN_FIXTURE' not found in any" >&2
    echo "  expansion. Either the toolchain changed what derives emit, or" >&2
    echo "  -Zunpretty=expanded's format moved. Re-derive the filters in" >&2
    echo "  strip_noise() before trusting this gate again." >&2
    exit 2
fi

if [ -n "$offenders" ]; then
    echo "check_unsafe_expansion: macro-injected unsafe in a kernel crate's expansion:" >&2
    echo "$offenders" | sed 's/^/    /' >&2
    echo "  These do not appear in the crate's source and forbid(unsafe_code) cannot" >&2
    echo "  see them. Either route the operation through a safe OSTD API, or — if the" >&2
    echo "  obligation genuinely belongs to the invoking crate — add the trait, section" >&2
    echo "  or symbol to the matching allowlist in this script with a reason." >&2
    exit 1
fi

echo "check_unsafe_expansion: OK — $crates_scanned crate expansions carry no unsafe"
echo "check_unsafe_expansion: beyond the allowlisted traits, sections and symbols"
