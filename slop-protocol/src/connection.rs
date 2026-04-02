//! Socket connection with length-prefixed framing.
//!
//! Design principles (Wayland-inspired):
//! - Socket is ALWAYS non-blocking (`O_NONBLOCK` set once at creation).
//! - All blocking is done via `poll(fd, POLLIN, timeout)`, never via
//!   blocking `recv()` or sleep-spin loops.
//! - `send()` writes directly to the socket — no write buffer, no
//!   forgotten-flush bugs.
//! - `recv()` is non-blocking (returns `Ok(None)` if no data).
//! - `wait_recv()` uses `poll()` for efficient blocking with timeout.

use crate::codec::{Decode, Encode};
use crate::types::ProtocolError;
use slopos_abi::syscall::posix::{F_SETFL, O_NONBLOCK, POLLERR, POLLHUP, POLLIN, POLLOUT};
use slopos_abi::syscall::types::UserPollFd;
use slopos_slibc::errno;
use slopos_slibc::pal::{Pal, Sys};

const READ_BUF_SIZE: usize = 16384;
const MAX_MSG_SIZE: usize = 8192;

/// Set O_NONBLOCK on a socket FD, preserving any existing flags.
pub fn set_nonblock(fd: i32) {
    use slopos_abi::syscall::posix::F_GETFL;
    let flags = Sys::fcntl(fd, F_GETFL as i32, 0).unwrap_or(0) as u64;
    let _ = Sys::fcntl(fd, F_SETFL as i32, flags | O_NONBLOCK);
}

pub struct Connection {
    fd: i32,
    read_buf: alloc::boxed::Box<[u8; READ_BUF_SIZE]>,
    read_len: usize,
    read_pos: usize,
}

