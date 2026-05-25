#!/usr/bin/env bash
set -euo pipefail

# Ensure the Limine bootloader binaries are present.
#
# As of Limine v12.0.0 the project no longer publishes a `vX.x-binary` git
# branch; prebuilt binaries ship as a `limine-binary.tar.xz` asset attached to
# each GitHub release. We download and unpack a pinned release into
# third_party/limine. Offline environments may pre-populate that directory
# (it just needs limine-bios.sys + BOOTX64.EFI) to skip the download.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

LIMINE_DIR="${LIMINE_DIR:-${REPO_ROOT}/third_party/limine}"
LIMINE_VERSION="${LIMINE_VERSION:-12.3.1}"
LIMINE_URL="${LIMINE_URL:-https://github.com/limine-bootloader/limine/releases/download/v${LIMINE_VERSION}/limine-binary.tar.xz}"
# SHA-256 of limine-binary.tar.xz for the pinned version (GitHub release assets
# are immutable). Set to empty to skip integrity verification.
LIMINE_TARBALL_SHA256="${LIMINE_TARBALL_SHA256:-52e84e1d371cdbbeb7bdf01139f33a4bae30a8a6f3d67fccb2ee07d21f8b886b}"

# Already populated (downloaded earlier or pre-staged for offline builds).
if [ -f "$LIMINE_DIR/limine-bios.sys" ] && [ -f "$LIMINE_DIR/BOOTX64.EFI" ]; then
    exit 0
fi

if [ -d "$LIMINE_DIR" ] && [ -n "$(ls -A "$LIMINE_DIR" 2>/dev/null)" ]; then
    echo "Limine directory exists but lacks Limine binaries: $LIMINE_DIR" >&2
    echo "Remove it or point LIMINE_DIR to a valid Limine binary release." >&2
    exit 1
fi

echo "Fetching Limine v${LIMINE_VERSION} binaries..." >&2
mkdir -p "$LIMINE_DIR"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

curl -L --fail --progress-bar "$LIMINE_URL" -o "$TMP/limine-binary.tar.xz"

if [ -n "$LIMINE_TARBALL_SHA256" ]; then
    echo "${LIMINE_TARBALL_SHA256}  ${TMP}/limine-binary.tar.xz" | sha256sum -c - >&2
fi

# The tarball unpacks into a single top-level `limine-binary/` directory; flatten
# its contents into LIMINE_DIR.
tar -xf "$TMP/limine-binary.tar.xz" -C "$TMP"
SRC="$TMP/limine-binary"
if [ ! -d "$SRC" ]; then
    SRC="$(find "$TMP" -maxdepth 1 -type d -name 'limine*' | head -n1)"
fi
if [ -z "$SRC" ] || [ ! -f "$SRC/limine-bios.sys" ]; then
    echo "Unexpected Limine binary tarball layout under $TMP" >&2
    exit 1
fi

cp -a "$SRC"/. "$LIMINE_DIR"/

echo "Limine v${LIMINE_VERSION} ready in $LIMINE_DIR" >&2
