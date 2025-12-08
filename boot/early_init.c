/*
 * SlopOS Early Initialization
 * Main 64-bit kernel entry point and early setup
 */

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include "../drivers/serial.h"
#include "limine_protocol.h"
#include "init.h"
#include "../lib/klog.h"
#include "../sched/scheduler.h"
#include "../drivers/wl_currency.h"
#include "kernel_panic.h"
#include "../lib/string.h"

// Kernel state tracking
static volatile int kernel_initialized = 0;

struct boot_runtime_context {
    const struct limine_memmap_response *memmap;
    uint64_t hhdm_offset;
    const char *cmdline;
};

static struct boot_runtime_context boot_ctx = {0};

const struct limine_memmap_response *boot_get_memmap(void) {
    return boot_ctx.memmap;
}

uint64_t boot_get_hhdm_offset(void) {
    return boot_ctx.hhdm_offset;
}

const char *boot_get_cmdline(void) {
    return boot_ctx.cmdline;
}

void boot_mark_initialized(void) {
    kernel_initialized = 1;
}

static void boot_info(const char *text) {
    klog_info(text);
}

static void boot_debug(const char *text) {
    klog_debug(text);
}

struct boot_init_phase_desc {
    const char *name;
    const struct boot_init_step *start;
    const struct boot_init_step *end;
};

#define DECLARE_PHASE_BOUNDS(phase) \
    extern const struct boot_init_step __start_boot_init_##phase[]; \
    extern const struct boot_init_step __stop_boot_init_##phase[];

BOOT_INIT_PHASES(DECLARE_PHASE_BOUNDS)
#undef DECLARE_PHASE_BOUNDS

