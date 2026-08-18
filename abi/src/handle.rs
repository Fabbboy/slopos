//! Native window and display handle types for renderer interop.
//!
//! The interop seam between window providers (the `windowing` crate) and
//! rendering backends: a backend depends only on `slopos-abi` to consume these
//! traits. The raw structs hold plain integers; the borrowed wrappers tie a
//! handle's lifetime to the window or connection that issued it.

use core::marker::PhantomData;

use crate::pixel::PixelFormat;

/// Raw identifiers for a compositor surface. All fields are plain integers, so
/// the struct is `Send + Sync`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct RawWindowHandle {
    pub surface_id: u32,
    /// Compositor-assigned toplevel identifier (0 if no toplevel role).
    pub toplevel_id: u32,
}

/// Raw identifiers for a compositor connection. All fields are plain values, so
/// the struct is `Send + Sync`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct RawDisplayHandle {
    /// File descriptor of the compositor socket.
    pub fd: i32,
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
}

/// Borrowed handle to a compositor surface. The lifetime `'a` is tied to the
/// issuing surface, so it cannot be used after that surface is destroyed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowHandle<'a> {
    raw: RawWindowHandle,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> WindowHandle<'a> {
    #[inline]
    pub fn new(raw: RawWindowHandle) -> Self {
        Self {
            raw,
            _lifetime: PhantomData,
        }
    }

    #[inline]
    pub fn as_raw(&self) -> RawWindowHandle {
        self.raw
    }

    #[inline]
    pub fn surface_id(&self) -> u32 {
        self.raw.surface_id
    }

    #[inline]
    pub fn toplevel_id(&self) -> u32 {
        self.raw.toplevel_id
    }
}

impl From<WindowHandle<'_>> for RawWindowHandle {
    #[inline]
    fn from(handle: WindowHandle<'_>) -> Self {
        handle.raw
    }
}

/// Borrowed handle to a compositor connection. The lifetime `'a` is tied to the
/// issuing protocol connection, so it cannot be used after disconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DisplayHandle<'a> {
    raw: RawDisplayHandle,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> DisplayHandle<'a> {
    #[inline]
    pub fn new(raw: RawDisplayHandle) -> Self {
        Self {
            raw,
            _lifetime: PhantomData,
        }
    }

    #[inline]
    pub fn as_raw(&self) -> RawDisplayHandle {
        self.raw
    }

    #[inline]
    pub fn fd(&self) -> i32 {
        self.raw.fd
    }

    #[inline]
    pub fn format(&self) -> PixelFormat {
        self.raw.format
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.raw.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.raw.height
    }
}

impl From<DisplayHandle<'_>> for RawDisplayHandle {
    #[inline]
    fn from(handle: DisplayHandle<'_>) -> Self {
        handle.raw
    }
}

/// Implemented by types that can identify a compositor surface. The returned
/// handle borrows `&self`, so it cannot outlive the issuing surface.
pub trait HasWindowHandle {
    fn window_handle(&self) -> WindowHandle<'_>;
}

/// Implemented by types that can identify a compositor connection. The returned
/// handle borrows `&self`, so it cannot outlive the issuing connection.
pub trait HasDisplayHandle {
    fn display_handle(&self) -> DisplayHandle<'_>;
}

impl<T: HasWindowHandle> HasWindowHandle for &T {
    #[inline]
    fn window_handle(&self) -> WindowHandle<'_> {
        (**self).window_handle()
    }
}

impl<T: HasWindowHandle> HasWindowHandle for &mut T {
    #[inline]
    fn window_handle(&self) -> WindowHandle<'_> {
        (**self).window_handle()
    }
}

impl<T: HasDisplayHandle> HasDisplayHandle for &T {
    #[inline]
    fn display_handle(&self) -> DisplayHandle<'_> {
        (**self).display_handle()
    }
}

impl<T: HasDisplayHandle> HasDisplayHandle for &mut T {
    #[inline]
    fn display_handle(&self) -> DisplayHandle<'_> {
        (**self).display_handle()
    }
}
