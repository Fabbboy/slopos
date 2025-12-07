/*
 * SlopOS Early Initialization
 * Main 64-bit kernel entry point and early setup
 */

#include <stdint.h>
#include <stddef.h>
#include "../drivers/serial.h"
#include "../mm/mm_constants.h"
#include "idt.h"
#include "gdt.h"
#include "limine_protocol.h"
#include "init.h"
#include "../lib/klog.h"
#include "safe_stack.h"
#include "shutdown.h"
#include "../drivers/apic.h"
#include "../drivers/pit.h"
#include "../drivers/irq.h"
#include "../drivers/ioapic.h"
#include "../drivers/pic_quiesce.h"
#include "../drivers/interrupt_test.h"
#include "../sched/task.h"
#include "../sched/scheduler.h"
#include "../shell/shell.h"
#include "../fs/ramfs.h"
#include "../video/framebuffer.h"
#include "../video/graphics.h"
#include "../video/font.h"
#include "../video/splash.h"
#include "../drivers/pci.h"
#include "../drivers/wl_currency.h"
#include "kernel_panic.h"
#include "cpu_verify.h"
#include "../mm/memory_init.h"
#include "../lib/string.h"

// Kernel state tracking
static volatile int kernel_initialized = 0;

struct boot_runtime_context {
    const struct limine_memmap_response *memmap;
    uint64_t hhdm_offset;
    const char *cmdline;
};

static struct boot_runtime_context boot_ctx = {0};

static int optional_steps_enabled = 1;

static void boot_info(const char *text) {
    klog_info(text);
}

static void boot_debug(const char *text) {
    klog_debug(text);
}

void boot_init_set_optional_enabled(int enabled) {
    optional_steps_enabled = enabled ? 1 : 0;
}

