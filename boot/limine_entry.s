# Limine bootloader jumps directly to 64-bit mode with paging enabled

.code64
.intel_syntax noprefix

.equ COM1_BASE, 0x3F8
.equ COM1_IER, COM1_BASE + 1
.equ COM1_FCR, COM1_BASE + 2
.equ COM1_LCR, COM1_BASE + 3
.equ COM1_MCR, COM1_BASE + 4
.equ COM1_DLL, COM1_BASE + 0
.equ COM1_DLH, COM1_BASE + 1

.equ SERIAL_MARKER_L, 'L'
.equ SERIAL_MARKER_S, 'S'

.equ KERNEL_STACK_SIZE, 524288

# Offset of the `current_task` field inside the PCR.  Must match
# `karch::pcr::offsets::CURRENT_TASK`.  The SafeStack naked
# `__safestack_pointer_address` fn loads `gs:[CURRENT_TASK]` to
# discover the running task's `unsafe_stack_sp` slot; this trampoline
# must preseed the field to the BSP bootstrap Task stub before any
# instrumented Rust runs.
.equ PCR_OFFSET_CURRENT_TASK, 40

.equ MSR_IA32_GS_BASE, 0xC0000101

.section .text
.global _start

_start:
    cli

    lea rax, [rip + kernel_stack_top]
    mov rsp, rax

    # Ensure 16-byte stack alignment (required by System V ABI)
    and rsp, -16

    cld

    # Zero out base pointer for clean stack traces
    xor rbp, rbp

    call early_serial_init
    mov dx, COM1_BASE
    mov al, SERIAL_MARKER_L
    out dx, al
    mov dx, COM1_BASE
    mov al, SERIAL_MARKER_S
    out dx, al

    # `fninit` below is x87 and #UDs with CR0.EM set; the FPU save/restore
    # needs CR4.OSFXSR. The kernel is +soft-float — this is for user state.
    mov rax, cr0
    or rax, 1 << 1          # CR0.MP
    and rax, ~(1 << 2)      # clear CR0.EM
    mov cr0, rax

    mov rax, cr4
    or rax, (1 << 9) | (1 << 10)   # CR4.OSFXSR | CR4.OSXMMEXCPT
    mov cr4, rax
    fninit

    # SafeStack bootstrap.  Every function compiled with -Zsanitizer=safestack
    # calls __safestack_pointer_address on entry, which reads
    # `gs:[PCR_OFFSET_CURRENT_TASK]` and adds `TASK_UNSAFE_STACK_SP_OFFSET`.
    # GS_BASE, BSP_PCR.current_task and the task's unsafe_stack_sp must
    # therefore all be valid before kernel_main's own prologue runs.
    lea rax, [rip + BSP_PCR]
    mov [rax], rax                                   # BSP_PCR.self_ref = &BSP_PCR

    lea rdx, [rip + BSP_BOOTSTRAP_TASK]
    mov [rax + PCR_OFFSET_CURRENT_TASK], rdx

    # Offset derived from Rust's TASK_UNSAFE_STACK_SP_OFFSET constant,
    # exported to the linker as the symbol `BOOTSTRAP_TASK_UNSAFE_SP_OFFSET`
    # by `slopos_core::scheduler::safestack_rt`.
    lea rcx, [rip + BOOTSTRAP_UNSAFE_STACK]
    add rcx, 65536                                   # top of 64 KiB buffer
    and rcx, -16                                     # 16-byte alignment
    mov r8, [rip + BOOTSTRAP_TASK_UNSAFE_SP_OFFSET]
    mov [rdx + r8], rcx                              # BSP_BOOTSTRAP_TASK.unsafe_stack_sp = top

    # WRMSR IA32_GS_BASE = &BSP_PCR (in rax)
    mov rcx, rax
    mov rdx, rax
    shr rdx, 32
    mov eax, ecx
    mov ecx, MSR_IA32_GS_BASE
    wrmsr

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

    call kernel_main

    # If kernel_main returns (it shouldn't), halt
    cli
.halt_loop:
    hlt
    jmp .halt_loop

# Initializes COM1 for 115200 baud, 8N1
early_serial_init:
    push rax
    push rdx

    # Disable interrupts on COM1
    mov dx, COM1_IER
    xor al, al
    out dx, al

    # Enable DLAB (Divisor Latch Access Bit)
    mov dx, COM1_LCR
    mov al, 0x80
    out dx, al

    # Set divisor to 1 (115200 baud)
    mov dx, COM1_DLL
    mov al, 0x01
    out dx, al

    mov dx, COM1_DLH
    xor al, al
    out dx, al

    # 8 bits, no parity, one stop bit (8N1)
    mov dx, COM1_LCR
    mov al, 0x03
    out dx, al

    # Enable FIFO, clear TX/RX queues, 14-byte threshold
    mov dx, COM1_FCR
    mov al, 0xC7
    out dx, al

    # Mark data terminal ready, request to send, auxiliary output 2
    mov dx, COM1_MCR
    mov al, 0x0B
    out dx, al

    pop rdx
    pop rax
    ret

.size _start, . - _start

# 512KB stack — test harness needs extra headroom in debug mode
.section .bss
.align 16
.global kernel_stack_bottom
kernel_stack_bottom:
    .skip KERNEL_STACK_SIZE             # 512KB stack
.global kernel_stack_top
kernel_stack_top:
