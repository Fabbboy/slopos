use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddrV4, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::syscall::process;

const MAX_HEADERS: usize = 8;
const MAX_REDIRECTS: usize = 10;
const MAX_RESPONSE_SIZE: usize = 1024 * 1024;
const IO_TIMEOUT_MS: i64 = 5000;

#[derive(Clone, Copy)]
struct ParsedUrl {
    host: [u8; 256],
    host_len: usize,
    ip: [u8; 4],
    port: u16,
    path: [u8; 512],
    path_len: usize,
}

struct CurlConfig {
    verbose: bool,
    follow_redirects: bool,
    method: Option<String>,
    #[allow(dead_code)] // parsed but not yet implemented
    output_file: Option<String>,
    headers: Vec<String>,
    data: Option<Vec<u8>>,
    url: String,
}

struct ParsedResponseHeaders {
    status_code: u16,
    content_length: Option<usize>,
    chunked: bool,
    location: Option<Vec<u8>>,
}

enum BodyKind {
    ContentLength(usize),
    Chunked,
    UntilClose,
}

enum ChunkState {
    Size,
    Data(usize),
    Trailer,
}

struct ChunkDecoder {
    state: ChunkState,
    cursor: usize,
    done: bool,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum CurlError {
    Usage,
    InvalidFlag,
    MissingValue,
    TooManyHeaders,
    InvalidMethod,
    InvalidUrl,
    UnsupportedScheme,
    InvalidHost,
    InvalidPort,
    InvalidPath,
    ResolveFailed,
    SocketFailed,
    ConnectFailed,
    SendFailed,
    Timeout,
    RecvFailed,
    ResponseTooLarge,
    InvalidResponse,
    InvalidChunkedEncoding,
    RedirectWithoutLocation,
    RedirectLimit,
}

fn print_usage() {
    println!("usage: curl [-v] [-L] [-o output] [-X method] [-H header] [-d data] <url>");
}

fn print_error(err: CurlError) {
    let msg = match err {
        CurlError::Usage => "curl: invalid usage",
        CurlError::InvalidFlag => "curl: invalid flag",
        CurlError::MissingValue => "curl: missing flag value",
        CurlError::TooManyHeaders => "curl: too many custom headers (max 8)",
        CurlError::InvalidMethod => "curl: invalid HTTP method",
        CurlError::InvalidUrl => "curl: invalid URL",
        CurlError::UnsupportedScheme => "curl: only http:// URLs are supported",
        CurlError::InvalidHost => "curl: invalid host",
        CurlError::InvalidPort => "curl: invalid port",
        CurlError::InvalidPath => "curl: invalid path",
        CurlError::ResolveFailed => "curl: hostname resolution failed",
        CurlError::SocketFailed => "curl: socket creation failed",
        CurlError::ConnectFailed => "curl: connect failed",
        CurlError::SendFailed => "curl: send failed",
        CurlError::Timeout => "curl: network timeout",
        CurlError::RecvFailed => "curl: receive failed",
        CurlError::ResponseTooLarge => "curl: response too large (limit 1 MiB)",
        CurlError::InvalidResponse => "curl: invalid HTTP response",
        CurlError::InvalidChunkedEncoding => "curl: invalid chunked response",
        CurlError::RedirectWithoutLocation => "curl: redirect without Location header",
        CurlError::RedirectLimit => "curl: too many redirects",
    };
    eprintln!("{msg}");
}

fn parse_args(args: Vec<String>) -> Result<CurlConfig, CurlError> {
    if args.len() <= 1 {
        return Err(CurlError::Usage);
    }

    let mut verbose = false;
    let mut follow_redirects = false;
    let mut method: Option<String> = None;
    let mut output_file: Option<String> = None;
    let mut headers: Vec<String> = Vec::new();
    let mut data: Option<Vec<u8>> = None;
    let mut url: Option<String> = None;

    let mut i = 1usize;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "-v" => {
                verbose = true;
                i += 1;
            }
            "-L" => {
                follow_redirects = true;
                i += 1;
            }
            "-o" => {
                i += 1;
                if i >= args.len() {
                    return Err(CurlError::MissingValue);
                }
                output_file = Some(args[i].clone());
                i += 1;
            }
            "-X" => {
                i += 1;
                if i >= args.len() {
                    return Err(CurlError::MissingValue);
                }
                if args[i].is_empty() {
                    return Err(CurlError::InvalidMethod);
                }
                method = Some(args[i].clone());
                i += 1;
            }
            "-H" => {
                i += 1;
                if i >= args.len() {
                    return Err(CurlError::MissingValue);
                }
                if headers.len() >= MAX_HEADERS {
                    return Err(CurlError::TooManyHeaders);
                }
                headers.push(args[i].clone());
                i += 1;
            }
            "-d" => {
                i += 1;
                if i >= args.len() {
                    return Err(CurlError::MissingValue);
                }
                data = Some(args[i].as_bytes().to_vec());
                i += 1;
            }
            _ if arg.starts_with('-') => return Err(CurlError::InvalidFlag),
            _ => {
                if url.is_some() {
                    return Err(CurlError::Usage);
                }
                url = Some(args[i].clone());
                i += 1;
            }
        }
    }

    let url = url.ok_or(CurlError::Usage)?;
    Ok(CurlConfig {
        verbose,
        follow_redirects,
        method,
        output_file,
        headers,
        data,
        url,
    })
}

