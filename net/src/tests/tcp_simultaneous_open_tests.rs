//! Simultaneous open: both peers send a SYN and neither is a listener
//! (RFC 9293 §3.5). The crossed SYN moves `SynSent` → `SynRecv`, where the
//! SYN-ACK is retransmitted on the connection's own `TcpRetransmit` timer
//! rather than a listener's SYN queue.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use super::net_scope::NetTestScope;
use super::tcp_common::{LOCAL_IP, REMOTE_IP, REMOTE_PORT, make_header};
use crate::tcp::header::{TCP_FLAG_ACK, TCP_FLAG_SYN};
use crate::tcp::{self, ACTIVE_SYN_RETRIES_MAX, RetransmitAction, TcpState};
use crate::timer::TimerKind;

const PEER_IRS: u32 = 9_000;

struct Crossed {
    scope: NetTestScope,
    id: tcp::ConnId,
    local_port: u16,
    iss: u32,
}

/// `connect`, then answer with a bare SYN instead of a SYN-ACK.
fn cross(scope_ms: u64) -> Result<Crossed, &'static str> {
    let scope = NetTestScope::enter_at_mock_ms(scope_ms).map_err(|_| "net scope")?;
    let (id, syn) = tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT).map_err(|_| "connect")?;
    tcp::arm_syn_retransmit(id);
    let local_port = syn.tuple.local_port;
    let iss = syn.seq_num;

    let peer_syn = make_header(REMOTE_PORT, local_port, PEER_IRS, 0, TCP_FLAG_SYN, 16384);
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &peer_syn, &[], &[], 0);

    if tcp::get_state(id) != Some(TcpState::SynReceived) {
        return Err("the crossed SYN did not reach SYN_RECEIVED");
    }
    Ok(Crossed {
        scope,
        id,
        local_port,
        iss,
    })
}

fn pcb_is_live(id: tcp::ConnId) -> bool {
    tcp::with_pcb(id, |_| ()).is_some()
}

/// Without this the connection's only timer is the SYN one, which the
/// transition cancels — leaving nothing to re-send the SYN-ACK and nothing to
/// ever reclaim the shard slot.
pub fn test_crossed_syn_arms_a_retransmit() -> TestResult {
    let c = match cross(1_000) {
        Ok(c) => c,
        Err(m) => return fail!("{}", m),
    };

    let fired = c.scope.dispatch_due(TimerKind::TcpRetransmit);
    assert_test!(
        fired.is_empty(),
        "a timer armed at the transition fired before its delay elapsed"
    );

    crate::tcp::clock::MockClock::advance(2_000);
    let fired = c.scope.dispatch_due(TimerKind::TcpRetransmit);
    assert_eq_test!(fired.len(), 1, "the SynRecv transition armed no retransmit");
    assert_eq_test!(
        fired[0].key,
        c.id.raw(),
        "the retransmit timer names a different connection"
    );
    pass!()
}

/// A SYN-ACK re-sent at `snd_nxt` (== iss + 1) is a segment the peer cannot
/// match to the handshake it is in.
pub fn test_retransmitted_syn_ack_carries_the_original_iss() -> TestResult {
    let c = match cross(2_000) {
        Ok(c) => c,
        Err(m) => return fail!("{}", m),
    };

    match tcp::on_retransmit(c.id.raw()) {
        RetransmitAction::Segment(seg) => {
            assert_eq_test!(
                seg.flags & (TCP_FLAG_SYN | TCP_FLAG_ACK),
                TCP_FLAG_SYN | TCP_FLAG_ACK,
                "a simultaneous-open retransmit must be a SYN-ACK"
            );
            assert_eq_test!(seg.seq_num, c.iss, "retransmitted SYN-ACK moved the ISS");
            assert_eq_test!(
                seg.ack_num,
                PEER_IRS.wrapping_add(1),
                "retransmitted SYN-ACK stopped acknowledging the peer's SYN"
            );
        }
        RetransmitAction::Data(_) => {
            return fail!("a SynRecv PCB was routed to the data retransmit path");
        }
        RetransmitAction::Nothing => {
            return fail!("the SYN-ACK was not retransmitted");
        }
    }
    assert_test!(
        pcb_is_live(c.id),
        "a first retransmit dropped the connection"
    );
    pass!()
}

