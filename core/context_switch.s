#
# SlopOS Context Switching Assembly
# Low-level task context switching for x86_64
# AT&T syntax for cooperative task switching
#

.section .text
.global context_switch

#
# context_switch(void *old_context, void *new_context)
#   rdi = old_context (may be NULL)
#   rsi = new_context (must not be NULL)
# Saves the current CPU state into old_context and restores the state from
# new_context, then returns into new_context->rip. Uses RET to avoid consuming
# a general-purpose register for the branch target.
#
# Context layout (task_context_t):
#   0x00 rax  0x08 rbx  0x10 rcx  0x18 rdx
#   0x20 rsi  0x28 rdi  0x30 rbp  0x38 rsp
#   0x40 r8   0x48 r9   0x50 r10  0x58 r11
#   0x60 r12  0x68 r13  0x70 r14  0x78 r15
#   0x80 rip  0x88 rflags
#   0x90 cs   0x98 ds   0xA0 es   0xA8 fs   0xB0 gs   0xB8 ss
#   0xC0 cr3
#

context_switch:
    /* Save current context if provided */
    test    %rdi, %rdi
    jz      .Lctx_load

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

    movq    (%rsp), %rax            /* return address -> rip */
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
    movq    %rsi, %r15              /* new_context base */

    /* Switch CR3 if needed */
    movq    0xC0(%r15), %rax
    movq    %cr3, %rdx
    cmpq    %rax, %rdx
    je      .Lctx_cr3_done
    movq    %rax, %cr3
.Lctx_cr3_done:

    /* Segments (CS remains unchanged for kernel switches) */
    movq    0x98(%r15), %rax
    movw    %ax, %ds
    movq    0xA0(%r15), %rax
    movw    %ax, %es
    movq    0xA8(%r15), %rax
    movw    %ax, %fs
    movq    0xB0(%r15), %rax
    movw    %ax, %gs
    movq    0xB8(%r15), %rax
    movw    %ax, %ss

    /* General purpose registers (except rsp) */
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

    /* RFLAGS from context */
    movq    0x88(%r15), %rax
    pushq   %rax
    popfq

    /* Stack pointer and return target */
    movq    0x38(%r15), %rsp
    pushq   0x80(%r15)              /* push rip onto new stack */

    /* Restore r15 last */
    movq    0x78(%r15), %r15

    retq

#
# context_switch_user(void *old_context, void *new_context)
# Save kernel context (if provided) and enter user mode via IRETQ.
# new_context must contain user selectors for CS/SS.
#
.global context_switch_user
context_switch_user:
    /* Save current context if provided */
    test    %rdi, %rdi
    jz      .Lctx_user_load

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
    movq    %rsi, %r15              /* new_context base */

    /* Build IRET frame on kernel stack (target RSP/SS are user values) */
    movq    0xB8(%r15), %rax        /* ss */
    pushq   %rax
    movq    0x38(%r15), %rax        /* user rsp */
    pushq   %rax
    movq    0x88(%r15), %rax        /* rflags */
    pushq   %rax
    movq    0x90(%r15), %rax        /* cs */
    pushq   %rax
    movq    0x80(%r15), %rax        /* rip */
    pushq   %rax

    /* Switch CR3 to user address space if needed */
    movq    0xC0(%r15), %rax
    movq    %cr3, %rdx
    cmpq    %rax, %rdx
    je      .Lctx_user_cr3_done
    movq    %rax, %cr3
.Lctx_user_cr3_done:

    /* User data segments (CS/SS via IRET frame) */
    movq    0x98(%r15), %rax
    movw    %ax, %ds
    movq    0xA0(%r15), %rax
    movw    %ax, %es
    movq    0xA8(%r15), %rax
    movw    %ax, %fs
    movq    0xB0(%r15), %rax
    movw    %ax, %gs

    /* General purpose registers */
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
# Alternative simplified context switch for debugging
# Uses simple jmp instead of full iret mechanism
#
.global simple_context_switch
simple_context_switch:
    # Save actual RDI and RSI register values before using them as context pointers
    # Use R8 and R9 as temporary storage for the context pointers
    movq    %rdi, %r8               # Save old_context pointer to r8
    movq    %rsi, %r9               # Save new_context pointer to r9

    # Check if we need to save old context
    test    %r8, %r8                # Test if old_context is NULL
    jz      simple_load_new         # Skip save if NULL

    # Save essential registers only (using r8 as pointer)
    movq    %rsp, 0x38(%r8)         # Save stack pointer
    movq    %rbp, 0x30(%r8)         # Save base pointer
    movq    %rbx, 0x08(%r8)         # Save rbx (callee-saved)
    movq    %rsi, 0x20(%r8)         # Save rsi (actual task value)
    movq    %rdi, 0x28(%r8)         # Save rdi (actual task value)
    movq    %r12, 0x60(%r8)         # Save r12 (callee-saved)
    movq    %r13, 0x68(%r8)         # Save r13 (callee-saved)
    movq    %r14, 0x70(%r8)         # Save r14 (callee-saved)
    movq    %r15, 0x78(%r8)         # Save r15 (callee-saved)

    # Save return address
    movq    (%rsp), %rax            # Get return address
    movq    %rax, 0x80(%r8)         # Save as rip

    # Restore new_context pointer from r9
    movq    %r9, %rsi               # Restore new_context pointer to rsi

simple_load_new:
    # Load new context (using r9 which still holds new_context pointer)
    movq    0x38(%r9), %rsp         # Load stack pointer
    movq    0x30(%r9), %rbp         # Load base pointer
    movq    0x08(%r9), %rbx         # Load rbx
    movq    0x60(%r9), %r12         # Load r12
    movq    0x68(%r9), %r13         # Load r13
    movq    0x70(%r9), %r14         # Load r14
    movq    0x78(%r9), %r15         # Load r15
    movq    0x20(%r9), %rsi         # Load rsi (actual task value)
    movq    0x28(%r9), %rdi         # Load rdi (actual task value)

    # Jump to new instruction pointer
    jmpq    *0x80(%r9)              # Jump to new rip (using r9 as pointer)

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
