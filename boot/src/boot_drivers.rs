use core::ffi::{c_char, CStr};

use slopos_lib::klog::{self, KlogLevel};
use slopos_lib::{klog_debug, klog_info};
use slopos_video as video;
use slopos_tests::{
    interrupt_suite_desc, tests_register_suite, tests_register_system_suites, tests_reset_registry,
    tests_run_all, InterruptTestConfig, InterruptTestVerbosity, TestRunSummary, TestSuiteResult,
};

use crate::early_init::{boot_get_cmdline, boot_init_priority};
use crate::kernel_panic::kernel_panic;
use crate::limine_protocol;

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

unsafe extern "C" {
    fn gdt_init();
    fn idt_init();
    fn safe_stack_init();
    fn idt_load();

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

fn serial_note(msg: &str) {
    slopos_drivers::serial::write_line(msg);
}

extern "C" fn boot_step_debug_subsystem_fn() {
    klog_debug!("Debug/logging subsystem initialized.");
}

extern "C" fn boot_step_gdt_setup_fn() {
    klog_debug!("Initializing GDT/TSS...");
    unsafe { gdt_init() };
    klog_debug!("GDT/TSS initialized.");
}

extern "C" fn boot_step_idt_setup_fn() {
    klog_debug!("Initializing IDT...");
    serial_note("boot: idt setup start");
    unsafe {
        idt_init();
        safe_stack_init();
        idt_load();
    }
    serial_note("boot: idt setup done");
    klog_debug!("IDT initialized and loaded.");
}

extern "C" fn boot_step_irq_setup_fn() {
    klog_debug!("Configuring IRQ dispatcher...");
    slopos_drivers::irq::init();
    let rc = unsafe { serial_enable_interrupts(COM1_BASE, SERIAL_COM1_IRQ) };
    if rc != 0 {
        klog_info!("WARNING: Failed to enable COM1 serial interrupts");
    } else {
        klog_debug!("COM1 serial interrupts armed.");
    }
    klog_debug!("IRQ dispatcher ready.");
}

extern "C" fn boot_step_timer_setup_fn() {
    klog_debug!("Initializing programmable interval timer...");
    unsafe { pit_init(PIT_DEFAULT_FREQUENCY_HZ) };
    klog_debug!("Programmable interval timer configured.");

    let ticks_before = slopos_drivers::irq::get_timer_ticks();
    unsafe { pit_poll_delay_ms(100) };
    let ticks_after = slopos_drivers::irq::get_timer_ticks();
    klog_info!(
        "BOOT: PIT ticks after 100ms poll: {} -> {}",
        ticks_before,
        ticks_after
    );
    if ticks_after == ticks_before {
        klog_info!("BOOT: WARNING - no PIT IRQs observed in 100ms window");
    }

    if unsafe { framebuffer_init() } != 0 {
        klog_info!(
            "WARNING: Limine framebuffer not available (will rely on alternative graphics initialization)"
        );
    }
    let fb = limine_protocol::boot_info().framebuffer;
    video::init(fb);
}

extern "C" fn boot_step_apic_setup_fn() {
    klog_debug!("Detecting Local APIC...");
    if unsafe { apic_detect() } == 0 {
        kernel_panic(
            b"SlopOS requires a Local APIC - legacy PIC is gone\0".as_ptr() as *const c_char,
        );
    }

    klog_debug!("Initializing Local APIC...");
    if unsafe { apic_init() } != 0 {
        kernel_panic(b"Local APIC initialization failed\0".as_ptr() as *const c_char);
    }

    unsafe { pic_quiesce_disable() };

    klog_debug!("Local APIC initialized (legacy PIC path removed).");
}

extern "C" fn boot_step_ioapic_setup_fn() {
    klog_debug!("Discovering IOAPIC controllers via ACPI MADT...");
    if unsafe { ioapic_init() } != 0 {
        kernel_panic(
            b"IOAPIC discovery failed - SlopOS cannot operate without it\0".as_ptr()
                as *const c_char,
        );
    }
    klog_debug!("IOAPIC: discovery complete, ready for redirection programming.");
}

extern "C" fn boot_step_pci_init_fn() {
    klog_debug!("Enumerating PCI devices...");
    unsafe {
        virtio_gpu_register_driver();
    }
    if unsafe { pci_init() } == 0 {
        klog_debug!("PCI subsystem initialized.");
        let gpu = unsafe { pci_get_primary_gpu() };
        if !gpu.is_null() {
            let info = unsafe { &*gpu };
            if info.present != 0 {
                klog_debug!(
                    "PCI: Primary GPU detected (bus {}, device {}, function {})",
                    info.device.bus,
                    info.device.device,
                    info.device.function
                );
                if !info.mmio_virt_base.is_null() {
                    klog_debug!(
                        "PCI: GPU MMIO virtual base {:#x}, size {:#x}",
                        info.mmio_virt_base as u64,
                        info.mmio_size
                    );
                } else {
                    klog_info!("PCI: WARNING GPU MMIO mapping unavailable");
                }
            } else {
                klog_debug!("PCI: No GPU-class device discovered during enumeration");
            }
        }
    } else {
        klog_info!("WARNING: PCI initialization failed");
    }
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
        klog_info!("INTERRUPT_TEST: No suites selected, skipping execution");
        test_config.enabled = 0;
        test_config.shutdown_on_complete = 0;
    }

    if test_config.enabled == 0 {
        klog_debug!("INTERRUPT_TEST: Harness disabled");
        return 0;
    }

    klog_info!("INTERRUPT_TEST: Running orchestrated harness");

    if klog::is_enabled_level(KlogLevel::Debug) {
        let suites = unsafe { interrupt_test_suite_string(test_config.suite_mask) };
        let suites_str = unsafe { CStr::from_ptr(suites) }
            .to_str()
            .unwrap_or("?");
        let verbosity = unsafe { interrupt_test_verbosity_string(test_config.verbosity) };
        let verbosity_str = unsafe { CStr::from_ptr(verbosity) }
            .to_str()
            .unwrap_or("?");
        klog_info!("INTERRUPT_TEST: Suites -> {}", suites_str);
        klog_info!("INTERRUPT_TEST: Verbosity -> {}", verbosity_str);
        klog_info!("INTERRUPT_TEST: Timeout (ms) -> {}", test_config.timeout_ms);
    }

    tests_reset_registry();
    tests_register_suite(&interrupt_suite_desc);
    tests_register_system_suites();

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

    let rc = tests_run_all(&test_config, &mut summary);

    if test_config.shutdown_on_complete != 0 {
        klog_debug!("INTERRUPT_TEST: Auto shutdown enabled after harness");
        unsafe { interrupt_test_request_shutdown(summary.failed as i32) };
    }

    if summary.failed > 0 {
        klog_info!("INTERRUPT_TEST: Failures detected");
    } else {
        klog_info!("INTERRUPT_TEST: Completed successfully");
    }

    rc
}

