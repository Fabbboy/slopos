#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
STD_PAL_SRC="$REPO_ROOT/slibc/std_pal"

# Resolve sysroot from the active toolchain (follows rust-toolchain.toml).
# This avoids hardcoding a specific nightly date so the patch survives
# toolchain upgrades without manual edits.
RUST_CHANNEL="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "${REPO_ROOT}/rust-toolchain.toml")"
SYSROOT="$(rustc +"$RUST_CHANNEL" --print sysroot 2>/dev/null || rustc --print sysroot)"
STD_SYS="$SYSROOT/lib/rustlib/src/rust/library/std/src/sys"

sed_in_place() {
    local file="${@: -1}"
    sed -i.bak "$@"
    rm -f "$file.bak"
}

perl_in_place() {
    local script="$1"
    local file="$2"
    perl -0777 -i.bak -pe "$script" "$file"
    rm -f "$file.bak"
}

slopos_before_no_threads() {
    local file="$1"
    awk '
        /mod no_threads;/ { done = 1; exit found ? 0 : 1 }
        /target_os = "slopos"/ { found = 1 }
        END { if (!done) exit found ? 0 : 1 }
    ' "$file"
}

remove_slopos_before_no_threads() {
    local file="$1"
    perl -i.bak -ne 'if (!$seen && /^\s*target_os = "slopos",$/) { next } print; $seen = 1 if /mod no_threads;/' "$file"
    rm -f "$file.bak"
}

if [ ! -d "$STD_SYS" ]; then
    echo "ERROR: Rust std source not found at $STD_SYS"
    echo "Run: rustup component add rust-src"
    exit 1
fi

# Don't add an outer idempotency marker — a stale marker would prevent
# new patch sections from applying to already-patched sysroots.  Every
# section below is individually idempotent via its own `grep -q` guard,
# and the post-patch verification at the bottom catches drift.  Strip
# any legacy marker so old installations recover.
MARKER="$STD_SYS/.slopos_patched"
if [ -f "$MARKER" ]; then
    rm -f "$MARKER"
fi

echo "Patching Rust std source for SlopOS target..."

# 1. Copy PAL files
mkdir -p "$STD_SYS/pal/slopos"
cp "$STD_PAL_SRC/pal/slopos/mod.rs"   "$STD_SYS/pal/slopos/mod.rs"
cp "$STD_PAL_SRC/pal/slopos/os.rs"    "$STD_SYS/pal/slopos/os.rs"
# A futex.rs from an older checkout would be dead but compiled; remove it.
rm -f "$STD_SYS/pal/slopos/futex.rs"
echo "  Copied pal/slopos/"

# 2. Copy sys module files
cp "$STD_PAL_SRC/alloc/slopos.rs"   "$STD_SYS/alloc/slopos.rs"
cp "$STD_PAL_SRC/args/slopos.rs"    "$STD_SYS/args/slopos.rs"
cp "$STD_PAL_SRC/env/slopos.rs"     "$STD_SYS/env/slopos.rs"
cp "$STD_PAL_SRC/stdio/slopos.rs"   "$STD_SYS/stdio/slopos.rs"
cp "$STD_PAL_SRC/thread/slopos.rs"  "$STD_SYS/thread/slopos.rs"
cp "$STD_PAL_SRC/time/slopos.rs"    "$STD_SYS/time/slopos.rs"
cp "$STD_PAL_SRC/random/slopos.rs"  "$STD_SYS/random/slopos.rs"
cp "$STD_PAL_SRC/pipe/slopos.rs"    "$STD_SYS/pipe/slopos.rs"
echo "  Copied sys module files"

# Copy fs and process if they exist
if [ -f "$STD_PAL_SRC/fs/slopos.rs" ]; then
    cp "$STD_PAL_SRC/fs/slopos.rs" "$STD_SYS/fs/slopos.rs"
    echo "  Copied fs/slopos.rs"
fi
if [ -f "$STD_PAL_SRC/process/slopos.rs" ]; then
    cp "$STD_PAL_SRC/process/slopos.rs" "$STD_SYS/process/slopos.rs"
    echo "  Copied process/slopos.rs"
fi

# 3. Patch routing in mod.rs files using sed
# Each patch adds a `target_os = "slopos"` arm BEFORE the fallback `_ =>` arm

