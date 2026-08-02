//! Slot-reuse regression tests for [`ConnId`]'s generation.
//!
//! The established table is 16 shards × 4 slots and the listener table is 16
//! entries, so a busy stack recycles slots constantly. Without a generation an
//! id names a *slot*, and a holder that outlived its connection — a queued
//! timer, a socket that has not noticed the close, a local across a dropped
//! lock — silently addresses whichever connection moved in.
//!
//! Each test drives that reuse deterministically on the single-CPU `stest!`
//! harness: install, release, install into the same slot, then act with the
//! first id.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::tcp::pcb::{ListenState, PcbState, SynSentState};
use crate::tcp::seq::SeqNum;
use crate::tcp::table::{self, ConnId};
use crate::tcp::tuple::TcpTuple;

fn reset() {
    table::clear_all();
}

fn syn_sent_state() -> PcbState {
    PcbState::SynSent(SynSentState::new(SeqNum::new(0x1000)))
}

/// The one tuple every test here installs. Reusing it is the point: the same
/// 4-tuple hashes to the same shard and takes the same slot, which is exactly
/// the recycling a stale id has to survive.
fn tuple() -> TcpTuple {
    TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 5100,
        remote_ip: [10, 0, 0, 2],
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

/// Two connections through the same slot must not share an id.
pub fn test_generation_advances_on_release() -> TestResult {
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

/// Every accessor consults the generation. One that did not would hand the
/// previous occupant's caller a reference to the current connection.
pub fn test_stale_id_rejected_by_every_accessor() -> TestResult {
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
/// and `close`/`abort` reach `release` with it. Acting on it would tear down
/// the unrelated connection that took the slot.
pub fn test_stale_release_does_not_evict_live_connection() -> TestResult {
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

/// The rejection is counted, so slot reuse racing a stale holder is visible
/// rather than silently absorbed. A malformed id is a different condition and
/// must not inflate it.
pub fn test_stale_lookup_counter_increments() -> TestResult {
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

/// `clear_all` republishes the indices; advancing rather than resetting the
/// generations is what stops it revalidating ids the cleared connections had
/// already issued.
pub fn test_clear_all_advances_generations() -> TestResult {
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

/// Listener slots recycle on the same terms as established ones.
pub fn test_listener_generation() -> TestResult {
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
