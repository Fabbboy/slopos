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

    #[inline(always)]
    pub fn read_rsp() -> u64 {
        let rsp: u64;
        unsafe {
            asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
        }
        rsp
    }

    #[inline(always)]
    pub fn read_r15() -> u64 {
        let r15: u64;
        unsafe {
            asm!("mov {}, r15", out(reg) r15, options(nomem, nostack, preserves_flags));
        }
        r15
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct RegSnapshot {
        pub rax: u64,
        pub rbx: u64,
        pub rcx: u64,
        pub rdx: u64,
        pub rsi: u64,
        pub rdi: u64,
        pub rbp: u64,
        pub rsp: u64,
        pub r8: u64,
        pub r9: u64,
        pub r10: u64,
        pub r11: u64,
        pub r12: u64,
        pub r13: u64,
        pub r14: u64,
        pub r15: u64,
    }

    #[inline(never)]
    pub fn snapshot_regs() -> RegSnapshot {
        let (rax, rbx, rcx, rdx): (u64, u64, u64, u64);
        let (rsi, rdi, rbp, rsp): (u64, u64, u64, u64);
        let (r8, r9, r10, r11): (u64, u64, u64, u64);
        let (r12, r13, r14, r15): (u64, u64, u64, u64);
        unsafe {
            asm!(
                "mov {0}, rax",
                "mov {1}, rbx",
                "mov {2}, rcx",
                "mov {3}, rdx",
                out(reg) rax,
                out(reg) rbx,
                out(reg) rcx,
                out(reg) rdx,
                options(nomem, nostack, preserves_flags)
            );
            asm!(
                "mov {0}, rsi",
                "mov {1}, rdi",
                "mov {2}, rbp",
                "mov {3}, rsp",
                out(reg) rsi,
                out(reg) rdi,
                out(reg) rbp,
                out(reg) rsp,
                options(nomem, nostack, preserves_flags)
            );
            asm!(
                "mov {0}, r8",
                "mov {1}, r9",
                "mov {2}, r10",
                "mov {3}, r11",
                out(reg) r8,
                out(reg) r9,
                out(reg) r10,
                out(reg) r11,
                options(nomem, nostack, preserves_flags)
            );
            asm!(
                "mov {0}, r12",
                "mov {1}, r13",
                "mov {2}, r14",
                "mov {3}, r15",
                out(reg) r12,
                out(reg) r13,
                out(reg) r14,
                out(reg) r15,
                options(nomem, nostack, preserves_flags)
            );
        }
        RegSnapshot {
            rax, rbx, rcx, rdx,
            rsi, rdi, rbp, rsp,
            r8, r9, r10, r11,
            r12, r13, r14, r15,
        }
    }
}

pub mod io;
pub mod ports;

pub mod ffi {
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
pub mod service_cell;
pub mod service_macro;
pub mod spinlock;
pub mod stacktrace;
pub mod string;
pub mod sync;
pub mod wl_currency;

#[doc(hidden)]
pub use paste;

pub use alignment::{align_down_u64, align_down_usize, align_up_u64, align_up_usize};
pub use alignment::{align_down_usize as align_down, align_up_usize as align_up};
pub use kdiag::kdiag_dump_interrupt_frame;
pub use kdiag::{InterruptFrame, KDIAG_STACK_TRACE_DEPTH, kdiag_timestamp};
pub use klog::{
    KlogLevel, klog_attach_serial, klog_get_level, klog_init, klog_is_enabled, klog_newline,
    klog_set_level,
};
pub use math::{abs_i32, max_i32, max_u32, min_i32, min_u32};
pub use ports::COM1;
pub use ring_buffer::RingBuffer;
pub use service_cell::ServiceCell;
pub use spinlock::{IrqMutex, IrqMutexGuard, Spinlock};
pub use stacktrace::StacktraceEntry;
pub use sync::{
    CleanLockToken, L0, L1, L2, L3, L4, L5, Level, LockToken, Lower, Mutex as OrderedMutex,
    MutexGuard as OrderedMutexGuard,
};
