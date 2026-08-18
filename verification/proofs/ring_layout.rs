// Verus mirror of the two pure-arithmetic safety properties the SlopRing
// relies on every time it touches shared memory:
//
//   (LEMMA-mask-in-bounds)   For a power-of-two ring size `n`, masking any
//        index with `n - 1` lands strictly inside `[0, n)`, which is what
//        keeps every `cursor & (entries - 1)` SQE/CQE slot index inside its
//        fixed-size array. That `entries` is a power of two is a hypothesis
//        here, established at runtime by `enter.rs`'s `is_power_of_two` gate.
//
//   (LEMMA-locate-safe)      `RingRegion::locate(offset, len)` returns
//        `Ok((frame_idx, in_frame))` only when `[offset, offset + len)` lies
//        wholly within the region AND wholly within a single 4 KiB frame,
//        with `frame_idx == offset / PAGE_SIZE`.
//
// The volatile `UFrame` accessors these feed are not modelled here: this file
// proves the index/offset arithmetic is in bounds, never the memory op.

use vstd::prelude::*;

verus! {

/// Power-of-two predicate mirroring `u32::is_power_of_two`. The `as u32`
/// keeps the subtraction in the bit-vector domain.
pub open spec fn is_power_of_two_u32(n: u32) -> bool {
    n != 0 && (n & ((n - 1) as u32)) == 0
}

/// (LEMMA-mask-in-bounds) For a power-of-two ring size `n`, `i & (n - 1)` is
/// strictly less than `n`, so every masked SQE/CQE index is in bounds.
///
/// The `by (bit_vector)` block restates the power-of-two hypothesis in its own
/// `requires`: that solver reasons only from the goal plus its local clauses
/// and discards the ambient context.
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

/// Power-of-two predicate on a `nat`; the `#[trigger]` pins quantifier
/// instantiation to `pow2(k)`.
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

/// The masking identity in the nat model: for power-of-two `n` the hardware
/// `i & (n - 1)` computes exactly `i % n`. Stands alone from the bit-vector
/// lemma above, which it duplicates as a fallback.
pub open spec fn mask_nat(i: nat, n: nat) -> nat {
    i % n
}

/// (LEMMA-mask-in-bounds, nat fallback) Pure modular arithmetic, no
/// bit-vector backend. Holds for every `n > 0`, a superset of the runtime
/// power-of-two hypothesis.
pub proof fn mask_in_bounds_nat(i: nat, n: nat)
    requires
        n > 0,
    ensures
        mask_nat(i, n) < n,
{
}

/// The region page size (`region.rs` `PAGE_SIZE = 4096`).
pub open spec fn page_size() -> nat {
    4096
}

/// `RingRegion::locate(offset, len)` succeeding: non-empty, in-region, no
/// page straddle. The real code's `checked_add` overflow guard has no image
/// here — a `nat` never wraps, so the model never reaches that case.
pub open spec fn locate_ok(offset: nat, len: nat, bytes: nat) -> bool {
    &&& len > 0
    &&& offset + len <= bytes
    &&& (offset % page_size()) + len <= page_size()
}

/// The `(frame_idx, in_frame)` pair `locate` returns: `offset / PAGE_SIZE`
/// and `offset % PAGE_SIZE`.
pub open spec fn locate_frame(offset: nat) -> nat {
    offset / page_size()
}

pub open spec fn locate_in_frame(offset: nat) -> nat {
    offset % page_size()
}

/// (LEMMA-locate-safe) The no-OOB / no-straddle guarantee behind every
/// volatile `UFrame` access: when `locate` succeeds, `[offset, offset + len)`
/// lies inside the region and inside a single frame.
pub proof fn locate_safe(offset: nat, len: nat, bytes: nat)
    requires
        locate_ok(offset, len, bytes),
    ensures
        offset + len <= bytes,
        locate_in_frame(offset) + len <= page_size(),
        locate_frame(offset) == offset / page_size(),
        locate_in_frame(offset) < page_size(),
{
}

/// Abstract image of the `RingLayout` sub-area offsets, in construction
/// order: header -> SQ control -> CQ control -> SQE array -> CQE array, each
/// followed by its size, all inside `region_bytes`.
pub struct LayoutAreas {
    /// `sqe_array_off`, page-aligned.
    pub sqe_array_off: nat,
    /// `sq_entries * size_of::<Sqe>()` == `sq_entries * 64`.
    pub sqe_bytes: nat,
    /// `cqe_array_off`, page-aligned after the SQE array.
    pub cqe_array_off: nat,
    /// `cq_entries * size_of::<Cqe>()` == `cq_entries * 16`.
    pub cqe_bytes: nat,
    /// `region_bytes`, page-aligned after the CQE array.
    pub region_bytes: nat,
}

/// The ordering `RingLayout::new` constructs by stacking `align_up`'d offsets:
/// the CQE array begins at or after the SQE array's end and the region covers
/// the CQE array's end. Taken as hypothesis — the alignment arithmetic is left
/// to the construction rather than re-derived here. Only the two big arrays
/// are modelled; the control blocks sit below the page-aligned
/// `sqe_array_off`.
pub open spec fn layout_wf(l: LayoutAreas) -> bool {
    &&& l.cqe_array_off >= l.sqe_array_off + l.sqe_bytes
    &&& l.region_bytes >= l.cqe_array_off + l.cqe_bytes
}

/// (LEMMA-layout-disjoint-fits) Under `layout_wf`, the SQE and CQE arrays are
/// disjoint and both fit inside the region.
pub proof fn layout_disjoint_fits(l: LayoutAreas)
    requires
        layout_wf(l),
    ensures
        l.sqe_array_off + l.sqe_bytes <= l.cqe_array_off,
        l.cqe_array_off + l.cqe_bytes <= l.region_bytes,
{
}

} // verus!
