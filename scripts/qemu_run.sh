#!/usr/bin/env bash
set -euo pipefail

# Run SlopOS in QEMU with mode-specific configuration.
#
# Usage: qemu_run.sh <mode> <iso> <fs_image>
#
#   mode: interactive - Full interactive boot (Ctrl+C to exit)
#         logged      - Headless boot with timeout, logs to file
#         test        - Test harness with exit-code interpretation
#
# Environment (all optional, sensible defaults provided):
#   QEMU_BIN, QEMU_SMP, QEMU_MEM, QEMU_ACCEL,
#   VIDEO, QEMU_DISPLAY,
#   QEMU_FB_WIDTH, QEMU_FB_HEIGHT, QEMU_FB_AUTO,
#   QEMU_FB_AUTO_POLICY, QEMU_FB_AUTO_OUTPUT,
#   QEMU_GTK_ZOOM_TO_FIT,
#   QEMU_ENABLE_ISA_EXIT, QEMU_PCI_DEVICES,
#   OVMF_DIR,
#   NET, NET_PORTS,
#   BOOT_LOG_TIMEOUT, LOG_FILE

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MODE="${1:?Usage: qemu_run.sh <interactive|logged|test> <iso> <fs_image>}"
ISO="${2:?Usage: qemu_run.sh <mode> <iso> <fs_image>}"
FS_IMAGE="${3:?Usage: qemu_run.sh <mode> <iso> <fs_image>}"

# ── Configuration with defaults ──────────────────────────────────────────────
QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
QEMU_SMP="${QEMU_SMP:-4}"
QEMU_MEM="${QEMU_MEM:-512M}"

# Platform-aware acceleration and CPU model defaults
if [ "$(uname -s)" = "Darwin" ]; then
    QEMU_ACCEL="${QEMU_ACCEL:-hvf:tcg}"
    QEMU_DISPLAY="${QEMU_DISPLAY:-cocoa}"
    QEMU_CPU="${QEMU_CPU:-host}"
else
    QEMU_ACCEL="${QEMU_ACCEL:-kvm:tcg}"
    QEMU_DISPLAY="${QEMU_DISPLAY:-auto}"
    QEMU_CPU="${QEMU_CPU:-host}"
fi

# ── Auto-detect KVM and fix CPU model ────────────────────────────────────────
# -cpu host requires KVM (or HVF on macOS). When the hypervisor is not
# available QEMU falls back to TCG, but -cpu host is incompatible with TCG
# and causes an immediate exit — which the test harness misreads as "pass".
# Detect this and switch to -cpu max (TCG's full-feature model) instead.
needs_tcg_cpu=0
if [ "$QEMU_CPU" = "host" ]; then
    case "$QEMU_ACCEL" in
        tcg) needs_tcg_cpu=1 ;;  # explicit TCG-only — host won't work
    esac
    if [ "$needs_tcg_cpu" = "0" ]; then
        case "$(uname -s)" in
            Linux)
                if [ ! -c /dev/kvm ] || [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
                    needs_tcg_cpu=1
                fi
                ;;
            Darwin)
                if ! "$QEMU_BIN" -accel help 2>/dev/null | grep -q hvf; then
                    needs_tcg_cpu=1
                fi
                ;;
        esac
    fi
    if [ "$needs_tcg_cpu" = "1" ]; then
        QEMU_CPU="max"
        QEMU_ACCEL="tcg"
        echo "No hardware acceleration — using TCG with -cpu max" >&2
    fi
fi

VIDEO="${VIDEO:-0}"
QEMU_FB_WIDTH="${QEMU_FB_WIDTH:-1920}"
QEMU_FB_HEIGHT="${QEMU_FB_HEIGHT:-1080}"
QEMU_FB_AUTO="${QEMU_FB_AUTO:-1}"
QEMU_FB_AUTO_POLICY="${QEMU_FB_AUTO_POLICY:-primary}"
QEMU_FB_AUTO_OUTPUT="${QEMU_FB_AUTO_OUTPUT:-}"
QEMU_FB_DETECT_SCRIPT="${QEMU_FB_DETECT_SCRIPT:-${SCRIPT_DIR}/detect_qemu_resolution.sh}"
QEMU_GTK_ZOOM_TO_FIT="${QEMU_GTK_ZOOM_TO_FIT:-off}"
QEMU_ENABLE_ISA_EXIT="${QEMU_ENABLE_ISA_EXIT:-0}"
QEMU_PCI_DEVICES="${QEMU_PCI_DEVICES:-}"

NET="${NET:-0}"
NET_PORTS="${NET_PORTS:-7777,8080,8081}"

