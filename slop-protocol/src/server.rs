//! Server-side multi-client protocol manager.
//!
//! The compositor creates a `Server`, which listens on a Unix domain socket,
//! accepts client connections, and dispatches typed requests.
//!
//! # Non-blocking event delivery
//!
//! Events are written into a per-client [`WriteBuf`] (4 KB, matching
//! libwayland-server's design).  The compositor calls [`Server::flush_clients`]
//! once per frame to drain the buffers with non-blocking `send()`.
//!
//! If a flush gets `EAGAIN` the data stays in the buffer and is retried next
//! frame.  Only when the buffer itself overflows (client truly unresponsive)
//! is the client disconnected.  This gives healthy clients a grace period for
//! transient slowness (long paint, GC pause, scheduling delay) while still
//! protecting the compositor from blocking.

use crate::codec::Encode;
use crate::connection::Connection;
use crate::types::{Event, ProtocolError, Request};
use slopos_abi::net::AF_UNIX;
use slopos_abi::unix::{SockAddrUn, UNIX_PATH_MAX};
use slopos_slibc::pal::{Pal, Sys};

const MAX_CLIENTS: usize = 32;

// ---------------------------------------------------------------------------
// Per-client write buffer (matches libwayland-server's 4 KB design)
// ---------------------------------------------------------------------------

/// Outgoing event buffer size per client.  4 KB holds ~200+ typical events
/// (PointerMotion ~17 B, Key ~18 B, FrameDone ~13 B), giving a client
/// roughly 3 seconds of backlog at 60 fps before disconnection.
const WRITE_BUF_SIZE: usize = 4096;

/// Per-client outgoing buffer.
///
/// Events are serialized into this buffer by [`Server::queue_event`].
/// The compositor calls [`Server::flush_clients`] once per frame to
/// drain all buffers with non-blocking `send()`.
struct WriteBuf {
    data: [u8; WRITE_BUF_SIZE],
    len: usize,
}

impl WriteBuf {
    const fn new() -> Self {
        Self {
            data: [0; WRITE_BUF_SIZE],
            len: 0,
        }
    }

    /// Append a length-prefixed message.  Returns `false` if there is not
    /// enough room (caller should flush or disconnect).
    fn put(&mut self, payload: &[u8]) -> bool {
        let framed_len = 4 + payload.len();
        if self.len + framed_len > WRITE_BUF_SIZE {
            return false;
        }
        let header = (payload.len() as u32).to_le_bytes();
        self.data[self.len..self.len + 4].copy_from_slice(&header);
        self.data[self.len + 4..self.len + framed_len].copy_from_slice(payload);
        self.len += framed_len;
        true
    }

