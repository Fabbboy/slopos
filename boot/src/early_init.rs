use core::{
    ffi::{CStr, c_char},
    ptr,
};
use slopos_ostd::lock_class;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use slopos_drivers::serial;
use slopos_ostd::klog::{self, KlogLevel};
use slopos_ostd::sync::KernelSync;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_ostd::sync::spin::SpinLock;
use slopos_ostd::wl_currency;
use slopos_ostd::{klog_debug, klog_info, klog_set_level};
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

/// Every step takes the `&mut BootCtx` capability, whether or not it calls a
/// boot-time-only mutator, so there is one signature rather than two. The HRTB
/// keeps `'brand` unified with the brand `run_bsp_init` mints at call time
/// instead of baking it into the fn-pointer type.
pub type BootInitFn = for<'b> fn(&mut BootCtx<'b, BspInit>) -> i32;

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

/// Translates the phase ident into the OSTD registry backing it;
/// `boot_init_run_all` walks the five in enum order.
#[macro_export]
#[doc(hidden)]
macro_rules! __boot_init_link_section {
    (early_hw, $($item:tt)*) => {
        ::slopos_ostd::registry_entry!(boot_init_early_hw, $($item)*);
    };
    (memory, $($item:tt)*) => {
        ::slopos_ostd::registry_entry!(boot_init_memory, $($item)*);
    };
    (drivers, $($item:tt)*) => {
        ::slopos_ostd::registry_entry!(boot_init_drivers, $($item)*);
    };
    (services, $($item:tt)*) => {
        ::slopos_ostd::registry_entry!(boot_init_services, $($item)*);
    };
    (optional, $($item:tt)*) => {
        ::slopos_ostd::registry_entry!(boot_init_optional, $($item)*);
    };
}

/// Register a boot-init step.
///
/// The `fallible` form keeps the step's `i32` return code; the bare form wraps a
/// unit-returning `fn(&mut BootCtx)` into `0`. Both take `flags = $expr` for
/// priority, or the `optional` shorthand that makes failure non-fatal.
#[macro_export]
macro_rules! boot_init {
    ($static_name:ident, $phase:ident, $label:expr, $func:path, fallible, flags = $flags:expr) => {
        const _: () = {
            fn wrapper<'b>(
                ctx: &mut $crate::early_init::BootCtx<'b, $crate::early_init::BspInit>,
            ) -> i32 {
                $func(ctx)
            }
            $crate::__boot_init_link_section! {
                $phase,
                static STEP: $crate::early_init::BootInitStep = $crate::early_init::BootInitStep::new(
                    $label,
                    wrapper as $crate::early_init::BootInitFn,
                    $flags,
                );
            }
        };
        #[allow(dead_code)]
        const $static_name: () = ();
    };

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

    ($static_name:ident, $phase:ident, $label:expr, $func:path, fallible) => {
        $crate::boot_init!($static_name, $phase, $label, $func, fallible, flags = 0);
    };

    ($static_name:ident, $phase:ident, $label:expr, $func:path, flags = $flags:expr) => {
        const _: () = {
            fn wrapper<'b>(
                ctx: &mut $crate::early_init::BootCtx<'b, $crate::early_init::BspInit>,
            ) -> i32 {
                $func(ctx);
                0
            }
            $crate::__boot_init_link_section! {
                $phase,
                static STEP: $crate::early_init::BootInitStep = $crate::early_init::BootInitStep::new(
                    $label,
                    wrapper as $crate::early_init::BootInitFn,
                    $flags,
                );
            }
        };
        #[allow(dead_code)]
        const $static_name: () = ();
    };

    ($static_name:ident, $phase:ident, $label:expr, $func:path, optional) => {
        $crate::boot_init!(
            $static_name,
            $phase,
            $label,
            $func,
            flags = $crate::early_init::BOOT_INIT_FLAG_OPTIONAL
        );
    };

    ($static_name:ident, $phase:ident, $label:expr, $func:path) => {
        $crate::boot_init!($static_name, $phase, $label, $func, flags = 0);
    };
}

