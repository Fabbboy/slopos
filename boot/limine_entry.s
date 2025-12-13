# SlopOS Limine Entry Point
# Limine bootloader jumps directly to 64-bit mode with paging enabled
# No 32-bit entry code needed - Limine handles the transition

.code64
.intel_syntax noprefix
.section .text
.global _start

_start:
    # Limine provides 64-bit long mode with paging enabled
    # Set up our own kernel stack for safety
    cli

    # Load kernel stack pointer (use absolute address in higher half)
    lea rax, [rip + kernel_stack_top]
    mov rsp, rax

    # Ensure 16-byte stack alignment (required by System V ABI)
    and rsp, -16

    # Clear direction flag for string operations
    cld

    # Zero out base pointer for clean stack traces
    xor rbp, rbp

    # Initialize COM1 properly and then emit markers
    call early_serial_init
    mov dx, 0x3F8          # COM1 port
    mov al, 0x4C           # 'L' after init
    out dx, al
    mov dx, 0x3F8          # COM1 port
    mov al, 0x53           # 'S'
    out dx, al

    # Enable SSE/FXSR so Rust-generated memcpy instructions don't #UD
    mov rax, cr0
    or rax, 1 << 1          # CR0.MP
    and rax, ~(1 << 2)      # clear CR0.EM
    mov cr0, rax

    mov rax, cr4
    or rax, (1 << 9) | (1 << 10)   # CR4.OSFXSR | CR4.OSXMMEXCPT
    mov cr4, rax
    fninit

    # Zero out registers for clean state
    xor rax, rax
    xor rbx, rbx
    xor rcx, rcx
    xor rdx, rdx
    xor rsi, rsi
    xor rdi, rdi
    xor r8, r8
    xor r9, r9
    xor r10, r10
    xor r11, r11
    xor r12, r12
    xor r13, r13
    xor r14, r14
    xor r15, r15

    # Call kernel_main with no parameters
    call kernel_main

    # If kernel_main returns (it shouldn't), halt
    cli
.halt_loop:
    hlt
    jmp .halt_loop

# Minimal serial port initialization
# Initializes COM1 for 115200 baud, 8N1
early_serial_init:
    push rax
    push rdx

    # Disable interrupts on COM1
    mov dx, 0x3F9          # COM1 + 1 (IER)
    xor al, al
    out dx, al

    # Enable DLAB (Divisor Latch Access Bit)
    mov dx, 0x3FB          # COM1 + 3 (LCR)
    mov al, 0x80
    out dx, al

    # Set divisor to 1 (115200 baud)
    mov dx, 0x3F8          # COM1 + 0 (DLL)
    mov al, 0x01
    out dx, al

    mov dx, 0x3F9          # COM1 + 1 (DLH)
    xor al, al
    out dx, al

    # 8 bits, no parity, one stop bit (8N1)
    mov dx, 0x3FB          # COM1 + 3 (LCR)
    mov al, 0x03
    out dx, al

    # Enable FIFO, clear TX/RX queues, 14-byte threshold
    mov dx, 0x3FA          # COM1 + 2 (FCR)
    mov al, 0xC7
    out dx, al

    # Mark data terminal ready, request to send, auxiliary output 2
    mov dx, 0x3FC          # COM1 + 4 (MCR)
    mov al, 0x0B
    out dx, al

    pop rdx
    pop rax
    ret

.size _start, . - _start

# Kernel stack in BSS section
# 64KB stack should be plenty for early boot
.section .bss
.align 16
.global kernel_stack_bottom
kernel_stack_bottom:
    .skip 65536             # 64KB stack
.global kernel_stack_top
kernel_stack_top:
