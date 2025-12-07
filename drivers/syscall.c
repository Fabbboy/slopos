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
#include "../boot/shutdown.h"
#include "../drivers/tty.h"
#include "../drivers/serial.h"
#include "../mm/user_copy.h"
#include "../mm/process_vm.h"
#include "../video/framebuffer.h"
#include "../video/graphics.h"
#include "../video/font.h"
#include "../drivers/random.h"
#include "../drivers/pit.h"
#include "../shell/shell.h"
#include "../lib/user_syscall_defs.h"

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

#define SYS_TEXT_MAX 256

static int copy_rect_from_user(user_rect_t *dst, const user_rect_t *user_rect) {
    if (!dst || !user_rect) {
        return -1;
    }
    if (user_copy_from_user(dst, user_rect, sizeof(*dst)) != 0) {
        return -1;
    }
    if (dst->width <= 0 || dst->height <= 0) {
        return -1;
    }
    if (dst->width > 8192 || dst->height > 8192) {
        return -1;
    }
    return 0;
}

static int copy_line_from_user(user_line_t *dst, const user_line_t *user_line) {
    if (!dst || !user_line) {
        return -1;
    }
    if (user_copy_from_user(dst, user_line, sizeof(*dst)) != 0) {
        return -1;
    }
    return 0;
}

static int copy_circle_from_user(user_circle_t *dst, const user_circle_t *user_circle) {
    if (!dst || !user_circle) {
        return -1;
    }
    if (user_copy_from_user(dst, user_circle, sizeof(*dst)) != 0) {
        return -1;
    }
    if (dst->radius <= 0 || dst->radius > 4096) {
        return -1;
    }
    return 0;
}

static int copy_text_from_user(user_text_t *dst, const user_text_t *user_text) {
    if (!dst || !user_text) {
        return -1;
    }
    if (user_copy_from_user(dst, user_text, sizeof(*dst)) != 0) {
        return -1;
    }
    if (!dst->str || dst->len == 0) {
        return -1;
    }
    if (dst->len >= SYS_TEXT_MAX) {
        dst->len = SYS_TEXT_MAX - 1;
    }
    return 0;
}

