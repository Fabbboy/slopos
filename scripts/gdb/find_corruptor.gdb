# find_corruptor.gdb — batch reverse-debug: run to the user fault, then (if a
# watch target is given) reverse-continue to the instruction that corrupted it.
#
# Usage:
#   gdb -q -batch -x scripts/gdb/find_corruptor.gdb              # dump fault only
#   gdb -q -batch -ex 'set $watch_va = 0x4090fe' \               # + find writer
#       -x scripts/gdb/find_corruptor.gdb
#
# $watch_va is a guest VIRTUAL address (in the victim's address space) whose
# corruptor you want to find — e.g. the faulting RIP (corrupted code/return)
# or a smashed stack slot. The script resolves it to a physical address under
# the victim CR3 and arms a physical watchpoint so the write is caught no
# matter which CR3/alias performed it, then reverse-continues to that write.

set pagination off
set confirm off
set architecture i386:x86-64
file builddir/kernel.elf

python
import os, gdb
ue = os.environ.get("SLOPOS_USER_ELF", "builddir/io_capture_test.elf")
if os.path.exists(ue):
    gdb.execute("add-symbol-file %s" % ue)
end

source scripts/gdb/slopos_mmu.py

break terminate_user_task
break panic_with_frame
target remote :1234

printf "[find_corruptor] running to the fault...\n"
continue

printf "\n========= FAULT REACHED =========\n"
printf "handler PC=%#lx CR3=%#lx (victim address space)\n", $pc, $cr3
# `frame` is the &InterruptFrame arg to terminate_user_task (debug build).
printf "fault frame: vec=%lu err=%#lx\n", frame->vector, frame->error_code
printf "  user RIP=%#lx CS=%#lx RSP=%#lx RFLAGS=%#lx\n", frame->rip, frame->cs, frame->rsp, frame->rflags
printf "  user RAX=%#lx RBX=%#lx RCX=%#lx RDX=%#lx\n", frame->rax, frame->rbx, frame->rcx, frame->rdx
printf "  user RBP=%#lx RSI=%#lx RDI=%#lx\n", frame->rbp, frame->rsi, frame->rdi
set $victim_cr3 = $cr3

if $watch_va != 0
  printf "\n[find_corruptor] arming physical watchpoint for VA %#lx under CR3 %#lx\n", $watch_va, $victim_cr3
  wpva $victim_cr3 $watch_va
  printf "[find_corruptor] reverse-continuing to the corrupting write...\n"
  reverse-continue
  printf "\n========= CORRUPTOR =========\n"
  printf "writer PC=%#lx CS=%#lx CR3=%#lx\n", $pc, $cs, $cr3
  if ($cs & 3) == 3
    printf "writer privilege: USER (CPL3)\n"
  else
    printf "writer privilege: KERNEL (CPL0)\n"
  end
  if $cr3 == $victim_cr3
    printf "writer CR3 == victim CR3 (same address space)\n"
  else
    printf "writer CR3 != victim CR3 (FOREIGN address space — frame aliasing/UAF suspect)\n"
  end
  maintenance packet Qqemu.PhyMemMode:0
  bt 30
else
  printf "\n[find_corruptor] no $watch_va set — fault dumped above.\n"
  printf "Re-run with:  -ex 'set $watch_va = <corrupted-VA>'  to find the writer.\n"
  printf "Good first targets: the user RIP (corrupted code/return) or a stack slot near RSP.\n"
end

quit