// Re-exported so `boot_init!` expansions can name these by the canonical
// `crate::early_init::…` paths.
pub use slopos_hermetic::{BootCtx, BspInit};

pub const fn boot_init_priority(val: u32) -> u32 {
    (val << BOOT_INIT_PRIORITY_SHIFT) & BOOT_INIT_PRIORITY_MASK
}

struct BootRuntimeContext {
    /// Points at bootloader-published data that is immutable for the kernel's
    /// lifetime. `KernelSync` satisfies the enclosing `SpinLock`'s `Sync` bound
    /// without a hand-written `unsafe impl Send`.
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

static BOOT_RUNTIME: SpinLock<BootRuntimeContext> = SpinLock::new(
    BootRuntimeContext::new(),
    lock_class!("BOOT_RUNTIME", LOCK_LEVEL_RESOURCE),
);
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

use slopos_ostd::ffi::registry::{RegistryId, registry_slice};

impl slopos_ostd::ffi::registry::RegistryEntry for BootInitStep {
    // One entry type, five sections: the boot phase is the registry.
    const REGISTRIES: &'static [RegistryId] = &[
        RegistryId::BootInitEarlyHw,
        RegistryId::BootInitMemory,
        RegistryId::BootInitDrivers,
        RegistryId::BootInitServices,
        RegistryId::BootInitOptional,
    ];
}

/// Borrows the contiguous `[BootInitStep]` array the linker built for a phase.
fn phase_steps(phase: BootInitPhase) -> &'static [BootInitStep] {
    registry_slice::<BootInitStep>(match phase {
        BootInitPhase::EarlyHw => RegistryId::BootInitEarlyHw,
        BootInitPhase::Memory => RegistryId::BootInitMemory,
        BootInitPhase::Drivers => RegistryId::BootInitDrivers,
        BootInitPhase::Services => RegistryId::BootInitServices,
        BootInitPhase::Optional => RegistryId::BootInitOptional,
    })
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

fn boot_run_step<'b>(
    ctx: &mut BootCtx<'b, BspInit>,
    phase_name: &[u8],
    step: &BootInitStep,
) -> i32 {
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

pub fn boot_init_run_phase<'b>(ctx: &mut BootCtx<'b, BspInit>, phase: BootInitPhase) -> i32 {
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

pub fn boot_init_run_all<'b>(ctx: &mut BootCtx<'b, BspInit>) -> i32 {
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

use slopos_sched::scheduler::enter_scheduler;

fn boot_step_serial_init_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    serial::write_line("BOOT: serial step -> init");
    serial::init();
    serial::write_line("BOOT: serial step -> after serial::init");

    serial::write_line("BOOT: serial step -> klog backend registered by serial::init");

    slopos_drivers::serial::write_line("SERIAL: init ok");
    boot_debug(b"Serial console ready on COM1\0");
}

fn boot_step_boot_banner_fn(_ctx: &mut BootCtx<'_, BspInit>) {
    boot_info(b"SlopOS Kernel Started!\0");
    boot_info(b"Booting via Limine Protocol...\0");
}

