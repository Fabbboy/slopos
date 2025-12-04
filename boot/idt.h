/*
 * SlopOS Interrupt Descriptor Table (IDT) Implementation
 * x86_64 IDT setup and exception handling
 */

#ifndef IDT_H
#define IDT_H

#include <stdint.h>

// IDT gate types
#define IDT_GATE_INTERRUPT 0x8E  // 32-bit interrupt gate
#define IDT_GATE_TRAP      0x8F  // 32-bit trap gate
#define IDT_GATE_CALL      0x8C  // 32-bit call gate

// Exception vector numbers
#define EXCEPTION_DIVIDE_ERROR       0
#define EXCEPTION_DEBUG             1
#define EXCEPTION_NMI               2
#define EXCEPTION_BREAKPOINT        3
#define EXCEPTION_OVERFLOW          4
#define EXCEPTION_BOUND_RANGE       5
#define EXCEPTION_INVALID_OPCODE    6
#define EXCEPTION_DEVICE_NOT_AVAIL  7
#define EXCEPTION_DOUBLE_FAULT      8
#define EXCEPTION_INVALID_TSS       10
#define EXCEPTION_SEGMENT_NOT_PRES  11
#define EXCEPTION_STACK_FAULT       12
#define EXCEPTION_GENERAL_PROTECTION 13
#define EXCEPTION_PAGE_FAULT        14
#define EXCEPTION_FPU_ERROR         16
#define EXCEPTION_ALIGNMENT_CHECK   17
#define EXCEPTION_MACHINE_CHECK     18
#define EXCEPTION_SIMD_FP_EXCEPTION 19

// IRQ vector numbers (remapped from PIC)
#define IRQ_BASE_VECTOR 32
#define IRQ_TIMER       (IRQ_BASE_VECTOR + 0)
#define IRQ_KEYBOARD    (IRQ_BASE_VECTOR + 1)
#define IRQ_CASCADE     (IRQ_BASE_VECTOR + 2)
#define IRQ_COM2        (IRQ_BASE_VECTOR + 3)
#define IRQ_COM1        (IRQ_BASE_VECTOR + 4)
#define IRQ_LPT2        (IRQ_BASE_VECTOR + 5)
#define IRQ_FLOPPY      (IRQ_BASE_VECTOR + 6)
#define IRQ_LPT1        (IRQ_BASE_VECTOR + 7)
#define IRQ_RTC         (IRQ_BASE_VECTOR + 8)
#define IRQ_FREE1       (IRQ_BASE_VECTOR + 9)
#define IRQ_FREE2       (IRQ_BASE_VECTOR + 10)
#define IRQ_FREE3       (IRQ_BASE_VECTOR + 11)
#define IRQ_MOUSE       (IRQ_BASE_VECTOR + 12)
#define IRQ_FPU         (IRQ_BASE_VECTOR + 13)
#define IRQ_ATA_PRIMARY (IRQ_BASE_VECTOR + 14)
#define IRQ_ATA_SECONDARY (IRQ_BASE_VECTOR + 15)

#define IDT_ENTRIES 256

enum exception_mode {
    EXCEPTION_MODE_NORMAL = 0,
    EXCEPTION_MODE_TEST = 1,
};

// IDT Entry structure (64-bit)
struct idt_entry {
    uint16_t offset_low;    // Lower 16 bits of handler address
    uint16_t selector;      // Code segment selector
    uint8_t  ist;          // Interrupt Stack Table offset (0 for now)
    uint8_t  type_attr;    // Type and attributes
    uint16_t offset_mid;    // Middle 16 bits of handler address
    uint32_t offset_high;   // Upper 32 bits of handler address
    uint32_t zero;         // Reserved, must be zero
} __attribute__((packed));

// IDT Pointer structure
struct idt_ptr {
    uint16_t limit;        // Size of IDT - 1
    uint64_t base;         // Base address of IDT
} __attribute__((packed));

