//! Socket address types, constants, and network byte-order helpers.

pub const AF_UNIX: i32 = 1;
pub const AF_INET: i32 = 2;
pub const AF_INET6: i32 = 10;

pub const SOCK_STREAM: i32 = 1;
pub const SOCK_DGRAM: i32 = 2;
pub const SOCK_NONBLOCK: i32 = 2048;
pub const SOCK_CLOEXEC: i32 = 524288;

pub const IPPROTO_TCP: i32 = 6;
pub const IPPROTO_UDP: i32 = 17;

pub const SHUT_RD: i32 = 0;
pub const SHUT_WR: i32 = 1;
pub const SHUT_RDWR: i32 = 2;

pub const SOL_SOCKET: i32 = 1;
pub const SO_REUSEADDR: i32 = 2;
pub const SO_ERROR: i32 = 4;
pub const SO_SNDBUF: i32 = 7;
pub const SO_RCVBUF: i32 = 8;
pub const SO_KEEPALIVE: i32 = 9;
pub const SO_RCVTIMEO: i32 = 20;
pub const SO_SNDTIMEO: i32 = 21;
pub const TCP_NODELAY: i32 = 1;

pub const INADDR_ANY: u32 = 0;
pub const INADDR_NONE: u32 = u32::MAX;

/// Generic socket address — compatible with POSIX `struct sockaddr`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SockAddr {
    pub sa_family: u16,
    pub sa_data: [u8; 14],
}

/// IPv4 socket address — compatible with POSIX `struct sockaddr_in`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SockAddrIn {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: u32,
    pub sin_zero: [u8; 8],
}

const _: () = assert!(core::mem::size_of::<SockAddr>() == 16);
const _: () = assert!(core::mem::size_of::<SockAddrIn>() == 16);

#[unsafe(no_mangle)]
pub extern "C" fn htons(x: u16) -> u16 {
    x.to_be()
}

#[unsafe(no_mangle)]
pub extern "C" fn ntohs(x: u16) -> u16 {
    u16::from_be(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn htonl(x: u32) -> u32 {
    x.to_be()
}

#[unsafe(no_mangle)]
pub extern "C" fn ntohl(x: u32) -> u32 {
    u32::from_be(x)
}

/// Parse dotted-decimal IPv4 into a network-byte-order `u32`, or
/// `INADDR_NONE` if the string does not parse.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inet_addr(cp: *const u8) -> u32 {
    if cp.is_null() {
        return INADDR_NONE;
    }

    let mut octets = [0u8; 4];
    let mut octet_idx = 0usize;
    let mut cur_val: u32 = 0;
    let mut ptr = cp;
    let mut has_digit = false;

    loop {
        let ch = *ptr;
        if ch == b'.' || ch == 0 {
            if !has_digit || cur_val > 255 || octet_idx >= 4 {
                return INADDR_NONE;
            }
            octets[octet_idx] = cur_val as u8;
            octet_idx += 1;
            cur_val = 0;
            has_digit = false;
            if ch == 0 {
                break;
            }
        } else if ch.is_ascii_digit() {
            cur_val = cur_val * 10 + (ch - b'0') as u32;
            has_digit = true;
        } else {
            return INADDR_NONE;
        }
        ptr = ptr.add(1);
    }

    if octet_idx != 4 {
        return INADDR_NONE;
    }

    // The octets are already in network order, so no swap.
    u32::from_ne_bytes(octets)
}

static mut INET_NTOA_BUF: [u8; 16] = [0u8; 16];

/// Format a network-byte-order IPv4 `u32` as dotted decimal. The result points
/// into a shared static buffer — NOT thread-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inet_ntoa(addr: u32) -> *const u8 {
    let bytes = addr.to_ne_bytes();
    let mut pos = 0usize;
    let buf = &raw mut INET_NTOA_BUF;

    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 {
            (*buf)[pos] = b'.';
            pos += 1;
        }
        if b >= 100 {
            (*buf)[pos] = b'0' + b / 100;
            pos += 1;
            (*buf)[pos] = b'0' + (b / 10) % 10;
            pos += 1;
            (*buf)[pos] = b'0' + b % 10;
            pos += 1;
        } else if b >= 10 {
            (*buf)[pos] = b'0' + b / 10;
            pos += 1;
            (*buf)[pos] = b'0' + b % 10;
            pos += 1;
        } else {
            (*buf)[pos] = b'0' + b;
            pos += 1;
        }
    }
    (*buf)[pos] = 0;
    (*buf).as_ptr()
}
