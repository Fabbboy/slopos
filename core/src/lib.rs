#![no_std]
#![feature(allocator_api)]
#![feature(sync_unsafe_cell)]

use core::arch::global_asm;

global_asm!(include_str!("../context_switch.s"), options(att_syntax));

pub mod driver_hooks;
pub mod exec;
pub mod irq;
pub mod scheduler;
#[cfg(feature = "itests")]
pub mod tests;
#[macro_use]
pub mod syscall;

#[cfg(feature = "itests")]
pub use scheduler::context_tests;
pub use scheduler::fate_api;
pub use scheduler::ffi_boundary;
pub use scheduler::kthread;
pub use scheduler::per_cpu;
#[cfg(feature = "itests")]
pub use scheduler::sched_tests;
pub use scheduler::scheduler as sched;
pub use scheduler::task;
pub use scheduler::work_steal;
