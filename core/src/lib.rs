#![no_std]

extern crate alloc;

use core::arch::global_asm;

global_asm!(include_str!("../context_switch.s"), options(att_syntax));

pub mod exec;
pub mod irq;
pub mod irq_tests;
pub mod platform;
pub mod scheduler;
#[macro_use]
pub mod syscall;
pub mod syscall_services;
pub use slopos_lib::wl_currency;

pub use scheduler::context_tests;
pub use scheduler::fate_api;
pub use scheduler::ffi_boundary;
pub use scheduler::kthread;
pub use scheduler::sched_tests;
pub use scheduler::scheduler as sched;
pub use scheduler::task;
pub use scheduler::test_tasks;

pub use scheduler::fate_api::*;
pub use scheduler::kthread::*;
pub use scheduler::sched_tests::*;
pub use scheduler::scheduler::*;
pub use scheduler::task::*;
pub use scheduler::test_tasks::*;
