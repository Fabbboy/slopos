use core::arch::asm;
use core::ffi::c_char;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_lib::{cpu, io, klog_printf, KlogLevel};

static SHUTDOWN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static INTERRUPTS_QUIESCED: AtomicBool = AtomicBool::new(false);
static SERIAL_DRAINED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn scheduler_shutdown();
    fn task_shutdown_all() -> i32;
    fn task_set_current(task: *mut core::ffi::c_void);

    fn apic_is_available() -> i32;
    fn apic_send_eoi();
    fn apic_timer_stop();
    fn apic_disable();

    fn pit_poll_delay_ms(ms: u32);

    fn page_allocator_paint_all(value: u8);
}

fn serial_flush() {
    // Best-effort drain by waiting for line status transmit empty bit.
    const LINE_STATUS_PORT_OFFSET: u16 = 5;
    const COM1_BASE: u16 = 0x3F8;
    for _ in 0..1024 {
        let lsr = unsafe { io::inb(COM1_BASE + LINE_STATUS_PORT_OFFSET) };
        if (lsr & 0x40) != 0 {
            break;
        }
        cpu::pause();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kernel_quiesce_interrupts() {
    cpu::disable_interrupts();
    if INTERRUPTS_QUIESCED.swap(true, Ordering::SeqCst) {
        return;
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Kernel shutdown: quiescing interrupt controllers\n\0".as_ptr() as *const c_char,
        );
    }

    unsafe {
        if apic_is_available() != 0 {
            apic_send_eoi();
            apic_timer_stop();
            apic_disable();
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kernel_drain_serial_output() {
    if SERIAL_DRAINED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Kernel shutdown: draining serial output\n\0".as_ptr() as *const c_char,
        );
    }
    serial_flush();
}

#[unsafe(no_mangle)]
pub extern "C" fn kernel_shutdown(reason: *const c_char) {
    cpu::disable_interrupts();

    if SHUTDOWN_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        kernel_quiesce_interrupts();
        kernel_drain_serial_output();
        halt();
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"=== Kernel Shutdown Requested ===\n\0".as_ptr() as *const c_char,
        );
        if !reason.is_null() {
            klog_printf(
                KlogLevel::Info,
                b"Reason: %s\n\0".as_ptr() as *const c_char,
                reason,
            );
        }
    }

    unsafe {
        scheduler_shutdown();
    }

    unsafe {
        if task_shutdown_all() != 0 {
            klog_printf(
                KlogLevel::Info,
                b"Warning: Failed to terminate one or more tasks\n\0".as_ptr() as *const c_char,
            );
        }
        task_set_current(core::ptr::null_mut());
    }

    kernel_quiesce_interrupts();
    kernel_drain_serial_output();

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Kernel shutdown complete. Halting processors.\n\0".as_ptr() as *const c_char,
        );
    }

    halt();
}

fn halt() -> ! {
    loop {
        unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kernel_reboot(reason: *const c_char) {
    cpu::disable_interrupts();

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"=== Kernel Reboot Requested ===\n\0".as_ptr() as *const c_char,
        );
        if !reason.is_null() {
            klog_printf(
                KlogLevel::Info,
                b"Reason: %s\n\0".as_ptr() as *const c_char,
                reason,
            );
        }
    }

    kernel_drain_serial_output();

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Rebooting via keyboard controller...\n\0".as_ptr() as *const c_char,
        );
    }

    unsafe {
        pit_poll_delay_ms(50);
        io::outb(0x64, 0xFE);
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Keyboard reset failed, attempting triple fault...\n\0".as_ptr() as *const c_char,
        );
    }

    #[repr(C, packed)]
    struct InvalidIdt {
        limit: u16,
        base: u64,
    }

    let invalid_idt = InvalidIdt { limit: 0, base: 0 };
    unsafe {
        asm!("lidt [{}]", in(reg) &invalid_idt, options(nostack, preserves_flags));
        asm!("int3", options(nostack, preserves_flags));
    }

    halt();
}

#[unsafe(no_mangle)]
pub extern "C" fn execute_kernel() {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"=== EXECUTING KERNEL PURIFICATION RITUAL ===\n\0".as_ptr() as *const c_char,
        );
        klog_printf(
            KlogLevel::Info,
            b"Painting memory with the essence of slop (0x69)...\n\0".as_ptr() as *const c_char,
        );
        page_allocator_paint_all(0x69);
        klog_printf(
            KlogLevel::Info,
            b"Memory purification complete. The slop has been painted eternal.\n\0".as_ptr()
                as *const c_char,
        );
    }
}
