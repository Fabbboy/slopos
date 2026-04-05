use crate::syscall::{UserPollFd, fs};
use slopos_abi::syscall::POLLIN;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Instant;

use super::{
    NcConfig, StdinResult, verbose_addr, verbose_bytes, verbose_msg, verbose_recv, write_stdout,
    writeln_stdout,
};

/// Extract the raw fd number from a `UdpSocket`.
///
/// `std::os::fd::AsRawFd` is not available on the SlopOS target because the
/// `os::fd` module is gated behind `target_family = "unix"`.  The internal
/// layout of `UdpSocket` is a chain of single-field newtypes that bottoms
/// out at a bare `i32` file descriptor, so reading the first `i32` at the
/// struct's address gives us the fd.
fn socket_raw_fd(socket: &UdpSocket) -> i32 {
    // SAFETY: UdpSocket → net_imp::UdpSocket → Socket → FileDesc → OwnedFd → i32.
    // All are #[repr(transparent)] or single-field structs with no padding
    // before the fd field, so the i32 sits at offset 0.
    unsafe { std::ptr::read(socket as *const UdpSocket as *const i32) }
}

/// Extract the IPv4 octets and port from a `SocketAddr`, falling back to
/// zeros for IPv6 (should never happen in practice).
fn addr_parts(addr: &SocketAddr) -> ([u8; 4], u16) {
    match addr {
        SocketAddr::V4(v4) => (v4.ip().octets(), v4.port()),
        SocketAddr::V6(_) => ([0; 4], 0),
    }
}

pub(super) fn udp_client(config: &NcConfig) -> u8 {
    let bind_addr = if config.local_port != 0 {
        format!("0.0.0.0:{}", config.local_port)
    } else {
        "0.0.0.0:0".into()
    };

    let socket = match UdpSocket::bind(&bind_addr) {
        Ok(s) => s,
        Err(_) => {
            write_stdout(b"nc: socket creation failed\n");
            return 1;
        }
    };

    if let Err(_) = socket.set_nonblocking(true) {
        write_stdout(b"nc: failed to set non-blocking\n");
        return 1;
    }

    verbose_addr(
        config,
        "connected to ",
        config.remote_addr,
        config.remote_port,
    );
    verbose_msg(config, "protocol: udp");

    let dest = SocketAddrV4::new(Ipv4Addr::from(config.remote_addr), config.remote_port);
    let sock_fd = socket_raw_fd(&socket);

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
                fd: sock_fd,
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
                }
                Ok(n) => {
                    for i in 0..n {
                        match super::process_raw_stdin_char(
                            read_buf[i],
                            &mut line_buf,
                            &mut line_pos,
                        ) {
                            StdinResult::SendLine(len) => {
                                match socket.send_to(&line_buf[..len], dest) {
                                    Ok(sent) => {
                                        verbose_bytes(config, "sent ", sent);
                                        last_activity_ms = clock_start.elapsed().as_millis() as u64;
                                    }
                                    Err(_) => {
                                        write_stdout(b"nc: send failed\n");
                                    }
                                }
                                line_pos = 0;
                            }
                            StdinResult::Quit => {
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
            match socket.recv_from(&mut recv_buf) {
                Ok((0, _)) => {}
                Ok((received, src_addr)) => {
                    write_stdout(&recv_buf[..received]);
                    if recv_buf[received - 1] != b'\n' {
                        write_stdout(b"\n");
                    }
                    let (ip, port) = addr_parts(&src_addr);
                    verbose_recv(config, received, ip, port);
                    last_activity_ms = clock_start.elapsed().as_millis() as u64;
                }
                Err(_) => {}
            }
        }

        if config.timeout_ms > 0 {
            let now = clock_start.elapsed().as_millis() as u64;
            if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                write_stdout(b"nc: timeout\n");
                return 1;
            }
        }
    }
}

pub(super) fn udp_listen(config: &NcConfig) -> u8 {
    let bind_addr = format!("0.0.0.0:{}", config.local_port);

    let socket = match UdpSocket::bind(&bind_addr) {
        Ok(s) => s,
        Err(_) => {
            write_stdout(b"nc: bind failed (port in use?)\n");
            return 1;
        }
    };

    if let Err(_) = socket.set_nonblocking(true) {
        write_stdout(b"nc: failed to set non-blocking\n");
        return 1;
    }

    if config.verbose {
        writeln_stdout(format_args!(
            "nc: listening on 0.0.0.0:{} (udp)",
            config.local_port
        ));
    }

    let sock_fd = socket_raw_fd(&socket);

    let mut read_buf = [0u8; 64];
    let mut line_buf = [0u8; 1024];
    let mut line_pos = 0usize;
    let mut recv_buf = [0u8; 2048];
    let mut stdin_closed = false;
    let mut last_peer: Option<SocketAddr> = None;
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
                fd: sock_fd,
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
                }
                Ok(n) => {
                    for i in 0..n {
                        match super::process_raw_stdin_char(
                            read_buf[i],
                            &mut line_buf,
                            &mut line_pos,
                        ) {
                            StdinResult::SendLine(len) => {
                                if let Some(peer) = last_peer {
                                    match socket.send_to(&line_buf[..len], peer) {
                                        Ok(sent) => {
                                            verbose_bytes(config, "sent ", sent);
                                            last_activity_ms =
                                                clock_start.elapsed().as_millis() as u64;
                                        }
                                        Err(_) => {
                                            write_stdout(b"nc: send failed\n");
                                        }
                                    }
                                }
                                line_pos = 0;
                            }
                            StdinResult::Quit => {
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
            match socket.recv_from(&mut recv_buf) {
                Ok((0, _)) => {}
                Ok((received, src_addr)) => {
                    write_stdout(&recv_buf[..received]);
                    if recv_buf[received - 1] != b'\n' {
                        write_stdout(b"\n");
                    }
                    let (ip, port) = addr_parts(&src_addr);
                    verbose_recv(config, received, ip, port);
                    last_peer = Some(src_addr);
                    last_activity_ms = clock_start.elapsed().as_millis() as u64;
                }
                Err(_) => {}
            }
        }

        if config.timeout_ms > 0 {
            let now = clock_start.elapsed().as_millis() as u64;
            if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                write_stdout(b"nc: timeout\n");
                return 1;
            }
        }
    }
}
