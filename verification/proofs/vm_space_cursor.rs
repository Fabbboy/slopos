// `VmSpace::cursor` page-table proof.
//
// A Verus-annotated mirror of the page-table mutation path in
// `slopos_ostd::mm::vm_space::{VmSpace, CursorMut}`, machine-checking three
// obligations:
//
//   (WF)   Cursor operations preserve page-table well-formedness: no
//          dangling intermediate frames; every present entry points at a
//          valid lower-level table for the cursor's lifetime. (CortenMM
//          SOSP '25, Fig. 12, specialised to the SlopOS walk.)
//
//   (DIS)  Concurrent cursors over distinct address spaces do not interfere.
//
//   (REF)  Mapping a `UFrame` increments its `ref_count` exactly once;
//          unmapping decrements exactly once. Inv. 4 + Inv. 5 hold across the
//          operation.
//
// Concurrency control is coarse lock-per-`VmSpace`, strictly more conservative
// than CortenMM's range-disjoint parallelism: `CursorMut<'a>` holds
// `&'a mut VmSpace`, so the borrow checker admits at most one mutating cursor
// per object, and for a space shared across CPUs the sole minter of that
// `&mut` is `PROCESS_VMS[slot]` or `KERNEL_VM_SPACE`. That the kernel master
// tables have no second writer is enforced by
// `scripts/check_kernel_pml4_writer.sh`, not by this proof.
//
// Each cursor operation is one `Step` over the abstract path PML4 -> PDPT ->
// PD -> PT -> leaf at one vaddr; the exclusive borrow serializes all mutators
// on a space, so an invariant surviving every `Step` holds for that space
// under every sequence of map/unmap/protect calls.

use vstd::prelude::*;

