//! Native window and display handle types for renderer interop.
//!
//! This module defines the interop seam between window providers (the
//! `windowing` crate) and rendering backends. A backend depends only on
//! `slopos-abi` to consume these traits — no coupling to the full windowing
//! or compositor stack.
//!
//! # Architecture
//!
//! Two raw data structs hold plain integer identifiers:
//!
//! - [`RawWindowHandle`] — surface ID, toplevel ID, shared-memory token.
//! - [`RawDisplayHandle`] — compositor socket fd, pixel format, dimensions.
//!
//! Two borrowed wrappers tie the handle lifetime to the window that issued it:
//!
//! - [`WindowHandle<'a>`] — borrows a `RawWindowHandle`.
//! - [`DisplayHandle<'a>`] — borrows a `RawDisplayHandle`.
//!
//! Two traits let any type advertise its native identity:
//!
//! - [`HasWindowHandle`] — "I can identify a compositor surface."
//! - [`HasDisplayHandle`] — "I can identify a compositor connection."
//!
//! All types are `Copy`, all constructors are safe, and the entire module
//! compiles under `#![forbid(unsafe_code)]`.

use core::marker::PhantomData;

use crate::pixel::PixelFormat;

// ---------------------------------------------------------------------------
// Raw handle data
// ---------------------------------------------------------------------------

/// Raw identifiers for a compositor surface.
///
/// A rendering backend uses these to create its own shared-memory buffer and
/// attach it, or to issue damage/commit on an existing buffer.
///
/// All fields are plain integers — no pointers. The struct is `Send + Sync`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct RawWindowHandle {
    /// Compositor-assigned surface identifier.
    pub surface_id: u32,
    /// Compositor-assigned toplevel identifier (0 if no toplevel role).
    pub toplevel_id: u32,
    /// Kernel-assigned shared-memory token for the backing buffer.
    pub shm_token: u32,
}

/// Raw identifiers for a compositor connection.
///
/// A rendering backend uses these to negotiate formats, manage buffers,
/// or open its own protocol channel.
///
/// All fields are plain values — no pointers. The struct is `Send + Sync`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct RawDisplayHandle {
    /// File descriptor of the compositor socket.
    pub fd: i32,
    /// Display pixel format.
    pub format: PixelFormat,
    /// Display width in pixels.
    pub width: u32,
    /// Display height in pixels.
    pub height: u32,
}

// ---------------------------------------------------------------------------
// Borrowed wrappers
// ---------------------------------------------------------------------------

/// Borrowed handle to a compositor surface.
///
/// The lifetime `'a` is tied to the window or surface that issued the handle,
/// preventing use after the surface is destroyed (and its IDs freed).
///
/// `Copy` and cheap — just three `u32` values plus a zero-sized lifetime tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowHandle<'a> {
    raw: RawWindowHandle,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> WindowHandle<'a> {
    /// Wrap raw identifiers with a lifetime guarantee.
    #[inline]
    pub fn new(raw: RawWindowHandle) -> Self {
        Self {
            raw,
            _lifetime: PhantomData,
        }
    }

    /// Extract the raw identifiers.
    #[inline]
    pub fn as_raw(&self) -> RawWindowHandle {
        self.raw
    }

    /// Compositor-assigned surface identifier.
    #[inline]
    pub fn surface_id(&self) -> u32 {
        self.raw.surface_id
    }

    /// Compositor-assigned toplevel identifier (0 if no toplevel role).
    #[inline]
    pub fn toplevel_id(&self) -> u32 {
        self.raw.toplevel_id
    }

    /// Kernel-assigned shared-memory token for the backing buffer.
    #[inline]
    pub fn shm_token(&self) -> u32 {
        self.raw.shm_token
    }
}

impl From<WindowHandle<'_>> for RawWindowHandle {
    #[inline]
    fn from(handle: WindowHandle<'_>) -> Self {
        handle.raw
    }
}

/// Borrowed handle to a compositor connection.
///
/// The lifetime `'a` is tied to the protocol connection that issued the
/// handle, preventing use after disconnect.
///
/// `Copy` and cheap — just an `i32`, a `PixelFormat`, and two `u32` values
/// plus a zero-sized lifetime tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DisplayHandle<'a> {
    raw: RawDisplayHandle,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> DisplayHandle<'a> {
    /// Wrap raw identifiers with a lifetime guarantee.
    #[inline]
    pub fn new(raw: RawDisplayHandle) -> Self {
        Self {
            raw,
            _lifetime: PhantomData,
        }
    }

    /// Extract the raw identifiers.
    #[inline]
    pub fn as_raw(&self) -> RawDisplayHandle {
        self.raw
    }

    /// File descriptor of the compositor socket.
    #[inline]
    pub fn fd(&self) -> i32 {
        self.raw.fd
    }

    /// Display pixel format.
    #[inline]
    pub fn format(&self) -> PixelFormat {
        self.raw.format
    }

    /// Display width in pixels.
    #[inline]
    pub fn width(&self) -> u32 {
        self.raw.width
    }

    /// Display height in pixels.
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

// ---------------------------------------------------------------------------
// Provider traits
// ---------------------------------------------------------------------------

/// Implemented by types that can identify a compositor surface.
///
/// The returned handle is borrowed from `&self` — it cannot outlive the
/// window or surface that issued it.
pub trait HasWindowHandle {
    fn window_handle(&self) -> WindowHandle<'_>;
}

/// Implemented by types that can identify a compositor connection.
///
/// The returned handle is borrowed from `&self` — it cannot outlive the
/// protocol connection that issued it.
pub trait HasDisplayHandle {
    fn display_handle(&self) -> DisplayHandle<'_>;
}

// Blanket impls so `&Window` and `&mut Window` also implement the traits.

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
