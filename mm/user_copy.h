/*
 * SlopOS User Copy Helpers
 * Safe-ish copy primitives for moving data between ring3 buffers and the kernel.
 */

#ifndef MM_USER_COPY_H
#define MM_USER_COPY_H

#include <stddef.h>
#include <stdint.h>

/* Copy data from a user-provided buffer into a kernel buffer. Returns 0 on success. */
int user_copy_from_user(void *kernel_dst, const void *user_src, size_t len);

/* Copy data from a kernel buffer into a user-provided buffer. Returns 0 on success. */
int user_copy_to_user(void *user_dst, const void *kernel_src, size_t len);

#endif /* MM_USER_COPY_H */

