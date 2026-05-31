//! Leaf futures over single ring ops.
//!
//! [`OpFuture`] is the one real `Future` in userland: on first poll it
//! submits its SQE and returns `Pending`; on a later poll it returns
//! `Ready` once the reactor has harvested its completion. Its `Drop` fires
//! `OP_CANCEL` for an op still in flight (the kernel cancel is a safe
//! table-row removal), and the data buffer stays owned by the reactor
//! until the completion lands — so a dropped future can never leave the
//! kernel writing freed memory.
//!
//! The public constructors return small concrete `Unpin` wrapper futures
//! ([`BufOp`] / [`IntOp`]) rather than `async fn`s, because the `select`
//! combinators poll their children through `Pin::new(&mut _)` and so
//! require `Unpin` children — which an `async fn` state machine is not.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use slopos_abi::net::SockAddrIn;
use slopos_abi::ring::{
    OP_ACCEPT, OP_CLOSE, OP_NOP, OP_OPENAT, OP_POLL_ADD, OP_READ, OP_RECVFROM, OP_RECVMSG,
    OP_TIMEOUT, OP_WRITE, Sqe,
};

use super::reactor::with_reactor;

/// Result stuffed into a future whose submit failed (ring broken). `-EIO`.
const RES_IO_ERR: i32 = -5;

/// A completed data-plane op: the kernel's `res` (>= 0 byte count, or a
/// negated errno) plus the buffer handed back for reuse. For `res > 0` the
/// first `res` bytes of `buf` are the transferred data; `buf` keeps its
/// original length (slice it as `&buf[..res as usize]`).
pub struct BufResult {
    pub res: i32,
    pub buf: Vec<u8>,
}

/// A completed `OP_RECVFROM`: the kernel's `res` (>= 0 datagram byte
/// count, or a negated errno), the data buffer handed back (its first
/// `res` bytes are the datagram), and the datagram's *source* address
/// (`SockAddrIn`, valid only when `res >= 0`). The source addr rides back
/// in the result struct — the ownership-passing analogue of `recvfrom(2)`
/// filling `src_addr`.
pub struct RecvFromResult {
    pub res: i32,
    pub buf: Vec<u8>,
    pub src: SockAddrIn,
}

enum State {
    /// Not yet submitted. `sqe` has opcode/fd/len/off/op_flags filled;
    /// `addr` is stamped from `buf` at submit time. If `addr2_off` is
    /// `Some(o)`, `addr2` is stamped to `buf_ptr + o` too — used by
    /// OP_RECVFROM, whose source-address out-struct rides in the same
    /// owned buffer (the last 16 bytes), so a single owned `Vec` keeps
    /// both the data region and the addr-out region alive in-flight.
    Start {
        sqe: Sqe,
        buf: Option<Vec<u8>>,
        addr2_off: Option<u32>,
    },
    /// Submitted; awaiting completion under this cookie.
    InFlight { cookie: u64 },
    /// Completion observed (terminal).
    Done,
}

/// The raw leaf future. Yields `(res, buf)`; the typed wrappers below map
/// that into [`BufResult`] or a bare `i32`.
struct OpFuture {
    state: State,
}

impl OpFuture {
    fn new(sqe: Sqe, buf: Option<Vec<u8>>) -> Self {
        Self {
            state: State::Start {
                sqe,
                buf,
                addr2_off: None,
            },
        }
    }

    /// Like [`OpFuture::new`] but stamps `addr2` to an offset within the
    /// same owned buffer at submit time (OP_RECVFROM source-addr out-ptr).
    fn new_with_addr2(sqe: Sqe, buf: Vec<u8>, addr2_off: u32) -> Self {
        Self {
            state: State::Start {
                sqe,
                buf: Some(buf),
                addr2_off: Some(addr2_off),
            },
        }
    }
}

