//! TCP header parsing and serialization (RFC 793 §3.1, RFC 7323 §2).
//!
//! Contains only header-level primitives: flag bits, option kinds, the
//! `TcpHeader` wire struct, parsers for the fixed header and for MSS / window
//! scale options, and writers for the header and its options.  No state
//! machine, no connection table, no timers.

// ---------------------------------------------------------------------------
// Header constants
// ---------------------------------------------------------------------------

/// Minimum TCP header length (no options).
pub const TCP_HEADER_LEN: usize = 20;

/// Maximum TCP header length (with 40 bytes of options).
pub const TCP_HEADER_MAX_LEN: usize = 60;

/// Default Maximum Segment Size (Ethernet MTU 1500 − IP 20 − TCP 20).
pub const DEFAULT_MSS: u16 = 1460;

/// Default receive window size advertised in SYN (unscaled, fits in u16).
pub const DEFAULT_WINDOW_SIZE: u16 = 32768;

// ---------------------------------------------------------------------------
// TCP flag bits (in the flags byte of the header)
// ---------------------------------------------------------------------------

pub const TCP_FLAG_FIN: u8 = 0x01;
pub const TCP_FLAG_SYN: u8 = 0x02;
pub const TCP_FLAG_RST: u8 = 0x04;
pub const TCP_FLAG_PSH: u8 = 0x08;
pub const TCP_FLAG_ACK: u8 = 0x10;
pub const TCP_FLAG_URG: u8 = 0x20;

// ---------------------------------------------------------------------------
// TCP option kinds
// ---------------------------------------------------------------------------

pub const TCP_OPT_END: u8 = 0;
pub const TCP_OPT_NOP: u8 = 1;
pub const TCP_OPT_MSS: u8 = 2;
pub const TCP_OPT_MSS_LEN: u8 = 4;
pub const TCP_OPT_WINDOW_SCALE: u8 = 3;
pub const TCP_OPT_WINDOW_SCALE_LEN: u8 = 3;

// =============================================================================
// TCP Header
// =============================================================================

/// Parsed TCP header.
///
/// All multi-byte fields are stored in **host** byte order after parsing.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq_num: u32,
    pub ack_num: u32,
    /// Data offset in 32-bit words (5–15).
    pub data_offset: u8,
    pub flags: u8,
    pub window_size: u16,
    pub checksum: u16,
    pub urgent_ptr: u16,
}

impl TcpHeader {
    /// Header length in bytes (data_offset × 4).
    #[inline]
    pub const fn header_len(&self) -> usize {
        (self.data_offset as usize) * 4
    }

    /// Options length in bytes (header_len − 20).
    #[inline]
    pub const fn options_len(&self) -> usize {
        self.header_len().saturating_sub(TCP_HEADER_LEN)
    }

    // --- Flag helpers -------------------------------------------------------

    #[inline]
    pub const fn is_syn(&self) -> bool {
        (self.flags & TCP_FLAG_SYN) != 0
    }

    #[inline]
    pub const fn is_ack(&self) -> bool {
        (self.flags & TCP_FLAG_ACK) != 0
    }

    #[inline]
    pub const fn is_fin(&self) -> bool {
        (self.flags & TCP_FLAG_FIN) != 0
    }

    #[inline]
    pub const fn is_rst(&self) -> bool {
        (self.flags & TCP_FLAG_RST) != 0
    }

    #[inline]
    pub const fn is_psh(&self) -> bool {
        (self.flags & TCP_FLAG_PSH) != 0
    }

    #[inline]
    pub const fn is_urg(&self) -> bool {
        (self.flags & TCP_FLAG_URG) != 0
    }

    #[inline]
    pub const fn is_syn_ack(&self) -> bool {
        self.is_syn() && self.is_ack()
    }

    #[inline]
    pub const fn is_fin_ack(&self) -> bool {
        self.is_fin() && self.is_ack()
    }
}

// =============================================================================
// Parsing
// =============================================================================

/// Parse a TCP header from a byte slice.
///
/// Returns `None` if the slice is too short or the data offset is invalid.
pub fn parse_header(data: &[u8]) -> Option<TcpHeader> {
    if data.len() < TCP_HEADER_LEN {
        return None;
    }

    let src_port = u16::from_be_bytes([data[0], data[1]]);
    let dst_port = u16::from_be_bytes([data[2], data[3]]);
    let seq_num = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ack_num = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

    let data_offset = (data[12] >> 4) & 0x0F;
    let flags = data[13] & 0x3F; // low 6 bits of byte 13

    let window_size = u16::from_be_bytes([data[14], data[15]]);
    let checksum = u16::from_be_bytes([data[16], data[17]]);
    let urgent_ptr = u16::from_be_bytes([data[18], data[19]]);

    // Data offset must be at least 5 (20 bytes) and at most 15 (60 bytes).
    if data_offset < 5 || data_offset > 15 {
        return None;
    }

    let header_len = (data_offset as usize) * 4;
    if data.len() < header_len {
        return None;
    }

    Some(TcpHeader {
        src_port,
        dst_port,
        seq_num,
        ack_num,
        data_offset,
        flags,
        window_size,
        checksum,
        urgent_ptr,
    })
}

