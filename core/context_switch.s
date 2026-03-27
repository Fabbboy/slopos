#
# SlopOS Context Switching Assembly
# Low-level task context switching for x86_64
# AT&T syntax for cooperative task switching
#
# FPU/SIMD state is saved/restored unconditionally using XSAVE64/XRSTOR64.
# XSAVE is a hard boot requirement — the kernel panics in xsave::init() if
# the CPU does not support it.  There is no FXSAVE fallback.
#
# The component mask ACTIVE_XCR0 (AtomicU64, #[no_mangle]) is defined in
# lib/src/cpu/xsave.rs and referenced here via RIP-relative addressing.
#

.section .text
.global context_switch

# FPU state offset from TaskContext pointer (TaskContext is 200 bytes + 8 padding for 16-byte alignment)
.equ FPU_STATE_OFFSET, 0xD0

# ---------------------------------------------------------------------------
# FPU_SAVE / FPU_RESTORE macros
#
# Save or restore the full FPU/SIMD state (x87, SSE, AVX, AVX-512) using
# unconditional XSAVE64/XRSTOR64.  XSAVE is a hard boot requirement.
#
# Parameters:
#   \ctx_reg — register holding the context / task pointer.
#              Must NOT be %rax, %rcx, or %rdx.
#
# Clobbers: %rax, %rcx, %rdx  (all volatile / caller-saved).
#
# The FPU state buffer at FPU_STATE_OFFSET(\ctx_reg) must be 64-byte
# aligned and at least `xsave::area_size()` bytes (see FpuState in
# task_struct.rs — compile-time max 2688 B).
# ---------------------------------------------------------------------------

.macro FPU_SAVE ctx_reg
    leaq    FPU_STATE_OFFSET(\ctx_reg), %rcx
    movq    ACTIVE_XCR0(%rip), %rax
    movq    %rax, %rdx
    shrq    $32, %rdx
    xsave64 (%rcx)
.endm

.macro FPU_RESTORE ctx_reg
    leaq    FPU_STATE_OFFSET(\ctx_reg), %rcx
    movq    ACTIVE_XCR0(%rip), %rax
    movq    %rax, %rdx
    shrq    $32, %rdx
    xrstor64 (%rcx)
.endm

#
# context_switch(void *old_context, void *new_context)
#   rdi = old_context (may be NULL)
#   rsi = new_context (must not be NULL)
#
# Context layout (TaskContext, 200 bytes):
#   0x00-0x78: GPRs (rax-r15)
#   0x80: rip, 0x88: rflags
#   0x90-0xB8: segment registers
#   0xC0: cr3
# FPU/SIMD state at FPU_STATE_OFFSET from context pointer (up to 2688 bytes, 64-byte aligned)
#

context_switch:
    test    %rdi, %rdi
    jz      .Lctx_load

    # Save FPU/SIMD state first (before we clobber any XMM/YMM/ZMM regs).
    # Clobbers rax, rcx, rdx — all volatile and saved to context below.
    FPU_SAVE %rdi

    # Save GPRs
    movq    %rax, 0x00(%rdi)
    movq    %rbx, 0x08(%rdi)
    movq    %rcx, 0x10(%rdi)
    movq    %rdx, 0x18(%rdi)
    movq    %rsi, 0x20(%rdi)
    movq    %rdi, 0x28(%rdi)
    movq    %rbp, 0x30(%rdi)

    # Save RSP+8 (skip the return address pushed by `call`) so that the
    # restore path's pushq+retq leaves RSP correctly aligned for the Rust
    # wrapper's epilogue. Without this, the extra pushq shifts the stack by
    # 8 bytes and the wrapper's pop rbp / ret read wrong slots.
    leaq    8(%rsp), %rax
    movq    %rax, 0x38(%rdi)

    movq    %r8,  0x40(%rdi)
    movq    %r9,  0x48(%rdi)
    movq    %r10, 0x50(%rdi)
    movq    %r11, 0x58(%rdi)
    movq    %r12, 0x60(%rdi)
    movq    %r13, 0x68(%rdi)
    movq    %r14, 0x70(%rdi)
    movq    %r15, 0x78(%rdi)

    movq    (%rsp), %rax
    movq    %rax, 0x80(%rdi)

    pushfq
    popq    %rax
    movq    %rax, 0x88(%rdi)

    movw    %cs, %ax
    movq    %rax, 0x90(%rdi)
    movw    %ds, %ax
    movq    %rax, 0x98(%rdi)
    movw    %es, %ax
    movq    %rax, 0xA0(%rdi)
    movw    %fs, %ax
    movq    %rax, 0xA8(%rdi)
    movw    %gs, %ax
    movq    %rax, 0xB0(%rdi)
    movw    %ss, %ax
    movq    %rax, 0xB8(%rdi)

    movq    %cr3, %rax
    movq    %rax, 0xC0(%rdi)

