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
# The trampoline saves user state (including the syscall number in RAX)
# into the per-task `UserContext` stashed by
# `PcrUserModeBackend::execute_round_trip`, then unwinds back to the
# caller of `execute_round_trip` by restoring the saved kernel
# callee-save snapshot from `pcr.kernel_return_ctx` and `jmp`ing to the
# saved RIP.  The return reason is derived from that per-task
# `UserContext` (always a syscall on this path) — there is no per-CPU
# return-reason slot, so a preemption/migration after the `sti` below
# cannot corrupt it.
#
# Every offset below is a `const offset_of!` operand supplied by the
# `global_asm!` invocation that includes this file, so the names here
# resolve to whatever the Rust structs actually lay out. There is no
# mirror to drift.
#
# AT&T syntax mode is selected by the `options(att_syntax)` flag on
# the same invocation.

.section .text

# `ProcessorControlRegion` field offsets, reached through `gs:`.
.equ PCR_USER_RSP_TMP,        {pcr_user_rsp_tmp}
.equ PCR_KERNEL_RSP,          {pcr_kernel_rsp}
.equ PCR_USER_CTX_PTR,        {pcr_user_ctx_ptr}
.equ PCR_KERNEL_RETURN_CTX,   {pcr_kernel_return_ctx}
.equ PCR_USER_RAX_TMP,        {pcr_user_rax_tmp}

# KernelReturnContext field offsets, relative to PCR_KERNEL_RETURN_CTX.
.equ KRC_RBX, {krc_rbx}
.equ KRC_RBP, {krc_rbp}
.equ KRC_R12, {krc_r12}
.equ KRC_R13, {krc_r13}
.equ KRC_R14, {krc_r14}
.equ KRC_R15, {krc_r15}
.equ KRC_RSP, {krc_rsp}
.equ KRC_RIP, {krc_rip}

# UserRegs field offsets. `UserContext` keeps its register file at
# offset zero, so these double as displacements off the context pointer
# the PCR hands over.
.equ UR_RAX, {ur_rax}
.equ UR_RBX, {ur_rbx}
.equ UR_RCX, {ur_rcx}
.equ UR_RDX, {ur_rdx}
.equ UR_RSI, {ur_rsi}
.equ UR_RDI, {ur_rdi}
.equ UR_RBP, {ur_rbp}
.equ UR_RSP, {ur_rsp}
.equ UR_R8,  {ur_r8}
.equ UR_R9,  {ur_r9}
.equ UR_R10, {ur_r10}
.equ UR_R11, {ur_r11}
.equ UR_R12, {ur_r12}
.equ UR_R13, {ur_r13}
.equ UR_R14, {ur_r14}
.equ UR_R15, {ur_r15}
.equ UR_RIP, {ur_rip}
.equ UR_RFLAGS, {ur_rflags}

# Kernel data segment selector (matches SegmentSelector::KERNEL_DATA).
.equ SEL_KERNEL_DATA, {sel_kernel_data}

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

    # ctx.rax = saved user RAX (from PCR scratch slot) = the syscall
    # number.  This per-task write is the sole record of the return
    # reason: `execute_round_trip` reads it back from the UserContext
    # after the jmp, so no per-CPU return-reason slot is needed (the
    # per-task value is migration-safe across the trampoline's `sti`).
    movq %gs:PCR_USER_RAX_TMP, %rdx
    movq %rdx, UR_RAX(%rax)

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
