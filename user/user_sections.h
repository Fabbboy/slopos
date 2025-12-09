/*
 * Section helpers for code/data that must be user-accessible (U/S=1).
 * Functions tagged USER_TEXT live in the dedicated .user_text section,
 * constants in USER_RODATA, and writable globals in USER_DATA.
 */
#ifndef USER_USER_SECTIONS_H
#define USER_USER_SECTIONS_H

#define USER_TEXT __attribute__((section(".user_text")))
#define USER_RODATA __attribute__((section(".user_rodata")))
#define USER_DATA __attribute__((section(".user_data")))

#endif /* USER_USER_SECTIONS_H */


