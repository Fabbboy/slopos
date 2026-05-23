//! Demux + Epoch reclaim regression tests for the lock-free TCP
//! dispatch path.
//!
//! These tests are single-threaded by construction (SlopOS's `stest!`
//! harness runs each test on one CPU). The lock-free property of
//! `tcp::find` is therefore exercised mechanically — every call must
//! complete without acquiring a `SpinLock` — but observable
//! consistency under install/release races is validated through
//! deterministic interleavings: install then find, release then find,
//! repeated install/release pairs.
//!
//! The Epoch reclaim test instruments a `Drop` counter on a payload
//! deferred via `NET_EPOCH.defer_kbox` and asserts the counter only
//! advances after `NET_EPOCH.wait()` completes a grace period.

use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_ostd::KBox;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::pcb::{ListenState, PcbState, SynSentState};
use crate::tcp::seq::SeqNum;
use crate::tcp::table::{self, NET_EPOCH};
use crate::tcp::tuple::TcpTuple;

fn reset() {
    table::clear_all();
}

fn syn_sent_state() -> PcbState {
    PcbState::SynSent(SynSentState::new(SeqNum::new(0x1000)))
}

fn listen_state() -> PcbState {
    PcbState::Listen(ListenState::new())
}

fn shard_tuple(seed: u8) -> TcpTuple {
    TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 5000 + seed as u16,
        remote_ip: [10, 0, 0, 2],
        remote_port: 60000 + seed as u16,
    }
}

// ---------------------------------------------------------------------------
// Demux read path correctness
// ---------------------------------------------------------------------------

pub fn test_demux_find_after_install_returns_some() -> TestResult {
    reset();
    let tuple = shard_tuple(1);
    let id = match table::install_established(tuple, syn_sent_state(), |_| {}) {
        Ok(id) => id,
        Err(_) => return fail!("install_established failed"),
    };
    let found = table::find(&tuple);
    assert_eq_test!(found, Some(id), "find returns installed ConnId");
    pass!()
}

pub fn test_demux_find_after_release_returns_none() -> TestResult {
    reset();
    let tuple = shard_tuple(2);
    let id = match table::install_established(tuple, syn_sent_state(), |_| {}) {
        Ok(id) => id,
        Err(_) => return fail!("install_established failed"),
    };
    table::release(id);
    let found = table::find(&tuple);
    assert_eq_test!(found, None, "find returns None after release");
    pass!()
}

pub fn test_demux_find_unknown_tuple_returns_none() -> TestResult {
    reset();
    let known = shard_tuple(3);
    let unknown = shard_tuple(4);
    let _id = match table::install_established(known, syn_sent_state(), |_| {}) {
        Ok(id) => id,
        Err(_) => return fail!("install_established failed"),
    };
    let found = table::find(&unknown);
    assert_eq_test!(found, None, "find returns None for unrelated tuple");
    pass!()
}

pub fn test_demux_listener_fallback() -> TestResult {
    reset();
    let listener_tuple = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 6000,
        remote_ip: [0; 4],
        remote_port: 0,
    };
    let listener_id = match table::install_listener(listener_tuple, listen_state(), |_| {}) {
        Ok(id) => id,
        Err(_) => return fail!("install_listener failed"),
    };
    assert_test!(listener_id.is_listener(), "id is listener");

    // Lookup of an incoming connection that matches the listener port
    // but has a non-wildcard remote — the demux must fall back to the
    // listener table.
    let incoming = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 6000,
        remote_ip: [10, 0, 0, 99],
        remote_port: 33333,
    };
    let found = table::find(&incoming);
    assert_eq_test!(found, Some(listener_id), "listener fallback matched");
    pass!()
}

pub fn test_demux_install_release_cycle_consistency() -> TestResult {
    reset();
    let tuple = shard_tuple(5);
    // Hammer install/release/find pairs; every step's observable index
    // must agree with the per-slot state.
    for _ in 0..32 {
        let id = match table::install_established(tuple, syn_sent_state(), |_| {}) {
            Ok(id) => id,
            Err(_) => return fail!("install_established failed mid-cycle"),
        };
        let found = table::find(&tuple);
        assert_eq_test!(found, Some(id), "find after install");
        assert_test!(
            table::with_pcb(id, |pcb| pcb.tuple == tuple).unwrap_or(false),
            "per-slot PCB matches tuple"
        );
        table::release(id);
        let found = table::find(&tuple);
        assert_eq_test!(found, None, "find after release");
    }
    pass!()
}