// CPU register state saved during interrupts
struct interrupt_frame {
    // Pushed by our assembly handlers
    uint64_t r15, r14, r13, r12, r11, r10, r9, r8;
    uint64_t rbp, rdi, rsi, rdx, rcx, rbx, rax;

    // Pushed by CPU during interrupt
    uint64_t vector;       // Interrupt vector number
    uint64_t error_code;   // Error code (if present)
    uint64_t rip;          // Return instruction pointer
    uint64_t cs;           // Code segment
    uint64_t rflags;       // CPU flags
    uint64_t rsp;          // Stack pointer
    uint64_t ss;           // Stack segment
} __attribute__((packed));

// Exception handler function type
typedef void (*exception_handler_t)(struct interrupt_frame *frame);

// Exception routing configuration
void exception_set_mode(enum exception_mode mode);
int exception_is_critical(uint8_t vector);

// IDT management functions
void idt_init(void);
void idt_set_gate(uint8_t vector, uint64_t handler, uint16_t selector, uint8_t type);
void idt_install_exception_handler(uint8_t vector, exception_handler_t handler);
void idt_set_ist(uint8_t vector, uint8_t ist_index);
void idt_load(void);

// Exception handlers
void exception_divide_error(struct interrupt_frame *frame);
void exception_debug(struct interrupt_frame *frame);
void exception_nmi(struct interrupt_frame *frame);
void exception_breakpoint(struct interrupt_frame *frame);
void exception_overflow(struct interrupt_frame *frame);
void exception_bound_range(struct interrupt_frame *frame);
void exception_invalid_opcode(struct interrupt_frame *frame);
void exception_device_not_available(struct interrupt_frame *frame);
void exception_double_fault(struct interrupt_frame *frame);
void exception_invalid_tss(struct interrupt_frame *frame);
void exception_segment_not_present(struct interrupt_frame *frame);
void exception_stack_fault(struct interrupt_frame *frame);
void exception_general_protection(struct interrupt_frame *frame);
void exception_page_fault(struct interrupt_frame *frame);
void exception_fpu_error(struct interrupt_frame *frame);
void exception_alignment_check(struct interrupt_frame *frame);
void exception_machine_check(struct interrupt_frame *frame);
void exception_simd_fp_exception(struct interrupt_frame *frame);

// Debug utilities
void dump_interrupt_frame(struct interrupt_frame *frame);
const char *get_exception_name(uint8_t vector);

// Assembly interrupt handlers (defined in idt_handlers.s)
extern void isr0(void);   // Divide Error
extern void isr1(void);   // Debug
extern void isr2(void);   // NMI
extern void isr3(void);   // Breakpoint
extern void isr4(void);   // Overflow
extern void isr5(void);   // Bound Range
extern void isr6(void);   // Invalid Opcode
extern void isr7(void);   // Device Not Available
extern void isr8(void);   // Double Fault
extern void isr10(void);  // Invalid TSS
extern void isr11(void);  // Segment Not Present
extern void isr12(void);  // Stack Fault
extern void isr13(void);  // General Protection
extern void isr14(void);  // Page Fault
extern void isr16(void);  // FPU Error
extern void isr17(void);  // Alignment Check
extern void isr18(void);  // Machine Check
extern void isr19(void);  // SIMD FP Exception

// IRQ handlers
extern void irq0(void);   // Timer
extern void irq1(void);   // Keyboard
extern void irq2(void);   // Cascade
extern void irq3(void);   // COM2
extern void irq4(void);   // COM1
extern void irq5(void);   // LPT2
extern void irq6(void);   // Floppy
extern void irq7(void);   // LPT1
extern void irq8(void);   // RTC
extern void irq9(void);   // Free
extern void irq10(void);  // Free
extern void irq11(void);  // Free
extern void irq12(void);  // Mouse
extern void irq13(void);  // FPU
extern void irq14(void);  // ATA Primary
extern void irq15(void);  // ATA Secondary

#endif // IDT_H