patch_cfg_select() {
    local file="$1"
    local module_name="$2"
    local use_clause="$3"

    if grep -q 'target_os = "slopos"' "$file" 2>/dev/null; then
        echo "  $file already patched"
        return
    fi

    # Insert slopos arm before the `_ =>` fallback
    sed_in_place '/^[[:space:]]*_ => {/{
i\
    target_os = "slopos" => {\
        mod '"$module_name"';\
        pub use '"$module_name"'::'"$use_clause"';\
    }
}' "$file"
    echo "  Patched $file"
}

# Patch that adds slopos to an existing multi-target arm (for futex-based sync)
add_to_futex_arm() {
    local file="$1"
    if grep -q 'target_os = "slopos"' "$file" 2>/dev/null; then
        echo "  $file already patched"
        return
    fi
    # Add target_os = "slopos" after target_os = "hermit" in the futex arm
    sed_in_place 's/target_os = "hermit",/target_os = "hermit",\
        target_os = "slopos",/' "$file"
    echo "  Patched $file (futex arm)"
}

# 3a. PAL routing
if ! grep -q 'target_os = "slopos"' "$STD_SYS/pal/mod.rs" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        pub use self::slopos::*;\
    }
}' "$STD_SYS/pal/mod.rs"
    echo "  Patched pal/mod.rs"
fi

# 3b. Alloc routing. Matched through the zkvm arm's closing brace rather than by
# line count, which lands the arm inside zkvm when upstream adds a line to it.
# Every arm must bind `imp`: mod.rs re-exports `imp::{alloc, dealloc, realloc}`
# and owns the GlobalAlloc impl itself.
if ! grep -q 'target_os = "slopos"' "$STD_SYS/alloc/mod.rs" 2>/dev/null; then
    perl_in_place 's/(target_os = "zkvm" => \{\n(?:[^\n]*\n)*?    \})/$1\n    target_os = "slopos" => {\n        mod slopos;\n        use slopos as imp;\n    }/' "$STD_SYS/alloc/mod.rs"
    if ! grep -q 'target_os = "slopos"' "$STD_SYS/alloc/mod.rs" 2>/dev/null; then
        echo "ERROR: alloc/mod.rs zkvm arm did not match; upstream shape changed"
        exit 1
    fi
    echo "  Patched alloc/mod.rs"
fi

# 3b'. Futex routing (sys/sync/futex/<os>.rs since 2026-09-03). Its cfg_select!
# fallback is `_ => {}`, so an unrouted target exports nothing at all.
cp "$STD_PAL_SRC/sync_futex_slopos.rs" "$STD_SYS/sync/futex/slopos.rs"
echo "  Copied sync/futex/slopos.rs"
if ! grep -q 'target_os = "slopos"' "$STD_SYS/sync/futex/mod.rs" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {}/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        pub use slopos::*;\
    }
}' "$STD_SYS/sync/futex/mod.rs"
    echo "  Patched sync/futex/mod.rs"
fi

# 3c. Individual module routing with `_ =>` fallback
patch_cfg_select "$STD_SYS/stdio/mod.rs" "slopos" "*"

# Time uses `use ... as imp;` pattern (pub use imp::{...} outside cfg_select!)
if ! grep -q 'target_os = "slopos"' "$STD_SYS/time/mod.rs" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        use slopos as imp;\
    }
}' "$STD_SYS/time/mod.rs"
    echo "  Patched time/mod.rs"
fi

# Thread needs specific exports
if ! grep -q 'target_os = "slopos"' "$STD_SYS/thread/mod.rs" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        pub use slopos::{Thread, available_parallelism, sleep, yield_now, DEFAULT_MIN_STACK_SIZE};\
        #[expect(dead_code)]\
        mod unsupported;\
        pub use unsupported::{current_os_id, set_name};\
    }
}' "$STD_SYS/thread/mod.rs"
    echo "  Patched thread/mod.rs"
fi

# Args routing
if ! grep -q 'target_os = "slopos"' "$STD_SYS/args/mod.rs" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        pub use slopos::*;\
    }
}' "$STD_SYS/args/mod.rs"
    echo "  Patched args/mod.rs"
fi

# Env routing
patch_cfg_select "$STD_SYS/env/mod.rs" "slopos" "*"

# Pipe routing
if ! grep -q 'target_os = "slopos"' "$STD_SYS/pipe/mod.rs" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        pub use slopos::{Pipe, pipe};\
    }
}' "$STD_SYS/pipe/mod.rs"
    echo "  Patched pipe/mod.rs"
fi

# Random routing (has `_ => {}` not `_ => { mod unsupported; }`)
if ! grep -q 'target_os = "slopos"' "$STD_SYS/random/mod.rs" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {}$/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        pub use slopos::fill_bytes;\
    }
}' "$STD_SYS/random/mod.rs"
    echo "  Patched random/mod.rs"
