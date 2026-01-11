use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicPtr, Ordering};

#[repr(C)]
pub struct PlatformServices {
    pub timer_get_ticks: fn() -> u64,
    pub timer_get_frequency: fn() -> u32,
    pub timer_poll_delay_ms: fn(u32),
    pub timer_enable_irq: fn(),
    pub timer_disable_irq: fn(),

    pub console_putc: fn(u8),
    pub console_puts: fn(&[u8]),

    pub rng_next: fn() -> u64,

    pub gdt_set_kernel_rsp0: fn(u64),

    pub kernel_panic: fn(*const c_char) -> !,
    pub kernel_shutdown: fn(*const c_char) -> !,
    pub kernel_reboot: fn(*const c_char) -> !,
    pub is_rsdp_available: fn() -> bool,
    pub get_rsdp_address: fn() -> *const c_void,
    pub is_kernel_initialized: fn() -> bool,
    pub idt_get_gate: fn(u8, *mut c_void) -> c_int,

    pub irq_send_eoi: fn(),
    pub irq_mask_gsi: fn(u32) -> i32,
    pub irq_unmask_gsi: fn(u32) -> i32,
}

static PLATFORM: AtomicPtr<PlatformServices> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_platform(services: &'static PlatformServices) {
    let prev = PLATFORM.swap(services as *const _ as *mut _, Ordering::Release);
    assert!(prev.is_null(), "platform already registered");
}

#[inline(always)]
pub fn platform() -> &'static PlatformServices {
    let ptr = PLATFORM.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "platform not initialized");
    unsafe { &*ptr }
}

pub fn is_platform_initialized() -> bool {
    !PLATFORM.load(Ordering::Acquire).is_null()
}

#[inline(always)]
pub fn timer_ticks() -> u64 {
    (platform().timer_get_ticks)()
}

#[inline(always)]
pub fn timer_frequency() -> u32 {
    (platform().timer_get_frequency)()
}

#[inline(always)]
pub fn get_time_ms() -> u64 {
    let ticks = timer_ticks();
    let freq = timer_frequency();
    if freq == 0 {
        return 0;
    }
    (ticks * 1000) / freq as u64
}

#[inline(always)]
pub fn timer_enable_irq() {
    (platform().timer_enable_irq)()
}

#[inline(always)]
pub fn timer_disable_irq() {
    (platform().timer_disable_irq)()
}

#[inline(always)]
pub fn console_putc(c: u8) {
    (platform().console_putc)(c)
}

#[inline(always)]
pub fn console_puts(s: &[u8]) {
    (platform().console_puts)(s)
}

#[inline(always)]
pub fn rng_next() -> u64 {
    (platform().rng_next)()
}

#[inline(always)]
pub fn gdt_set_kernel_rsp0(rsp0: u64) {
    (platform().gdt_set_kernel_rsp0)(rsp0)
}

#[inline(always)]
pub fn kernel_panic(msg: *const c_char) -> ! {
    (platform().kernel_panic)(msg)
}

#[inline(always)]
pub fn kernel_shutdown(reason: *const c_char) -> ! {
    (platform().kernel_shutdown)(reason)
}

#[inline(always)]
pub fn kernel_reboot(reason: *const c_char) -> ! {
    (platform().kernel_reboot)(reason)
}

#[inline(always)]
pub fn is_rsdp_available() -> bool {
    (platform().is_rsdp_available)()
}

#[inline(always)]
pub fn get_rsdp_address() -> *const c_void {
    (platform().get_rsdp_address)()
}

#[inline(always)]
pub fn is_kernel_initialized() -> bool {
    (platform().is_kernel_initialized)()
}

#[inline(always)]
pub fn idt_get_gate(vector: u8, entry: *mut c_void) -> c_int {
    (platform().idt_get_gate)(vector, entry)
}
