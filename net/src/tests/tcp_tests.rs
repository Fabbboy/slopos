//! TCP regression tests.
//!
//! Covers: header parsing & construction, checksum computation & verification,
//! sequence number arithmetic, state machine transitions, connection table
//! management, three-way handshake (active open and passive open), connection
//! teardown (active close, passive close, simultaneous close), RST handling,
//! MSS option parsing, ephemeral port allocation, and TIME_WAIT expiry.
//!
//! All tests run in-kernel during the integration test harness (`itests=on`).

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::table::MAX_CONNECTIONS;
use crate::tcp::{
    self, ConnId, DEFAULT_MSS, DEFAULT_WINDOW_SIZE, SocketNotify, TCP_FLAG_ACK, TCP_FLAG_FIN,
    TCP_FLAG_PSH, TCP_FLAG_RST, TCP_FLAG_SYN, TCP_FLAG_URG, TcpError, TcpHeader, TcpState,
    TcpTuple,
};
use crate::with_data_state;

// =============================================================================
// Helper: reset global state before each test
// =============================================================================

use crate::tests::tcp_common::reset_all as reset;

// =============================================================================
// 1. Header parsing
// =============================================================================

/// Build a minimal valid TCP header in wire format (big-endian).
fn make_wire_header(
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    data_offset: u8,
    flags: u8,
    window: u16,
) -> [u8; 20] {
    let mut buf = [0u8; 20];
    buf[0..2].copy_from_slice(&src_port.to_be_bytes());
    buf[2..4].copy_from_slice(&dst_port.to_be_bytes());
    buf[4..8].copy_from_slice(&seq.to_be_bytes());
    buf[8..12].copy_from_slice(&ack.to_be_bytes());
    buf[12] = (data_offset << 4) & 0xF0;
    buf[13] = flags & 0x3F;
    buf[14..16].copy_from_slice(&window.to_be_bytes());
    // checksum = 0, urgent_ptr = 0
    buf
}

pub fn test_tcp_parse_minimal_header() -> TestResult {
    let buf = make_wire_header(8080, 80, 1000, 2000, 5, TCP_FLAG_SYN | TCP_FLAG_ACK, 32768);
    let hdr = match tcp::parse_header(&buf) {
        Some(h) => h,
        None => return fail!("parse_header returned None for valid header"),
    };
    assert_eq_test!(hdr.src_port, 8080, "src_port");
    assert_eq_test!(hdr.dst_port, 80, "dst_port");
    assert_eq_test!(hdr.seq_num, 1000, "seq_num");
    assert_eq_test!(hdr.ack_num, 2000, "ack_num");
    assert_eq_test!(hdr.data_offset, 5, "data_offset");
    assert_test!(hdr.is_syn(), "SYN flag");
    assert_test!(hdr.is_ack(), "ACK flag");
    assert_test!(hdr.is_syn_ack(), "SYN+ACK");
    assert_test!(!hdr.is_fin(), "FIN not set");
    assert_test!(!hdr.is_rst(), "RST not set");
    assert_eq_test!(hdr.window_size, 32768, "window_size");
    assert_eq_test!(hdr.header_len(), 20, "header_len");
    assert_eq_test!(hdr.options_len(), 0, "options_len");
    pass!()
}

pub fn test_tcp_parse_too_short() -> TestResult {
    let buf = [0u8; 19];
    assert_test!(
        tcp::parse_header(&buf).is_none(),
        "parse 19 bytes should fail"
    );
    assert_test!(
        tcp::parse_header(&[]).is_none(),
        "parse 0 bytes should fail"
    );
    pass!()
}

pub fn test_tcp_parse_invalid_data_offset() -> TestResult {
    // data_offset = 4 (< 5 minimum) should fail.
    let mut buf = make_wire_header(1, 2, 0, 0, 5, 0, 0);
    buf[12] = (4 << 4) & 0xF0;
    assert_test!(
        tcp::parse_header(&buf).is_none(),
        "data_offset=4 should fail"
    );

    // data_offset = 0 should fail.
    buf[12] = 0;
    assert_test!(
        tcp::parse_header(&buf).is_none(),
        "data_offset=0 should fail"
    );
    pass!()
}

pub fn test_tcp_parse_with_options() -> TestResult {
    // data_offset = 6 (24 bytes) — need at least 24 bytes of data.
    let mut buf = [0u8; 24];
    buf[0..2].copy_from_slice(&1234u16.to_be_bytes());
    buf[2..4].copy_from_slice(&5678u16.to_be_bytes());
    buf[12] = (6 << 4) & 0xF0;
    buf[13] = TCP_FLAG_SYN;
    buf[14..16].copy_from_slice(&8192u16.to_be_bytes());

    let hdr = match tcp::parse_header(&buf) {
        Some(h) => h,
        None => return fail!("parse with options returned None"),
    };
    assert_eq_test!(hdr.data_offset, 6, "data_offset=6");
    assert_eq_test!(hdr.header_len(), 24, "header_len=24");
    assert_eq_test!(hdr.options_len(), 4, "options_len=4");
    pass!()
}

pub fn test_tcp_parse_data_offset_exceeds_buffer() -> TestResult {
    // data_offset = 15 (60 bytes) but buffer only 20 bytes.
    let mut buf = make_wire_header(1, 2, 0, 0, 5, 0, 0);
    buf[12] = (15 << 4) & 0xF0;
    assert_test!(
        tcp::parse_header(&buf).is_none(),
        "data_offset=15 with 20-byte buf should fail"
    );
    pass!()
}

pub fn test_tcp_parse_all_flags() -> TestResult {
    let buf = make_wire_header(
        100,
        200,
        0,
        0,
        5,
        TCP_FLAG_SYN | TCP_FLAG_ACK | TCP_FLAG_FIN | TCP_FLAG_RST | TCP_FLAG_PSH | TCP_FLAG_URG,
        0,
    );
    let hdr = tcp::parse_header(&buf).unwrap();
    assert_test!(hdr.is_syn(), "SYN");
    assert_test!(hdr.is_ack(), "ACK");
    assert_test!(hdr.is_fin(), "FIN");
    assert_test!(hdr.is_rst(), "RST");
    assert_test!(hdr.is_psh(), "PSH");
    assert_test!(hdr.is_urg(), "URG");
    assert_test!(hdr.is_syn_ack(), "SYN+ACK");
    assert_test!(hdr.is_fin_ack(), "FIN+ACK");
    pass!()
}

// =============================================================================
// 2. Header construction
// =============================================================================

pub fn test_tcp_write_header_roundtrip() -> TestResult {
    let orig = tcp::build_header(4321, 80, 0xDEADBEEF, 0xCAFEBABE, TCP_FLAG_ACK, 16384, 5);
    let mut buf = [0u8; 20];
    let written = match tcp::write_header(&orig, &mut buf) {
        Some(n) => n,
        None => return fail!("write_header returned None"),
    };
    assert_eq_test!(written, 20, "wrote 20 bytes");

    let parsed = match tcp::parse_header(&buf) {
        Some(h) => h,
        None => return fail!("parse after write returned None"),
    };
    assert_eq_test!(parsed.src_port, 4321, "roundtrip src_port");
    assert_eq_test!(parsed.dst_port, 80, "roundtrip dst_port");
    assert_eq_test!(parsed.seq_num, 0xDEADBEEF, "roundtrip seq_num");
    assert_eq_test!(parsed.ack_num, 0xCAFEBABE, "roundtrip ack_num");
    assert_eq_test!(parsed.data_offset, 5, "roundtrip data_offset");
    assert_eq_test!(parsed.flags, TCP_FLAG_ACK, "roundtrip flags");
    assert_eq_test!(parsed.window_size, 16384, "roundtrip window_size");
    assert_eq_test!(parsed.checksum, 0, "checksum placeholder is 0");
    assert_eq_test!(parsed.urgent_ptr, 0, "urgent_ptr");
    pass!()
}

