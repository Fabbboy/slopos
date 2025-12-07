/*
 * SlopOS Syscall Gateway (int 0x80)
 * Provides a narrow ABI for user-mode tasks to enter the kernel.
 */

#include "syscall.h"
#include "../sched/scheduler.h"
#include "../sched/task.h"
#include "../drivers/wl_currency.h"
#include "../lib/klog.h"
#include "../lib/string.h"
#include "../boot/gdt_defs.h"
#include "../boot/kernel_panic.h"
#include "../drivers/tty.h"
#include "../drivers/serial.h"
#include "../mm/user_copy.h"
#include "../mm/process_vm.h"

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

static int syscall_user_write(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    const void *user_buf = (const void *)frame->rdi;
    uint64_t len = frame->rsi;
    if (!user_buf || len == 0) {
        wl_award_loss();
        frame->rax = (uint64_t)-1;
        return -1;
    }

    if (len > 512) {
        len = 512; /* Clamp to keep buffers small */
    }

    char tmp[512];
    if (user_copy_from_user(tmp, user_buf, (size_t)len) != 0) {
        wl_award_loss();
        frame->rax = (uint64_t)-1;
        return -1;
    }

    serial_write(COM1_BASE, tmp, (size_t)len);
    wl_award_win();
    frame->rax = len;
    return 0;
}

static int syscall_user_read(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    void *user_buf = (void *)frame->rdi;
    uint64_t buf_len = frame->rsi;

    if (!user_buf || buf_len == 0) {
        wl_award_loss();
        frame->rax = (uint64_t)-1;
        return -1;
    }

    if (buf_len > 512) {
        buf_len = 512; /* Clamp */
    }

    char tmp[512];
    size_t read_len = tty_read_line(tmp, (size_t)buf_len);

    if (user_copy_to_user(user_buf, tmp, read_len + 1) != 0) {
        wl_award_loss();
        frame->rax = (uint64_t)-1;
        return -1;
    }

    wl_award_win();
    frame->rax = read_len;
    return 0;
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
    case SYSCALL_WRITE:
        syscall_user_write(task, frame);
        return;
    case SYSCALL_READ:
        syscall_user_read(task, frame);
        return;
    case SYSCALL_ROULETTE:
        wl_award_win();
        kernel_roulette();
        frame->rax = 0;
        return;
    default:
        klog_printf(KLOG_INFO, "SYSCALL: Unknown syscall %llu\n",
                    (unsigned long long)sysno);
        wl_award_loss();
        frame->rax = (uint64_t)-1;
        return;
    }
}

