/*
 * Minimal userland runtime helpers (standalone, no kernel deps).
 */
#ifndef USER_RUNTIME_H
#define USER_RUNTIME_H

#include <stddef.h>
#include <stdint.h>
#include "user_sections.h"

USER_TEXT void *u_memcpy(void *dst, const void *src, size_t n);
USER_TEXT void *u_memset(void *dst, int c, size_t n);
USER_TEXT size_t u_strlen(const char *s);
USER_TEXT size_t u_strnlen(const char *s, size_t maxlen);

#endif /* USER_RUNTIME_H */


