#![allow(dead_code)]

use core::{
    cell::UnsafeCell,
    ffi::{c_char, CStr},
    fmt,
    ptr,
};

use slopos_drivers::serial;
use slopos_drivers::wl_currency;
use slopos_lib::klog::{self, KlogLevel};
use slopos_lib::{klog_debug, klog_info, klog_newline, klog_set_level};

use crate::{gdt, idt, kernel_panic::kernel_panic, safe_stack};
use crate::limine_protocol;

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

#[repr(C)]
pub struct BootInitStep {
    name: *const c_char,
    func: Option<extern "C" fn() -> i32>,
    func_unit: Option<extern "C" fn() -> ()>,
    flags: u32,
}

unsafe impl Sync for BootInitStep {}

impl BootInitStep {
    pub const fn new(label: &'static [u8], func: extern "C" fn() -> i32, flags: u32) -> Self {
        Self {
            name: label.as_ptr() as *const c_char,
            func: Some(func),
            func_unit: None,
            flags,
        }
    }

    pub const fn new_unit(label: &'static [u8], func: extern "C" fn() -> (), flags: u32) -> Self {
        Self {
            name: label.as_ptr() as *const c_char,
            func: None,
            func_unit: Some(func),
            flags,
        }
    }

    fn priority(&self) -> u32 {
        self.flags & BOOT_INIT_PRIORITY_MASK
    }
}

#[macro_export]
macro_rules! boot_init_step {
    ($static_name:ident, $phase:ident, $label:expr, $func:ident) => {
        #[used]
        #[unsafe(link_section = concat!(".boot_init_", stringify!($phase)))]
        static $static_name: $crate::early_init::BootInitStep =
            $crate::early_init::BootInitStep::new($label, $func, 0);
    };
}

#[macro_export]
macro_rules! boot_init_step_unit {
    ($static_name:ident, $phase:ident, $label:expr, $func:ident) => {
        #[used]
        #[unsafe(link_section = concat!(".boot_init_", stringify!($phase)))]
        static $static_name: $crate::early_init::BootInitStep =
            $crate::early_init::BootInitStep::new_unit($label, $func, 0);
    };
}

#[macro_export]
macro_rules! boot_init_step_with_flags {
    ($static_name:ident, $phase:ident, $label:expr, $func:ident, $flags:expr) => {
        #[used]
        #[unsafe(link_section = concat!(".boot_init_", stringify!($phase)))]
        static $static_name: $crate::early_init::BootInitStep =
            $crate::early_init::BootInitStep::new($label, $func, $flags);
    };
}

#[macro_export]
macro_rules! boot_init_step_with_flags_unit {
    ($static_name:ident, $phase:ident, $label:expr, $func:ident, $flags:expr) => {
        #[used]
        #[unsafe(link_section = concat!(".boot_init_", stringify!($phase)))]
        static $static_name: $crate::early_init::BootInitStep =
            $crate::early_init::BootInitStep::new_unit($label, $func, $flags);
    };
}

#[macro_export]
macro_rules! boot_init_optional_step {
    ($static_name:ident, $phase:ident, $label:expr, $func:ident) => {
        $crate::boot_init_step_with_flags!(
            $static_name,
            $phase,
            $label,
            $func,
            $crate::early_init::BOOT_INIT_FLAG_OPTIONAL
        );
    };
}

#[macro_export]
macro_rules! boot_init_optional_step_unit {
    ($static_name:ident, $phase:ident, $label:expr, $func:ident) => {
        $crate::boot_init_step_with_flags_unit!(
            $static_name,
            $phase,
            $label,
            $func,
            $crate::early_init::BOOT_INIT_FLAG_OPTIONAL
        );
    };
}

pub const fn boot_init_priority(val: u32) -> u32 {
    ((val << BOOT_INIT_PRIORITY_SHIFT) & BOOT_INIT_PRIORITY_MASK)
}

struct BootRuntimeContext {
    memmap: *const limine_protocol::LimineMemmapResponse,
    hhdm_offset: u64,
    cmdline: Option<&'static str>,
}

impl BootRuntimeContext {
    const fn new() -> Self {
        Self {
            memmap: ptr::null(),
            hhdm_offset: 0,
            cmdline: None,
        }
    }
}

struct BootState {
    initialized: bool,
    ctx: BootRuntimeContext,
}

struct BootStateCell(UnsafeCell<BootState>);

unsafe impl Sync for BootStateCell {}

static BOOT_STATE: BootStateCell = BootStateCell(UnsafeCell::new(BootState {
    initialized: false,
    ctx: BootRuntimeContext::new(),
}));

fn boot_state() -> &'static BootState {
    unsafe { &*BOOT_STATE.0.get() }
}

fn boot_state_mut() -> &'static mut BootState {
    unsafe { &mut *BOOT_STATE.0.get() }
}

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
    klog::log_args(level, format_args!("[boot:init] {}{}\n", prefix_str, value_str));
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