pub fn test_tcp_write_header_buffer_too_small() -> TestResult {
    let hdr = tcp::build_header(1, 2, 0, 0, 0, 0, 5);
    let mut buf = [0u8; 19];
    assert_test!(
        tcp::write_header(&hdr, &mut buf).is_none(),
        "buffer too small"
    );
    pass!()
}

pub fn test_tcp_write_header_with_options() -> TestResult {
    let hdr = tcp::build_header(1, 2, 0, 0, TCP_FLAG_SYN, 8192, 6); // 24 bytes
    let mut buf = [0u8; 24];
    let written = match tcp::write_header(&hdr, &mut buf) {
        Some(n) => n,
        None => return fail!("write 24-byte header failed"),
    };
    assert_eq_test!(written, 24, "wrote 24 bytes");
    // Options area should be zeroed.
    assert_eq_test!(buf[20], 0, "option byte 0");
    assert_eq_test!(buf[21], 0, "option byte 1");
    assert_eq_test!(buf[22], 0, "option byte 2");
    assert_eq_test!(buf[23], 0, "option byte 3");
    pass!()
}

// =============================================================================
// 3. MSS option parsing
// =============================================================================

pub fn test_tcp_parse_mss_option() -> TestResult {
    // MSS option: kind=2, len=4, value=1460
    let opts = [
        tcp::TCP_OPT_MSS,
        tcp::TCP_OPT_MSS_LEN,
        0x05,
        0xB4, // 1460 big-endian
    ];
    let mss = match tcp::parse_tcp_options(&opts).mss {
        Some(m) => m,
        None => return fail!("parse_tcp_options returned None for MSS"),
    };
    assert_eq_test!(mss, 1460, "MSS should be 1460");
    pass!()
}

pub fn test_tcp_parse_mss_option_with_nop_padding() -> TestResult {
    // NOP + MSS option
    let opts = [
        tcp::TCP_OPT_NOP,
        tcp::TCP_OPT_MSS,
        tcp::TCP_OPT_MSS_LEN,
        0x02,
        0x18, // 536 big-endian
    ];
    let mss = match tcp::parse_tcp_options(&opts).mss {
        Some(m) => m,
        None => return fail!("MSS with NOP padding returned None"),
    };
    assert_eq_test!(mss, 536, "MSS should be 536");
    pass!()
}

pub fn test_tcp_parse_mss_option_not_present() -> TestResult {
    let opts = [tcp::TCP_OPT_NOP, tcp::TCP_OPT_END];
    assert_test!(
        tcp::parse_tcp_options(&opts).mss.is_none(),
        "no MSS should return None"
    );
    assert_test!(
        tcp::parse_tcp_options(&[]).mss.is_none(),
        "empty options should return None"
    );
    pass!()
}

pub fn test_tcp_write_mss_option() -> TestResult {
    let mut buf = [0u8; 4];
    let written = match tcp::write_mss_option(1460, &mut buf) {
        Some(n) => n,
        None => return fail!("write_mss_option returned None"),
    };
    assert_eq_test!(written, 4, "MSS option is 4 bytes");
    assert_eq_test!(buf[0], tcp::TCP_OPT_MSS, "kind");
    assert_eq_test!(buf[1], tcp::TCP_OPT_MSS_LEN, "length");
    let val = u16::from_be_bytes([buf[2], buf[3]]);
    assert_eq_test!(val, 1460, "MSS value");
    pass!()
}

pub fn test_tcp_write_mss_option_buffer_too_small() -> TestResult {
    let mut buf = [0u8; 3];
    assert_test!(
        tcp::write_mss_option(1460, &mut buf).is_none(),
        "3-byte buffer should fail"
    );
    pass!()
}

// =============================================================================
// 4. Checksum
// =============================================================================

pub fn test_tcp_checksum_zero_payload() -> TestResult {
    let src_ip = [10, 0, 0, 1];
    let dst_ip = [10, 0, 0, 2];

    // Build a SYN segment with checksum = 0.
    let hdr = tcp::build_header(8080, 80, 1000, 0, TCP_FLAG_SYN, 32768, 5);
    let mut segment = [0u8; 20];
    tcp::write_header(&hdr, &mut segment);

    let csum = tcp::tcp_checksum(src_ip, dst_ip, &segment);
    assert_test!(csum != 0, "checksum should be non-zero");

    // Patch checksum into segment and verify.
    segment[16..18].copy_from_slice(&csum.to_be_bytes());
    assert_test!(
        tcp::verify_checksum(src_ip, dst_ip, &segment),
        "verify should pass after patching"
    );
    pass!()
}

pub fn test_tcp_checksum_with_payload() -> TestResult {
    let src_ip = [192, 168, 1, 100];
    let dst_ip = [192, 168, 1, 1];

    let hdr = tcp::build_header(12345, 80, 5000, 6000, TCP_FLAG_ACK | TCP_FLAG_PSH, 16384, 5);
    let payload = b"Hello, TCP!";
    let mut segment = [0u8; 20 + 11]; // header + payload
    tcp::write_header(&hdr, &mut segment);
    segment[20..31].copy_from_slice(payload);

    let csum = tcp::tcp_checksum(src_ip, dst_ip, &segment);
    segment[16..18].copy_from_slice(&csum.to_be_bytes());
    assert_test!(
        tcp::verify_checksum(src_ip, dst_ip, &segment),
        "verify with payload"
    );
    pass!()
}

pub fn test_tcp_checksum_odd_payload_length() -> TestResult {
    let src_ip = [10, 0, 0, 1];
    let dst_ip = [10, 0, 0, 2];

    let hdr = tcp::build_header(1, 2, 0, 0, TCP_FLAG_ACK, 1024, 5);
    // Odd-length payload to test trailing byte handling.
    let payload = [0xAA, 0xBB, 0xCC];
    let mut segment = [0u8; 23]; // 20 + 3
    tcp::write_header(&hdr, &mut segment);
    segment[20..23].copy_from_slice(&payload);

    let csum = tcp::tcp_checksum(src_ip, dst_ip, &segment);
    segment[16..18].copy_from_slice(&csum.to_be_bytes());
    assert_test!(
        tcp::verify_checksum(src_ip, dst_ip, &segment),
        "verify with odd payload"
    );
    pass!()
}

