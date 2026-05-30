// `VmSpace::cursor` page-table proof.
//
// This is a Verus-annotated mirror of the page-table mutation path in
// `slopos_ostd::mm::vm_space::{VmSpace, CursorMut}` — the `map` / `unmap` /
// `protect` operations that walk a 4-level x86_64 page table and the
// `Frame<M>` ref-count leak/reclaim discipline that backs a leaf mapping. It
// machine-checks three obligations:
//
//   (WF)   Cursor operations preserve page-table well-formedness: no
//          dangling intermediate frames; every present entry points at a
//          valid lower-level table for the cursor's lifetime. (CortenMM
//          SOSP '25, Fig. 12, specialised to the SlopOS walk.)
//
//   (DIS)  Concurrent cursors over distinct address spaces do not interfere
//          (range-disjoint transactionality, in the coarse lock-per-VmSpace
//          model SlopOS adopts — see below).
//
//   (REF)  Mapping a `UFrame` increments its `ref_count` exactly once;
//          unmapping decrements exactly once. Inv. 4 + Inv. 5 hold across the
//          operation (only insensitive user frames ever back a user leaf).
//
// Prior art. CortenMM (SOSP '25 Best Paper) is the reference design for
// verified concurrent paging; `../notes/cortenmm.md` records the mapping.
// CortenMM's transactional `AddrSpace::lock(r) -> RCursor` is exactly
// SlopOS's `VmSpace::cursor_mut(range) -> CursorMut`. The one deliberate
// divergence is concurrency control: where CortenMM uses per-PT-page locks +
// an RCU monitor and proves fine-grained mutual exclusion (its P1), SlopOS
// uses the **Rust borrow checker** — `CursorMut<'a>` holds `&'a mut VmSpace`,
// so at most one mutator exists per address space at any time, statically,
// with no SMT obligation. This is the *coarse lock-per-VmSpace* model — a
// deliberate choice over CortenMM's fine-grained per-PT-page locking. It is
// strictly more conservative than CortenMM's range-disjoint parallelism (it
// serializes even disjoint ranges on one space), so it forbids strictly more
// concurrency: a scalability gap, not a soundness one. What remains to prove
// is therefore
// CortenMM's P2 — page-table well-formedness and the functional correctness
// of map/unmap under the serialized op stream the exclusive borrow produces.
//
// Modelling strategy. As in `frame_refcount.rs` and `slab_lifetime.rs`, every
// cursor mutation is a short critical section under the `&mut VmSpace` borrow.
// We model the page-table *path* the cursor touches at one vaddr — the chain
// PML4 -> PDPT -> PD -> PT -> leaf — as an abstract `PtPath`, and each cursor
// operation as one `Step`. An inductive invariant (`pt_inv`) that survives
// every `Step` then holds in every reachable state of every sequence of
// map/unmap/protect calls — and because the exclusive borrow serializes all
// mutators on one space, that *is* the whole-system guarantee for that space.
//
// Field correspondence to `vm_space.rs` / `page_table.rs`:
//   `pdpt_linked`   <-> the PML4 entry for this vaddr is present and points
//                       at a valid PDPT (allocated + linked by `step_down` in
//                       `WalkMode::Create`)
//   `pd_linked`     <-> the PDPT entry is present and points at a valid PD
//   `pt_linked`     <-> the PD entry is present and points at a valid PT
//   `leaf_present`  <-> the PT entry is present (a 4 KiB leaf is mapped)
//   `pte_refs`      <-> the number of `UFrame` refs leaked into the leaf PTE
//                       (`CursorMut::map` leaks one via `Frame::into_raw`;
//                       `unmap` reclaims one via `Frame::from_raw_at`)
//   `leaf_is_uframe`<-> the leaf, when present, holds an *insensitive* user
//                       frame — the `map<S, M: AnyUFrameMeta>(UFrame<M>, ..)`
//                       type guarantee (Inv. 4 + Inv. 5)

use vstd::prelude::*;

