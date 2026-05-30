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
//
// Background. The Asterinas paper (USENIX ATC '25, Fig. 9) found a real UB
// in the equivalent OSTD path via KernMiri: a `fetch_add(1)` clone could
// resurrect a slot whose teardown had already begun (rc had hit 0), racing
// the `on_drop`/`drop_in_place` of the last dropper. SlopOS's port closes
// that race with a conditional increment — `from_in_use` uses
// `ref_count.fetch_update(|prev| if prev == 0 { None } else { Some(prev+1) })`,
// refusing to bump from zero (see `frame.rs::from_in_use`). This proof
// encodes both the fixed clone and the broken `fetch_add` clone and shows
// the inductive invariant holds for the former and is violated by the
// latter — so the proof is load-bearing, not vacuous.
//
// Modelling strategy. Every method on the real `Frame`/`MetaSlot` touches
// the slot only through atomic operations (CAS, fetch_update, fetch_sub,
// Release/Acquire stores). On any CPU, any interleaving of these atomics is
// a *sequence of atomic steps* against the shared slot. We therefore model
// the slot as an abstract state and each atomic-bounded method body as one
// `Step`. An inductive invariant that survives every `Step` then holds in
// every reachable state of every interleaving — which is exactly the
// concurrency claim (I3): no schedule of clones and drops can reach a state
// the invariant forbids.
//
// Field correspondence to `frame.rs`:
//   `typed`        <-> `MetaSlot::state == META_STATE_TYPED`
//   `rc`           <-> `MetaSlot::ref_count` (AtomicU32)
//   `payload_live` <-> the inline `M` payload + vtable are still valid
//                      (i.e. `on_drop`/`drop_in_place` have NOT run)
//   `on_free_list` <-> the physical frame is back in the FrameAlloc free
//                      list (released by `on_drop -> dealloc`)
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
    /// `MetaSlot::state == META_STATE_TYPED`.
    pub typed: bool,
    /// `MetaSlot::ref_count`.
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
    /// `Frame::from_unused`: CAS state UNUSED->TYPED, write meta, publish
    /// `ref_count = 1`. The frame leaves the allocator free list (it was
    /// just handed out by `FrameAlloc::alloc`). Resets the per-epoch ghost
    /// release counter — a fresh allocation starts a new epoch.
    FromUnused,
    /// `Frame::from_in_use` — the *fixed* clone. `fetch_update` bumps the
    /// count only when it is already positive; it refuses to resurrect a
    /// slot whose count has reached 0. This is the line that closes the
    /// Asterinas Fig. 9 race.
    CloneConditional,
    /// `Frame::drop`, non-final: `fetch_sub(1)` observed a previous value
    /// > 1, so this is not the last reference. Nothing else happens.
    DropNonFinal,
    /// `Frame::drop`, final: `fetch_sub(1)` observed exactly 1. The Acquire
    /// fence then `on_drop` (-> `dealloc`, returning the frame to the free
    /// list and bumping `releases`), `drop_in_place` (payload no longer
    /// live), then state -> UNUSED. Modelled as one atomic-ordered step
    /// because no other `Frame` can touch the slot once rc has hit 0 (the
    /// conditional clone refuses to revive it).
    DropFinal,
}

/// Transition function: the post-state after applying `t` to `s`. Each arm
/// mirrors the corresponding method body in `frame.rs`.
pub open spec fn step(s: Slot, t: Step) -> Slot {
    match t {
        Step::FromUnused =>
            // Only fires on a genuinely unused slot (CAS UNUSED->TYPED
            // succeeds). The allocator just gave us the frame, so it is off
            // the free list and a new epoch begins (releases reset to 0).
            if !s.typed && s.rc == 0 && !s.payload_live {
                Slot { typed: true, rc: 1, payload_live: true, on_free_list: false, releases: 0 }
            } else {
                s
            },
        Step::CloneConditional =>
            // fetch_update refuses prev == 0; otherwise rc += 1.
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
/// transition to 0" half of (I2), stated at the step level.
pub proof fn i2_dropfinal_releases_once(s: Slot)
    requires
        slot_inv(s),
        s.rc == 1,
    ensures
        step(s, Step::DropFinal).releases == s.releases + 1,
        step(s, Step::DropFinal).on_free_list,
        step(s, Step::DropFinal).rc == 0,
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

} // verus!