pub fn test_tcp_checksum_wrong_ip_fails_verify() -> TestResult {
    let src_ip = [10, 0, 0, 1];
    let dst_ip = [10, 0, 0, 2];

    let hdr = tcp::build_header(80, 8080, 100, 200, TCP_FLAG_ACK, 4096, 5);
    let mut segment = [0u8; 20];
    tcp::write_header(&hdr, &mut segment);

    let csum = tcp::tcp_checksum(src_ip, dst_ip, &segment);
    segment[16..18].copy_from_slice(&csum.to_be_bytes());

    // Verify with wrong destination IP should fail.
    let wrong_dst = [10, 0, 0, 99];
    assert_test!(
        !tcp::verify_checksum(src_ip, wrong_dst, &segment),
        "wrong dst_ip should fail verify"
    );

    // Verify with wrong source IP should also fail.
    let wrong_src = [10, 0, 0, 99];
    assert_test!(
        !tcp::verify_checksum(wrong_src, dst_ip, &segment),
        "wrong src_ip should fail verify"
    );
    pass!()
}

pub fn test_tcp_checksum_deterministic() -> TestResult {
    let src_ip = [172, 16, 0, 1];
    let dst_ip = [172, 16, 0, 2];

    let hdr = tcp::build_header(443, 50000, 0xABCD1234, 0, TCP_FLAG_SYN, 65535, 5);
    let mut seg1 = [0u8; 20];
    let mut seg2 = [0u8; 20];
    tcp::write_header(&hdr, &mut seg1);
    tcp::write_header(&hdr, &mut seg2);

    let c1 = tcp::tcp_checksum(src_ip, dst_ip, &seg1);
    let c2 = tcp::tcp_checksum(src_ip, dst_ip, &seg2);
    assert_eq_test!(
        c1,
        c2,
        "identical segments must produce identical checksums"
    );
    pass!()
}

// =============================================================================
// 5. Sequence number arithmetic
// =============================================================================

pub fn test_tcp_seq_lt() -> TestResult {
    assert_test!(tcp::seq_lt(0, 1), "0 < 1");
    assert_test!(tcp::seq_lt(100, 200), "100 < 200");
    assert_test!(!tcp::seq_lt(1, 0), "1 not < 0");
    assert_test!(!tcp::seq_lt(5, 5), "5 not < 5");
    // Wrapping: u32::MAX is "before" 0 in sequence space.
    assert_test!(tcp::seq_lt(u32::MAX, 0), "MAX < 0 (wrapping)");
    assert_test!(tcp::seq_lt(u32::MAX - 10, 5), "MAX-10 < 5 (wrapping)");
    pass!()
}

pub fn test_tcp_seq_le() -> TestResult {
    assert_test!(tcp::seq_le(0, 0), "0 <= 0");
    assert_test!(tcp::seq_le(0, 1), "0 <= 1");
    assert_test!(!tcp::seq_le(1, 0), "1 not <= 0");
    assert_test!(tcp::seq_le(u32::MAX, 0), "MAX <= 0 (wrapping)");
    pass!()
}

pub fn test_tcp_seq_gt() -> TestResult {
    assert_test!(tcp::seq_gt(1, 0), "1 > 0");
    assert_test!(!tcp::seq_gt(0, 0), "0 not > 0");
    assert_test!(tcp::seq_gt(0, u32::MAX), "0 > MAX (wrapping)");
    pass!()
}

pub fn test_tcp_seq_ge() -> TestResult {
    assert_test!(tcp::seq_ge(0, 0), "0 >= 0");
    assert_test!(tcp::seq_ge(1, 0), "1 >= 0");
    assert_test!(!tcp::seq_ge(0, 1), "0 not >= 1");
    pass!()
}

// =============================================================================
// 6. Connection table management
// =============================================================================

pub fn test_tcp_table_initially_empty() -> TestResult {
    reset();
    assert_eq_test!(tcp::active_count(), 0, "table should start empty");
    pass!()
}

pub fn test_tcp_connect_creates_syn_sent() -> TestResult {
    reset();
    let (id, seg) = match tcp::connect([10, 0, 0, 1], [10, 0, 0, 2], 80) {
        Ok(r) => r,
        Err(e) => return fail!("tcp_connect failed: {:?}", e),
    };

    assert_eq_test!(tcp::active_count(), 1, "one active connection");
    let state = tcp::get_state(id);
    assert_eq_test!(state, Some(TcpState::SynSent), "state should be SYN_SENT");

    // Outgoing segment should be SYN.
    assert_test!(seg.flags & TCP_FLAG_SYN != 0, "SYN flag set");
    assert_test!(seg.flags & TCP_FLAG_ACK == 0, "ACK flag not set");
    assert_eq_test!(seg.mss, Some(DEFAULT_MSS), "MSS advertised");
    assert_eq_test!(seg.window_size, DEFAULT_WINDOW_SIZE, "window advertised");

    // Tuple should be correct.
    assert_eq_test!(seg.tuple.remote_ip, [10, 0, 0, 2], "remote IP");
    assert_eq_test!(seg.tuple.remote_port, 80, "remote port");
    assert_eq_test!(seg.tuple.local_ip, [10, 0, 0, 1], "local IP");
    assert_test!(seg.tuple.local_port >= 49152, "ephemeral port");
    pass!()
}

pub fn test_tcp_table_full_returns_error() -> TestResult {
    reset();
    // Fill all slots.
    for i in 0..MAX_CONNECTIONS {
        match tcp::connect([10, 0, 0, 1], [10, 0, 0, 2], (80 + i) as u16) {
            Ok(_) => {}
            Err(e) => return fail!("connect {} failed: {:?}", i, e),
        }
    }
    assert_eq_test!(tcp::active_count(), MAX_CONNECTIONS, "table full");

    // Next connect should fail.
    match tcp::connect([10, 0, 0, 1], [10, 0, 0, 2], 9999) {
        Err(TcpError::TableFull) => {}
        other => return fail!("expected TableFull, got {:?}", other),
    }
    pass!()
}

pub fn test_tcp_listen_creates_listen_state() -> TestResult {
    reset();
    let id = match tcp::listen([0; 4], 8080) {
        Ok(i) => i,
        Err(e) => return fail!("tcp_listen failed: {:?}", e),
    };

    assert_eq_test!(tcp::get_state(id), Some(TcpState::Listen), "LISTEN state");
    assert_eq_test!(tcp::active_count(), 1, "one active");
    pass!()
}

pub fn test_tcp_listen_duplicate_port_fails() -> TestResult {
    reset();
    tcp::listen([0; 4], 8080).unwrap();

    match tcp::listen([0; 4], 8080) {
        Err(TcpError::AddrInUse) => {}
        other => return fail!("expected AddrInUse, got {:?}", other),
    }
    pass!()
}

pub fn test_tcp_close_listen_releases_slot() -> TestResult {
    reset();
    let id = tcp::listen([0; 4], 8080).unwrap();
    assert_eq_test!(tcp::active_count(), 1, "one active");

    let result = tcp::close(id);
    assert_test!(result.is_ok(), "close should succeed");
    assert_test!(result.unwrap().is_none(), "no FIN for listen socket");
    assert_eq_test!(tcp::active_count(), 0, "released");
    pass!()
}

pub fn test_tcp_close_syn_sent_releases_slot() -> TestResult {
    reset();
    let (id, _) = tcp::connect([10, 0, 0, 1], [10, 0, 0, 2], 80).unwrap();
    assert_eq_test!(tcp::active_count(), 1, "one active");

    let result = tcp::close(id).unwrap();
    assert_test!(result.is_none(), "no FIN from SYN_SENT");
    assert_eq_test!(tcp::active_count(), 0, "released");
    pass!()
}

