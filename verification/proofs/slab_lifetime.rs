// Slab / `HeapSlot` lifetime proof.
//
// This is a Verus-annotated mirror of the slab allocator's object
// lifecycle — the kernel-side `mm::slab::allocator::SlabAllocator<SIZE>`
// (per-class slab pages, intrusive free-lists) plus the size-class
// dispatch in `mm::slab::KernelSlab` and OSTD's `mm::slab::Slab` trait.
// It machine-checks the two framekernel soundness invariants that govern
// slab-derived objects:
//
//   (Inv. 9)  A `HeapSlot` — or any object derived from it — must not
//             outlive its parent `Slab`. Concretely: a cell handed out by
//             `SlabAllocator::alloc_one` lives inside a slab page, and that
//             page may not be returned to the buddy allocator while any of
//             its cells are still outstanding. A reclaimed (dead) slab has
//             zero outstanding cells.
//
//   (Inv. 10) An object is created from a `HeapSlot` only if the slot meets
//             the object's size and alignment requirements. Concretely:
//             `KernelSlab::alloc` rounds the request up to 16 and selects
//             the smallest size class whose `object_size >= request`, so the
//             cell it returns is always at least as large (and at least as
//             aligned) as the type the caller constructs in it.
//
// Background. Inv. 9 is the slab analogue of the `Frame<M>` use-after-free
// closed in `frame_refcount.rs`: a slot outliving its page is a dangling
// pointer into memory the buddy allocator may have recycled. SlopOS's slab tier closes
// it structurally — slab pages, once grown, stay linked on the class's
// partial list for the lifetime of the kernel and are *never* freed back to
// the buddy (see `allocator.rs`: empty slabs stay linked; there is no
// `free_kernel_page` on the steady-state free path). This proof models the
// stronger, general rule — a page may be reclaimed, but only with zero
// outstanding cells — and shows that the guard is load-bearing: a "broken
// reclaim" that frees a page with live cells violates the invariant.
//
// Inv. 10 is enforced by `KernelSlab::class_of` + the 16-byte round-up in
// `KernelSlab::alloc` (the size half) and the slab's 16-byte cell alignment
// (the alignment half; layouts demanding `align > 16` route through the
// cookie path in `mm::heap::KernelHeap::alloc`, already SAFETY-noted there).
//
// Modelling strategy. As in `frame_refcount.rs`, every slab mutation is a
// short critical section under the class `SpinLock` (or an atomic magazine
// op). We model the class's slab page as an abstract `SlabState` and each
// critical section as one `Step`. An inductive invariant that survives
// every `Step` then holds in every reachable state of every interleaving of
// grow / alloc / dealloc / reclaim calls — exactly the Inv. 9 concurrency
// claim. Inv. 10 is a pure size/align fact about the size-class chooser and
// is proved directly.
//
// Field correspondence:
//   `live`         <-> the slab page is allocated from the buddy and linked
//                      on the class's partial list (header magic == SLAB_MAGIC)
//   `capacity`     <-> `SlabHeader::total_count` (cells the page holds)
//   `free`         <-> `SlabHeader::free_count` (cells on the page free-list)
//   `outstanding`  <-> cells handed to callers and not yet returned
//   `object_size`  <-> `SlabHeader::object_size` == the class `SIZE`
//   `object_align` <-> the slab's cell alignment (16)

use vstd::prelude::*;

