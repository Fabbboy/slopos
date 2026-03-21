#![no_std]
#![feature(sync_unsafe_cell)]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod arch;
pub mod cpu;
mod init_flag;
mod interrupt_frame;
pub mod pcr;
pub mod tsc;

pub use init_flag::InitFlag;
pub use interrupt_frame::InterruptFrame;
pub use pcr::{
    apic_id_from_cpu_index, cpu_index_from_apic_id, get_bsp_apic_id, get_cpu_count,
    get_current_cpu, get_online_cpu_count, is_bsp, is_cpu_online, mark_cpu_offline,
    mark_cpu_online, MAX_CPUS,
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
