/*
 * User copy validation helpers for syscall handlers.
 * Centralizes geometry/text sanity checks so handlers stay lean.
 */

#ifndef MM_USER_COPY_HELPERS_H
#define MM_USER_COPY_HELPERS_H

#include <stdint.h>
#include "../lib/user_syscall_defs.h"

/* Maximum text length accepted from user-space for font drawing. */
#define USER_TEXT_MAX_BYTES 256

int user_copy_rect_checked(user_rect_t *dst, const user_rect_t *user_rect);
int user_copy_line_checked(user_line_t *dst, const user_line_t *user_line);
int user_copy_circle_checked(user_circle_t *dst, const user_circle_t *user_circle);

/*
 * Copy and clamp a user_text header from user space.
 * - Ensures pointer is non-null
 * - Clamps length to USER_TEXT_MAX_BYTES - 1
 */
int user_copy_text_header(user_text_t *dst, const user_text_t *user_text);

#endif /* MM_USER_COPY_HELPERS_H */

