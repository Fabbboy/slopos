//! Core protocol types: Request, Event, ProtocolError, OutputInfo.
//!
//! Protocol version 2 -- version handshake, capability discovery,
//! configure/ack semantics, explicit frame callbacks, input serials,
//! and interactive move/resize.

/// Current wire protocol version.
pub const PROTOCOL_VERSION: u32 = 2;

/// Capability flags advertised by the compositor in [`Event::Hello`].
pub mod caps {
    /// Compositor supports toplevel windows.
    pub const TOPLEVEL: u64 = 1 << 0;
    /// Compositor supports clipboard copy/paste.
    pub const CLIPBOARD: u64 = 1 << 1;
    /// Compositor supports interactive move/resize.
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

/// Error type for all protocol operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    /// Socket send/recv failed.
    Io,
    /// Message too large for buffer.
    MessageTooLarge,
    /// Received malformed message (bad tag, truncated, etc.).
    MalformedMessage,
    /// Timeout waiting for response.
    Timeout,
    /// Connection was closed by peer.
    Disconnected,
    /// Buffer full, message couldn't be queued.
    BufferFull,
    /// Incompatible protocol version.
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

/// Heap-allocated clipboard payload to avoid bloating the Request/Event
/// enums to 4 KiB+ per value.
#[derive(Clone)]
pub struct ClipboardData {
    pub data: [u8; 4096],
    pub len: u16,
}

/// Maximum byte length for title and app-id strings on the wire.
pub const MAX_STRING_LEN: usize = 128;

/// Client-to-server request.
#[derive(Clone)]
pub enum Request {
    /// Protocol version handshake (client response to server Hello).
    Hello {
        version: u32,
    },

    // -- Surface lifecycle ------------------------------------------------
    CreateSurface {
        new_id: u32,
    },
    SurfaceAttach {
        surface: u32,
        shm_token: u32,
        width: u32,
        height: u32,
    },
    SurfaceDamage {
        surface: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    SurfaceCommit {
        surface: u32,
    },
    /// Request a [`Event::FrameDone`] callback after the next present.
    SurfaceFrame {
        surface: u32,
    },
    SurfaceDestroy {
        surface: u32,
    },

    // -- Toplevel lifecycle ------------------------------------------------
    GetToplevel {
        surface: u32,
        new_id: u32,
    },
    ToplevelSetTitle {
        toplevel: u32,
        title: [u8; MAX_STRING_LEN],
        len: u8,
    },
    ToplevelSetAppId {
        toplevel: u32,
        app_id: [u8; MAX_STRING_LEN],
        len: u8,
    },
    ToplevelDestroy {
        toplevel: u32,
    },
    /// Acknowledge a [`Event::Configure`] serial before committing.
    AckConfigure {
        serial: u32,
    },

    // -- Cursor -----------------------------------------------------------
    SetCursorShape {
        surface: u32,
        shape: u8,
    },

    // -- Clipboard --------------------------------------------------------
    ClipboardCopy(alloc::boxed::Box<ClipboardData>),
    ClipboardPaste,

    // -- Interactive (compositor-driven) -----------------------------------
    /// Start an interactive window move. Serial must match a recent pointer event.
    InteractiveMove {
        toplevel: u32,
        serial: u32,
    },
    /// Start an interactive window resize. Serial must match a recent pointer event.
    InteractiveResize {
        toplevel: u32,
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

    // -- Display ----------------------------------------------------------
    OutputInfo {
        width: u32,
        height: u32,
        format: u32,
        pitch: u32,
        scale: u32,
    },

    // -- Frame synchronization --------------------------------------------
    /// Sent only to surfaces that requested it via [`Request::SurfaceFrame`].
    FrameDone {
        surface: u32,
        timestamp_ms: u32,
    },

    // -- Toplevel ---------------------------------------------------------
    /// Window state/size change. Client must [`Request::AckConfigure`] the
    /// serial before committing a buffer at the new size.
    Configure {
        toplevel: u32,
        serial: u32,
        width: u32,
        height: u32,
        states: u32,
    },
    Close {
        toplevel: u32,
    },

    // -- Pointer ----------------------------------------------------------
    PointerEnter {
        surface: u32,
        x: i32,
        y: i32,
    },
    PointerLeave {
        surface: u32,
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

    // -- Keyboard ---------------------------------------------------------
    KeyboardEnter {
        surface: u32,
    },
    KeyboardLeave {
        surface: u32,
    },
    Key {
        serial: u32,
        time: u32,
        scancode: u32,
        ascii: u32,
        pressed: bool,
    },
    Modifiers {
        mods: u32,
    },

    // -- Clipboard --------------------------------------------------------
    PasteResult(alloc::boxed::Box<ClipboardData>),

    // -- Error ------------------------------------------------------------
    Error {
        object_id: u32,
        code: u32,
    },
}
