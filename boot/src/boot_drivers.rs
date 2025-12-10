use core::ffi::c_char;

use slopos_lib::{klog_is_enabled, klog_printf, KlogLevel};
use slopos_tests::{
    interrupt_suite_desc, tests_register_suite, tests_register_system_suites, tests_reset_registry,
    tests_run_all, InterruptTestConfig, InterruptTestVerbosity, TestRunSummary, TestSuiteResult,
};

use crate::early_init::{boot_get_cmdline, boot_init_priority, BootInitStep};
use crate::kernel_panic::kernel_panic;

const COM1_BASE: u16 = 0x3F8;
const SERIAL_COM1_IRQ: u8 = 4;
const PIT_DEFAULT_FREQUENCY_HZ: u32 = 100;

#[repr(C)]
struct PciBarInfo {
    base: u64,
    size: u64,
    is_io: u8,
    is_64bit: u8,
    prefetchable: u8,
}

#[repr(C)]
struct PciDeviceInfo {
    bus: u8,
    device: u8,
    function: u8,
    vendor_id: u16,
    device_id: u16,
    class_code: u8,
    subclass: u8,
    prog_if: u8,
    revision: u8,
    header_type: u8,
    irq_line: u8,
    irq_pin: u8,
    bar_count: u8,
    bars: [PciBarInfo; 6],
}

#[repr(C)]
struct PciGpuInfo {
    present: i32,
    device: PciDeviceInfo,
    mmio_phys_base: u64,
    mmio_virt_base: *mut core::ffi::c_void,
    mmio_size: u64,
}

extern "C" {
    fn gdt_init();
    fn idt_init();
    fn safe_stack_init();
    fn idt_load();

    fn irq_init();
    fn irq_get_timer_ticks() -> u64;

    fn serial_enable_interrupts(port: u16, irq: u8) -> i32;

    fn pit_init(freq: u32);
    fn pit_poll_delay_ms(ms: u32);

    fn framebuffer_init() -> i32;

    fn apic_detect() -> i32;
    fn apic_init() -> i32;
    fn pic_quiesce_disable();

    fn ioapic_init() -> i32;

    fn virtio_gpu_register_driver();
    fn pci_init() -> i32;
    fn pci_get_primary_gpu() -> *const PciGpuInfo;

    fn interrupt_test_config_init_defaults(cfg: *mut InterruptTestConfig);
    fn interrupt_test_config_parse_cmdline(cfg: *mut InterruptTestConfig, cmdline: *const c_char);
    fn interrupt_test_suite_string(mask: u32) -> *const c_char;
    fn interrupt_test_verbosity_string(verbosity: InterruptTestVerbosity) -> *const c_char;

    fn interrupt_test_request_shutdown(failed: i32);
}

fn log(level: KlogLevel, msg: &[u8]) {
    unsafe { klog_printf(level, msg.as_ptr() as *const c_char) };
}

fn log_info(msg: &[u8]) {
    log(KlogLevel::Info, msg);
}

fn log_debug(msg: &[u8]) {
    log(KlogLevel::Debug, msg);
}

extern "C" fn boot_step_debug_subsystem_fn() -> i32 {
    log_debug(b"Debug/logging subsystem initialized.\0");
    0
}

extern "C" fn boot_step_gdt_setup_fn() -> i32 {
    log_debug(b"Initializing GDT/TSS...\0");
    unsafe { gdt_init() };
    log_debug(b"GDT/TSS initialized.\0");
    0
}

extern "C" fn boot_step_idt_setup_fn() -> i32 {
    log_debug(b"Initializing IDT...\0");
    unsafe {
        idt_init();
        safe_stack_init();
        idt_load();
    }
    log_debug(b"IDT initialized and loaded.\0");
    0
}

extern "C" fn boot_step_irq_setup_fn() -> i32 {
    log_debug(b"Configuring IRQ dispatcher...\0");
    unsafe { irq_init() };
    let rc = unsafe { serial_enable_interrupts(COM1_BASE, SERIAL_COM1_IRQ) };
    if rc != 0 {
        log_info(b"WARNING: Failed to enable COM1 serial interrupts\0");
    } else {
        log_debug(b"COM1 serial interrupts armed.\0");
    }
    log_debug(b"IRQ dispatcher ready.\0");
    0
}

