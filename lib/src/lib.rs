#![no_std]
#![feature(c_variadic)]
#![forbid(unsafe_op_in_unsafe_fn)]

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
        let (mut eax, mut ebx, mut ecx, mut edx) = (0u32, 0u32, 0u32, 0u32);
        unsafe {
            asm!(
                "cpuid",
                inout("eax") leaf => eax,
                out("ebx") ebx,
                out("ecx") ecx,
                out("edx") edx,
                options(nomem, nostack)
            );
        }
        (eax, ebx, ecx, edx)
    }
}

pub mod io {
    use core::arch::asm;

    #[inline(always)]
    pub unsafe fn outb(port: u16, value: u8) {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }

    #[inline(always)]
    pub unsafe fn inb(port: u16) -> u8 {
        let value: u8;
        asm!(
            "in al, dx",
            out("al") value,
            in("dx") port,
            options(nomem, nostack, preserves_flags)
        );
        value
    }

    #[inline(always)]
    pub unsafe fn outw(port: u16, value: u16) {
        asm!(
            "out dx, ax",
            in("dx") port,
            in("ax") value,
            options(nomem, nostack, preserves_flags)
        );
    }

    #[inline(always)]
    pub unsafe fn inw(port: u16) -> u16 {
        let value: u16;
        asm!(
            "in ax, dx",
            out("ax") value,
            in("dx") port,
            options(nomem, nostack, preserves_flags)
        );
        value
    }

    #[inline(always)]
    pub unsafe fn io_wait() {
        outb(0x80, 0);
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

pub mod string;
pub mod memory;
pub mod numfmt;
pub mod klog;
pub mod stacktrace;
pub mod kdiag;
pub mod alignment;
pub mod math;
pub mod ring_buffer;
pub mod spinlock;
pub mod syscall_numbers;
pub mod user_syscall_defs;
pub mod user_syscall;

pub use kdiag::{interrupt_frame, KDIAG_STACK_TRACE_DEPTH};
pub use klog::{
    klog_attach_serial, klog_get_level, klog_init, klog_is_enabled, klog_newline, klog_printf,
    klog_set_level, KlogLevel,
};
pub use stacktrace::stacktrace_entry;
pub use alignment::{align_down_u64, align_up_u64};
pub use math::{abs_i32, max_i32, max_u32, min_i32, min_u32};
pub use ring_buffer::RingBuffer;
pub use spinlock::Spinlock;
pub use syscall_numbers::*;
pub use user_syscall::*;
pub use user_syscall_defs::*;

#[inline(always)]
pub const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

#[inline(always)]
pub const fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