fi

# FS routing (if fs/slopos.rs exists)
if [ -f "$STD_SYS/fs/slopos.rs" ]; then
    if ! grep -q 'target_os = "slopos"' "$STD_SYS/fs/mod.rs" 2>/dev/null; then
        sed_in_place '/^[[:space:]]*_ => {/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        use slopos as imp;\
    }
}' "$STD_SYS/fs/mod.rs"
        echo "  Patched fs/mod.rs"
    fi
fi

# Process routing (if process/slopos.rs exists)
if [ -f "$STD_SYS/process/slopos.rs" ]; then
    if ! grep -q 'target_os = "slopos"' "$STD_SYS/process/mod.rs" 2>/dev/null; then
        sed_in_place '/^[[:space:]]*_ => {/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        use slopos as imp;\
    }
}' "$STD_SYS/process/mod.rs"
        echo "  Patched process/mod.rs"
    fi
fi

# 3d. Sync modules — add slopos to futex arms
add_to_futex_arm "$STD_SYS/sync/mutex/mod.rs"
add_to_futex_arm "$STD_SYS/sync/condvar/mod.rs"
add_to_futex_arm "$STD_SYS/sync/rwlock/mod.rs"
add_to_futex_arm "$STD_SYS/sync/once/mod.rs"
add_to_futex_arm "$STD_SYS/sync/thread_parking/mod.rs"

# 3e. env_consts.rs — match only the standalone #[else] (not the one inside macro def)
ENV_CONSTS="$STD_SYS/env_consts.rs"
if ! grep -q 'target_os = "slopos"' "$ENV_CONSTS" 2>/dev/null; then
    sed_in_place '/^#\[else\]$/i\
#[cfg(target_os = "slopos")]\
pub mod os {\
    pub const FAMILY: \&str = "";\
    pub const OS: \&str = "slopos";\
    pub const DLL_PREFIX: \&str = "";\
    pub const DLL_SUFFIX: \&str = "";\
    pub const DLL_EXTENSION: \&str = "";\
    pub const EXE_SUFFIX: \&str = "";\
    pub const EXE_EXTENSION: \&str = "";\
}\
' "$ENV_CONSTS"
    echo "  Patched env_consts.rs"
fi

# 3f. thread_local — slopos uses the compiler-native (#[thread_local])
#     storage backed by FS_BASE variant-II per-thread TLS, so it does NOT
#     join the no_threads arm. The userland target json sets
#     has-thread-local:true + tls-model:local-exec, so by NOT routing slopos
#     into the no_threads storage arm, cfg_select! falls through to the
#     `target_thread_local => native` arm automatically — one real per-thread
#     cell per OS thread, which is what slibc's pthread/CLONE_SETTLS path wires.
#     We still add slopos to the guard hermit/xous no-op arm: std runs the
#     native thread-local destructors itself, so guard::enable() is a no-op.
TL_MOD="$STD_SYS/thread_local/mod.rs"
# Self-heal: strip any stale slopos line from the no_threads storage arm
# (the first cfg_select! arm, which ends at `mod no_threads;`). Older
# revisions of this script routed slopos there; leaving it in place would
# collapse every OS thread onto one process-global cell and silently mask the
# native (FS_BASE) arm — mirrors the io/error legacy-strip below.
if slopos_before_no_threads "$TL_MOD"; then
    remove_slopos_before_no_threads "$TL_MOD"
    echo "  Removed stale slopos entry from no_threads storage arm in thread_local/mod.rs"
fi
# Matched as a whole set with flexible whitespace: upstream reflows this arm
# between one line and one-target-per-line, which breaks a line-oriented anchor.
if ! grep -q 'target_os = "slopos"' "$TL_MOD" 2>/dev/null; then
    perl_in_place 's/any\(\s*target_os = "hermit",\s*target_os = "xous",?\s*\)/any(target_os = "hermit", target_os = "xous", target_os = "slopos")/' "$TL_MOD"
    if ! grep -q 'target_os = "slopos"' "$TL_MOD" 2>/dev/null; then
        echo "ERROR: thread_local/mod.rs guard no-op arm did not match; upstream shape changed"
        echo "       file: $TL_MOD"
        exit 1
    fi
    echo "  Patched thread_local/mod.rs"
fi