OVMF_DIR="${OVMF_DIR:-${REPO_ROOT}/third_party/ovmf}"
OVMF_CODE="${OVMF_DIR}/OVMF_CODE.fd"
OVMF_VARS="${OVMF_DIR}/OVMF_VARS.fd"

BOOT_LOG_TIMEOUT="${BOOT_LOG_TIMEOUT:-15}"
LOG_FILE="${LOG_FILE:-test_output.log}"
LOG_FILE_RAW="${LOG_FILE}.raw"

run_with_timeout() {
    local seconds="$1"
    shift

    if command -v timeout >/dev/null 2>&1; then
        timeout "${seconds}s" "$@"
        return $?
    fi
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout "${seconds}s" "$@"
        return $?
    fi

    local timeout_dir marker
    timeout_dir="$(mktemp -d)"
    marker="${timeout_dir}/timed_out"

    "$@" &
    local child_pid=$!
    (
        sleep "$seconds"
        if kill -0 "$child_pid" 2>/dev/null; then
            touch "$marker"
            kill -TERM "$child_pid" 2>/dev/null || true
            sleep 2
            kill -KILL "$child_pid" 2>/dev/null || true
        fi
    ) &
    local watchdog_pid=$!

    wait "$child_pid"
    local status=$?

    kill "$watchdog_pid" 2>/dev/null || true
    wait "$watchdog_pid" 2>/dev/null || true

    if [ -f "$marker" ]; then
        rm -rf "$timeout_dir"
        return 124
    fi

    rm -rf "$timeout_dir"
    return "$status"
}

# ── Validate SMP ─────────────────────────────────────────────────────────────
if [ "$QEMU_SMP" -lt 1 ]; then
    echo "QEMU_SMP must be >= 1" >&2
    exit 1
fi
if [ $(( QEMU_SMP & (QEMU_SMP - 1) )) -ne 0 ]; then
    echo "QEMU_SMP must be a power of 2 (got $QEMU_SMP)" >&2
    exit 1
fi

# ── Ensure OVMF firmware ─────────────────────────────────────────────────────
"$SCRIPT_DIR/setup_ovmf.sh"

# ── Check ISO exists ─────────────────────────────────────────────────────────
if [ ! -f "$ISO" ]; then
    echo "ISO not found at $ISO" >&2
    exit 1
fi

# ── Create runtime OVMF_VARS copy ────────────────────────────────────────────
OVMF_VARS_RUNTIME="$(mktemp "${OVMF_DIR}/OVMF_VARS.runtime.XXXXXX")"
cleanup() { rm -f "$OVMF_VARS_RUNTIME"; }
trap cleanup EXIT INT TERM
cp "$OVMF_VARS" "$OVMF_VARS_RUNTIME"

# ── Resolve framebuffer dimensions ───────────────────────────────────────────
fb_width="$QEMU_FB_WIDTH"
fb_height="$QEMU_FB_HEIGHT"
if [ "$QEMU_FB_AUTO" != "0" ] && [ "$VIDEO" != "0" ] && [ -x "$QEMU_FB_DETECT_SCRIPT" ]; then
    detected="$(QEMU_FB_WIDTH="$fb_width" QEMU_FB_HEIGHT="$fb_height" \
        QEMU_FB_AUTO_POLICY="$QEMU_FB_AUTO_POLICY" \
        QEMU_FB_AUTO_OUTPUT="$QEMU_FB_AUTO_OUTPUT" \
        "$QEMU_FB_DETECT_SCRIPT")" || true
    detected_w="${detected%% *}"
    detected_h="${detected##* }"
    if [ -n "${detected_w:-}" ] && [ -n "${detected_h:-}" ]; then
        fb_width="$detected_w"
        fb_height="$detected_h"
        echo "QEMU framebuffer auto-detected: ${fb_width} x ${fb_height}"
    fi
fi

# ── Detect available display backends ────────────────────────────────────────
HAS_SDL=0
HAS_COCOA=0
if $QEMU_BIN -display help 2>/dev/null | grep -q 'sdl'; then
    HAS_SDL=1
fi
if $QEMU_BIN -display help 2>/dev/null | grep -q 'cocoa'; then
    HAS_COCOA=1
fi

# ── Resolve display, serial, and extra args per mode ─────────────────────────
DISPLAY_ARGS=(-display none)
SERIAL_ARGS=(-serial stdio)
ADD_ISA_EXIT=0
ADD_NO_REBOOT=0
# Disposable scratch block device (virtio-disk1) attached only for the test
# harness. Destructive block-device tests target THIS device, never the live
# root-fs image (virtio-disk0) — so a buggy test cannot corrupt an on-disk
# binary (the nightly-2026-05-25 io_capture incident). Recreated blank each run.
ADD_SCRATCH_DISK=0