extern "C" fn boot_step_timer_setup_fn() -> i32 {
    log_debug(b"Initializing programmable interval timer...\0");
    unsafe { pit_init(PIT_DEFAULT_FREQUENCY_HZ) };
    log_debug(b"Programmable interval timer configured.\0");

    let ticks_before = unsafe { irq_get_timer_ticks() };
    unsafe { pit_poll_delay_ms(100) };
    let ticks_after = unsafe { irq_get_timer_ticks() };
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"BOOT: PIT ticks after 100ms poll: %llu -> %llu\n\0".as_ptr() as *const c_char,
            ticks_before,
            ticks_after,
        );
    }
    if ticks_after == ticks_before {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"BOOT: WARNING - no PIT IRQs observed in 100ms window\n\0".as_ptr()
                    as *const c_char,
            );
        }
    }

    if unsafe { framebuffer_init() } != 0 {
        log_info(
            b"WARNING: Limine framebuffer not available (will rely on alternative graphics initialization)\0",
        );
    }

    0
}

extern "C" fn boot_step_apic_setup_fn() -> i32 {
    log_debug(b"Detecting Local APIC...\0");
    if unsafe { apic_detect() } == 0 {
        kernel_panic(
            b"SlopOS requires a Local APIC - legacy PIC is gone\0".as_ptr() as *const c_char,
        );
    }

    log_debug(b"Initializing Local APIC...\0");
    if unsafe { apic_init() } != 0 {
        kernel_panic(b"Local APIC initialization failed\0".as_ptr() as *const c_char);
    }

    unsafe { pic_quiesce_disable() };

    log_debug(b"Local APIC initialized (legacy PIC path removed).\0");
    0
}

extern "C" fn boot_step_ioapic_setup_fn() -> i32 {
    log_debug(b"Discovering IOAPIC controllers via ACPI MADT...\0");
    if unsafe { ioapic_init() } != 0 {
        kernel_panic(
            b"IOAPIC discovery failed - SlopOS cannot operate without it\0".as_ptr()
                as *const c_char,
        );
    }
    log_debug(b"IOAPIC: discovery complete, ready for redirection programming.\0");
    0
}

extern "C" fn boot_step_pci_init_fn() -> i32 {
    log_debug(b"Enumerating PCI devices...\0");
    unsafe {
        virtio_gpu_register_driver();
    }
    if unsafe { pci_init() } == 0 {
        log_debug(b"PCI subsystem initialized.\0");
        let gpu = unsafe { pci_get_primary_gpu() };
        if !gpu.is_null() {
            let info = unsafe { &*gpu };
            if info.present != 0 {
                unsafe {
                    klog_printf(
                        KlogLevel::Debug,
                        b"PCI: Primary GPU detected (bus %u, device %u, function %u)\n\0".as_ptr()
                            as *const c_char,
                        info.device.bus as u32,
                        info.device.device as u32,
                        info.device.function as u32,
                    );
                }
                if !info.mmio_virt_base.is_null() {
                    unsafe {
                        klog_printf(
                            KlogLevel::Debug,
                            b"PCI: GPU MMIO virtual base 0x%llx, size 0x%llx\n\0".as_ptr()
                                as *const c_char,
                            info.mmio_virt_base as u64,
                            info.mmio_size,
                        );
                    }
                } else {
                    log_info(b"PCI: WARNING GPU MMIO mapping unavailable\0");
                }
            } else {
                log_debug(b"PCI: No GPU-class device discovered during enumeration\0");
            }
        }
    } else {
        log_info(b"WARNING: PCI initialization failed\0");
    }
    0
}

