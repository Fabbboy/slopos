//! Non-registry-gated typed I/O port handle.
//!
//! This is the pre-registry sibling of [`crate::io::port::IoPort`]. The
//! registry-gated `IoPort<T>` requires
//! [`register_io_port_registry`](crate::io::port::register_io_port_registry)
//! to have run on the BSP — which means it cannot serve early-boot
//! consumers (the panic logger, the boot driver init paths that touch
//! the UART / PIT / PS/2 before BSP-init completes).
//!
//! `raw_port::Port<T>` provides the same `in/out` asm primitives without
//! the registry gate. Construction is `pub const fn new(u16)`; reading and
//! writing remain `unsafe` because port I/O has arbitrary hardware side
//! effects.
//!
//! New code should prefer the registry-gated [`IoPort`](crate::io::port::IoPort).

#[allow(unused_imports)]
use core::arch::asm;
use core::marker::PhantomData;

mod private {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
}

/// Trait for types that can be read from and written to I/O ports.
/// Sealed: only implemented for `u8`, `u16`, `u32`.
pub trait PortValue: private::Sealed + Copy {
    /// # Safety
    /// Port I/O can have arbitrary side effects on hardware state.
    unsafe fn read_from_port(port: u16) -> Self;

    /// # Safety
    /// Port I/O can have arbitrary side effects on hardware state.
    unsafe fn write_to_port(port: u16, value: Self);
}

impl PortValue for u8 {
    #[inline(always)]
    unsafe fn read_from_port(port: u16) -> u8 {
        #[cfg(target_os = "none")]
        {
            let value: u8;
            unsafe {
                asm!(
                    "in al, dx",
                    out("al") value,
                    in("dx") port,
                    options(nomem, nostack, preserves_flags)
                );
            }
            value
        }
        #[cfg(not(target_os = "none"))]
        {
            host_mock::read(port) as u8
        }
    }

    #[inline(always)]
    unsafe fn write_to_port(port: u16, value: u8) {
        #[cfg(target_os = "none")]
        unsafe {
            asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags)
            );
        }
        #[cfg(not(target_os = "none"))]
        {
            host_mock::write(port, value as u32);
        }
    }
}

impl PortValue for u16 {
    #[inline(always)]
    unsafe fn read_from_port(port: u16) -> u16 {
        #[cfg(target_os = "none")]
        {
            let value: u16;
            unsafe {
                asm!(
                    "in ax, dx",
                    out("ax") value,
                    in("dx") port,
                    options(nomem, nostack, preserves_flags)
                );
            }
            value
        }
        #[cfg(not(target_os = "none"))]
        {
            host_mock::read(port) as u16
        }
    }

    #[inline(always)]
    unsafe fn write_to_port(port: u16, value: u16) {
        #[cfg(target_os = "none")]
        unsafe {
            asm!(
                "out dx, ax",
                in("dx") port,
                in("ax") value,
                options(nomem, nostack, preserves_flags)
            );
        }
        #[cfg(not(target_os = "none"))]
        {
            host_mock::write(port, value as u32);
        }
    }
}

impl PortValue for u32 {
    #[inline(always)]
    unsafe fn read_from_port(port: u16) -> u32 {
        #[cfg(target_os = "none")]
        {
            let value: u32;
            unsafe {
                asm!(
                    "in eax, dx",
                    out("eax") value,
                    in("dx") port,
                    options(nomem, nostack, preserves_flags)
                );
            }
            value
        }
        #[cfg(not(target_os = "none"))]
        {
            host_mock::read(port)
        }
    }

    #[inline(always)]
    unsafe fn write_to_port(port: u16, value: u32) {
        #[cfg(target_os = "none")]
        unsafe {
            asm!(
                "out dx, eax",
                in("dx") port,
                in("eax") value,
                options(nomem, nostack, preserves_flags)
            );
        }
        #[cfg(not(target_os = "none"))]
        {
            host_mock::write(port, value);
        }
    }
}

/// Host-only I/O-port mock store.
///
/// A 64-slot direct-mapped table: each port's value lives at
/// `port mod CAP`. Collisions overwrite. For the host integration
/// tests + `cargo miri test`, the only consumers of port I/O are
/// the early-console UART probe and a handful of identity-port
/// drivers in tests/io_port.rs that don't depend on any particular
/// real value being returned from probes.
///
/// Special-cased ports:
/// - `0x3FD` (UART LSR): always returns `0x20` (TX empty) so the
///   poll loop in [`crate::early_console::write_byte`] terminates
///   immediately on host. Without this, host tests that emit klog
///   bytes early would spin forever.
#[cfg(not(target_os = "none"))]
mod host_mock {
    use core::sync::atomic::{AtomicU32, Ordering};

    const CAP: usize = 64;
    static SLOTS: [AtomicU32; CAP] = [const { AtomicU32::new(0) }; CAP];

    fn slot_index(port: u16) -> usize {
        (port as usize) % CAP
    }

    pub(super) fn read(port: u16) -> u32 {
        // COM1 LSR at 0x3FD: pretend TX is always empty.
        if port == 0x3FD {
            return 0x20;
        }
        SLOTS[slot_index(port)].load(Ordering::Relaxed)
    }

    pub(super) fn write(port: u16, value: u32) {
        SLOTS[slot_index(port)].store(value, Ordering::Relaxed);
    }
}

/// Type-safe I/O port. `T` must be `u8`, `u16`, or `u32`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Port<T: PortValue> {
    port: u16,
    _phantom: PhantomData<T>,
}

impl<T: PortValue> Port<T> {
    #[inline]
    pub const fn new(port: u16) -> Self {
        Self {
            port,
            _phantom: PhantomData,
        }
    }

    #[inline]
    pub const fn address(&self) -> u16 {
        self.port
    }

    #[inline]
    pub const fn offset(self, off: u16) -> Self {
        Self::new(self.port.wrapping_add(off))
    }

    /// # Safety
    /// Port I/O can have arbitrary side effects on hardware state.
    #[inline(always)]
    pub unsafe fn read(&self) -> T {
        unsafe { T::read_from_port(self.port) }
    }

    /// # Safety
    /// Port I/O can have arbitrary side effects on hardware state.
    #[inline(always)]
    pub unsafe fn write(&self, value: T) {
        unsafe { T::write_to_port(self.port, value) }
    }
}

impl<T: PortValue> core::fmt::Debug for Port<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Port")
            .field("address", &format_args!("0x{:04x}", self.port))
            .field("size", &core::mem::size_of::<T>())
            .finish()
    }
}

/// I/O delay via port 0x80 (POST diagnostic port).
///
/// # Safety
/// Should only be called in contexts where port I/O is appropriate.
#[inline(always)]
pub unsafe fn io_wait() {
    const DELAY_PORT: Port<u8> = Port::new(0x80);
    // SAFETY: port 0x80 is the POST diagnostic port; writing 0 to it
    // has no observable hardware effect beyond a short stall, and on
    // host builds the port-I/O is routed through `host_mock` so no
    // privileged instruction executes.
    unsafe { DELAY_PORT.write(0) }
}