pub fn test_tcp_abort_sends_rst() -> TestResult {
    reset();
    let (id, _) = tcp::connect([10, 0, 0, 1], [10, 0, 0, 2], 80).unwrap();
    let result = tcp::abort(id).unwrap();
    assert_test!(result.is_some(), "RST segment expected");
    let seg = result.unwrap();
    assert_test!(seg.flags & TCP_FLAG_RST != 0, "RST flag");
    assert_eq_test!(tcp::active_count(), 0, "released after abort");
    pass!()
}

pub fn test_tcp_abort_listen_no_rst() -> TestResult {
    reset();
    let id = tcp::listen([0; 4], 80).unwrap();
    let result = tcp::abort(id).unwrap();
    assert_test!(result.is_none(), "no RST for LISTEN");
    assert_eq_test!(tcp::active_count(), 0, "released");
    pass!()
}

pub fn test_tcp_close_not_found() -> TestResult {
    reset();
    match tcp::close(ConnId(999)) {
        Err(TcpError::NotFound) => {}
        other => return fail!("expected NotFound, got {:?}", other),
    }
    pass!()
}

// =============================================================================
// 7. Active open: three-way handshake (client)
// =============================================================================

pub fn test_tcp_active_handshake_complete() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    // Step 1: Client sends SYN.
    let (id, syn_seg) = tcp::connect(local_ip, remote_ip, 80).unwrap();
    assert_eq_test!(tcp::get_state(id), Some(TcpState::SynSent), "SYN_SENT");

    let client_iss = syn_seg.seq_num;
    let client_port = syn_seg.tuple.local_port;

    // Step 2: Server responds with SYN+ACK.
    let server_iss = 5000u32;
    let syn_ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss,
        ack_num: client_iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };

    let result = tcp::input(remote_ip, local_ip, &syn_ack, &[], &[], 0);
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::Established),
        "ESTABLISHED"
    );
    assert_test!(result.segments().next().is_some(), "should send ACK");
    let ack_seg = result.segments().next().unwrap().clone();
    assert_test!(ack_seg.flags & TCP_FLAG_ACK != 0, "ACK flag");
    assert_test!(ack_seg.flags & TCP_FLAG_SYN == 0, "no SYN in ACK");
    assert_eq_test!(ack_seg.ack_num, server_iss.wrapping_add(1), "ACK number");

    // Verify connection state.
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::Established),
        "connection established"
    );
    assert_eq_test!(
        with_data_state!(id, |d| d.irs.raw()),
        server_iss,
        "IRS stored"
    );
    assert_eq_test!(
        with_data_state!(id, |d| d.rcv_nxt.raw()),
        server_iss.wrapping_add(1),
        "rcv_nxt"
    );
    assert_eq_test!(
        with_data_state!(id, |d| d.snd_una.raw()),
        client_iss.wrapping_add(1),
        "snd_una advanced"
    );
    pass!()
}

pub fn test_tcp_active_rst_in_syn_sent() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (id, syn_seg) = tcp::connect(local_ip, remote_ip, 80).unwrap();
    let client_iss = syn_seg.seq_num;
    let client_port = syn_seg.tuple.local_port;

    // Peer sends RST+ACK.
    let rst = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: 0,
        ack_num: client_iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_RST | TCP_FLAG_ACK,
        window_size: 0,
        checksum: 0,
        urgent_ptr: 0,
    };

    let result = tcp::input(remote_ip, local_ip, &rst, &[], &[], 0);
    assert_test!(
        result.notify.contains(SocketNotify::RESET_RECEIVED),
        "reset flag should be set"
    );
    // Released PCB = None state.
    assert_eq_test!(tcp::get_state(id), None, "connection released");
    assert_eq_test!(tcp::active_count(), 0, "connection released");
    pass!()
}

pub fn test_tcp_active_bad_ack_in_syn_sent() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (id, syn_seg) = tcp::connect(local_ip, remote_ip, 80).unwrap();
    let client_port = syn_seg.tuple.local_port;

    // Peer sends SYN+ACK with wrong ack_num.
    let bad_synack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: 5000,
        ack_num: 99999, // Wrong — should be ISS+1.
        data_offset: 5,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };

    let result = tcp::input(remote_ip, local_ip, &bad_synack, &[], &[], 0);
    // Should respond with RST.
    assert_test!(
        result.segments().next().is_some(),
        "should send RST for bad ACK"
    );
    let seg = result.segments().next().unwrap().clone();
    assert_test!(seg.flags & TCP_FLAG_RST != 0, "RST flag");

    // Connection should still be in SYN_SENT (not destroyed by bad ACK).
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::SynSent),
        "still SYN_SENT"
    );
    pass!()
}

pub fn test_tcp_active_mss_negotiation() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (id, syn_seg) = tcp::connect(local_ip, remote_ip, 80).unwrap();
    let client_port = syn_seg.tuple.local_port;
    let client_iss = syn_seg.seq_num;

    // SYN+ACK with MSS option = 536.
    let syn_ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: 7000,
        ack_num: client_iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 4096,
        checksum: 0,
        urgent_ptr: 0,
    };
    let mss_opt = [tcp::TCP_OPT_MSS, tcp::TCP_OPT_MSS_LEN, 0x02, 0x18]; // 536

    let _ = tcp::input(remote_ip, local_ip, &syn_ack, &mss_opt, &[], 0);

    assert_eq_test!(
        with_data_state!(id, |d| d.peer_mss),
        536,
        "peer MSS should be 536"
    );
    assert_eq_test!(
        with_data_state!(id, |d| d.snd_wnd),
        4096,
        "send window from SYN+ACK"
    );
    pass!()
}

// =============================================================================
// 8. Passive open: three-way handshake (server)
// =============================================================================

