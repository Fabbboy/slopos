//! Tests for the DHCP client state machine and wire codec.
//!
//! Everything here is a pure unit: the client takes events and returns actions,
//! so a lease can be walked from DISCOVER through renewal, rebinding, refusal
//! and expiry without a network and without touching the live NIC the rest of
//! the suite is using. Time is explicit — the client is handed `now_ms`, so a
//! sixty-four-second backoff is a function argument rather than a minute of
//! test runtime.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::dhcp::client::{
    DhcpAction, DhcpClient, DhcpDest, DhcpEvent, DhcpState, RETRY_BASE_MS, RETRY_JITTER_MS,
    RETRY_MAX_MS, UnbindReason,
};
use crate::dhcp::codec::{
    self, BOOTP_HEADER_LEN, DHCP_FRAME_LEN, MSG_ACK, MSG_NAK, MSG_OFFER, MSG_REQUEST,
};

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const SEED: u32 = 0x1234_5678;

const SERVER: [u8; 4] = [10, 0, 2, 2];
const CLIENT_IP: [u8; 4] = [10, 0, 2, 15];
const MASK: [u8; 4] = [255, 255, 255, 0];
const ROUTER: [u8; 4] = [10, 0, 2, 2];
const DNS: [u8; 4] = [10, 0, 2, 3];

/// Build a BOOTP reply carrying `msg_type` and whichever lease options are
/// supplied, by hand, so the test does not depend on the encoder it is checking
/// the decoder against.
///
/// It borrows the caller's frame rather than returning one: a 320-byte buffer
/// returned by value carries a function over the build's 2 KiB stack-frame
/// gate.
struct ReplyBuilder<'a> {
    buf: &'a mut [u8; DHCP_FRAME_LEN],
    at: usize,
}

