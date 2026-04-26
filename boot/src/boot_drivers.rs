use core::ffi::CStr;
#[cfg(feature = "xe-gpu")]
use core::ffi::c_char;

use slopos_testing::{
    TestRunSummary, tests_request_shutdown, tests_reset_panic_state, tests_run_all,
};
use slopos_utils::klog::{self, KlogLevel};
use slopos_utils::{klog_debug, klog_info};
use slopos_video as video;

use crate::early_init::{boot_get_cmdline, boot_init_priority};
use crate::idt::{idt_init, idt_load};
use crate::ist_stacks::ist_stacks_init;
use crate::limine_protocol;
use crate::smp::smp_init;
#[cfg(feature = "xe-gpu")]
use slopos_drivers::xe;
use slopos_drivers::{
    apic, hpet, ioapic,
    pci::{pci_get_primary_gpu, pci_init, pci_probe_drivers},
    pic::pic_quiesce_disable,
    virtio_blk::virtio_blk_register_driver,
    virtio_net::virtio_net_register_driver,
};
use slopos_mm::tlb;

fn sync_mouse_bounds(display: Option<slopos_abi::FramebufferData>) {
    let Some(display) = display else {
        return;
    };

    let width = display.info.width as i32;
    let height = display.info.height as i32;
    if width > 0 && height > 0 {
        slopos_drivers::mouse::set_bounds(width, height);
    }
}

fn serial_note(msg: &str) {
    slopos_drivers::serial::write_line(msg);
}

#[cfg(feature = "xe-gpu")]
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
    #[cfg(feature = "xe-gpu")]
    {
        let cmdline = boot_get_cmdline();
        if cmdline_contains(cmdline, "video=xe") {
            return video::VideoBackend::Xe;
        }
    }
    video::VideoBackend::Framebuffer
}

fn apply_serial_mirror_cmdline() {
    let cmdline = boot_get_cmdline();
    if cmdline.is_null() {
        return;
    }
    let Ok(cmdline) = (unsafe { CStr::from_ptr(cmdline) }).to_str() else {
        return;
    };
    if cmdline
        .split_ascii_whitespace()
        .any(|arg| arg == "serial_mirror=off")
    {
        slopos_drivers::tty::vconsole::set_serial_mirror(false);
    }
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
    slopos_drivers::tty::init();
    apply_serial_mirror_cmdline();
    // Register input cleanup so exec() and task termination tear down
    // keyboard/pointer focus and event queues for the old process image.
    slopos_core::task::register_task_resource_cleanup_hook(
        slopos_drivers::input_event::input_cleanup_task,
    );
    klog_debug!("IRQ dispatcher ready.");
}

fn boot_step_timer_setup_fn() {
    // HPET + LAPIC timer are mandatory (enforced at boot priorities 55–58).
    // The PIT hardware counter is left free-running for LAPIC calibration
    // fallback only — no PIT init, no PIT IRQs.

    // Verify the timer tick counter is advancing.  With LAPIC timer active
    // we expect ticks to arrive during a polled delay.
    let ticks_before = slopos_core::irq::get_timer_ticks();
    hpet::delay_ms(100);
    let ticks_after = slopos_core::irq::get_timer_ticks();
    klog_info!(
        "BOOT: Timer ticks after 100ms delay: {} -> {} (delta {})",
        ticks_before,
        ticks_after,
        ticks_after.wrapping_sub(ticks_before),
    );
    if ticks_after == ticks_before {
        klog_info!("BOOT: WARNING - no timer IRQs observed in 100ms window");
    }

    let boot_fb = limine_protocol::boot_info().framebuffer;
    if boot_fb.is_none() {
        klog_info!(
            "WARNING: Limine framebuffer not available (will rely on alternative graphics initialization)"
        );
    }
    let backend = boot_video_backend();
    #[cfg(feature = "xe-gpu")]
    if backend == video::VideoBackend::Xe {
        klog_info!("BOOT: deferring video init until PCI for GPU backend");
        return;
    }
    let fb = boot_fb.map(|bf| slopos_abi::FramebufferData {
        address: bf.address,
        info: bf.info,
    });
    video::init(fb, backend);
    if let (Some(base), Some(info)) = (
        video::framebuffer::get_fb_base_ptr(),
        video::framebuffer::get_display_info(),
    ) {
        slopos_drivers::tty::vconsole::register_framebuffer(
            base,
            info.pitch,
            info.width,
            info.height,
            info.bytes_per_pixel(),
        );
    }
    sync_mouse_bounds(fb);
}