pub fn test_tcp_passive_handshake_complete() -> TestResult {
    reset();
    let server_ip = [10, 0, 0, 1];
    let client_ip = [10, 0, 0, 2];

    // Step 1: Server listens.
    let listen_id = tcp::listen(server_ip, 80).unwrap();

    // Step 2: Client sends SYN.
    let client_iss = 3000u32;
    let syn = TcpHeader {
        src_port: 50000,
        dst_port: 80,
        seq_num: client_iss,
        ack_num: 0,
        data_offset: 5,
        flags: TCP_FLAG_SYN,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let mss_opt = [tcp::TCP_OPT_MSS, tcp::TCP_OPT_MSS_LEN, 0x05, 0xB4]; // 1460

    let result = tcp::input(client_ip, server_ip, &syn, &mss_opt, &[], 0);
    assert_test!(result.accepted.is_some(), "new connection accepted");

    // Find the child's ConnId via its tuple.
    let child_tuple = TcpTuple {
        local_ip: server_ip,
        local_port: 80,
        remote_ip: client_ip,
        remote_port: 50000,
    };
    let child_id = tcp::find(&child_tuple).expect("child should be in table");
    assert_eq_test!(
        tcp::get_state(child_id),
        Some(TcpState::SynReceived),
        "SYN_RECEIVED"
    );

    // Should respond with SYN+ACK.
    assert_test!(result.segments().next().is_some(), "SYN+ACK response");
    let syn_ack = result.segments().next().unwrap().clone();
    assert_test!(syn_ack.flags & TCP_FLAG_SYN != 0, "SYN flag");
    assert_test!(syn_ack.flags & TCP_FLAG_ACK != 0, "ACK flag");
    assert_eq_test!(
        syn_ack.ack_num,
        client_iss.wrapping_add(1),
        "ACK = client ISS + 1"
    );
    let server_iss = syn_ack.seq_num;

    // Listen socket should still be active.
    assert_eq_test!(
        tcp::get_state(listen_id),
        Some(TcpState::Listen),
        "listen still active"
    );

    // Step 3: Client completes handshake with ACK.
    let ack = TcpHeader {
        src_port: 50000,
        dst_port: 80,
        seq_num: client_iss.wrapping_add(1),
        ack_num: server_iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };

    let _result = tcp::input(client_ip, server_ip, &ack, &[], &[], 0);
    assert_eq_test!(
        tcp::get_state(child_id),
        Some(TcpState::Established),
        "ESTABLISHED"
    );

    assert_eq_test!(
        with_data_state!(child_id, |d| d.peer_mss),
        1460,
        "peer MSS from options"
    );
    assert_eq_test!(
        tcp::with_pcb(child_id, |pcb| pcb.tuple.remote_port).unwrap(),
        50000,
        "remote port"
    );
    assert_eq_test!(
        tcp::with_pcb(child_id, |pcb| pcb.tuple.remote_ip).unwrap(),
        client_ip,
        "remote IP"
    );
    pass!()
}

pub fn test_tcp_passive_rst_in_syn_received() -> TestResult {
    reset();
    let server_ip = [10, 0, 0, 1];
    let client_ip = [10, 0, 0, 2];

    tcp::listen(server_ip, 80).unwrap();

    // Client SYN.
    let syn = TcpHeader {
        src_port: 50000,
        dst_port: 80,
        seq_num: 1000,
        ack_num: 0,
        data_offset: 5,
        flags: TCP_FLAG_SYN,
        window_size: 8192,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _result = tcp::input(client_ip, server_ip, &syn, &[], &[], 0);
    let child_tuple = TcpTuple {
        local_ip: server_ip,
        local_port: 80,
        remote_ip: client_ip,
        remote_port: 50000,
    };
    let child_id = tcp::find(&child_tuple).expect("child should be in table");

    // Client sends RST.
    let rst = TcpHeader {
        src_port: 50000,
        dst_port: 80,
        seq_num: 1001,
        ack_num: 0,
        data_offset: 5,
        flags: TCP_FLAG_RST,
        window_size: 0,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(client_ip, server_ip, &rst, &[], &[], 0);
    assert_test!(
        result.notify.contains(SocketNotify::RESET_RECEIVED),
        "reset flag"
    );
    // Released PCB = None.
    assert_eq_test!(tcp::get_state(child_id), None, "child released");

    // Child connection released, listen still active.
    assert_test!(tcp::get_state(child_id).is_none(), "child released");
    pass!()
}

pub fn test_tcp_passive_ack_to_listen_sends_rst() -> TestResult {
    reset();
    tcp::listen([10, 0, 0, 1], 80).unwrap();

    // Random ACK to a LISTEN socket → should get RST.
    let ack = TcpHeader {
        src_port: 50000,
        dst_port: 80,
        seq_num: 0,
        ack_num: 1234,
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 0,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 0);
    assert_test!(result.segments().next().is_some(), "should send RST");
    let seg = result.segments().next().unwrap().clone();
    assert_test!(seg.flags & TCP_FLAG_RST != 0, "RST flag");
    pass!()
}

// =============================================================================
// 9. Connection teardown
// =============================================================================

/// Helper: establish a connection (client side) and return (id, server_iss, client_port).
fn establish_client_connection(
    local_ip: [u8; 4],
    remote_ip: [u8; 4],
    remote_port: u16,
) -> (ConnId, u32, u16) {
    let (id, syn_seg) = tcp::connect(local_ip, remote_ip, remote_port).unwrap();
    let client_iss = syn_seg.seq_num;
    let client_port = syn_seg.tuple.local_port;

    let server_iss = 5000u32;
    let syn_ack = TcpHeader {
        src_port: remote_port,
        dst_port: client_port,
        seq_num: server_iss,
        ack_num: client_iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(remote_ip, local_ip, &syn_ack, &[], &[], 0);
    (id, server_iss, client_port)
}

pub fn test_tcp_active_close() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (id, server_iss, client_port) = establish_client_connection(local_ip, remote_ip, 80);
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::Established),
        "ESTABLISHED"
    );

    // Step 1: Client closes → sends FIN.
    let close_result = tcp::close(id).unwrap();
    assert_test!(close_result.is_some(), "FIN segment");
    let fin_seg = close_result.unwrap();
    assert_test!(fin_seg.flags & TCP_FLAG_FIN != 0, "FIN flag");
    assert_test!(fin_seg.flags & TCP_FLAG_ACK != 0, "ACK with FIN");
    assert_eq_test!(tcp::get_state(id), Some(TcpState::FinWait1), "FIN_WAIT_1");

    // Step 2: Server ACKs the FIN.
    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: fin_seg.seq_num.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _result = tcp::input(remote_ip, local_ip, &ack, &[], &[], 0);
    assert_eq_test!(tcp::get_state(id), Some(TcpState::FinWait2), "FIN_WAIT_2");

    // Step 3: Server sends its FIN.
    let server_fin = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: fin_seg.seq_num.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(remote_ip, local_ip, &server_fin, &[], &[], 100);
    assert_eq_test!(tcp::get_state(id), Some(TcpState::TimeWait), "TIME_WAIT");
    assert_test!(result.segments().next().is_some(), "ACK the server's FIN");
    let ack_seg = result.segments().next().unwrap().clone();
    assert_test!(ack_seg.flags & TCP_FLAG_ACK != 0, "ACK flag");
    pass!()
}

pub fn test_tcp_passive_close() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (id, server_iss, client_port) = establish_client_connection(local_ip, remote_ip, 80);

    // Server sends FIN first.
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let server_fin = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: snd_nxt,
        data_offset: 5,
        flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(remote_ip, local_ip, &server_fin, &[], &[], 0);
    assert_eq_test!(tcp::get_state(id), Some(TcpState::CloseWait), "CLOSE_WAIT");
    assert_test!(result.segments().next().is_some(), "ACK the FIN");

    // Client closes.
    let close_result = tcp::close(id).unwrap();
    assert_test!(close_result.is_some(), "FIN segment");
    assert_eq_test!(tcp::get_state(id), Some(TcpState::LastAck), "LAST_ACK");

    // Server ACKs our FIN.
    let fin_seg = close_result.unwrap();
    let server_ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(2),
        ack_num: fin_seg.seq_num.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _result = tcp::input(remote_ip, local_ip, &server_ack, &[], &[], 0);
    // Released PCB = None state.
    assert_eq_test!(tcp::get_state(id), None, "released");
    assert_eq_test!(tcp::active_count(), 0, "connection released");
    pass!()
}