fn parse_port(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() || bytes.len() > 5 {
        return None;
    }
    let mut acc: u32 = 0;
    for b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.saturating_mul(10).saturating_add((b - b'0') as u32);
        if acc > u16::MAX as u32 {
            return None;
        }
    }
    if acc == 0 {
        return None;
    }
    Some(acc as u16)
}

fn parse_url(url: &[u8]) -> Result<ParsedUrl, CurlError> {
    let prefix = b"http://";
    if !url.starts_with(prefix) {
        if url.starts_with(b"https://") {
            return Err(CurlError::UnsupportedScheme);
        }
        return Err(CurlError::InvalidUrl);
    }

    let rest = &url[prefix.len()..];
    if rest.is_empty() {
        return Err(CurlError::InvalidHost);
    }

    let mut slash_pos = rest.len();
    for (idx, b) in rest.iter().enumerate() {
        if *b == b'/' {
            slash_pos = idx;
            break;
        }
    }

    let host_port = &rest[..slash_pos];
    if host_port.is_empty() {
        return Err(CurlError::InvalidHost);
    }

    let path_bytes = if slash_pos < rest.len() {
        &rest[slash_pos..]
    } else {
        b"/"
    };

    if path_bytes.len() > 512 {
        return Err(CurlError::InvalidPath);
    }

    let mut host = [0u8; 256];
    let mut host_len = host_port.len();
    let mut port = 80u16;

    let mut colon_pos = None;
    for (idx, b) in host_port.iter().enumerate() {
        if *b == b':' {
            colon_pos = Some(idx);
            break;
        }
    }

    if let Some(cp) = colon_pos {
        let host_part = &host_port[..cp];
        let port_part = &host_port[cp + 1..];
        if host_part.is_empty() {
            return Err(CurlError::InvalidHost);
        }
        port = parse_port(port_part).ok_or(CurlError::InvalidPort)?;
        host_len = host_part.len();
        if host_len > host.len() {
            return Err(CurlError::InvalidHost);
        }
        host[..host_len].copy_from_slice(host_part);
    } else {
        if host_len > host.len() {
            return Err(CurlError::InvalidHost);
        }
        host[..host_len].copy_from_slice(host_port);
    }

    let mut path = [0u8; 512];
    path[..path_bytes.len()].copy_from_slice(path_bytes);

    Ok(ParsedUrl {
        host,
        host_len,
        ip: [0; 4],
        port,
        path,
        path_len: path_bytes.len(),
    })
}

fn parse_ipv4(host: &[u8]) -> Option<[u8; 4]> {
    let host_str = core::str::from_utf8(host).ok()?;
    Some(host_str.parse::<Ipv4Addr>().ok()?.octets())
}

fn resolve_host(parsed: &mut ParsedUrl) -> Result<(), CurlError> {
    let host = &parsed.host[..parsed.host_len];
    if let Some(ip) = parse_ipv4(host) {
        parsed.ip = ip;
        return Ok(());
    }

    let host_str = core::str::from_utf8(host).map_err(|_| CurlError::ResolveFailed)?;
    let addr = (host_str, 0u16)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.find(|a| a.is_ipv4()))
        .ok_or(CurlError::ResolveFailed)?;
    match addr {
        std::net::SocketAddr::V4(v4) => {
            parsed.ip = v4.ip().octets();
            Ok(())
        }
        _ => Err(CurlError::ResolveFailed),
    }
}