.Lctx_load:
    movq    %rsi, %r15

    # Validate kernel-mode switch targets before touching CR3/segments.
    # If CS is ring 0, RIP must be inside kernel .text. Otherwise divert
    # to a recovery handler instead of retq'ing into data memory.
    movq    0x90(%r15), %rax
    testb   $0x3, %al
    jnz     .Lctx_target_ok
    movq    0x80(%r15), %rax
    leaq    _text_start(%rip), %rcx
    cmpq    %rcx, %rax
    jb      .Lctx_bad_kernel_target
    leaq    _text_end(%rip), %rcx
    cmpq    %rcx, %rax
    jae     .Lctx_bad_kernel_target
.Lctx_target_ok:

    # Switch CR3 if needed
    movq    0xC0(%r15), %rax
    movq    %cr3, %rdx
    cmpq    %rax, %rdx
    je      .Lctx_cr3_done
    movq    %rax, %cr3
.Lctx_cr3_done:

    # Restore FPU/SIMD state before loading GPRs.
    # Clobbers rax, rcx, rdx — all overwritten by GPR restore below.
    FPU_RESTORE %r15

    # Segments - restore DS, ES, SS but NOT GS or FS
    # Writing to GS selector zeros IA32_GS_BASE MSR in long mode, breaking per-CPU access
    # Writing to FS selector zeros IA32_FS_BASE MSR in long mode, breaking user TLS
    # Both are managed via their respective MSRs by execute_task()
    movq    0x98(%r15), %rax
    movw    %ax, %ds
    movq    0xA0(%r15), %rax
    movw    %ax, %es
    movq    0xB8(%r15), %rax
    movw    %ax, %ss

    # GPRs
    movq    0x00(%r15), %rax
    movq    0x08(%r15), %rbx
    movq    0x10(%r15), %rcx
    movq    0x18(%r15), %rdx
    movq    0x20(%r15), %rsi
    movq    0x28(%r15), %rdi
    movq    0x30(%r15), %rbp
    movq    0x40(%r15), %r8
    movq    0x48(%r15), %r9
    movq    0x50(%r15), %r10
    movq    0x58(%r15), %r11
    movq    0x60(%r15), %r12
    movq    0x68(%r15), %r13
    movq    0x70(%r15), %r14

    # Stack and return — restore RSP and push target RIP BEFORE restoring
    # RFLAGS.  The old code restored RFLAGS (re-enabling interrupts via IF)
    # then switched RSP and pushed the return address.  A timer interrupt
    # in that window would push an interrupt frame onto the target kernel
    # stack, overwriting the return address.
    movq    0x38(%r15), %rsp
    pushq   0x80(%r15)

    # Restore RFLAGS with IF *always* cleared.  Callers re-enable
    # interrupts explicitly (restore_flags / sti) after context_switch
    # returns.  Without this mask, a pending IPI can fire between popfq
    # and retq.  At that point per-CPU current_task already points to the
    # dispatched task (set by execute_task before the switch), but RSP is
    # the idle stack — the IPI handler's switch_from_current_to_idle then
    # saves idle-stack RSP into the dispatched task's context, corrupting
    # it.  The next dispatch of that task runs on the idle stack, and two
    # concurrent users of the same stack corrupt each other's IRET frames.
    movq    0x88(%r15), %rax
    andq    $~0x200, %rax       # clear IF (bit 9) — no interrupts before retq
    movq    0x78(%r15), %r15
    pushq   %rax
    popfq
    retq

.Lctx_bad_kernel_target:
    movq    %r15, %rdi
    callq   context_switch_bad_target
    ud2