int boot_init_optional_enabled(void) {
    return optional_steps_enabled;
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

static void boot_init_report_skip(const char *value) {
    if (!klog_is_enabled(KLOG_DEBUG)) {
        return;
    }
    klog_printf(KLOG_DEBUG, "    skip -> %s\n", value ? value : "(unnamed)");
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

    if ((step->flags & BOOT_INIT_FLAG_OPTIONAL) && !boot_init_optional_enabled()) {
        boot_init_report_skip(step->name);
        return 0;
    }

    boot_init_report_step(KLOG_DEBUG, "step", step->name);
    int rc = step->fn();
    if (rc != 0) {
        boot_init_report_failure(phase_name, step->name);
        kernel_panic("Boot init step failed");
    }
    return rc;
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
    const struct boot_init_step *cursor = desc->start;
    while (cursor < desc->end) {
        boot_run_step(desc->name, cursor);
        cursor++;
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

    if (str_has_token(boot_ctx.cmdline, "demo=off") ||
        str_has_token(boot_ctx.cmdline, "demo=disabled") ||
        str_has_token(boot_ctx.cmdline, "video=off") ||
        str_has_token(boot_ctx.cmdline, "no-demo")) {
        boot_init_set_optional_enabled(0);
        boot_info("Boot option: framebuffer demo disabled");
    } else if (str_has_token(boot_ctx.cmdline, "demo=on") ||
               str_has_token(boot_ctx.cmdline, "demo=enabled")) {
        boot_init_set_optional_enabled(1);
        boot_info("Boot option: framebuffer demo enabled");
    }

    return 0;
}

BOOT_INIT_STEP(early_hw, "serial", boot_step_serial_init);
BOOT_INIT_STEP(early_hw, "boot banner", boot_step_boot_banner);
BOOT_INIT_STEP(early_hw, "limine", boot_step_limine_protocol);
BOOT_INIT_STEP(early_hw, "boot config", boot_step_boot_config);

/* Memory phase ----------------------------------------------------------- */
static int boot_step_memory_init(void) {
    if (!boot_ctx.memmap) {
        boot_info("ERROR: Memory map not available");
        return -1;
    }

    boot_debug("Initializing memory management from Limine data...");
    if (init_memory_system(boot_ctx.memmap, boot_ctx.hhdm_offset) != 0) {
        boot_info("ERROR: Memory system initialization failed");
        return -1;
    }
    boot_info("Memory management initialized.");
    return 0;
}

static int boot_step_memory_verify(void) {
    uint64_t stack_ptr;
    __asm__ volatile ("movq %%rsp, %0" : "=r" (stack_ptr));

    if (klog_is_enabled(KLOG_DEBUG)) {
        boot_debug("Stack pointer read successfully!");
        klog_printf(KLOG_INFO, "Current Stack Pointer: 0x%lx\n", stack_ptr);

        void *current_ip = __builtin_return_address(0);
        klog_printf(KLOG_INFO, "Kernel Code Address: 0x%lx\n", (uint64_t)current_ip);

        if ((uint64_t)current_ip >= KERNEL_VIRTUAL_BASE) {
            boot_debug("Running in higher-half virtual memory - CORRECT");
        } else {
            boot_info("WARNING: Not running in higher-half virtual memory");
        }
    }

    return 0;
}

BOOT_INIT_STEP(memory, "memory init", boot_step_memory_init);
BOOT_INIT_STEP(memory, "address verification", boot_step_memory_verify);

/* Driver phase ----------------------------------------------------------- */
static int boot_step_debug_subsystem(void) {
    boot_debug("Debug/logging subsystem initialized.");
    return 0;
}

static int boot_step_gdt_setup(void) {
    boot_debug("Initializing GDT/TSS...");
    gdt_init();
    boot_debug("GDT/TSS initialized.");
    return 0;
}

static int boot_step_idt_setup(void) {
    boot_debug("Initializing IDT...");
    idt_init();
    safe_stack_init();
    idt_load();
    boot_debug("IDT initialized and loaded.");
    return 0;
}

static int boot_step_irq_setup(void) {
    boot_debug("Configuring IRQ dispatcher...");
    irq_init();
    if (serial_enable_interrupts(COM1_BASE, SERIAL_COM1_IRQ) != 0) {
        boot_info("WARNING: Failed to enable COM1 serial interrupts");
    } else {
        boot_debug("COM1 serial interrupts armed.");
    }
    boot_debug("IRQ dispatcher ready.");
    return 0;
}

static int boot_step_timer_setup(void) {
    boot_debug("Initializing programmable interval timer...");
    pit_init(PIT_DEFAULT_FREQUENCY_HZ);
    boot_debug("Programmable interval timer configured.");

    /* Observe early PIT IRQ health: count ticks after a short polling delay. */
    uint64_t ticks_before = irq_get_timer_ticks();
    pit_poll_delay_ms(100);
    uint64_t ticks_after = irq_get_timer_ticks();
    klog_printf(KLOG_INFO, "BOOT: PIT ticks after 100ms poll: %llu -> %llu\n",
                (unsigned long long)ticks_before,
                (unsigned long long)ticks_after);
    if (ticks_after == ticks_before) {
        klog_printf(KLOG_INFO, "BOOT: WARNING - no PIT IRQs observed in 100ms window\n");
    }

    // Initialize framebuffer and splash screen right after PIT is ready
    if (framebuffer_init() == 0) {
        splash_show_boot_screen();
        splash_report_progress(10, "Graphics initialized");

        // Report the upcoming driver steps with delays to show progression
        splash_report_progress(20, "Initializing debug...");
        splash_report_progress(30, "Setting up GDT/TSS...");
        splash_report_progress(40, "Setting up interrupts...");
        splash_report_progress(50, "Setting up IRQ dispatcher...");
    }

    return 0;
}

static int boot_step_apic_setup(void) {
    boot_debug("Detecting Local APIC...");
    splash_report_progress(60, "Detecting APIC...");
    if (!apic_detect()) {
        kernel_panic("SlopOS requires a Local APIC - legacy PIC is gone");
    }

    boot_debug("Initializing Local APIC...");
    splash_report_progress(65, "Initializing APIC...");
    if (apic_init() != 0) {
        kernel_panic("Local APIC initialization failed");
    }

    pic_quiesce_disable();

    boot_debug("Local APIC initialized (legacy PIC path removed).");
    return 0;
}

static int boot_step_ioapic_setup(void) {
    boot_debug("Discovering IOAPIC controllers via ACPI MADT...");
    splash_report_progress(67, "Discovering IOAPIC...");
    if (ioapic_init() != 0) {
        kernel_panic("IOAPIC discovery failed - SlopOS cannot operate without it");
    }
    boot_debug("IOAPIC: discovery complete, ready for redirection programming.");
    return 0;
}

static int boot_step_pci_init(void) {
    boot_debug("Enumerating PCI devices...");
    splash_report_progress(70, "Enumerating PCI devices...");
    if (pci_init() == 0) {
        boot_debug("PCI subsystem initialized");
        const pci_gpu_info_t *gpu = pci_get_primary_gpu();
        if (gpu && gpu->present) {
            klog_printf(KLOG_DEBUG, "PCI: Primary GPU detected (bus %u, device %u, function %u)\n",
                        gpu->device.bus,
                        gpu->device.device,
                        gpu->device.function);
            if (gpu->mmio_virt_base) {
                klog_printf(KLOG_DEBUG, "PCI: GPU MMIO virtual base 0x%llx, size 0x%llx\n",
                            (unsigned long long)(uintptr_t)gpu->mmio_virt_base,
                            (unsigned long long)gpu->mmio_size);
            } else {
                klog_printf(KLOG_DEBUG, "PCI: WARNING GPU MMIO mapping unavailable\n");
            }
        } else {
            boot_debug("PCI: No GPU-class device discovered during enumeration");
        }
    } else {
        boot_info("WARNING: PCI initialization failed");
    }
    return 0;
}


static int boot_step_interrupt_tests(void) {
    struct interrupt_test_config test_config;
    interrupt_test_config_init_defaults(&test_config);

    if (boot_ctx.cmdline) {
        interrupt_test_config_parse_cmdline(&test_config, boot_ctx.cmdline);
    }

    if (test_config.enabled && test_config.suite_mask == 0) {
        boot_info("INTERRUPT_TEST: No suites selected, skipping execution");
        test_config.enabled = 0;
        test_config.shutdown_on_complete = 0;
    }

    if (!test_config.enabled) {
        boot_debug("INTERRUPT_TEST: Harness disabled");
        return 0;
    }

    boot_info("INTERRUPT_TEST: Running interrupt harness");
    splash_report_progress(75, "Running interrupt tests...");

    if (klog_is_enabled(KLOG_DEBUG)) {
        klog_printf(KLOG_INFO, "INTERRUPT_TEST: Suites -> %s\n",
                    interrupt_test_suite_string(test_config.suite_mask));

        klog_printf(KLOG_INFO, "INTERRUPT_TEST: Verbosity -> %s\n",
                    interrupt_test_verbosity_string(test_config.verbosity));

        klog_printf(KLOG_INFO, "INTERRUPT_TEST: Timeout (ms) -> %u\n", test_config.timeout_ms);
    }

    interrupt_test_init(&test_config);
    int passed = run_all_interrupt_tests(&test_config);
    const struct test_stats *stats = test_get_stats();
    uint32_t failed_tests = stats ? stats->failed_cases : 0;
    interrupt_test_cleanup();

    if (klog_is_enabled(KLOG_DEBUG)) {
        klog_printf(KLOG_INFO, "INTERRUPT_TEST: Boot run passed tests -> %d\n", passed);
    }

    if (test_config.shutdown_on_complete) {
        boot_debug("INTERRUPT_TEST: Auto shutdown enabled after harness");
        interrupt_test_request_shutdown((int)failed_tests);
    }

    if (failed_tests > 0) {
        boot_info("INTERRUPT_TEST: Failures detected");
    } else {
        boot_info("INTERRUPT_TEST: Completed successfully");
    }
    return 0;
}

BOOT_INIT_STEP(drivers, "debug", boot_step_debug_subsystem);
BOOT_INIT_STEP(drivers, "gdt/tss", boot_step_gdt_setup);
BOOT_INIT_STEP(drivers, "idt", boot_step_idt_setup);
BOOT_INIT_STEP(drivers, "apic", boot_step_apic_setup);
BOOT_INIT_STEP(drivers, "ioapic", boot_step_ioapic_setup);
BOOT_INIT_STEP(drivers, "irq dispatcher", boot_step_irq_setup);
BOOT_INIT_STEP(drivers, "timer", boot_step_timer_setup);
BOOT_INIT_STEP(drivers, "pci", boot_step_pci_init);
BOOT_INIT_STEP(drivers, "interrupt tests", boot_step_interrupt_tests);

/* Services phase --------------------------------------------------------- */
static int boot_step_ramfs_init(void) {
    if (ramfs_init() != 0) {
        boot_info("ERROR: RamFS initialization failed");
        return -1;
    }
    boot_debug("RamFS initialized.");
    return 0;
}

static int boot_step_task_manager_init(void) {
    boot_debug("Initializing task manager...");
    splash_report_progress(85, "Initializing scheduler...");
    if (init_task_manager() != 0) {
        boot_info("ERROR: Task manager initialization failed");
        return -1;
    }
    boot_debug("Task manager initialized.");
    return 0;
}

static int boot_step_scheduler_init(void) {
    boot_debug("Initializing scheduler subsystem...");
    splash_report_progress(90, "Starting task manager...");
    if (init_scheduler() != 0) {
        boot_info("ERROR: Scheduler initialization failed");
        return -1;
    }
    boot_debug("Scheduler initialized.");
    return 0;
}

static void roulette_gatekeeper_task(void *arg) {
    (void)arg;

    /* Spin the wheel! */
    kernel_roulette();

    /* If we return, we won. Spawn the shell. */
    klog_printf(KLOG_INFO, "ROULETTE: You survived. Spawning shell...\n");

    /* Create shell task */
    uint32_t shell_task_id = task_create("shell", shell_main, NULL, 5, 0x02);
    if (shell_task_id == INVALID_TASK_ID) {
        kernel_panic("Failed to spawn shell after roulette win");
    }
    
    task_t *shell_task;
    if (task_get_info(shell_task_id, &shell_task) == 0) {
        if (schedule_task(shell_task) != 0) {
            task_terminate(shell_task_id);
            kernel_panic("Failed to schedule shell after roulette win");
        }
    }

    /* We are done. The shell is now running. */
    task_terminate(task_get_current_id());
    
    /* Should not be reached */
    while(1) { yield(); }
}

static int boot_step_roulette_task(void) {
    boot_debug("Creating roulette gatekeeper task...");
    uint32_t roulette_task_id = task_create("roulette", roulette_gatekeeper_task, NULL, 5, 0x02);
    if (roulette_task_id == INVALID_TASK_ID) {
        boot_info("ERROR: Failed to create roulette task");
        return -1;
    }

    task_t *roulette_task_info;
    if (task_get_info(roulette_task_id, &roulette_task_info) != 0) {
        boot_info("ERROR: Failed to get roulette task info");
        return -1;
    }

    if (schedule_task(roulette_task_info) != 0) {
        boot_info("ERROR: Failed to schedule roulette task");
        task_terminate(roulette_task_id);
        return -1;
    }

    boot_debug("Roulette task created and scheduled successfully!");
    return 0;
}

static int boot_step_idle_task(void) {
    boot_debug("Creating idle task...");
    if (create_idle_task() != 0) {
        boot_info("ERROR: Failed to create idle task");
        return -1;
    }
    boot_debug("Idle task ready.");
    return 0;
}

static int boot_step_mark_kernel_ready(void) {
    kernel_initialized = 1;
    boot_info("Kernel core services initialized.");
    splash_report_progress(95, "Boot complete");
    splash_finish();
    return 0;
}

BOOT_INIT_STEP(services, "ramfs", boot_step_ramfs_init);
BOOT_INIT_STEP(services, "task manager", boot_step_task_manager_init);
BOOT_INIT_STEP(services, "scheduler", boot_step_scheduler_init);
BOOT_INIT_STEP(services, "roulette task", boot_step_roulette_task);
BOOT_INIT_STEP(services, "idle task", boot_step_idle_task);
BOOT_INIT_STEP(services, "mark ready", boot_step_mark_kernel_ready);

/* Optional/demo phase ---------------------------------------------------- */

static int boot_step_framebuffer_demo(void) {
    klog_debug("Graphics demo: framebuffer already initialized");

    if (!framebuffer_is_initialized()) {
        klog_debug("WARNING: Framebuffer not available for demo");
        return 0;
    }

    framebuffer_info_t *fb_info = framebuffer_get_info();
    if (fb_info && fb_info->virtual_addr && fb_info->virtual_addr != (void*)fb_info->physical_addr) {
        if (klog_is_enabled(KLOG_DEBUG)) {
            klog_printf(KLOG_INFO, "Graphics: Framebuffer using translated virtual address 0x%lx (translation verified)\n",
                        (uint64_t)fb_info->virtual_addr);
        }
    }

    // Splash screen is already running - this step just validates graphics work
    klog_debug("Graphics demo: framebuffer validation complete");
    return 0;
}

BOOT_INIT_OPTIONAL_STEP(optional, "framebuffer demo", boot_step_framebuffer_demo);

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
    if (!boot_init_optional_enabled()) {
        boot_info("Optional graphics demo: skipped");
    }
    boot_info("Kernel initialization complete - ALL SYSTEMS OPERATIONAL!");

    /*
     * The Wheel of Fate is now handled by the roulette_gatekeeper_task
     * which runs as the first scheduled task.
     */
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
