#![allow(unsafe_op_in_unsafe_fn)]

pub mod cpuid;
pub mod cr3;
pub mod exception;
pub mod gdt;
pub mod kernel_ptr;
pub mod linker;
pub mod mem_fence;
pub mod msr;
pub mod naked;
pub mod per_cpu_gdt;
pub mod safestack;
pub mod tsc;
pub mod tss;
