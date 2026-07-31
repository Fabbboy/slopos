#!/usr/bin/env bash
set -euo pipefail

# Build a bootable SlopOS ISO image.
#
# Usage: build_iso.sh <output> <build_dir> [cmdline]
#
# Environment:
#   KERNEL_ELF - path to the kernel ELF to stage (required)
#   LIMINE_DIR - path to Limine directory (default: third_party/limine)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

OUTPUT="${1:?Usage: build_iso.sh <output> <build_dir> [cmdline]}"
BUILD_DIR="${2:?Usage: build_iso.sh <output> <build_dir> [cmdline]}"
CMDLINE="${3:-}"

LIMINE_DIR="${LIMINE_DIR:-${REPO_ROOT}/third_party/limine}"

# Named by the caller, not defaulted: a default that happened to exist would
# stage whichever variant was built last.
KERNEL="${KERNEL_ELF:?build_iso: KERNEL_ELF must name the kernel ELF to stage}"

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

# Stage the initramfs as a Limine module and reference it from the boot entry.
# Done dynamically (rather than statically in limine.conf) so an ISO built
# without an initramfs never points Limine at a missing module.
if [ -n "${INITRAMFS_FILE:-}" ] && [ -f "${INITRAMFS_FILE}" ]; then
    cp "${INITRAMFS_FILE}" "$ISO_ROOT/boot/initramfs.cpio"
    printf '    module_path: boot():/boot/initramfs.cpio\n' >> "$ISO_ROOT/boot/limine.conf"
    printf '    module_string: initramfs\n' >> "$ISO_ROOT/boot/limine.conf"
fi

cp "$LIMINE_DIR/limine-bios.sys" "$ISO_ROOT/boot/"
cp "$LIMINE_DIR/limine-bios-cd.bin" "$ISO_ROOT/boot/"
cp "$LIMINE_DIR/limine-uefi-cd.bin" "$ISO_ROOT/boot/"
cp "$LIMINE_DIR/BOOTX64.EFI" "$ISO_ROOT/EFI/BOOT/"
cp "$LIMINE_DIR/BOOTIA32.EFI" "$ISO_ROOT/EFI/BOOT/" 2>/dev/null || true

# Limine's BSD-2-Clause requires its copyright notice, condition list and
# disclaimer to accompany any binary redistribution; NOTICE.md carries the
# same for every other third-party component on the image.
cp "$LIMINE_DIR/LICENSE" "$ISO_ROOT/boot/LICENSE.limine"
cp "$REPO_ROOT/NOTICE.md" "$ISO_ROOT/boot/NOTICE.md"

ISO_DIR="$(dirname "$OUTPUT")"
mkdir -p "$ISO_DIR"

xorriso -as mkisofs \
    -R -r -J \
    -V 'SLOPOS' \
    -b boot/limine-bios-cd.bin \
    -no-emul-boot \
    -boot-load-size 4 \
    -boot-info-table \
    -hfsplus \
    -apm-block-size 2048 \
    --efi-boot boot/limine-uefi-cd.bin \
    -efi-boot-part \
    --efi-boot-image \
    --protective-msdos-label \
    "$ISO_ROOT" \
    -o "$TMP_OUTPUT"

"$LIMINE_DIR/limine" bios-install "$TMP_OUTPUT" 2>/dev/null || true

mv "$TMP_OUTPUT" "$OUTPUT"
trap - EXIT INT TERM
rm -rf "$STAGING"