# 3g. io/error — install a real SlopOS errno decoder.
#
# The upstream fallback (io/error/generic.rs) maps every errno to
# `ErrorKind::Uncategorized`, which makes `e.kind()` useless in
# userland (curl, nc, std tests). We install a dedicated slopos.rs
# decoder mirroring sys/io/error/unix.rs's shape but keyed on the
# numeric errnos defined in slopos-abi::syscall::errno_defs.
IO_ERROR="$STD_SYS/io/error/mod.rs"
IO_ERROR_SLOPOS="$STD_SYS/io/error/slopos.rs"

cp "$STD_PAL_SRC/io_error/slopos.rs" "$IO_ERROR_SLOPOS"
echo "  Copied io/error/slopos.rs"

# Strip any previous (legacy) patch that merely added slopos to the
# generic fallback arm — leaving it in place would dead-code the new
# dedicated arm because cfg_select! picks the first matching branch.
if grep -q '^[[:space:]]*target_os = "slopos",$' "$IO_ERROR" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*target_os = "slopos",$/d' "$IO_ERROR"
    echo "  Removed legacy slopos entry from generic fallback in io/error/mod.rs"
fi

# Insert a dedicated slopos arm. Anchor before the existing motor
# arm so it lands at the top of the cfg_select! and wins.
if ! grep -q '^[[:space:]]*target_os = "slopos" => {' "$IO_ERROR" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*target_os = "motor" => {$/i\
    target_os = "slopos" => {\
        mod slopos;\
        pub use slopos::*;\
    }' "$IO_ERROR"
    echo "  Patched io/error/mod.rs with dedicated slopos arm"
fi

# 3h. sys/exit.rs — route slopos exit() to PAL instead of the intrinsics::abort() fallback.
#     Anchored on `fn exit`'s `_ =>` fallback, which the slopos arm must precede:
#     placed after the wildcard it is dead and exit() compiles to a ud2.
EXIT_RS="$STD_SYS/exit.rs"
if [ -f "$EXIT_RS" ] && ! grep -q 'target_os = "slopos"' "$EXIT_RS" 2>/dev/null; then
    perl_in_place 's/(\n        )(_ => \{\n            let _ = code;\n            crate::intrinsics::abort\(\)\n        \})/$1target_os = "slopos" => {\n            crate::sys::pal::os::exit(code)\n        }$1$2/' "$EXIT_RS"
    if ! grep -q 'target_os = "slopos"' "$EXIT_RS" 2>/dev/null; then
        echo "ERROR: exit.rs fallback arm did not match; upstream shape changed"
        exit 1
    fi
    echo "  Patched exit.rs"
fi

# 3i. Net routing — wire std::net through SlopOS socket layer
if [ -f "$STD_PAL_SRC/net/slopos.rs" ]; then
    cp "$STD_PAL_SRC/net/slopos.rs" "$STD_SYS/net/connection/socket/slopos.rs"
    echo "  Copied net/connection/socket/slopos.rs"
fi

# Patch connection/socket/mod.rs — add slopos arm before `_ => {}`
SOCK_MOD="$STD_SYS/net/connection/socket/mod.rs"
if [ -f "$SOCK_MOD" ] && ! grep -q 'target_os = "slopos"' "$SOCK_MOD" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {}$/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        pub use slopos::*;\
    }
}' "$SOCK_MOD"
    echo "  Patched net/connection/socket/mod.rs"
fi

# Patch connection/mod.rs — add slopos to the socket-based arm
CONN_MOD="$STD_SYS/net/connection/mod.rs"
if [ -f "$CONN_MOD" ] && ! grep -q 'target_os = "slopos"' "$CONN_MOD" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {/{
i\
    target_os = "slopos" => {\
        mod socket;\
        pub use socket::*;\
    }
}' "$CONN_MOD"
    echo "  Patched net/connection/mod.rs"
fi

# Patch hostname/mod.rs — use unsupported (returns error, acceptable)
HOST_MOD="$STD_SYS/net/hostname/mod.rs"
if [ -f "$HOST_MOD" ] && ! grep -q 'target_os = "slopos"' "$HOST_MOD" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {/{
i\
    target_os = "slopos" => {\
        mod unsupported;\
        pub use unsupported::hostname;\
    }
}' "$HOST_MOD"
    echo "  Patched net/hostname/mod.rs"
fi

# 3j. File descriptor abstraction — sys::fd::FileDesc
if [ -f "$STD_PAL_SRC/fd/slopos.rs" ]; then
    cp "$STD_PAL_SRC/fd/slopos.rs" "$STD_SYS/fd/slopos.rs"
    echo "  Copied fd/slopos.rs"
fi

FD_MOD="$STD_SYS/fd/mod.rs"
if [ -f "$FD_MOD" ] && ! grep -q 'target_os = "slopos"' "$FD_MOD" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {}$/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        pub use slopos::*;\
    }
}' "$FD_MOD"
    echo "  Patched fd/mod.rs"
