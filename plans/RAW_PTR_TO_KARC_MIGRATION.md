# Finish the unconstrained-lifetime excision

## Intent

`scripts/check_task_ownership.sh` check 8 flags a safe function whose return type
names a generic lifetime no argument constrains. The caller then picks `'a`, so
two calls yield two simultaneously-live references to one place — and for the
`&mut` forms that is instant aliasing UB on the second call, reachable from a
`#![forbid(unsafe_code)]` crate with no `unsafe` in sight.

Two hits remain, and they are the pair the shape is named for:

```
slopos-ostd/src/util/ptr_buf.rs:255  borrow_ref<'a, T>(*const T)  -> &'a T
slopos-ostd/src/util/ptr_buf.rs:263  borrow_ref_mut<'a, T>(*mut T) -> &'a mut T
```

`CHECK8_ALLOWLIST` holds one entry, that file. Both helpers have **zero callers
outside their own definitions**, so nothing blocks the deletion — the whole of
§1 below is ready to land.

**A green check 8 is not the acceptance criterion.** The gate cannot see three
shapes: `&self` minting `&mut`; `'static` substitution (`ptr_buf::install_buf_mut`
and `dev/mod.rs:102`'s `borrow_dyn` are in-tree instances); and a named closure
lifetime. §1 must carry that argument into the gate's own header before §4
deletes this file, or it exists nowhere.

## Order

§2 and §3 are independent cleanups and may land in either order, before or after
§1. §4 is last.

## 1. Delete the helpers and the allowlist machinery

**Entry gate, before writing any edit:** `CHECK8_ALLOWLIST` contains exactly
`slopos-ostd/src/util/ptr_buf.rs`, and `grep -rn 'borrow_ref' --include='*.rs' .`
returns only `ptr_buf.rs:255,256,261,263,264`.

`slopos-ostd/src/util/ptr_buf.rs`: delete `borrow_ref` with its doc (`:242-259`)
and `borrow_ref_mut` with its doc (`:261-268`). `cargo build` is the test.

Same file, delete the orphan doc line at `:387` — a blank line does not
terminate a doc-attribute run, so rustdoc attaches "Write value to out if out is
non-null" to `section_slice` as that function's summary. It is a stale duplicate
of `nullable_write` / `write_if_non_null`.

`scripts/check_task_ownership.sh`:

- Delete `:759-804` — the residue banner (`:760-762`), the prose,
  `CHECK8_ALLOWLIST` (`:776`), `filter_check8_allowlist` (`:780`), and its
  application (`findings="$(printf '%s\n' "$findings" | filter_check8_allowlist)"`).
  Nothing replaces it at the scan site; the header already documents check 8 at
  length.
  `CHECK_TAGS`, `CHECK_DESC` and the report loop are unchanged — removing the
  filter simply lets check-8 findings reach `total` and fail the gate.
- Count drifts: `:13` "The seven checks" → eight; `:83` drop the ordinal that
  counted the known hits; `:235` "all six source-level checks" → seven
  (`scan_sources` emits 1, 2, 3a, 3b, 4, 5, 6, 8; check 7 is `scan_fault_paths`).
- `:101-122` known limits: drop the `KBox::leak` census sentence — those hits no
  longer exist, and the in-tree `KBox::leak` returns `&'static mut T` and does not
  trip the check. Cite std's `Box::leak` as the hypothetical and keep the
  limitation itself.
- **Migrate the three blind shapes into that known-limits list**, beside the
  existing `'static`-is-excluded bullet: `&self` minting `&mut`; `'static`
  substitution, naming `ptr_buf::install_buf_mut` and `dev/mod.rs:102`'s
  `borrow_dyn`; a named closure lifetime. Add a sentence to `anchored_buf`'s doc
  (`ptr_buf.rs:116-128`) that a token anchor (`&()`, `&len`) bounds the lifetime
  to the frame and constrains nothing else.
- The OK line: **split the clause.** `SANCTIONED_SURFACES` exempts checks 1 and 3
  only, so folding check 8 inside the trailing "outside the sanctioned surfaces"
  tells the reader check 8 has exemptions — the opposite of what this commit
  establishes. Phrase it as what was checked: "…no declared output lifetime that
  no argument constrains, no borrow accessors, and no fault-path lookups; no raw
  task pointers or `c_void` launders outside the sanctioned surfaces."

`justfile` `check-framekernel-gates` (`:373-383`): insert
`scripts/check_task_ownership.sh --self-test` immediately before the scan. ~70 ms.
The self-test has never run in CI, because the allowlist filter was applied only
on the real-run path, so a parser edit that breaks the fixtures has always been
silent. CI calls this recipe directly, so no workflow edit is needed. Do **not**
add fixtures for the removal — after this commit there is no allowlist to test.

## 2. Delete the surface the page-table descent made dead

Pure deletion; `cargo build` is the test.

- `PageTable::is_empty` (`page_table_defs.rs`) — zero callers, and it tests
  `is_unused()` where the freeing predicate is `!is_present()`. Sitting beside
  `table_empty_at` it is a trap that would silently change the freed-table set.
- `PageTable`'s `Index` / `IndexMut` impls — their only users were the deleted
  walks.
- `PageTableEntry::table_ptr` and `points_to_table` — a raw-pointer factory
  pointing the wrong way, plus its only caller.
- `tables.rs::get_memory_layout_info` and its `mod.rs` re-export — zero callers;
  removes two `*mut u64` and the last `nullable_write` from the crate's public
  surface.
- **Demote** `PageTable::{entry, entry_mut, iter, zero}` to module-private. Their
  only remaining callers are the accessors in the same file.

That last one is the payoff: with the walker's borrow-returning half,
`pml4_table{,_mut}`, `table_ptr` and `Index`/`IndexMut` gone and these private,
**there is no way anywhere in the tree to obtain a `&PageTable` or a
`&mut PageTableEntry`.** Keep `PageTableEntry`'s value API, `PageTable` the type
(`process_vm.rs` needs the pointee), `PAGE_TABLE_ENTRIES`, `PageTableLevel`,
`WalkResult`, `paging_get_kernel_directory`.

`mm/src/tests/paging_descent_tests.rs` reaches `PageTable::{entry, entry_mut,
zero}` through its own `set_entry` / `entry` / `new_table` helpers, which are
deliberately independent of the production accessors so a bug in those cannot
make the tests agree with them. Give the demoted methods `pub(crate)` rather than
private visibility, or move the helpers onto `page_table_defs`' accessors and
accept the coupling — the first is preferable.

## 3. Drop the page-directory's cached PML4 pointer

`ProcessPageDir::pml4: KernelSync<*mut PageTable>` is written at four sites and
read nowhere: the descent roots on `pml4_phys`, and `pml4_ptr_from` /
`pml4_table{,_mut}` were its only readers.

Delete the field, the `ProcessPageDir::new` parameter, the `KERNEL_PAGE_DIR`
initialiser line and the `init_paging` write; delete `process_vm.rs:1645`/`:3076`'s
`as_mut_ptr::<PageTable>()` and the two `new` arguments, plus the `PageTable`
imports that go unused in `process_vm.rs` and `tables.rs` (`unused_imports` is a
hard error). Rewrite the struct doc **and the field doc** — both describe the
deleted field, and the field doc's `Send + Sync` sentence describes a derivation
that then comes from elsewhere. What remains is a `pml4_phys` + refcount +
intrusive-`next` bookkeeping handle.

One `KernelSync<*mut T>` and one raw-pointer field leave the tree. This is the
only file outside `mm/src/paging/` the section touches.

## 4. Delete this plan

`git rm plans/RAW_PTR_TO_KARC_MIGRATION.md` and delete its row in
`plans/README.md`. A tree-wide grep returns exactly those two hits. Run the full
acceptance sweep before committing, not after.

## Verification

Per commit: `cargo fmt --all` and stage the reformat; `cargo build -p <crate>`
(seconds, catches the unused-import hard errors); `just build`; `just test`;
`just check-framekernel`.

- **Read `just test`'s pass count, not its exit code.** The baseline is whatever
  `scripts/check_test_count.sh` says — read it from the script, never restate it.
  `just check-test-count` on §4.
- **`just test` overwrites `builddir/kernel.elf` with the tests-feature build.**
  Re-run `just build` before `just check-framekernel`, or `check_stack_sizes.sh`
  measures test functions and fails.
- **Confirm `/dev/kvm` before believing any full-suite failure.** `QEMU_ACCEL`
  defaults to `kvm:tcg` and falls back silently; TCG hits a panic-recovery NMI
  hang that reads as a real regression.
- Boot with `BOOT_CMDLINE="roulette=skip" BOOT_LOG_TIMEOUT=35 just boot-log` and
  confirm `grep -ac 'v0.2-slop' test_output.log`. Without `roulette=skip` the
  boot animation's variable duration ends the log mid-spin, which reads as a hang.
- `cargo test -p slopos-ostd` on any commit touching a `#[cfg(test)]` module or a
  `#[cfg(not(all(target_arch = "x86_64", not(test))))]` branch in ostd — neither
  `just build` nor `just test` compiles those.
- `scripts/check_task_ownership.sh --self-test` on any commit editing that script.
- **Negative control on §1**, worth thirty seconds: paste
  `pub fn probe<'a>(p: *const u8) -> &'a u8 { todo!() }` into any kernel crate,
  run the gate, watch it FAIL naming check 8, remove it. That is the only way to
  prove the filter removal made the check load-bearing rather than merely absent.

## Acceptance

- Check 8 reads zero with `CHECK8_ALLOWLIST` gone and the machinery deleted with it.
- **No `&PageTable` or `&mut PageTableEntry` is constructible anywhere.**
- `grep -rn 'borrow_ref' --include='*.rs' .` returns nothing;
  `grep -rn 'CHECK8\|filter_check8' scripts/` returns nothing.
- Every framekernel gate green, the Verus set green, KernMiri green under both
  Stacked and Tree Borrows, full `just test` green under KVM at the recorded count.

## Prior art

Rust-for-Linux's `Opaque<T>` — `get(&self) -> *mut T`, deliberately no `get_mut`
— is the shape `TaskOwnCell::get_ptr` and `UserContext`'s cell both settled on;
the `PidNamespace` commit (e0020ba6cbcb) names the failure mode this gate exists
to catch: "could be abused to created an unbounded lifetime". Linux derives each
page-table level from the parent's *value* (`pmd_offset(pud, addr)` is
`pud_pgtable(*pud) + pmd_index(addr)`), and `free_pte_range` is the read-value /
clear-parent / defer-free triple the prune's single free site is positioned for
(`MMU_GATHER_RCU_TABLE_FREE`; Asterinas's `RcuDrop`). The `x86_64` crate anchors
each child on the parent entry's borrow, which is why its `unmap` cannot prune and
`clean_up_addr_range` re-walks returning `bool` upward. The Rustonomicon's
"Unbounded Lifetimes" is `borrow_ref_mut` with `&mut`.

In-tree: `slopos-ostd/src/mm/page_table.rs`'s `Pte` is the per-entry value handle
`page_table_defs`' accessors mirror, `InterruptFrame::from_ptr` is the anchored
shape, and `with_parked` / `with_parked_node` in
`slopos-ostd/src/task/placement.rs` are the scoped shape applied to task nodes.
