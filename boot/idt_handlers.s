# SlopOS boot-side asm trampolines.
#
# Pared down from the full per-vector ISR set: those stubs now live in
# `slopos-ostd/src/irq/asm/handlers.s` and are wired into the IDT via
# `IdtBuilder::install_default_handlers`. The boot crate keeps only the
# entry points that other kernel asm depends on:
#
#   - syscall_entry   — LSTAR target for the SYSCALL fast path
#   - ret_from_fork   — destination for newly-dispatched tasks created
#                       via task_create / task_fork; jumped to by the
#                       context switch in `core/context_switch.s`
#
# AT&T syntax mode is required for the % register prefix and $ immediate
# prefix used throughout. The `bad_asm_style` lint is silenced via the
# `#![allow(bad_asm_style)]` attribute at the top of boot/src/idt.rs.

.att_syntax prefix
.section .text

# Segment selector constants (kept locally because syscall_entry and
# ret_from_fork build user IRET frames using them).
.equ SEL_KERNEL_DATA, 0x10    # Kernel data segment (GDT index 2, RPL 0)
.equ SEL_USER_DATA,   0x1B    # User data segment   (GDT index 3, RPL 3)
.equ SEL_USER_CODE,   0x23    # User code segment   (GDT index 4, RPL 3)

# Kernel-side dispatch helper called from syscall_entry.
.extern common_exception_handler

# -----------------------------------------------------------------------------
# Entry point for newly-created tasks dispatched via switch_registers.
#
# The kernel stack already holds a synthetic InterruptFrame pushed by
# task_create / task_fork; this stub pops it and IRETQs to user mode
# (or kernel mode for kthreads). Conditional swapgs based on the return CS.
# -----------------------------------------------------------------------------
.global ret_from_fork
ret_from_fork:
    movw $SEL_KERNEL_DATA, %ax
    movw %ax, %ds
    movw %ax, %es

    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax

    addq $16, %rsp

    testb $3, 8(%rsp)
    jz .Lret_from_fork_noswap
    swapgs
.Lret_from_fork_noswap:
    iretq

# =============================================================================
# SYSCALL Entry Point (modern fast syscall via SYSCALL instruction)
# =============================================================================
#
# On SYSCALL entry, the CPU performs:
#   - RCX = return RIP (next instruction after SYSCALL)
#   - R11 = RFLAGS
#   - CS = STAR[47:32] & 0xFFFC (kernel code segment)
#   - SS = STAR[47:32] + 8       (kernel data segment)
#   - RIP = LSTAR (this handler)
#   - RFLAGS &= ~SFMASK         (typically clears IF, TF, DF)
#
# Register convention (Linux/SlopOS compatible):
#   - RAX = syscall number
#   - RDI = arg0, RSI = arg1, RDX = arg2, R10 = arg3, R8 = arg4, R9 = arg5
#   - RAX = return value
#
# RCX and R11 are clobbered by SYSCALL/SYSRET, so userspace must save
# them if needed; R10 is used instead of RCX for arg3.
.global syscall_entry
syscall_entry:
    swapgs

    movq %rsp, %gs:8
    movq %gs:16, %rsp

    pushq $SEL_USER_DATA
    pushq %gs:8
    pushq %r11
    pushq $SEL_USER_CODE
    pushq %rcx
    pushq $0
    pushq $128

    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rsi
    pushq %rdi
    pushq %rbp
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15

    # Set up kernel data segments for syscall context (excluding GS and FS).
    # GS is managed by SWAPGS for per-CPU access.
    # FS holds user TLS base via FS_BASE MSR — must not be clobbered.
    movw $SEL_KERNEL_DATA, %ax
    movw %ax, %ds
    movw %ax, %es

    sti

    movq %rsp, %rdi
    call common_exception_handler

    cli

    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax

    addq $16, %rsp
    popq %rcx
    addq $8, %rsp
    popq %r11
    orq $0x200, %r11

    # SYSRET safety: validate RCX (user RIP) is canonical user address.
    # User addresses must be < 0x0000_8000_0000_0000 (lower half).
    # Use stack to preserve RAX (syscall return value).
    pushq %rax
    movq %rcx, %rax
    shrq $47, %rax                  # If user addr, bits 63:47 are all 0
    jnz .sysret_unsafe              # Non-zero → non-canonical or kernel addr
    popq %rax

    movq (%rsp), %rsp

    swapgs

    sysretq

.sysret_unsafe:
    # RCX contains non-canonical or kernel address — fall back to IRETQ.
    # This is safer than SYSRET which can #GP in ring 0.
    popq %rax                       # Restore RAX (return value)

    # Build IRET frame on kernel stack.
    # Current: RCX=RIP, R11=RFLAGS, gs:8=user RSP, gs:16=kernel stack
    movq %gs:16, %rsp               # Switch to kernel stack

    pushq $SEL_USER_DATA            # SS
    pushq %gs:8                     # RSP (user RSP)
    pushq %r11                      # RFLAGS
    pushq $SEL_USER_CODE            # CS
    pushq %rcx                      # RIP

    swapgs

    iretq
