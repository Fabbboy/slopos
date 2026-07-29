# Finish the unconstrained-lifetime excision

## Intent

`KArc<Task>` is the single owning handle for every kernel task, the ownership
rules are machine-checked, and `scripts/check_task_ownership.sh` runs hard on
every `just check-framekernel`. Checks 1–7 read zero: no raw task pointer in a
binding, no `KernelSync<*mut Task>`, no `c_void` launder, no `task_borrow`, no
refcount-justified `Send`/`Sync`, no manual refcount, no fault-path lookup.

Check 8 is the residue. It catches a **shape**, not a name: a safe function
whose return type names a lifetime no argument constrains. `'a` is then chosen
by the caller, so two calls yield two simultaneously-live references to one
place — and for the `&mut` forms that is instant aliasing UB on the second call,
reachable from a `#![forbid(unsafe_code)]` crate with no `unsafe` in sight.

Three files are allowlisted in `CHECK8_ALLOWLIST`
(`scripts/check_task_ownership.sh:783`). **The allowlist may only shrink.** A
fourth, `sched/src/trap.rs`, is not allowlisted but calls the helpers and gates
their deletion.

## What the gate cannot see

Check 8 parses the function's own declared generic-lifetime list. Three shapes
are invisible to it, and a fix landing in any of them turns the gate green on
unchanged UB.

- **`&self` minting `&mut`.** `fn f(&self) -> &mut T` is elided and
  `fn f<'a>(&'a self) -> &'a mut T` is argument-constrained. Both read clean;
  both let a shared borrow hand out exclusivity, which is the entire defect in
  §2. **A green check 8 is not evidence §2 is done.**
- **`'static` substitution.** `fn f<T: 'static>(ptr: *mut T) -> &'static mut T`
  is silent and strictly worse than the caller-chosen form, because a `'static`
  borrow re-derives without bound at every call site. The `'static` carve-outs
  below rest on their documented caller obligation, not on the gate.
- **A named closure lifetime.** `impl FnOnce(&'a mut [T]) -> R` puts `'a` in the
  argument list and passes while preserving the exact hole the `with_*` family
  closes. That family is correct because its closure lifetime is higher-ranked.
  Do not add a lifetime parameter to `with_ref_mut`.

`--self-test` never consults the allowlist, which is applied only on the real-run
path (`:813`), and CI runs the real scan only — so a parser edit that breaks the
fixtures goes unnoticed unless `--self-test` is run by hand.

## Order

§1 is one line and unblocks §4. §2 is contained, kills six aliasing derivations,
and shrinks the two tightest stack frames in the tree. §3 is the multi-session
rewrite. §4 is deletion. Drop the `ptr_buf.rs` entry as early as its dependencies
allow: while it stands, a new caller-chosen-lifetime helper added to the one file
that defines the sanctioned shapes is invisible to the gate.

## 1. `sched/src/trap.rs` — the fourth `borrow_ref` caller

`save_preempt_context` calls `ptr_buf::borrow_ref` at `:73`, independent of §3.
It converts to the idiom already at five sites in `boot/`:

```rust
let frame_anchor = ();
let Some(frame_ref) = InterruptFrame::from_ptr(&frame_anchor, frame) else { return };
```

`InterruptFrame::from_ptr` (`slopos-ostd/src/irq/interrupt_frame.rs:44`) is the
worked example of the anchored shape and folds in the null check done by hand at
`:60-62`. Preserve the ordering: that check happens *before* the `Current` guard
is taken at `:64`.

## 2. `slopos-ostd/src/user/context.rs` — stop minting `&mut`

`UserContext::from_ptr_mut` is derived from a raw pointer at six places, five of
which never touch `SyscallContext`: `dispatch.rs:16`, `:67`, `:93`, `:140`,
`signal.rs:657`, and `SyscallContext::user_ctx_mut` (`context.rs:220`). Anchoring
`user_ctx_mut` on `&mut self` constrains only the sixth, costs a signature change
on all 132 `define_syscall!` handlers, and still reads clean to the gate.

The fix is to stop producing `&mut`, not to relocate the lifetime. Make
`UserContext` mutable through `&self` and let `SyscallContext` hold a borrow:

```rust
#[repr(C)]
pub struct UserContext {
    regs: core::cell::SyncUnsafeCell<UserRegs>,
    fpu_state: FpuStateRef,
}
const _: () = assert!(core::mem::offset_of!(UserContext, regs) == 0);

pub fn from_ptr<'a, A: ?Sized>(anchor: &'a A, ptr: *const UserContext) -> Option<&'a UserContext>;
pub fn regs(&self) -> UserRegs;          // by value
pub fn set_regs(&self, regs: UserRegs);  // &self
pub fn set_rax(&self, value: u64);       // &self
```

`SyncUnsafeCell<T>` is `#[repr(transparent)]`, so the `UserRegs` layout
`__ostd_user_return` indexes is untouched. `SyscallContext`'s `user_ctx_ptr:
*mut UserContext` becomes `user_ctx: &'a UserContext` — the pure-borrow struct
its own doc comment already claims it is — and `user_ctx_mut` / `user_ctx_ptr`
are deleted. **The handler signature `fn(&SyscallContext) -> SyscallResult` does
not change**, so no handler, macro arm or dispatch test is edited.

The risky step is the field flip, and it is silent: a wrong layout scrambles user
GPRs on the next round trip rather than failing to build. Land it alone, with
`from_ptr_mut` and the `&mut self` setters still in place so nothing outside the
file moves, and prove it with a full `just test` plus a boot to a shell. The rest
is mechanical: value-returning `regs()`, then the five dispatcher derivations,
then the `SyscallContext` field, then the two handler bodies
(`process_handlers.rs:415`, `signal.rs:356`), then `task_fork` / `task_clone`
taking `Option<&UserContext>`, then `UserMode<'a>`.

Three things decide correctness:

- `FpuStateRef`'s `Send`/`Sync` justification (`context.rs:145-152`) leans on
  `UserMode<'a>` holding `&'a mut UserContext`. The guarantee survives — the task
  owns the buffer and `execute(self)` consumes the wrapper — but the stated
  reason becomes a lie and must be rewritten.
- Nothing in the type stops two CPUs calling a `&self` setter. Today the syscall
  path is the only writer and runs on the task's own CPU; state that as the
  `__ostd_user_return` contract in `UserContext`'s doc, since `UserContext` is
  `Send + Sync` and cannot lean on `!Sync`.
- The legacy `int 0x80` adapter (`boot/src/idt.rs:391-466`) builds its
  `UserContext` on the kernel stack, not in the task struct — which is why the
  anchor must be frame-local and never a task witness. It is rarely exercised, so
  a regression there will not show in `just test`.

Deleting the `ctx_anchor` / `try_anchored_ref` pairs at `task_lifecycle.rs:1336`
and `:1554` makes this net-negative on the stack gate, in the two frames closest
to it: `task_fork` at 1944 B and `task_clone` at 1848 B against the 2048 B
ceiling. Re-measure anyway — `regs()` by value must not materialise copies in the
hot dispatcher path.

## 3. `mm/src/paging/tables.rs` — a value-carrying descent

The walks hold `&mut PageTableEntry` into the PML4, the PDPT, the PD and the PT
**simultaneously**. That is legitimate — four different tables — but it only
typechecks because each borrow's lifetime is fabricated independently.
`unmap_page_in_directory` is the clearest case: `pml4_entry` (`:453`) stays live
through `:536` while the walk descends to `pt_entry` (`:503`), because the
teardown-on-empty cascade clears each parent *after* descending.

The target: **no `&PageTable` outlives one statement; the descent state is
`(PhysAddr, usize)` per level, and entries move by value.** `PageTableEntry` is a
`Copy` `#[repr(transparent)]` u64 and phys→virt is free HHDM arithmetic, so this
costs nothing. `slopos-ostd/src/mm/page_table.rs` already writes the shape —
`walk_to_leaf` carries `Paddr` down with `Pte { raw: *mut u64 }` as a `Copy`
handle.