impl Future for OpFuture {
    type Output = (i32, Option<Vec<u8>>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // OpFuture holds no self-references, so it is Unpin and we can take
        // a plain &mut.
        let this = self.get_mut();
        match &mut this.state {
            State::Start {
                sqe,
                buf,
                addr2_off,
            } => {
                let mut s = *sqe;
                if let Some(b) = buf.as_mut() {
                    // Stamp the buffer's (heap-stable) address now, at the
                    // last moment before ownership moves into the reactor.
                    let base = b.as_mut_ptr() as u64;
                    s.addr = base;
                    if let Some(off) = *addr2_off {
                        // The source-addr out-struct lives in the same owned
                        // buffer at `base + off`, kept alive by the same Vec.
                        s.addr2 = base + off as u64;
                    }
                }
                let taken = buf.take();
                match with_reactor(|r| r.submit(s, taken)) {
                    Ok(cookie) => {
                        // Register this task's waker so the reactor wakes us
                        // when the completion lands.
                        with_reactor(|r| r.register_waker(cookie, cx.waker().clone()));
                        this.state = State::InFlight { cookie };
                        Poll::Pending
                    }
                    Err(_) => {
                        this.state = State::Done;
                        Poll::Ready((RES_IO_ERR, None))
                    }
                }
            }
            State::InFlight { cookie } => {
                let cookie = *cookie;
                match with_reactor(|r| r.take_result(cookie)) {
                    Some((res, buf)) => {
                        this.state = State::Done;
                        Poll::Ready((res, buf))
                    }
                    None => {
                        // Refresh the waker (the awaiting task may have moved).
                        with_reactor(|r| r.register_waker(cookie, cx.waker().clone()));
                        Poll::Pending
                    }
                }
            }
            State::Done => panic!("slopfut: OpFuture polled after completion"),
        }
    }
}

impl Drop for OpFuture {
    fn drop(&mut self) {
        if let State::InFlight { cookie } = self.state {
            // Future dropped mid-flight: cancel and let the reactor keep
            // the buffer alive until the cancellation is harvested.
            with_reactor(|r| r.cancel(cookie));
        }
    }
}

fn op_sqe(opcode: u8, fd: i32, off: u64, len: u32, op_flags: u32) -> Sqe {
    let mut sqe = Sqe::ZERO;
    sqe.opcode = opcode;
    sqe.fd = fd;
    sqe.off = off;
    sqe.len = len;
    sqe.op_flags = op_flags;
    sqe
}

// --- typed wrapper futures (Unpin, usable in `select`) ---------------------

/// A data-plane op future resolving to [`BufResult`] (read/write/send).
pub struct BufOp(OpFuture);

