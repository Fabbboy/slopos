use std::net::{Ipv4Addr, SocketAddrV4};
use std::thread;
use std::time::{Duration, Instant};

use crate::syscall::net;
use slopos_abi::net::{AF_INET, SOCK_STREAM, SockAddrIn};

use super::{NcConfig, verbose_addr, verbose_msg};

/// Build a kernel `SockAddrIn` from a high-level `SocketAddrV4`.
pub(super) fn to_sockaddr(addr: SocketAddrV4) -> SockAddrIn {
    SockAddrIn {
        family: AF_INET,
        port: addr.port().to_be(),
        addr: addr.ip().octets(),
        _pad: [0; 8],
    }
}

/// Extract IP octets and host-order port from a kernel `SockAddrIn`.
fn from_sockaddr(sa: &SockAddrIn) -> ([u8; 4], u16) {
    (sa.addr, u16::from_be(sa.port))
}

/// Owned raw socket fd.  Closes via `net::shutdown` + drop of the underlying
/// `OwnedFd` returned by `net::socket()`.  We store the raw i32 alongside the
/// owning handle so that poll can borrow it without moving the fd.
pub(super) struct TcpConn {
    fd: crate::syscall::OwnedFd,
}

impl TcpConn {
    /// Wrap an already-connected socket fd (used by the UDP-client path,
    /// which `connect()`s a datagram socket and then drives it through the
    /// same async [`Session`](super::ring_io) as TCP).
    pub(super) fn from_fd(fd: crate::syscall::OwnedFd) -> Self {
        Self { fd }
    }

    pub(super) fn raw(&self) -> i32 {
        self.fd.raw()
    }

    pub(super) fn shutdown_write(&self) {
        let _ = net::shutdown(self.raw(), slopos_abi::syscall::SHUT_WR);
    }

    pub(super) fn shutdown_both(&self) {
        let _ = net::shutdown(self.raw(), slopos_abi::syscall::SHUT_RDWR);
    }

    fn set_nonblocking(&self) -> Result<(), ()> {
        net::set_nonblocking(self.raw()).map_err(|_| ())
    }
}

pub(super) fn tcp_client(config: &NcConfig) -> u8 {
    let remote_ip = Ipv4Addr::from(config.remote_addr);
    let remote = SocketAddrV4::new(remote_ip, config.remote_port);
    let dest = to_sockaddr(remote);

    verbose_addr(
        config,
        "connecting to ",
        config.remote_addr,
        config.remote_port,
    );

    // Create socket.
    let fd = match net::socket(AF_INET, SOCK_STREAM, 0) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("nc: socket creation failed");
            return 1;
        }
    };

    // Optional local-port bind before connect.
    if config.local_port != 0 {
        if net::bind_any(fd.raw(), config.local_port).is_err() {
            eprintln!("nc: bind failed (port in use?)");
            return 1;
        }
    }

    if let Err(e) = net::connect(fd.raw(), &dest) {
        eprintln!("nc: connect failed: {}", e.as_str());
        return 1;
    }

    let conn = TcpConn { fd };

    verbose_addr(
        config,
        "connected to ",
        config.remote_addr,
        config.remote_port,
    );
    verbose_msg(config, "protocol: tcp");

    if conn.set_nonblocking().is_err() {
        eprintln!("nc: failed to set non-blocking");
        return 1;
    }

    run_conn_loop(config, &conn)
}

/// Established-connection I/O loop for the client path — Ring-driven.
/// Delegates to the SlopRing [`Session`](super::ring_io) so stdin +
/// socket are multiplexed through `OP_READ`/`OP_WRITE` and harvested via a
/// blocking `ring_enter`. The `connect` that produced `conn` stayed a
/// regular syscall (SLOPRING § 12).
fn run_conn_loop(config: &NcConfig, conn: &TcpConn) -> u8 {
    super::ring_io::Session::new(config, conn, false)
        .run()
        .unwrap_or(0)
}

pub(super) fn tcp_listen(config: &NcConfig) -> u8 {
    let listen_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, config.local_port);

    let fd = match net::socket(AF_INET, SOCK_STREAM, 0) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("nc: socket creation failed");
            return 1;
        }
    };

    let _ = net::set_reuse_addr(fd.raw());

    if net::bind_any(fd.raw(), listen_addr.port()).is_err() {
        eprintln!("nc: bind failed (port in use?)");
        return 1;
    }

    if net::listen(fd.raw(), 1).is_err() {
        eprintln!("nc: listen failed");
        return 1;
    }

    if net::set_nonblocking(fd.raw()).is_err() {
        eprintln!("nc: failed to set non-blocking");
        return 1;
    }

    if config.verbose {
        println!("nc: listening on {listen_addr} (tcp)");
    }

    let accept_start = Instant::now();

    loop {
        let mut peer = SockAddrIn::default();
        let client_fd = loop {
            if config.timeout_ms > 0 {
                let elapsed = accept_start.elapsed().as_millis() as u64;
                if elapsed >= config.timeout_ms as u64 {
                    eprintln!("nc: timeout waiting for connection");
                    return 1;
                }
            }

            match net::accept(fd.raw(), Some(&mut peer)) {
                Ok(c) => break c,
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        };

        let (ip, port) = from_sockaddr(&peer);
        verbose_addr(config, "connection from ", ip, port);

        let client = TcpConn { fd: client_fd };

        if client.set_nonblocking().is_err() {
            eprintln!("nc: failed to set non-blocking on client socket");
            client.shutdown_both();
            if !config.keep_listen {
                return 1;
            }
            continue;
        }

        let exit_code = run_listen_session(config, &client);

        if let Some(code) = exit_code {
            return code;
        }

        if !config.keep_listen {
            verbose_msg(config, "exiting (single connection mode)");
            return 0;
        }

        verbose_msg(config, "waiting for next connection");
    }
}

/// I/O loop for a single accepted connection in listen mode — Ring-driven.
/// Returns `Some(code)` to exit immediately, or `None` to allow the listen
/// loop to keep accepting (`keep_listen`). The `accept` that produced
/// `client` stayed a regular syscall (SLOPRING § 12).
fn run_listen_session(config: &NcConfig, client: &TcpConn) -> Option<u8> {
    super::ring_io::Session::new(config, client, true).run()
}