case "$MODE" in
    test)
        # `-nographic` historically combined "no GUI" with implicit
        # `-serial mon:stdio`. Combined with our explicit `-serial stdio`,
        # QEMU's chardev layer silently mirrors every UART byte to BOTH
        # the explicit and the implicit stdio backend — every kernel
        # klog line shows up TWICE on the host pipe, corrupting the
        # KTAP wire stream the test harness emits. Use the modern
        # `-display none` instead so only the explicit `-serial stdio`
        # backend is wired to COM1.
        DISPLAY_ARGS=(-display none)
        ADD_ISA_EXIT=1
        ADD_NO_REBOOT=1
        SCRATCH_IMG="${SCRATCH_IMG:-${REPO_ROOT}/builddir/scratch-disk.img}"
        mkdir -p "$(dirname "$SCRATCH_IMG")"
        # Fresh, blank 8 MiB raw scratch each run (no filesystem; raw-sector tests only).
        rm -f "$SCRATCH_IMG"
        truncate -s 8M "$SCRATCH_IMG"
        ADD_SCRATCH_DISK=1
        ;;
    interactive|logged)
        if [ "$QEMU_ENABLE_ISA_EXIT" != "0" ]; then
            ADD_ISA_EXIT=1
        fi
        if [ "$VIDEO" != "0" ]; then
            if [ "$QEMU_DISPLAY" = "cocoa" ] && [ "$HAS_COCOA" = "1" ]; then
                DISPLAY_ARGS=(-display cocoa)
            elif [ "$QEMU_DISPLAY" = "sdl" ]; then
                DISPLAY_ARGS=(-display "sdl,grab-mod=lctrl-lalt")
            elif [ "$QEMU_DISPLAY" = "gtk" ]; then
                DISPLAY_ARGS=(-display "gtk,grab-on-hover=on,zoom-to-fit=$QEMU_GTK_ZOOM_TO_FIT")
            elif [ "$HAS_COCOA" = "1" ]; then
                DISPLAY_ARGS=(-display cocoa)
            elif [ "${XDG_SESSION_TYPE:-x11}" = "wayland" ] && [ "$HAS_SDL" = "1" ]; then
                DISPLAY_ARGS=(-display "sdl,grab-mod=lctrl-lalt")
            else
                DISPLAY_ARGS=(-display "gtk,grab-on-hover=on,zoom-to-fit=$QEMU_GTK_ZOOM_TO_FIT")
            fi
        fi
        if [ "$MODE" = "logged" ]; then
            SERIAL_ARGS=(-serial "file:${LOG_FILE_RAW}")
        fi
        ;;
    *)
        echo "Unknown mode: $MODE (expected: interactive, logged, test)" >&2
        exit 1
        ;;
esac

# ── Display device selection ─────────────────────────────────────────────────
# GPU selects the emulated display adapter SlopOS drives:
#   virtio-vga (default) — virtio-gpu plus a VGA-compatible linear framebuffer.
#                          The firmware framebuffer is present from power-on, so
#                          the boot console (splash + ESC log) is live through
#                          early boot; the kernel driver then upgrades scanout
#                          to virtio-gpu (the image is copied across).
#   virtio-gpu-pci       — pure virtio-gpu; NO early framebuffer (OVMF's GOP for
#                          it dies at ExitBootServices), so nothing renders on
#                          screen until the driver probes at PCI init.
#   vga                  — plain stdvga (passive framebuffer only). Pair with
#                          BOOT_CMDLINE='video=framebuffer'.
# A device the kernel can't instantiate falls back to stdvga (still an early
# framebuffer). Applies to every mode, including headless test.
GPU="${GPU:-virtio-vga}"

# Plain stdvga (passive framebuffer): VRAM sized to the resolution (2x
# headroom) and rounded up to a power of two as QEMU's stdvga requires.
stdvga_args() {
    local fb_vram_bytes vgamem_mb p
    fb_vram_bytes=$((fb_width * fb_height * 4 * 2))
    vgamem_mb=$(( (fb_vram_bytes + 1048575) / 1048576 ))
    if [ "$vgamem_mb" -lt 16 ]; then vgamem_mb=16; fi
    p=1; while [ "$p" -lt "$vgamem_mb" ]; do p=$((p * 2)); done
    vgamem_mb=$p
    VIDEO_ARGS=(-vga none -device "VGA,edid=on,xres=${fb_width},yres=${fb_height},vgamem_mb=${vgamem_mb}")
}

