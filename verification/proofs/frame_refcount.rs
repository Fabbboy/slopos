// `Frame<M>` reference-count proof: a Verus mirror of the state machine in
// `slopos_ostd::mm::frame::{MetaSlot, Frame, Drop}`. It machine-checks:
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
//        races a still-TYPED slot.
//
// `MetaSlot::ref_count` (AtomicU32) carries the slot's whole lifecycle in one
// atomic, with two sentinels:
//   `REF_COUNT_UNUSED` (u32::MAX) — free and claimable by `from_unused`.
//   `REF_COUNT_BUSY`   (0)        — transient: a `from_unused` or a `Drop`
//                                   owns the slot exclusively to construct
//                                   or tear down the metadata.
//   `1..=REF_COUNT_MAX`           — that many live `Frame` handles.
// Both construction and destruction park the slot at BUSY, where `from_unused`
// retries and `from_in_use` refuses. That interlock is what makes each method
// body one atomic-bounded `Step`, so an inductive invariant over `Step` holds
// in every reachable state of every interleaving — the concurrency claim (I3).

use vstd::prelude::*;

verus! {

/// Abstract image of a single `MetaSlot` plus the allocator's view of the
/// physical frame it tracks.
pub struct Slot {
    /// The slot is live: `MetaSlot::ref_count` in `1..=REF_COUNT_MAX`.
    pub typed: bool,
    /// The live reference count (`0` models the UNUSED/BUSY sentinels).
    pub rc: nat,
    /// The inline `M` and vtable are still valid — `drop_in_place` has not
    /// run. A live `Frame` borrow dereferences memory whose validity is
    /// exactly this bit.
    pub payload_live: bool,
    /// The physical frame is in the FrameAlloc free list — a future `alloc`
    /// may hand it out again.
    pub on_free_list: bool,
    /// Ghost: number of times this epoch's frame was returned to the
    /// allocator. Behind (I2)'s "exactly once".
    pub releases: nat,
}

/// The inductive slot invariant. Every reachable slot state satisfies it;
/// each `Step` preserves it (`step_preserves` below).
pub open spec fn slot_inv(s: Slot) -> bool {
    // (I1) A positive ref-count pins the frame: typed, payload valid, not on
    //      the allocator free list.
    &&& (s.rc > 0 ==> s.typed)
    &&& (s.rc > 0 ==> s.payload_live)
    &&& (s.rc > 0 ==> !s.on_free_list)
    // (I3) No use-after-free: the bytes a `Frame::borrow` reads are never
    //      memory the allocator considers reusable.
    &&& !(s.payload_live && s.on_free_list)
    // (I4) Reclaim-after-reset: a frame on the free list has a claimable
    //      (non-TYPED) slot, so a concurrent `from_unused` handed the recycled
    //      paddr never finds it still TYPED (a spurious `StateMismatch` →
    //      `PathCorrupt`).
    &&& (s.on_free_list ==> !s.typed)
    // (I2) Exactly-once release: `on_free_list` is reached only via that one
    //      release, so a second `dealloc` is unreachable.
    &&& (s.payload_live ==> s.releases == 0)
    &&& (s.releases <= 1)
    &&& (s.on_free_list ==> s.releases == 1)
    // Drop zeroes rc before clearing `payload_live`.
    &&& (s.typed && !s.payload_live ==> s.rc == 0)
}

/// The state a fresh, never-allocated slot starts in — `META_SLOTS` is
/// zero-initialised at boot. `on_free_list` is the FrameAlloc's *released*-
/// frame predicate, so a slot that has never completed an alloc/dealloc round
/// trip starts `false` and only becomes `true` via the single `DropFinal`
/// release; that is what makes `releases` a faithful double-free counter.
pub open spec fn slot_init(s: Slot) -> bool {
    &&& s.typed == false
    &&& s.rc == 0
    &&& s.payload_live == false
    &&& s.on_free_list == false
    &&& s.releases == 0
}

/// One step per atomic-bounded `MetaSlot` method body.
pub enum Step {
    /// `Frame::from_unused`: CAS UNUSED->BUSY, write meta, publish
    /// `ref_count = 1`. The frame leaves the allocator free list, and the
    /// per-epoch ghost release counter resets — a fresh allocation is a new
    /// epoch.
    FromUnused,
    /// `Frame::from_in_use` — the *fixed* clone. `fetch_update` bumps only
    /// from a live value; refusing BUSY/UNUSED is what stops it resurrecting
    /// a slot whose last ref just hit BUSY.
    CloneConditional,
    /// `Frame::drop`, non-final: `fetch_sub(1)` observed a previous value
    /// > 1, so this is not the last reference.
    DropNonFinal,
    /// `Frame::drop`, final: `fetch_sub(1)` observed exactly 1, landing the
    /// slot at BUSY. Acquire fence, `drop_in_place`, slot published UNUSED,
    /// then — *last* — the page returned to the free list. Modelled as one
    /// step: the BUSY interlock excludes every peer, and the only sub-step a
    /// peer can observe (the page appearing on the free list) happens after
    /// the slot is already UNUSED. That reset-before-free ordering is what
    /// makes the atomic model sound — see
    /// `broken_drop_ordering_violates_invariant`.
    DropFinal,
    /// `Frame::drop`, final, for a meta whose `returns_frame_on_last_drop()`
    /// is `false` (the statically-borrowed kernel-master PML4 and
    /// externally-owned DMA segment frames): same teardown, but the page is
    /// not handed back, so `on_free_list` and `releases` are untouched.
    DropFinalNoFree,
}

/// Transition function: the post-state after applying `t` to `s`. Each arm
/// mirrors the corresponding method body in `frame.rs`.
pub open spec fn step(s: Slot, t: Step) -> Slot {
    match t {
        Step::FromUnused =>
            if !s.typed && s.rc == 0 && !s.payload_live {
                Slot { typed: true, rc: 1, payload_live: true, on_free_list: false, releases: 0 }
            } else {
                s
            },
        Step::CloneConditional =>
            if s.rc > 0 {
                Slot { rc: (s.rc + 1) as nat, ..s }
            } else {
                s
            },
        Step::DropNonFinal =>
            if s.rc > 1 {
                Slot { rc: (s.rc - 1) as nat, ..s }
            } else {
                s
            },
        Step::DropFinal =>
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
            if s.rc == 1 {
                Slot { typed: false, rc: 0, payload_live: false, ..s }
            } else {
                s
            },
    }
}

/// Every `Step` preserves `slot_inv`. Each step is the image of an
/// atomic-bounded method body and any interleaving is a sequence of such steps
/// against the shared slot, so this single inductive fact is the whole-system
/// concurrency guarantee.
pub proof fn step_preserves(s: Slot, t: Step)
    requires
        slot_inv(s),
    ensures
        slot_inv(step(s, t)),
{
}

/// The initial slot state satisfies the invariant — base case.
pub proof fn init_inv(s: Slot)
    requires
        slot_init(s),
    ensures
        slot_inv(s),
{
}

/// Replay a finite trace of steps from a start state: any interleaving of
/// `from_unused` / `from_in_use` / `drop` calls from any number of CPUs, since
/// each leaves the shared slot only through these atomic-bounded transitions.
pub open spec fn run(s: Slot, trace: Seq<Step>) -> Slot
    decreases trace.len(),
{
    if trace.len() == 0 {
        s
    } else {
        step(run(s, trace.drop_last()), trace.last())
    }
}

/// MAIN THEOREM. From any initial slot, after *any* trace of clone/drop steps
/// (any concurrent interleaving), the invariant still holds.
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

/// (I1) In any reachable state, a positive ref-count implies the frame is
/// typed (allocated) and absent from the allocator's free list.
pub proof fn i1_positive_rc_is_allocated(s0: Slot, trace: Seq<Step>)
    requires
        slot_init(s0),
    ensures
        run(s0, trace).rc > 0 ==> run(s0, trace).typed && !run(s0, trace).on_free_list,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (I2) A reachable state never exceeds one release, and being on the free
/// list witnesses exactly one — so no double-free.
pub proof fn i2_release_at_most_once(s0: Slot, trace: Seq<Step>)
    requires
        slot_init(s0),
    ensures
        run(s0, trace).releases <= 1,
        run(s0, trace).on_free_list ==> run(s0, trace).releases == 1,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (I2, step level) `DropFinal` is the only step that releases, and it bumps
/// `releases` by exactly one.
pub proof fn i2_dropfinal_releases_once(s: Slot)
    requires
        slot_inv(s),
        s.rc == 1,
    ensures
        step(s, Step::DropFinal).releases == s.releases + 1,
        step(s, Step::DropFinal).on_free_list,
        step(s, Step::DropFinal).rc == 0,
        step(s, Step::DropFinalNoFree).rc == 0,
        step(s, Step::DropFinalNoFree).releases == s.releases,
        !step(s, Step::DropFinalNoFree).on_free_list,
        step(s, Step::CloneConditional).releases == s.releases,
        step(s, Step::DropNonFinal).releases == s.releases,
        step(s, Step::FromUnused).releases == s.releases,
{
}

/// (I3) In every reachable state a valid payload and free-list membership are
/// mutually exclusive: no live `Frame::borrow` ever reads memory the allocator
/// has reclaimed.
pub proof fn i3_no_use_after_free(s0: Slot, trace: Seq<Step>)
    requires
        slot_init(s0),
    ensures
        !(run(s0, trace).payload_live && run(s0, trace).on_free_list),
{
    invariant_holds_on_every_trace(s0, trace);
}

/// The Asterinas Fig. 9 bug (USENIX ATC '25), modelled: an *unconditional*
/// `fetch_add(1)` clone that bumps the ref-count even from 0 — what
/// `from_in_use` would be without the conditional `fetch_update`.
pub open spec fn broken_clone(s: Slot) -> Slot {
    Slot { rc: (s.rc + 1) as nat, ..s }
}

/// Witness that the conditional clone is *not* redundant, so this proof is not
/// vacuous. From a teardown-complete slot (`step(_, DropFinal)`'s own output)
/// the broken clone revives rc to 1 on a freed frame with a dead payload,
/// violating (I1); the fixed `CloneConditional` refuses the bump.
pub proof fn broken_clone_violates_invariant()
    ensures
        exists|s: Slot|
            #![trigger broken_clone(s)]
            slot_inv(s) && !slot_inv(broken_clone(s)),
        forall|s: Slot| slot_inv(s) ==> #[trigger] slot_inv(step(s, Step::CloneConditional)),
{
    let torn = Slot { typed: false, rc: 0, payload_live: false, on_free_list: true, releases: 1 };
    assert(slot_inv(torn));
    let revived = broken_clone(torn);
    assert(revived.rc == 1);
    assert(revived.on_free_list);
    // rc > 0 yet on_free_list — violates the (I1) conjunct of slot_inv.
    assert(!slot_inv(revived));
    assert(slot_inv(torn) && !slot_inv(broken_clone(torn)));
    assert(exists|s: Slot| #![trigger broken_clone(s)] slot_inv(s) && !slot_inv(broken_clone(s)));
    assert forall|s: Slot| slot_inv(s) implies #[trigger] slot_inv(step(s, Step::CloneConditional)) by {
        step_preserves(s, Step::CloneConditional);
    }
}

/// FIXED ordering, sub-step 1: payload dropped and slot published UNUSED. The
/// page is not yet on the free list — still owned by the dropper — so no
/// `from_unused` for this paddr can fire here.
pub open spec fn drop_reset(s: Slot) -> Slot {
    Slot { typed: false, rc: 0, payload_live: false, on_free_list: false, releases: 0 }
}

/// FIXED ordering, sub-step 2: hand the page back to the allocator. The slot
/// is already UNUSED, so the recycled paddr is immediately claimable.
pub open spec fn drop_free(s: Slot) -> Slot {
    Slot { on_free_list: true, releases: (s.releases + 1) as nat, ..s }
}

/// BROKEN ordering (rejected): the page goes back to the allocator while the
/// slot is still non-UNUSED — the window in which a concurrent `from_unused`
/// would be handed a paddr whose slot is not yet claimable.
pub open spec fn broken_drop_free(s: Slot) -> Slot {
    Slot { rc: 0, payload_live: false, on_free_list: true, releases: (s.releases + 1) as nat, ..s }
}

/// Witness that the teardown ordering is load-bearing. From a live,
/// singly-referenced slot the free-before-reset ordering reaches an
/// intermediate violating (I4) — a page on the free list whose slot is still
/// TYPED, exactly the state in which a concurrent `from_unused` on the
/// recycled paddr fails its CAS → `StateMismatch` → `PathCorrupt`. The fixed
/// reset-before-free ordering keeps every sub-state invariant.
pub proof fn broken_drop_ordering_violates_invariant()
    ensures
        exists|s: Slot|
            #![trigger broken_drop_free(s)]
            slot_inv(s) && s.rc == 1 && !slot_inv(broken_drop_free(s)),
        forall|s: Slot|
            (slot_inv(s) && s.rc == 1) ==> #[trigger] slot_inv(drop_reset(s))
                && slot_inv(drop_free(drop_reset(s))),
{
    // A live slot holding its last reference — the instant before `Drop`.
    let live = Slot { typed: true, rc: 1, payload_live: true, on_free_list: false, releases: 0 };
    assert(slot_inv(live));
    let broken = broken_drop_free(live);
    assert(broken.on_free_list && broken.typed);
    // on_free_list yet typed — violates the (I4) conjunct of slot_inv.
    assert(!slot_inv(broken));
    assert(slot_inv(live) && live.rc == 1 && !slot_inv(broken_drop_free(live)));
    assert(exists|s: Slot|
        #![trigger broken_drop_free(s)]
        slot_inv(s) && s.rc == 1 && !slot_inv(broken_drop_free(s)));
    assert forall|s: Slot| (slot_inv(s) && s.rc == 1) implies #[trigger] slot_inv(drop_reset(s))
        && slot_inv(drop_free(drop_reset(s))) by {
        let reset = drop_reset(s);
        assert(slot_inv(reset));
        assert(slot_inv(drop_free(reset)));
    }
}

} // verus!
