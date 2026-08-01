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
| `slopos_ostd::mm::frame` | `proofs/frame_refcount.rs` | `Frame<M>` ref-count: no double-free, no use-after-free, `ref_count > 0` ⇒ allocated, free-listed ⇒ slot reset-before-free (I1–I4) |
| `slopos_ostd::mm::slab` | `proofs/slab_lifetime.rs` | `HeapSlot` lifetime: a slot can't outlive its slab (Inv. 9); a cell fits any in-range type (Inv. 10) |
| `slopos_ostd::mm::vm_space` | `proofs/vm_space_cursor.rs` | `Cursor`: page-table well-formedness, balanced map/unmap/map_kernel/map_io, user-visible leaves are insensitive frames (Inv. 4 + 5) |
| `slopos_ostd::task` ownership core **LOGIC** | `proofs/task_ownership.rs` | Task ownership: the existence reference is flag-elected and parked/released at most once (T1), container transitions conserve the strong count (T2), registered ⟺ holds its existence reference (T3), no use-after-free (T4), exactly one winner of the 1→0 release with destruction exactly once (T5), a reap never fires on a dispatch-pinned task (T6), destruction implies full detachment (T7) |
| `slopos_ring` index/state-machine **LOGIC** | `proofs/ring_cursor.rs` + `proofs/ring_layout.rs` | SlopRing cursors: CQ no-overwrite, CQ-full correctness, overflow monotone-latch, cq_tail advance-exactly-one, in-flight cap, submit/consume bound; masked SQE/CQE indices in bounds + `locate` no-OOB/no-straddle |
| `slopos_net::tcp` zero-copy send queue **LOGIC** | `proofs/tcp_zc_pin.rs` | TCP `MSG_ZEROCOPY` pin lifetime: every (re)transmit reads in-bounds of its pin (INV-TCPZC-PIN-IN-BOUNDS); a pin is held across retransmits and freed only on cumulative ACK / teardown, never mid-DMA (INV-TCPZC-HELD-UNTIL-ACK) |

> `mm::vm_space` uses the coarse lock-per-`VmSpace` model, in two tiers. The
> borrow checker admits one `CursorMut` per `VmSpace` *object* (`CursorMut<'a>`
> holds `&'a mut VmSpace`) — Phase 3D.3's sanctioned fallback. That alone is
> not the whole-system guarantee: it says nothing about a second walker over
> the same *physical* page tables reached some other way. So every `VmSpace`
> shared across CPUs lives behind the lock that is the sole minter of the
> `&mut` — `PROCESS_VMS[slot]` per process, `KERNEL_VM_SPACE` for the kernel
> master — and no other writer of those tables exists.
> `scripts/check_kernel_pml4_writer.sh` fails the build if one reappears; the
> tier-2 argument is enforced there, not in prose.
>
> The Verus block covers all four mutators — `map`, `map_kernel`, `map_io`,
> `unmap` — with `Inv. 4 + 5` conditioned on user visibility, plus three
> broken-variant witnesses (`broken_double_leak`, `broken_map_kernel_user`,
> `broken_unmap_reclaims_io`) showing the `Overlap` guard, `map_kernel`'s
> `!prop.user` guard, and `unmap`'s software-bit branch are each
> load-bearing. It does **not** read the surrounding prose: the two-tier
> premise above is reviewed text, not a checked claim.
>
> This is a *scalability* gap vs. CortenMM's range-disjoint parallelism, not a
> *soundness* one; see `notes/cortenmm.md`. Re-attempt the fine-grained proof
> on each Verus bump if SlopOS ever grows per-PT-page locking.

> `slopos_ring` verifies the **index/state-machine LOGIC only** — the
> abstract SQ/CQ cursor arithmetic, overflow accounting, in-flight cap,
> submit/consume bounds, and the masked-index / `locate` in-bounds algebra.
> The kernel is modelled as a single sequential `Step` machine, sound because
> every ring mutation runs under the per-ring SpinLock; the user-owned
> cursors (`sq_tail`, `cq_head`) are modelled as adversarial-monotone inputs.
> The volatile `UFrame` accessors beneath it and the kernel/userland
> release/acquire memory-ordering protocol are **NOT** machine-checked: they
> remain audited-only and KernMiri-covered (see the Phase-3G paragraph
> below). The proof covers the kernel's half of the protocol, not a malicious
> user racing the shared cells at the memory level.

