use core::ffi::{c_char, CStr};

use slopos_lib::klog::{self, KlogLevel};
use slopos_lib::{klog_debug, klog_info};
use slopos_tests::{
    tests_register_suite, tests_register_system_suites, tests_request_shutdown,
    tests_reset_registry, tests_run_all, TestRunSummary, TestSuiteResult,
};
use slopos_video as video;

use crate::early_init::{boot_get_cmdline, boot_init_priority};
use crate::gdt::gdt_init;
use crate::idt::{idt_init, idt_load};
use crate::ist_stacks::ist_stacks_init;
use crate::limine_protocol;
use crate::smp::smp_init;
use slopos_drivers::{
    apic::{apic_detect, apic_init, send_ipi_all_excluding_self},
    interrupts::config_from_cmdline,
    ioapic::init,
    pci::{pci_get_primary_gpu, pci_init, pci_probe_drivers},
    pic::pic_quiesce_disable,
    pit::{pit_init, pit_poll_delay_ms},
    virtio_blk::virtio_blk_register_driver,
    virtio_gpu::virtio_gpu_register_driver,
    xe,
};
use slopos_mm::tlb;

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
    if cmdline_contains(cmdline, "video=xe") {
        video::VideoBackend::Xe
    } else if cmdline_contains(cmdline, "video=virgl") {
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
    ist_stacks_init();
    idt_load();
    serial_note("boot: idt setup done");
    klog_debug!("IDT initialized and loaded.");
}

fn boot_step_irq_setup_fn() {
    klog_debug!("Configuring IRQ dispatcher...");
    slopos_drivers::irq::init();
    klog_debug!("IRQ dispatcher ready.");
}

fn boot_step_timer_setup_fn() {
    klog_debug!("Initializing programmable interval timer...");
    pit_init(PIT_DEFAULT_FREQUENCY_HZ);
    klog_debug!("Programmable interval timer configured.");

    let ticks_before = slopos_core::irq::get_timer_ticks();
    pit_poll_delay_ms(100);
    let ticks_after = slopos_core::irq::get_timer_ticks();
    klog_info!(
        "BOOT: PIT ticks after 100ms poll: {} -> {}",
        ticks_before,
        ticks_after
    );
    if ticks_after == ticks_before {
        klog_info!("BOOT: WARNING - no PIT IRQs observed in 100ms window");
    }

    let boot_fb = limine_protocol::boot_info().framebuffer;
    if boot_fb.is_none() {
        klog_info!(
            "WARNING: Limine framebuffer not available (will rely on alternative graphics initialization)"
        );
    }
    let backend = boot_video_backend();
    if backend == video::VideoBackend::Virgl || backend == video::VideoBackend::Xe {
        klog_info!("BOOT: deferring video init until PCI for GPU backend");
        return;
    }
    let fb = boot_fb.map(|bf| slopos_abi::FramebufferData {
        address: bf.address,
        info: bf.info,
    });
    video::init(fb, backend);
}

fn boot_step_apic_setup_fn() {
    klog_debug!("Detecting Local APIC...");
    if apic_detect() == 0 {
        panic!("SlopOS requires a Local APIC - legacy PIC is gone");
    }

    klog_debug!("Initializing Local APIC...");
    if apic_init() != 0 {
        panic!("Local APIC initialization failed");
    }

    pic_quiesce_disable();

    tlb::register_ipi_sender(send_ipi_all_excluding_self);
    tlb::init();

    klog_debug!("Local APIC initialized (legacy PIC path removed).");
}

fn boot_step_smp_setup_fn() {
    klog_debug!("Discovering CPUs and starting APs...");
    smp_init();
}

fn boot_step_ioapic_setup_fn() {
    klog_debug!("Discovering IOAPIC controllers via ACPI MADT...");
    if init() != 0 {
        panic!("IOAPIC discovery failed - SlopOS cannot operate without it");
    }
    klog_debug!("IOAPIC: discovery complete, ready for redirection programming.");
}

fn boot_step_pci_init_fn() {
    klog_debug!("Enumerating PCI devices...");
    virtio_blk_register_driver();
    virtio_gpu_register_driver();
    pci_init();
    pci_probe_drivers();
    if boot_video_backend() == video::VideoBackend::Xe {
        xe::xe_probe();
    }

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

    let backend = boot_video_backend();
    if backend == video::VideoBackend::Virgl {
        let boot_fb = limine_protocol::boot_info().framebuffer;
        let fb = boot_fb.map(|bf| slopos_abi::FramebufferData {
            address: bf.address,
            info: bf.info,
        });
        video::init(fb, backend);
    } else if backend == video::VideoBackend::Xe {
        let boot_fb = limine_protocol::boot_info().framebuffer;
        let fb = boot_fb.map(|bf| slopos_abi::FramebufferData {
            address: bf.address,
            info: bf.info,
        });
        let xe_fb = xe::xe_framebuffer_init(fb);
        video::init(xe_fb, backend);
    }
}

use slopos_drivers::interrupts::SUITE_SCHEDULER;
use slopos_lib::{define_test_suite, register_test_suites};

