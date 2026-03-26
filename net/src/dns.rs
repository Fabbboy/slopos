//! DNS client: wire protocol, cache, and resolver.
//!
//! Implements a minimal DNS stub resolver for A-record lookups over UDP.
//! The resolver lives in-kernel (matching the DHCP client pattern) with a
//! synchronous `dns_resolve()` entry point called from the `SYSCALL_RESOLVE`
//! handler.

use core::sync::atomic::{AtomicU16, Ordering};

use slopos_sync::IrqMutex;
use slopos_utils::klog_debug;

// =============================================================================
// Constants
// =============================================================================

/// Standard DNS UDP port.
pub const DNS_PORT: u16 = 53;
/// Maximum DNS name length (RFC 1035).
const DNS_NAME_MAX: usize = 253;
/// Maximum label length (RFC 1035).
const DNS_LABEL_MAX: usize = 63;
/// DNS header length.
const DNS_HEADER_LEN: usize = 12;
/// Maximum standard DNS UDP response size.
const DNS_MAX_RESPONSE: usize = 512;
/// Maximum CNAME hops before giving up.
const MAX_CNAME_HOPS: usize = 8;
/// Maximum compression pointer follows (loop detection).
const MAX_POINTER_FOLLOWS: usize = 16;
/// DNS resolve timeout per attempt (ms).
const DNS_TIMEOUT_MS: u32 = 3000;
/// Number of resolve attempts.
const DNS_MAX_RETRIES: usize = 3;
/// DNS cache size.
const DNS_CACHE_SIZE: usize = 16;

/// Monotonically increasing query ID.
static QUERY_ID: AtomicU16 = AtomicU16::new(0x4242);

// =============================================================================
// Types
// =============================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DnsType {
    A = 1,
    CNAME = 5,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DnsClass {
    IN = 1,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum DnsRcode {
    NoError = 0,
    ServFail = 2,
    NXDomain = 3,
    Refused = 5,
}

/// Parsed DNS header (12 bytes).
#[derive(Clone, Copy, Default)]
pub struct DnsHeader {
    pub id: u16,
    pub flags: u16,
    pub qdcount: u16,
    pub ancount: u16,
    pub nscount: u16,
    pub arcount: u16,
}

impl DnsHeader {
    /// QR bit: 0 = query, 1 = response.
    pub fn qr(&self) -> bool {
        (self.flags & 0x8000) != 0
    }

    /// RCODE (low 4 bits of flags).
    pub fn rcode(&self) -> u8 {
        (self.flags & 0x000F) as u8
    }

    fn to_bytes(&self, buf: &mut [u8]) {
        buf[0..2].copy_from_slice(&self.id.to_be_bytes());
        buf[2..4].copy_from_slice(&self.flags.to_be_bytes());
        buf[4..6].copy_from_slice(&self.qdcount.to_be_bytes());
        buf[6..8].copy_from_slice(&self.ancount.to_be_bytes());
        buf[8..10].copy_from_slice(&self.nscount.to_be_bytes());
        buf[10..12].copy_from_slice(&self.arcount.to_be_bytes());
    }

    fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < DNS_HEADER_LEN {
            return None;
        }
        Some(DnsHeader {
            id: u16::from_be_bytes([buf[0], buf[1]]),
            flags: u16::from_be_bytes([buf[2], buf[3]]),
            qdcount: u16::from_be_bytes([buf[4], buf[5]]),
            ancount: u16::from_be_bytes([buf[6], buf[7]]),
            nscount: u16::from_be_bytes([buf[8], buf[9]]),
            arcount: u16::from_be_bytes([buf[10], buf[11]]),
        })
    }
}

/// Successful DNS resolution result.
#[derive(Clone, Copy)]
pub struct DnsResponse {
    pub addr: [u8; 4],
    pub ttl: u32,
}

// =============================================================================
// DNS name encoding
// =============================================================================

