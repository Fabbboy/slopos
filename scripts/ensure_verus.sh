#!/usr/bin/env bash
set -euo pipefail

# Ensure the pinned Verus toolchain is present under third_party/verus.
#
# The pin (release tag, commit, asset URL, sha256, and the Rust toolchain
# Verus itself requires) lives in verification/verus.toml — the single
# source of truth. This script parses that file,
# downloads the prebuilt x86_64-linux release asset, verifies its sha256
# (release assets are immutable; same integrity pattern as
# scripts/ensure_limine.sh), unpacks it, and installs the Rust toolchain
# Verus links against (with the rustc-dev + llvm-tools components).
#
# Offline / pre-staged environments may populate third_party/verus
# directly (it just needs a runnable `verus` launcher) to skip the
# download. Non-x86_64-linux hosts must build Verus from the pinned commit;
# see verification/README.md.
#
# Output: prints the absolute path to the `verus` launcher on the last
# line so callers can `VERUS_BIN="$(scripts/ensure_verus.sh)"`.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

VERUS_TOML="${VERUS_TOML:-${REPO_ROOT}/verification/verus.toml}"
VERUS_DIR="${VERUS_DIR:-${REPO_ROOT}/third_party/verus}"

if [ ! -f "$VERUS_TOML" ]; then
    echo "ensure_verus: pin file not found: $VERUS_TOML" >&2
    exit 1
fi

# Minimal TOML key reader: pulls `key = "value"` (string) from the flat
# file. Good enough for verus.toml's shape; avoids a Python/jq dependency
# on the build host. Returns the first match across all sections.
toml_get() {
    local key="$1"
    sed -n -E "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*\"([^\"]*)\".*/\1/p" \
        "$VERUS_TOML" | head -n1
}

VERUS_VERSION="$(toml_get version)"
VERUS_COMMIT="$(toml_get commit)"
VERUS_RUST_TOOLCHAIN="$(toml_get rust_toolchain)"
VERUS_URL="$(toml_get url)"
VERUS_SHA256="$(toml_get sha256)"

if [ -z "$VERUS_URL" ] || [ -z "$VERUS_VERSION" ]; then
    echo "ensure_verus: verus.toml missing url/version; cannot proceed" >&2
    exit 1
fi

launcher="$VERUS_DIR/verus"

ensure_rust_toolchain() {
    # Verus links against the exact Rust toolchain it was built with,
    # including the rustc-dev + llvm-tools components. Install on demand;
    # this is independent of the kernel's nightly toolchain.
    [ -z "$VERUS_RUST_TOOLCHAIN" ] && return 0
    if ! command -v rustup >/dev/null 2>&1; then
        echo "ensure_verus: rustup not found; cannot install Rust ${VERUS_RUST_TOOLCHAIN} for Verus" >&2
        echo "  Install rustup or pre-stage the toolchain, then re-run." >&2
        return 1
    fi
    if ! rustup toolchain list 2>/dev/null | grep -q "^${VERUS_RUST_TOOLCHAIN}-"; then
        echo "ensure_verus: installing Rust ${VERUS_RUST_TOOLCHAIN} (Verus host toolchain)..." >&2
        rustup toolchain install "$VERUS_RUST_TOOLCHAIN" --profile minimal >&2
    fi
    # rustc-dev + llvm-tools are required by the verifier; adding an
    # already-present component is a no-op.
    rustup component add rustc-dev llvm-tools --toolchain "$VERUS_RUST_TOOLCHAIN" >&2
}

# Already populated (downloaded earlier or pre-staged for offline builds).
if [ -x "$launcher" ]; then
    ensure_rust_toolchain
    echo "$launcher"
    exit 0
fi

if [ -d "$VERUS_DIR" ] && [ -n "$(ls -A "$VERUS_DIR" 2>/dev/null)" ]; then
    echo "ensure_verus: $VERUS_DIR exists but has no runnable 'verus' launcher." >&2
    echo "  Remove it or pre-stage a valid Verus release there." >&2
    exit 1
fi

echo "ensure_verus: fetching Verus ${VERUS_VERSION} (commit ${VERUS_COMMIT:0:12})..." >&2
mkdir -p "$VERUS_DIR"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

curl -L --fail --progress-bar "$VERUS_URL" -o "$TMP/verus.zip"

if [ -n "$VERUS_SHA256" ]; then
    echo "${VERUS_SHA256}  ${TMP}/verus.zip" | sha256sum -c - >&2
fi

if command -v unzip >/dev/null 2>&1; then
    unzip -q "$TMP/verus.zip" -d "$TMP/unpacked"
else
    echo "ensure_verus: 'unzip' not found; install it (apt-get install unzip)." >&2
    exit 1
fi

# The asset unpacks into a single top-level `verus-x86-linux/` directory;
# flatten its contents into VERUS_DIR.
SRC="$(find "$TMP/unpacked" -maxdepth 1 -type d -name 'verus-*' | head -n1)"
if [ -z "$SRC" ]; then
    # Some assets may unpack flat; fall back to the unpack root.
    SRC="$TMP/unpacked"
fi
cp -a "$SRC"/. "$VERUS_DIR"/

if [ ! -x "$launcher" ]; then
    echo "ensure_verus: unpacked archive but '$launcher' is missing or not executable." >&2
    exit 1
fi

ensure_rust_toolchain

echo "ensure_verus: Verus ${VERUS_VERSION} ready at $VERUS_DIR" >&2
echo "$launcher"
