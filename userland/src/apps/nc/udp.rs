use crate::syscall::{SockAddrIn, UserPollFd, fs, net};
use slopos_abi::syscall::POLLIN;
use std::time::Instant;

use super::{
    NcConfig, StdinResult, verbose_addr, verbose_bytes, verbose_msg, verbose_recv, write_stdout,
    writeln_stdout,
};

pub(super) fn udp_client(config: &NcConfig) -> u8 {
    let fd = match net::socket(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_DGRAM, 0) {
        Ok(fd) => fd,
        Err(_) => {
            write_stdout(b"nc: socket creation failed\n");
            return 1;
        }
    };

    if config.local_port != 0 {
        if let Err(_) = net::bind_any(fd, config.local_port) {
            write_stdout(b"nc: bind failed (port in use?)\n");
            return 1;
        }
    }

    if let Err(_) = net::set_nonblocking(fd) {
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

    let dest = SockAddrIn {
        family: slopos_abi::net::AF_INET,
        port: config.remote_port.to_be(),
        addr: config.remote_addr,
        _pad: [0; 8],
    };

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
                fd,
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
                                match net::sendto(fd, &line_buf[..len], 0, &dest) {
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
                                let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
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
            let mut src_addr = SockAddrIn::default();
            match net::recvfrom(fd, &mut recv_buf, 0, Some(&mut src_addr)) {
                Ok(0) => {}
                Ok(received) => {
                    write_stdout(&recv_buf[..received]);
                    if recv_buf[received - 1] != b'\n' {
                        write_stdout(b"\n");
                    }
                    verbose_recv(config, received, src_addr.addr, u16::from_be(src_addr.port));
                    last_activity_ms = clock_start.elapsed().as_millis() as u64;
                }
                Err(_) => {}
            }
        }

        if config.timeout_ms > 0 {
            let now = clock_start.elapsed().as_millis() as u64;
            if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                write_stdout(b"nc: timeout\n");
                let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                return 1;
            }
        }
    }
}

pub(super) fn udp_listen(config: &NcConfig) -> u8 {
    let fd = match net::socket(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_DGRAM, 0) {
        Ok(fd) => fd,
        Err(_) => {
            write_stdout(b"nc: socket creation failed\n");
            return 1;
        }
    };

    let _ = net::set_reuse_addr(fd);

    if let Err(_) = net::bind_any(fd, config.local_port) {
        write_stdout(b"nc: bind failed (port in use?)\n");
        return 1;
    }

    if let Err(_) = net::set_nonblocking(fd) {
        write_stdout(b"nc: failed to set non-blocking\n");
        return 1;
    }

    if config.verbose {
        writeln_stdout(format_args!(
            "nc: listening on 0.0.0.0:{} (udp)",
            config.local_port
        ));
    }

    let mut read_buf = [0u8; 64];
    let mut line_buf = [0u8; 1024];
    let mut line_pos = 0usize;
    let mut recv_buf = [0u8; 2048];
    let mut stdin_closed = false;
    let mut last_peer = SockAddrIn::default();
    let mut has_peer = false;
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
                fd,
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
                                if has_peer {
                                    match net::sendto(fd, &line_buf[..len], 0, &last_peer) {
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
                                let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
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
            let mut src_addr = SockAddrIn::default();
            match net::recvfrom(fd, &mut recv_buf, 0, Some(&mut src_addr)) {
                Ok(0) => {}
                Ok(received) => {
                    write_stdout(&recv_buf[..received]);
                    if recv_buf[received - 1] != b'\n' {
                        write_stdout(b"\n");
                    }
                    verbose_recv(config, received, src_addr.addr, u16::from_be(src_addr.port));
                    last_peer = SockAddrIn {
                        family: src_addr.family,
                        port: src_addr.port,
                        addr: src_addr.addr,
                        _pad: [0; 8],
                    };
                    has_peer = true;
                    last_activity_ms = clock_start.elapsed().as_millis() as u64;
                }
                Err(_) => {}
            }
        }

        if config.timeout_ms > 0 {
            let now = clock_start.elapsed().as_millis() as u64;
            if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                write_stdout(b"nc: timeout\n");
                let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                return 1;
            }
        }
    }
}
