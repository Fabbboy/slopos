/*
 * Unified syscall handlers and dispatch table.
 */

#include "syscall_handlers.h"
#include "syscall_common.h"
#include "../drivers/tty.h"
#include "../drivers/serial.h"
#include "../drivers/pit.h"
#include "../drivers/fate.h"
#include "../drivers/random.h"
#include "../video/framebuffer.h"
#include "../video/graphics.h"
#include "../video/font.h"
#include "../lib/klog.h"
#include "../lib/user_syscall_defs.h"
#include "../mm/page_alloc.h"
#include "../mm/kernel_heap.h"
#include "../mm/user_copy_helpers.h"
#include "../mm/user_copy.h"
#include "../sched/scheduler.h"
#include "../sched/task.h"
#include "../boot/shutdown.h"
#include "../lib/syscall_numbers.h"

static enum syscall_disposition syscall_finish_gfx(struct interrupt_frame *frame, int rc) {
    if (rc == 0) {
        return syscall_return_ok(frame, 0);
    }
    return syscall_return_err(frame, -1);
}

enum syscall_disposition syscall_yield(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    syscall_return_ok(frame, 0);
    yield();
    return SYSCALL_DISP_OK;
}

enum syscall_disposition syscall_exit(task_t *task, struct interrupt_frame *frame) {
    (void)frame;
    if (task) {
        task->exit_reason = TASK_EXIT_REASON_NORMAL;
        task->fault_reason = TASK_FAULT_NONE;
        task->exit_code = 0;
    }
    task_terminate(task ? task->task_id : INVALID_TASK_ID);
    schedule();
    return SYSCALL_DISP_NO_RETURN;
}

enum syscall_disposition syscall_user_write(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    char tmp[USER_IO_MAX_BYTES];
    size_t write_len = 0;

    if (!frame->rdi || syscall_bounded_from_user(tmp, sizeof(tmp), (const void *)frame->rdi,
                                                 frame->rsi, USER_IO_MAX_BYTES, &write_len) != 0) {
        return syscall_return_err(frame, -1);
    }

    serial_write(COM1_BASE, tmp, write_len);
    return syscall_return_ok(frame, write_len);
}

enum syscall_disposition syscall_user_read(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    char tmp[USER_IO_MAX_BYTES];
    if (!frame->rdi || frame->rsi == 0) {
        return syscall_return_err(frame, -1);
    }

    size_t max_len = (frame->rsi > USER_IO_MAX_BYTES) ? USER_IO_MAX_BYTES : (size_t)frame->rsi;
    size_t read_len = tty_read_line(tmp, max_len);

    if (syscall_copy_to_user_bounded((void *)frame->rdi, tmp, read_len + 1) != 0) {
        return syscall_return_err(frame, -1);
    }

    return syscall_return_ok(frame, read_len);
}

enum syscall_disposition syscall_roulette_spin(task_t *task, struct interrupt_frame *frame) {
    struct fate_result res = fate_spin();
    if (!task) {
        return syscall_return_err(frame, -1);
    }

    if (fate_set_pending(res, task->task_id) != 0) {
        return syscall_return_err(frame, -1);
    }
    uint64_t packed = ((uint64_t)res.token << 32) | res.value;
    return syscall_return_ok(frame, packed);
}

enum syscall_disposition syscall_sleep_ms(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    uint64_t ms = frame->rdi;
    if (ms > 60000) {
        ms = 60000;
    }
    if (scheduler_is_preemption_enabled()) {
        pit_sleep_ms((uint32_t)ms);
    } else {
        pit_poll_delay_ms((uint32_t)ms);
    }
    return syscall_return_ok(frame, 0);
}

enum syscall_disposition syscall_fb_info(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_fb_info_t info = {0};
    framebuffer_info_t *fb = framebuffer_get_info();
    if (!fb || !fb->initialized) {
        return syscall_return_err(frame, -1);
    }

    info.width = fb->width;
    info.height = fb->height;
    info.pitch = fb->pitch;
    info.bpp = fb->bpp;
    info.pixel_format = fb->pixel_format;

    if (syscall_copy_to_user_bounded((void *)frame->rdi, &info, sizeof(info)) != 0) {
        return syscall_return_err(frame, -1);
    }

    return syscall_return_ok(frame, 0);
}

enum syscall_disposition syscall_gfx_fill_rect(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_rect_t rect;
    if (user_copy_rect_checked(&rect, (const user_rect_t *)frame->rdi) != 0) {
        return syscall_return_err(frame, -1);
    }
    int rc = graphics_draw_rect_filled_fast(rect.x, rect.y, rect.width, rect.height, rect.color);
    return syscall_finish_gfx(frame, rc);
}

