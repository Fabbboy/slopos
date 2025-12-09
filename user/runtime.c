/*
 * Minimal userland runtime helpers placed in user-accessible sections.
 */
#include "runtime.h"

#if defined(__clang__)
#pragma clang section text=".user_text" rodata=".user_rodata" data=".user_data"
#else
#pragma GCC push_options
#pragma GCC section text=".user_text" rodata=".user_rodata" data=".user_data"
#endif

USER_TEXT void *u_memcpy(void *dst, const void *src, size_t n) {
    uint8_t *d = (uint8_t *)dst;
    const uint8_t *s = (const uint8_t *)src;
    for (size_t i = 0; i < n; i++) {
        d[i] = s[i];
    }
    return dst;
}

USER_TEXT void *u_memset(void *dst, int c, size_t n) {
    uint8_t *d = (uint8_t *)dst;
    for (size_t i = 0; i < n; i++) {
        d[i] = (uint8_t)c;
    }
    return dst;
}

USER_TEXT size_t u_strlen(const char *s) {
    if (!s) {
        return 0;
    }
    size_t len = 0;
    while (s[len] != '\0') {
        len++;
    }
    return len;
}

USER_TEXT size_t u_strnlen(const char *s, size_t maxlen) {
    if (!s) {
        return 0;
    }
    size_t len = 0;
    while (len < maxlen && s[len] != '\0') {
        len++;
    }
    return len;
}

#if defined(__clang__)
#pragma clang section text="" rodata="" data=""
#else
#pragma GCC pop_options
#endif


