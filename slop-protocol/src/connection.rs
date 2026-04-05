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
//!
//! fd passing (SCM_RIGHTS) uses recvmsg for ALL socket reads. Received
//! fds are queued in a pending FIFO and consumed inline by the Decode
//! trait implementation for message types that carry fds (e.g.
//! SurfaceAttach). This matches libwayland's design and avoids the
//! message-framing race where multiple protocol messages arrive in a
//! single socket read.

use crate::codec::{Decode, Encode, FdFifo};
use crate::types::ProtocolError;
use slopos_abi::syscall::posix::{F_SETFL, O_NONBLOCK, POLLERR, POLLHUP, POLLIN, POLLOUT};
use slopos_abi::syscall::types::UserPollFd;
use slopos_abi::syscall::{CmsgHdr, MsgHdr, SCM_RIGHTS};
use slopos_slibc::errno;
use slopos_slibc::pal::{Pal, Sys};

const READ_BUF_SIZE: usize = 16384;
const MAX_MSG_SIZE: usize = 8192;

/// Maximum queued fds from recvmsg ancillary data.
pub(crate) const MAX_PENDING_FDS: usize = 8;

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
    /// FIFO of fds received via SCM_RIGHTS but not yet consumed by the codec.
    pending_fds: [i32; MAX_PENDING_FDS],
    pending_fd_count: u8,
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
            pending_fds: [-1; MAX_PENDING_FDS],
            pending_fd_count: 0,
        }
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// Enqueue a raw fd into the pending FIFO. If the FIFO is full, the fd
    /// is closed immediately to prevent leaking. This is the ONLY place
    /// that adds fds to the FIFO — all receive paths go through here.
    fn enqueue_fd(&mut self, fd: i32) {
        if (self.pending_fd_count as usize) < MAX_PENDING_FDS {
            self.pending_fds[self.pending_fd_count as usize] = fd;
            self.pending_fd_count += 1;
        } else if fd >= 0 {
            let _ = Sys::close(fd);
        }
    }

    // ── Send ────────────────────────────────────────────────────────────

    /// Send a message immediately to the socket. No write buffer.
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

    /// Send a message with an attached file descriptor via SCM_RIGHTS.
    pub fn send_with_fd<T: Encode>(&self, msg: &T, fd: i32) -> Result<(), ProtocolError> {
        let mut buf = [0u8; MAX_MSG_SIZE];
        let payload_len = msg.encode(&mut buf[4..])?;
        if payload_len > MAX_MSG_SIZE - 4 {
            return Err(ProtocolError::MessageTooLarge);
        }
        let len_bytes = (payload_len as u32).to_le_bytes();
        buf[0..4].copy_from_slice(&len_bytes);
        let total = 4 + payload_len;

        // Build ancillary data: CmsgHdr + one i32 fd
        let hdr_size = core::mem::size_of::<CmsgHdr>();
        let mut cmsg_buf = [0u8; 32];
        let cmsg = CmsgHdr {
            cmsg_len: (hdr_size + 4) as u32,
            cmsg_level: slopos_abi::syscall::posix::SOL_SOCKET as u32,
            cmsg_type: SCM_RIGHTS,
        };
        let cmsg_bytes =
            unsafe { core::slice::from_raw_parts(&cmsg as *const _ as *const u8, hdr_size) };
        cmsg_buf[..hdr_size].copy_from_slice(cmsg_bytes);
        let fd_bytes = fd.to_le_bytes();
        cmsg_buf[hdr_size..hdr_size + 4].copy_from_slice(&fd_bytes);

        let msg_hdr = MsgHdr {
            iov_base: buf.as_ptr() as u64,
            iov_len: total as u64,
            control: cmsg_buf.as_ptr() as u64,
            control_len: (hdr_size + 4) as u64,
        };

        match Sys::sendmsg(self.fd, &msg_hdr, 0) {
            Ok(_) => Ok(()),
            Err(e) if e == errno::EAGAIN || e == errno::EWOULDBLOCK => {
                self.poll_writable(2000)?;
                Sys::sendmsg(self.fd, &msg_hdr, 0).map_err(|_| ProtocolError::Io)?;
                Ok(())
            }
            Err(_) => Err(ProtocolError::Io),
        }
    }

    // ── Receive ─────────────────────────────────────────────────────────

    /// Try to receive one complete message (non-blocking).
    /// Returns `Ok(None)` if no complete message is available.
    /// File descriptors received via SCM_RIGHTS are consumed inline by the
    /// decoder for message types that carry fds (e.g. `SurfaceAttach`).
    pub fn recv<T: Decode>(&mut self) -> Result<Option<T>, ProtocolError> {
        // First check if we already have a complete frame in the buffer.
        if let Some(msg) = self.try_decode::<T>()? {
            return Ok(Some(msg));
        }
        // Try to read more data from the socket (captures any fds too).
        self.try_fill_buf()?;
        // Check again after filling.
        self.try_decode::<T>()
    }

    /// Block via poll() until a complete message arrives or timeout expires.
    pub fn wait_recv<T: Decode>(&mut self, timeout_ms: i32) -> Result<T, ProtocolError> {
        // Check buffer first — message might already be there.
        if let Some(msg) = self.recv::<T>()? {
            return Ok(msg);
        }

        let start = crate::timestamp_ms();
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

    // ── Internal ────────────────────────────────────────────────────────

    /// Block via poll() until the socket has data to read.
    /// Retries automatically if interrupted by a signal (EINTR),
    /// correctly maintaining the real-time deadline.
    fn poll_readable(&self, timeout_ms: i32) -> Result<(), ProtocolError> {
        let mut pfd = UserPollFd {
            fd: self.fd,
            events: POLLIN,
            revents: 0,
        };
        loop {
            match Sys::poll(&mut pfd as *mut _ as *mut u8, 1, timeout_ms) {
                Ok(rc) if rc < 0 => return Err(ProtocolError::Io),
                Err(_) => {
                    let e = errno::errno_get();
                    if e == errno::EINTR.raw() {
                        continue;
                    }
                    return Err(ProtocolError::Io);
                }
                _ => {}
            }
            if pfd.revents & (POLLERR | POLLHUP) != 0 {
                return Err(ProtocolError::Disconnected);
            }
            return Ok(());
        }
    }

    /// Block via poll() until the socket is writable.
    fn poll_writable(&self, timeout_ms: i32) -> Result<(), ProtocolError> {
        let mut pfd = UserPollFd {
            fd: self.fd,
            events: POLLOUT,
            revents: 0,
        };
        match Sys::poll(&mut pfd as *mut _ as *mut u8, 1, timeout_ms) {
            Ok(rc) if rc < 0 => return Err(ProtocolError::Io),
            Err(_) => return Err(ProtocolError::Io),
            _ => {}
        }
        if pfd.revents & (POLLERR | POLLHUP) != 0 {
            return Err(ProtocolError::Io);
        }
        Ok(())
    }

    /// Decode one length-prefixed frame from the read buffer.
    ///
    /// Constructs an [`FdFifo`] from the pending fd state and passes it to
    /// `T::decode`, so message types that carry fds (e.g. `SurfaceAttach`)
    /// can pop them inline during decoding.
    fn try_decode<T: Decode>(&mut self) -> Result<Option<T>, ProtocolError> {
        let available = self.read_len - self.read_pos;
        if available < 4 {
            return Ok(None);
        }

        let len_bytes = &self.read_buf[self.read_pos..self.read_pos + 4];
        let payload_len =
            u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;

        if payload_len > MAX_MSG_SIZE {
            return Err(ProtocolError::MalformedMessage);
        }
        if available < 4 + payload_len {
            return Ok(None); // Incomplete frame — need more data.
        }

        let payload_start = self.read_pos + 4;
        let payload = &self.read_buf[payload_start..payload_start + payload_len];
        let mut fifo = FdFifo::new(&mut self.pending_fds, &mut self.pending_fd_count);
        let (msg, _consumed) = T::decode(payload, &mut fifo)?;
        self.read_pos += 4 + payload_len;
        Ok(Some(msg))
    }

    /// Fill the read buffer using recvmsg (captures SCM_RIGHTS fds too).
    /// This replaces the old try_fill_buf that used plain recv().
    fn try_fill_buf(&mut self) -> Result<(), ProtocolError> {
        if self.read_len >= READ_BUF_SIZE {
            self.compact_buf();
            if self.read_len >= READ_BUF_SIZE {
                return Err(ProtocolError::BufferFull);
            }
        }

        let ptr = unsafe { self.read_buf.as_mut_ptr().add(self.read_len) };
        let avail = READ_BUF_SIZE - self.read_len;

        let hdr_size = core::mem::size_of::<CmsgHdr>();
        let mut cmsg_buf = [0u8; 32];

        let mut msg_hdr = MsgHdr {
            iov_base: ptr as u64,
            iov_len: avail as u64,
            control: cmsg_buf.as_mut_ptr() as u64,
            control_len: cmsg_buf.len() as u64,
        };

        match Sys::recvmsg(self.fd, &mut msg_hdr, 0) {
            Ok(n) if n > 0 => {
                self.read_len += n;
            }
            Ok(0) => return Err(ProtocolError::Disconnected),
            Err(e) if e == errno::EAGAIN || e == errno::EWOULDBLOCK => return Ok(()),
            Err(_) => return Err(ProtocolError::Io),
            Ok(_) => return Ok(()),
        }

        // Queue any fds received via SCM_RIGHTS into the pending FIFO.
        if msg_hdr.control_len as usize >= hdr_size + 4 {
            let cmsg: CmsgHdr = unsafe { core::ptr::read(cmsg_buf.as_ptr() as *const CmsgHdr) };
            if cmsg.cmsg_type == SCM_RIGHTS && cmsg.cmsg_len as usize >= hdr_size + 4 {
                let fd_data_len = cmsg.cmsg_len as usize - hdr_size;
                let n_fds = fd_data_len / 4;
                for i in 0..n_fds {
                    let off = hdr_size + i * 4;
                    let mut fb = [0u8; 4];
                    fb.copy_from_slice(&cmsg_buf[off..off + 4]);
                    let fd = i32::from_le_bytes(fb);
                    self.enqueue_fd(fd);
                }
            }
        }

        Ok(())
    }

    /// Compact read buffer: move unconsumed data to the front.
    fn compact_buf(&mut self) {
        if self.read_pos == 0 {
            return;
        }
        let remaining = self.read_len - self.read_pos;
        self.read_buf.copy_within(self.read_pos..self.read_len, 0);
        self.read_pos = 0;
        self.read_len = remaining;
    }

    /// Check if the peer has disconnected by attempting a non-blocking read.
    pub fn probe_disconnected(&mut self) -> bool {
        matches!(self.try_fill_buf(), Err(ProtocolError::Disconnected))
    }

    /// Consume the connection and return the raw FD without closing it.
    pub fn into_raw_fd(mut self) -> i32 {
        let fd = self.fd;
        self.fd = -1;
        fd
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if self.fd >= 0 {
            let _ = Sys::close(self.fd);
        }
        // Close any unclaimed pending fds to prevent leaks.
        for i in 0..self.pending_fd_count as usize {
            if self.pending_fds[i] >= 0 {
                let _ = Sys::close(self.pending_fds[i]);
            }
        }
    }
}
