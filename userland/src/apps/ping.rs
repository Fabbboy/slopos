use core::fmt::Write;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::syscall::{SockAddrIn, UserPollFd, fs, net, process};
use slopos_abi::signal::SIGPIPE;
use slopos_abi::syscall::{LocalFlags, POLLIN};

struct PingConfig {
    count: Option<u32>,
    interval: Duration,
    payload_size: usize,
    timeout_ms: i64,
    verbose: bool,
    host: String,
}

#[derive(Clone, Copy)]
struct PingStats {
    sent: u32,
    received: u32,
    min_rtt_ms: f64,
    max_rtt_ms: f64,
    total_rtt_ms: f64,
}

impl PingStats {
    fn record_rtt(&mut self, rtt_ms: f64) {
        self.received += 1;
        self.min_rtt_ms = self.min_rtt_ms.min(rtt_ms);
        self.max_rtt_ms = self.max_rtt_ms.max(rtt_ms);
        self.total_rtt_ms += rtt_ms;
    }
}

struct WriteBuf {
    buf: [u8; 512],
    pos: usize,
}

impl WriteBuf {
    fn new() -> Self {
        Self {
            buf: [0; 512],
            pos: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.pos]
    }
}

impl core::fmt::Write for WriteBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let avail = self.buf.len().saturating_sub(self.pos);
        let n = bytes.len().min(avail);
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
        Ok(())
    }
}

fn write_stdout(buf: &[u8]) {
    let mut remaining = buf;
    while !remaining.is_empty() {
        match fs::write_slice(1, remaining) {
            Ok(0) => break,
            Ok(n) => remaining = &remaining[n..],
            Err(_) => break,
        }
    }
}

fn writeln_stdout(args: core::fmt::Arguments<'_>) {
    let mut buf = WriteBuf::new();
    let _ = write!(buf, "{}\n", args);
    write_stdout(buf.as_bytes());
}

fn print_usage() {
    write_stdout(b"usage: ping [-c count] [-i interval] [-s size] [-W timeout] [-v] <host>\n");
}

fn parse_ipv4(host: &str) -> Option<[u8; 4]> {
    Some(host.parse::<Ipv4Addr>().ok()?.octets())
}

fn resolve_host(host: &str) -> Option<[u8; 4]> {
    if let Some(ip) = parse_ipv4(host) {
        return Some(ip);
    }
    net::resolve(host.as_bytes())
}

fn parse_u32_arg(value: &str) -> Option<u32> {
    value.parse::<u32>().ok()
}

fn parse_f64_arg(value: &str) -> Option<f64> {
    value.parse::<f64>().ok()
}

fn parse_usize_arg(value: &str) -> Option<usize> {
    value.parse::<usize>().ok()
}

fn parse_args(args: &[String]) -> Result<PingConfig, ()> {
    if args.len() < 2 {
        return Err(());
    }

    let mut count: Option<u32> = None;
    let mut interval_secs = 1.0f64;
    let mut payload_size = 56usize;
    let mut timeout_secs = 5.0f64;
    let mut verbose = false;
    let mut host: Option<String> = None;

    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Err(()),
            "-v" => {
                verbose = true;
                i += 1;
            }
            "-c" => {
                i += 1;
                if i >= args.len() {
                    return Err(());
                }
                let parsed = parse_u32_arg(&args[i]).ok_or(())?;
                if parsed == 0 {
                    return Err(());
                }
                count = Some(parsed);
                i += 1;
            }
            "-i" => {
                i += 1;
                if i >= args.len() {
                    return Err(());
                }
                let parsed = parse_f64_arg(&args[i]).ok_or(())?;
                if parsed <= 0.0 {
                    return Err(());
                }
                interval_secs = parsed;
                i += 1;
            }
            "-s" => {
                i += 1;
                if i >= args.len() {
                    return Err(());
                }
                let parsed = parse_usize_arg(&args[i]).ok_or(())?;
                payload_size = parsed;
                i += 1;
            }
            "-W" => {
                i += 1;
                if i >= args.len() {
                    return Err(());
                }
                let parsed = parse_f64_arg(&args[i]).ok_or(())?;
                if parsed <= 0.0 {
                    return Err(());
                }
                timeout_secs = parsed;
                i += 1;
            }
            value if value.starts_with('-') => return Err(()),
            value => {
                if host.is_some() {
                    return Err(());
                }
                host = Some(value.to_string());
                i += 1;
            }
        }
    }

    let host = host.ok_or(())?;
    Ok(PingConfig {
        count,
        interval: Duration::from_secs_f64(interval_secs),
        payload_size,
        timeout_ms: (timeout_secs * 1000.0) as i64,
        verbose,
        host,
    })
}

