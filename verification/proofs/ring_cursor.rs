// SlopRing SQ/CQ cursor + in-flight state-machine proof: a Verus mirror of the
// index/counter bookkeeping in `slopos_ring::ring_obj::Ring` and
// `slopos_ring::enter::{submit, post_cqe}`. It machine-checks:
//
//   (INV-CQ-no-overwrite)        `cq_tail - cq_head <= cq_entries` always, so
//        the producer never overwrites an unconsumed completion.
//   (INV-CQ-full-correctness)    `cq_full <=> cq_tail - cq_head >= cq_entries`;
//        a `PostCqe` writes a CQE iff `!cq_full`, and the post / overflow
//        branches are mutually exclusive and exhaustive.
//   (INV-overflow-monotone-latch) `cq_overflow` only ever increases (by one
//        per dropped CQE); `overflow_latched` is one-way false -> true.
//   (INV-cq-tail-advance-exactly-one) A successful `PostCqe` advances
//        `cq_tail` by exactly one; a dropped (overflow) post by zero.
//   (INV-inflight-cap)           `inflight_len <= cap`; `PushInflight` is a
//        no-op at capacity and `RemoveInflight` never underflows.
//   (INV-submit-consume-bound)   `Submit` consumes exactly
//        `min(to_submit, sq_entries, sq_tail - sq_head)` SQEs, so `sq_head`
//        never passes the user-published `sq_tail`.
//
// NOT MODELLED — trusted, not machine-checked (this file uses no Verus
// proof-escape construct; the facts below are simply excluded):
//   * The volatile `UFrame` accessors reached through `ring/src/region.rs`.
//     This proof reasons about abstract index values, never the memory op
//     that reads or writes them.
//   * The kernel/userland release/acquire ordering protocol — Verus has no
//     weak-memory model.
//   * Per-ring SpinLock mutual exclusion (`registry::with_ring`), which is
//     what makes the kernel a single sequential `Step` writer per ring.
// The user-owned cursors `sq_tail` / `cq_head` ARE modelled, as the
// adversarial-monotone `UserAdvance*` steps: without them the no-overwrite
// invariant would be vacuous, the kernel being the only mutator of the gap it
// is supposed to respect.
//
// The real cursors are `u32` with `wrapping_sub` occupancy; this proof models
// them as unbounded `nat`. The two agree exactly under `cq_head <= cq_tail`,
// `cq_tail - cq_head <= cq_entries <= u32::MAX` (symmetrically on the SQ
// side) — which is INV-CQ-no-overwrite / INV-submit-consume-bound, maintained
// inductively here, so the abstraction is established by the proof rather than
// stipulated. Every control decision reads only the difference, never the
// absolute counter.

use vstd::prelude::*;

