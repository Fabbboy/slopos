#
# SlopOS Context Switching Assembly
# Low-level task context switching for x86_64
# AT&T syntax for cooperative task switching
#

.section .text
.global context_switch

#
# context_switch(void *old_context, void *new_context)
#
# Switches from current task context to new task context
#
# Parameters:
#   rdi - Pointer to old task context (may be NULL for first switch)
#   rsi - Pointer to new task context (must not be NULL)
#
# Context structure layout (matches task_context_t in task.c):
#   Offset 0x00: rax
#   Offset 0x08: rbx
#   Offset 0x10: rcx
#   Offset 0x18: rdx
#   Offset 0x20: rsi
#   Offset 0x28: rdi
#   Offset 0x30: rbp
#   Offset 0x38: rsp
#   Offset 0x40: r8
#   Offset 0x48: r9
#   Offset 0x50: r10
#   Offset 0x58: r11
#   Offset 0x60: r12
#   Offset 0x68: r13
#   Offset 0x70: r14
#   Offset 0x78: r15
#   Offset 0x80: rip
#   Offset 0x88: rflags
#   Offset 0x90: cs
#   Offset 0x98: ds
#   Offset 0xA0: es
#   Offset 0xA8: fs
#   Offset 0xB0: gs
#   Offset 0xB8: ss
#   Offset 0xC0: cr3
#

context_switch:
    # Save actual RDI and RSI register values before using them as context pointers
    # We need to preserve the task's real register state, not the function arguments
    # Use R8 and R9 as temporary storage for the context pointers
    movq    %rdi, %r8               # Save old_context pointer to r8
    movq    %rsi, %r9               # Save new_context pointer to r9

    # Check if we need to save old context
    test    %r8, %r8                # Test if old_context is NULL
    jz      load_new_context        # Skip save if NULL (first task switch)

    # Save current CPU state to old context (using r8 as pointer)
    movq    %rax, 0x00(%r8)         # Save rax
    movq    %rbx, 0x08(%r8)         # Save rbx
    movq    %rcx, 0x10(%r8)         # Save rcx
    movq    %rdx, 0x18(%r8)         # Save rdx
    movq    %rsi, 0x20(%r8)         # Save rsi (actual task value, not pointer)
    movq    %rdi, 0x28(%r8)         # Save rdi (actual task value, not pointer)
    movq    %rbp, 0x30(%r8)         # Save rbp
    movq    %rsp, 0x38(%r8)         # Save current stack pointer
    movq    %r8,  0x40(%r8)         # Save r8 (overwrites pointer, but we save it)
    movq    %r9,  0x48(%r8)         # Save r9 (overwrites pointer, but we save it)
    movq    %r10, 0x50(%r8)         # Save r10
    movq    %r11, 0x58(%r8)         # Save r11
    movq    %r12, 0x60(%r8)         # Save r12
    movq    %r13, 0x68(%r8)         # Save r13
    movq    %r14, 0x70(%r8)         # Save r14
    movq    %r15, 0x78(%r8)         # Save r15

    # Save return address as instruction pointer
    # The return address is on top of stack
    movq    (%rsp), %rax            # Get return address from stack
    movq    %rax, 0x80(%r8)         # Save as rip in context

    # Save flags register
    pushfq                          # Push flags onto stack
    popq    %rax                    # Pop flags into rax
    movq    %rax, 0x88(%r8)         # Save rflags

    # Save segment registers
    movw    %cs, %ax                # Get code segment
    movq    %rax, 0x90(%r8)         # Save cs (zero-extended)
    movw    %ds, %ax                # Get data segment
    movq    %rax, 0x98(%r8)         # Save ds (zero-extended)
    movw    %es, %ax                # Get extra segment
    movq    %rax, 0xA0(%r8)         # Save es (zero-extended)
    movw    %fs, %ax                # Get fs segment
    movq    %rax, 0xA8(%r8)         # Save fs (zero-extended)
    movw    %gs, %ax                # Get fs segment
    movq    %rax, 0xB0(%r8)         # Save gs (zero-extended)
    movw    %ss, %ax                # Get stack segment
    movq    %rax, 0xB8(%r8)         # Save ss (zero-extended)

    # Save page directory (CR3)
    movq    %cr3, %rax              # Get current page directory
    movq    %rax, 0xC0(%r8)          # Save cr3

    # Move new_context pointer from r9 to rsi for loading
    movq    %r9, %rsi                # Restore new_context pointer to rsi

