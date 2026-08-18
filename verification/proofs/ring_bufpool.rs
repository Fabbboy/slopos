// Verus mirror of the two in-bounds properties the kernel's SlopRing buffer
// selection (`ring/src/buffers.rs`) relies on:
//
//   (INV-FIXED-INDEX-BOUND) `BufferRegistry::resolve_fixed` /
//        `check_out_fixed` accept an `Sqe.buf_index` iff it is a valid slot of
//        the pin list, and the staged length is `len.min(pin.len())`, so the
//        volatile `copy_out`/`copy_in` window never exceeds the selected pin.
//
//   (INV-PBUF-SLOT-BOUND) For a provided buffer ring, the consumer cursor
//        `head` never passes the user-published producer `tail`, and the
//        masked slot byte window `(head % entries) * stride .. + stride`
//        always lies inside the `entries * stride`-byte pinned ring.

use vstd::prelude::*;

verus! {

/// `resolve_fixed(index)` / `check_out_fixed(index)` accept the index iff it
/// is a valid slot of the `count`-long pin list.
pub open spec fn fixed_resolve_ok(index: nat, count: nat) -> bool {
    index < count
}

/// An accepted fixed-buffer index is a valid `KVec<PinnedUserBuffer>` slot —
/// trivial, but it pins the contract the kernel's bounds check enforces.
pub proof fn fixed_index_valid_slot(index: nat, count: nat)
    requires
        fixed_resolve_ok(index, count),
    ensures
        index < count,
{
}

/// The staged transfer length `len.min(pin_len)` of
/// `stage_fixed_out`/`publish_fixed_in`.
pub open spec fn staged_len(len: nat, pin_len: nat) -> nat {
    if len <= pin_len {
        len
    } else {
        pin_len
    }
}

pub proof fn staged_len_within_pin(len: nat, pin_len: nat)
    ensures
        staged_len(len, pin_len) <= pin_len,
{
}

/// The masked slot index: for power-of-two `entries` the kernel's
/// `head & (entries - 1)` is `head % entries`, always below `entries`. Pure
/// modular arithmetic, so it holds for every positive `entries`.
pub proof fn pbuf_slot_index_in_range(head: nat, entries: nat)
    requires
        entries > 0,
    ensures
        (head % entries) < entries,
{
}

/// The slot byte window `idx * stride .. idx * stride + stride` lies inside
/// the `entries * stride`-byte ring region whenever `idx < entries` — the
/// in-region guarantee behind `peek`'s `copy_out`, and its nonlinear step.
pub proof fn pbuf_slot_offset_in_region(idx: nat, entries: nat, stride: nat)
    requires
        idx < entries,
        stride > 0,
    ensures
        idx * stride + stride <= entries * stride,
{
    assert(idx * stride + stride == (idx + 1) * stride) by (nonlinear_arith);
    assert((idx + 1) * stride <= entries * stride) by (nonlinear_arith)
        requires
            idx + 1 <= entries,
    ;
}

pub struct PbufState {
    /// Kernel-owned consumer cursor (`ProvidedBufRing::head`).
    pub head: nat,
    /// User-owned producer cursor (read via `read_tail`); an
    /// adversarial-monotone input to the model.
    pub tail: nat,
    /// Ring slot count (power of two at runtime; here only `entries > 0`).
    pub entries: nat,
}

pub open spec fn pbuf_inv(s: PbufState) -> bool {
    &&& s.entries > 0
    &&& s.head <= s.tail
    &&& s.tail - s.head <= s.entries
}

pub enum PbufStep {
    /// `commit()`: consume one peeked buffer. A no-op when the ring is empty
    /// (`head == tail`), matching `peek` returning `None` before any commit.
    Commit,
    /// The user publishes `by` buffers; occupancy is clamped to the ring size
    /// (a real producer cannot overwrite un-consumed slots).
    UserPublish { by: nat },
}

pub open spec fn pbuf_step(s: PbufState, t: PbufStep) -> PbufState {
    match t {
        PbufStep::Commit => if s.head < s.tail {
            PbufState { head: (s.head + 1) as nat, ..s }
        } else {
            s
        },
        PbufStep::UserPublish { by } => {
            let room = (s.entries - (s.tail - s.head)) as nat;
            let adv = if by <= room {
                by
            } else {
                room
            };
            PbufState { tail: (s.tail + adv) as nat, ..s }
        },
    }
}

pub proof fn pbuf_step_preserves(s: PbufState, t: PbufStep)
    requires
        pbuf_inv(s),
    ensures
        pbuf_inv(pbuf_step(s, t)),
{
}

pub open spec fn pbuf_init(s: PbufState) -> bool {
    &&& s.head == 0
    &&& s.tail == 0
    &&& s.entries > 0
}

pub proof fn pbuf_init_inv(s: PbufState)
    requires
        pbuf_init(s),
    ensures
        pbuf_inv(s),
{
}

pub open spec fn pbuf_run(s: PbufState, trace: Seq<PbufStep>) -> PbufState
    decreases trace.len(),
{
    if trace.len() == 0 {
        s
    } else {
        pbuf_step(pbuf_run(s, trace.drop_last()), trace.last())
    }
}

/// Every reachable provided-ring state satisfies the invariant.
pub proof fn pbuf_inv_on_every_trace(s0: PbufState, trace: Seq<PbufStep>)
    requires
        pbuf_init(s0),
    ensures
        pbuf_inv(pbuf_run(s0, trace)),
    decreases trace.len(),
{
    if trace.len() == 0 {
        pbuf_init_inv(s0);
    } else {
        pbuf_inv_on_every_trace(s0, trace.drop_last());
        pbuf_step_preserves(pbuf_run(s0, trace.drop_last()), trace.last());
    }
}

/// (INV-PBUF-SLOT-BOUND) In every reachable state the consumer never passes
/// the producer and the masked slot byte window lies inside the pinned ring,
/// so a `buf_group`-selected provided buffer index is always valid.
pub proof fn pbuf_selected_slot_in_bounds(s0: PbufState, trace: Seq<PbufStep>, stride: nat)
    requires
        pbuf_init(s0),
        stride > 0,
    ensures
        pbuf_run(s0, trace).head <= pbuf_run(s0, trace).tail,
        (pbuf_run(s0, trace).head % pbuf_run(s0, trace).entries) < pbuf_run(s0, trace).entries,
        (pbuf_run(s0, trace).head % pbuf_run(s0, trace).entries) * stride + stride
            <= pbuf_run(s0, trace).entries * stride,
{
    let s = pbuf_run(s0, trace);
    pbuf_inv_on_every_trace(s0, trace);
    pbuf_slot_index_in_range(s.head, s.entries);
    pbuf_slot_offset_in_region(s.head % s.entries, s.entries, stride);
}

} // verus!
