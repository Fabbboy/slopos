//! Core protocol types: Request, Event, ProtocolError, OutputInfo.

pub const PROTOCOL_VERSION: u32 = 3;

/// Compile-time-safe surface identifier: prevents accidental interchange with
/// toplevel IDs or raw integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct SurfaceId(u32);

/// Compile-time-safe toplevel identifier: prevents accidental interchange with
/// surface IDs or raw integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct ToplevelId(u32);

impl SurfaceId {
    pub const NONE: Self = Self(0);
    pub fn from_raw(v: u32) -> Self {
        Self(v)
    }
    pub fn raw(self) -> u32 {
        self.0
    }
}

impl ToplevelId {
    pub const NONE: Self = Self(0);
    pub fn from_raw(v: u32) -> Self {
        Self(v)
    }
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// Capability flags advertised by the compositor in [`Event::Hello`].
pub mod caps {
    pub const TOPLEVEL: u64 = 1 << 0;
    pub const CLIPBOARD: u64 = 1 << 1;
    pub const INTERACTIVE_MOVE_RESIZE: u64 = 1 << 2;
}

/// Toplevel window state flags (bitfield in [`Event::Configure`]).
pub mod toplevel_state {
    pub const ACTIVATED: u32 = 1 << 0;
    pub const MAXIMIZED: u32 = 1 << 1;
    pub const FULLSCREEN: u32 = 1 << 2;
    pub const RESIZING: u32 = 1 << 3;
    pub const MINIMIZED: u32 = 1 << 4;
}

/// Resize edge flags for [`Request::InteractiveResize`].
pub mod resize_edge {
    pub const TOP: u32 = 1;
    pub const BOTTOM: u32 = 2;
    pub const LEFT: u32 = 4;
    pub const RIGHT: u32 = 8;
}

/// Move-only file descriptor wrapper. Closes on drop unless consumed via
/// [`into_raw`](OwnedFd::into_raw).
pub struct OwnedFd(i32);

impl OwnedFd {
    pub fn from_raw(fd: i32) -> Self {
        Self(fd)
    }
    pub fn raw(&self) -> i32 {
        self.0
    }
    pub fn into_raw(self) -> i32 {
        let fd = self.0;
        core::mem::forget(self);
        fd
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            use slopos_slibc::pal::{Pal, Sys};
            let _ = Sys::close(self.0);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    Io,
    MessageTooLarge,
    MalformedMessage,
    Timeout,
    Disconnected,
    BufferFull,
    VersionMismatch,
}

/// Display output info, received during connection bootstrap.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutputInfo {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub pitch: u32,
    pub scale: u32,
}

/// Maximum byte length for title and app-id strings on the wire.
pub const MAX_STRING_LEN: usize = 128;

/// Client-to-server request.
pub enum Request {
    /// Protocol version handshake (client response to server Hello).
    Hello {
        version: u32,
    },