/// The leak this suite exists for: a `SynRecv` entry nothing retires holds its
/// shard slot and ephemeral port for the life of the boot.
pub fn test_syn_ack_retransmits_are_bounded_and_release_the_pcb() -> TestResult {
    let c = match cross(3_000) {
        Ok(c) => c,
        Err(m) => return fail!("{}", m),
    };

    for attempt in 1..=ACTIVE_SYN_RETRIES_MAX {
        match tcp::on_retransmit(c.id.raw()) {
            RetransmitAction::Segment(_) => {}
            _ => {
                return fail!(
                    "retransmit {} of {} did not re-send the SYN-ACK",
                    attempt,
                    ACTIVE_SYN_RETRIES_MAX
                );
            }
        }
    }

    match tcp::on_retransmit(c.id.raw()) {
        RetransmitAction::Nothing => {}
        RetransmitAction::Segment(_) => {
            return fail!(
                "a {}th retransmit was sent -- the attempt is unbounded",
                ACTIVE_SYN_RETRIES_MAX as u32 + 1
            );
        }
        RetransmitAction::Data(_) => return fail!("routed to the data retransmit path"),
    }
    assert_test!(
        !pcb_is_live(c.id),
        "the exhausted attempt left its PCB in the table"
    );
    pass!()
}

/// A crossed SYN answers the SYN already sent; granting a fresh budget would
/// let the peer restart a backed-off RTO at its 1 s base.
pub fn test_the_cross_does_not_refill_the_retransmit_budget() -> TestResult {
    let scope = match NetTestScope::enter_at_mock_ms(4_000) {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let _ = &scope;

    let (id, syn) = match tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT) {
        Ok(v) => v,
        Err(e) => return fail!("connect: {:?}", e),
    };
    tcp::arm_syn_retransmit(id);

    const SPENT: u8 = 2;
    for _ in 0..SPENT {
        match tcp::on_retransmit(id.raw()) {
            RetransmitAction::Segment(_) => {}
            _ => return fail!("a SynSent retransmit did not re-send the SYN"),
        }
    }

    let peer_syn = make_header(
        REMOTE_PORT,
        syn.tuple.local_port,
        PEER_IRS,
        0,
        TCP_FLAG_SYN,
        16384,
    );
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &peer_syn, &[], &[], 0);
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::SynReceived),
        "the crossed SYN did not reach SYN_RECEIVED"
    );

    for attempt in 1..=(ACTIVE_SYN_RETRIES_MAX - SPENT) {
        match tcp::on_retransmit(id.raw()) {
            RetransmitAction::Segment(_) => {}
            _ => return fail!("SYN-ACK retransmit {} was refused early", attempt),
        }
    }
    match tcp::on_retransmit(id.raw()) {
        RetransmitAction::Nothing => {}
        _ => {
            return fail!(
                "the cross refilled the budget -- more than {} total attempts were granted",
                ACTIVE_SYN_RETRIES_MAX
            );
        }
    }
    pass!()
}

/// A token left armed across `SynRecv` -> `Data` is one `cancel_pcb_timers` can
/// no longer find; it later fires against the established connection's own RTO
/// and drives a spurious congestion collapse.
pub fn test_completed_handshake_retires_the_syn_ack_timer() -> TestResult {
    let c = match cross(5_000) {
        Ok(c) => c,
        Err(m) => return fail!("{}", m),
    };

    let armed = crate::timer::wheel().pending_count();
    assert_test!(
        armed > 0,
        "the transition armed no timer, so nothing below is exercised"
    );

    let ack = make_header(
        REMOTE_PORT,
        c.local_port,
        PEER_IRS.wrapping_add(1),
        c.iss.wrapping_add(1),
        TCP_FLAG_ACK,
        16384,
    );
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &ack, &[], &[], 0);

    assert_eq_test!(
        tcp::get_state(c.id),
        Some(TcpState::Established),
        "the peer's ACK did not complete the simultaneous open"
    );

    crate::tcp::clock::MockClock::advance(600_000);
    let fired = c.scope.dispatch_due(TimerKind::TcpRetransmit);
    assert_test!(
        fired.is_empty(),
        "the established connection kept the handshake's retransmit timer armed"
    );
    pass!()
}

