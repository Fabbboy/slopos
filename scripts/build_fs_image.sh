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
#   VERITY        - `on` (default) appends the integrity trailer, which makes
#                   the kernel mount the image read-only; `off` leaves the
#                   image writable, for the images the test suite writes to.
#   PRESERVE_FS_IMAGE - `1` refreshes the binaries of an existing writable
#                   image in place instead of running mkfs, so a developer
#                   iterating on the kernel keeps whatever the guest wrote.
#                   Ignored when no image exists, when the image carries a
#                   verity trailer (its hashes cover the bytes we would
#                   rewrite), or when the image is a different size.

IMAGE_PATH="${1:?Usage: build_fs_image.sh <image_path> <build_dir> <bin1> [bin2] ...}"
BUILD_DIR="${2:?Usage: build_fs_image.sh <image_path> <build_dir> <bin1> [bin2] ...}"
shift 2
BINS=("$@")

FS_IMAGE_SIZE="${FS_IMAGE_SIZE:-16M}"
VERITY="${VERITY:-on}"
case "$VERITY" in
    on|off) ;;
    *) echo "build_fs_image: VERITY must be 'on' or 'off', got '$VERITY'" >&2; exit 2 ;;
esac

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

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

PRESERVE_FS_IMAGE="${PRESERVE_FS_IMAGE:-0}"
STAMP_PATH="${IMAGE_PATH}.stamp"

