//! High-level client-side protocol API.
//!
//! `Client::connect()` opens a Unix domain socket to the compositor,
//! waits for `OutputInfo`, and provides typed methods for every protocol
//! request plus event polling.
//!
//! Design: The socket is ALWAYS non-blocking (`O_NONBLOCK` set in
//! `Connection::new`). Synchronous request-response pairs use
//! `conn.wait_recv()` which blocks via `poll(fd, POLLIN, timeout)`
//! — never via sleep-spin or mode switching.

use crate::connection::Connection;
use crate::types::{Event, OutputInfo, ProtocolError, Request};
use slopos_abi::net::AF_UNIX;
use slopos_abi::unix::{SockAddrUn, UNIX_PATH_MAX};
use slopos_slibc::pal::{Pal, Sys};

pub struct Client {
    conn: Connection,
    /// Cached display geometry from the initial OutputInfo event.
    pub output: OutputInfo,
    /// Monotonically increasing ID counter for client-assigned object IDs.
    next_id: u32,
}

impl Client {
    /// Connect to the compositor (non-blocking, Wayland-style).
    ///
    /// Like `wl_display_connect()`, this only opens the socket — it does
    /// NOT wait for any server response.  The `connect()` succeeds via
    /// the kernel's listen backlog even if the compositor hasn't called
    /// `accept()` yet.
    ///
    /// Display geometry (`OutputInfo`) is received lazily: call
    /// [`ensure_output_info()`] before the first operation that needs it
    /// (typically surface creation).  By that point the compositor has
    /// had time to accept the connection and push the event.
    pub fn connect(path: &[u8]) -> Result<Self, ProtocolError> {
        let fd = Sys::socket(AF_UNIX as i32, slopos_abi::net::SOCK_STREAM as i32, 0)
            .map_err(|_| ProtocolError::Io)?;

        let mut addr = SockAddrUn::default();
        addr.family = AF_UNIX;
        let copy_len = path.len().min(UNIX_PATH_MAX - 1);
        addr.path[..copy_len].copy_from_slice(&path[..copy_len]);

        let addr_ptr = &addr as *const SockAddrUn as *const u8;
        let addr_len = core::mem::size_of::<SockAddrUn>() as u32;
        if Sys::connect(fd, addr_ptr, addr_len).is_err() {
            let _ = Sys::close(fd);
            return Err(ProtocolError::Io);
        }

        let conn = Connection::new(fd);

        Ok(Self {
            conn,
            output: OutputInfo::default(),
            next_id: 1,
        })
    }

