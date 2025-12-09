#ifndef DRIVERS_SYSCALL_COMMON_H
#define DRIVERS_SYSCALL_COMMON_H

#include <stddef.h>
#include <stdint.h>
#include "../sched/task.h"
#include "../boot/idt.h"

#define USER_IO_MAX_BYTES 512
#define USER_PATH_MAX     128

enum syscall_disposition {
    SYSCALL_DISP_OK = 0,
    SYSCALL_DISP_NO_RETURN, /* Handler does not return to the same context */
};

typedef enum syscall_disposition (*syscall_handler_t)(task_t *task, struct interrupt_frame *frame);

struct syscall_entry {
    syscall_handler_t handler;
    const char *name;
};

enum syscall_disposition syscall_return_ok(struct interrupt_frame *frame, uint64_t value);
enum syscall_disposition syscall_return_err(struct interrupt_frame *frame, uint64_t err_value);

int syscall_copy_user_str(char *dst, size_t dst_len, const char *user_src);
int syscall_bounded_from_user(void *dst,
                              size_t dst_len,
                              const void *user_src,
                              uint64_t requested_len,
                              size_t cap_len,
                              size_t *copied_len_out);
int syscall_copy_to_user_bounded(void *user_dst, const void *src, size_t len);

#endif /* DRIVERS_SYSCALL_COMMON_H */

