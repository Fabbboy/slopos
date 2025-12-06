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
    klog_printf(KLOG_INFO, "=== CPU STATE DUMP ===\n");

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

    klog_printf(KLOG_INFO,
        "General Purpose Registers:\n"
        "  RAX: 0x%lx  RBX: 0x%lx  RCX: 0x%lx  RDX: 0x%lx\n"
        "  RSI: 0x%lx  RDI: 0x%lx  RBP: 0x%lx  RSP: 0x%lx\n"
        "  R8 : 0x%lx  R9 : 0x%lx  R10: 0x%lx  R11: 0x%lx\n"
        "  R12: 0x%lx  R13: 0x%lx  R14: 0x%lx  R15: 0x%lx\n",
        rax, rbx, rcx, rdx, rsi, rdi, rbp, rsp,
        r8, r9, r10, r11, r12, r13, r14, r15);

    klog_printf(KLOG_INFO,
        "Flags Register:\n"
        "  RFLAGS: 0x%lx [CF:%d PF:%d AF:%d ZF:%d SF:%d TF:%d IF:%d DF:%d OF:%d]\n",
        rflags,
        !!(rflags & (1 << 0)), !!(rflags & (1 << 2)), !!(rflags & (1 << 4)),
        !!(rflags & (1 << 6)), !!(rflags & (1 << 7)), !!(rflags & (1 << 8)),
        !!(rflags & (1 << 9)), !!(rflags & (1 << 10)), !!(rflags & (1 << 11)));

    klog_printf(KLOG_INFO,
        "Segment Registers:\n"
        "  CS: 0x%04x  DS: 0x%04x  ES: 0x%04x  FS: 0x%04x  GS: 0x%04x  SS: 0x%04x\n",
        (unsigned)cs, (unsigned)ds, (unsigned)es, (unsigned)fs, (unsigned)gs, (unsigned)ss);

    klog_printf(KLOG_INFO,
        "Control Registers:\n"
        "  CR0: 0x%lx  CR2: 0x%lx\n"
        "  CR3: 0x%lx  CR4: 0x%lx\n",
        cr0, cr2, cr3, cr4);

    klog_printf(KLOG_INFO, "=== END CPU STATE DUMP ===\n");
}

void kdiag_dump_interrupt_frame(struct interrupt_frame *frame) {
    if (!frame) {
        return;
    }

    klog_printf(KLOG_INFO, "=== INTERRUPT FRAME DUMP ===\n");

    klog_printf(KLOG_INFO, "Vector: %u (%s) Error Code: 0x%lx\n",
                (unsigned)frame->vector, get_exception_name((uint8_t)frame->vector), frame->error_code);

    klog_printf(KLOG_INFO, "RIP: 0x%lx  CS: 0x%lx  RFLAGS: 0x%lx\n",
                frame->rip, frame->cs, frame->rflags);

    klog_printf(KLOG_INFO, "RSP: 0x%lx  SS: 0x%lx\n", frame->rsp, frame->ss);

    klog_printf(KLOG_INFO, "RAX: 0x%lx  RBX: 0x%lx  RCX: 0x%lx\n",
                frame->rax, frame->rbx, frame->rcx);

    klog_printf(KLOG_INFO, "RDX: 0x%lx  RSI: 0x%lx  RDI: 0x%lx\n",
                frame->rdx, frame->rsi, frame->rdi);

    klog_printf(KLOG_INFO, "RBP: 0x%lx  R8: 0x%lx  R9: 0x%lx\n",
                frame->rbp, frame->r8, frame->r9);

    klog_printf(KLOG_INFO, "R10: 0x%lx  R11: 0x%lx  R12: 0x%lx\n",
                frame->r10, frame->r11, frame->r12);

    klog_printf(KLOG_INFO, "R13: 0x%lx  R14: 0x%lx  R15: 0x%lx\n",
                frame->r13, frame->r14, frame->r15);

    klog_printf(KLOG_INFO, "=== END INTERRUPT FRAME DUMP ===\n");
}

void kdiag_dump_stack_trace(void) {
    uint64_t rbp = cpu_read_rbp();
    klog_printf(KLOG_INFO, "=== STACK TRACE ===\n");
    kdiag_dump_stack_trace_from_rbp(rbp);
    klog_printf(KLOG_INFO, "=== END STACK TRACE ===\n");
}

void kdiag_dump_stack_trace_from_rbp(uint64_t rbp) {
    struct stacktrace_entry entries[KDIAG_STACK_TRACE_DEPTH];
    int frame_count = stacktrace_capture_from(rbp, entries, KDIAG_STACK_TRACE_DEPTH);

    if (frame_count == 0) {
        klog_printf(KLOG_INFO, "No stack frames found\n");
        return;
    }

    for (int i = 0; i < frame_count; i++) {
        klog_printf(KLOG_INFO, "Frame %d: RBP=0x%lx RIP=0x%lx\n",
                    i, entries[i].frame_pointer, entries[i].return_address);
    }
}

void kdiag_dump_stack_trace_from_frame(struct interrupt_frame *frame) {
    klog_printf(KLOG_INFO, "=== STACK TRACE FROM EXCEPTION ===\n");
    klog_printf(KLOG_INFO, "Exception occurred at RIP: 0x%lx\n", frame->rip);

    kdiag_dump_stack_trace_from_rbp(frame->rbp);
    klog_printf(KLOG_INFO, "=== END STACK TRACE ===\n");
}

void kdiag_hexdump(const void *data, size_t length, uint64_t base_address) {
    const uint8_t *bytes = (const uint8_t *)data;
    size_t i, j;

    for (i = 0; i < length; i += 16) {
        klog_printf(KLOG_INFO, "0x%lx: ", base_address + i);

        for (j = 0; j < 16 && i + j < length; j++) {
            if (j == 8) {
                klog_printf(KLOG_INFO, " ");
            }
            klog_printf(KLOG_INFO, "%02x ", bytes[i + j]);
        }

        for (; j < 16; j++) {
            if (j == 8) {
                klog_printf(KLOG_INFO, " ");
            }
            klog_printf(KLOG_INFO, "   ");
        }

        klog_printf(KLOG_INFO, " |");
        uint16_t port = COM1_BASE;
        for (j = 0; j < 16 && i + j < length; j++) {
            uint8_t c = bytes[i + j];
            if (c >= 32 && c <= 126) {
                serial_putc(port, c);
            } else {
                serial_putc(port, '.');
            }
        }
        klog_printf(KLOG_INFO, "|\n");
    }
}