enum syscall_disposition syscall_gfx_draw_line(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_line_t line;
    if (user_copy_line_checked(&line, (const user_line_t *)frame->rdi) != 0) {
        return syscall_return_err(frame, -1);
    }
    int rc = graphics_draw_line(line.x0, line.y0, line.x1, line.y1, line.color);
    return syscall_finish_gfx(frame, rc);
}

enum syscall_disposition syscall_gfx_draw_circle(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_circle_t circle;
    if (user_copy_circle_checked(&circle, (const user_circle_t *)frame->rdi) != 0) {
        return syscall_return_err(frame, -1);
    }
    int rc = graphics_draw_circle(circle.cx, circle.cy, circle.radius, circle.color);
    return syscall_finish_gfx(frame, rc);
}

enum syscall_disposition syscall_gfx_draw_circle_filled(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_circle_t circle;
    if (user_copy_circle_checked(&circle, (const user_circle_t *)frame->rdi) != 0) {
        return syscall_return_err(frame, -1);
    }
    int rc = graphics_draw_circle_filled(circle.cx, circle.cy, circle.radius, circle.color);
    return syscall_finish_gfx(frame, rc);
}

enum syscall_disposition syscall_font_draw(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_text_t text;
    if (user_copy_text_header(&text, (const user_text_t *)frame->rdi) != 0) {
        return syscall_return_err(frame, -1);
    }
    if (text.len == 0 || text.len >= USER_TEXT_MAX_BYTES) {
        return syscall_return_err(frame, -1);
    }
    char buf[USER_TEXT_MAX_BYTES];
    if (user_copy_from_user(buf, text.str, text.len) != 0) {
        return syscall_return_err(frame, -1);
    }
    buf[text.len] = '\0';
    int rc = font_draw_string(text.x, text.y, buf, text.fg_color, text.bg_color);
    return syscall_finish_gfx(frame, rc);
}

enum syscall_disposition syscall_random_next(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    uint32_t value = random_next();
    return syscall_return_ok(frame, value);
}

enum syscall_disposition syscall_roulette_result(task_t *task, struct interrupt_frame *frame) {
    if (!task) {
        return syscall_return_err(frame, -1);
    }
    struct fate_result stored = {0};
    if (fate_take_pending(task->task_id, &stored) != 0) {
        return syscall_return_err(frame, -1);
    }
    uint32_t token = (uint32_t)(frame->rdi >> 32);
    if (token != stored.token) {
        return syscall_return_err(frame, -1);
    }
    fate_apply_outcome(&stored, FATE_RESOLUTION_REBOOT_ON_LOSS, true);
    return syscall_return_ok(frame, 0);
}

enum syscall_disposition syscall_sys_info(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    if (!frame->rdi) {
        return syscall_return_err(frame, -1);
    }

    user_sys_info_t info = {0};
    get_page_allocator_stats(&info.total_pages, &info.free_pages, &info.allocated_pages);
    get_task_stats(&info.total_tasks, &info.active_tasks, &info.task_context_switches);
    get_scheduler_stats(&info.scheduler_context_switches, &info.scheduler_yields,
                        &info.ready_tasks, &info.schedule_calls);

    if (syscall_copy_to_user_bounded((void *)frame->rdi, &info, sizeof(info)) != 0) {
        return syscall_return_err(frame, -1);
    }

    return syscall_return_ok(frame, 0);
}

enum syscall_disposition syscall_halt(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    (void)frame;
    kernel_shutdown("user halt");
    return SYSCALL_DISP_NO_RETURN;
}

/* Filesystem handlers */
enum syscall_disposition syscall_fs_open(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_fs_close(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_fs_read(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_fs_write(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_fs_stat(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_fs_mkdir(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_fs_unlink(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_fs_list(task_t *task, struct interrupt_frame *frame);

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
    [SYSCALL_FS_OPEN] = { syscall_fs_open, "fs_open" },
    [SYSCALL_FS_CLOSE] = { syscall_fs_close, "fs_close" },
    [SYSCALL_FS_READ] = { syscall_fs_read, "fs_read" },
    [SYSCALL_FS_WRITE] = { syscall_fs_write, "fs_write" },
    [SYSCALL_FS_STAT] = { syscall_fs_stat, "fs_stat" },
    [SYSCALL_FS_MKDIR] = { syscall_fs_mkdir, "fs_mkdir" },
    [SYSCALL_FS_UNLINK] = { syscall_fs_unlink, "fs_unlink" },
    [SYSCALL_FS_LIST] = { syscall_fs_list, "fs_list" },
    [SYSCALL_SYS_INFO] = { syscall_sys_info, "sys_info" },
    [SYSCALL_HALT] = { syscall_halt, "halt" },
};

const struct syscall_entry *syscall_lookup(uint64_t sysno) {
    if (sysno >= (sizeof(syscall_table) / sizeof(syscall_table[0]))) {
        return NULL;
    }
    return syscall_table[sysno].handler ? &syscall_table[sysno] : NULL;
}

