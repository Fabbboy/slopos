use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::ring::{Ring, slopfut};
use crate::syscall::{SockAddrIn, fs, net, process};
use slopos_abi::signal::SIGPIPE;
use slopos_abi::syscall::LocalFlags;

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

fn print_usage() {
    println!("usage: ping [-c count] [-i interval] [-s size] [-W timeout] [-v] <host>");
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

/// Pure computation of the next `select` deadline (ms) from the loop's
/// three timing cases. Extracted so the state machine is host-testable
/// without a socket or clock:
///   - draining (Ctrl-C pressed): fixed `DRAIN_TIMEOUT_MS`;
///   - all packets sent: the per-reply timeout;
///   - still sending: time remaining until the next send (>= 1ms, or 0 if
///     the send is already due).
///
/// `remaining_to_next_send_ms` is `None` when `next_send` is already in the
/// past (send due now), `Some(ms)` otherwise.
fn compute_timeout_ms(
    stop_requested: bool,
    all_sent: bool,
    reply_timeout_ms: i64,
    remaining_to_next_send_ms: Option<i64>,
) -> i64 {
    if stop_requested {
        DRAIN_TIMEOUT_MS
    } else if all_sent {
        reply_timeout_ms
    } else {
        match remaining_to_next_send_ms {
            Some(remaining) => remaining.max(1),
            None => 0,
        }
    }
}

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
            eprintln!("ping: sendto failed");
            false
        }
    }
}

/// Process a datagram the reactor already received: the source address
/// rides in `src` (from `RecvFromResult.src` — no second syscall), and the
/// data region is `data`. Records the RTT and prints the reply line.
fn handle_reply(data: &[u8], src: &SockAddrIn, clock_start: &Instant, stats: &mut PingStats) {
    let received = data.len();
    if received < ICMP_HEADER_LEN {
        return;
    }
    if let Some((icmp_type, _id, reply_seq, sent_ts)) = parse_icmp_reply(data) {
        if icmp_type != ICMP_TYPE_ECHO_REPLY {
            return;
        }
        let now_ms = clock_start.elapsed().as_millis() as u64;
        let rtt_ms = match sent_ts {
            Some(ts) if ts <= now_ms => (now_ms - ts) as f64,
            _ => 0.0,
        };
        stats.record_rtt(rtt_ms);

        println!(
            "{} bytes from {}: icmp_seq={} time={:.3} ms",
            received,
            Ipv4Addr::from(src.addr),
            reply_seq,
            rtt_ms
        );
    }
}

/// `true` if a stdin read burst contained Ctrl-C (0x03).
fn burst_has_ctrl_c(buf: &[u8]) -> bool {
    buf.contains(&0x03)
}

/// socket recv buffer capacity (max ICMP reply we care about).
const RECV_CAP: usize = 1600;
/// stdin read buffer capacity (one keystroke burst; we only scan for ^C).
const STDIN_CAP: usize = 32;

