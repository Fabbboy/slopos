use slopos_abi::net::{AF_INET, SOCK_STREAM};
use slopos_abi::syscall::{SO_KEEPALIVE, SOL_SOCKET};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::socket;
use crate::tcp::{self, ConnId, TCP_FLAG_ACK, TCP_FLAG_SYN, TcpHeader, TcpOutSegment, TcpState};
use crate::tests::tcp_common::reset_all as reset;
use crate::timer::{NET_TIMER_WHEEL, TimerKind};
use crate::with_data_state;

// Keepalive periods (mirrors the private production constants in tcp/mod.rs).
// Advancing the unified mock clock by just past a period crosses the next
// keepalive deadline so the timer wheel fires it in a single `process_due()`.
const KEEPALIVE_IDLE_MS: u64 = 7_200 * 1_000;
const KEEPALIVE_INTERVAL_MS: u64 = 75 * 1_000;
const IDLE_ADVANCE_MS: u64 = KEEPALIVE_IDLE_MS + 1;
const INTERVAL_ADVANCE_MS: u64 = KEEPALIVE_INTERVAL_MS + 1;

/// Start each keepalive test on the mock clock so every keepalive deadline is
/// expressed in mock time and a later `MockClock::advance` can cross it.
///
/// Returns the [`MockClockGuard`](crate::clock::MockClockGuard) that restores
/// real time on drop; callers bind it for the whole test body
/// (`let _clock = keepalive_setup();`) so the pinned clock cannot leak into a
/// later test or the userland phase.
#[must_use = "bind the returned MockClockGuard for the test body, else the clock is restored immediately"]
fn keepalive_setup() -> crate::clock::MockClockGuard {
    reset();
    crate::clock::MockClockGuard::install_at(1)
}

fn connect_and_establish(keepalive_enabled: bool) -> Result<(u32, ConnId), &'static str> {
    let sock = socket::socket_create(AF_INET, SOCK_STREAM, 0);
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
    let rc = socket::socket_connect(sock, [10, 0, 0, 2], 80);
    if rc < 0 && rc != -115 {
        return Err("socket_connect failed");
    }

    let Some(tcp_id) = socket::socket_lookup_tcp_idx(sock) else {
        return Err("socket_lookup_tcp_idx failed");
    };
    let (tuple, iss) = tcp::with_pcb(tcp_id, |pcb| {
        let iss = match &pcb.state {
            tcp::PcbState::SynSent(s) => s.iss.raw(),
            tcp::PcbState::Data(d) => d.iss.raw(),
            _ => return Err("unexpected PCB state"),
        };
        Ok((pcb.tuple, iss))
    })
    .ok_or("PCB not found")??;

    let syn_ack = TcpHeader {
        src_port: tuple.remote_port,
        dst_port: tuple.local_port,
        seq_num: 9000,
        ack_num: iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(tuple.remote_ip, tuple.local_ip, &syn_ack, &[], &[], 0);
    socket::socket_notify_tcp_activity(&result);

    Ok((sock, tcp_id))
}

/// Advance the unified clock by `advance_ms`, fire every now-due timer, and
/// return this connection's keepalive outcome if one fired: `Some(Some(seg))`
/// for a probe, `Some(None)` if the connection was released, `None` if no
/// keepalive for this connection was due.
fn dispatch_next_keepalive_for_conn(
    tcp_id: ConnId,
    advance_ms: u64,
) -> Option<Option<TcpOutSegment>> {
    let key = tcp_id.0;
    crate::clock::MockClock::advance(advance_ms);
    let fired = NET_TIMER_WHEEL.process_due();
    for timer in fired {
        if timer.kind != TimerKind::TcpKeepalive {
            continue;
        }

        let probe = tcp::on_keepalive(timer.key);
        if timer.key == key {
            return Some(probe);
        }
    }

    None
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

    let result = tcp::input(tuple.remote_ip, tuple.local_ip, &hdr, &[], payload, 0);
    socket::socket_notify_tcp_activity(&result);
}

pub fn test_keepalive_fires_after_idle() -> TestResult {
    let _clock = keepalive_setup();

    let (_sock, tcp_id) = match connect_and_establish(true) {
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

    let fired = dispatch_next_keepalive_for_conn(tcp_id, IDLE_ADVANCE_MS);
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
    let _clock = keepalive_setup();

    let (_sock, tcp_id) = match connect_and_establish(true) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };

    let first_fire = dispatch_next_keepalive_for_conn(tcp_id, IDLE_ADVANCE_MS);
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

    let old_deadline_fire = dispatch_next_keepalive_for_conn(tcp_id, INTERVAL_ADVANCE_MS);
    assert_test!(
        old_deadline_fire.is_none(),
        "old keepalive deadline should not fire after reset"
    );

    let new_deadline_fire = dispatch_next_keepalive_for_conn(tcp_id, IDLE_ADVANCE_MS);
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
    let _clock = keepalive_setup();

    let (_sock, tcp_id) = match connect_and_establish(true) {
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

        let fired = match dispatch_next_keepalive_for_conn(tcp_id, max_wait) {
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
    let _clock = keepalive_setup();

    let (_sock, tcp_id) = match connect_and_establish(false) {
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
    let _clock = keepalive_setup();

    let (sock, tcp_id) = match connect_and_establish(true) {
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

    let fired_after_close = dispatch_next_keepalive_for_conn(tcp_id, IDLE_ADVANCE_MS);
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
