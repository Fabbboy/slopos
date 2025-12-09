/*
 * Syscall dispatch table.
 */

#include "syscall_handlers.h"
#include "syscall_common.h"
#include "../lib/syscall_numbers.h"

/* Core handlers */
enum syscall_disposition syscall_yield(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_exit(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_user_write(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_user_read(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_roulette_spin(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_sleep_ms(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_fb_info(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_gfx_fill_rect(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_gfx_draw_line(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_gfx_draw_circle(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_gfx_draw_circle_filled(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_font_draw(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_random_next(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_roulette_result(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_sys_info(task_t *task, struct interrupt_frame *frame);
enum syscall_disposition syscall_halt(task_t *task, struct interrupt_frame *frame);

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

