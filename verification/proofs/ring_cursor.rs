// SlopRing SQ/CQ cursor + in-flight state-machine proof.
//
// This is a Verus-annotated mirror of the index/counter state machine in
// `slopos_ring::ring_obj::Ring` and `slopos_ring::enter::{submit, post_cqe}`
// — the kernel-side bookkeeping that decides, on every submit and every
// completion post, whether a slot is free, whether the CQ has overflowed,
// and whether the in-flight table is at capacity. It machine-checks the
// SlopRing index invariants:
//
//   (INV-CQ-no-overwrite)        The producer never overwrites an
//        unconsumed completion: `cq_tail - cq_head <= cq_entries` always.
//   (INV-CQ-full-correctness)    `cq_full <=> cq_tail - cq_head >= cq_entries`;
//        a `PostCqe` writes a CQE iff `!cq_full`, and the post / overflow
//        branches are mutually exclusive and exhaustive.
//   (INV-overflow-monotone-latch) `cq_overflow` only ever increases (by one
//        per dropped CQE), and the sticky `overflow_latched` flag is a
//        one-way false -> true transition.
//   (INV-cq-tail-advance-exactly-one) A successful `PostCqe` advances
//        `cq_tail` by exactly one; a dropped (overflow) post advances it by
//        zero.
//   (INV-inflight-cap)           `inflight_len <= cap`; `PushInflight` is a
//        no-op at capacity (`InFlightVec::push` returns false), and
//        `RemoveInflight` never underflows (the SLOPRING § 9 / slab Inv. 9
//        analogue).
//   (INV-submit-consume-bound)   `Submit` consumes exactly
//        `min(to_submit, sq_entries, sq_tail - sq_head)` SQEs and advances
//        `sq_head` by exactly that — so `sq_head` never passes the
//        user-published `sq_tail`.
//
// The broken-witness at the bottom is load-bearing and NON-VACUOUS: a
// `broken_post` that advances `cq_tail` WITHOUT the `cq_full` check produces
// a reachable, invariant-satisfying state on which the invariant breaks
// (the producer overwrites an unconsumed CQE), while the real `PostCqe`
// keeps every state invariant. This mirrors `frame_refcount.rs`'s
// `broken_clone_violates_invariant`.
//
// TRUSTED BOUNDARY (handled by EXCLUSION — these facts are simply NOT
// modelled here; this file contains no Verus proof-escape constructs):
//   * The four volatile `UFrame` accessors
//     (`slopos_ostd::mm::uframe::{load_u32_acquire, store_u32_release,
//     copy_out_volatile, copy_in_volatile}`, reached through
//     `ring/src/region.rs`): taken on faith, NOT modelled. This proof
//     reasons about abstract index/counter values, never the memory op that
//     reads or writes them. KernMiri-covered, audited-only.
//   * The kernel/userland release/acquire memory-ordering protocol
//     (`ring_obj.rs:96-105` publish_*; `:120-145` post_cqe fences): trusted,
//     not machine-checked — Verus has no weak-memory model.
//   * Per-ring SpinLock mutual exclusion (`registry::with_ring` holds it
//     across each submit / harvest_step / post_cqe): trusted. It is what
//     lets us model the KERNEL as a single sequential `Step` machine — there
//     is exactly one kernel writer per ring at a time.
//   * Userland cursor monotonicity: `sq_tail` (SQ producer) and `cq_head`
//     (CQ consumer) are USER-OWNED cells (`read_sq_tail` / `read_cq_head`).
//     A malicious user racing those cells is NOT modelled at the memory
//     level; instead the `UserAdvanceSqTail` / `UserAdvanceCqHead` steps
//     model them as adversarial-monotone inputs. This is REQUIRED for
//     soundness: without an adversarial user-cursor step the no-overwrite
//     invariant would be vacuously / dishonestly true (the kernel would be
//     the only mutator of the gap it is supposed to respect).
//
// WRAPPING-ARITHMETIC ABSTRACTION GAP. The real cursors are `u32` and the
// occupancy is computed with `wrapping_sub` (`available_cqes`,
// `cq_full`, `submit`'s `available`). This proof models them as
// unbounded `nat` counters with ordinary subtraction. The two models agree
// exactly under the occupancy bound this proof itself establishes:
// `cq_head <= cq_tail` with `cq_tail - cq_head <= cq_entries <= u32::MAX`
// (and symmetrically `sq_head <= sq_tail`, `sq_tail - sq_head <=
// sq_entries`). Under that bound `cq_tail.wrapping_sub(cq_head)` equals the
// nat difference, so modelling the difference as a `nat` loses nothing. The
// bound is exactly `INV-CQ-no-overwrite` / `INV-submit-consume-bound`, which
// the proof maintains inductively — so the abstraction is established by the
// proof, not stipulated. (The actual u32 wraparound of the absolute counters
// is irrelevant to every control decision, which reads only the difference.)
//
// Field correspondence:
//   `sq_head`          <-> `Ring::sq_head`            (ring_obj.rs:56)
//   `sq_tail`          <-> user-owned SQ producer,    read via
//                          `Ring::read_sq_tail`        (ring_obj.rs:90-93)
//   `cq_head`          <-> user-owned CQ consumer,    read via
//                          `Ring::read_cq_head`        (ring_obj.rs:84-87)
//   `cq_tail`          <-> `Ring::cq_tail`            (ring_obj.rs:59)
//   `cq_entries`       <-> `Ring::layout.cq_entries`  (abi/ring.rs:520)
//   `sq_entries`       <-> `Ring::layout.sq_entries`  (abi/ring.rs:519)
//   `inflight_len`     <-> `InFlightVec::len`         (ring_obj.rs:175)
//   `cap`              <-> `InFlightVec::cap` (== cq_entries; the
//                          `cq_cap` passed at setup, enter.rs:93,99)
//   `cq_overflow`      <-> `Ring::cq_overflow`        (ring_obj.rs:68,
//                          bumped at ring_obj.rs:122)
//   `overflow_latched` <-> the `SLOPRING_CQ_OVERFLOW` bit stored into the
//                          CQ flags word                (ring_obj.rs:128-129)

