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

if [ ! -d "$STD_SYS" ]; then
    echo "ERROR: Rust std source not found at $STD_SYS"
    echo "Run: rustup component add rust-src"
    exit 1
fi

MARKER="$STD_SYS/.slopos_patched"
if [ -f "$MARKER" ]; then
    echo "SlopOS std patches already applied (marker: $MARKER)"
    echo "To re-apply, run: rm $MARKER && $0"
    exit 0
fi

echo "Patching Rust std source for SlopOS target..."

# 1. Copy PAL files
mkdir -p "$STD_SYS/pal/slopos"
cp "$STD_PAL_SRC/pal/slopos/mod.rs"   "$STD_SYS/pal/slopos/mod.rs"
cp "$STD_PAL_SRC/pal/slopos/futex.rs" "$STD_SYS/pal/slopos/futex.rs"
cp "$STD_PAL_SRC/pal/slopos/os.rs"    "$STD_SYS/pal/slopos/os.rs"
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
    sed -i '/^[[:space:]]*_ => {/{
i\    target_os = "slopos" => {\
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
    sed -i 's/target_os = "hermit",/target_os = "hermit",\n        target_os = "slopos",/' "$file"
    echo "  Patched $file (futex arm)"
}

# 3a. PAL routing
if ! grep -q 'target_os = "slopos"' "$STD_SYS/pal/mod.rs" 2>/dev/null; then
    sed -i '/^[[:space:]]*_ => {/{
i\    target_os = "slopos" => {\
        mod slopos;\
        pub use self::slopos::*;\
    }
}' "$STD_SYS/pal/mod.rs"
    echo "  Patched pal/mod.rs"
fi

# 3b. Alloc routing (no fallback — insert after the last zkvm entry)
if ! grep -q 'target_os = "slopos"' "$STD_SYS/alloc/mod.rs" 2>/dev/null; then
    sed -i '/target_os = "zkvm" => {/{
N;N
a\    target_os = "slopos" => {\
        mod slopos;\
    }
}' "$STD_SYS/alloc/mod.rs"
    echo "  Patched alloc/mod.rs"
fi

# 3c. Individual module routing with `_ =>` fallback
patch_cfg_select "$STD_SYS/stdio/mod.rs" "slopos" "*"

# Time uses `use ... as imp;` pattern (pub use imp::{...} outside cfg_select!)
if ! grep -q 'target_os = "slopos"' "$STD_SYS/time/mod.rs" 2>/dev/null; then
    sed -i '/^[[:space:]]*_ => {/{
i\    target_os = "slopos" => {\
        mod slopos;\
        use slopos as imp;\
    }
}' "$STD_SYS/time/mod.rs"
    echo "  Patched time/mod.rs"
fi

# Thread needs specific exports
if ! grep -q 'target_os = "slopos"' "$STD_SYS/thread/mod.rs" 2>/dev/null; then
    sed -i '/^[[:space:]]*_ => {/{
i\    target_os = "slopos" => {\
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
    sed -i '/^[[:space:]]*_ => {/{
i\    target_os = "slopos" => {\
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
    sed -i '/^[[:space:]]*_ => {/{
i\    target_os = "slopos" => {\
        mod slopos;\
        pub use slopos::{Pipe, pipe};\
    }
}' "$STD_SYS/pipe/mod.rs"
    echo "  Patched pipe/mod.rs"
fi

# Random routing (has `_ => {}` not `_ => { mod unsupported; }`)
if ! grep -q 'target_os = "slopos"' "$STD_SYS/random/mod.rs" 2>/dev/null; then
    sed -i '/^[[:space:]]*_ => {}$/{
i\    target_os = "slopos" => {\
        mod slopos;\
        pub use slopos::fill_bytes;\
    }
}' "$STD_SYS/random/mod.rs"
    echo "  Patched random/mod.rs"
fi

# FS routing (if fs/slopos.rs exists)
if [ -f "$STD_SYS/fs/slopos.rs" ]; then
    if ! grep -q 'target_os = "slopos"' "$STD_SYS/fs/mod.rs" 2>/dev/null; then
        sed -i '/^[[:space:]]*_ => {/{
i\    target_os = "slopos" => {\
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
        sed -i '/^[[:space:]]*_ => {/{
i\    target_os = "slopos" => {\
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
    sed -i '/^#\[else\]$/i\
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

# 3f. thread_local — slopos needs the no_threads storage (no real TLS)
#     and a no-op guard::enable() (hermit/xous arm).
TL_MOD="$STD_SYS/thread_local/mod.rs"
if ! grep -q 'target_os = "slopos"' "$TL_MOD" 2>/dev/null; then
    # Add slopos to the no_threads arm (first vexos occurrence = main cfg_select)
    sed -i '0,/target_os = "vexos",/{s/target_os = "vexos",/target_os = "vexos",\n        target_os = "slopos",/}' "$TL_MOD"
    # Add slopos to the guard hermit/xous no-op arm (hermit immediately followed by xous)
    sed -i '/target_os = "hermit",/{n;s/target_os = "xous",/target_os = "xous",\n            target_os = "slopos",/}' "$TL_MOD"
    echo "  Patched thread_local/mod.rs"
fi

# 3g. io/error/mod.rs — add slopos to the generic errno arm
IO_ERROR="$STD_SYS/io/error/mod.rs"
if ! grep -q 'target_os = "slopos"' "$IO_ERROR" 2>/dev/null; then
    sed -i 's/target_os = "vexos",/target_os = "vexos",\n        target_os = "slopos",/' "$IO_ERROR"
    echo "  Patched io/error/mod.rs"
fi

# 3h. sys/exit.rs — route slopos exit() to PAL instead of the intrinsics::abort() fallback.
#     Anchors on "xous" (last arm before the fallback in fn exit) to avoid the
#     unrelated unique_thread_exit cfg_select.
EXIT_RS="$STD_SYS/exit.rs"
if [ -f "$EXIT_RS" ] && ! grep -q 'target_os = "slopos"' "$EXIT_RS" 2>/dev/null; then
    sed -i '/crate::os::xous::ffi::exit/,/^[[:space:]]*}$/{
        /^[[:space:]]*}$/a\        target_os = "slopos" => {\
            crate::sys::pal::os::exit(code)\
        }
    }' "$EXIT_RS"
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
    sed -i '/^[[:space:]]*_ => {}$/{