impl Future for BufOp {
    type Output = BufResult;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<BufResult> {
        match Pin::new(&mut self.get_mut().0).poll(cx) {
            Poll::Ready((res, buf)) => Poll::Ready(BufResult {
                res,
                buf: buf.unwrap_or_default(),
            }),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A control op future resolving to the raw `res` (timeout/poll_add/nop).
pub struct IntOp(OpFuture);

impl Future for IntOp {
    type Output = i32;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<i32> {
        match Pin::new(&mut self.get_mut().0).poll(cx) {
            Poll::Ready((res, _)) => Poll::Ready(res),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Byte length of the `SockAddrIn` out-region appended to a recvfrom
/// buffer (the kernel writes the source address here on success).
const SOCKADDR_IN_LEN: usize = core::mem::size_of::<SockAddrIn>();

/// A datagram-recv future resolving to [`RecvFromResult`]. The owned
/// buffer carries the data region followed by a [`SockAddrIn`] out-slot
/// (the last [`SOCKADDR_IN_LEN`] bytes); on completion the source address
/// is decoded from that slot and the data region is handed back trimmed
/// to its original capacity.
pub struct RecvFromOp {
    inner: OpFuture,
    /// Length of the data region (the buffer is `data_len + SOCKADDR_IN_LEN`).
    data_len: usize,
}

impl Future for RecvFromOp {
    type Output = RecvFromResult;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<RecvFromResult> {
        let this = self.get_mut();
        let data_len = this.data_len;
        match Pin::new(&mut this.inner).poll(cx) {
            Poll::Ready((res, buf)) => {
                let mut buf = buf.unwrap_or_default();
                // Decode the source SockAddrIn from the appended out-slot,
                // then truncate the buffer back to the data region so the
                // caller sees only its data (the addr rides in `src`).
                let src = decode_sockaddr_in(&buf, data_len);
                if buf.len() >= data_len {
                    buf.truncate(data_len);
                }
                Poll::Ready(RecvFromResult { res, buf, src })
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Decode the `SockAddrIn` the kernel wrote into the out-slot at
/// `buf[data_len .. data_len + SOCKADDR_IN_LEN]`. Falls back to a zeroed
/// addr if the buffer is short (a failed/`-EAGAIN` op never wrote it).
fn decode_sockaddr_in(buf: &[u8], data_len: usize) -> SockAddrIn {
    let mut src = SockAddrIn::default();
    let end = data_len + SOCKADDR_IN_LEN;
    if buf.len() >= end {
        let s = &buf[data_len..end];
        src.family = u16::from_le_bytes([s[0], s[1]]);
        src.port = u16::from_le_bytes([s[2], s[3]]);
        src.addr = [s[4], s[5], s[6], s[7]];
    }
    src
}

// --- public constructors ---------------------------------------------------

/// `OP_NOP` — completes with `res == 0`. Useful as a fence / smoke test.
pub fn nop() -> IntOp {
    IntOp(OpFuture::new(op_sqe(OP_NOP, -1, 0, 0, 0), None))
}

/// `OP_READ` — read up to `len` bytes from `fd` into `buf` (capacity must
/// be at least `len`). Resolves to [`BufResult`]; `buf` returns owned.
pub fn read(fd: i32, buf: Vec<u8>, len: u32) -> BufOp {
    BufOp(OpFuture::new(op_sqe(OP_READ, fd, 0, len, 0), Some(buf)))
}

/// `OP_WRITE` — write all of `buf` to `fd`. Resolves to [`BufResult`]
/// (`res` is the byte count); `buf` returns owned for reuse.
pub fn write(fd: i32, buf: Vec<u8>) -> BufOp {
    let len = buf.len() as u32;
    BufOp(OpFuture::new(op_sqe(OP_WRITE, fd, 0, len, 0), Some(buf)))
}

/// `OP_TIMEOUT` — resolve after `ns` nanoseconds (`res == -ETIME`). Used to
/// bound an otherwise I/O-only wait.
pub fn timeout(ns: u64) -> IntOp {
    IntOp(OpFuture::new(op_sqe(OP_TIMEOUT, -1, ns, 0, 0), None))
}

/// `OP_POLL_ADD` — resolve when `fd` is ready for any bit in `mask`
/// (`POLLIN`/`POLLOUT`/…); `res` carries the ready `revents`.
pub fn poll_add(fd: i32, mask: u16) -> IntOp {
    IntOp(OpFuture::new(
        op_sqe(OP_POLL_ADD, fd, 0, 0, mask as u32),
        None,
    ))
}

/// `OP_ACCEPT` — accept a connection on listening socket `fd`. Resolves to
/// the new connection fd (`>= 0`) or a negated errno.
pub fn accept(fd: i32) -> IntOp {
    IntOp(OpFuture::new(op_sqe(OP_ACCEPT, fd, 0, 0, 0), None))
}

/// `OP_RECVFROM` — receive a datagram from `fd` (an AF_INET UDP socket),
/// returning the source address. Reads up to `len` bytes; `buf` capacity
/// must be at least `len`. Resolves to [`RecvFromResult`]: `res` is the
/// datagram byte count (or a negated errno), `buf` is handed back owned
/// (first `res` bytes are the data), and `src` is the sender's address.
///
/// Ownership model: `buf` is consumed and a [`SOCKADDR_IN_LEN`]-byte
/// out-slot is appended, so a single owned `Vec` keeps both the data
/// region and the addr-out region alive in-flight (the same UAF guard the
/// other ops use). The buffer is trimmed back to `len` before it returns.
pub fn recvfrom(fd: i32, mut buf: Vec<u8>, len: u32) -> RecvFromOp {
    let data_len = len as usize;
    // Grow the owned buffer so it has room for the appended out-slot. The
    // kernel writes the source SockAddrIn at `addr + len`.
    if buf.len() < data_len + SOCKADDR_IN_LEN {
        buf.resize(data_len + SOCKADDR_IN_LEN, 0);
    }
    let sqe = op_sqe(OP_RECVFROM, fd, 0, len, 0);
    RecvFromOp {
        inner: OpFuture::new_with_addr2(sqe, buf, len),
        data_len,
    }
}

/// `OP_OPENAT` — open the file at `path` with POSIX open `flags`. Opens
/// are immediate (no disk blocking), so this resolves inline to the new
/// fd (`>= 0`) or a negated errno. The path bytes are owned by the future
/// while in flight (the ownership-passing buffer model).
pub fn openat(path: &[u8], flags: u32) -> IntOp {
    let buf = path.to_vec();
    let len = buf.len() as u32;
    let sqe = op_sqe(OP_OPENAT, -1, 0, len, flags);
    IntOp(OpFuture::new(sqe, Some(buf)))
}

/// `OP_CLOSE` — close `fd` via the ring. Resolves inline to `0` or a
/// negated errno (`-EBADF`).
pub fn close(fd: i32) -> IntOp {
    IntOp(OpFuture::new(op_sqe(OP_CLOSE, fd, 0, 0, 0), None))
}

// --- multishot stream -------------------------------------------------------

/// A multishot op modelled as an async stream of `i32` results. The
/// kernel keeps one armed row in flight and posts a CQE on every yield
/// (each carrying `F_MORE`) until a terminal event; this surface yields
/// one `Some(res)` per interim CQE and finally `None` once the terminal
/// CQE (F_MORE clear) is observed.
///
/// One SQE replaces an N-resubmit loop: a server accept-loop becomes
/// `while let Some(fd) = accept_multishot(l).next().await { … }`. Dropping
/// the stream mid-flight fires `OP_CANCEL` (the kernel terminal CQE retires
/// the armed row), exactly like [`OpFuture`].
pub struct MultishotStream {
    state: MultishotState,
}

enum MultishotState {
    /// Not yet submitted; `sqe` carries opcode/fd/op_flags.
    Start { sqe: Sqe },
    /// Armed under this cookie; yielding interim results.
    Armed { cookie: u64 },
    /// Terminal CQE observed — the stream has ended.
    Done,
}

impl MultishotStream {
    fn new(sqe: Sqe) -> Self {
        Self {
            state: MultishotState::Start { sqe },
        }
    }

    /// Poll for the next stream item. `Poll::Ready(Some(res))` for an
    /// interim result, `Poll::Ready(None)` when the stream ends.
    pub fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<i32>> {
        let this = self.get_mut();
        match &mut this.state {
            MultishotState::Start { sqe } => {
                let s = *sqe;
                match with_reactor(|r| r.submit_multishot(s, None)) {
                    Ok(cookie) => {
                        with_reactor(|r| r.register_waker(cookie, cx.waker().clone()));
                        this.state = MultishotState::Armed { cookie };
                        Poll::Pending
                    }
                    Err(_) => {
                        this.state = MultishotState::Done;
                        Poll::Ready(None)
                    }
                }
            }
            MultishotState::Armed { cookie } => {
                let cookie = *cookie;
                match with_reactor(|r| r.take_next(cookie)) {
                    Some((res, _flags, terminal)) => {
                        if terminal {
                            // The terminal CQE itself is the stream-end
                            // marker (error / EOF / cancel), not a data
                            // item — so it ends the stream rather than
                            // yielding `res`.
                            this.state = MultishotState::Done;
                            Poll::Ready(None)
                        } else {
                            // Refresh the waker for the next yield.
                            with_reactor(|r| r.register_waker(cookie, cx.waker().clone()));
                            Poll::Ready(Some(res))
                        }
                    }
                    None => {
                        with_reactor(|r| r.register_waker(cookie, cx.waker().clone()));
                        Poll::Pending
                    }
                }
            }
            MultishotState::Done => Poll::Ready(None),
        }
    }

    /// Await the next item (`None` at stream end). Convenience over
    /// [`MultishotStream::poll_next`] for `while let Some(x) = s.next().await`.
    pub fn next(&mut self) -> Next<'_> {
        Next { stream: self }
    }
}

impl Drop for MultishotStream {
    fn drop(&mut self) {
        if let MultishotState::Armed { cookie } = self.state {
            // Stream dropped while armed: cancel the kernel row. The
            // terminal -ECANCELED CQE retires it (SLOPRING §1.3 trigger 4).
            with_reactor(|r| r.cancel(cookie));
        }
    }
}

/// Future returned by [`MultishotStream::next`].
pub struct Next<'a> {
    stream: &'a mut MultishotStream,
}

impl Future for Next<'_> {
    type Output = Option<i32>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<i32>> {
        let this = self.get_mut();
        Pin::new(&mut *this.stream).poll_next(cx)
    }
}

/// `OP_ACCEPT` armed as multishot — yields each accepted connection fd as
/// a stream item, then `None` when the listener errors / is cancelled.
/// The server-loop primitive: one SQE, a stream of inbound connections.
pub fn accept_multishot(fd: i32) -> MultishotStream {
    MultishotStream::new(op_sqe(OP_ACCEPT, fd, 0, 0, 0))
}

/// `OP_RECVMSG` armed as multishot — yields each received datagram/chunk
/// byte count, terminating on EOF (`res == 0`) or error.
pub fn recvmsg_multishot(fd: i32, msghdr_addr: u64) -> MultishotStream {
    let mut sqe = op_sqe(OP_RECVMSG, fd, 0, 0, 0);
    sqe.addr = msghdr_addr;
    MultishotStream::new(sqe)
}

/// `OP_POLL_ADD` armed as multishot — yields the ready `revents` each time
/// `fd`'s readiness *transitions* into `mask` (edge-tracked, no level
/// flood), terminating on `POLLERR`/`POLLHUP`.
pub fn poll_add_multishot(fd: i32, mask: u16) -> MultishotStream {
    MultishotStream::new(op_sqe(OP_POLL_ADD, fd, 0, 0, mask as u32))
}
