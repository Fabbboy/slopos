#![no_std]
#![feature(c_variadic)]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod cpu {
    use core::arch::asm;

    #[inline(always)]
    pub fn hlt() {
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }

    #[inline(always)]
    pub fn pause() {
        unsafe {
            asm!("pause", options(nomem, nostack, preserves_flags));
        }
    }

    #[inline(always)]
    pub fn enable_interrupts() {
        unsafe {
            asm!("sti", options(nomem, nostack));
        }
    }

    #[inline(always)]
    pub fn disable_interrupts() {
        unsafe {
            asm!("cli", options(nomem, nostack));
        }
    }

    /// Save RFLAGS and disable interrupts (irqsave pattern).
    /// Returns the saved RFLAGS value.
    #[inline(always)]
    pub fn save_flags_cli() -> u64 {
        let flags: u64;
        unsafe {
            asm!(
                "pushfq",
                "pop {}",
                "cli",
                out(reg) flags,
                options(nomem)
            );
        }
        flags
    }

    /// Restore interrupt flag from saved RFLAGS (irqrestore pattern).
    /// Only re-enables interrupts if they were enabled in the saved flags.
    #[inline(always)]
    pub fn restore_flags(flags: u64) {
        // Check if IF (bit 9) was set in the saved flags
        if flags & (1 << 9) != 0 {
            enable_interrupts();
        }
    }

    #[inline(always)]
    pub fn halt_loop() -> ! {
        loop {
            hlt();
        }
    }

    #[inline(always)]
    pub fn read_rbp() -> u64 {
        let rbp: u64;
        unsafe {
            asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack, preserves_flags));
        }
        rbp
    }

    #[inline(always)]
    pub fn read_cr3() -> u64 {
        let value: u64;
        unsafe {
            asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    #[inline(always)]
    pub fn read_msr(msr: u32) -> u64 {
        let low: u32;
        let high: u32;
        unsafe {
            asm!(
                "rdmsr",
                out("eax") low,
                out("edx") high,
                in("ecx") msr,
                options(nomem, nostack, preserves_flags)
            );
        }
        ((high as u64) << 32) | (low as u64)
    }

    #[inline(always)]
    pub fn write_msr(msr: u32, value: u64) {
        let low = value as u32;
        let high = (value >> 32) as u32;
        unsafe {
            asm!(
                "wrmsr",
                in("eax") low,
                in("edx") high,
                in("ecx") msr,
                options(nomem, nostack, preserves_flags)
            );
        }
    }

    #[inline(always)]
    pub fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
        unsafe {
            let res = core::arch::x86_64::__cpuid(leaf);
            (res.eax, res.ebx, res.ecx, res.edx)
        }
    }
}

pub mod io {
    use core::arch::asm;

    #[inline(always)]
    pub unsafe fn outb(port: u16, value: u8) {
        unsafe {
            asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags)
            );
        }
    }

    #[inline(always)]
    pub unsafe fn inb(port: u16) -> u8 {
        unsafe {
            let value: u8;
            asm!(
                "in al, dx",
                out("al") value,
                in("dx") port,
                options(nomem, nostack, preserves_flags)
            );
            value
        }
    }

    #[inline(always)]
    pub unsafe fn outw(port: u16, value: u16) {
        unsafe {
            asm!(
                "out dx, ax",
                in("dx") port,
                in("ax") value,
                options(nomem, nostack, preserves_flags)
            );
        }
    }

    #[inline(always)]
    pub unsafe fn inw(port: u16) -> u16 {
        unsafe {
            let value: u16;
            asm!(
                "in ax, dx",
                out("ax") value,
                in("dx") port,
                options(nomem, nostack, preserves_flags)
            );
            value
        }
    }

    #[inline(always)]
    pub unsafe fn io_wait() {
        unsafe {
            outb(0x80, 0);
        }
    }
    pub extern "C" fn cpuid_ffi(
        leaf: u32,
        eax: *mut u32,
        ebx: *mut u32,
        ecx: *mut u32,
        edx: *mut u32,
    ) {
        let (a, b, c, d) = crate::cpu::cpuid(leaf);
        unsafe {
            if !eax.is_null() {
                *eax = a;
            }
            if !ebx.is_null() {
                *ebx = b;
            }
            if !ecx.is_null() {
                *ecx = c;
            }
            if !edx.is_null() {
                *edx = d;
            }
        }
    }
    pub extern "C" fn cpu_read_msr_ffi(msr: u32) -> u64 {
        crate::cpu::read_msr(msr)
    }
}

pub mod tsc {
    use core::arch::asm;

    #[inline(always)]
    pub fn rdtsc() -> u64 {
        let lo: u32;
        let hi: u32;
        unsafe {
            asm!(
                "rdtsc",
                out("eax") lo,
                out("edx") hi,
                options(nomem, nostack, preserves_flags)
            );
        }
        ((hi as u64) << 32) | (lo as u64)
    }
}

pub mod alignment;
pub mod kdiag;
pub mod klog;
pub mod math;
pub mod memory;
pub mod numfmt;
pub mod ring_buffer;
pub mod spinlock;
pub mod stacktrace;
pub mod string;

pub use alignment::{align_down_u64, align_down_usize, align_up_u64, align_up_usize};
pub use alignment::{align_down_usize as align_down, align_up_usize as align_up};
pub use kdiag::kdiag_dump_interrupt_frame;
pub use kdiag::{InterruptFrame, KDIAG_STACK_TRACE_DEPTH, kdiag_timestamp};
pub use klog::{
    COM1_BASE, KlogLevel, klog_attach_serial, klog_get_level, klog_init, klog_is_enabled,
    klog_newline, klog_set_level,
};
pub use math::{abs_i32, max_i32, max_u32, min_i32, min_u32};
pub use ring_buffer::RingBuffer;
pub use spinlock::{IrqMutex, IrqMutexGuard, Spinlock};
pub use stacktrace::StacktraceEntry;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FramebufferInfo {
    pub address: *mut u8,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
}
