# OSTD user-return trampoline.
#
# `__ostd_user_return` is the LSTAR target installed by
# `boot/src/gdt.rs::syscall_msr_init`.  When the user executes
# SYSCALL, the CPU lands here with:
#   - RCX  = user RIP (return address)
#   - R11  = user RFLAGS
#   - RAX  = syscall number
#   - RDI/RSI/RDX/R10/R8/R9 = syscall args (Linux x86_64 ABI)
#   - User RSP intact; user GS intact (SWAPGS not yet performed).
#
# The trampoline saves user state into the per-CPU `UserContext` slot
# stashed by `PcrUserModeBackend::execute_round_trip`, encodes
# `ReturnReason::Syscall(rax)` into the per-CPU return-reason slot, and
# then unwinds back to the caller of `execute_round_trip` by restoring
# the saved kernel callee-save snapshot from
# `pcr.kernel_return_ctx` and `jmp`ing to the saved RIP.
#
# Offsets are mirrored from `slopos-ostd/src/cpu/x86_64/pcr.rs` (the
# `pub mod offsets` block) and `slopos-ostd/src/user/context.rs`
# (`UserRegs` field offsets).  Drift between Rust and asm is caught at
# compile time by the `const _: () = assert!(...)` razors in those
# files.
#
# AT&T syntax mode is selected by the `options(att_syntax)` flag on
# the `global_asm!` invocation that includes this file.

.section .text

# PCR offsets.  Mirrored from `pcr::offsets::*`.
.equ PCR_USER_RSP_TMP,        8
.equ PCR_KERNEL_RSP,          16
.equ PCR_USER_CTX_PTR,        96
.equ PCR_KERNEL_RETURN_CTX,   104
.equ PCR_RETURN_REASON_KIND,  168
.equ PCR_RETURN_REASON_PAYLD, 176
.equ PCR_USER_RAX_TMP,        184

# KernelReturnContext field offsets, relative to PCR_KERNEL_RETURN_CTX.
.equ KRC_RBX, 0
.equ KRC_RBP, 8
.equ KRC_R12, 16
.equ KRC_R13, 24
.equ KRC_R14, 32
.equ KRC_R15, 40
.equ KRC_RSP, 48
.equ KRC_RIP, 56

# UserRegs field offsets.  Mirrored from `UserRegs` in
# `slopos-ostd/src/user/context.rs`.
.equ UR_RAX, 0
.equ UR_RBX, 8
.equ UR_RCX, 16
.equ UR_RDX, 24
.equ UR_RSI, 32
.equ UR_RDI, 40
.equ UR_RBP, 48
.equ UR_RSP, 56
.equ UR_R8,  64
.equ UR_R9,  72
.equ UR_R10, 80
.equ UR_R11, 88
.equ UR_R12, 96
.equ UR_R13, 104
.equ UR_R14, 112
.equ UR_R15, 120
.equ UR_RIP, 128
.equ UR_RFLAGS, 136

# Kernel data segment selector (matches SegmentSelector::KERNEL_DATA).
.equ SEL_KERNEL_DATA, 0x10

# Return-reason kind discriminants (mirrored from pcr.rs constants).
.equ RR_KIND_SYSCALL, 1

.global __ostd_user_return
.global __ostd_user_return_end

