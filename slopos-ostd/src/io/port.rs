//! `IoPort<T>`: typed safe wrapper for x86 port I/O.
//!
//! Construction is gated behind [`IoPortRegistry`] so only ports the platform
//! has marked insensitive (Inv. 7) are reachable. `slopos-utils::io::Port`
//! stays alive in parallel: the early-boot panic logger needs port I/O before
//! any registry exists.

use core::arch::asm;
use core::marker::PhantomData;
use core::mem::size_of;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::mm::pod::Pod;
use crate::sync::BspToken;

mod private {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
}

/// Sealed trait identifying types that fit a single x86 `in` / `out`
/// instruction. Implemented for `u8`, `u16`, `u32` only.
///
/// # Safety
///
/// Implementor's `read_from_port` / `write_to_port` must use the
/// correct-width opcode for `Self` and must not access memory or the
/// stack (they are `nomem nostack preserves_flags`). This is sealed
/// to prevent downstream impls from breaking those invariants.
pub unsafe trait PortAccessible: private::Sealed + Pod {
    /// # Safety
    /// Port reads can have arbitrary side effects on hardware state.
    unsafe fn read_from_port(port: u16) -> Self;

    /// # Safety
    /// Port writes can have arbitrary side effects on hardware state.
    unsafe fn write_to_port(port: u16, value: Self);
}

// SAFETY: `in al, dx` is the correct-width opcode for `u8`; the asm
// block declares no memory or stack effects. Inv. 7 / Inv. 3.
unsafe impl PortAccessible for u8 {
    #[inline(always)]
    unsafe fn read_from_port(port: u16) -> u8 {
        let value: u8;
        // SAFETY: see impl-level comment.
        unsafe {
            asm!(
                "in al, dx",
                out("al") value,
                in("dx") port,
                options(nomem, nostack, preserves_flags),
            );
        }
        value
    }

    #[inline(always)]
    unsafe fn write_to_port(port: u16, value: u8) {
        // SAFETY: see impl-level comment.
        unsafe {
            asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags),
            );
        }
    }
}

// SAFETY: `in ax, dx` is the correct-width opcode for `u16`; otherwise
// as above.
unsafe impl PortAccessible for u16 {
    #[inline(always)]
    unsafe fn read_from_port(port: u16) -> u16 {
        let value: u16;
        // SAFETY: see impl-level comment.
        unsafe {
            asm!(
                "in ax, dx",
                out("ax") value,
                in("dx") port,
                options(nomem, nostack, preserves_flags),
            );
        }
        value
    }

    #[inline(always)]
    unsafe fn write_to_port(port: u16, value: u16) {
        // SAFETY: see impl-level comment.
        unsafe {
            asm!(
                "out dx, ax",
                in("dx") port,
                in("ax") value,
                options(nomem, nostack, preserves_flags),
            );
        }
    }
}

// SAFETY: `in eax, dx` is the correct-width opcode for `u32`; otherwise
// as above.
unsafe impl PortAccessible for u32 {
    #[inline(always)]
    unsafe fn read_from_port(port: u16) -> u32 {
        let value: u32;
        // SAFETY: see impl-level comment.
        unsafe {
            asm!(
                "in eax, dx",
                out("eax") value,
                in("dx") port,
                options(nomem, nostack, preserves_flags),
            );
        }
        value
    }

    #[inline(always)]
    unsafe fn write_to_port(port: u16, value: u32) {
        // SAFETY: see impl-level comment.
        unsafe {
            asm!(
                "out dx, eax",
                in("dx") port,
                in("eax") value,
                options(nomem, nostack, preserves_flags),
            );
        }
    }
}

/// Half-open port range `[start, end)`.
#[derive(Clone, Copy, Debug)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    /// True if `[port, port + access_size)` is entirely within
    /// `[start, end)`. Overflow-safe.
    #[inline]
    pub fn contains(&self, port: u16, access_size: usize) -> bool {
        if self.end <= self.start {
            return false;
        }
        let Some(req_end) = (port as u32).checked_add(access_size as u32) else {
            return false;
        };
        if req_end > u16::MAX as u32 + 1 {
            return false;
        }
        port >= self.start && req_end as u16 <= self.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoPortError {
    /// `IoPortRegistry::reserve` could not find a containing range.
    NotReserved,
    /// [`register_io_port_registry`] has not been called yet.
    Uninitialised,
}

/// Typed handle to a single x86 I/O port.
///
/// Construction is gated by [`IoPortRegistry::reserve`] so only ports the
/// platform has certified insensitive (Inv. 7) can be reached; `read` /
/// `write` stay `unsafe` because the registry approves the port, not the
/// sequencing.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct IoPort<T: PortAccessible> {
    port: u16,
    _phantom: PhantomData<T>,
}

impl<T: PortAccessible> IoPort<T> {
    #[inline]
    pub const fn address(&self) -> u16 {
        self.port
    }

    /// Wrapping advance of the port number; the result is not re-checked
    /// against the registry, so pair with [`IoPortRegistry::reserve`] when it
    /// must be.
    #[inline]
    pub const fn offset(self, off: u16) -> Self {
        Self {
            port: self.port.wrapping_add(off),
            _phantom: PhantomData,
        }
    }

