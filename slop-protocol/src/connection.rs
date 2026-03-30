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
use slopos_abi::syscall::posix::{F_SETFL, O_NONBLOCK, POLLIN, POLLOUT};
use slopos_abi::syscall::types::UserPollFd;
use slopos_slibc::pal::{Pal, Sys};

const READ_BUF_SIZE: usize = 16384;
const MAX_MSG_SIZE: usize = 8192;

pub struct Connection {
    fd: i32,
    read_buf: [u8; READ_BUF_SIZE],
    read_len: usize,
    read_pos: usize,
}

impl Connection {
    /// Create a connection from an already-connected socket FD.
    /// Sets the socket to non-blocking mode immediately.
    pub fn new(fd: i32) -> Self {
        // Set non-blocking ONCE. Never changed again.
        let _ = Sys::fcntl(fd, F_SETFL as i32, O_NONBLOCK);
        Self {
            fd,
            read_buf: [0u8; READ_BUF_SIZE],
            read_len: 0,
            read_pos: 0,
        }
    }

    /// Create a connection that stays in blocking mode (for the initial
    /// connect + handshake phase before the caller converts it).
    pub fn new_blocking(fd: i32) -> Self {
        Self {
            fd,
            read_buf: [0u8; READ_BUF_SIZE],
            read_len: 0,
            read_pos: 0,
        }
    }

    /// Switch this connection to non-blocking mode.
    pub fn set_nonblocking(&self) {
        let _ = Sys::fcntl(self.fd, F_SETFL as i32, O_NONBLOCK);
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
                _ => {
                    // EAGAIN — socket buffer full. Wait via poll(POLLOUT)
                    // then retry. This is the standard non-blocking send pattern.
                    self.poll_writable(2000)?;
                }
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
    /// frame arrived). The timeout is a total deadline, not per-iteration.
    pub fn wait_recv<T: Decode>(&mut self, timeout_ms: i32) -> Result<T, ProtocolError> {
        // Check buffer first — message might already be there.
        if let Some(msg) = self.recv::<T>()? {
            return Ok(msg);
        }

        let mut remaining = timeout_ms;
        loop {
            self.poll_readable(remaining)?;
            self.try_fill_buf()?;
            if let Some(msg) = self.try_decode::<T>()? {
                return Ok(msg);
            }
            // Partial read — reduce remaining time and retry.
            let step = 50i32.min(remaining);
            remaining -= step;
            if remaining <= 0 {
                return Err(ProtocolError::Timeout);
            }
        }
    }

    /// Block via poll() until the socket has data to read.
    fn poll_readable(&self, timeout_ms: i32) -> Result<(), ProtocolError> {
        let mut pfd = UserPollFd {
            fd: self.fd,
            events: POLLIN,
            revents: 0,
        };
        let result = crate::raw_poll(&mut pfd, 1, timeout_ms as i64);
        if result <= 0 {
            return Err(ProtocolError::Timeout);
        }
        if pfd.revents & POLLIN == 0 {
            return Err(ProtocolError::Timeout);
        }
        Ok(())
    }

    /// Block via poll() until the socket is ready for writing.
    fn poll_writable(&self, timeout_ms: i32) -> Result<(), ProtocolError> {
        let mut pfd = UserPollFd {
            fd: self.fd,
            events: POLLOUT,
            revents: 0,
        };
        let result = crate::raw_poll(&mut pfd, 1, timeout_ms as i64);
        if result <= 0 {
            return Err(ProtocolError::Timeout);
        }
        if pfd.revents & POLLOUT == 0 {
            return Err(ProtocolError::Timeout);
        }
        Ok(())
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
            _ => Ok(()), // EAGAIN — no data available, that's fine
        }
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