# True only if the named display device both EXISTS and INITIALIZES without
# crashing. A grep of `-device help` is not enough: a virtio-gpu PCI module
# installed without its base `virtio-gpu-device` module is listed but segfaults
# at instance_init (the child type is unregistered). Spin up a paused throwaway
# VM (`-S` halts before guest code, after device realize) and check it survives.
gpu_device_usable() {
    local dev="$1"
    "$QEMU_BIN" -device help 2>/dev/null | grep -q "\"$dev\"" || return 1
    "$QEMU_BIN" -machine q35 -nodefaults -no-user-config -display none \
        -serial null -S -device "$dev" >/dev/null 2>&1 &
    local pid=$!
    sleep 0.5
    if kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null
        return 0
    fi
    wait "$pid" 2>/dev/null
    return 1
}

# Build VIDEO_ARGS for a virtio display device, falling back to stdvga (the
# kernel then sees no virtio-gpu PCI device and stays on the passive
# framebuffer) when this QEMU lacks or can't instantiate it.
virtio_display_args() {
    local dev="$1"
    if gpu_device_usable "$dev"; then
        VIDEO_ARGS=(-vga none -device "${dev},edid=on,xres=${fb_width},yres=${fb_height}")
    else
        echo "warning: this QEMU can't instantiate '${dev}' (missing or broken module);" >&2
        echo "         falling back to stdvga. SlopOS runs on the passive framebuffer." >&2
        echo "         On Arch/CachyOS: pacman -S qemu-hw-display-virtio-gpu qemu-hw-display-virtio-gpu-pci" >&2
        stdvga_args
    fi
}

case "$GPU" in
    vga|std|stdvga)
        stdvga_args
        ;;
    virtio-vga)
        virtio_display_args "virtio-vga"
        ;;
    virtio-gpu-pci|virtio|virtio-gpu)
        virtio_display_args "virtio-gpu-pci"
        ;;
    *)
        echo "Unknown GPU=$GPU (expected: virtio-gpu-pci, virtio-vga, vga)" >&2
        exit 1
        ;;
esac

# Handle optional PCI devices
HAVE_PCI_ARGS=0
if [ -n "$QEMU_PCI_DEVICES" ]; then
    # Split space-separated PCI device strings into array elements
    HAVE_PCI_ARGS=1
    read -ra PCI_ARGS <<< "$QEMU_PCI_DEVICES"
fi

# ── Network port forwarding ────────────────────────────────────────────────
NET_HOSTFWD=""
if [[ "$NET" =~ ^(1|true|on|yes)$ ]]; then
    if [ -z "$NET_PORTS" ]; then
        echo "NET_PORTS must not be empty when NET is enabled" >&2
        exit 1
    fi
    IFS=',' read -ra NET_PORT_ARRAY <<< "$NET_PORTS"
    for _p in "${NET_PORT_ARRAY[@]}"; do
        if [[ "$_p" == *:* ]]; then
            # host:guest format
            _host="${_p%%:*}"
            _guest="${_p#*:}"
        else
            # single port: same on both sides
            _host="$_p"
            _guest="$_p"
        fi
        NET_HOSTFWD+=",hostfwd=tcp::${_host}-:${_guest}"
    done
    echo "Network port forwarding enabled: ${NET_PORTS}"
fi

# ── Debug-mode plumbing ─────────────────────────────────────────────────────
# Set QEMU_DEBUG=1 to enable the QEMU monitor on a Unix socket plus the GDB
# stub on TCP :1234. The monitor lets you run `info cpus`, `info registers`,
# `cpu N`, etc. when the system freezes; GDB gives backtraces per CPU.
#
#   Monitor:  socat - UNIX-CONNECT:/tmp/slopos-monitor.sock
#   GDB:      gdb builddir/kernel.elf -ex "target remote :1234"
if [ "${QEMU_DEBUG:-0}" != "0" ]; then
    rm -f /tmp/slopos-monitor.sock
    DEBUG_ARGS=(
        -monitor "unix:/tmp/slopos-monitor.sock,server,nowait"
        -s
    )
else
    DEBUG_ARGS=(-monitor none)
fi

# Root-fs disk (virtio-disk0). Omitted when QEMU_NO_ROOT_DISK=1 to prove the
# kernel boots purely from the Limine initramfs with no storage device
# attached — the real-hardware scenario. Otherwise it is the ext2 image the
# kernel mounts at /mnt (secondary) once the RAM root is up.
if [[ ! "${QEMU_NO_ROOT_DISK:-0}" =~ ^(1|true|on|yes)$ ]]; then
    ADD_ROOT_DISK=1
