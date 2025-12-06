#include "keyboard.h"
#include "serial.h"
#include "tty.h"
#include "../sched/scheduler.h"
#include "../lib/cpu.h"
#include "../lib/ring_buffer.h"

#include <stdint.h>
#include <stddef.h>
#include "../lib/klog.h"

/* ========================================================================
 * KEYBOARD BUFFER CONFIGURATION
 * ======================================================================== */

#define KEYBOARD_BUFFER_SIZE 256

/* ========================================================================
 * KEYBOARD STATE
 * ======================================================================== */

/* Modifier key states */
typedef struct keyboard_state {
    int shift_left;      /* Left Shift key pressed */
    int shift_right;     /* Right Shift key pressed */
    int ctrl_left;       /* Left Ctrl key pressed */
    int ctrl_right;      /* Right Ctrl key pressed */
    int alt_left;        /* Left Alt key pressed */
    int alt_right;       /* Right Alt key pressed */
    int caps_lock;       /* Caps Lock toggle state */
} keyboard_state_t;

/* Character buffer (circular buffer) */
typedef struct keyboard_buffer {
    char data[KEYBOARD_BUFFER_SIZE];
    uint32_t head;      /* Write position */
    uint32_t tail;      /* Read position */
    uint32_t count;      /* Number of characters in buffer */
} keyboard_buffer_t;

static keyboard_state_t kb_state = {0};
static keyboard_buffer_t char_buffer = {0};
static keyboard_buffer_t scancode_buffer = {0}; /* For debugging */

/* ========================================================================
 * SCANCODE TO ASCII MAPPING (PS/2 Scancode Set 1)
 * ======================================================================== */

/* Base scancode map for letters (a-z) and symbols */
static const char scancode_letters[] = {
    0x00, 0x00, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, /* 0x00-0x07 (2-7) */
    0x37, 0x38, 0x39, 0x30, 0x2D, 0x3D, 0x00, 0x09, /* 0x08-0x0F (8-0, - =, Tab) */
    0x71, 0x77, 0x65, 0x72, 0x74, 0x79, 0x75, 0x69, /* 0x10-0x17 (q-w-e-r-t-y-u-i) */
    0x6F, 0x70, 0x5B, 0x5D, 0x00, 0x00, 0x61, 0x73, /* 0x18-0x1F (o-p-[-], a-s) */
    0x64, 0x66, 0x67, 0x68, 0x6A, 0x6B, 0x6C, 0x3B, /* 0x20-0x27 (d-f-g-h-j-k-l-;) */
    0x27, 0x60, 0x00, 0x5C, 0x7A, 0x78, 0x63, 0x76, /* 0x28-0x2F (', `, (Shift), \, z-x-c-v) */
    0x62, 0x6E, 0x6D, 0x2C, 0x2E, 0x2F, 0x00, 0x00, /* 0x30-0x37 (b-n-m-,-.-/, (unused)) */
    0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x38-0x3F (Space) */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x40-0x47 */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x48-0x4F */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x50-0x57 */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x58-0x5F */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x60-0x67 */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x68-0x6F */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x70-0x77 */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00  /* 0x78-0x7F */
};

/* Shifted scancode map for numbers and symbols */
static const char scancode_shifted[] = {
    0x00, 0x00, 0x21, 0x40, 0x23, 0x24, 0x25, 0x5E, /* 0x00-0x07 (!-@-#-$-%-^) */
    0x26, 0x2A, 0x28, 0x29, 0x5F, 0x2B, 0x00, 0x00, /* 0x08-0x0F (&-*-(-)-_-+) */
    0x51, 0x57, 0x45, 0x52, 0x54, 0x59, 0x55, 0x49, /* 0x10-0x17 (Q-W-E-R-T-Y-U-I) */
    0x4F, 0x50, 0x7B, 0x7D, 0x00, 0x00, 0x41, 0x53, /* 0x18-0x1F (O-P-{-}, A-S) */
    0x44, 0x46, 0x47, 0x48, 0x4A, 0x4B, 0x4C, 0x3A, /* 0x20-0x27 (D-F-G-H-J-K-L-:) */
    0x22, 0x7E, 0x00, 0x7C, 0x5A, 0x58, 0x43, 0x56, /* 0x28-0x2F ("-~-|, Z-X-C-V) */
    0x42, 0x4E, 0x4D, 0x3C, 0x3E, 0x3F, 0x00, 0x00, /* 0x30-0x37 (B-N-M-<- ->-?) */
    0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x38-0x3F (Space) */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x40-0x47 */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x50-0x57 */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x58-0x5F */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x60-0x67 */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x68-0x6F */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, /* 0x70-0x77 */
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00  /* 0x78-0x7F */
};

/* ========================================================================
 * BUFFER OPERATIONS
 * ======================================================================== */

static inline void kb_buffer_push_overwrite(keyboard_buffer_t *buf, char c) {
    RING_BUFFER_PUSH_OVERWRITE(buf, data, c);
}

static char kb_buffer_pop(keyboard_buffer_t *buf) {
    char out = 0;
    int success = 0;

    cpu_cli();
    RING_BUFFER_TRY_POP(buf, data, &out, success);
    cpu_sti();

    return success ? out : 0;
}

static int kb_buffer_has_data(keyboard_buffer_t *buf) {
    int has_data = 0;
    cpu_cli();
    has_data = !RING_BUFFER_IS_EMPTY(buf);
    cpu_sti();
    return has_data;
}

/* ========================================================================
 * SCANCODE TRANSLATION
 * ======================================================================== */

/*
 * Check if scancode is a break code (key release)
 */
static inline int is_break_code(uint8_t scancode) {
    return (scancode & 0x80) != 0;
}

