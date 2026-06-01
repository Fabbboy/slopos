// SlopRing registered / provided buffer-selection bounds proof.
//
// Machine-checks the two in-bounds properties the kernel's buffer selection
// (`ring/src/buffers.rs`) relies on — the obligation the buffer-rings task
// names ("a buf_group-selected buffer index stays in bounds"):
//
//   (INV-FIXED-INDEX-BOUND) A registered fixed buffer is selected by
//        `Sqe.buf_index`. `BufferRegistry::resolve_fixed` /
//        `check_out_fixed` accept the index iff `buf_index < buf_count`
//        (`pins.get(index)` / the explicit `i >= set.pins.len()` guard), and
//        the staged length is `len.min(pin.len())`, so the volatile
//        `copy_out`/`copy_in` window never exceeds the selected pin. Modelled
//        as pure facts: an accepted index is a valid `KVec` slot, and the
//        staged length is within the pin.
//
//   (INV-PBUF-SLOT-BOUND) A provided buffer ring is consumed by
//        `ProvidedBufRing::{peek, commit}`. The consumer cursor `head` never
//        passes the user-published producer `tail`, and the masked slot byte
//        window `(head % entries) * stride .. + stride` always lies inside the
//        `entries * stride`-byte pinned ring — so `peek`'s
//        `copy_out((head & mask) * 16, [u8; 16])` is always in range. Mirrors
//        ring_cursor.rs's adversarial-monotone user-cursor model and
//        ring_layout.rs's masked-index lemma.

use vstd::prelude::*;

verus! {

// ===========================================================================
// (INV-FIXED-INDEX-BOUND) — registered fixed buffers (separate pins).
// ===========================================================================

/// `resolve_fixed(index)` / `check_out_fixed(index)` accept the index iff it
/// is a valid slot of the `count`-long pin list (`pins.get(index)` is `Some`,
/// or `i < pins.len()`).
pub open spec fn fixed_resolve_ok(index: nat, count: nat) -> bool {
    index < count
}

/// An accepted fixed-buffer index is a valid `KVec<PinnedUserBuffer>` slot.
/// (Trivial, but it pins the contract the kernel's bounds check enforces.)
pub proof fn fixed_index_valid_slot(index: nat, count: nat)
    requires
        fixed_resolve_ok(index, count),
    ensures
        index < count,
{
}

/// The staged transfer length is `len.min(pin_len)` (see
/// `stage_fixed_out`/`publish_fixed_in`), which is always within the selected
/// pin — so the volatile copy window never runs off the buffer.
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

// ===========================================================================
// (INV-PBUF-SLOT-BOUND, part a) — masked slot index + slot byte offset.
// ===========================================================================

/// The masked slot index `head & (entries - 1)` for a power-of-two `entries`
/// equals `head % entries` (the bit-twiddle identity ring_layout.rs proves),
/// always strictly less than `entries`. Pure modular arithmetic — holds for
/// every positive `entries`.
pub proof fn pbuf_slot_index_in_range(head: nat, entries: nat)
    requires
        entries > 0,
    ensures
        (head % entries) < entries,
{
}

/// The slot byte window `idx * stride .. idx * stride + stride` lies inside the
/// `entries * stride`-byte ring region whenever `idx < entries`. This is the
/// `copy_out((head & mask) * size_of::<IouringBuf>(), [u8; 16])` in-region
/// guarantee — the nonlinear step the bound rests on.
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

// ===========================================================================
// (INV-PBUF-SLOT-BOUND, part b) — head/tail cursor state machine.
//
// `ProvidedBufRing` keeps a kernel-owned consumer `head`; the user publishes
// buffers by advancing the producer `tail`. The kernel only ever advances
// `head` (on commit) when `head != tail`, and never past `tail` — so a peeked
// slot is one the user actually published. The user-published `tail` is an
// adversarial-monotone input clamped to the ring size (it cannot overwrite
// un-consumed slots), exactly as ring_cursor.rs models `cq_head`/`sq_tail`.
// ===========================================================================

pub struct PbufState {
    /// Kernel-owned consumer cursor (`ProvidedBufRing::head`).
    pub head: nat,
    /// User-owned producer cursor (read via `read_tail`).
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

/// Every reachable provided-ring state satisfies the invariant — the inductive
/// whole-execution guarantee (mirrors ring_cursor.rs's main theorem).
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

/// (INV-PBUF-SLOT-BOUND) The headline corollary: in every reachable state the
/// consumer never passes the producer, and the masked slot byte window lies
/// inside the pinned ring — so a `buf_group`-selected (peeked) provided buffer
/// index is always valid.
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
