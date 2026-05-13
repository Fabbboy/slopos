use core::{
    ffi::{CStr, c_char},
    ptr,
};

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use slopos_drivers::serial;
use slopos_ostd::sync::KernelSync;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_ostd::sync::spin::SpinLock;
use slopos_utils::klog::{self, KlogLevel};
use slopos_utils::wl_currency;
use slopos_utils::{klog_debug, klog_info, klog_set_level};
use slopos_video::splash;

use crate::limine_protocol;
use crate::{gdt, idt};

pub const BOOT_INIT_FLAG_OPTIONAL: u32 = 1 << 0;
const BOOT_INIT_PRIORITY_SHIFT: u32 = 8;
const BOOT_INIT_PRIORITY_MASK: u32 = 0xFF << BOOT_INIT_PRIORITY_SHIFT;

const BOOT_INIT_MAX_STEPS: usize = 64;

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum BootInitPhase {
    EarlyHw = 0,
    Memory = 1,
    Drivers = 2,
    Services = 3,
    Optional = 4,
}

impl BootInitPhase {
    pub const fn name(self) -> &'static [u8] {
        match self {
            Self::EarlyHw => b"early_hw\0",
            Self::Memory => b"memory\0",
            Self::Drivers => b"drivers\0",
            Self::Services => b"services\0",
            Self::Optional => b"optional\0",
        }
    }
}

/// Type alias for boot init step functions. Every step receives a
/// `&mut BootCtx` capability so it can call boot-time-only mutators
/// (gdt_set_ist, init_scheduler, etc.). Steps that don't touch boot
/// mutators still take the parameter — uniform signature avoids a
/// two-tone API and the borrow checker permits the unused param.
pub type BootInitFn = fn(&mut BootCtx) -> i32;

pub struct BootInitStep {
    name: &'static [u8],
    func: BootInitFn,
    flags: u32,
}

impl BootInitStep {
    pub const fn new(label: &'static [u8], func: BootInitFn, flags: u32) -> Self {
        Self {
            name: label,
            func,
            flags,
        }
    }

    fn priority(&self) -> u32 {
        self.flags & BOOT_INIT_PRIORITY_MASK
    }
}

/// Register a boot-init step.
///
/// All boot init functions take `&mut BootCtx` and return `i32`. Use
/// the `fallible` form for steps whose `i32` return code is consulted;
/// the bare form wraps a `fn(&mut BootCtx)` (returning unit) into
/// `Ok(0)`.
///
/// Both forms accept `flags = $expr` for explicit priority/optional
/// flags, or `optional` shorthand to mark a step as
/// `BOOT_INIT_FLAG_OPTIONAL` (failures are non-fatal).
#[macro_export]
macro_rules! boot_init {
    // Fallible fn(&mut BootCtx) -> i32, explicit flags
    ($static_name:ident, $phase:ident, $label:expr, $func:path, fallible, flags = $flags:expr) => {
        #[used]
        #[unsafe(link_section = concat!(".boot_init_", stringify!($phase)))]
        static $static_name: $crate::early_init::BootInitStep =
            $crate::early_init::BootInitStep::new($label, $func, $flags);
    };

    // Fallible fn(&mut BootCtx) -> i32, optional shorthand
    ($static_name:ident, $phase:ident, $label:expr, $func:path, fallible, optional) => {
        $crate::boot_init!(
            $static_name,
            $phase,
            $label,
            $func,
            fallible,
            flags = $crate::early_init::BOOT_INIT_FLAG_OPTIONAL
        );
    };

    // Fallible fn(&mut BootCtx) -> i32, no flags
    ($static_name:ident, $phase:ident, $label:expr, $func:path, fallible) => {
        $crate::boot_init!($static_name, $phase, $label, $func, fallible, flags = 0);
    };

    // Infallible fn(&mut BootCtx), explicit flags
    ($static_name:ident, $phase:ident, $label:expr, $func:path, flags = $flags:expr) => {
        const _: () = {
            fn wrapper(ctx: &mut $crate::early_init::BootCtx) -> i32 {
                $func(ctx);
                0
            }
            #[used]
            #[unsafe(link_section = concat!(".boot_init_", stringify!($phase)))]
            static STEP: $crate::early_init::BootInitStep =
                $crate::early_init::BootInitStep::new($label, wrapper, $flags);
        };
        #[allow(dead_code)]
        const $static_name: () = ();
    };

    // Infallible fn(&mut BootCtx), optional shorthand
    ($static_name:ident, $phase:ident, $label:expr, $func:path, optional) => {
        $crate::boot_init!(
            $static_name,
            $phase,
            $label,
            $func,
            flags = $crate::early_init::BOOT_INIT_FLAG_OPTIONAL
        );
    };

    // Infallible fn(&mut BootCtx), no flags (most common)
    ($static_name:ident, $phase:ident, $label:expr, $func:path) => {
        $crate::boot_init!($static_name, $phase, $label, $func, flags = 0);
    };
}