pub fn test_tcp_simultaneous_close() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (id, server_iss, client_port) = establish_client_connection(local_ip, remote_ip, 80);

    // Both sides send FIN simultaneously.
    // Client sends FIN first.
    let close_result = tcp::close(id).unwrap();
    assert_eq_test!(tcp::get_state(id), Some(TcpState::FinWait1), "FIN_WAIT_1");
    let fin_seg = close_result.unwrap();

    // Server's FIN arrives (not ACKing our FIN yet — it was sent simultaneously).
    let snd_una = with_data_state!(id, |d| d.snd_una.raw());
    let server_fin = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: snd_una, // Doesn't ACK our FIN.
        data_offset: 5,
        flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(remote_ip, local_ip, &server_fin, &[], &[], 0);
    assert_eq_test!(tcp::get_state(id), Some(TcpState::Closing), "CLOSING");
    assert_test!(result.segments().next().is_some(), "ACK the peer FIN");

    // Server ACKs our FIN.
    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(2),
        ack_num: fin_seg.seq_num.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _result = tcp::input(remote_ip, local_ip, &ack, &[], &[], 200);
    assert_eq_test!(tcp::get_state(id), Some(TcpState::TimeWait), "TIME_WAIT");
    pass!()
}

// =============================================================================
// 10. TIME_WAIT handling
// =============================================================================

pub fn test_tcp_time_wait_expiry() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (id, server_iss, client_port) = establish_client_connection(local_ip, remote_ip, 80);

    // Active close → FIN_WAIT_1.
    let close_result = tcp::close(id).unwrap().unwrap();
    let fin_seq = close_result.seq_num;

    // Server ACKs FIN → FIN_WAIT_2.
    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: fin_seq.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(remote_ip, local_ip, &ack, &[], &[], 0);

    // Server sends FIN → TIME_WAIT.
    let server_fin = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: fin_seq.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(remote_ip, local_ip, &server_fin, &[], &[], 1000);
    assert_eq_test!(tcp::get_state(id), Some(TcpState::TimeWait), "TIME_WAIT");

    // Fire the TIME_WAIT timer directly (the timer wheel approach replaced
    // the old `tcp_timer_tick` sweep).
    tcp::on_time_wait_expire(id.0);
    assert_eq_test!(tcp::get_state(id), None, "released");
    assert_eq_test!(tcp::active_count(), 0, "released");
    pass!()
}

pub fn test_tcp_time_wait_retransmitted_fin() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (id, server_iss, client_port) = establish_client_connection(local_ip, remote_ip, 80);

    // Full close sequence to TIME_WAIT.
    let close_result = tcp::close(id).unwrap().unwrap();
    let fin_seq = close_result.seq_num;

    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: fin_seq.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(remote_ip, local_ip, &ack, &[], &[], 0);

    let server_fin = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: fin_seq.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(remote_ip, local_ip, &server_fin, &[], &[], 500);
    assert_eq_test!(tcp::get_state(id), Some(TcpState::TimeWait), "TIME_WAIT");

    // Server retransmits FIN in TIME_WAIT → should re-ACK.
    let result = tcp::input(remote_ip, local_ip, &server_fin, &[], &[], 1000);
    assert_test!(
        result.segments().next().is_some(),
        "re-ACK the retransmitted FIN"
    );
    let seg = result.segments().next().unwrap().clone();
    assert_test!(seg.flags & TCP_FLAG_ACK != 0, "ACK flag");
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::TimeWait),
        "still TIME_WAIT"
    );
    pass!()
}

pub fn test_tcp_retransmit_timer() -> TestResult {
    reset();

    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (id, _server_iss, _client_port) = establish_client_connection(local_ip, remote_ip, 80);
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::Established),
        "ESTABLISHED"
    );

    let wrote = tcp::send(id, b"hello").unwrap();
    assert_eq_test!(wrote, 5, "wrote 5 bytes to send buffer");

    let mut payload = [0u8; 1460];
    let now_ms = 0u64;
    let seg = tcp::poll_transmit(id, &mut payload, now_ms);
    assert_test!(seg.is_some(), "segment produced for transmit");

    let has_retransmit_token = with_data_state!(id, |d| d.retransmit_token.is_some());
    assert_test!(has_retransmit_token, "retransmit timer scheduled");

    let rto_before = with_data_state!(id, |d| d.rtt.rto_ms());
    let result = tcp::on_retransmit(id.0);
    assert_test!(result.is_some(), "retransmit handler returned conn id");
    assert_eq_test!(result.unwrap(), id, "correct conn_id");

    let rto_after = with_data_state!(id, |d| d.rtt.rto_ms());
    assert_test!(rto_after > rto_before, "RTO doubled");

    pass!()
}

pub fn test_tcp_time_wait_timer() -> TestResult {
    reset();

    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (id, server_iss, client_port) = establish_client_connection(local_ip, remote_ip, 80);

    let close_result = tcp::close(id).unwrap().unwrap();
    let fin_seq = close_result.seq_num;

    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: fin_seq.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(remote_ip, local_ip, &ack, &[], &[], 0);

    let server_fin = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: fin_seq.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(remote_ip, local_ip, &server_fin, &[], &[], 1000);

    assert_eq_test!(tcp::get_state(id), Some(TcpState::TimeWait), "in TIME_WAIT");

    tcp::on_time_wait_expire(id.0);

    assert_eq_test!(tcp::get_state(id), None, "connection released");
    assert_eq_test!(tcp::active_count(), 0, "no active connections");

    pass!()
}

// =============================================================================
// 11. RST handling in various states
// =============================================================================

pub fn test_tcp_rst_in_established() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (_id, server_iss, client_port) = establish_client_connection(local_ip, remote_ip, 80);

    let rst = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: 0,
        data_offset: 5,
        flags: TCP_FLAG_RST,
        window_size: 0,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(remote_ip, local_ip, &rst, &[], &[], 0);
    assert_test!(
        result.notify.contains(SocketNotify::RESET_RECEIVED),
        "reset flag"
    );
    assert_eq_test!(tcp::active_count(), 0, "released");
    pass!()
}

pub fn test_tcp_rst_to_unknown_ignored() -> TestResult {
    reset();
    // RST to a non-existent connection → should be silently ignored.
    let rst = TcpHeader {
        src_port: 80,
        dst_port: 12345,
        seq_num: 0,
        ack_num: 0,
        data_offset: 5,
        flags: TCP_FLAG_RST,
        window_size: 0,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &rst, &[], &[], 0);
    assert_test!(
        result.segments().next().is_none(),
        "no response to RST for unknown connection"
    );
    pass!()
}

pub fn test_tcp_syn_in_established_sends_rst() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (_id, _server_iss, client_port) = establish_client_connection(local_ip, remote_ip, 80);

    // Unexpected SYN in ESTABLISHED → should RST and close.
    let syn = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: 99999,
        ack_num: 0,
        data_offset: 5,
        flags: TCP_FLAG_SYN,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(remote_ip, local_ip, &syn, &[], &[], 0);
    assert_test!(result.segments().next().is_some(), "RST response");
    let seg = result.segments().next().unwrap().clone();
    assert_test!(seg.flags & TCP_FLAG_RST != 0, "RST flag");
    assert_test!(
        result.notify.contains(SocketNotify::RESET_RECEIVED),
        "reset"
    );
    pass!()
}

// =============================================================================
// 12. Segment to no matching connection
// =============================================================================

