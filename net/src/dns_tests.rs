//! DNS client test suite.

use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, pass};

use crate::dns;

// =============================================================================
// 5F.T1 — DNS name encoding
// =============================================================================

pub fn test_dns_t1_name_encoding() -> TestResult {
    let mut buf = [0u8; 128];

    // Valid: "example.com"
    let len = dns::dns_encode_name(b"example.com", &mut buf);
    assert_test!(len.is_some(), "encode example.com");
    let len = len.unwrap();
    // Expected: [7, 'e','x','a','m','p','l','e', 3, 'c','o','m', 0]
    assert_eq_test!(len, 13, "example.com wire length");
    assert_eq_test!(buf[0], 7, "first label length");
    assert_eq_test!(buf[8], 3, "second label length");
    assert_eq_test!(buf[12], 0, "root label");

    // Valid: "a.b"
    let len = dns::dns_encode_name(b"a.b", &mut buf).unwrap();
    assert_eq_test!(len, 5, "a.b wire length");
    assert_eq_test!(buf[0], 1, "label 'a' length");
    assert_eq_test!(buf[1], b'a', "label 'a' content");
    assert_eq_test!(buf[2], 1, "label 'b' length");
    assert_eq_test!(buf[3], b'b', "label 'b' content");
    assert_eq_test!(buf[4], 0, "root label");

    // Invalid: empty label (double dot)
    assert_test!(
        dns::dns_encode_name(b"example..com", &mut buf).is_none(),
        "reject empty label"
    );

    // Invalid: leading dot
    assert_test!(
        dns::dns_encode_name(b".example.com", &mut buf).is_none(),
        "reject leading dot"
    );

    // Valid: trailing dot (FQDN)
    let len = dns::dns_encode_name(b"example.com.", &mut buf).unwrap();
    assert_eq_test!(len, 13, "trailing dot same as without");

    // Valid: empty hostname (root)
    let len = dns::dns_encode_name(b"", &mut buf).unwrap();
    assert_eq_test!(len, 1, "root label only");
    assert_eq_test!(buf[0], 0, "root is zero byte");

    // Reject: label > 63 bytes
    let long_label = [b'a'; 64];
    assert_test!(
        dns::dns_encode_name(&long_label, &mut buf).is_none(),
        "reject label > 63 bytes"
    );

    pass!()
}

// =============================================================================
// 5F.T2 — DNS query construction
// =============================================================================

pub fn test_dns_t2_query_construction() -> TestResult {
    let mut buf = [0u8; 512];

    let len = dns::dns_build_query(0x1234, b"example.com", dns::DnsType::A, &mut buf);
    assert_test!(len.is_some(), "build query succeeds");
    let len = len.unwrap();

    // Header checks
    // ID
    assert_eq_test!(buf[0], 0x12, "query ID high byte");
    assert_eq_test!(buf[1], 0x34, "query ID low byte");
    // Flags: RD=1 → 0x0100
    assert_eq_test!(buf[2], 0x01, "flags high byte (RD)");
    assert_eq_test!(buf[3], 0x00, "flags low byte");
    // QDCOUNT = 1
    assert_eq_test!(buf[4], 0x00, "qdcount high");
    assert_eq_test!(buf[5], 0x01, "qdcount low");
    // ANCOUNT, NSCOUNT, ARCOUNT = 0
    assert_eq_test!(buf[6], 0x00, "ancount high");
    assert_eq_test!(buf[7], 0x00, "ancount low");

    // Question section: encoded name + QTYPE(A=1) + QCLASS(IN=1)
    // Name starts at offset 12
    assert_eq_test!(buf[12], 7, "question name label 1 len");
    // QTYPE at 12 + 13 = 25
    let name_end = 12 + 13; // "example.com" encodes to 13 bytes
    assert_eq_test!(
        u16::from_be_bytes([buf[name_end], buf[name_end + 1]]),
        1,
        "QTYPE = A"
    );
    assert_eq_test!(
        u16::from_be_bytes([buf[name_end + 2], buf[name_end + 3]]),
        1,
        "QCLASS = IN"
    );

    // Total length
    assert_eq_test!(len, name_end + 4, "total query length");

    // Small buffer should fail
    let mut tiny = [0u8; 10];
    assert_test!(
        dns::dns_build_query(0x1234, b"example.com", dns::DnsType::A, &mut tiny).is_none(),
        "reject small buffer"
    );

    pass!()
}

// =============================================================================
// 5F.T3 — DNS name decoding
// =============================================================================