fi

# 3k. Enable os::fd + patch raw.rs/owned.rs for SlopOS
#
# These files have complex multi-line cfg blocks that sed can't reliably
# handle. We use targeted string replacements instead.
STD_OS="$SYSROOT/lib/rustlib/src/rust/library/std/src/os"

patch_os_fd() {
    local OS_MOD="$STD_OS/mod.rs"
    local RAW="$STD_OS/fd/raw.rs"
    local OWNED="$STD_OS/fd/owned.rs"

    # -- os/mod.rs: add slopos to os::fd gate --
    if ! grep -q 'target_os = "slopos"' "$OS_MOD" 2>/dev/null; then
        sed_in_place 's/target_os = "motor",/target_os = "motor",\
    target_os = "slopos",/' "$OS_MOD"
    fi

    # -- raw.rs: RawFd = i32 for slopos --
    # Widen the hermit|motor arm to include slopos
    if ! grep -q 'any(target_os = "hermit", target_os = "motor", target_os = "slopos")' "$RAW" 2>/dev/null; then
        sed_in_place 's/any(target_os = "hermit", target_os = "motor")/any(target_os = "hermit", target_os = "motor", target_os = "slopos")/' "$RAW"
    fi
    # Exclude slopos from the raw::c_int arm and the os::raw import
    if ! grep -q 'all(not(target_os = "hermit"), not(target_os = "motor"), not(target_os = "slopos"))' "$RAW" 2>/dev/null; then
        sed_in_place 's/all(not(target_os = "hermit"), not(target_os = "motor"))/all(not(target_os = "hermit"), not(target_os = "motor"), not(target_os = "slopos"))/' "$RAW"
    fi
    # Add slopos to the motor OwnedFd import (NOT the moto_rt::libc import)
    if ! grep -q '#\[cfg(any(target_os = "motor", target_os = "slopos"))\]' "$RAW" 2>/dev/null; then
        perl_in_place 's/#\[cfg\(target_os = "motor"\)\]\nuse super::owned::OwnedFd;/#[cfg(any(target_os = "motor", target_os = "slopos"))]\nuse super::owned::OwnedFd;/' "$RAW"
    fi
    # Replace libc stdio constants with slopos-safe cfg-gated versions
    if ! grep -q '#\[cfg(target_os = "slopos")\] { 0 }' "$RAW" 2>/dev/null; then
        perl_in_place 's/libc::STDIN_FILENO/#[cfg(target_os = "slopos")] { 0 }\n        #[cfg(not(target_os = "slopos"))] { libc::STDIN_FILENO }/g' "$RAW"
    fi
    if ! grep -q '#\[cfg(target_os = "slopos")\] { 1 }' "$RAW" 2>/dev/null; then
        perl_in_place 's/libc::STDOUT_FILENO/#[cfg(target_os = "slopos")] { 1 }\n        #[cfg(not(target_os = "slopos"))] { libc::STDOUT_FILENO }/g' "$RAW"
    fi
    if ! grep -q '#\[cfg(target_os = "slopos")\] { 2 }' "$RAW" 2>/dev/null; then
        perl_in_place 's/libc::STDERR_FILENO/#[cfg(target_os = "slopos")] { 2 }\n        #[cfg(not(target_os = "slopos"))] { libc::STDERR_FILENO }/g' "$RAW"
    fi

    # -- owned.rs: try_clone_to_owned + Drop for slopos --
    # 1) Exclude slopos from the libc-based try_clone_to_owned and cvt import
    #    (both cfg blocks contain "target_os = "trusty"," so this adds slopos to both)
    if ! grep -q 'target_os = "slopos"' "$OWNED" 2>/dev/null; then
        sed_in_place 's/target_os = "trusty",/target_os = "trusty",\
        target_os = "slopos",/' "$OWNED"
    fi

    # 2) Add slopos try_clone_to_owned block after motor's
    #    Anchor on map_motor_error (unique to motor impl), find its closing }, append after
    if ! grep -q 'fn dup(fd: i32) -> i32' "$OWNED" 2>/dev/null; then
        sed_in_place '/map_motor_error/,/^    }/{
        /^    }/a\
