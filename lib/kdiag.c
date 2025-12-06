/*
 * Diagnostic helpers for CPU state, stack traces, and interrupt frames.
 */

#include "kdiag.h"
#include "../drivers/serial.h"
#include "../drivers/irq.h"
#include "../boot/idt.h"
#include "../lib/cpu.h"
#include "../lib/stacktrace.h"
#include "klog.h"

uint64_t kdiag_timestamp(void) {
    static uint64_t monotonic_time = 0;
    static uint64_t last_tick_count = 0;

    uint64_t tick_count = irq_get_timer_ticks();
    if (tick_count > last_tick_count) {
        monotonic_time += tick_count - last_tick_count;
        last_tick_count = tick_count;
    }

    if (tick_count != 0) {
        return monotonic_time;
    }

    uint64_t tsc = cpu_read_tsc();
    if (tsc <= monotonic_time) {
        tsc = monotonic_time + 1;
    }

    monotonic_time = tsc;
    return monotonic_time;
}

void kdiag_dump_cpu_state(void) {
    klog(KLOG_INFO, "=== CPU STATE DUMP ===");

    uint64_t rsp, rbp, rax, rbx, rcx, rdx, rsi, rdi;
    uint64_t r8, r9, r10, r11, r12, r13, r14, r15;
    uint64_t rflags, cr0, cr2, cr3, cr4;
    uint16_t cs, ds, es, fs, gs, ss;

    __asm__ volatile ("movq %%rsp, %0" : "=r" (rsp));
    rbp = cpu_read_rbp();
    __asm__ volatile ("movq %%rax, %0" : "=r" (rax));
    __asm__ volatile ("movq %%rbx, %0" : "=r" (rbx));
    __asm__ volatile ("movq %%rcx, %0" : "=r" (rcx));
    __asm__ volatile ("movq %%rdx, %0" : "=r" (rdx));
    __asm__ volatile ("movq %%rsi, %0" : "=r" (rsi));
    __asm__ volatile ("movq %%rdi, %0" : "=r" (rdi));
    __asm__ volatile ("movq %%r8, %0" : "=r" (r8));
    __asm__ volatile ("movq %%r9, %0" : "=r" (r9));
    __asm__ volatile ("movq %%r10, %0" : "=r" (r10));
    __asm__ volatile ("movq %%r11, %0" : "=r" (r11));
    __asm__ volatile ("movq %%r12, %0" : "=r" (r12));
    __asm__ volatile ("movq %%r13, %0" : "=r" (r13));
    __asm__ volatile ("movq %%r14, %0" : "=r" (r14));
    __asm__ volatile ("movq %%r15, %0" : "=r" (r15));

    __asm__ volatile ("pushfq; popq %0" : "=r" (rflags));

    __asm__ volatile ("movw %%cs, %0" : "=r" (cs));
    __asm__ volatile ("movw %%ds, %0" : "=r" (ds));
    __asm__ volatile ("movw %%es, %0" : "=r" (es));
    __asm__ volatile ("movw %%fs, %0" : "=r" (fs));
    __asm__ volatile ("movw %%gs, %0" : "=r" (gs));
    __asm__ volatile ("movw %%ss, %0" : "=r" (ss));

    __asm__ volatile ("movq %%cr0, %0" : "=r" (cr0));
    __asm__ volatile ("movq %%cr2, %0" : "=r" (cr2));
    __asm__ volatile ("movq %%cr3, %0" : "=r" (cr3));
    __asm__ volatile ("movq %%cr4, %0" : "=r" (cr4));

    klog(KLOG_INFO, "General Purpose Registers:");
    klog_raw(KLOG_INFO, "  RAX: "); klog_hex(KLOG_INFO, rax);
    klog_raw(KLOG_INFO, "  RBX: "); klog_hex(KLOG_INFO, rbx);
    klog_raw(KLOG_INFO, "  RCX: "); klog_hex(KLOG_INFO, rcx);
    klog_raw(KLOG_INFO, "  RDX: "); klog_hex(KLOG_INFO, rdx);
    klog(KLOG_INFO, "");

    klog_raw(KLOG_INFO, "  RSI: "); klog_hex(KLOG_INFO, rsi);
    klog_raw(KLOG_INFO, "  RDI: "); klog_hex(KLOG_INFO, rdi);
    klog_raw(KLOG_INFO, "  RBP: "); klog_hex(KLOG_INFO, rbp);
    klog_raw(KLOG_INFO, "  RSP: "); klog_hex(KLOG_INFO, rsp);
    klog(KLOG_INFO, "");

    klog_raw(KLOG_INFO, "  R8:  "); klog_hex(KLOG_INFO, r8);
    klog_raw(KLOG_INFO, "  R9:  "); klog_hex(KLOG_INFO, r9);
    klog_raw(KLOG_INFO, "  R10: "); klog_hex(KLOG_INFO, r10);
    klog_raw(KLOG_INFO, "  R11: "); klog_hex(KLOG_INFO, r11);
    klog(KLOG_INFO, "");

    klog_raw(KLOG_INFO, "  R12: "); klog_hex(KLOG_INFO, r12);
    klog_raw(KLOG_INFO, "  R13: "); klog_hex(KLOG_INFO, r13);
    klog_raw(KLOG_INFO, "  R14: "); klog_hex(KLOG_INFO, r14);
    klog_raw(KLOG_INFO, "  R15: "); klog_hex(KLOG_INFO, r15);
    klog(KLOG_INFO, "");

    klog(KLOG_INFO, "Flags Register:");
    klog_raw(KLOG_INFO, "  RFLAGS: "); klog_hex(KLOG_INFO, rflags); klog_raw(KLOG_INFO, " [");
    if (rflags & (1 << 0)) klog_raw(KLOG_INFO, "CF ");
    if (rflags & (1 << 2)) klog_raw(KLOG_INFO, "PF ");
    if (rflags & (1 << 4)) klog_raw(KLOG_INFO, "AF ");
    if (rflags & (1 << 6)) klog_raw(KLOG_INFO, "ZF ");
    if (rflags & (1 << 7)) klog_raw(KLOG_INFO, "SF ");
    if (rflags & (1 << 8)) klog_raw(KLOG_INFO, "TF ");
    if (rflags & (1 << 9)) klog_raw(KLOG_INFO, "IF ");
    if (rflags & (1 << 10)) klog_raw(KLOG_INFO, "DF ");
    if (rflags & (1 << 11)) klog_raw(KLOG_INFO, "OF ");
    klog(KLOG_INFO, "]");

    klog(KLOG_INFO, "Segment Registers:");
    klog_raw(KLOG_INFO, "  CS: "); klog_hex(KLOG_INFO, cs);
    klog_raw(KLOG_INFO, "  DS: "); klog_hex(KLOG_INFO, ds);
    klog_raw(KLOG_INFO, "  ES: "); klog_hex(KLOG_INFO, es);
    klog_raw(KLOG_INFO, "  FS: "); klog_hex(KLOG_INFO, fs);
    klog_raw(KLOG_INFO, "  GS: "); klog_hex(KLOG_INFO, gs);
    klog_raw(KLOG_INFO, "  SS: "); klog_hex(KLOG_INFO, ss);
    klog(KLOG_INFO, "");

    klog(KLOG_INFO, "Control Registers:");
    klog_raw(KLOG_INFO, "  CR0: "); klog_hex(KLOG_INFO, cr0);
    klog_raw(KLOG_INFO, "  CR2: "); klog_hex(KLOG_INFO, cr2);
    klog(KLOG_INFO, "");
    klog_raw(KLOG_INFO, "  CR3: "); klog_hex(KLOG_INFO, cr3);
    klog_raw(KLOG_INFO, "  CR4: "); klog_hex(KLOG_INFO, cr4);
    klog(KLOG_INFO, "");

    klog(KLOG_INFO, "=== END CPU STATE DUMP ===");
}