/// Encode a hostname into DNS wire format (length-prefixed labels).
///
/// `"example.com"` → `[7, e,x,a,m,p,l,e, 3, c,o,m, 0]`
///
/// Returns the number of bytes written, or `None` on invalid input.
pub fn dns_encode_name(hostname: &[u8], buf: &mut [u8]) -> Option<usize> {
    if hostname.is_empty() {
        // Root label
        if buf.is_empty() {
            return None;
        }
        buf[0] = 0;
        return Some(1);
    }

    // Validate total length
    if hostname.len() > DNS_NAME_MAX {
        return None;
    }

    // Strip trailing dot if present
    let hostname = if hostname.last() == Some(&b'.') {
        &hostname[..hostname.len() - 1]
    } else {
        hostname
    };

    if hostname.is_empty() {
        if buf.is_empty() {
            return None;
        }
        buf[0] = 0;
        return Some(1);
    }

    let mut out_pos = 0usize;

    for label in hostname.split(|&b| b == b'.') {
        if label.is_empty() || label.len() > DNS_LABEL_MAX {
            return None;
        }
        // Need space for length byte + label + at least the trailing zero
        if out_pos + 1 + label.len() >= buf.len() {
            return None;
        }
        buf[out_pos] = label.len() as u8;
        out_pos += 1;
        buf[out_pos..out_pos + label.len()].copy_from_slice(label);
        out_pos += label.len();
    }

    // Trailing root label
    if out_pos >= buf.len() {
        return None;
    }
    buf[out_pos] = 0;
    out_pos += 1;

    Some(out_pos)
}

// =============================================================================
// DNS query construction
// =============================================================================

/// Build a DNS query packet for the given hostname and query type.
///
/// Returns the total packet length, or `None` on error.
pub fn dns_build_query(id: u16, hostname: &[u8], qtype: DnsType, buf: &mut [u8]) -> Option<usize> {
    if buf.len() < DNS_HEADER_LEN + 4 {
        return None;
    }

    // Header: QR=0, OPCODE=0, RD=1
    let header = DnsHeader {
        id,
        flags: 0x0100, // RD = 1
        qdcount: 1,
        ancount: 0,
        nscount: 0,
        arcount: 0,
    };
    header.to_bytes(&mut buf[..DNS_HEADER_LEN]);

    // Question section: encoded name
    let name_len = dns_encode_name(hostname, &mut buf[DNS_HEADER_LEN..])?;
    let q_start = DNS_HEADER_LEN + name_len;

    if q_start + 4 > buf.len() {
        return None;
    }

    // QTYPE
    buf[q_start..q_start + 2].copy_from_slice(&(qtype as u16).to_be_bytes());
    // QCLASS = IN
    buf[q_start + 2..q_start + 4].copy_from_slice(&(DnsClass::IN as u16).to_be_bytes());

    Some(q_start + 4)
}

// =============================================================================
// DNS name decoding (with compression pointer support)
// =============================================================================

/// Decode a DNS name from wire format with compression pointer support.
///
/// Returns `(decoded_name_len, wire_bytes_consumed)` or `None` on error.
/// The decoded name is written as a dotted string (e.g., `example.com`).
pub fn dns_decode_name(packet: &[u8], offset: usize, out: &mut [u8]) -> Option<(usize, usize)> {
    let mut pos = offset;
    let mut out_pos = 0usize;
    let mut jumped = false;
    let mut wire_consumed = 0usize;
    let mut pointer_count = 0usize;

    loop {
        if pos >= packet.len() {
            return None;
        }

        let len_or_ptr = packet[pos];

        if len_or_ptr == 0 {
            // Root label — end of name
            if !jumped {
                wire_consumed = pos + 1 - offset;
            }
            break;
        }

        if (len_or_ptr & 0xC0) == 0xC0 {
            // Compression pointer
            if pos + 1 >= packet.len() {
                return None;
            }
            if !jumped {
                wire_consumed = pos + 2 - offset;
            }
            let ptr_offset = ((len_or_ptr as usize & 0x3F) << 8) | (packet[pos + 1] as usize);
            if ptr_offset >= packet.len() {
                return None;
            }
            pointer_count += 1;
            if pointer_count > MAX_POINTER_FOLLOWS {
                return None; // Loop detection
            }
            pos = ptr_offset;
            jumped = true;
            continue;
        }

        if (len_or_ptr & 0xC0) != 0 {
            // Reserved label type
            return None;
        }

        let label_len = len_or_ptr as usize;
        if label_len > DNS_LABEL_MAX {
            return None;
        }
        pos += 1;
        if pos + label_len > packet.len() {
            return None;
        }

        // Add dot separator between labels
        if out_pos > 0 {
            if out_pos >= out.len() {
                return None;
            }
            out[out_pos] = b'.';
            out_pos += 1;
        }

        if out_pos + label_len > out.len() {
            return None;
        }
        out[out_pos..out_pos + label_len].copy_from_slice(&packet[pos..pos + label_len]);
        out_pos += label_len;
        pos += label_len;
    }

    if !jumped {
        // wire_consumed already set in the loop
    }

    Some((out_pos, wire_consumed))
}

