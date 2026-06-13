// `Frame<M>` reference-count proof.
//
// This is a Verus-annotated mirror of the load-bearing state machine in
// `slopos_ostd::mm::frame::{MetaSlot, Frame, Drop}`. It machine-checks the
// three reference-count invariants:
//
//   (I1) If `frame.ref_count() > 0`, the underlying physical frame is
//        allocated and not on the free list.
//   (I2) `Drop` decrements `ref_count`; on the transition to 0 it releases
//        the frame to the parent allocator *exactly once*.
//   (I3) Concurrent `Frame::clone` (`from_in_use`) and `Frame::drop` cannot
//        produce a use-after-free.
//   (I4) A frame on the allocator free list has a claimable (non-TYPED)
//        slot — `Drop` resets the slot to UNUSED *before* it returns the
//        page, so a concurrent `from_unused` on the recycled paddr never
//        races a still-TYPED slot (the SMP `PathCorrupt` ring-map bug).
//
// State machine. `MetaSlot::ref_count` (AtomicU32) is the slot's whole
// lifecycle in one atomic, with two sentinels:
//   `REF_COUNT_UNUSED` (u32::MAX) — free and claimable by `from_unused`.
//   `REF_COUNT_BUSY`   (0)        — transient: a `from_unused` or a `Drop`
//                                   owns the slot exclusively to construct
//                                   or tear down the metadata.
//   `1..=REF_COUNT_MAX`           — that many live `Frame` handles.
// `from_unused` is `CAS(UNUSED -> BUSY)` (retrying while it reads BUSY);
// `Drop`'s `fetch_sub(1)` from the last ref (1) lands on BUSY (0). So both
// construction and destruction hold the slot at BUSY, where `from_unused`
// retries and `from_in_use` refuses (`from_in_use` bumps only a live count).
//
// Background. The Asterinas paper (USENIX ATC '25, Fig. 9) found a real UB
// in the equivalent OSTD path via KernMiri: a `fetch_add(1)` clone could
// resurrect a slot whose teardown had already begun (the count had hit the
// BUSY value), racing the `drop_in_place` of the last dropper. SlopOS closes
// that race with the conditional increment — `from_in_use` refuses to bump
// from BUSY/UNUSED (see `frame.rs::from_in_use`). This proof encodes both
// the fixed clone and the broken `fetch_add` clone and shows the inductive
// invariant holds for the former and is violated by the latter — so the
// proof is load-bearing, not vacuous.
//
// Modelling strategy. Every method on the real `Frame`/`MetaSlot` touches
// the slot only through atomic operations (CAS, fetch_update, fetch_sub,
// Release/Acquire stores). The BUSY sentinel makes construction and
// destruction *exclusively owned* (no other step can claim or bump a BUSY
// slot), so each method body is one atomic-bounded `Step` against the shared
// slot. An inductive invariant that survives every `Step` then holds in
// every reachable state of every interleaving — which is exactly the
// concurrency claim (I3): no schedule of clones and drops can reach a state
// the invariant forbids.
//
// Field correspondence to `frame.rs`:
//   `typed`        <-> the slot is live (`ref_count` in `1..=REF_COUNT_MAX`)
//   `rc`           <-> the live reference count (`0` models the UNUSED/BUSY
//                      sentinels — i.e. "no live handle")
//   `payload_live` <-> the inline `M` payload + vtable are still valid
//                      (i.e. `drop_in_place` has NOT run)
//   `on_free_list` <-> the physical frame is back in the FrameAlloc free
//                      list (released by `return_frame_to_allocator`)
//   `releases`     <-> ghost counter: how many times the slot's frame has
//                      been handed back to the allocator this epoch
//                      (must be exactly 0 while live, at most 1 after).

use vstd::prelude::*;

