use slopos_hermetic::{BootCtx, BspInit};
use slopos_ostd::klog::{self, KlogLevel};
use slopos_ostd::{klog_debug, klog_info};
use slopos_testing::{
    TestRunSummary, kernel_phase_summary, tests_reset_panic_state, tests_run_all,
};
use slopos_video as video;

use crate::early_init::{boot_get_cmdline, boot_init_priority};
use crate::idt::{idt_init, idt_load};
use crate::ist_stacks::ist_stacks_init;
use crate::limine_protocol;
use crate::smp::smp_init;
use slopos_acpi::madt::Madt;
use slopos_acpi::tables::AcpiTables;
use slopos_drivers::{
    apic, hpet, ioapic,
    pci::{pci_init, pci_probe_drivers},
};
use slopos_kernel_services::platform;
use slopos_kernel_services::syscall_services::scanout;
use slopos_mm::tlb;

fn serial_note(msg: &str) {
    slopos_drivers::serial::write_line(msg);
}

/// Lowest rank by default, so any GPU driver wins the display. `video=framebuffer`
/// lifts it above every GPU instead, keeping the passive firmware framebuffer up
/// without having to gate a single driver's `matches`.
fn boot_firmware_priority() -> i32 {
    if let Some(cmdline) = slopos_ostd::util::cstr::cstr_from_kernel_ptr_str(boot_get_cmdline()) {
        if cmdline.contains("video=framebuffer") {
            return scanout::PRIO_FIRMWARE_FB + scanout::PRIO_CMDLINE_HINT_BUMP;
        }
    }
    scanout::PRIO_FIRMWARE_FB
}

fn apply_serial_mirror_cmdline() {
    let Some(cmdline) = slopos_ostd::util::cstr::cstr_from_kernel_ptr_str(boot_get_cmdline())
    else {
        return;
    };
    if cmdline
        .split_ascii_whitespace()
        .any(|arg| arg == "serial_mirror=off")
    {
        slopos_drivers::tty::vconsole::set_serial_mirror(false);
    }
}

fn boot_step_idt_setup_fn(ctx: &mut BootCtx<'_, BspInit>) {
    klog_debug!("Initializing IDT...");
    serial_note("boot: idt setup start");
    idt_init(&ctx.bsp_token());
    ist_stacks_init(ctx);
    idt_load(&ctx.bsp_token());
    serial_note("boot: idt setup done");
    klog_debug!("IDT initialized and loaded.");
}

fn boot_step_irq_setup_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    klog_debug!("Configuring IRQ dispatcher...");
    // `i8042.legacy` selects the hardcoded PS/2 bring-up over the platform-bus
    // binding; must be set before `irq::init` so both paths see one value.
    let legacy_i8042 = slopos_ostd::util::cstr::cstr_from_kernel_ptr_str(boot_get_cmdline())
        .map(|s| s.contains("i8042.legacy"))
        .unwrap_or(false);
    slopos_drivers::ps2::set_legacy_mode(legacy_i8042);
    slopos_drivers::irq::init();
    slopos_drivers::tty::init();
    apply_serial_mirror_cmdline();
    slopos_sched::task::register_task_resource_cleanup_hook(
        slopos_drivers::input_event::input_cleanup_task,
    );
    // Seat release is arbiter revocation, never the holder descriptor's `Drop`:
    // a reference cycle among holders would wedge the display with no way back.
    // The hook also runs before `exec`, so a compositor that execs something
    // else hands the screen back rather than keeping it across the image swap.
    slopos_sched::task::register_task_resource_cleanup_hook(slopos_ostd::seat::revoke_for_task);
    klog_debug!("IRQ dispatcher ready.");
}

fn boot_step_timer_setup_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    // HPET + LAPIC timer are mandatory. The PIT counter is left free-running
    // for LAPIC calibration fallback only — no PIT init, no PIT IRQs.
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
    let fb = boot_fb.map(|bf| slopos_abi::FramebufferData {
        address: *bf.address,
        info: bf.info,
    });
    // The default scanout provider; GPU drivers take it over through the
    // arbiter during `pci_probe_drivers`.
    video::init(fb, boot_firmware_priority());
}

