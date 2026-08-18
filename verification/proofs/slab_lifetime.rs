// Verus mirror of the slab allocator's object lifecycle
// (`mm::slab::allocator::SlabAllocator<SIZE>`, `mm::slab::KernelSlab`),
// machine-checking two framekernel invariants:
//
//   (Inv. 9)  A `HeapSlot` must not outlive its parent `Slab`: a slab page
//             may not be returned to the buddy allocator while any of its
//             cells are still outstanding.
//
//   (Inv. 10) An object is created from a `HeapSlot` only if the slot meets
//             the object's size and alignment requirements — enforced by
//             `KernelSlab::class_of` plus the slab's 16-byte cell alignment.

use vstd::prelude::*;

verus! {

/// Abstract image of one class's slab page plus the allocator's view of how
/// many of its cells are in flight.
pub struct SlabState {
    /// The page is allocated from the buddy and linked on the class list.
    pub live: bool,
    /// Total cells the page carves out of its 4 KiB body.
    pub capacity: nat,
    /// Cells currently sitting on the page's intrusive free-list.
    pub free: nat,
    /// Cells handed to callers (`alloc_one`) and not yet returned
    /// (`dealloc_one`).
    pub outstanding: nat,
    /// The class object size (`SIZE` const generic).
    pub object_size: nat,
    /// The cell alignment the slab guarantees (16).
    pub object_align: nat,
}

/// The inductive slab invariant, preserved by every `Step`.
pub open spec fn slab_inv(s: SlabState) -> bool {
    &&& s.capacity == s.free + s.outstanding
    // (Inv. 9) A handed-out cell pins the page to buddy-backed memory.
    &&& (s.outstanding > 0 ==> s.live)
    &&& (s.free > 0 ==> s.live)
    &&& (!s.live ==> s.outstanding == 0)
    &&& (!s.live ==> s.free == 0)
    &&& (!s.live ==> s.capacity == 0)
}

/// A fresh class with no slab page grown yet. Mirrors `SlabClassState::new`.
pub open spec fn slab_init(s: SlabState) -> bool {
    &&& s.live == false
    &&& s.capacity == 0
    &&& s.free == 0
    &&& s.outstanding == 0
}

/// One critical section against the class's slab page.
pub enum Step {
    /// `SlabAllocator::grow_one`. The model carries a single page per class
    /// epoch, so this fires only when no page is live.
    Grow { cap: nat, size: nat, align: nat },
    /// `SlabAllocator::alloc_one` slow path: pop one cell off the page
    /// free-list.
    Alloc,
    /// `SlabAllocator::dealloc_one`: push one cell back onto the page
    /// free-list.
    Dealloc,
    /// Hypothetical page reclaim (`free_kernel_page`), guarded on
    /// `outstanding == 0`. SlopOS's steady-state path never fires it —
    /// slab pages stay linked for the kernel's life.
    Reclaim,
}

/// Post-state after applying `t` to `s`; each arm mirrors one critical section.
pub open spec fn step(s: SlabState, t: Step) -> SlabState {
    match t {
        Step::Grow { cap, size, align } =>
            if !s.live && s.capacity == 0 {
                SlabState {
                    live: true,
                    capacity: cap,
                    free: cap,
                    outstanding: 0,
                    object_size: size,
                    object_align: align,
                }
            } else {
                s
            },
        Step::Alloc =>
            if s.live && s.free > 0 {
                SlabState { free: (s.free - 1) as nat, outstanding: (s.outstanding + 1) as nat, ..s }
            } else {
                s
            },
        Step::Dealloc =>
            if s.outstanding > 0 {
                SlabState { free: (s.free + 1) as nat, outstanding: (s.outstanding - 1) as nat, ..s }
            } else {
                s
            },
        Step::Reclaim =>
            if s.live && s.outstanding == 0 {
                SlabState { live: false, capacity: 0, free: 0, outstanding: 0, ..s }
            } else {
                s
            },
    }
}

/// Every `Step` preserves `slab_inv`. Each step is the image of one critical
/// section, so this inductive fact covers every concurrent interleaving.
pub proof fn step_preserves(s: SlabState, t: Step)
    requires
        slab_inv(s),
    ensures
        slab_inv(step(s, t)),
{
}

/// The fresh-class state satisfies the invariant — base case.
pub proof fn init_inv(s: SlabState)
    requires
        slab_init(s),
    ensures
        slab_inv(s),
{
}

/// Replay a finite trace — any interleaving of slab operations across CPUs.
pub open spec fn run(s: SlabState, trace: Seq<Step>) -> SlabState
    decreases trace.len(),
{
    if trace.len() == 0 {
        s
    } else {
        step(run(s, trace.drop_last()), trace.last())
    }
}

/// Main theorem: from a fresh class, `slab_inv` holds after any trace —
/// Inv. 9 over every execution.
pub proof fn invariant_holds_on_every_trace(s0: SlabState, trace: Seq<Step>)
    requires
        slab_init(s0),
    ensures
        slab_inv(run(s0, trace)),
    decreases trace.len(),
{
    if trace.len() == 0 {
        init_inv(s0);
    } else {
        invariant_holds_on_every_trace(s0, trace.drop_last());
        step_preserves(run(s0, trace.drop_last()), trace.last());
    }
}

/// (Inv. 9) "A `HeapSlot` must not outlive its parent `Slab`." In every
/// reachable state, an outstanding cell implies its page is still live.
pub proof fn inv9_outstanding_implies_live(s0: SlabState, trace: Seq<Step>)
    requires
        slab_init(s0),
    ensures
        run(s0, trace).outstanding > 0 ==> run(s0, trace).live,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (Inv. 9, contrapositive) A reclaimed page has zero outstanding cells.
pub proof fn inv9_dead_slab_has_no_slots(s0: SlabState, trace: Seq<Step>)
    requires
        slab_init(s0),
    ensures
        !run(s0, trace).live ==> run(s0, trace).outstanding == 0,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (Inv. 9, guard) `Reclaim` is a no-op while a cell is outstanding.
pub proof fn inv9_no_reclaim_with_outstanding(s: SlabState)
    requires
        slab_inv(s),
        s.outstanding > 0,
    ensures
        step(s, Step::Reclaim) == s,
        step(s, Step::Reclaim).live == s.live,
        step(s, Step::Reclaim).outstanding == s.outstanding,
{
}

/// A *broken* reclaim — `free_kernel_page` with no `outstanding == 0` check:
/// it marks the page dead but leaves the outstanding cells stranded.
pub open spec fn broken_reclaim(s: SlabState) -> SlabState {
    SlabState { live: false, capacity: 0, free: 0, ..s }
}

/// Inv. 9 genuinely depends on the `outstanding == 0` guard: the broken
/// reclaim reaches a state violating `slab_inv`, the fixed one never does.
pub proof fn broken_reclaim_violates_invariant()
    ensures
        exists|s: SlabState|
            #![trigger broken_reclaim(s)]
            slab_inv(s) && !slab_inv(broken_reclaim(s)),
        forall|s: SlabState| slab_inv(s) ==> #[trigger] slab_inv(step(s, Step::Reclaim)),
{
    // Reachable by Grow then Alloc.
    let busy = SlabState {
        live: true,
        capacity: 4,
        free: 3,
        outstanding: 1,
        object_size: 64,
        object_align: 16,
    };
    assert(slab_inv(busy));
    let stranded = broken_reclaim(busy);
    assert(stranded.live == false);
    assert(stranded.outstanding == 1);
    assert(!slab_inv(stranded));
    assert(slab_inv(busy) && !slab_inv(broken_reclaim(busy)));
    assert(exists|s: SlabState| #![trigger broken_reclaim(s)] slab_inv(s) && !slab_inv(broken_reclaim(s)));
    assert forall|s: SlabState| slab_inv(s) implies #[trigger] slab_inv(step(s, Step::Reclaim)) by {
        step_preserves(s, Step::Reclaim);
    }
}

/// The largest slab class (`mm::slab::SIZE_CLASSES` = 16..=2048); strictly
/// larger requests route through the large-alloc tier, out of scope here.
pub open spec fn max_slab_class() -> nat {
    2048
}

/// The size-class chooser: mirrors `KernelSlab::class_of`, returning the
/// smallest class `>= req` after the 16-byte round-up.
pub open spec fn class_size(req: nat) -> nat {
    if req <= 16 {
        16
    } else if req <= 32 {
        32
    } else if req <= 64 {
        64
    } else if req <= 128 {
        128
    } else if req <= 256 {
        256
    } else if req <= 512 {
        512
    } else if req <= 1024 {
        1024
    } else if req <= 2048 {
        2048
    } else {
        // Out of slab range: routes to the large-alloc tier.
        0
    }
}

/// The cell alignment the slab guarantees: cells are carved at 16-byte
/// boundaries from the page body and every class size is a multiple of 16.
pub open spec fn slab_align() -> nat {
    16
}

/// Abstract image of a `HeapSlot`: the cell handed to a caller.
pub struct HeapSlotState {
    /// The cell's usable size (== the class `object_size`).
    pub size: nat,
    /// The cell's alignment (== `slab_align`).
    pub align: nat,
}

/// Models OSTD's size/align fit gate: `HeapSlot::into_box::<T>(val)` is
/// permitted exactly when the cell is big enough and aligned enough for `T`.
pub open spec fn into_box_ok(slot: HeapSlotState, t_size: nat, t_align: nat) -> bool {
    &&& slot.size >= t_size
    &&& slot.align >= t_align
}

/// The size half of Inv. 10: the chooser always returns a class at least as
/// large as the request.
pub proof fn class_size_covers(req: nat)
    requires
        1 <= req <= max_slab_class(),
    ensures
        req <= class_size(req),
{
}

/// The cell a slab class hands out for a `T`-sized request.
pub open spec fn slot_for(t_size: nat) -> HeapSlotState {
    HeapSlotState { size: class_size(t_size), align: slab_align() }
}

/// (Inv. 10) "An object is created from a `HeapSlot` only if the slot meets
/// the object's size and alignment requirements." Types demanding
/// `align > 16` route through the over-allocating cookie path in
/// `mm::heap::KernelHeap::alloc`, out of this proof's scope.
pub proof fn inv10_into_box_fits(t_size: nat, t_align: nat)
    requires
        1 <= t_size <= max_slab_class(),
        t_align <= slab_align(),
    ensures
        into_box_ok(slot_for(t_size), t_size, t_align),
{
    class_size_covers(t_size);
}

/// Inv. 10 depends on the scan returning a class `>= req`: a chooser that
/// always picked the 16-byte class would let a caller build a 2048-byte
/// object in a 16-byte cell, overflowing into neighbouring slots.
pub proof fn undersized_class_violates_inv10()
    ensures
        exists|t_size: nat|
            1 <= t_size <= max_slab_class()
                && !into_box_ok(HeapSlotState { size: 16, align: 16 }, t_size, 0),
        forall|t_size: nat|
            (1 <= t_size <= max_slab_class()) ==> #[trigger] into_box_ok(slot_for(t_size), t_size, 0),
{
    let big: nat = 2048;
    assert(!into_box_ok(HeapSlotState { size: 16, align: 16 }, big, 0));
    assert(exists|t_size: nat|
        1 <= t_size <= max_slab_class()
            && !into_box_ok(HeapSlotState { size: 16, align: 16 }, t_size, 0));
    assert forall|t_size: nat| (1 <= t_size <= max_slab_class())
        implies #[trigger] into_box_ok(slot_for(t_size), t_size, 0) by {
        class_size_covers(t_size);
    }
}

} // verus!
