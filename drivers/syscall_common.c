#include "syscall_common.h"
#include "../drivers/wl_currency.h"
#include "../mm/user_copy.h"
#include "../lib/klog.h"

enum syscall_disposition syscall_return_ok(struct interrupt_frame *frame, uint64_t value) {
    if (!frame) {
        wl_award_loss();
        return SYSCALL_DISP_OK;
    }
    wl_award_win();
    frame->rax = value;
    return SYSCALL_DISP_OK;
}

enum syscall_disposition syscall_return_err(struct interrupt_frame *frame, uint64_t err_value) {
    (void)err_value;
    if (!frame) {
        wl_award_loss();
        return SYSCALL_DISP_OK;
    }
    wl_award_loss();
    frame->rax = (uint64_t)-1;
    return SYSCALL_DISP_OK;
}

int syscall_copy_user_str(char *dst, size_t dst_len, const char *user_src) {
    if (!dst || dst_len == 0 || !user_src) {
        return -1;
    }
    /* Always leave space for a terminator. */
    size_t cap = dst_len - 1;
    if (user_copy_from_user(dst, user_src, cap) != 0) {
        return -1;
    }
    dst[cap] = '\0';
    /* Ensure zero termination even if user provided longer string. */
    for (size_t i = 0; i < cap; i++) {
        if (dst[i] == '\0') {
            return 0;
        }
    }
    dst[cap] = '\0';
    return 0;
}

int syscall_bounded_from_user(void *dst,
                              size_t dst_len,
                              const void *user_src,
                              uint64_t requested_len,
                              size_t cap_len,
                              size_t *copied_len_out) {
    if (!dst || dst_len == 0 || !user_src || requested_len == 0) {
        return -1;
    }

    size_t len = (size_t)requested_len;
    if (len > cap_len) {
        len = cap_len;
    }
    if (len > dst_len) {
        len = dst_len;
    }

    if (user_copy_from_user(dst, user_src, len) != 0) {
        return -1;
    }

    if (copied_len_out) {
        *copied_len_out = len;
    }
    return 0;
}

int syscall_copy_to_user_bounded(void *user_dst, const void *src, size_t len) {
    if (!user_dst || !src || len == 0) {
        return -1;
    }
    return user_copy_to_user(user_dst, src, len);
}


