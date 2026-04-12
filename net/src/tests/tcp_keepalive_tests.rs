use slopos_abi::net::{AF_INET, SOCK_STREAM};
use slopos_abi::syscall::{SO_KEEPALIVE, SOL_SOCKET};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::socket;
use crate::tcp::{self, ConnId, TCP_FLAG_ACK, TCP_FLAG_SYN, TcpHeader, TcpOutSegment, TcpState};
use crate::tests::tcp_common::reset_all as reset;
use crate::timer::{NET_TIMER_WHEEL, TimerKind};
use crate::with_data_state;

const MAX_IDLE_WAIT_TICKS: u64 = 900_000;
const MAX_INTERVAL_WAIT_TICKS: u64 = 20_000;

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

fn dispatch_next_keepalive_for_conn(
    tcp_id: ConnId,
    max_ticks: u64,
) -> Option<Option<TcpOutSegment>> {
    let key = tcp_id.0;
    for _ in 0..max_ticks {
        let fired = NET_TIMER_WHEEL.tick();
        for timer in fired {
            if timer.kind != TimerKind::TcpKeepalive {
                continue;
            }

            let probe = tcp::on_keepalive(timer.key);
            if timer.key == key {
                return Some(probe);
            }
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
    reset();

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

    let fired = dispatch_next_keepalive_for_conn(tcp_id, MAX_IDLE_WAIT_TICKS);
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
    reset();

    let (_sock, tcp_id) = match connect_and_establish(true) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };

    let first_fire = dispatch_next_keepalive_for_conn(tcp_id, MAX_IDLE_WAIT_TICKS);
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

    let old_deadline_fire = dispatch_next_keepalive_for_conn(tcp_id, MAX_INTERVAL_WAIT_TICKS);
    assert_test!(
        old_deadline_fire.is_none(),
        "old keepalive deadline should not fire after reset"
    );

    let new_deadline_fire = dispatch_next_keepalive_for_conn(tcp_id, MAX_IDLE_WAIT_TICKS);
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
    reset();

    let (_sock, tcp_id) = match connect_and_establish(true) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };

    let mut keepalive_fires = 0usize;
    let mut probes_emitted = 0usize;

    for _ in 0..12 {
        let max_wait = if keepalive_fires == 0 {
            MAX_IDLE_WAIT_TICKS
        } else {
            MAX_INTERVAL_WAIT_TICKS
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
    reset();

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
    reset();

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

    let fired_after_close = dispatch_next_keepalive_for_conn(tcp_id, MAX_IDLE_WAIT_TICKS);
    assert_test!(
        fired_after_close.is_none(),
        "no keepalive dispatch occurs after close cancels timer"
    );

    pass!()
}

slopos_testing::define_test_suite!(
    tcp_keepalive,
    [
        test_keepalive_fires_after_idle,
        test_keepalive_reset_on_data,
        test_keepalive_max_probes_rst,
        test_keepalive_disabled_no_timer,
        test_keepalive_cancelled_on_close,
    ]
);
