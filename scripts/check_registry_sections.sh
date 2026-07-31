#!/usr/bin/env bash
# Hold the built image to the linker-section contract.
#
# scripts/check_unsafe_expansion.sh checks what the *source tree* expands to.
# This checks what actually linked, which is the only place a third-party
# crate's `#[unsafe(link_section)]` shows up: registry dependencies expand
# their own macros, `cargo` links them, and no first-party scan sees it.
# `bitflags`' bytemuck integration is one feature flag from being live.
#
# Two properties:
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

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ELF="${1:-$REPO_ROOT/builddir/kernel.elf}"

if [ ! -f "$ELF" ]; then
    echo "check_registry_sections: $ELF not found — run 'just build' first" >&2
    exit 2
fi

READELF="$(command -v llvm-readelf || command -v readelf || true)"
if [ -z "$READELF" ]; then
    echo "check_registry_sections: readelf not found" >&2
    exit 2
fi

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
)

# Sections that are neither a registry nor emitted by the toolchain.
# `.limine_requests*` are the boot protocol's; the rest are standard output
# sections, debug info, or metadata the build produces.
KNOWN_RE='^\.(limine_requests(_start_marker|_end_marker)?|text|rodata|data|bss|got|plt|init_array|fini_array|eh_frame(_hdr)?|gcc_except_table|stack_sizes|comment|note[.a-zA-Z_-]*|debug[._a-zA-Z-]*|symtab|strtab|shstrtab|relro_padding|dynamic|dynsym|dynstr|hash|gnu[.a-zA-Z_-]*|ARM[.a-zA-Z_-]*)'

sections="$("$READELF" -SW "$ELF")"

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

echo "check_registry_sections: OK — $checked registries hold whole entries; no unblessed sections"