pub fn test_tcp_segment_no_connection_sends_rst() -> TestResult {
    reset();
    // Non-RST segment to a port with no listener → RST.
    let syn = TcpHeader {
        src_port: 50000,
        dst_port: 9999,
        seq_num: 1000,
        ack_num: 0,
        data_offset: 5,
        flags: TCP_FLAG_SYN,
        window_size: 8192,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &syn, &[], &[], 0);
    assert_test!(result.segments().next().is_some(), "RST response expected");
    let seg = result.segments().next().unwrap().clone();
    assert_test!(seg.flags & TCP_FLAG_RST != 0, "RST flag");
    pass!()
}

// =============================================================================
// 13. Ephemeral port allocation
// =============================================================================

pub fn test_tcp_ephemeral_ports_unique() -> TestResult {
    reset();
    let table = tcp::table::PCB_TABLE.lock();
    let p1 = table.alloc_ephemeral_port().unwrap();
    let p2 = table.alloc_ephemeral_port().unwrap();
    let p3 = table.alloc_ephemeral_port().unwrap();
    drop(table);
    assert_test!(p1 >= 49152, "p1 in range");
    assert_test!(p2 >= 49152, "p2 in range");
    assert_test!(p3 >= 49152, "p3 in range");
    assert_test!(p1 != p2, "p1 != p2");
    assert_test!(p2 != p3, "p2 != p3");
    assert_test!(p1 != p3, "p1 != p3");
    pass!()
}

// =============================================================================
// 14. TcpState helpers
// =============================================================================

pub fn test_tcp_state_names() -> TestResult {
    assert_eq_test!(TcpState::Listen.name(), "LISTEN");
    assert_eq_test!(TcpState::SynSent.name(), "SYN_SENT");
    assert_eq_test!(TcpState::SynReceived.name(), "SYN_RECEIVED");
    assert_eq_test!(TcpState::Established.name(), "ESTABLISHED");
    assert_eq_test!(TcpState::FinWait1.name(), "FIN_WAIT_1");
    assert_eq_test!(TcpState::FinWait2.name(), "FIN_WAIT_2");
    assert_eq_test!(TcpState::CloseWait.name(), "CLOSE_WAIT");
    assert_eq_test!(TcpState::Closing.name(), "CLOSING");
    assert_eq_test!(TcpState::LastAck.name(), "LAST_ACK");
    assert_eq_test!(TcpState::TimeWait.name(), "TIME_WAIT");
    pass!()
}

pub fn test_tcp_state_is_open() -> TestResult {
    assert_test!(TcpState::Established.is_open(), "ESTABLISHED is open");
    assert_test!(TcpState::FinWait1.is_open(), "FIN_WAIT_1 is open");
    assert_test!(TcpState::FinWait2.is_open(), "FIN_WAIT_2 is open");
    assert_test!(TcpState::CloseWait.is_open(), "CLOSE_WAIT is open");
    assert_test!(!TcpState::Listen.is_open(), "LISTEN not open");
    assert_test!(!TcpState::SynSent.is_open(), "SYN_SENT not open");
    assert_test!(!TcpState::TimeWait.is_open(), "TIME_WAIT not open");
    pass!()
}

pub fn test_tcp_state_is_closing() -> TestResult {
    assert_test!(TcpState::FinWait1.is_closing(), "FIN_WAIT_1");
    assert_test!(TcpState::FinWait2.is_closing(), "FIN_WAIT_2");
    assert_test!(TcpState::CloseWait.is_closing(), "CLOSE_WAIT");
    assert_test!(TcpState::Closing.is_closing(), "CLOSING");
    assert_test!(TcpState::LastAck.is_closing(), "LAST_ACK");
    assert_test!(TcpState::TimeWait.is_closing(), "TIME_WAIT");
    assert_test!(
        !TcpState::Established.is_closing(),
        "ESTABLISHED not closing"
    );
    pass!()
}

// =============================================================================
// 15. Connection table find — exact vs wildcard
// =============================================================================

pub fn test_tcp_find_exact_match() -> TestResult {
    reset();
    let (id, syn_seg) = tcp::connect([10, 0, 0, 1], [10, 0, 0, 2], 80).unwrap();
    let tuple = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: syn_seg.tuple.local_port,
        remote_ip: [10, 0, 0, 2],
        remote_port: 80,
    };
    let found = tcp::find(&tuple);
    assert_eq_test!(found, Some(id), "exact match found");
    pass!()
}

pub fn test_tcp_find_wildcard_listen() -> TestResult {
    reset();
    let listen_id = tcp::listen([0; 4], 80).unwrap();

    // A connection from any IP to port 80 should match the wildcard listener.
    let tuple = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 80,
        remote_ip: [10, 0, 0, 2],
        remote_port: 50000,
    };
    let found = tcp::find(&tuple);
    assert_eq_test!(found, Some(listen_id), "wildcard listen match");
    pass!()
}

// =============================================================================
// 16. TcpTuple::matches
// =============================================================================

pub fn test_tcp_tuple_matches_exact() -> TestResult {
    let t1 = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 80,
        remote_ip: [10, 0, 0, 2],
        remote_port: 50000,
    };
    assert_test!(t1.matches(&t1), "exact self-match");
    pass!()
}

pub fn test_tcp_tuple_matches_wildcard() -> TestResult {
    let listen = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 80,
        remote_ip: [0; 4], // wildcard
        remote_port: 0,    // wildcard
    };
    let incoming = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 80,
        remote_ip: [10, 0, 0, 99],
        remote_port: 54321,
    };
    assert_test!(listen.matches(&incoming), "wildcard match");
    pass!()
}

pub fn test_tcp_tuple_mismatch() -> TestResult {
    let t1 = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 80,
        remote_ip: [10, 0, 0, 2],
        remote_port: 50000,
    };
    let t2 = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 81, // different port
        remote_ip: [10, 0, 0, 2],
        remote_port: 50000,
    };
    assert_test!(!t1.matches(&t2), "port mismatch");
    pass!()
}

// =============================================================================
// 17. Simultaneous open (SYN_SENT receives SYN without ACK)
// =============================================================================

pub fn test_tcp_simultaneous_open() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];
    let remote_ip = [10, 0, 0, 2];

    let (id, syn_seg) = tcp::connect(local_ip, remote_ip, 80).unwrap();
    let client_port = syn_seg.tuple.local_port;

    // Peer also sends SYN (without ACK — simultaneous open).
    let peer_syn = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: 7000,
        ack_num: 0,
        data_offset: 5,
        flags: TCP_FLAG_SYN, // SYN only, no ACK
        window_size: 16384,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(remote_ip, local_ip, &peer_syn, &[], &[], 0);
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::SynReceived),
        "SYN_RECEIVED (simultaneous)"
    );
    assert_test!(result.segments().next().is_some(), "SYN+ACK response");
    let seg = result.segments().next().unwrap().clone();
    assert_test!(seg.flags & TCP_FLAG_SYN != 0, "SYN flag");
    assert_test!(seg.flags & TCP_FLAG_ACK != 0, "ACK flag");
    pass!()
}

// =============================================================================
// 18. Multiple concurrent connections
// =============================================================================

