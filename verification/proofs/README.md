# Verus proofs

Each `*.rs` file directly in this directory is a **standalone Verus
crate-of-one** that mirrors a slice of `slopos-ostd` under `verus! { ... }`
and states its load-bearing invariants with `requires` / `ensures` /
`invariant`. `just verify` (`scripts/verify.sh`) runs the pinned Verus
toolchain over each one and fails on any unverified obligation.

## Conventions

- One proof file per OSTD critical-path module. Name it after the module:
  `frame_refcount.rs`, `slab_lifetime.rs`, `vm_space_cursor.rs`.
- A file whose name starts with `_` is a **shared helper module** — it is
  `include!`d by the proofs that need it and is **not** run as a top-level
  entry point by `verify.sh`.
- Proofs `use vstd::prelude::*;` and therefore only build under the Verus
  toolchain, never under the kernel's nightly. They are not cargo targets.
- When a proof closes, update `../STATUS.md` (mark the module **verified**
  against the pinned Verus SHA) and port the Verus-annotated source back
  into `slopos-ostd` (Verus emits a normal build for the kernel) so
  `just build` + `just test` still pass.

`frame_refcount.rs` (Phase 3B) is the first proof to land: 10 obligations
machine-checking the `Frame<M>` reference-count invariants (I1 allocated-
while-referenced, I2 release-exactly-once, I3 no clone/drop use-after-free,
I4 free-listed ⇒ slot reset-before-free) plus two load-bearing witnesses:
the broken `fetch_add(1)` clone violates I1–I3, and the pre-fix
free-before-reset `Drop` ordering violates I4 (the SMP `from_unused` /
`PathCorrupt` bug).

`slab_lifetime.rs` (Phase 3C) is the second: 11 obligations machine-checking
the slab object lifecycle — Inv. 9 (a slot cannot outlive its parent slab:
an outstanding cell pins its page) and Inv. 10 (a slot is only used for an
object that fits its size + alignment) — plus load-bearing witnesses that an
unconditional page reclaim breaks Inv. 9 and an always-smallest size class
breaks Inv. 10.

`vm_space_cursor.rs` (Phase 3D) is the third: 12 obligations machine-checking
the `VmSpace::cursor` page-table mutation path — (WF) page-table
well-formedness (no dangling intermediate frames; CortenMM SOSP '25 Fig. 12
for the 4-level x86_64 walk), (REF) `map` leaks one `UFrame` ref and `unmap`
reclaims one exactly, and (Inv. 4 + Inv. 5) a present user leaf is always an
insensitive `UFrame` — plus load-bearing witnesses that removing the
`Overlap` guard double-leaks and that accepting a raw `Frame` lets a
sensitive frame reach a user PTE. The proof uses the coarse
lock-per-`VmSpace` model (the Phase-3D.3 fallback: `CursorMut` holds
`&mut VmSpace`, so the borrow checker serializes mutators); see
`../notes/cortenmm.md` for the design mapping and the gap vs. CortenMM's
fine-grained per-PT-page locking. See `../STATUS.md` for the per-obligation
summaries.

`ring_cursor.rs` (Phase 7) is the fourth: ~13 obligations machine-checking
the SlopRing SQ/CQ cursor + in-flight state machine — INV-CQ-no-overwrite
(`cq_tail - cq_head <= cq_entries`), INV-CQ-full-correctness (the post /
overflow branches are mutually exclusive + exhaustive), INV-overflow-monotone
-latch (the dropped-CQE counter only grows; the sticky flag is one-way),
INV-cq-tail-advance-exactly-one, INV-inflight-cap (no over-push, no
underflow-on-remove; the SLOPRING § 9 / slab Inv. 9 analogue), and
INV-submit-consume-bound (`sq_head` never passes the user-published
`sq_tail`) — plus a load-bearing, non-vacuous witness that a `post_cqe`
without the `cq_full` check overwrites an unharvested CQE. The user-owned
cursors (`sq_tail`, `cq_head`) are modelled as adversarial-monotone `Step`s,
which is what keeps the no-overwrite obligation honest rather than vacuous.
The proof verifies the index/state-machine **logic only**; the volatile
`UFrame` shared-memory accessors and the release/acquire memory-ordering
protocol beneath it remain audited-only / KernMiri-covered (Verus has no
weak-memory model) — see the file header and `../STATUS.md`.