extern "C" fn boot_step_interrupt_tests_fn() -> i32 {
    let mut test_config = InterruptTestConfig {
        enabled: 0,
        verbosity: InterruptTestVerbosity::INTERRUPT_TEST_VERBOSITY_SUMMARY,
        suite_mask: 0,
        timeout_ms: 0,
        shutdown_on_complete: 0,
        stacktrace_demo: 0,
    };

    unsafe {
        interrupt_test_config_init_defaults(&mut test_config);
    }

    let cmdline = boot_get_cmdline();
    if !cmdline.is_null() {
        unsafe {
            interrupt_test_config_parse_cmdline(&mut test_config, cmdline);
        }
    }

    if test_config.enabled != 0 && test_config.suite_mask == 0 {
        log_info(b"INTERRUPT_TEST: No suites selected, skipping execution\0");
        test_config.enabled = 0;
        test_config.shutdown_on_complete = 0;
    }

    if test_config.enabled == 0 {
        log_debug(b"INTERRUPT_TEST: Harness disabled\0");
        return 0;
    }

    log_info(b"INTERRUPT_TEST: Running orchestrated harness\0");

    if unsafe { klog_is_enabled(KlogLevel::Debug) } != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"INTERRUPT_TEST: Suites -> %s\n\0".as_ptr() as *const c_char,
                interrupt_test_suite_string(test_config.suite_mask),
            );
            klog_printf(
                KlogLevel::Info,
                b"INTERRUPT_TEST: Verbosity -> %s\n\0".as_ptr() as *const c_char,
                interrupt_test_verbosity_string(test_config.verbosity),
            );
            klog_printf(
                KlogLevel::Info,
                b"INTERRUPT_TEST: Timeout (ms) -> %u\n\0".as_ptr() as *const c_char,
                test_config.timeout_ms,
            );
        }
    }

    unsafe {
        tests_reset_registry();
        tests_register_suite(&interrupt_suite_desc);
        tests_register_system_suites();
    }

    let mut summary = TestRunSummary {
        suites: [TestSuiteResult {
            name: core::ptr::null(),
            total: 0,
            passed: 0,
            failed: 0,
            exceptions_caught: 0,
            unexpected_exceptions: 0,
            elapsed_ms: 0,
            timed_out: 0,
        }; 8],
        suite_count: 0,
        total_tests: 0,
        passed: 0,
        failed: 0,
        exceptions_caught: 0,
        unexpected_exceptions: 0,
        elapsed_ms: 0,
        timed_out: 0,
    };

    let rc = unsafe { tests_run_all(&test_config, &mut summary) };

    if test_config.shutdown_on_complete != 0 {
        log_debug(b"INTERRUPT_TEST: Auto shutdown enabled after harness\0");
        unsafe { interrupt_test_request_shutdown(summary.failed as i32) };
    }

    if summary.failed > 0 {
        log_info(b"INTERRUPT_TEST: Failures detected\0");
    } else {
        log_info(b"INTERRUPT_TEST: Completed successfully\0");
    }

    rc
}

#[used]
#[link_section = ".boot_init_drivers"]
static BOOT_STEP_DEBUG_SUBSYSTEM: BootInitStep =
    BootInitStep::new(b"debug\0", boot_step_debug_subsystem_fn, 0);
#[used]
#[link_section = ".boot_init_drivers"]
static BOOT_STEP_GDT_SETUP: BootInitStep =
    BootInitStep::new(b"gdt/tss\0", boot_step_gdt_setup_fn, 0);
#[used]
#[link_section = ".boot_init_drivers"]
static BOOT_STEP_IDT_SETUP: BootInitStep =
    BootInitStep::new(b"idt\0", boot_step_idt_setup_fn, 0);
#[used]
#[link_section = ".boot_init_drivers"]
static BOOT_STEP_APIC_SETUP: BootInitStep =
    BootInitStep::new(b"apic\0", boot_step_apic_setup_fn, 0);
#[used]
#[link_section = ".boot_init_drivers"]
static BOOT_STEP_IOAPIC_SETUP: BootInitStep =
    BootInitStep::new(b"ioapic\0", boot_step_ioapic_setup_fn, 0);
#[used]
#[link_section = ".boot_init_drivers"]
static BOOT_STEP_IRQ_SETUP: BootInitStep =
    BootInitStep::new(b"irq dispatcher\0", boot_step_irq_setup_fn, 0);
#[used]
#[link_section = ".boot_init_drivers"]
static BOOT_STEP_TIMER_SETUP: BootInitStep =
    BootInitStep::new(b"timer\0", boot_step_timer_setup_fn, 0);
#[used]
#[link_section = ".boot_init_drivers"]
static BOOT_STEP_PCI_INIT: BootInitStep =
    BootInitStep::new(b"pci\0", boot_step_pci_init_fn, 0);
#[used]
#[link_section = ".boot_init_drivers"]
static BOOT_STEP_INTERRUPT_TESTS: BootInitStep =
    BootInitStep::new(b"interrupt tests\0", boot_step_interrupt_tests_fn, 0);