pub fn test_tcp_multiple_connections() -> TestResult {
    reset();
    let local_ip = [10, 0, 0, 1];

    // Create 10 connections to different servers.
    let mut ids = [ConnId(0); 10];
    for i in 0..10 {
        let remote = [10, 0, (i / 256) as u8, (i % 256 + 1) as u8];
        let (id, _) = tcp::connect(local_ip, remote, (80 + i) as u16).unwrap();
        ids[i] = id;
    }
    assert_eq_test!(tcp::active_count(), 10, "10 active connections");

    // Close half of them.
    for i in (0..10).step_by(2) {
        tcp::close(ids[i]).unwrap();
    }
    assert_eq_test!(tcp::active_count(), 5, "5 remaining after closing evens");

    // The remaining odd-indexed connections should still be SYN_SENT.
    for i in (1..10).step_by(2) {
        assert_eq_test!(
            tcp::get_state(ids[i]),
            Some(TcpState::SynSent),
            "odd connection still SYN_SENT"
        );
    }
    pass!()
}

// =============================================================================
// ISN generator (fixes SLOPOS-2026-0007 — predictable monotonic ISN)
// =============================================================================

/// Two calls with the same 4-tuple within the same 4-us drift bucket yield
/// the same ISN; across buckets the delta is strictly the drift increment.
///
/// We can't freeze `monotonic_ns()` in-kernel without adding a clock trait
/// to the ISN path just for tests, so this test verifies the weaker but
/// still-useful property: the 32-bit delta between two same-tuple calls
/// equals the drift difference (which is bounded by elapsed time and
/// therefore tiny for back-to-back calls).
pub fn test_tcp_isn_same_tuple_delta_is_drift_only() -> TestResult {
    reset();
    let tuple = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 49152,
        remote_ip: [10, 0, 0, 2],
        remote_port: 80,
    };
    let a = tcp::isn::generate_isn(&tuple);
    let b = tcp::isn::generate_isn(&tuple);
    // Forward drift must be small for back-to-back calls, and negative
    // drift is impossible because monotonic_ns is monotonic.
    let delta = b.wrapping_sub(a);
    assert_test!(
        delta <= 1_000_000,
        "back-to-back same-tuple ISN delta should be small drift, not a hash change"
    );
    pass!()
}

/// Different 4-tuples must not produce the same ISN.
pub fn test_tcp_isn_varies_by_tuple() -> TestResult {
    reset();
    let t1 = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 49152,
        remote_ip: [10, 0, 0, 2],
        remote_port: 80,
    };
    let t2 = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 49153,
        remote_ip: [10, 0, 0, 2],
        remote_port: 80,
    };
    let t3 = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 49152,
        remote_ip: [10, 0, 0, 2],
        remote_port: 443,
    };
    let isn_1 = tcp::isn::generate_isn(&t1);
    let isn_2 = tcp::isn::generate_isn(&t2);
    let isn_3 = tcp::isn::generate_isn(&t3);
    assert_test!(isn_1 != isn_2, "differing local port -> differing ISN");
    assert_test!(isn_1 != isn_3, "differing remote port -> differing ISN");
    assert_test!(isn_2 != isn_3, "independent tuples -> independent ISNs");
    pass!()
}

/// The ISN must NOT equal the previous ISN + 64000.  The old
/// `ISN_COUNTER.fetch_add(64000)` scheme is gone; any caller that relied
/// on that delta needs to update.
pub fn test_tcp_isn_not_monotonic_counter() -> TestResult {
    reset();
    let tuple = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 49152,
        remote_ip: [10, 0, 0, 2],
        remote_port: 80,
    };
    let t2 = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 49153,
        remote_ip: [10, 0, 0, 2],
        remote_port: 80,
    };
    let isn_a = tcp::isn::generate_isn(&tuple);
    let isn_b = tcp::isn::generate_isn(&t2);
    let delta = isn_b.wrapping_sub(isn_a);
    assert_test!(delta != 64_000, "ISN delta must not be the legacy 64000");
    pass!()
}

// =============================================================================
// Register the test suite
// =============================================================================

slopos_testing::define_test_suite!(
    tcp,
    [
        // Header parsing (6)
        test_tcp_parse_minimal_header,
        test_tcp_parse_too_short,
        test_tcp_parse_invalid_data_offset,
        test_tcp_parse_with_options,
        test_tcp_parse_data_offset_exceeds_buffer,
        test_tcp_parse_all_flags,
        // Header construction (3)
        test_tcp_write_header_roundtrip,
        test_tcp_write_header_buffer_too_small,
        test_tcp_write_header_with_options,
        // MSS options (5)
        test_tcp_parse_mss_option,
        test_tcp_parse_mss_option_with_nop_padding,
        test_tcp_parse_mss_option_not_present,
        test_tcp_write_mss_option,
        test_tcp_write_mss_option_buffer_too_small,
        // Checksum (5)
        test_tcp_checksum_zero_payload,
        test_tcp_checksum_with_payload,
        test_tcp_checksum_odd_payload_length,
        test_tcp_checksum_wrong_ip_fails_verify,
        test_tcp_checksum_deterministic,
        // Sequence number arithmetic (4)
        test_tcp_seq_lt,
        test_tcp_seq_le,
        test_tcp_seq_gt,
        test_tcp_seq_ge,
        // Connection table (10)
        test_tcp_table_initially_empty,
        test_tcp_connect_creates_syn_sent,
        test_tcp_table_full_returns_error,
        test_tcp_listen_creates_listen_state,
        test_tcp_listen_duplicate_port_fails,
        test_tcp_close_listen_releases_slot,
        test_tcp_close_syn_sent_releases_slot,
        test_tcp_abort_sends_rst,
        test_tcp_abort_listen_no_rst,
        test_tcp_close_not_found,
        // Active open handshake (4)
        test_tcp_active_handshake_complete,
        test_tcp_active_rst_in_syn_sent,
        test_tcp_active_bad_ack_in_syn_sent,
        test_tcp_active_mss_negotiation,
        // Passive open handshake (3)
        test_tcp_passive_handshake_complete,
        test_tcp_passive_rst_in_syn_received,
        test_tcp_passive_ack_to_listen_sends_rst,
        // Connection teardown (3)
        test_tcp_active_close,
        test_tcp_passive_close,
        test_tcp_simultaneous_close,
        // TIME_WAIT (2)
        test_tcp_time_wait_expiry,
        test_tcp_time_wait_retransmitted_fin,
        test_tcp_retransmit_timer,
        test_tcp_time_wait_timer,
        // RST handling (3)
        test_tcp_rst_in_established,
        test_tcp_rst_to_unknown_ignored,
        test_tcp_syn_in_established_sends_rst,
        // Misc (1)
        test_tcp_segment_no_connection_sends_rst,
        // Ephemeral ports (1)
        test_tcp_ephemeral_ports_unique,
        // State helpers (3)
        test_tcp_state_names,
        test_tcp_state_is_open,
        test_tcp_state_is_closing,
        // Table find (2)
        test_tcp_find_exact_match,
        test_tcp_find_wildcard_listen,
        // Tuple matching (3)
        test_tcp_tuple_matches_exact,
        test_tcp_tuple_matches_wildcard,
        test_tcp_tuple_mismatch,
        // Simultaneous open (1)
        test_tcp_simultaneous_open,
        // Multiple connections (1)
        test_tcp_multiple_connections,
        // ISN generator, RFC 6528-ish (3 — fixes SLOPOS-2026-0007)
        test_tcp_isn_same_tuple_delta_is_drift_only,
        test_tcp_isn_varies_by_tuple,
        test_tcp_isn_not_monotonic_counter,
    ]
);
