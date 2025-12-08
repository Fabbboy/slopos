/*
 * Boot initialization framework
 * Provides phased init tables that subsystems can extend without
 * modifying kernel_main directly.
 */

#ifndef BOOT_INIT_H
#define BOOT_INIT_H

#include <stdint.h>

struct boot_init_step {
    const char *name;
    int (*fn)(void);
    uint32_t flags;
};

#define BOOT_INIT_PHASES(_) \
    /* Early hardware bring-up before memory/paging */ \
    _(early_hw) \
    /* Memory allocators and address verification */ \
    _(memory) \
    /* Interrupt controllers, timers, core drivers */ \
    _(drivers) \
    /* Filesystems, scheduler, and kernel services */ \
    _(services) \
    /* Optional demos or features safe to skip */ \
    _(optional)

#define BOOT_INIT_ENUM_ENTRY(phase) BOOT_INIT_PHASE_##phase,
enum boot_init_phase {
    BOOT_INIT_PHASES(BOOT_INIT_ENUM_ENTRY)
    BOOT_INIT_PHASE_COUNT
};
#undef BOOT_INIT_ENUM_ENTRY

int boot_init_run_all(void);
int boot_init_run_phase(enum boot_init_phase phase);
int is_kernel_initialized(void);

#define BOOT_INIT_STEP_WITH_FLAGS(phase, label, fn, flag_value) \
    static const struct boot_init_step boot_init_step_##fn \
    __attribute__((used, section(".boot_init_" #phase))) = { label, fn, flag_value }

#define BOOT_INIT_STEP(phase, label, fn) \
    BOOT_INIT_STEP_WITH_FLAGS(phase, label, fn, 0)

#define BOOT_INIT_OPTIONAL_STEP(phase, label, fn) \
    BOOT_INIT_STEP_WITH_FLAGS(phase, label, fn, 0)

#endif /* BOOT_INIT_H */
