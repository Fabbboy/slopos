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

    /// Read RFLAGS register without modifying interrupt state.
    /// Use `save_flags_cli()` if you need to disable interrupts atomically.
    #[inline(always)]
    pub fn read_rflags() -> u64 {
        let flags: u64;
        unsafe {
            asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags));
        }
        flags
    }

    #[inline(always)]
    pub fn are_interrupts_enabled() -> bool {
        (read_rflags() & (1 << 9)) != 0
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
    pub fn read_cr0() -> u64 {
        let value: u64;
        unsafe {
            asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    #[inline(always)]
    pub fn write_cr0(value: u64) {
        unsafe {
            asm!("mov cr0, {}", in(reg) value, options(nostack, preserves_flags));
        }
    }

    #[inline(always)]
    pub fn read_cr2() -> u64 {
        let value: u64;
        unsafe {
            asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
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
    pub fn write_cr3(value: u64) {
        unsafe {
            asm!("mov cr3, {}", in(reg) value, options(nostack, preserves_flags));
        }
    }

    #[inline(always)]
    pub fn read_cr4() -> u64 {
        let value: u64;
        unsafe {
            asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    #[inline(always)]
    pub fn write_cr4(value: u64) {
        unsafe {
            asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags));
        }
    }

    #[inline(always)]
    pub fn wbinvd() {
        unsafe {
            asm!("wbinvd", options(nostack, preserves_flags));
        }
    }

    #[inline(always)]
    pub fn flush_tlb_all() {
        let cr3 = read_cr3();
        write_cr3(cr3);
    }

    #[inline(always)]
    pub fn invlpg(vaddr: u64) {
        unsafe {
            asm!("invlpg [{}]", in(reg) vaddr, options(nostack, preserves_flags));
        }
    }

    pub const CR0_PE: u64 = 1 << 0;
    pub const CR0_MP: u64 = 1 << 1;
    pub const CR0_EM: u64 = 1 << 2;
    pub const CR0_TS: u64 = 1 << 3;
    pub const CR0_ET: u64 = 1 << 4;
    pub const CR0_NE: u64 = 1 << 5;
    pub const CR0_WP: u64 = 1 << 16;
    pub const CR0_AM: u64 = 1 << 18;
    pub const CR0_NW: u64 = 1 << 29;
    pub const CR0_CD: u64 = 1 << 30;
    pub const CR0_PG: u64 = 1 << 31;

    pub const CR4_VME: u64 = 1 << 0;
    pub const CR4_PVI: u64 = 1 << 1;
    pub const CR4_TSD: u64 = 1 << 2;
    pub const CR4_DE: u64 = 1 << 3;
    pub const CR4_PSE: u64 = 1 << 4;
    pub const CR4_PAE: u64 = 1 << 5;
    pub const CR4_MCE: u64 = 1 << 6;
    pub const CR4_PGE: u64 = 1 << 7;
    pub const CR4_PCE: u64 = 1 << 8;
    pub const CR4_OSFXSR: u64 = 1 << 9;
    pub const CR4_OSXMMEXCPT: u64 = 1 << 10;
    pub const CR4_UMIP: u64 = 1 << 11;
    pub const CR4_LA57: u64 = 1 << 12;
    pub const CR4_VMXE: u64 = 1 << 13;
    pub const CR4_SMXE: u64 = 1 << 14;
    pub const CR4_FSGSBASE: u64 = 1 << 16;
    pub const CR4_PCIDE: u64 = 1 << 17;
    pub const CR4_OSXSAVE: u64 = 1 << 18;
    pub const CR4_SMEP: u64 = 1 << 20;
    pub const CR4_SMAP: u64 = 1 << 21;
    pub const CR4_PKE: u64 = 1 << 22;

    pub fn enable_sse() {
        let mut cr0 = read_cr0();
        cr0 &= !CR0_EM;
        cr0 &= !CR0_TS;
        cr0 |= CR0_MP;
        write_cr0(cr0);

        let mut cr4 = read_cr4();
        cr4 |= CR4_OSFXSR;
        cr4 |= CR4_OSXMMEXCPT;
        write_cr4(cr4);
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
        let res = core::arch::x86_64::__cpuid(leaf);
        (res.eax, res.ebx, res.ecx, res.edx)
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
            rax,
            rbx,
            rcx,
            rdx,
            rsi,
            rdi,
            rbp,
            rsp,
            r8,
            r9,
            r10,
            r11,
            r12,
            r13,
            r14,
            r15,
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
pub mod free_list;
pub mod init_flag;
pub mod kdiag;
pub mod klog;
pub mod math;
pub mod memory;
pub mod numfmt;
pub mod panic_recovery;
pub mod percpu;
pub mod preempt;
pub mod ring_buffer;
pub mod service_cell;
pub mod service_macro;
pub mod spinlock;
pub mod stacktrace;
pub mod string;
pub mod sync;
pub mod testing;
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
pub use preempt::{IrqPreemptGuard, PreemptGuard, is_preemption_disabled, preempt_count};
pub use ring_buffer::RingBuffer;
pub use service_cell::ServiceCell;
pub use spinlock::{
    IrqMutex, IrqMutexGuard, IrqRwLock, IrqRwLockReadGuard, IrqRwLockWriteGuard, Spinlock,
};
pub use stacktrace::StacktraceEntry;
pub use sync::{
    CleanLockToken, L0, L1, L2, L3, L4, L5, Level, LockToken, Lower, Mutex as OrderedMutex,
    MutexGuard as OrderedMutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

pub use init_flag::{InitFlag, StateFlag};
pub use percpu::{
    MAX_CPUS, PerCpuData, apic_id_from_cpu_index, cpu_index_from_apic_id, get_bsp_apic_id,
    get_cpu_count, get_current_cpu, get_online_cpu_count, get_percpu_data, init_bsp,
    init_percpu_for_cpu, is_bsp, is_cpu_online, mark_cpu_offline, mark_cpu_online,
    register_lapic_id_fn, register_send_ipi_to_cpu_fn, send_ipi_to_cpu,
};
