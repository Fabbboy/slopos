# OSTD-side IDT entry-point asm stubs.
#
# These stubs are referenced by the IDT after `IdtBuilder::install_default_handlers`
# wires them up. The shape mirrors the legacy boot-side asm: each entry pushes
# (vector, error_code) plus the GP register set, calls a kernel-supplied
# `common_exception_handler(*mut InterruptFrame)`, then unwinds.
#
# Two external Rust symbols are referenced via `.extern`:
#   - common_exception_handler  — kernel-side dispatcher (see boot/src/ffi_boundary.rs)
#   - isr_iret_frame_corrupt    — kernel-side panic on bad IRET (same source)
# Both resolve cross-crate at link time, the same way the legacy boot-side asm
# already calls them.
#
# AT&T syntax mode is selected by the options(att_syntax) flag on the
# global_asm! invocation that includes this file. Do not add a syntax
# directive here — the bad_asm_style lint matches on the literal text.

.section .text

# Segment selector constants. Must match SegmentSelector::KERNEL_DATA in
# slopos-ostd/src/arch/x86_64/gdt.rs.
.equ SEL_KERNEL_DATA, 0x10

# External Rust functions.
.extern common_exception_handler
.extern isr_iret_frame_corrupt

# -----------------------------------------------------------------------------
# Generic interrupt handler macro.
#
# Pushes (error_code if absent, vector), conditional swapgs on user entry,
# pushes GP registers in the order matching slopos_ostd::irq::InterruptFrame,
# loads kernel data segments, calls common_exception_handler with RDI = frame
# pointer, then unwinds and IRETQs (with conditional swapgs back to user GS).
# -----------------------------------------------------------------------------
.macro INTERRUPT_HANDLER vector, has_error_code
    .if \has_error_code == 0
        pushq $0
    .endif

    pushq $\vector

    testb $3, 24(%rsp)
    jz 1f
    swapgs
1:

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

    # Set up kernel data segments (excluding GS and FS).
    # GS is managed by SWAPGS for per-CPU access.
    # FS holds user TLS base via FS_BASE MSR — must not be clobbered.
    movw $SEL_KERNEL_DATA, %ax
    movw %ax, %ds
    movw %ax, %es

    movq %rsp, %rdi
    call common_exception_handler

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

    # Check if returning to user mode - if so, swap GS back.
    # CS is at offset 8 from RSP (after dropping vector+error: [RIP] [CS] ...).
    testb $3, 8(%rsp)
    jz 2f
    swapgs
2:
    iretq
.endm

# -----------------------------------------------------------------------------
# Exception handlers (vectors 0-19, except reserved 9 and 15).
# -----------------------------------------------------------------------------
.global isr0
isr0:  INTERRUPT_HANDLER 0,  0    # Divide Error
.global isr1
isr1:  INTERRUPT_HANDLER 1,  0    # Debug
.global isr2
isr2:  INTERRUPT_HANDLER 2,  0    # NMI
.global isr3
isr3:  INTERRUPT_HANDLER 3,  0    # Breakpoint
.global isr4
isr4:  INTERRUPT_HANDLER 4,  0    # Overflow
.global isr5
isr5:  INTERRUPT_HANDLER 5,  0    # Bound Range
.global isr6
isr6:  INTERRUPT_HANDLER 6,  0    # Invalid Opcode
.global isr7
isr7:  INTERRUPT_HANDLER 7,  0    # Device Not Available
.global isr8
isr8:  INTERRUPT_HANDLER 8,  1    # Double Fault (error code)
.global isr10
isr10: INTERRUPT_HANDLER 10, 1    # Invalid TSS (error code)
.global isr11
isr11: INTERRUPT_HANDLER 11, 1    # Segment Not Present (error code)
.global isr12
isr12: INTERRUPT_HANDLER 12, 1    # Stack Fault (error code)
.global isr13
isr13: INTERRUPT_HANDLER 13, 1    # General Protection (error code)
.global isr14
isr14: INTERRUPT_HANDLER 14, 1    # Page Fault (error code)
.global isr16
isr16: INTERRUPT_HANDLER 16, 0    # FPU Error
.global isr17
isr17: INTERRUPT_HANDLER 17, 1    # Alignment Check (error code)
.global isr18
isr18: INTERRUPT_HANDLER 18, 0    # Machine Check
.global isr19
isr19: INTERRUPT_HANDLER 19, 0    # SIMD FP Exception

# -----------------------------------------------------------------------------
# Syscall trap-gate stub (vector 0x80). Vestigial: the SYSCALL fast path
# (LSTAR) is the production entry, but the IDT slot must still be populated
# in case userland issues `int 0x80` directly.
# -----------------------------------------------------------------------------
.global isr128
isr128: INTERRUPT_HANDLER 128, 0