const ICMP_HEADER_LEN: usize = 8;
const ICMP_TYPE_ECHO_REQUEST: u8 = 8;
const ICMP_TYPE_ECHO_REPLY: u8 = 0;
const DRAIN_TIMEOUT_MS: i64 = 200;

fn build_icmp_request(
    ident: u16,
    sequence: u16,
    timestamp_ms: u64,
    payload_size: usize,
) -> Vec<u8> {
    let total = ICMP_HEADER_LEN + payload_size;
    let mut buf = vec![0u8; total];

    buf[0] = ICMP_TYPE_ECHO_REQUEST;
    buf[1] = 0;
    buf[2..4].copy_from_slice(&0u16.to_be_bytes());
    buf[4..6].copy_from_slice(&ident.to_be_bytes());
    buf[6..8].copy_from_slice(&sequence.to_be_bytes());

    if payload_size >= 8 {
        buf[ICMP_HEADER_LEN..ICMP_HEADER_LEN + 8].copy_from_slice(&timestamp_ms.to_be_bytes());
    }
    for i in ICMP_HEADER_LEN + 8..total {
        buf[i] = 0x53;
    }
    buf
}

fn parse_icmp_reply(buf: &[u8]) -> Option<(u8, u16, u16, Option<u64>)> {
    if buf.len() < ICMP_HEADER_LEN {
        return None;
    }
    let icmp_type = buf[0];
    let identifier = u16::from_be_bytes([buf[4], buf[5]]);
    let sequence = u16::from_be_bytes([buf[6], buf[7]]);
    let payload = &buf[ICMP_HEADER_LEN..];
    let timestamp = if payload.len() >= 8 {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&payload[..8]);
        Some(u64::from_be_bytes(raw))
    } else {
        None
    };
    Some((icmp_type, identifier, sequence, timestamp))
}

fn send_ping(
    fd: i32,
    target: &SockAddrIn,
    ident: u16,
    sequence: u16,
    clock_start: &Instant,
    payload_size: usize,
    stats: &mut PingStats,
) -> bool {
    let timestamp_ms = clock_start.elapsed().as_millis() as u64;
    let icmp_buf = build_icmp_request(ident, sequence, timestamp_ms, payload_size);
    match net::sendto(fd, &icmp_buf, 0, target) {
        Ok(_) => {
            stats.sent += 1;
            true
        }
        Err(_) => {
            write_stdout(b"ping: sendto failed\n");
            false
        }
    }
}

fn try_receive(fd: i32, clock_start: &Instant, stats: &mut PingStats, verbose: bool) {
    let mut recv_buf = [0u8; 1600];
    let mut src = SockAddrIn::default();
    match net::recvfrom(fd, &mut recv_buf, 0, Some(&mut src)) {
        Ok(received) if received >= ICMP_HEADER_LEN => {
            if let Some((icmp_type, _id, reply_seq, sent_ts)) =
                parse_icmp_reply(&recv_buf[..received])
            {
                if icmp_type != ICMP_TYPE_ECHO_REPLY {
                    return;
                }
                let now_ms = clock_start.elapsed().as_millis() as u64;
                let rtt_ms = match sent_ts {
                    Some(ts) if ts <= now_ms => (now_ms - ts) as f64,
                    _ => 0.0,
                };
                stats.record_rtt(rtt_ms);

                writeln_stdout(format_args!(
                    "{} bytes from {}: icmp_seq={} time={:.3} ms",
                    received,
                    Ipv4Addr::from(src.addr),
                    reply_seq,
                    rtt_ms
                ));
            }
        }
        Ok(_) => {}
        Err(_) => {
            if verbose {
                writeln_stdout(format_args!("ping: recvfrom failed"));
            }
        }
    }
}

fn stdin_has_ctrl_c() -> bool {
    let mut read_buf = [0u8; 32];
    if let Ok(n) = fs::read_slice(0, &mut read_buf) {
        return read_buf[..n].contains(&0x03);
    }
    false
}

