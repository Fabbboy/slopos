#!/usr/bin/env bash
set -euo pipefail

# Build SlopOS userland binaries.
#
# Usage: build_userland.sh <build_dir> <cargo_target_dir> [--test]
#
# With --test:    also builds userland test binaries (requires testbins feature)
#
# Environment:
#   CARGO        - cargo binary (default: cargo)
#   RUST_CHANNEL - toolchain channel (parsed from rust-toolchain.toml if unset)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BUILD_DIR="${1:?Usage: build_userland.sh <build_dir> <cargo_target_dir> [--test]}"
CARGO_TARGET_DIR="${2:?Usage: build_userland.sh <build_dir> <cargo_target_dir> [--test]}"
TEST_MODE="${3:-}"

CARGO="${CARGO:-cargo}"
RUST_CHANNEL="${RUST_CHANNEL:-$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "${REPO_ROOT}/rust-toolchain.toml")}"
USERLAND_TARGET="${USERLAND_TARGET:-${REPO_ROOT}/targets/x86_64-slos-userland.json}"

BINS="init shell terminal compositor roulette file_manager image_viewer sysmon nmap ip keymap ss nc curl ping widget_gallery oops_smoke"
BUILD_STD="${BUILD_STD:-core,alloc,std,panic_abort}"

# Ensure toolchain is available and std patches are applied
"$SCRIPT_DIR/ensure_toolchain.sh"
if [[ "$BUILD_STD" == *"std"* ]]; then
    "$SCRIPT_DIR/patch_std.sh"
    # Purge any stale build-std artifacts if the patches changed since the
    # cached libstd was compiled. Content-addressed and reliable — see the
    # script header for why the old mtime heuristic was insufficient.
    "$SCRIPT_DIR/std_cache_guard.sh" "$CARGO_TARGET_DIR" "x86_64-slos-userland"
fi

mkdir -p "$BUILD_DIR"

# Build main userland binaries
BIN_ARGS=()
for bin in $BINS; do
    BIN_ARGS+=(--bin "$bin")
done

CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
$CARGO +"$RUST_CHANNEL" build \
    -Zbuild-std="$BUILD_STD" \
    -Zbuild-std-features=compiler-builtins-mem \
    -Zunstable-options \
    --target "$USERLAND_TARGET" \
    --package slopos-userland \
    "${BIN_ARGS[@]}" \
    --no-default-features \
    --release

# Copy built binaries
RELEASE_DIR="${CARGO_TARGET_DIR}/x86_64-slos-userland/release"
for bin in $BINS; do
    if [ -f "$RELEASE_DIR/$bin" ]; then
        cp "$RELEASE_DIR/$bin" "$BUILD_DIR/${bin}.elf"
    fi
done

echo "Userland binaries built: $(for b in $BINS; do printf '%s/%s.elf ' "$BUILD_DIR" "$b"; done)"

