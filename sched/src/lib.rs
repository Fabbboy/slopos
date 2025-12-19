#![no_std]

use core::arch::global_asm;

global_asm!(include_str!("../context_switch.s"), options(att_syntax));

pub mod fate_api;
pub mod ffi_boundary;
pub mod kthread;
pub mod scheduler;
pub mod task;
pub mod test_tasks;

pub use fate_api::*;
pub use kthread::*;
pub use scheduler::*;
pub use task::*;
pub use test_tasks::*;