> `slopos_net::tcp` zero-copy send queue verifies the **send-queue
> state-machine LOGIC only** — the abstract `SendQ` over
> `Send`/`Transmit`/`Reclaim`/`Ack`/`Teardown`, proving every in-flight
> zero-copy segment's read window stays inside its pin and a pin is freed only on
> cumulative ACK / teardown (never mid-retransmit). The runtime refcounted
> `ZcNotifToken` (the driver→ring buffer-reusable `F_NOTIF` signal: one reference
> per in-flight DMA plus the send-queue chunk's, reaching zero only after ACK +
> all reclaims) is an atomic weak-memory protocol Verus cannot model — it remains
> audited-only and KernMiri-covered, like the `slopos_ring` accessors above.

> `slopos_ostd::task` verifies the **ownership state-machine LOGIC only** — the
> strong-count ledger split by owner class, the existence-reference flag
> election, the registry and dispatch-pin gates, and the deferred-destruction
> accounting, modelled as one atomic-bounded `Step` machine over an abstract
> `TaskOwn`. The weak-memory ordering of the `existence_ref_parked`
> compare-exchange and of `KArc`'s `refcount_release` CAS loop, the intrusive
> placement links, and the provenance of the raw pointers
> `task_placement_leak`/`_reclaim` hand around are **NOT** machine-checked:
> Verus has no weak-memory model. Neither is the rest of the module's `unsafe`
> surface — `cell`, `accessors`, `switch`, `fpu`, `kernel_task` — which the
> ownership model does not reach at all. `KArc`'s saturation arm (a count at
> `isize::MAX` never destroys, so the allocation leaks rather than freeing) and
> the weak count behind `KWeak::upgrade` are excluded from the model rather
> than proved; a registry entry is the bare boolean `registered`. All of that
> remains audited-only and KernMiri-covered (`tests/task_existence.rs`,
> `tests/karc_deferred.rs`, `tests/task_cells.rs`, `tests/intrusive_list.rs`).
> Where the model diverges from the tree it is deliberately *more permissive*:
> the reap step drops the real gate's `TaskStatus::Terminated` and
> entry-present preconditions, so the proof covers a superset of the real
> behaviours. The registry-reset fixture path (`force_reap_registration`),
> which bypasses the status and dispatch-pin gates on purpose, is out of model.

### Audited only

All other `unsafe`-carrying OSTD modules — the load-bearing remainder.
Reviewed against the invariants, KernMiri-covered, but **not**
machine-checked. Full-OSTD Verus verification is out of scope; critical-path
verification gets the bulk of the credibility for a fraction of the cost.

`arch` · `boot` · `cpu` · `dma` · `io` · `irq` · `sync` · `user` ·
`util` · the `mm` remainder (`frame_alloc`, `heap`, `hhdm_bytes`, `io_mem`,
`page_table`, `phys`, `tlb`, `uframe`) · `acpi` · `dev` · `ffi` · `pci` ·
and the top-level support modules carrying contained `unsafe` (`memory`,
`string`, `ring_buffer`, `bitmap`, `atomic_bitmap`, `stacktrace`,
`panic_recovery`, `early_console`).

The Phase-3G SlopRing accessor (`mm::uframe::{load_u32_acquire,
store_u32_release, copy_out_volatile, copy_in_volatile}`) is the only new
OSTD `unsafe` added since the proofs landed: ~6 lines of `read_volatile`/
`write_volatile` + acquire/release fences, each with a `// SAFETY:` note
naming Inv. 4/5, KernMiri-covered (`tests/uframe_round_trip.rs`). The
`ring/` kernel crate that consumes it is `#![forbid(unsafe_code)]` and so
needs no audit — it reaches ring memory only through this verified-by-Miri
byte-copy surface (AD-3 / Inv. 4/5). The Phase-7 SlopRing proofs
(`proofs/ring_cursor.rs`, `proofs/ring_layout.rs`) machine-check the ring's
index/state-machine **logic only** (cursor bounds, overflow accounting,
in-flight cap, in-bounds masking); the four `mm::uframe` accessors above and
the release/acquire memory-ordering protocol they sit on top of **remain
audited-only / KernMiri-covered, NOT machine-checked** — Verus has no
weak-memory model, so that boundary is excluded from the proof rather than
verified.

### Unaudited

Pure safe Rust within OSTD — no `unsafe`, no Inv. obligation, sound by the
type system: `handle` (generation-counter `Handle<T>`/`HandleTable<T>`),
the POD/zeroable markers, boot-handoff data types, and the safe helper
modules (`bitmap_slice`, `numfmt`, `wl_currency`, `kdiag`, `klog`,
`test_support`).

