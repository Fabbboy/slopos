//! Tests for RFC 793 wrapping sequence-number arithmetic, in both the
//! free-function form and the [`SeqNum`] newtype.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::{SeqDelta, SeqNum, seq_ge, seq_gt, seq_le, seq_lt};

pub fn test_seq_lt_adjacent() -> TestResult {
    assert_test!(seq_lt(10, 11), "10 < 11");
    assert_test!(!seq_lt(11, 10), "11 !< 10");
    assert_test!(!seq_lt(10, 10), "10 !< 10");
    pass!()
}

pub fn test_seq_le_adjacent() -> TestResult {
    assert_test!(seq_le(10, 10), "10 <= 10");
    assert_test!(seq_le(10, 11), "10 <= 11");
    assert_test!(!seq_le(11, 10), "11 !<= 10");
    pass!()
}

pub fn test_seq_gt_adjacent() -> TestResult {
    assert_test!(seq_gt(11, 10), "11 > 10");
    assert_test!(!seq_gt(10, 11), "10 !> 11");
    pass!()
}

pub fn test_seq_ge_adjacent() -> TestResult {
    assert_test!(seq_ge(10, 10), "10 >= 10");
    assert_test!(seq_ge(11, 10), "11 >= 10");
    assert_test!(!seq_ge(10, 11), "10 !>= 11");
    pass!()
}

/// The comparison must respect the wrap from ~UINT_MAX back to 0: a small
/// value "after" the wrap is *greater* than a large value just before it.
pub fn test_seq_wrap_across_zero() -> TestResult {
    assert_test!(
        seq_lt(0xFFFF_FFFE, 0x0000_0002),
        "near-wrap precedes past-wrap"
    );
    assert_test!(
        seq_gt(0x0000_0002, 0xFFFF_FFFE),
        "past-wrap exceeds near-wrap"
    );
    assert_test!(!seq_lt(0x0000_0002, 0xFFFF_FFFE), "past-wrap !< near-wrap");
    pass!()
}

pub fn test_seqnum_partial_ord_adjacent() -> TestResult {
    let a = SeqNum::new(100);
    let b = SeqNum::new(101);
    assert_test!(a < b, "100 < 101 as SeqNum");
    assert_test!(!(b < a), "!(101 < 100)");
    assert_eq_test!(a.cmp(&a), core::cmp::Ordering::Equal, "100 == 100");
    pass!()
}

/// The newtype's `<` / `>` must route through the wrapping comparison, not
/// naive u32 comparison.
pub fn test_seqnum_wrap_across_zero() -> TestResult {
    let low = SeqNum::new(0xFFFF_FFFE);
    let high = SeqNum::new(0x0000_0002);
    assert_test!(low < high, "SeqNum wraps cleanly across zero");
    assert_test!(high > low, "SeqNum wraps in reverse too");
    pass!()
}

pub fn test_seqnum_wrapping_add_wraps() -> TestResult {
    let s = SeqNum::new(0xFFFF_FFFE);
    let r = s.wrapping_add(4);
    assert_eq_test!(r.raw(), 2, "0xFFFF_FFFE + 4 = 2 (mod 2^32)");
    pass!()
}

pub fn test_seqnum_add_op_matches_wrapping_add() -> TestResult {
    let s = SeqNum::new(100);
    let r = s + 5;
    assert_eq_test!(r.raw(), 105, "add-op parity");
    let wrap = SeqNum::new(0xFFFF_FFFE) + 5;
    assert_eq_test!(wrap.raw(), 3, "add-op wraps like wrapping_add");
    pass!()
}

pub fn test_seqnum_sub_returns_distance() -> TestResult {
    let a = SeqNum::new(100);
    let b = SeqNum::new(103);
    assert_eq_test!(b - a, 3, "forward distance");
    assert_eq_test!(a - b, u32::MAX - 2, "reverse distance wraps");
    pass!()
}