unsafe extern "C" {
    static __start_boot_init_early_hw: BootInitStep;
    static __stop_boot_init_early_hw: BootInitStep;
    static __start_boot_init_memory: BootInitStep;
    static __stop_boot_init_memory: BootInitStep;
    static __start_boot_init_drivers: BootInitStep;
    static __stop_boot_init_drivers: BootInitStep;
    static __start_boot_init_services: BootInitStep;
    static __stop_boot_init_services: BootInitStep;
    static __start_boot_init_optional: BootInitStep;
    static __stop_boot_init_optional: BootInitStep;
}

fn phase_bounds(phase: BootInitPhase) -> (*const BootInitStep, *const BootInitStep) {
    match phase {
        BootInitPhase::EarlyHw => (unsafe { &__start_boot_init_early_hw }, unsafe {
            &__stop_boot_init_early_hw
        }),
        BootInitPhase::Memory => (unsafe { &__start_boot_init_memory }, unsafe {
            &__stop_boot_init_memory
        }),
        BootInitPhase::Drivers => (unsafe { &__start_boot_init_drivers }, unsafe {
            &__stop_boot_init_drivers
        }),
        BootInitPhase::Services => (unsafe { &__start_boot_init_services }, unsafe {
            &__stop_boot_init_services
        }),
        BootInitPhase::Optional => (unsafe { &__start_boot_init_optional }, unsafe {
            &__stop_boot_init_optional
        }),
    }
}