Four scoped accessors in `page_table_defs.rs` over the existing `with_*` family
(`with_ref_mut`, `ptr_buf.rs:290`, has no callers today and becomes the
workhorse): `entry_at`, `set_entry_at`, `table_empty_at`, `zero_table_at`. Map
becomes a straight-line descent over `ProcessPageDir::pml4_phys_from_raw`
(`:86`, already scoped). `split_pdpt_huge` / `split_pd_huge` take the parent
entry by value and return `Option<(PhysAddr, PageTableEntry)>` for the caller to
write, which removes the map path's only parent-and-child overlap. Unmap records
`[(PhysAddr, usize); 4]` on the way down, clears the leaf, flushes, then prunes
bottom-up off the array. No two references coexist because there are none.

`pml4_table` / `pml4_table_mut` are then deleted — both `pub`, neither called
outside the file — and the two read-only walks (`:293`, `:594`) move onto a
`PageTableWalker::walk_phys(&self, pml4_phys, vaddr)`.

Two defects must be fixed in the same restructure, because preserving statement
order preserves them:

1. **`map_page_in_directory` frees the old leaf frame before the flush** —
   `free_page_frame` at `:424`, new PTE at `:428`, flush at `:434`. In that
   window the frame is back in the buddy while every CPU may still hold a
   writable translation for `vaddr`. Reorder to write-PTE → flush → free.
2. **The prune's TLB invariant is unwritten.** Freeing an intermediate table is
   covered only because the earlier leaf `invlpg` and its `SinglePage` shootdown
   invalidated the paging-structure-cache entries along that linear address, and
   because the pruned table is empty. Sound as written, and thin. Put it in a
   comment at the prune site: batching the flush, or moving a free ahead of it,
   breaks it silently.

Constraints:

- `PageTableWalker::next_table_mut` (`walker.rs:137`) is not the primitive to
  build on: it ties one table to `&mut self`, so taking the PDPT kills the PML4
  borrow and the prune cascade cannot be expressed.
- Do not nest four `with_ref_mut` closures to avoid the path array. Closure
  captures are what pushed `task_fork` / `task_clone` against the 2 KiB gate
  (`task_lifecycle.rs:1333-1335`). The flat array is ≤64 B against ~1300 B of
  slack.
- `table_empty` (`:164`) tests `!e.is_present()`; `PageTable::is_empty`
  (`page_table_defs.rs:232`) tests `is_unused()`. `table_empty_at` must delegate
  to the former, or the set of freed tables changes.
- Do not add a lock over `KERNEL_PAGE_DIR`, and do not reach for an
  Asterinas-style guard-stack cursor. Both put `alloc_page_table` → buddy → LUF
  cross-CPU drain under a cli-lock, which is the open deadlock. The guard-stack
  cursor is the right long-term target for the OSTD `VmSpace` — where
  `verification/proofs/vm_space_cursor.rs` already names the coarse `&mut
  VmSpace` model as a gap — and belongs in its own plan.
