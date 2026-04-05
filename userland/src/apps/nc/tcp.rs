use crate::syscall::{UserPollFd, fs, net};
use slopos_abi::net::{AF_INET, SOCK_STREAM, SockAddrIn};
use slopos_abi::syscall::POLLIN;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::thread;
use std::time::{Duration, Instant};

use super::{
    NcConfig, StdinResult, verbose_addr, verbose_bytes, verbose_msg, write_stdout, writeln_stdout,
};

/// Build a kernel `SockAddrIn` from a high-level `SocketAddrV4`.
fn to_sockaddr(addr: SocketAddrV4) -> SockAddrIn {
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
struct TcpConn {
    fd: crate::syscall::OwnedFd,
}

impl TcpConn {
    fn raw(&self) -> i32 {
        self.fd.raw()
    }

    fn send(&self, data: &[u8]) -> Result<usize, ()> {
        net::send(self.raw(), data, 0).map_err(|_| ())
    }

    fn recv(&self, buf: &mut [u8]) -> Result<usize, ()> {
        net::recv(self.raw(), buf, 0).map_err(|_| ())
    }

    fn shutdown_write(&self) {
        let _ = net::shutdown(self.raw(), slopos_abi::syscall::SHUT_WR);
    }

    fn shutdown_both(&self) {
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
            write_stdout(b"nc: socket creation failed\n");
            return 1;
        }
    };

    // Optional local-port bind before connect.
    if config.local_port != 0 {
        if net::bind_any(fd.raw(), config.local_port).is_err() {
            write_stdout(b"nc: bind failed (port in use?)\n");
            return 1;
        }
    }

    if let Err(e) = net::connect(fd.raw(), &dest) {
        write_stdout(b"nc: connect failed: ");
        write_stdout(e.as_str().as_bytes());
        write_stdout(b"\n");
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
        write_stdout(b"nc: failed to set non-blocking\n");
        return 1;
    }

    run_conn_loop(config, &conn)
}

/// Shared poll/IO loop for an established TCP connection.  Used by both the
/// client path and each accepted connection in listen mode.
fn run_conn_loop(config: &NcConfig, conn: &TcpConn) -> u8 {
    let mut read_buf = [0u8; 64];
    let mut line_buf = [0u8; 1024];
    let mut line_pos = 0usize;
    let mut recv_buf = [0u8; 2048];
    let mut stdin_closed = false;
    let clock_start = Instant::now();
    let mut last_activity_ms = 0u64;

    loop {
        let mut pfds = [
            UserPollFd {
                fd: 0,
                events: if stdin_closed { 0 } else { POLLIN },
                revents: 0,
            },
            UserPollFd {
                fd: conn.raw(),
                events: POLLIN,
                revents: 0,
            },
        ];

        let _ = fs::poll(&mut pfds, 100);

        if !stdin_closed && (pfds[0].revents & POLLIN) != 0 {
            match fs::read_slice(0, &mut read_buf) {
                Ok(0) => {
                    stdin_closed = true;
                    verbose_msg(config, "stdin EOF");
                    conn.shutdown_write();
                }
                Ok(n) => {
                    for i in 0..n {
                        match super::process_raw_stdin_char(
                            read_buf[i],
                            &mut line_buf,
                            &mut line_pos,
                        ) {
                            StdinResult::SendLine(len) => {
                                match conn.send(&line_buf[..len]) {
                                    Ok(sent) => {
                                        verbose_bytes(config, "sent ", sent);
                                        last_activity_ms = clock_start.elapsed().as_millis() as u64;
                                    }
                                    Err(_) => {
                                        write_stdout(b"nc: send failed (broken pipe)\n");
                                        conn.shutdown_both();
                                        return 1;
                                    }
                                }
                                line_pos = 0;
                            }
                            StdinResult::Quit => {
                                conn.shutdown_both();
                                return 0;
                            }
                            StdinResult::Continue => {}
                        }
                    }
                }
                Err(_) => {}
            }
        }

        if (pfds[1].revents & POLLIN) != 0 {
            match conn.recv(&mut recv_buf) {
                Ok(0) => {
                    verbose_msg(config, "connection closed by remote");
                    conn.shutdown_both();
                    return 0;
                }
                Ok(received) => {
                    write_stdout(&recv_buf[..received]);
                    if recv_buf[received - 1] != b'\n' {
                        write_stdout(b"\n");
                    }
                    verbose_bytes(config, "received ", received);
                    last_activity_ms = clock_start.elapsed().as_millis() as u64;
                }
                Err(_) => {}
            }
        }

        if (pfds[1].revents & (slopos_abi::syscall::POLLHUP | slopos_abi::syscall::POLLERR)) != 0 {
            verbose_msg(config, "connection closed");
            conn.shutdown_both();
            return 0;
        }

        if config.timeout_ms > 0 {
            let now = clock_start.elapsed().as_millis() as u64;
            if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                write_stdout(b"nc: timeout\n");
                conn.shutdown_both();
                return 1;
            }
        }
    }
}