# What the image's content is a function of: an equal stamp means a preserved
# image already carries these binaries and assets, so it needs no work.
build_stamp() {
    echo "size=$FS_IMAGE_SIZE verity=$VERITY"
    for bin in "${BINS[@]}"; do
        printf '%s ' "$bin"
        sha256sum "${BUILD_DIR}/${bin}.elf" 2>/dev/null | cut -d' ' -f1 || echo missing
    done
    for asset in "${REPO_ROOT}/assets/fonts"/* "${REPO_ROOT}/assets/keymaps"/* \
                 "${REPO_ROOT}/assets/logo.png"; do
        [ -f "$asset" ] || continue
        sha256sum "$asset" | cut -d' ' -f1
    done
}

# Read from the file rather than from $VERITY: the caller's intent for *this*
# build says nothing about what is already on disk.
image_carries_verity_trailer() {
    [ -s "$1" ] || return 1
    local magic
    magic=$(tail -c 32 "$1" | head -c 4 | od -An -tx1 | tr -d ' \n')
    [ "$magic" = "54525653" ]
}

# Only an image e2fsck vouches for: refreshing binaries into a damaged
# filesystem propagates the damage into the next boot's /sbin/init, and the
# fallback (a fresh mkfs) is the recovery a developer wants anyway.
image_is_preservable() {
    [ "$PRESERVE_FS_IMAGE" = "1" ] || return 1
    [ -f "$IMAGE_PATH" ] || return 1
    if image_carries_verity_trailer "$IMAGE_PATH"; then
        echo "preserve: $IMAGE_PATH carries a verity trailer — rebuilding"
        return 1
    fi
    local want have
    want=$(numfmt --from=iec "$FS_IMAGE_SIZE")
    have=$(stat -c %s "$IMAGE_PATH" 2>/dev/null || echo 0)
    if [ "$want" != "$have" ]; then
        echo "preserve: $IMAGE_PATH is ${have}B, want ${want}B — rebuilding"
        return 1
    fi
    if ! e2fsck -fn "$IMAGE_PATH" >/dev/null 2>&1; then
        echo "preserve: e2fsck rejects $IMAGE_PATH — rebuilding"
        return 1
    fi
    return 0
}

# `write` refuses an existing name, so a refresh unlinks first. debugfs writes
# the raw structures, so EXT2_IMMUTABLE_FL does not stop it.
install_binary() {
    local src="$1" dst="$2"
    debugfs -w -R "rm $dst" "$IMAGE_PATH" >/dev/null 2>&1 || true
    debugfs -w -R "write $src $dst" "$IMAGE_PATH" >/dev/null
    debugfs -w -R "set_inode_field $dst mode 0100755" "$IMAGE_PATH" >/dev/null
    # EXT2_IMMUTABLE_FL: the on-disk carrier of the VFS seal. Program-identity
    # privilege is keyed on a binary's path, so a shipped binary that is not
    # sealed is one any task holding a write descriptor can replace and then
    # spawn into the grant. `lsattr` shows this as `i`.
    debugfs -w -R "set_inode_field $dst flags 0x10" "$IMAGE_PATH" >/dev/null
}

install_file() {
    local src="$1" dst="$2"
    debugfs -w -R "rm $dst" "$IMAGE_PATH" >/dev/null 2>&1 || true
    debugfs -w -R "write $src $dst" "$IMAGE_PATH" >/dev/null
}

mkdir_p() {
    debugfs -w -R "mkdir $1" "$IMAGE_PATH" >/dev/null 2>&1 || true
}

if [ -f "$STAMP_PATH" ] && [ "$PRESERVE_FS_IMAGE" = "1" ] \
   && [ "$(cat "$STAMP_PATH")" = "$(build_stamp)" ] && image_is_preservable; then
    echo "preserve: $IMAGE_PATH is current — leaving it and its contents alone"
    exit 0
fi

if image_is_preservable; then
    echo "preserve: refreshing binaries in $IMAGE_PATH, keeping everything else"
    # A refresh that dies midway must not leave the old stamp, or the next run
    # reports an image missing a binary as current.
    rm -f "$STAMP_PATH"
else
    echo "Rebuilding ext2 image at $IMAGE_PATH ($FS_IMAGE_SIZE)"
    rm -f "$IMAGE_PATH" "$STAMP_PATH"
    truncate -s "$FS_IMAGE_SIZE" "$IMAGE_PATH"
    mkfs.ext2 -F -b 4096 "$IMAGE_PATH" >/dev/null
fi

mkdir_p /bin
mkdir_p /sbin
# Created here rather than left to the first writer: the ext2 root does not
# auto-create parents the way ramfs does, and both roots must agree about
# whether a path is writable. Mirrors gen_initramfs.py's EMPTY_DIRS.
mkdir_p /etc
mkdir_p /var

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

    install_binary "$src" "$dst"
done

# The directories too, on a root userland can write: a sealed binary cannot be
# overwritten, but until now its *directory* could be renamed aside and a
# fresh /bin/halt planted under the path the grant is keyed on. debugfs is not
# subject to the flag, so a preserved image still refreshes in place.
seal_dir() {
    debugfs -w -R "set_inode_field $1 flags 0x10" "$IMAGE_PATH" >/dev/null
}
seal_dir /bin
seal_dir /sbin

# Install font files into /usr/share/fonts/ if assets/fonts/ exists
FONTS_DIR="${REPO_ROOT}/assets/fonts"

mkdir_p /usr
mkdir_p /usr/share

if [ -d "$FONTS_DIR" ]; then
    mkdir_p /usr/share/fonts

    # The OFL license texts ship beside the fonts they cover: the license
    # requires each copy of the font to carry its notice.
    for font in "$FONTS_DIR"/*.ttf "$FONTS_DIR"/*-OFL.txt; do
        [ -f "$font" ] || continue
        fname="$(basename "$font")"
        install_file "$font" "/usr/share/fonts/$fname"
        echo "Installed font asset: /usr/share/fonts/$fname"
    done
fi

mkdir_p /usr/share/slopos
mkdir_p /usr/share/slopos/wallpapers

if [ -f "${REPO_ROOT}/assets/logo.png" ]; then
    install_file "${REPO_ROOT}/assets/logo.png" /usr/share/slopos/wallpapers/default.png
    echo "Installed wallpaper: /usr/share/slopos/wallpapers/default.png"
fi

# Install keyboard layout files into /usr/share/keymaps/
KEYMAPS_DIR="${REPO_ROOT}/assets/keymaps"
if [ -d "$KEYMAPS_DIR" ]; then
    mkdir_p /usr/share/keymaps
    for layout in "$KEYMAPS_DIR"/*.layout; do
        [ -e "$layout" ] || continue
        lname=$(basename "$layout")
        install_file "$layout" "/usr/share/keymaps/$lname"
        echo "Installed keymap: /usr/share/keymaps/$lname"
    done
fi

# Append a block-integrity (verity) trailer so the kernel detects on-disk
# corruption at read time (fs/src/verity.rs). A trailer makes the mount
# read-only, so it must be the LAST step — it hashes the finished image — and
# it must be skipped for an image a boot is expected to write to.
if [ "$VERITY" = "off" ]; then
    echo "verity: VERITY=off — $IMAGE_PATH will mount unverified and writable"
elif command -v python3 >/dev/null 2>&1; then
    python3 "${SCRIPT_DIR}/gen_verity.py" "$IMAGE_PATH"
else
    # Not a warning: a build that silently produced a writable image where a
    # verified one was asked for is the fail-open this trailer exists to end.
    echo "gen_verity: python3 is required to build a verified image (or pass VERITY=off)" >&2
    exit 1
fi

# Last: a stamp for an unfinished build would make the next run skip the work
# that failed.
build_stamp > "$STAMP_PATH"
