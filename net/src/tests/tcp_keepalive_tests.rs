use slopos_abi::net::{AF_INET, SOCK_STREAM};
use slopos_abi::syscall::{ERRNO_EINPROGRESS, SO_KEEPALIVE, SOL_SOCKET};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::clock::MockClock;
use crate::socket;
use crate::tcp::{self, ConnId, TCP_FLAG_ACK, TcpHeader, TcpOutSegment, TcpState};
use crate::tests::env_wait::errno_i32;
use crate::tests::net_scope::NetTestScope;
use crate::tests::tcp_common::PEER_ISS;
use crate::timer::TimerKind;
use crate::with_data_state;

// Mirrors the private production constants in tcp/mod.rs. The `+ 1` lands just
// past a period so the timer wheel fires that deadline in one dispatch.
const KEEPALIVE_IDLE_MS: u64 = 7_200 * 1_000;
const KEEPALIVE_INTERVAL_MS: u64 = 75 * 1_000;
const IDLE_ADVANCE_MS: u64 = KEEPALIVE_IDLE_MS + 1;
const INTERVAL_ADVANCE_MS: u64 = KEEPALIVE_INTERVAL_MS + 1;

/// Pins keepalive deadlines to mock time so `MockClock::advance` can cross them.
const MOCK_START_MS: u64 = 1;

fn connect_and_establish(
    scope: &NetTestScope,
    keepalive_enabled: bool,
) -> Result<(u32, ConnId), &'static str> {
    let sock = socket::socket_create(AF_INET, SOCK_STREAM, 0, socket::SocketOwner::UNOWNED);
    if sock < 0 {
        return Err("socket_create failed");
    }
    let sock = sock as u32;

    if keepalive_enabled {
        let one: i32 = 1;
        if socket::socket_setsockopt(sock, SOL_SOCKET, SO_KEEPALIVE, &one.to_ne_bytes()) != 0 {
            return Err("socket_setsockopt(SO_KEEPALIVE) failed");
        }
    }

    socket::socket_set_nonblocking(sock, true);
    let rc = socket::socket_connect(sock, scope.peer_ip(), scope.peer_port());
    if rc < 0 && rc != errno_i32(ERRNO_EINPROGRESS) {
        return Err("socket_connect failed");
    }

    let Some(tcp_id) = socket::socket_lookup_tcp_idx(sock) else {
        return Err("socket_lookup_tcp_idx failed");
    };
    if scope.inject_syn_ack(tcp_id, PEER_ISS).is_none() {
        return Err("no PCB in a handshake state for the synthetic SYN+ACK");
    }

    Ok((sock, tcp_id))
}

/// Every popped keepalive must be dispatched before returning: `dispatch_due`
/// removes what it hands back, so an early return would discard the rest.
fn dispatch_next_keepalive_for_conn(
    scope: &NetTestScope,
    tcp_id: ConnId,
    advance_ms: u64,
) -> Option<Option<TcpOutSegment>> {
    let key = tcp_id.raw();
    MockClock::advance(advance_ms);

    let mut ours = None;
    for timer in scope.dispatch_due(TimerKind::TcpKeepalive) {
        let probe = tcp::on_keepalive(timer.key);
        if timer.key == key {
            ours = Some(probe);
        }
    }
    ours
}

fn inject_inbound_data(tcp_id: ConnId, payload: &[u8]) {
    let (tuple, rcv_nxt, snd_nxt) = tcp::with_pcb(tcp_id, |pcb| match &pcb.state {
        tcp::PcbState::Data(d) => (pcb.tuple, d.rcv_nxt.raw(), d.snd_nxt.raw()),
        other => panic!("expected Data state, got {}", other.name()),
    })
    .expect("PCB should exist");
    let hdr = TcpHeader {
        src_port: tuple.remote_port,
        dst_port: tuple.local_port,
        seq_num: rcv_nxt,
        ack_num: snd_nxt,
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };

    let result = tcp::input(
        tuple.remote_ip,
        tuple.local_ip,
        &hdr,
        &[],
        payload,
        crate::clock::now_ms(),
    );
    socket::socket_notify_tcp_activity(&result);
}

pub fn test_keepalive_fires_after_idle() -> TestResult {
    let scope = match NetTestScope::enter_at_mock_ms(MOCK_START_MS) {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };

    let (_sock, tcp_id) = match connect_and_establish(&scope, true) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };

    assert_eq_test!(
        tcp::get_state(tcp_id),
        Some(TcpState::Established),
        "connection established"
    );
    with_data_state!(tcp_id, |d| {
        assert_test!(d.keepalive_token.is_some(), "keepalive timer scheduled");
        assert_eq_test!(d.keepalive_probes_sent, 0, "no probes before idle expiry");
    });

    let fired = dispatch_next_keepalive_for_conn(&scope, tcp_id, IDLE_ADVANCE_MS);
    assert_test!(fired.is_some(), "keepalive timer should fire after idle");
    assert_test!(
        fired.unwrap().is_some(),
        "keepalive dispatch returns a probe segment"
    );

    with_data_state!(tcp_id, |d| {
        assert_eq_test!(
            d.keepalive_probes_sent,
            1,
            "first keepalive probe increments counter"
        );
        assert_test!(
            d.keepalive_token.is_some(),
            "next keepalive timer is scheduled"
        );
    });

    pass!()
}

