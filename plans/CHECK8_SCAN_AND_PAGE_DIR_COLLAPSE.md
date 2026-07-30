# Two owner-less handles the excision left standing

## Intent

`scripts/check_task_ownership.sh` check 8 reports a declared lifetime that the
return type names and no argument names. It tests *mention*, not *supply*, so the
shape it is named for can be made to pass. And `ProcessPageDir` is a six-field
`kmalloc`'d descriptor of which exactly one field is ever read; the other five,
and the raw handle every consumer treats as a sentinel, are the owner-less form
the gate exists to remove.

The two sections are independent and may land in either order.

## 1. Make check 8's argument scan test supply, not mention

`unconstrained_lifetime` reports `'a` when `names_lifetime(ret, 'a) &&
!names_lifetime(args, 'a)`, and `names_lifetime` is a whole-token regex over the
raw argument text. Any occurrence anywhere in an argument's written type
therefore counts as constraining, including occurrences that supply nothing.

**The bypass.** `borrow_ref<'a, T>(p: *const T) -> &'a T` — the exact shape the
check is named for — passes the gate once any one of these parameters is added:

```rust
_w: PhantomData<&'a ()>
_f: fn(&'a ())
g: impl FnOnce(&'a T)
```

**What must keep firing.** Both bound spellings are reported today and are
reported correctly, because neither the generic list nor the `where` clause is
part of the argument list:

```rust
pub fn wherefn<'a, T, F>(p: *const T, g: F) -> &'a T where F: FnOnce(&'a T)
pub fn boundfn<'a, T, F: FnOnce(&'a T)>(p: *const T, g: F) -> &'a T
```

**Do not remove the `where`-clause strip.** `ret` is truncated at `where` on
purpose. Without the strip, `fn f<'a, F>(x: u32, f: F) -> u32 where
F: Fn(&'a u8)` is reported for a function that returns no reference at all.

**The false-positive surface is empty.** No kernel-crate function argument
currently writes a non-static lifetime inside a closure bound, a bare `fn(…)`
type, or a `PhantomData`. Every in-tree `PhantomData<&'a …>` is a *struct field*
(`abi/src/handle.rs:83`, `slopos-ostd/src/irq/line.rs:277`,
`slopos-ostd/src/sync/epoch.rs:119`, and eight more), which this scan never
reads. The tightening cannot change the verdict on today's tree; it constrains
future code only. That is what makes it cheap, and it is also why the fixtures
carry the whole weight of the change.

### Work

Narrow the argument scan so an occurrence counts only when the argument could
actually supply the lifetime: ignore occurrences inside a `PhantomData<…>` type
argument, inside a bare `fn(…)` type, and inside an `Fn`/`FnMut`/`FnOnce(…)`
parameter list written in argument position. An occurrence at the argument's own
head (`&'a T`, `&'a mut T`) or in a nominal type's generic arguments (`Foo<'a>`)
still supplies — a caller that has to present a `Foo<'a>` has to own one.

The splitter already counts paren depth (`match_paren`), so a closure bound
inside the argument list does not terminate it. The change is to the inner scan
over `args`, not to the split.

Fixtures carry the proof, and the counts move with them:

- `lifetimes_bad.rs` — one positive per bypass spelling (`PhantomData`, `fn(…)`,
  inline `impl FnOnce(…)`), plus the two bound spellings above, which must fire
  after the change as they do before it. Bump `expect 8` and the fixture-count
  comment above the pair.
- `lifetimes_ok.rs` — the supplying spellings that must stay silent: a nominal
  argument (`t: Branded<'brand>`) whose lifetime the caller must own, and the
  `-> u32 where F: Fn(&'a u8)` case that the `where` strip exists for. A negative
  fixture here is a claim that the gate is *right* to be silent, so nothing that
  merely happens to be silent belongs in it.

### Verification

`scripts/check_task_ownership.sh --self-test`; then a negative control per bypass
spelling — paste it into a kernel crate, watch the gate FAIL naming check 8,
remove it. The self-test cannot cover this: it returns before the tally path.
Then `just check-framekernel-gates`.

## 2. Collapse `ProcessPageDir` to the one field anyone reads

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
dead code); `just test` — `mm/src/tests/tests.rs` is the only coverage of the
sentinel and is compiled by nothing else; `just check-framekernel-gates`;
`BOOT_CMDLINE="roulette=skip" BOOT_LOG_TIMEOUT=35 just boot-log` and confirm
`grep -ac 'v0.2-slop' test_output.log`, since `init_paging` runs at priority 10
and a broken kernel-side page directory does not survive to a test.

## Acceptance

- Adding a `PhantomData<&'a ()>`, `fn(&'a ())` or `impl FnOnce(&'a T)` parameter
  to a fabricated-lifetime signature no longer makes it pass, and both bound
  spellings still fail.
- `ProcessPageDir` holds no field that nothing reads, and no consumer holds a raw
  pointer to it.
- Every framekernel gate green, full `just test` green under KVM at or above the
  count in `scripts/check_test_count.sh`.

## Prior art

Rust-for-Linux's `Opaque<T>` — `get(&self) -> *mut T`, deliberately no `get_mut`
— is the shape check 8 cannot see and the header now records: the escalation from
shared to exclusive is invisible to a lifetime scan. The Rustonomicon's
"Unbounded Lifetimes" is the shape the check does see. Linux's `struct mm_struct`
is the counter-example for §2: its per-process page-table root is one field with
a refcount that is actually read, which is what makes the descriptor worth having
— a descriptor whose refcount nothing reads is a `PhysAddr` wearing a struct.
