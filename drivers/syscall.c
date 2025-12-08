/*
 * SlopOS Syscall Gateway (int 0x80)
 * Provides a narrow ABI for user-mode tasks to enter the kernel.
 *
 * PRIVILEGE ELEVATION (Ring 3 → Ring 0):
 * When a user task executes `int 0x80`, the CPU automatically:
 *  1. Validates that IDT[0x80].DPL (3) ≥ CPL (3) ✓
 *  2. Saves the user's SS and RSP
 *  3. Loads kernel SS from the code segment descriptor
 *  4. Loads kernel RSP from TSS.RSP0 (set by scheduler before user task execution)
 *  5. Pushes user SS, user RSP, RFLAGS, user CS, user RIP onto the kernel stack
 *  6. Sets CPL to the target segment's DPL (Ring 0)
 *  7. Jumps to the interrupt handler (isr128 → syscall_handle)
 *
 * The kernel handler then:
 *  - Receives an interrupt_frame with the user's full CPU state
 *  - Validates all user pointers before dereferencing (see user_copy.c)
 *  - Executes the requested kernel operation
 *  - Returns via IRETQ, which automatically demotes back to Ring 3
 *
 * Security guarantees:
 *  - User code cannot directly access kernel memory (enforced by page table U/S bits)
 *  - User code cannot execute privileged instructions (enforced by CPL checks)
 *  - All kernel←→user data transfers use safe copy primitives (user_copy_*)
 *  - Separate stacks prevent user stack overflow from corrupting kernel state
 *
 * Syscall ABI:
 *  - rax: syscall number (SYSCALL_YIELD, SYSCALL_WRITE, etc.)
 *  - rdi, rsi, rdx, rcx, r8, r9: syscall arguments
 *  - Return value in rax
 *
 * See docs/PRIVILEGE_SEPARATION.md for architecture details.
 */

#include "syscall.h"
#include "../sched/scheduler.h"
#include "../sched/task.h"
#include "../drivers/wl_currency.h"
#include "../drivers/syscall_handlers.h"
#include "../lib/klog.h"
#include "../boot/gdt_defs.h"

static void save_user_context(struct interrupt_frame *frame, task_t *task) {
    if (!frame || !task) {
        return;
    }

    task_context_t *ctx = &task->context;
    ctx->rax = frame->rax;
    ctx->rbx = frame->rbx;
    ctx->rcx = frame->rcx;
    ctx->rdx = frame->rdx;
    ctx->rsi = frame->rsi;
    ctx->rdi = frame->rdi;
    ctx->rbp = frame->rbp;
    ctx->r8  = frame->r8;
    ctx->r9  = frame->r9;
    ctx->r10 = frame->r10;
    ctx->r11 = frame->r11;
    ctx->r12 = frame->r12;
    ctx->r13 = frame->r13;
    ctx->r14 = frame->r14;
    ctx->r15 = frame->r15;
    ctx->rip = frame->rip;
    ctx->rsp = frame->rsp;
    ctx->rflags = frame->rflags;
    ctx->cs = frame->cs;
    ctx->ss = frame->ss;
    ctx->ds = GDT_USER_DATA_SELECTOR;
    ctx->es = GDT_USER_DATA_SELECTOR;
    ctx->fs = 0;
    ctx->gs = 0;

    task->context_from_user = 1;
    task->user_started = 1;
}

void syscall_handle(struct interrupt_frame *frame) {
    if (!frame) {
        wl_award_loss();
        return;
    }

    task_t *task = scheduler_get_current_task();
    if (!task || !(task->flags & TASK_FLAG_USER_MODE)) {
        wl_award_loss();
        return;
    }

    save_user_context(frame, task);

    uint64_t sysno = frame->rax;
    const struct syscall_entry *entry = syscall_lookup(sysno);
    if (!entry || !entry->handler) {
        klog_printf(KLOG_INFO, "SYSCALL: Unknown syscall %llu\n",
                    (unsigned long long)sysno);
        wl_award_loss();
        frame->rax = (uint64_t)-1;
        return;
    }

    enum syscall_disposition disp = entry->handler(task, frame);
    (void)disp;
}