    /// Non-blocking flush to socket.  Returns:
    /// - `Ok(true)`  — buffer fully drained.
    /// - `Ok(false)` — partial send or EAGAIN, data remains for retry.
    /// - `Err(…)`    — hard error (disconnected / IO).
    fn flush(&mut self, fd: i32) -> Result<bool, ProtocolError> {
        while self.len > 0 {
            match Sys::send(fd, self.data.as_ptr(), self.len, 0) {
                Ok(n) if n > 0 => {
                    // Compact: shift remaining bytes to front.
                    let remaining = self.len - n;
                    if remaining > 0 {
                        self.data.copy_within(n..self.len, 0);
                    }
                    self.len = remaining;
                }
                Ok(0) => return Err(ProtocolError::Disconnected),
                Err(e)
                    if e == slopos_slibc::errno::EAGAIN
                        || e == slopos_slibc::errno::EWOULDBLOCK =>
                {
                    return Ok(false); // retry next frame
                }
                Err(_) => return Err(ProtocolError::Io),
                Ok(_) => unreachable!(),
            }
        }
        Ok(true)
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub struct ClientConn {
    pub conn: Connection,
    pub active: bool,
    write_buf: WriteBuf,
}

pub struct Server {
    listen_fd: i32,
    pub clients: alloc::boxed::Box<[Option<ClientConn>; MAX_CLIENTS]>,
    pub client_count: usize,
}

impl Server {
    /// Create a Server from an inherited (socket-activated) listen FD.
    ///
    /// The FD must already be bound and listening. This is the systemd-style
    /// activation path: init pre-creates the socket, the compositor inherits
    /// it, and calls this constructor instead of `bind()`.
    pub fn from_fd(listen_fd: i32) -> Result<Self, ProtocolError> {
        // Ensure non-blocking for accept().
        crate::connection::set_nonblock(listen_fd);

        const NONE: Option<ClientConn> = None;
        Ok(Self {
            listen_fd,
            clients: alloc::boxed::Box::new([NONE; MAX_CLIENTS]),
            client_count: 0,
        })
    }

    /// Create a listening Unix domain socket at the given path.
    pub fn bind(path: &[u8]) -> Result<Self, ProtocolError> {
        let fd = Sys::socket(AF_UNIX as i32, slopos_abi::net::SOCK_STREAM as i32, 0)
            .map_err(|_| ProtocolError::Io)?;

        let mut addr = SockAddrUn::default();
        addr.family = AF_UNIX;
        let copy_len = path.len().min(UNIX_PATH_MAX - 1);
        addr.path[..copy_len].copy_from_slice(&path[..copy_len]);

        let addr_ptr = &addr as *const SockAddrUn as *const u8;
        let addr_len = core::mem::size_of::<SockAddrUn>() as u32;
        Sys::bind(fd, addr_ptr, addr_len).map_err(|_| ProtocolError::Io)?;
        Sys::listen(fd, 32).map_err(|_| ProtocolError::Io)?;
        // Non-blocking so accept() returns EAGAIN instead of blocking
        crate::connection::set_nonblock(fd);

        const NONE: Option<ClientConn> = None;
        Ok(Self {
            listen_fd: fd,
            clients: alloc::boxed::Box::new([NONE; MAX_CLIENTS]),
            client_count: 0,
        })
    }

    /// Non-blocking accept. Returns client index if a new connection was accepted.
    ///
    /// Returns `Ok(None)` when no pending connections (EAGAIN/EWOULDBLOCK).
    /// Propagates real errors (EMFILE, ENOMEM, etc.) as `Err`.
    pub fn accept(&mut self) -> Result<Option<usize>, ProtocolError> {
        let client_fd =
            match Sys::accept(self.listen_fd, core::ptr::null_mut(), core::ptr::null_mut()) {
                Ok(fd) => fd,
                Err(e)
                    if e == slopos_slibc::errno::EAGAIN
                        || e == slopos_slibc::errno::EWOULDBLOCK =>
                {
                    return Ok(None);
                }
                Err(_) => return Err(ProtocolError::Io),
            };

        let idx = match self.clients.iter().position(|c| c.is_none()) {
            Some(i) => i,
            None => {
                let _ = Sys::close(client_fd);
                return Err(ProtocolError::BufferFull);
            }
        };

        // Connection::new sets O_NONBLOCK automatically.
        let conn = Connection::new(client_fd);
        self.clients[idx] = Some(ClientConn {
            conn,
            active: true,
            write_buf: WriteBuf::new(),
        });
        self.client_count += 1;
        Ok(Some(idx))
    }

    /// Read one request from a client (non-blocking).
    pub fn recv_request(&mut self, client: usize) -> Result<Option<Request>, ProtocolError> {
        match self.clients.get_mut(client) {
            Some(Some(c)) if c.active => c.conn.recv::<Request>(),
            _ => Ok(None),
        }
    }

    /// Queue an event into the client's write buffer (never blocks).
    ///
    /// The event is serialized and appended to the per-client [`WriteBuf`].
    /// Actual socket I/O happens in [`flush_clients`], called once per frame.
    ///
    /// If the write buffer is full, an emergency non-blocking flush is
    /// attempted.  The client is disconnected only if both the buffer and
    /// the kernel socket buffer are saturated — matching libwayland-server's
    /// overflow policy.
    pub fn queue_event(&mut self, client: usize, event: &Event) -> Result<(), ProtocolError> {
        // Encode the payload into a scratch buffer (no length header — WriteBuf
        // adds its own framing, identical to Connection's wire format).
        let mut scratch = [0u8; 8192];
        let payload_len = event.encode(&mut scratch)?;
        let payload = &scratch[..payload_len];

        let c = match self.clients.get_mut(client) {
            Some(Some(c)) if c.active => c,
            _ => return Err(ProtocolError::Disconnected),
        };

        // Fast path: room in the buffer.
        if c.write_buf.put(payload) {
            return Ok(());
        }

        // Buffer full — emergency flush to make room.
        match c.write_buf.flush(c.conn.fd()) {
            Ok(_) => {}
            Err(ProtocolError::Disconnected) => {
                let idx = client;
                self.disconnect(idx);
                return Err(ProtocolError::Disconnected);
            }
            Err(_) => {
                let idx = client;
                self.disconnect(idx);
                return Err(ProtocolError::Disconnected);
            }
        }

        // Retry the put after flush.
        let c = match self.clients.get_mut(client) {
            Some(Some(c)) if c.active => c,
            _ => return Err(ProtocolError::Disconnected),
        };
        if c.write_buf.put(payload) {
            return Ok(());
        }

        // Buffer AND kernel socket both full — client is genuinely stuck.
        self.disconnect(client);
        Err(ProtocolError::Disconnected)
    }

    /// Non-blocking flush of all clients' write buffers.
    ///
    /// Call once per frame.  Clients whose flush hits a hard error
    /// (broken pipe, EOF) are disconnected.  `EAGAIN` is benign — data
    /// stays buffered for the next frame.
    pub fn flush_clients(&mut self) {
        for i in 0..MAX_CLIENTS {
            let should_disconnect = match self.clients.get_mut(i) {
                Some(Some(c)) if c.active && !c.write_buf.is_empty() => {
                    match c.write_buf.flush(c.conn.fd()) {
                        Ok(_) => false,
                        Err(_) => true,
                    }
                }
                _ => false,
            };
            if should_disconnect {
                self.disconnect(i);
            }
        }
    }

    /// Check if a client is still connected.
    pub fn is_connected(&self, client: usize) -> bool {
        matches!(self.clients.get(client), Some(Some(c)) if c.active)
    }

    /// Probe a client for disconnection without consuming any messages.
    ///
    /// Performs a non-blocking `recv()` into the connection's read buffer.
    /// If EOF is detected, marks the client as disconnected and returns
    /// `true`.  Any data received is buffered for the next `recv_request`.
    pub fn probe_disconnected(&mut self, client: usize) -> bool {
        match self.clients.get_mut(client) {
            Some(Some(c)) if c.active => {
                if c.conn.probe_disconnected() {
                    c.active = false;
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Disconnect and clean up a client slot.
    pub fn disconnect(&mut self, client: usize) {
        if let Some(slot) = self.clients.get_mut(client) {
            if slot.take().is_some() {
                self.client_count = self.client_count.saturating_sub(1);
            }
        }
    }

    /// Get the server's listening socket FD.
    pub fn listen_fd(&self) -> i32 {
        self.listen_fd
    }

    /// Get the number of connected clients.
    pub fn client_count(&self) -> usize {
        self.client_count
    }

    /// Build an array of poll FDs for the listen socket + all connected clients.
    /// Returns the number of valid entries.
    pub fn build_poll_fds(&self, out: &mut [slopos_abi::syscall::types::UserPollFd]) -> usize {
        use slopos_abi::syscall::posix::POLLIN;
        use slopos_abi::syscall::types::UserPollFd;

        let max = out.len();
        if max == 0 {
            return 0;
        }

        // Listen socket first
        out[0] = UserPollFd {
            fd: self.listen_fd,
            events: POLLIN,
            revents: 0,
        };
        let mut count = 1;

        for client in self.clients.iter() {
            if let Some(c) = client {
                if c.active && count < max {
                    out[count] = UserPollFd {
                        fd: c.conn.fd(),
                        events: POLLIN,
                        revents: 0,
                    };
                    count += 1;
                }
            }
        }

        count
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Client connections are dropped via Box<[Option<ClientConn>]>,
        // each Connection::drop closes its FD. But the listen FD is
        // owned directly and must be closed explicitly.
        let _ = Sys::close(self.listen_fd);
    }
}
