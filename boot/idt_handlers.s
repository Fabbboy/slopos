# SlopOS Interrupt Descriptor Table (IDT) Assembly Handlers
# x86_64 interrupt and exception handlers
#
# This file defines low-level assembly interrupt handlers that save
# CPU state and call high-level C handlers

.att_syntax prefix
.section .text

# Segment selector constants
.equ SEL_KERNEL_DATA, 0x10    # Kernel data segment (GDT index 2, RPL 0)
.equ SEL_USER_DATA,   0x1B    # User data segment (GDT index 3, RPL 3)
.equ SEL_USER_CODE,   0x23    # User code segment (GDT index 4, RPL 3)

# Canonical address check - user addresses must be < 0x0000_8000_0000_0000
# (lower half of virtual address space, bit 47 = 0, bits 63:48 = 0)
.equ USER_ADDR_MAX_HIGH, 0x00007FFF  # Upper 32 bits must be <= this

# External C function
.extern common_exception_handler

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

    # Set up kernel data segments (excluding GS which is managed by SWAPGS)
    movw $SEL_KERNEL_DATA, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    # GS is NOT touched - SWAPGS manages the GS base MSRs

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

    # Check if returning to user mode - if so, swap GS back
    # CS is now at offset 8 from RSP (after removing vector+error: [RIP] [CS] ...)
    testb $3, 8(%rsp)
    jz 2f
    swapgs
2:
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

# Reschedule IPI handler (vector 0xFC = 252)
.global isr_reschedule_ipi
isr_reschedule_ipi:
    INTERRUPT_HANDLER 252, 0

# TLB Shootdown IPI handler (vector 0xFD = 253)
.global isr_tlb_shootdown
isr_tlb_shootdown:
    INTERRUPT_HANDLER 253, 0

# Shutdown IPI handler (vector 0xFE = 254)
.global isr_shutdown_ipi
isr_shutdown_ipi:
    INTERRUPT_HANDLER 254, 0

# APIC spurious interrupt handler (vector 0xFF = 255)
.global isr_spurious
isr_spurious:
    INTERRUPT_HANDLER 255, 0

# =============================================================================
# SYSCALL Entry Point (modern fast syscall via SYSCALL instruction)
# =============================================================================
#
# On SYSCALL entry, the CPU performs:
#   - RCX = return RIP (next instruction after SYSCALL)
#   - R11 = RFLAGS
#   - CS = STAR[47:32] & 0xFFFC (kernel code segment)
#   - SS = STAR[47:32] + 8 (kernel data segment)
#   - RIP = LSTAR (this handler)
#   - RFLAGS &= ~SFMASK (typically clears IF, TF, DF)
#
# Register convention (Linux/SlopOS compatible):
#   - RAX = syscall number
#   - RDI = arg0, RSI = arg1, RDX = arg2, R10 = arg3, R8 = arg4, R9 = arg5
#   - RAX = return value
#
# Note: RCX and R11 are clobbered by SYSCALL/SYSRET, so userspace must
# save them if needed. R10 is used instead of RCX for arg3.
#
.global syscall_entry
syscall_entry:
    swapgs

    movq %rsp, %gs:0
    movq %gs:8, %rsp

    pushq $SEL_USER_DATA
    pushq %gs:0
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

    # Set up kernel data segments for syscall context
    movw $SEL_KERNEL_DATA, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    # GS is NOT touched - SWAPGS manages the GS base MSRs

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

    # SYSRET safety: validate RCX (user RIP) is canonical user address
    # User addresses must be < 0x0000_8000_0000_0000 (lower half)
    # Use stack to preserve RAX (syscall return value)
    pushq %rax
    movq %rcx, %rax
    shrq $47, %rax                  # If user addr, bits 63:47 are all 0
    jnz .sysret_unsafe              # Non-zero means non-canonical or kernel addr
    popq %rax

    movq (%rsp), %rsp

    swapgs

    sysretq

.sysret_unsafe:
    # RCX contains non-canonical or kernel address - fall back to IRETQ
    # This is safer than SYSRET which can #GP in ring 0
    popq %rax                       # Restore RAX (return value)
    
    # Build IRET frame on kernel stack
    # Current: RCX=RIP, R11=RFLAGS, gs:0=user RSP, gs:8=kernel stack
    movq %gs:8, %rsp                # Switch to kernel stack
    
    pushq $SEL_USER_DATA            # SS
    pushq %gs:0                     # RSP (user RSP)
    pushq %r11                      # RFLAGS
    pushq $SEL_USER_CODE            # CS  
    pushq %rcx                      # RIP
    
    swapgs
    
    iretq
