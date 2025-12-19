# SlopOS Interrupt Descriptor Table (IDT) Assembly Handlers
# x86_64 interrupt and exception handlers
#
# This file defines low-level assembly interrupt handlers that save
# CPU state and call high-level C handlers

.att_syntax prefix
.section .text

# External C function
.extern common_exception_handler

# Common interrupt handler macro
# Saves all registers and calls C handler
.macro INTERRUPT_HANDLER vector, has_error_code
    .if \has_error_code == 0
        # Push dummy error code if none provided by CPU
        pushq $0
    .endif

    # Push vector number
    pushq $\vector

    # Save all general-purpose registers
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

    # Set up kernel data segments
    movw $0x10, %ax    # Kernel data segment
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs

    # Call C handler with interrupt frame pointer
    movq %rsp, %rdi    # First argument: pointer to interrupt frame
    call common_exception_handler

    # Restore all general-purpose registers
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

    # Remove vector number and error code from stack
    addq $16, %rsp

    # Return from interrupt
    iretq
.endm

# Exception handlers (vectors 0-19)
.global isr0
isr0:
    INTERRUPT_HANDLER 0, 0    # Divide Error

.global isr1
isr1:
    INTERRUPT_HANDLER 1, 0    # Debug

.global isr2
isr2:
    INTERRUPT_HANDLER 2, 0    # NMI

.global isr3
isr3:
    INTERRUPT_HANDLER 3, 0    # Breakpoint

.global isr4
isr4:
    INTERRUPT_HANDLER 4, 0    # Overflow

.global isr5
isr5:
    INTERRUPT_HANDLER 5, 0    # Bound Range

.global isr6
isr6:
    INTERRUPT_HANDLER 6, 0    # Invalid Opcode

.global isr7
isr7:
    INTERRUPT_HANDLER 7, 0    # Device Not Available

.global isr8
isr8:
    INTERRUPT_HANDLER 8, 1    # Double Fault (has error code)

# ISR 9 is reserved

.global isr10
isr10:
    INTERRUPT_HANDLER 10, 1   # Invalid TSS (has error code)

.global isr11
isr11:
    INTERRUPT_HANDLER 11, 1   # Segment Not Present (has error code)

.global isr12
isr12:
    INTERRUPT_HANDLER 12, 1   # Stack Fault (has error code)

.global isr13
isr13:
    INTERRUPT_HANDLER 13, 1   # General Protection (has error code)

.global isr14
isr14:
    INTERRUPT_HANDLER 14, 1   # Page Fault (has error code)

# ISR 15 is reserved

.global isr16
isr16:
    INTERRUPT_HANDLER 16, 0   # FPU Error

.global isr17
isr17:
    INTERRUPT_HANDLER 17, 1   # Alignment Check (has error code)

.global isr18
isr18:
    INTERRUPT_HANDLER 18, 0   # Machine Check

.global isr19
isr19:
    INTERRUPT_HANDLER 19, 0   # SIMD FP Exception

# Syscall entry (int 0x80) - user accessible
.global isr128
isr128:
    INTERRUPT_HANDLER 128, 0

# IRQ handlers (vectors 32-47)
# These will be used after PIC is set up

.global irq0
irq0:
    INTERRUPT_HANDLER 32, 0   # Timer

.global irq1
irq1:
    INTERRUPT_HANDLER 33, 0   # Keyboard

.global irq2
irq2:
    INTERRUPT_HANDLER 34, 0   # Cascade

.global irq3
irq3:
    INTERRUPT_HANDLER 35, 0   # COM2

.global irq4
irq4:
    INTERRUPT_HANDLER 36, 0   # COM1

.global irq5
irq5:
    INTERRUPT_HANDLER 37, 0   # LPT2

.global irq6
irq6:
    INTERRUPT_HANDLER 38, 0   # Floppy

.global irq7
irq7:
    INTERRUPT_HANDLER 39, 0   # LPT1

.global irq8
irq8:
    INTERRUPT_HANDLER 40, 0   # RTC

.global irq9
irq9:
    INTERRUPT_HANDLER 41, 0   # Free

.global irq10
irq10:
    INTERRUPT_HANDLER 42, 0   # Free

.global irq11
irq11:
    INTERRUPT_HANDLER 43, 0   # Free

.global irq12
irq12:
    INTERRUPT_HANDLER 44, 0   # Mouse

.global irq13
irq13:
    INTERRUPT_HANDLER 45, 0   # FPU

.global irq14
irq14:
    INTERRUPT_HANDLER 46, 0   # ATA Primary

.global irq15
irq15:
    INTERRUPT_HANDLER 47, 0   # ATA Secondary