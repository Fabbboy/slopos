# slopos.gdb — interactive driver for replaying a recorded SlopOS run.
#
# Loads kernel + userland symbols, the MMU helpers, connects to the replay
# gdbstub, and breaks where a user task faults fatally. After the break, use
# `udinfo` to dump the fault context, then `v2p`/`wpva` + `reverse-continue`
# to find whoever corrupted the memory.
#
# Swap the userland binary under test by setting SLOPOS_USER_ELF before
# launching (default: builddir/io_capture_test.elf). All userland test ELFs
# link at 0x400000, so only one can be loaded at a time.

set pagination off
set confirm off
set architecture i386:x86-64

# Kernel: fixed link address 0xFFFFFFFF80000000, full DWARF — symbols match runtime.
file builddir/kernel.elf

# Userland: ET_EXEC fixed at 0x400000 — add as-is (no relocation offset).
python
import os, gdb
ue = os.environ.get("SLOPOS_USER_ELF", "builddir/io_capture_test.elf")
if os.path.exists(ue):
    gdb.execute("add-symbol-file %s" % ue)
    gdb.write("[slopos] userland symbols: %s\n" % ue)
else:
    gdb.write("[slopos] no userland ELF at %s (set SLOPOS_USER_ELF)\n" % ue)
end

source scripts/gdb/slopos_mmu.py

# Catch both fault paths: #UD/page-fault user terminations and fatal panics.
break terminate_user_task
break panic_with_frame

# udinfo — fault context (privilege, RIP/CS/CR3) + backtrace.
define udinfo
  printf "RIP=%#lx CS=%#x SS=%#x RSP=%#lx CR3=%#lx\n", $pc, $cs, $ss, $sp, $cr3
  if ($cs & 3) == 3
    printf "privilege: USER (CPL3)\n"
  else
    printf "privilege: KERNEL (CPL0)\n"
  end
  bt 25
end
document udinfo
Print faulting context (RIP/CS/CR3/privilege) and a backtrace.
end

target remote :1234

echo \n[slopos] connected. 'continue' to run to the fault, then 'udinfo'.\n
echo [slopos] then: v2p $cr3 <corrupted-VA> ; wpva $cr3 <corrupted-VA> ; reverse-continue\n