fn boot_step_apic_setup_fn() {
    klog_debug!("Detecting Local APIC...");
    if !apic::detect() {
        panic!("SlopOS requires a Local APIC - legacy PIC is gone");
    }

    klog_debug!("Initializing Local APIC...");
    if apic::init() != 0 {
        panic!("Local APIC initialization failed");
    }

    pic_quiesce_disable();

    tlb::register_ipi_sender(apic::send_ipi_all_excluding_self);
    tlb::init();

    klog_debug!("Local APIC initialized (legacy PIC path removed).");
}

fn boot_step_xsave_setup_fn() {
    klog_debug!("Detecting XSAVE support...");
    let rc = slopos_arch::cpu::xsave::init();
    if rc != 0 {
        panic!("XSAVE initialization failed");
    }
    // Ensure the detected XSAVE area fits in our compile-time FpuState buffer.
    slopos_core::scheduler::task_struct::validate_fpu_state_size();
}

fn boot_step_smp_setup_fn() {
    klog_debug!("Discovering CPUs and starting APs...");
    smp_init();
}

fn boot_step_ioapic_setup_fn() {
    klog_debug!("Discovering IOAPIC controllers via ACPI MADT...");
    if ioapic::init() != 0 {
        panic!("IOAPIC discovery failed - SlopOS cannot operate without it");
    }
    klog_debug!("IOAPIC: discovery complete, ready for redirection programming.");
}

fn boot_step_hpet_setup_fn() {
    klog_debug!("Discovering HPET via ACPI...");
    if hpet::init() != 0 {
        panic!("SlopOS requires HPET — ACPI HPET table not found or hardware unavailable");
    }
    klog_debug!("HPET: Initialization complete, main counter running.");
}

fn boot_step_lapic_calibration_fn() {
    klog_debug!("Calibrating LAPIC timer...");
    let freq = apic::timer::calibrate();
    if freq == 0 {
        panic!("SlopOS requires a calibrated LAPIC timer — calibration returned 0 Hz");
    }
}

/// Scheduler preemption interval in milliseconds (100 Hz).
const LAPIC_TIMER_PERIOD_MS: u32 = 10;

fn boot_step_lapic_timer_start_fn() {
    use slopos_arch::arch::idt::LAPIC_TIMER_VECTOR;

    // Start BSP timer.
    if !apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, LAPIC_TIMER_PERIOD_MS) {
        panic!(
            "SlopOS requires LAPIC timer — set_periodic_ms failed (vector 0x{:x}, {}ms)",
            LAPIC_TIMER_VECTOR, LAPIC_TIMER_PERIOD_MS
        );
    }

    klog_info!(
        "BOOT: LAPIC timer started — vector 0x{:x}, period {}ms ({}Hz)",
        LAPIC_TIMER_VECTOR,
        LAPIC_TIMER_PERIOD_MS,
        1000 / LAPIC_TIMER_PERIOD_MS,
    );

    // Register a callback so APs can start their LAPIC timers from their
    // scheduler loops.  APs boot before calibration (SMP priority 45 <
    // HPET priority 55 < calibration priority 57), so they defer timer
    // start until the callback is registered here.
    fn ap_start_timer() -> bool {
        use slopos_arch::arch::idt::LAPIC_TIMER_VECTOR;
        apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, LAPIC_TIMER_PERIOD_MS)
    }
    slopos_core::scheduler::runtime::register_ap_timer_start(ap_start_timer);
}