__ostd_user_return:
    # Switch to kernel GS so gs:[…] addresses the local PCR.
    swapgs

    # CRITICAL: spill user RAX into a per-CPU PCR scratch slot rather
    # than pushing onto the kernel stack at `kernel_rsp - 8`.  That
    # address is the SS slot of the next CPU-pushed IRET frame at
    # TSS.RSP0; pushing user RAX there silently corrupts the SS field
    # for any subsequent interrupt that finds the slot before being
    # CPU-pushed-over.  Asterinas / Linux use the equivalent per-CPU
    # scratch.
    movq %rax, %gs:PCR_USER_RAX_TMP

    # Stash user RSP into a separate per-CPU slot, then switch %rsp
    # to the per-task kernel stack top.  We deliberately do NOT push
    # anything onto this stack — `pushq` here would write to
    # `kernel_rsp - 8`, which is the SS slot of the next CPU-pushed
    # IRET frame.  All scratch goes through PCR slots; the final
    # RSP/RIP come from `pcr.kernel_return_ctx` so this stack value
    # is only valid for the brief window before we `jmp` away.
    movq %rsp, %gs:PCR_USER_RSP_TMP
    movq %gs:PCR_KERNEL_RSP, %rsp

    # Load active UserContext pointer.  If `execute_round_trip` did its
    # job, this is non-null; otherwise we fault here (which is exactly
    # what we want — it surfaces a configuration error immediately).
    movq %gs:PCR_USER_CTX_PTR, %rax

    # Save user GPRs (RAX comes from PCR scratch slot below).
    movq %rbx, UR_RBX(%rax)
    movq %rcx, UR_RCX(%rax)        # SYSCALL puts user RIP in RCX.
    movq %rdx, UR_RDX(%rax)
    movq %rsi, UR_RSI(%rax)
    movq %rdi, UR_RDI(%rax)
    movq %rbp, UR_RBP(%rax)
    movq %r8,  UR_R8(%rax)
    movq %r9,  UR_R9(%rax)
    movq %r10, UR_R10(%rax)
    movq %r11, UR_R11(%rax)        # SYSCALL puts user RFLAGS in R11.
    movq %r12, UR_R12(%rax)
    movq %r13, UR_R13(%rax)
    movq %r14, UR_R14(%rax)
    movq %r15, UR_R15(%rax)

    # ctx.rip = user RIP (RCX), ctx.rflags_user_subset = R11.
    movq %rcx, UR_RIP(%rax)
    movq %r11, UR_RFLAGS(%rax)

    # ctx.rsp = saved user RSP (from PCR scratch slot).
    movq %gs:PCR_USER_RSP_TMP, %rdx
    movq %rdx, UR_RSP(%rax)

    # ctx.rax = saved user RAX (from PCR scratch slot).  Also used as
    # the syscall-number payload in the ReturnReason encoding.
    movq %gs:PCR_USER_RAX_TMP, %rdx
    movq %rdx, UR_RAX(%rax)

    # Encode ReturnReason::Syscall(rax).
    movq %rdx, %gs:PCR_RETURN_REASON_PAYLD
    movq $RR_KIND_SYSCALL, %gs:PCR_RETURN_REASON_KIND

    # Restore kernel data segments (matches what the kernel left them
    # as on the way out — the exception-handler asm does the same).
    movw $SEL_KERNEL_DATA, %ax
    movw %ax, %ds
    movw %ax, %es

    # Restore kernel callee-saves from kernel_return_ctx.
    movq %gs:(PCR_KERNEL_RETURN_CTX + KRC_RBX), %rbx
    movq %gs:(PCR_KERNEL_RETURN_CTX + KRC_RBP), %rbp
    movq %gs:(PCR_KERNEL_RETURN_CTX + KRC_R12), %r12
    movq %gs:(PCR_KERNEL_RETURN_CTX + KRC_R13), %r13
    movq %gs:(PCR_KERNEL_RETURN_CTX + KRC_R14), %r14
    movq %gs:(PCR_KERNEL_RETURN_CTX + KRC_R15), %r15

    # Restore RSP back to where execute_round_trip left it (just below
    # the call that entered user_mode_round_trip_asm), then re-enable
    # IRQs and jmp to the saved return RIP.
    #
    # `sti` is essential: SFMASK clears IF on SYSCALL entry, so we land
    # here with interrupts disabled.  Without re-enabling, all
    # kernel-side syscall handler work runs with IF=0 — no timer ticks
    # fire on this CPU, the per-CPU tick counter falls behind the
    # global counter (incremented by other CPUs), and the cross-CPU
    # watchdog at `core::scheduler::runtime::check_watchdog_for_neighbor`
    # eventually NMIs us as "stuck".  The legacy `syscall_entry` issued
    # `sti` before calling into Rust for the same reason; Linux's
    # `entry_SYSCALL_64` does the equivalent `ENABLE_INTERRUPTS` after
    # publishing the user pt_regs.  The `sti` shadow inhibits IRQs
    # until the immediately-following `jmpq` completes, so this is safe
    # to do as the last step before the jmp.
    movq %gs:(PCR_KERNEL_RETURN_CTX + KRC_RIP), %rax
    movq %gs:(PCR_KERNEL_RETURN_CTX + KRC_RSP), %rsp
    sti
    jmpq *%rax

__ostd_user_return_end:
    # Marker symbol so consumers can compute the trampoline byte-size
    # for diagnostics; not reached at runtime.
    ret
