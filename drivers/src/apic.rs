#![allow(dead_code)]

use core::ffi::c_char;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_lib::{cpu, klog_printf, KlogLevel};

use crate::wl_currency;

extern "C" {
    fn is_hhdm_available() -> i32;
    fn get_hhdm_offset() -> u64;
}

// CPUID feature flags (leaf 1)
const CPUID_FEAT_EDX_APIC: u32 = 1 << 9;
const CPUID_FEAT_ECX_X2APIC: u32 = 1 << 21;

// APIC MSRs
const MSR_APIC_BASE: u32 = 0x1B;

// APIC base register flags
const APIC_BASE_BSP: u64 = 1 << 8;
const APIC_BASE_X2APIC: u64 = 1 << 10;
const APIC_BASE_GLOBAL_ENABLE: u64 = 1 << 11;
const APIC_BASE_ADDR_MASK: u64 = 0xFFFF_F000;

// Local APIC register offsets
const LAPIC_ID: u32 = 0x020;
const LAPIC_VERSION: u32 = 0x030;
const LAPIC_SPURIOUS: u32 = 0x0F0;
const LAPIC_ESR: u32 = 0x280;
const LAPIC_LVT_TIMER: u32 = 0x320;
const LAPIC_LVT_PERFCNT: u32 = 0x340;
const LAPIC_LVT_LINT0: u32 = 0x350;
const LAPIC_LVT_LINT1: u32 = 0x360;
const LAPIC_LVT_ERROR: u32 = 0x370;
const LAPIC_TIMER_ICR: u32 = 0x380;
const LAPIC_TIMER_CCR: u32 = 0x390;
const LAPIC_TIMER_DCR: u32 = 0x3E0;
const LAPIC_EOI: u32 = 0x0B0;

// LAPIC flags
const LAPIC_SPURIOUS_ENABLE: u32 = 1 << 8;
const LAPIC_LVT_MASKED: u32 = 1 << 16;
const LAPIC_LVT_DELIVERY_MODE_EXTINT: u32 = 0x7 << 8;

// Timer modes/divisors
const LAPIC_TIMER_PERIODIC: u32 = 0x0002_0000;
const LAPIC_TIMER_DIV_16: u32 = 0x3;

static APIC_AVAILABLE: AtomicBool = AtomicBool::new(false);
static X2APIC_AVAILABLE: AtomicBool = AtomicBool::new(false);
static APIC_ENABLED: AtomicBool = AtomicBool::new(false);
static APIC_BASE_ADDRESS: AtomicU64 = AtomicU64::new(0);
static APIC_BASE_PHYSICAL: AtomicU64 = AtomicU64::new(0);

#[inline]
fn log(level: KlogLevel, msg: &[u8]) {
    unsafe { klog_printf(level, msg.as_ptr() as *const c_char) };
}

#[inline]
fn hhdm_virt_for(phys: u64) -> Option<u64> {
    if phys == 0 {
        return None;
    }
    unsafe {
        if is_hhdm_available() != 0 {
            Some(phys + get_hhdm_offset())
        } else {
            None
        }
    }
}