verus! {

// ===========================================================================
// Part 1 — Inv. 9: a HeapSlot cannot outlive its parent Slab.
// ===========================================================================

/// Abstract image of one class's slab page plus the allocator's view of how
/// many of its cells are in flight. `outstanding` is the count a
/// use-after-free would strand on a freed page; `live` is whether the page
/// is still backed by buddy memory.
pub struct SlabState {
    /// The page is allocated from the buddy and linked on the class list.
    pub live: bool,
    /// Total cells the page carves out of its 4 KiB body.
    pub capacity: nat,
    /// Cells currently sitting on the page's intrusive free-list.
    pub free: nat,
    /// Cells handed to callers (`alloc_one`) and not yet returned
    /// (`dealloc_one`). The quantity Inv. 9 protects.
    pub outstanding: nat,
    /// The class object size (`SIZE` const generic).
    pub object_size: nat,
    /// The cell alignment the slab guarantees (16).
    pub object_align: nat,
}

/// The inductive slab invariant. Every reachable slab state satisfies it;
/// each `Step` preserves it (`step_preserves` below).
pub open spec fn slab_inv(s: SlabState) -> bool {
    // Conservation: every cell is either free or outstanding.
    &&& s.capacity == s.free + s.outstanding
    // (Inv. 9) A handed-out cell pins the page: if any cell is outstanding,
    //          the page is still backed by buddy memory. This is the core
    //          "slot cannot outlive its slab" guarantee.
    &&& (s.outstanding > 0 ==> s.live)
    // A cell can only be on the free-list of a live page.
    &&& (s.free > 0 ==> s.live)
    // A reclaimed (dead) page holds no cells at all — nothing dangling.
    &&& (!s.live ==> s.outstanding == 0)
    &&& (!s.live ==> s.free == 0)
    &&& (!s.live ==> s.capacity == 0)
}

/// A class starts with no slab page grown yet: not live, no cells. Mirrors
/// `SlabClassState::new` (a null `RawLink` head — no pages).
pub open spec fn slab_init(s: SlabState) -> bool {
    &&& s.live == false
    &&& s.capacity == 0
    &&& s.free == 0
    &&& s.outstanding == 0
}

/// One critical section against the class's slab page.
pub enum Step {
    /// `SlabAllocator::grow_one`: claim a fresh page from the buddy, stamp
    /// the header, build the free-list. Fires only when no page is live
    /// (capacity 0) — the abstract model carries a single page per class
    /// epoch; a reclaimed page can be regrown.
    Grow { cap: nat, size: nat, align: nat },
    /// `SlabAllocator::alloc_one` slow path: pop one cell off the page
    /// free-list. Requires a live page with a free cell.
    Alloc,
    /// `SlabAllocator::dealloc_one`: push one cell back onto the page
    /// free-list. Requires an outstanding cell.
    Dealloc,
    /// Hypothetical page reclaim (`free_kernel_page`): return the page to
    /// the buddy. The *fixed* guard refuses unless every cell has been
    /// returned (`outstanding == 0`). SlopOS's steady-state path never
    /// fires this — pages stay linked for the kernel's life — but the model
    /// proves the guard is what Inv. 9 leans on.
    Reclaim,
}

/// Transition function: post-state after applying `t` to `s`. Each arm
/// mirrors the corresponding critical section.
pub open spec fn step(s: SlabState, t: Step) -> SlabState {
    match t {
        Step::Grow { cap, size, align } =>
            // Only grows when no page is currently live (fresh class, or a
            // reclaimed epoch). All `cap` cells start free.
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
            // Pop a free cell: one fewer free, one more outstanding.
            if s.live && s.free > 0 {
                SlabState { free: (s.free - 1) as nat, outstanding: (s.outstanding + 1) as nat, ..s }
            } else {
                s
            },
        Step::Dealloc =>
            // Return a cell: one more free, one fewer outstanding.
            if s.outstanding > 0 {
                SlabState { free: (s.free + 1) as nat, outstanding: (s.outstanding - 1) as nat, ..s }
            } else {
                s
            },
        Step::Reclaim =>
            // Free the page to the buddy — ONLY with no cell outstanding.
            // This refusal is the Inv. 9 guard.
            if s.live && s.outstanding == 0 {
                SlabState { live: false, capacity: 0, free: 0, outstanding: 0, ..s }
            } else {
                s
            },
    }
}

/// Every `Step` preserves `slab_inv`. Because each step is the image of a
/// single critical section and any concurrent interleaving is a sequence of
/// such steps, this one inductive fact is the whole-system Inv. 9 guarantee:
/// no schedule of grow/alloc/dealloc/reclaim reaches a state with a cell
/// outstanding on a dead page.
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

/// Replay a finite trace of steps from a start state. A trace is any finite
/// interleaving of grow/alloc/dealloc/reclaim calls from any number of CPUs.
pub open spec fn run(s: SlabState, trace: Seq<Step>) -> SlabState
    decreases trace.len(),
{
    if trace.len() == 0 {
        s
    } else {
        step(run(s, trace.drop_last()), trace.last())
    }
}

/// MAIN THEOREM. From a fresh class, after *any* trace of slab operations
/// (any concurrent interleaving), the invariant still holds. The
/// machine-checked statement of Inv. 9 over all executions.
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

// ---------------------------------------------------------------------------
// Named Inv. 9 corollaries.
// ---------------------------------------------------------------------------

/// (Inv. 9) "A `HeapSlot` must not outlive its parent `Slab`." In every
/// reachable state, an outstanding cell implies its page is still live — so
/// no cell handle ever points into a page the buddy has reclaimed.
pub proof fn inv9_outstanding_implies_live(s0: SlabState, trace: Seq<Step>)
    requires
        slab_init(s0),
    ensures
        run(s0, trace).outstanding > 0 ==> run(s0, trace).live,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (Inv. 9, contrapositive) A dead (reclaimed) page has zero outstanding
/// cells: once a slab is returned to the buddy, nothing derived from it
/// survives.
pub proof fn inv9_dead_slab_has_no_slots(s0: SlabState, trace: Seq<Step>)
    requires
        slab_init(s0),
    ensures
        !run(s0, trace).live ==> run(s0, trace).outstanding == 0,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (Inv. 9, guard) `Reclaim` cannot retire a page while a cell is
/// outstanding — the transition is a no-op in that case, stated at the step
/// level. This is the line the whole invariant leans on.
pub proof fn inv9_no_reclaim_with_outstanding(s: SlabState)
    requires
        slab_inv(s),
        s.outstanding > 0,
    ensures
        // The page survives the reclaim attempt unchanged.
        step(s, Step::Reclaim) == s,
        step(s, Step::Reclaim).live == s.live,
        step(s, Step::Reclaim).outstanding == s.outstanding,
{
}

// ---------------------------------------------------------------------------
// The guard is load-bearing: an unconditional reclaim breaks Inv. 9.
// ---------------------------------------------------------------------------

/// A *broken* reclaim that returns the page to the buddy while ignoring
/// outstanding cells — i.e. `free_kernel_page` with no `outstanding == 0`
/// check. It marks the page dead but leaves the outstanding cells stranded.
pub open spec fn broken_reclaim(s: SlabState) -> SlabState {
    SlabState { live: false, capacity: 0, free: 0, ..s }
}

/// Witness that the reclaim guard is not redundant. Take a reachable state
/// with a cell outstanding on a live page (grow, then alloc). The broken
/// reclaim retires the page while the cell is still live, landing in a state
/// that violates `slab_inv` (a cell outstanding on a dead page — a dangling
/// pointer into buddy-reclaimed memory). The fixed `Reclaim` refuses and
/// preserves the invariant on every state. This proves Inv. 9 genuinely
/// depends on the `outstanding == 0` guard.
pub proof fn broken_reclaim_violates_invariant()
    ensures
        // There is a reachable, invariant-satisfying state on which the
        // broken reclaim produces an invariant-violating state...
        exists|s: SlabState|
            #![trigger broken_reclaim(s)]
            slab_inv(s) && !slab_inv(broken_reclaim(s)),
        // ...while the fixed reclaim keeps every such state invariant.
        forall|s: SlabState| slab_inv(s) ==> #[trigger] slab_inv(step(s, Step::Reclaim)),
{
    // A live page with one cell handed out (reachable: Grow then Alloc).
    let busy = SlabState {
        live: true,
        capacity: 4,
        free: 3,
        outstanding: 1,
        object_size: 64,
        object_align: 16,
    };
    assert(slab_inv(busy));
    // The broken reclaim marks the page dead but leaves the cell stranded.
    let stranded = broken_reclaim(busy);
    assert(stranded.live == false);
    assert(stranded.outstanding == 1);
    // outstanding > 0 on a dead page — violates the (Inv. 9) conjunct.
    assert(!slab_inv(stranded));
    assert(slab_inv(busy) && !slab_inv(broken_reclaim(busy)));
    assert(exists|s: SlabState| #![trigger broken_reclaim(s)] slab_inv(s) && !slab_inv(broken_reclaim(s)));
    // The fixed reclaim preserves the invariant on every state.
    assert forall|s: SlabState| slab_inv(s) implies #[trigger] slab_inv(step(s, Step::Reclaim)) by {
        step_preserves(s, Step::Reclaim);
    }
}

// ===========================================================================
// Part 2 — Inv. 10: an object is created from a slot only when the slot
// meets the object's size and alignment requirements.
// ===========================================================================

/// The eight slab size classes, in ascending order. Mirrors
/// `mm::slab::SIZE_CLASSES = [16, 32, 64, 128, 256, 512, 1024, 2048]`.
/// The largest class is `MAX_SLAB_CLASS_BYTES`; strictly larger requests
/// route through the large-alloc tier and are out of this proof's scope.
pub open spec fn max_slab_class() -> nat {
    2048
}

/// The size-class chooser: the `object_size` of the cell `KernelSlab::alloc`
/// hands back for a request of `req` bytes. Mirrors `KernelSlab::class_of`'s
/// linear scan over `SIZE_CLASSES` after the 16-byte round-up — it returns
/// the smallest class whose size is `>= req`.
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

/// The cell alignment the slab guarantees. Cells are carved at 16-byte
/// boundaries from the page body (`SlabHeader::object_start_offset` rounds
/// up to 16, and every class size is a multiple of 16).
pub open spec fn slab_align() -> nat {
    16
}

/// Abstract image of a `HeapSlot`: the cell handed to a caller, carrying the
/// size and alignment of its class.
pub struct HeapSlotState {
    /// The cell's usable size (== the class `object_size`).
    pub size: nat,
    /// The cell's alignment (== `slab_align`).
    pub align: nat,
}

/// `HeapSlot::into_box::<T>(val)` is permitted exactly when the cell is big
/// enough and aligned enough for `T`. The model of the size/align fit gate
/// in OSTD's heap path.
pub open spec fn into_box_ok(slot: HeapSlotState, t_size: nat, t_align: nat) -> bool {
    &&& slot.size >= t_size
    &&& slot.align >= t_align
}

/// The size-class chooser always returns a class at least as large as the
/// request. The size half of Inv. 10, stated over the whole slab range.
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
/// the object's size and alignment requirements." For any `T` whose size is
/// in the slab range and whose alignment is at most the slab's 16-byte
/// guarantee, the cell `KernelSlab::alloc` returns admits `into_box::<T>`:
/// it is at least `size_of::<T>` bytes and at least `align_of::<T>`-aligned.
/// (Types demanding `align > 16` route through the cookie path in
/// `mm::heap::KernelHeap::alloc`, which over-allocates to honour the larger
/// alignment — out of this slab proof's scope.)
pub proof fn inv10_into_box_fits(t_size: nat, t_align: nat)
    requires
        1 <= t_size <= max_slab_class(),
        t_align <= slab_align(),
    ensures
        into_box_ok(slot_for(t_size), t_size, t_align),
{
    class_size_covers(t_size);
}

/// The fit gate is load-bearing: a *broken* chooser that always picks the
/// smallest class (16 bytes) regardless of request would let a caller build
/// a 2048-byte object in a 16-byte cell — a heap buffer overflow straddling
/// neighbouring slots. This proves Inv. 10 genuinely depends on the
/// size-class scan returning a class `>= req`.
pub proof fn undersized_class_violates_inv10()
    ensures
        // A request the broken (always-16) chooser cannot satisfy...
        exists|t_size: nat|
            1 <= t_size <= max_slab_class()
                && !into_box_ok(HeapSlotState { size: 16, align: 16 }, t_size, 0),
        // ...while the real chooser satisfies every in-range request.
        forall|t_size: nat|
            (1 <= t_size <= max_slab_class()) ==> #[trigger] into_box_ok(slot_for(t_size), t_size, 0),
{
    // A maximal in-range request the always-16 chooser undersizes.
    let big: nat = 2048;
    assert(!into_box_ok(HeapSlotState { size: 16, align: 16 }, big, 0));
    assert(exists|t_size: nat|
        1 <= t_size <= max_slab_class()
            && !into_box_ok(HeapSlotState { size: 16, align: 16 }, t_size, 0));
    // The real chooser fits every in-range request.
    assert forall|t_size: nat| (1 <= t_size <= max_slab_class())
        implies #[trigger] into_box_ok(slot_for(t_size), t_size, 0) by {
        class_size_covers(t_size);
    }
}

} // verus!
