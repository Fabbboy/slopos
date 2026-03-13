//! nc — SlopOS network Swiss army knife (UDP + TCP)
//!
//! Exercises the full socket lifecycle: socket() → bind()/connect() → send/recv → shutdown().
//! Supports UDP client and listen modes with half-duplex I/O, TCP client and
//! listen (with `-k` keep-listening), and defaults to TCP.

pub mod tcp;
pub mod udp;

use core::ffi::c_void;

use crate::syscall::{core::exit_with_code, fs, process, tty};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum NcMode {
    Client,
    Listen,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum NcProtocol {
    Udp,
    Tcp,
}

/// Parsed command-line configuration — built once, never mutated.
struct NcConfig {
    mode: NcMode,
    protocol: NcProtocol,
    remote_addr: [u8; 4],
    remote_port: u16,
    local_port: u16,
    verbose: bool,
    timeout_ms: u32,
    keep_listen: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum NcError {
    MissingHost,
    MissingPort,
    InvalidPort,
    ResolveFailed,
    UnknownFlag,
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn write_out(buf: &[u8]) {
    if fs::write_slice(1, buf).is_err() {
        let _ = tty::write(buf);
    }
}

/// Result of processing a single raw stdin character.
pub(super) enum StdinResult {
    /// No action needed.
    Continue,
    /// Line is ready: send `line_buf[..len]`, then reset `line_pos` to 0.
    SendLine(usize),
}

/// Process one raw stdin byte: echo printable chars, handle backspace, detect Enter.
/// Caller is responsible for performing the actual network send when `SendLine` is returned.
pub(super) fn process_raw_stdin_char(
    c: u8,
    line_buf: &mut [u8; 1024],
    line_pos: &mut usize,
) -> StdinResult {
    match c {
        0x08 | 0x7F => {
            if *line_pos > 0 {
                *line_pos -= 1;
                write_out(b"\x08 \x08");
            }
            StdinResult::Continue
        }
        b'\n' | b'\r' => {
            write_out(b"\n");
            if *line_pos > 0 {
                let send_len = if *line_pos < line_buf.len() {
                    line_buf[*line_pos] = b'\n';
                    *line_pos + 1
                } else {
                    *line_pos
                };
                StdinResult::SendLine(send_len)
            } else {
                StdinResult::Continue
            }
        }
        0x20..=0x7E => {
            if *line_pos < line_buf.len() - 1 {
                line_buf[*line_pos] = c;
                *line_pos += 1;
                write_out(&[c]);
            }
            StdinResult::Continue
        }
        _ => StdinResult::Continue,
    }
}

fn write_u8_dec(mut value: u8, out: &mut [u8], idx: &mut usize) {
    let mut tmp = [0u8; 3];
    let mut n = 0usize;
    loop {
        tmp[n] = b'0' + (value % 10);
        value /= 10;
        n += 1;
        if value == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        if *idx < out.len() {
            out[*idx] = tmp[n];
            *idx += 1;
        }
    }
}

fn write_u16_dec(mut value: u16, out: &mut [u8], idx: &mut usize) {
    let mut tmp = [0u8; 5];
    let mut n = 0usize;
    loop {
        tmp[n] = b'0' + (value % 10) as u8;
        value /= 10;
        n += 1;
        if value == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        if *idx < out.len() {
            out[*idx] = tmp[n];
            *idx += 1;
        }
    }
}

fn write_u32_dec(mut value: u32, out: &mut [u8], idx: &mut usize) {
    let mut tmp = [0u8; 10];
    let mut n = 0usize;
    loop {
        tmp[n] = b'0' + (value % 10) as u8;
        value /= 10;
        n += 1;
        if value == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        if *idx < out.len() {
            out[*idx] = tmp[n];
            *idx += 1;
        }
    }
}

fn write_ipv4(ip: [u8; 4], out: &mut [u8], idx: &mut usize) {
    write_u8_dec(ip[0], out, idx);
    out[*idx] = b'.';
    *idx += 1;
    write_u8_dec(ip[1], out, idx);
    out[*idx] = b'.';
    *idx += 1;
    write_u8_dec(ip[2], out, idx);
    out[*idx] = b'.';
    *idx += 1;
    write_u8_dec(ip[3], out, idx);
}

fn append_bytes(buf: &mut [u8], idx: &mut usize, src: &[u8]) {
    let avail = buf.len() - *idx;
    let len = if src.len() < avail { src.len() } else { avail };
    buf[*idx..*idx + len].copy_from_slice(&src[..len]);
    *idx += len;
}

/// Print a verbose message: `nc: <msg>\n`.  Only emits output when verbose is on.
fn verbose_msg(config: &NcConfig, msg: &[u8]) {
    if !config.verbose {
        return;
    }
    let mut line = [0u8; 256];
    let mut i = 0usize;
    append_bytes(&mut line, &mut i, b"nc: ");
    append_bytes(&mut line, &mut i, msg);
    append_bytes(&mut line, &mut i, b"\n");
    write_out(&line[..i]);
}

/// Print verbose with IP:port: `nc: <prefix> <ip>:<port>\n`
fn verbose_addr(config: &NcConfig, prefix: &[u8], ip: [u8; 4], port: u16) {
    if !config.verbose {
        return;
    }
    let mut line = [0u8; 256];
    let mut i = 0usize;
    append_bytes(&mut line, &mut i, b"nc: ");
    append_bytes(&mut line, &mut i, prefix);
    write_ipv4(ip, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b":");
    write_u16_dec(port, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b"\n");
    write_out(&line[..i]);
}

/// Print verbose with byte count: `nc: <prefix> <count> bytes\n`
fn verbose_bytes(config: &NcConfig, prefix: &[u8], count: usize) {
    if !config.verbose {
        return;
    }
    let mut line = [0u8; 256];
    let mut i = 0usize;
    append_bytes(&mut line, &mut i, b"nc: ");
    append_bytes(&mut line, &mut i, prefix);
    write_u32_dec(count as u32, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b" bytes\n");
    write_out(&line[..i]);
}

/// Print verbose with byte count and source addr:
/// `nc: received <N> bytes from <ip>:<port>\n`
fn verbose_recv(config: &NcConfig, count: usize, ip: [u8; 4], port: u16) {
    if !config.verbose {
        return;
    }
    let mut line = [0u8; 256];
    let mut i = 0usize;
    append_bytes(&mut line, &mut i, b"nc: received ");
    write_u32_dec(count as u32, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b" bytes from ");
    write_ipv4(ip, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b":");
    write_u16_dec(port, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b"\n");
    write_out(&line[..i]);
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

fn print_usage() {
    write_out(b"usage: nc [-ulvk] [-p port] [-w timeout] [host] port\n");
    write_out(b"\n");
    write_out(b"  -u        UDP mode (default is TCP)\n");
    write_out(b"  -l        Listen mode (bind and accept/receive)\n");
    write_out(b"  -v        Verbose output\n");
    write_out(b"  -k        Keep listening after client disconnects (TCP -l only)\n");
    write_out(b"  -p port   Source port (client mode)\n");
    write_out(b"  -w secs   Timeout in seconds\n");
    write_out(b"  host      Remote hostname or IP (client mode)\n");
    write_out(b"  port      Remote port (client) or listen port (listen mode)\n");
}

fn print_error(err: NcError) {
    let msg = match err {
        NcError::MissingHost => b"nc: missing host\n" as &[u8],
        NcError::MissingPort => b"nc: missing port\n" as &[u8],
        NcError::InvalidPort => b"nc: invalid port number\n" as &[u8],
        NcError::ResolveFailed => b"nc: cannot resolve hostname\n" as &[u8],
        NcError::UnknownFlag => b"nc: unknown flag\n" as &[u8],
    };
    write_out(msg);
}

/// Parse a port number from a byte slice.  Returns `None` on invalid input.
fn parse_port(s: &[u8]) -> Option<u16> {
    if s.is_empty() {
        return None;
    }
    let mut val: u32 = 0;
    for &b in s {
        if b < b'0' || b > b'9' {
            return None;
        }
        val = val * 10 + (b - b'0') as u32;
        if val > 65535 {
            return None;
        }
    }
    if val == 0 {
        return None;
    }
    Some(val as u16)
}

/// Parse a dotted-quad IPv4 address (e.g. "10.0.2.2").
fn parse_ipv4(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut octet_idx = 0usize;
    let mut current: u16 = 0;
    let mut has_digit = false;

    for &b in s {
        if b == b'.' {
            if !has_digit || octet_idx >= 3 {
                return None;
            }
            if current > 255 {
                return None;
            }
            octets[octet_idx] = current as u8;
            octet_idx += 1;
            current = 0;
            has_digit = false;
        } else if b >= b'0' && b <= b'9' {
            current = current * 10 + (b - b'0') as u16;
            has_digit = true;
        } else {
            return None;
        }
    }

    if !has_digit || octet_idx != 3 || current > 255 {
        return None;
    }
    octets[3] = current as u8;
    Some(octets)
}

/// Resolve a host argument: try dotted-quad first, then kernel DNS.
fn resolve_host(host: &[u8]) -> Result<[u8; 4], NcError> {
    if let Some(ip) = parse_ipv4(host) {
        return Ok(ip);
    }
    // Try kernel DNS resolution
    match crate::syscall::net::resolve(host) {
        Some(ip) => Ok(ip),
        None => Err(NcError::ResolveFailed),
    }
}

/// Compare two byte slices for equality.
fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Core argument parsing logic operating on clean Rust slices.
///
/// The first element (`args[0]`) is the program name and is skipped.
/// This function is separated from the raw-pointer entry so it can be
/// tested without constructing C-style argv arrays.
fn parse_args_from_slices(args: &[&[u8]]) -> Result<NcConfig, NcError> {
    let mut udp = false;
    let mut listen = false;
    let mut verbose = false;
    let mut keep_listen = false;
    let mut local_port: u16 = 0;
    let mut timeout_secs: u32 = 0;
    let mut positional: [&[u8]; 2] = [&[], &[]];
    let mut pos_count = 0usize;

    let mut i = 1usize; // skip argv[0]
    while i < args.len() {
        let arg = args[i];

        if arg.is_empty() {
            i += 1;
            continue;
        }

        if arg[0] == b'-' {
            // Flag processing — may contain bundled flags like -ulvk
            if bytes_eq(arg, b"-h") || bytes_eq(arg, b"--help") {
                print_usage();
                exit_with_code(0);
            }

            if bytes_eq(arg, b"-p") {
                // Next arg is port number
                i += 1;
                if i >= args.len() {
                    return Err(NcError::MissingPort);
                }
                local_port = parse_port(args[i]).ok_or(NcError::InvalidPort)?;
                i += 1;
                continue;
            }

            if bytes_eq(arg, b"-w") {
                // Next arg is timeout in seconds
                i += 1;
                if i >= args.len() {
                    return Err(NcError::InvalidPort); // reuse error for missing value
                }
                // Parse timeout as u32
                let mut t: u32 = 0;
                for &b in args[i] {
                    if b < b'0' || b > b'9' {
                        return Err(NcError::InvalidPort);
                    }
                    t = t * 10 + (b - b'0') as u32;
                }
                timeout_secs = t;
                i += 1;
                continue;
            }

            // Process bundled flags: -ulvk
            let mut j = 1usize;
            while j < arg.len() {
                match arg[j] {
                    b'u' => udp = true,
                    b'l' => listen = true,
                    b'v' => verbose = true,
                    b'k' => keep_listen = true,
                    _ => return Err(NcError::UnknownFlag),
                }
                j += 1;
            }
        } else {
            // Positional argument
            if pos_count < 2 {
                positional[pos_count] = arg;
                pos_count += 1;
            }
        }

        i += 1;
    }

    // TCP is the default; -u switches to UDP
    let protocol = if udp {
        NcProtocol::Udp
    } else {
        NcProtocol::Tcp
    };

    let mode = if listen {
        NcMode::Listen
    } else {
        NcMode::Client
    };

    match mode {
        NcMode::Listen => {
            // Listen mode: expect exactly one positional arg (port)
            if pos_count == 0 {
                return Err(NcError::MissingPort);
            }
            let port = parse_port(positional[0]).ok_or(NcError::InvalidPort)?;
            Ok(NcConfig {
                mode,
                protocol,
                remote_addr: [0; 4],
                remote_port: 0,
                local_port: port,
                verbose,
                timeout_ms: timeout_secs * 1000,
                keep_listen,
            })
        }
        NcMode::Client => {
            // Client mode: expect host + port
            if pos_count < 1 {
                return Err(NcError::MissingHost);
            }
            if pos_count < 2 {
                return Err(NcError::MissingPort);
            }
            let addr = resolve_host(positional[0])?;
            let port = parse_port(positional[1]).ok_or(NcError::InvalidPort)?;
            Ok(NcConfig {
                mode,
                protocol,
                remote_addr: addr,
                remote_port: port,
                local_port,
                verbose,
                timeout_ms: timeout_secs * 1000,
                keep_listen,
            })
        }
    }
}

/// Parse argv into an NcConfig.
///
/// Converts raw C-style argv pointers from the kernel into Rust slices,
/// then delegates to [`parse_args_from_slices`] for the actual parsing logic.
fn parse_args(argc: usize, argv: *const *const u8) -> Result<NcConfig, NcError> {
    // Stack-allocated scratch space — 16 args is more than enough for nc.
    let mut args: [&[u8]; 16] = [&[]; 16];
    let count = if argc > 16 { 16 } else { argc };

    for idx in 0..count {
        let ptr = unsafe { *argv.add(idx) };
        if ptr.is_null() {
            args[idx] = &[];
        } else {
            let len = crate::runtime::u_strlen(ptr);
            args[idx] = unsafe { core::slice::from_raw_parts(ptr, len) };
        }
    }

    parse_args_from_slices(&args[..count])
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Entry point when launched with argc/argv extracted from the user stack.
/// This is the primary entry for fork+execve launches from the shell.
pub fn nc_main_args(argc: usize, argv: *const *const u8) -> ! {
    if argc <= 1 || argv.is_null() {
        print_usage();
        exit_with_code(1);
    }

    let config = match parse_args(argc, argv) {
        Ok(c) => c,
        Err(e) => {
            print_error(e);
            print_usage();
            exit_with_code(1);
        }
    };

    // Disable kernel echo, canonical mode, AND signal generation — nc handles
    // its own line editing and Ctrl+C detection.  Also ignore SIGPIPE so that
    // a broken TCP connection returns an error instead of killing the process.
    process::ignore_signal(slopos_abi::signal::SIGPIPE);
    let saved_termios = fs::tcgetattr(0).ok();
    if let Some(ref t) = saved_termios {
        let mut raw = *t;
        raw.c_lflag &=
            !(slopos_abi::syscall::ECHO | slopos_abi::syscall::ICANON | slopos_abi::syscall::ISIG);
        let _ = fs::tcsetattr(0, &raw);
    }

    let exit_code = match (config.protocol, config.mode) {
        (NcProtocol::Udp, NcMode::Client) => udp::udp_client(&config),
        (NcProtocol::Udp, NcMode::Listen) => udp::udp_listen(&config),
        (NcProtocol::Tcp, NcMode::Client) => tcp::tcp_client(&config),
        (NcProtocol::Tcp, NcMode::Listen) => tcp::tcp_listen(&config),
    };

    // Restore termios before exiting.
    if let Some(ref t) = saved_termios {
        let _ = fs::tcsetattr(0, t);
    }

    exit_with_code(exit_code as i32);
}

/// Legacy entry point for the standard entry! macro (no args).
/// Prints usage and exits since nc requires arguments.
pub fn nc_main(_arg: *mut c_void) -> ! {
    print_usage();
    exit_with_code(1);
}

// ---------------------------------------------------------------------------
// Tests (argument parsing & helpers — no kernel needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helper function tests (carried forward)
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_port_valid() {
        assert_eq!(parse_port(b"80"), Some(80));
        assert_eq!(parse_port(b"443"), Some(443));
        assert_eq!(parse_port(b"65535"), Some(65535));
        assert_eq!(parse_port(b"1"), Some(1));
        assert_eq!(parse_port(b"12345"), Some(12345));
    }

    #[test]
    fn test_parse_port_invalid() {
        assert_eq!(parse_port(b""), None);
        assert_eq!(parse_port(b"0"), None);
        assert_eq!(parse_port(b"65536"), None);
        assert_eq!(parse_port(b"abc"), None);
        assert_eq!(parse_port(b"12a"), None);
        assert_eq!(parse_port(b"99999"), None);
    }

    #[test]
    fn test_parse_ipv4_valid() {
        assert_eq!(parse_ipv4(b"10.0.2.2"), Some([10, 0, 2, 2]));
        assert_eq!(parse_ipv4(b"192.168.1.1"), Some([192, 168, 1, 1]));
        assert_eq!(parse_ipv4(b"0.0.0.0"), Some([0, 0, 0, 0]));
        assert_eq!(parse_ipv4(b"255.255.255.255"), Some([255, 255, 255, 255]));
        assert_eq!(parse_ipv4(b"127.0.0.1"), Some([127, 0, 0, 1]));
    }

    #[test]
    fn test_parse_ipv4_invalid() {
        assert_eq!(parse_ipv4(b""), None);
        assert_eq!(parse_ipv4(b"10.0.2"), None);
        assert_eq!(parse_ipv4(b"10.0.2.2.1"), None);
        assert_eq!(parse_ipv4(b"256.0.0.1"), None);
        assert_eq!(parse_ipv4(b"10.0.2.abc"), None);
        assert_eq!(parse_ipv4(b"..."), None);
        assert_eq!(parse_ipv4(b"1.2.3."), None);
        assert_eq!(parse_ipv4(b".1.2.3"), None);
    }

    #[test]
    fn test_write_u8_dec() {
        let mut buf = [0u8; 8];
        let mut idx = 0;
        write_u8_dec(0, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"0");

        idx = 0;
        write_u8_dec(255, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"255");

        idx = 0;
        write_u8_dec(42, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"42");
    }

    #[test]
    fn test_write_u16_dec() {
        let mut buf = [0u8; 8];
        let mut idx = 0;
        write_u16_dec(0, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"0");

        idx = 0;
        write_u16_dec(65535, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"65535");

        idx = 0;
        write_u16_dec(8080, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"8080");
    }

    #[test]
    fn test_write_u32_dec() {
        let mut buf = [0u8; 16];
        let mut idx = 0;
        write_u32_dec(0, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"0");

        idx = 0;
        write_u32_dec(1000, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"1000");

        idx = 0;
        write_u32_dec(4294967295, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"4294967295");
    }

    #[test]
    fn test_write_ipv4() {
        let mut buf = [0u8; 20];
        let mut idx = 0;
        write_ipv4([10, 0, 2, 2], &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"10.0.2.2");

        idx = 0;
        write_ipv4([192, 168, 1, 1], &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"192.168.1.1");

        idx = 0;
        write_ipv4([255, 255, 255, 255], &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"255.255.255.255");
    }

    #[test]
    fn test_bytes_eq() {
        assert!(bytes_eq(b"hello", b"hello"));
        assert!(!bytes_eq(b"hello", b"world"));
        assert!(!bytes_eq(b"hi", b"hello"));
        assert!(bytes_eq(b"", b""));
    }

    #[test]
    fn test_append_bytes() {
        let mut buf = [0u8; 16];
        let mut idx = 0;
        append_bytes(&mut buf, &mut idx, b"hello");
        assert_eq!(&buf[..idx], b"hello");
        append_bytes(&mut buf, &mut idx, b" world");
        assert_eq!(&buf[..idx], b"hello world");
    }

    #[test]
    fn test_append_bytes_overflow() {
        let mut buf = [0u8; 5];
        let mut idx = 0;
        append_bytes(&mut buf, &mut idx, b"hello world");
        assert_eq!(idx, 5);
        assert_eq!(&buf[..], b"hello");
    }

    // -----------------------------------------------------------------------
    // Argument parsing tests 
    // -----------------------------------------------------------------------
    //
    // These test `parse_args_from_slices` which takes clean `&[&[u8]]` slices
    // instead of raw C pointers, making them unit-testable.  The tests serve
    // as regression documentation; they can be extracted to a host-side test
    // crate if needed (the no_std kernel target cannot run them directly).

    #[test]
    fn test_tcp_is_default_protocol() {
        // `nc host port` without -u should default to TCP
        let args: &[&[u8]] = &[b"nc", b"10.0.2.2", b"80"];
        let config = parse_args_from_slices(args).unwrap();
        assert_eq!(config.protocol, NcProtocol::Tcp);
        assert_eq!(config.mode, NcMode::Client);
        assert_eq!(config.remote_addr, [10, 0, 2, 2]);
        assert_eq!(config.remote_port, 80);
    }

    #[test]
    fn test_udp_with_u_flag() {
        let args: &[&[u8]] = &[b"nc", b"-u", b"10.0.2.2", b"80"];
        let config = parse_args_from_slices(args).unwrap();
        assert_eq!(config.protocol, NcProtocol::Udp);
        assert_eq!(config.mode, NcMode::Client);
    }

    #[test]
    fn test_tcp_listen_mode() {
        let args: &[&[u8]] = &[b"nc", b"-l", b"8080"];
        let config = parse_args_from_slices(args).unwrap();
        assert_eq!(config.protocol, NcProtocol::Tcp);
        assert_eq!(config.mode, NcMode::Listen);
        assert_eq!(config.local_port, 8080);
        assert!(!config.keep_listen);
    }

    #[test]
    fn test_udp_listen_mode() {
        let args: &[&[u8]] = &[b"nc", b"-ul", b"12345"];
        let config = parse_args_from_slices(args).unwrap();
        assert_eq!(config.protocol, NcProtocol::Udp);
        assert_eq!(config.mode, NcMode::Listen);
        assert_eq!(config.local_port, 12345);
    }

    #[test]
    fn test_keep_listen_flag() {
        let args: &[&[u8]] = &[b"nc", b"-l", b"-k", b"8080"];
        let config = parse_args_from_slices(args).unwrap();
        assert!(config.keep_listen);
        assert_eq!(config.mode, NcMode::Listen);
        assert_eq!(config.protocol, NcProtocol::Tcp);
    }

    #[test]
    fn test_keep_listen_bundled() {
        let args: &[&[u8]] = &[b"nc", b"-lk", b"8080"];
        let config = parse_args_from_slices(args).unwrap();
        assert!(config.keep_listen);
        assert_eq!(config.mode, NcMode::Listen);
    }

    #[test]
    fn test_all_flags_bundled() {
        let args: &[&[u8]] = &[b"nc", b"-lvk", b"8080"];
        let config = parse_args_from_slices(args).unwrap();
        assert!(config.verbose);
        assert!(config.keep_listen);
        assert_eq!(config.mode, NcMode::Listen);
        assert_eq!(config.protocol, NcProtocol::Tcp);
    }

    #[test]
    fn test_verbose_tcp_client() {
        let args: &[&[u8]] = &[b"nc", b"-v", b"192.168.1.1", b"443"];
        let config = parse_args_from_slices(args).unwrap();
        assert!(config.verbose);
        assert_eq!(config.protocol, NcProtocol::Tcp);
        assert_eq!(config.remote_addr, [192, 168, 1, 1]);
        assert_eq!(config.remote_port, 443);
    }

    #[test]
    fn test_timeout_flag() {
        let args: &[&[u8]] = &[b"nc", b"-w", b"5", b"10.0.2.2", b"80"];
        let config = parse_args_from_slices(args).unwrap();
        assert_eq!(config.timeout_ms, 5000);
    }

    #[test]
    fn test_source_port_flag() {
        let args: &[&[u8]] = &[b"nc", b"-p", b"54321", b"10.0.2.2", b"80"];
        let config = parse_args_from_slices(args).unwrap();
        assert_eq!(config.local_port, 54321);
    }

    #[test]
    fn test_combined_flags_separate() {
        // -v -u -l -k -p 1234 -w 10 8080
        let args: &[&[u8]] = &[
            b"nc", b"-v", b"-u", b"-l", b"-k", b"-p", b"1234", b"-w", b"10", b"8080",
        ];
        let config = parse_args_from_slices(args).unwrap();
        assert!(config.verbose);
        assert!(config.keep_listen);
        assert_eq!(config.protocol, NcProtocol::Udp);
        assert_eq!(config.mode, NcMode::Listen);
        assert_eq!(config.local_port, 8080);
        assert_eq!(config.timeout_ms, 10_000);
    }

    #[test]
    fn test_error_missing_host() {
        // Client mode with no positional args
        let args: &[&[u8]] = &[b"nc"];
        let err = parse_args_from_slices(args).unwrap_err();
        assert_eq!(err, NcError::MissingHost);
    }

    #[test]
    fn test_error_missing_port_client() {
        // Client mode with host but no port
        let args: &[&[u8]] = &[b"nc", b"10.0.2.2"];
        let err = parse_args_from_slices(args).unwrap_err();
        assert_eq!(err, NcError::MissingPort);
    }

    #[test]
    fn test_error_missing_port_listen() {
        // Listen mode with no port
        let args: &[&[u8]] = &[b"nc", b"-l"];
        let err = parse_args_from_slices(args).unwrap_err();
        assert_eq!(err, NcError::MissingPort);
    }

    #[test]
    fn test_error_invalid_port() {
        let args: &[&[u8]] = &[b"nc", b"-l", b"abc"];
        let err = parse_args_from_slices(args).unwrap_err();
        assert_eq!(err, NcError::InvalidPort);
    }

    #[test]
    fn test_error_unknown_flag() {
        let args: &[&[u8]] = &[b"nc", b"-x", b"10.0.2.2", b"80"];
        let err = parse_args_from_slices(args).unwrap_err();
        assert_eq!(err, NcError::UnknownFlag);
    }

    #[test]
    fn test_error_port_out_of_range() {
        let args: &[&[u8]] = &[b"nc", b"-l", b"99999"];
        let err = parse_args_from_slices(args).unwrap_err();
        assert_eq!(err, NcError::InvalidPort);
    }

    #[test]
    fn test_error_port_zero() {
        let args: &[&[u8]] = &[b"nc", b"-l", b"0"];
        let err = parse_args_from_slices(args).unwrap_err();
        assert_eq!(err, NcError::InvalidPort);
    }

    #[test]
    fn test_keep_listen_default_false() {
        let args: &[&[u8]] = &[b"nc", b"10.0.2.2", b"80"];
        let config = parse_args_from_slices(args).unwrap();
        assert!(!config.keep_listen);
    }

    #[test]
    fn test_keep_listen_in_client_mode_silently_accepted() {
        // -k without -l is silently accepted (like BSD nc)
        let args: &[&[u8]] = &[b"nc", b"-k", b"10.0.2.2", b"80"];
        let config = parse_args_from_slices(args).unwrap();
        assert!(config.keep_listen);
        assert_eq!(config.mode, NcMode::Client);
    }

    #[test]
    fn test_missing_p_value() {
        let args: &[&[u8]] = &[b"nc", b"-p"];
        let err = parse_args_from_slices(args).unwrap_err();
        assert_eq!(err, NcError::MissingPort);
    }

    #[test]
    fn test_missing_w_value() {
        let args: &[&[u8]] = &[b"nc", b"-w"];
        let err = parse_args_from_slices(args).unwrap_err();
        // Reuses InvalidPort for missing -w value
        assert_eq!(err, NcError::InvalidPort);
    }
}