// Re-export BootCtx so the boot_init! macro expansion can name it via
// the canonical path `crate::early_init::BootCtx`.
pub use slopos_hermetic::BootCtx;

pub const fn boot_init_priority(val: u32) -> u32 {
    (val << BOOT_INIT_PRIORITY_SHIFT) & BOOT_INIT_PRIORITY_MASK
}

struct BootRuntimeContext {
    /// Bootloader memmap pointer; lives in a `'static` `SyncUnsafeCell`
    /// published by `limine_protocol::limine_get_memmap_response` whose
    /// contents are immutable for the kernel's lifetime. Wrapped in
    /// `KernelSync` so the surrounding `SpinLock<BootRuntimeContext>:
    /// Sync` is satisfied without a hand-written `unsafe impl Send`.
    memmap: KernelSync<*const limine_protocol::LimineMemmapResponse>,
    hhdm_offset: u64,
    cmdline: Option<&'static str>,
}

impl BootRuntimeContext {
    const fn new() -> Self {
        Self {
            memmap: KernelSync::new(ptr::null()),
            hhdm_offset: 0,
            cmdline: None,
        }
    }
}

static BOOT_RUNTIME: SpinLock<BootRuntimeContext> =
    SpinLock::new(BootRuntimeContext::new(), LOCK_LEVEL_RESOURCE);
static BOOT_INITIALIZED: AtomicBool = AtomicBool::new(false);

static BOOT_TOTAL_STEPS: AtomicUsize = AtomicUsize::new(0);
static BOOT_DONE_STEPS: AtomicUsize = AtomicUsize::new(0);

fn bytes_to_str(bytes: &[u8]) -> &str {
    CStr::from_bytes_with_nul(bytes)
        .ok()
        .and_then(|c| c.to_str().ok())
        .unwrap_or("<invalid>")
}

fn boot_info(msg: &'static [u8]) {
    klog_info!("{}", bytes_to_str(msg));
}

fn boot_debug(msg: &'static [u8]) {
    klog_debug!("{}", bytes_to_str(msg));
}

fn boot_init_report_phase(level: KlogLevel, prefix: &[u8], value: Option<&[u8]>) {
    if !klog::is_enabled_level(level) {
        return;
    }
    let prefix_str = bytes_to_str(prefix);
    let value_str = value.map(bytes_to_str).unwrap_or("");
    klog::log_args(
        level,
        format_args!("[boot:init] {}{}\n", prefix_str, value_str),
    );
}

fn boot_init_report_step(level: KlogLevel, label: &[u8], value: Option<&[u8]>) {
    if !klog::is_enabled_level(level) {
        return;
    }
    let label_str = bytes_to_str(label);
    let value_str = value.map(bytes_to_str).unwrap_or("(unnamed)");
    klog::log_args(level, format_args!("    {}: {}\n", label_str, value_str));
}

fn boot_init_report_failure(phase: &[u8], step_name: Option<&[u8]>) {
    let phase_str = bytes_to_str(phase);
    let step_str = step_name.map(bytes_to_str).unwrap_or("(unnamed)");
    klog_info!("[boot:init] FAILURE in {} -> {}", phase_str, step_str);
}

// Linker symbols from FFI boundary, accessed through the safe
// `<symbol>_addr()` accessors generated by `slopos_ostd::extern_block!`.
use crate::ffi_boundary::externs as boot_init_externs;

