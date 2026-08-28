use core::ffi::{c_char, c_int, c_void};

use crate::{early_init, gdt, idt, limine_protocol, shutdown};
use slopos_drivers::{apic, hpet, ioapic, random, serial};
use slopos_kernel_services::platform::{PlatformServices, register_platform_services};

fn kernel_shutdown_fn(reason: *const c_char) -> ! {
    shutdown::kernel_shutdown(reason)
}

fn kernel_reboot_fn(reason: *const c_char) -> ! {
    shutdown::kernel_reboot(reason)
}

fn is_rsdp_available_fn() -> bool {
    limine_protocol::is_rsdp_available() != 0
}

fn get_rsdp_address_fn() -> *const c_void {
    limine_protocol::get_rsdp_address()
}

fn get_rsdp_phys_fn() -> u64 {
    limine_protocol::get_rsdp_phys_address()
}

fn is_kernel_initialized_fn() -> bool {
    early_init::is_kernel_initialized() != 0
}

fn idt_get_gate_fn(vector: u8, entry: *mut c_void) -> c_int {
    idt::idt_get_gate_opaque(vector, entry)
}

static PLATFORM_SERVICES: PlatformServices = PlatformServices {
    timer_ticks: || slopos_core::irq::get_timer_ticks(),
    // Only the BSP's ISR advances the counter, so 100 Hz is its real rate.
    timer_frequency: || 100,
    timer_poll_delay_ms: |ms| hpet::delay_ms(ms),
    timer_sleep_ms: |ms| hpet::delay_ms(ms),
    timer_enable_irq: || apic::timer::unmask(),
    timer_disable_irq: || apic::timer::mask(),
    timer_program_next_wakeup_ms: |ms| {
        apic::timer::set_oneshot_ms(slopos_arch::arch::idt::LAPIC_TIMER_VECTOR, ms)
    },
    timer_restore_periodic: || {
        const LAPIC_TIMER_PERIOD_MS: u32 = 10;
        let _ = apic::timer::set_periodic_ms(
            slopos_arch::arch::idt::LAPIC_TIMER_VECTOR,
            LAPIC_TIMER_PERIOD_MS,
        );
    },
    console_putc: |c| serial::serial_putc_com1(c),
    console_puts: |s| {
        for &c in s {
            serial::serial_putc_com1(c);
        }
    },
    console_write_serialized: serial::serial_locked_write_bytes,
    rng_next: || random::random_next(),
    gdt_set_kernel_rsp0: gdt::gdt_set_kernel_rsp0,
    kernel_shutdown: kernel_shutdown_fn,
    kernel_reboot: kernel_reboot_fn,
    is_rsdp_available: is_rsdp_available_fn,
    get_rsdp_address: get_rsdp_address_fn,
    get_rsdp_phys: get_rsdp_phys_fn,
    is_kernel_initialized: is_kernel_initialized_fn,
    idt_get_gate: idt_get_gate_fn,
    irq_send_eoi: || apic::send_eoi(),
    irq_mask_gsi: |gsi| ioapic::mask_gsi(gsi),
    irq_unmask_gsi: |gsi| ioapic::unmask_gsi(gsi),
    clock_monotonic_ns: || hpet::nanoseconds(hpet::read_counter()),
};

pub fn register_boot_services() {
    register_platform_services(&PLATFORM_SERVICES);
    // OSTD owns the *authority* for power -- the witness and the single choke
    // point -- while the sequence stays here, where the ACPI and UEFI state it
    // needs lives. `boot` sits above OSTD, so the mechanism is registered in
    // rather than called out to.
    slopos_ostd::platform::power::register(slopos_ostd::platform::power::PowerOps {
        shutdown: kernel_shutdown_fn,
        reboot: kernel_reboot_fn,
    });
}
