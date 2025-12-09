#![allow(dead_code)]

use core::{
    cell::UnsafeCell,
    ffi::{c_char, CStr},
    ptr,
};

use slopos_drivers::serial;
use slopos_drivers::wl_currency;
use slopos_lib::{klog_is_enabled, klog_newline, klog_printf, klog_set_level, KlogLevel};

use crate::kernel_panic::kernel_panic;
use crate::limine_protocol;

const BOOT_INIT_FLAG_OPTIONAL: u32 = 1 << 0;
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
    flags: u32,
}

impl BootInitStep {
    pub const fn new(label: &'static [u8], func: extern "C" fn() -> i32, flags: u32) -> Self {
        Self {
            name: label.as_ptr() as *const c_char,
            func: Some(func),
            flags,
        }
    }

    fn priority(&self) -> u32 {
        self.flags & BOOT_INIT_PRIORITY_MASK
    }
}

#[macro_export]
macro_rules! boot_init_step {
    ($phase:ident, $label:expr, $func:ident) => {
        #[used]
        #[link_section = concat!(".boot_init_", stringify!($phase))]
        static $func: $crate::early_init::BootInitStep =
            $crate::early_init::BootInitStep::new($label, $func, 0);
    };
}

#[macro_export]
macro_rules! boot_init_step_with_flags {
    ($phase:ident, $label:expr, $func:ident, $flags:expr) => {
        #[used]
        #[link_section = concat!(".boot_init_", stringify!($phase))]
        static $func: $crate::early_init::BootInitStep =
            $crate::early_init::BootInitStep::new($label, $func, $flags);
    };
}

#[macro_export]
macro_rules! boot_init_optional_step {
    ($phase:ident, $label:expr, $func:ident) => {
        $crate::boot_init_step_with_flags!(
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

fn boot_info(msg: &'static [u8]) {
    unsafe {
        klog_printf(KlogLevel::Info, msg.as_ptr() as *const c_char);
    }
}

fn boot_debug(msg: &'static [u8]) {
    unsafe {
        klog_printf(KlogLevel::Debug, msg.as_ptr() as *const c_char);
    }
}

fn boot_init_report_phase(level: KlogLevel, prefix: &[u8], value: Option<&[u8]>) {
    if unsafe { klog_is_enabled(level) } == 0 {
        return;
    }
    unsafe {
        klog_printf(
            level,
            b"[boot:init] %s%s\n\0".as_ptr() as *const c_char,
            prefix.as_ptr() as *const c_char,
            value.unwrap_or(b"\0").as_ptr() as *const c_char,
        );
    }
}

fn boot_init_report_step(level: KlogLevel, label: &[u8], value: Option<&[u8]>) {
    if unsafe { klog_is_enabled(level) } == 0 {
        return;
    }
    unsafe {
        klog_printf(
            level,
            b"    %s: %s\n\0".as_ptr() as *const c_char,
            label.as_ptr() as *const c_char,
            value.unwrap_or(b"(unnamed)\0").as_ptr() as *const c_char,
        );
    }
}

fn boot_init_report_failure(phase: &[u8], step_name: Option<&[u8]>) {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"[boot:init] FAILURE in %s -> %s\n\0".as_ptr() as *const c_char,
            phase.as_ptr() as *const c_char,
            step_name.unwrap_or(b"(unnamed)\0").as_ptr() as *const c_char,
        );
    }
}

extern "C" {
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
    let Some(func) = step.func else {
        return 0;
    };

    boot_init_report_step(
        KlogLevel::Debug,
        b"step\0",
        unsafe { CStr::from_ptr(step.name).to_bytes_with_nul() }
            .get(0..)
            .unwrap_or(b"(unnamed)\0"),
    );

    let rc = func();
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
        kernel_panic("Boot init step failed");
    }
    0
}