else
    echo "QEMU_NO_ROOT_DISK=1 → booting RAM-only (no virtio-disk0 attached)"
    ADD_ROOT_DISK=0
fi

# ── Assemble common QEMU arguments ──────────────────────────────────────────
QEMU_ARGS=(
    -machine "q35,accel=$QEMU_ACCEL"
    -cpu "$QEMU_CPU"
    -smp "$QEMU_SMP"
    -m "$QEMU_MEM"
    -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE"
    -drive "if=pflash,format=raw,file=$OVMF_VARS_RUNTIME"
    -device "ich9-ahci,id=ahci0,bus=pcie.0,addr=0x3"
    -drive "if=none,id=cdrom,media=cdrom,readonly=on,file=$ISO"
    -device "ide-cd,bus=ahci0.0,drive=cdrom,bootindex=0"
)
if [ "$ADD_ROOT_DISK" = "1" ]; then
    QEMU_ARGS+=(
        -drive "file=$FS_IMAGE,if=none,id=virtio-disk0,format=raw"
        -object "iothread,id=iot0"
        -device "virtio-blk-pci,drive=virtio-disk0,disable-legacy=on,iothread=iot0"
    )
fi
if [ "$ADD_SCRATCH_DISK" = "1" ]; then
    QEMU_ARGS+=(
        -drive "file=$SCRATCH_IMG,if=none,id=virtio-disk1,format=raw"
        -device "virtio-blk-pci,drive=virtio-disk1,disable-legacy=on"
    )
fi
QEMU_ARGS+=(
    -netdev "user,id=slopnet0,dns=1.1.1.1${NET_HOSTFWD}"
    -device "virtio-net-pci,netdev=slopnet0,disable-legacy=on"
    -boot "order=d,menu=off"
    "${SERIAL_ARGS[@]}"
    "${DEBUG_ARGS[@]}"
    "${DISPLAY_ARGS[@]}"
    "${VIDEO_ARGS[@]}"
)
if [ "$ADD_ISA_EXIT" = "1" ]; then
    QEMU_ARGS+=(-device "isa-debug-exit,iobase=0xf4,iosize=0x01")
fi
if [ "$ADD_NO_REBOOT" = "1" ]; then
    QEMU_ARGS+=(-no-reboot)
fi
if [ "$HAVE_PCI_ARGS" = "1" ]; then
    QEMU_ARGS+=("${PCI_ARGS[@]}")
fi

# ── Launch QEMU ──────────────────────────────────────────────────────────────
case "$MODE" in
    interactive)
        echo "Starting QEMU in interactive mode (Ctrl+C to exit)..."
        "$QEMU_BIN" "${QEMU_ARGS[@]}"
        ;;

    logged)
        echo "Starting QEMU with ${BOOT_LOG_TIMEOUT}s timeout (logging to ${LOG_FILE})..."
        : > "$LOG_FILE_RAW"
        tail -n +1 -F "$LOG_FILE_RAW" 2>/dev/null &
        tail_pid=$!
        trap 'kill "$tail_pid" 2>/dev/null; wait "$tail_pid" 2>/dev/null || true; rm -f "$OVMF_VARS_RUNTIME" "$LOG_FILE_RAW"' EXIT INT TERM

        set +e
        run_with_timeout "$BOOT_LOG_TIMEOUT" "$QEMU_BIN" "${QEMU_ARGS[@]}"
        status=$?
        set -e

        sleep 0.2
        kill "$tail_pid" 2>/dev/null
        wait "$tail_pid" 2>/dev/null || true
        trap - EXIT INT TERM
        rm -f "$OVMF_VARS_RUNTIME"

        sed 's/\x1b\[[^a-zA-Z]*[a-zA-Z]//g' "$LOG_FILE_RAW" > "$LOG_FILE"
        rm -f "$LOG_FILE_RAW"
        if [ $status -eq 124 ]; then
            echo "QEMU terminated after ${BOOT_LOG_TIMEOUT}s timeout" | tee -a "$LOG_FILE"
            exit 0
        fi
        exit $status
        ;;

    test)
        echo "Starting QEMU for test harness..."
        set +e
        "$QEMU_BIN" "${QEMU_ARGS[@]}"
        status=$?
        set -e
        trap - EXIT INT TERM
        rm -f "$OVMF_VARS_RUNTIME"
        if [ $status -eq 1 ]; then
            echo "Tests passed."
        elif [ $status -eq 3 ]; then
            echo "Tests reported failures." >&2
            exit 1
        else
            echo "Unexpected QEMU exit status $status" >&2
            exit $status
        fi
        ;;
esac
