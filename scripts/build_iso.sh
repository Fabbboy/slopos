#!/usr/bin/env bash
set -euo pipefail

# Build a bootable SlopOS ISO image.
#
# Usage: build_iso.sh <output> <build_dir> [cmdline]
#
# Environment:
#   LIMINE_DIR - path to Limine directory (default: third_party/limine)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

OUTPUT="${1:?Usage: build_iso.sh <output> <build_dir> [cmdline]}"
BUILD_DIR="${2:?Usage: build_iso.sh <output> <build_dir> [cmdline]}"
CMDLINE="${3:-}"

LIMINE_DIR="${LIMINE_DIR:-${REPO_ROOT}/third_party/limine}"
KERNEL="${BUILD_DIR}/kernel.elf"

if [ ! -f "$KERNEL" ]; then
    echo "Kernel not found at $KERNEL. Build the kernel first." >&2
    exit 1
fi

# Ensure Limine is available
"$SCRIPT_DIR/ensure_limine.sh"

STAGING="$(mktemp -d)"
TMP_OUTPUT="${OUTPUT}.tmp"
trap 'rm -rf "$STAGING"; rm -f "$TMP_OUTPUT"' EXIT INT TERM

ISO_ROOT="${STAGING}/iso_root"
mkdir -p "$ISO_ROOT/boot" "$ISO_ROOT/EFI/BOOT"

cp "$KERNEL" "$ISO_ROOT/boot/kernel.elf"
cp "$REPO_ROOT/limine.conf" "$ISO_ROOT/boot/limine.conf"

# Inject framebuffer resolution into Limine config
fb_w="${QEMU_FB_WIDTH:-1920}"
fb_h="${QEMU_FB_HEIGHT:-1080}"
if [ "${QEMU_FB_AUTO:-0}" != "0" ] && [ -x "$SCRIPT_DIR/detect_qemu_resolution.sh" ]; then
    detected="$(QEMU_FB_WIDTH="$fb_w" QEMU_FB_HEIGHT="$fb_h" \
        QEMU_FB_AUTO_POLICY="${QEMU_FB_AUTO_POLICY:-primary}" \
        QEMU_FB_AUTO_OUTPUT="${QEMU_FB_AUTO_OUTPUT:-}" \
        "$SCRIPT_DIR/detect_qemu_resolution.sh")" || true
    if [ -n "$detected" ]; then
        fb_w="${detected%% *}"
        fb_h="${detected##* }"
    fi
fi
printf '    resolution: %sx%s\n' "$fb_w" "$fb_h" >> "$ISO_ROOT/boot/limine.conf"

if [ -n "$CMDLINE" ]; then
    printf '    cmdline: %s\n' "$CMDLINE" >> "$ISO_ROOT/boot/limine.conf"
fi

cp "$LIMINE_DIR/limine-bios.sys" "$ISO_ROOT/boot/"
cp "$LIMINE_DIR/limine-bios-cd.bin" "$ISO_ROOT/boot/"
cp "$LIMINE_DIR/limine-uefi-cd.bin" "$ISO_ROOT/boot/"
cp "$LIMINE_DIR/BOOTX64.EFI" "$ISO_ROOT/EFI/BOOT/"
cp "$LIMINE_DIR/BOOTIA32.EFI" "$ISO_ROOT/EFI/BOOT/" 2>/dev/null || true

ISO_DIR="$(dirname "$OUTPUT")"
mkdir -p "$ISO_DIR"

xorriso -as mkisofs \
    -V 'SLOPOS' \
    -b boot/limine-bios-cd.bin \
    -no-emul-boot \
    -boot-load-size 4 \
    -boot-info-table \
    -eltorito-alt-boot \
    -e boot/limine-uefi-cd.bin \
    -no-emul-boot \
    -isohybrid-gpt-basdat \
    "$ISO_ROOT" \
    -o "$TMP_OUTPUT"

"$LIMINE_DIR/limine" bios-install "$TMP_OUTPUT" 2>/dev/null || true

mv "$TMP_OUTPUT" "$OUTPUT"
trap - EXIT INT TERM
rm -rf "$STAGING"
