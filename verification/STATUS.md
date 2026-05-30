# OSTD Verification Status

The coverage map for the trusted core. `just verify` proves the proofs that
*exist*; this file is the honest record of what is and isn't covered — the
negative space the gate can't tell you about.

- **Pinned Verus**: `0.2026.05.24.ecee80a`
  (commit `ecee80a2139923d503338e6989f79fb690ec7847`) — see `verus.toml`.
- **Status legend**:
  - **verified** — a Verus proof in `proofs/` machine-checks the module's
    load-bearing invariants on the pinned toolchain.
  - **audited only** — manually reviewed against the soundness invariants
    and covered by KernMiri (`just check-miri`) + `// SAFETY:` annotations
    (every `unsafe` block names ≥1 of Inv. 1–10); no machine-checked proof.
  - **unaudited** — pure safe Rust, no `unsafe`; sound by the type system,
    no Inv. obligation.

## Coverage map

### Verified

The OSTD memory-safety critical path: the three places a bug would be UB
rather than a typed error. Per-obligation detail lives in each proof file's
own doc-comments — not duplicated here.

| Module | Proof file | Proves |
|---|---|---|
| `slopos_ostd::mm::frame` | `proofs/frame_refcount.rs` | `Frame<M>` ref-count: no double-free, no use-after-free, `ref_count > 0` ⇒ allocated (I1–I3) |
| `slopos_ostd::mm::slab` | `proofs/slab_lifetime.rs` | `HeapSlot` lifetime: a slot can't outlive its slab (Inv. 9); a cell fits any in-range type (Inv. 10) |
| `slopos_ostd::mm::vm_space` | `proofs/vm_space_cursor.rs` | `Cursor`: page-table well-formedness, balanced map/unmap, user leaves are insensitive `UFrame`s (Inv. 4 + 5) |

> `mm::vm_space` uses the coarse lock-per-`VmSpace` model (`CursorMut<'a>`
> holds `&'a mut VmSpace`, so the borrow checker serializes all mutators —
> Phase 3D.3's sanctioned fallback). This is a *scalability* gap vs.
> CortenMM's range-disjoint parallelism, not a *soundness* one; see
> `notes/cortenmm.md`. Re-attempt the fine-grained proof on each Verus bump
> if SlopOS ever grows per-PT-page locking.

### Audited only

All other `unsafe`-carrying OSTD modules — the load-bearing remainder.
Reviewed against the invariants, KernMiri-covered, but **not**
machine-checked. Full-OSTD Verus verification is out of scope; critical-path
verification gets the bulk of the credibility for a fraction of the cost.

`arch` · `boot` · `cpu` · `dma` · `io` · `irq` · `sync` · `task` · `user` ·
`util` · the `mm` remainder (`frame_alloc`, `heap`, `hhdm_bytes`, `io_mem`,
`page_table`, `phys`, `tlb`, `uframe`) · `acpi` · `dev` · `ffi` · `pci` ·
and the top-level support modules carrying contained `unsafe` (`memory`,
`string`, `ring_buffer`, `bitmap`, `atomic_bitmap`, `stacktrace`,
`panic_recovery`, `early_console`).

### Unaudited

Pure safe Rust within OSTD — no `unsafe`, no Inv. obligation, sound by the
type system: `handle` (generation-counter `Handle<T>`/`HandleTable<T>`),
the POD/zeroable markers, boot-handoff data types, and the safe helper
modules (`bitmap_slice`, `numfmt`, `wl_currency`, `kdiag`, `klog`,
`test_support`).

## Public claim

> **SlopOS is the smallest verified-TCB Linux-ABI Rust kernel.**

The claim is the conjunction of four adjectives; no other kernel meets all
four at once:

| Kernel | Small TCB | Verified | Linux-ABI | Rust |
|---|:--:|:--:|:--:|:--:|
| **SlopOS** | ✅ <1 % | ✅ critical path | ✅ | ✅ |
| seL4 | ✅ | ✅ whole kernel | ❌ | ❌ C |
| Asterinas | ➖ ~14 % | ➖ in progress (vostd) | ✅ | ✅ |
| Theseus | ❌ ~62 % | ❌ | ❌ | ✅ |
| Linux RFL | ❌ multi-MLoC | ❌ | ✅ | ➖ Rust-in-C |
| Hubris | ✅ | ❌ | ❌ | ✅ |

Sources: Asterinas (USENIX ATC '25, arXiv:2506.03876 — ~14 % TCB, vostd
ongoing); seL4 (SOSP '09 — verified C, no Linux ABI); Theseus (OSDI '20 —
~62 % TCB, unverified). SlopOS TCB ratio is measured live by
`scripts/tcb_ratio.sh`; the Linux-ABI syscall surface is in `abi/`. Full
citation list in the plan's § 10 *References*.
