#![no_std]
#![feature(c_variadic)]
#![feature(sync_unsafe_cell)]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod arch;
pub mod boot_info;
pub mod clock;
pub mod cpu;

pub mod io;
pub mod ports;

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
pub mod boot_flags;
pub mod cpu_local;
pub mod init_flag;
pub mod kdiag;
pub mod kernel_services;
pub mod klog;
pub mod memory;
pub mod numfmt;
pub mod once_lock;
pub mod panic_recovery;
pub mod pcr;
pub mod preempt;
pub mod ring_buffer;
pub mod service_cell;
pub mod service_macro;
pub mod spinlock;
pub mod stacktrace;
pub mod string;
pub mod testing;
pub mod waitqueue;
pub mod wl_currency;

#[doc(hidden)]
pub use paste;

pub use alignment::{align_down_u64, align_down_usize, align_up_u64, align_up_usize};
pub use alignment::{align_down_usize as align_down, align_up_usize as align_up};
pub use kdiag::kdiag_dump_interrupt_frame;
pub use kdiag::{kdiag_timestamp, InterruptFrame, KDIAG_STACK_TRACE_DEPTH};
pub use klog::{
    klog_get_level, klog_init, klog_is_enabled, klog_register_backend, klog_set_level, KlogLevel,
};
pub use ports::COM1;
pub use preempt::{is_preemption_disabled, preempt_count, IrqPreemptGuard, PreemptGuard};
pub use ring_buffer::RingBuffer;
pub use service_cell::ServiceCell;
pub use spinlock::{
    IrqMutex, IrqMutexGuard, IrqRwLock, IrqRwLockReadGuard, IrqRwLockWriteGuard, PreemptMutex,
    PreemptMutexGuard,
};
pub use stacktrace::StacktraceEntry;

pub use cpu_local::{CacheAligned, CpuLocal, CpuPinned, CpuPinnedMut};
pub use init_flag::{InitFlag, StateFlag};
pub use once_lock::OnceLock;
pub use pcr::{
    apic_id_from_cpu_index, cpu_index_from_apic_id, get_bsp_apic_id, get_cpu_count,
    get_current_cpu, get_online_cpu_count, is_bsp, is_cpu_online, mark_cpu_offline,
    mark_cpu_online, register_lapic_id_fn, register_send_ipi_to_cpu_fn, send_ipi_to_cpu,
    SendIpiToCpuFn, MAX_CPUS,
};
pub use waitqueue::WaitQueue;