    CreateSurface {
        new_id: SurfaceId,
    },
    SurfaceAttach {
        surface: SurfaceId,
        /// Client-assigned buffer slot. The fd is sent via SCM_RIGHTS only the
        /// first time a slot is used; later attaches re-select by id, no fd.
        buffer_id: u32,
        width: u32,
        height: u32,
        /// Whether this attach carries an SCM_RIGHTS fd; a no-fd re-select must
        /// never pop a later message's fd from the decoder's FIFO.
        has_fd: bool,
        /// Memfd-backed buffer fd received via SCM_RIGHTS. `None` when
        /// re-selecting a registered slot, or on the send side before transmission.
        buffer_fd: Option<OwnedFd>,
    },
    SurfaceDamage {
        surface: SurfaceId,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    SurfaceCommit {
        surface: SurfaceId,
    },
    /// Request a [`Event::FrameDone`] callback after the next present.
    SurfaceFrame {
        surface: SurfaceId,
    },
    SurfaceDestroy {
        surface: SurfaceId,
    },

    GetToplevel {
        surface: SurfaceId,
        new_id: ToplevelId,
    },
    ToplevelSetTitle {
        toplevel: ToplevelId,
        title: [u8; MAX_STRING_LEN],
        len: u8,
    },
    ToplevelSetAppId {
        toplevel: ToplevelId,
        app_id: [u8; MAX_STRING_LEN],
        len: u8,
    },
    ToplevelDestroy {
        toplevel: ToplevelId,
    },
    /// Acknowledge a [`Event::Configure`] serial before committing.
    AckConfigure {
        serial: u32,
    },

    /// Request a cursor shape for `surface`. `serial` must match the surface's
    /// most recent [`Event::PointerEnter`], so only the surface under the
    /// pointer can change the cursor.
    SetCursorShape {
        surface: SurfaceId,
        serial: u32,
        shape: u8,
    },

    /// Publish a clipboard selection: the bytes live in a memfd passed via
    /// SCM_RIGHTS, `len` is the valid byte count (the memfd is page-rounded).
    /// `serial` must match a recent key event delivered to a surface of this
    /// client that holds keyboard focus, as `wl_data_device::set_selection`
    /// requires. Without it any connected process can replace what the user
    /// last copied, or read it.
    ClipboardCopy {
        len: u32,
        serial: u32,
        buffer_fd: Option<OwnedFd>,
    },
    /// Ask the compositor for the current clipboard. It replies with
    /// `Event::PasteReady { len }`; the client then sends `ClipboardRead`.
    /// Same serial rule as [`Request::ClipboardCopy`].
    ClipboardPaste {
        serial: u32,
    },
    /// Provide a destination memfd (via SCM_RIGHTS) for the compositor to copy
    /// `len` clipboard bytes into; it replies with `Event::PasteResult`. The
    /// receiver provides the buffer because the server→client event path
    /// cannot carry an fd. Same serial rule as [`Request::ClipboardCopy`].
    ClipboardRead {
        len: u32,
        serial: u32,
        buffer_fd: Option<OwnedFd>,
    },

    /// Start an interactive window move. Serial must match a recent pointer event.
    InteractiveMove {
        toplevel: ToplevelId,
        serial: u32,
    },
    /// Start an interactive window resize. Serial must match a recent pointer event.
    InteractiveResize {
        toplevel: ToplevelId,
        serial: u32,
        edges: u32,
    },
}

/// Server-to-client event.
#[derive(Clone)]
pub enum Event {
    /// Protocol version handshake (server initiates on accept).
    Hello {
        version: u32,
        capabilities: u64,
    },

    /// Object destroy acknowledgement -- client may reuse the ID.
    ObjectDestroyed {
        id: u32,
    },

    OutputInfo {
        width: u32,
        height: u32,
        format: u32,
        pitch: u32,
        scale: u32,
    },

    /// Sent only to surfaces that requested it via [`Request::SurfaceFrame`].
    FrameDone {
        surface: SurfaceId,
        timestamp_ms: u32,
    },

    /// A buffer slot the compositor has finished compositing from and the client
    /// may draw into again. Sent when a newer buffer for the surface is committed.
    BufferRelease {
        surface: SurfaceId,
        buffer_id: u32,
    },

    /// Window state/size change. Client must [`Request::AckConfigure`] the
    /// serial before committing a buffer at the new size.
    Configure {
        toplevel: ToplevelId,
        serial: u32,
        width: u32,
        height: u32,
        states: u32,
    },
    Close {
        toplevel: ToplevelId,
    },

    /// Pointer entered `surface`. `serial` identifies this focus grant; the
    /// client echoes it in [`Request::SetCursorShape`] to set the cursor.
    PointerEnter {
        surface: SurfaceId,
        serial: u32,
        x: i32,
        y: i32,
    },
    PointerLeave {
        surface: SurfaceId,
    },
    PointerMotion {
        time: u32,
        x: i32,
        y: i32,
    },
    PointerButton {
        serial: u32,
        time: u32,
        button: u32,
        pressed: bool,
    },
    PointerAxis {
        time: u32,
        axis: u32,
        value: i32,
    },

    KeyboardEnter {
        surface: SurfaceId,
    },
    KeyboardLeave {
        surface: SurfaceId,
    },
    Key {
        serial: u32,
        time: u32,
        /// Legacy PS/2 set-1 make code (low 7 bits). Preserved for consumers
        /// that decode it directly.
        scancode: u32,
        /// Legacy single-byte text/pseudo-code (nav keys use 0x80..=0x88).
        ascii: u32,
        /// Canonical layout-independent keycode (USB HID usage).
        keycode: u32,
        /// Final text codepoint after layout + modifiers (0 = no text).
        codepoint: u32,
        /// `MODIFIER_*` snapshot at the time of the event, so a client can
        /// resolve the key without tracking modifier/lock state itself.
        modifiers: u32,
        pressed: bool,
    },
    Modifiers {
        mods: u32,
    },

    /// The clipboard holds `len` bytes; the client should send a destination
    /// memfd of that size via `Request::ClipboardRead`. `len == 0` means empty.
    PasteReady {
        len: u32,
    },
    /// The destination memfd from `ClipboardRead` now holds `len` valid bytes.
    PasteResult {
        len: u32,
    },

    Error {
        object_id: u32,
        code: u32,
    },
}
