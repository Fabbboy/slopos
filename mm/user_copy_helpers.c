/*
 * User copy validation helpers for syscall handlers.
 * Shared across syscall domains to avoid duplicating geometry checks.
 */

#include "user_copy_helpers.h"
#include "user_copy.h"

int user_copy_rect_checked(user_rect_t *dst, const user_rect_t *user_rect) {
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

int user_copy_line_checked(user_line_t *dst, const user_line_t *user_line) {
    if (!dst || !user_line) {
        return -1;
    }
    if (user_copy_from_user(dst, user_line, sizeof(*dst)) != 0) {
        return -1;
    }
    return 0;
}

int user_copy_circle_checked(user_circle_t *dst, const user_circle_t *user_circle) {
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

int user_copy_text_header(user_text_t *dst, const user_text_t *user_text) {
    if (!dst || !user_text) {
        return -1;
    }
    if (user_copy_from_user(dst, user_text, sizeof(*dst)) != 0) {
        return -1;
    }
    if (!dst->str || dst->len == 0) {
        return -1;
    }
    if (dst->len >= USER_TEXT_MAX_BYTES) {
        dst->len = USER_TEXT_MAX_BYTES - 1;
    }
    return 0;
}