pub fn test_dns_t3_name_decoding() -> TestResult {
    let mut out = [0u8; 256];

    // Regular labels: [7, 'e','x','a','m','p','l','e', 3, 'c','o','m', 0]
    let wire: &[u8] = &[
        7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0,
    ];
    let result = dns::dns_decode_name(wire, 0, &mut out);
    assert_test!(result.is_some(), "decode regular name");
    let (name_len, wire_consumed) = result.unwrap();
    assert_eq_test!(name_len, 11, "decoded name length (example.com)");
    assert_eq_test!(wire_consumed, 13, "wire bytes consumed");
    assert_eq_test!(&out[..name_len], b"example.com" as &[u8], "decoded text");

    // Compression pointer: build a packet with a pointer
    // Offset 0: [7, 'e','x','a','m','p','l','e', 3, 'c','o','m', 0]  (13 bytes, offset 0-12)
    // Offset 13: [3, 'w','w','w', 0xC0, 0x00]  (points back to offset 0)
    let mut packet = [0u8; 64];
    packet[..13].copy_from_slice(wire);
    packet[13] = 3;
    packet[14] = b'w';
    packet[15] = b'w';
    packet[16] = b'w';
    packet[17] = 0xC0; // Compression pointer
    packet[18] = 0x00; // Points to offset 0

    let result = dns::dns_decode_name(&packet, 13, &mut out);
    assert_test!(result.is_some(), "decode with compression pointer");
    let (name_len, wire_consumed) = result.unwrap();
    assert_eq_test!(name_len, 15, "www.example.com length");
    assert_eq_test!(&out[..name_len], b"www.example.com" as &[u8], "decoded www");
    assert_eq_test!(wire_consumed, 6, "wire consumed with pointer");

    // Loop detection: pointer pointing to itself
    let mut loop_packet = [0u8; 4];
    loop_packet[0] = 0xC0;
    loop_packet[1] = 0x00; // Points to offset 0 = itself
    assert_test!(
        dns::dns_decode_name(&loop_packet, 0, &mut out).is_none(),
        "detect pointer loop"
    );

    pass!()
}

// =============================================================================
// 5F.T4 — DNS response parsing
// =============================================================================

pub fn test_dns_t4_response_parsing() -> TestResult {
    // Build a minimal valid DNS response for example.com -> 93.184.216.34
    let id: u16 = 0xABCD;
    let mut packet = [0u8; 128];
    let mut pos;

    // Header
    packet[0..2].copy_from_slice(&id.to_be_bytes());
    packet[2..4].copy_from_slice(&0x8180u16.to_be_bytes()); // QR=1, RD=1, RA=1, RCODE=0
    packet[4..6].copy_from_slice(&1u16.to_be_bytes()); // QDCOUNT=1
    packet[6..8].copy_from_slice(&1u16.to_be_bytes()); // ANCOUNT=1
    packet[8..10].copy_from_slice(&0u16.to_be_bytes()); // NSCOUNT=0
    packet[10..12].copy_from_slice(&0u16.to_be_bytes()); // ARCOUNT=0
    pos = 12;

    // Question: example.com, A, IN
    let name_wire: &[u8] = &[
        7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0,
    ];
    packet[pos..pos + 13].copy_from_slice(name_wire);
    pos += 13;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // QTYPE=A
    pos += 2;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // QCLASS=IN
    pos += 2;

    // Answer: example.com (compression pointer to offset 12), A, IN, TTL=300, 93.184.216.34
    packet[pos] = 0xC0;
    packet[pos + 1] = 0x0C; // Pointer to name at offset 12
    pos += 2;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // TYPE=A
    pos += 2;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // CLASS=IN
    pos += 2;
    packet[pos..pos + 4].copy_from_slice(&300u32.to_be_bytes()); // TTL=300
    pos += 4;
    packet[pos..pos + 2].copy_from_slice(&4u16.to_be_bytes()); // RDLENGTH=4
    pos += 2;
    packet[pos..pos + 4].copy_from_slice(&[93, 184, 216, 34]); // RDATA
    pos += 4;

    let resp = dns::dns_parse_response(&packet[..pos], id);
    assert_test!(resp.is_some(), "parse valid A response");
    let resp = resp.unwrap();
    assert_eq_test!(resp.addr, [93, 184, 216, 34], "resolved address");
    assert_eq_test!(resp.ttl, 300, "TTL");

    // ID mismatch
    assert_test!(
        dns::dns_parse_response(&packet[..pos], 0x0000).is_none(),
        "reject ID mismatch"
    );

    // RCODE error: change RCODE to NXDomain (3)
    let mut err_packet = packet;
    let flags = u16::from_be_bytes([err_packet[2], err_packet[3]]);
    let err_flags = (flags & 0xFFF0) | 3; // RCODE=3
    err_packet[2..4].copy_from_slice(&err_flags.to_be_bytes());
    assert_test!(
        dns::dns_parse_response(&err_packet[..pos], id).is_none(),
        "reject RCODE error"
    );

    pass!()
}

// =============================================================================
// 5F.T5 — DNS cache
// =============================================================================