fn boot_step_apic_setup_fn(ctx: &mut BootCtx<'_, BspInit>) {
    klog_debug!("Detecting Local APIC...");
    if !apic::detect() {
        panic!("SlopOS requires a Local APIC - legacy PIC is gone");
    }

    let token = ctx.bsp_token();

    if platform::is_rsdp_available() {
        match AcpiTables::from_phys(platform::get_rsdp_phys()).and_then(|tables| {
            Madt::from_tables(&tables).map(|madt| madt.has_pcat_compat_dual_8259())
        }) {
            Some(true) => {
                slopos_ostd::io::init_and_disable_legacy_8259(&token);
                klog_info!("PIC: ACPI PCAT_COMPAT set; legacy dual 8259 initialized and masked");
            }
            Some(false) => {
                klog_debug!("PIC: ACPI PCAT_COMPAT clear; no legacy 8259 disable needed");
            }
            None => {
                klog_info!(
                    "PIC: MADT unavailable during APIC setup; IOAPIC setup will enforce APIC platform"
                );
            }
        }
    } else {
        klog_info!(
            "PIC: RSDP unavailable during APIC setup; IOAPIC setup will enforce APIC platform"
        );
    }

    klog_debug!("Initializing Local APIC...");
    if apic::init(&token) != 0 {
        panic!("Local APIC initialization failed");
    }

    tlb::register_ipi_sender(apic::send_ipi_all_excluding_self);
    tlb::init();

    klog_debug!("Local APIC initialized (legacy PIC path removed).");
}

fn boot_step_xsave_setup_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    klog_debug!("Detecting XSAVE support...");
    let rc = slopos_arch::cpu::xsave::init();
    if rc != 0 {
        panic!("XSAVE initialization failed");
    }
    slopos_sched::task_struct::validate_fpu_state_size();
}

fn boot_step_smp_setup_fn(ctx: &mut BootCtx<'_, BspInit>) {
    klog_debug!("Discovering CPUs and starting APs...");
    smp_init(ctx);
}

fn boot_step_ioapic_setup_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    klog_debug!("Discovering IOAPIC controllers via ACPI MADT...");
    if ioapic::init() != 0 {
        panic!("IOAPIC discovery failed - SlopOS cannot operate without it");
    }
    klog_debug!("IOAPIC: discovery complete, ready for redirection programming.");
}

fn boot_step_hpet_setup_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    klog_debug!("Discovering HPET via ACPI...");
    if hpet::init() != 0 {
        panic!("SlopOS requires HPET — ACPI HPET table not found or hardware unavailable");
    }
    klog_debug!("HPET: Initialization complete, main counter running.");
}

fn boot_step_lapic_calibration_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    klog_debug!("Calibrating LAPIC timer...");
    let freq = apic::timer::calibrate();
    if freq == 0 {
        panic!("SlopOS requires a calibrated LAPIC timer — calibration returned 0 Hz");
    }
}

/// Scheduler preemption interval in milliseconds (100 Hz).
const LAPIC_TIMER_PERIOD_MS: u32 = 10;

fn boot_step_lapic_timer_start_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    use slopos_arch::arch::idt::LAPIC_TIMER_VECTOR;

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

    // The quarantine needs a tick to ack a quiesce epoch; before this point
    // freeing goes straight back to the free lists rather than parking memory
    // nothing can release.
    slopos_mm::mmu::quiesce::activate();

    // APs boot before calibration, so they defer their own timer start until
    // this callback exists.
    fn ap_start_timer() -> bool {
        use slopos_arch::arch::idt::LAPIC_TIMER_VECTOR;
        apic::timer::set_periodic_ms(LAPIC_TIMER_VECTOR, LAPIC_TIMER_PERIOD_MS)
    }
    slopos_sched::runtime::register_ap_timer_start(ap_start_timer);
}

fn boot_step_register_spawner_fn(ctx: &mut BootCtx<'_, BspInit>) {
    // PCI probes spawn long-lived service threads through OSTD's spawn facade,
    // so the sched-side spawner has to be wired before that phase.
    slopos_ostd::task::register_kernel_thread_spawner(
        &ctx.bsp_token(),
        slopos_sched::runtime::kernel_thread_spawner_handle(),
    );
    klog_debug!("OSTD: kernel-thread spawner registered (sched/runtime)");

    // Installed alongside the spawner so any later `spawn_kernel_io!` already
    // has a working `yield_with_deadline` path.
    slopos_ostd::sync::kernel_io_task::register_yield_backend(
        &ctx.bsp_token(),
        slopos_sched::runtime::kernel_io_yield_backend(),
    );
    klog_debug!("OSTD: KernelIoToken yield backend registered (sched/runtime)");

    // Not armed earlier because the drain frees to the allocator and walks the
    // task registry, neither of which a half-built kernel can survive.
    slopos_sched::runtime::arm_bottom_half(&ctx.bsp_token());
    klog_debug!("OSTD: bottom-half point armed (sched/runtime)");
}