/// `socket_connect_nonblock` matched only `SynSent`, so a healthy simultaneous
/// open was reported to the caller as a refusal.
pub fn test_connect_does_not_report_refusal_mid_cross() -> TestResult {
    use slopos_abi::net::{AF_INET, SOCK_STREAM};
    use slopos_abi::syscall::{ERRNO_EAGAIN, ERRNO_ECONNREFUSED};

    let scope = match NetTestScope::enter_at_mock_ms(6_000) {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };

    let sock =
        crate::socket::socket_create(AF_INET, SOCK_STREAM, 0, crate::socket::SocketOwner::UNOWNED);
    if sock < 0 {
        return fail!("socket_create: {}", sock);
    }
    let sock = sock as u32;

    let rc = crate::socket::socket_connect_nonblock(sock, scope.peer_ip(), scope.peer_port());
    assert_eq_test!(
        rc,
        super::env_wait::errno_i32(ERRNO_EAGAIN),
        "the first nonblocking connect did not start a handshake"
    );

    let Some(id) = crate::socket::socket_lookup_tcp_idx(sock) else {
        return fail!("no PCB after connect");
    };
    let Some(tuple) = tcp::with_pcb(id, |pcb| pcb.tuple) else {
        return fail!("PCB vanished");
    };

    let peer_syn = make_header(
        tuple.remote_port,
        tuple.local_port,
        PEER_IRS,
        0,
        TCP_FLAG_SYN,
        16384,
    );
    let _ = tcp::input(tuple.remote_ip, tuple.local_ip, &peer_syn, &[], &[], 0);
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::SynReceived),
        "the crossed SYN did not reach SYN_RECEIVED"
    );

    let rc = crate::socket::socket_connect_nonblock(sock, scope.peer_ip(), scope.peer_port());
    assert_test!(
        rc != super::env_wait::errno_i32(ERRNO_ECONNREFUSED),
        "a simultaneous open in progress was reported as ECONNREFUSED"
    );
    assert_eq_test!(
        rc,
        super::env_wait::errno_i32(ERRNO_EAGAIN),
        "a handshake still in flight must read as EAGAIN"
    );

    let _ = crate::socket::socket_close(sock);
    pass!()
}

/// `init_new` hardcodes `sack_permitted: false` and `ts_recent: 0`, so every
/// connection promoted through `SynRecv` lost both negotiations.
pub fn test_promotion_keeps_sack_and_timestamps() -> TestResult {
    let scope = match NetTestScope::enter_at_mock_ms(7_000) {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let _ = &scope;

    let (id, syn) = match tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT) {
        Ok(v) => v,
        Err(e) => return fail!("connect: {:?}", e),
    };
    tcp::arm_syn_retransmit(id);
    let local_port = syn.tuple.local_port;

    const PEER_TSVAL: u32 = 0x0BAD_F00D;
    // SACK-permitted, then NOP+NOP+timestamp.
    let mut opts = [0u8; 16];
    opts[0] = 4;
    opts[1] = 2;
    opts[2] = 1;
    opts[3] = 1;
    opts[4] = 8;
    opts[5] = 10;
    opts[6..10].copy_from_slice(&PEER_TSVAL.to_be_bytes());
    opts[10..14].copy_from_slice(&0u32.to_be_bytes());

    let peer_syn = make_header(REMOTE_PORT, local_port, PEER_IRS, 0, TCP_FLAG_SYN, 16384);
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &peer_syn, &opts[..14], &[], 0);

    let ack = make_header(
        REMOTE_PORT,
        local_port,
        PEER_IRS.wrapping_add(1),
        syn.seq_num.wrapping_add(1),
        TCP_FLAG_ACK,
        16384,
    );
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &ack, &[], &[], 0);

    let observed = tcp::with_pcb(id, |pcb| match &pcb.state {
        tcp::PcbState::Data(d) => Some((d.sack_permitted, d.ts_enabled, d.ts_recent)),
        _ => None,
    })
    .flatten();

    let Some((sack, ts_enabled, ts_recent)) = observed else {
        return fail!("the connection did not reach Data");
    };
    assert_test!(sack, "SACK-permitted was dropped promoting SynRecv -> Data");
    assert_test!(
        ts_enabled,
        "timestamps were dropped promoting SynRecv -> Data"
    );
    assert_eq_test!(
        ts_recent,
        PEER_TSVAL,
        "ts_recent was zeroed, so our first ACK echoes TSecr=0"
    );
    pass!()
}

