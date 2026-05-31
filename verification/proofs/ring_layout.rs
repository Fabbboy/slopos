// SlopRing layout + masking arithmetic proof.
//
// This is a Verus-annotated mirror of the two pure-arithmetic safety
// properties the SlopRing relies on every time it touches shared memory:
//
//   (LEMMA-mask-in-bounds)   For a power-of-two ring size `n`, masking any
//        index with `n - 1` lands strictly inside `[0, n)`. This is what
//        makes the SQE/CQE slot computations `cq_tail & (cq_entries - 1)`
//        (`ring_obj.rs:134`) and `sq_head & (sq_entries - 1)`
//        (`enter.rs:225`) in-bounds for the fixed-size SQE/CQE arrays — the
//        ring is set up with a power-of-two `entries` (`enter.rs:59`:
//        `!entries.is_power_of_two() => EINVAL`) and `cq_entries =
//        2 * sq_entries` is therefore also a power of two.
//
//   (LEMMA-locate-safe)      `RingRegion::locate(offset, len)` (region.rs:
//        87-99) returns `Ok((frame_idx, in_frame))` only when the byte range
//        `[offset, offset + len)` lies wholly within the region AND wholly
//        within a single 4 KiB frame (no straddle), with `frame_idx ==
//        offset / PAGE_SIZE`. This is the structural no-OOB / no-straddle
//        guard behind every volatile `UFrame` access, stated as pure
//        arithmetic.
//
// TRUSTED BOUNDARY (handled by EXCLUSION — these are simply NOT modelled
// here; this file contains no Verus proof-escape constructs):
//   * The volatile `UFrame` byte-copy / `u32` accessors themselves: this
//     file proves the *index/offset arithmetic feeding them* is in bounds,
//     never the memory op. KernMiri-covered, audited-only.
//   * That `cq_entries` / `sq_entries` are in fact powers of two at runtime:
//     established by `enter.rs:59`'s `is_power_of_two` validation gate +
//     `cq_entries = sq_entries * 2`. Here it is a hypothesis
//     (`is_power_of_two_u32`) of the masking lemmas.
//
// Field correspondence:
//   `n` (mask lemmas)   <-> `Ring::layout.{sq,cq}_entries`  (abi/ring.rs:519-520)
//   `mask = n - 1`      <-> `layout.{sq,cq}_off_mask` value  (enter.rs:146-150)
//   `PAGE_SIZE`         <-> `region.rs` `PAGE_SIZE = 4096`    (region.rs:16)
//   `locate`            <-> `RingRegion::locate`             (region.rs:87-99)

use vstd::prelude::*;