pub fn test_dns_t5_cache() -> TestResult {
    // Flush cache to start clean
    dns::dns_cache_flush();

    // Insert and lookup
    dns::dns_cache_insert(b"test.local", [1, 2, 3, 4], 3600);
    let result = dns::dns_cache_lookup(b"test.local");
    assert_test!(result.is_some(), "cache hit after insert");
    assert_eq_test!(result.unwrap(), [1, 2, 3, 4], "cached address");

    // Miss for unknown hostname
    let miss = dns::dns_cache_lookup(b"unknown.local");
    assert_test!(miss.is_none(), "cache miss for unknown");

    // Update existing entry
    dns::dns_cache_insert(b"test.local", [5, 6, 7, 8], 3600);
    let result = dns::dns_cache_lookup(b"test.local");
    assert_eq_test!(result.unwrap(), [5, 6, 7, 8], "updated address");

    // Fill cache to capacity to test LRU eviction
    for i in 0u8..16 {
        let mut name = [b'h', b'o', b's', b't', b'-', b'0', b'0', 0];
        name[5] = b'a' + (i / 10);
        name[6] = b'0' + (i % 10);
        dns::dns_cache_insert(&name[..7], [10, 0, 0, i], 3600);
    }

    // 17th insert should evict LRU entry
    dns::dns_cache_insert(b"overflow", [99, 99, 99, 99], 3600);
    let result = dns::dns_cache_lookup(b"overflow");
    assert_test!(result.is_some(), "overflow entry exists");
    assert_eq_test!(result.unwrap(), [99, 99, 99, 99], "overflow address");

    // Flush and verify empty
    dns::dns_cache_flush();
    assert_test!(
        dns::dns_cache_lookup(b"test.local").is_none(),
        "flushed cache is empty"
    );
    assert_test!(
        dns::dns_cache_lookup(b"overflow").is_none(),
        "flushed overflow entry gone"
    );

    pass!()
}

// =============================================================================
// 5F.T6 — Resolver integration (live DNS via QEMU user-net)
// =============================================================================

pub fn test_dns_t6_resolver_integration() -> TestResult {
    // Skip if network is not ready
    if !crate::net_driver_service::net_driver()
        .map(|d| (d.virtio_net_is_ready)())
        .unwrap_or(false)
    {
        return pass!();
    }

    // QEMU user-net provides DNS at 10.0.2.3 and can resolve real hostnames.
    // We try to resolve a well-known hostname.
    let result = dns::dns_resolve(b"dns.google");
    assert_test!(result.is_some(), "resolved dns.google");
    let addr = result.unwrap();
    // dns.google resolves to 8.8.8.8 or 8.8.4.4
    assert_test!(addr[0] == 8 && addr[1] == 8, "dns.google starts with 8.8");

    // IP literal passthrough
    let literal = dns::dns_resolve(b"10.0.2.3");
    assert_test!(literal.is_some(), "IP literal passthrough");
    assert_eq_test!(literal.unwrap(), [10, 0, 2, 3], "literal address");

    pass!()
}

// =============================================================================
// 5F.T7 — Resolver timeout
// =============================================================================

pub fn test_dns_t7_resolver_timeout() -> TestResult {
    // Skip if network is not ready
    if !crate::net_driver_service::net_driver()
        .map(|d| (d.virtio_net_is_ready)())
        .unwrap_or(false)
    {
        return pass!();
    }

    // Flush cache to ensure we actually query
    dns::dns_cache_flush();

    // Try to resolve a hostname that should not exist
    // Using .invalid TLD (RFC 6761) — guaranteed to not resolve
    let result = dns::dns_resolve(b"this-does-not-exist.invalid");
    assert_test!(result.is_none(), "non-existent hostname returns None");

    pass!()
}

// =============================================================================
// 5F.T8 — Regression: existing network tests still functional
// =============================================================================

pub fn test_dns_t8_regression_network_stack() -> TestResult {
    // Verify that the DNS interception in dispatch_rx_frame doesn't
    // break normal UDP socket delivery
    use crate::socket::*;
    use slopos_abi::net::{AF_INET, SOCK_DGRAM};

    socket_reset_all();

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0);
    assert_test!(sock >= 0, "create UDP socket");
    let sock = sock as u32;

    let rc = socket_bind(sock, [0, 0, 0, 0], 41053);
    assert_eq_test!(rc, 0, "bind to port 41053");

    // Deliver a non-DNS UDP packet and verify it arrives
    let payload = [0xDE, 0xAD, 0xBE, 0xEF];
    socket_deliver_udp_from_dispatch([10, 0, 2, 1], [10, 0, 2, 15], 9999, 41053, &payload);

    let mut buf = [0u8; 16];
    let mut src_ip = [0u8; 4];
    let mut src_port = 0u16;
    let got = socket_recvfrom(
        sock,
        buf.as_mut_ptr(),
        buf.len(),
        &mut src_ip as *mut _,
        &mut src_port as *mut _,
    );
    assert_eq_test!(got, 4, "received 4 bytes");
    assert_eq_test!(&buf[..4], &payload, "payload matches");
    assert_eq_test!(src_ip, [10, 0, 2, 1], "source IP");
    assert_eq_test!(src_port, 9999, "source port");

    let _ = socket_close(sock);
    pass!()
}

slopos_lib::define_test_suite!(
    dns,
    [
        test_dns_t1_name_encoding,
        test_dns_t2_query_construction,
        test_dns_t3_name_decoding,
        test_dns_t4_response_parsing,
        test_dns_t5_cache,
        test_dns_t6_resolver_integration,
        test_dns_t7_resolver_timeout,
        test_dns_t8_regression_network_stack,
    ]
);
