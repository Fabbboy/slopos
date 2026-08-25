//! Slot-reuse regression tests for [`ConnId`]'s generation.
//!
//! Without a generation an id names a *slot*, so a holder that outlived its
//! connection silently addresses whichever connection moved in. Each test
//! drives that reuse deterministically: install, release, install into the same
//! slot, then act with the first id.
//!
//! Every test holds a [`NetTestScope`]: the slots, generations and stale-lookup
//! counter below are global table state the live ingress and net-timer threads
//! mutate too. `reset()` stays inside the scope, which clears the table before
//! it gates ingress, so a frame already in flight can still install after that
//! clear.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::pcb::{ListenState, PcbState, SynSentState};
use crate::tcp::seq::SeqNum;
use crate::tcp::table::{self, ConnId};
use crate::tcp::tuple::TcpTuple;
use crate::tests::net_scope::{NetTestScope, ScopeError};
use crate::tests::tcp_common::{LOCAL_IP, REMOTE_IP};

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

/// The one tuple every test installs: the same 4-tuple hashes to the same shard
/// and takes the same slot, which is the recycling under test.
fn tuple() -> TcpTuple {
    TcpTuple {
        local_ip: LOCAL_IP,
        local_port: 5100,
        remote_ip: REMOTE_IP,
        remote_port: 60100,
    }
}

fn install() -> ConnId {
    table::install_established(tuple(), syn_sent_state(), |_| {}).expect("install")
}

fn install_listener() -> ConnId {
    table::install_listener(
        TcpTuple {
            local_ip: [0; 4],
            local_port: 8080,
            remote_ip: [0; 4],
            remote_port: 0,
        },
        PcbState::Listen(ListenState::new()),
        |_| {},
    )
    .expect("install listener")
}

pub fn test_generation_advances_on_release() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    reset();
    let first = install();
    table::release(first);
    let second = install();

    assert_eq_test!(second.shard(), first.shard(), "same shard");
    assert_eq_test!(second.slot(), first.slot(), "same slot");
    assert_test!(
        second != first,
        "a refilled slot must not answer to the previous occupant's id"
    );
    assert_eq_test!(
        second.generation(),
        first.generation() + 1,
        "release advances the slot's generation"
    );
    pass!()
}

pub fn test_stale_id_rejected_by_every_accessor() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    reset();
    let stale = install();
    table::release(stale);
    let live = install();
    assert_eq_test!(live.slot(), stale.slot(), "reused the same slot");

    assert_test!(
        table::with_pcb(stale, |_| ()).is_none(),
        "with_pcb resolved a stale id"
    );
    assert_test!(
        table::with_pcb_mut(stale, |_| ()).is_none(),
        "with_pcb_mut resolved a stale id"
    );
    assert_test!(
        table::with_pcb_and_bufs(stale, |_, _| ()).is_none(),
        "with_pcb_and_bufs resolved a stale id"
    );
    assert_test!(
        table::with_bufs(stale, |_| ()).is_none(),
        "with_bufs resolved a stale id"
    );
    assert_test!(!table::has_buffer(stale), "has_buffer resolved a stale id");

    assert_test!(
        table::with_pcb(live, |_| ()).is_some(),
        "the live id must still resolve"
    );
    pass!()
}

/// A socket that has not noticed its connection close still holds the old id,
/// and `close`/`abort` reach `release` with it.
pub fn test_stale_release_does_not_evict_live_connection() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    reset();
    let stale = install();
    table::release(stale);
    let live = install();

    table::release(stale);

    assert_eq_test!(
        table::find(&tuple()),
        Some(live),
        "a stale release must not evict the connection now in the slot"
    );
    assert_test!(
        table::with_pcb(live, |_| ()).is_some(),
        "the live connection survives a stale release"
    );
    pass!()
}

/// Demux mints ids from the same RCU snapshot it matched the tuple in, so a
/// lookup can never hand back an id for an occupant that snapshot did not see.
pub fn test_find_returns_current_generation() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    reset();
    let first = install();
    table::release(first);
    let second = install();

    assert_eq_test!(
        table::find(&tuple()),
        Some(second),
        "find names the occupant"
    );
    pass!()
}

pub fn test_stale_lookup_counter_increments() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    reset();
    let stale = install();
    table::release(stale);
    let _live = install();

    let before = table::stale_lookup_count();
    let _ = table::with_pcb(stale, |_| ());
    let after = table::stale_lookup_count();
    assert_test!(after > before, "a rejected stale lookup must be counted");

    let malformed = table::stale_lookup_count();
    let _ = table::with_pcb(ConnId::from_raw(999), |_| ());
    assert_eq_test!(
        table::stale_lookup_count(),
        malformed,
        "a malformed id is not a stale one"
    );
    pass!()
}

/// `clear_all` advances rather than resets the generations, so ids the cleared
/// connections had already issued do not revalidate.
pub fn test_clear_all_advances_generations() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    reset();
    let stale = install();
    table::clear_all();
    let live = install();

    assert_test!(stale != live, "clear_all must not hand back a live id");
    assert_test!(
        table::with_pcb(stale, |_| ()).is_none(),
        "an id from before clear_all must not resolve"
    );
    pass!()
}

pub fn test_listener_generation() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    reset();
    let first = install_listener();
    table::release(first);
    let second = install_listener();

    assert_eq_test!(second.slot(), first.slot(), "same listener slot");
    assert_test!(second != first, "a reused listener slot gets a fresh id");
    assert_test!(
        table::with_pcb(first, |_| ()).is_none(),
        "a stale listener id must not resolve"
    );
    assert_test!(
        table::with_pcb(second, |_| ()).is_some(),
        "the live listener resolves"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_generation_advances_on_release,
    suite = tcp_connid
);
slopos_testing::stest!(
    name = test_stale_id_rejected_by_every_accessor,
    suite = tcp_connid
);
slopos_testing::stest!(
    name = test_stale_release_does_not_evict_live_connection,
    suite = tcp_connid
);
slopos_testing::stest!(
    name = test_find_returns_current_generation,
    suite = tcp_connid
);
slopos_testing::stest!(
    name = test_stale_lookup_counter_increments,
    suite = tcp_connid
);
slopos_testing::stest!(
    name = test_clear_all_advances_generations,
    suite = tcp_connid
);
slopos_testing::stest!(name = test_listener_generation, suite = tcp_connid);
