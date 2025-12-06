#ifndef LIB_KDIAG_H
#define LIB_KDIAG_H

#include <stdint.h>
#include <stddef.h>
#include "../boot/idt.h"

#define KDIAG_STACK_TRACE_DEPTH 16

/*
 * Diagnostic helpers for CPU state, stack traces, and frame dumps.
 */

uint64_t kdiag_timestamp(void);

void kdiag_dump_cpu_state(void);
void kdiag_dump_interrupt_frame(struct interrupt_frame *frame);
void kdiag_dump_stack_trace(void);
void kdiag_dump_stack_trace_from_rbp(uint64_t rbp);
void kdiag_dump_stack_trace_from_frame(struct interrupt_frame *frame);
void kdiag_hexdump(const void *data, size_t length, uint64_t base_address);

#endif /* LIB_KDIAG_H */