pub(super) fn tcp_listen(config: &NcConfig) -> u8 {
    let listen_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, config.local_port);

    let fd = match net::socket(AF_INET, SOCK_STREAM, 0) {
        Ok(f) => f,
        Err(_) => {
            write_stdout(b"nc: socket creation failed\n");
            return 1;
        }
    };

    let _ = net::set_reuse_addr(fd.raw());

    if net::bind_any(fd.raw(), listen_addr.port()).is_err() {
        write_stdout(b"nc: bind failed (port in use?)\n");
        return 1;
    }

    if net::listen(fd.raw(), 1).is_err() {
        write_stdout(b"nc: listen failed\n");
        return 1;
    }

    if net::set_nonblocking(fd.raw()).is_err() {
        write_stdout(b"nc: failed to set non-blocking\n");
        return 1;
    }

    if config.verbose {
        writeln_stdout(format_args!("nc: listening on {} (tcp)", listen_addr));
    }

    let accept_start = Instant::now();

    loop {
        let mut peer = SockAddrIn::default();
        let client_fd = loop {
            if config.timeout_ms > 0 {
                let elapsed = accept_start.elapsed().as_millis() as u64;
                if elapsed >= config.timeout_ms as u64 {
                    write_stdout(b"nc: timeout waiting for connection\n");
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
            write_stdout(b"nc: failed to set non-blocking on client socket\n");
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

/// Poll/IO loop for a single accepted connection in listen mode.
///
/// Returns `Some(code)` to exit immediately, or `None` to allow the listen
/// loop to continue accepting connections (when `keep_listen` is set).
fn run_listen_session(config: &NcConfig, client: &TcpConn) -> Option<u8> {
    let mut read_buf = [0u8; 64];
    let mut line_buf = [0u8; 1024];
    let mut line_pos = 0usize;
    let mut recv_buf = [0u8; 2048];
    let mut stdin_closed = false;
    let clock_start = Instant::now();
    let mut last_activity_ms = clock_start.elapsed().as_millis() as u64;

    loop {
        let mut pfds = [
            UserPollFd {
                fd: 0,
                events: if stdin_closed { 0 } else { POLLIN },
                revents: 0,
            },
            UserPollFd {
                fd: client.raw(),
                events: POLLIN,
                revents: 0,
            },
        ];

        let _ = fs::poll(&mut pfds, 100);

        if !stdin_closed && (pfds[0].revents & POLLIN) != 0 {
            match fs::read_slice(0, &mut read_buf) {
                Ok(0) => {
                    stdin_closed = true;
                    verbose_msg(config, "stdin EOF");
                    client.shutdown_write();
                }
                Ok(n) => {
                    for i in 0..n {
                        match super::process_raw_stdin_char(
                            read_buf[i],
                            &mut line_buf,
                            &mut line_pos,
                        ) {
                            StdinResult::SendLine(len) => {
                                match client.send(&line_buf[..len]) {
                                    Ok(sent) => {
                                        verbose_bytes(config, "sent ", sent);
                                        last_activity_ms = clock_start.elapsed().as_millis() as u64;
                                    }
                                    Err(_) => {
                                        write_stdout(b"nc: send failed (broken pipe)\n");
                                        client.shutdown_both();
                                        return Some(1);
                                    }
                                }
                                line_pos = 0;
                            }
                            StdinResult::Quit => {
                                client.shutdown_both();
                                return Some(0);
                            }
                            StdinResult::Continue => {}
                        }
                    }
                }
                Err(_) => {}
            }
        }

        if (pfds[1].revents & POLLIN) != 0 {
            match client.recv(&mut recv_buf) {
                Ok(0) => {
                    verbose_msg(config, "connection closed by remote");
                    client.shutdown_both();
                    return None;
                }
                Ok(received) => {
                    write_stdout(&recv_buf[..received]);
                    if recv_buf[received - 1] != b'\n' {
                        write_stdout(b"\n");
                    }
                    verbose_bytes(config, "received ", received);
                    last_activity_ms = clock_start.elapsed().as_millis() as u64;
                }
                Err(_) => {}
            }
        }

        if (pfds[1].revents & (slopos_abi::syscall::POLLHUP | slopos_abi::syscall::POLLERR)) != 0 {
            verbose_msg(config, "connection closed");
            client.shutdown_both();
            return None;
        }

        if config.timeout_ms > 0 {
            let now = clock_start.elapsed().as_millis() as u64;
            if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                write_stdout(b"nc: timeout\n");
                client.shutdown_both();
                return None;
            }
        }
    }
}