void kdiag_dump_interrupt_frame(struct interrupt_frame *frame) {
    if (!frame) {
        return;
    }

    klog(KLOG_INFO, "=== INTERRUPT FRAME DUMP ===");

    klog_raw(KLOG_INFO, "Vector: "); klog_decimal(KLOG_INFO, frame->vector);
    klog_raw(KLOG_INFO, " ("); klog_raw(KLOG_INFO, get_exception_name((uint8_t)frame->vector)); klog_raw(KLOG_INFO, ")");
    klog_raw(KLOG_INFO, " Error Code: "); klog_hex(KLOG_INFO, frame->error_code);
    klog(KLOG_INFO, "");

    klog_raw(KLOG_INFO, "RIP: "); klog_hex(KLOG_INFO, frame->rip);
    klog_raw(KLOG_INFO, " CS: "); klog_hex(KLOG_INFO, frame->cs);
    klog_raw(KLOG_INFO, " RFLAGS: "); klog_hex(KLOG_INFO, frame->rflags);
    klog(KLOG_INFO, "");

    klog_raw(KLOG_INFO, "RSP: "); klog_hex(KLOG_INFO, frame->rsp);
    klog_raw(KLOG_INFO, " SS: "); klog_hex(KLOG_INFO, frame->ss);
    klog(KLOG_INFO, "");

    klog_raw(KLOG_INFO, "RAX: "); klog_hex(KLOG_INFO, frame->rax);
    klog_raw(KLOG_INFO, " RBX: "); klog_hex(KLOG_INFO, frame->rbx);
    klog_raw(KLOG_INFO, " RCX: "); klog_hex(KLOG_INFO, frame->rcx);
    klog(KLOG_INFO, "");

    klog_raw(KLOG_INFO, "RDX: "); klog_hex(KLOG_INFO, frame->rdx);
    klog_raw(KLOG_INFO, " RSI: "); klog_hex(KLOG_INFO, frame->rsi);
    klog_raw(KLOG_INFO, " RDI: "); klog_hex(KLOG_INFO, frame->rdi);
    klog(KLOG_INFO, "");

    klog_raw(KLOG_INFO, "RBP: "); klog_hex(KLOG_INFO, frame->rbp);
    klog_raw(KLOG_INFO, " R8: "); klog_hex(KLOG_INFO, frame->r8);
    klog_raw(KLOG_INFO, " R9: "); klog_hex(KLOG_INFO, frame->r9);
    klog(KLOG_INFO, "");

    klog_raw(KLOG_INFO, "R10: "); klog_hex(KLOG_INFO, frame->r10);
    klog_raw(KLOG_INFO, " R11: "); klog_hex(KLOG_INFO, frame->r11);
    klog_raw(KLOG_INFO, " R12: "); klog_hex(KLOG_INFO, frame->r12);
    klog(KLOG_INFO, "");

    klog_raw(KLOG_INFO, "R13: "); klog_hex(KLOG_INFO, frame->r13);
    klog_raw(KLOG_INFO, " R14: "); klog_hex(KLOG_INFO, frame->r14);
    klog_raw(KLOG_INFO, " R15: "); klog_hex(KLOG_INFO, frame->r15);
    klog(KLOG_INFO, "");

    klog(KLOG_INFO, "=== END INTERRUPT FRAME DUMP ===");
}