// =============================================================================
// DNS response parsing
// =============================================================================

/// Parse a DNS response packet and extract the first A record.
///
/// Chases CNAME records up to `MAX_CNAME_HOPS` deep.
pub fn dns_parse_response(packet: &[u8], expected_id: u16) -> Option<DnsResponse> {
    let header = DnsHeader::from_bytes(packet)?;

    // Validate response
    if !header.qr() {
        return None; // Not a response
    }
    if header.id != expected_id {
        return None; // ID mismatch
    }
    let rcode = header.rcode();
    if rcode != DnsRcode::NoError as u8 {
        return None; // Error response
    }

    // Skip question section
    let mut pos = DNS_HEADER_LEN;
    for _ in 0..header.qdcount {
        // Skip QNAME
        pos = skip_dns_name(packet, pos)?;
        // Skip QTYPE + QCLASS
        if pos + 4 > packet.len() {
            return None;
        }
        pos += 4;
    }

    // Parse answer section, chasing CNAMEs
    let mut a_addr: Option<([u8; 4], u32)> = None;
    let mut _cname_hops = 0usize;

    for _ in 0..header.ancount {
        if pos >= packet.len() {
            break;
        }

        // Skip RR name
        let name_end = skip_dns_name(packet, pos)?;
        pos = name_end;

        // Read TYPE, CLASS, TTL, RDLENGTH
        if pos + 10 > packet.len() {
            return None;
        }
        let rr_type = u16::from_be_bytes([packet[pos], packet[pos + 1]]);
        let _rr_class = u16::from_be_bytes([packet[pos + 2], packet[pos + 3]]);
        let ttl = u32::from_be_bytes([
            packet[pos + 4],
            packet[pos + 5],
            packet[pos + 6],
            packet[pos + 7],
        ]);
        let rdlength = u16::from_be_bytes([packet[pos + 8], packet[pos + 9]]) as usize;
        pos += 10;

        if pos + rdlength > packet.len() {
            return None;
        }

        if rr_type == DnsType::A as u16 && rdlength == 4 {
            let mut addr = [0u8; 4];
            addr.copy_from_slice(&packet[pos..pos + 4]);
            klog_debug!(
                "dns: A record at offset {} -> {}.{}.{}.{} (raw: {:02x?})",
                pos,
                addr[0],
                addr[1],
                addr[2],
                addr[3],
                &packet[pos..pos + 4]
            );
            a_addr = Some((addr, ttl));
            // Don't break — continue to find the best answer
        } else if rr_type == DnsType::CNAME as u16 {
            _cname_hops += 1;
            if _cname_hops > MAX_CNAME_HOPS {
                return None;
            }
            // CNAME: the A record should follow for the canonical name.
            // We continue parsing answers — the A record for the CNAME
            // target typically appears later in the answer section.
        }

        pos += rdlength;
    }

    a_addr.map(|(addr, ttl)| DnsResponse { addr, ttl })
}

/// Skip a DNS name in wire format, returning the offset after it.
fn skip_dns_name(packet: &[u8], mut pos: usize) -> Option<usize> {
    let mut pointer_count = 0usize;
    loop {
        if pos >= packet.len() {
            return None;
        }
        let b = packet[pos];
        if b == 0 {
            return Some(pos + 1);
        }
        if (b & 0xC0) == 0xC0 {
            // Compression pointer — 2 bytes, doesn't recurse for skipping
            if pos + 1 >= packet.len() {
                return None;
            }
            return Some(pos + 2);
        }
        if (b & 0xC0) != 0 {
            return None; // Reserved
        }
        let label_len = b as usize;
        pos += 1 + label_len;
        pointer_count += 1;
        if pointer_count > MAX_POINTER_FOLLOWS {
            return None;
        }
    }
}

// =============================================================================
// DNS cache
// =============================================================================

