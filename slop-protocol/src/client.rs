//! High-level client-side protocol API.
//!
//! `Client::connect()` opens a Unix domain socket to the compositor,
//! performs the version handshake, and provides typed methods for every
//! protocol request plus event polling.

use crate::connection::Connection;
use crate::types::{Event, MAX_STRING_LEN, OutputInfo, PROTOCOL_VERSION, ProtocolError, Request};
use slopos_abi::net::AF_UNIX;
use slopos_abi::unix::{SockAddrUn, UNIX_PATH_MAX};
use slopos_slibc::pal::{Pal, Sys};

pub struct Client {
    conn: Connection,
    /// Cached display geometry from the initial OutputInfo event.
    pub output: OutputInfo,
    /// Compositor capability flags from the Hello handshake.
    pub capabilities: u64,
    /// Monotonically increasing ID counter for client-assigned object IDs.
    next_id: u32,
}

impl Client {
    /// Connect to the compositor and perform the version handshake.
    ///
    /// 1. Opens the AF_UNIX socket at `path`.
    /// 2. Receives `Event::Hello { version, capabilities }`.
    /// 3. Sends `Request::Hello { version }` back.
    ///
    /// Display geometry (`OutputInfo`) is received lazily via
    /// [`ensure_output_info()`] before the first surface operation.
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

        let mut conn = Connection::new(fd);

        // Receive server Hello (blocking, 5 second timeout).
        let hello: Event = conn.wait_recv(5000)?;
        let capabilities = match hello {
            Event::Hello {
                version,
                capabilities,
            } => {
                if version != PROTOCOL_VERSION {
                    return Err(ProtocolError::VersionMismatch);
                }
                capabilities
            }
            _ => return Err(ProtocolError::MalformedMessage),
        };

        // Send client Hello response.
        conn.send(&Request::Hello {
            version: PROTOCOL_VERSION,
        })?;

        Ok(Self {
            conn,
            output: OutputInfo::default(),
            capabilities,
            next_id: 1,
        })
    }

    fn allocate_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        id
    }

    // -- Surface operations -----------------------------------------------

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

    /// Request a FrameDone callback for this surface.
    pub fn surface_frame(&mut self, surface: u32) -> Result<(), ProtocolError> {
        self.conn.send(&Request::SurfaceFrame { surface })
    }

    pub fn surface_destroy(&mut self, surface: u32) -> Result<(), ProtocolError> {
        self.conn.send(&Request::SurfaceDestroy { surface })
    }

    // -- Toplevel operations ----------------------------------------------

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
        let mut title = [0u8; MAX_STRING_LEN];
        let copy_len = title_data.len().min(MAX_STRING_LEN);
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
        let mut app_id = [0u8; MAX_STRING_LEN];
        let copy_len = app_id_data.len().min(MAX_STRING_LEN);
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

    /// Acknowledge a Configure event's serial before committing a resized buffer.
    pub fn ack_configure(&mut self, serial: u32) -> Result<(), ProtocolError> {
        self.conn.send(&Request::AckConfigure { serial })
    }

    // -- Cursor -----------------------------------------------------------

    pub fn set_cursor_shape(&mut self, surface: u32, shape: u8) -> Result<(), ProtocolError> {
        self.conn.send(&Request::SetCursorShape { surface, shape })
    }

    // -- Clipboard --------------------------------------------------------

    pub fn clipboard_copy(&mut self, src: &[u8]) -> Result<(), ProtocolError> {
        let mut data = [0u8; 4096];
        let copy_len = src.len().min(4096);
        data[..copy_len].copy_from_slice(&src[..copy_len]);
        self.conn
            .send(&Request::ClipboardCopy(alloc::boxed::Box::new(
                crate::types::ClipboardData {
                    data,
                    len: copy_len as u16,
                },
            )))
    }

    pub fn clipboard_paste(&mut self) -> Result<(), ProtocolError> {
        self.conn.send(&Request::ClipboardPaste)
    }

    // -- Interactive (compositor-driven) -----------------------------------

    /// Start an interactive move. Serial must come from a recent pointer event.
    pub fn interactive_move(&mut self, toplevel: u32, serial: u32) -> Result<(), ProtocolError> {
        self.conn
            .send(&Request::InteractiveMove { toplevel, serial })
    }

    /// Start an interactive resize. Serial must come from a recent pointer event.
    pub fn interactive_resize(
        &mut self,
        toplevel: u32,
        serial: u32,
        edges: u32,
    ) -> Result<(), ProtocolError> {
        self.conn.send(&Request::InteractiveResize {
            toplevel,
            serial,
            edges,
        })
    }

    // -- Event polling ----------------------------------------------------

    /// Poll for one event (non-blocking). Returns `Ok(None)` if nothing pending.
    ///
    /// The first `OutputInfo` event is consumed silently to populate the
    /// cached display geometry. Subsequent ones are returned to the caller.
    pub fn poll_event(&mut self) -> Result<Option<Event>, ProtocolError> {
        match self.conn.recv::<Event>()? {
            Some(Event::OutputInfo {
                width,
                height,
                format,
                pitch,
                scale,
            }) => {
                let first = self.output.width == 0;
                self.output = OutputInfo {
                    width,
                    height,
                    format,
                    pitch,
                    scale,
                };
                if first {
                    Ok(None)
                } else {
                    Ok(Some(Event::OutputInfo {
                        width,
                        height,
                        format,
                        pitch,
                        scale,
                    }))
                }
            }
            other => Ok(other),
        }
    }

    /// Block until OutputInfo has been received.
    pub fn ensure_output_info(&mut self) -> Result<(), ProtocolError> {
        if self.output.width > 0 {
            return Ok(());
        }

        const TOTAL_TIMEOUT_MS: u64 = 10_000;
        let start = crate::timestamp_ms();
        let deadline = start.saturating_add(TOTAL_TIMEOUT_MS);

        loop {
            let event = match self.conn.recv::<Event>()? {
                Some(e) => e,
                None => {
                    let now = crate::timestamp_ms();
                    if now >= deadline {
                        return Err(ProtocolError::Timeout);
                    }
                    let remaining = (deadline - now) as i32;
                    self.conn.wait_recv::<Event>(remaining)?
                }
            };
            if let Event::OutputInfo {
                width,
                height,
                format,
                pitch,
                scale,
            } = event
            {
                self.output = OutputInfo {
                    width,
                    height,
                    format,
                    pitch,
                    scale,
                };
                return Ok(());
            }
        }
    }

    // -- Convenience accessors --------------------------------------------

    pub fn fd(&self) -> i32 {
        self.conn.fd()
    }

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

    pub fn display_scale(&self) -> u32 {
        self.output.scale
    }
}
