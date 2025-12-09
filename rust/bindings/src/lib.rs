#![no_std]
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(unsafe_op_in_unsafe_fn)]

//! SlopOS C Bindings for Rust
//!
//! This crate provides safe Rust bindings to C kernel functions
//! like kmalloc, kernel_panic, klog, etc.
//!
//! Generated automatically by bindgen from kernel headers.

// Include the auto-generated bindings from build.rs
// These are unsafe extern blocks wrapping C FFI calls
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