void kdiag_dump_stack_trace(void) {
    uint64_t rbp = cpu_read_rbp();
    klog(KLOG_INFO, "=== STACK TRACE ===");
    kdiag_dump_stack_trace_from_rbp(rbp);
    klog(KLOG_INFO, "=== END STACK TRACE ===");
}

void kdiag_dump_stack_trace_from_rbp(uint64_t rbp) {
    struct stacktrace_entry entries[KDIAG_STACK_TRACE_DEPTH];
    int frame_count = stacktrace_capture_from(rbp, entries, KDIAG_STACK_TRACE_DEPTH);

    if (frame_count == 0) {
        klog(KLOG_INFO, "No stack frames found");
        return;
    }

    for (int i = 0; i < frame_count; i++) {
        klog_raw(KLOG_INFO, "Frame "); klog_decimal(KLOG_INFO, i);
        klog_raw(KLOG_INFO, ": RBP="); klog_hex(KLOG_INFO, entries[i].frame_pointer);
        klog_raw(KLOG_INFO, " RIP="); klog_hex(KLOG_INFO, entries[i].return_address);
        klog(KLOG_INFO, "");
    }
}

void kdiag_dump_stack_trace_from_frame(struct interrupt_frame *frame) {
    klog(KLOG_INFO, "=== STACK TRACE FROM EXCEPTION ===");
    klog_raw(KLOG_INFO, "Exception occurred at RIP: "); klog_hex(KLOG_INFO, frame->rip);
    klog(KLOG_INFO, "");

    kdiag_dump_stack_trace_from_rbp(frame->rbp);
    klog(KLOG_INFO, "=== END STACK TRACE ===");
}

void kdiag_hexdump(const void *data, size_t length, uint64_t base_address) {
    const uint8_t *bytes = (const uint8_t *)data;
    size_t i, j;

    for (i = 0; i < length; i += 16) {
        klog_hex(KLOG_INFO, base_address + i);
        klog_raw(KLOG_INFO, ": ");

        for (j = 0; j < 16 && i + j < length; j++) {
            if (j == 8) klog_raw(KLOG_INFO, " ");
            klog_hex_byte(KLOG_INFO, bytes[i + j]);
            klog_raw(KLOG_INFO, " ");
        }

        for (; j < 16; j++) {
            if (j == 8) klog_raw(KLOG_INFO, " ");
            klog_raw(KLOG_INFO, "   ");
        }

        klog_raw(KLOG_INFO, " |");
        uint16_t port = COM1_BASE;
        for (j = 0; j < 16 && i + j < length; j++) {
            uint8_t c = bytes[i + j];
            if (c >= 32 && c <= 126) {
                serial_putc(port, c);
            } else {
                serial_putc(port, '.');
            }
        }
        klog(KLOG_INFO, "|");
    }
}