#[derive(Clone, Copy)]
struct DnsCacheEntry {
    /// FNV-1a hash of the hostname for fast comparison.
    hostname_hash: u32,
    /// Resolved IPv4 address.
    addr: [u8; 4],
    /// Absolute expiry time in ms (from `clock::uptime_ms()`).
    expiry_ms: u64,
    /// Last-used timestamp for LRU eviction.
    last_used_ms: u64,
    /// Whether this entry is occupied.
    valid: bool,
}

impl DnsCacheEntry {
    const fn empty() -> Self {
        Self {
            hostname_hash: 0,
            addr: [0; 4],
            expiry_ms: 0,
            last_used_ms: 0,
            valid: false,
        }
    }
}

struct DnsCache {
    entries: [DnsCacheEntry; DNS_CACHE_SIZE],
}

impl DnsCache {
    const fn new() -> Self {
        Self {
            entries: [DnsCacheEntry::empty(); DNS_CACHE_SIZE],
        }
    }

    fn lookup(&mut self, hostname: &[u8]) -> Option<[u8; 4]> {
        let hash = fnv1a_hash(hostname);
        let now = slopos_utils::clock::uptime_ms();

        for entry in self.entries.iter_mut() {
            if entry.valid && entry.hostname_hash == hash {
                if now < entry.expiry_ms {
                    entry.last_used_ms = now;
                    return Some(entry.addr);
                } else {
                    // TTL expired
                    entry.valid = false;
                    return None;
                }
            }
        }
        None
    }

    fn insert(&mut self, hostname: &[u8], addr: [u8; 4], ttl_secs: u32) {
        let hash = fnv1a_hash(hostname);
        let now = slopos_utils::clock::uptime_ms();
        // Minimum TTL of 60s to avoid thrashing
        let ttl_ms = (ttl_secs.max(60) as u64) * 1000;

        // Check if already cached — update in place
        for entry in self.entries.iter_mut() {
            if entry.valid && entry.hostname_hash == hash {
                entry.addr = addr;
                entry.expiry_ms = now + ttl_ms;
                entry.last_used_ms = now;
                return;
            }
        }

        // Find a free slot
        for entry in self.entries.iter_mut() {
            if !entry.valid {
                *entry = DnsCacheEntry {
                    hostname_hash: hash,
                    addr,
                    expiry_ms: now + ttl_ms,
                    last_used_ms: now,
                    valid: true,
                };
                return;
            }
        }

        // Evict LRU entry
        let mut lru_idx = 0usize;
        let mut lru_time = u64::MAX;
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.last_used_ms < lru_time {
                lru_time = entry.last_used_ms;
                lru_idx = i;
            }
        }
        self.entries[lru_idx] = DnsCacheEntry {
            hostname_hash: hash,
            addr,
            expiry_ms: now + ttl_ms,
            last_used_ms: now,
            valid: true,
        };
    }

    fn flush(&mut self) {
        for entry in self.entries.iter_mut() {
            entry.valid = false;
        }
    }
}