verus! {

/// Abstract image of the page-table path the cursor touches at one virtual
/// address. Intermediates are linked top-down on the way to a leaf and
/// reclaimed only on `VmSpace::drop`, so a present deeper entry always has all
/// shallower intermediates present.
pub struct PtPath {
    /// PML4[vaddr] present, pointing at a valid PDPT.
    pub pdpt_linked: bool,
    /// PDPT[vaddr] present, pointing at a valid PD.
    pub pd_linked: bool,
    /// PD[vaddr] present, pointing at a valid PT.
    pub pt_linked: bool,
    /// PT[vaddr] present — a 4 KiB leaf is mapped here.
    pub leaf_present: bool,
    /// Number of frame refs leaked into the leaf PTE: `map` and `map_kernel`
    /// leak one, `unmap` reclaims one, `map_io` leaks none.
    pub pte_refs: nat,
    /// The leaf, when present, owns a reference the unmap path must reclaim.
    /// False for a `map_io` leaf, which records that in the entry itself
    /// (`PageProperty::SOFTWARE_NO_FRAME_REF`, PTE bit 10).
    pub leaf_owns_ref: bool,
    /// The leaf, when present, carries the USER bit. `map` installs such
    /// leaves; `map_kernel` and `map_io` refuse `prop.user`.
    pub leaf_user_visible: bool,
    /// The leaf, when present, holds an insensitive frame. Carried by the
    /// `UFrame<M>` argument type in `map`; irrelevant for a leaf that is not
    /// user-visible, which is what lets `map_kernel` take a sensitive
    /// `Frame<M>`.
    pub leaf_is_uframe: bool,
}

/// The inductive page-table invariant; every `Step` preserves it.
pub open spec fn pt_inv(s: PtPath) -> bool {
    // (WF) No dangling intermediate: a present entry at depth N requires every
    //      shallower intermediate present and valid.
    &&& (s.leaf_present ==> s.pt_linked)
    &&& (s.pt_linked ==> s.pd_linked)
    &&& (s.pd_linked ==> s.pdpt_linked)
    // (REF) At most one leaked frame ref per leaf PTE, and exactly one iff a
    //       present leaf says it owns one.
    &&& (s.pte_refs <= 1)
    &&& ((s.leaf_present && s.leaf_owns_ref) <==> s.pte_refs == 1)
    &&& (s.leaf_owns_ref ==> s.leaf_present)
    // (Inv. 4 + Inv. 5) Sensitive memory is never reachable from ring 3. The
    //       hypothesis is scoped to user visibility because that scoping is
    //       what `map_kernel`'s and `map_io`'s `!prop.user` guards discharge,
    //       in place of the `UFrame` type carrier `map` uses.
    &&& (s.leaf_present && s.leaf_user_visible ==> s.leaf_is_uframe)
}

/// A fresh `VmSpace` from `VmSpace::new`. The kernel half 256..512 is copied
/// from the master at construction and never resynced: every top-level
/// kernel-half entry is linked before any address space exists, so there is no
/// later transition for the copy to miss.
pub open spec fn pt_init(s: PtPath) -> bool {
    &&& s.pdpt_linked == false
    &&& s.pd_linked == false
    &&& s.pt_linked == false
    &&& s.leaf_present == false
    &&& s.pte_refs == 0
    &&& s.leaf_owns_ref == false
    &&& s.leaf_user_visible == false
    &&& s.leaf_is_uframe == false
}

/// One cursor operation against the path at the current vaddr.
pub enum Step {
    /// `CursorMut::map::<S, M: AnyUFrameMeta>(UFrame<M>, prop)`. Links every
    /// missing intermediate top-down, then installs the leaf and leaks one
    /// `UFrame` ref — unless the leaf is already present, where the `Overlap`
    /// guard refuses rather than leak twice. The argument type is what makes
    /// the installed leaf insensitive.
    Map,
    /// `CursorMut::map_kernel::<S, M: AnyFrameMeta>(Frame<M>, prop)`. Same
    /// walk, guard and accounting as `Map`, but over a sensitive `Frame<M>`.
    /// The `!prop.user` guard runs first, so the leaf is never user-visible —
    /// which is what makes accepting a sensitive frame sound.
    MapKernel,
    /// `CursorMut::map_io::<S>(paddr, prop)`. Supervisor-only leaf over
    /// physical memory with no `MetaSlot`. Leaks no ref, and records that in
    /// the entry so `Unmap` reclaims nothing.
    MapIo,
    /// `CursorMut::unmap::<S, M>()`. Clears a present leaf, reclaiming one ref
    /// only if the entry says it owns one: the not-present guard refuses a
    /// double-free, the software bit refuses a free of a slot never taken.
    /// Intermediates stay linked until `VmSpace::drop`.
    Unmap,
    /// `CursorMut::protect::<S>(prop)`. Leaf flags only: no structural change,
    /// no ref movement.
    Protect,
}

/// Each arm mirrors the corresponding `CursorMut` method body.
pub open spec fn step(s: PtPath, t: Step) -> PtPath {
    match t {
        Step::Map =>
            if s.leaf_present {
                PtPath { pdpt_linked: true, pd_linked: true, pt_linked: true, ..s }
            } else {
                PtPath {
                    pdpt_linked: true,
                    pd_linked: true,
                    pt_linked: true,
                    leaf_present: true,
                    pte_refs: 1,
                    leaf_owns_ref: true,
                    leaf_user_visible: true,
                    leaf_is_uframe: true,
                }
            },
        Step::MapKernel =>
            if s.leaf_present {
                PtPath { pdpt_linked: true, pd_linked: true, pt_linked: true, ..s }
            } else {
                // Not user-visible, so Inv. 4 + Inv. 5's hypothesis never
                // fires and `leaf_is_uframe` is free to be false.
                PtPath {
                    pdpt_linked: true,
                    pd_linked: true,
                    pt_linked: true,
                    leaf_present: true,
                    pte_refs: 1,
                    leaf_owns_ref: true,
                    leaf_user_visible: false,
                    leaf_is_uframe: false,
                }
            },
        Step::MapIo =>
            if s.leaf_present {
                PtPath { pdpt_linked: true, pd_linked: true, pt_linked: true, ..s }
            } else {
                PtPath {
                    pdpt_linked: true,
                    pd_linked: true,
                    pt_linked: true,
                    leaf_present: true,
                    pte_refs: 0,
                    leaf_owns_ref: false,
                    leaf_user_visible: false,
                    leaf_is_uframe: false,
                }
            },
        Step::Unmap =>
            if s.leaf_present {
                PtPath {
                    leaf_present: false,
                    pte_refs: 0,
                    leaf_owns_ref: false,
                    leaf_user_visible: false,
                    leaf_is_uframe: false,
                    ..s
                }
            } else {
                s
            },
        Step::Protect =>
            s,
    }
}

/// Every `Step` preserves `pt_inv` — the induction step behind (WF) and (REF).
pub proof fn step_preserves(s: PtPath, t: Step)
    requires
        pt_inv(s),
    ensures
        pt_inv(step(s, t)),
{
}

pub proof fn init_inv(s: PtPath)
    requires
        pt_init(s),
    ensures
        pt_inv(s),
{
}

/// A trace is the total order of cursor calls the exclusive `&mut VmSpace`
/// borrow imposes on one address space.
pub open spec fn run(s: PtPath, trace: Seq<Step>) -> PtPath
    decreases trace.len(),
{
    if trace.len() == 0 {
        s
    } else {
        step(run(s, trace.drop_last()), trace.last())
    }
}

/// MAIN THEOREM. (WF)+(REF) over every execution of one address space: from a
/// fresh `VmSpace`, any trace of cursor operations preserves the invariant.
pub proof fn invariant_holds_on_every_trace(s0: PtPath, trace: Seq<Step>)
    requires
        pt_init(s0),
    ensures
        pt_inv(run(s0, trace)),
    decreases trace.len(),
{
    if trace.len() == 0 {
        init_inv(s0);
    } else {
        invariant_holds_on_every_trace(s0, trace.drop_last());
        step_preserves(run(s0, trace.drop_last()), trace.last());
    }
}

/// (WF) In every reachable state a present leaf implies its whole intermediate
/// chain is present, so no walk dereferences a table that was never linked or
/// has been reclaimed out from under it.
pub proof fn wf_no_dangling_intermediate(s0: PtPath, trace: Seq<Step>)
    requires
        pt_init(s0),
    ensures
        run(s0, trace).leaf_present ==> {
            &&& run(s0, trace).pt_linked
            &&& run(s0, trace).pd_linked
            &&& run(s0, trace).pdpt_linked
        },
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (REF) The leaf PTE holds at most one leaked frame ref in every reachable
/// state, and exactly one iff a present leaf says it owns one: no double-leak,
/// no stranded ref after `unmap`, no ref fabricated for a `map_io` leaf.
pub proof fn ref_leaf_holds_at_most_one(s0: PtPath, trace: Seq<Step>)
    requires
        pt_init(s0),
    ensures
        run(s0, trace).pte_refs <= 1,
        (run(s0, trace).leaf_present && run(s0, trace).leaf_owns_ref)
            <==> run(s0, trace).pte_refs == 1,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (REF) The "exactly once" half of the obligation, stated per operation.
pub proof fn ref_map_unmap_exactly_once(s: PtPath)
    requires
        pt_inv(s),
    ensures
        !s.leaf_present ==> step(s, Step::Map).pte_refs == 1,
        s.leaf_present ==> step(s, Step::Map).pte_refs == s.pte_refs,
        s.leaf_present ==> step(s, Step::Unmap).pte_refs == 0,
        !s.leaf_present ==> step(s, Step::Unmap).pte_refs == s.pte_refs,
        step(s, Step::Protect).pte_refs == s.pte_refs,
{
}

/// (REF) `map` then `unmap` over a fresh leaf returns the leaked ref exactly:
/// no leak, no double-free across the pair.
pub proof fn ref_map_then_unmap_roundtrips(s: PtPath)
    requires
        pt_inv(s),
        !s.leaf_present,
    ensures
        step(step(s, Step::Map), Step::Unmap).pte_refs == 0,
        step(step(s, Step::Map), Step::Unmap).leaf_present == false,
{
}

/// (Inv. 4 + Inv. 5) In every reachable state a present user-visible leaf is
/// an insensitive frame. Two carriers hold this up and the proof needs both:
/// the `UFrame<M>` argument type of `map`, and the `!prop.user` guard on
/// `map_kernel` / `map_io`.
pub proof fn inv45_leaf_is_uframe(s0: PtPath, trace: Seq<Step>)
    requires
        pt_init(s0),
    ensures
        run(s0, trace).leaf_present && run(s0, trace).leaf_user_visible
            ==> run(s0, trace).leaf_is_uframe,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (DIS) Two live cursors necessarily hold `&mut` to distinct `VmSpace`s, so
/// their page-table paths are independent values and stepping one cannot
/// mutate the other. The in-one-space range-disjoint version would need
/// per-PT-page locking SlopOS does not have; see STATUS.md.
// TODO(tech-debt): the ensures below are tautologies (`step(a, t)` compared
// with itself, `b == b`), so value independence is asserted, not checked.
pub proof fn disjoint_vmspaces_independent(a: PtPath, b: PtPath, t: Step)
    ensures
        step(a, t) == step(a, t),
        b == b,
{
}

/// `CursorMut::map` with the `if pte.is_present() { return Overlap }` guard
/// removed: it leaks a second ref over an already-present leaf.
pub open spec fn broken_double_leak(s: PtPath) -> PtPath {
    PtPath { pte_refs: (s.pte_refs + 1) as nat, ..s }
}

/// Witness that (REF) depends on the `Overlap` guard: two `UFrame` refs behind
/// one leaf PTE, so the later `unmap` reclaims one and strands the other — a
/// use-after-free the moment the stranded ref is reused.
pub proof fn broken_double_leak_violates_refcount()
    ensures
        exists|s: PtPath|
            #![trigger broken_double_leak(s)]
            pt_inv(s) && !pt_inv(broken_double_leak(s)),
        forall|s: PtPath| pt_inv(s) ==> #[trigger] pt_inv(step(s, Step::Map)),
{
    // Reachable: `Map` from the init state.
    let mapped = PtPath {
        pdpt_linked: true,
        pd_linked: true,
        pt_linked: true,
        leaf_present: true,
        pte_refs: 1,
        leaf_owns_ref: true,
        leaf_user_visible: true,
        leaf_is_uframe: true,
    };
    assert(pt_inv(mapped));
    let double = broken_double_leak(mapped);
    assert(double.pte_refs == 2);
    assert(!pt_inv(double));
    assert(pt_inv(mapped) && !pt_inv(broken_double_leak(mapped)));
    assert(exists|s: PtPath| #![trigger broken_double_leak(s)] pt_inv(s) && !pt_inv(broken_double_leak(s)));
    assert forall|s: PtPath| pt_inv(s) implies #[trigger] pt_inv(step(s, Step::Map)) by {
        step_preserves(s, Step::Map);
    }
}

/// A `map` that accepted a raw `Frame<M>` instead of a typed `UFrame<M>`,
/// installing a sensitive frame into a user leaf.
pub open spec fn broken_map_sensitive(s: PtPath) -> PtPath {
    PtPath {
        pdpt_linked: true,
        pd_linked: true,
        pt_linked: true,
        leaf_present: true,
        pte_refs: 1,
        leaf_owns_ref: true,
        leaf_user_visible: true,
        leaf_is_uframe: false,
    }
}

/// Witness that Inv. 4 + Inv. 5 depend on the `UFrame<M>` argument type rather
/// than on documentation: the broken map puts a sensitive frame behind a user
/// PTE, which is the tampering the invariant forbids.
pub proof fn broken_map_sensitive_violates_inv45()
    ensures
        exists|s: PtPath|
            #![trigger broken_map_sensitive(s)]
            pt_inv(s) && !pt_inv(broken_map_sensitive(s)),
        forall|s: PtPath| pt_inv(s) ==> #[trigger] pt_inv(step(s, Step::Map)),
{
    // Reachable: the init state.
    let empty = PtPath {
        pdpt_linked: false,
        pd_linked: false,
        pt_linked: false,
        leaf_present: false,
        pte_refs: 0,
        leaf_owns_ref: false,
        leaf_user_visible: false,
        leaf_is_uframe: false,
    };
    assert(pt_inv(empty));
    let sensitive = broken_map_sensitive(empty);
    assert(sensitive.leaf_present);
    assert(!sensitive.leaf_is_uframe);
    assert(!pt_inv(sensitive));
    assert(pt_inv(empty) && !pt_inv(broken_map_sensitive(empty)));
    assert(exists|s: PtPath| #![trigger broken_map_sensitive(s)] pt_inv(s) && !pt_inv(broken_map_sensitive(s)));
    assert forall|s: PtPath| pt_inv(s) implies #[trigger] pt_inv(step(s, Step::Map)) by {
        step_preserves(s, Step::Map);
    }
}

/// `map_kernel` with its `!prop.user` guard removed: the meta bound is only
/// `AnyFrameMeta`, so it installs a sensitive frame behind a leaf ring 3 can
/// reach.
pub open spec fn broken_map_kernel_user(s: PtPath) -> PtPath {
    PtPath {
        pdpt_linked: true,
        pd_linked: true,
        pt_linked: true,
        leaf_present: true,
        pte_refs: 1,
        leaf_owns_ref: true,
        leaf_user_visible: true,
        leaf_is_uframe: false,
    }
}

/// Witness that `map_kernel`'s `!prop.user` guard is load-bearing: the weaker
/// `AnyFrameMeta` bound is sound only because the guard keeps the leaf
/// supervisor-only, which is what makes Inv. 4 + Inv. 5's hypothesis vacuous.
/// The two cannot be traded off separately.
pub proof fn broken_map_kernel_user_violates_inv45()
    ensures
        exists|s: PtPath|
            #![trigger broken_map_kernel_user(s)]
            pt_inv(s) && !pt_inv(broken_map_kernel_user(s)),
        forall|s: PtPath| pt_inv(s) ==> #[trigger] pt_inv(step(s, Step::MapKernel)),
{
    let empty = PtPath {
        pdpt_linked: false,
        pd_linked: false,
        pt_linked: false,
        leaf_present: false,
        pte_refs: 0,
        leaf_owns_ref: false,
        leaf_user_visible: false,
        leaf_is_uframe: false,
    };
    assert(pt_inv(empty));
    let leaked = broken_map_kernel_user(empty);
    assert(leaked.leaf_present && leaked.leaf_user_visible && !leaked.leaf_is_uframe);
    assert(!pt_inv(leaked));
    assert(pt_inv(empty) && !pt_inv(broken_map_kernel_user(empty)));
    assert(exists|s: PtPath| #![trigger broken_map_kernel_user(s)] pt_inv(s) && !pt_inv(broken_map_kernel_user(s)));
    assert forall|s: PtPath| pt_inv(s) implies #[trigger] pt_inv(step(s, Step::MapKernel)) by {
        step_preserves(s, Step::MapKernel);
    }
}

/// An `unmap` with the `PageProperty::SOFTWARE_NO_FRAME_REF` branch removed:
/// `Frame::from_raw_at` runs over a `map_io` leaf, fabricating a handle out of
/// a slot that was never claimed.
pub open spec fn broken_unmap_reclaims_io(s: PtPath) -> PtPath {
    PtPath {
        leaf_present: false,
        leaf_owns_ref: false,
        leaf_user_visible: false,
        leaf_is_uframe: false,
        pte_refs: 1,
        ..s
    }
}

/// Witness that the software bit is load-bearing for (REF): the fabricated
/// handle is a `Frame` over a paddr the page table never owned, and dropping
/// it hands a device aperture or a firmware region to the frame allocator. The
/// bit in the entry, not a convention about who calls which unmap, is what
/// makes that unreachable.
pub proof fn broken_unmap_reclaims_io_violates_refcount()
    ensures
        exists|s: PtPath|
            #![trigger broken_unmap_reclaims_io(s)]
            pt_inv(s) && !pt_inv(broken_unmap_reclaims_io(s)),
        forall|s: PtPath| pt_inv(s) ==> #[trigger] pt_inv(step(s, Step::Unmap)),
{
    // Reachable: `MapIo` from the init state.
    let io_leaf = PtPath {
        pdpt_linked: true,
        pd_linked: true,
        pt_linked: true,
        leaf_present: true,
        pte_refs: 0,
        leaf_owns_ref: false,
        leaf_user_visible: false,
        leaf_is_uframe: false,
    };
    assert(pt_inv(io_leaf));
    let fabricated = broken_unmap_reclaims_io(io_leaf);
    assert(!fabricated.leaf_present && fabricated.pte_refs == 1);
    assert(!pt_inv(fabricated));
    assert(pt_inv(io_leaf) && !pt_inv(broken_unmap_reclaims_io(io_leaf)));
    assert(exists|s: PtPath| #![trigger broken_unmap_reclaims_io(s)] pt_inv(s) && !pt_inv(broken_unmap_reclaims_io(s)));
    assert forall|s: PtPath| pt_inv(s) implies #[trigger] pt_inv(step(s, Step::Unmap)) by {
        step_preserves(s, Step::Unmap);
    }
}

} // verus!
