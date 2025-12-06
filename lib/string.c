#include "string.h"

size_t strlen(const char *str) {
    if (!str) {
        return 0;
    }

    size_t length = 0;
    while (str[length] != '\0') {
        length++;
    }

    return length;
}

int strcmp(const char *lhs, const char *rhs) {
    if (lhs == rhs) {
        return 0;
    }

    if (!lhs) {
        return -1;
    }

    if (!rhs) {
        return 1;
    }

    while (*lhs && (*lhs == *rhs)) {
        lhs++;
        rhs++;
    }

    return (unsigned char)*lhs - (unsigned char)*rhs;
}

int strncmp(const char *lhs, const char *rhs, size_t n) {
    if (n == 0) {
        return 0;
    }

    if (!lhs) {
        return rhs ? -1 : 0;
    }

    if (!rhs) {
        return 1;
    }

    while (n > 0 && *lhs == *rhs) {
        if (*lhs == '\0') {
            return 0;
        }

        lhs++;
        rhs++;
        n--;
    }

    if (n == 0) {
        return 0;
    }

    return (unsigned char)*lhs - (unsigned char)*rhs;
}

char *strcpy(char *dest, const char *src) {
    if (!dest || !src) {
        return dest;
    }

    char *out = dest;
    while ((*out++ = *src++) != '\0') {
        /* Copy including null terminator */
    }

    return dest;
}

char *strncpy(char *dest, const char *src, size_t n) {
    if (!dest || n == 0) {
        return dest;
    }

    size_t i = 0;
    for (; i < n && src && src[i] != '\0'; i++) {
        dest[i] = src[i];
    }

    for (; i < n; i++) {
        dest[i] = '\0';
    }

    return dest;
}

int isspace_k(int c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v';
}

int isdigit_k(int c) {
    return c >= '0' && c <= '9';
}

int tolower_k(int c) {
    if (c >= 'A' && c <= 'Z') {
        return c - 'A' + 'a';
    }
    return c;
}

int toupper_k(int c) {
    if (c >= 'a' && c <= 'z') {
        return c - 'a' + 'A';
    }
    return c;
}

int strcasecmp(const char *lhs, const char *rhs) {
    if (lhs == rhs) {
        return 0;
    }
    if (!lhs) {
        return -1;
    }
    if (!rhs) {
        return 1;
    }

    while (*lhs && *rhs) {
        int l = tolower_k((int)*lhs);
        int r = tolower_k((int)*rhs);
        if (l != r) {
            return l - r;
        }
        lhs++;
        rhs++;
    }

    return (int)(unsigned char)*lhs - (int)(unsigned char)*rhs;
}

int strncasecmp(const char *lhs, const char *rhs, size_t n) {
    if (n == 0) {
        return 0;
    }
    if (!lhs) {
        return rhs ? -1 : 0;
    }
    if (!rhs) {
        return 1;
    }

    size_t idx = 0;
    while (idx < n && lhs[idx] && rhs[idx]) {
        int l = tolower_k((int)lhs[idx]);
        int r = tolower_k((int)rhs[idx]);
        if (l != r) {
            return l - r;
        }
        idx++;
    }

    if (idx == n) {
        return 0;
    }

    return (int)(unsigned char)lhs[idx] - (int)(unsigned char)rhs[idx];
}

char *strchr(const char *str, int c) {
    if (!str) {
        return NULL;
    }

    char ch = (char)c;
    while (*str) {
        if (*str == ch) {
            return (char *)str;
        }
        str++;
    }

    if (ch == '\0') {
        return (char *)str;
    }
    return NULL;
}

char *strstr(const char *haystack, const char *needle) {
    if (!haystack || !needle) {
        return NULL;
    }

    if (*needle == '\0') {
        return (char *)haystack;
    }

    size_t needle_len = strlen(needle);
    for (const char *h = haystack; *h; h++) {
        if (*h == *needle) {
            if (strncmp(h, needle, needle_len) == 0) {
                return (char *)h;
            }
        }
    }
    return NULL;
}

int str_has_token(const char *str, const char *token) {
    if (!str || !token) {
        return 0;
    }

    size_t token_len = strlen(token);
    if (token_len == 0) {
        return 0;
    }

    const char *cursor = str;
    while (*cursor) {
        while (*cursor && isspace_k((int)*cursor)) {
            cursor++;
        }
        if (!*cursor) {
            break;
        }

        const char *start = cursor;
        while (*cursor && !isspace_k((int)*cursor)) {
            cursor++;
        }

        size_t len = (size_t)(cursor - start);
        if (len == token_len && strncmp(start, token, token_len) == 0) {
            return 1;
        }
    }

    return 0;
}
