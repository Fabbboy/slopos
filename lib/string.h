#ifndef LIB_STRING_H
#define LIB_STRING_H

#include <stddef.h>

/* Basic string manipulation helpers for the freestanding kernel */
size_t strlen(const char *str);
int strcmp(const char *lhs, const char *rhs);
int strncmp(const char *lhs, const char *rhs, size_t n);
char *strcpy(char *dest, const char *src);
char *strncpy(char *dest, const char *src, size_t n);

/* Character classification and case helpers (ASCII-only) */
int isspace_k(int c);
int isdigit_k(int c);
int tolower_k(int c);
int toupper_k(int c);

/* Case-insensitive comparison */
int strcasecmp(const char *lhs, const char *rhs);
int strncasecmp(const char *lhs, const char *rhs, size_t n);

/* Search helpers */
char *strchr(const char *str, int c);
char *strstr(const char *haystack, const char *needle);

/* Token helper: does str contain token as whitespace-separated entry? */
int str_has_token(const char *str, const char *token);

#endif /* LIB_STRING_H */