`ring_layout.rs` (Phase 7) is the fifth: ~5 obligations machine-checking the
SlopRing layout + masking arithmetic — LEMMA-mask-in-bounds (for power-of-two
`n`, `i & (n - 1) < n`, via the `bit_vector` backend, with a documented
`i % n` nat-arithmetic fallback model the orchestrator can switch to if the
bit-vector lemma proves brittle on the pin) and LEMMA-locate-safe
(`RingRegion::locate`'s no-OOB / no-straddle guard as pure arithmetic). It
isolates the bit-vector friction in a separate file so it can never block
`ring_cursor.rs`, and carries an optional STRETCH `layout_disjoint_fits`
block (delete it if it does not discharge). See `../STATUS.md`.

`tcp_zc_pin.rs` machine-checks the TCP `MSG_ZEROCOPY` send-queue pin lifetime:
~8 obligations over an abstract `SendQ` state machine
(`Send`/`Transmit`/`Reclaim`/`Ack`/`Teardown`) proving
INV-TCPZC-PIN-IN-BOUNDS (every in-flight zero-copy segment's read window stays
inside its pin, so every transmit / retransmit re-DMA / copy-fallback reads only
its own pinned bytes) and INV-TCPZC-HELD-UNTIL-ACK (a `Transmit`/`Reclaim` never
drops a segment, and an `Ack` frees a head segment only once fully cumulatively
ACKed — no free before ACK / mid-retransmit), plus a non-vacuity witness. The
state-machine **logic only**: the runtime refcounted `ZcNotifToken` weak-memory
protocol (buffer-reusable `F_NOTIF` signal) is audited-only / KernMiri-covered
(Verus has no weak-memory model) — see the file header and `../STATUS.md`.

`task_ownership.rs` machine-checks the task lifetime's ownership core —
`slopos_ostd::task::placement` (the existence reference plus the four placement
primitives), `KArc`'s deferred strong-release split
(`release_deferrable` / `destroy_deferred`), `task_reclaim`'s `task_put` and
graveyard, and `task_table`'s `register_task` / `reap_task_registration` gates.
23 obligations over an abstract `TaskOwn` ledger driven by a 14-variant `Step`
machine, proving T1 the existence reference is flag-elected (never
count-elected) and parked/released at most once, T2 container transitions
conserve the strong count so linked implies owned, T3 registered ⟺ holds its
existence reference (what lets the registry be a pure `KWeak` index), T4 no
use-after-free, T5 exactly one caller wins the 1→0 release and destruction runs
exactly once, T6 a reap never fires on a dispatch-pinned task, and T7
destruction implies full detachment. The invariant is split in two on purpose:
`own_inv` survives every *sub*-step of a decomposed transition, while the two
biconditionals in `flag_agrees` cannot — `register_task` inserts the registry
entry before parking the reference and `reap_task_registration` releases the
reference before removing the entry, opposite orders under the same lock — so
carrying them separately is what keeps the model honest. Five load-bearing
witnesses, each pairing an `exists` that the broken shape violates the invariant
with a `forall` that the real step preserves it: a release elected by reading
`strong_count == 1`, a park that claims the flag before retaining, a reap that
unhashes before winning the release, a reap that ignores the dispatch-pin gate,
and a destroy that never won the one-to-zero. The flagship witness state is
additionally proved *reachable* by an explicit five-step trace. The
state-machine **logic only**: the flag CAS and `KArc` CAS-loop memory ordering,
the intrusive links, pointer provenance, and `KArc` saturation stay audited-only
/ KernMiri-covered — see the file header and `../STATUS.md`.

The narrative obligation total is 105 across the nine files, confirmed by
`just verify` on the pinned Verus — `verify.sh` auto-sums the exact `N verified`
count it reports per file.
