//! Ring-driven I/O session for nc's TCP recv/send loop.
//!
//! This is the *real-app* port of the SlopRing async edge: nc's
//! established-connection loop — historically a `poll(2)` over stdin +
//! socket with blocking `recv`/`send` syscalls — is rewritten here as a
//! submission/completion loop over a [`Ring`]. Every data-plane transfer
//! (`OP_READ` on stdin, `OP_READ` on the socket, `OP_WRITE` to the
//! socket) is submitted as an SQE and harvested via a **blocking
//! `ring_enter`** (`Ring::submit_and_wait`), so the deferred-completion
//! path (SLOPRING § 7.1) is exercised end-to-end by a genuine program.
//!
//! `connect`/`bind`/`listen`/`shutdown` stay regular syscalls — they are
//! outside the nine-opcode data-plane set (SLOPRING § 12), exactly as the
//! plan calls for. The multiplexing that `poll` used to do is now the
//! kernel's caller-as-waiter harvest: the ring registers the calling task
//! on *both* fds' wait queues and wakes on whichever is first ready, the
//! same readiness substrate `poll` rode on.
//!
//! Buffer-lifetime discipline (no UAF): every in-flight `OP_READ`'s
//! destination buffer is a stable field of [`Session`] that lives for the
//! whole loop, and a buffer is only re-armed once its prior completion
//! has been consumed. `OP_WRITE` is driven to completion synchronously
//! (its source buffer cannot be reused until the CQE lands), with any
//! unrelated completions that arrive meanwhile stashed and replayed.

use std::collections::VecDeque;
use std::io::Write;
use std::time::Instant;

use slopos_abi::ring::{Cqe, OP_READ, OP_TIMEOUT, OP_WRITE, Sqe};

use super::tcp::TcpConn;
use super::{NcConfig, StdinResult, verbose_bytes, verbose_msg};
use crate::ring::Ring;

// Correlation cookies (SQE.user_data → CQE.user_data). One per logical
// in-flight op; the loop never has two of the same kind outstanding.
const CK_STDIN: u64 = 1;
const CK_SOCK: u64 = 2;
const CK_WRITE: u64 = 3;
const CK_TIMER: u64 = 4;

const STDIN_FD: i32 = 0;
/// Periodic timer tick (ns) used to enforce the inactivity timeout while
/// the harvest is otherwise blocked on I/O readiness. Mirrors the old
/// `poll(.., 100)` cadence (SLOPRING § 12 OP_TIMEOUT note).
const TIMER_TICK_NS: u64 = 200_000_000;

/// One ring-driven established-connection session.
///
/// `listen_mode` selects nc's accept-loop semantics: a normal remote
/// close / timeout returns `None` (let the listener accept again) rather
/// than the client's terminal exit code.
pub(super) struct Session<'a> {
    config: &'a NcConfig,
    conn: &'a TcpConn,
    listen_mode: bool,
    ring: Ring,

    // Stable in-flight read destinations (their addresses are handed to
    // the kernel; they must not move or alias while an op is in flight).
    stdin_buf: [u8; 64],
    sock_buf: [u8; 2048],

    // Line assembly for raw-mode stdin → socket sends.
    line_buf: [u8; 1024],
    line_pos: usize,

    stdin_closed: bool,
    stdin_armed: bool,
    sock_armed: bool,
    timer_armed: bool,

    // Completions harvested out of order (e.g. a socket read that landed
    // while a synchronous send was draining) wait here for the main loop.
    pending: VecDeque<Cqe>,

    clock_start: Instant,
    last_activity_ms: u64,
}

impl<'a> Session<'a> {
    pub(super) fn new(config: &'a NcConfig, conn: &'a TcpConn, listen_mode: bool) -> Option<Self> {
        // 16 SQ slots is comfortably more than the loop's peak in-flight
        // count (stdin + socket + one write + one timer = 4).
        let ring = Ring::setup(16).ok()?;
        Some(Self {
            config,
            conn,
            listen_mode,
            ring,
            stdin_buf: [0u8; 64],
            sock_buf: [0u8; 2048],
            line_buf: [0u8; 1024],
            line_pos: 0,
            stdin_closed: false,
            stdin_armed: false,
            sock_armed: false,
            timer_armed: false,
            pending: VecDeque::new(),
            clock_start: Instant::now(),
            last_activity_ms: 0,
        })
    }

