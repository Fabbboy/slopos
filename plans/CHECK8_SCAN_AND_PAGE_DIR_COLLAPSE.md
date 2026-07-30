# The owner-less page directory the excision left standing

## Intent

`ProcessPageDir` is a six-field `kmalloc`'d descriptor of which exactly one field
is ever read; the other five, and the raw handle every consumer treats as a
sentinel, are the owner-less form `scripts/check_task_ownership.sh` exists to
remove.

## Collapse `ProcessPageDir` to the one field anyone reads

`pml4_phys` is the only field read anywhere: `tables.rs:95`, inside
`pml4_phys_from_raw`. `ref_count`, `process_id`, `next`, `kernel_mapping_gen`
and `mm_ctx_id` are written by `ProcessPageDir::new` and the `KERNEL_PAGE_DIR`
initialiser and read by nothing — a tree-wide grep for `.<field>` finds no
`ProcessPageDir` reader for any of the five.

No crate outside `mm/` names the type. Every out-of-crate consumer uses the raw
`*mut ProcessPageDir` purely as a "this process has a VM" sentinel:

- `core/src/exec/mod.rs:511-515` null-checks it, then `let _ = page_dir;` under a
  comment recording that OSTD reads route through `process_id`.
- `sched/src/task/task_lifecycle.rs:1342,1570` null-check it, then read the PML4
  they actually want with `process_vm_get_ostd_pml4_paddr(child_process_id)`.
- `boot/src/tests/shutdown_tests.rs:363` and the `process_vm_get_page_dir` calls
  in `mm/src/tests/tests.rs` assert non-null and nothing else.

So the descriptor is a `PhysAddr` plus a bit, reached through a raw pointer, and
the bit is what every caller pairs with an OSTD lookup keyed on `process_id`.

### The question to settle first

Does `ProcessVm.page_dir` become a `PhysAddr` with the sentinel expressed as
`PhysAddr::is_null`, or does `process_vm_get_page_dir` go away entirely? Every
consumer pairs its null-check with an OSTD lookup by `process_id`, which suggests
the sentinel already duplicates something `process_vm_get_ostd_pml4_paddr` or the
slot's own validity check reports. Answer that by reading the four consumers
before writing any edit; the answer decides whether this is a field-type change
or a deletion.

### What leaves the tree either way

The `kmalloc`'d descriptor and its two `kmalloc`/`kfree` pairs
(`process_vm.rs:1647,3076`), `init_in_kmalloc_slot`, `pml4_phys_from_raw`,
`KERNEL_PAGE_DIR`'s `SyncUnsafeCell<ProcessPageDir>`, the last
`KernelSync<*mut T>` in `tables.rs` and the one on `ProcessVm.page_dir`, and the
last `use core::ptr` in `tables.rs`. `KERNEL_PAGE_DIR` reduces to a static
holding CR3's frame address, which is all `kernel_pml4_phys` (`tables.rs:213`)
and `paging_get_kernel_directory` are consulted for.

`warnings = "deny"` makes every orphaned import a hard error, so each file's edit
lands with the import change in the same commit.

### Also in this section

`mm/src/process_vm.rs:32-42` is two stacked doc paragraphs on `ProcessVm`, and
the first is false: `page_dir` does not "drive every user mapping today", and
`vm_space` is not "not yet used as the CR3 source". `tables.rs:1-11` records the
per-process surface as OSTD-only and `core/src/exec/mod.rs:515` says the reads
route through `process_id`. Because the two paragraphs are stacked, the stale one
is the rendered summary. `process_vm.rs:440` carries the same claim in miniature
("until the legacy half retires").

### Verification

`just build` (the default-feature build is what catches the orphaned imports and
dead code); `just _iso-tests` — the `tests` modules are `#[cfg(feature =
"test-hooks")]`, off by default, so `just build` alone compiles almost none of
the edits; `just test` — `mm/src/tests/tests.rs` is the only coverage of the
sentinel; `just check-framekernel-gates`;
`BOOT_CMDLINE="roulette=skip" BOOT_LOG_TIMEOUT=35 just boot-log` and confirm
`grep -ac 'v0.2-slop' test_output.log`, since `init_paging` runs early in boot
and a broken kernel-side page directory does not survive to a test.

## Acceptance

- `ProcessPageDir` holds no field that nothing reads, and no consumer holds a raw
  pointer to it.
- Every framekernel gate green, full `just test` green under KVM at or above the
  count in `scripts/check_test_count.sh`.

## Prior art

Linux's `struct mm_struct` is the counter-example: its per-process page-table
root is one field with a refcount that is actually read, which is what makes the
descriptor worth having — a descriptor whose refcount nothing reads is a
`PhysAddr` wearing a struct.
