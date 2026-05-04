# SlopOS boot-side asm trampolines.
#
# The SYSCALL fast path lives in
# `slopos-ostd/src/user/asm/user_return.s` (`__ostd_user_return`),
# which the boot-side LSTAR setup in `boot/src/gdt.rs::syscall_msr_init`
# points at.  This file keeps only the entry point that other kernel
# asm depends on:
#
#   - ret_from_fork   — historical IRETQ-based task entry trampoline.
#                       Currently unreferenced: kernel-mode tasks enter
#                       via `task_entry_trampoline` in
#                       `core/src/scheduler/switch_asm.rs`, and user-
#                       mode tasks enter via `user_task_first_run` in
#                       `core/src/syscall/user_loop.rs`.  Kept until
#                       the broader scheduler/task migration removes
#                       the legacy `TaskContext`-based InterruptFrame
#                       layout this stub consumes.
#
# AT&T syntax mode is required for the % register prefix and $ immediate
# prefix used throughout. The `bad_asm_style` lint is silenced via the
# `#![allow(bad_asm_style)]` attribute at the top of boot/src/idt.rs.

.att_syntax prefix
.section .text

# Segment selector constants (kept locally because ret_from_fork builds
# IRET frames using them).
.equ SEL_KERNEL_DATA, 0x10    # Kernel data segment (GDT index 2, RPL 0)
.equ SEL_USER_DATA,   0x1B    # User data segment   (GDT index 3, RPL 3)
.equ SEL_USER_CODE,   0x23    # User code segment   (GDT index 4, RPL 3)

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
