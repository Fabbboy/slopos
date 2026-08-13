#!/usr/bin/env bash
# Hold the built image to the linker-section contract.
#
# scripts/check_unsafe_expansion.sh checks what the *source tree* expands to.
# This checks what actually linked, which is the only place a third-party
# crate's `#[unsafe(link_section)]` shows up: registry dependencies expand
# their own macros, `cargo` links them, and no first-party scan sees it.
# `bitflags`' bytemuck integration is one feature flag from being live.
#
# Three properties:
#
#   1. Every non-standard PROGBITS section in kernel.elf is one link.ld
#      declares. A section nobody put there means something placed a static
#      outside the sanctioned set.
#   2. Each bracketed registry's byte span is an exact multiple of its entry
#      size. That is not cosmetic: `registry_slice` derives its count from
#      the span, and `ptr::offset_from` requires the distance be a whole
#      number of elements. A wrong-sized entry landing in a registry is
#      exactly what this catches, and it is reachable from a crate that
#      forbids unsafe.
#   3. The unwinder's FDE index is present and is the only finder linked.
#      `.eh_frame_hdr` is a `12 + 8N` byte binary-search table over
#      `.eh_frame`; without it the unwinder resolves each return address by
#      parsing every FDE from the start, ~47 ms per lookup at opt-level 0.
#      That path fails open — `EhFrameHdr::parse`'s error is swallowed by a
#      `.ok()?` and the finder falls through to a linear scan — so nothing
#      else in the build notices. Asserting the `fde-static` finder's symbol
#      is absent pins the configuration from the other side: reverting the
#      Cargo feature or the target spec's `eh-frame-header` both fail here.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SELF_TEST=0
ELF=""
while [ $# -gt 0 ]; do
    case "$1" in
        --self-test) SELF_TEST=1; shift ;;
        -*) echo "check_registry_sections: unknown option $1" >&2; exit 2 ;;
        *) ELF="$1"; shift ;;
    esac
done
ELF="${ELF:-$REPO_ROOT/builddir/kernel-dev.elf}"

# Pinned toolchain, not a host readelf: this gate reads column positions.
READELF="$("$SCRIPT_DIR/llvm_tool.sh" llvm-readobj)"
NM="$("$SCRIPT_DIR/llvm_tool.sh" llvm-nm)"
readelf_sections() {
    "$READELF" --section-headers --elf-output-style=GNU -W "$1"
}

# `.eh_frame_hdr`'s 12-byte prologue followed by an 8-byte (initial location,
# FDE address) pair per entry.
EH_FRAME_HDR_PROLOGUE=12
EH_FRAME_HDR_ENTRY=8

# The `fde-static` finder, whose presence means the linear scan is still
# reachable. Matched against the mangled name so a rename fails loudly.
LINEAR_FINDER_RE='find_fde5fixed.*StaticFinder.*find_fde'

# Registries link.ld brackets, with the `size_of` of their entry type. A
# change to an entry type that forgets this table shows up as a mismatch
# rather than as a silently wrong count at boot.
declare -A ENTRY_SIZE=(
    ['.boot_init_early_hw']=32
    ['.boot_init_memory']=32
    ['.boot_init_drivers']=32
    ['.boot_init_services']=32
    ['.boot_init_optional']=32
    ['.driver_registry']=56
    ['.platform_driver_registry']=56
    ['.test_registry']=104
    ['.hermetic_state_registry']=48
    ['.kconsole_registry']=48
    ['.charge_audit_registry']=40
)

# Sections that are neither a registry nor emitted by the toolchain.
# `.limine_requests*` are the boot protocol's; the rest are standard output
# sections, debug info, or metadata the build produces.
KNOWN_RE='^\.(limine_requests(_start_marker|_end_marker)?|text|rodata|data|bss|got|plt|init_array|fini_array|eh_frame(_hdr)?|gcc_except_table|stack_sizes|comment|note[.a-zA-Z_-]*|debug[._a-zA-Z-]*|symtab|strtab|shstrtab|relro_padding|dynamic|dynsym|dynstr|hash|gnu[.a-zA-Z_-]*|ARM[.a-zA-Z_-]*)'