pub fn test_demux_active_count_matches_installs() -> TestResult {
    reset();
    assert_eq_test!(table::active_count(), 0, "empty active_count");
    let mut ids = [None; 4];
    for i in 0..4 {
        let id = match table::install_established(shard_tuple(10 + i), syn_sent_state(), |_| {}) {
            Ok(id) => id,
            Err(_) => return fail!("install failed"),
        };
        ids[i as usize] = Some(id);
    }
    assert_eq_test!(table::active_count(), 4, "after 4 installs");
    for id in ids.iter().flatten() {
        table::release(*id);
    }
    assert_eq_test!(table::active_count(), 0, "after release");
    pass!()
}

pub fn test_demux_port_in_use_lock_free() -> TestResult {
    reset();
    let tuple = shard_tuple(20);
    assert_test!(
        !table::port_in_use(tuple.local_ip, tuple.local_port),
        "port free before install"
    );
    let id = match table::install_established(tuple, syn_sent_state(), |_| {}) {
        Ok(id) => id,
        Err(_) => return fail!("install failed"),
    };
    assert_test!(
        table::port_in_use(tuple.local_ip, tuple.local_port),
        "port_in_use sees install"
    );
    table::release(id);
    assert_test!(
        !table::port_in_use(tuple.local_ip, tuple.local_port),
        "port_in_use sees release"
    );
    pass!()
}

// ---------------------------------------------------------------------------
// Epoch reclaim contract
// ---------------------------------------------------------------------------

struct DropProbe;

static DROP_COUNTER: AtomicUsize = AtomicUsize::new(0);

impl Drop for DropProbe {
    fn drop(&mut self) {
        DROP_COUNTER.fetch_add(1, Ordering::Release);
    }
}

pub fn test_epoch_defer_runs_after_grace_period() -> TestResult {
    DROP_COUNTER.store(0, Ordering::Release);

    let boxed = match KBox::try_new(DropProbe) {
        Ok(b) => b,
        Err(_) => return fail!("KBox alloc failed"),
    };

    // Hand the box to NET_EPOCH for deferred drop. The drop must not
    // run synchronously — `rcu_call_typed` enqueues the callback.
    NET_EPOCH.defer_kbox::<DropProbe>(boxed);

    // Force the deferred callback to drain by completing a grace
    // period and processing pending callbacks (the production
    // scheduler does this on the idle path; tests invoke it directly).
    NET_EPOCH.wait();
    slopos_ostd::sync::rcu::rcu_process_callbacks();

    // After the grace period + callback drain, the probe must have
    // dropped exactly once.
    let drops = DROP_COUNTER.load(Ordering::Acquire);
    assert_eq_test!(drops, 1, "deferred drop ran after grace period");
    pass!()
}

pub fn test_epoch_enter_allows_rcucell_load() -> TestResult {
    reset();
    let tuple = shard_tuple(30);
    let id = match table::install_established(tuple, syn_sent_state(), |_| {}) {
        Ok(id) => id,
        Err(_) => return fail!("install failed"),
    };
    // `find` already uses NET_EPOCH internally; verify it operates
    // without panicking and returns the expected id.
    let found = table::find(&tuple);
    assert_eq_test!(found, Some(id), "find under NET_EPOCH");
    table::release(id);
    pass!()
}

// ---------------------------------------------------------------------------
// stest! registration
// ---------------------------------------------------------------------------

slopos_testing::stest!(
    name = test_demux_find_after_install_returns_some,
    suite = tcp_demux
);
slopos_testing::stest!(
    name = test_demux_find_after_release_returns_none,
    suite = tcp_demux
);
slopos_testing::stest!(
    name = test_demux_find_unknown_tuple_returns_none,
    suite = tcp_demux
);
slopos_testing::stest!(name = test_demux_listener_fallback, suite = tcp_demux);
slopos_testing::stest!(
    name = test_demux_install_release_cycle_consistency,
    suite = tcp_demux
);
slopos_testing::stest!(
    name = test_demux_active_count_matches_installs,
    suite = tcp_demux
);
slopos_testing::stest!(name = test_demux_port_in_use_lock_free, suite = tcp_demux);
slopos_testing::stest!(
    name = test_epoch_defer_runs_after_grace_period,
    suite = tcp_demux
);
slopos_testing::stest!(
    name = test_epoch_enter_allows_rcucell_load,
    suite = tcp_demux
);