static int spawn_shell_once(void) {
    static int shell_spawned = 0;
    if (shell_spawned) {
        return 0;
    }

    uint32_t shell_task_id = task_create("shell", shell_main, NULL, 5, TASK_FLAG_USER_MODE);
    if (shell_task_id == INVALID_TASK_ID) {
        kernel_panic("Failed to spawn shell");
    }

    task_t *shell_task;
    if (task_get_info(shell_task_id, &shell_task) != 0) {
        kernel_panic("Failed to fetch shell task info");
    }

    if (schedule_task(shell_task) != 0) {
        kernel_panic("Failed to schedule shell task");
    }

    shell_spawned = 1;
    return 0;
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
    case SYSCALL_SLEEP_MS: {
        uint64_t ms = frame->rdi;
        if (ms > 60000) {
            ms = 60000;
        }
        /*
         * Prefer IRQ-driven sleep when preemption is enabled; fall back to
         * polling when running cooperatively before PIT IRQs are armed.
         */
        wl_award_win();
        if (scheduler_is_preemption_enabled()) {
            pit_sleep_ms((uint32_t)ms);
        } else {
            pit_poll_delay_ms((uint32_t)ms);
        }
        frame->rax = 0;
        return;
    }
    case SYSCALL_FB_INFO: {
        user_fb_info_t info = {0};
        framebuffer_info_t *fb = framebuffer_get_info();
        if (!fb || !fb->initialized) {
            wl_award_loss();
            frame->rax = (uint64_t)-1;
            return;
        }
        info.width = fb->width;
        info.height = fb->height;
        info.pitch = fb->pitch;
        info.bpp = fb->bpp;
        info.pixel_format = fb->pixel_format;

        if (user_copy_to_user((void *)frame->rdi, &info, sizeof(info)) != 0) {
            wl_award_loss();
            frame->rax = (uint64_t)-1;
            return;
        }
        wl_award_win();
        frame->rax = 0;
        return;
    }
    case SYSCALL_GFX_FILL_RECT: {
        user_rect_t rect;
        if (copy_rect_from_user(&rect, (const user_rect_t *)frame->rdi) != 0) {
            wl_award_loss();
            frame->rax = (uint64_t)-1;
            return;
        }
        int rc = graphics_draw_rect_filled_fast(rect.x, rect.y, rect.width, rect.height, rect.color);
        frame->rax = rc;
        if (rc == 0) {
            wl_award_win();
        } else {
            wl_award_loss();
        }
        return;
    }
    case SYSCALL_GFX_DRAW_LINE: {
        user_line_t line;
        if (copy_line_from_user(&line, (const user_line_t *)frame->rdi) != 0) {
            wl_award_loss();
            frame->rax = (uint64_t)-1;
            return;
        }
        int rc = graphics_draw_line(line.x0, line.y0, line.x1, line.y1, line.color);
        frame->rax = rc;
        if (rc == 0) {
            wl_award_win();
        } else {
            wl_award_loss();
        }
        return;
    }
    case SYSCALL_GFX_DRAW_CIRCLE: {
        user_circle_t circle;
        if (copy_circle_from_user(&circle, (const user_circle_t *)frame->rdi) != 0) {
            wl_award_loss();
            frame->rax = (uint64_t)-1;
            return;
        }
        int rc = graphics_draw_circle(circle.cx, circle.cy, circle.radius, circle.color);
        frame->rax = rc;
        if (rc == 0) {
            wl_award_win();
        } else {
            wl_award_loss();
        }
        return;
    }
    case SYSCALL_GFX_DRAW_CIRCLE_FILLED: {
        user_circle_t circle;
        if (copy_circle_from_user(&circle, (const user_circle_t *)frame->rdi) != 0) {
            wl_award_loss();
            frame->rax = (uint64_t)-1;
            return;
        }
        int rc = graphics_draw_circle_filled(circle.cx, circle.cy, circle.radius, circle.color);
        frame->rax = rc;
        if (rc == 0) {
            wl_award_win();
        } else {
            wl_award_loss();
        }
        return;
    }
    case SYSCALL_FONT_DRAW: {
        user_text_t text_req;
        if (copy_text_from_user(&text_req, (const user_text_t *)frame->rdi) != 0) {
            wl_award_loss();
            frame->rax = (uint64_t)-1;
            return;
        }

        char buffer[SYS_TEXT_MAX];
        if (user_copy_from_user(buffer, text_req.str, text_req.len) != 0) {
            wl_award_loss();
            frame->rax = (uint64_t)-1;
            return;
        }
        buffer[text_req.len] = '\0';

        int rc = font_draw_string(text_req.x, text_req.y, buffer, text_req.fg_color, text_req.bg_color);
        frame->rax = rc;
        if (rc == 0) {
            wl_award_win();
        } else {
            wl_award_loss();
        }
        return;
    }
    case SYSCALL_RANDOM_NEXT:
        wl_award_win();
        frame->rax = random_next();
        return;
    case SYSCALL_ROULETTE_RESULT: {
        uint32_t fate = (uint32_t)frame->rdi;
        klog_printf(KLOG_INFO, "SYSCALL_ROULETTE_RESULT fate=0x%x (%u)\n", fate, fate);
        if ((fate & 1U) == 0) {
            wl_award_loss();
            kernel_reboot("Roulette loss - spinning again");
        } else {
            wl_award_win();
            if (spawn_shell_once() != 0) {
                kernel_panic("Failed to start shell after roulette win");
            }
            frame->rax = 0;
        }
        return;
    }
    default:
        klog_printf(KLOG_INFO, "SYSCALL: Unknown syscall %llu\n",
                    (unsigned long long)sysno);
        wl_award_loss();
        frame->rax = (uint64_t)-1;
        return;
    }
}

