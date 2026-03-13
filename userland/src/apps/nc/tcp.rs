use crate::syscall::{
    SockAddrIn, UserPollFd,
    core::{get_time_ms, sleep_ms},
    fs, net,
};
use slopos_abi::syscall::POLLIN;

use super::{NcConfig, StdinResult, verbose_addr, verbose_bytes, verbose_msg, write_out};

pub(super) fn tcp_client(config: &NcConfig) -> u8 {
    let fd = match net::socket(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_STREAM, 0) {
        Ok(fd) => fd,
        Err(_) => {
            write_out(b"nc: socket creation failed\n");
            return 1;
        }
    };

    if config.local_port != 0 {
        if let Err(_) = net::bind_any(fd, config.local_port) {
            write_out(b"nc: bind failed (port in use?)\n");
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
        b"connecting to ",
        config.remote_addr,
        config.remote_port,
    );

    if let Err(_) = net::connect(fd, &dest) {
        write_out(b"nc: connect failed\n");
        return 1;
    }

    verbose_addr(
        config,
        b"connected to ",
        config.remote_addr,
        config.remote_port,
    );
    verbose_msg(config, b"protocol: tcp");

    if let Err(_) = net::set_nonblocking(fd) {
        write_out(b"nc: failed to set non-blocking\n");
        return 1;
    }

    // Separate read buffer for raw chars from terminal.
    let mut read_buf = [0u8; 64];
    // Line accumulation buffer: chars build up here until Enter sends them.
    let mut line_buf = [0u8; 1024];
    let mut line_pos = 0usize;
    let mut recv_buf = [0u8; 2048];
    let mut stdin_closed = false;
    let mut last_activity_ms = get_time_ms();

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
                    verbose_msg(config, b"stdin EOF");
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
                                        verbose_bytes(config, b"sent ", sent);
                                        last_activity_ms = get_time_ms();
                                    }
                                    Err(_) => {
                                        write_out(b"nc: send failed (broken pipe)\n");
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
                    verbose_msg(config, b"connection closed by remote");
                    let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                    return 0;
                }
                Ok(received) => {
                    write_out(&recv_buf[..received]);
                    if recv_buf[received - 1] != b'\n' {
                        write_out(b"\n");
                    }
                    verbose_bytes(config, b"received ", received);
                    last_activity_ms = get_time_ms();
                }
                Err(_) => {}
            }
        }

        if (pfds[1].revents & (slopos_abi::syscall::POLLHUP | slopos_abi::syscall::POLLERR)) != 0 {
            verbose_msg(config, b"connection closed");
            let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
            return 0;
        }

        if config.timeout_ms > 0 {
            let now = get_time_ms();
            if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                write_out(b"nc: timeout\n");
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
            write_out(b"nc: socket creation failed\n");
            return 1;
        }
    };

    let _ = net::set_reuse_addr(fd);

    if let Err(_) = net::bind_any(fd, config.local_port) {
        write_out(b"nc: bind failed (port in use?)\n");
        return 1;
    }

    if let Err(_) = net::listen(fd, 1) {
        write_out(b"nc: listen failed\n");
        return 1;
    }

    if let Err(_) = net::set_nonblocking(fd) {
        write_out(b"nc: failed to set non-blocking\n");
        return 1;
    }

    if config.verbose {
        let mut line = [0u8; 128];
        let mut i = 0usize;
        super::append_bytes(&mut line, &mut i, b"nc: listening on 0.0.0.0:");
        super::write_u16_dec(config.local_port, &mut line, &mut i);
        super::append_bytes(&mut line, &mut i, b" (tcp)\n");
        write_out(&line[..i]);
    }

    let accept_start = get_time_ms();

    loop {
        let mut peer = SockAddrIn::default();
        let client_fd = loop {
            if config.timeout_ms > 0 {
                let elapsed = get_time_ms().wrapping_sub(accept_start);
                if elapsed >= config.timeout_ms as u64 {
                    write_out(b"nc: timeout waiting for connection\n");
                    let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                    return 1;
                }
            }

            match net::accept(fd, Some(&mut peer)) {
                Ok(cfd) => break cfd,
                Err(_) => sleep_ms(10),
            }
        };

        verbose_addr(
            config,
            b"connection from ",
            peer.addr,
            u16::from_be(peer.port),
        );

        if let Err(_) = net::set_nonblocking(client_fd) {
            write_out(b"nc: failed to set non-blocking on client socket\n");
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
        let mut last_activity_ms = get_time_ms();

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
                        verbose_msg(config, b"stdin EOF");
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
                                            verbose_bytes(config, b"sent ", sent);
                                            last_activity_ms = get_time_ms();
                                        }
                                        Err(_) => {
                                            write_out(b"nc: send failed (broken pipe)\n");
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
                        verbose_msg(config, b"connection closed by remote");
                        let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_RDWR);
                        break 'client None;
                    }
                    Ok(received) => {
                        write_out(&recv_buf[..received]);
                        if recv_buf[received - 1] != b'\n' {
                            write_out(b"\n");
                        }
                        verbose_bytes(config, b"received ", received);
                        last_activity_ms = get_time_ms();
                    }
                    Err(_) => {}
                }
            }

            if (pfds[1].revents & (slopos_abi::syscall::POLLHUP | slopos_abi::syscall::POLLERR))
                != 0
            {
                verbose_msg(config, b"connection closed");
                let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_RDWR);
                break 'client None;
            }

            if config.timeout_ms > 0 {
                let now = get_time_ms();
                if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                    write_out(b"nc: timeout\n");
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
            verbose_msg(config, b"exiting (single connection mode)");
            let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
            return 0;
        }

        verbose_msg(config, b"waiting for next connection");
    }
}