/// The async send/receive loop. Races stdin / `OP_RECVFROM` / timer, acts
/// on whichever fires, and breaks on the same termination conditions as the
/// old `poll`-driven loop (Ctrl-C drain done, all sent + drained/timed out).
async fn run_loop(
    config: &PingConfig,
    sock_fd: i32,
    target: &SockAddrIn,
    ident: u16,
    clock_start: &Instant,
    stats: &mut PingStats,
) {
    type DynBuf = core::pin::Pin<Box<dyn core::future::Future<Output = slopfut::BufResult>>>;

    let mut sequence: u16 = 0;
    let mut stop_requested = false;
    let mut stdin_open = true;
    let mut next_send = *clock_start;

    // Buffers ping-pong between the loop and the in-flight reads, mirroring
    // the nc template: the winner returns its buffer; a cancelled loser
    // keeps its buffer in the reactor, so the loser gets a fresh one.
    let mut stdin_buf = vec![0u8; STDIN_CAP];
    let mut recv_buf = vec![0u8; RECV_CAP];

    loop {
        let now = Instant::now();
        let all_sent = config.count.map_or(false, |limit| stats.sent >= limit);

        if !stop_requested && !all_sent && now >= next_send {
            if !send_ping(
                sock_fd,
                target,
                ident,
                sequence,
                clock_start,
                config.payload_size,
                stats,
            ) {
                break;
            }
            sequence = sequence.wrapping_add(1);
            next_send = Instant::now() + config.interval;
        }

        let remaining_to_next_send_ms = next_send
            .checked_duration_since(Instant::now())
            .map(|d| d.as_millis() as i64);
        let timeout_ms = compute_timeout_ms(
            stop_requested,
            all_sent,
            config.timeout_ms,
            remaining_to_next_send_ms,
        );

        // `Either`-style winner discrimination: A = stdin, B = recvfrom,
        // C/B = timer (depending on whether stdin is in the race).
        let mut timed_out = false;
        if stop_requested || !stdin_open {
            // No stdin branch: either draining after Ctrl-C, or stdin has
            // closed — only the socket + timer race.
            let f_recv =
                slopfut::recvfrom(sock_fd, core::mem::take(&mut recv_buf), RECV_CAP as u32);
            let f_timer = slopfut::time::sleep_ms(timeout_ms.max(0) as u64);
            match slopfut::select2(f_recv, Box::pin(f_timer)).await {
                slopfut::Either2::A(rf) => {
                    if rf.res < 0 {
                        if config.verbose {
                            eprintln!("ping: recvfrom failed");
                        }
                        break;
                    }
                    let received = (rf.res as usize).min(rf.buf.len());
                    handle_reply(&rf.buf[..received], &rf.src, clock_start, stats);
                    recv_buf = rf.buf;
                }
                slopfut::Either2::B(_) => {
                    recv_buf = vec![0u8; RECV_CAP];
                    timed_out = true;
                }
            }
        } else {
            let f_stdin: DynBuf = Box::pin(slopfut::read(
                0,
                core::mem::take(&mut stdin_buf),
                STDIN_CAP as u32,
            ));
            let f_recv =
                slopfut::recvfrom(sock_fd, core::mem::take(&mut recv_buf), RECV_CAP as u32);
            let f_timer = Box::pin(slopfut::time::sleep_ms(timeout_ms.max(0) as u64));
            match slopfut::select3(f_stdin, f_recv, f_timer).await {
                slopfut::Either3::A(br) => {
                    recv_buf = vec![0u8; RECV_CAP];
                    if br.res <= 0 {
                        // stdin EOF/error: stop racing it (don't busy re-arm a
                        // read that completes immediately every turn).
                        stdin_open = false;
                        stdin_buf = br.buf;
                        continue;
                    }
                    let n = (br.res as usize).min(br.buf.len());
                    if burst_has_ctrl_c(&br.buf[..n]) {
                        print!("^C\n");
                        stop_requested = true;
                    }
                    stdin_buf = br.buf;
                    continue;
                }
                slopfut::Either3::B(rf) => {
                    stdin_buf = vec![0u8; STDIN_CAP];
                    if rf.res < 0 {
                        if config.verbose {
                            eprintln!("ping: recvfrom failed");
                        }
                        break;
                    }
                    let received = (rf.res as usize).min(rf.buf.len());
                    handle_reply(&rf.buf[..received], &rf.src, clock_start, stats);
                    recv_buf = rf.buf;
                }
                slopfut::Either3::C(_) => {
                    stdin_buf = vec![0u8; STDIN_CAP];
                    recv_buf = vec![0u8; RECV_CAP];
                    timed_out = true;
                }
            }
        }

        if stop_requested && (timed_out || stats.received >= stats.sent) {
            break;
        }

        if all_sent && (stats.received >= stats.sent || timed_out) {
            break;
        }
    }
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

    let target_ip = match crate::net::resolve_host_raw(&config.host) {
        Ok(ip) => ip,
        Err(err) => {
            eprintln!("ping: {}: {}", config.host, err);
            std::process::exit(2);
        }
    };

    let fd = match net::socket(
        slopos_abi::net::AF_INET,
        slopos_abi::net::SOCK_DGRAM,
        slopos_abi::net::IPPROTO_ICMP,
    ) {
        Ok(fd) => fd,
        Err(_) => {
            eprintln!("ping: socket creation failed");
            std::process::exit(2);
        }
    };

    let ident = (process::getpid() & 0xFFFF) as u16;
    if let Err(_) = net::bind_addr(fd.raw(), [0, 0, 0, 0], ident) {
        eprintln!("ping: bind failed");
        std::process::exit(2);
    }

    if let Err(_) = net::set_nonblocking(fd.raw()) {
        eprintln!("ping: failed to set non-blocking");
        std::process::exit(2);
    }

    let saved_termios = fs::tcgetattr(0).ok();
    if let Some(ref t) = saved_termios {
        let mut raw = *t;
        raw.c_lflag &= !(LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::ISIG);
        let _ = fs::tcsetattr(0, &raw);
    }

    println!(
        "PING {} ({}): {} data bytes",
        config.host,
        Ipv4Addr::from(target_ip),
        config.payload_size
    );

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

    let clock_start = Instant::now();

    // The send/receive loop now rides the slopfut runtime: the dual
    // `poll([stdin, socket], t)` is replaced by `select3` over a stdin
    // `OP_READ`, an `OP_RECVFROM` (whose result carries the ICMP reply's
    // source address — no second recvfrom syscall), and an `OP_TIMEOUT`
    // bounding the wait to the next send / reply deadline. Once Ctrl-C is
    // pressed (drain state) the stdin branch is dropped via `select2`.
    let sock_fd = fd.raw();
    // 16 SQ slots comfortably covers the loop's peak in-flight (stdin read +
    // recvfrom + timer); ring setup failure means no loop at all.
    match Ring::setup(16) {
        Ok(ring) => {
            slopfut::block_on(
                ring,
                run_loop(&config, sock_fd, &target, ident, &clock_start, &mut stats),
            );
        }
        Err(_) => {
            eprintln!("ping: ring setup failed");
            if let Some(ref t) = saved_termios {
                let _ = fs::tcsetattr(0, t);
            }
            std::process::exit(2);
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

    println!("--- {} ping statistics ---", config.host);
    println!(
        "{} packets transmitted, {} received, {}% packet loss",
        stats.sent, stats.received, loss
    );

    let (min_rtt, avg_rtt, max_rtt) = if stats.received > 0 {
        (
            stats.min_rtt_ms,
            stats.total_rtt_ms / stats.received as f64,
            stats.max_rtt_ms,
        )
    } else {
        (0.0, 0.0, 0.0)
    };
    println!(
        "rtt min/avg/max = {:.3}/{:.3}/{:.3} ms",
        min_rtt, avg_rtt, max_rtt
    );

    let code = if stats.received > 0 { 0 } else { 1 };
    std::process::exit(code);
}
