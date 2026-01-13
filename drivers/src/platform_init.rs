use core::ffi::{c_char, c_int, c_void};

use crate::{apic, ioapic, pit, random, serial};
use slopos_core::irq;
use slopos_core::platform::{PlatformServices, register_platform};

use spin::Once;

static GDT_SET_RSP0: Once<fn(u64)> = Once::new();
static KERNEL_SHUTDOWN: Once<fn(*const c_char) -> !> = Once::new();
static KERNEL_REBOOT: Once<fn(*const c_char) -> !> = Once::new();
static IS_RSDP_AVAILABLE: Once<fn() -> bool> = Once::new();
static GET_RSDP_ADDRESS: Once<fn() -> *const c_void> = Once::new();
static IS_KERNEL_INITIALIZED: Once<fn() -> bool> = Once::new();
static IDT_GET_GATE: Once<fn(u8, *mut c_void) -> c_int> = Once::new();

pub fn register_gdt_rsp0_callback(cb: fn(u64)) {
    GDT_SET_RSP0.call_once(|| cb);
}

pub fn register_kernel_shutdown_callback(cb: fn(*const c_char) -> !) {
    KERNEL_SHUTDOWN.call_once(|| cb);
}

pub fn register_kernel_reboot_callback(cb: fn(*const c_char) -> !) {
    KERNEL_REBOOT.call_once(|| cb);
}

pub fn register_rsdp_callbacks(is_available: fn() -> bool, get_address: fn() -> *const c_void) {
    IS_RSDP_AVAILABLE.call_once(|| is_available);
    GET_RSDP_ADDRESS.call_once(|| get_address);
}

pub fn register_kernel_initialized_callback(cb: fn() -> bool) {
    IS_KERNEL_INITIALIZED.call_once(|| cb);
}

pub fn register_idt_get_gate_callback(cb: fn(u8, *mut c_void) -> c_int) {
    IDT_GET_GATE.call_once(|| cb);
}

fn gdt_set_kernel_rsp0_impl(rsp0: u64) {
    if let Some(cb) = GDT_SET_RSP0.get() {
        cb(rsp0);
    }
}

fn kernel_shutdown_impl(reason: *const c_char) -> ! {
    if let Some(cb) = KERNEL_SHUTDOWN.get() {
        cb(reason)
    }
    loop {
        core::hint::spin_loop();
    }
}

fn kernel_reboot_impl(reason: *const c_char) -> ! {
    if let Some(cb) = KERNEL_REBOOT.get() {
        cb(reason)
    }
    loop {
        core::hint::spin_loop();
    }
}

fn is_rsdp_available_impl() -> bool {
    IS_RSDP_AVAILABLE.get().map(|cb| cb()).unwrap_or(false)
}

fn get_rsdp_address_impl() -> *const c_void {
    GET_RSDP_ADDRESS
        .get()
        .map(|cb| cb())
        .unwrap_or(core::ptr::null())
}

fn is_kernel_initialized_impl() -> bool {
    IS_KERNEL_INITIALIZED.get().map(|cb| cb()).unwrap_or(false)
}

fn idt_get_gate_impl(vector: u8, entry: *mut c_void) -> c_int {
    IDT_GET_GATE.get().map(|cb| cb(vector, entry)).unwrap_or(-1)
}

static PLATFORM_SERVICES: PlatformServices = PlatformServices {
    timer_get_ticks: || irq::get_timer_ticks(),
    timer_get_frequency: || pit::pit_get_frequency(),
    timer_poll_delay_ms: |ms| pit::pit_poll_delay_ms(ms),
    timer_sleep_ms: |ms| pit::pit_sleep_ms(ms),
    timer_enable_irq: || pit::pit_enable_irq(),
    timer_disable_irq: || pit::pit_disable_irq(),
    console_putc: |c| serial::serial_putc_com1(c),
    console_puts: |s| {
        for &c in s {
            serial::serial_putc_com1(c);
        }
    },
    rng_next: || random::random_next(),
    gdt_set_kernel_rsp0: gdt_set_kernel_rsp0_impl,
    kernel_shutdown: kernel_shutdown_impl,
    kernel_reboot: kernel_reboot_impl,
    is_rsdp_available: is_rsdp_available_impl,
    get_rsdp_address: get_rsdp_address_impl,
    is_kernel_initialized: is_kernel_initialized_impl,
    idt_get_gate: idt_get_gate_impl,
    irq_send_eoi: || apic::send_eoi(),
    irq_mask_gsi: |gsi| ioapic::mask_gsi(gsi),
    irq_unmask_gsi: |gsi| ioapic::unmask_gsi(gsi),
};

pub fn init_platform_services() {
    register_platform(&PLATFORM_SERVICES);
}