pub fn detect() -> bool {
    log(
        KlogLevel::Debug,
        b"APIC: Detecting Local APIC availability...\0",
    );

    let (_, _, ecx, edx) = cpu::cpuid(1);
    if edx & CPUID_FEAT_EDX_APIC == 0 {
        log(KlogLevel::Debug, b"APIC: Local APIC is not available\0");
        APIC_AVAILABLE.store(false, Ordering::Relaxed);
        return false;
    }

    APIC_AVAILABLE.store(true, Ordering::Relaxed);
    let x2 = (ecx & CPUID_FEAT_ECX_X2APIC) != 0;
    X2APIC_AVAILABLE.store(x2, Ordering::Relaxed);

    let apic_base_msr = cpu::read_msr(MSR_APIC_BASE);
    let apic_phys = apic_base_msr & APIC_BASE_ADDR_MASK;
    APIC_BASE_PHYSICAL.store(apic_phys, Ordering::Relaxed);

    if let Some(virt) = hhdm_virt_for(apic_phys) {
        APIC_BASE_ADDRESS.store(virt, Ordering::Relaxed);
        unsafe {
            klog_printf(
                KlogLevel::Debug,
                b"APIC: Physical base: 0x%llx, Virtual base (HHDM): 0x%llx\n\0".as_ptr()
                    as *const c_char,
                apic_phys,
                virt,
            );
            klog_printf(
                KlogLevel::Debug,
                b"APIC: MSR flags:%s%s%s\n\0".as_ptr() as *const c_char,
                if apic_base_msr & APIC_BASE_BSP != 0 {
                    b" BSP\0".as_ptr() as *const c_char
                } else {
                    b"\0".as_ptr() as *const c_char
                },
                if apic_base_msr & APIC_BASE_X2APIC != 0 {
                    b" X2APIC\0".as_ptr() as *const c_char
                } else {
                    b"\0".as_ptr() as *const c_char
                },
                if apic_base_msr & APIC_BASE_GLOBAL_ENABLE != 0 {
                    b" ENABLED\0".as_ptr() as *const c_char
                } else {
                    b"\0".as_ptr() as *const c_char
                },
            );
        }
        true
    } else {
        log(
            KlogLevel::Info,
            b"APIC: ERROR - HHDM not available, cannot map APIC registers\0",
        );
        APIC_AVAILABLE.store(false, Ordering::Relaxed);
        false
    }
}

pub fn init() -> i32 {
    if !is_available() {
        log(
            KlogLevel::Info,
            b"APIC: Cannot initialize - APIC not available\0",
        );
        wl_currency::award_loss();
        return -1;
    }

    log(KlogLevel::Debug, b"APIC: Initializing Local APIC\0");

    let mut apic_base_msr = cpu::read_msr(MSR_APIC_BASE);
    if apic_base_msr & APIC_BASE_GLOBAL_ENABLE == 0 {
        apic_base_msr |= APIC_BASE_GLOBAL_ENABLE;
        cpu::write_msr(MSR_APIC_BASE, apic_base_msr);
        log(KlogLevel::Debug, b"APIC: Enabled APIC globally via MSR\0");
    }

    enable();

    // Mask all LVT entries to prevent spurious interrupts.
    write_register(LAPIC_LVT_TIMER, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_LINT0, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_LINT1, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_ERROR, LAPIC_LVT_MASKED);
    write_register(LAPIC_LVT_PERFCNT, LAPIC_LVT_MASKED);

    // Route legacy PIC interrupts through LINT0 in ExtINT mode.
    write_register(LAPIC_LVT_LINT0, LAPIC_LVT_DELIVERY_MODE_EXTINT);

    // Clear error status register twice per Intel SDM guidance.
    write_register(LAPIC_ESR, 0);
    write_register(LAPIC_ESR, 0);

    send_eoi();

    let apic_id = get_id();
    let apic_version = get_version();
    unsafe {
        klog_printf(
            KlogLevel::Debug,
            b"APIC: ID: 0x%x, Version: 0x%x\n\0".as_ptr() as *const c_char,
            apic_id,
            apic_version,
        );
    }

    APIC_ENABLED.store(true, Ordering::Relaxed);
    log(KlogLevel::Debug, b"APIC: Initialization complete\0");
    wl_currency::award_win();
    0
}

pub fn is_available() -> bool {
    APIC_AVAILABLE.load(Ordering::Relaxed)
}

pub fn is_x2apic_available() -> bool {
    X2APIC_AVAILABLE.load(Ordering::Relaxed)
}

pub fn is_bsp() -> bool {
    if !is_available() {
        return false;
    }
    let apic_base_msr = cpu::read_msr(MSR_APIC_BASE);
    (apic_base_msr & APIC_BASE_BSP) != 0
}

pub fn is_enabled() -> bool {
    APIC_ENABLED.load(Ordering::Relaxed)
}

pub fn enable() {
    if !is_available() {
        return;
    }
    let mut spurious = read_register(LAPIC_SPURIOUS);
    spurious |= LAPIC_SPURIOUS_ENABLE;
    spurious |= 0xFF;
    write_register(LAPIC_SPURIOUS, spurious);
    APIC_ENABLED.store(true, Ordering::Relaxed);
    log(KlogLevel::Debug, b"APIC: Local APIC enabled\0");
}