/*
 * Get make code from scancode (clear break bit)
 */
static inline uint8_t get_make_code(uint8_t scancode) {
    return scancode & 0x7F;
}

/*
 * Check if any Shift key is pressed
 */
static inline int shift_pressed(void) {
    return kb_state.shift_left || kb_state.shift_right;
}

/*
 * Check if Caps Lock is enabled
 */
static inline int caps_lock_enabled(void) {
    return kb_state.caps_lock;
}

/*
 * Translate letter scancode to ASCII
 * Handles Shift and Caps Lock modifiers
 */
static char translate_letter(uint8_t make_code) {
    /* Check if shifted version exists and shift is pressed */
    if (shift_pressed() && make_code < sizeof(scancode_shifted)) {
        char shifted = scancode_shifted[make_code];
        if (shifted != 0) {
            return shifted;
        }
    }
    
    /* Use base mapping */
    if (make_code < sizeof(scancode_letters) && scancode_letters[make_code] != 0) {
        char base_char = scancode_letters[make_code];
        
        /* Check if it's a letter (a-z) */
        if (base_char >= 'a' && base_char <= 'z') {
            int should_uppercase = shift_pressed() ^ caps_lock_enabled();
            if (should_uppercase) {
                return base_char - 0x20; /* Convert to uppercase */
            }
            return base_char;
        }
        
        /* Non-letter character - return as-is */
        return base_char;
    }
    
    return 0;
}

/*
 * Translate scancode to ASCII character
 * Returns ASCII character if applicable, 0 if not a character key
 */
static char translate_scancode(uint8_t scancode) {
    uint8_t make_code = get_make_code(scancode);
    char result = 0;
    
    /* Handle special keys first */
    switch (make_code) {
    case 0x1C: /* Enter */
        return '\n'; /* Newline */
    
    case 0x0E: /* Backspace */
        return '\b'; /* Backspace */
    
    case 0x39: /* Space */
        return ' ';
    
    case 0x0F: /* Tab */
        return '\t'; /* Tab */
    
    case 0x01: /* Escape */
        return '\x1B'; /* ESC */
    
    default:
        /* Try to translate as letter/symbol */
        result = translate_letter(make_code);
        break;
    }
    
    return result;
}

/*
 * Handle modifier key press/release
 */
static void handle_modifier_key(uint8_t make_code, int is_press) {
    switch (make_code) {
    case 0x2A: /* Left Shift */
        kb_state.shift_left = is_press;
        break;
    
    case 0x36: /* Right Shift */
        kb_state.shift_right = is_press;
        break;
    
    case 0x1D: /* Left Ctrl */
        kb_state.ctrl_left = is_press;
        break;
    
    case 0x38: /* Left Alt */
        kb_state.alt_left = is_press;
        break;
    
    case 0x3A: /* Caps Lock */
        if (is_press) {
            /* Toggle on press only */
            kb_state.caps_lock = !kb_state.caps_lock;
        }
        break;
    
    default:
        /* Other modifiers not handled yet */
        break;
    }
}

/* ========================================================================
 * PUBLIC API
 * ======================================================================== */

void keyboard_init(void) {
    /* Initialize keyboard state */
    kb_state.shift_left = 0;
    kb_state.shift_right = 0;
    kb_state.ctrl_left = 0;
    kb_state.ctrl_right = 0;
    kb_state.alt_left = 0;
    kb_state.alt_right = 0;
    kb_state.caps_lock = 0;
    
    /* Initialize buffers */
    char_buffer.head = 0;
    char_buffer.tail = 0;
    char_buffer.count = 0;
    
    scancode_buffer.head = 0;
    scancode_buffer.tail = 0;
    scancode_buffer.count = 0;
}

void keyboard_handle_scancode(uint8_t scancode) {
    klog_printf(KLOG_DEBUG, "[KBD] Scancode: 0x%02x\n", scancode);

    int is_press = !is_break_code(scancode);
    uint8_t make_code = get_make_code(scancode);

    klog_printf(KLOG_DEBUG, "[KBD] Make code: 0x%02x is_press: %u\n",
                make_code, (unsigned int)is_press);

    /* Store scancode for debugging */
    kb_buffer_push_overwrite(&scancode_buffer, (char)scancode);
    
    /* Handle modifier keys */
    if (make_code == 0x2A || make_code == 0x36 || /* Shift */
        make_code == 0x1D || /* Ctrl */
        make_code == 0x38 || /* Alt */
        make_code == 0x3A) {  /* Caps Lock */
        handle_modifier_key(make_code, is_press);
        return;
    }
    
    /* Only process key presses for character generation */
    if (!is_press) {
        return;
    }
    
    /* Translate scancode to ASCII */
    char ascii = translate_scancode(scancode);
    klog_printf(KLOG_DEBUG, "[KBD] ASCII: 0x%02x\n", (unsigned int)(uint8_t)ascii);

    if (ascii != 0) {
        klog_printf(KLOG_DEBUG, "[KBD] Adding to buffer\n");
        kb_buffer_push_overwrite(&char_buffer, ascii);
        scheduler_request_reschedule_from_interrupt();
    }
}

char keyboard_getchar(void) {
    return kb_buffer_pop(&char_buffer);
}

int keyboard_has_input(void) {
    int has_data = kb_buffer_has_data(&char_buffer);
    klog_printf(KLOG_DEBUG, "[KBD] buffer_has_data() = %d (count=%u)\n",
                has_data, (unsigned int)char_buffer.count);
    return has_data;
}

int keyboard_buffer_pending(void) {
    return char_buffer.count > 0;
}

uint8_t keyboard_get_scancode(void) {
    return (uint8_t)kb_buffer_pop(&scancode_buffer);
}