#[no_mangle]
pub extern "C" fn boot_init_run_phase(phase: BootInitPhase) -> i32 {
    let (start, end) = phase_bounds(phase);
    if start.is_null() || end.is_null() {
        return 0;
    }

    let phase_name = match phase {
        BootInitPhase::EarlyHw => b"early_hw\0",
        BootInitPhase::Memory => b"memory\0",
        BootInitPhase::Drivers => b"drivers\0",
        BootInitPhase::Services => b"services\0",
        BootInitPhase::Optional => b"optional\0",
    };

    boot_init_report_phase(KlogLevel::Debug, b"phase start -> \0", Some(phase_name));

    let mut ordered: [*const BootInitStep; BOOT_INIT_MAX_STEPS] =
        [ptr::null(); BOOT_INIT_MAX_STEPS];
    let mut ordered_count = 0usize;

    let mut cursor = start;
    while cursor < end {
        if ordered_count >= BOOT_INIT_MAX_STEPS {
            kernel_panic("Boot init: too many steps for phase");
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

#[no_mangle]
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

#[no_mangle]
pub extern "C" fn boot_get_memmap() -> *const limine_protocol::LimineMemmapResponse {
    boot_state().ctx.memmap
}

#[no_mangle]
pub extern "C" fn boot_get_hhdm_offset() -> u64 {
    boot_state().ctx.hhdm_offset
}

#[no_mangle]
pub extern "C" fn boot_get_cmdline() -> *const c_char {
    boot_state()
        .ctx
        .cmdline
        .map(|s| s.as_ptr() as *const c_char)
        .unwrap_or(ptr::null())
}

#[no_mangle]
pub extern "C" fn boot_mark_initialized() {
    boot_state_mut().initialized = true;
}

#[no_mangle]
pub extern "C" fn is_kernel_initialized() -> i32 {
    boot_state().initialized as i32
}

#[no_mangle]
pub extern "C" fn get_initialization_progress() -> i32 {
    if boot_state().initialized {
        100
    } else {
        50
    }
}

#[no_mangle]
pub extern "C" fn report_kernel_status() {
    if boot_state().initialized {
        boot_info(b"SlopOS: Kernel status - INITIALIZED\0");
    } else {
        boot_info(b"SlopOS: Kernel status - INITIALIZING\0");
    }
}

extern "C" {
    fn start_scheduler() -> i32;
}

fn boot_step_serial_init() -> i32 {
    serial::init();
    slopos_lib::klog_attach_serial();
    boot_debug(b"Serial console ready on COM1\0");
    0
}

fn boot_step_boot_banner() -> i32 {
    boot_info(b"SlopOS Kernel Started!\0");
    boot_info(b"Booting via Limine Protocol...\0");
    0
}

fn boot_step_limine_protocol() -> i32 {
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

fn boot_step_boot_config() -> i32 {
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

    0
}

boot_init_step!(early_hw, b"serial\0", boot_step_serial_init);
boot_init_step!(early_hw, b"boot banner\0", boot_step_boot_banner);
boot_init_step!(early_hw, b"limine\0", boot_step_limine_protocol);
boot_init_step!(early_hw, b"boot config\0", boot_step_boot_config);

#[no_mangle]
pub extern "C" fn kernel_main() {
    wl_currency::reset();

    if boot_init_run_all() != 0 {
        kernel_panic("Boot initialization failed");
    }

    if unsafe { klog_is_enabled(KlogLevel::Info) } != 0 {
        unsafe { klog_newline() };
    }

    boot_info(b"=== KERNEL BOOT SUCCESSFUL ===\0");
    boot_info(b"Operational subsystems: serial, interrupts, memory, scheduler, shell\0");
    boot_info(b"Graphics: framebuffer required and active\0");
    boot_info(b"Kernel initialization complete - ALL SYSTEMS OPERATIONAL!\0");
    boot_info(b"The kernel has initialized. Handing over to scheduler...\0");
    boot_info(b"Starting scheduler...\0");

    if unsafe { klog_is_enabled(KlogLevel::Info) } != 0 {
        unsafe { klog_newline() };
    }

    let rc = unsafe { start_scheduler() };
    if rc != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"ERROR: Scheduler startup failed\n\0".as_ptr() as *const c_char,
            );
        }
        kernel_panic("Scheduler startup failed");
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"WARNING: Scheduler exited unexpectedly\n\0".as_ptr() as *const c_char,
        );
    }

    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

#[no_mangle]
pub extern "C" fn kernel_main_no_multiboot() {
    kernel_main();
}