i\    target_os = "slopos" => {\
        mod slopos;\
        pub use slopos::*;\
    }
}' "$SOCK_MOD"
    echo "  Patched net/connection/socket/mod.rs"
fi

# Patch connection/mod.rs — add slopos to the socket-based arm
CONN_MOD="$STD_SYS/net/connection/mod.rs"
if [ -f "$CONN_MOD" ] && ! grep -q 'target_os = "slopos"' "$CONN_MOD" 2>/dev/null; then
    sed -i '/^[[:space:]]*_ => {/{
i\    target_os = "slopos" => {\
        mod socket;\
        pub use socket::*;\
    }
}' "$CONN_MOD"
    echo "  Patched net/connection/mod.rs"
fi

# Patch hostname/mod.rs — use unsupported (returns error, acceptable)
HOST_MOD="$STD_SYS/net/hostname/mod.rs"
if [ -f "$HOST_MOD" ] && ! grep -q 'target_os = "slopos"' "$HOST_MOD" 2>/dev/null; then
    sed -i '/^[[:space:]]*_ => {/{
i\    target_os = "slopos" => {\
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
    sed -i '/^[[:space:]]*_ => {}$/{
i\    target_os = "slopos" => {\
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
    sed -i 's/target_os = "motor",/target_os = "motor",\n    target_os = "slopos",/' "$OS_MOD"

    # -- raw.rs: RawFd = i32 for slopos --
    # Widen the hermit|motor arm to include slopos
    sed -i 's/any(target_os = "hermit", target_os = "motor")/any(target_os = "hermit", target_os = "motor", target_os = "slopos")/' "$RAW"
    # Exclude slopos from the raw::c_int arm and the os::raw import
    sed -i 's/all(not(target_os = "hermit"), not(target_os = "motor"))/all(not(target_os = "hermit"), not(target_os = "motor"), not(target_os = "slopos"))/' "$RAW"
    # Add slopos to the motor OwnedFd import (NOT the moto_rt::libc import)
    sed -i '/#\[cfg(target_os = "motor")\]/{N;/OwnedFd/s/cfg(target_os = "motor")/cfg(any(target_os = "motor", target_os = "slopos"))/}' "$RAW"
    # Replace libc stdio constants with slopos-safe cfg-gated versions
    sed -i 's/libc::STDIN_FILENO/#[cfg(target_os = "slopos")] { 0 }\n        #[cfg(not(target_os = "slopos"))] { libc::STDIN_FILENO }/' "$RAW"
    sed -i 's/libc::STDOUT_FILENO/#[cfg(target_os = "slopos")] { 1 }\n        #[cfg(not(target_os = "slopos"))] { libc::STDOUT_FILENO }/' "$RAW"
    sed -i 's/libc::STDERR_FILENO/#[cfg(target_os = "slopos")] { 2 }\n        #[cfg(not(target_os = "slopos"))] { libc::STDERR_FILENO }/' "$RAW"

    # -- owned.rs: try_clone_to_owned + Drop for slopos --
    # 1) Exclude slopos from the libc-based try_clone_to_owned and cvt import
    #    (both cfg blocks contain "target_os = "trusty"," so this adds slopos to both)
    sed -i 's/target_os = "trusty",/target_os = "trusty",\n        target_os = "slopos",/' "$OWNED"

    # 2) Add slopos try_clone_to_owned block after motor's
    #    Anchor on map_motor_error (unique to motor impl), find its closing }, append after
    sed -i '/map_motor_error/,/^    }/{
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

    # 3) Patch Drop: exclude slopos from the #[cfg(not(target_os = "hermit"))] close block
    #    and add a slopos close block
    sed -i 's/#\[cfg(not(target_os = "hermit"))\]/#[cfg(not(any(target_os = "hermit", target_os = "slopos")))]/' "$OWNED"
    # Add slopos close block after the hermit close
    sed -i '/hermit_abi::close(self.fd.as_inner());/a\
            #[cfg(target_os = "slopos")]\
            {\
                unsafe extern "C" { fn close(fd: i32) -> i32; }\
                let _ = unsafe { close(self.fd.as_inner()) };\
            }' "$OWNED"

    # 4) Exclude slopos from the cvt import (we have our own cvt signature)
    # The gate is: #[cfg(not(any(target_arch = "wasm32", target_env = "sgx", target_os = "hermit", target_os = "trusty", target_os = "motor")))]
    # We already added slopos to this list via step 1 (same cfg block)

    echo "  Patched os/mod.rs, os/fd/raw.rs, os/fd/owned.rs"
}

if ! grep -q 'target_os = "slopos"' "$STD_OS/mod.rs" 2>/dev/null; then
    patch_os_fd
fi

# 4. Create marker file
echo "SlopOS std patches applied on $(date)" > "$MARKER"
echo ""
echo "SlopOS std patches applied successfully!"
echo "Marker: $MARKER"