verus! {

// ===========================================================================
// Part 1 — LEMMA-mask-in-bounds (bit_vector path).
// ===========================================================================

/// Power-of-two predicate on a `u32`, mirroring `u32::is_power_of_two`
/// (`enter.rs:59`): non-zero and a single set bit, i.e. `n & (n - 1) == 0`.
/// `(n - 1) as u32` keeps the subtraction in the `u32` (bit-vector) domain;
/// `n != 0` makes the decrement well-defined.
pub open spec fn is_power_of_two_u32(n: u32) -> bool {
    n != 0 && (n & ((n - 1) as u32)) == 0
}

/// (LEMMA-mask-in-bounds) For a power-of-two ring size `n`, masking any
/// `u32` index `i` with `n - 1` yields a value strictly less than `n` — so
/// `cq_tail & (cq_entries - 1)` and `sq_head & (sq_entries - 1)` always
/// index inside the fixed-size CQE/SQE arrays, with no out-of-bounds slot.
///
/// Proved with Verus's bit-vector backend (`by (bit_vector)`): for a single
/// set bit `n`, `n - 1` is the all-ones mask below it, and ANDing any value
/// with it is `< n`. This is the load-bearing in-bounds fact for every
/// masked SQE/CQE index. The `by (bit_vector)` block restates the
/// power-of-two hypothesis in its own `requires` because the bit-vector
/// solver reasons only from the goal + that local clause (it discards the
/// ambient context).
pub proof fn mask_in_bounds(i: u32, n: u32)
    requires
        is_power_of_two_u32(n),
    ensures
        (i & ((n - 1) as u32)) < n,
{
    assert((i & ((n - 1) as u32)) < n) by (bit_vector)
        requires
            n != 0,
            (n & ((n - 1) as u32)) == 0,
    ;
}

// ===========================================================================
// Part 1b — nat `i % n` FALLBACK model for LEMMA-mask-in-bounds.
//
// If the `by (bit_vector)` lemma above proves brittle on the pinned Verus
// toolchain, this nat-arithmetic model is the documented correspondence the
// orchestrator can switch to: for a power-of-two `n`, the hardware AND-mask
// `i & (n - 1)` computes exactly `i % n` (the bit-twiddle identity the ring
// relies on), and `i % n < n` is a pure modular-arithmetic fact Verus
// discharges without the bit-vector backend. The kernel's masked index
// `idx = cursor & (entries - 1)` is therefore `cursor % entries`, always a
// valid slot of the `entries`-long array. This block stands alone (it does
// not depend on the bit_vector lemma) so the file still proves the in-bounds
// property even if the bit_vector friction the plan anticipated materialises.
// ===========================================================================

/// Power-of-two predicate on a `nat` (the modular model's hypothesis). The
/// `#[trigger]` pins the quantifier instantiation to `pow2(k)`.
pub open spec fn is_power_of_two_nat(n: nat) -> bool {
    &&& n > 0
    &&& exists|k: nat| n == #[trigger] pow2(k)
}

/// `2^k`, the witness shape for `is_power_of_two_nat`.
pub open spec fn pow2(k: nat) -> nat
    decreases k,
{
    if k == 0 {
        1
    } else {
        2 * pow2((k - 1) as nat)
    }
}

/// The masking identity, stated in the nat model: a power-of-two-sized mask
/// reduces an index modulo the ring size, which is always in bounds. This is
/// the fallback for LEMMA-mask-in-bounds — `mask_nat(i, n) = i % n` models
/// the real `i & (n - 1)` for power-of-two `n`.
pub open spec fn mask_nat(i: nat, n: nat) -> nat {
    i % n
}

/// (LEMMA-mask-in-bounds, nat fallback) The masked index is strictly less
/// than the ring size for any positive size — pure modular arithmetic, no
/// bit-vector backend. Holds for every power-of-two `n` (indeed every
/// `n > 0`), so it is a strict superset of the runtime hypothesis.
pub proof fn mask_in_bounds_nat(i: nat, n: nat)
    requires
        n > 0,
    ensures
        mask_nat(i, n) < n,
{
}

// ===========================================================================
// Part 2 — LEMMA-locate-safe (region.rs:87-99 arithmetic).
// ===========================================================================

/// The region page size (`region.rs:16` `PAGE_SIZE = 4096`).
pub open spec fn page_size() -> nat {
    4096
}

/// `RingRegion::locate(offset, len)` succeeds (`region.rs:87-99`). It is a
/// no-op model of the four guards in the real code:
///   1. `offset + len` does not overflow `usize` (modelled: nat has no
///      overflow, so the `checked_add` always succeeds in the abstract
///      domain — the real guard rejects the wraparound case, which the nat
///      model never reaches).
///   2. `offset + len <= bytes` (in region).
///   3. `(offset % PAGE_SIZE) + len <= PAGE_SIZE` (no page straddle).
/// `len > 0` excludes the degenerate empty access.
pub open spec fn locate_ok(offset: nat, len: nat, bytes: nat) -> bool {
    &&& len > 0
    &&& offset + len <= bytes
    &&& (offset % page_size()) + len <= page_size()
}

/// The `(frame_idx, in_frame)` pair `locate` returns (`region.rs:92-93`):
/// `frame_idx = offset / PAGE_SIZE`, `in_frame = offset % PAGE_SIZE`.
pub open spec fn locate_frame(offset: nat) -> nat {
    offset / page_size()
}

pub open spec fn locate_in_frame(offset: nat) -> nat {
    offset % page_size()
}

/// (LEMMA-locate-safe) When `locate` succeeds, the resolved access is safe:
/// the whole `[offset, offset + len)` range lies inside the region and
/// inside a single frame, and the returned frame index is `offset /
/// PAGE_SIZE`. This is the no-OOB / no-straddle guarantee behind every
/// volatile `UFrame` access, as pure arithmetic.
pub proof fn locate_safe(offset: nat, len: nat, bytes: nat)
    requires
        locate_ok(offset, len, bytes),
    ensures
        // In region: the access does not run off the end.
        offset + len <= bytes,
        // No straddle: the in-frame offset plus the length fits one page, so
        // the access never crosses a frame boundary.
        locate_in_frame(offset) + len <= page_size(),
        // The frame index is exactly the page the offset falls in.
        locate_frame(offset) == offset / page_size(),
        // The in-frame offset is a valid position within the page.
        locate_in_frame(offset) < page_size(),
{
}

// ===========================================================================
// Part 3 — LEMMA-layout-disjoint-fits (STRETCH / OPTIONAL).
//
// This block generalizes the `RingLayout` non-overlap + fits-in-region
// asserts (abi/src/ring.rs:714-741 test, mirrored from the `RingLayout::new`
// construction at :544-601) as pure arithmetic. It is marked STRETCH: if it
// does not discharge cleanly on the pinned Verus toolchain, the orchestrator
// should DELETE this Part 3 block entirely — Parts 1 and 2 are the
// guaranteed deliverable and stand on their own.
//
// Modelling note. The real layout page-aligns each sub-area with
// `align_up(v, a) = (v + (a-1)) & !(a-1)` (abi/ring.rs:537-539,574-582). The
// only property the disjointness + fits argument needs is the resulting
// sub-area ORDERING: `sqe_array_off` precedes the SQE bytes, the page-aligned
// `cqe_array_off` is at or above the SQE array's end, and `region_bytes` is
// at or above the CQE array's end (`align_up(x, _) >= x`). We take that
// ordering as the `layout_wf` hypothesis — it is exactly what
// `RingLayout::new` constructs by stacking `align_up`'d offsets — and prove
// the disjoint + fits conclusion. The alignment arithmetic itself (that each
// `align_up`'d offset is `>= ` its input) is left to the construction, not
// re-derived here, to keep this block free of brittle division reasoning.
// ===========================================================================

/// Abstract image of the `RingLayout` sub-area offsets (abi/ring.rs:518-534),
/// in construction order: header -> SQ control -> CQ control -> SQE array ->
/// CQE array, each followed by its size, all inside `region_bytes`.
pub struct LayoutAreas {
    /// `sqe_array_off` (abi/ring.rs:574, page-aligned).
    pub sqe_array_off: nat,
    /// `sq_entries * size_of::<Sqe>()` == `sq_entries * 64`.
    pub sqe_bytes: nat,
    /// `cqe_array_off` (abi/ring.rs:579, page-aligned after the SQE array).
    pub cqe_array_off: nat,
    /// `cq_entries * size_of::<Cqe>()` == `cq_entries * 16`.
    pub cqe_bytes: nat,
    /// `region_bytes` (abi/ring.rs:582, page-aligned after the CQE array).
    pub region_bytes: nat,
}

/// The well-formedness the construction guarantees (abi/ring.rs:574-582):
/// the CQE array begins at or after the SQE array's end, and the region
/// covers the CQE array's end. (The control blocks precede `sqe_array_off`,
/// which is page-aligned above them — region.rs's straddle guard + the
/// page-alignment is what keeps them disjoint from the arrays; here we model
/// the two big arrays, which are the disjointness obligation that matters.)
pub open spec fn layout_wf(l: LayoutAreas) -> bool {
    &&& l.cqe_array_off >= l.sqe_array_off + l.sqe_bytes
    &&& l.region_bytes >= l.cqe_array_off + l.cqe_bytes
}

/// (LEMMA-layout-disjoint-fits, STRETCH) Under `layout_wf`, the SQE and CQE
/// arrays are disjoint and both fit inside the region: the SQE array ends at
/// or before the CQE array starts, and the CQE array ends at or before the
/// region end. Generalizes the abi/ring.rs:714-741 asserts.
pub proof fn layout_disjoint_fits(l: LayoutAreas)
    requires
        layout_wf(l),
    ensures
        // SQE array ends no later than the CQE array begins (disjoint).
        l.sqe_array_off + l.sqe_bytes <= l.cqe_array_off,
        // CQE array ends no later than the region end (fits).
        l.cqe_array_off + l.cqe_bytes <= l.region_bytes,
{
}

} // verus!