fn boot_step_pci_init_fn() {
    // register the loopback device BEFORE any physical NIC so it
    // gets DevIndex(0) by convention.  This must happen before pci_init()
    // triggers VirtIO-net probe.
    slopos_net::loopback::init_loopback();

    klog_debug!("Enumerating PCI devices...");
    virtio_blk_register_driver();
    virtio_net_register_driver();
    pci_init();
    pci_probe_drivers();
    #[cfg(feature = "xe-gpu")]
    if boot_video_backend() == video::VideoBackend::Xe {
        xe::xe_probe();
    }

    klog_debug!("PCI subsystem initialized.");
    let gpu = pci_get_primary_gpu();
    if gpu.present != 0 {
        klog_debug!(
            "PCI: Primary GPU detected (bus {}, device {}, function {})",
            gpu.device.bus,
            gpu.device.device,
            gpu.device.function
        );
        if gpu.mmio_region.is_mapped() {
            klog_debug!(
                "PCI: GPU MMIO virtual base {:#x}, size {:#x}",
                gpu.mmio_region.virt_base(),
                gpu.mmio_size
            );
        } else {
            klog_info!("PCI: WARNING GPU MMIO mapping unavailable");
        }
    } else {
        klog_debug!("PCI: No GPU-class device discovered during enumeration");
    }

    #[cfg(feature = "xe-gpu")]
    {
        let backend = boot_video_backend();
        if backend == video::VideoBackend::Xe {
            let boot_fb = limine_protocol::boot_info().framebuffer;
            let fb = boot_fb.map(|bf| slopos_abi::FramebufferData {
                address: bf.address,
                info: bf.info,
            });
            let xe_fb = xe::xe_framebuffer_init(fb);
            video::init(xe_fb, backend);
            sync_mouse_bounds(xe_fb);
        }
    }
}

use slopos_testing::config_from_cmdline;

fn boot_step_run_tests_fn() -> i32 {
    // Parse command line to get test config
    let cmdline = boot_get_cmdline();
    let cmdline_str = if cmdline.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(cmdline) }.to_str().ok()
    };
    let test_config = config_from_cmdline(cmdline_str);

    if !test_config.enabled {
        klog_debug!("TESTS: Harness disabled");
        return 0;
    }

    klog_info!("TESTS: Running orchestrated harness");

    if klog::is_enabled_level(KlogLevel::Debug) {
        klog_info!("TESTS: Verbosity -> {}", test_config.verbosity);
        klog_info!("TESTS: Timeout (ms) -> {}", test_config.timeout_ms);
    }

    tests_reset_panic_state();

    use crate::ffi_boundary::{__start_test_registry, __stop_test_registry};
    let registry_start: *const slopos_testing::TestSuiteDesc = unsafe { &__start_test_registry };
    let registry_end: *const slopos_testing::TestSuiteDesc = unsafe { &__stop_test_registry };

    // Heap-box the summary: TestRunSummary is ~2.6 KiB and inlining it
    // keeps this function's stack frame above the gate even though the
    // entire test run is skipped on production builds.
    let mut summary_box = match slopos_alloc::KBox::<TestRunSummary>::zeroed() {
        Ok(b) => b,
        Err(_) => {
            klog_info!("TESTS: alloc failed");
            return -1;
        }
    };
    let summary: &mut TestRunSummary = &mut *summary_box;

    let rc = tests_run_all(&test_config, summary, registry_start, registry_end);

    // LUF (Lazy Unmap Flush) counters aggregated over all CPUs — proves
    // that cross-CPU coherence is actually flowing through the ring
    // and the drain IPI, not silently short-circuiting. Non-zero
    // `queued` on a fork-heavy or munmap-heavy run means the migration
    // is live; `reuse_drains` reflects how many times a frame was
    // reclaimed while still carrying a deferred entry.
    {
        let mut q = 0u64;
        let mut d = 0u64;
        let mut r = 0u64;
        let mut o = 0u64;
        for cpu in 0..slopos_arch::pcr::MAX_CPUS {
            q = q.saturating_add(slopos_mm::mmu::luf::queued_count(cpu));
            d = d.saturating_add(slopos_mm::mmu::luf::deferred_saves_count(cpu));
            r = r.saturating_add(slopos_mm::mmu::luf::reuse_drains_count(cpu));
            o = o.saturating_add(slopos_mm::mmu::luf::overflow_drains_count(cpu));
        }
        klog_info!(
            "LUF SUMMARY: queued={} reuse_drains={} overflow_drains={} deferred_saves={}",
            q,
            r,
            o,
            d,
        );
    }

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