fn boot_step_limine_protocol_fn(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
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

fn boot_step_boot_config_fn(_ctx: &mut BootCtx<'_, BspInit>) {
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
        slopos_ostd::boot_flags::set_flag(slopos_ostd::boot_flags::BOOT_FLAG_ROULETTE_SKIP);
        boot_info(b"Boot option: roulette skip enabled\0");
    }

    if cmdline.contains("tests=on") {
        slopos_ostd::boot_flags::set_flag(slopos_ostd::boot_flags::BOOT_FLAG_TESTS_ENABLED);
        boot_info(b"Boot option: userland test mode enabled\0");
    } else {
        // The Wheel of Fate's reboot is armed for an interactive boot only. A
        // test image must expose no user-reachable path that power-cycles the
        // machine mid-run -- the same reasoning that gates `test_panic`.
        slopos_ostd::boot_flags::set_flag(slopos_ostd::boot_flags::BOOT_FLAG_FATE_REBOOT);
    }

    if cmdline.contains("panic.on_oops=on")
        || cmdline.contains("panic.on_oops=1")
        || cmdline.contains("panic.on_oops=true")
    {
        slopos_ostd::boot_flags::set_flag(slopos_ostd::boot_flags::BOOT_FLAG_PANIC_ON_OOPS);
        boot_info(b"Boot option: panic.on_oops enabled\0");
    }

    if cmdline.contains("panic.recover_smoke=on")
        || cmdline.contains("panic.recover_smoke=1")
        || cmdline.contains("panic.recover_smoke=true")
    {
        slopos_ostd::boot_flags::set_flag(slopos_ostd::boot_flags::BOOT_FLAG_PANIC_RECOVER_SMOKE);
        boot_info(b"Boot option: panic recovery smoke enabled\0");
    }

    // Budget of recovered production panics per boot; reaching it makes the
    // limit-crossing panic fatal. `panic.oops_limit=0` disables the limit.
    for token in cmdline.split_whitespace() {
        if let Some(value) = token.strip_prefix("panic.oops_limit=") {
            if let Ok(limit) = value.parse::<u64>() {
                slopos_ostd::panic_recovery::set_oops_limit(limit);
                boot_info(b"Boot option: panic.oops_limit set\0");
            } else {
                boot_info(b"Boot option: panic.oops_limit ignored (not a u64)\0");
            }
        }
    }

    // `lockdep=warn` reports each distinct finding once and keeps booting, so
    // one boot enumerates the whole tree; `lockdep=off` still keeps the
    // held-lock walk panic recovery needs, and only drops the ordering checks.
    for token in cmdline.split_whitespace() {
        if let Some(value) = token.strip_prefix("lockdep=") {
            match value {
                "off" => {
                    slopos_ostd::sync::set_lockdep_mode(
                        slopos_ostd::sync::lock_tracking::LockdepMode::Off,
                    );
                    boot_info(b"Boot option: lockdep=off (ordering checks disabled)\0");
                }
                "warn" => {
                    slopos_ostd::sync::set_lockdep_mode(
                        slopos_ostd::sync::lock_tracking::LockdepMode::Warn,
                    );
                    boot_info(b"Boot option: lockdep=warn (report, do not panic)\0");
                }
                "panic" | "on" => {
                    slopos_ostd::sync::set_lockdep_mode(
                        slopos_ostd::sync::lock_tracking::LockdepMode::Panic,
                    );
                    boot_info(b"Boot option: lockdep=panic\0");
                }
                _ => boot_info(b"Boot option: lockdep= ignored (want off|warn|panic)\0"),
            }
        }
    }

    // `warn` reports each capability once and keeps booting, so one desktop
    // boot enumerates what the real userland needs — which `tests=on` cannot
    // show, since init exits before spawning the compositor or shell.
    for token in cmdline.split_whitespace() {
        if let Some(value) = token.strip_prefix("authority=") {
            match value {
                "off" => {
                    slopos_ostd::authority::set_mode(slopos_ostd::authority::AuthorityMode::Off);
                    boot_info(b"Boot option: authority=off (capability checks disabled)\0");
                }
                "warn" => {
                    slopos_ostd::authority::set_mode(slopos_ostd::authority::AuthorityMode::Warn);
                    boot_info(b"Boot option: authority=warn (report, do not deny)\0");
                }
                "enforce" | "on" => {
                    slopos_ostd::authority::set_mode(
                        slopos_ostd::authority::AuthorityMode::Enforce,
                    );
                    boot_info(b"Boot option: authority=enforce\0");
                }
                _ => boot_info(b"Boot option: authority= ignored (want off|warn|enforce)\0"),
            }
        }
    }

    // `quota=warn` is the only tier a real high-water mark can be measured on;
    // `quota=off` still moves the counters, so attribution survives without
    // enforcement.
    for token in cmdline.split_whitespace() {
        if let Some(value) = token.strip_prefix("quota=") {
            match value {
                "off" => {
                    slopos_ostd::process::quota::set_quota_mode(slopos_abi::quota::QuotaMode::Off);
                    boot_info(b"Boot option: quota=off (ceilings not consulted)\0");
                }
                "warn" => {
                    slopos_ostd::process::quota::set_quota_mode(slopos_abi::quota::QuotaMode::Warn);
                    boot_info(b"Boot option: quota=warn (grant and count, do not refuse)\0");
                }
                "enforce" | "on" => {
                    slopos_ostd::process::quota::set_quota_mode(
                        slopos_abi::quota::QuotaMode::Enforce,
                    );
                    boot_info(b"Boot option: quota=enforce\0");
                }
                _ => boot_info(b"Boot option: quota= ignored (want off|warn|enforce)\0"),
            }
        }
    }

    // `watchdog.miss_threshold=` counts consecutive unchanged samples before a
    // CPU is reported: 100 is one second at the 100 Hz tick.
    for token in cmdline.split_whitespace() {
        match token {
            "watchdog=off" => {
                slopos_ostd::watchdog::set_enabled(false);
                boot_info(b"Boot option: watchdog disabled\0");
            }
            "watchdog=on" => slopos_ostd::watchdog::set_enabled(true),
            "watchdog.panic=off" => {
                slopos_ostd::watchdog::set_panic_enabled(false);
                boot_info(b"Boot option: watchdog.panic disabled\0");
            }
            "watchdog.panic=on" => slopos_ostd::watchdog::set_panic_enabled(true),
            _ => {
                if let Some(value) = token.strip_prefix("watchdog.miss_threshold=") {
                    let accepted = value
                        .parse::<u32>()
                        .ok()
                        .is_some_and(slopos_ostd::watchdog::set_miss_threshold);
                    if accepted {
                        boot_info(b"Boot option: watchdog.miss_threshold set\0");
                    } else {
                        boot_info(b"Boot option: watchdog.miss_threshold ignored\0");
                    }
                }
            }
        }
    }

    // The unset default is `root=auto`: the initramfs when Limine loaded a
    // module, otherwise the ext2 disk.
    if cmdline.contains("root=initramfs") {
        crate::boot_services::set_root_mode(crate::boot_services::ROOT_INITRAMFS);
        boot_info(b"Boot option: root=initramfs\0");
    } else if cmdline.contains("root=virtio") {
        crate::boot_services::set_root_mode(crate::boot_services::ROOT_VIRTIO);
        boot_info(b"Boot option: root=virtio\0");
    }

    // A typed parser rather than more `contains` arms, so a malformed value
    // degrades to the shipped policy instead of to a disabled console.
    let kconsole = slopos_ostd::kconsole::config::parse(cmdline);
    slopos_ostd::kconsole::install(kconsole);
    if kconsole.mask == 0 {
        boot_info(b"Boot option: kconsole=off\0");
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

fn boot_step_init_phys_virt_offset_fn(ctx: &mut BootCtx<'_, BspInit>) {
    let hhdm = boot_get_hhdm_offset();
    let tok = ctx.bsp_token();
    slopos_ostd::mm::phys::init_phys_virt_offset(&tok, hhdm);
    // `mm::phys::PHYS_VIRT_OFFSET` and `boot::hhdm` hold the same u64; both are
    // populated here so a consumer can read through whichever its layer permits.
    slopos_ostd::boot::hhdm::register_hhdm_offset(&tok, hhdm);
}

boot_init!(
    BOOT_STEP_INIT_PHYS_VIRT_OFFSET,
    early_hw,
    b"phys_virt_offset\0",
    boot_step_init_phys_virt_offset_fn,
    flags = boot_init_priority(10)
);

/// Called from the `kernel_main` FFI boundary.
pub fn kernel_main_impl() {
    wl_currency::reset();

    #[cfg(feature = "tests")]
    slopos_ostd::panic::register_test_abort_shutdown(slopos_testing::tests_request_shutdown);

    slopos_ostd::sync::run_bsp_init(|token| {
        // `current_pcr()` is callable from every later boot step once this runs.
        let bsp_apic_id = crate::apic_id::read_bsp_apic_id();
        slopos_arch::pcr::init_bsp_pcr(token, bsp_apic_id);
        // The per-CPU-slot borrow contract holds trivially here: pre-SMP the BSP
        // is the only writer and the slot was minted immediately above.
        let pcr = slopos_arch::pcr::get_pcr_mut_via_token(0).expect("BSP PCR not initialized");
        pcr.bsp_init_gdt_and_install(token);

        // The earliest point acquisitions can be tracked: `get_current_cpu()`
        // only becomes callable once the PCR is live.
        slopos_ostd::sync::enable_lock_tracking();

        // Mask the low CR3 bits before handing the value over as a table base:
        // they carry PCID with CR4.PCIDE, PWT/PCD without it.
        let cr3 = slopos_arch::cpu::control_regs::read_cr3();
        slopos_ostd::mm::vm_space::register_kernel_master_pml4(
            token,
            slopos_abi::addr::PhysAddr::new(cr3 & 0x000F_FFFF_FFFF_F000),
        );

        idt::idt_init(token);
        serial::write_line("BOOT: before idt_load (early)");
        idt::idt_load(token);
        serial::write_line("BOOT: after idt_load (early)");
        gdt::syscall_msr_init(token);
        serial::write_line("BOOT: early GDT/IDT/SYSCALL initialized");

        // Service tables must be registered before the init phases run.
        crate::boot_impl::register_boot_services();
        slopos_core::driver_hooks::register_driver_services();
        slopos_drivers::syscall_services_init::init_syscall_services();

        // Inlined rather than delegated so every `register_*` hook below sees
        // the same `&BspToken<'brand>` as the surrounding init scope.
        use slopos_kernel_services::ostd_backends::diagnostic_sink::CONSOLE_SINK;
        use slopos_kernel_services::ostd_backends::local_tlb::LOCAL_TLB_DYN;
        use slopos_kernel_services::ostd_backends::preempt::PCR_PREEMPT;
        use slopos_kernel_services::ostd_bridge::{RCU_OPS, WAIT_QUEUE_OPS};
        use slopos_kernel_services::ostd_bridge_tables::{
            MMIO_RANGES, PORT_RANGES, RESERVED_VECTORS,
        };
        slopos_ostd::sync::wait_queue::register_wait_queue_backend(token, &WAIT_QUEUE_OPS);
        slopos_ostd::sync::rcu::register_rcu_backend(token, &RCU_OPS);
        slopos_ostd::klog::klog_register_clock(|| {
            slopos_drivers::hpet::nanoseconds(slopos_drivers::hpet::read_counter())
        });
        slopos_ostd::mm::io_mem::register_io_mem_registry(token, MMIO_RANGES);
        slopos_ostd::io::port::register_io_port_registry(token, PORT_RANGES);
        slopos_ostd::irq::line::register_irq_reserved(token, RESERVED_VECTORS);
        slopos_ostd::irq::idt::register_diagnostic_sink(token, &CONSOLE_SINK);
        slopos_ostd::cpu::preempt::register_preempt_backend(token, &PCR_PREEMPT);
        slopos_ostd::mm::tlb::register_local_tlb_flusher(token, &LOCAL_TLB_DYN);
        slopos_ostd::user::mode::register_user_mode_backend(
            token,
            &slopos_ostd::user::mode::DEFAULT_USER_MODE_BACKEND,
        );
        slopos_kernel_services::platform::console_puts(
            b"BOOT: register_with_ostd: registered preempt/diag/tlb/io_mem/io_port/irq/user_mode tables\n",
        );

        slopos_ostd::mm::io_mem::register_io_mem_mapper(
            token,
            &slopos_mm::io_mem_mapper_shim::LEGACY_IO_MEM_MAPPER_DYN,
        );

        slopos_sched::scheduler::install_ostd_task_exit_hook(token);
        slopos_core::syscall::user_loop::install_user_task_entry(token);

        serial::write_line("BOOT: entering boot init");
        let mut boot_ctx = slopos_hermetic::take_for_boot(token);
        if boot_init_run_all(&mut boot_ctx) != 0 {
            panic!("Boot initialization failed");
        }
        slopos_hermetic::return_after_boot(boot_ctx);
        serial::write_line("BOOT: boot init complete");

        // Must happen while the EFI memory map is still live, so firmware
        // `ResetSystem` stays callable at shutdown. No-op on a BIOS boot.
        crate::uefi_runtime::map_runtime_regions(boot_get_hhdm_offset());
    });

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

    if slopos_ostd::boot_flags::has_flag(slopos_ostd::boot_flags::BOOT_FLAG_PANIC_RECOVER_SMOKE) {
        RECOVER_SMOKE_DROP_RAN.store(false, Ordering::Release);
        RECOVER_SMOKE_TASK_ID.store(u32::MAX, Ordering::Release);
        let priority = slopos_sched::task::TaskPriority::Normal;
        let smoke_task =
            slopos_ostd::task::spawn("panic-recover-smoke", panic_recover_smoke_task, priority);
        let Ok(smoke_task) = smoke_task else {
            klog_info!("PANIC RECOVERY SMOKE: failed to spawn panic task");
            slopos_ostd::panic::abort_now();
        };
        RECOVER_SMOKE_TASK_ID.store(smoke_task.as_u32(), Ordering::Release);
        if slopos_ostd::task::spawn(
            "panic-recover-check",
            panic_recover_smoke_observer,
            priority,
        )
        .is_err()
        {
            klog_info!("PANIC RECOVERY SMOKE: failed to spawn observer task");
            slopos_ostd::panic::abort_now();
        }
        if slopos_ostd::task::spawn(
            "panic-syscall-smoke",
            panic_syscall_smoke_observer,
            priority,
        )
        .is_err()
        {
            klog_info!("SYSCALL PANIC SMOKE: failed to spawn observer task");
            slopos_ostd::panic::abort_now();
        }
    }

    // Panic-path smokes, all off by default: each raises a deliberate panic so a
    // `just boot-log` run can confirm one clean "=== KERNEL PANIC ===" instead
    // of a recursive fault. Each halts the machine, so never enabled by `just test`.
    if let Some(cmdline) = crate::limine_protocol::kernel_cmdline_str() {
        if cmdline.contains("panic.post_boot_unwind=on") {
            boot_info(b"PANIC SMOKE: raising a deliberate post-boot unwind panic\0");
            post_boot_unwind_smoke_outer();
        }
        if cmdline.contains("panic.fatal_smoke=on") {
            boot_info(b"PANIC SMOKE: raising a deliberate deep fatal panic\0");
            let _ = fatal_smoke_deep(28);
        }
        if cmdline.contains("panic.nested_drop_smoke=on") {
            boot_info(b"PANIC SMOKE: raising a Drop panic during a recoverable unwind\0");
            nested_drop_smoke();
        }
    }

    enter_scheduler(0);
}

static RECOVER_SMOKE_DROP_RAN: AtomicBool = AtomicBool::new(false);
static RECOVER_SMOKE_TASK_ID: AtomicU32 = AtomicU32::new(u32::MAX);

struct RecoverSmokeDrop;

impl Drop for RecoverSmokeDrop {
    fn drop(&mut self) {
        RECOVER_SMOKE_DROP_RAN.store(true, Ordering::Release);
    }
}

fn panic_recover_smoke_task() {
    let _guard = RecoverSmokeDrop;
    panic!("panic.recover_smoke: deliberate recoverable kthread panic");
}

fn panic_recover_smoke_observer() {
    let task_id = RECOVER_SMOKE_TASK_ID.load(Ordering::Acquire);
    if task_id == u32::MAX {
        klog_info!("PANIC RECOVERY SMOKE: missing panic task id");
        slopos_ostd::panic::abort_now();
    }

    let _ = slopos_sched::scheduler::task_wait_for(task_id);
    if RECOVER_SMOKE_DROP_RAN.load(Ordering::Acquire) {
        klog_info!("PANIC RECOVERY SMOKE: cleanup observed; kernel survived");
    } else {
        klog_info!("PANIC RECOVERY SMOKE: cleanup was not observed");
        slopos_ostd::panic::abort_now();
    }
}

/// `/bin/oops_smoke` panics inside its own syscall context; the recovery
/// boundary kills the task and this observer confirms the kernel outlived it.
/// The transcript's `panic recovery: syscall` line is the positive assertion.
fn panic_syscall_smoke_observer() {
    use slopos_sched::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE};

    let pid = match slopos_core::exec::spawn_program_with_attrs(
        b"/bin/oops_smoke",
        None,
        None,
        slopos_sched::task::TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &[],
        0,
        None,
        INVALID_TASK_ID,
    ) {
        Ok(pid) => pid,
        Err(err) => {
            klog_info!("SYSCALL PANIC SMOKE: failed to spawn /bin/oops_smoke: {err:?}");
            slopos_ostd::panic::abort_now();
        }
    };
    let _ = slopos_sched::scheduler::task_wait_for(pid);
    klog_info!("SYSCALL PANIC SMOKE: task died; kernel survived");
}

struct NestedDropPanic;

impl Drop for NestedDropPanic {
    fn drop(&mut self) {
        panic!("panic.nested_drop_smoke: Drop panic during unwind");
    }
}

/// A Drop that panics while a caught unwind is already in flight must land on
/// the fatal path, printing exactly one clean `=== KERNEL PANIC ===`.
fn nested_drop_smoke() {
    let _ = slopos_ostd::panic_recovery::run_recoverable(|| {
        let _guard = NestedDropPanic;
        panic!("panic.nested_drop_smoke: outer recoverable panic");
    });
    boot_info(b"PANIC SMOKE: nested drop smoke unexpectedly survived\0");
}

#[inline(never)]
fn post_boot_unwind_smoke_outer() -> ! {
    post_boot_unwind_smoke_middle(0x51);
}

#[inline(never)]
fn post_boot_unwind_smoke_middle(seed: u64) -> ! {
    post_boot_unwind_smoke_inner(seed ^ 0x5a5a);
}

#[inline(never)]
fn post_boot_unwind_smoke_inner(canary: u64) -> ! {
    panic!("panic.post_boot_unwind: deliberate post-boot panic (canary={canary:#x})");
}

/// Reproduces "panic while the SafeStack data stack is near-full": an uncaught
/// panic at the bottom of a deep chain of address-taken buffers, which must
/// switch to the emergency stacks rather than overflow while formatting.
#[inline(never)]
fn fatal_smoke_deep(depth: u32) -> u64 {
    let mut buf = [0u8; 512];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i as u8) ^ (depth as u8);
    }
    // By reference: a by-value `black_box` would copy and bust the 2 KiB frame
    // gate, but the address still has to be taken or the buffer is optimised away.
    core::hint::black_box(&buf);
    if depth == 0 {
        let mut sum = 0u64;
        for &b in buf.iter() {
            sum = sum.wrapping_add(b as u64);
        }
        panic!("panic.fatal_smoke: deliberate fatal panic from a deep stack (canary={sum})");
    }
    let inner = fatal_smoke_deep(depth - 1);
    let mut acc = inner;
    for &b in buf.iter() {
        acc = acc.wrapping_add(b as u64);
    }
    acc
}

pub fn kernel_main_no_multiboot() {
    crate::ffi_boundary::kernel_main();
}