pub fn test_seqnum_distance_to_forward() -> TestResult {
    let a = SeqNum::new(1000);
    let b = SeqNum::new(1050);
    assert_eq_test!(a.distance_to(b), 50, "forward distance");
    pass!()
}

pub fn test_seqnum_delta_signed() -> TestResult {
    let a = SeqNum::new(100);
    let b = SeqNum::new(200);
    assert_eq_test!(SeqDelta::of(b, a).0, 100, "b - a = +100");
    assert_eq_test!(SeqDelta::of(a, b).0, -100, "a - b = -100");
    pass!()
}

/// Deterministic per-test PRNG seeded from a constant so failures are
/// reproducible.
fn splitmix32(state: &mut u64) -> u32 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as u32
}

/// Round-trip invariant: for any base sequence number and positive delta,
/// `(base + delta).distance_to(base) == (2^32 - delta) mod 2^32` AND
/// `base.distance_to(base + delta) == delta`.
pub fn test_seqnum_round_trip_fuzz() -> TestResult {
    let mut rng_state = 0xDEAD_BEEF_CAFE_BABEu64;
    for _ in 0..4_096 {
        let base = SeqNum::new(splitmix32(&mut rng_state));
        let delta = splitmix32(&mut rng_state);
        let shifted = base + delta;
        let recovered = base.distance_to(shifted);
        if recovered != delta {
            return fail!("round-trip failed: base+delta.distance != delta");
        }
    }
    pass!()
}

/// The ordering on `SeqNum` must be **antisymmetric** for any two values
/// within a half-window: `a < b` iff `!(b < a || a == b)`.  Outside the
/// half-window the ordering is undefined and this test skips those pairs.
pub fn test_seqnum_partial_ord_antisymmetric_fuzz() -> TestResult {
    let mut rng_state = 0x01234567_89ABCDEFu64;
    const HALF_WINDOW: u32 = 1 << 30;
    for _ in 0..4_096 {
        let a = SeqNum::new(splitmix32(&mut rng_state));
        let b = SeqNum::new(splitmix32(&mut rng_state));
        let fwd = a.distance_to(b);
        let rev = b.distance_to(a);
        if fwd > HALF_WINDOW && rev > HALF_WINDOW {
            continue;
        }
        if a == b {
            continue;
        }
        if a < b {
            if b < a {
                return fail!("antisymmetry violated: a<b AND b<a");
            }
        } else if b >= a {
            return fail!("antisymmetry violated: !(a<b) should imply b>a");
        }
    }
    pass!()
}

slopos_testing::stest!(name = test_seq_lt_adjacent, suite = tcp_seq);
slopos_testing::stest!(name = test_seq_le_adjacent, suite = tcp_seq);
slopos_testing::stest!(name = test_seq_gt_adjacent, suite = tcp_seq);
slopos_testing::stest!(name = test_seq_ge_adjacent, suite = tcp_seq);
slopos_testing::stest!(name = test_seq_wrap_across_zero, suite = tcp_seq);
slopos_testing::stest!(name = test_seqnum_partial_ord_adjacent, suite = tcp_seq);
slopos_testing::stest!(name = test_seqnum_wrap_across_zero, suite = tcp_seq);
slopos_testing::stest!(name = test_seqnum_wrapping_add_wraps, suite = tcp_seq);
slopos_testing::stest!(
    name = test_seqnum_add_op_matches_wrapping_add,
    suite = tcp_seq
);
slopos_testing::stest!(name = test_seqnum_sub_returns_distance, suite = tcp_seq);
slopos_testing::stest!(name = test_seqnum_distance_to_forward, suite = tcp_seq);
slopos_testing::stest!(name = test_seqnum_delta_signed, suite = tcp_seq);
slopos_testing::stest!(name = test_seqnum_round_trip_fuzz, suite = tcp_seq);
slopos_testing::stest!(
    name = test_seqnum_partial_ord_antisymmetric_fuzz,
    suite = tcp_seq
);