impl<'a> ReplyBuilder<'a> {
    fn new(buf: &'a mut [u8; DHCP_FRAME_LEN], xid: u32, msg_type: u8, yiaddr: [u8; 4]) -> Self {
        buf.fill(0);
        buf[0] = 2; // BOOTREPLY
        buf[1] = 1; // htype ethernet
        buf[2] = 6;
        buf[4..8].copy_from_slice(&xid.to_be_bytes());
        buf[16..20].copy_from_slice(&yiaddr);
        buf[20..24].copy_from_slice(&SERVER); // siaddr
        buf[28..34].copy_from_slice(&MAC);
        buf[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
        let mut me = Self {
            buf,
            at: BOOTP_HEADER_LEN,
        };
        me.opt(53, &[msg_type]);
        me
    }

    fn opt(&mut self, code: u8, data: &[u8]) -> &mut Self {
        self.buf[self.at] = code;
        self.buf[self.at + 1] = data.len() as u8;
        self.buf[self.at + 2..self.at + 2 + data.len()].copy_from_slice(data);
        self.at += 2 + data.len();
        self
    }

    fn standard(&mut self) -> &mut Self {
        self.opt(54, &SERVER)
            .opt(1, &MASK)
            .opt(3, &ROUTER)
            .opt(6, &DNS)
    }

    fn finish(&mut self) -> usize {
        self.buf[self.at] = 255;
        self.at + 1
    }
}

fn offer(buf: &mut [u8; DHCP_FRAME_LEN], xid: u32) -> usize {
    let mut b = ReplyBuilder::new(buf, xid, MSG_OFFER, CLIENT_IP);
    b.standard();
    b.finish()
}

fn ack_with_times(buf: &mut [u8; DHCP_FRAME_LEN], xid: u32, lease: u32, t1: u32, t2: u32) -> usize {
    let mut b = ReplyBuilder::new(buf, xid, MSG_ACK, CLIENT_IP);
    b.standard();
    b.opt(51, &lease.to_be_bytes());
    b.opt(58, &t1.to_be_bytes());
    b.opt(59, &t2.to_be_bytes());
    b.finish()
}

/// An ACK carrying a lease time and nothing else, so the defaults apply.
fn ack_lease_only(buf: &mut [u8; DHCP_FRAME_LEN], xid: u32, lease: u32) -> usize {
    let mut b = ReplyBuilder::new(buf, xid, MSG_ACK, CLIENT_IP);
    b.standard();
    b.opt(51, &lease.to_be_bytes());
    b.finish()
}

fn nak(buf: &mut [u8; DHCP_FRAME_LEN], xid: u32) -> usize {
    let mut b = ReplyBuilder::new(buf, xid, MSG_NAK, [0; 4]);
    b.opt(54, &SERVER);
    b.finish()
}

fn bound_client(
    buf: &mut [u8; DHCP_FRAME_LEN],
    lease: u32,
    t1: u32,
    t2: u32,
) -> Option<DhcpClient> {
    let mut c = DhcpClient::new(MAC, SEED);
    if !matches!(c.step(DhcpEvent::Start, 0), DhcpAction::Send { .. }) {
        return None;
    }
    let len = offer(buf, c.xid());
    if !matches!(
        c.step(DhcpEvent::Reply(&buf[..len]), 10),
        DhcpAction::Send { .. }
    ) {
        return None;
    }
    let len = ack_with_times(buf, c.xid(), lease, t1, t2);
    if !matches!(
        c.step(DhcpEvent::Reply(&buf[..len]), 20),
        DhcpAction::Bind(_)
    ) {
        return None;
    }
    Some(c)
}

fn frame_msg_type(frame: &[u8]) -> Option<u8> {
    let reply_view = codec::parse_reply(frame, 0);
    let _ = reply_view; // outgoing frames are BOOTREQUEST, so parse by hand
    let mut i = BOOTP_HEADER_LEN;
    while i + 1 < frame.len() {
        let code = frame[i];
        if code == 255 {
            return None;
        }
        if code == 0 {
            i += 1;
            continue;
        }
        let len = frame[i + 1] as usize;
        if i + 2 + len > frame.len() {
            return None;
        }
        if code == 53 && len >= 1 {
            return Some(frame[i + 2]);
        }
        i += 2 + len;
    }
    None
}

fn frame_option(frame: &[u8], want: u8) -> Option<&[u8]> {
    let mut i = BOOTP_HEADER_LEN;
    while i + 1 < frame.len() {
        let code = frame[i];
        if code == 255 {
            return None;
        }
        if code == 0 {
            i += 1;
            continue;
        }
        let len = frame[i + 1] as usize;
        if i + 2 + len > frame.len() {
            return None;
        }
        if code == want {
            return Some(&frame[i + 2..i + 2 + len]);
        }
        i += 2 + len;
    }
    None
}

fn frame_ciaddr(frame: &[u8]) -> [u8; 4] {
    [frame[12], frame[13], frame[14], frame[15]]
}

fn frame_is_broadcast(frame: &[u8]) -> bool {
    u16::from_be_bytes([frame[10], frame[11]]) & 0x8000 != 0
}

fn test_dhcp_start_discovers() -> TestResult {
    let mut c = DhcpClient::new(MAC, SEED);
    assert_eq_test!(
        c.state(),
        DhcpState::Init,
        "a fresh client has done nothing"
    );

    let action = c.step(DhcpEvent::Start, 0);
    let DhcpAction::Send { dest, retry_ms } = action else {
        return fail!("Start must transmit, got {:?}", action);
    };
    assert_eq_test!(
        dest,
        DhcpDest::Broadcast,
        "a client with no address broadcasts"
    );
    assert_eq_test!(c.state(), DhcpState::Selecting, "and is now selecting");
    assert_eq_test!(
        frame_msg_type(c.frame()),
        Some(codec::MSG_DISCOVER),
        "the frame is a DISCOVER"
    );
    assert_test!(frame_is_broadcast(c.frame()), "with the broadcast flag set");
    assert_eq_test!(
        frame_option(c.frame(), 61),
        Some(&[1u8, MAC[0], MAC[1], MAC[2], MAC[3], MAC[4], MAC[5]][..]),
        "and the client identifier, without which a reboot burns a fresh lease"
    );
    assert_test!(
        retry_ms >= RETRY_BASE_MS - RETRY_JITTER_MS && retry_ms <= RETRY_BASE_MS + RETRY_JITTER_MS,
        "the first retransmission is the base delay, jittered"
    );
    assert_test!(c.xid() != 0, "and the transaction has an id");
    pass!()
}

/// The REQUEST names both the address and the server — which is what tells the
/// servers that lost the race to release their offers.
fn test_dhcp_offer_produces_request_with_options_50_and_54() -> TestResult {
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let mut c = DhcpClient::new(MAC, SEED);
    c.step(DhcpEvent::Start, 0);

    let len = offer(&mut buf, c.xid());
    let action = c.step(DhcpEvent::Reply(&buf[..len]), 10);
    let DhcpAction::Send { dest, .. } = action else {
        return fail!("an OFFER must produce a REQUEST, got {:?}", action);
    };

    assert_eq_test!(
        dest,
        DhcpDest::Broadcast,
        "the selecting REQUEST is broadcast"
    );
    assert_eq_test!(
        c.state(),
        DhcpState::Requesting,
        "and the client is requesting"
    );
    assert_eq_test!(
        frame_msg_type(c.frame()),
        Some(MSG_REQUEST),
        "the frame is a REQUEST"
    );
    assert_eq_test!(
        frame_option(c.frame(), 50),
        Some(&CLIENT_IP[..]),
        "option 50 names the address being accepted"
    );
    assert_eq_test!(
        frame_option(c.frame(), 54),
        Some(&SERVER[..]),
        "option 54 names whose offer it is"
    );
    pass!()
}

fn test_dhcp_ack_binds_with_full_configuration() -> TestResult {
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let mut c = DhcpClient::new(MAC, SEED);
    c.step(DhcpEvent::Start, 0);
    let len = offer(&mut buf, c.xid());
    c.step(DhcpEvent::Reply(&buf[..len]), 10);

    let len = ack_with_times(&mut buf, c.xid(), 3600, 1800, 3150);
    let action = c.step(DhcpEvent::Reply(&buf[..len]), 20);
    let DhcpAction::Bind(binding) = action else {
        return fail!("an ACK must bind, got {:?}", action);
    };

    assert_eq_test!(binding.addr, CLIENT_IP, "the address");
    assert_eq_test!(binding.mask, MASK, "the mask, for the connected route");
    assert_eq_test!(binding.router, ROUTER, "the router, for the default route");
    assert_eq_test!(binding.dns, DNS, "and the resolver");
    assert_eq_test!(binding.server_id, SERVER, "plus who to renew with");
    assert_eq_test!(binding.lease_secs, 3600, "the lease");
    assert_eq_test!(binding.t1_secs, 1800, "T1 as the server stated it");
    assert_eq_test!(binding.t2_secs, 3150, "T2 as the server stated it");
    assert_eq_test!(c.state(), DhcpState::Bound, "and the client is bound");
    pass!()
}

/// A server may send a lease and no timers. RFC 2131 §4.4.5 supplies the
/// defaults; a client that read the absent options as zero would renew
/// continuously and never stop.
fn test_dhcp_lease_times_default_to_half_and_seven_eighths() -> TestResult {
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let mut c = DhcpClient::new(MAC, SEED);
    c.step(DhcpEvent::Start, 0);
    let len = offer(&mut buf, c.xid());
    c.step(DhcpEvent::Reply(&buf[..len]), 10);

    let len = ack_lease_only(&mut buf, c.xid(), 3600);
    let action = c.step(DhcpEvent::Reply(&buf[..len]), 20);
    let DhcpAction::Bind(binding) = action else {
        return fail!("an ACK must bind, got {:?}", action);
    };

    assert_eq_test!(
        binding.lease_secs,
        3600,
        "the lease is what the server said"
    );
    assert_eq_test!(binding.t1_secs, 1800, "T1 defaults to half the lease");
    assert_eq_test!(binding.t2_secs, 3150, "T2 defaults to seven eighths");
    assert_test!(
        binding.t1_secs < binding.t2_secs && binding.t2_secs < binding.lease_secs,
        "and the three are ordered, or renewal would start after expiry"
    );
    pass!()
}

/// Backoff doubles 4→8→16→32→64 and then stays there: a failed acquisition at
/// probe must never become permanent.
fn test_dhcp_backoff_caps_and_retries_forever() -> TestResult {
    let mut c = DhcpClient::new(MAC, SEED);
    c.step(DhcpEvent::Start, 0);
    assert_eq_test!(c.retry_base_ms(), RETRY_BASE_MS, "the first delay is 4 s");

    const EXPECTED: [u32; 4] = [8_000, 16_000, 32_000, 64_000];
    for (round, want) in EXPECTED.iter().enumerate() {
        let action = c.step(DhcpEvent::Retransmit, 1_000 * round as u64);
        let DhcpAction::Send { dest, retry_ms } = action else {
            return fail!("round {} must retransmit, got {:?}", round, action);
        };
        assert_eq_test!(dest, DhcpDest::Broadcast, "still broadcasting");
        assert_eq_test!(c.retry_base_ms(), *want, "the backoff doubles");
        assert_test!(
            retry_ms >= want.saturating_sub(RETRY_JITTER_MS)
                && retry_ms <= want.saturating_add(RETRY_JITTER_MS),
            "and the armed delay is that, jittered"
        );
    }

    for round in 0..20 {
        let action = c.step(DhcpEvent::Retransmit, 100_000 + round as u64);
        let DhcpAction::Send { retry_ms, .. } = action else {
            return fail!(
                "round {} past the cap stopped transmitting: {:?}",
                round,
                action
            );
        };
        assert_eq_test!(c.retry_base_ms(), RETRY_MAX_MS, "capped at 64 s");
        assert_test!(retry_ms > 0, "and always arms another attempt");
    }
    assert_eq_test!(
        c.state(),
        DhcpState::Selecting,
        "a client that never got an answer is still trying, not stuck"
    );
    pass!()
}

/// T1 renews by unicast to the granting server, keeping the address in
/// `ciaddr` and omitting options 50 and 54 as RFC 2131 §4.3.2 requires.
fn test_dhcp_t1_renews_by_unicast_keeping_the_address() -> TestResult {
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let Some(mut c) = bound_client(&mut buf, 3600, 1800, 3150) else {
        return fail!("could not reach Bound");
    };

    let action = c.step(DhcpEvent::T1, 1_800_000);
    let DhcpAction::Send { dest, .. } = action else {
        return fail!("T1 must renew, got {:?}", action);
    };

    assert_eq_test!(
        dest,
        DhcpDest::Server(SERVER),
        "a renewal is a unicast conversation with the server that granted it"
    );
    assert_eq_test!(c.state(), DhcpState::Renewing, "and the client is renewing");
    assert_eq_test!(c.address(), CLIENT_IP, "the address is kept throughout");
    assert_eq_test!(
        frame_ciaddr(c.frame()),
        CLIENT_IP,
        "and travels in ciaddr, not option 50"
    );
    assert_test!(
        frame_option(c.frame(), 50).is_none(),
        "option 50 must not appear in a renewal"
    );
    assert_test!(frame_option(c.frame(), 54).is_none(), "nor may option 54");
    assert_test!(
        !frame_is_broadcast(c.frame()),
        "a renewing client can receive unicast, so it does not ask for broadcast"
    );

    let len = ack_with_times(&mut buf, c.xid(), 3600, 1800, 3150);
    let action = c.step(DhcpEvent::Reply(&buf[..len]), 1_800_010);
    assert_test!(
        matches!(action, DhcpAction::Bind(_)),
        "the renewal ACK re-binds"
    );
    assert_eq_test!(c.state(), DhcpState::Bound, "back to bound");
    pass!()
}

/// T2 broadcasts: the granting server has stopped answering, so any server may.
fn test_dhcp_t2_rebinds_by_broadcast() -> TestResult {
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let Some(mut c) = bound_client(&mut buf, 3600, 1800, 3150) else {
        return fail!("could not reach Bound");
    };
    c.step(DhcpEvent::T1, 1_800_000);

    let action = c.step(DhcpEvent::T2, 3_150_000);
    let DhcpAction::Send { dest, .. } = action else {
        return fail!("T2 must rebind, got {:?}", action);
    };

    assert_eq_test!(dest, DhcpDest::Broadcast, "rebinding asks anybody");
    assert_eq_test!(c.state(), DhcpState::Rebinding, "and says so");
    assert_eq_test!(c.address(), CLIENT_IP, "still holding the address");
    assert_eq_test!(frame_ciaddr(c.frame()), CLIENT_IP, "still in ciaddr");
    assert_test!(
        frame_option(c.frame(), 54).is_none(),
        "and naming no server, because it does not know which one will answer"
    );
    pass!()
}

/// A NAK while renewing gives the address up at once. Keeping it would risk two
/// hosts on one address, which is worse than having none.
fn test_dhcp_nak_in_renewing_unbinds_immediately() -> TestResult {
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let Some(mut c) = bound_client(&mut buf, 3600, 1800, 3150) else {
        return fail!("could not reach Bound");
    };
    c.step(DhcpEvent::T1, 1_800_000);

    let len = nak(&mut buf, c.xid());
    let action = c.step(DhcpEvent::Reply(&buf[..len]), 1_800_010);
    let DhcpAction::UnbindThenSend { reason, dest, .. } = action else {
        return fail!("a NAK must unbind and restart, got {:?}", action);
    };

    assert_eq_test!(reason, UnbindReason::Nak, "and says why");
    assert_eq_test!(
        dest,
        DhcpDest::Broadcast,
        "then starts over with a DISCOVER"
    );
    assert_eq_test!(c.address(), [0; 4], "the address is gone");
    assert_eq_test!(c.state(), DhcpState::Selecting, "and it is selecting again");
    assert_eq_test!(
        frame_msg_type(c.frame()),
        Some(codec::MSG_DISCOVER),
        "the frame really is a DISCOVER"
    );
    pass!()
}

/// Expiry unbinds. Nothing is sent — there is nobody left to tell.
fn test_dhcp_expiry_unbinds() -> TestResult {
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let Some(mut c) = bound_client(&mut buf, 3600, 1800, 3150) else {
        return fail!("could not reach Bound");
    };

    let action = c.step(DhcpEvent::Expire, 3_600_000);
    assert_eq_test!(
        action,
        DhcpAction::Unbind(UnbindReason::Expired),
        "an expired lease is torn down"
    );
    assert_eq_test!(c.address(), [0; 4], "and the address released");
    assert_eq_test!(c.state(), DhcpState::Init, "back to the start");
    pass!()
}

/// Stopping releases before unbinding: the order is what lets the server hand
/// the address out again immediately rather than waiting out the lease.
fn test_dhcp_stop_releases_then_unbinds() -> TestResult {
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let Some(mut c) = bound_client(&mut buf, 3600, 1800, 3150) else {
        return fail!("could not reach Bound");
    };

    let action = c.step(DhcpEvent::Stop, 100_000);
    let DhcpAction::SendThenUnbind { dest, reason } = action else {
        return fail!("Stop must release then unbind, got {:?}", action);
    };

    assert_eq_test!(
        dest,
        DhcpDest::Server(SERVER),
        "unicast to the granting server"
    );
    assert_eq_test!(reason, UnbindReason::Stopped, "for the stated reason");
    assert_eq_test!(
        frame_msg_type(c.frame()),
        Some(codec::MSG_RELEASE),
        "and the frame is a RELEASE"
    );
    assert_eq_test!(
        frame_ciaddr(c.frame()),
        CLIENT_IP,
        "carrying the address being given back"
    );
    assert_eq_test!(
        frame_option(c.frame(), 54),
        Some(&SERVER[..]),
        "and naming the server it belongs to"
    );

    let mut fresh = DhcpClient::new(MAC, SEED);
    assert_eq_test!(
        fresh.step(DhcpEvent::Stop, 0),
        DhcpAction::Idle,
        "there is nothing to give back before a lease exists"
    );
    pass!()
}

fn test_dhcp_carrier_loss_keeps_the_lease() -> TestResult {
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let Some(mut c) = bound_client(&mut buf, 3600, 1800, 3150) else {
        return fail!("could not reach Bound");
    };

    assert_eq_test!(
        c.step(DhcpEvent::CarrierDown, 100_000),
        DhcpAction::Idle,
        "a cable coming out does not invalidate a lease the server still holds"
    );
    assert_eq_test!(c.address(), CLIENT_IP, "so the address stays");
    assert_eq_test!(c.state(), DhcpState::Bound, "and so does the state");

    assert_eq_test!(
        c.step(DhcpEvent::T1, 200_000),
        DhcpAction::Idle,
        "and a stale T1 cannot start a renewal over a dead link"
    );

    let action = c.step(DhcpEvent::CarrierUp, 300_000);
    let DhcpAction::Send { dest, .. } = action else {
        return fail!("carrier return must confirm the address, got {:?}", action);
    };
    assert_eq_test!(dest, DhcpDest::Broadcast, "broadcast, per INIT-REBOOT");
    assert_eq_test!(c.state(), DhcpState::Requesting, "straight to requesting");
    assert_eq_test!(
        frame_option(c.frame(), 50),
        Some(&CLIENT_IP[..]),
        "asking for the address it still holds"
    );
    assert_test!(
        frame_option(c.frame(), 54).is_none(),
        "and naming no server — the segment may not be the one the lease came from"
    );
    pass!()
}

fn test_dhcp_ignores_foreign_and_untimely_replies() -> TestResult {
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let mut c = DhcpClient::new(MAC, SEED);
    c.step(DhcpEvent::Start, 0);
    let ours = c.xid();

    let len = offer(&mut buf, ours ^ 0xFFFF_FFFF);
    assert_eq_test!(
        c.step(DhcpEvent::Reply(&buf[..len]), 10),
        DhcpAction::Idle,
        "an offer for another transaction is not ours to act on"
    );
    assert_eq_test!(c.state(), DhcpState::Selecting, "so nothing moved");
    assert_eq_test!(c.xid(), ours, "and the transaction is unchanged");

    let len = ack_with_times(&mut buf, ours, 3600, 1800, 3150);
    assert_eq_test!(
        c.step(DhcpEvent::Reply(&buf[..len]), 10),
        DhcpAction::Idle,
        "an ACK before any OFFER is not a lease"
    );
    assert_eq_test!(c.state(), DhcpState::Selecting, "still selecting");

    let mut b = ReplyBuilder::new(&mut buf, ours, MSG_OFFER, CLIENT_IP);
    b.opt(1, &MASK);
    let len = b.finish();
    assert_eq_test!(
        c.step(DhcpEvent::Reply(&buf[..len]), 10),
        DhcpAction::Idle,
        "an offer with no server identifier is unusable"
    );
    pass!()
}

/// Every truncation of a valid ACK decodes or refuses, and none panics: this
/// parses an unauthenticated broadcast from a machine nobody has proved
/// anything about.
fn test_dhcp_truncated_replies_never_panic() -> TestResult {
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let len = ack_with_times(&mut buf, 0xDEAD_BEEF, 3600, 1800, 3150);
    for n in 0..=len {
        let decoded = codec::parse_reply(&buf[..n], 0xDEAD_BEEF);
        if n < BOOTP_HEADER_LEN {
            assert_test!(
                decoded.is_none(),
                "a frame shorter than the BOOTP header is not a reply"
            );
        }
    }

    for n in 0..=len {
        let mut c = DhcpClient::new(MAC, SEED);
        c.step(DhcpEvent::Start, 0);
        c.step(DhcpEvent::Reply(&buf[..n]), 10);
    }

    let mut junk = [0u8; DHCP_FRAME_LEN];
    for (i, b) in junk.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
    }
    for n in 0..junk.len() {
        let _ = codec::parse_reply(&junk[..n], 0);
    }
    pass!()
}