# Build test binaries if requested
if [ "$TEST_MODE" = "--test" ]; then
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
    $CARGO +"$RUST_CHANNEL" build \
        -Zbuild-std="$BUILD_STD" \
        -Zbuild-std-features=compiler-builtins-mem \
        -Zunstable-options \
        --target "$USERLAND_TARGET" \
        --package slopos-userland \
        --bin fork_test \
        --bin io_capture_test \
        --bin heap_allocator_test \
        --bin image_test \
        --bin curl_recv_repro_test \
        --bin curl_e2e_test \
        --bin cd_test \
        --bin ring_test \
        --bin pidfd_e2e_test \
        --bin signalfd_test \
        --bin slopfut_test \
        --bin multishot_test \
        --bin tls_independence_test \
        --bin percore_reactor_test \
        --bin signal_handler_test \
        --bin sigwinch_default_test \
        --bin ctrlc_flood_test \
        --bin pty_flow_test \
        --bin mm_stress_test \
        --bin spin_signal_test \
        --bin terminal_grid_test \
        --bin sysmon_selection_test \
        --bin clipboard_test \
        --bin keymap_test \
        --bin appkit_test \
        --bin spawn_privilege_test \
        --bin shell_script_test \
        --bin stdio_stream_test \
        --bin ip_e2e_test \
        --bin session_smoke_test \
        --features testbins \
        --no-default-features \
        --release

    if [ -f "$RELEASE_DIR/fork_test" ]; then
        cp "$RELEASE_DIR/fork_test" "$BUILD_DIR/fork_test.elf"
    fi
    if [ -f "$RELEASE_DIR/io_capture_test" ]; then
        cp "$RELEASE_DIR/io_capture_test" "$BUILD_DIR/io_capture_test.elf"
    fi
    if [ -f "$RELEASE_DIR/heap_allocator_test" ]; then
        cp "$RELEASE_DIR/heap_allocator_test" "$BUILD_DIR/heap_allocator_test.elf"
    fi
    if [ -f "$RELEASE_DIR/image_test" ]; then
        cp "$RELEASE_DIR/image_test" "$BUILD_DIR/image_test.elf"
    fi
    if [ -f "$RELEASE_DIR/curl_recv_repro_test" ]; then
        cp "$RELEASE_DIR/curl_recv_repro_test" "$BUILD_DIR/curl_recv_repro_test.elf"
    fi
    if [ -f "$RELEASE_DIR/curl_e2e_test" ]; then
        cp "$RELEASE_DIR/curl_e2e_test" "$BUILD_DIR/curl_e2e_test.elf"
    fi
    if [ -f "$RELEASE_DIR/cd_test" ]; then
        cp "$RELEASE_DIR/cd_test" "$BUILD_DIR/cd_test.elf"
    fi
    if [ -f "$RELEASE_DIR/ring_test" ]; then
        cp "$RELEASE_DIR/ring_test" "$BUILD_DIR/ring_test.elf"
    fi
    if [ -f "$RELEASE_DIR/pidfd_e2e_test" ]; then
        cp "$RELEASE_DIR/pidfd_e2e_test" "$BUILD_DIR/pidfd_e2e_test.elf"
    fi
    if [ -f "$RELEASE_DIR/signalfd_test" ]; then
        cp "$RELEASE_DIR/signalfd_test" "$BUILD_DIR/signalfd_test.elf"
    fi
    if [ -f "$RELEASE_DIR/slopfut_test" ]; then
        cp "$RELEASE_DIR/slopfut_test" "$BUILD_DIR/slopfut_test.elf"
    fi
    if [ -f "$RELEASE_DIR/multishot_test" ]; then
        cp "$RELEASE_DIR/multishot_test" "$BUILD_DIR/multishot_test.elf"
    fi
    if [ -f "$RELEASE_DIR/tls_independence_test" ]; then
        cp "$RELEASE_DIR/tls_independence_test" "$BUILD_DIR/tls_independence_test.elf"
    fi
    if [ -f "$RELEASE_DIR/percore_reactor_test" ]; then
        cp "$RELEASE_DIR/percore_reactor_test" "$BUILD_DIR/percore_reactor_test.elf"
    fi
    if [ -f "$RELEASE_DIR/signal_handler_test" ]; then
        cp "$RELEASE_DIR/signal_handler_test" "$BUILD_DIR/signal_handler_test.elf"
    fi
    if [ -f "$RELEASE_DIR/ctrlc_flood_test" ]; then
        cp "$RELEASE_DIR/ctrlc_flood_test" "$BUILD_DIR/ctrlc_flood_test.elf"
    fi
    if [ -f "$RELEASE_DIR/pty_flow_test" ]; then
        cp "$RELEASE_DIR/pty_flow_test" "$BUILD_DIR/pty_flow_test.elf"
    fi
    if [ -f "$RELEASE_DIR/mm_stress_test" ]; then
        cp "$RELEASE_DIR/mm_stress_test" "$BUILD_DIR/mm_stress_test.elf"
    fi
    if [ -f "$RELEASE_DIR/sigwinch_default_test" ]; then
        cp "$RELEASE_DIR/sigwinch_default_test" "$BUILD_DIR/sigwinch_default_test.elf"
    fi
    if [ -f "$RELEASE_DIR/spin_signal_test" ]; then
        cp "$RELEASE_DIR/spin_signal_test" "$BUILD_DIR/spin_signal_test.elf"
    fi
    if [ -f "$RELEASE_DIR/terminal_grid_test" ]; then
        cp "$RELEASE_DIR/terminal_grid_test" "$BUILD_DIR/terminal_grid_test.elf"
    fi
    if [ -f "$RELEASE_DIR/sysmon_selection_test" ]; then
        cp "$RELEASE_DIR/sysmon_selection_test" "$BUILD_DIR/sysmon_selection_test.elf"
    fi
    if [ -f "$RELEASE_DIR/clipboard_test" ]; then
        cp "$RELEASE_DIR/clipboard_test" "$BUILD_DIR/clipboard_test.elf"
    fi
    if [ -f "$RELEASE_DIR/keymap_test" ]; then
        cp "$RELEASE_DIR/keymap_test" "$BUILD_DIR/keymap_test.elf"
    fi
    if [ -f "$RELEASE_DIR/appkit_test" ]; then
        cp "$RELEASE_DIR/appkit_test" "$BUILD_DIR/appkit_test.elf"
    fi
    if [ -f "$RELEASE_DIR/spawn_privilege_test" ]; then
        cp "$RELEASE_DIR/spawn_privilege_test" "$BUILD_DIR/spawn_privilege_test.elf"
    fi
    if [ -f "$RELEASE_DIR/stdio_stream_test" ]; then
        cp "$RELEASE_DIR/stdio_stream_test" "$BUILD_DIR/stdio_stream_test.elf"
    fi
    if [ -f "$RELEASE_DIR/shell_script_test" ]; then
        cp "$RELEASE_DIR/shell_script_test" "$BUILD_DIR/shell_script_test.elf"
    fi
    if [ -f "$RELEASE_DIR/stdio_stream_test" ]; then
        cp "$RELEASE_DIR/stdio_stream_test" "$BUILD_DIR/stdio_stream_test.elf"
    fi
    if [ -f "$RELEASE_DIR/ip_e2e_test" ]; then
        cp "$RELEASE_DIR/ip_e2e_test" "$BUILD_DIR/ip_e2e_test.elf"
    fi
    if [ -f "$RELEASE_DIR/session_smoke_test" ]; then
        cp "$RELEASE_DIR/session_smoke_test" "$BUILD_DIR/session_smoke_test.elf"
    fi

    echo "Userland test binaries built:$BUILD_DIR/fork_test.elf $BUILD_DIR/io_capture_test.elf $BUILD_DIR/heap_allocator_test.elf $BUILD_DIR/image_test.elf $BUILD_DIR/curl_recv_repro_test.elf $BUILD_DIR/curl_e2e_test.elf $BUILD_DIR/cd_test.elf $BUILD_DIR/ring_test.elf $BUILD_DIR/pidfd_e2e_test.elf $BUILD_DIR/signalfd_test.elf $BUILD_DIR/slopfut_test.elf $BUILD_DIR/multishot_test.elf $BUILD_DIR/tls_independence_test.elf $BUILD_DIR/percore_reactor_test.elf $BUILD_DIR/signal_handler_test.elf $BUILD_DIR/ctrlc_flood_test.elf $BUILD_DIR/pty_flow_test.elf $BUILD_DIR/mm_stress_test.elf $BUILD_DIR/sigwinch_default_test.elf $BUILD_DIR/spin_signal_test.elf $BUILD_DIR/terminal_grid_test.elf $BUILD_DIR/sysmon_selection_test.elf $BUILD_DIR/clipboard_test.elf $BUILD_DIR/keymap_test.elf $BUILD_DIR/appkit_test.elf $BUILD_DIR/spawn_privilege_test.elf $BUILD_DIR/stdio_stream_test.elf $BUILD_DIR/shell_script_test.elf $BUILD_DIR/ip_e2e_test.elf"
fi
