#!/usr/bin/env bash
set -euo pipefail

# Deterministic record/replay launcher for SlopOS kernel debugging.
#
# Wraps qemu-system-x86_64's full-system record/replay (icount + rr) so a
# failing run can be captured once and then replayed deterministically under
# GDB — including reverse-continue / reverse-step and reverse watchpoints,
# the only practical way to answer "which instruction corrupted this memory?"
# for a load-dependent kernel bug.
#
# Usage: qemu_rr.sh <record|replay> <iso> <fs_image>
#
# Record captures every nondeterministic input (device DMA, timers, serial)
# tagged by instruction count into RRFILE, plus a VM snapshot (rrsnapshot)
# so replay can rewind. Replay re-injects them at the identical instruction
# counts, halts at reset (-S), and exposes the GDB stub on GDB_PORT.
#
# HARD constraints (enforced below; deviating causes "replay diverged"):
#   * TCG only — KVM cannot count instructions.   (-machine accel=tcg)
#   * Single CPU — icount has no cross-vCPU order. (-smp 1)
#   * -cpu max  — TCG's full-feature model (host needs KVM).
#   * No iothread on virtio-blk — off-vCPU I/O breaks determinism.
#   * Every writable block device must be snapshot-capable (qcow2) OR
#     read-only; blkreplay nodes need explicit read-only=on when backing a
#     read-only image (the CD), else rrsnapshot refuses to snapshot them.
#   * UEFI vars pflash is mounted read-only (Limine does not persist vars).
#
# Environment (optional):
#   RRFILE          record log path           (default builddir/replay.bin)
#   RRDISK          qcow2 wrapper for fs image (default builddir/rrdisk.qcow2)
#   GDB_PORT        replay gdbstub TCP port    (default 1234)
#   ICOUNT_SHIFT    icount scaling             (default auto)
#   QEMU_MEM        guest RAM                  (default 512M)
#   QEMU_BIN        qemu binary                (default qemu-system-x86_64)
#   OVMF_DIR        firmware dir               (default third_party/ovmf)
#   RECORD_TIMEOUT  seconds to cap a record    (default 900; 0 disables)
#   SERIAL_LOG      serial capture path        (default builddir/rr-serial.log)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MODE="${1:?Usage: qemu_rr.sh <record|replay> <iso> <fs_image>}"
ISO="${2:?Usage: qemu_rr.sh <record|replay> <iso> <fs_image>}"
FS_IMAGE="${3:?Usage: qemu_rr.sh <record|replay> <iso> <fs_image>}"

QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
QEMU_MEM="${QEMU_MEM:-512M}"
RRFILE="${RRFILE:-${REPO_ROOT}/builddir/replay.bin}"
RRDISK="${RRDISK:-${REPO_ROOT}/builddir/rrdisk.qcow2}"
GDB_PORT="${GDB_PORT:-1234}"
ICOUNT_SHIFT="${ICOUNT_SHIFT:-auto}"
OVMF_DIR="${OVMF_DIR:-${REPO_ROOT}/third_party/ovmf}"
RECORD_TIMEOUT="${RECORD_TIMEOUT:-900}"
SERIAL_LOG="${SERIAL_LOG:-${REPO_ROOT}/builddir/rr-serial.log}"

OVMF_CODE="${OVMF_DIR}/OVMF_CODE.fd"
OVMF_VARS_RO="${REPO_ROOT}/builddir/rr-OVMF_VARS.fd"

"$SCRIPT_DIR/setup_ovmf.sh"

if [ ! -f "$ISO" ];      then echo "ISO not found: $ISO" >&2; exit 1; fi
if [ ! -f "$FS_IMAGE" ]; then echo "fs image not found: $FS_IMAGE" >&2; exit 1; fi

# Read-only UEFI vars copy (record and replay must see identical firmware state).
cp "${OVMF_DIR}/OVMF_VARS.fd" "$OVMF_VARS_RO"

# Snapshot-capable qcow2 wrapper around the (raw) fs image. Built ONLY at
# record time and reused verbatim for replay — the rrsnapshot VM-state lives
# inside it, so recreating it between record and replay would lose the
# snapshot and break rewind.
if [ "$MODE" = "record" ]; then
    rm -f "$RRDISK"
    qemu-img convert -f raw -O qcow2 "$FS_IMAGE" "$RRDISK"
    rm -f "$RRFILE"
fi
if [ ! -f "$RRDISK" ]; then
    echo "qcow2 wrapper missing ($RRDISK) — run 'record' first." >&2
    exit 1
fi

COMMON=(
    -machine q35,accel=tcg
    -cpu max
    -smp 1
    -m "$QEMU_MEM"
    -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE"
    -drive "if=pflash,format=raw,readonly=on,file=$OVMF_VARS_RO"
    -device "ich9-ahci,id=ahci0,bus=pcie.0,addr=0x3"
    -drive "file=$ISO,if=none,format=raw,media=cdrom,readonly=on,id=cd-raw"
    -drive "driver=blkreplay,if=none,image=cd-raw,id=cd-rr,read-only=on"
    -device "ide-cd,bus=ahci0.0,drive=cd-rr,bootindex=0"
    -drive "file=$RRDISK,if=none,format=qcow2,id=disk-raw"
    -drive "driver=blkreplay,if=none,image=disk-raw,id=disk-rr"
    -device "virtio-blk-pci,drive=disk-rr,disable-legacy=on"
    # Keep the NIC so the kernel's virtio-net / MSI-X / net tests still pass;
    # filter-replay records its packets so replay stays deterministic.
    -netdev "user,id=slopnet0,dns=1.1.1.1"
    -object "filter-replay,id=replay0,netdev=slopnet0"
    -device "virtio-net-pci,netdev=slopnet0,disable-legacy=on"
    -boot "order=d,menu=off"
    -device "isa-debug-exit,iobase=0xf4,iosize=0x01"
    -no-reboot
    -display none
)

case "$MODE" in
    record)
        echo "Recording to $RRFILE (serial -> $SERIAL_LOG; timeout ${RECORD_TIMEOUT}s)..."
        : > "$SERIAL_LOG"
        ICOUNT="shift=${ICOUNT_SHIFT},rr=record,rrfile=${RRFILE},rrsnapshot=rrinit"
        set +e
        if [ "$RECORD_TIMEOUT" -gt 0 ]; then
            timeout "${RECORD_TIMEOUT}s" "$QEMU_BIN" "${COMMON[@]}" \
                -icount "$ICOUNT" -serial "file:$SERIAL_LOG"
        else
            "$QEMU_BIN" "${COMMON[@]}" -icount "$ICOUNT" -serial "file:$SERIAL_LOG"
        fi
        status=$?
        set -e
        echo "Record finished (qemu exit $status). Replay with: scripts/qemu_rr.sh replay '$ISO' '$FS_IMAGE'"
        ;;
    replay)
        if [ ! -f "$RRFILE" ]; then
            echo "RRFILE missing ($RRFILE) — run 'record' first." >&2
            exit 1
        fi
        echo "Replaying $RRFILE; gdbstub on tcp::$GDB_PORT (halted at reset). Connect GDB now."
        ICOUNT="shift=${ICOUNT_SHIFT},rr=replay,rrfile=${RRFILE},rrsnapshot=rrinit"
        exec "$QEMU_BIN" "${COMMON[@]}" \
            -icount "$ICOUNT" -serial "file:$SERIAL_LOG" \
            -S -gdb "tcp::$GDB_PORT"
        ;;
    *)
        echo "Unknown mode: $MODE (record|replay)" >&2
        exit 1
        ;;
esac