/// Borrow the contiguous `[BootInitStep]` array the linker places between
/// the `__start_boot_init_<phase>` and `__stop_boot_init_<phase>` symbols.
fn phase_steps(phase: BootInitPhase) -> &'static [BootInitStep] {
    let (start, stop): (*const BootInitStep, *const BootInitStep) = match phase {
        BootInitPhase::EarlyHw => (
            boot_init_externs::__start_boot_init_early_hw_addr(),
            boot_init_externs::__stop_boot_init_early_hw_addr(),
        ),
        BootInitPhase::Memory => (
            boot_init_externs::__start_boot_init_memory_addr(),
            boot_init_externs::__stop_boot_init_memory_addr(),
        ),
        BootInitPhase::Drivers => (
            boot_init_externs::__start_boot_init_drivers_addr(),
            boot_init_externs::__stop_boot_init_drivers_addr(),
        ),
        BootInitPhase::Services => (
            boot_init_externs::__start_boot_init_services_addr(),
            boot_init_externs::__stop_boot_init_services_addr(),
        ),
        BootInitPhase::Optional => (
            boot_init_externs::__start_boot_init_optional_addr(),
            boot_init_externs::__stop_boot_init_optional_addr(),
        ),
    };
    // SAFETY: the linker guarantees that for each phase there is exactly
    // one contiguous array of `BootInitStep` values bracketed by the
    // `__start_*` / `__stop_*` symbols (link.ld + the `boot_init!` macro
    // place every registration into `.boot_init_<phase>`). Computing the
    // byte distance is sound; the resulting slice covers exactly the
    // registered steps and never aliases other writable memory.
    unsafe {
        let len = stop.offset_from(start).max(0) as usize;
        core::slice::from_raw_parts(start, len)
    }
}

fn boot_init_count_phase(phase: BootInitPhase) -> usize {
    phase_steps(phase).len()
}

fn phase_from_u8(p: u8) -> Option<BootInitPhase> {
    match p {
        0 => Some(BootInitPhase::EarlyHw),
        1 => Some(BootInitPhase::Memory),
        2 => Some(BootInitPhase::Drivers),
        3 => Some(BootInitPhase::Services),
        4 => Some(BootInitPhase::Optional),
        _ => None,
    }
}

fn boot_init_prepare_progress() {
    let total = boot_init_count_phase(BootInitPhase::EarlyHw)
        + boot_init_count_phase(BootInitPhase::Memory)
        + boot_init_count_phase(BootInitPhase::Drivers)
        + boot_init_count_phase(BootInitPhase::Services)
        + boot_init_count_phase(BootInitPhase::Optional);
    BOOT_TOTAL_STEPS.store(total.max(1), Ordering::Relaxed);
    BOOT_DONE_STEPS.store(0, Ordering::Relaxed);
}

fn boot_init_report_progress(step: &BootInitStep) {
    let total = BOOT_TOTAL_STEPS.load(Ordering::Relaxed);
    if total == 0 {
        return;
    }
    let done = BOOT_DONE_STEPS.fetch_add(1, Ordering::Relaxed) + 1;
    let progress = ((done * 100) / total).min(100) as i32;
    let _ = splash::splash_report_progress(progress, step.name);
}

fn boot_run_step(ctx: &mut BootCtx, phase_name: &[u8], step: &BootInitStep) -> i32 {
    serial::write_line("BOOT: running init step");
    boot_init_report_step(KlogLevel::Debug, b"step\0", Some(step.name));

    let rc = (step.func)(ctx);

    if rc != 0 {
        let optional = (step.flags & BOOT_INIT_FLAG_OPTIONAL) != 0;
        boot_init_report_failure(phase_name, Some(step.name));
        if optional {
            boot_info(b"Optional boot step failed, continuing...\0");
            boot_init_report_progress(step);
            return 0;
        }
        panic!("Boot init step failed");
    }
    boot_init_report_progress(step);
    0
}