# -----------------------------------------------------------------------------
# Legacy IRQ handlers (vectors 32-47, IOAPIC RTE 0-15).
# -----------------------------------------------------------------------------
.global irq0
irq0:  INTERRUPT_HANDLER 32, 0    # Timer
.global irq1
irq1:  INTERRUPT_HANDLER 33, 0    # Keyboard
.global irq2
irq2:  INTERRUPT_HANDLER 34, 0    # Cascade
.global irq3
irq3:  INTERRUPT_HANDLER 35, 0    # COM2
.global irq4
irq4:  INTERRUPT_HANDLER 36, 0    # COM1
.global irq5
irq5:  INTERRUPT_HANDLER 37, 0    # LPT2
.global irq6
irq6:  INTERRUPT_HANDLER 38, 0    # Floppy
.global irq7
irq7:  INTERRUPT_HANDLER 39, 0    # LPT1
.global irq8
irq8:  INTERRUPT_HANDLER 40, 0    # RTC
.global irq9
irq9:  INTERRUPT_HANDLER 41, 0
.global irq10
irq10: INTERRUPT_HANDLER 42, 0
.global irq11
irq11: INTERRUPT_HANDLER 43, 0
.global irq12
irq12: INTERRUPT_HANDLER 44, 0    # Mouse
.global irq13
irq13: INTERRUPT_HANDLER 45, 0    # FPU
.global irq14
irq14: INTERRUPT_HANDLER 46, 0    # ATA Primary
.global irq15
irq15: INTERRUPT_HANDLER 47, 0    # ATA Secondary

# -----------------------------------------------------------------------------
# Reschedule IPI handler (vector 0xFC = 252).
# Custom shape: identical to INTERRUPT_HANDLER except the post-handler
# unwind validates CS before IRETQ. Same pre-IRET CS check pattern as
# the LAPIC timer; both diverted to isr_iret_frame_corrupt on bad CS.
# -----------------------------------------------------------------------------
.global isr_reschedule_ipi
isr_reschedule_ipi:
    pushq $0
    pushq $252

    testb $3, 24(%rsp)
    jz .Lresched_noswap_entry
    swapgs
.Lresched_noswap_entry:

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

    movw $SEL_KERNEL_DATA, %ax
    movw %ax, %ds
    movw %ax, %es

    movq %rsp, %rdi
    call common_exception_handler

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

    pushq %rax
    movq  16(%rsp), %rax
    cmpq  $0x08, %rax
    je    .Lresched_cs_ok
    cmpq  $0x23, %rax
    je    .Lresched_cs_ok
    popq  %rax
    movq  %rsp, %rdi
    jmp   isr_iret_frame_corrupt
.Lresched_cs_ok:
    popq %rax

    testb $3, 8(%rsp)
    jz .Lresched_noswap_exit
    swapgs
.Lresched_noswap_exit:
    iretq

# -----------------------------------------------------------------------------
# LAPIC Timer handler (vector 0xEC = 236).
#
# Same custom unwind as the reschedule IPI: pre-IRETQ CS validation that
# diverts to isr_iret_frame_corrupt on bad CS rather than taking #GP from
# a corrupted IRETQ.
# -----------------------------------------------------------------------------
.global isr_lapic_timer
isr_lapic_timer:
    pushq $0
    pushq $236

    testb $3, 24(%rsp)
    jz .Ltimer_noswap_entry
    swapgs
.Ltimer_noswap_entry:

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

    movw $SEL_KERNEL_DATA, %ax
    movw %ax, %ds
    movw %ax, %es

    movq %rsp, %rdi
    call common_exception_handler

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

    # ---- IRET frame validation ----
    # [RSP+0]=RIP  [RSP+8]=CS  [RSP+16]=RFLAGS  [RSP+24]=RSP  [RSP+32]=SS
    pushq %rax
    movq  16(%rsp), %rax
    cmpq  $0x08, %rax
    je    .Ltimer_cs_ok
    cmpq  $0x23, %rax
    je    .Ltimer_cs_ok
    popq  %rax
    movq  %rsp, %rdi
    jmp   isr_iret_frame_corrupt
.Ltimer_cs_ok:
    popq %rax

    testb $3, 8(%rsp)
    jz .Ltimer_noswap_exit
    swapgs
.Ltimer_noswap_exit:
    iretq

# -----------------------------------------------------------------------------
# Other IPI handlers (generic INTERRUPT_HANDLER shape).
# -----------------------------------------------------------------------------
.global isr_tlb_shootdown
isr_tlb_shootdown:  INTERRUPT_HANDLER 253, 0   # 0xFD
.global isr_rcu_qs_ipi
isr_rcu_qs_ipi:     INTERRUPT_HANDLER 251, 0   # 0xFB
.global isr_shutdown_ipi
isr_shutdown_ipi:   INTERRUPT_HANDLER 254, 0   # 0xFE
.global isr_spurious
isr_spurious:       INTERRUPT_HANDLER 255, 0   # 0xFF

# =============================================================================
# MSI Interrupt Stubs (vectors 48-223)
# =============================================================================
#
# Generated programmatically using .altmacro + .rept. Each stub uses the
# generic INTERRUPT_HANDLER macro — the vector number is embedded in the
# pushed frame so the dispatcher can route to the registered MSI handler.
#
# msi_vector_table[i] = address of stub for vector (48 + i). Consumed by
# IdtBuilder::install_default_handlers to populate the IDT.

.altmacro

.macro MAKE_MSI_STUB vec
    .global msi_vector_\vec
    msi_vector_\vec:
        INTERRUPT_HANDLER \vec, 0
.endm

.macro EMIT_MSI_TABLE_ENTRY vec
    .quad msi_vector_\vec
.endm

.set _msi_vec, 48
.rept 176
    MAKE_MSI_STUB %_msi_vec
    .set _msi_vec, _msi_vec + 1
.endr

.section .rodata
.align 8
.global msi_vector_table
msi_vector_table:
.set _msi_vec, 48
.rept 176
    EMIT_MSI_TABLE_ENTRY %_msi_vec
    .set _msi_vec, _msi_vec + 1
.endr

.noaltmacro

.section .text