crate::boot_init_step_with_flags_unit!(
    BOOT_STEP_DEBUG_SUBSYSTEM,
    drivers,
    b"debug\0",
    boot_step_debug_subsystem_fn,
    boot_init_priority(10)
);
crate::boot_init_step_with_flags_unit!(
    BOOT_STEP_GDT_SETUP,
    drivers,
    b"gdt/tss\0",
    boot_step_gdt_setup_fn,
    boot_init_priority(20)
);
crate::boot_init_step_with_flags_unit!(
    BOOT_STEP_IDT_SETUP,
    drivers,
    b"idt\0",
    boot_step_idt_setup_fn,
    boot_init_priority(30)
);
crate::boot_init_step_with_flags_unit!(
    BOOT_STEP_APIC_SETUP,
    drivers,
    b"apic\0",
    boot_step_apic_setup_fn,
    boot_init_priority(40)
);
crate::boot_init_step_with_flags_unit!(
    BOOT_STEP_IOAPIC_SETUP,
    drivers,
    b"ioapic\0",
    boot_step_ioapic_setup_fn,
    boot_init_priority(50)
);
crate::boot_init_step_with_flags_unit!(
    BOOT_STEP_IRQ_SETUP,
    drivers,
    b"irq dispatcher\0",
    boot_step_irq_setup_fn,
    boot_init_priority(60)
);
crate::boot_init_step_with_flags_unit!(
    BOOT_STEP_TIMER_SETUP,
    drivers,
    b"timer\0",
    boot_step_timer_setup_fn,
    boot_init_priority(70)
);
crate::boot_init_step_with_flags_unit!(
    BOOT_STEP_PCI_INIT,
    drivers,
    b"pci\0",
    boot_step_pci_init_fn,
    boot_init_priority(80)
);
crate::boot_init_step_with_flags!(
    BOOT_STEP_INTERRUPT_TESTS,
    drivers,
    b"interrupt tests\0",
    boot_step_interrupt_tests_fn,
    boot_init_priority(90)
);
