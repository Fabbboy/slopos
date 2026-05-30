# CortenMM (SOSP '25 Best Paper) — notes for the `VmSpace::cursor` proof

Source: *CortenMM: Efficient Memory Management with Strong Correctness
Guarantees*, Junyang Zhang et al., Peking University / Ant Group / UCLA,
SOSP '25 — <http://web.cs.ucla.edu/~tamir/papers/sosp25.pdf>. Prior art for
Phase 3D (`slopos_ostd::mm::vm_space::Cursor`).

These notes capture only what is load-bearing for the SlopOS proof; they are
not a full paper summary.

## 1. What CortenMM is

A memory-management subsystem that **eliminates the software-level VMA
abstraction** and programs the MMU page table directly through a single
*transactional interface*. The page table is the only source of truth; each
PT page carries a *page descriptor* (a lock + a per-PTE metadata array)
indexed by physical page number, allocated contiguously at boot.

The headline result: the transactional interface is **formally verified with
Verus** (the same verifier this plan pins, AD-10), and the design outperforms
Linux's fine-grained MM by up to 15× on real benchmarks. Proof-to-code ratio
≈ 5.2 : 1, ≈ 8 person-months of verification effort.

## 2. The transactional interface (Figure 4)

```rust
impl AddrSpace {
    pub fn lock(&self, r: Range<Vaddr>) -> RCursor;   // acquire a range
}
impl RCursor {
    fn query(&mut self, addr: Vaddr) -> Status;
    fn map(&mut self, addr: Vaddr, page: PhysPage);
    fn mark(&mut self, range: Range<Vaddr>, status: Status);
    fn unmap(&mut self, range: Range<Vaddr>);
}
impl Drop for RCursor { /* releases the acquired locks */ }
```

The shape is **exactly** SlopOS's `VmSpace::cursor_mut(range) -> CursorMut`
with `map` / `unmap` / `protect` / `query` and a `Drop` that finalises the
session. The interface *decouples concurrency control from operations*: `lock`
does all the locking; the basic ops then run inside the locked region with no
further synchronisation reasoning. That decoupling is what makes the proof
tractable (verify locking and operations separately).

## 3. Concurrency-control semantics (§3.3)

1. All operations within one transaction execute **atomically**.
2. Concurrent transactions serialize **only when their ranges overlap**;
   transactions on **disjoint ranges run in parallel**.

CortenMM achieves (2) with two locking protocols:

- **CortenMMrw** (Figure 5): readers-writer locks. Traverse from the root
  taking read locks; the *covering PT page* (lowest page whose subtree covers
  the whole range) is upgraded to a write lock. "Lock one implicitly locks
  all descendants." Locks released in reverse on `RCursor::drop`.
- **CortenMMadv** (Figure 6): lockless RCU traversal + per-PT-page spin-locks.
  The traverse phase takes **no** locks (better scalability); the locking
  phase locks the covering PT page and DFS-locks all descendants. A
  concurrent `unmap` that frees a covering PT page is handled by RCU: clear
  the parent PTE atomically, mark the freed page `stale`, park it in an *RCU
  monitor*, and `rcu_delay_free` once no reader can still reach it. A locker
  that lands on a `stale` page retries from the root.

## 4. What CortenMM proves (§5)

- **P1 — Mutual exclusion.** Both protocols correctly serialize `lock(r)`
  calls on overlapping ranges (`lemma_mutual_exclusion`, Figure 11): once a
  core locks a covering PT page, no other core can lock an ancestor or
  descendant of it until the first unlocks. Proved by state-machine
  refinement: an *Atomic Tree Spec* refines an *Atomic Spec*; `interp` maps
  tree states to atomic states; the non-overlapping property carries the
  invariants up.
- **P2 — Functional correctness + page-table well-formedness.** `map` creates
  the correct entries at every level with the right permissions; `unmap`
  removes entries across the covered range; `mark` updates per-PTE metadata;
  `query` walks and returns the right status. And the global **well-formedness
  invariant** (Figure 12) holds under every operation:

  > For any page table entry with its present bit set, it must either be a
  > last-level (leaf) entry, or point to a **valid PT page belonging to the
  > next lower level**. Each child PTE is itself valid, points to a valid
  > page, and sits exactly one level below its parent.

  A lower *WF Tree Spec* proves subtrees well-formed and lifts to the global
  tree.

Trusted base: the hardware, Verus + its SMT solver, the physical page
allocator, the DMA code, and the lock/RCU primitive implementations.
Verus-proven code is verified **separately** from the rest of the OS (ported
into `verus!` and not linked with the nightly-only kernel build), because
parts of the kernel use nightly-only Rust features that don't compile under
Verus. CortenMM "ensures the proven code matches the implementation." This is
exactly the `proofs/README.md` discipline this repo already follows: the proof
is a standalone Verus mirror, cross-referenced to the source, not a rewrite of
it.