use vstd::prelude::*;

verus! {

// ---------------------------------------------------------------------------
// Abstract ring index / counter state.
// ---------------------------------------------------------------------------

/// Abstract image of one ring's SQ/CQ cursors, in-flight table size, and
/// overflow accounting. The four cursors are modelled as monotone `nat`
/// counters (see the wrapping-arithmetic note in the header); the kernel
/// owns `sq_head` and `cq_tail`, the user owns `sq_tail` and `cq_head`.
pub struct RingState {
    /// Kernel-owned SQ consumer cursor (`Ring::sq_head`). Advances only in
    /// `submit`, by exactly the number of SQEs consumed.
    pub sq_head: nat,
    /// User-owned SQ producer cursor (read via `Ring::read_sq_tail`).
    /// Adversarial-monotone input (`UserAdvanceSqTail`).
    pub sq_tail: nat,
    /// User-owned CQ consumer cursor (read via `Ring::read_cq_head`).
    /// Adversarial-monotone input (`UserAdvanceCqHead`).
    pub cq_head: nat,
    /// Kernel-owned CQ producer cursor (`Ring::cq_tail`). Advances only in
    /// a successful (non-overflow) `PostCqe`, by exactly one.
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
    // The two cursor-ordering facts that make the nat model faithful to the
    // real `wrapping_sub` occupancy (header gap note). The kernel cursor
    // never passes the user cursor it trails.
    &&& s.cq_head <= s.cq_tail
    &&& s.sq_head <= s.sq_tail
    // (INV-CQ-no-overwrite) The producer never overwrites an unconsumed
    //      completion: at most `cq_entries` CQEs are outstanding. This is the
    //      core CQ-correctness guarantee — the gap between the kernel
    //      producer and the user consumer never exceeds the ring size.
    &&& s.cq_tail - s.cq_head <= s.cq_entries
    // (INV-submit-consume-bound) Symmetrically on the SQ side: the kernel
    //      never claims to have consumed more SQEs than the user published.
    &&& s.sq_tail - s.sq_head <= s.sq_entries
    // (INV-inflight-cap) The in-flight table never exceeds its capacity.
    &&& s.inflight_len <= s.cap
    // The capacity equals the CQ ring size (set once at setup, never moves).
    &&& s.cap == s.cq_entries
    // Both ring sizes are positive (a 0-entry ring is rejected at setup,
    // `entries == 0 => EINVAL`, enter.rs:59).
    &&& s.cq_entries > 0
    &&& s.sq_entries > 0
}

/// A fresh ring from `ring_setup`: both kernel cursors zero, the shared
/// user cursors zero (the region is zero-filled and `write_initial_region`
/// stores 0 into every index, enter.rs:152-164), no in-flight rows, no
/// overflow, flag clear. `cap == cq_entries == sq_entries * SLOPRING_SQ_TO_CQ`
/// — here we only need `cap == cq_entries` and both sizes positive.
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

// ---------------------------------------------------------------------------
// Helper spec functions mirroring the real predicates.
// ---------------------------------------------------------------------------

/// `Ring::cq_full` (ring_obj.rs:79-81): the CQ has no free slot iff the
/// outstanding-CQE count has reached the ring size. The real code computes
/// `cq_tail.wrapping_sub(cq_head) >= cq_entries`; under `ring_inv` the nat
/// difference equals that `wrapping_sub` (header gap note).
pub open spec fn cq_full(s: RingState) -> bool {
    s.cq_tail - s.cq_head >= s.cq_entries
}

/// The submit clamp (`submit`, enter.rs:220-221): consume
/// `min(to_submit, sq_entries, sq_tail - sq_head)`.
pub open spec fn submit_count(s: RingState, to_submit: nat) -> nat {
    let available = (s.sq_tail - s.sq_head) as nat;
    min3(to_submit, s.sq_entries, available)
}

/// Three-way minimum.
pub open spec fn min3(a: nat, b: nat, c: nat) -> nat {
    min2(min2(a, b), c)
}

/// Two-way minimum.
pub open spec fn min2(a: nat, b: nat) -> nat {
    if a <= b {
        a
    } else {
        b
    }
}

// ---------------------------------------------------------------------------
// Steps: one per atomic-bounded ring bookkeeping body (all under the
// per-ring SpinLock, so the kernel is a single sequential writer).
// ---------------------------------------------------------------------------

pub enum Step {
    /// `submit(pid, ring, to_submit)` (enter.rs:215-238). Consumes
    /// `min(to_submit, sq_entries, sq_tail - sq_head)` SQEs and advances the
    /// kernel `sq_head` by exactly that count. Models only the cursor
    /// advance — the per-SQE `process_sqe` side effects (which may
    /// `PostCqe` / `PushInflight`) are separate steps.
    Submit { to_submit: nat },
    /// `Ring::post_cqe` (ring_obj.rs:114-147), the *successful* branch:
    /// `!cq_full`, so write the CQE and advance `cq_tail` by exactly one.
    PostCqe,
    /// `Ring::post_cqe`, the *overflow* branch (ring_obj.rs:121-133): the CQ
    /// is full, so drop the CQE — bump `cq_overflow` by one and latch the
    /// sticky flag. `cq_tail` does NOT advance.
    PostOverflow,
    /// `InFlightVec::push` (ring_obj.rs:186-191): record one in-flight row.
    /// A no-op at capacity (`is_full` -> returns false; the caller then
    /// completes the SQE inline with `-EAGAIN`).
    PushInflight,
    /// `InFlightVec::remove_at` (ring_obj.rs:195-200): drop one in-flight
    /// row (harvest completion / cancel). A no-op when the table is empty.
    RemoveInflight,
    /// The user advancing its CQ consumer cursor `cq_head` by `by`
    /// (harvesting `by` CQEs). Adversarial-monotone input: a real user can
    /// only ever *increase* `cq_head`, and never past the published
    /// `cq_tail` (it has nothing to harvest beyond what the kernel posted).
    UserAdvanceCqHead { by: nat },
    /// The user advancing its SQ producer cursor `sq_tail` by `by`
    /// (publishing `by` fresh SQEs). Adversarial-monotone input: a real user
    /// can only ever *increase* `sq_tail`; it is bounded by the SQ ring size
    /// over what the kernel has already consumed (it cannot publish into
    /// slots the kernel has not yet drained without overwriting — modelled
    /// as the clamp to keep occupancy `<= sq_entries`).
    UserAdvanceSqTail { by: nat },
}

/// Transition function: the post-state after applying `t` to `s`. Each arm
/// mirrors the corresponding body. Every arm is total — the else-branch is
/// the identity, matching the real no-op paths (clamp to 0, full guard,
/// at-capacity push, empty remove).
pub open spec fn step(s: RingState, t: Step) -> RingState {
    match t {
        Step::Submit { to_submit } => {
            // Advance sq_head by exactly the clamped consume count.
            let n = submit_count(s, to_submit);
            RingState { sq_head: (s.sq_head + n) as nat, ..s }
        },
        Step::PostCqe =>
            // Successful post only when the CQ has a free slot.
            if !cq_full(s) {
                RingState { cq_tail: (s.cq_tail + 1) as nat, ..s }
            } else {
                s
            },
        Step::PostOverflow =>
            // The overflow branch fires only when the CQ is full; drop the
            // CQE, bump the counter, latch the sticky flag.
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
            // Push one row unless already at capacity (no-op at cap).
            if s.inflight_len < s.cap {
                RingState { inflight_len: (s.inflight_len + 1) as nat, ..s }
            } else {
                s
            },
        Step::RemoveInflight =>
            // Remove one row unless the table is empty (no underflow).
            if s.inflight_len > 0 {
                RingState { inflight_len: (s.inflight_len - 1) as nat, ..s }
            } else {
                s
            },
        Step::UserAdvanceCqHead { by } => {
            // The user harvests `by` CQEs, but can never advance cq_head past
            // cq_tail (it has only `cq_tail - cq_head` posted CQEs to read).
            // Clamp keeps cq_head <= cq_tail — the adversarial-monotone model
            // of a well-behaved-OR-lagging consumer; it can race but cannot
            // manufacture completions the kernel never posted.
            let room = (s.cq_tail - s.cq_head) as nat;
            let adv = min2(by, room);
            RingState { cq_head: (s.cq_head + adv) as nat, ..s }
        },
        Step::UserAdvanceSqTail { by } => {
            // The user publishes `by` SQEs, but occupancy cannot exceed the
            // SQ ring size (it would overwrite un-consumed SQEs otherwise).
            // Clamp keeps sq_tail - sq_head <= sq_entries.
            let room = (s.sq_entries - (s.sq_tail - s.sq_head)) as nat;
            let adv = min2(by, room);
            RingState { sq_tail: (s.sq_tail + adv) as nat, ..s }
        },
    }
}

// ---------------------------------------------------------------------------
// The invariant is inductive over every step.
// ---------------------------------------------------------------------------

/// Every `Step` preserves `ring_inv`. Because each step is the image of one
/// atomic-bounded body run under the per-ring SpinLock (the kernel is the
/// single sequential writer, and the two `UserAdvance*` steps model the
/// adversarial user cursors), this one inductive fact is the whole-system
/// guarantee: no interleaving of submit / post / overflow / push / remove /
/// user-harvest / user-publish reaches a state violating `ring_inv`.
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

// ---------------------------------------------------------------------------
// Reachability: lift the inductive step to whole execution traces.
// ---------------------------------------------------------------------------

/// Replay a finite trace of steps from a start state. Under the per-ring
/// lock a trace is the total order the lock imposes on the kernel's
/// submit/post/push/remove bodies, interleaved with the user's
/// (adversarial-monotone) cursor advances.
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
/// invariant still holds. The machine-checked statement of every SlopRing
/// index invariant over all executions.
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

// ---------------------------------------------------------------------------
// Named corollaries, one per ring index invariant.
// ---------------------------------------------------------------------------

/// (INV-CQ-no-overwrite) In every reachable state the kernel producer never
/// overwrites an unconsumed completion: the gap between `cq_tail` and the
/// user's `cq_head` never exceeds the ring size, so a `PostCqe` slot
/// (`cq_tail & (cq_entries - 1)`) is always one the user has already
/// consumed or never held.
pub proof fn inv_cq_no_overwrite(s0: RingState, trace: Seq<Step>)
    requires
        ring_init(s0),
    ensures
        run(s0, trace).cq_tail - run(s0, trace).cq_head <= run(s0, trace).cq_entries,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (INV-CQ-full-correctness) `cq_full` is exactly "no free slot", and the
/// `PostCqe` / `PostOverflow` branches are mutually exclusive and
/// exhaustive: in any invariant state, a `PostCqe` advances `cq_tail` iff
/// `!cq_full`, and `PostOverflow` bumps the drop counter iff `cq_full` — the
/// two never both act, and exactly one is enabled.
pub proof fn inv_cq_full_correctness(s: RingState)
    requires
        ring_inv(s),
    ensures
        // PostCqe writes (advances cq_tail) iff there is a free slot.
        !cq_full(s) ==> step(s, Step::PostCqe).cq_tail == s.cq_tail + 1,
        cq_full(s) ==> step(s, Step::PostCqe).cq_tail == s.cq_tail,
        // PostOverflow drops (bumps overflow) iff the CQ is full.
        cq_full(s) ==> step(s, Step::PostOverflow).cq_overflow == s.cq_overflow + 1,
        !cq_full(s) ==> step(s, Step::PostOverflow).cq_overflow == s.cq_overflow,
        // Mutually exclusive: never both a write and a drop on one state.
        !(step(s, Step::PostCqe).cq_tail != s.cq_tail
            && step(s, Step::PostOverflow).cq_overflow != s.cq_overflow),
{
}

/// (INV-overflow-monotone-latch) `cq_overflow` never decreases across any
/// step (it only ever grows by one in `PostOverflow`), and the sticky
/// `overflow_latched` flag is monotone false -> true: once raised it is
/// never cleared.
pub proof fn inv_overflow_monotone_latch(s: RingState, t: Step)
    requires
        ring_inv(s),
    ensures
        step(s, t).cq_overflow >= s.cq_overflow,
        s.overflow_latched ==> step(s, t).overflow_latched,
{
}

/// (INV-cq-tail-advance-exactly-one) A successful `PostCqe` advances
/// `cq_tail` by exactly one; a dropped (`PostOverflow`) post advances it by
/// zero. No step ever advances `cq_tail` by more than one.
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
/// capacity; `PushInflight` is a no-op at capacity and `RemoveInflight`
/// never underflows below zero.
pub proof fn inv_inflight_cap(s0: RingState, trace: Seq<Step>)
    requires
        ring_init(s0),
    ensures
        run(s0, trace).inflight_len <= run(s0, trace).cap,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (INV-inflight-cap, step level) Push at capacity changes nothing; remove
/// on an empty table changes nothing — the two no-op guards Inv-inflight-cap
/// leans on.
pub proof fn inv_inflight_guards(s: RingState)
    requires
        ring_inv(s),
    ensures
        s.inflight_len == s.cap ==> step(s, Step::PushInflight).inflight_len == s.inflight_len,
        s.inflight_len == 0 ==> step(s, Step::RemoveInflight).inflight_len == 0,
{
}

/// (INV-submit-consume-bound) A `Submit` consumes exactly
/// `min(to_submit, sq_entries, sq_tail - sq_head)` and advances `sq_head` by
/// exactly that — so in every reachable state `sq_head <= sq_tail`: the
/// kernel never claims an SQE the user has not published.
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

// ---------------------------------------------------------------------------
// The full check is load-bearing: a PostCqe that skips it breaks
// INV-CQ-no-overwrite.
// ---------------------------------------------------------------------------

/// A *broken* `post_cqe` that advances `cq_tail` UNCONDITIONALLY — i.e.
/// `Ring::post_cqe` with the `if self.cq_full(cq_head) { ... return }` guard
/// (ring_obj.rs:121) removed. It writes a CQE and bumps `cq_tail` even when
/// the CQ is already full, overwriting a completion the user has not yet
/// harvested.
pub open spec fn broken_post(s: RingState) -> RingState {
    RingState { cq_tail: (s.cq_tail + 1) as nat, ..s }
}

/// Witness that the `cq_full` guard is not redundant. Take a reachable,
/// invariant-satisfying state with the CQ exactly full — `cq_tail - cq_head
/// == cq_entries`, every slot holding an unharvested CQE (reachable:
/// `cq_entries` successful `PostCqe`s with the user never advancing
/// `cq_head`). The broken post bumps `cq_tail` to `cq_head + cq_entries + 1`,
/// so `cq_tail - cq_head > cq_entries` — the producer has wrapped onto a slot
/// the user still owns and overwritten an unconsumed completion, violating
/// `ring_inv` (INV-CQ-no-overwrite). The real `PostCqe` refuses the advance
/// (it takes the overflow branch instead) and preserves the invariant on
/// every state. This proves CQ correctness genuinely depends on the
/// `cq_full` check.
pub proof fn broken_post_violates_invariant()
    ensures
        // There is a reachable, invariant-satisfying state on which the
        // broken post produces an invariant-violating state...
        exists|s: RingState|
            #![trigger broken_post(s)]
            ring_inv(s) && !ring_inv(broken_post(s)),
        // ...while the real (full-checked) PostCqe keeps every state
        // invariant.
        forall|s: RingState| ring_inv(s) ==> #[trigger] ring_inv(step(s, Step::PostCqe)),
{
    // A full CQ: cq_entries unharvested CQEs, user cursor at the bottom.
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
    // The broken post overruns the consumer: cq_tail - cq_head == 5 > 4.
    let overrun = broken_post(full);
    assert(overrun.cq_tail == 5);
    assert(overrun.cq_tail - overrun.cq_head == 5);
    assert(overrun.cq_entries == 4);
    // gap > cq_entries — violates the (INV-CQ-no-overwrite) conjunct.
    assert(!ring_inv(overrun));
    assert(ring_inv(full) && !ring_inv(broken_post(full)));
    assert(exists|s: RingState| #![trigger broken_post(s)] ring_inv(s) && !ring_inv(broken_post(s)));
    // The real full-checked PostCqe preserves the invariant on every state.
    assert forall|s: RingState| ring_inv(s) implies #[trigger] ring_inv(step(s, Step::PostCqe)) by {
        step_preserves(s, Step::PostCqe);
    }
}

} // verus!
