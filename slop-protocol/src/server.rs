//! Server-side multi-client protocol manager.
//!
//! The compositor creates a `Server`, which listens on a Unix domain socket,
//! accepts client connections, and dispatches typed requests.

use crate::connection::Connection;
use crate::types::{Event, ProtocolError, Request};
use slopos_abi::net::AF_UNIX;
use slopos_abi::syscall::posix::{F_SETFL, O_NONBLOCK};
use slopos_abi::unix::{SockAddrUn, UNIX_PATH_MAX};
use slopos_slibc::pal::{Pal, Sys};

const MAX_CLIENTS: usize = 32;

pub struct ClientConn {
    pub conn: Connection,
    pub active: bool,
}

pub struct Server {
    listen_fd: i32,
    pub clients: alloc::boxed::Box<[Option<ClientConn>; MAX_CLIENTS]>,
    pub client_count: usize,
    next_id: u32,
}

impl Server {
    /// Create a Server from an inherited (socket-activated) listen FD.
    ///
    /// The FD must already be bound and listening. This is the systemd-style
    /// activation path: init pre-creates the socket, the compositor inherits
    /// it, and calls this constructor instead of `bind()`.
    pub fn from_fd(listen_fd: i32) -> Result<Self, ProtocolError> {
        // Ensure non-blocking for accept().
        let _ = Sys::fcntl(listen_fd, F_SETFL as i32, O_NONBLOCK);

        const NONE: Option<ClientConn> = None;
        Ok(Self {
            listen_fd,
            clients: alloc::boxed::Box::new([NONE; MAX_CLIENTS]),
            client_count: 0,
            next_id: 1,
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
        let _ = Sys::fcntl(fd, F_SETFL as i32, O_NONBLOCK);

        const NONE: Option<ClientConn> = None;
        Ok(Self {
            listen_fd: fd,
            clients: alloc::boxed::Box::new([NONE; MAX_CLIENTS]),
            client_count: 0,
            next_id: 1,
        })
    }

    /// Non-blocking accept. Returns client index if a new connection was accepted.
    pub fn accept(&mut self) -> Result<Option<usize>, ProtocolError> {
        let client_fd =
            match Sys::accept(self.listen_fd, core::ptr::null_mut(), core::ptr::null_mut()) {
                Ok(fd) => fd,
                Err(_) => return Ok(None), // EAGAIN
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
        self.clients[idx] = Some(ClientConn { conn, active: true });
        self.client_count += 1;
        Ok(Some(idx))
    }

    /// Read one request from a client (non-blocking).
    pub fn recv_request(&mut self, client: usize) -> Result<Option<Request>, ProtocolError> {
        match &mut self.clients[client] {
            Some(c) if c.active => c.conn.recv::<Request>(),
            _ => Ok(None),
        }
    }

    /// Send an event to a client (immediate flush, no write buffer).
    pub fn send_event(&mut self, client: usize, event: &Event) -> Result<(), ProtocolError> {
        match &self.clients[client] {
            Some(c) if c.active => c.conn.send(event),
            _ => Err(ProtocolError::Disconnected),
        }
    }

    /// Check if a client is still connected.
    pub fn is_connected(&self, client: usize) -> bool {
        matches!(&self.clients[client], Some(c) if c.active)
    }

    /// Disconnect and clean up a client slot.
    pub fn disconnect(&mut self, client: usize) {
        if let Some(c) = self.clients[client].take() {
            let _ = Sys::close(c.conn.fd());
            self.client_count -= 1;
        }
    }

    /// Allocate a monotonically increasing ID for surfaces/toplevels/etc.
    pub fn allocate_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Get the server's listening socket FD.
    pub fn listen_fd(&self) -> i32 {
        self.listen_fd
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
