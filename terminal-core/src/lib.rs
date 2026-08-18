//! Pure terminal-emulator core: the VT grid model and the input/selection
//! logic, with zero dependency on syscalls, the compositor protocol, or font
//! globals — which is what makes it host-testable. The userland terminal app
//! supplies the IO, rendering, and protocol bridge; the kernel does not link
//! it.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod damage;
pub mod grid;
pub mod input;
