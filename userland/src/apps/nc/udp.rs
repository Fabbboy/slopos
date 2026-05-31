use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Instant;

use slopos_abi::net::SockAddrIn;

use crate::ring::{Ring, slopfut};

use super::{NcConfig, StdinResult, verbose_addr, verbose_bytes, verbose_msg, verbose_recv};

/// UDP client — ported to the async ring edge.
///
/// A datagram socket is `connect()`ed to the peer (a plain syscall that, for
/// UDP, just pins `remote_addr`), after which the socket carries the same
/// connected `recv`/`send` semantics a TCP socket does. So the established
/// loop is the *identical* async [`Session`](super::ring_io) the TCP path
/// uses — `OP_READ`/`OP_WRITE` over the ring, no `poll(2)`, no
/// `std::net::UdpSocket`, no raw-fd transmute.
pub(super) fn udp_client(config: &NcConfig) -> u8 {
    use slopos_abi::net::{AF_INET, SOCK_DGRAM};

    let remote = SocketAddrV4::new(Ipv4Addr::from(config.remote_addr), config.remote_port);
    let dest = super::tcp::to_sockaddr(remote);

    let fd = match crate::syscall::net::socket(AF_INET, SOCK_DGRAM, 0) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("nc: socket creation failed");
            return 1;
        }
    };

    if config.local_port != 0 && crate::syscall::net::bind_any(fd.raw(), config.local_port).is_err()
    {
        eprintln!("nc: bind failed (port in use?)");
        return 1;
    }

    if crate::syscall::net::connect(fd.raw(), &dest).is_err() {
        eprintln!("nc: connect failed");
        return 1;
    }

    let conn = super::tcp::TcpConn::from_fd(fd);

    if crate::syscall::net::set_nonblocking(conn.raw()).is_err() {
        eprintln!("nc: failed to set non-blocking");
        return 1;
    }

    verbose_addr(
        config,
        "connected to ",
        config.remote_addr,
        config.remote_port,
    );
    verbose_msg(config, "protocol: udp");

    super::ring_io::Session::new(config, &conn, false)
        .run()
        .unwrap_or(0)
}

/// UDP listen — ported to the async ring edge with `OP_RECVFROM`.
///
/// Listen mode must learn each datagram's *source* address to reply
/// (`last_peer`). `OP_RECVFROM` (SLOPRING § 12) returns that source
/// `SockAddrIn` alongside the data, so the loop is now the same
/// `slopfut::block_on` + `select` shape the UDP *client* and TCP use — no
/// `poll(2)`, no `std::net::UdpSocket`, no raw-fd transmute. Receive is
/// `OP_RECVFROM`; replies to `last_peer` go out as a connected `OP_SEND`
/// after a per-line `connect` (UDP `connect` just pins the remote addr).
pub(super) fn udp_listen(config: &NcConfig) -> u8 {
    use slopos_abi::net::{AF_INET, SOCK_DGRAM};

    let fd = match crate::syscall::net::socket(AF_INET, SOCK_DGRAM, 0) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("nc: socket creation failed");
            return 1;
        }
    };
    if crate::syscall::net::set_reuse_addr(fd.raw()).is_err() {
        // Non-fatal: reuse-addr is a convenience, not a correctness need.
    }
    if crate::syscall::net::bind_any(fd.raw(), config.local_port).is_err() {
        eprintln!("nc: bind failed (port in use?)");
        return 1;
    }
    if crate::syscall::net::set_nonblocking(fd.raw()).is_err() {
        eprintln!("nc: failed to set non-blocking");
        return 1;
    }

    if config.verbose {
        println!("nc: listening on 0.0.0.0:{} (udp)", config.local_port);
    }

    let ring = match Ring::setup(16) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("nc: ring setup failed");
            return 1;
        }
    };

    let sock_fd = fd.raw();
    slopfut::block_on(ring, listen_async(config, sock_fd))
}

/// stdin read buffer capacity (one keystroke burst at a time).
const STDIN_CAP: usize = 64;
/// datagram receive buffer capacity.
const RECV_CAP: usize = 2048;
/// Periodic timer tick (ns) — bounds the otherwise I/O-only `select` so
/// the inactivity timeout is checked even while no data flows.
const TIMER_TICK_NS: u64 = 200_000_000;