load_new_context:
    # Load new context from new_context pointer (in rsi)
    # Use r15 to hold context pointer throughout (it's callee-saved)
    movq    %rsi, %r15               # Save context pointer in r15

    # Extract critical values we need BEFORE loading registers
    movq    0x38(%r15), %r14         # Get new RSP -> r14
    movq    0x80(%r15), %r13         # Get RIP -> r13
    movq    0x90(%r15), %r12         # Get CS -> r12
    movq    0x88(%r15), %r11         # Get RFLAGS -> r11

    # Calculate IRET frame location on new task's stack (we'll write it after CR3 switch)
    # IRET expects: [rsp] = RIP, [rsp+8] = CS, [rsp+16] = RFLAGS
    # Formula: (rsp - 24) & ~0xF (16-byte aligned)
    
    # Load page directory first (if it's different)
    movq    0xC0(%r15), %rax         # Load new cr3
    movq    %cr3, %rdx               # Get current cr3
    cmpq    %rax, %rdx               # Compare with new cr3
    je      skip_cr3_load            # Skip if same page directory
    movq    %rax, %cr3               # Load new page directory
skip_cr3_load:
    
    # Now calculate and write IRET frame AFTER CR3 switch (using correct page tables)
    movq    %r14, %rax               # New RSP from context
    subq    $24, %rax                # Make space for IRET frame (3 words = 24 bytes)
    andq    $-16, %rax               # Align to 16 bytes
    # rax now holds the IRET frame location on the new task's stack
    # Save this location - we MUST use the exact same location, not recalculate it!
    movq    %rax, %r8                # Save IRET frame location in r8 (before loading registers)

    # Write IRET frame to new stack AFTER switching CR3 (using correct page tables)
    movq    %r13, (%rax)             # Write RIP at [rax]
    # Ensure CS selector is properly zero-extended (only lower 16 bits should be set)
    movzwq  %r12w, %rcx              # Zero-extend CS selector (16-bit to 64-bit)
    movq    %rcx, 8(%rax)            # Write CS at [rax+8] (zero-extended 16-bit value)
    movq    %r11, 16(%rax)           # Write RFLAGS at [rax+16]

    # Load segment registers BEFORE loading general registers
    movq    0x98(%r15), %rax         # Load ds
    movw    %ax, %ds                 # Set data segment
    movq    0xA0(%r15), %rax         # Load es
    movw    %ax, %es                 # Set extra segment
    movq    0xA8(%r15), %rax         # Load fs
    movw    %ax, %fs                 # Set fs segment
    movq    0xB0(%r15), %rax         # Load gs
    movw    %ax, %gs                 # Set gs segment
    # Note: cs and ss are not loaded - they remain at kernel values
    # Note: rflags is not loaded - it remains as set

    # Load ALL general purpose registers from context
    movq    0x00(%r15), %rax         # Load rax
    movq    0x08(%r15), %rbx         # Load rbx
    movq    0x10(%r15), %rcx         # Load rcx
    movq    0x18(%r15), %rdx         # Load rdx
    movq    0x20(%r15), %rsi         # Load rsi
    movq    0x28(%r15), %rdi         # Load rdi
    movq    0x30(%r15), %rbp         # Load rbp
    movq    0x38(%r15), %rsp         # Load rsp
    movq    0x40(%r15), %r8          # Load r8
    movq    0x48(%r15), %r9          # Load r9
    movq    0x50(%r15), %r10         # Load r10
    movq    0x58(%r15), %r11         # Load r11
    movq    0x60(%r15), %r12         # Load r12
    movq    0x68(%r15), %r13         # Load r13
    movq    0x70(%r15), %r14         # Load r14
    # Don't load r15 yet - we need it to get RIP and RFLAGS

    # Get RIP and RFLAGS before loading r15
    movq    0x80(%r15), %r13         # Load RIP into r13 (overwrites task's r13)
    movq    0x88(%r15), %r11         # Load RFLAGS into r11 temporarily

    # Restore RFLAGS
    pushq   %r11                    # Push RFLAGS onto stack
    popfq                           # Pop into RFLAGS register

    # Now load r15
    movq    0x78(%r15), %r15         # Load r15 (overwrites context pointer)

    # Jump to new instruction pointer
    jmpq    *%r13                    # Jump to new rip (stored in r13)

