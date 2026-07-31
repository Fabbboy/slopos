# inspect_fault.gdb — forward fault inspection (no record/replay required).
#
# Attaches to a live gdbstub (real KVM/SMP repro environment), breaks at the
# user-fault path, and dumps everything needed to characterize the corruption:
# the fault frame, the ACTUAL bytes/instructions in guest memory at the
# faulting RIP (vs. what the ELF says they should be), and the page-table walk
# for that VA. Answers "is this page corrupted / mis-mapped, or is the #UD
# from CPU state?" without needing reverse debugging.
#
# Usage (kernel must be booting under qemu with -s -S):
#   gdb -q -batch -x scripts/gdb/inspect_fault.gdb

set pagination off
set confirm off
set architecture i386:x86-64
file builddir/kernel-dev.elf

python
import os, gdb
ue = os.environ.get("SLOPOS_USER_ELF", "builddir/io_capture_test.elf")
if os.path.exists(ue):
    gdb.execute("add-symbol-file %s" % ue)
end
source scripts/gdb/slopos_mmu.py

# Hardware breakpoints: software int3 can't be written to kernel VAs that
# aren't mapped yet at reset (and KVM .text may be write-protected). hbreak
# triggers on linear-address execution once paging is up. File:line targets
# avoid the pub(crate) name-resolution ambiguity.
target remote :1234
hbreak exception.rs:43
hbreak user_fault.rs:71

printf "[inspect] running to the first user fault...\n"
continue

# GDB is in Rust mode (kernel is Rust). `frame` is `*mut InterruptFrame`;
# deref + field access uses Rust syntax `(*frame).field`. Capture the values
# we need into convenience vars (numbers) so the rest is language-agnostic.
set $urip  = (*frame).rip
set $ursp  = (*frame).rsp
set $u_vec = (*frame).vector
set $u_err = (*frame).error_code
set $u_cs  = (*frame).cs

printf "\n========= USER FAULT =========\n"
printf "active CR3=%#lx  handler PC=%#lx  frame@%p\n", $cr3, $pc, frame
printf "fault: vec=%lu err=%#lx\n", $u_vec, $u_err
printf "user RIP=%#lx CS=%#lx RSP=%#lx\n", $urip, $u_cs, $ursp

printf "\n--- ACTUAL bytes/instructions at faulting RIP (as the CPU sees them) ---\n"
x/8i $urip
printf "raw bytes @RIP (64B): does it wrap at 0x100 (=> i as u8 fill)?\n"
x/64xb $urip
printf "raw bytes at page base (RIP & ~0xfff):\n"
set $pg = $urip & 0xfffffffffffff000
x/64xb $pg

printf "\n--- page-table walk for the faulting RIP under the active CR3 ---\n"
v2p $cr3 $urip

printf "\n--- user stack around RSP (return addresses / smashed slots) ---\n"
x/16xg $ursp

printf "\n--- page-table walk for the user stack ---\n"
v2p $cr3 $ursp

printf "\n[inspect] done.\n"
quit