impl Connection {
    /// Create a connection from an already-connected socket FD.
    /// Sets the socket to non-blocking mode immediately.
    pub fn new(fd: i32) -> Self {
        // Set non-blocking ONCE. Never changed again.
        set_nonblock(fd);
        Self {
            fd,
            read_buf: alloc::boxed::Box::new([0u8; READ_BUF_SIZE]),
            read_len: 0,
            read_pos: 0,
        }
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// Send a message immediately to the socket. No write buffer.
    /// Handles EAGAIN on non-blocking sockets by waiting with poll(POLLOUT).
    pub fn send<T: Encode>(&self, msg: &T) -> Result<(), ProtocolError> {
        let mut buf = [0u8; MAX_MSG_SIZE];
        let payload_len = msg.encode(&mut buf[4..])?;
        if payload_len > MAX_MSG_SIZE - 4 {
            return Err(ProtocolError::MessageTooLarge);
        }
        let len_bytes = (payload_len as u32).to_le_bytes();
        buf[0..4].copy_from_slice(&len_bytes);
        let total = 4 + payload_len;

        let mut sent = 0usize;
        while sent < total {
            let ptr = unsafe { buf.as_ptr().add(sent) };
            let remaining = total - sent;
            match Sys::send(self.fd, ptr, remaining, 0) {
                Ok(n) if n > 0 => sent += n,
                Ok(0) => return Err(ProtocolError::Disconnected),
                Err(e) if e == errno::EAGAIN || e == errno::EWOULDBLOCK => {
                    self.poll_writable(2000)?;
                }
                Err(_) => return Err(ProtocolError::Io),
                Ok(_) => unreachable!(),
            }
        }
        Ok(())
    }

    /// Try to receive one complete message (non-blocking).
    /// Returns `Ok(None)` if no complete message is available.
    pub fn recv<T: Decode>(&mut self) -> Result<Option<T>, ProtocolError> {
        // First check if we already have a complete frame in the buffer.
        if let Some(msg) = self.try_decode::<T>()? {
            return Ok(Some(msg));
        }
        // Try to read more data from the socket (non-blocking).
        self.try_fill_buf()?;
        // Check again after filling.
        self.try_decode::<T>()
    }

    /// Block via poll() until a complete message arrives or timeout expires.
    /// Loops to handle partial reads (poll wakes but only part of the
    /// frame arrived). Uses a real-time deadline to avoid unbounded waits.
    pub fn wait_recv<T: Decode>(&mut self, timeout_ms: i32) -> Result<T, ProtocolError> {
        // Check buffer first — message might already be there.
        if let Some(msg) = self.recv::<T>()? {
            return Ok(msg);
        }

        let start = crate::timestamp_ms();
        // Treat negative timeout as "no timeout" by saturating to u64::MAX.
        let deadline = if timeout_ms < 0 {
            u64::MAX
        } else {
            start.saturating_add(timeout_ms as u64)
        };
        loop {
            let now = crate::timestamp_ms();
            if now >= deadline {
                return Err(ProtocolError::Timeout);
            }
            let remaining = (deadline - now) as i32;
            self.poll_readable(remaining)?;
            self.try_fill_buf()?;
            if let Some(msg) = self.try_decode::<T>()? {
                return Ok(msg);
            }
        }
    }

    /// Block via poll() until the socket has data to read.
    /// Retries automatically if interrupted by a signal (EINTR),
    /// using a real-time deadline to avoid unbounded waits.
    fn poll_readable(&self, timeout_ms: i32) -> Result<(), ProtocolError> {
        const EINTR: i32 = -4;
        let deadline = if timeout_ms < 0 {
            u64::MAX
        } else {
            crate::timestamp_ms().saturating_add(timeout_ms as u64)
        };
        loop {
            let now = crate::timestamp_ms();
            if now >= deadline {
                return Err(ProtocolError::Timeout);
            }
            let remaining = (deadline - now) as i32;
            let mut pfd = UserPollFd {
                fd: self.fd,
                events: POLLIN,
                revents: 0,
            };
            let result = crate::raw_poll(&mut pfd, remaining as i64);
            if result == EINTR {
                continue;
            }
            if result <= 0 {
                return Err(ProtocolError::Timeout);
            }
            if pfd.revents & POLLERR != 0 {
                return Err(ProtocolError::Io);
            }
            if pfd.revents & POLLHUP != 0 {
                return Err(ProtocolError::Disconnected);
            }
            if pfd.revents & POLLIN != 0 {
                return Ok(());
            }
            // Spurious wakeup — retry.
        }
    }

    /// Block via poll() until the socket is ready for writing.
    /// Retries automatically if interrupted by a signal (EINTR),
    /// using a real-time deadline to avoid unbounded waits.
    fn poll_writable(&self, timeout_ms: i32) -> Result<(), ProtocolError> {
        const EINTR: i32 = -4;
        let deadline = if timeout_ms < 0 {
            u64::MAX
        } else {
            crate::timestamp_ms().saturating_add(timeout_ms as u64)
        };
        loop {
            let now = crate::timestamp_ms();
            if now >= deadline {
                return Err(ProtocolError::Timeout);
            }
            let remaining = (deadline - now) as i32;
            let mut pfd = UserPollFd {
                fd: self.fd,
                events: POLLOUT,
                revents: 0,
            };
            let result = crate::raw_poll(&mut pfd, remaining as i64);
            if result == EINTR {
                continue;
            }
            if result <= 0 {
                return Err(ProtocolError::Timeout);
            }
            if pfd.revents & POLLERR != 0 {
                return Err(ProtocolError::Io);
            }
            if pfd.revents & POLLHUP != 0 {
                return Err(ProtocolError::Disconnected);
            }
            if pfd.revents & POLLOUT != 0 {
                return Ok(());
            }
        }
    }

    /// Try to decode one frame from the read buffer without any I/O.
    fn try_decode<T: Decode>(&mut self) -> Result<Option<T>, ProtocolError> {
        let available = self.read_len - self.read_pos;
        if available < 4 {
            return Ok(None);
        }

        let p = self.read_pos;
        let payload_len = u32::from_le_bytes([
            self.read_buf[p],
            self.read_buf[p + 1],
            self.read_buf[p + 2],
            self.read_buf[p + 3],
        ]) as usize;

        if payload_len > MAX_MSG_SIZE {
            return Err(ProtocolError::MalformedMessage);
        }
        if available < 4 + payload_len {
            return Ok(None); // incomplete frame, need more data
        }

        let payload = &self.read_buf[p + 4..p + 4 + payload_len];
        let (msg, _) = T::decode(payload)?;
        self.read_pos += 4 + payload_len;

        if self.read_pos > READ_BUF_SIZE / 2 {
            self.compact_buf();
        }

        Ok(Some(msg))
    }

    /// Probe whether the peer has disconnected without consuming any
    /// framed messages.
    ///
    /// Does a non-blocking `recv()` into the read buffer.  If the peer
    /// has closed, returns `true`.  Any data received is buffered for
    /// the next `recv()` / `try_decode()` call — nothing is lost.
    pub fn probe_disconnected(&mut self) -> bool {
        matches!(self.try_fill_buf(), Err(ProtocolError::Disconnected))
    }

    /// Non-blocking read from socket into buffer.
    fn try_fill_buf(&mut self) -> Result<(), ProtocolError> {
        if self.read_len >= READ_BUF_SIZE {
            self.compact_buf();
            if self.read_len >= READ_BUF_SIZE {
                return Err(ProtocolError::BufferFull);
            }
        }
        let ptr = unsafe { self.read_buf.as_mut_ptr().add(self.read_len) };
        let avail = READ_BUF_SIZE - self.read_len;
        match Sys::recv(self.fd, ptr, avail, 0) {
            Ok(n) if n > 0 => {
                self.read_len += n;
                Ok(())
            }
            Ok(0) => Err(ProtocolError::Disconnected),
            Err(e) if e == errno::EAGAIN || e == errno::EWOULDBLOCK => Ok(()),
            Err(_) => Err(ProtocolError::Io),
            Ok(_) => unreachable!(),
        }
    }

    /// Consume the connection and return the raw FD without closing it.
    ///
    /// The caller takes ownership of the FD and is responsible for closing it.
    /// The heap-allocated read buffer is properly freed.
    pub fn into_raw_fd(mut self) -> i32 {
        let fd = self.fd;
        // Prevent Drop from closing the FD by setting it to an invalid value.
        self.fd = -1;
        // `self` drops here normally, which:
        // - Frees `read_buf` (Box drop)
        // - Calls Drop::drop which calls close(-1) — harmless no-op
        fd
    }

    fn compact_buf(&mut self) {
        if self.read_pos == 0 {
            return;
        }
        let remaining = self.read_len - self.read_pos;
        if remaining > 0 {
            self.read_buf.copy_within(self.read_pos..self.read_len, 0);
        }
        self.read_len = remaining;
        self.read_pos = 0;
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = Sys::close(self.fd);
    }
}