#
# context_switch_user(void *old_context, void *new_context)
# Save current kernel context (if provided) and enter user mode with IRET.
# Expects new_context->cs/ss to be user selectors.
#
# PRIVILEGE DEMOTION (Ring 0 → Ring 3):
# This function transitions from kernel mode to user mode by:
#  1. Saving the current kernel context (if switching from another task)
#  2. Loading user segment selectors into DS, ES, FS, GS (but not CS/SS yet)
#  3. Building an IRET frame on the kernel stack with user CS/SS/RFLAGS/RSP/RIP
#  4. Switching to the user task's page tables (CR3)
#  5. Loading all general-purpose registers from the user context
#  6. Executing IRETQ to atomically:
#     - Pop CS (0x23, Ring 3), RIP, RFLAGS, SS (0x1B, Ring 3), RSP from the stack
#     - Set CPL (Current Privilege Level) to CS.RPL (Ring 3)
#     - Switch to the user stack
#     - Resume execution at user RIP with Ring 3 privileges
#
# The CPU automatically validates that:
#  - CS.DPL == CS.RPL == 3 (code segment must be Ring 3)
#  - SS.DPL == SS.RPL == 3 (stack segment must be Ring 3)
#  - The target privilege level is less privileged than the current one (0 < 3)
#
# If any validation fails, the CPU triggers a General Protection Fault (#GP).
#
# Return to kernel mode happens through:
#  - Syscalls (int 0x80, see drivers/syscall.c)
#  - Hardware interrupts (timer, keyboard)
#  - Exceptions (page faults, invalid opcodes, etc.)
# All of these automatically elevate to Ring 0 using TSS.RSP0.
#
.global context_switch_user
context_switch_user:
    movq    %rdi, %r8               # old_context pointer
    movq    %rsi, %rbx              # new_context pointer (keep in rbx until all loads done)

    /* Save old context if present (same layout as context_switch) */
    test    %r8, %r8
    jz      csu_after_save

    movq    %rax, 0x00(%r8)
    movq    %rbx, 0x08(%r8)
    movq    %rcx, 0x10(%r8)
    movq    %rdx, 0x18(%r8)
    movq    %rsi, 0x20(%r8)
    movq    %rdi, 0x28(%r8)
    movq    %rbp, 0x30(%r8)
    movq    %rsp, 0x38(%r8)
    movq    %r8,  0x40(%r8)
    movq    %r9,  0x48(%r8)
    movq    %r10, 0x50(%r8)
    movq    %r11, 0x58(%r8)
    movq    %r12, 0x60(%r8)
    movq    %r13, 0x68(%r8)
    movq    %r14, 0x70(%r8)
    movq    %r15, 0x78(%r8)

    movq    (%rsp), %rax
    movq    %rax, 0x80(%r8)

    pushfq
    popq    %rax
    movq    %rax, 0x88(%r8)

    movw    %cs, %ax
    movq    %rax, 0x90(%r8)
    movw    %ds, %ax
    movq    %rax, 0x98(%r8)
    movw    %es, %ax
    movq    %rax, 0xA0(%r8)
    movw    %fs, %ax
    movq    %rax, 0xA8(%r8)
    movw    %gs, %ax
    movq    %rax, 0xB0(%r8)
    movw    %ss, %ax
    movq    %rax, 0xB8(%r8)

    movq    %cr3, %rax
    movq    %rax, 0xC0(%r8)

csu_after_save:
    /* Load data segments for user mode */
    movq    0x98(%rbx), %rax        # ds
    movw    %ax, %ds
    movq    0xA0(%rbx), %rax        # es
    movw    %ax, %es
    movq    0xA8(%rbx), %rax        # fs
    movw    %ax, %fs
    movq    0xB0(%rbx), %rax        # gs
    movw    %ax, %gs

    /* Build IRET frame on current (kernel) stack */
    movq    0xB8(%rbx), %rax        # ss
    pushq   %rax
    movq    0x38(%rbx), %rax        # user rsp
    pushq   %rax
    movq    0x88(%rbx), %rax        # rflags
    pushq   %rax
    movq    0x90(%rbx), %rax        # cs
    pushq   %rax
    movq    0x80(%rbx), %rax        # rip
    pushq   %rax

    /* Switch CR3 to target user page tables while context pointer is still valid */
    movq    0xC0(%rbx), %rax
    movq    %cr3, %rdx
    cmpq    %rax, %rdx
    je      csu_skip_cr3
    movq    %rax, %cr3
csu_skip_cr3:

    /* Load general-purpose registers from new context */
    movq    0x00(%rbx), %rax
    movq    0x10(%rbx), %rcx
    movq    0x18(%rbx), %rdx
    movq    0x20(%rbx), %rsi
    movq    0x30(%rbx), %rbp
    movq    0x40(%rbx), %r8
    movq    0x48(%rbx), %r9
    movq    0x50(%rbx), %r10
    movq    0x58(%rbx), %r11
    movq    0x60(%rbx), %r12
    movq    0x68(%rbx), %r13
    movq    0x70(%rbx), %r14
    movq    0x78(%rbx), %r15
    movq    0x28(%rbx), %rdi        # load user rdi near end
    movq    0x08(%rbx), %rbx        # load user rbx last, pointer no longer needed

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