pub fn disable() {
    if !is_available() {
        return;
    }
    let mut spurious = read_register(LAPIC_SPURIOUS);
    spurious &= !LAPIC_SPURIOUS_ENABLE;
    write_register(LAPIC_SPURIOUS, spurious);
    APIC_ENABLED.store(false, Ordering::Relaxed);
    log(KlogLevel::Debug, b"APIC: Local APIC disabled\0");
}

pub fn send_eoi() {
    if !is_enabled() {
        return;
    }
    write_register(LAPIC_EOI, 0);
}

pub fn get_id() -> u32 {
    if !is_available() {
        return 0;
    }
    read_register(LAPIC_ID) >> 24
}

pub fn get_version() -> u32 {
    if !is_available() {
        return 0;
    }
    read_register(LAPIC_VERSION) & 0xFF
}

pub fn timer_init(vector: u32, frequency: u32) {
    if !is_enabled() {
        return;
    }
    unsafe {
        klog_printf(
            KlogLevel::Debug,
            b"APIC: Initializing timer with vector 0x%x and frequency %u\n\0".as_ptr()
                as *const c_char,
            vector,
            frequency,
        );
    }

    timer_set_divisor(LAPIC_TIMER_DIV_16);

    let lvt_timer = vector | LAPIC_TIMER_PERIODIC;
    write_register(LAPIC_LVT_TIMER, lvt_timer);

    let initial_count = 1_000_000u32.saturating_div(frequency.max(1));
    timer_start(initial_count);
    log(KlogLevel::Debug, b"APIC: Timer initialized\0");
}

pub fn timer_start(initial_count: u32) {
    if !is_enabled() {
        return;
    }
    write_register(LAPIC_TIMER_ICR, initial_count);
}

pub fn timer_stop() {
    if !is_enabled() {
        return;
    }
    write_register(LAPIC_TIMER_ICR, 0);
}

pub fn timer_get_current_count() -> u32 {
    if !is_enabled() {
        return 0;
    }
    read_register(LAPIC_TIMER_CCR)
}

pub fn timer_set_divisor(divisor: u32) {
    if !is_enabled() {
        return;
    }
    write_register(LAPIC_TIMER_DCR, divisor);
}

pub fn get_base_address() -> u64 {
    APIC_BASE_ADDRESS.load(Ordering::Relaxed)
}

pub fn set_base_address(base: u64) {
    if !is_available() {
        return;
    }
    let masked_base = base & APIC_BASE_ADDR_MASK;
    let mut apic_base_msr = cpu::read_msr(MSR_APIC_BASE);
    apic_base_msr = (apic_base_msr & !APIC_BASE_ADDR_MASK) | masked_base;
    cpu::write_msr(MSR_APIC_BASE, apic_base_msr);

    APIC_BASE_PHYSICAL.store(masked_base, Ordering::Relaxed);
    if let Some(virt) = hhdm_virt_for(masked_base) {
        APIC_BASE_ADDRESS.store(virt, Ordering::Relaxed);
    } else {
        APIC_BASE_ADDRESS.store(0, Ordering::Relaxed);
    }
}

pub fn read_register(reg: u32) -> u32 {
    let base = APIC_BASE_ADDRESS.load(Ordering::Relaxed);
    if !is_available() || base == 0 {
        return 0;
    }
    let reg_ptr = (base + reg as u64) as *const u32;
    unsafe { read_volatile(reg_ptr) }
}

pub fn write_register(reg: u32, value: u32) {
    let base = APIC_BASE_ADDRESS.load(Ordering::Relaxed);
    if !is_available() || base == 0 {
        return;
    }
    let reg_ptr = (base + reg as u64) as *mut u32;
    unsafe { write_volatile(reg_ptr, value) };
}

