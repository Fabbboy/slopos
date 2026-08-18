//! Leaf futures over single ring ops.
//!
//! [`OpFuture`] is the one real `Future` in userland: on first poll it submits
//! its SQE and returns `Pending`, and its `Drop` fires `OP_CANCEL` for an op
//! still in flight.
//!
//! The public constructors return concrete `Unpin` wrapper futures ([`BufOp`] /
//! [`IntOp`]) rather than `async fn`s, because the `select` combinators poll
//! their children through `Pin::new(&mut _)` and so require `Unpin` children —
//! which an `async fn` state machine is not.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use slopos_abi::net::SockAddrIn;
use slopos_abi::ring::{
    OP_ACCEPT, OP_CLOSE, OP_CONNECT, OP_NOP, OP_OPENAT, OP_POLL_ADD, OP_READ, OP_RECVFROM,
    OP_RECVMSG, OP_TIMEOUT, OP_WRITE, Sqe,
};

use super::reactor::with_reactor;

/// Result stuffed into a future whose submit failed (ring broken). `-EIO`.
const RES_IO_ERR: i32 = -5;

/// A completed data-plane op: the kernel's `res` (>= 0 byte count, or a
/// negated errno) plus the buffer handed back for reuse. `buf` keeps its
/// original length — slice it as `&buf[..res as usize]`.
pub struct BufResult {
    pub res: i32,
    pub buf: Vec<u8>,
}

/// A completed `OP_RECVFROM`: the kernel's `res` (>= 0 datagram byte
/// count, or a negated errno), the data buffer handed back, and the
/// datagram's *source* address (valid only when `res >= 0`).
pub struct RecvFromResult {
    pub res: i32,
    pub buf: Vec<u8>,
    pub src: SockAddrIn,
}

enum State {
    /// Not yet submitted. `addr` is stamped from `buf` at submit time, and
    /// `addr2_off` — OP_RECVFROM's source-address out-struct — is stamped to
    /// `buf_ptr + off`, so one owned `Vec` keeps both regions alive in flight.
    Start {
        sqe: Sqe,
        buf: Option<Vec<u8>>,
        addr2_off: Option<u32>,
    },
    InFlight {
        cookie: u64,
    },
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
        // OpFuture holds no self-references, so it is Unpin.
        let this = self.get_mut();
        match &mut this.state {
            State::Start {
                sqe,
                buf,
                addr2_off,
            } => {
                let mut s = *sqe;
                if let Some(b) = buf.as_mut() {
                    // Stamp the buffer's (heap-stable) address at the last
                    // moment before ownership moves into the reactor.
                    let base = b.as_mut_ptr() as u64;
                    s.addr = base;
                    if let Some(off) = *addr2_off {
                        s.addr2 = base + off as u64;
                    }
                }
                let taken = buf.take();
                match with_reactor(|r| r.submit(s, taken)) {
                    Ok(cookie) => {
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
                        // Refresh the waker: the awaiting task may have moved.
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

/// A datagram-recv future resolving to [`RecvFromResult`]. The owned buffer
/// carries the data region followed by a [`SockAddrIn`] out-slot (the last
/// [`SOCKADDR_IN_LEN`] bytes); on completion the source address is decoded from
/// that slot and the data region is handed back trimmed.
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
/// `buf[data_len .. data_len + SOCKADDR_IN_LEN]`. Falls back to a zeroed addr
/// if the buffer is short — a failed op never wrote one.
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

/// Encode a [`SockAddrIn`] into its 16-byte `#[repr(C)]` memory image, which
/// the kernel reads back as a raw struct via `copy_from_user`. `family`/`port`
/// keep their stored values — the kernel applies `from_be` to the
/// network-order port itself.
fn encode_sockaddr_in(a: &SockAddrIn) -> Vec<u8> {
    let mut v = vec![0u8; SOCKADDR_IN_LEN];
    v[0..2].copy_from_slice(&a.family.to_le_bytes());
    v[2..4].copy_from_slice(&a.port.to_le_bytes());
    v[4..8].copy_from_slice(&a.addr);
    // bytes [8..16] are the struct pad — left zero.
    v
}

/// `OP_CONNECT` — connect socket `fd` to `addr` (AF_INET). Resolves to `0` on
/// success or a negated errno (`-ECONNREFUSED`, `-ETIMEDOUT`, …). The
/// `SockAddrIn` is copied into an owned buffer carried by the future, so the
/// caller's `&SockAddrIn` need not outlive the await.
pub fn connect(fd: i32, addr: &SockAddrIn) -> IntOp {
    let buf = encode_sockaddr_in(addr);
    let sqe = op_sqe(OP_CONNECT, fd, 0, SOCKADDR_IN_LEN as u32, 0);
    IntOp(OpFuture::new(sqe, Some(buf)))
}

/// `OP_RECVFROM` — receive a datagram from `fd` (an AF_INET UDP socket),
/// returning the source address. Reads up to `len` bytes; `buf` capacity
/// must be at least `len`. Resolves to [`RecvFromResult`]: `res` is the
/// datagram byte count (or a negated errno), `buf` is handed back owned and
/// trimmed to `len`, and `src` is the sender's address.
pub fn recvfrom(fd: i32, mut buf: Vec<u8>, len: u32) -> RecvFromOp {
    let data_len = len as usize;
    // The kernel writes the source SockAddrIn at `addr + len`.
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
/// fd (`>= 0`) or a negated errno.
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

/// A multishot op modelled as an async stream of `i32` results. The kernel
/// keeps one armed row in flight and posts a CQE on every yield (each carrying
/// `F_MORE`) until a terminal event; this surface yields one `Some(res)` per
/// interim CQE and finally `None` once the terminal CQE (F_MORE clear) is
/// observed. Dropping the stream mid-flight fires `OP_CANCEL`.
pub struct MultishotStream {
    state: MultishotState,
}

enum MultishotState {
    Start { sqe: Sqe },
    Armed { cookie: u64 },
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
                            // The terminal CQE is the stream-end marker, not a
                            // data item, so its `res` is not yielded.
                            this.state = MultishotState::Done;
                            Poll::Ready(None)
                        } else {
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

    /// Await the next item (`None` at stream end).
    pub fn next(&mut self) -> Next<'_> {
        Next { stream: self }
    }
}

impl Drop for MultishotStream {
    fn drop(&mut self) {
        if let MultishotState::Armed { cookie } = self.state {
            // The terminal -ECANCELED CQE retires the armed kernel row
            // (SLOPRING §1.3 trigger 4).
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