    /// Drive the session to completion. Returns `Some(code)` to exit the
    /// program, or `None` (listen mode only) to resume accepting.
    pub(super) fn run(mut self) -> Option<u8> {
        self.last_activity_ms = self.clock_start.elapsed().as_millis() as u64;
        loop {
            self.rearm();
            let cqe = match self.next_completion() {
                Some(c) => c,
                // Ring/enter failure — treat as a closed connection.
                None => return self.on_closed(),
            };
            if let Some(outcome) = self.dispatch(cqe) {
                return outcome;
            }
            if let Some(outcome) = self.check_timeout() {
                return outcome;
            }
        }
    }

    // -- arming -------------------------------------------------------------

    /// (Re-)submit any read/timer op that is not currently in flight, then
    /// publish the batch with a non-blocking submit.
    fn rearm(&mut self) {
        let mut dirty = false;
        if !self.stdin_closed && !self.stdin_armed {
            let addr = self.stdin_buf.as_mut_ptr() as u64;
            let len = self.stdin_buf.len() as u32;
            self.push_read(CK_STDIN, STDIN_FD, addr, len);
            self.stdin_armed = true;
            dirty = true;
        }
        if !self.sock_armed {
            let addr = self.sock_buf.as_mut_ptr() as u64;
            let len = self.sock_buf.len() as u32;
            self.push_read(CK_SOCK, self.conn.raw(), addr, len);
            self.sock_armed = true;
            dirty = true;
        }
        if self.config.timeout_ms > 0 && !self.timer_armed {
            self.push_timer();
            self.timer_armed = true;
            dirty = true;
        }
        if dirty {
            // Publish the freshly-armed SQEs without blocking; the actual
            // wait happens in `next_completion`.
            let _ = self.ring.submit();
        }
    }

    fn push_read(&mut self, cookie: u64, fd: i32, addr: u64, len: u32) {
        let mut sqe = Sqe::ZERO;
        sqe.opcode = OP_READ;
        sqe.fd = fd;
        sqe.addr = addr;
        sqe.len = len;
        sqe.user_data = cookie;
        // SQ has room (16 slots ≫ peak in-flight); a full SQ would just
        // mean we retry on the next loop turn.
        let _ = self.ring.push_sqe(&sqe);
    }

    fn push_timer(&mut self) {
        let mut sqe = Sqe::ZERO;
        sqe.opcode = OP_TIMEOUT;
        sqe.fd = -1;
        sqe.off = TIMER_TICK_NS;
        sqe.user_data = CK_TIMER;
        let _ = self.ring.push_sqe(&sqe);
    }

    // -- harvesting ---------------------------------------------------------

    /// Next completion, from the stash or by blocking on the ring.
    fn next_completion(&mut self) -> Option<Cqe> {
        if let Some(c) = self.pending.pop_front() {
            return Some(c);
        }
        loop {
            if let Some(c) = self.ring.poll_completion() {
                return Some(c);
            }
            // Block the calling task on every in-flight fd until at least
            // one completes (the caller-as-waiter harvest, SLOPRING § 8.3).
            self.ring.submit_and_wait(1).ok()?;
        }
    }

    fn dispatch(&mut self, cqe: Cqe) -> Option<Option<u8>> {
        match cqe.user_data {
            CK_STDIN => {
                self.stdin_armed = false;
                self.on_stdin(cqe.res)
            }
            CK_SOCK => {
                self.sock_armed = false;
                self.on_sock(cqe.res)
            }
            CK_TIMER => {
                self.timer_armed = false;
                None
            }
            // Stray write completion (shouldn't reach here — writes drain
            // synchronously). Ignore defensively.
            _ => None,
        }
    }

    // -- stdin → socket -----------------------------------------------------