fn boot_step_identity_dma_fn(ctx: &mut BootCtx<'_, BspInit>) {
    // Must precede any driver's DMA probe so `DmaCoherent`/`DmaStream` have a
    // live mapper.
    slopos_ostd::mm::register_identity_dma_mapper(&ctx.bsp_token());
    klog_debug!("OSTD: identity DMA mapper registered (IOVA == phys)");
}

fn boot_step_pci_init_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    // `tp.off` disables the LPSS I²C probe and touchpad bring-up; it has to be
    // honoured before driver probe runs.
    let tp_off = slopos_ostd::util::cstr::cstr_from_kernel_ptr_str(boot_get_cmdline())
        .map(|s| s.contains("tp.off"))
        .unwrap_or(false);
    slopos_drivers::i2c::set_lpss_disabled(tp_off);

    // Before `pci_init()` triggers the VirtIO-net probe, so loopback takes
    // DevIndex(0) by convention.
    slopos_net::loopback::init_loopback();

    // The filter chain must be published before any NIC delivers a packet.
    slopos_net::xdp::init();

    klog_debug!("Enumerating PCI devices...");
    pci_init();

    // The xe driver must see its configuration before its probe runs; with no
    // `xe.*` knobs it stays passive on the firmware framebuffer.
    let xe_cmdline =
        slopos_ostd::util::cstr::cstr_from_kernel_ptr_str(boot_get_cmdline()).unwrap_or("");
    slopos_drivers::xe::set_config(slopos_drivers::xe_logic::cmdline::parse(xe_cmdline));

    pci_probe_drivers();
    klog_debug!("PCI subsystem initialized.");

    // Non-PCI ACPI platform devices (the i8042 at PNP0303, the I²C-HID touchpad
    // at PNP0C50), after PCI so any controller dependencies are already claimed.
    let rsdp_phys = limine_protocol::get_rsdp_phys_address();
    if rsdp_phys != 0 {
        let pdbg = slopos_ostd::util::cstr::cstr_from_kernel_ptr_str(boot_get_cmdline())
            .map(|s| s.contains("platform.debug"))
            .unwrap_or(false);
        install_touchpad_config();
        slopos_drivers::platform_bus::probe_drivers(rsdp_phys, pdbg);
    }
}

/// The touchpad probe cannot reach the framebuffer geometry or the cmdline, so
/// the boot step hands them over before the platform bus binds it.
fn install_touchpad_config() {
    let (width, height) = limine_protocol::boot_info()
        .framebuffer
        .map(|fb| (fb.info.width, fb.info.height))
        .unwrap_or((0, 0));
    let cmdline = slopos_ostd::util::cstr::cstr_from_kernel_ptr_str(boot_get_cmdline());
    let debug = cmdline.map(|s| s.contains("tp.debug")).unwrap_or(false);
    let force_poll = cmdline.map(|s| s.contains("tp.poll")).unwrap_or(false);
    if debug {
        // Lost-wake diagnostics: a stranded-task sweep in the tick path. Stays
        // here rather than in the probe — `slopos-drivers` does not depend on
        // `slopos-sched`.
        slopos_sched::sleep::arm_strand_sweep();
    }
    slopos_drivers::touchpad::platform::set_config(
        slopos_drivers::touchpad::platform::TouchpadConfig {
            width,
            height,
            debug,
            force_poll,
        },
    );
}

use slopos_testing::config_from_cmdline;