# ---------------------------------------------------------------------------
# The gate has to be able to fail. `llc` places a global in an arbitrary
# section — the exact shape of a dependency's own `#[unsafe(link_section)]`.
# ---------------------------------------------------------------------------
if [ "$SELF_TEST" -eq 1 ]; then
    llc="$("$SCRIPT_DIR/llvm_tool.sh" llc)"
    fixture_root="$(mktemp -d)"
    trap 'rm -rf "$fixture_root"' EXIT INT TERM
    self_test_fail=0

    build_fixture() {
        printf 'target triple = "x86_64-unknown-none"\n%s\n' "$2" > "$fixture_root/$1.ll"
        "$llc" -mtriple=x86_64-unknown-none -filetype=obj \
            -o "$fixture_root/$1.o" "$fixture_root/$1.ll"
    }

    expect() {
        local label="$1" want_exit="$2" want_text="$3" obj="$4"
        local out status
        set +e
        out="$("$0" "$fixture_root/$obj" 2>&1)"
        status=$?
        set -e
        if [ "$status" -ne "$want_exit" ] \
            || { [ -n "$want_text" ] && ! printf '%s\n' "$out" | grep -qF "$want_text"; }; then
            echo "check_registry_sections --self-test: $label — expected exit $want_exit" >&2
            echo "  mentioning '$want_text', got exit $status:" >&2
            printf '%s\n' "$out" | sed 's/^/      /' >&2
            self_test_fail=1
            return
        fi
        echo "  $label: ok"
    }

    echo "check_registry_sections: self-test against synthesised objects"

    # A well-formed index every fixture that should reach property 3 carries.
    INDEX='@h = global [20 x i8] zeroinitializer, section ".eh_frame_hdr"'

    build_fixture clean "@r = global [56 x i8] zeroinitializer, section \".driver_registry\"
$INDEX"
    expect "a whole number of entries is accepted" 0 "OK" clean.o

    build_fixture ragged "@r = global [60 x i8] zeroinitializer, section \".driver_registry\"
$INDEX"
    expect "a partial entry is rejected" 1 "not a multiple" ragged.o

    build_fixture unblessed "@r = global [8 x i8] zeroinitializer, section \".sneaky_registry\"
@k = global [56 x i8] zeroinitializer, section \".driver_registry\"
$INDEX"
    expect "an undeclared section is rejected" 1 ".sneaky_registry" unblessed.o

    build_fixture bare '@r = global i64 0'
    expect "an ELF with no registry at all is rejected" 2 "refusing to report OK" bare.o

    build_fixture noindex '@r = global [56 x i8] zeroinitializer, section ".driver_registry"'
    expect "a missing unwind index is rejected" 1 "carries no .eh_frame_hdr" noindex.o

    build_fixture raggedindex '@r = global [56 x i8] zeroinitializer, section ".driver_registry"
@h = global [18 x i8] zeroinitializer, section ".eh_frame_hdr"'
    expect "a malformed unwind index is rejected" 1 "not
  12 + 8 x N" raggedindex.o

    build_fixture linearfinder "@r = global [56 x i8] zeroinitializer, section \".driver_registry\"
$INDEX
@_RNvXNtNtNtCs0000000000_9unwinding8unwinder8find_fde5fixedNtB2_12StaticFinderNtB4_9FDEFinder8find_fde = global i64 0"
    expect "the linear fde-static finder is rejected" 1 "linear fde-static finder" linearfinder.o

    rm -rf "$fixture_root"
    trap - EXIT INT TERM
    if [ "$self_test_fail" -ne 0 ]; then
        echo "check_registry_sections: SELF-TEST FAILED — the gate cannot be trusted to reject" >&2
        exit 1
    fi
    echo "check_registry_sections: self-test OK"
    exit 0
fi

if [ ! -f "$ELF" ]; then
    echo "check_registry_sections: $ELF not found — run 'just build' first" >&2
    exit 2
fi

sections="$(readelf_sections "$ELF")"

