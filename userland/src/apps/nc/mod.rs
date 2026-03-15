//! nc -- SlopOS network Swiss army knife (UDP + TCP)
//!
//! Exercises the full socket lifecycle: socket() -> bind()/connect() -> send/recv -> shutdown().
//! Supports UDP client and listen modes with half-duplex I/O, TCP client and
//! listen (with `-k` keep-listening), and defaults to TCP.

pub mod tcp;
pub mod udp;

use std::net::Ipv4Addr;

use crate::syscall::{fs, process};

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

/// Parsed command-line configuration -- built once, never mutated.
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

pub(super) fn write_stdout(buf: &[u8]) {
    let mut remaining = buf;
    while !remaining.is_empty() {
        match fs::write_slice(1, remaining) {
            Ok(0) => break,
            Ok(n) => remaining = &remaining[n..],
            Err(_) => break,
        }
    }
}

/// Formatted write to stdout with newline, via raw fd 1 syscall.
/// Avoids std::io::Stdout buffering which doesn't flush through pipes.
pub(super) fn writeln_stdout(args: core::fmt::Arguments<'_>) {
    use core::fmt::Write;
    let mut buf = WriteBuf::new();
    let _ = write!(buf, "{}\n", args);
    write_stdout(buf.as_bytes());
}

struct WriteBuf {
    buf: [u8; 256],
    pos: usize,
}