pub fn dump_state() {
    log(KlogLevel::Info, b"=== APIC STATE DUMP ===\0");
    if !is_available() {
        log(KlogLevel::Info, b"APIC: Not available\0");
        log(KlogLevel::Info, b"=== END APIC STATE DUMP ===\0");
        return;
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"APIC Available: Yes, x2APIC: %s\n\0".as_ptr() as *const c_char,
            if is_x2apic_available() {
                b"Yes\0".as_ptr() as *const c_char
            } else {
                b"No\0".as_ptr() as *const c_char
            },
        );
        klog_printf(
            KlogLevel::Info,
            b"APIC Enabled: %s\n\0".as_ptr() as *const c_char,
            if is_enabled() {
                b"Yes\0".as_ptr() as *const c_char
            } else {
                b"No\0".as_ptr() as *const c_char
            },
        );
        klog_printf(
            KlogLevel::Info,
            b"Bootstrap Processor: %s\n\0".as_ptr() as *const c_char,
            if is_bsp() {
                b"Yes\0".as_ptr() as *const c_char
            } else {
                b"No\0".as_ptr() as *const c_char
            },
        );
        klog_printf(
            KlogLevel::Info,
            b"Base Address: 0x%llx\n\0".as_ptr() as *const c_char,
            get_base_address(),
        );
    }

    if is_enabled() {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"APIC ID: 0x%x\n\0".as_ptr() as *const c_char,
                get_id(),
            );
            klog_printf(
                KlogLevel::Info,
                b"APIC Version: 0x%x\n\0".as_ptr() as *const c_char,
                get_version(),
            );

            let spurious = read_register(LAPIC_SPURIOUS);
            klog_printf(
                KlogLevel::Info,
                b"Spurious Vector Register: 0x%x\n\0".as_ptr() as *const c_char,
                spurious,
            );

            let esr = read_register(LAPIC_ESR);
            klog_printf(
                KlogLevel::Info,
                b"Error Status Register: 0x%x\n\0".as_ptr() as *const c_char,
                esr,
            );

            let lvt_timer = read_register(LAPIC_LVT_TIMER);
            klog_printf(
                KlogLevel::Info,
                b"Timer LVT: 0x%x%s\n\0".as_ptr() as *const c_char,
                lvt_timer,
                if lvt_timer & LAPIC_LVT_MASKED != 0 {
                    b" (MASKED)\0".as_ptr() as *const c_char
                } else {
                    b"\0".as_ptr() as *const c_char
                },
            );

            let timer_count = timer_get_current_count();
            klog_printf(
                KlogLevel::Info,
                b"Timer Current Count: 0x%x\n\0".as_ptr() as *const c_char,
                timer_count,
            );
        }
    }

    log(KlogLevel::Info, b"=== END APIC STATE DUMP ===\0");
}

#[no_mangle]
pub extern "C" fn apic_detect() -> i32 {
    if detect() {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn apic_init() -> i32 {
    init()
}

#[no_mangle]
pub extern "C" fn apic_is_available() -> i32 {
    if is_available() {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn apic_is_x2apic_available() -> i32 {
    if is_x2apic_available() {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn apic_is_bsp() -> i32 {
    if is_bsp() {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn apic_is_enabled() -> i32 {
    if is_enabled() {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn apic_enable() {
    enable();
}

#[no_mangle]
pub extern "C" fn apic_disable() {
    disable();
}

#[no_mangle]
pub extern "C" fn apic_send_eoi() {
    send_eoi();
}

#[no_mangle]
pub extern "C" fn apic_get_id() -> u32 {
    get_id()
}

#[no_mangle]
pub extern "C" fn apic_get_version() -> u32 {
    get_version()
}

#[no_mangle]
pub extern "C" fn apic_timer_init(vector: u32, frequency: u32) {
    timer_init(vector, frequency);
}

#[no_mangle]
pub extern "C" fn apic_timer_start(initial_count: u32) {
    timer_start(initial_count);
}

#[no_mangle]
pub extern "C" fn apic_timer_stop() {
    timer_stop();
}

#[no_mangle]
pub extern "C" fn apic_timer_get_current_count() -> u32 {
    timer_get_current_count()
}

#[no_mangle]
pub extern "C" fn apic_timer_set_divisor(divisor: u32) {
    timer_set_divisor(divisor);
}

#[no_mangle]
pub extern "C" fn apic_dump_state() {
    dump_state();
}

#[no_mangle]
pub extern "C" fn apic_get_base_address() -> u64 {
    get_base_address()
}

#[no_mangle]
pub extern "C" fn apic_set_base_address(base: u64) {
    set_base_address(base);
}

#[no_mangle]
pub extern "C" fn apic_read_register(reg: u32) -> u32 {
    read_register(reg)
}

#[no_mangle]
pub extern "C" fn apic_write_register(reg: u32, value: u32) {
    write_register(reg, value);
}