unknown=""
while read -r name; do
    [ -z "$name" ] && continue
    [[ "$name" =~ $KNOWN_RE ]] && continue
    [ -n "${ENTRY_SIZE[$name]+x}" ] && continue
    unknown+="$name"$'\n'
done < <(awk '/PROGBITS|NOBITS/ { for (i = 1; i <= NF; i++) if ($i ~ /^\./) { print $i; break } }' <<< "$sections")

if [ -n "$unknown" ]; then
    echo "check_registry_sections: section in kernel.elf that link.ld does not declare:" >&2
    echo "$unknown" | sed 's/^/    /' >&2
    echo "  Something placed a static outside the sanctioned set — most likely a" >&2
    echo "  dependency's own #[unsafe(link_section)]. Identify it before adding it here." >&2
    exit 1
fi

bad=""
checked=0
for name in "${!ENTRY_SIZE[@]}"; do
    line="$(grep -E "[[:space:]]${name//./\\.}[[:space:]]" <<< "$sections" | head -1 || true)"
    [ -z "$line" ] && continue
    size_hex="$(awk -v n="$name" '{ for (i = 1; i <= NF; i++) if ($i == n) { print $(i + 4); exit } }' <<< "$line")"
    size=$((16#$size_hex))
    stride="${ENTRY_SIZE[$name]}"
    checked=$((checked + 1))
    if [ "$size" -eq 0 ]; then
        continue
    fi
    if [ $((size % stride)) -ne 0 ]; then
        bad+="$name: $size bytes is not a multiple of the $stride-byte entry"$'\n'
    fi
done

if [ "$checked" -eq 0 ]; then
    echo "check_registry_sections: no registry section found in $ELF — refusing to report OK" >&2
    exit 2
fi

if [ -n "$bad" ]; then
    echo "check_registry_sections: registry span is not a whole number of entries:" >&2
    echo "$bad" | sed 's/^/    /' >&2
    echo "  Either an entry type changed size without ENTRY_SIZE here being updated," >&2
    echo "  or a wrong-typed static landed in the section." >&2
    exit 1
fi

hdr_line="$(grep -E '[[:space:]]\.eh_frame_hdr[[:space:]]' <<< "$sections" | head -1 || true)"
if [ -z "$hdr_line" ]; then
    echo "check_registry_sections: $ELF carries no .eh_frame_hdr" >&2
    echo "  The unwinder falls back to a full linear .eh_frame scan per lookup." >&2
    echo "  link.ld must declare the output section and targets/x86_64-slos.json" >&2
    echo "  must set eh-frame-header true." >&2
    exit 1
fi
hdr_size=$((16#$(awk '{ for (i = 1; i <= NF; i++) if ($i == ".eh_frame_hdr") { print $(i + 4); exit } }' <<< "$hdr_line")))
if [ "$hdr_size" -lt "$((EH_FRAME_HDR_PROLOGUE + EH_FRAME_HDR_ENTRY))" ] \
    || [ $(((hdr_size - EH_FRAME_HDR_PROLOGUE) % EH_FRAME_HDR_ENTRY)) -ne 0 ]; then
    echo "check_registry_sections: .eh_frame_hdr is $hdr_size bytes, not" >&2
    echo "  $EH_FRAME_HDR_PROLOGUE + $EH_FRAME_HDR_ENTRY x N. The search table is malformed or absent," >&2
    echo "  and a header that parses without a table degrades to the linear scan." >&2
    exit 1
fi

if "$NM" "$ELF" 2>/dev/null | grep -qE "$LINEAR_FINDER_RE"; then
    echo "check_registry_sections: the linear fde-static finder is linked into $ELF" >&2
    echo "  It is tried whenever the indexed lookup misses, which silently restores" >&2
    echo "  the cost .eh_frame_hdr exists to remove. Root Cargo.toml must select" >&2
    echo "  fde-gnu-eh-frame-hdr instead of fde-static, not alongside it." >&2
    exit 1
fi

echo "check_registry_sections: OK — $checked registries hold whole entries; no unblessed sections;" \
    "unwind index holds $(((hdr_size - EH_FRAME_HDR_PROLOGUE) / EH_FRAME_HDR_ENTRY)) entries"
