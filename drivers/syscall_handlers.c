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
#include "../fs/fileio.h"
#include "../fs/ramfs.h"
#include "../lib/klog.h"
#include "../mm/user_copy.h"
#include "../mm/user_copy_helpers.h"
#include "../mm/process_vm.h"
#include "../video/framebuffer.h"
#include "../video/graphics.h"
#include "../video/font.h"
#include "../shell/shell.h"
#include "../boot/shutdown.h"
#include "../mm/kernel_heap.h"
#include "../lib/string.h"
#include "../mm/page_alloc.h"

static enum syscall_disposition syscall_error(struct interrupt_frame *frame) {
    wl_award_loss();
    frame->rax = (uint64_t)-1;
    return SYSCALL_DISP_OK;
}

/* Bounded user buffer helpers to keep copy/length checks consistent. */
#define USER_IO_MAX_BYTES 512

static int syscall_validate_len(uint64_t requested_len, size_t *validated_len_out) {
    if (!validated_len_out || requested_len == 0) {
        return -1;
    }
    size_t len = (requested_len > USER_IO_MAX_BYTES) ? USER_IO_MAX_BYTES : (size_t)requested_len;
    *validated_len_out = len;
    return 0;
}

static int syscall_copy_from_user_bounded(char *dst,
                                          size_t dst_size,
                                          const void *user_src,
                                          size_t user_len,
                                          size_t *copied_len_out) {
    if (!dst || dst_size == 0 || !user_src || user_len == 0) {
        return -1;
    }

    size_t len = (user_len > dst_size) ? dst_size : user_len;
    if (user_copy_from_user(dst, user_src, len) != 0) {
        return -1;
    }

    if (copied_len_out) {
        *copied_len_out = len;
    }
    return 0;
}

static int syscall_copy_to_user_bounded(void *user_dst,
                                        const void *src,
                                        size_t src_len) {
    if (!user_dst || src_len == 0) {
        return -1;
    }
    return user_copy_to_user(user_dst, src, src_len);
}

static enum syscall_disposition syscall_finish_gfx(struct interrupt_frame *frame, int rc) {
    frame->rax = rc;
    if (rc == 0) {
        wl_award_win();
    } else {
        wl_award_loss();
    }
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
    if (task) {
        task->exit_reason = TASK_EXIT_REASON_NORMAL;
        task->fault_reason = TASK_FAULT_NONE;
        task->exit_code = 0;
    }
    task_terminate(task->task_id);
    schedule();
    return SYSCALL_DISP_NO_RETURN;
}

