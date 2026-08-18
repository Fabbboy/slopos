//! Re-export shim preserving the historical `slopos_arch::*` import paths.
//! New code should import from `slopos_ostd` directly.

#![no_std]
#![forbid(unsafe_code)]

pub mod pcr {
    pub use slopos_ostd::cpu::x86_64::pcr::*;
}

pub mod cpu {
    pub use slopos_ostd::arch::x86_64::cpuid::*;
    pub use slopos_ostd::arch::x86_64::msr::*;
    pub use slopos_ostd::arch::x86_64::{cpuid, msr};
    pub use slopos_ostd::cpu::x86_64::*;
    pub use slopos_ostd::cpu::x86_64::{
        apic_msr, control_regs, core, interrupts, rdrand, security, sse, stack, tlb, xsave,
    };
}

pub mod arch {
    pub use slopos_ostd::arch::x86_64::gdt::*;
    pub use slopos_ostd::arch::x86_64::{exception, gdt};

    pub mod idt {
        pub use slopos_ostd::irq::idt::*;
    }
}

pub mod tsc {
    pub use slopos_ostd::arch::x86_64::tsc::*;
}

pub use slopos_ostd::irq::interrupt_frame::InterruptFrame;
pub use slopos_ostd::sync::init_flag::InitFlag;

pub use slopos_ostd::cpu::x86_64::pcr::{
    MAX_CPUS, apic_id_from_cpu_index, cpu_index_from_apic_id, get_bsp_apic_id, get_cpu_count,
    get_current_cpu, get_online_cpu_count, is_bsp, is_cpu_online, mark_cpu_offline,
    mark_cpu_online,
};

#[macro_export]
macro_rules! klog_info {
    ($($arg:tt)*) => {{
        let _ = core::format_args!($($arg)*);
    }};
}

#[macro_export]
macro_rules! klog_debug {
    ($($arg:tt)*) => {{
        let _ = core::format_args!($($arg)*);
    }};
}
