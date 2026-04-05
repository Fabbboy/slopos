use crate::syscall::{SockAddrIn, UserPollFd, fs, net};
use slopos_abi::syscall::POLLIN;
use std::thread;
use std::time::{Duration, Instant};

use super::{
    NcConfig, StdinResult, verbose_addr, verbose_bytes, verbose_msg, write_stdout, writeln_stdout,
};

pub(super) fn tcp_client(config: &NcConfig) -> u8 {
    let sock = match net::Socket::new(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_STREAM, 0) {
        Ok(s) => s,
        Err(_) => {
            write_stdout(b"nc: socket creation failed\n");
            return 1;
        }
    };

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

    // If a local port is specified, bind first then connect.
    let conn: net::Socket<net::Connected> = if config.local_port != 0 {
        let bound = match sock.bind_any(config.local_port) {
            Ok(b) => b,
            Err(_) => {
                write_stdout(b"nc: bind failed (port in use?)\n");
                return 1;
            }
        };
        match bound.connect(&dest) {
            Ok(c) => c,
            Err(e) => {
                write_stdout(b"nc: connect failed: ");
                write_stdout(e.as_str().as_bytes());
                write_stdout(b"\n");
                return 1;
            }
        }
    } else {
        match sock.connect(&dest) {
            Ok(c) => c,
            Err(e) => {
                write_stdout(b"nc: connect failed: ");
                write_stdout(e.as_str().as_bytes());
                write_stdout(b"\n");
                return 1;
            }
        }
    };

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
                fd: conn.raw_fd(),
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
                    let _ = conn.shutdown(slopos_abi::syscall::SHUT_WR);
                }
                Ok(n) => {
                    for i in 0..n {
                        match super::process_raw_stdin_char(
                            read_buf[i],
                            &mut line_buf,
                            &mut line_pos,
                        ) {
                            StdinResult::SendLine(len) => {
                                match conn.send(&line_buf[..len], 0) {
                                    Ok(sent) => {
                                        verbose_bytes(config, "sent ", sent);
                                        last_activity_ms = clock_start.elapsed().as_millis() as u64;
                                    }
                                    Err(_) => {
                                        write_stdout(b"nc: send failed (broken pipe)\n");
                                        let _ = conn.shutdown(slopos_abi::syscall::SHUT_RDWR);
                                        return 1;
                                    }
                                }
                                line_pos = 0;
                            }
                            StdinResult::Quit => {
                                let _ = conn.shutdown(slopos_abi::syscall::SHUT_RDWR);
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
            match conn.recv(&mut recv_buf, 0) {
                Ok(0) => {
                    verbose_msg(config, "connection closed by remote");
                    let _ = conn.shutdown(slopos_abi::syscall::SHUT_RDWR);
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
            let _ = conn.shutdown(slopos_abi::syscall::SHUT_RDWR);
            return 0;
        }

        if config.timeout_ms > 0 {
            let now = clock_start.elapsed().as_millis() as u64;
            if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                write_stdout(b"nc: timeout\n");
                let _ = conn.shutdown(slopos_abi::syscall::SHUT_RDWR);
                return 1;
            }
        }
    }
}

pub(super) fn tcp_listen(config: &NcConfig) -> u8 {
    let sock = match net::Socket::new(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_STREAM, 0) {
        Ok(s) => s,
        Err(_) => {
            write_stdout(b"nc: socket creation failed\n");
            return 1;
        }
    };

    let _ = sock.set_reuse_addr();

    let bound = match sock.bind_any(config.local_port) {
        Ok(b) => b,
        Err(_) => {
            write_stdout(b"nc: bind failed (port in use?)\n");
            return 1;
        }
    };

    let listener = match bound.listen(1) {
        Ok(l) => l,
        Err(_) => {
            write_stdout(b"nc: listen failed\n");
            return 1;
        }
    };

    if listener.set_nonblocking().is_err() {
        write_stdout(b"nc: failed to set non-blocking\n");
        return 1;
    }

    if config.verbose {
        writeln_stdout(format_args!(
            "nc: listening on 0.0.0.0:{} (tcp)",
            config.local_port
        ));
    }

    let clock_start = Instant::now();
    let accept_start = Instant::now();

    loop {
        let mut peer = SockAddrIn::default();
        let client = loop {
            if config.timeout_ms > 0 {
                let elapsed = accept_start.elapsed().as_millis() as u64;
                if elapsed >= config.timeout_ms as u64 {
                    write_stdout(b"nc: timeout waiting for connection\n");
                    return 1;
                }
            }

            match listener.accept(Some(&mut peer)) {
                Ok(c) => break c,
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        };

        verbose_addr(
            config,
            "connection from ",
            peer.addr,
            u16::from_be(peer.port),
        );

        if client.set_nonblocking().is_err() {
            write_stdout(b"nc: failed to set non-blocking on client socket\n");
            let _ = client.shutdown(slopos_abi::syscall::SHUT_RDWR);
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
                    fd: client.raw_fd(),
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
                        let _ = client.shutdown(slopos_abi::syscall::SHUT_WR);
                    }
                    Ok(n) => {
                        for i in 0..n {
                            match super::process_raw_stdin_char(
                                read_buf[i],
                                &mut line_buf,
                                &mut line_pos,
                            ) {
                                StdinResult::SendLine(len) => {
                                    match client.send(&line_buf[..len], 0) {
                                        Ok(sent) => {
                                            verbose_bytes(config, "sent ", sent);
                                            last_activity_ms =
                                                clock_start.elapsed().as_millis() as u64;
                                        }
                                        Err(_) => {
                                            write_stdout(b"nc: send failed (broken pipe)\n");
                                            let _ = client.shutdown(slopos_abi::syscall::SHUT_RDWR);
                                            break 'client Some(1u8);
                                        }
                                    }
                                    line_pos = 0;
                                }
                                StdinResult::Quit => {
                                    let _ = client.shutdown(slopos_abi::syscall::SHUT_RDWR);
                                    break 'client Some(0u8);
                                }
                                StdinResult::Continue => {}
                            }
                        }
                    }
                    Err(_) => {}
                }
            }

            if (pfds[1].revents & POLLIN) != 0 {
                match client.recv(&mut recv_buf, 0) {
                    Ok(0) => {
                        verbose_msg(config, "connection closed by remote");
                        let _ = client.shutdown(slopos_abi::syscall::SHUT_RDWR);
                        break 'client None;
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

            if (pfds[1].revents & (slopos_abi::syscall::POLLHUP | slopos_abi::syscall::POLLERR))
                != 0
            {
                verbose_msg(config, "connection closed");
                let _ = client.shutdown(slopos_abi::syscall::SHUT_RDWR);
                break 'client None;
            }

            if config.timeout_ms > 0 {
                let now = clock_start.elapsed().as_millis() as u64;
                if now.wrapping_sub(last_activity_ms) >= config.timeout_ms as u64 {
                    write_stdout(b"nc: timeout\n");
                    let _ = client.shutdown(slopos_abi::syscall::SHUT_RDWR);
                    break 'client None;
                }
            }
        };

        if let Some(code) = client_exit {
            return code;
        }

        if !config.keep_listen {
            verbose_msg(config, "exiting (single connection mode)");
            return 0;
        }

        verbose_msg(config, "waiting for next connection");
    }
}
