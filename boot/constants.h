/*
 * SlopOS Boot Constants (compatibility shim)
 * Legacy header retained for transitional includes. Prefer domain headers:
 *  - mm/mm_constants.h
 *  - drivers/serial_hw.h
 *  - boot/gdt_defs.h
 *  - boot/cpu_defs.h
 *  - video/fb_config.h
 */

#ifndef BOOT_CONSTANTS_H
#define BOOT_CONSTANTS_H

#include "../mm/mm_constants.h"
#include "../drivers/serial_hw.h"
#include "gdt_defs.h"
#include "cpu_defs.h"
#include "../video/fb_config.h"

/* String and buffer sizes */
#define PANIC_MESSAGE_MAX_LEN         256      /* Maximum panic message length */
#define BOOT_CMDLINE_MAX_LEN          512      /* Maximum command line length */

/* Legacy assembly helpers */
#ifdef __ASSEMBLER__
.set CODE_SEL, GDT_CODE_SELECTOR
.set DATA_SEL, GDT_DATA_SELECTOR
#endif /* __ASSEMBLER__ */

#endif /* BOOT_CONSTANTS_H */
