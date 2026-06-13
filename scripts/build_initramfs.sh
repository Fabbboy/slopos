#!/usr/bin/env bash
set -euo pipefail

# Build a newc cpio initramfs from userland binaries (+ fonts/wallpaper).
#
# Usage: build_initramfs.sh <out.cpio> <build_dir> <bin1> [bin2] ...
#
# Thin wrapper over gen_initramfs.py (python-only, no host `cpio` dependency),
# mirroring build_fs_image.sh's signature so the RAM root and the ext2 disk
# image are populated from the same binary list.

OUT="${1:?Usage: build_initramfs.sh <out.cpio> <build_dir> <bin1> [bin2] ...}"
BUILD_DIR="${2:?Usage: build_initramfs.sh <out.cpio> <build_dir> <bin1> [bin2] ...}"
shift 2

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! command -v python3 >/dev/null 2>&1; then
    echo "python3 is required to build the initramfs ($OUT)" >&2
    exit 1
fi

python3 "${SCRIPT_DIR}/gen_initramfs.py" "$OUT" "$BUILD_DIR" "$@"