pub fn ping_main(args: Vec<String>) -> ! {
    process::ignore_signal(SIGPIPE);

    let config = match parse_args(&args) {
        Ok(cfg) => cfg,
        Err(_) => {
            print_usage();
            std::process::exit(2);
        }
    };

    let target_ip = if let Some(ip) = resolve_host(&config.host) {
        ip
    } else {
        writeln_stdout(format_args!("ping: cannot resolve {}", config.host));
        std::process::exit(2);
    };

    let fd = match net::socket(
        slopos_abi::net::AF_INET,
        slopos_abi::net::SOCK_DGRAM,
        slopos_abi::net::IPPROTO_ICMP,
    ) {
        Ok(fd) => fd,
        Err(_) => {
            write_stdout(b"ping: socket creation failed\n");
            std::process::exit(2);
        }
    };

    let ident = (process::getpid() & 0xFFFF) as u16;
    if let Err(_) = net::bind_addr(fd, [0, 0, 0, 0], ident) {
        write_stdout(b"ping: bind failed\n");
        std::process::exit(2);
    }

    if let Err(_) = net::set_nonblocking(fd) {
        write_stdout(b"ping: failed to set non-blocking\n");
        std::process::exit(2);
    }

    let saved_termios = fs::tcgetattr(0).ok();
    if let Some(ref t) = saved_termios {
        let mut raw = *t;
        raw.c_lflag &= !(LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::ISIG);
        let _ = fs::tcsetattr(0, &raw);
    }

    writeln_stdout(format_args!(
        "PING {} ({}): {} data bytes",
        config.host,
        Ipv4Addr::from(target_ip),
        config.payload_size
    ));

    let target = SockAddrIn {
        family: slopos_abi::net::AF_INET,
        port: 0,
        addr: target_ip,
        _pad: [0; 8],
    };

    let mut stats = PingStats {
        sent: 0,
        received: 0,
        min_rtt_ms: f64::MAX,
        max_rtt_ms: 0.0,
        total_rtt_ms: 0.0,
    };

    let mut sequence: u16 = 0;
    let clock_start = Instant::now();
    let mut stop_requested = false;
    let mut next_send = clock_start;

    loop {
        let now = Instant::now();
        let all_sent = config.count.map_or(false, |limit| stats.sent >= limit);

        if !stop_requested && !all_sent && now >= next_send {
            if !send_ping(
                fd,
                &target,
                ident,
                sequence,
                &clock_start,
                config.payload_size,
                &mut stats,
            ) {
                break;
            }
            sequence = sequence.wrapping_add(1);
            next_send = Instant::now() + config.interval;
        }

        let timeout_ms = if stop_requested {
            DRAIN_TIMEOUT_MS
        } else if all_sent {
            config.timeout_ms
        } else {
            let now = Instant::now();
            match next_send.checked_duration_since(now) {
                Some(remaining) => (remaining.as_millis() as i64).max(1),
                None => 0,
            }
        };

        let (stdin_ready, socket_ready, timed_out) = if stop_requested {
            let mut pfd = [UserPollFd {
                fd,
                events: POLLIN,
                revents: 0,
            }];
            let p = fs::poll(&mut pfd, timeout_ms).unwrap_or(0);
            (false, (pfd[0].revents & POLLIN) != 0, p == 0)
        } else {
            let mut pfds = [
                UserPollFd {
                    fd: 0,
                    events: POLLIN,
                    revents: 0,
                },
                UserPollFd {
                    fd,
                    events: POLLIN,
                    revents: 0,
                },
            ];
            let p = fs::poll(&mut pfds, timeout_ms).unwrap_or(0);
            (
                (pfds[0].revents & POLLIN) != 0,
                (pfds[1].revents & POLLIN) != 0,
                p == 0,
            )
        };

        if socket_ready {
            try_receive(fd, &clock_start, &mut stats, config.verbose);
        }

        if stdin_ready && !stop_requested && stdin_has_ctrl_c() {
            write_stdout(b"^C\n");
            stop_requested = true;
            continue;
        }

        if stop_requested && (timed_out || stats.received >= stats.sent) {
            break;
        }

        if all_sent && (stats.received >= stats.sent || timed_out) {
            break;
        }
    }

    if let Some(ref t) = saved_termios {
        let _ = fs::tcsetattr(0, t);
    }

    let loss = if stats.sent == 0 {
        0
    } else {
        ((stats.sent - stats.received) * 100) / stats.sent
    };

    writeln_stdout(format_args!("--- {} ping statistics ---", config.host));
    writeln_stdout(format_args!(
        "{} packets transmitted, {} received, {}% packet loss",
        stats.sent, stats.received, loss
    ));

    let (min_rtt, avg_rtt, max_rtt) = if stats.received > 0 {
        (
            stats.min_rtt_ms,
            stats.total_rtt_ms / stats.received as f64,
            stats.max_rtt_ms,
        )
    } else {
        (0.0, 0.0, 0.0)
    };
    writeln_stdout(format_args!(
        "rtt min/avg/max = {:.3}/{:.3}/{:.3} ms",
        min_rtt, avg_rtt, max_rtt
    ));

    let code = if stats.received > 0 { 0 } else { 1 };
    std::process::exit(code);
}