pub fn boot_init_run_phase(ctx: &mut BootCtx, phase: BootInitPhase) -> i32 {
    let steps = phase_steps(phase);
    if steps.is_empty() {
        return 0;
    }

    let phase_name = phase.name();

    boot_init_report_phase(KlogLevel::Debug, b"phase start -> \0", Some(phase_name));

    serial::write_str("BOOT: phase ");
    serial::write_line(bytes_to_str(phase_name));

    let mut ordered: [Option<&'static BootInitStep>; BOOT_INIT_MAX_STEPS] =
        [None; BOOT_INIT_MAX_STEPS];
    let mut ordered_count = 0usize;

    for step in steps {
        if ordered_count >= BOOT_INIT_MAX_STEPS {
            panic!("Boot init: too many steps for phase");
        }

        let prio = step.priority();
        let mut idx = ordered_count;
        while idx > 0 {
            let prev = ordered[idx - 1]
                .expect("ordered slot populated before this index")
                .priority();
            if prio >= prev {
                break;
            }
            ordered[idx] = ordered[idx - 1];
            idx -= 1;
        }
        ordered[idx] = Some(step);
        ordered_count += 1;
    }

    for slot in ordered.iter().take(ordered_count) {
        if let Some(step) = slot {
            boot_run_step(ctx, phase_name, step);
        }
    }

    boot_init_report_phase(KlogLevel::Info, b"phase complete -> \0", Some(phase_name));
    0
}

pub fn boot_init_run_all(ctx: &mut BootCtx) -> i32 {
    boot_init_prepare_progress();
    let mut phase_idx = BootInitPhase::EarlyHw as u8;
    while phase_idx <= BootInitPhase::Optional as u8 {
        let Some(phase) = phase_from_u8(phase_idx) else {
            break;
        };
        let rc = boot_init_run_phase(ctx, phase);
        if rc != 0 {
            return rc;
        }
        phase_idx += 1;
    }
    0
}

pub fn boot_get_memmap() -> *const limine_protocol::LimineMemmapResponse {
    *BOOT_RUNTIME.lock().memmap
}

pub fn boot_get_hhdm_offset() -> u64 {
    BOOT_RUNTIME.lock().hhdm_offset
}

pub fn boot_get_cmdline() -> *const c_char {
    BOOT_RUNTIME
        .lock()
        .cmdline
        .map(|s| s.as_ptr() as *const c_char)
        .unwrap_or(ptr::null())
}

pub fn boot_mark_initialized() {
    BOOT_INITIALIZED.store(true, Ordering::Release);
}

pub fn is_kernel_initialized() -> i32 {
    BOOT_INITIALIZED.load(Ordering::Acquire) as i32
}

pub fn get_initialization_progress() -> i32 {
    if BOOT_INITIALIZED.load(Ordering::Acquire) {
        100
    } else {
        50
    }
}

pub fn report_kernel_status() {
    if BOOT_INITIALIZED.load(Ordering::Acquire) {
        boot_info(b"SlopOS: Kernel status - INITIALIZED\0");
    } else {
        boot_info(b"SlopOS: Kernel status - INITIALIZING\0");
    }
}

use slopos_core::sched::enter_scheduler;

fn boot_step_serial_init_fn(_ctx: &mut BootCtx) {
    serial::write_line("BOOT: serial step -> init");
    serial::init();
    serial::write_line("BOOT: serial step -> after serial::init");

    serial::write_line("BOOT: serial step -> klog backend registered by serial::init");

    slopos_drivers::serial::write_line("SERIAL: init ok");
    boot_debug(b"Serial console ready on COM1\0");
}

fn boot_step_boot_banner_fn(_ctx: &mut BootCtx) {
    boot_info(b"SlopOS Kernel Started!\0");
    boot_info(b"Booting via Limine Protocol...\0");
}

fn boot_step_limine_protocol_fn(_ctx: &mut BootCtx) -> i32 {
    boot_debug(b"Initializing Limine protocol interface...\0");
    if limine_protocol::init_limine_protocol() != 0 {
        boot_info(b"ERROR: Limine protocol initialization failed\0");
        return -1;
    }
    boot_info(b"Limine protocol interface ready.\0");

    if limine_protocol::is_memory_map_available() == 0 {
        boot_info(b"ERROR: Limine did not provide a memory map\0");
        return -1;
    }

    let memmap = limine_protocol::limine_get_memmap_response();
    if memmap.is_null() {
        boot_info(b"ERROR: Limine memory map response pointer is NULL\0");
        return -1;
    }

    {
        let mut state = BOOT_RUNTIME.lock();
        state.memmap = KernelSync::new(memmap);
        state.hhdm_offset = limine_protocol::get_hhdm_offset();
        state.cmdline = limine_protocol::kernel_cmdline_str();
    }

    0
}

fn boot_step_boot_config_fn(_ctx: &mut BootCtx) {
    let cmdline = BOOT_RUNTIME.lock().cmdline.unwrap_or_default();
    let enable_debug = cmdline.contains("boot.debug=on")
        || cmdline.contains("boot.debug=1")
        || cmdline.contains("boot.debug=true")
        || cmdline.contains("bootdebug=on");
    let disable_debug = cmdline.contains("boot.debug=off")
        || cmdline.contains("boot.debug=0")
        || cmdline.contains("boot.debug=false")
        || cmdline.contains("bootdebug=off");

    if enable_debug {
        klog_set_level(KlogLevel::Debug);
        boot_info(b"Boot option: debug logging enabled\0");
    } else if disable_debug {
        klog_set_level(KlogLevel::Info);
        boot_debug(b"Boot option: debug logging disabled\0");
    }

    if cmdline.contains("roulette=skip") {
        slopos_utils::boot_flags::set_flag(slopos_utils::boot_flags::BOOT_FLAG_ROULETTE_SKIP);
        boot_info(b"Boot option: roulette skip enabled\0");
    }

    if cmdline.contains("tests=on") {
        slopos_utils::boot_flags::set_flag(slopos_utils::boot_flags::BOOT_FLAG_TESTS_ENABLED);
        boot_info(b"Boot option: userland test mode enabled\0");
    }
}

boot_init!(
    BOOT_STEP_SERIAL_INIT,
    early_hw,
    b"serial\0",
    boot_step_serial_init_fn
);
boot_init!(
    BOOT_STEP_BOOT_BANNER,
    early_hw,
    b"boot banner\0",
    boot_step_boot_banner_fn
);
boot_init!(
    BOOT_STEP_LIMINE,
    early_hw,
    b"limine\0",
    boot_step_limine_protocol_fn,
    fallible
);
boot_init!(
    BOOT_STEP_BOOT_CONFIG,
    early_hw,
    b"boot config\0",
    boot_step_boot_config_fn
);

fn boot_step_init_phys_virt_offset_fn(_ctx: &mut BootCtx) {
    let hhdm = boot_get_hhdm_offset();
    // SAFETY: one-shot init; the limine step has already populated
    // `hhdm_offset`, and this is the canonical wiring point for the
    // OSTD phys/virt offset.
    unsafe {
        slopos_ostd::mm::phys::init_phys_virt_offset(hhdm);
    }
}

boot_init!(
    BOOT_STEP_INIT_PHYS_VIRT_OFFSET,
    early_hw,
    b"phys_virt_offset\0",
    boot_step_init_phys_virt_offset_fn,
    flags = boot_init_priority(10)
);

/// Implementation of kernel_main - called from FFI boundary
pub fn kernel_main_impl() {
    wl_currency::reset();

    unsafe {
        let bsp_apic_id = crate::apic_id::read_bsp_apic_id();
        slopos_arch::pcr::init_bsp_pcr(bsp_apic_id);
        let pcr = slopos_arch::pcr::get_pcr_mut(0).expect("BSP PCR not initialized");
        pcr.init_gdt();
        pcr.install();
    }

    // Tell OSTD which PML4 was loaded by the bootloader. CR3 is a pure
    // CPU register read; the value persists for the lifetime of the
    // kernel since this PML4 holds the canonical kernel mappings.
    // SAFETY: one-shot init.
    unsafe {
        let cr3 = slopos_arch::cpu::control_regs::read_cr3();
        slopos_ostd::mm::vm_space::register_kernel_master_pml4(slopos_abi::addr::PhysAddr::new(
            cr3,
        ));
    }

    idt::idt_init();
    serial::write_line("BOOT: before idt_load (early)");
    idt::idt_load();
    serial::write_line("BOOT: after idt_load (early)");
    gdt::syscall_msr_init();
    serial::write_line("BOOT: early GDT/IDT/SYSCALL initialized");

    // Register platform and syscall service tables before the init phases run.
    crate::boot_impl::register_boot_services();
    slopos_core::driver_hooks::register_driver_services();
    slopos_drivers::syscall_services_init::init_syscall_services();
    // Wire OSTD's wait-queue / RCU backends to the kernel-services facades.
    // Must come after the platform + driver-runtime services are registered.
    // SAFETY: both required services are now registered above; the bridge
    // is a `'static` ZST so its registration cannot dangle.
    unsafe {
        slopos_kernel_services::ostd_bridge::register_with_ostd();
        slopos_mm::io_mem_mapper_shim::register_with_ostd();
    }
    slopos_core::sched::install_ostd_task_exit_hook();

    serial::write_line("BOOT: entering boot init");
    let mut boot_ctx = slopos_hermetic::take_for_boot();
    if boot_init_run_all(&mut boot_ctx) != 0 {
        panic!("Boot initialization failed");
    }
    slopos_hermetic::return_after_boot(boot_ctx);
    serial::write_line("BOOT: boot init complete");

    if klog::is_enabled_level(KlogLevel::Info) {
        klog_info!("");
    }

    boot_info(b"=== KERNEL BOOT SUCCESSFUL ===\0");
    boot_info(b"Operational subsystems: serial, interrupts, memory, scheduler, init\0");
    boot_info(b"Graphics: framebuffer required and active\0");
    boot_info(b"Kernel initialization complete - ALL SYSTEMS OPERATIONAL!\0");
    boot_info(b"The kernel has initialized. Handing over to scheduler...\0");
    boot_info(b"Starting scheduler...\0");

    if klog::is_enabled_level(KlogLevel::Info) {
        klog_info!("");
    }

    enter_scheduler(0);
}
pub fn kernel_main_no_multiboot() {
    crate::ffi_boundary::kernel_main();
}