- Keep the frame free to a single call site. Immediate free is defensible on an
  IPI-flushing arch, but only as long as shootdown target-set correctness holds;
  the moment a lockless page-table walker appears, deferred free (Linux's
  `MMU_GATHER_RCU_TABLE_FREE`, Asterinas's `RcuDrop`) becomes mandatory, and one
  call site makes that one line.
- Page-table frames are raw `PhysAddr` with manual free and no refcount, so the
  path array is the only ledger: a lost phys leaks a page, a double-tracked one
  corrupts the buddy. Review its `depth` bookkeeping across the 1 GiB, 2 MiB and
  4 KiB branches hardest — the prune depths differ.

`init_paging`'s `borrow_ref_mut::<ProcessPageDir>` (`:553`) converts with the
rest; split the body so `virt_to_phys` (`:562`, `:572`) — which re-borrows the
same static — runs after the closure returns.

## 4. `slopos-ostd/src/util/ptr_buf.rs` — deletion

After §1 and §3, delete `borrow_ref` (`:255`) and `borrow_ref_mut` (`:263`);
`cargo build` is the test. Sweep the comments naming them (`tables.rs:84`, `:105`,
`:179`, `:225`, `:324`; `walker.rs:104`, which says `borrow_ref` while the code
calls `anchored_ref`; `trap.rs:70`) and the doc cross-references inside
`ptr_buf.rs`. Then drop the last allowlist entry and delete
`filter_check8_allowlist`, the array, and the residue header with it.

## The shapes to use instead

Already in `ptr_buf`, and the right one differs per call site:

- **`with_*`** — scoped closure, for callers that consume the borrow and return
  something else. The argument lifetime is higher-ranked: the caller cannot name
  it, so it cannot choose it twice. This is the target for `tables.rs`, because
  no borrow needs to escape.
- **`anchored_*`** — for accessors that must *return* a reference, like
  `as_slice(&self)` over a buffer the receiver owns. `&mut A` for the mutable
  forms, so exclusivity is the anchor's too. **Only as strong as its anchor**: a
  token (`&()`, `&len`) recovers whatever lifetime the caller wants and satisfies
  check 8 while constraining nothing. Seven of thirteen in-tree sites are tokens;
  the honest ones are `windowing/src/memfd_buf.rs:75` (`self` owns the mapping)
  and `mm/src/paging/walker.rs:137` (`&mut self` *is* the exclusivity). "Migrate
  to `anchored_*`" is not by itself a soundness argument.
- **`'static`** — only for *shared* references to data that genuinely is never
  freed: a linker section, the bootloader command line, a registry-published
  device handle. **Never for a `&mut` form.**
- **`install_buf_mut`** — one-shot install of a region that is never freed, with
  the one-shot property named as a caller obligation because the type cannot
  carry it.

## Verification

Every commit: `cargo fmt --all`, `just build`, full green `just test`.

- Clear `builddir/.kernel-elf-gates.stamp` before `just build` — the ELF gates
  cache on ELF hash and print `skipped` otherwise.
- Read `just test`'s **pass count**, not its exit code. Baseline **2701 passed,
  0 failed**.
- Confirm `/dev/kvm` before trusting any full-suite failure: no KVM means a
  silent TCG fallback and a panic-recovery NMI hang that reads as a real
  regression.
- §2's field flip wants `just boot-log` to a shell — a scrambled `UserRegs`
  layout fails no build and may fail no test.
- §3 wants `just boot-log` ×3, exercising the 1 GiB, 2 MiB and 4 KiB branches — a
  broken unmap path does not necessarily fail a test, and it does not boot to a
  shell.
- Re-run `scripts/check_stack_sizes.sh` after §2 and §3, and
  `check_task_ownership.sh --self-test` after any header or regex edit.

## Acceptance

- Check 8 reads zero with an empty `CHECK8_ALLOWLIST`, and the allowlist
  machinery is deleted with it.
- **No `&mut UserContext` is derived from a raw pointer anywhere in
  `core/src/syscall/`** — check 8 cannot see that shape, so it is not §2's
  criterion.
- No caller of `ptr_buf::borrow_ref` or `borrow_ref_mut` remains in any crate.
- Every framekernel gate green, the Verus set green, full `just test` green
  under KVM.

Then delete this plan.

## Prior art

Rust-for-Linux's `Opaque<T>` — `get(&self) -> *mut T`, deliberately no `get_mut`
— and `VmaNew`'s `set_io(&self)` are §2 exactly; the `PidNamespace` commit
(e0020ba6cbcb) names the failure mode: "could be abused to created an unbounded
lifetime". Redox copies syscall argument registers out of `*mut InterruptStack`
into scalars rather than threading the frame, and its `rmm` `PageTable::next`
returns a new owned value — a cursor that is a value, not a borrow. Linux derives
each page-table level from the parent's *value* (`pmd_offset(pud, addr)` is
`pud_pgtable(*pud) + pmd_index(addr)`; `__pte_offset_map` snapshots first), and
`free_pte_range` is the read-value / clear-parent / defer-free triple. The
`x86_64` crate anchors each child on the parent entry's borrow, which is why its
`unmap` cannot prune and `clean_up_addr_range` re-walks returning `bool` upward.
The Rustonomicon's "Unbounded Lifetimes" is `borrow_ref_mut` with `&mut`.

In-tree: `walk_to_leaf` and `Pte` in `slopos-ostd/src/mm/page_table.rs` are §3's
shape already written, `InterruptFrame::from_ptr` is §1's and §2's, and
`with_parked` / `with_parked_node` in `slopos-ostd/src/task/placement.rs` are the
scoped shape applied to task nodes.
