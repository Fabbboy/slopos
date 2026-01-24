#
# SlopOS Context Switching Assembly
# Low-level task context switching for x86_64
# AT&T syntax for cooperative task switching
#

.section .text
.global context_switch

# FPU state offset from TaskContext pointer (TaskContext is 200 bytes, +8 padding for 16-byte alignment)
.equ FPU_STATE_OFFSET, 0xD0

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
# FPU state at offset 0xD0 from context pointer (512 bytes, 16-byte aligned)
#

context_switch:
    test    %rdi, %rdi
    jz      .Lctx_load

    # Save FPU/SSE state first (before we clobber any XMM regs)
    leaq    FPU_STATE_OFFSET(%rdi), %rax
    fxsave64 (%rax)

    # Save GPRs
    movq    %rax, 0x00(%rdi)
    movq    %rbx, 0x08(%rdi)
    movq    %rcx, 0x10(%rdi)
    movq    %rdx, 0x18(%rdi)
    movq    %rsi, 0x20(%rdi)
    movq    %rdi, 0x28(%rdi)
    movq    %rbp, 0x30(%rdi)
    movq    %rsp, 0x38(%rdi)
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

    # Switch CR3 if needed
    movq    0xC0(%r15), %rax
    movq    %cr3, %rdx
    cmpq    %rax, %rdx
    je      .Lctx_cr3_done
    movq    %rax, %cr3
.Lctx_cr3_done:

    # Restore FPU/SSE state before loading GPRs
    leaq    FPU_STATE_OFFSET(%r15), %rax
    fxrstor64 (%rax)

    # Segments - restore DS, ES, FS, SS but NOT GS
    # Writing to GS selector zeros IA32_GS_BASE MSR in long mode, breaking per-CPU access
    movq    0x98(%r15), %rax
    movw    %ax, %ds
    movq    0xA0(%r15), %rax
    movw    %ax, %es
    movq    0xA8(%r15), %rax
    movw    %ax, %fs
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

    # RFLAGS
    movq    0x88(%r15), %rax
    pushq   %rax
    popfq

    # Stack and return
    movq    0x38(%r15), %rsp
    pushq   0x80(%r15)

    movq    0x78(%r15), %r15

    retq

#
# context_switch_user(void *old_context, void *new_context)
# Save kernel context (if provided) and enter user mode via IRETQ.
#
.global context_switch_user
context_switch_user:
    test    %rdi, %rdi
    jz      .Lctx_user_load

    # Save FPU/SSE state first
    leaq    FPU_STATE_OFFSET(%rdi), %rax
    fxsave64 (%rax)

    # Save GPRs
    movq    %rax, 0x00(%rdi)
    movq    %rbx, 0x08(%rdi)
    movq    %rcx, 0x10(%rdi)
    movq    %rdx, 0x18(%rdi)
    movq    %rsi, 0x20(%rdi)
    movq    %rdi, 0x28(%rdi)
    movq    %rbp, 0x30(%rdi)
    movq    %rsp, 0x38(%rdi)
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

    # Restore FPU/SSE state
    leaq    FPU_STATE_OFFSET(%r15), %rax
    fxrstor64 (%rax)

    # Segments (excluding GS - managed by SWAPGS for SYSCALL compatibility)
    movq    0x98(%r15), %rax
    movw    %ax, %ds
    movq    0xA0(%r15), %rax
    movw    %ax, %es
    movq    0xA8(%r15), %rax
    movw    %ax, %fs
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

    # First: Restore KERNEL_GS_BASE to the per-CPU data pointer
    # MSR 0xC0000102 = IA32_KERNEL_GS_BASE
    movl $0xC0000102, %ecx
    movq SYSCALL_CPU_DATA_PTR(%rip), %rax
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

    # Save FPU/SSE state
    leaq    FPU_STATE_OFFSET(%r8), %rax
    fxsave64 (%rax)

    # Save callee-saved registers
    movq    %rsp, 0x38(%r8)
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
    # Restore FPU/SSE state
    leaq    FPU_STATE_OFFSET(%r9), %rax
    fxrstor64 (%rax)

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
    movq    %rsp, 0x38(%rdi)        # Save rsp
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
