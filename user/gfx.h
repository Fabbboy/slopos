/*
 * Userland graphics helpers wrapping the syscall ABI.
 */
#ifndef USER_GFX_H
#define USER_GFX_H

#include <stdint.h>
#include "../lib/user_syscall.h"
#include "user_sections.h"

static inline USER_TEXT long ugfx_fb_info(user_fb_info_t *info) {
    return sys_fb_info(info);
}

static inline USER_TEXT long ugfx_fill_rect(const user_rect_t *rect) {
    return sys_gfx_fill_rect(rect);
}

static inline USER_TEXT long ugfx_draw_line(const user_line_t *line) {
    return sys_gfx_draw_line(line);
}

static inline USER_TEXT long ugfx_draw_circle(const user_circle_t *circle) {
    return sys_gfx_draw_circle(circle);
}

static inline USER_TEXT long ugfx_draw_circle_filled(const user_circle_t *circle) {
    return sys_gfx_draw_circle_filled(circle);
}

static inline USER_TEXT long ugfx_draw_text(const user_text_t *text) {
    return sys_font_draw(text);
}

#endif /* USER_GFX_H */


