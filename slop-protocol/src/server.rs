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

/// Maximum number of simultaneous client connections the server tracks.
///
/// Exposed so owners (e.g. the compositor) can size their own per-client
/// scratch buffers — such as the index array passed to
/// [`Server::take_disconnected`] — to match the server's capacity.
pub const MAX_CLIENTS: usize = 32;

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
    // Each client is heap-allocated (`Box<ClientConn>`) so the slot array is
    // a tiny 32-pointer table whose `Option` niche is the single, unambiguous
    // `Box` null pointer (None == null). Storing the 4 KB+ `ClientConn`
    // *inline* gave `Option<ClientConn>` two competing niches (the inner
    // `read_buf: Box` null and the `active: bool` 2..=255 range); rustc then
    // const-initialised `[None; N]` with a `memset(0x02)` (the bool niche)
    // while *reading* the discriminant through the `Box` niche, so every
    // freshly-`bind()`-ed slot read back as `Some(garbage)`. Boxing the
    // client collapses that to one canonical `Option<Box<T>>` niche and
    // mirrors libwayland-server, which heap-allocates every `wl_client`.
    pub clients: alloc::boxed::Box<[Option<alloc::boxed::Box<ClientConn>>; MAX_CLIENTS]>,
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

        const NONE: Option<alloc::boxed::Box<ClientConn>> = None;
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

        const NONE: Option<alloc::boxed::Box<ClientConn>> = None;
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
        self.clients[idx] = Some(alloc::boxed::Box::new(ClientConn {
            conn,
            active: true,
            write_buf: WriteBuf::new(),
        }));
        self.client_count += 1;
        Ok(Some(idx))
    }

    /// Read one request from a client (non-blocking).
    /// File descriptors received via SCM_RIGHTS are consumed inline by
    /// the decoder and embedded in the returned `Request` variant.
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
    /// attempted.  If both the buffer and the kernel socket buffer are
    /// saturated (client truly unresponsive) the client is *flagged*
    /// disconnected and `Err(Disconnected)` is returned — matching
    /// libwayland-server's overflow policy.  The connection slot is **not**
    /// freed here: reclaiming it (and tearing down any owner-side state keyed
    /// on this client) is the responsibility of the single teardown funnel,
    /// driven via [`take_disconnected`].  See that method for the rationale.
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
            Err(_) => {
                // Broken pipe / EOF / IO: peer is gone.  Flag, don't free.
                self.mark_disconnected(client);
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
        self.mark_disconnected(client);
        Err(ProtocolError::Disconnected)
    }

    /// Non-blocking flush of all clients' write buffers.
    ///
    /// Call once per frame.  Clients whose flush hits a hard error (broken
    /// pipe, EOF) are *flagged* disconnected — the common signal that a GUI
    /// client was killed, since the compositor continuously sends it input
    /// and frame events.  `EAGAIN` is benign — data stays buffered for the
    /// next frame.  Flagged clients are reclaimed by the owner via
    /// [`take_disconnected`]; the slot is not freed here.
    pub fn flush_clients(&mut self) {
        for i in 0..MAX_CLIENTS {
            let died = match self.clients.get_mut(i) {
                Some(Some(c)) if c.active && !c.write_buf.is_empty() => {
                    c.write_buf.flush(c.conn.fd()).is_err()
                }
                _ => false,
            };
            if died {
                self.mark_disconnected(i);
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
    /// If EOF is detected, flags the client disconnected and returns `true`.
    /// Any data received is buffered for the next `recv_request`.  The slot
    /// is reclaimed by the owner via [`take_disconnected`].
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

    /// Flag a client as disconnected without freeing its slot.
    ///
    /// This is the *detection* half of the single teardown funnel.  Every
    /// path that observes a dead peer (broken pipe on flush, write-buffer
    /// overflow, recv EOF) routes here, leaving the connection slot
    /// allocated but inactive.  The owner reclaims it — and tears down any
    /// state keyed on the client index — through [`take_disconnected`].
    /// Mirrors how libwayland-server splits disconnect *detection* (event
    /// loop sees `EPOLLHUP`) from disconnect *teardown* (`wl_client_destroy`
    /// walks and destroys the client's resources).
    fn mark_disconnected(&mut self, client: usize) {
        if let Some(Some(c)) = self.clients.get_mut(client) {
            c.active = false;
        }
    }

    /// Collect the indices of every client flagged disconnected but not yet
    /// reclaimed, writing them into `out` and returning the count.
    ///
    /// The slots stay allocated: the caller is expected to run per-client
    /// teardown (e.g. destroy the client's surfaces) and then call
    /// [`disconnect`] for each returned index.  This is the choke point that
    /// makes connection teardown a single funnel — a slot transitions to
    /// freed *only* after the owner has been given the chance to clean up,
    /// so owner-side state can never be orphaned by an internally-detected
    /// disconnect.
    pub fn take_disconnected(&mut self, out: &mut [usize]) -> usize {
        let mut n = 0;
        for (i, slot) in self.clients.iter().enumerate() {
            if n >= out.len() {
                break;
            }
            if let Some(c) = slot {
                if !c.active {
                    out[n] = i;
                    n += 1;
                }
            }
        }
        n
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
        // Client connections are dropped via Box<[Option<Box<ClientConn>>]>,
        // each Connection::drop closes its FD. But the listen FD is
        // owned directly and must be closed explicitly.
        let _ = Sys::close(self.listen_fd);
    }
}