/// The async UDP-listen loop: race stdin-read / socket-`recvfrom` / timer,
/// act on whichever fires, re-arm, repeat.
async fn listen_async(config: &NcConfig, sock_fd: i32) -> u8 {
    type DynStdin = core::pin::Pin<Box<dyn core::future::Future<Output = slopfut::BufResult>>>;
    type DynRecv = core::pin::Pin<Box<dyn core::future::Future<Output = slopfut::RecvFromResult>>>;
    type DynInt = core::pin::Pin<Box<dyn core::future::Future<Output = i32>>>;

    let mut line_buf = [0u8; 1024];
    let mut line_pos = 0usize;
    let mut last_peer: Option<SocketAddrV4> = None;
    let mut stdin_closed = false;
    let clock_start = Instant::now();
    let mut last_activity_ms = clock_start.elapsed().as_millis() as u64;

    // Buffers ping-pong between this loop and the in-flight reads (the
    // winner returns its buffer; a cancelled loser keeps its buffer in the
    // reactor until the cancel lands, so the loser gets a fresh one).
    let mut stdin_buf = vec![0u8; STDIN_CAP];
    let mut recv_buf = vec![0u8; RECV_CAP];

    loop {
        let f_stdin: DynStdin = if stdin_closed {
            Box::pin(core::future::pending())
        } else {
            Box::pin(slopfut::read(
                0,
                core::mem::take(&mut stdin_buf),
                STDIN_CAP as u32,
            ))
        };
        let f_recv: DynRecv = Box::pin(slopfut::recvfrom(
            sock_fd,
            core::mem::take(&mut recv_buf),
            RECV_CAP as u32,
        ));
        let f_timer: DynInt = if config.timeout_ms > 0 {
            Box::pin(slopfut::timeout(TIMER_TICK_NS))
        } else {
            Box::pin(core::future::pending())
        };

        match slopfut::select3(f_stdin, f_recv, f_timer).await {
            slopfut::Either3::A(br) => {
                recv_buf = vec![0u8; RECV_CAP];
                match on_stdin(
                    config,
                    sock_fd,
                    &mut line_buf,
                    &mut line_pos,
                    last_peer,
                    br.res,
                    &br.buf,
                )
                .await
                {
                    StdinAction::Quit => return 0,
                    StdinAction::Sent => {
                        last_activity_ms = clock_start.elapsed().as_millis() as u64
                    }
                    StdinAction::Eof => {
                        stdin_closed = true;
                        verbose_msg(config, "stdin EOF");
                    }
                    StdinAction::Continue => {}
                }
                stdin_buf = br.buf;
            }
            slopfut::Either3::B(rr) => {
                if !stdin_closed {
                    stdin_buf = vec![0u8; STDIN_CAP];
                }
                if rr.res > 0 {
                    let received = (rr.res as usize).min(rr.buf.len());
                    {
                        let mut out = std::io::stdout().lock();
                        let _ = out.write_all(&rr.buf[..received]);
                        if received > 0 && rr.buf[received - 1] != b'\n' {
                            let _ = out.write_all(b"\n");
                        }
                        let _ = out.flush();
                    }
                    let ip = rr.src.addr;
                    let port = u16::from_be(rr.src.port);
                    verbose_recv(config, received, ip, port);
                    last_peer = Some(SocketAddrV4::new(Ipv4Addr::from(ip), port));
                    last_activity_ms = clock_start.elapsed().as_millis() as u64;
                }
                recv_buf = rr.buf;
            }
            slopfut::Either3::C(_) => {
                if !stdin_closed {
                    stdin_buf = vec![0u8; STDIN_CAP];
                }
                recv_buf = vec![0u8; RECV_CAP];
                if config.timeout_ms > 0 {
                    let now = clock_start.elapsed().as_millis() as u64;
                    if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                        eprintln!("nc: timeout");
                        return 1;
                    }
                }
            }
        }
    }
}

/// What an stdin burst resolved to.
enum StdinAction {
    Continue,
    Sent,
    Eof,
    Quit,
}

/// Process an stdin read completion: assemble lines and reply to
/// `last_peer` (a connected `OP_SEND` after pinning the peer). A reply with
/// no known peer is dropped, matching the old poll path.
async fn on_stdin(
    config: &NcConfig,
    sock_fd: i32,
    line_buf: &mut [u8; 1024],
    line_pos: &mut usize,
    last_peer: Option<SocketAddrV4>,
    res: i32,
    buf: &[u8],
) -> StdinAction {
    if res == 0 {
        return StdinAction::Eof;
    }
    if res < 0 {
        // A genuine stdin error (would-block stays in-flight). Stop reading.
        return StdinAction::Eof;
    }
    let n = (res as usize).min(buf.len());
    let mut sent_any = false;
    for &byte in &buf[..n] {
        match super::process_raw_stdin_char(byte, line_buf, line_pos) {
            StdinResult::SendLine(len) => {
                if let Some(peer) = last_peer {
                    let line: Vec<u8> = line_buf[..len].to_vec();
                    if send_to_peer(config, sock_fd, peer, &line).await {
                        sent_any = true;
                    }
                }
                *line_pos = 0;
            }
            StdinResult::Quit => return StdinAction::Quit,
            StdinResult::Continue => {}
        }
    }
    if sent_any {
        StdinAction::Sent
    } else {
        StdinAction::Continue
    }
}

/// Send `data` to `peer` over the ring: pin the datagram socket's remote
/// addr with a (cheap, non-blocking) `connect`, then `OP_SEND`. Returns
/// `true` on a successful send.
async fn send_to_peer(config: &NcConfig, sock_fd: i32, peer: SocketAddrV4, data: &[u8]) -> bool {
    let dest = SockAddrIn {
        family: slopos_abi::net::AF_INET,
        port: peer.port().to_be(),
        addr: peer.ip().octets(),
        _pad: [0; 8],
    };
    if crate::syscall::net::connect(sock_fd, &dest).is_err() {
        eprintln!("nc: send failed");
        return false;
    }
    let br = slopfut::write(sock_fd, data.to_vec()).await;
    if br.res > 0 {
        verbose_bytes(config, "sent ", br.res as usize);
        true
    } else {
        eprintln!("nc: send failed");
        false
    }
}
