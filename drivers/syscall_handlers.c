/*
 * Per-domain syscall handlers and dispatch table.
 */

#include "syscall_handlers.h"
#include "syscall.h"
#include "../sched/scheduler.h"
#include "../drivers/wl_currency.h"
#include "../drivers/fate.h"
#include "../drivers/random.h"
#include "../drivers/tty.h"
#include "../drivers/serial.h"
#include "../drivers/pit.h"
#include "../lib/klog.h"
#include "../mm/user_copy.h"
#include "../mm/user_copy_helpers.h"
#include "../mm/process_vm.h"
#include "../video/framebuffer.h"
#include "../video/graphics.h"
#include "../video/font.h"
#include "../shell/shell.h"

static enum syscall_disposition syscall_error(struct interrupt_frame *frame) {
    wl_award_loss();
    frame->rax = (uint64_t)-1;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_yield(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    wl_award_win();
    frame->rax = 0;
    yield();
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_exit(task_t *task, struct interrupt_frame *frame) {
    (void)frame;
    wl_award_win();
    task_terminate(task->task_id);
    schedule();
    return SYSCALL_DISP_NO_RETURN;
}

static enum syscall_disposition syscall_user_write(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    const void *user_buf = (const void *)frame->rdi;
    uint64_t len = frame->rsi;
    if (!user_buf || len == 0) {
        return syscall_error(frame);
    }
    if (len > 512) {
        len = 512;
    }

    char tmp[512];
    if (user_copy_from_user(tmp, user_buf, (size_t)len) != 0) {
        return syscall_error(frame);
    }

    serial_write(COM1_BASE, tmp, (size_t)len);
    wl_award_win();
    frame->rax = len;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_user_read(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    void *user_buf = (void *)frame->rdi;
    uint64_t buf_len = frame->rsi;
    if (!user_buf || buf_len == 0) {
        return syscall_error(frame);
    }
    if (buf_len > 512) {
        buf_len = 512;
    }

    char tmp[512];
    size_t read_len = tty_read_line(tmp, (size_t)buf_len);

    if (user_copy_to_user(user_buf, tmp, read_len + 1) != 0) {
        return syscall_error(frame);
    }

    wl_award_win();
    frame->rax = read_len;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_roulette_spin(task_t *task, struct interrupt_frame *frame) {
    struct fate_result res = fate_spin();
    if (!task) {
        return syscall_error(frame);
    }

    if (fate_set_pending(res, task->task_id) != 0) {
        return syscall_error(frame);
    }
    frame->rax = ((uint64_t)res.token << 32) | res.value;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_sleep_ms(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    uint64_t ms = frame->rdi;
    if (ms > 60000) {
        ms = 60000;
    }
    wl_award_win();
    if (scheduler_is_preemption_enabled()) {
        pit_sleep_ms((uint32_t)ms);
    } else {
        pit_poll_delay_ms((uint32_t)ms);
    }
    frame->rax = 0;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_fb_info(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_fb_info_t info = {0};
    framebuffer_info_t *fb = framebuffer_get_info();
    if (!fb || !fb->initialized) {
        return syscall_error(frame);
    }

    info.width = fb->width;
    info.height = fb->height;
    info.pitch = fb->pitch;
    info.bpp = fb->bpp;
    info.pixel_format = fb->pixel_format;

    if (user_copy_to_user((void *)frame->rdi, &info, sizeof(info)) != 0) {
        return syscall_error(frame);
    }

    wl_award_win();
    frame->rax = 0;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_gfx_fill_rect(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_rect_t rect;
    if (user_copy_rect_checked(&rect, (const user_rect_t *)frame->rdi) != 0) {
        return syscall_error(frame);
    }
    int rc = graphics_draw_rect_filled_fast(rect.x, rect.y, rect.width, rect.height, rect.color);
    frame->rax = rc;
    if (rc == 0) {
        wl_award_win();
    } else {
        wl_award_loss();
    }
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_gfx_draw_line(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_line_t line;
    if (user_copy_line_checked(&line, (const user_line_t *)frame->rdi) != 0) {
        return syscall_error(frame);
    }
    int rc = graphics_draw_line(line.x0, line.y0, line.x1, line.y1, line.color);
    frame->rax = rc;
    if (rc == 0) {
        wl_award_win();
    } else {
        wl_award_loss();
    }
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_gfx_draw_circle(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_circle_t circle;
    if (user_copy_circle_checked(&circle, (const user_circle_t *)frame->rdi) != 0) {
        return syscall_error(frame);
    }
    int rc = graphics_draw_circle(circle.cx, circle.cy, circle.radius, circle.color);
    frame->rax = rc;
    if (rc == 0) {
        wl_award_win();
    } else {
        wl_award_loss();
    }
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_gfx_draw_circle_filled(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_circle_t circle;
    if (user_copy_circle_checked(&circle, (const user_circle_t *)frame->rdi) != 0) {
        return syscall_error(frame);
    }
    int rc = graphics_draw_circle_filled(circle.cx, circle.cy, circle.radius, circle.color);
    frame->rax = rc;
    if (rc == 0) {
        wl_award_win();
    } else {
        wl_award_loss();
    }
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_font_draw(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_text_t text_req;
    if (user_copy_text_header(&text_req, (const user_text_t *)frame->rdi) != 0) {
        return syscall_error(frame);
    }

    char buffer[USER_TEXT_MAX_BYTES];
    if (user_copy_from_user(buffer, text_req.str, text_req.len) != 0) {
        return syscall_error(frame);
    }
    buffer[text_req.len] = '\0';

    int rc = font_draw_string(text_req.x, text_req.y, buffer, text_req.fg_color, text_req.bg_color);
    frame->rax = rc;
    if (rc == 0) {
        wl_award_win();
    } else {
        wl_award_loss();
    }
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_random_next(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    wl_award_win();
    frame->rax = random_next();
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_roulette_result(task_t *task, struct interrupt_frame *frame) {
    uint64_t packed = frame->rdi;
    uint32_t token = (uint32_t)(packed >> 32);
    uint32_t value = (uint32_t)packed;

    struct fate_result res;
    if (!task || fate_take_pending(task->task_id, &res) != 0) {
        return syscall_error(frame);
    }

    if (res.token != token || res.value != value) {
        return syscall_error(frame);
    }

    klog_printf(KLOG_INFO, "SYSCALL_ROULETTE_RESULT fate=0x%x (%u)\n",
                res.value, res.value);

    fate_apply_outcome(&res, FATE_RESOLUTION_REBOOT_ON_LOSS, /*notify_hook=*/1);

    frame->rax = res.is_win ? 0 : (uint64_t)-1;
    return SYSCALL_DISP_OK;
}

static const struct syscall_entry syscall_table[] = {
    [SYSCALL_YIELD] = { syscall_yield, "yield" },
    [SYSCALL_EXIT] = { syscall_exit, "exit" },
    [SYSCALL_WRITE] = { syscall_user_write, "write" },
    [SYSCALL_READ] = { syscall_user_read, "read" },
    [SYSCALL_ROULETTE] = { syscall_roulette_spin, "roulette" },
    [SYSCALL_SLEEP_MS] = { syscall_sleep_ms, "sleep_ms" },
    [SYSCALL_FB_INFO] = { syscall_fb_info, "fb_info" },
    [SYSCALL_GFX_FILL_RECT] = { syscall_gfx_fill_rect, "gfx_fill_rect" },
    [SYSCALL_GFX_DRAW_LINE] = { syscall_gfx_draw_line, "gfx_draw_line" },
    [SYSCALL_GFX_DRAW_CIRCLE] = { syscall_gfx_draw_circle, "gfx_draw_circle" },
    [SYSCALL_GFX_DRAW_CIRCLE_FILLED] = { syscall_gfx_draw_circle_filled, "gfx_draw_circle_filled" },
    [SYSCALL_FONT_DRAW] = { syscall_font_draw, "font_draw" },
    [SYSCALL_RANDOM_NEXT] = { syscall_random_next, "random_next" },
    [SYSCALL_ROULETTE_RESULT] = { syscall_roulette_result, "roulette_result" },
};

const struct syscall_entry *syscall_lookup(uint64_t sysno) {
    if (sysno >= (sizeof(syscall_table) / sizeof(syscall_table[0]))) {
        return NULL;
    }
    return syscall_table[sysno].handler ? &syscall_table[sysno] : NULL;
}

