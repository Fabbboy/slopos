/*
 * SlopOS User-Mode Roulette using shared core and syscall-backed backend.
 */

#include <stdint.h>
#include <stddef.h>
#include "../lib/user_syscall.h"
#include "../lib/user_syscall_defs.h"
#include "../lib/string.h"
#include "../lib/memory.h"
#include "../lib/klog.h"
#include "../sched/task.h"
#include "../sched/scheduler.h"
#include "../boot/init.h"
#include "roulette.h"
#include "roulette_core.h"

static int user_get_size(void *ctx, int *w, int *h) {
    (void)ctx;
    user_fb_info_t info;
    if (sys_fb_info(&info) != 0) {
        return -1;
    }
    if (info.width == 0 || info.height == 0) {
        return -1;
    }
    if (w) *w = (int)info.width;
    if (h) *h = (int)info.height;
    return 0;
}

static int user_fill_rect(void *ctx, int x, int y, int w, int h, uint32_t color) {
    (void)ctx;
    user_rect_t r = { x, y, w, h, color };
    return (int)sys_gfx_fill_rect(&r);
}

static int user_draw_line(void *ctx, int x0, int y0, int x1, int y1, uint32_t color) {
    (void)ctx;
    user_line_t line = { x0, y0, x1, y1, color };
    return (int)sys_gfx_draw_line(&line);
}

static int user_draw_circle(void *ctx, int cx, int cy, int radius, uint32_t color) {
    (void)ctx;
    user_circle_t c = { cx, cy, radius, color };
    return (int)sys_gfx_draw_circle(&c);
}

static int user_draw_circle_filled(void *ctx, int cx, int cy, int radius, uint32_t color) {
    (void)ctx;
    user_circle_t c = { cx, cy, radius, color };
    return (int)sys_gfx_draw_circle_filled(&c);
}

static int user_draw_text(void *ctx, int x, int y, const char *text, uint32_t fg, uint32_t bg) {
    (void)ctx;
    if (!text) {
        return -1;
    }
    /* Copy the kernel-string into a user-accessible buffer to satisfy validation. */
    char buf[128];
    size_t len = strlen(text);
    if (len >= sizeof(buf)) {
        len = sizeof(buf) - 1;
    }
    memcpy(buf, text, len);
    buf[len] = '\0';

    user_text_t t = {
        .x = x,
        .y = y,
        .fg_color = fg,
        .bg_color = bg,
        .str = buf,
        .len = (uint32_t)len
    };
    return (int)sys_font_draw(&t);
}

static void user_sleep_ms(void *ctx, uint32_t ms) {
    (void)ctx;
    sys_sleep_ms(ms);
}

static const struct roulette_backend user_backend = {
    .ctx = NULL,
    .get_size = user_get_size,
    .fill_rect = user_fill_rect,
    .draw_line = user_draw_line,
    .draw_circle = user_draw_circle,
    .draw_circle_filled = user_draw_circle_filled,
    .draw_text = user_draw_text,
    .sleep_ms = user_sleep_ms,
};

static void roulette_text_fallback(uint32_t fate) {
    sys_write("ROULETTE: framebuffer unavailable, using text fallback\n", 59);
    sys_write("Fate number: ", 13);
    char digits[32];
    uint32_t n = fate;
    int idx = 0;
    if (n == 0) {
        digits[idx++] = '0';
    } else {
        char tmp[32];
        int t = 0;
        while (n && t < 32) {
            tmp[t++] = (char)('0' + (n % 10));
            n /= 10;
        }
        while (t--) {
            digits[idx++] = tmp[t];
        }
    }
    digits[idx] = '\0';
    sys_write(digits, (size_t)idx);
    sys_write("\n", 1);
}

void roulette_user_main(void *arg) {
    (void)arg;
    uint64_t spin = (uint64_t)sys_roulette();
    uint32_t fate = (uint32_t)spin;

    user_fb_info_t info = {0};
    int fb_rc = sys_fb_info(&info);

    /* Track render outcome for logging even if we fallback. */
    int rc = -1;
    int fb_ok = (fb_rc == 0 && info.width != 0 && info.height != 0);

    if (!fb_ok) {
        roulette_text_fallback(fate);
    } else {
        rc = roulette_run(&user_backend, fate);
        if (rc != 0) {
            roulette_text_fallback(fate);
        }
    }

    /* Keep the result visible briefly, then report and exit. */
    sys_sleep_ms(3000);

    /* Log the render result code for debugging. */
    sys_roulette_result(spin);
    sys_sleep_ms(500);

    /* Exit so the shell/demo can progress; framebuffer remains until something else draws. */
    sys_exit();
}

static int boot_step_roulette_task(void) {
    klog_debug("Creating roulette gatekeeper task...");
    uint32_t roulette_task_id = task_create("roulette", roulette_user_main, NULL, 5, TASK_FLAG_USER_MODE);
    if (roulette_task_id == INVALID_TASK_ID) {
        klog_printf(KLOG_INFO, "ERROR: Failed to create roulette task\n");
        return -1;
    }

    task_t *roulette_task_info;
    if (task_get_info(roulette_task_id, &roulette_task_info) != 0) {
        klog_printf(KLOG_INFO, "ERROR: Failed to get roulette task info\n");
        return -1;
    }

    if (schedule_task(roulette_task_info) != 0) {
        klog_printf(KLOG_INFO, "ERROR: Failed to schedule roulette task\n");
        task_terminate(roulette_task_id);
        return -1;
    }

    klog_debug("Roulette task created and scheduled successfully!");
    return 0;
}

BOOT_INIT_STEP_WITH_FLAGS(services, "roulette task", boot_step_roulette_task, BOOT_INIT_PRIORITY(40));