\
    #[cfg(target_os = "slopos")]\
    #[stable(feature = "io_safety", since = "1.63.0")]\
    pub fn try_clone_to_owned(&self) -> io::Result<OwnedFd> {\
        unsafe extern "C" { fn dup(fd: i32) -> i32; }\
        let fd = crate::sys::cvt(unsafe { dup(self.as_raw_fd()) })?;\
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })\
    }
    }' "$OWNED"
    fi

    # 3) Patch Drop: exclude slopos from the #[cfg(not(target_os = "hermit"))] close block
    #    and add a slopos close block
    if ! grep -q 'not(any(target_os = "hermit", target_os = "slopos"))' "$OWNED" 2>/dev/null; then
        sed_in_place 's/#\[cfg(not(target_os = "hermit"))\]/#[cfg(not(any(target_os = "hermit", target_os = "slopos")))]/' "$OWNED"
    fi
    # Add slopos close block after the hermit close
    if ! grep -q 'fn close(fd: i32) -> i32' "$OWNED" 2>/dev/null; then
        sed_in_place '/hermit_abi::close(self.fd.as_inner());/a\
            #[cfg(target_os = "slopos")]\
            {\
                unsafe extern "C" { fn close(fd: i32) -> i32; }\
                let _ = unsafe { close(self.fd.as_inner()) };\
            }' "$OWNED"
    fi

    # 4) Exclude slopos from the cvt import (we have our own cvt signature)
    # The gate is: #[cfg(not(any(target_arch = "wasm32", target_env = "sgx", target_os = "hermit", target_os = "trusty", target_os = "motor")))]
    # We already added slopos to this list via step 1 (same cfg block)

    echo "  Checked os/mod.rs, os/fd/raw.rs, os/fd/owned.rs"
}

patch_os_fd

# 3l. Path/cwd routing (sys/paths) — wire std::env::{current_dir,
# set_current_dir, temp_dir} through SlopOS getcwd/chdir.
#
# Upstream std relocated `getcwd`/`chdir` out of sys::pal::<os>::os into the
# dedicated sys::paths module. A target without a `paths` arm silently falls
# back to `unsupported`, which is exactly why `set_current_dir` used to always
# fail on SlopOS even though the kernel chdir/getcwd syscalls work. We model
# the slopos arm on wasi's: getcwd/chdir/temp_dir from our module, the rest
# (current_exe, split/join_paths, home_dir) from the unsupported stub.
if [ -f "$STD_PAL_SRC/paths/slopos.rs" ]; then
    cp "$STD_PAL_SRC/paths/slopos.rs" "$STD_SYS/paths/slopos.rs"
    echo "  Copied paths/slopos.rs"
fi

PATHS_MOD="$STD_SYS/paths/mod.rs"
if [ -f "$PATHS_MOD" ] && ! grep -q 'target_os = "slopos"' "$PATHS_MOD" 2>/dev/null; then
    sed_in_place '/^[[:space:]]*_ => {/{
i\
    target_os = "slopos" => {\
        mod slopos;\
        #[expect(dead_code)]\
        mod unsupported;\
        mod imp {\
            pub use super::slopos::{getcwd, chdir, temp_dir};\
            pub use super::unsupported::{current_exe, SplitPaths, split_paths, JoinPathsError, join_paths, home_dir};\
        }\
    }
}' "$PATHS_MOD"
    echo "  Patched paths/mod.rs"
fi

# Post-patch verification.  Each new patch section above needs a
# matching check_patched row so drift fails the build loudly instead
# of leaving the sysroot half-patched.
failed=0
check_patched() {
    local label="$1"
    local file="$2"
    local needle="$3"
    if ! grep -q "$needle" "$file" 2>/dev/null; then
        echo "  MISSING PATCH: $label"
        echo "    file  : $file"
        echo "    needle: $needle"
        failed=1
    fi
}