#
# context_switch_user(void *old_context, void *new_context)
# Save kernel context (if provided) and enter user mode via IRETQ.
#
.global context_switch_user
context_switch_user:
    test    %rdi, %rdi
    jz      .Lctx_user_load

    # Save FPU/SIMD state first.
    # Clobbers rax, rcx, rdx — all volatile and saved to context below.
    FPU_SAVE %rdi

    # Save GPRs
    movq    %rax, 0x00(%rdi)
    movq    %rbx, 0x08(%rdi)
    movq    %rcx, 0x10(%rdi)
    movq    %rdx, 0x18(%rdi)
    movq    %rsi, 0x20(%rdi)
    movq    %rdi, 0x28(%rdi)
    movq    %rbp, 0x30(%rdi)

    # Save RSP+8: see context_switch save comment for rationale.
    leaq    8(%rsp), %rax
    movq    %rax, 0x38(%rdi)

    movq    %r8,  0x40(%rdi)
    movq    %r9,  0x48(%rdi)
    movq    %r10, 0x50(%rdi)
    movq    %r11, 0x58(%rdi)
    movq    %r12, 0x60(%rdi)
    movq    %r13, 0x68(%rdi)
    movq    %r14, 0x70(%rdi)
    movq    %r15, 0x78(%rdi)

    movq    (%rsp), %rax
    movq    %rax, 0x80(%rdi)

    pushfq
    popq    %rax
    movq    %rax, 0x88(%rdi)

    movw    %cs, %ax
    movq    %rax, 0x90(%rdi)
    movw    %ds, %ax
    movq    %rax, 0x98(%rdi)
    movw    %es, %ax
    movq    %rax, 0xA0(%rdi)
    movw    %fs, %ax
    movq    %rax, 0xA8(%rdi)
    movw    %gs, %ax
    movq    %rax, 0xB0(%rdi)
    movw    %ss, %ax
    movq    %rax, 0xB8(%rdi)

    movq    %cr3, %rax
    movq    %rax, 0xC0(%rdi)

.Lctx_user_load:
    movq    %rsi, %r15

    # Switch CR3 FIRST (before using any stack that might not be mapped in new address space)
    movq    0xC0(%r15), %rax
    movq    %cr3, %rdx
    cmpq    %rax, %rdx
    je      .Lctx_user_cr3_done
    movq    %rax, %cr3
.Lctx_user_cr3_done:

    # Now build IRET frame (stack is guaranteed mapped since TSS RSP0 was set to new task's kernel stack)
    movq    0xB8(%r15), %rax
    pushq   %rax
    movq    0x38(%r15), %rax
    pushq   %rax
    movq    0x88(%r15), %rax
    pushq   %rax
    movq    0x90(%r15), %rax
    pushq   %rax
    movq    0x80(%r15), %rax
    pushq   %rax

    # Restore FPU/SIMD state.
    # Clobbers rax, rcx, rdx — overwritten by MSR writes and GPR restore below.
    FPU_RESTORE %r15

    # Segments (excluding GS - managed by SWAPGS for SYSCALL compatibility)
    movq    0x98(%r15), %rax
    movw    %ax, %ds
    movq    0xA0(%r15), %rax
    movw    %ax, %es
    # FS selector is NOT restored — loading FS=0 would clobber FS_BASE to 0.
    # FS_BASE is set via MSR by execute_task() before entering this function.
    # GS selector is NOT restored - SWAPGS manages GS_BASE MSR state
    # Writing to GS selector would not affect the MSR anyway in 64-bit mode

    # Set up GS_BASE for SYSCALL compatibility before returning to user mode.
    #
    # CRITICAL: KERNEL_GS_BASE may have been zeroed if the previous task did a
    # SYSCALL and we're switching from within that syscall handler. When a user
    # task does SYSCALL, SWAPGS swaps GS_BASE <-> KERNEL_GS_BASE. If the syscall
    # handler then calls schedule() -> context_switch_user, KERNEL_GS_BASE is
    # still 0 from that SWAPGS. We MUST restore it before returning to user mode.
    #
    # After IRETQ, user runs with GS_BASE=0. When user does SYSCALL,
    # SWAPGS will swap GS_BASE(0) <-> KERNEL_GS_BASE(per-cpu), which is correct.

    # First: Restore KERNEL_GS_BASE to the per-CPU PCR pointer
    # MSR 0xC0000102 = IA32_KERNEL_GS_BASE
    # Use gs:0 (self_ref) to get current CPU's PCR address
    movl $0xC0000102, %ecx
    movq %gs:0, %rax
    movq %rax, %rdx
    shrq $32, %rdx
    wrmsr

    # Second: Set GS_BASE = 0 (user mode sees GS_BASE=0)
    # MSR 0xC0000101 = IA32_GS_BASE
    movl $0xC0000101, %ecx
    xorl %eax, %eax
    xorl %edx, %edx
    wrmsr

    # GPRs (restore after MSR write since we clobbered eax/ecx/edx)
    movq    0x00(%r15), %rax
    movq    0x08(%r15), %rbx
    movq    0x10(%r15), %rcx
    movq    0x18(%r15), %rdx
    movq    0x20(%r15), %rsi
    movq    0x28(%r15), %rdi
    movq    0x30(%r15), %rbp
    movq    0x40(%r15), %r8
    movq    0x48(%r15), %r9
    movq    0x50(%r15), %r10
    movq    0x58(%r15), %r11
    movq    0x60(%r15), %r12
    movq    0x68(%r15), %r13
    movq    0x70(%r15), %r14
    movq    0x78(%r15), %r15

    iretq

