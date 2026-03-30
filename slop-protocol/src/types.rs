//! Core protocol types: Request, Event, ProtocolError, OutputInfo.

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
}

/// Display output info, received during connection bootstrap.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutputInfo {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub pitch: u32,
}

/// Client-to-server request.
#[derive(Clone)]
pub enum Request {
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
    SurfaceFrame {
        surface: u32,
    },
    SurfaceDestroy {
        surface: u32,
    },
    GetToplevel {
        surface: u32,
        new_id: u32,
    },
    ToplevelSetTitle {
        toplevel: u32,
        title: [u8; 32],
        len: u8,
    },
    ToplevelSetAppId {
        toplevel: u32,
        app_id: [u8; 32],
        len: u8,
    },
    ToplevelDestroy {
        toplevel: u32,
    },
    SetCursorShape {
        surface: u32,
        shape: u8,
    },
    GetSubsurface {
        surface: u32,
        parent: u32,
        new_id: u32,
    },
    SubsurfaceSetPosition {
        subsurface: u32,
        x: i32,
        y: i32,
    },
    SubsurfaceDestroy {
        subsurface: u32,
    },
    GetPopup {
        surface: u32,
        parent: u32,
        new_id: u32,
    },
    PopupDestroy {
        popup: u32,
    },
    GetPointer {
        new_id: u32,
    },
    GetKeyboard {
        new_id: u32,
    },
    ClipboardCopy {
        data: [u8; 4096],
        len: u16,
    },
    ClipboardPaste,
}

/// Server-to-client event.
#[derive(Clone, Copy)]
pub enum Event {
    FrameDone {
        surface: u32,
        timestamp_ms: u32,
    },
    Configure {
        toplevel: u32,
        width: u32,
        height: u32,
    },
    Close {
        toplevel: u32,
    },
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
        surface: u32,
    },
    KeyboardLeave {
        surface: u32,
    },
    Key {
        time: u32,
        scancode: u32,
        ascii: u32,
        pressed: bool,
    },
    Modifiers {
        mods: u32,
    },
    OutputInfo {
        width: u32,
        height: u32,
        format: u32,
        pitch: u32,
    },
    PasteResult {
        data: [u8; 4096],
        len: u16,
    },
    Error {
        code: u32,
    },
}