impl WriteBuf {
    fn new() -> Self {
        Self {
            buf: [0u8; 256],
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
        let avail = self.buf.len() - self.pos;
        let n = bytes.len().min(avail);
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Stdin processing (raw mode)
// ---------------------------------------------------------------------------

/// Result of processing a single raw stdin character.
pub(super) enum StdinResult {
    /// No action needed.
    Continue,
    /// Line is ready: send `line_buf[..len]`, then reset `line_pos` to 0.
    SendLine(usize),
    /// User requested exit (Ctrl+C or Ctrl+D).
    Quit,
}

/// Process one raw stdin byte: echo printable chars, handle backspace, detect
/// Enter and Ctrl+C/D.  Caller is responsible for performing the actual network
/// send when `SendLine` is returned and cleanup on `Quit`.
pub(super) fn process_raw_stdin_char(
    c: u8,
    line_buf: &mut [u8; 1024],
    line_pos: &mut usize,
) -> StdinResult {
    match c {
        // Ctrl+C / Ctrl+D -> quit
        0x03 => {
            write_stdout(b"^C\n");
            StdinResult::Quit
        }
        0x04 => StdinResult::Quit,
        // Backspace / DEL
        0x08 | 0x7F => {
            if *line_pos > 0 {
                *line_pos -= 1;
                write_stdout(b"\x08 \x08");
            }
            StdinResult::Continue
        }
        // Enter
        b'\n' | b'\r' => {
            write_stdout(b"\n");
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
        // Printable ASCII
        0x20..=0x7E => {
            if *line_pos < line_buf.len() - 1 {
                line_buf[*line_pos] = c;
                *line_pos += 1;
                write_stdout(&[c]);
            }
            StdinResult::Continue
        }
        _ => StdinResult::Continue,
    }
}

// ---------------------------------------------------------------------------
// Verbose output helpers (stdout so the shell can capture them)
// ---------------------------------------------------------------------------

/// Print a verbose message: `nc: <msg>\n`.  Only emits output when verbose is on.
fn verbose_msg(config: &NcConfig, msg: &str) {
    if !config.verbose {
        return;
    }
    writeln_stdout(format_args!("nc: {}", msg));
}

fn verbose_addr(config: &NcConfig, prefix: &str, ip: [u8; 4], port: u16) {
    if !config.verbose {
        return;
    }
    writeln_stdout(format_args!(
        "nc: {}{}:{}",
        prefix,
        Ipv4Addr::from(ip),
        port
    ));
}

fn verbose_bytes(config: &NcConfig, prefix: &str, count: usize) {
    if !config.verbose {
        return;
    }
    writeln_stdout(format_args!("nc: {}{} bytes", prefix, count));
}

fn verbose_recv(config: &NcConfig, count: usize, ip: [u8; 4], port: u16) {
    if !config.verbose {
        return;
    }
    writeln_stdout(format_args!(
        "nc: received {} bytes from {}:{}",
        count,
        Ipv4Addr::from(ip),
        port
    ));
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

fn print_usage() {
    write_stdout(b"usage: nc [-ulvk] [-p port] [-w timeout] [host] port\n");
    write_stdout(b"\n");
    write_stdout(b"  -u        UDP mode (default is TCP)\n");
    write_stdout(b"  -l        Listen mode (bind and accept/receive)\n");
    write_stdout(b"  -v        Verbose output\n");
    write_stdout(b"  -k        Keep listening after client disconnects (TCP -l only)\n");
    write_stdout(b"  -p port   Source port (client mode)\n");
    write_stdout(b"  -w secs   Timeout in seconds\n");
    write_stdout(b"  host      Remote hostname or IP (client mode)\n");
    write_stdout(b"  port      Remote port (client) or listen port (listen mode)\n");
}

fn print_error(err: NcError) {
    let msg = match err {
        NcError::MissingHost => "nc: missing host",
        NcError::MissingPort => "nc: missing port",
        NcError::InvalidPort => "nc: invalid port number",
        NcError::ResolveFailed => "nc: cannot resolve hostname",
        NcError::UnknownFlag => "nc: unknown flag",
    };
    writeln_stdout(format_args!("{}", msg));
}

fn parse_port(s: &str) -> Option<u16> {
    let parsed = s.parse::<u16>().ok()?;
    if parsed == 0 {
        return None;
    }
    Some(parsed)
}

/// Parse a dotted-quad IPv4 address (e.g. "10.0.2.2").
fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    Some(s.parse::<Ipv4Addr>().ok()?.octets())
}

/// Resolve a host argument: try dotted-quad first, then kernel DNS.
fn resolve_host(host: &[u8]) -> Result<[u8; 4], NcError> {
    if let Ok(host_str) = core::str::from_utf8(host)
        && let Some(ip) = parse_ipv4(host_str)
    {
        return Ok(ip);
    }
    // Try kernel DNS resolution
    match crate::syscall::net::resolve(host) {
        Some(ip) => Ok(ip),
        _ => Err(NcError::ResolveFailed),
    }
}

/// Core argument parsing logic operating on clean Rust slices.
///
/// The first element (`args[0]`) is the program name and is skipped.
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
            // Flag processing -- may contain bundled flags like -ulvk
            if arg == b"-h" || arg == b"--help" {
                print_usage();
                std::process::exit(0);
            }

            if arg == b"-p" {
                // Next arg is port number
                i += 1;
                if i >= args.len() {
                    return Err(NcError::MissingPort);
                }
                let port_str = core::str::from_utf8(args[i])
                    .ok()
                    .ok_or(NcError::InvalidPort)?;
                local_port = parse_port(port_str).ok_or(NcError::InvalidPort)?;
                i += 1;
                continue;
            }

            if arg == b"-w" {
                // Next arg is timeout in seconds
                i += 1;
                if i >= args.len() {
                    return Err(NcError::InvalidPort); // reuse error for missing value
                }
                let timeout_str = core::str::from_utf8(args[i])
                    .ok()
                    .ok_or(NcError::InvalidPort)?;
                timeout_secs = timeout_str
                    .parse::<u32>()
                    .ok()
                    .ok_or(NcError::InvalidPort)?;
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
            let port_str = core::str::from_utf8(positional[0])
                .ok()
                .ok_or(NcError::InvalidPort)?;
            let port = parse_port(port_str).ok_or(NcError::InvalidPort)?;
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
            let port_str = core::str::from_utf8(positional[1])
                .ok()
                .ok_or(NcError::InvalidPort)?;
            let port = parse_port(port_str).ok_or(NcError::InvalidPort)?;
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

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Main entry point for nc, called from the binary crate.
pub fn nc_main(args: Vec<String>) -> ! {
    if args.len() <= 1 {
        print_usage();
        std::process::exit(1);
    }

    let byte_args: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
    let slices: Vec<&[u8]> = byte_args.iter().map(|a| a.as_slice()).collect();

    let config = match parse_args_from_slices(&slices) {
        Ok(c) => c,
        Err(e) => {
            print_error(e);
            print_usage();
            std::process::exit(1);
        }
    };

    process::ignore_signal(slopos_abi::signal::SIGPIPE);

    let saved_termios = fs::tcgetattr(0).ok();
    if let Some(ref t) = saved_termios {
        let mut raw = *t;
        raw.c_lflag &= !(slopos_abi::syscall::ECHO | slopos_abi::syscall::ICANON);
        let _ = fs::tcsetattr(0, &raw);
    }

    let exit_code = match (config.protocol, config.mode) {
        (NcProtocol::Udp, NcMode::Client) => udp::udp_client(&config),
        (NcProtocol::Udp, NcMode::Listen) => udp::udp_listen(&config),
        (NcProtocol::Tcp, NcMode::Client) => tcp::tcp_client(&config),
        (NcProtocol::Tcp, NcMode::Listen) => tcp::tcp_listen(&config),
    };

    // Restore terminal state before exiting.
    if let Some(ref t) = saved_termios {
        let _ = fs::tcsetattr(0, t);
    }

    std::process::exit(exit_code as i32);
}

// ---------------------------------------------------------------------------
// Tests (argument parsing & helpers -- no kernel needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helper function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_port_valid() {
        assert_eq!(parse_port("80"), Some(80));
        assert_eq!(parse_port("443"), Some(443));
        assert_eq!(parse_port("65535"), Some(65535));
        assert_eq!(parse_port("1"), Some(1));
        assert_eq!(parse_port("12345"), Some(12345));
    }

    #[test]
    fn test_parse_port_invalid() {
        assert_eq!(parse_port(""), None);
        assert_eq!(parse_port("0"), None);
        assert_eq!(parse_port("65536"), None);
        assert_eq!(parse_port("abc"), None);
        assert_eq!(parse_port("12a"), None);
        assert_eq!(parse_port("99999"), None);
    }

    #[test]
    fn test_parse_ipv4_valid() {
        assert_eq!(parse_ipv4("10.0.2.2"), Some([10, 0, 2, 2]));
        assert_eq!(parse_ipv4("192.168.1.1"), Some([192, 168, 1, 1]));
        assert_eq!(parse_ipv4("0.0.0.0"), Some([0, 0, 0, 0]));
        assert_eq!(parse_ipv4("255.255.255.255"), Some([255, 255, 255, 255]));
        assert_eq!(parse_ipv4("127.0.0.1"), Some([127, 0, 0, 1]));
    }

    #[test]
    fn test_parse_ipv4_invalid() {
        assert_eq!(parse_ipv4(""), None);
        assert_eq!(parse_ipv4("10.0.2"), None);
        assert_eq!(parse_ipv4("10.0.2.2.1"), None);
        assert_eq!(parse_ipv4("256.0.0.1"), None);
        assert_eq!(parse_ipv4("10.0.2.abc"), None);
        assert_eq!(parse_ipv4("..."), None);
        assert_eq!(parse_ipv4("1.2.3."), None);
        assert_eq!(parse_ipv4(".1.2.3"), None);
    }

    // -----------------------------------------------------------------------
    // Argument parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_tcp_is_default_protocol() {
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
        let args: &[&[u8]] = &[b"nc"];
        let err = parse_args_from_slices(args).unwrap_err();
        assert_eq!(err, NcError::MissingHost);
    }

    #[test]
    fn test_error_missing_port_client() {
        let args: &[&[u8]] = &[b"nc", b"10.0.2.2"];
        let err = parse_args_from_slices(args).unwrap_err();
        assert_eq!(err, NcError::MissingPort);
    }

    #[test]
    fn test_error_missing_port_listen() {
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
        assert_eq!(err, NcError::InvalidPort);
    }
}
