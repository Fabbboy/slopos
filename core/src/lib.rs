#![no_std]

use core::arch::global_asm;

global_asm!(include_str!("../context_switch.s"), options(att_syntax));

pub mod irq;
pub mod platform;
pub mod scheduler;
pub mod wl_currency;

pub use scheduler::fate_api;
pub use scheduler::ffi_boundary;
pub use scheduler::kthread;
pub use scheduler::scheduler as sched;
pub use scheduler::task;
pub use scheduler::test_tasks;

pub use scheduler::fate_api::*;
pub use scheduler::kthread::*;
pub use scheduler::scheduler::*;
pub use scheduler::task::*;
pub use scheduler::test_tasks::*;