fn choose_method(config: &CurlConfig) -> &str {
    if let Some(ref method) = config.method {
        method.as_str()
    } else if config.data.is_some() {
        "POST"
    } else {
        "GET"
    }
}

fn build_request(config: &CurlConfig, parsed: &ParsedUrl) -> Result<Vec<u8>, CurlError> {
    let method = choose_method(config);
    if method.is_empty() {
        return Err(CurlError::InvalidMethod);
    }
    let path = &parsed.path[..parsed.path_len];
    let host = &parsed.host[..parsed.host_len];
    let body = config.data.as_deref().unwrap_or(&[]);

    let mut req = Vec::with_capacity(1024 + body.len());
    req.extend_from_slice(method.as_bytes());
    req.extend_from_slice(b" ");
    req.extend_from_slice(path);
    req.extend_from_slice(b" HTTP/1.1\r\n");
    req.extend_from_slice(b"Host: ");
    req.extend_from_slice(host);
    req.extend_from_slice(b"\r\n");
    req.extend_from_slice(b"User-Agent: SlopOS-curl/1.0\r\n");
    req.extend_from_slice(b"Accept: */*\r\n");
    req.extend_from_slice(b"Connection: close\r\n");

    if !body.is_empty() {
        req.extend_from_slice(b"Content-Length: ");
        req.extend_from_slice(format!("{}", body.len()).as_bytes());
        req.extend_from_slice(b"\r\n");
    }

    for header in &config.headers {
        req.extend_from_slice(header.as_bytes());
        req.extend_from_slice(b"\r\n");
    }

    req.extend_from_slice(b"\r\n");
    if !body.is_empty() {
        req.extend_from_slice(body);
    }

    Ok(req)
}