    /// # Safety
    /// Port reads can have arbitrary hardware side effects (CMOS
    /// index latching, FIFO pop, etc.). The caller must certify that
    /// issuing this read at this point in time does not violate the
    /// device's protocol or kernel safety.
    #[inline(always)]
    pub unsafe fn read(&self) -> T {
        // SAFETY: caller-certified.
        unsafe { T::read_from_port(self.port) }
    }

    /// # Safety
    /// Port writes can have arbitrary hardware side effects (PIC EOI,
    /// debug-exit, latch advance, etc.). The caller must certify the
    /// sequence is sound.
    #[inline(always)]
    pub unsafe fn write(&self, value: T) {
        // SAFETY: caller-certified.
        unsafe { T::write_to_port(self.port, value) }
    }
}

impl<T: PortAccessible> core::fmt::Debug for IoPort<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IoPort")
            .field("port", &format_args!("{:#06x}", self.port))
            .field("size", &size_of::<T>())
            .finish()
    }
}

struct PortRegistrySlot {
    base: AtomicPtr<PortRange>,
    len: AtomicUsize,
}

static IO_PORT_REGISTRY: PortRegistrySlot = PortRegistrySlot {
    base: AtomicPtr::new(core::ptr::null_mut()),
    len: AtomicUsize::new(0),
};

/// One-shot wiring point for the insensitive-port list; the `&BspToken<'brand>`
/// witnesses BSP-only init. Every entry must describe a port range the platform
/// has marked as insensitive (Inv. 7).
pub fn register_io_port_registry<'brand>(_token: &BspToken<'brand>, ranges: &'static [PortRange]) {
    let raw = ranges.as_ptr() as *mut PortRange;
    let prev = IO_PORT_REGISTRY.base.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::io::port::register_io_port_registry called twice"
    );
    IO_PORT_REGISTRY.len.store(ranges.len(), Ordering::Release);
}

fn current_io_port_registry() -> Option<&'static [PortRange]> {
    let base = IO_PORT_REGISTRY.base.load(Ordering::Acquire);
    if base.is_null() {
        return None;
    }
    let len = IO_PORT_REGISTRY.len.load(Ordering::Acquire);
    // SAFETY: Inv. 7. `base` was produced by `register_io_port_registry`
    // from a `&'static [PortRange]` of length `len`.
    Some(unsafe { core::slice::from_raw_parts(base, len) })
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    IO_PORT_REGISTRY
        .base
        .store(core::ptr::null_mut(), Ordering::Release);
    IO_PORT_REGISTRY.len.store(0, Ordering::Release);
}

/// Insensitive-port gate over [`IoPort`] construction.
pub struct IoPortRegistry;

impl IoPortRegistry {
    /// Reserve `[port, port + size_of::<T>())` as an `IoPort<T>`.
    pub fn reserve<T: PortAccessible>(port: u16) -> Result<IoPort<T>, IoPortError> {
        let ranges = current_io_port_registry().ok_or(IoPortError::Uninitialised)?;
        let access_size = size_of::<T>();
        if !ranges.iter().any(|r| r.contains(port, access_size)) {
            return Err(IoPortError::NotReserved);
        }
        Ok(IoPort {
            port,
            _phantom: PhantomData,
        })
    }
}

/// Diagnostic-port (`0x80`) write used as an I/O delay between
/// back-to-back port writes to slow legacy hardware.
///
/// # Safety
///
/// Writing port `0x80` has been benign on every PC architecture since
/// the original IBM PC, but the call still emits an `out` instruction
/// — only invoke from contexts where port I/O is appropriate.
#[inline(always)]
pub unsafe fn io_wait() {
    // SAFETY: caller-certified; port 0x80 is the POST diagnostic port.
    unsafe { u8::write_to_port(0x80, 0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_range_contains_simple() {
        let r = PortRange {
            start: 0x3F8,
            end: 0x400,
        };
        assert!(r.contains(0x3F8, 1));
        assert!(r.contains(0x3FF, 1));
        assert!(r.contains(0x3FE, 2));
        assert!(!r.contains(0x3FF, 2));
        assert!(!r.contains(0x400, 1));
    }

    #[test]
    fn port_range_rejects_inverted() {
        let r = PortRange {
            start: 0x100,
            end: 0x100,
        };
        assert!(!r.contains(0x100, 1));
    }

    #[test]
    fn port_range_handles_wrap() {
        let r = PortRange {
            start: 0xFFF0,
            end: 0xFFFF,
        };
        assert!(!r.contains(0xFFFE, 4));
    }

    #[test]
    fn io_port_address_and_offset() {
        let p: IoPort<u8> = IoPort {
            port: 0x3F8,
            _phantom: PhantomData,
        };
        assert_eq!(p.address(), 0x3F8);
        let p2 = p.offset(5);
        assert_eq!(p2.address(), 0x3FD);
    }

    #[test]
    fn io_port_error_eq() {
        assert_eq!(IoPortError::NotReserved, IoPortError::NotReserved);
        assert_ne!(IoPortError::NotReserved, IoPortError::Uninitialised);
    }
}
