#!/usr/bin/env bash
set -euo pipefail

# Live GDB-stub launcher for SlopOS in its REAL run environment (KVM/SMP, no
# icount). Use this when a bug only reproduces under the production timing/SMP
# config (so record/replay's TCG+smp=1 can't capture it) and you want to break
# at a fault and inspect state forward — page tables, the fault frame, the
# actual bytes at the faulting RIP — even without reverse debugging.
#
# Boots halted (-S) with the gdbstub on GDB_PORT so GDB can set hardware
# breakpoints on kernel VAs before paging is up. exec's qemu so the process
# persists when launched as a background task.
#
# Usage: qemu_dbg.sh <iso> <fs_image>
#
# Environment:
#   QEMU_SMP   guest CPUs        (default 4 — matches the real repro)
#   QEMU_ACCEL accelerator       (default kvm:tcg)
#   QEMU_CPU   cpu model         (default host)
#   QEMU_MEM   RAM               (default 512M)
#   GDB_PORT   gdbstub TCP port  (default 1234)
#   HALT       1=start halted -S (default 1)
#   SERIAL_LOG serial capture    (default builddir/dbg-serial.log)
#   OVMF_DIR   firmware dir       (default third_party/ovmf)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

ISO="${1:?Usage: qemu_dbg.sh <iso> <fs_image>}"
FS_IMAGE="${2:?Usage: qemu_dbg.sh <iso> <fs_image>}"

QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
QEMU_SMP="${QEMU_SMP:-4}"
QEMU_ACCEL="${QEMU_ACCEL:-kvm:tcg}"
QEMU_CPU="${QEMU_CPU:-host}"
QEMU_MEM="${QEMU_MEM:-512M}"
GDB_PORT="${GDB_PORT:-1234}"
HALT="${HALT:-1}"
OVMF_DIR="${OVMF_DIR:-${REPO_ROOT}/third_party/ovmf}"
SERIAL_LOG="${SERIAL_LOG:-${REPO_ROOT}/builddir/dbg-serial.log}"

OVMF_CODE="${OVMF_DIR}/OVMF_CODE.fd"
OVMF_VARS_RUNTIME="${REPO_ROOT}/builddir/dbg-OVMF_VARS.fd"

"$SCRIPT_DIR/setup_ovmf.sh"
cp "${OVMF_DIR}/OVMF_VARS.fd" "$OVMF_VARS_RUNTIME"
: > "$SERIAL_LOG"

ARGS=(
    -machine "q35,accel=$QEMU_ACCEL"
    -cpu "$QEMU_CPU"
    -smp "$QEMU_SMP"
    -m "$QEMU_MEM"
    -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE"
    -drive "if=pflash,format=raw,file=$OVMF_VARS_RUNTIME"
    -device "ich9-ahci,id=ahci0,bus=pcie.0,addr=0x3"
    -drive "file=$ISO,if=none,format=raw,media=cdrom,readonly=on,id=cdrom"
    -device "ide-cd,bus=ahci0.0,drive=cdrom,bootindex=0"
    -drive "file=$FS_IMAGE,if=none,id=vd0,format=raw"
    -object "iothread,id=iot0"
    -device "virtio-blk-pci,drive=vd0,disable-legacy=on,iothread=iot0"
    # Same in-network echo peer `qemu_run.sh` configures, so a network failure
    # reproduced under gdb sees the environment the test ran in.
    -netdev "user,id=slopnet0,dns=1.1.1.1,guestfwd=tcp:10.0.2.100:9999-cmd:/bin/cat"
    -device "virtio-net-pci,netdev=slopnet0,disable-legacy=on"
    -boot "order=d,menu=off"
    -device "isa-debug-exit,iobase=0xf4,iosize=0x01"
    -no-reboot
    -serial "file:$SERIAL_LOG"
    -display none
    -monitor none
    -gdb "tcp::$GDB_PORT"
)
if [ "$HALT" != "0" ]; then
    ARGS+=(-S)
fi

echo "qemu_dbg: gdbstub on tcp::$GDB_PORT (halted=$HALT, smp=$QEMU_SMP, accel=$QEMU_ACCEL); serial -> $SERIAL_LOG" >&2
exec "$QEMU_BIN" "${ARGS[@]}" < /dev/null