/// Parse MSS option from TCP header options region.
///
/// Returns the MSS value if found, otherwise `None`.
pub fn parse_mss_option(options: &[u8]) -> Option<u16> {
    let mut i = 0;
    while i < options.len() {
        match options[i] {
            TCP_OPT_END => break,
            TCP_OPT_NOP => {
                i += 1;
            }
            TCP_OPT_MSS => {
                if i + 3 < options.len() && options[i + 1] == TCP_OPT_MSS_LEN {
                    return Some(u16::from_be_bytes([options[i + 2], options[i + 3]]));
                }
                break;
            }
            _ => {
                // Unknown option: skip using length byte.
                if i + 1 >= options.len() {
                    break;
                }
                let opt_len = options[i + 1] as usize;
                if opt_len < 2 || i + opt_len > options.len() {
                    break;
                }
                i += opt_len;
            }
        }
    }
    None
}

pub struct ParsedTcpOptions {
    pub mss: Option<u16>,
    pub window_scale: Option<u8>,
}

pub fn parse_tcp_options(options: &[u8]) -> ParsedTcpOptions {
    let mut result = ParsedTcpOptions {
        mss: None,
        window_scale: None,
    };
    let mut i = 0;
    while i < options.len() {
        match options[i] {
            TCP_OPT_END => break,
            TCP_OPT_NOP => {
                i += 1;
            }
            TCP_OPT_MSS => {
                if i + 3 < options.len() && options[i + 1] == TCP_OPT_MSS_LEN {
                    result.mss = Some(u16::from_be_bytes([options[i + 2], options[i + 3]]));
                }
                i += TCP_OPT_MSS_LEN as usize;
            }
            TCP_OPT_WINDOW_SCALE => {
                if i + 2 < options.len() && options[i + 1] == TCP_OPT_WINDOW_SCALE_LEN {
                    // RFC 7323: shift count must be <= 14
                    result.window_scale = Some(options[i + 2].min(14));
                }
                i += TCP_OPT_WINDOW_SCALE_LEN as usize;
            }
            _ => {
                if i + 1 >= options.len() {
                    break;
                }
                let opt_len = options[i + 1] as usize;
                if opt_len < 2 || i + opt_len > options.len() {
                    break;
                }
                i += opt_len;
            }
        }
    }
    result
}

/// Compute our receive-side window scale shift count from `TCP_BUFFER_SIZE`.
/// RFC 7323 §2.2: shift count = ceil(log2(buffer_size / 65535)).
pub fn our_window_scale() -> u8 {
    let mut shift = 0u8;
    let mut size = super::TCP_BUFFER_SIZE;
    while size > u16::MAX as usize && shift < 14 {
        size >>= 1;
        shift += 1;
    }
    shift
}

pub fn write_window_scale_option(shift: u8, out: &mut [u8]) -> Option<usize> {
    if out.len() < 3 {
        return None;
    }
    out[0] = TCP_OPT_WINDOW_SCALE;
    out[1] = TCP_OPT_WINDOW_SCALE_LEN;
    out[2] = shift;
    Some(3)
}

// =============================================================================
// Construction
// =============================================================================

/// Write a TCP header into `out[..header_len]`.
///
/// Returns the header length on success, `None` if `out` is too short.
/// The checksum field is written as 0 — the caller must compute and patch it
/// afterwards using [`super::checksum::tcp_checksum`].
pub fn write_header(hdr: &TcpHeader, out: &mut [u8]) -> Option<usize> {
    let header_len = hdr.header_len();
    if out.len() < header_len || header_len < TCP_HEADER_LEN {
        return None;
    }

    out[0..2].copy_from_slice(&hdr.src_port.to_be_bytes());
    out[2..4].copy_from_slice(&hdr.dst_port.to_be_bytes());
    out[4..8].copy_from_slice(&hdr.seq_num.to_be_bytes());
    out[8..12].copy_from_slice(&hdr.ack_num.to_be_bytes());
    out[12] = (hdr.data_offset << 4) & 0xF0;
    out[13] = hdr.flags & 0x3F;
    out[14..16].copy_from_slice(&hdr.window_size.to_be_bytes());
    out[16..18].copy_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    out[18..20].copy_from_slice(&hdr.urgent_ptr.to_be_bytes());

    // Zero any options area beyond the minimum header.
    if header_len > TCP_HEADER_LEN {
        out[TCP_HEADER_LEN..header_len].fill(0);
    }

    Some(header_len)
}

/// Build a minimal TCP header with the given parameters.
pub fn build_header(
    src_port: u16,
    dst_port: u16,
    seq_num: u32,
    ack_num: u32,
    flags: u8,
    window_size: u16,
    data_offset: u8,
) -> TcpHeader {
    TcpHeader {
        src_port,
        dst_port,
        seq_num,
        ack_num,
        data_offset,
        flags,
        window_size,
        checksum: 0,
        urgent_ptr: 0,
    }
}

/// Write MSS option into `out` buffer.  Returns bytes written (4) or `None`.
pub fn write_mss_option(mss: u16, out: &mut [u8]) -> Option<usize> {
    if out.len() < 4 {
        return None;
    }
    out[0] = TCP_OPT_MSS;
    out[1] = TCP_OPT_MSS_LEN;
    out[2..4].copy_from_slice(&mss.to_be_bytes());
    Some(4)
}
