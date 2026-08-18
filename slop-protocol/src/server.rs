//! Server-side multi-client protocol manager: listens on a Unix domain socket,
//! accepts client connections, and dispatches typed requests.
//!
//! Events are written into a per-client [`WriteBuf`]; [`Server::flush_clients`]
//! drains them once per frame with non-blocking `send()`. `EAGAIN` leaves the
//! data buffered for the next frame; only a buffer overflow disconnects.

use crate::codec::Encode;
use crate::connection::Connection;
use crate::types::{Event, ProtocolError, Request};
use slopos_abi::net::AF_UNIX;
use slopos_abi::unix::{SockAddrUn, UNIX_PATH_MAX};
use slopos_slibc::pal::{Pal, Sys};

/// Maximum number of simultaneous client connections the server tracks.
/// Exposed so owners can size the index array passed to
/// [`Server::take_disconnected`].
pub const MAX_CLIENTS: usize = 32;

/// Outgoing event buffer size per client. 4 KB holds ~200+ typical events —
/// roughly 3 seconds of backlog at 60 fps before disconnection.
const WRITE_BUF_SIZE: usize = 4096;

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

    /// Append a length-prefixed message. Returns `false` if there is not
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

    /// Non-blocking flush to socket. Returns:
    /// - `Ok(true)`  — buffer fully drained.
    /// - `Ok(false)` — partial send or EAGAIN, data remains for retry.
    /// - `Err(…)`    — hard error (disconnected / IO).
    fn flush(&mut self, fd: i32) -> Result<bool, ProtocolError> {
        while self.len > 0 {
            match Sys::send(fd, self.data.as_ptr(), self.len, 0) {
                Ok(n) if n > 0 => {
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
                    return Ok(false);
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
    // Each client is boxed so `Option<Box<ClientConn>>` has one canonical niche
    // (None == null). Inline, `Option<ClientConn>` had two competing niches —
    // the inner `read_buf: Box` and `active: bool` — and rustc const-initialised
    // `[None; N]` through one while reading the discriminant through the other,
    // so a fresh slot read back as `Some(garbage)`.
    pub clients: alloc::boxed::Box<[Option<alloc::boxed::Box<ClientConn>>; MAX_CLIENTS]>,
    pub client_count: usize,
}

impl Server {
    /// Create a Server from an inherited (socket-activated) listen FD, which
    /// must already be bound and listening.
    pub fn from_fd(listen_fd: i32) -> Result<Self, ProtocolError> {
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
        crate::connection::set_nonblock(fd);

        const NONE: Option<alloc::boxed::Box<ClientConn>> = None;
        Ok(Self {
            listen_fd: fd,
            clients: alloc::boxed::Box::new([NONE; MAX_CLIENTS]),
            client_count: 0,
        })
    }

    /// Non-blocking accept. Returns the client index if a connection was
    /// accepted, `Ok(None)` on EAGAIN/EWOULDBLOCK, `Err` on a real error.
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
    pub fn recv_request(&mut self, client: usize) -> Result<Option<Request>, ProtocolError> {
        match self.clients.get_mut(client) {
            Some(Some(c)) if c.active => c.conn.recv::<Request>(),
            _ => Ok(None),
        }
    }

    /// Queue an event into the client's write buffer (never blocks); socket I/O
    /// happens in [`flush_clients`].
    ///
    /// If the buffer is full an emergency non-blocking flush is attempted; if
    /// that still leaves no room the client is *flagged* disconnected and
    /// `Err(Disconnected)` returned. The slot is **not** freed here — teardown
    /// runs through [`take_disconnected`].
    pub fn queue_event(&mut self, client: usize, event: &Event) -> Result<(), ProtocolError> {
        // No length header here — `WriteBuf::put` adds the framing.
        let mut scratch = [0u8; 8192];
        let payload_len = event.encode(&mut scratch)?;
        let payload = &scratch[..payload_len];

        let c = match self.clients.get_mut(client) {
            Some(Some(c)) if c.active => c,
            _ => return Err(ProtocolError::Disconnected),
        };

        if c.write_buf.put(payload) {
            return Ok(());
        }

        match c.write_buf.flush(c.conn.fd()) {
            Ok(_) => {}
            Err(_) => {
                self.mark_disconnected(client);
                return Err(ProtocolError::Disconnected);
            }
        }

        let c = match self.clients.get_mut(client) {
            Some(Some(c)) if c.active => c,
            _ => return Err(ProtocolError::Disconnected),
        };
        if c.write_buf.put(payload) {
            return Ok(());
        }

        self.mark_disconnected(client);
        Err(ProtocolError::Disconnected)
    }

    /// Non-blocking flush of all clients' write buffers; call once per frame.
    ///
    /// A hard error (broken pipe, EOF) *flags* the client disconnected for the
    /// owner to reclaim via [`take_disconnected`]; `EAGAIN` is benign and
    /// leaves the data buffered for the next frame.
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

    pub fn is_connected(&self, client: usize) -> bool {
        matches!(self.clients.get(client), Some(Some(c)) if c.active)
    }

    /// Probe a client for disconnection without consuming any messages: a
    /// non-blocking `recv()` whose data stays buffered for the next
    /// `recv_request`. EOF flags the client disconnected and returns `true`.
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
    /// Every path that observes a dead peer routes here; the owner reclaims the
    /// slot — and tears down any state keyed on the client index — through
    /// [`take_disconnected`].
    fn mark_disconnected(&mut self, client: usize) {
        if let Some(Some(c)) = self.clients.get_mut(client) {
            c.active = false;
        }
    }

    /// Collect the indices of every client flagged disconnected but not yet
    /// reclaimed, writing them into `out` and returning the count.
    ///
    /// The slots stay allocated: the caller runs per-client teardown and then
    /// calls [`disconnect`] for each returned index, so owner-side state can
    /// never be orphaned by an internally-detected disconnect.
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

    pub fn listen_fd(&self) -> i32 {
        self.listen_fd
    }

    pub fn client_count(&self) -> usize {
        self.client_count
    }

    /// Build an array of poll FDs: the listen socket at index 0, then every
    /// connected client. Returns the number of valid entries.
    pub fn build_poll_fds(&self, out: &mut [slopos_abi::syscall::types::UserPollFd]) -> usize {
        use slopos_abi::syscall::posix::POLLIN;
        use slopos_abi::syscall::types::UserPollFd;

        let max = out.len();
        if max == 0 {
            return 0;
        }

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
        // Client FDs close via `Connection::drop`; the listen FD is owned here.
        let _ = Sys::close(self.listen_fd);
    }
}