verus! {

// ---------------------------------------------------------------------------
// Abstract slot state.
// ---------------------------------------------------------------------------

/// Abstract image of a single `MetaSlot` plus the allocator's view of the
/// physical frame it tracks. `payload_live` and `on_free_list` are the two
/// facts a use-after-free would violate; `releases` is the ghost
/// double-free counter behind invariant (I2).
pub struct Slot {
    /// The slot is live: `MetaSlot::ref_count` in `1..=REF_COUNT_MAX`.
    pub typed: bool,
    /// The live reference count (`0` models the UNUSED/BUSY sentinels).
    pub rc: nat,
    /// The inline `M` and vtable are still valid — `on_drop` /
    /// `drop_in_place` have not yet run. A live `Frame` borrow
    /// (`Frame::borrow`, `as_bytes`, the HHDM accessors) dereferences
    /// memory whose validity is exactly this bit.
    pub payload_live: bool,
    /// The physical frame is sitting in the FrameAlloc free list — i.e. it
    /// has been `dealloc`'d and a future `alloc` may hand it out again.
    pub on_free_list: bool,
    /// Ghost: number of times this epoch's frame was returned to the
    /// allocator. Behind (I2)'s "exactly once".
    pub releases: nat,
}

/// The inductive slot invariant. Every reachable slot state satisfies it;
/// each `Step` preserves it (`step_preserves` below).
pub open spec fn slot_inv(s: Slot) -> bool {
    // (I1) A positive ref-count pins the frame: it is typed, its payload is
    //      valid, and it is NOT on the allocator free list. This is the
    //      core "allocated and not free" guarantee.
    &&& (s.rc > 0 ==> s.typed)
    &&& (s.rc > 0 ==> s.payload_live)
    &&& (s.rc > 0 ==> !s.on_free_list)
    // (I3) No use-after-free: a valid payload is never simultaneously on the
    //      free list. Equivalently, the bytes a `Frame::borrow` reads are
    //      never memory the allocator considers reusable.
    &&& !(s.payload_live && s.on_free_list)
    // (I4) Reclaim-after-reset: a frame on the allocator free list always has
    //      a claimable (non-TYPED) slot. This is the liveness guarantee the
    //      `Drop` teardown ordering must provide — reset the slot to UNUSED
    //      *before* returning the page — so that a concurrent `from_unused`
    //      handed the recycled paddr never finds it still TYPED (which would
    //      be a spurious `StateMismatch` → `PathCorrupt`, the SMP ring-map
    //      bug). `broken_drop_ordering_violates_invariant` below shows the
    //      pre-fix free-before-reset ordering breaks exactly this conjunct.
    &&& (s.on_free_list ==> !s.typed)
    // (I2) Exactly-once release. While the payload is live the frame has
    //      been released zero times; once teardown has run it is released at
    //      most once. `on_free_list` is reached only via that single
    //      release, so a second `dealloc` (double-free) is unreachable.
    &&& (s.payload_live ==> s.releases == 0)
    &&& (s.releases <= 1)
    &&& (s.on_free_list ==> s.releases == 1)
    // Teardown ordering: a typed-but-dead payload has already hit rc 0
    //      (Drop zeroes rc before clearing `payload_live`/state).
    &&& (s.typed && !s.payload_live ==> s.rc == 0)
}

/// The state a fresh, never-allocated slot starts in — `META_SLOTS` is
/// zero-initialised at boot, so every slot begins `UNUSED`, rc 0, and no
/// payload. `on_free_list` is the FrameAlloc's *released*-frame predicate:
/// a slot that has never completed an alloc/dealloc round trip is not a
/// reclaimed frame, so it starts `false` and only becomes `true` via the
/// single `DropFinal` release (this is what makes `releases` a faithful
/// double-free counter — see `slot_inv`'s `on_free_list ==> releases == 1`).
pub open spec fn slot_init(s: Slot) -> bool {
    &&& s.typed == false
    &&& s.rc == 0
    &&& s.payload_live == false
    &&& s.on_free_list == false
    &&& s.releases == 0
}

// ---------------------------------------------------------------------------
// Steps: one per atomic-bounded `MetaSlot` method body.
// ---------------------------------------------------------------------------

pub enum Step {
    /// `Frame::from_unused`: CAS `ref_count` UNUSED->BUSY (held exclusively),
    /// write meta, publish `ref_count = 1` (live). The frame leaves the
    /// allocator free list (just handed out by `FrameAlloc::alloc`). Resets
    /// the per-epoch ghost release counter — a fresh allocation is a new
    /// epoch. The BUSY interlock makes the construct atomic (no other step
    /// can claim or bump a BUSY slot), so it is one `Step`.
    FromUnused,
    /// `Frame::from_in_use` — the *fixed* clone. `fetch_update` bumps the
    /// count only from a live value; it refuses BUSY/UNUSED, so it cannot
    /// resurrect a slot whose last ref just hit BUSY. This is the line that
    /// closes the Asterinas Fig. 9 race.
    CloneConditional,
    /// `Frame::drop`, non-final: `fetch_sub(1)` observed a previous value
    /// > 1, so this is not the last reference. Nothing else happens.
    DropNonFinal,
    /// `Frame::drop`, final: `fetch_sub(1)` observed exactly 1, landing the
    /// slot at BUSY (held exclusively). The Acquire fence, then `drop_in_place`
    /// (payload no longer live), then the slot published UNUSED, then —
    /// *last* — the page returned to the free list (`dealloc`, bumping
    /// `releases`). Modelled as one atomic-ordered step: the BUSY interlock
    /// means no other step interleaves the teardown, and the only sub-step a
    /// peer observes (the page appearing on the free list) happens after the
    /// slot is already UNUSED, so `typed=false, on_free_list=true` faithfully
    /// captures every interleaving. (Reset-before-free is what makes this
    /// atomic model sound — see `broken_drop_ordering_violates_invariant`.)
    DropFinal,
    /// `Frame::drop`, final, for a meta whose
    /// `returns_frame_on_last_drop()` is `false` (the statically-borrowed
    /// kernel-master PML4 and externally-owned DMA segment frames). Same
    /// teardown as `DropFinal` — payload dropped, slot reset to UNUSED —
    /// but the page is NOT handed back to the allocator, so `on_free_list`
    /// stays false and `releases` is untouched. Models the `if return_page`
    /// false branch of the `Drop` impl.
    DropFinalNoFree,
}

/// Transition function: the post-state after applying `t` to `s`. Each arm
/// mirrors the corresponding method body in `frame.rs`.
pub open spec fn step(s: Slot, t: Step) -> Slot {
    match t {
        Step::FromUnused =>
            // Only fires on a genuinely unused slot (CAS UNUSED->BUSY then
            // publish live succeeds). The allocator just gave us the frame,
            // so it is off the free list and a new epoch begins (releases 0).
            if !s.typed && s.rc == 0 && !s.payload_live {
                Slot { typed: true, rc: 1, payload_live: true, on_free_list: false, releases: 0 }
            } else {
                s
            },
        Step::CloneConditional =>
            // fetch_update refuses BUSY/UNUSED (rc == 0); otherwise rc += 1.
            if s.rc > 0 {
                Slot { rc: (s.rc + 1) as nat, ..s }
            } else {
                s
            },
        Step::DropNonFinal =>
            // fetch_sub saw prev > 1: just decrement, teardown not reached.
            if s.rc > 1 {
                Slot { rc: (s.rc - 1) as nat, ..s }
            } else {
                s
            },
        Step::DropFinal =>
            // fetch_sub saw prev == 1: run teardown exactly once.
            if s.rc == 1 {
                Slot {
                    typed: false,
                    rc: 0,
                    payload_live: false,
                    on_free_list: true,
                    releases: (s.releases + 1) as nat,
                }
            } else {
                s
            },
        Step::DropFinalNoFree =>
            // fetch_sub saw prev == 1, but `returns_frame_on_last_drop` is
            // false: tear the slot down to UNUSED but keep the page (it is
            // owned elsewhere). on_free_list stays false; releases untouched.
            if s.rc == 1 {
                Slot { typed: false, rc: 0, payload_live: false, ..s }
            } else {
                s
            },
    }
}

// ---------------------------------------------------------------------------
// (I1)+(I2)+(I3): the invariant is inductive over every step.
// ---------------------------------------------------------------------------

/// Every `Step` preserves `slot_inv`. Because each step is the image of an
/// atomic-bounded method body, and any concurrent interleaving is a
/// sequence of such steps against the shared slot, this single inductive
/// fact is the whole-system concurrency guarantee: no schedule of clones
/// and drops reaches a state violating `slot_inv` (hence no UAF, no
/// double-free, no positive-rc-on-free-list).
pub proof fn step_preserves(s: Slot, t: Step)
    requires
        slot_inv(s),
    ensures
        slot_inv(step(s, t)),
{
}

/// The initial slot state satisfies the invariant — base case for the
/// induction over reachable states below.
pub proof fn init_inv(s: Slot)
    requires
        slot_init(s),
    ensures
        slot_inv(s),
{
}

// ---------------------------------------------------------------------------
// Reachability: lift the inductive step to whole execution traces.
// ---------------------------------------------------------------------------

/// Replay a finite trace of steps from a start state. A trace is any finite
/// sequence of `Step`s — i.e. any interleaving of `from_unused` /
/// `from_in_use` / `drop` calls from any number of CPUs, since each leaves
/// the shared slot only through these atomic-bounded transitions.
pub open spec fn run(s: Slot, trace: Seq<Step>) -> Slot
    decreases trace.len(),
{
    if trace.len() == 0 {
        s
    } else {
        step(run(s, trace.drop_last()), trace.last())
    }
}

/// MAIN THEOREM. From any initial slot, after *any* trace of clone/drop
/// steps (any concurrent interleaving), the invariant still holds. This is
/// the machine-checked statement of (I1)+(I2)+(I3) over all executions.
pub proof fn invariant_holds_on_every_trace(s0: Slot, trace: Seq<Step>)
    requires
        slot_init(s0),
    ensures
        slot_inv(run(s0, trace)),
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
// Named corollaries, one per reference-count invariant. These read the
// three invariants straight off the reachable-state guarantee.
// ---------------------------------------------------------------------------

/// (I1) "If `frame.ref_count() > 0`, the underlying physical frame is
/// allocated and not on the free list." In any reachable state, a positive
/// ref-count implies the frame is typed (allocated) and absent from the
/// allocator's free list.
pub proof fn i1_positive_rc_is_allocated(s0: Slot, trace: Seq<Step>)
    requires
        slot_init(s0),
    ensures
        run(s0, trace).rc > 0 ==> run(s0, trace).typed && !run(s0, trace).on_free_list,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (I2) "On the transition to 0, `Drop` releases the frame to the parent
/// allocator exactly once." A reachable state never exceeds one release,
/// and being on the free list witnesses exactly one — so no double-free.
pub proof fn i2_release_at_most_once(s0: Slot, trace: Seq<Step>)
    requires
        slot_init(s0),
    ensures
        run(s0, trace).releases <= 1,
        run(s0, trace).on_free_list ==> run(s0, trace).releases == 1,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// `DropFinal` is the only step that performs a release, and it does so by
/// incrementing `releases` by exactly one — the "exactly once on the
/// transition to 0" half of (I2), stated at the step level. The no-free
/// teardown (`DropFinalNoFree`) reaches rc 0 the same way but does NOT
/// release, so it leaves the counter untouched.
pub proof fn i2_dropfinal_releases_once(s: Slot)
    requires
        slot_inv(s),
        s.rc == 1,
    ensures
        step(s, Step::DropFinal).releases == s.releases + 1,
        step(s, Step::DropFinal).on_free_list,
        step(s, Step::DropFinal).rc == 0,
        // The no-free teardown tears down to rc 0 without releasing.
        step(s, Step::DropFinalNoFree).rc == 0,
        step(s, Step::DropFinalNoFree).releases == s.releases,
        !step(s, Step::DropFinalNoFree).on_free_list,
        // No other step touches the release counter or frees the frame.
        step(s, Step::CloneConditional).releases == s.releases,
        step(s, Step::DropNonFinal).releases == s.releases,
        step(s, Step::FromUnused).releases == s.releases,
{
}

/// (I3) "Concurrent `Frame::clone` and `Frame::drop` cannot produce a
/// use-after-free." In every reachable state a valid payload and free-list
/// membership are mutually exclusive: no live `Frame::borrow` ever reads
/// memory the allocator has reclaimed.
pub proof fn i3_no_use_after_free(s0: Slot, trace: Seq<Step>)
    requires
        slot_init(s0),
    ensures
        !(run(s0, trace).payload_live && run(s0, trace).on_free_list),
{
    invariant_holds_on_every_trace(s0, trace);
}

// ---------------------------------------------------------------------------
// The fix is load-bearing: the *broken* `fetch_add(1)` clone breaks (I3).
// ---------------------------------------------------------------------------

/// The Asterinas Fig. 9 bug, modelled: an *unconditional* `fetch_add(1)`
/// clone that bumps the ref-count even from 0. This is what `from_in_use`
/// would be if it used a plain `fetch_add` instead of the conditional
/// `fetch_update`.
pub open spec fn broken_clone(s: Slot) -> Slot {
    Slot { rc: (s.rc + 1) as nat, ..s }
}

/// Witness that the conditional clone is *not* redundant. Take a slot
/// mid-teardown — rc has hit 0, the payload has been dropped, and the frame
/// is back on the free list (a perfectly reachable state: it is exactly
/// `step(_, DropFinal)`'s output). The broken `fetch_add` clone revives it
/// to rc 1 while it is still on the free list and its payload is dead,
/// landing in a state that violates `slot_inv` (I1: rc > 0 yet on the free
/// list with a dead payload). The fixed `CloneConditional` refuses the bump
/// and stays invariant. This proves the soundness genuinely depends on the
/// conditional increment.
pub proof fn broken_clone_violates_invariant()
    ensures
        // There is a reachable, invariant-satisfying state on which the
        // broken clone produces an invariant-violating state...
        exists|s: Slot|
            #![trigger broken_clone(s)]
            slot_inv(s) && !slot_inv(broken_clone(s)),
        // ...while the fixed clone keeps every such state invariant.
        forall|s: Slot| slot_inv(s) ==> #[trigger] slot_inv(step(s, Step::CloneConditional)),
{
    // A teardown-complete slot: rc 0, payload dropped, frame freed.
    let torn = Slot { typed: false, rc: 0, payload_live: false, on_free_list: true, releases: 1 };
    assert(slot_inv(torn));
    // The broken clone revives rc to 1 while still on the free list.
    let revived = broken_clone(torn);
    assert(revived.rc == 1);
    assert(revived.on_free_list);
    // rc > 0 yet on_free_list — violates the (I1) conjunct of slot_inv.
    assert(!slot_inv(revived));
    assert(slot_inv(torn) && !slot_inv(broken_clone(torn)));
    assert(exists|s: Slot| #![trigger broken_clone(s)] slot_inv(s) && !slot_inv(broken_clone(s)));
    // The fixed clone preserves the invariant on every state.
    assert forall|s: Slot| slot_inv(s) implies #[trigger] slot_inv(step(s, Step::CloneConditional)) by {
        step_preserves(s, Step::CloneConditional);
    }
}

// ---------------------------------------------------------------------------
// The teardown ORDERING is load-bearing: free-before-reset breaks (I4).
// ---------------------------------------------------------------------------
//
// `Frame::drop`'s teardown is three distinct writes other CPUs can interleave
// with — drop the payload, reset the slot to UNUSED, return the page to the
// allocator. The `DropFinal` step models them atomically *because the fix
// orders the slot reset before the page free*: the only sub-step a peer can
// observe (the page appearing on the free list) happens after the slot is
// already UNUSED. The two specs below expose that ordering as its sub-steps
// and prove the fix is not cosmetic — the pre-fix free-before-reset ordering
// reaches a state the invariant forbids.

/// FIXED ordering, sub-step 1: payload dropped and slot published UNUSED.
/// The page is NOT yet on the free list (it is still owned by the dropper),
/// so no `from_unused` for this paddr can fire here. Fires on the final
/// drop (rc just hit 0).
pub open spec fn drop_reset(s: Slot) -> Slot {
    Slot { typed: false, rc: 0, payload_live: false, on_free_list: false, releases: 0 }
}

/// FIXED ordering, sub-step 2: hand the page back to the allocator. The slot
/// is already UNUSED, so the recycled paddr is immediately claimable.
pub open spec fn drop_free(s: Slot) -> Slot {
    Slot { on_free_list: true, releases: (s.releases + 1) as nat, ..s }
}

/// BROKEN ordering (rejected): returning the page to the allocator FIRST —
/// `on_free_list` becomes true while the slot is still non-UNUSED (the reset
/// happens afterward). `payload_live` is already false (`drop_in_place` ran).
/// This is the window in which a concurrent `from_unused` would be handed a
/// paddr whose slot is not yet claimable.
pub open spec fn broken_drop_free(s: Slot) -> Slot {
    Slot { rc: 0, payload_live: false, on_free_list: true, releases: (s.releases + 1) as nat, ..s }
}

/// Witness that the teardown ordering is load-bearing. From a live,
/// singly-referenced slot, the pre-fix free-before-reset ordering reaches an
/// intermediate that violates `slot_inv` (a page on the free list whose slot
/// is still TYPED — (I4) — which is exactly the state where a concurrent
/// `from_unused` on the recycled paddr fails its CAS → `StateMismatch` →
/// `PathCorrupt`). The fixed reset-before-free ordering keeps every
/// sub-state invariant. So soundness of the `from_unused` CAS genuinely
/// depends on resetting the slot before freeing the page.
pub proof fn broken_drop_ordering_violates_invariant()
    ensures
        // The broken ordering takes a valid last-ref state to an
        // invariant-violating intermediate...
        exists|s: Slot|
            #![trigger broken_drop_free(s)]
            slot_inv(s) && s.rc == 1 && !slot_inv(broken_drop_free(s)),
        // ...while the fixed ordering keeps both sub-steps invariant.
        forall|s: Slot|
            (slot_inv(s) && s.rc == 1) ==> #[trigger] slot_inv(drop_reset(s))
                && slot_inv(drop_free(drop_reset(s))),
{
    // A live slot holding its last reference — the instant before `Drop`.
    let live = Slot { typed: true, rc: 1, payload_live: true, on_free_list: false, releases: 0 };
    assert(slot_inv(live));
    // Broken: the page hits the free list while the slot is still TYPED.
    let broken = broken_drop_free(live);
    assert(broken.on_free_list && broken.typed);
    // on_free_list yet typed — violates the (I4) conjunct of slot_inv.
    assert(!slot_inv(broken));
    assert(slot_inv(live) && live.rc == 1 && !slot_inv(broken_drop_free(live)));
    assert(exists|s: Slot|
        #![trigger broken_drop_free(s)]
        slot_inv(s) && s.rc == 1 && !slot_inv(broken_drop_free(s)));
    // The fixed ordering: reset publishes a clean UNUSED slot, then the free
    // flips on_free_list with the slot already non-TYPED — both invariant.
    assert forall|s: Slot| (slot_inv(s) && s.rc == 1) implies #[trigger] slot_inv(drop_reset(s))
        && slot_inv(drop_free(drop_reset(s))) by {
        let reset = drop_reset(s);
        assert(slot_inv(reset));
        assert(slot_inv(drop_free(reset)));
    }
}

} // verus!