# cfg_select! takes the first matching arm, so a slopos arm after the `_`
# wildcard is dead and the target silently gets the fallback. That is not a
# build error and presence-grepping cannot see it; it shipped once as a ud2 in
# std::process::exit, reached only after a process had done its work.
check_arm_precedes_fallback() {
    local label="$1"
    local file="$2"
    if [ ! -f "$file" ]; then
        return
    fi
    # Compare within each cfg_select! block: a file may hold several, and a
    # fallback in an earlier block says nothing about an arm in a later one.
    local report
    report=$(awk '
        /cfg_select!/ { block++; fallback[block] = 0 }
        block == 0 { next }
        /^[[:space:]]*_ =>/ { if (!fallback[block]) fallback[block] = NR }
        /target_os = "slopos"/ {
            if (fallback[block] && NR > fallback[block]) {
                print NR " " fallback[block]
                exit
            }
        }
    ' "$file")
    if [ -n "$report" ]; then
        echo "  DEAD ARM: $label — slopos arm (line ${report% *}) follows the '_ =>' fallback (line ${report#* }) in the same cfg_select!"
        echo "    file  : $file"
        echo "    the arm is unreachable; slopos silently gets the fallback"
        failed=1
    fi
}

# Core PAL + routing surfaces
check_patched "pal/mod.rs"                     "$STD_SYS/pal/mod.rs"                        'target_os = "slopos"'
check_patched "alloc/mod.rs"                   "$STD_SYS/alloc/mod.rs"                      'use slopos as imp;'
check_patched "sync/futex/mod.rs"              "$STD_SYS/sync/futex/mod.rs"                 'target_os = "slopos"'
check_patched "sync/futex/slopos.rs"           "$STD_SYS/sync/futex/slopos.rs"              'pub fn futex_wait'
check_patched "args/mod.rs"                    "$STD_SYS/args/mod.rs"                       'target_os = "slopos"'
check_patched "env/mod.rs"                     "$STD_SYS/env/mod.rs"                        'target_os = "slopos"'
check_patched "stdio/mod.rs"                   "$STD_SYS/stdio/mod.rs"                      'target_os = "slopos"'
check_patched "time/mod.rs"                    "$STD_SYS/time/mod.rs"                       'target_os = "slopos"'
check_patched "thread/mod.rs"                  "$STD_SYS/thread/mod.rs"                     'target_os = "slopos"'
check_patched "pipe/mod.rs"                    "$STD_SYS/pipe/mod.rs"                       'target_os = "slopos"'
check_patched "random/mod.rs"                  "$STD_SYS/random/mod.rs"                     'target_os = "slopos"'
check_patched "fs/mod.rs"                      "$STD_SYS/fs/mod.rs"                         'target_os = "slopos"'
check_patched "process/mod.rs"                 "$STD_SYS/process/mod.rs"                    'target_os = "slopos"'
# thread_local: slopos must ride the guard no-op enable() arm (native TLS runs
# its own destructors) and must NOT be in the no_threads storage arm — that
# would route every OS thread to one process-global cell. Verify both
# directions so a regression that re-adds slopos to no_threads fails loudly.
check_thread_local_native() {
    local file="$STD_SYS/thread_local/mod.rs"
    # Positive: slopos must still be present (in the guard no-op enable() arm).
    if ! grep -q 'target_os = "slopos"' "$file" 2>/dev/null; then
        echo "  MISSING PATCH: thread_local/mod.rs guard no-op enable() arm (slopos)"
        echo "    file  : $file"
        failed=1
    fi
    # Negative: the first cfg_select! arm (the no_threads storage arm, which
    # ends at `mod no_threads;`) must NOT mention slopos.
    if slopos_before_no_threads "$file"; then
        echo "  STALE PATCH: thread_local/mod.rs routes slopos into the no_threads storage arm"
        echo "    file  : $file"
        echo "    expected slopos to fall through to the native (FS_BASE) arm"
        failed=1
    fi
}
check_thread_local_native
check_patched "io/error/mod.rs"                "$STD_SYS/io/error/mod.rs"                   'target_os = "slopos" => {'
check_patched "io/error/slopos.rs"             "$STD_SYS/io/error/slopos.rs"                'fn decode_error_kind'
check_patched "io/error/slopos.rs"             "$STD_SYS/io/error/slopos.rs"                'fn format_error'
check_patched "exit.rs"                        "$STD_SYS/exit.rs"                           'target_os = "slopos"'
check_arm_precedes_fallback "exit.rs"          "$STD_SYS/exit.rs"
check_arm_precedes_fallback "stdio/mod.rs"     "$STD_SYS/stdio/mod.rs"
check_arm_precedes_fallback "time/mod.rs"      "$STD_SYS/time/mod.rs"
check_arm_precedes_fallback "thread/mod.rs"    "$STD_SYS/thread/mod.rs"
check_arm_precedes_fallback "pipe/mod.rs"      "$STD_SYS/pipe/mod.rs"
check_arm_precedes_fallback "random/mod.rs"    "$STD_SYS/random/mod.rs"
check_arm_precedes_fallback "fs/mod.rs"        "$STD_SYS/fs/mod.rs"
check_arm_precedes_fallback "process/mod.rs"   "$STD_SYS/process/mod.rs"
check_arm_precedes_fallback "args/mod.rs"      "$STD_SYS/args/mod.rs"
check_arm_precedes_fallback "env/mod.rs"       "$STD_SYS/env/mod.rs"
check_arm_precedes_fallback "sync/futex/mod.rs" "$STD_SYS/sync/futex/mod.rs"
check_arm_precedes_fallback "net/connection/socket/mod.rs" "$STD_SYS/net/connection/socket/mod.rs"
check_patched "env_consts.rs"                  "$STD_SYS/env_consts.rs"                     'target_os = "slopos"'
check_patched "sync/mutex/mod.rs"              "$STD_SYS/sync/mutex/mod.rs"                 'target_os = "slopos"'
check_patched "sync/condvar/mod.rs"            "$STD_SYS/sync/condvar/mod.rs"               'target_os = "slopos"'
check_patched "sync/rwlock/mod.rs"             "$STD_SYS/sync/rwlock/mod.rs"                'target_os = "slopos"'
check_patched "sync/once/mod.rs"               "$STD_SYS/sync/once/mod.rs"                  'target_os = "slopos"'
check_patched "sync/thread_parking/mod.rs"     "$STD_SYS/sync/thread_parking/mod.rs"        'target_os = "slopos"'

check_patched "net/connection/mod.rs"          "$STD_SYS/net/connection/mod.rs"             'target_os = "slopos"'
check_patched "net/connection/socket/mod.rs"   "$STD_SYS/net/connection/socket/mod.rs"      'target_os = "slopos"'
check_patched "net/connection/socket/slopos.rs" "$STD_SYS/net/connection/socket/slopos.rs"  'SlopOS platform implementation for `std::net`'
check_patched "net/hostname/mod.rs"            "$STD_SYS/net/hostname/mod.rs"               'target_os = "slopos"'

# File descriptor surfaces
check_patched "fd/mod.rs"                      "$STD_SYS/fd/mod.rs"                         'target_os = "slopos"'
check_patched "fd/slopos.rs"                   "$STD_SYS/fd/slopos.rs"                      '.'
check_patched "os/mod.rs"                      "$STD_OS/mod.rs"                             'target_os = "slopos"'
check_patched "os/fd/raw.rs"                   "$STD_OS/fd/raw.rs"                          'target_os = "slopos"'
check_patched "os/fd/owned.rs"                 "$STD_OS/fd/owned.rs"                        'target_os = "slopos"'

# Path/cwd surfaces (std::env::{current_dir,set_current_dir,temp_dir})
check_patched "paths/mod.rs"                   "$STD_SYS/paths/mod.rs"                      'target_os = "slopos"'
check_patched "paths/slopos.rs"                "$STD_SYS/paths/slopos.rs"                   'pub fn chdir'

if [ "$failed" -ne 0 ]; then
    echo ""
    echo "ERROR: one or more std patches failed to apply."
    echo "       The sysroot at $SYSROOT is in a half-patched state."
    exit 1
fi

# Reliable build-std cache invalidation.
#
# `-Zbuild-std` caches a compiled `libstd-<fingerprint>.rlib` keyed on the std
# source files cargo knew about during the *previous* build. When a patch adds
# a NEW module file (e.g. paths/slopos.rs) that the old dep-info never listed,
# cargo concludes std is unchanged and silently reuses the stale rlib — so the
# new code is missing at link time even though the sysroot source is correct.
#
# The old mtime heuristic (touch Cargo.toml when a patched file looks newer)
# was racy and unreliable. Instead we write a deterministic *content stamp*
# describing the exact patched-std state: a hash over this script (captures the
# sed routing logic) plus every PAL source file (captures the copied content).
# The build scripts compare this stamp against a per-target-dir copy and purge
# stale build-std artifacts whenever it changes (see scripts/std_cache_guard.sh).
# This stamp is the single source of truth for "what std currently is".
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | cut -d' ' -f1
    else
        shasum -a 256 | cut -d' ' -f1
    fi
}

PATCH_STAMP="$STD_SYS/.slopos_patch_stamp"
stamp_hash="$(
    {
        cat "${BASH_SOURCE[0]}"
        find "$STD_PAL_SRC" -type f -name '*.rs' | LC_ALL=C sort | while IFS= read -r f; do
            printf '\n--- %s ---\n' "${f#"$STD_PAL_SRC"/}"
            cat "$f"
        done
    } | sha256
)"
echo "$stamp_hash" > "$PATCH_STAMP"
echo "  Wrote patch stamp ($stamp_hash) to $PATCH_STAMP"

echo ""
echo "SlopOS std patches applied successfully (verified)."
