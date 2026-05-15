//! Safe wrappers over slibc's heap allocator.
//!
//! `mem/` houses the dlmalloc engine; this module exposes safe-Rust
//! abstractions on top of it so test code does not have to touch raw
//! pointers.

pub mod raw_buffer;

pub use raw_buffer::RawBuffer;
