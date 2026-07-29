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

Three sites remain, allowlisted by file in the gate with the same reasons given
below. **The allowlist may only shrink.**

## The three

### 1. `mm/src/paging/tables.rs` — `pml4_table` / `pml4_table_mut`

The walks in that file hold `&mut PageTableEntry` into the PML4, the PDPT, the
PD and the PT **simultaneously**. That is legitimate — they are four different
tables — but it only typechecks because each borrow's lifetime is fabricated
independently. `unmap_page_in_directory` is the clearest case: it holds
`pml4_entry` live while it walks down to `pt_entry`, then clears the parents on
the way back up.

Scoping the borrows means restructuring the walk to carry *physical addresses*
down and re-borrow at each mutation point, rather than holding four live `&mut`.
That is a real change to the kernel's unmap path and wants its own session with
`just test` and boot verification, not a signature edit.

`PageTableWalker::next_table{,_mut}` already show the target shape: the borrow
is tied to the walker, and `next_table_mut` takes `&mut self` so two mutable
views cannot coexist.

### 2. `slopos-ostd/src/user/context.rs` — `UserContext::from_ptr{,_mut}`

`SyscallContext::user_ctx_mut` takes `&self` and returns the borrow out, so the
honest anchor for the result is `&mut self`. Making it so means threading
`&mut SyscallContext` through every syscall handler — a wide, mechanical change
to a surface that is otherwise stable, and one that should be judged on its own
merits rather than smuggled in behind a lifetime cleanup.

`InterruptFrame::from_ptr{,_mut}` are the worked example of the fix: they take
an anchor, and the `_mut` form takes `&mut A` so the frame's exclusivity is the
anchor's.

### 3. `slopos-ostd/src/util/ptr_buf.rs` — `borrow_ref` / `borrow_ref_mut`

Kept solely for (1); marked "no new caller" at the definition. They go with it.

## The shapes to use instead

Already in `ptr_buf`, and the right one differs per call site:

- **`with_*`** — scoped closure, for callers that consume the borrow and return
  something else. The argument lifetime is higher-ranked: the caller cannot name
  it, so it cannot choose it twice.
- **`anchored_*`** — for accessors that must *return* a reference, like
  `as_slice(&self)` over a buffer the receiver owns. The lifetime is the
  anchor's, so the caller has to present something that genuinely outlives the
  borrow. `&mut A` for the mutable forms, so exclusivity is the anchor's too.
- **`'static`** — only for *shared* references to data that genuinely is never
  freed: a linker section, the bootloader command line, a registry-published
  device handle. **Never for a `&mut` form** — two `&'static mut` to one place
  is still aliasing UB, and passing check 8 that way is the token substitution
  the gate's preamble warns about.
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
- `mm/src/paging/tables.rs` additionally wants `just boot-log` ×3 — a broken
  unmap path does not necessarily fail a test, and it does not boot to a shell.
- Re-run `scripts/check_task_ownership.sh --self-test` after any header or regex
  edit, and shrink `CHECK8_ALLOWLIST` as each file is finished.

## Acceptance

- Check 8 reads zero with an empty `CHECK8_ALLOWLIST`, and the allowlist
  machinery is deleted with it.
- Every framekernel gate green, the Verus set green, full `just test` green
  under KVM.

Then delete this plan.

## Prior art

Rust-for-Linux's `Opaque<T>` — `get(&self) -> *mut T`, never `&mut T`, for the
same aliasing reason `TaskOwnCell::get_ptr` has it. In-tree: `with_parked` and
`with_parked_node` in `slopos-ostd/src/task/placement.rs` are the scoped shape
applied to task nodes, and `PageTableWalker::next_table_mut` is it applied to
page tables.