static enum syscall_disposition syscall_user_write(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    const void *user_buf = (const void *)frame->rdi;
    uint64_t len = frame->rsi;
    char tmp[USER_IO_MAX_BYTES];
    size_t write_len = 0;

    if (!user_buf || syscall_validate_len(len, &write_len) != 0) {
        return syscall_error(frame);
    }

    if (syscall_copy_from_user_bounded(tmp, sizeof(tmp), user_buf, write_len, NULL) != 0) {
        return syscall_error(frame);
    }

    serial_write(COM1_BASE, tmp, write_len);
    wl_award_win();
    frame->rax = write_len;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_user_read(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    void *user_buf = (void *)frame->rdi;
    uint64_t buf_len = frame->rsi;
    char tmp[USER_IO_MAX_BYTES];
    size_t max_len = 0;

    if (!user_buf || syscall_validate_len(buf_len, &max_len) != 0) {
        return syscall_error(frame);
    }

    /* Use the validated length cap for the read operation. */
    size_t read_len = tty_read_line(tmp, max_len);

    if (syscall_copy_to_user_bounded(user_buf, tmp, read_len + 1) != 0) {
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
    return syscall_finish_gfx(frame, rc);
}

static enum syscall_disposition syscall_gfx_draw_line(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_line_t line;
    if (user_copy_line_checked(&line, (const user_line_t *)frame->rdi) != 0) {
        return syscall_error(frame);
    }
    int rc = graphics_draw_line(line.x0, line.y0, line.x1, line.y1, line.color);
    return syscall_finish_gfx(frame, rc);
}

static enum syscall_disposition syscall_gfx_draw_circle(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_circle_t circle;
    if (user_copy_circle_checked(&circle, (const user_circle_t *)frame->rdi) != 0) {
        return syscall_error(frame);
    }
    int rc = graphics_draw_circle(circle.cx, circle.cy, circle.radius, circle.color);
    return syscall_finish_gfx(frame, rc);
}

static enum syscall_disposition syscall_gfx_draw_circle_filled(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    user_circle_t circle;
    if (user_copy_circle_checked(&circle, (const user_circle_t *)frame->rdi) != 0) {
        return syscall_error(frame);
    }
    int rc = graphics_draw_circle_filled(circle.cx, circle.cy, circle.radius, circle.color);
    return syscall_finish_gfx(frame, rc);
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
    return syscall_finish_gfx(frame, rc);
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

/* ========================== FS & INFO HELPERS ========================== */
#define USER_PATH_MAX 128
#define USER_FS_MAX_ENTRIES 64

static int syscall_copy_path(char *dst, size_t dst_sz, uint64_t user_ptr) {
    if (!dst || dst_sz == 0 || user_ptr == 0) {
        return -1;
    }
    size_t copy_len = dst_sz - 1;
    if (user_copy_from_user(dst, (const void *)user_ptr, copy_len) != 0) {
        return -1;
    }
    dst[copy_len] = '\0';
    for (size_t i = 0; i < dst_sz; i++) {
        if (dst[i] == '\0') {
            return 0;
        }
    }
    dst[dst_sz - 1] = '\0';
    return 0;
}

static uint8_t map_ramfs_type(uint32_t type) {
    return (type == RAMFS_TYPE_DIRECTORY) ? 1 : 0;
}

static enum syscall_disposition syscall_fs_stat(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    char path[USER_PATH_MAX];
    user_fs_stat_t stat = { .type = 0xFF, .size = 0 };

    if (syscall_copy_path(path, sizeof(path), frame->rdi) != 0 || frame->rsi == 0) {
        return syscall_error(frame);
    }

    ramfs_node_t *node = ramfs_find_node(path);
    if (node) {
        stat.type = map_ramfs_type(node->type);
        stat.size = (uint32_t)node->size;
    }

    if (user_copy_to_user((void *)frame->rsi, &stat, sizeof(stat)) != 0) {
        return syscall_error(frame);
    }

    wl_award_win();
    frame->rax = 0;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_fs_read(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    char path[USER_PATH_MAX];
    void *user_buf = (void *)frame->rsi;
    uint64_t len_req = frame->rdx;
    char tmp[USER_IO_MAX_BYTES];

    if (syscall_copy_path(path, sizeof(path), frame->rdi) != 0 || !user_buf || len_req == 0) {
        return syscall_error(frame);
    }

    size_t len = (len_req > USER_IO_MAX_BYTES) ? USER_IO_MAX_BYTES : (size_t)len_req;
    int fd = file_open(path, FILE_OPEN_READ);
    if (fd < 0) {
        return syscall_error(frame);
    }

    ssize_t bytes_read = file_read(fd, tmp, len);
    file_close(fd);
    if (bytes_read < 0) {
        return syscall_error(frame);
    }

    if (user_copy_to_user(user_buf, tmp, (size_t)bytes_read) != 0) {
        return syscall_error(frame);
    }

    wl_award_win();
    frame->rax = (uint64_t)bytes_read;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_fs_write(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    char path[USER_PATH_MAX];
    const void *user_buf = (const void *)frame->rsi;
    uint64_t len_req = frame->rdx;
    char tmp[USER_IO_MAX_BYTES];

    if (syscall_copy_path(path, sizeof(path), frame->rdi) != 0 || !user_buf) {
        return syscall_error(frame);
    }

    size_t len = (len_req > USER_IO_MAX_BYTES) ? USER_IO_MAX_BYTES : (size_t)len_req;
    if (len > 0) {
        if (user_copy_from_user(tmp, user_buf, len) != 0) {
            return syscall_error(frame);
        }
    }

    int fd = file_open(path, FILE_OPEN_WRITE | FILE_OPEN_CREAT);
    if (fd < 0) {
        return syscall_error(frame);
    }
    ssize_t written = 0;
    if (len > 0) {
        written = file_write(fd, tmp, len);
    } else {
        written = 0;
    }
    file_close(fd);

    if (written < 0 || (size_t)written != len) {
        return syscall_error(frame);
    }

    wl_award_win();
    frame->rax = (uint64_t)written;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_fs_mkdir(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    char path[USER_PATH_MAX];
    if (syscall_copy_path(path, sizeof(path), frame->rdi) != 0) {
        return syscall_error(frame);
    }

    ramfs_node_t *created = ramfs_create_directory(path);
    if (!created) {
        return syscall_error(frame);
    }
    wl_award_win();
    frame->rax = 0;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_fs_unlink(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    char path[USER_PATH_MAX];
    if (syscall_copy_path(path, sizeof(path), frame->rdi) != 0) {
        return syscall_error(frame);
    }

    ramfs_node_t *node = ramfs_find_node(path);
    if (!node || node->type != RAMFS_TYPE_FILE) {
        return syscall_error(frame);
    }

    if (file_unlink(path) != 0) {
        return syscall_error(frame);
    }

    wl_award_win();
    frame->rax = 0;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_fs_list(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    char path[USER_PATH_MAX];
    if (syscall_copy_path(path, sizeof(path), frame->rdi) != 0 || frame->rsi == 0) {
        return syscall_error(frame);
    }

    user_fs_list_t list_hdr;
    if (user_copy_from_user(&list_hdr, (const void *)frame->rsi, sizeof(list_hdr)) != 0) {
        return syscall_error(frame);
    }

    uint32_t cap = list_hdr.max_entries;
    if (cap == 0 || cap > USER_FS_MAX_ENTRIES || !list_hdr.entries) {
        return syscall_error(frame);
    }

    ramfs_node_t **entries = NULL;
    int count = 0;
    if (ramfs_list_directory(path, &entries, &count) != 0) {
        return syscall_error(frame);
    }

    if (count < 0) count = 0;
    if ((uint32_t)count > cap) count = (int)cap;

    user_fs_entry_t *tmp = (user_fs_entry_t *)kmalloc(sizeof(user_fs_entry_t) * cap);
    if (!tmp) {
        if (entries) kfree(entries);
        return syscall_error(frame);
    }

    for (int i = 0; i < count; i++) {
        ramfs_node_t *e = entries[i];
        if (!e) {
            tmp[i].name[0] = '\0';
            tmp[i].type = 0;
            tmp[i].size = 0;
            continue;
        }
        size_t nlen = strlen(e->name);
        if (nlen >= sizeof(tmp[i].name)) {
            nlen = sizeof(tmp[i].name) - 1;
        }
        for (size_t j = 0; j < nlen; j++) {
            tmp[i].name[j] = e->name[j];
        }
        tmp[i].name[nlen] = '\0';
        tmp[i].type = map_ramfs_type(e->type);
        tmp[i].size = (uint32_t)e->size;
    }

    list_hdr.count = (uint32_t)count;

    int rc = user_copy_to_user(list_hdr.entries, tmp, sizeof(user_fs_entry_t) * count);
    if (rc == 0) {
        rc = user_copy_to_user((void *)frame->rsi, &list_hdr, sizeof(list_hdr));
    }

    if (entries) kfree(entries);
    kfree(tmp);

    if (rc != 0) {
        return syscall_error(frame);
    }

    wl_award_win();
    frame->rax = 0;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_sys_info(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    if (!frame->rdi) {
        return syscall_error(frame);
    }

    user_sys_info_t info = {0};
    get_page_allocator_stats(&info.total_pages, &info.free_pages, &info.allocated_pages);
    get_task_stats(&info.total_tasks, &info.active_tasks, &info.task_context_switches);
    get_scheduler_stats(&info.scheduler_context_switches, &info.scheduler_yields,
                        &info.ready_tasks, &info.schedule_calls);

    if (user_copy_to_user((void *)frame->rdi, &info, sizeof(info)) != 0) {
        return syscall_error(frame);
    }

    wl_award_win();
    frame->rax = 0;
    return SYSCALL_DISP_OK;
}

static enum syscall_disposition syscall_halt(task_t *task, struct interrupt_frame *frame) {
    (void)task;
    (void)frame;
    wl_award_win();
    kernel_shutdown("user halt");
    return SYSCALL_DISP_NO_RETURN;
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
    [SYSCALL_FS_STAT] = { syscall_fs_stat, "fs_stat" },
    [SYSCALL_FS_READ] = { syscall_fs_read, "fs_read" },
    [SYSCALL_FS_WRITE] = { syscall_fs_write, "fs_write" },
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