crate::boot_init!(
    BOOT_STEP_IDT_SETUP,
    drivers,
    b"idt\0",
    boot_step_idt_setup_fn,
    flags = boot_init_priority(30)
);
crate::boot_init!(
    BOOT_STEP_APIC_SETUP,
    drivers,
    b"apic\0",
    boot_step_apic_setup_fn,
    flags = boot_init_priority(40)
);
crate::boot_init!(
    BOOT_STEP_XSAVE_SETUP,
    drivers,
    b"xsave\0",
    boot_step_xsave_setup_fn,
    flags = boot_init_priority(42)
);
crate::boot_init!(
    BOOT_STEP_SMP_SETUP,
    drivers,
    b"smp\0",
    boot_step_smp_setup_fn,
    flags = boot_init_priority(45)
);
crate::boot_init!(
    BOOT_STEP_IOAPIC_SETUP,
    drivers,
    b"ioapic\0",
    boot_step_ioapic_setup_fn,
    flags = boot_init_priority(50)
);
crate::boot_init!(
    BOOT_STEP_HPET_SETUP,
    drivers,
    b"hpet\0",
    boot_step_hpet_setup_fn,
    flags = boot_init_priority(55)
);
fn boot_step_csprng_seed_fn() -> i32 {
    use slopos_arch::cpu::rdrand;
    use slopos_arch::tsc;

    let mut seed = [0u8; 32];
    let mut source = "tsc";

    // Primary: RDRAND (available on all modern x86-64, emulated by QEMU)
    if rdrand::has_rdrand() {
        source = "rdrand";
        for chunk in seed.chunks_exact_mut(8) {
            if let Some(val) = rdrand::rdrand64() {
                chunk.copy_from_slice(&val.to_le_bytes());
            }
        }
        // Bonus: XOR in RDSEED if available
        if rdrand::has_rdseed() {
            source = "rdrand+rdseed";
            for chunk in seed.chunks_exact_mut(8) {
                if let Some(val) = rdrand::rdseed64() {
                    let existing = u64::from_le_bytes(chunk.try_into().unwrap());
                    let mixed = existing ^ val;
                    chunk.copy_from_slice(&mixed.to_le_bytes());
                }
            }
        }
    } else {
        // Fallback: TSC with mixing constants
        let mixing: [u64; 4] = [
            0x9E37_79B9_7F4A_7C15,
            0x6C62_272E_07BB_0142,
            0xBF58_476D_1CE4_E5B9,
            0x94D0_49BB_1331_11EB,
        ];
        for (i, chunk) in seed.chunks_exact_mut(8).enumerate() {
            let val = tsc::rdtsc().wrapping_mul(mixing[i]).wrapping_add(mixing[i]);
            chunk.copy_from_slice(&val.to_le_bytes());
        }
    }

    slopos_drivers::random::init_csprng(&seed);
    klog_info!("CSPRNG seeded from {source}");
    0
}

crate::boot_init!(
    BOOT_STEP_CSPRNG_SEED,
    drivers,
    b"csprng seed\0",
    boot_step_csprng_seed_fn,
    flags = boot_init_priority(56)
);
crate::boot_init!(
    BOOT_STEP_LAPIC_CALIBRATION,
    drivers,
    b"lapic timer calibration\0",
    boot_step_lapic_calibration_fn,
    flags = boot_init_priority(57)
);
crate::boot_init!(
    BOOT_STEP_LAPIC_TIMER_START,
    drivers,
    b"lapic timer start\0",
    boot_step_lapic_timer_start_fn,
    flags = boot_init_priority(58)
);
crate::boot_init!(
    BOOT_STEP_IRQ_SETUP,
    drivers,
    b"irq dispatcher\0",
    boot_step_irq_setup_fn,
    flags = boot_init_priority(60)
);
crate::boot_init!(
    BOOT_STEP_TIMER_SETUP,
    drivers,
    b"timer\0",
    boot_step_timer_setup_fn,
    flags = boot_init_priority(70)
);
crate::boot_init!(
    BOOT_STEP_PCI_INIT,
    drivers,
    b"pci\0",
    boot_step_pci_init_fn,
    flags = boot_init_priority(80)
);
crate::boot_init!(
    BOOT_STEP_RUN_TESTS,
    drivers,
    b"tests\0",
    boot_step_run_tests_fn,
    fallible,
    flags = boot_init_priority(90)
);
