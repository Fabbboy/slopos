use crate::syscall::{SockAddrIn, UserPollFd, fs, net};
use slopos_abi::syscall::POLLIN;
use std::thread;
use std::time::{Duration, Instant};

use super::{NcConfig, StdinResult, stdout_write, verbose_addr, verbose_bytes, verbose_msg};

pub(super) fn tcp_client(config: &NcConfig) -> u8 {
    let fd = match net::socket(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_STREAM, 0) {
        Ok(fd) => fd,
        Err(_) => {
            stdout_write(b"nc: socket creation failed\n");
            return 1;
        }
    };

    if config.local_port != 0 {
        if let Err(_) = net::bind_any(fd, config.local_port) {
            stdout_write(b"nc: bind failed (port in use?)\n");
            return 1;
        }
    }

    let dest = SockAddrIn {
        family: slopos_abi::net::AF_INET,
        port: config.remote_port.to_be(),
        addr: config.remote_addr,
        _pad: [0; 8],
    };

    verbose_addr(
        config,
        "connecting to ",
        config.remote_addr,
        config.remote_port,
    );

    if let Err(_) = net::connect(fd, &dest) {
        stdout_write(b"nc: connect failed\n");
        return 1;
    }

    verbose_addr(
        config,
        "connected to ",
        config.remote_addr,
        config.remote_port,
    );
    verbose_msg(config, "protocol: tcp");

    if let Err(_) = net::set_nonblocking(fd) {
        stdout_write(b"nc: failed to set non-blocking\n");
        return 1;
    }

    // Separate read buffer for raw chars from terminal.
    let mut read_buf = [0u8; 64];
    // Line accumulation buffer: chars build up here until Enter sends them.
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

        // --- stdin (raw char-by-char) ---
        if !stdin_closed && (pfds[0].revents & POLLIN) != 0 {
            match fs::read_slice(0, &mut read_buf) {
                Ok(0) => {
                    stdin_closed = true;
                    verbose_msg(config, "stdin EOF");
                    let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_WR);
                }
                Ok(n) => {
                    for i in 0..n {
                        match super::process_raw_stdin_char(
                            read_buf[i],
                            &mut line_buf,
                            &mut line_pos,
                        ) {
                            StdinResult::SendLine(len) => {
                                match net::send(fd, &line_buf[..len], 0) {
                                    Ok(sent) => {
                                        verbose_bytes(config, "sent ", sent);
                                        last_activity_ms = clock_start.elapsed().as_millis() as u64;
                                    }
                                    Err(_) => {
                                        stdout_write(b"nc: send failed (broken pipe)\n");
                                        let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                                        return 1;
                                    }
                                }
                                line_pos = 0;
                            }
                            StdinResult::Continue => {}
                        }
                    }
                }
                Err(_) => {}
            }
        }

        // --- socket recv ---
        if (pfds[1].revents & POLLIN) != 0 {
            match net::recv(fd, &mut recv_buf, 0) {
                Ok(0) => {
                    verbose_msg(config, "connection closed by remote");
                    let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                    return 0;
                }
                Ok(received) => {
                    stdout_write(&recv_buf[..received]);
                    if recv_buf[received - 1] != b'\n' {
                        stdout_write(b"\n");
                    }
                    verbose_bytes(config, "received ", received);
                    last_activity_ms = clock_start.elapsed().as_millis() as u64;
                }
                Err(_) => {}
            }
        }

        if (pfds[1].revents & (slopos_abi::syscall::POLLHUP | slopos_abi::syscall::POLLERR)) != 0 {
            verbose_msg(config, "connection closed");
            let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
            return 0;
        }

        if config.timeout_ms > 0 {
            let now = clock_start.elapsed().as_millis() as u64;
            if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                stdout_write(b"nc: timeout\n");
                let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                return 1;
            }
        }
    }
}