fn starts_with_case_insensitive(haystack: &[u8], needle_lower: &[u8]) -> bool {
    if haystack.len() < needle_lower.len() {
        return false;
    }
    for i in 0..needle_lower.len() {
        if haystack[i].to_ascii_lowercase() != needle_lower[i] {
            return false;
        }
    }
    true
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while !bytes.is_empty() && bytes[0].is_ascii_whitespace() {
        bytes = &bytes[1..];
    }
    while !bytes.is_empty() && bytes[bytes.len() - 1].is_ascii_whitespace() {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn find_crlf(data: &[u8], start: usize) -> Option<usize> {
    if data.len() < 2 || start >= data.len().saturating_sub(1) {
        return None;
    }
    let mut i = start;
    while i + 1 < data.len() {
        if data[i] == b'\r' && data[i + 1] == b'\n' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_header_terminator(data: &[u8]) -> Option<usize> {
    if data.len() < 4 {
        return None;
    }
    let mut i = 0usize;
    while i + 3 < data.len() {
        if data[i] == b'\r' && data[i + 1] == b'\n' && data[i + 2] == b'\r' && data[i + 3] == b'\n'
        {
            return Some(i + 4);
        }
        i += 1;
    }
    None
}

fn parse_status_code(status_line: &[u8]) -> Option<u16> {
    let first_space = status_line.iter().position(|b| *b == b' ')?;
    let after_proto = &status_line[first_space + 1..];
    let second_space = after_proto
        .iter()
        .position(|b| *b == b' ')
        .unwrap_or(after_proto.len());
    let code_bytes = &after_proto[..second_space];
    if code_bytes.len() != 3 {
        return None;
    }
    let mut code: u16 = 0;
    for b in code_bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        code = code.saturating_mul(10).saturating_add((b - b'0') as u16);
    }
    Some(code)
}

fn parse_response_headers(header_block: &[u8]) -> Result<ParsedResponseHeaders, CurlError> {
    let mut pos = 0usize;
    let status_end = find_crlf(header_block, pos).ok_or(CurlError::InvalidResponse)?;
    let status_line = &header_block[pos..status_end];
    if !starts_with_case_insensitive(status_line, b"http/") {
        return Err(CurlError::InvalidResponse);
    }
    let status_code = parse_status_code(status_line).ok_or(CurlError::InvalidResponse)?;
    pos = status_end + 2;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut location: Option<Vec<u8>> = None;

    while pos < header_block.len() {
        let line_end = find_crlf(header_block, pos).ok_or(CurlError::InvalidResponse)?;
        if line_end == pos {
            break;
        }
        let line = &header_block[pos..line_end];
        if let Some(colon) = line.iter().position(|b| *b == b':') {
            let name = trim_ascii(&line[..colon]);
            let value = trim_ascii(&line[colon + 1..]);

            if starts_with_case_insensitive(name, b"content-length") {
                if let Ok(value_str) = core::str::from_utf8(value)
                    && let Ok(parsed) = value_str.parse::<usize>()
                {
                    content_length = Some(parsed);
                }
            } else if starts_with_case_insensitive(name, b"transfer-encoding") {
                let mut lower = [0u8; 64];
                let n = value.len().min(lower.len());
                for i in 0..n {
                    lower[i] = value[i].to_ascii_lowercase();
                }
                if n >= 7 {
                    let mut i = 0usize;
                    while i + 6 < n {
                        if &lower[i..i + 7] == b"chunked" {
                            chunked = true;
                            break;
                        }
                        i += 1;
                    }
                }
            } else if starts_with_case_insensitive(name, b"location") {
                location = Some(value.to_vec());
            }
        }

        pos = line_end + 2;
    }

    Ok(ParsedResponseHeaders {
        status_code,
        content_length,
        chunked,
        location,
    })
}

fn is_redirect_status(status: u16) -> bool {
    match status {
        301 | 302 | 303 | 307 | 308 => true,
        _ => false,
    }
}

fn send_all(stream: &mut TcpStream, data: &[u8]) -> Result<(), CurlError> {
    stream.write_all(data).map_err(|_| CurlError::SendFailed)
}

fn verbose_emit_prefixed(prefix: u8, block: &[u8]) {
    let mut err = io::stderr().lock();
    let mut start = 0usize;
    while start < block.len() {
        let mut end = start;
        while end + 1 < block.len() {
            if block[end] == b'\r' && block[end + 1] == b'\n' {
                break;
            }
            end += 1;
        }

        if end + 1 >= block.len() {
            break;
        }

        let line = &block[start..end];
        if line.is_empty() {
            break;
        }

        let _ = err.write_all(&[prefix, b' ']);
        let _ = err.write_all(line);
        let _ = err.write_all(b"\n");

        start = end + 2;
    }
    let _ = err.flush();
}

fn append_limited(dst: &mut Vec<u8>, src: &[u8]) -> Result<(), CurlError> {
    if dst.len().saturating_add(src.len()) > MAX_RESPONSE_SIZE {
        return Err(CurlError::ResponseTooLarge);
    }
    dst.extend_from_slice(src);
    Ok(())
}

impl ChunkDecoder {
    fn new() -> Self {
        Self {
            state: ChunkState::Size,
            cursor: 0,
            done: false,
        }
    }

    fn decode_from(&mut self, raw: &[u8], decoded: &mut Vec<u8>) -> Result<(), CurlError> {
        loop {
            if self.done {
                return Ok(());
            }

            match self.state {
                ChunkState::Size => {
                    let line_end = match find_crlf(raw, self.cursor) {
                        Some(v) => v,
                        _ => return Ok(()),
                    };
                    let mut size_field = &raw[self.cursor..line_end];
                    if let Some(semi) = size_field.iter().position(|b| *b == b';') {
                        size_field = &size_field[..semi];
                    }
                    size_field = trim_ascii(size_field);
                    if size_field.is_empty() {
                        return Err(CurlError::InvalidChunkedEncoding);
                    }
                    let size_str = core::str::from_utf8(size_field)
                        .map_err(|_| CurlError::InvalidChunkedEncoding)?;
                    let size = usize::from_str_radix(size_str, 16)
                        .map_err(|_| CurlError::InvalidChunkedEncoding)?;
                    self.cursor = line_end + 2;
                    if size == 0 {
                        self.state = ChunkState::Trailer;
                    } else {
                        self.state = ChunkState::Data(size);
                    }
                }
                ChunkState::Data(size) => {
                    if raw.len() < self.cursor.saturating_add(size).saturating_add(2) {
                        return Ok(());
                    }
                    append_limited(decoded, &raw[self.cursor..self.cursor + size])?;
                    self.cursor += size;
                    if raw[self.cursor] != b'\r' || raw[self.cursor + 1] != b'\n' {
                        return Err(CurlError::InvalidChunkedEncoding);
                    }
                    self.cursor += 2;
                    self.state = ChunkState::Size;
                }
                ChunkState::Trailer => {
                    if raw.len() < self.cursor + 2 {
                        return Ok(());
                    }
                    if raw[self.cursor] == b'\r' && raw[self.cursor + 1] == b'\n' {
                        self.cursor += 2;
                        self.done = true;
                        return Ok(());
                    }

                    let trailer_end = match find_header_terminator(&raw[self.cursor..]) {
                        Some(v) => self.cursor + v,
                        _ => return Ok(()),
                    };
                    self.cursor = trailer_end;
                    self.done = true;
                    return Ok(());
                }
            }
        }
    }
}

fn receive_http_response(
    stream: &mut TcpStream,
    verbose: bool,
) -> Result<(ParsedResponseHeaders, Vec<u8>), CurlError> {
    stream
        .set_read_timeout(Some(Duration::from_millis(IO_TIMEOUT_MS as u64)))
        .map_err(|_| CurlError::RecvFailed)?;

    let mut recv_buf = [0u8; 4096];
    let mut raw: Vec<u8> = Vec::new();
    let mut decoded_body = Vec::new();
    let mut headers: Option<ParsedResponseHeaders> = None;
    let mut header_end = 0usize;
    let mut body_kind = BodyKind::UntilClose;
    let mut chunk_decoder = ChunkDecoder::new();

    loop {
        match stream.read(&mut recv_buf) {
            Ok(0) => {
                if headers.is_none() {
                    return Err(CurlError::InvalidResponse);
                }
                match body_kind {
                    BodyKind::UntilClose => {
                        let parsed = headers.ok_or(CurlError::InvalidResponse)?;
                        return Ok((parsed, decoded_body));
                    }
                    BodyKind::ContentLength(expected) => {
                        if decoded_body.len() == expected {
                            let parsed = headers.ok_or(CurlError::InvalidResponse)?;
                            return Ok((parsed, decoded_body));
                        }
                        return Err(CurlError::InvalidResponse);
                    }
                    BodyKind::Chunked => {
                        if chunk_decoder.done {
                            let parsed = headers.ok_or(CurlError::InvalidResponse)?;
                            return Ok((parsed, decoded_body));
                        }
                        return Err(CurlError::InvalidChunkedEncoding);
                    }
                }
            }
            Ok(n) => {
                append_limited(&mut raw, &recv_buf[..n])?;

                if headers.is_none() {
                    if let Some(end) = find_header_terminator(&raw) {
                        header_end = end;
                        let parsed = parse_response_headers(&raw[..header_end])?;
                        if verbose {
                            verbose_emit_prefixed(b'<', &raw[..header_end]);
                        }

                        body_kind = if parsed.chunked {
                            BodyKind::Chunked
                        } else if let Some(len) = parsed.content_length {
                            BodyKind::ContentLength(len)
                        } else {
                            BodyKind::UntilClose
                        };

                        headers = Some(parsed);

                        let body_part = raw.get(header_end..).ok_or(CurlError::InvalidResponse)?;
                        match body_kind {
                            BodyKind::Chunked => {
                                chunk_decoder.decode_from(body_part, &mut decoded_body)?;
                                if chunk_decoder.done {
                                    let parsed = headers.ok_or(CurlError::InvalidResponse)?;
                                    return Ok((parsed, decoded_body));
                                }
                            }
                            BodyKind::ContentLength(expected) => {
                                let take = body_part.len().min(expected);
                                append_limited(&mut decoded_body, &body_part[..take])?;
                                if decoded_body.len() == expected {
                                    let parsed = headers.ok_or(CurlError::InvalidResponse)?;
                                    return Ok((parsed, decoded_body));
                                }
                            }
                            BodyKind::UntilClose => {
                                append_limited(&mut decoded_body, body_part)?;
                            }
                        }
                    }
                } else {
                    let parsed = headers.as_ref().ok_or(CurlError::InvalidResponse)?;
                    let body_part = raw.get(header_end..).ok_or(CurlError::InvalidResponse)?;
                    match body_kind {
                        BodyKind::Chunked => {
                            chunk_decoder.decode_from(body_part, &mut decoded_body)?;
                            if chunk_decoder.done {
                                return Ok((
                                    ParsedResponseHeaders {
                                        status_code: parsed.status_code,
                                        content_length: parsed.content_length,
                                        chunked: parsed.chunked,
                                        location: parsed.location.clone(),
                                    },
                                    decoded_body,
                                ));
                            }
                        }
                        BodyKind::ContentLength(expected) => {
                            if decoded_body.len() < expected {
                                let needed = expected - decoded_body.len();
                                let available = body_part.len().saturating_sub(decoded_body.len());
                                if available > 0 {
                                    let start = body_part.len() - available;
                                    let take = needed.min(available);
                                    append_limited(
                                        &mut decoded_body,
                                        &body_part[start..start + take],
                                    )?;
                                }
                            }
                            if decoded_body.len() == expected {
                                return Ok((
                                    ParsedResponseHeaders {
                                        status_code: parsed.status_code,
                                        content_length: parsed.content_length,
                                        chunked: parsed.chunked,
                                        location: parsed.location.clone(),
                                    },
                                    decoded_body,
                                ));
                            }
                        }
                        BodyKind::UntilClose => {
                            let already = decoded_body.len();
                            let total = body_part.len();
                            if total > already {
                                append_limited(&mut decoded_body, &body_part[already..])?;
                            }
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::TimedOut => {
                return Err(CurlError::Timeout);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(CurlError::Timeout);
            }
            Err(_) => return Err(CurlError::RecvFailed),
        }
    }
}

fn set_path(target: &mut ParsedUrl, path: &[u8]) -> Result<(), CurlError> {
    if path.is_empty() || path.len() > target.path.len() {
        return Err(CurlError::InvalidPath);
    }
    target.path = [0; 512];
    target.path[..path.len()].copy_from_slice(path);
    target.path_len = path.len();
    Ok(())
}

fn resolve_redirect(base: &ParsedUrl, location: &[u8]) -> Result<ParsedUrl, CurlError> {
    let location = trim_ascii(location);
    if location.starts_with(b"http://") {
        let mut parsed = parse_url(location)?;
        resolve_host(&mut parsed)?;
        return Ok(parsed);
    }

    let mut next = *base;
    if location.starts_with(b"/") {
        set_path(&mut next, location)?;
        return Ok(next);
    }

    let base_path = &base.path[..base.path_len];
    let mut split = base_path.len();
    while split > 0 {
        if base_path[split - 1] == b'/' {
            break;
        }
        split -= 1;
    }

    let mut merged = Vec::with_capacity(base_path.len() + location.len() + 1);
    if split == 0 {
        merged.extend_from_slice(b"/");
    } else {
        merged.extend_from_slice(&base_path[..split]);
    }
    merged.extend_from_slice(location);
    set_path(&mut next, &merged)?;
    Ok(next)
}

fn execute_request(
    config: &CurlConfig,
    parsed: &ParsedUrl,
) -> Result<(ParsedResponseHeaders, Vec<u8>), CurlError> {
    let addr = SocketAddrV4::new(Ipv4Addr::from(parsed.ip), parsed.port);

    let mut stream = TcpStream::connect(addr).map_err(|_| CurlError::ConnectFailed)?;

    let req = build_request(config, parsed)?;
    if config.verbose {
        if let Some(end) = find_header_terminator(&req) {
            verbose_emit_prefixed(b'>', &req[..end]);
        }
    }

    if send_all(&mut stream, &req).is_err() {
        let _ = stream.shutdown(Shutdown::Both);
        return Err(CurlError::SendFailed);
    }

    let result = receive_http_response(&mut stream, config.verbose);
    let _ = stream.shutdown(Shutdown::Both);
    result
}

fn run_curl(config: &CurlConfig) -> Result<(), CurlError> {
    let mut current = parse_url(config.url.as_bytes())?;
    resolve_host(&mut current)?;

    let mut redirects = 0usize;

    loop {
        let (headers, body) = execute_request(config, &current)?;
        if config.follow_redirects && is_redirect_status(headers.status_code) {
            let location = headers.location.ok_or(CurlError::RedirectWithoutLocation)?;
            redirects += 1;
            if redirects > MAX_REDIRECTS {
                return Err(CurlError::RedirectLimit);
            }
            current = resolve_redirect(&current, &location)?;
            if current.ip == [0; 4] {
                resolve_host(&mut current)?;
            }
            continue;
        }

        {
            let mut out = io::stdout().lock();
            let _ = out.write_all(&body);
            let _ = out.flush();
        }
        return Ok(());
    }
}

pub fn curl_main(args: Vec<String>) -> ! {
    process::ignore_signal(slopos_abi::signal::SIGPIPE);

    let config = match parse_args(args) {
        Ok(cfg) => cfg,
        Err(err) => {
            print_error(err);
            print_usage();
            std::process::exit(1);
        }
    };

    if config.verbose {
        eprintln!("* SlopOS curl HTTP/1.1");
    }

    match run_curl(&config) {
        Ok(_) => std::process::exit(0),
        Err(err) => {
            print_error(err);
            std::process::exit(1);
        }
    }
}
