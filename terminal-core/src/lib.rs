//! Pure terminal-emulator core: the VT grid model and the input/selection
//! logic, with zero dependency on syscalls, the compositor protocol, or font
//! globals. The userland terminal app (`slopos-userland`) depends on this and
//! supplies the IO, rendering, and protocol bridge; the kernel does not link
//! it. Being free of those couplings makes the whole core host-testable —
//! `cargo test -p slopos-terminal-core` actually runs (unlike the userland
//! crate, whose slibc C-runtime interposition segfaults host test binaries).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod grid;
pub mod input;