/// A transaction id an off-path attacker can guess is one he can answer, so it
/// must not be derivable from a fixed boot constant plus a counter.
fn test_dhcp_xid_depends_on_the_seed() -> TestResult {
    let mut a = DhcpClient::new(MAC, 0x1111_1111);
    let mut b = DhcpClient::new(MAC, 0x2222_2222);
    a.step(DhcpEvent::Start, 0);
    b.step(DhcpEvent::Start, 0);
    assert_test!(
        a.xid() != b.xid(),
        "different seeds must not produce the same transaction id"
    );

    let first = a.xid();
    a.step(DhcpEvent::Stop, 0);
    a.step(DhcpEvent::Start, 0);
    let second = a.xid();
    assert_test!(second != first, "a new transaction gets a new id");
    assert_test!(
        second != first.wrapping_add(1),
        "and it is not the obvious next one"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_dhcp_ack_binds_with_full_configuration,
    suite = dhcp_client
);
slopos_testing::stest!(
    name = test_dhcp_backoff_caps_and_retries_forever,
    suite = dhcp_client
);
slopos_testing::stest!(
    name = test_dhcp_carrier_loss_keeps_the_lease,
    suite = dhcp_client
);
slopos_testing::stest!(name = test_dhcp_expiry_unbinds, suite = dhcp_client);
slopos_testing::stest!(
    name = test_dhcp_ignores_foreign_and_untimely_replies,
    suite = dhcp_client
);
slopos_testing::stest!(
    name = test_dhcp_lease_times_default_to_half_and_seven_eighths,
    suite = dhcp_client
);
slopos_testing::stest!(
    name = test_dhcp_nak_in_renewing_unbinds_immediately,
    suite = dhcp_client
);
slopos_testing::stest!(
    name = test_dhcp_offer_produces_request_with_options_50_and_54,
    suite = dhcp_client
);
slopos_testing::stest!(name = test_dhcp_start_discovers, suite = dhcp_client);
slopos_testing::stest!(
    name = test_dhcp_stop_releases_then_unbinds,
    suite = dhcp_client
);
slopos_testing::stest!(
    name = test_dhcp_t1_renews_by_unicast_keeping_the_address,
    suite = dhcp_client
);
slopos_testing::stest!(
    name = test_dhcp_t2_rebinds_by_broadcast,
    suite = dhcp_client
);
slopos_testing::stest!(
    name = test_dhcp_truncated_replies_never_panic,
    suite = dhcp_client
);
slopos_testing::stest!(
    name = test_dhcp_xid_depends_on_the_seed,
    suite = dhcp_client
);