pub(super) fn tcp_listen(config: &NcConfig) -> u8 {
    let fd = match net::socket(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_STREAM, 0) {
        Ok(fd) => fd,
        Err(_) => {
            stdout_write(b"nc: socket creation failed\n");
            return 1;
        }
    };

    let _ = net::set_reuse_addr(fd);

    if let Err(_) = net::bind_any(fd, config.local_port) {
        stdout_write(b"nc: bind failed (port in use?)\n");
        return 1;
    }

    if let Err(_) = net::listen(fd, 1) {
        stdout_write(b"nc: listen failed\n");
        return 1;
    }

    if let Err(_) = net::set_nonblocking(fd) {
        stdout_write(b"nc: failed to set non-blocking\n");
        return 1;
    }

    if config.verbose {
        eprintln!("nc: listening on 0.0.0.0:{} (tcp)", config.local_port);
    }

    let clock_start = Instant::now();
    let accept_start = Instant::now();

    loop {
        let mut peer = SockAddrIn::default();
        let client_fd = loop {
            if config.timeout_ms > 0 {
                let elapsed = accept_start.elapsed().as_millis() as u64;
                if elapsed >= config.timeout_ms as u64 {
                    stdout_write(b"nc: timeout waiting for connection\n");
                    let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                    return 1;
                }
            }

            match net::accept(fd, Some(&mut peer)) {
                Ok(cfd) => break cfd,
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        };

        verbose_addr(
            config,
            "connection from ",
            peer.addr,
            u16::from_be(peer.port),
        );

        if let Err(_) = net::set_nonblocking(client_fd) {
            stdout_write(b"nc: failed to set non-blocking on client socket\n");
            let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_RDWR);
            if !config.keep_listen {
                return 1;
            }
            continue;
        }

        let mut read_buf = [0u8; 64];
        let mut line_buf = [0u8; 1024];
        let mut line_pos = 0usize;
        let mut recv_buf = [0u8; 2048];
        let mut stdin_closed = false;
        let mut last_activity_ms = clock_start.elapsed().as_millis() as u64;

        let client_exit = 'client: loop {
            let mut pfds = [
                UserPollFd {
                    fd: 0,
                    events: if stdin_closed { 0 } else { POLLIN },
                    revents: 0,
                },
                UserPollFd {
                    fd: client_fd,
                    events: POLLIN,
                    revents: 0,
                },
            ];

            let _ = fs::poll(&mut pfds, 100);

            // --- stdin (raw char-by-char) ---
            if !stdin_closed && (pfds[0].revents & POLLIN) != 0 {
                match fs::read_slice(0, &mut read_buf) {
                    Ok(0) => {
                        stdin_closed = true;
                        verbose_msg(config, "stdin EOF");
                        let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_WR);
                    }
                    Ok(n) => {
                        for i in 0..n {
                            match super::process_raw_stdin_char(
                                read_buf[i],
                                &mut line_buf,
                                &mut line_pos,
                            ) {
                                StdinResult::SendLine(len) => {
                                    match net::send(client_fd, &line_buf[..len], 0) {
                                        Ok(sent) => {
                                            verbose_bytes(config, "sent ", sent);
                                            last_activity_ms =
                                                clock_start.elapsed().as_millis() as u64;
                                        }
                                        Err(_) => {
                                            stdout_write(b"nc: send failed (broken pipe)\n");
                                            let _ = net::shutdown(
                                                client_fd,
                                                slopos_abi::syscall::SHUT_RDWR,
                                            );
                                            break 'client Some(1u8);
                                        }
                                    }
                                    line_pos = 0;
                                }
                                StdinResult::Continue => {}
                            }
                        }
                    }
                    Err(_) => {}
                }
            }

            // --- socket recv ---
            if (pfds[1].revents & POLLIN) != 0 {
                match net::recv(client_fd, &mut recv_buf, 0) {
                    Ok(0) => {
                        verbose_msg(config, "connection closed by remote");
                        let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_RDWR);
                        break 'client None;
                    }
                    Ok(received) => {
                        stdout_write(&recv_buf[..received]);
                        if recv_buf[received - 1] != b'\n' {
                            stdout_write(b"\n");
                        }
                        verbose_bytes(config, "received ", received);
                        last_activity_ms = clock_start.elapsed().as_millis() as u64;
                    }
                    Err(_) => {}
                }
            }

            if (pfds[1].revents & (slopos_abi::syscall::POLLHUP | slopos_abi::syscall::POLLERR))
                != 0
            {
                verbose_msg(config, "connection closed");
                let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_RDWR);
                break 'client None;
            }

            if config.timeout_ms > 0 {
                let now = clock_start.elapsed().as_millis() as u64;
                if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                    stdout_write(b"nc: timeout\n");
                    let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_RDWR);
                    break 'client None;
                }
            }
        };

        // If the inner loop requested a hard exit (broken pipe), propagate.
        if let Some(code) = client_exit {
            let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
            return code;
        }

        if !config.keep_listen {
            verbose_msg(config, "exiting (single connection mode)");
            let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
            return 0;
        }

        verbose_msg(config, "waiting for next connection");
    }
}
