#!/usr/bin/env bash
set -euo pipefail

# Build an ext2 filesystem image populated with userland binaries.
#
# Usage: build_fs_image.sh <image_path> <build_dir> <bin1> [bin2] ...
#
# Each binary is placed in /bin/<name> except 'init' which goes to /sbin/init.
#
# Environment:
#   FS_IMAGE_SIZE - image size (default: 16M)

IMAGE_PATH="${1:?Usage: build_fs_image.sh <image_path> <build_dir> <bin1> [bin2] ...}"
BUILD_DIR="${2:?Usage: build_fs_image.sh <image_path> <build_dir> <bin1> [bin2] ...}"
shift 2
BINS=("$@")

FS_IMAGE_SIZE="${FS_IMAGE_SIZE:-16M}"

# macOS: extend PATH to find e2fsprogs tools installed via Homebrew
if [ "$(uname -s)" = "Darwin" ]; then
    BREW_PREFIX="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
    export PATH="${BREW_PREFIX}/opt/e2fsprogs/sbin:${BREW_PREFIX}/opt/e2fsprogs/bin:${PATH}"
fi

if ! command -v mkfs.ext2 >/dev/null 2>&1; then
    echo "mkfs.ext2 is required to create $IMAGE_PATH" >&2
    exit 1
fi

if ! command -v debugfs >/dev/null 2>&1; then
    echo "debugfs is required to populate $IMAGE_PATH" >&2
    exit 1
fi

IMAGE_DIR="$(dirname "$IMAGE_PATH")"
mkdir -p "$IMAGE_DIR"

echo "Rebuilding ext2 image at $IMAGE_PATH ($FS_IMAGE_SIZE)"
rm -f "$IMAGE_PATH"
truncate -s "$FS_IMAGE_SIZE" "$IMAGE_PATH"
mkfs.ext2 -F -b 4096 "$IMAGE_PATH" >/dev/null
debugfs -w -R "mkdir /bin" "$IMAGE_PATH" >/dev/null
debugfs -w -R "mkdir /sbin" "$IMAGE_PATH" >/dev/null

for bin in "${BINS[@]}"; do
    src="${BUILD_DIR}/${bin}.elf"
    if [ ! -f "$src" ]; then
        echo "Missing userland binary: $src" >&2
        exit 1
    fi

    dst="/bin/${bin}"
    if [ "$bin" = "init" ]; then
        dst="/sbin/init"
    fi

    debugfs -w -R "write $src $dst" "$IMAGE_PATH" >/dev/null
    debugfs -w -R "set_inode_field $dst mode 0100755" "$IMAGE_PATH" >/dev/null
done

# Install font files into /usr/share/fonts/ if assets/fonts/ exists
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FONTS_DIR="${REPO_ROOT}/assets/fonts"

if [ -d "$FONTS_DIR" ]; then
    debugfs -w -R "mkdir /usr" "$IMAGE_PATH" >/dev/null 2>&1 || true
    debugfs -w -R "mkdir /usr/share" "$IMAGE_PATH" >/dev/null 2>&1 || true
    debugfs -w -R "mkdir /usr/share/fonts" "$IMAGE_PATH" >/dev/null 2>&1 || true

    for font in "$FONTS_DIR"/*.ttf; do
        [ -f "$font" ] || continue
        fname="$(basename "$font")"
        debugfs -w -R "write $font /usr/share/fonts/$fname" "$IMAGE_PATH" >/dev/null
        echo "Installed font: /usr/share/fonts/$fname"
    done
fi

debugfs -w -R "mkdir /usr" "$IMAGE_PATH" >/dev/null 2>&1 || true
debugfs -w -R "mkdir /usr/share" "$IMAGE_PATH" >/dev/null 2>&1 || true
debugfs -w -R "mkdir /usr/share/slopos" "$IMAGE_PATH" >/dev/null 2>&1 || true
debugfs -w -R "mkdir /usr/share/slopos/wallpapers" "$IMAGE_PATH" >/dev/null 2>&1 || true

if [ -f "${REPO_ROOT}/assets/logo.png" ]; then
    debugfs -w -R "write ${REPO_ROOT}/assets/logo.png /usr/share/slopos/wallpapers/default.png" "$IMAGE_PATH" >/dev/null
    echo "Installed wallpaper: /usr/share/slopos/wallpapers/default.png"
fi

# Append a block-integrity (verity) trailer so the kernel detects on-disk
# corruption of read-only content at read time (fs/src/verity.rs). Must be the
# LAST step — it hashes the finished image. Optional: the kernel mounts
# unverified if the trailer is absent.
if command -v python3 >/dev/null 2>&1; then
    python3 "${SCRIPT_DIR}/gen_verity.py" "$IMAGE_PATH"
else
    echo "gen_verity: python3 not found — image will mount WITHOUT integrity verification" >&2
fi
