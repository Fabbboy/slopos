# KPTI ring-transition trampolines.
#
# These replace the ring-3 → ring-0 and ring-0 → ring-3 paths owned by
# `slopos-ostd::user::mode` when KPTI is enabled.  They are not yet
# linked into the IDT / LSTAR targets — see `mm/src/mmu/kpti.rs` for
# the activation gate.  Once activation lands, `mmu::kpti::enable()`
# points IA32_LSTAR at `kpti_syscall_entry` and patches each
# ring-transition vector in the IDT to jump here instead of the
# pre-KPTI stubs.
#
# Per-CPU slot offsets (into the PCR):
#   0x00  self_ref
#   ...
#   +USER_RSP_SLOT        saved user RSP during entry
#   +TRAMPOLINE_RSP_SLOT  trampoline-local stack top
#   +KERNEL_RSP_SLOT      real kernel stack (TSS.RSP0)
#   +KERNEL_CR3_SLOT      Cr3Value::bits() for this CPU's kernel PCID
#   +USER_CR3_SLOT        Cr3Value::bits() for this CPU's user PCID
#
# TODO(tech-debt): hand-written PCR offsets — back them with karch::pcr const fns.

.set USER_RSP_SLOT,       0x100
.set TRAMPOLINE_RSP_SLOT, 0x108
.set KERNEL_RSP_SLOT,     0x110
.set KERNEL_CR3_SLOT,     0x118
.set USER_CR3_SLOT,       0x120

.section .text.kpti_trampoline, "ax"
.global kpti_syscall_entry
.global kpti_sysret_exit

# Hardware has already loaded:
#   rcx = user RIP (return)
#   r11 = user RFLAGS
#   rip = IA32_LSTAR (this symbol)
#   cs/ss = kernel selectors via STAR
# IF is cleared by SFMASK.  Interrupts remain off through the prologue.
kpti_syscall_entry:
    swapgs                              # %gs -> per-CPU PCR

    movq %rsp, %gs:USER_RSP_SLOT
    movq %gs:TRAMPOLINE_RSP_SLOT, %rsp

    # Free a scratch register.
    pushq %rax

    # NOFLUSH is baked into the stored CR3 value.
    movq %gs:KERNEL_CR3_SLOT, %rax
    movq %rax, %cr3

    popq %rax
    movq %gs:KERNEL_RSP_SLOT, %rsp

    jmp syscall_common_dispatch

# Called after `syscall_common_dispatch` finishes.
kpti_sysret_exit:
    # Caller guarantees all GPRs (except rax) are in their user-return
    # slots; rax holds the syscall return value.
    movq %gs:USER_CR3_SLOT, %rcx
    movq %rcx, %cr3

    movq %gs:USER_RSP_SLOT, %rsp
    swapgs
    sysretq