## 5. How this maps onto SlopOS `VmSpace::cursor`

| CortenMM | SlopOS OSTD |
|---|---|
| `AddrSpace` | `VmSpace` |
| `AddrSpace::lock(r) -> RCursor` | `VmSpace::cursor_mut(range) -> CursorMut` |
| `RCursor::{query, map, mark, unmap}` | `CursorMut::{query, map, protect, unmap}` |
| `RCursor::drop` releases locks | `CursorMut::drop` bumps the generation counter |
| PT-page descriptor lock + RCU monitor | **Rust `&mut VmSpace` exclusive borrow** |
| WF invariant (Fig. 12) | `pt_inv` path-link conjuncts in `vm_space_cursor.rs` |

The one **deliberate divergence**: SlopOS's concurrency control is the Rust
borrow checker, not CortenMM's per-PT-page locks + RCU. `CursorMut<'a>` holds
`&'a mut VmSpace`, so the type system guarantees **at most one mutator per
`VmSpace` at a time** — the coarse *lock-per-`VmSpace`* model. This is the
fallback Phase 3D.3 explicitly sanctions:

> "Falling back to a coarser lock-per-`VmSpace` proof is acceptable if the
> fine-grained one doesn't close — document the gap."

We take that fallback by construction. The consequences:

- **CortenMM's hardest proof obligation — P1 mutual exclusion across
  fine-grained PT-page locks + RCU stale-retry — does not arise in SlopOS.**
  There is no fine-grained locking to verify: `&mut VmSpace` *is* the mutual
  exclusion, discharged statically by `rustc`, no SMT needed. The RCU monitor,
  the stale-bit retry, the covering-PT-page DFS lock — none of it exists in
  the SlopOS cursor, so none of it needs a proof.
- **What remains to prove is CortenMM's P2**: page-table well-formedness and
  the functional correctness of `map`/`unmap` under the serialized op stream
  the exclusive borrow produces. That is what `proofs/vm_space_cursor.rs`
  machine-checks.
- **The gap vs. the fine-grained model**: SlopOS serializes *all* cursor
  mutations on one `VmSpace`, including those over disjoint ranges, where
  CortenMM would run them in parallel. This is a **scalability** difference,
  not a **soundness** one — the SlopOS model is strictly more conservative
  (it forbids strictly more concurrency), so every state reachable under it is
  reachable under CortenMM's looser model, and the well-formedness invariant
  we prove is therefore *a fortiori* valid. Re-attempt the fine-grained,
  range-disjoint parallel version on each Verus bump (3A.4) if SlopOS ever
  grows per-PT-page locking; until then the coarse model is the honest and
  sufficient one. Recorded in `STATUS.md`.

## 6. The invariants we adapt (3D.2)

Translating CortenMM's Figure-12 well-formedness + the SlopOS ref-count
discipline into the three Phase-3D obligations:

1. **PT well-formedness** — "Cursor operations preserve page-table
   well-formedness: no dangling intermediate frames; every present entry
   points at a valid lower-level table for the cursor's lifetime." Modelled by
   the path-link conjuncts `pt_linked ==> pd_linked`, `pd_linked ==>
   pdpt_linked`, `leaf_present ==> pt_linked` — a present deeper entry always
   has all shallower intermediates present and valid. This is CortenMM Fig. 12
   specialised to the SlopOS 4-level x86_64 walk (`walk_to_leaf` in
   `page_table.rs`, which in `WalkMode::Create` allocates + links intermediate
   tables top-down before touching the leaf, and never reclaims them on
   `unmap` — only on `VmSpace::drop`).
2. **Range-disjoint non-interference** — under the lock-per-`VmSpace` model,
   two cursors necessarily live on two distinct `VmSpace`s (you cannot hold two
   `&mut` to one), so their abstract states are independent values and stepping
   one cannot mutate the other. Captured by `disjoint_vmspaces_independent`.
   This is the coarse-model discharge of CortenMM's §3.3 semantics-(2).
3. **Map/unmap ref-count exactly once** — "Mapping a `UFrame` increments its
   `ref_count` exactly once; unmapping decrements exactly once. Inv. 4 + Inv. 5
   hold across the operation." Modelled by `pte_refs <= 1`, `leaf_present <==>
   pte_refs == 1`, and `leaf_present ==> leaf_is_uframe`. `CursorMut::map`
   leaks exactly one `UFrame` ref into the leaf PTE (`Frame::into_raw`);
   `unmap` reclaims exactly one (`Frame::from_raw_at`); the `Overlap` guard
   refuses a second map over a present leaf, so no double-leak; the
   not-present guard in `unmap` refuses a reclaim with no leaked ref, so no
   double-free. `map`'s `M: AnyUFrameMeta` / `UFrame<M>` argument type is the
   Inv. 4/Inv. 5 carrier — only insensitive user frames ever reach a user
   leaf.
