/*
 * Core driver bring-up executed after memory is online.
 * Keeps boot-critical steps focused on required hardware only.
 */

#include <stdint.h>
#include "../boot/init.h"
#include "../boot/kernel_panic.h"
#include "../boot/safe_stack.h"
#include "../boot/gdt.h"
#include "../boot/idt.h"
#include "../drivers/serial.h"
#include "../drivers/pit.h"
#include "../drivers/irq.h"
#include "../drivers/apic.h"
#include "../drivers/ioapic.h"
#include "../drivers/pic_quiesce.h"
#include "../drivers/pci.h"
#include "../drivers/virtio_gpu.h"
#include "../drivers/interrupt_test.h"
#include "../drivers/interrupt_test_config.h"
#include "../video/framebuffer.h"
#include "../lib/klog.h"
#include "../lib/string.h"
#include "../tests/core.h"
#include "../tests/interrupt_suite.h"
#include "../tests/system_suites.h"

static int boot_step_debug_subsystem(void) {
    klog_debug("Debug/logging subsystem initialized.");
    return 0;
}

static int boot_step_gdt_setup(void) {
    klog_debug("Initializing GDT/TSS...");
    gdt_init();
    klog_debug("GDT/TSS initialized.");
    return 0;
}

static int boot_step_idt_setup(void) {
    klog_debug("Initializing IDT...");
    idt_init();
    safe_stack_init();
    idt_load();
    klog_debug("IDT initialized and loaded.");
    return 0;
}

static int boot_step_irq_setup(void) {
    klog_debug("Configuring IRQ dispatcher...");
    irq_init();
    if (serial_enable_interrupts(COM1_BASE, SERIAL_COM1_IRQ) != 0) {
        klog_info("WARNING: Failed to enable COM1 serial interrupts");
    } else {
        klog_debug("COM1 serial interrupts armed.");
    }
    klog_debug("IRQ dispatcher ready.");
    return 0;
}

static int boot_step_timer_setup(void) {
    klog_debug("Initializing programmable interval timer...");
    pit_init(PIT_DEFAULT_FREQUENCY_HZ);
    klog_debug("Programmable interval timer configured.");

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

    /* Framebuffer is optional - graphics stack may be initialized later (e.g., via Rust/virtio-gpu) */
    if (framebuffer_init() != 0) {
        klog_info("WARNING: Limine framebuffer not available (will rely on alternative graphics initialization)");
    }

    return 0;
}

static int boot_step_apic_setup(void) {
    klog_debug("Detecting Local APIC...");
    if (!apic_detect()) {
        kernel_panic("SlopOS requires a Local APIC - legacy PIC is gone");
    }

    klog_debug("Initializing Local APIC...");
    if (apic_init() != 0) {
        kernel_panic("Local APIC initialization failed");
    }

    pic_quiesce_disable();

    klog_debug("Local APIC initialized (legacy PIC path removed).");
    return 0;
}

static int boot_step_ioapic_setup(void) {
    klog_debug("Discovering IOAPIC controllers via ACPI MADT...");
    if (ioapic_init() != 0) {
        kernel_panic("IOAPIC discovery failed - SlopOS cannot operate without it");
    }
    klog_debug("IOAPIC: discovery complete, ready for redirection programming.");
    return 0;
}

static int boot_step_pci_init(void) {
    klog_debug("Enumerating PCI devices...");
    virtio_gpu_register_driver();
    if (pci_init() == 0) {
        klog_debug("PCI subsystem initialized");
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
            klog_debug("PCI: No GPU-class device discovered during enumeration");
        }
    } else {
        klog_info("WARNING: PCI initialization failed");
    }
    return 0;
}

static int boot_step_interrupt_tests(void) {
    struct interrupt_test_config test_config;
    interrupt_test_config_init_defaults(&test_config);

    const char *cmdline = boot_get_cmdline();
    if (cmdline) {
        interrupt_test_config_parse_cmdline(&test_config, cmdline);
    }

    if (test_config.enabled && test_config.suite_mask == 0) {
        klog_info("INTERRUPT_TEST: No suites selected, skipping execution");
        test_config.enabled = 0;
        test_config.shutdown_on_complete = 0;
    }

    if (!test_config.enabled) {
        klog_debug("INTERRUPT_TEST: Harness disabled");
        return 0;
    }

    klog_info("INTERRUPT_TEST: Running orchestrated harness");

    if (klog_is_enabled(KLOG_DEBUG)) {
        klog_printf(KLOG_INFO, "INTERRUPT_TEST: Suites -> %s\n",
                    interrupt_test_suite_string(test_config.suite_mask));

        klog_printf(KLOG_INFO, "INTERRUPT_TEST: Verbosity -> %s\n",
                    interrupt_test_verbosity_string(test_config.verbosity));

        klog_printf(KLOG_INFO, "INTERRUPT_TEST: Timeout (ms) -> %u\n", test_config.timeout_ms);
    }

    tests_reset_registry();
    tests_register_suite(&interrupt_suite_desc);
    tests_register_system_suites();

    struct test_run_summary summary = {0};
    int rc = tests_run_all(&test_config, &summary);

    if (test_config.shutdown_on_complete) {
        klog_debug("INTERRUPT_TEST: Auto shutdown enabled after harness");
        interrupt_test_request_shutdown((int)summary.failed);
    }

    if (summary.failed > 0) {
        klog_info("INTERRUPT_TEST: Failures detected");
    } else {
        klog_info("INTERRUPT_TEST: Completed successfully");
    }
    return rc;
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