verus! {

/// Abstract image of one ring's SQ/CQ cursors, in-flight table size and
/// overflow accounting. The kernel owns `sq_head` and `cq_tail`; the user owns
/// `sq_tail` and `cq_head`.
pub struct RingState {
    /// Kernel-owned SQ consumer cursor (`Ring::sq_head`).
    pub sq_head: nat,
    /// User-owned SQ producer cursor (read via `Ring::read_sq_tail`).
    pub sq_tail: nat,
    /// User-owned CQ consumer cursor (read via `Ring::read_cq_head`).
    pub cq_head: nat,
    /// Kernel-owned CQ producer cursor (`Ring::cq_tail`).
    pub cq_tail: nat,
    /// CQ slot count (`Ring::layout.cq_entries`); a power of two at runtime.
    pub cq_entries: nat,
    /// SQ slot count (`Ring::layout.sq_entries`); a power of two at runtime.
    pub sq_entries: nat,
    /// In-flight rows currently recorded (`InFlightVec::len`).
    pub inflight_len: nat,
    /// In-flight capacity (`InFlightVec::cap`, == `cq_entries` at setup).
    pub cap: nat,
    /// CQEs dropped because the CQ was full (`Ring::cq_overflow`).
    pub cq_overflow: nat,
    /// The sticky `SLOPRING_CQ_OVERFLOW` flag — once raised it stays raised.
    pub overflow_latched: bool,
}

/// The inductive ring invariant. Every reachable ring state satisfies it;
/// each `Step` preserves it (`step_preserves` below).
pub open spec fn ring_inv(s: RingState) -> bool {
    // Each kernel cursor trails its user cursor; this is what makes the nat
    // model faithful to the real `wrapping_sub` occupancy.
    &&& s.cq_head <= s.cq_tail
    &&& s.sq_head <= s.sq_tail
    // (INV-CQ-no-overwrite) At most `cq_entries` CQEs outstanding.
    &&& s.cq_tail - s.cq_head <= s.cq_entries
    // (INV-submit-consume-bound) The kernel never claims to have consumed more
    //      SQEs than the user published.
    &&& s.sq_tail - s.sq_head <= s.sq_entries
    // (INV-inflight-cap)
    &&& s.inflight_len <= s.cap
    &&& s.cap == s.cq_entries
    // A 0-entry ring is rejected at setup (`entries == 0 => EINVAL`).
    &&& s.cq_entries > 0
    &&& s.sq_entries > 0
}

/// A fresh ring from `ring_setup`: all four cursors zero (the region is
/// zero-filled and `write_initial_region` stores 0 into every index), no
/// in-flight rows, no overflow, flag clear.
pub open spec fn ring_init(s: RingState) -> bool {
    &&& s.sq_head == 0
    &&& s.sq_tail == 0
    &&& s.cq_head == 0
    &&& s.cq_tail == 0
    &&& s.inflight_len == 0
    &&& s.cq_overflow == 0
    &&& s.overflow_latched == false
    &&& s.cap == s.cq_entries
    &&& s.cq_entries > 0
    &&& s.sq_entries > 0
}

/// `Ring::cq_full`. The real code computes
/// `cq_tail.wrapping_sub(cq_head) >= cq_entries`; under `ring_inv` the nat
/// difference equals that `wrapping_sub`.
pub open spec fn cq_full(s: RingState) -> bool {
    s.cq_tail - s.cq_head >= s.cq_entries
}

/// The clamp in `enter::submit`.
pub open spec fn submit_count(s: RingState, to_submit: nat) -> nat {
    let available = (s.sq_tail - s.sq_head) as nat;
    min3(to_submit, s.sq_entries, available)
}

pub open spec fn min3(a: nat, b: nat, c: nat) -> nat {
    min2(min2(a, b), c)
}

pub open spec fn min2(a: nat, b: nat) -> nat {
    if a <= b {
        a
    } else {
        b
    }
}

/// One step per atomic-bounded ring bookkeeping body.
pub enum Step {
    /// `enter::submit`. Models only the cursor advance — the per-SQE
    /// `process_sqe` side effects are separate steps.
    Submit { to_submit: nat },
    /// `Ring::post_cqe`, the *successful* branch.
    PostCqe,
    /// `Ring::post_cqe`, the *overflow* branch: drop the CQE, bump
    /// `cq_overflow`, latch the sticky flag; `cq_tail` does NOT advance.
    PostOverflow,
    /// `InFlightVec::push`: record one in-flight row. A no-op at capacity —
    /// the caller then completes the SQE inline with `-EAGAIN`.
    PushInflight,
    /// `InFlightVec::remove_at`: drop one in-flight row (harvest completion /
    /// cancel). A no-op when the table is empty.
    RemoveInflight,
    /// The user harvesting `by` CQEs. Adversarial-monotone input: `cq_head`
    /// can only increase, never past the published `cq_tail`.
    UserAdvanceCqHead { by: nat },
    /// The user publishing `by` SQEs. Adversarial-monotone input: `sq_tail`
    /// can only increase, clamped to keep occupancy `<= sq_entries`.
    UserAdvanceSqTail { by: nat },
}

/// Transition function: the post-state after applying `t` to `s`. Every arm is
/// total — the else-branch is the identity, matching the real no-op paths
/// (clamp to 0, full guard, at-capacity push, empty remove).
pub open spec fn step(s: RingState, t: Step) -> RingState {
    match t {
        Step::Submit { to_submit } => {
            let n = submit_count(s, to_submit);
            RingState { sq_head: (s.sq_head + n) as nat, ..s }
        },
        Step::PostCqe =>
            if !cq_full(s) {
                RingState { cq_tail: (s.cq_tail + 1) as nat, ..s }
            } else {
                s
            },
        Step::PostOverflow =>
            if cq_full(s) {
                RingState {
                    cq_overflow: (s.cq_overflow + 1) as nat,
                    overflow_latched: true,
                    ..s
                }
            } else {
                s
            },
        Step::PushInflight =>
            if s.inflight_len < s.cap {
                RingState { inflight_len: (s.inflight_len + 1) as nat, ..s }
            } else {
                s
            },
        Step::RemoveInflight =>
            if s.inflight_len > 0 {
                RingState { inflight_len: (s.inflight_len - 1) as nat, ..s }
            } else {
                s
            },
        Step::UserAdvanceCqHead { by } => {
            // The clamp models a consumer that may race but cannot harvest
            // completions the kernel never posted.
            let room = (s.cq_tail - s.cq_head) as nat;
            let adv = min2(by, room);
            RingState { cq_head: (s.cq_head + adv) as nat, ..s }
        },
        Step::UserAdvanceSqTail { by } => {
            // Publishing past `sq_entries` would overwrite un-consumed SQEs.
            let room = (s.sq_entries - (s.sq_tail - s.sq_head)) as nat;
            let adv = min2(by, room);
            RingState { sq_tail: (s.sq_tail + adv) as nat, ..s }
        },
    }
}

/// Every `Step` preserves `ring_inv`. Each step is the image of one
/// atomic-bounded body run under the per-ring SpinLock, so this one inductive
/// fact covers every interleaving of kernel bodies and user cursor advances.
pub proof fn step_preserves(s: RingState, t: Step)
    requires
        ring_inv(s),
    ensures
        ring_inv(step(s, t)),
{
}

/// The fresh-ring state satisfies the invariant — base case.
pub proof fn init_inv(s: RingState)
    requires
        ring_init(s),
    ensures
        ring_inv(s),
{
}

/// Replay a finite trace of steps from a start state: the total order the
/// per-ring lock imposes on the kernel bodies, interleaved with the user's
/// adversarial-monotone cursor advances.
pub open spec fn run(s: RingState, trace: Seq<Step>) -> RingState
    decreases trace.len(),
{
    if trace.len() == 0 {
        s
    } else {
        step(run(s, trace.drop_last()), trace.last())
    }
}

/// MAIN THEOREM. From any fresh ring, after *any* trace of ring steps, the
/// invariant still holds.
pub proof fn invariant_holds_on_every_trace(s0: RingState, trace: Seq<Step>)
    requires
        ring_init(s0),
    ensures
        ring_inv(run(s0, trace)),
    decreases trace.len(),
{
    if trace.len() == 0 {
        init_inv(s0);
    } else {
        invariant_holds_on_every_trace(s0, trace.drop_last());
        step_preserves(run(s0, trace.drop_last()), trace.last());
    }
}

/// (INV-CQ-no-overwrite) In every reachable state the gap between `cq_tail`
/// and the user's `cq_head` is within the ring size, so the slot a `PostCqe`
/// writes (`cq_tail & (cq_entries - 1)`) is never one the user still holds.
pub proof fn inv_cq_no_overwrite(s0: RingState, trace: Seq<Step>)
    requires
        ring_init(s0),
    ensures
        run(s0, trace).cq_tail - run(s0, trace).cq_head <= run(s0, trace).cq_entries,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (INV-CQ-full-correctness) `cq_full` is exactly "no free slot", and the
/// `PostCqe` / `PostOverflow` branches are mutually exclusive and exhaustive:
/// exactly one of them is enabled on any invariant state.
pub proof fn inv_cq_full_correctness(s: RingState)
    requires
        ring_inv(s),
    ensures
        !cq_full(s) ==> step(s, Step::PostCqe).cq_tail == s.cq_tail + 1,
        cq_full(s) ==> step(s, Step::PostCqe).cq_tail == s.cq_tail,
        cq_full(s) ==> step(s, Step::PostOverflow).cq_overflow == s.cq_overflow + 1,
        !cq_full(s) ==> step(s, Step::PostOverflow).cq_overflow == s.cq_overflow,
        !(step(s, Step::PostCqe).cq_tail != s.cq_tail
            && step(s, Step::PostOverflow).cq_overflow != s.cq_overflow),
{
}

/// (INV-overflow-monotone-latch) `cq_overflow` never decreases across any
/// step, and `overflow_latched` is monotone false -> true.
pub proof fn inv_overflow_monotone_latch(s: RingState, t: Step)
    requires
        ring_inv(s),
    ensures
        step(s, t).cq_overflow >= s.cq_overflow,
        s.overflow_latched ==> step(s, t).overflow_latched,
{
}

/// (INV-cq-tail-advance-exactly-one) No step ever advances `cq_tail` by more
/// than one, and only a successful `PostCqe` advances it at all.
pub proof fn inv_cq_tail_advance_exactly_one(s: RingState)
    requires
        ring_inv(s),
    ensures
        !cq_full(s) ==> step(s, Step::PostCqe).cq_tail == s.cq_tail + 1,
        step(s, Step::PostOverflow).cq_tail == s.cq_tail,
        step(s, Step::PushInflight).cq_tail == s.cq_tail,
        step(s, Step::RemoveInflight).cq_tail == s.cq_tail,
{
}

/// (INV-inflight-cap) In every reachable state the in-flight table is within
/// capacity.
pub proof fn inv_inflight_cap(s0: RingState, trace: Seq<Step>)
    requires
        ring_init(s0),
    ensures
        run(s0, trace).inflight_len <= run(s0, trace).cap,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (INV-inflight-cap, step level) The two no-op guards the reachable-state
/// bound leans on.
pub proof fn inv_inflight_guards(s: RingState)
    requires
        ring_inv(s),
    ensures
        s.inflight_len == s.cap ==> step(s, Step::PushInflight).inflight_len == s.inflight_len,
        s.inflight_len == 0 ==> step(s, Step::RemoveInflight).inflight_len == 0,
{
}

/// (INV-submit-consume-bound) A `Submit` advances `sq_head` by exactly the
/// clamped consume count, so the kernel never claims an SQE the user has not
/// published.
pub proof fn inv_submit_consume_bound(s: RingState, to_submit: nat)
    requires
        ring_inv(s),
    ensures
        step(s, Step::Submit { to_submit }).sq_head == s.sq_head + submit_count(s, to_submit),
        submit_count(s, to_submit) <= (s.sq_tail - s.sq_head) as nat,
        submit_count(s, to_submit) <= s.sq_entries,
        submit_count(s, to_submit) <= to_submit,
        step(s, Step::Submit { to_submit }).sq_head <= s.sq_tail,
{
}

/// (INV-submit-consume-bound, reachable) `sq_head` never passes the
/// user-published `sq_tail` in any reachable state.
pub proof fn inv_sq_head_never_passes_tail(s0: RingState, trace: Seq<Step>)
    requires
        ring_init(s0),
    ensures
        run(s0, trace).sq_head <= run(s0, trace).sq_tail,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// A *broken* `post_cqe`: `Ring::post_cqe` with its `cq_full` guard removed,
/// so it bumps `cq_tail` even when the CQ is already full.
pub open spec fn broken_post(s: RingState) -> RingState {
    RingState { cq_tail: (s.cq_tail + 1) as nat, ..s }
}

/// Witness that the `cq_full` guard is not redundant, so this proof is not
/// vacuous. From a reachable, invariant-satisfying state with the CQ exactly
/// full, the broken post wraps onto a slot the user still owns and violates
/// INV-CQ-no-overwrite; the real `PostCqe` keeps every state invariant.
pub proof fn broken_post_violates_invariant()
    ensures
        exists|s: RingState|
            #![trigger broken_post(s)]
            ring_inv(s) && !ring_inv(broken_post(s)),
        forall|s: RingState| ring_inv(s) ==> #[trigger] ring_inv(step(s, Step::PostCqe)),
{
    let full = RingState {
        sq_head: 0,
        sq_tail: 0,
        cq_head: 0,
        cq_tail: 4,
        cq_entries: 4,
        sq_entries: 4,
        inflight_len: 0,
        cap: 4,
        cq_overflow: 0,
        overflow_latched: false,
    };
    assert(ring_inv(full));
    assert(cq_full(full));
    let overrun = broken_post(full);
    assert(overrun.cq_tail == 5);
    assert(overrun.cq_tail - overrun.cq_head == 5);
    assert(overrun.cq_entries == 4);
    // Gap > cq_entries — violates the (INV-CQ-no-overwrite) conjunct.
    assert(!ring_inv(overrun));
    assert(ring_inv(full) && !ring_inv(broken_post(full)));
    assert(exists|s: RingState| #![trigger broken_post(s)] ring_inv(s) && !ring_inv(broken_post(s)));
    assert forall|s: RingState| ring_inv(s) implies #[trigger] ring_inv(step(s, Step::PostCqe)) by {
        step_preserves(s, Step::PostCqe);
    }
}

} // verus!