    fn on_stdin(&mut self, res: i32) -> Option<Option<u8>> {
        if res == 0 {
            // stdin EOF: half-close the write side, keep receiving.
            self.stdin_closed = true;
            verbose_msg(self.config, "stdin EOF");
            self.conn.shutdown_write();
            return None;
        }
        if res < 0 {
            // A negative stdin completion is a genuine read error (not
            // would-block — those stay in-flight). Re-arming would
            // busy-spin, so stop reading stdin and half-close the write
            // side, exactly as on EOF.
            self.stdin_closed = true;
            self.conn.shutdown_write();
            return None;
        }
        let n = res as usize;
        let bytes = {
            let mut tmp = [0u8; 64];
            let n = n.min(tmp.len());
            tmp[..n].copy_from_slice(&self.stdin_buf[..n]);
            (tmp, n)
        };
        for i in 0..bytes.1 {
            let mut line_buf = self.line_buf;
            let mut line_pos = self.line_pos;
            let result = super::process_raw_stdin_char(bytes.0[i], &mut line_buf, &mut line_pos);
            self.line_buf = line_buf;
            self.line_pos = line_pos;
            match result {
                StdinResult::SendLine(len) => {
                    let line = {
                        let mut tmp = [0u8; 1024];
                        tmp[..len].copy_from_slice(&self.line_buf[..len]);
                        (tmp, len)
                    };
                    match self.ring_send(&line.0[..line.1]) {
                        Ok(sent) => {
                            verbose_bytes(self.config, "sent ", sent);
                            self.touch();
                        }
                        Err(_) => {
                            eprintln!("nc: send failed (broken pipe)");
                            self.conn.shutdown_both();
                            return Some(Some(1));
                        }
                    }
                    self.line_pos = 0;
                }
                StdinResult::Quit => {
                    self.conn.shutdown_both();
                    return Some(Some(0));
                }
                StdinResult::Continue => {}
            }
        }
        None
    }

    /// Send `data` over the socket via `OP_WRITE`, driven to completion.
    /// Unrelated completions that arrive meanwhile are stashed for the
    /// main loop (the write's source buffer cannot be reused until its
    /// CQE lands, so we cannot return early).
    fn ring_send(&mut self, data: &[u8]) -> Result<usize, ()> {
        let mut total = 0usize;
        while total < data.len() {
            let mut sqe = Sqe::ZERO;
            sqe.opcode = OP_WRITE;
            sqe.fd = self.conn.raw();
            sqe.addr = data[total..].as_ptr() as u64;
            sqe.len = (data.len() - total) as u32;
            sqe.user_data = CK_WRITE;
            self.ring.push_sqe(&sqe).map_err(|_| ())?;
            let res = self.await_cookie(CK_WRITE)?;
            if res <= 0 {
                return Err(());
            }
            total += res as usize;
        }
        Ok(total)
    }

    /// Block until the completion for `cookie` arrives, stashing every
    /// other completion into `pending` for the main loop to process.
    fn await_cookie(&mut self, cookie: u64) -> Result<i32, ()> {
        loop {
            // Submit the pending SQE(s) and block for at least one CQE.
            self.ring.submit_and_wait(1).map_err(|_| ())?;
            while let Some(c) = self.ring.poll_completion() {
                if c.user_data == cookie {
                    return Ok(c.res);
                }
                self.pending.push_back(c);
            }
        }
    }

    // -- socket → stdout ----------------------------------------------------

    fn on_sock(&mut self, res: i32) -> Option<Option<u8>> {
        if res == 0 {
            verbose_msg(self.config, "connection closed by remote");
            self.conn.shutdown_both();
            return Some(self.on_closed());
        }
        if res < 0 {
            // A negative socket-read completion is a *genuine* error
            // (would-block never reaches here — the kernel keeps those
            // in-flight, SLOPRING § 7.1). A reset peer surfaces here as
            // -ECONNRESET on every probe; treating it as transient would
            // busy-spin the loop at 100% CPU. Tear the connection down.
            verbose_msg(self.config, "connection error");
            self.conn.shutdown_both();
            return Some(self.on_closed());
        }
        let received = (res as usize).min(self.sock_buf.len());
        {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(&self.sock_buf[..received]);
            if received > 0 && self.sock_buf[received - 1] != b'\n' {
                let _ = out.write_all(b"\n");
            }
            let _ = out.flush();
        }
        verbose_bytes(self.config, "received ", received);
        self.touch();
        None
    }

    // -- bookkeeping --------------------------------------------------------

    fn touch(&mut self) {
        self.last_activity_ms = self.clock_start.elapsed().as_millis() as u64;
    }

    fn check_timeout(&mut self) -> Option<Option<u8>> {
        if self.config.timeout_ms == 0 {
            return None;
        }
        let now = self.clock_start.elapsed().as_millis() as u64;
        if now.wrapping_sub(self.last_activity_ms) >= self.config.timeout_ms as u64 {
            eprintln!("nc: timeout");
            self.conn.shutdown_both();
            return Some(if self.listen_mode { None } else { Some(1) });
        }
        None
    }

    /// Terminal "connection finished" outcome, honoring listen vs client.
    fn on_closed(&self) -> Option<u8> {
        if self.listen_mode { None } else { Some(0) }
    }
}