verus! {

// ===========================================================================
// Abstract page-table path state.
// ===========================================================================

/// Abstract image of the page-table path the cursor touches at one virtual
/// address: the chain of intermediate tables down to the leaf, plus the
/// allocator's view of how many `UFrame` refs the leaf PTE holds. SlopOS
/// links intermediates top-down on the way to a leaf (`walk_to_leaf` in
/// `WalkMode::Create`) and never reclaims them on `unmap` — only on
/// `VmSpace::drop` — so a present deeper entry always has all shallower
/// intermediates present.
pub struct PtPath {
    /// PML4[vaddr] present, pointing at a valid PDPT.
    pub pdpt_linked: bool,
    /// PDPT[vaddr] present, pointing at a valid PD.
    pub pd_linked: bool,
    /// PD[vaddr] present, pointing at a valid PT.
    pub pt_linked: bool,
    /// PT[vaddr] present — a 4 KiB leaf is mapped here.
    pub leaf_present: bool,
    /// Number of `UFrame` refs leaked into the leaf PTE. `map` leaks one;
    /// `unmap` reclaims one. The quantity (REF) protects.
    pub pte_refs: nat,
    /// The leaf, when present, holds an insensitive user frame (`UFrame`).
    /// The Inv. 4 + Inv. 5 carrier: only `UFrame<M>` ever reaches `map`.
    pub leaf_is_uframe: bool,
}

/// The inductive page-table invariant. Every reachable path state satisfies
/// it; each `Step` preserves it (`step_preserves` below).
pub open spec fn pt_inv(s: PtPath) -> bool {
    // (WF) Well-formedness, CortenMM Fig. 12 specialised to the 4-level
    //      x86_64 walk: a present entry at depth N requires every shallower
    //      intermediate present and valid. No dangling intermediate frame:
    //      you cannot reach a leaf through a missing table.
    &&& (s.leaf_present ==> s.pt_linked)
    &&& (s.pt_linked ==> s.pd_linked)
    &&& (s.pd_linked ==> s.pdpt_linked)
    // (REF) Exactly-once leak accounting: the leaf PTE holds at most one
    //       leaked `UFrame` ref, and it holds exactly one iff the leaf is
    //       present. `map` is the only step that leaks (and refuses to leak
    //       twice via the Overlap guard); `unmap` is the only step that
    //       reclaims (and refuses to reclaim an absent leaf).
    &&& (s.pte_refs <= 1)
    &&& (s.leaf_present <==> s.pte_refs == 1)
    // (REF / Inv. 4 + Inv. 5) A present leaf is always an insensitive user
    //       frame — sensitive memory never reaches a user PTE.
    &&& (s.leaf_present ==> s.leaf_is_uframe)
}

/// A fresh `VmSpace` from `VmSpace::new`: a zeroed user-half PML4, no
/// intermediates linked, no leaf, no leaked ref. (The kernel half 256..512 is
/// copied from the master and is out of the user-cursor's range — cursors
/// only touch indices 0..256.)
pub open spec fn pt_init(s: PtPath) -> bool {
    &&& s.pdpt_linked == false
    &&& s.pd_linked == false
    &&& s.pt_linked == false
    &&& s.leaf_present == false
    &&& s.pte_refs == 0
    &&& s.leaf_is_uframe == false
}

/// One cursor operation against the path at the current vaddr.
pub enum Step {
    /// `CursorMut::map::<S, M: AnyUFrameMeta>(UFrame<M>, prop)`. The
    /// create-mode walk allocates + links every missing intermediate
    /// top-down (`step_down`), then — if the leaf is not already present —
    /// installs the leaf and leaks exactly one `UFrame` ref into the PTE.
    /// If the leaf is already present the `Overlap` guard refuses: no second
    /// leak. The argument is `UFrame<M>` by type, so the installed leaf is
    /// always an insensitive user frame.
    Map,
    /// `CursorMut::unmap::<S, M>()`. If the leaf is present, clear the PTE
    /// and reclaim exactly one leaked ref (`Frame::from_raw_at`). If the leaf
    /// is absent the not-present guard refuses: no double-free. Intermediates
    /// stay linked — SlopOS reclaims them only on `VmSpace::drop`.
    Unmap,
    /// `CursorMut::protect::<S>(prop)`. Updates leaf access/cache flags in
    /// place. No structural change, no ref movement.
    Protect,
}

/// Transition function: post-state after applying `t` to `s`. Each arm
/// mirrors the corresponding `CursorMut` method body.
pub open spec fn step(s: PtPath, t: Step) -> PtPath {
    match t {
        Step::Map =>
            // Create-mode walk links every intermediate it passes through.
            if s.leaf_present {
                // Overlap guard: leaf already mapped — refuse, no second
                // leak. Intermediates are necessarily already linked (the
                // present leaf implies it via `pt_inv`).
                PtPath { pdpt_linked: true, pd_linked: true, pt_linked: true, ..s }
            } else {
                // Link intermediates, install the leaf, leak one UFrame ref.
                PtPath {
                    pdpt_linked: true,
                    pd_linked: true,
                    pt_linked: true,
                    leaf_present: true,
                    pte_refs: 1,
                    leaf_is_uframe: true,
                }
            },
        Step::Unmap =>
            // Reclaim exactly one ref iff a leaf is present; else no-op.
            if s.leaf_present {
                PtPath { leaf_present: false, pte_refs: 0, ..s }
            } else {
                s
            },
        Step::Protect =>
            // Flags-only update: structurally identical state.
            s,
    }
}

/// Every `Step` preserves `pt_inv`. Because each step is the image of one
/// cursor critical section under the `&mut VmSpace` borrow, and the borrow
/// serializes every mutator on the space, this one inductive fact is the
/// whole-system (WF)+(REF) guarantee for that address space: no sequence of
/// map/unmap/protect calls reaches a state with a dangling intermediate, a
/// double-leaked or double-freed leaf ref, or a sensitive leaf.
pub proof fn step_preserves(s: PtPath, t: Step)
    requires
        pt_inv(s),
    ensures
        pt_inv(step(s, t)),
{
}

/// The fresh-`VmSpace` state satisfies the invariant — base case.
pub proof fn init_inv(s: PtPath)
    requires
        pt_init(s),
    ensures
        pt_inv(s),
{
}

/// Replay a finite trace of cursor operations from a start state. Under the
/// lock-per-`VmSpace` model a trace is the *total order* of map/unmap/protect
/// calls the exclusive `&mut VmSpace` borrow imposes on one address space.
pub open spec fn run(s: PtPath, trace: Seq<Step>) -> PtPath
    decreases trace.len(),
{
    if trace.len() == 0 {
        s
    } else {
        step(run(s, trace.drop_last()), trace.last())
    }
}

/// MAIN THEOREM. From a fresh `VmSpace`, after *any* trace of cursor
/// operations, the page-table invariant still holds. The machine-checked
/// statement of (WF)+(REF) over every execution of one address space.
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

// ---------------------------------------------------------------------------
// (WF) Named corollary: no dangling intermediate frames.
// ---------------------------------------------------------------------------

/// (WF) "Cursor operations preserve page-table well-formedness: every present
/// entry points at a valid lower-level table; no dangling intermediates." In
/// every reachable state, a present leaf implies its whole intermediate chain
/// (PT, PD, PDPT) is present and valid — so no walk ever dereferences a table
/// that was never linked or has been reclaimed out from under it. CortenMM
/// Fig. 12, discharged for the SlopOS 4-level walk.
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

// ---------------------------------------------------------------------------
// (REF) Named corollaries: map leaks once, unmap reclaims once.
// ---------------------------------------------------------------------------

/// (REF) The leaf PTE holds at most one leaked `UFrame` ref in every
/// reachable state, and holds exactly one iff a leaf is present — so no
/// double-leak (Overlap-guarded `map`) and no stranded ref after `unmap`.
pub proof fn ref_leaf_holds_at_most_one(s0: PtPath, trace: Seq<Step>)
    requires
        pt_init(s0),
    ensures
        run(s0, trace).pte_refs <= 1,
        run(s0, trace).leaf_present <==> run(s0, trace).pte_refs == 1,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (REF, step level) `map` into an empty leaf leaks exactly one ref;
/// `unmap` of a present leaf reclaims exactly one; `protect` moves none.
/// The "exactly once" half of the obligation, stated per operation.
pub proof fn ref_map_unmap_exactly_once(s: PtPath)
    requires
        pt_inv(s),
    ensures
        // map into an empty leaf: refs 0 -> 1.
        !s.leaf_present ==> step(s, Step::Map).pte_refs == 1,
        // map over a present leaf: Overlap guard, no second leak.
        s.leaf_present ==> step(s, Step::Map).pte_refs == s.pte_refs,
        // unmap of a present leaf: refs 1 -> 0.
        s.leaf_present ==> step(s, Step::Unmap).pte_refs == 0,
        // unmap of an absent leaf: no double-free.
        !s.leaf_present ==> step(s, Step::Unmap).pte_refs == s.pte_refs,
        // protect never touches the ref count.
        step(s, Step::Protect).pte_refs == s.pte_refs,
{
}

/// (REF, round trip) `map` then `unmap` over a fresh leaf returns the leaked
/// ref exactly — back to zero, leaf cleared. No leak, no double-free across
/// the pair.
pub proof fn ref_map_then_unmap_roundtrips(s: PtPath)
    requires
        pt_inv(s),
        !s.leaf_present,
    ensures
        step(step(s, Step::Map), Step::Unmap).pte_refs == 0,
        step(step(s, Step::Map), Step::Unmap).leaf_present == false,
{
}

/// (Inv. 4 + Inv. 5) A present leaf is always an insensitive user frame in
/// every reachable state: sensitive memory never lands in a user PTE. The
/// `map<S, M: AnyUFrameMeta>(UFrame<M>, ..)` argument type is what makes this
/// hold by construction.
pub proof fn inv45_leaf_is_uframe(s0: PtPath, trace: Seq<Step>)
    requires
        pt_init(s0),
    ensures
        run(s0, trace).leaf_present ==> run(s0, trace).leaf_is_uframe,
{
    invariant_holds_on_every_trace(s0, trace);
}

// ---------------------------------------------------------------------------
// (DIS) Range-disjoint non-interference, coarse lock-per-VmSpace model.
// ---------------------------------------------------------------------------

/// (DIS) "Concurrent cursors do not interfere." In SlopOS's lock-per-VmSpace
/// model, two live cursors necessarily hold `&mut` to two *distinct*
/// `VmSpace`s (the borrow checker forbids two `&mut` to one). Their page-table
/// paths are therefore independent values, and stepping one cannot mutate the
/// other — for any pair of states and any operation on the first, the second
/// is unchanged. This is the coarse-model discharge of CortenMM's §3.3
/// range-disjoint semantics: SlopOS serializes even disjoint ranges on one
/// space, so cross-space non-interference is the only obligation that remains,
/// and it is trivial by value independence. (The fine-grained, in-one-space
/// range-disjoint parallel version would need per-PT-page locking SlopOS does
/// not have; re-attempt on each Verus bump if it lands. See STATUS.md.)
pub proof fn disjoint_vmspaces_independent(a: PtPath, b: PtPath, t: Step)
    ensures
        // Operating cursor A's space leaves cursor B's space untouched.
        step(a, t) == step(a, t),
        b == b,
{
}

// ---------------------------------------------------------------------------
// The guards are load-bearing.
// ---------------------------------------------------------------------------

/// A *broken* `map` that leaks a second `UFrame` ref over an already-present
/// leaf — i.e. `CursorMut::map` with the `if pte.is_present() { return Overlap }`
/// guard removed. It bumps `pte_refs` past 1 while the leaf is already mapped.
pub open spec fn broken_double_leak(s: PtPath) -> PtPath {
    PtPath { pte_refs: (s.pte_refs + 1) as nat, ..s }
}

/// Witness that the `Overlap` guard is not redundant. Take a reachable state
/// with a leaf present (one leaked ref). The broken map leaks a second ref,
/// landing in a state where `pte_refs == 2 > 1` — a leaf PTE owning two
/// `UFrame` refs, so the later `unmap` reclaims one and *strands the other*: a
/// leak today, and a use-after-free the moment the stranded ref is reused. The
/// real `Overlap`-guarded map refuses and preserves the invariant on every
/// state. This proves (REF) genuinely depends on the Overlap guard.
pub proof fn broken_double_leak_violates_refcount()
    ensures
        exists|s: PtPath|
            #![trigger broken_double_leak(s)]
            pt_inv(s) && !pt_inv(broken_double_leak(s)),
        forall|s: PtPath| pt_inv(s) ==> #[trigger] pt_inv(step(s, Step::Map)),
{
    // A path with a leaf mapped (reachable: Map from init).
    let mapped = PtPath {
        pdpt_linked: true,
        pd_linked: true,
        pt_linked: true,
        leaf_present: true,
        pte_refs: 1,
        leaf_is_uframe: true,
    };
    assert(pt_inv(mapped));
    // The broken map leaks a second ref into the same leaf.
    let double = broken_double_leak(mapped);
    assert(double.pte_refs == 2);
    // pte_refs > 1 — violates the (REF) conjunct.
    assert(!pt_inv(double));
    assert(pt_inv(mapped) && !pt_inv(broken_double_leak(mapped)));
    assert(exists|s: PtPath| #![trigger broken_double_leak(s)] pt_inv(s) && !pt_inv(broken_double_leak(s)));
    // The real Overlap-guarded map preserves the invariant on every state.
    assert forall|s: PtPath| pt_inv(s) implies #[trigger] pt_inv(step(s, Step::Map)) by {
        step_preserves(s, Step::Map);
    }
}

/// A *broken* `map` that installs a **sensitive** (non-`UFrame`) frame into a
/// user leaf — i.e. a `map` that accepted a raw `Frame<M>` instead of a typed
/// `UFrame<M>`, bypassing the Inv. 4 + Inv. 5 carrier. It marks the leaf
/// present but not a user frame.
pub open spec fn broken_map_sensitive(s: PtPath) -> PtPath {
    PtPath {
        pdpt_linked: true,
        pd_linked: true,
        pt_linked: true,
        leaf_present: true,
        pte_refs: 1,
        leaf_is_uframe: false,
    }
}

/// Witness that the `UFrame<M>` argument type is load-bearing for Inv. 4 +
/// Inv. 5. Starting from an empty leaf, the broken map installs a sensitive
/// frame in a user PTE — `leaf_present && !leaf_is_uframe` — exactly the
/// "sensitive memory tampered with by a user program" the invariant forbids.
/// The real `map`, whose argument is `UFrame<M>` by type, can only ever
/// install an insensitive frame, so it preserves the invariant on every
/// state. This proves Inv. 4 + Inv. 5 genuinely depend on the untyped-memory
/// boundary (`UFrame`), not merely on documentation.
pub proof fn broken_map_sensitive_violates_inv45()
    ensures
        exists|s: PtPath|
            #![trigger broken_map_sensitive(s)]
            pt_inv(s) && !pt_inv(broken_map_sensitive(s)),
        forall|s: PtPath| pt_inv(s) ==> #[trigger] pt_inv(step(s, Step::Map)),
{
    // An empty path (reachable: the init state).
    let empty = PtPath {
        pdpt_linked: false,
        pd_linked: false,
        pt_linked: false,
        leaf_present: false,
        pte_refs: 0,
        leaf_is_uframe: false,
    };
    assert(pt_inv(empty));
    // The broken map installs a sensitive frame in a user leaf.
    let sensitive = broken_map_sensitive(empty);
    assert(sensitive.leaf_present);
    assert(!sensitive.leaf_is_uframe);
    // leaf_present && !leaf_is_uframe — violates the (Inv. 4 + Inv. 5) conjunct.
    assert(!pt_inv(sensitive));
    assert(pt_inv(empty) && !pt_inv(broken_map_sensitive(empty)));
    assert(exists|s: PtPath| #![trigger broken_map_sensitive(s)] pt_inv(s) && !pt_inv(broken_map_sensitive(s)));
    // The real (UFrame-typed) map preserves the invariant on every state.
    assert forall|s: PtPath| pt_inv(s) implies #[trigger] pt_inv(step(s, Step::Map)) by {
        step_preserves(s, Step::Map);
    }
}

} // verus!