#
# Simplified context switch for debugging (uses jmp instead of iret)
#
.global simple_context_switch
simple_context_switch:
    movq    %rdi, %r8
    movq    %rsi, %r9

    test    %r8, %r8
    jz      simple_load_new

    # Save FPU/SIMD state.
    # Clobbers rax, rcx, rdx — all volatile.
    FPU_SAVE %r8

    # Save callee-saved registers (RSP+8: see context_switch save comment)
    leaq    8(%rsp), %rax
    movq    %rax, 0x38(%r8)
    movq    %rbp, 0x30(%r8)
    movq    %rbx, 0x08(%r8)
    movq    %rsi, 0x20(%r8)
    movq    %rdi, 0x28(%r8)
    movq    %r12, 0x60(%r8)
    movq    %r13, 0x68(%r8)
    movq    %r14, 0x70(%r8)
    movq    %r15, 0x78(%r8)

    movq    (%rsp), %rax
    movq    %rax, 0x80(%r8)

    movq    %r9, %rsi

simple_load_new:
    # Restore FPU/SIMD state.
    # Clobbers rax, rcx, rdx — overwritten by register restore below.
    FPU_RESTORE %r9

    # Restore callee-saved registers
    movq    0x38(%r9), %rsp
    movq    0x30(%r9), %rbp
    movq    0x08(%r9), %rbx
    movq    0x60(%r9), %r12
    movq    0x68(%r9), %r13
    movq    0x70(%r9), %r14
    movq    0x78(%r9), %r15
    movq    0x20(%r9), %rsi
    movq    0x28(%r9), %rdi

    jmpq    *0x80(%r9)

#
# Task entry point wrapper
# This is called when a new task starts execution for the first time
#
.global task_entry_wrapper
task_entry_wrapper:
    # At this point, the task entry point is in %rdi (from context setup)
    # and the task argument is already in %rsi

    # Preserve entry point and move argument into ABI position
    movq    %rdi, %rax              # Save entry function pointer
    movq    %rsi, %rdi              # Move argument into first parameter register

    # Call the task entry function
    callq   *%rax

    # If task returns, hand control back to the scheduler to terminate
    callq   scheduler_task_exit

    # Should never reach here, but halt defensively
    hlt

#
# Initialize first task context for kernel
# Used when transitioning from kernel boot to first task
#
.global init_kernel_context
init_kernel_context:
    # rdi points to kernel context structure to initialize
    # This saves current kernel state as a "task" context

    # Save current kernel registers
    movq    %rax, 0x00(%rdi)        # Save rax
    movq    %rbx, 0x08(%rdi)        # Save rbx
    movq    %rcx, 0x10(%rdi)        # Save rcx
    movq    %rdx, 0x18(%rdi)        # Save rdx
    movq    %rsi, 0x20(%rdi)        # Save rsi
    movq    %rdi, 0x28(%rdi)        # Save rdi
    movq    %rbp, 0x30(%rdi)        # Save rbp
    leaq    8(%rsp), %rax           # Save rsp+8 (see context_switch save comment)
    movq    %rax, 0x38(%rdi)
    movq    %r8,  0x40(%rdi)        # Save r8
    movq    %r9,  0x48(%rdi)        # Save r9
    movq    %r10, 0x50(%rdi)        # Save r10
    movq    %r11, 0x58(%rdi)        # Save r11
    movq    %r12, 0x60(%rdi)        # Save r12
    movq    %r13, 0x68(%rdi)        # Save r13
    movq    %r14, 0x70(%rdi)        # Save r14
    movq    %r15, 0x78(%rdi)        # Save r15

    # Save return address as rip
    movq    (%rsp), %rax            # Get return address
    movq    %rax, 0x80(%rdi)        # Save as rip

    # Save current flags
    pushfq                          # Push flags
    popq    %rax                    # Pop to rax
    movq    %rax, 0x88(%rdi)        # Save rflags

    # Save current segments
    movw    %cs, %ax
    movq    %rax, 0x90(%rdi)        # Save cs
    movw    %ds, %ax
    movq    %rax, 0x98(%rdi)        # Save ds
    movw    %es, %ax
    movq    %rax, 0xA0(%rdi)        # Save es
    movw    %fs, %ax
    movq    %rax, 0xA8(%rdi)        # Save fs
    movw    %gs, %ax
    movq    %rax, 0xB0(%rdi)        # Save gs
    movw    %ss, %ax
    movq    %rax, 0xB8(%rdi)        # Save ss

    # Save current page directory
    movq    %cr3, %rax
    movq    %rax, 0xC0(%rdi)        # Save cr3

    ret
