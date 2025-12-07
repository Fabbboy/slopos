/*
 * SlopOS Syscall Gateway (int 0x80)
 * Provides a narrow ABI for user-mode tasks to enter the kernel.
 */

#include "syscall.h"
#include "../sched/scheduler.h"
#include "../sched/task.h"
#include "../drivers/wl_currency.h"
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

    switch (sysno) {
    case SYSCALL_YIELD:
        wl_award_win();
        yield();
        __builtin_unreachable();
    case SYSCALL_EXIT:
        wl_award_win();
        task_terminate(task->task_id);
        schedule();
        __builtin_unreachable();
    default:
        klog_printf(KLOG_INFO, "SYSCALL: Unknown syscall %llu\n",
                    (unsigned long long)sysno);
        wl_award_loss();
        frame->rax = (uint64_t)-1;
        return;
    }
}