static const struct boot_init_phase_desc boot_phase_table[BOOT_INIT_PHASE_COUNT] = {
#define PHASE_ENTRY(phase) \
    [BOOT_INIT_PHASE_##phase] = { #phase, __start_boot_init_##phase, __stop_boot_init_##phase },
    BOOT_INIT_PHASES(PHASE_ENTRY)
#undef PHASE_ENTRY
};

#define BOOT_INIT_MAX_STEPS 64

static uint32_t boot_step_priority(const struct boot_init_step *step) {
    if (!step) {
        return 0;
    }
    return (step->flags & BOOT_INIT_PRIORITY_MASK);
}

static void boot_init_report_phase(enum klog_level level,
                                   const char *prefix,
                                   const char *value) {
    if (!klog_is_enabled(level)) {
        return;
    }
    klog_printf(level, "[boot:init] %s%s\n", prefix, value ? value : "");
}

static void boot_init_report_step(enum klog_level level,
                                  const char *label,
                                  const char *value) {
    if (!klog_is_enabled(level)) {
        return;
    }
    klog_printf(level, "    %s: %s\n", label, value ? value : "(unnamed)");
}

static void boot_init_report_failure(const char *phase, const char *step_name) {
    klog_printf(KLOG_INFO, "[boot:init] FAILURE in %s -> %s\n",
                phase ? phase : "(unknown)",
                step_name ? step_name : "(unnamed)");
}

static int boot_run_step(const char *phase_name, const struct boot_init_step *step) {
    if (!step || !step->fn) {
        return 0;
    }

    boot_init_report_step(KLOG_DEBUG, "step", step->name);
    int rc = step->fn();
    if (rc != 0) {
        bool optional = (step->flags & BOOT_INIT_FLAG_OPTIONAL) != 0;
        boot_init_report_failure(phase_name, step->name);
        if (optional) {
            boot_info("Optional boot step failed, continuing...");
            return 0;
        }
        kernel_panic("Boot init step failed");
    }
    return 0;
}

int boot_init_run_phase(enum boot_init_phase phase) {
    if (phase < 0 || phase >= BOOT_INIT_PHASE_COUNT) {
        return -1;
    }

    const struct boot_init_phase_desc *desc = &boot_phase_table[phase];
    if (!desc->start || !desc->end) {
        return 0;
    }

    boot_init_report_phase(KLOG_DEBUG, "phase start -> ", desc->name);
    size_t total_steps = (size_t)(desc->end - desc->start);
    if (total_steps > BOOT_INIT_MAX_STEPS) {
        kernel_panic("Boot init: too many steps for phase");
    }

    const struct boot_init_step *ordered[BOOT_INIT_MAX_STEPS];
    size_t ordered_count = 0;

    const struct boot_init_step *cursor = desc->start;
    while (cursor < desc->end) {
        /* Insertion sort by priority to keep deterministic ordering */
        uint32_t prio = boot_step_priority(cursor);
        size_t idx = ordered_count;
        while (idx > 0 && prio < boot_step_priority(ordered[idx - 1])) {
            ordered[idx] = ordered[idx - 1];
            idx--;
        }
        ordered[idx] = cursor;
        ordered_count++;
        cursor++;
    }

    for (size_t i = 0; i < ordered_count; i++) {
        boot_run_step(desc->name, ordered[i]);
    }
    boot_init_report_phase(KLOG_INFO, "phase complete -> ", desc->name);
    return 0;
}

int boot_init_run_all(void) {
    for (int phase = 0; phase < BOOT_INIT_PHASE_COUNT; phase++) {
        int rc = boot_init_run_phase((enum boot_init_phase)phase);
        if (rc != 0) {
            return rc;
        }
    }
    return 0;
}

/* Early hardware phase --------------------------------------------------- */
static int boot_step_serial_init(void) {
    if (serial_init_com1() != 0) {
        boot_info("ERROR: Serial initialization failed");
        return -1;
    }
    klog_attach_serial();
    boot_debug("Serial console ready on COM1");
    return 0;
}

static int boot_step_boot_banner(void) {
    boot_info("SlopOS Kernel Started!");
    boot_info("Booting via Limine Protocol...");
    return 0;
}

static int boot_step_limine_protocol(void) {
    boot_debug("Initializing Limine protocol interface...");
    if (init_limine_protocol() != 0) {
        boot_info("ERROR: Limine protocol initialization failed");
        return -1;
    }
    boot_info("Limine protocol interface ready.");

    if (!is_memory_map_available()) {
        boot_info("ERROR: Limine did not provide a memory map");
        return -1;
    }

    const struct limine_memmap_response *limine_memmap = limine_get_memmap_response();
    if (!limine_memmap) {
        boot_info("ERROR: Limine memory map response pointer is NULL");
        return -1;
    }

    boot_ctx.memmap = limine_memmap;

    if (is_hhdm_available()) {
        boot_ctx.hhdm_offset = get_hhdm_offset();
    } else {
        boot_ctx.hhdm_offset = 0;
        boot_info("WARNING: Limine did not report an HHDM offset");
    }

    boot_ctx.cmdline = get_kernel_cmdline();
    if (boot_ctx.cmdline) {
        boot_debug("Boot command line detected");
    } else {
        boot_debug("Boot command line unavailable");
    }

    return 0;
}

static int boot_step_boot_config(void) {
    if (!boot_ctx.cmdline) {
        return 0;
    }

    if (str_has_token(boot_ctx.cmdline, "boot.debug=on") ||
        str_has_token(boot_ctx.cmdline, "boot.debug=1") ||
        str_has_token(boot_ctx.cmdline, "boot.debug=true") ||
        str_has_token(boot_ctx.cmdline, "bootdebug=on")) {
        klog_set_level(KLOG_DEBUG);
        boot_info("Boot option: debug logging enabled");
    } else if (str_has_token(boot_ctx.cmdline, "boot.debug=off") ||
               str_has_token(boot_ctx.cmdline, "boot.debug=0") ||
               str_has_token(boot_ctx.cmdline, "boot.debug=false") ||
               str_has_token(boot_ctx.cmdline, "bootdebug=off")) {
        klog_set_level(KLOG_INFO);
        boot_debug("Boot option: debug logging disabled");
    }

    return 0;
}

BOOT_INIT_STEP(early_hw, "serial", boot_step_serial_init);
BOOT_INIT_STEP(early_hw, "boot banner", boot_step_boot_banner);
BOOT_INIT_STEP(early_hw, "limine", boot_step_limine_protocol);
BOOT_INIT_STEP(early_hw, "boot config", boot_step_boot_config);

/*
 * Main 64-bit kernel entry point
 * Called from assembly code after successful boot via Limine bootloader
 *
 * This is the Limine protocol version - no parameters needed,
 * Limine provides boot information via static request structures.
 */
void kernel_main(void) {
    /* Initialize the gambling ledger before any subsystem records wins/losses. */
    wl_init();

    if (boot_init_run_all() != 0) {
        kernel_panic("Boot initialization failed");
    }

    if (klog_is_enabled(KLOG_INFO)) {
        klog_newline();
    }
    boot_info("=== KERNEL BOOT SUCCESSFUL ===");
    boot_info("Operational subsystems: serial, interrupts, memory, scheduler, shell");
    boot_info("Graphics: framebuffer required and active");
    boot_info("Kernel initialization complete - ALL SYSTEMS OPERATIONAL!");

    /* The Wheel of Fate now runs only via the user-mode roulette gatekeeper task. */
    boot_info("The kernel has initialized. Handing over to scheduler...");

    boot_info("Starting scheduler...");
    if (klog_is_enabled(KLOG_INFO)) {
        klog_newline();
    }

    // Start scheduler (this will switch to shell task and run it)
    if (start_scheduler() != 0) {
        klog_printf(KLOG_INFO, "ERROR: Scheduler startup failed\n");
        kernel_panic("Scheduler startup failed");
    }
    
    // If we get here, scheduler has exited (shouldn't happen in normal operation)
    klog_printf(KLOG_INFO, "WARNING: Scheduler exited unexpectedly\n");
    while (1) {
        __asm__ volatile ("hlt");  // Halt until next interrupt
    }
}

/*
 * Alternative entry point for compatibility
 */
void kernel_main_no_multiboot(void) {
    kernel_main();
}

/*
 * Get kernel initialization status
 * Returns non-zero if kernel is fully initialized
 */
int is_kernel_initialized(void) {
    return kernel_initialized;
}

/*
 * Get kernel initialization progress as percentage
 * Returns 0-100 indicating initialization progress
 */
int get_initialization_progress(void) {
    if (!kernel_initialized) {
        return 50;  // Basic boot complete, subsystems pending
    }
    return 100;     // Fully initialized
}

/*
 * Early kernel status reporting
 */
void report_kernel_status(void) {
    if (is_kernel_initialized()) {
        klog_info("SlopOS: Kernel status - INITIALIZED");
    } else {
        klog_info("SlopOS: Kernel status - INITIALIZING");
    }
}
