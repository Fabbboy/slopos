//! Ring-driven, `async`/`await` I/O session for nc's TCP recv/send loop.
//!
//! The established-connection loop races three leaf futures with
//! [`slopfut::select3`]: an `OP_READ` on stdin, an `OP_READ` on the socket, and
//! a periodic `OP_TIMEOUT` tick that bounds the wait so the inactivity timeout
//! is enforced. Multiplexing is the kernel's caller-as-waiter harvest
//! (SLOPRING § 7.1/§ 8.3).
//!
//! `connect`/`listen`/`accept`/`shutdown` stay regular syscalls, outside the
//! nine-opcode data plane (SLOPRING § 12).
//!
//! Buffer lifetime (no UAF): each `OP_READ`/`OP_WRITE` buffer is a `Vec`
//! *owned by the reactor* while in flight, so a buffer the kernel might still
//! write is never freed — even when `select3` drops the losing read and fires
//! its `OP_CANCEL`.

use std::io::Write;
use std::time::Instant;

use super::tcp::TcpConn;
use super::{NcConfig, StdinResult, verbose_bytes, verbose_msg};
use crate::ring::{Ring, slopfut};

const STDIN_FD: i32 = 0;
/// stdin read buffer capacity (one keystroke burst at a time).
const STDIN_CAP: usize = 64;
const SOCK_CAP: usize = 2048;
/// Periodic timer tick (ns). Bounds an otherwise I/O-only `select` so the
/// inactivity timeout is checked even while no data flows
/// (SLOPRING § 12 OP_TIMEOUT note).
const TIMER_TICK_NS: u64 = 200_000_000;

/// One ring-driven established-connection session.
///
/// `listen_mode` selects nc's accept-loop semantics: a normal remote close or
/// timeout returns `None` so the listener accepts again, rather than the
/// client's terminal exit code.
pub(super) struct Session<'a> {
    config: &'a NcConfig,
    conn: &'a TcpConn,
    listen_mode: bool,

    line_buf: [u8; 1024],
    line_pos: usize,

    stdin_closed: bool,

    clock_start: Instant,
    last_activity_ms: u64,
}

impl<'a> Session<'a> {
    pub(super) fn new(config: &'a NcConfig, conn: &'a TcpConn, listen_mode: bool) -> Self {
        Self {
            config,
            conn,
            listen_mode,
            line_buf: [0u8; 1024],
            line_pos: 0,
            stdin_closed: false,
            clock_start: Instant::now(),
            last_activity_ms: 0,
        }
    }

    /// Drive the session to completion. Returns `Some(code)` to exit the
    /// program, or `None` (listen mode only) to resume accepting.
    pub(super) fn run(mut self) -> Option<u8> {
        // 16 SQ slots is comfortably more than the loop's peak in-flight count
        // (stdin + socket + one write + one timer).
        let ring = match Ring::setup(16) {
            Ok(r) => r,
            Err(_) => {
                eprintln!("nc: ring setup failed");
                self.conn.shutdown_both();
                return Some(1);
            }
        };
        self.last_activity_ms = self.clock_start.elapsed().as_millis() as u64;
        slopfut::block_on(ring, self.run_async())
    }