fn boot_run_step(phase_name: &[u8], step: &BootInitStep) -> i32 {
    serial::write_line("BOOT: running init step");
    boot_init_report_step(
        KlogLevel::Debug,
        b"step\0",
        Some(
            unsafe { CStr::from_ptr(step.name).to_bytes_with_nul() }
                .get(0..)
                .unwrap_or(b"(unnamed)\0"),
        ),
    );

    let rc = if let Some(func) = step.func {
        func()
    } else if let Some(func_unit) = step.func_unit {
        func_unit();
        0 // Unit return is always success
    } else {
        return 0;
    };

    if rc != 0 {
        let optional = (step.flags & BOOT_INIT_FLAG_OPTIONAL) != 0;
        boot_init_report_failure(
            phase_name,
            Some(unsafe { CStr::from_ptr(step.name).to_bytes_with_nul() }),
        );
        if optional {
            boot_info(b"Optional boot step failed, continuing...\0");
            return 0;
        }
        kernel_panic(b"Boot init step failed\0".as_ptr() as *const c_char);
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn boot_init_run_phase(phase: BootInitPhase) -> i32 {
    let (start, end) = phase_bounds(phase);
    if start.is_null() || end.is_null() {
        return 0;
    }

    let phase_name: &[u8] = match phase {
        BootInitPhase::EarlyHw => b"early_hw\0".as_slice(),
        BootInitPhase::Memory => b"memory\0".as_slice(),
        BootInitPhase::Drivers => b"drivers\0".as_slice(),
        BootInitPhase::Services => b"services\0".as_slice(),
        BootInitPhase::Optional => b"optional\0".as_slice(),
    };

    boot_init_report_phase(KlogLevel::Debug, b"phase start -> \0", Some(phase_name));

    let phase_label = match phase {
        BootInitPhase::EarlyHw => "BOOT: phase early_hw",
        BootInitPhase::Memory => "BOOT: phase memory",
        BootInitPhase::Drivers => "BOOT: phase drivers",
        BootInitPhase::Services => "BOOT: phase services",
        BootInitPhase::Optional => "BOOT: phase optional",
    };
    serial::write_line(phase_label);

    let mut ordered: [*const BootInitStep; BOOT_INIT_MAX_STEPS] =
        [ptr::null(); BOOT_INIT_MAX_STEPS];
    let mut ordered_count = 0usize;

    let mut cursor = start;
    while cursor < end {
        if ordered_count >= BOOT_INIT_MAX_STEPS {
            kernel_panic(b"Boot init: too many steps for phase\0".as_ptr() as *const c_char);
        }

        let prio = unsafe { (*cursor).priority() };
        let mut idx = ordered_count;
        while idx > 0 {
            let prev = unsafe { (*ordered[idx - 1]).priority() };
            if prio >= prev {
                break;
            }
            ordered[idx] = ordered[idx - 1];
            idx -= 1;
        }
        ordered[idx] = cursor;
        ordered_count += 1;

        cursor = unsafe { cursor.add(1) };
    }

    for i in 0..ordered_count {
        let step_ptr = ordered[i];
        if step_ptr.is_null() {
            continue;
        }
        boot_run_step(phase_name, unsafe { &*step_ptr });
    }

    boot_init_report_phase(KlogLevel::Info, b"phase complete -> \0", Some(phase_name));
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn boot_init_run_all() -> i32 {
    let mut phase = BootInitPhase::EarlyHw as u8;
    while phase <= BootInitPhase::Optional as u8 {
        let rc = boot_init_run_phase(unsafe { core::mem::transmute(phase) });
        if rc != 0 {
            return rc;
        }
        phase += 1;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn boot_get_memmap() -> *const limine_protocol::LimineMemmapResponse {
    boot_state().ctx.memmap
}

#[unsafe(no_mangle)]
pub extern "C" fn boot_get_hhdm_offset() -> u64 {
    boot_state().ctx.hhdm_offset
}

#[unsafe(no_mangle)]
pub extern "C" fn boot_get_cmdline() -> *const c_char {
    boot_state()
        .ctx
        .cmdline
        .map(|s| s.as_ptr() as *const c_char)
        .unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub extern "C" fn boot_mark_initialized() {
    boot_state_mut().initialized = true;
}

#[unsafe(no_mangle)]
pub extern "C" fn is_kernel_initialized() -> i32 {
    boot_state().initialized as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn get_initialization_progress() -> i32 {
    if boot_state().initialized {
        100
    } else {
        50
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn report_kernel_status() {
    if boot_state().initialized {
        boot_info(b"SlopOS: Kernel status - INITIALIZED\0");
    } else {
        boot_info(b"SlopOS: Kernel status - INITIALIZING\0");
    }
}

unsafe extern "C" {
    fn start_scheduler() -> i32;
}

extern "C" fn boot_step_serial_init_fn() {
    serial::write_line("BOOT: serial step -> init");
    serial::init();
    serial::write_line("BOOT: serial step -> after serial::init");

    slopos_lib::klog_attach_serial();
    serial::write_line("BOOT: serial step -> after klog_attach_serial");

    slopos_drivers::serial::write_line("SERIAL: init ok");
    boot_debug(b"Serial console ready on COM1\0");
}

extern "C" fn boot_step_boot_banner_fn() {
    boot_info(b"SlopOS Kernel Started!\0");
    boot_info(b"Booting via Limine Protocol...\0");
}

extern "C" fn boot_step_limine_protocol_fn() -> i32 {
    boot_debug(b"Initializing Limine protocol interface...\0");
    if unsafe { limine_protocol::init_limine_protocol() } != 0 {
        boot_info(b"ERROR: Limine protocol initialization failed\0");
        return -1;
    }
    boot_info(b"Limine protocol interface ready.\0");

    if unsafe { limine_protocol::is_memory_map_available() } == 0 {
        boot_info(b"ERROR: Limine did not provide a memory map\0");
        return -1;
    }

    let memmap = unsafe { limine_protocol::limine_get_memmap_response() };
    if memmap.is_null() {
        boot_info(b"ERROR: Limine memory map response pointer is NULL\0");
        return -1;
    }

    {
        let state = boot_state_mut();
        state.ctx.memmap = memmap;
        state.ctx.hhdm_offset = unsafe { limine_protocol::get_hhdm_offset() };
        state.ctx.cmdline = limine_protocol::kernel_cmdline_str();
    }

    0
}

extern "C" fn boot_step_boot_config_fn() {
    let cmdline = boot_state().ctx.cmdline.unwrap_or_default();
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
}

crate::boot_init_step_unit!(
    BOOT_STEP_SERIAL_INIT,
    early_hw,
    b"serial\0",
    boot_step_serial_init_fn
);
crate::boot_init_step_unit!(
    BOOT_STEP_BOOT_BANNER,
    early_hw,
    b"boot banner\0",
    boot_step_boot_banner_fn
);
boot_init_step!(BOOT_STEP_LIMINE, early_hw, b"limine\0", boot_step_limine_protocol_fn);
crate::boot_init_step_unit!(
    BOOT_STEP_BOOT_CONFIG,
    early_hw,
    b"boot config\0",
    boot_step_boot_config_fn
);

#[unsafe(no_mangle)]
pub extern "C" fn kernel_main() {
    wl_currency::reset();

    // Install basic GDT/IDT early so faults have somewhere to land. IST
    // stacks are deferred to the driver phase (after memory is online) so
    // safe_stack_init can allocate pages.
    unsafe {
        gdt::gdt_init();
        idt::idt_init();
        serial::write_line("BOOT: before idt_load (early)");
        idt::idt_load();
        serial::write_line("BOOT: after idt_load (early)");
    }
    serial::write_line("BOOT: early GDT/IDT initialized");

    serial::write_line("BOOT: entering boot init");
    if boot_init_run_all() != 0 {
        kernel_panic(b"Boot initialization failed\0".as_ptr() as *const c_char);
    }
    serial::write_line("BOOT: boot init complete");

    if klog::is_enabled_level(KlogLevel::Info) {
        klog_newline();
    }

    boot_info(b"=== KERNEL BOOT SUCCESSFUL ===\0");
    boot_info(b"Operational subsystems: serial, interrupts, memory, scheduler, shell\0");
    boot_info(b"Graphics: framebuffer required and active\0");
    boot_info(b"Kernel initialization complete - ALL SYSTEMS OPERATIONAL!\0");
    boot_info(b"The kernel has initialized. Handing over to scheduler...\0");
    boot_info(b"Starting scheduler...\0");

    if klog::is_enabled_level(KlogLevel::Info) {
        klog_newline();
    }

    let rc = unsafe { start_scheduler() };
    if rc != 0 {
        klog_info!("ERROR: Scheduler startup failed");
        kernel_panic(b"Scheduler startup failed\0".as_ptr() as *const c_char);
    }

    klog_info!("WARNING: Scheduler exited unexpectedly");

    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kernel_main_no_multiboot() {
    kernel_main();
}
