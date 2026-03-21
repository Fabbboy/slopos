//! Type-safe x86 I/O port access.

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

    #[inline(always)]
    unsafe fn write_to_port(port: u16, value: u8) {
        unsafe {
            asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
}

impl PortValue for u16 {
    #[inline(always)]
    unsafe fn read_from_port(port: u16) -> u16 {
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

    #[inline(always)]
    unsafe fn write_to_port(port: u16, value: u16) {
        unsafe {
            asm!(
                "out dx, ax",
                in("dx") port,
                in("ax") value,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
}

impl PortValue for u32 {
    #[inline(always)]
    unsafe fn read_from_port(port: u16) -> u32 {
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

    #[inline(always)]
    unsafe fn write_to_port(port: u16, value: u32) {
        unsafe {
            asm!(
                "out dx, eax",
                in("dx") port,
                in("eax") value,
                options(nomem, nostack, preserves_flags)
            );
        }
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
    unsafe { DELAY_PORT.write(0) }
}