pub fn test_keepalive_reset_on_data() -> TestResult {
    let scope = match NetTestScope::enter_at_mock_ms(MOCK_START_MS) {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };

    let (_sock, tcp_id) = match connect_and_establish(&scope, true) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };

    let first_fire = dispatch_next_keepalive_for_conn(&scope, tcp_id, IDLE_ADVANCE_MS);
    assert_test!(first_fire.is_some(), "first idle keepalive should fire");
    assert_test!(first_fire.unwrap().is_some(), "first keepalive emits probe");
    with_data_state!(tcp_id, |d| {
        assert_eq_test!(
            d.keepalive_probes_sent,
            1,
            "probe count is 1 after first keepalive"
        );
    });

    inject_inbound_data(tcp_id, b"x");
    with_data_state!(tcp_id, |d| {
        assert_eq_test!(
            d.keepalive_probes_sent,
            0,
            "inbound data resets probe count"
        );
        assert_test!(
            d.keepalive_token.is_some(),
            "inbound data keeps keepalive timer armed"
        );
    });

    let old_deadline_fire = dispatch_next_keepalive_for_conn(&scope, tcp_id, INTERVAL_ADVANCE_MS);
    assert_test!(
        old_deadline_fire.is_none(),
        "old keepalive deadline should not fire after reset"
    );

    let new_deadline_fire = dispatch_next_keepalive_for_conn(&scope, tcp_id, IDLE_ADVANCE_MS);
    assert_test!(
        new_deadline_fire.is_some(),
        "keepalive should fire again after new idle period"
    );
    assert_test!(
        new_deadline_fire.unwrap().is_some(),
        "new keepalive expiry emits probe"
    );
    with_data_state!(tcp_id, |d| {
        assert_eq_test!(
            d.keepalive_probes_sent,
            1,
            "probe count restarts from 0 and becomes 1"
        );
    });

    pass!()
}

pub fn test_keepalive_max_probes_rst() -> TestResult {
    let scope = match NetTestScope::enter_at_mock_ms(MOCK_START_MS) {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };

    let (_sock, tcp_id) = match connect_and_establish(&scope, true) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };

    let mut keepalive_fires = 0usize;
    let mut probes_emitted = 0usize;

    for _ in 0..12 {
        let max_wait = if keepalive_fires == 0 {
            IDLE_ADVANCE_MS
        } else {
            INTERVAL_ADVANCE_MS
        };

        let fired = match dispatch_next_keepalive_for_conn(&scope, tcp_id, max_wait) {
            Some(v) => v,
            None => return fail!("expected keepalive timer fire before close"),
        };

        keepalive_fires += 1;
        if fired.is_some() {
            probes_emitted += 1;
        }

        if tcp::get_state(tcp_id).is_none() {
            break;
        }
    }

    assert_eq_test!(
        probes_emitted,
        9,
        "exactly max keepalive probes are emitted"
    );
    assert_eq_test!(
        keepalive_fires,
        10,
        "connection closes on the keepalive fire after max probes"
    );
    assert_test!(
        tcp::get_state(tcp_id).is_none(),
        "connection is released after max keepalive probes"
    );

    pass!()
}

pub fn test_keepalive_disabled_no_timer() -> TestResult {
    let scope = match NetTestScope::enter_at_mock_ms(MOCK_START_MS) {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };

    let (_sock, tcp_id) = match connect_and_establish(&scope, false) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };

    assert_eq_test!(
        tcp::get_state(tcp_id),
        Some(TcpState::Established),
        "connection established"
    );
    with_data_state!(tcp_id, |d| {
        assert_test!(
            d.keepalive_token.is_none(),
            "keepalive disabled should not schedule timer"
        );
        assert_eq_test!(d.keepalive_probes_sent, 0, "probe count remains zero");
    });

    pass!()
}

pub fn test_keepalive_cancelled_on_close() -> TestResult {
    let scope = match NetTestScope::enter_at_mock_ms(MOCK_START_MS) {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };

    let (sock, tcp_id) = match connect_and_establish(&scope, true) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };

    with_data_state!(tcp_id, |d| {
        assert_test!(
            d.keepalive_token.is_some(),
            "keepalive timer is armed before close"
        );
    });

    assert_eq_test!(socket::socket_close(sock), 0, "socket_close succeeds");

    let fired_after_close = dispatch_next_keepalive_for_conn(&scope, tcp_id, IDLE_ADVANCE_MS);
    assert_test!(
        fired_after_close.is_none(),
        "no keepalive dispatch occurs after close cancels timer"
    );

    pass!()
}

slopos_testing::stest!(
    name = test_keepalive_fires_after_idle,
    suite = tcp_keepalive
);
slopos_testing::stest!(name = test_keepalive_reset_on_data, suite = tcp_keepalive);
slopos_testing::stest!(name = test_keepalive_max_probes_rst, suite = tcp_keepalive);
slopos_testing::stest!(
    name = test_keepalive_disabled_no_timer,
    suite = tcp_keepalive
);
slopos_testing::stest!(
    name = test_keepalive_cancelled_on_close,
    suite = tcp_keepalive
);