## The soundness invariants

Every `// SAFETY:` comment and the whole *audited only* classification are
written in this vocabulary, so it belongs somewhere readable rather than in
the reader's head. The numbering follows Asterinas's ATC '25 §4.3, which
SlopOS's framekernel structure is taken from; what each one means *in this
tree* is:

| | Invariant |
|---|---|
| **Inv. 1** | A frame slot is never handed out while already live, and its refcount tracks exactly the live handles. |
| **Inv. 2** | Userland cannot forge privileged CPU state. `UserContext` masks every flag or register field that would let a user task step outside its sandbox. |
| **Inv. 3** | An `asm!` block's operand widths and clobbers match the instruction it names. |
| **Inv. 4** | Page-table walks stay well-formed: every present entry names a real table or frame, and map/unmap are balanced. |
| **Inv. 5** | A user-visible leaf mapping only ever names an insensitive frame — never kernel-sensitive memory. |
| **Inv. 5'** | No stack frame exceeds 2 KiB. Enforced by `check_stack_sizes.sh` against the final ELF of every build variant, each against its own measured allowlist, with a second ceiling at the 4 KiB guard page that no allowlist can raise. |
| **Inv. 6** | A DMA or IOMMU mapping is only used through the handle that created it, for as long as the device may still read it. |
| **Inv. 7** | Only I/O ports the platform has marked insensitive are reachable, and only through `IoPortRegistry`. |
| **Inv. 8** | The calling CPU is the sole accessor of the task state it touches. |
| **Inv. 9** | A slab slot cannot outlive the slab it came from; an outstanding cell pins its page. |
| **Inv. 10** | A slab slot is only ever used for a type that fits its size class. |

## What is enforced, and by what

`#![forbid(unsafe_code)]` on every kernel crate is necessary but not
sufficient. rustc drops any `unsafe_code` diagnostic whose primary span
satisfies `in_external_macro`, and `UNSAFE_CODE` does not declare
`report_in_external_macro` — so a macro defined in another crate expands
`unsafe` into a forbid crate with zero diagnostics, and the call site holds
no keyword for a source scan to find. `--force-warn unsafe_code` does not
change this; it fires in the defining crate and stays silent at the call
site. Asterinas has the same hole at larger scale and its ATC '25 paper
names no enforcement mechanism at all.

Three gates carry the claim between them:

- `check_unsafe_outside_ostd.sh` — no `unsafe` keyword is *authored* outside
  `slopos-ostd`, and every crate in the kernel binary's dependency closure
  carries the lint attribute.
- `check_unsafe_expansion.sh` — no `unsafe` *reaches the compiler* in a
  kernel crate beyond an allowlisted `unsafe impl`, `link_section` or
  `no_mangle`, each with a recorded reason. Run over each crate's feature
  configurations, because the ~2 700 `stest!` registrations live behind
  test features.
- `check_registry_sections.sh` — `kernel.elf` holds only the sections
  `link.ld` declares, each registry a whole number of entries. This is the
  only gate that sees a *dependency's* `link_section`.

## Trusted surface

The honest measure is not how many times the keyword appears — folding
`unsafe` behind a sound safe wrapper is the framekernel design, not an
evasion. It is how much of OSTD's exported API has to be trusted by reading:

| Surface | Count |
|---|---:|
| `pub unsafe fn` | 54 |
| `pub unsafe trait` | 15 |
| safe `pub fn` carrying a `# Safety` section | 0 |

The last row is the one that matters most and the one being driven down:
a `# Safety` section on a function that is not `unsafe fn` is a written
admission that a safe caller can break it. `check_safe_contract_surface.sh`
ratchets it.

`scripts/tcb_ratio.sh` reports 0.52 % unsafe-line density over the kernel
binary's dependency closure. Read it as a trend. It is **not** comparable to
the TCB fractions other projects publish: Asterinas's ~14 % is LCS over
post-LTO linked code including its dependency closure, while this
denominator is raw first-party LoC — and 41 kLoC of it is the vendored DWARF
reader, which contributes 20 unsafe lines.

## Vendored annexes

`vendor/unwinding` (3 308 LoC, 177 unsafe) and `vendor/gimli` (41 494 LoC,
20 unsafe) link into `kernel.elf` and are exempt from every source gate.
They are trusted code that is neither verified nor audited here; their
integrity rests on `check_vendor_pin.sh`, which pins each to an upstream
commit and a content hash. Both are counted in the TCB ratio's numerator
and denominator.
