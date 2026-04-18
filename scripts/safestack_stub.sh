#!/usr/bin/env bash
set -euo pipefail

# Materialise the SafeStack sanitizer runtime archive that rustc
# auto-links when `-Zsanitizer=safestack` is active on our custom
# `x86_64-slos` target.
#
# Rust's driver unconditionally appends `--whole-archive
# <sysroot>/lib/rustlib/<target>/lib/librustc-nightly_rt.safestack.a
# --no-whole-archive` to the final link line for any crate compiled
# with the safestack sanitizer.  For our bare-metal target that file
# does not ship with the toolchain (rustup only prebuilds it for hosted
# targets).  We use `-C llvm-args=-safestack-use-pointer-address` so
# the instrumented prologues call our own `__safestack_pointer_address`
# instead of touching any runtime symbol — meaning an **empty** static
# archive satisfies the link requirement.  If future toolchains ever
# emit a reference to `__safestack_init` or `__safestack_unsafe_stack_ptr`,
# extend this script to stuff matching stub symbols into the archive.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

RUST_CHANNEL="${RUST_CHANNEL:-$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$REPO_ROOT/rust-toolchain.toml")}"
RUST_TARGET="${RUST_TARGET:-$REPO_ROOT/targets/x86_64-slos.json}"

# Compute the sysroot path rustc expects.  `--print target-libdir` on a
# custom target JSON returns `<sysroot>/lib/rustlib/<target-name>/lib`
# — exactly where rustc will look for the sanitizer runtime archive.
LIBDIR=$(rustc +"$RUST_CHANNEL" -Zunstable-options --print target-libdir --target "$RUST_TARGET")
ARCHIVE_NAME="librustc-nightly_rt.safestack.a"
ARCHIVE_PATH="$LIBDIR/$ARCHIVE_NAME"

mkdir -p "$LIBDIR"

# Prefer llvm-ar for reproducibility; fall back to system ar.
AR="${AR:-}"
if [ -z "$AR" ]; then
    if command -v llvm-ar >/dev/null 2>&1; then
        AR=llvm-ar
    elif command -v ar >/dev/null 2>&1; then
        AR=ar
    else
        echo "error: no archiver available (need llvm-ar or ar)" >&2
        exit 1
    fi
fi

# Build (or rebuild) the archive whenever:
#   - it doesn't exist, or
#   - this script has been touched since the archive was last written.
if [ ! -f "$ARCHIVE_PATH" ] || [ "$0" -nt "$ARCHIVE_PATH" ]; then
    tmpdir=$(mktemp -d)
    trap 'rm -rf "$tmpdir"' EXIT
    # Creating an empty archive with `ar` requires a dummy file;
    # `llvm-ar` can do it directly via `rc`.  Try both forms.
    if ! "$AR" rcs "$ARCHIVE_PATH" 2>/dev/null; then
        : > "$tmpdir/empty.o"
        "$AR" rcs "$ARCHIVE_PATH" "$tmpdir/empty.o"
        "$AR" d "$ARCHIVE_PATH" "empty.o" 2>/dev/null || true
    fi
    echo "safestack_stub: wrote $ARCHIVE_PATH"
else
    echo "safestack_stub: $ARCHIVE_PATH up to date"
fi