use crate::gdt_tests::{
    test_current_cs_is_kernel, test_current_ss_is_kernel, test_data_segment_selectors,
    test_double_fault_uses_ist, test_efer_sce_enabled, test_gdt_double_init,
    test_gdt_entry_order_matches_selectors, test_gdt_loaded_valid_limit,
    test_gdt_set_ist_index_overflow, test_gdt_set_ist_index_zero, test_gdt_set_ist_valid_indices,
    test_gdt_set_kernel_rsp0_null, test_gdt_set_kernel_rsp0_user_address,
    test_gdt_set_kernel_rsp0_valid, test_gp_fault_handler_valid, test_ist_stacks_have_guard_pages,
    test_lstar_msr_valid, test_lstar_points_to_executable_code, test_page_fault_handler_valid,
    test_sfmask_msr_valid, test_star_msr_valid, test_star_sysret_selector_calculation,
    test_syscall_idt_entry, test_syscall_msr_double_init, test_tss_loaded,
    test_tss_rsp0_value_valid,
};

use crate::shutdown_tests::{
    test_acpi_pm1a_ports_defined, test_apic_availability_queryable, test_apic_enabled_queryable,
    test_com1_lsr_offset, test_com1_port_defined, test_double_scheduler_shutdown,
    test_kernel_page_directory_available, test_ps2_command_port_defined, test_qemu_debug_exit_port,
    test_rapid_shutdown_cycles, test_scheduler_reinit_after_shutdown,
    test_scheduler_shutdown_clears_state, test_scheduler_shutdown_disables,
    test_scheduler_shutdown_idempotent, test_serial_flush_terminates, test_shutdown_e2e_full_flow,
    test_shutdown_e2e_interrupt_state_preservation, test_shutdown_e2e_stress_with_allocation,
    test_shutdown_from_clean_state, test_shutdown_many_tasks, test_shutdown_mixed_priorities,
    test_shutdown_partial_init, test_shutdown_sequence_ordering, test_stateflag_concurrent_pattern,
    test_stateflag_enter_first_call, test_stateflag_enter_idempotent, test_stateflag_independence,
    test_stateflag_relaxed_access, test_stateflag_reset_and_reenter,
    test_stateflag_starts_inactive, test_stateflag_take_consumption, test_task_shutdown_all_empty,
    test_task_shutdown_all_idempotent, test_task_shutdown_all_terminates,
    test_task_shutdown_skips_current,
};

define_test_suite!(
    gdt,
    SUITE_SCHEDULER,
    [
        test_gdt_loaded_valid_limit,
        test_current_cs_is_kernel,
        test_current_ss_is_kernel,
        test_data_segment_selectors,
        test_tss_loaded,
        test_gdt_set_kernel_rsp0_valid,
        test_gdt_set_kernel_rsp0_null,
        test_gdt_set_kernel_rsp0_user_address,
        test_gdt_set_ist_valid_indices,
        test_gdt_set_ist_index_zero,
        test_gdt_set_ist_index_overflow,
        test_efer_sce_enabled,
        test_star_msr_valid,
        test_lstar_msr_valid,
        test_sfmask_msr_valid,
        test_double_fault_uses_ist,
        test_page_fault_handler_valid,
        test_gp_fault_handler_valid,
        test_syscall_idt_entry,
        test_gdt_double_init,
        test_syscall_msr_double_init,
        test_gdt_entry_order_matches_selectors,
        test_star_sysret_selector_calculation,
        test_tss_rsp0_value_valid,
        test_ist_stacks_have_guard_pages,
        test_lstar_points_to_executable_code,
    ]
);

define_test_suite!(
    shutdown,
    SUITE_SCHEDULER,
    [
        test_stateflag_starts_inactive,
        test_stateflag_enter_first_call,
        test_stateflag_enter_idempotent,
        test_stateflag_reset_and_reenter,
        test_stateflag_take_consumption,
        test_stateflag_independence,
        test_stateflag_relaxed_access,
        test_stateflag_concurrent_pattern,
        test_scheduler_shutdown_disables,
        test_scheduler_shutdown_idempotent,
        test_scheduler_shutdown_clears_state,
        test_double_scheduler_shutdown,
        test_scheduler_reinit_after_shutdown,
        test_task_shutdown_all_terminates,
        test_task_shutdown_all_empty,
        test_task_shutdown_all_idempotent,
        test_task_shutdown_skips_current,
        test_kernel_page_directory_available,
        test_apic_availability_queryable,
        test_apic_enabled_queryable,
        test_qemu_debug_exit_port,
        test_acpi_pm1a_ports_defined,
        test_ps2_command_port_defined,
        test_com1_port_defined,
        test_com1_lsr_offset,
        test_serial_flush_terminates,
        test_shutdown_sequence_ordering,
        test_shutdown_from_clean_state,
        test_shutdown_partial_init,
        test_rapid_shutdown_cycles,
        test_shutdown_many_tasks,
        test_shutdown_mixed_priorities,
        test_shutdown_e2e_full_flow,
        test_shutdown_e2e_stress_with_allocation,
        test_shutdown_e2e_interrupt_state_preservation,
    ]
);

fn register_boot_test_suites() {
    register_test_suites!(tests_register_suite, GDT_SUITE_DESC, SHUTDOWN_SUITE_DESC,);
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
    tests_register_system_suites();
    register_boot_test_suites();

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
        }; slopos_tests::TESTS_MAX_SUITES],
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
        klog_debug!("TESTS: Auto shutdown enabled after harness");
        tests_request_shutdown(summary.failed as i32);
    }

    if summary.failed > 0 {
        klog_info!("TESTS: Failures detected");
    } else {
        klog_info!("TESTS: Completed successfully");
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
    BOOT_STEP_SMP_SETUP,
    drivers,
    b"smp\0",
    boot_step_smp_setup_fn,
    boot_init_priority(45)
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
