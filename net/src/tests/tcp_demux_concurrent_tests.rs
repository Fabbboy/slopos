//! Demux + Epoch reclaim regression tests for the lock-free TCP dispatch path.
//!
//! `stest!` runs each test on one CPU, so install/release races are covered
//! only through deterministic interleavings, not real concurrency.
//!
//! Every test that reads the table holds a [`NetTestScope`]: `find`,
//! `port_in_use` and the active-slot snapshot are global state the live ingress
//! and net-timer threads mutate too. The epoch test asserts only on its own
//! drop counter and takes none.

use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_ostd::KBox;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::pcb::{ListenState, PcbState, SynSentState};
use crate::tcp::seq::SeqNum;
use crate::tcp::table::{self, ConnId, NET_EPOCH};
use crate::tcp::tuple::TcpTuple;
use crate::tests::net_scope::{NetTestScope, ScopeError};
use crate::tests::tcp_common::{LOCAL_IP, REMOTE_IP};

/// A peer no tuple here installs, so the listener match cannot be a full-tuple hit.
const OTHER_REMOTE_IP: [u8; 4] = [REMOTE_IP[0], REMOTE_IP[1], REMOTE_IP[2], 99];

#[cold]
#[inline(never)]
fn scope_error(e: ScopeError) -> TestResult {
    fail!("net scope: {:?}", e)
}

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
        local_ip: LOCAL_IP,
        local_port: 5000 + seed as u16,
        remote_ip: REMOTE_IP,
        remote_port: 60000 + seed as u16,
    }
}

pub fn test_demux_find_after_install_returns_some() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
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
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
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
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
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
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    reset();
    let listener_tuple = TcpTuple {
        local_ip: LOCAL_IP,
        local_port: 6000,
        remote_ip: [0; 4],
        remote_port: 0,
    };
    let listener_id = match table::install_listener(listener_tuple, listen_state(), |_| {}) {
        Ok(id) => id,
        Err(_) => return fail!("install_listener failed"),
    };
    assert_test!(listener_id.is_listener(), "id is listener");

    // Matching local port with a non-wildcard remote must fall back to the
    // listener table.
    let incoming = TcpTuple {
        local_ip: LOCAL_IP,
        local_port: 6000,
        remote_ip: OTHER_REMOTE_IP,
        remote_port: 33333,
    };
    let found = table::find(&incoming);
    assert_eq_test!(found, Some(listener_id), "listener fallback matched");
    pass!()
}

pub fn test_demux_install_release_cycle_consistency() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    reset();
    let tuple = shard_tuple(5);
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

/// How many of `ids` the table still lists as active. The snapshot array is
/// this function's frame, not its caller's.
fn active_among(ids: &[Option<ConnId>]) -> usize {
    let mut live = [None; table::TOTAL_PCB_SLOTS];
    let n = table::snapshot_shard_conn_ids(&mut live);
    ids.iter()
        .flatten()
        .filter(|id| live[..n].contains(&Some(**id)))
        .count()
}

pub fn test_demux_active_count_matches_installs() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    reset();
    let tuples = [
        shard_tuple(10),
        shard_tuple(11),
        shard_tuple(12),
        shard_tuple(13),
    ];
    for tuple in &tuples {
        assert_eq_test!(table::find(tuple), None, "test tuple starts uninstalled");
    }

    let mut ids = [None; 4];
    for (slot, tuple) in ids.iter_mut().zip(tuples.iter()) {
        match table::install_established(*tuple, syn_sent_state(), |_| {}) {
            Ok(id) => *slot = Some(id),
            Err(_) => return fail!("install failed"),
        }
    }
    // The generation in a ConnId makes this count the test's own connections
    // rather than the slots they occupy, so a concurrent install elsewhere in
    // the table is neither counted nor mistaken for one of these.
    assert_eq_test!(active_among(&ids), 4, "after 4 installs");

    for id in ids.iter().flatten() {
        table::release(*id);
    }
    assert_eq_test!(active_among(&ids), 0, "after release");
    pass!()
}

pub fn test_demux_port_in_use_lock_free() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
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

    NET_EPOCH.defer_kbox::<DropProbe>(boxed);

    // A grace period elapsing and the callback having been invoked are two
    // separate facts; `rcu_barrier` is the one that reports the second, and it
    // drives the drain itself rather than waiting on an idle peer.
    NET_EPOCH.wait();
    slopos_ostd::sync::rcu::rcu_barrier();

    let drops = DROP_COUNTER.load(Ordering::Acquire);
    assert_eq_test!(drops, 1, "deferred drop ran after grace period");
    pass!()
}

pub fn test_epoch_enter_allows_rcucell_load() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    reset();
    let tuple = shard_tuple(30);
    let id = match table::install_established(tuple, syn_sent_state(), |_| {}) {
        Ok(id) => id,
        Err(_) => return fail!("install failed"),
    };
    // `find` already uses NET_EPOCH internally.
    let found = table::find(&tuple);
    assert_eq_test!(found, Some(id), "find under NET_EPOCH");
    table::release(id);
    pass!()
}

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
