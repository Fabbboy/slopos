use core::ffi::{CStr, c_char};

use slopos_lib::klog::{self, KlogLevel};
use slopos_lib::{klog_debug, klog_info};
use slopos_tests::{
    INTERRUPT_SUITE_DESC, TestRunSummary, TestSuiteResult, tests_register_suite,
    tests_register_system_suites, tests_reset_registry, tests_run_all,
};
use slopos_video as video;

use crate::early_init::{boot_get_cmdline, boot_init_priority};
use crate::gdt::gdt_init;
use crate::idt::{idt_init, idt_load};
use crate::kernel_panic::kernel_panic;
use crate::limine_protocol;
use crate::safe_stack::safe_stack_init;
use slopos_drivers::{
    apic::{apic_detect, apic_init},
    interrupt_test::interrupt_test_request_shutdown,
    interrupts::config_from_cmdline,
    ioapic::init,
    mouse::mouse_init,
    pci::{pci_get_primary_gpu, pci_init},
    pic::pic_quiesce_disable,
    pit::{pit_init, pit_poll_delay_ms},
    virtio_gpu::virtio_gpu_register_driver,
};

const PIT_DEFAULT_FREQUENCY_HZ: u32 = 100;

fn serial_note(msg: &str) {
    slopos_drivers::serial::write_line(msg);
}

fn cmdline_contains(cmdline: *const c_char, needle: &str) -> bool {
    if cmdline.is_null() {
        return false;
    }

    let haystack = unsafe { CStr::from_ptr(cmdline) }.to_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }

    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn boot_video_backend() -> video::VideoBackend {
    let cmdline = boot_get_cmdline();
    if cmdline_contains(cmdline, "video=virgl") {
        video::VideoBackend::Virgl
    } else {
        video::VideoBackend::Framebuffer
    }
}

fn boot_step_debug_subsystem_fn() {
    klog_debug!("Debug/logging subsystem initialized.");
}

fn boot_step_gdt_setup_fn() {
    klog_debug!("Initializing GDT/TSS...");
    gdt_init();
    klog_debug!("GDT/TSS initialized.");
}

fn boot_step_idt_setup_fn() {
    klog_debug!("Initializing IDT...");
    serial_note("boot: idt setup start");
    idt_init();
    safe_stack_init();
    idt_load();
    serial_note("boot: idt setup done");
    klog_debug!("IDT initialized and loaded.");
}

fn boot_step_irq_setup_fn() {
    klog_debug!("Configuring IRQ dispatcher...");
    slopos_drivers::irq::init();
    klog_debug!("IRQ dispatcher ready.");
}

fn boot_step_mouse_init_fn() {
    klog_debug!("Initializing PS/2 mouse...");
    mouse_init();
    klog_debug!("PS/2 mouse initialized.");
}

fn boot_step_timer_setup_fn() {
    klog_debug!("Initializing programmable interval timer...");
    pit_init(PIT_DEFAULT_FREQUENCY_HZ);
    klog_debug!("Programmable interval timer configured.");

    let ticks_before = slopos_drivers::irq::get_timer_ticks();
    pit_poll_delay_ms(100);
    let ticks_after = slopos_drivers::irq::get_timer_ticks();
    klog_info!(
        "BOOT: PIT ticks after 100ms poll: {} -> {}",
        ticks_before,
        ticks_after
    );
    if ticks_after == ticks_before {
        klog_info!("BOOT: WARNING - no PIT IRQs observed in 100ms window");
    }

    let fb = limine_protocol::boot_info().framebuffer;
    if fb.is_none() {
        klog_info!(
            "WARNING: Limine framebuffer not available (will rely on alternative graphics initialization)"
        );
    }
    let backend = boot_video_backend();
    if backend == video::VideoBackend::Virgl {
        klog_info!("BOOT: deferring video init until PCI for virgl");
        return;
    }
    video::init(fb, backend);
}

fn boot_step_apic_setup_fn() {
    klog_debug!("Detecting Local APIC...");
    if apic_detect() == 0 {
        kernel_panic(
            b"SlopOS requires a Local APIC - legacy PIC is gone\0".as_ptr() as *const c_char,
        );
    }

    klog_debug!("Initializing Local APIC...");
    if apic_init() != 0 {
        kernel_panic(b"Local APIC initialization failed\0".as_ptr() as *const c_char);
    }

    pic_quiesce_disable();

    klog_debug!("Local APIC initialized (legacy PIC path removed).");
}

fn boot_step_ioapic_setup_fn() {
    klog_debug!("Discovering IOAPIC controllers via ACPI MADT...");
    if init() != 0 {
        kernel_panic(
            b"IOAPIC discovery failed - SlopOS cannot operate without it\0".as_ptr()
                as *const c_char,
        );
    }
    klog_debug!("IOAPIC: discovery complete, ready for redirection programming.");
}

fn boot_step_pci_init_fn() {
    klog_debug!("Enumerating PCI devices...");
    virtio_gpu_register_driver();
    if pci_init() == 0 {
        klog_debug!("PCI subsystem initialized.");
        let gpu = pci_get_primary_gpu();
        if !gpu.is_null() {
            let info = unsafe { &*gpu };
            if info.present != 0 {
                klog_debug!(
                    "PCI: Primary GPU detected (bus {}, device {}, function {})",
                    info.device.bus,
                    info.device.device,
                    info.device.function
                );
                if info.mmio_region.is_mapped() {
                    klog_debug!(
                        "PCI: GPU MMIO virtual base {:#x}, size {:#x}",
                        info.mmio_region.virt_base(),
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

    let backend = boot_video_backend();
    if backend == video::VideoBackend::Virgl {
        let fb = limine_protocol::boot_info().framebuffer;
        video::init(fb, backend);
    }
}

fn boot_step_interrupt_tests_fn() -> i32 {
    // Parse command line to get test config
    let cmdline = boot_get_cmdline();
    let cmdline_str = if cmdline.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(cmdline) }.to_str().ok()
    };
    let mut test_config = config_from_cmdline(cmdline_str);

    if test_config.enabled && test_config.suite_mask == 0 {
        klog_info!("INTERRUPT_TEST: No suites selected, skipping execution");
        test_config.enabled = false;
        test_config.shutdown = false;
    }

    if !test_config.enabled {
        klog_debug!("INTERRUPT_TEST: Harness disabled");
        return 0;
    }

    klog_info!("INTERRUPT_TEST: Running orchestrated harness");

    if klog::is_enabled_level(KlogLevel::Debug) {
        klog_info!("INTERRUPT_TEST: Suites -> {}", test_config.suite());
        klog_info!("INTERRUPT_TEST: Verbosity -> {}", test_config.verbosity);
        klog_info!("INTERRUPT_TEST: Timeout (ms) -> {}", test_config.timeout_ms);
    }

    tests_reset_registry();
    tests_register_suite(&INTERRUPT_SUITE_DESC);
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

    if test_config.shutdown {
        klog_debug!("INTERRUPT_TEST: Auto shutdown enabled after harness");
        interrupt_test_request_shutdown(summary.failed as i32);
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
    BOOT_STEP_MOUSE_INIT,
    drivers,
    b"mouse\0",
    boot_step_mouse_init_fn,
    boot_init_priority(65)
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