/// RFC 7323 §2.3: scaling is enabled only if both SYNs carried the option. A
/// `SynRecv` that scales its own window without advertising it accepts sequence
/// space the peer never believed it could use.
pub fn test_syn_ack_advertises_window_scale_only_when_offered() -> TestResult {
    let scope = match NetTestScope::enter_at_mock_ms(8_000) {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let _ = &scope;

    let (id, syn) = match tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT) {
        Ok(v) => v,
        Err(e) => return fail!("connect: {:?}", e),
    };
    tcp::arm_syn_retransmit(id);

    // Window Scale, shift 7.
    let opts = [1u8, 3, 3, 7];
    let peer_syn = make_header(
        REMOTE_PORT,
        syn.tuple.local_port,
        PEER_IRS,
        0,
        TCP_FLAG_SYN,
        16384,
    );
    let actions = tcp::input(REMOTE_IP, LOCAL_IP, &peer_syn, &opts, &[], 0);

    let Some(seg) = actions.segments().next() else {
        return fail!("the crossed SYN produced no SYN-ACK");
    };
    assert_test!(
        seg.wscale.is_some(),
        "the peer offered window scale and our SYN-ACK did not echo it, \
         so we scale a window the peer reads raw"
    );
    pass!()
}

/// The same path with no option offered must not advertise one.
pub fn test_syn_ack_omits_window_scale_when_not_offered() -> TestResult {
    let c = match cross(9_000) {
        Ok(c) => c,
        Err(m) => return fail!("{}", m),
    };

    match tcp::on_retransmit(c.id.raw()) {
        RetransmitAction::Segment(seg) => {
            assert_test!(
                seg.wscale.is_none(),
                "a SYN-ACK advertised window scale the peer never offered"
            );
        }
        _ => return fail!("the SYN-ACK was not retransmitted"),
    }
    pass!()
}

slopos_testing::stest!(
    name = test_crossed_syn_arms_a_retransmit,
    suite = tcp_simultaneous_open
);
slopos_testing::stest!(
    name = test_retransmitted_syn_ack_carries_the_original_iss,
    suite = tcp_simultaneous_open
);
slopos_testing::stest!(
    name = test_syn_ack_retransmits_are_bounded_and_release_the_pcb,
    suite = tcp_simultaneous_open
);
slopos_testing::stest!(
    name = test_the_cross_does_not_refill_the_retransmit_budget,
    suite = tcp_simultaneous_open
);
slopos_testing::stest!(
    name = test_completed_handshake_retires_the_syn_ack_timer,
    suite = tcp_simultaneous_open
);
slopos_testing::stest!(
    name = test_connect_does_not_report_refusal_mid_cross,
    suite = tcp_simultaneous_open
);
slopos_testing::stest!(
    name = test_promotion_keeps_sack_and_timestamps,
    suite = tcp_simultaneous_open
);
slopos_testing::stest!(
    name = test_syn_ack_advertises_window_scale_only_when_offered,
    suite = tcp_simultaneous_open
);
slopos_testing::stest!(
    name = test_syn_ack_omits_window_scale_when_not_offered,
    suite = tcp_simultaneous_open
);