/// FNV-1a hash for hostname cache keys.
fn fnv1a_hash(data: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in data {
        // Case-insensitive: lowercase ASCII letters
        let b = if byte >= b'A' && byte <= b'Z' {
            byte | 0x20
        } else {
            byte
        };
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

static DNS_CACHE: IrqMutex<DnsCache> = IrqMutex::new(DnsCache::new());

/// Look up a hostname in the DNS cache.
pub fn dns_cache_lookup(hostname: &[u8]) -> Option<[u8; 4]> {
    DNS_CACHE.lock().lookup(hostname)
}

/// Insert a resolved address into the DNS cache.
pub fn dns_cache_insert(hostname: &[u8], addr: [u8; 4], ttl_secs: u32) {
    DNS_CACHE.lock().insert(hostname, addr, ttl_secs);
}

/// Flush the entire DNS cache.
pub fn dns_cache_flush() {
    DNS_CACHE.lock().flush();
}

// =============================================================================
// Resolver
// =============================================================================

/// Resolve a hostname to an IPv4 address using the kernel's DNS client.
///
/// 1. Checks the DNS cache
/// 2. Sends a DNS query to the DHCP-provided DNS server
/// 3. Waits for a response with timeout
/// 4. Parses and caches the result
/// 5. Retries once on timeout
pub fn dns_resolve(hostname: &[u8]) -> Option<[u8; 4]> {
    // Shortcut: if it looks like an IP literal, parse it
    if let Some(addr) = parse_ip_literal(hostname) {
        return Some(addr);
    }

    // Check cache first
    if let Some(addr) = dns_cache_lookup(hostname) {
        klog_debug!(
            "dns: cache hit for {:?} -> {}.{}.{}.{}",
            core::str::from_utf8(hostname).unwrap_or("?"),
            addr[0],
            addr[1],
            addr[2],
            addr[3]
        );
        return Some(addr);
    }

    // Get DNS server IP from DHCP lease
    let dns_server = crate::net_driver_service::net_driver().and_then(|d| (d.virtio_net_dns)())?;
    if dns_server == [0; 4] {
        klog_debug!("dns: no DNS server configured");
        return None;
    }

    // Get our IP for source address
    let src_ip = crate::netstack::NET_STACK
        .first_ipv4()
        .map(|ip| ip.0)
        .unwrap_or([0; 4]);

    for attempt in 0..DNS_MAX_RETRIES {
        let id = QUERY_ID.fetch_add(1, Ordering::Relaxed);

        // Build query
        let mut query_buf = [0u8; 512];
        let query_len = dns_build_query(id, hostname, DnsType::A, &mut query_buf)?;

        // Use an ephemeral source port
        let src_port = 49152 + (id % 16384);

        // Clear any stale RX data
        if let Some(d) = crate::net_driver_service::net_driver() {
            (d.dns_rx_clear)();
        }

        // Send query
        let transmitted = crate::net_driver_service::net_driver()
            .map(|d| {
                (d.transmit_udp_packet)(
                    src_ip,
                    dns_server,
                    src_port,
                    DNS_PORT,
                    &query_buf[..query_len],
                )
            })
            .unwrap_or(false);
        if !transmitted {
            klog_debug!("dns: transmit failed (attempt {})", attempt);
            continue;
        }

        // Wait for response
        let got_response = crate::net_driver_service::net_driver()
            .map(|d| (d.dns_rx_wait)(DNS_TIMEOUT_MS))
            .unwrap_or(false);
        if !got_response {
            klog_debug!("dns: timeout (attempt {})", attempt);
            continue;
        }

        // Read response
        let mut resp_buf = [0u8; DNS_MAX_RESPONSE];
        let resp_len = crate::net_driver_service::net_driver()
            .map(|d| (d.dns_rx_read)(&mut resp_buf))
            .unwrap_or(0);
        if resp_len == 0 {
            klog_debug!("dns: empty response (attempt {})", attempt);
            continue;
        }

        klog_debug!(
            "dns: response {} bytes, first 32: {:02x?}",
            resp_len,
            &resp_buf[..resp_len.min(32)]
        );

        // Parse response
        if let Some(response) = dns_parse_response(&resp_buf[..resp_len], id) {
            klog_debug!(
                "dns: resolved {:?} -> {}.{}.{}.{} (ttl={}s)",
                core::str::from_utf8(hostname).unwrap_or("?"),
                response.addr[0],
                response.addr[1],
                response.addr[2],
                response.addr[3],
                response.ttl
            );
            dns_cache_insert(hostname, response.addr, response.ttl);
            return Some(response.addr);
        }

        klog_debug!("dns: parse failed (attempt {})", attempt);
    }

    None
}

/// Try to parse a dotted-decimal IPv4 literal (e.g., `"10.0.2.3"`).
fn parse_ip_literal(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut octet_idx = 0usize;
    let mut current: u16 = 0;
    let mut digit_count = 0usize;

    for &b in s {
        if b == b'.' {
            if digit_count == 0 || octet_idx >= 3 {
                return None;
            }
            if current > 255 {
                return None;
            }
            octets[octet_idx] = current as u8;
            octet_idx += 1;
            current = 0;
            digit_count = 0;
        } else if b >= b'0' && b <= b'9' {
            current = current * 10 + (b - b'0') as u16;
            digit_count += 1;
            if digit_count > 3 {
                return None;
            }
        } else {
            return None;
        }
    }

    if digit_count == 0 || octet_idx != 3 || current > 255 {
        return None;
    }
    octets[3] = current as u8;
    Some(octets)
}