fn boot_step_run_tests_fn(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
    let cmdline_str = slopos_ostd::util::cstr::cstr_from_kernel_ptr_str(boot_get_cmdline());
    let test_config = config_from_cmdline(cmdline_str);

    if !test_config.enabled {
        klog_debug!("TESTS: Harness disabled");
        return 0;
    }

    // TTY tests write fixture strings through this mirror to COM1, which would
    // interleave them with the KTAP emit and corrupt the host parser. Never
    // restored: nothing after this point wants the mirror either.
    slopos_drivers::tty::vconsole::set_serial_mirror(false);

    klog_info!("TESTS: Running orchestrated harness");

    if klog::is_enabled_level(KlogLevel::Debug) {
        klog_info!("TESTS: Verbosity -> {}", test_config.verbosity);
        klog_info!("TESTS: Warn (ms) -> {}", test_config.warn_ms);
    }

    tests_reset_panic_state();

    let mut summary = TestRunSummary::default();
    let rc = tests_run_all(&test_config, &mut summary);

    // The mock clock overrides the monotonic source the production net stack
    // reads, so a test that forgets its `MockClockGuard` would freeze every net
    // timer for the userland phase.
    #[cfg(feature = "test-hooks")]
    slopos_net::clock::MockClock::clear();

    {
        let (epoch, advance_requested, deferred) = slopos_mm::mmu::quiesce::stats();
        klog_info!(
            "QUIESCE SUMMARY: epoch={} quarantined_frames={} advance_requested={} last_deferred_epoch={}",
            epoch,
            slopos_mm::page_alloc::quarantine_frames(),
            advance_requested,
            deferred,
        );
    }

    // Counterpart to the boot-time dump: the delta between the two lines is what
    // catches the suite itself exhausting a pool.
    slopos_ostd::kdiag::kdiag_dump_lock_graph("post-kernel-tests");
    slopos_sched::quota_console::quota_report("post-kernel-tests");
    slopos_sched::per_cpu::ap_pause_report("post-kernel-tests");
    slopos_sched::lifecycle::sched_cpu_report("post-kernel-tests");

    // Shutdown is always deferred to `SYSCALL_RUN_USERLAND_TESTS`, which merges
    // these counters, so that both phases run before QEMU exits.
    kernel_phase_summary::store_kernel_phase(&summary, rc, &test_config);

    if summary.failed > 0 {
        klog_info!("TESTS: Failures detected (kernel phase)");
    } else {
        klog_info!("TESTS: Kernel phase completed successfully");
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
fn boot_step_csprng_seed_fn(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
    use slopos_arch::cpu::rdrand;
    use slopos_arch::tsc;

    let mut seed = [0u8; 32];
    let mut source = "tsc";

    if let Some(rd) = rdrand::RdRand::probe() {
        source = "rdrand";
        for chunk in seed.chunks_exact_mut(8) {
            if let Some(val) = rd.next() {
                chunk.copy_from_slice(&val.to_le_bytes());
            }
        }
        if let Some(rs) = rdrand::RdSeed::probe() {
            source = "rdrand+rdseed";
            for chunk in seed.chunks_exact_mut(8) {
                if let Some(val) = rs.next() {
                    let existing = u64::from_le_bytes(chunk.try_into().unwrap());
                    let mixed = existing ^ val;
                    chunk.copy_from_slice(&mixed.to_le_bytes());
                }
            }
        }
    } else {
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
    BOOT_STEP_REGISTER_SPAWNER,
    drivers,
    b"register kernel-thread spawner with OSTD\0",
    boot_step_register_spawner_fn,
    flags = boot_init_priority(75)
);
crate::boot_init!(
    BOOT_STEP_IDENTITY_DMA,
    drivers,
    b"identity dma mapper\0",
    boot_step_identity_dma_fn,
    flags = boot_init_priority(78)
);
crate::boot_init!(
    BOOT_STEP_PCI_INIT,
    drivers,
    b"pci\0",
    boot_step_pci_init_fn,
    flags = boot_init_priority(80)
);
/// Runs immediately before the test step, by which point every driver has taken
/// its locks once, so the counters read as boot steady state.
fn boot_step_lockdep_report_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    slopos_ostd::kdiag::kdiag_dump_lock_graph("boot");
    slopos_sched::quota_console::quota_report("boot");
    slopos_sched::per_cpu::ap_pause_report("boot");
    slopos_sched::lifecycle::sched_cpu_report("boot");
}

crate::boot_init!(
    BOOT_STEP_LOCKDEP_REPORT,
    drivers,
    b"lockdep report\0",
    boot_step_lockdep_report_fn,
    flags = boot_init_priority(89)
);
crate::boot_init!(
    BOOT_STEP_RUN_TESTS,
    drivers,
    b"tests\0",
    boot_step_run_tests_fn,
    fallible,
    flags = boot_init_priority(90)
);

// No userland-test step here: the runner is `SYSCALL_RUN_USERLAND_TESTS` in
// `core/src/syscall/test_handlers.rs`, invoked synchronously from `/sbin/init`'s
// task context, where `task_wait_for` works by construction.