    async fn run_async(mut self) -> Option<u8> {
        type DynBuf = core::pin::Pin<Box<dyn core::future::Future<Output = slopfut::BufResult>>>;
        type DynInt = core::pin::Pin<Box<dyn core::future::Future<Output = i32>>>;

        // The winning read returns its buffer; a cancelled read keeps its buffer
        // in the reactor until the cancellation lands, so the loser is handed a
        // fresh one next turn.
        let mut stdin_buf = vec![0u8; STDIN_CAP];
        let mut sock_buf = vec![0u8; SOCK_CAP];

        loop {
            let fd_sock = self.conn.raw();
            // A closed stdin or disabled timer becomes a never-resolving
            // `pending()` so the `select3` shape stays fixed.
            let f_stdin: DynBuf = if self.stdin_closed {
                Box::pin(core::future::pending())
            } else {
                Box::pin(slopfut::read(
                    STDIN_FD,
                    core::mem::take(&mut stdin_buf),
                    STDIN_CAP as u32,
                ))
            };
            let f_sock: DynBuf = Box::pin(slopfut::read(
                fd_sock,
                core::mem::take(&mut sock_buf),
                SOCK_CAP as u32,
            ));
            let f_timer: DynInt = if self.config.timeout_ms > 0 {
                Box::pin(slopfut::timeout(TIMER_TICK_NS))
            } else {
                Box::pin(core::future::pending())
            };

            match slopfut::select3(f_stdin, f_sock, f_timer).await {
                slopfut::Either3::A(br) => {
                    sock_buf = vec![0u8; SOCK_CAP];
                    let outcome = self.on_stdin(br.res, &br.buf).await;
                    stdin_buf = br.buf;
                    if let Some(out) = outcome {
                        return out;
                    }
                }
                slopfut::Either3::B(br) => {
                    if !self.stdin_closed {
                        stdin_buf = vec![0u8; STDIN_CAP];
                    }
                    let outcome = self.on_sock(br.res, &br.buf);
                    sock_buf = br.buf;
                    if let Some(out) = outcome {
                        return out;
                    }
                }
                slopfut::Either3::C(_) => {
                    if !self.stdin_closed {
                        stdin_buf = vec![0u8; STDIN_CAP];
                    }
                    sock_buf = vec![0u8; SOCK_CAP];
                    if let Some(out) = self.check_timeout() {
                        return out;
                    }
                }
            }
        }
    }

    async fn on_stdin(&mut self, res: i32, buf: &[u8]) -> Option<Option<u8>> {
        if res <= 0 {
            // EOF or a genuine error; would-block never reaches here, the kernel
            // keeps those in-flight. Re-arming on an error would busy-spin.
            self.stdin_closed = true;
            if res == 0 {
                verbose_msg(self.config, "stdin EOF");
            }
            self.conn.shutdown_write();
            return None;
        }
        let n = (res as usize).min(buf.len());
        for &byte in &buf[..n] {
            let result =
                super::process_raw_stdin_char(byte, &mut self.line_buf, &mut self.line_pos);
            match result {
                StdinResult::SendLine(len) => {
                    let line: Vec<u8> = self.line_buf[..len].to_vec();
                    if self.send_all(&line).await {
                        verbose_bytes(self.config, "sent ", line.len());
                        self.touch();
                    } else {
                        eprintln!("nc: send failed (broken pipe)");
                        self.conn.shutdown_both();
                        return Some(Some(1));
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

    /// Send all of `data` over the socket via `OP_WRITE`, awaiting each
    /// chunk. Returns `false` on a write error (broken pipe).
    async fn send_all(&self, data: &[u8]) -> bool {
        let mut total = 0usize;
        while total < data.len() {
            let chunk = data[total..].to_vec();
            let br = slopfut::write(self.conn.raw(), chunk).await;
            if br.res <= 0 {
                return false;
            }
            total += br.res as usize;
        }
        true
    }

    fn on_sock(&mut self, res: i32, buf: &[u8]) -> Option<Option<u8>> {
        if res == 0 {
            verbose_msg(self.config, "connection closed by remote");
            self.conn.shutdown_both();
            return Some(self.on_closed());
        }
        if res < 0 {
            // A reset peer surfaces here on every probe; treating it as
            // transient would busy-spin.
            verbose_msg(self.config, "connection error");
            self.conn.shutdown_both();
            return Some(self.on_closed());
        }
        let received = (res as usize).min(buf.len());
        {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(&buf[..received]);
            if received > 0 && buf[received - 1] != b'\n' {
                let _ = out.write_all(b"\n");
            }
            let _ = out.flush();
        }
        verbose_bytes(self.config, "received ", received);
        self.touch();
        None
    }

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