    fn allocate_id(&mut self) -> u32 {
        let id = self.next_id;
        // Wrapping add, but skip 0 to avoid collisions with sentinel values.
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    // ── Surface operations ────────────────────────────────────────────────

    /// Create a new surface. Client assigns the ID; returns immediately.
    pub fn create_surface(&mut self) -> Result<u32, ProtocolError> {
        let id = self.allocate_id();
        self.conn.send(&Request::CreateSurface { new_id: id })?;
        Ok(id)
    }

    pub fn surface_attach(
        &mut self,
        surface: u32,
        shm_token: u32,
        width: u32,
        height: u32,
    ) -> Result<(), ProtocolError> {
        self.conn.send(&Request::SurfaceAttach {
            surface,
            shm_token,
            width,
            height,
        })
    }

    pub fn surface_damage(
        &mut self,
        surface: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<(), ProtocolError> {
        self.conn.send(&Request::SurfaceDamage {
            surface,
            x,
            y,
            w,
            h,
        })
    }

    pub fn surface_commit(&mut self, surface: u32) -> Result<(), ProtocolError> {
        self.conn.send(&Request::SurfaceCommit { surface })
    }

    pub fn surface_frame(&mut self, surface: u32) -> Result<(), ProtocolError> {
        self.conn.send(&Request::SurfaceFrame { surface })
    }

    pub fn surface_destroy(&mut self, surface: u32) -> Result<(), ProtocolError> {
        self.conn.send(&Request::SurfaceDestroy { surface })
    }

    // ── Toplevel operations ───────────────────────────────────────────────

    /// Get a toplevel role. Client assigns the ID; returns immediately.
    pub fn get_toplevel(&mut self, surface: u32) -> Result<u32, ProtocolError> {
        let id = self.allocate_id();
        self.conn.send(&Request::GetToplevel {
            surface,
            new_id: id,
        })?;
        Ok(id)
    }

    pub fn toplevel_set_title(
        &mut self,
        toplevel: u32,
        title_data: &[u8],
    ) -> Result<(), ProtocolError> {
        let mut title = [0u8; 32];
        let copy_len = title_data.len().min(32);
        title[..copy_len].copy_from_slice(&title_data[..copy_len]);
        self.conn.send(&Request::ToplevelSetTitle {
            toplevel,
            title,
            len: copy_len as u8,
        })
    }

    pub fn toplevel_set_app_id(
        &mut self,
        toplevel: u32,
        app_id_data: &[u8],
    ) -> Result<(), ProtocolError> {
        let mut app_id = [0u8; 32];
        let copy_len = app_id_data.len().min(32);
        app_id[..copy_len].copy_from_slice(&app_id_data[..copy_len]);
        self.conn.send(&Request::ToplevelSetAppId {
            toplevel,
            app_id,
            len: copy_len as u8,
        })
    }

    pub fn toplevel_destroy(&mut self, toplevel: u32) -> Result<(), ProtocolError> {
        self.conn.send(&Request::ToplevelDestroy { toplevel })
    }

    // ── Cursor ────────────────────────────────────────────────────────────

    pub fn set_cursor_shape(&mut self, surface: u32, shape: u8) -> Result<(), ProtocolError> {
        self.conn.send(&Request::SetCursorShape { surface, shape })
    }

    // ── Subsurface ────────────────────────────────────────────────────────

    pub fn get_subsurface(&mut self, surface: u32, parent: u32) -> Result<u32, ProtocolError> {
        let id = self.allocate_id();
        self.conn.send(&Request::GetSubsurface {
            surface,
            parent,
            new_id: id,
        })?;
        Ok(id)
    }

    pub fn subsurface_set_position(
        &mut self,
        subsurface: u32,
        x: i32,
        y: i32,
    ) -> Result<(), ProtocolError> {
        self.conn
            .send(&Request::SubsurfaceSetPosition { subsurface, x, y })
    }

    pub fn subsurface_destroy(&mut self, subsurface: u32) -> Result<(), ProtocolError> {
        self.conn.send(&Request::SubsurfaceDestroy { subsurface })
    }

    // ── Popup ─────────────────────────────────────────────────────────────

    pub fn get_popup(&mut self, surface: u32, parent: u32) -> Result<u32, ProtocolError> {
        let id = self.allocate_id();
        self.conn.send(&Request::GetPopup {
            surface,
            parent,
            new_id: id,
        })?;
        Ok(id)
    }

    pub fn popup_destroy(&mut self, popup: u32) -> Result<(), ProtocolError> {
        self.conn.send(&Request::PopupDestroy { popup })
    }

    // ── Input ─────────────────────────────────────────────────────────────

    pub fn get_pointer(&mut self) -> Result<u32, ProtocolError> {
        let id = self.allocate_id();
        self.conn.send(&Request::GetPointer { new_id: id })?;
        Ok(id)
    }

    pub fn get_keyboard(&mut self) -> Result<u32, ProtocolError> {
        let id = self.allocate_id();
        self.conn.send(&Request::GetKeyboard { new_id: id })?;
        Ok(id)
    }

    // ── Clipboard ─────────────────────────────────────────────────────────

    pub fn clipboard_copy(&mut self, src: &[u8]) -> Result<(), ProtocolError> {
        let mut data = [0u8; 4096];
        let copy_len = src.len().min(4096);
        data[..copy_len].copy_from_slice(&src[..copy_len]);
        self.conn.send(&Request::ClipboardCopy {
            data,
            len: copy_len as u16,
        })
    }

    pub fn clipboard_paste(&mut self) -> Result<(), ProtocolError> {
        self.conn.send(&Request::ClipboardPaste)
    }

    // ── Event polling ─────────────────────────────────────────────────────

    /// Poll for one event (non-blocking). Returns `Ok(None)` if nothing pending.
    ///
    /// The first `OutputInfo` event is consumed silently to populate the
    /// cached display geometry.  Subsequent `OutputInfo` events (e.g.,
    /// display mode changes) are returned to the caller so applications
    /// can react to resolution changes.
    pub fn poll_event(&mut self) -> Result<Option<Event>, ProtocolError> {
        match self.conn.recv::<Event>()? {
            Some(Event::OutputInfo {
                width,
                height,
                format,
                pitch,
            }) => {
                let first = self.output.width == 0;
                self.output = OutputInfo {
                    width,
                    height,
                    format,
                    pitch,
                };
                if first {
                    Ok(None)
                } else {
                    Ok(Some(Event::OutputInfo {
                        width,
                        height,
                        format,
                        pitch,
                    }))
                }
            }
            other => Ok(other),
        }
    }

    /// Block until OutputInfo has been received (like `wl_display_roundtrip`).
    ///
    /// The compositor pushes `OutputInfo` immediately upon accepting the
    /// connection.  This method uses a non-blocking `recv()` first (data
    /// is usually already buffered by the time any app needs geometry),
    /// falling back to a blocking `wait_recv()` only if it hasn't arrived.
    ///
    /// Call this before the first operation that needs display geometry
    /// (typically surface creation).
    pub fn ensure_output_info(&mut self) -> Result<(), ProtocolError> {
        if self.output.width > 0 {
            return Ok(());
        }

        // Fast path: data may already be in the socket buffer — try a
        // non-blocking recv before resorting to poll().  This avoids the
        // kernel poll path entirely in the common case where the compositor
        // has already accepted and pushed OutputInfo.
        if let Some(event) = self.conn.recv::<Event>()? {
            if let Event::OutputInfo {
                width,
                height,
                format,
                pitch,
            } = event
            {
                self.output = OutputInfo {
                    width,
                    height,
                    format,
                    pitch,
                };
                return Ok(());
            }
            return Err(ProtocolError::MalformedMessage);
        }

        // Slow path: data not yet buffered.  Block via poll().
        let event = self.conn.wait_recv::<Event>(10_000)?;
        if let Event::OutputInfo {
            width,
            height,
            format,
            pitch,
        } = event
        {
            self.output = OutputInfo {
                width,
                height,
                format,
                pitch,
            };
            Ok(())
        } else {
            Err(ProtocolError::MalformedMessage)
        }
    }

    // ── Convenience accessors ─────────────────────────────────────────────

    pub fn display_width(&self) -> u32 {
        self.output.width
    }

    pub fn display_height(&self) -> u32 {
        self.output.height
    }

    pub fn display_format(&self) -> u32 {
        self.output.format
    }

    pub fn display_pitch(&self) -> u32 {
        self.output.pitch
    }
}
