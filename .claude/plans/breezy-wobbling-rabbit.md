# Plan: Unify all slot+generation handles onto `slopos_ostd::handle`

## Context

SlopOS already ships a canonical generation-checked handle primitive —
`Handle<T>` + `HandleTable<T>` in `slopos-ostd/src/handle.rs` — that gives
every slot-based kernel object table the same use-after-reuse defence (a stale
handle resolves to a typed error, never an aliasing read). Yet the slot +
generation pattern is independently re-derived in **seven** places, in two
flavours:

- **Already on `HandleTable<T>`, but the integer-packing is copy-pasted**:
  `fs/src/pipe.rs`, `signalfd/src/registry.rs`, `ring/src/registry.rs`.
  signalfd and ring have *byte-identical* `pack`/`unpack` (same `(1<<52)` mask,
  `SLOT_BITS=12`); pipe repeats the same logic with `SLOT_BITS=8`.
- **Hand-rolled struct that ignores the canonical primitive entirely** —
  duplicating *both* the bit-encoding *and* a bespoke slot-array + generation
  counter: `mm/src/memfd.rs` (`MemfdHandle`), `fs/src/vfs_file_ops.rs`
  (`VnodeHandle`), `net/src/unix_socket/handle.rs` (`SocketHandle`),
  `drivers/src/virtio_blk.rs` (`DevHandle`).

The result is ~500 LoC of duplicated, individually-buggable slot/generation
machinery and four parallel "is this handle still valid" implementations.

**Outcome:** one encoding mechanism and one storage mechanism. After this change
there is exactly one place that packs a `(slot, generation)` into an integer
(`Handle::pack`/`unpack`) and exactly one slot table (`HandleTable<T>`). Every
subsystem uses them directly; all bespoke handle structs, slot arrays, generation
counters, and `validate_*` functions are deleted and **all** consumers migrated.
No shims, no dual encodings, no legacy path left behind.

## End-state architecture

1. **`Handle<T>` gains the packing helper** (the only integer-encoding code in
   the kernel):
   ```rust
   // slopos-ostd/src/handle.rs
   impl<T> Handle<T> {
       /// Pack into a `usize`: low `slot_bits` hold the slot, the rest the
       /// generation. Lossless while slot < 2^slot_bits and
       /// generation < 2^(64 - slot_bits).
       pub const fn pack(self, slot_bits: u32) -> usize {
           let slot_mask = (1u64 << slot_bits) - 1;
           (((self.generation() & (u64::MAX >> slot_bits)) << slot_bits)
               | (self.slot() as u64 & slot_mask)) as usize
       }
       pub const fn unpack(raw: usize, slot_bits: u32) -> Self {
           let raw = raw as u64;
           Self::from_parts((raw & ((1u64 << slot_bits) - 1)) as u32, raw >> slot_bits)
       }
   }
   ```
   Add round-trip unit tests in the existing `#[cfg(test)] mod tests` for
   `slot_bits ∈ {8, 10, 12}`.

2. **Bespoke handle newtypes are removed.** `Handle<T>` is already generic over
   `T`, so `Handle<MemfdObject>` and `Handle<UnixSlot>` are distinct,
   non-interchangeable types — the domain newtypes (`MemfdHandle`, `VnodeHandle`,
   `SocketHandle`, `DevHandle`) add only the bit-twiddling we are deleting, so
   they go away and callers use `Handle<T>` directly (matching the existing
   signalfd/ring idiom). The fd-`usize` boundary is the only place packing
   happens, via `Handle::pack(SLOT_BITS)` in each subsystem's `FileOps`/syscall
   glue.

3. **Bespoke slot arrays + generation counters are replaced by
   `HandleTable<T>`.** The per-slot `generation`/`valid`/`active` fields and the
   `validate_*` functions are deleted; `table.get`/`get_mut`/`remove` provide
   validation and stale-detection.

## Changes

### A. `slopos-ostd/src/handle.rs`
Add `pack`/`unpack` (above) + tests. No other change to the primitive.

### B. Flavour-1 dedup (already on `HandleTable`) — encoding only
- `signalfd/src/registry.rs`, `ring/src/registry.rs`: delete the local
  `pack`/`unpack` free fns and `SLOT_MASK`; call `h.pack(SLOT_BITS)` /
  `Handle::unpack(raw, SLOT_BITS)`. Keep the `SLOT_BITS` const at the call site.
- `fs/src/pipe.rs`: `PipeHandle` is a thin wrapper over `Handle<Pipe>` with a
  meaningful `INVALID` sentinel and a `>= MAX_PIPES` bounds check, so it stays —
  but its `pack`/`to_internal` bit-twiddle is replaced by `Handle::pack`/`unpack`.

### C. `mm/src/memfd.rs` (`MemfdHandle` → `Handle<MemfdObject>`)
- Registry becomes `HandleTable<MemfdObject>` built with
  `with_fixed_capacity(MAX_MEMFDS)`; drop `MemfdObject::{active, generation}`,
  `find_free_slot`, `next_generation`, and `validate_handle`
  (use `table.get`/`get_mut`/`remove`).
- Keep the lock-free `MEMFD_PHYS`/`MEMFD_SIZE` static atomic arrays for the
  `fb_flip` hot path — they are keyed by slot index, which `Handle::slot()`
  still provides, and `with_fixed_capacity` guarantees stable slot indices.
- **Sentinel removal (critical):** `MemfdHandle::NONE == 0` (slot 0 / gen 0)
  collides with `HandleTable`'s legitimate first handle. Eliminate the in-band
  sentinel: `RegionBacking::SharedMemfd` (in `mm/src/vma_region.rs`) stores
  `Handle<MemfdObject>` directly, and `VmaRegion::memfd_handle()` returns
  `Option<Handle<MemfdObject>>` (`None` for non-shared regions). `process_vm.rs`
  call sites switch from `!is_none()` to `if let Some(h)`; `process_vm_mmap_inner`
  takes `Option<Handle<MemfdObject>>` for the backing.
- Consumers to migrate (representative): `mm/src/vma_region.rs`,
  `mm/src/process_vm.rs` (mmap/munmap/fork mapcount paths),
  `core/src/syscall/memory_handlers.rs`, `core/src/syscall/ui_handlers.rs`
  (`memfd_get_phys`). The public `memfd_*` fns keep their `handle: usize` ABI at
  the fd boundary but unpack via `Handle::unpack(h, SLOT_BITS)` internally.

### D. `fs/src/vfs_file_ops.rs` (`VnodeHandle` → `Handle<OpenVnode>`)
- Cleanest of the four. Registry becomes `HandleTable<OpenVnode>`; drop the
  `valid`/`generation` slot fields, the global `VNODE_GENERATION` atomic, the
  linear free-slot scan, and `validate_vnode`. The per-open `refcount` stays in
  the slot value (dup increments it; `release` decrements and `table.remove`s at
  zero).
- Consumers: only the `VfsFileOps` methods in this file plus the install site
  `fs/src/fileio/fdops.rs`. Pack via `Handle::pack(SLOT_BITS)` at `as_usize`/
  `from_usize` boundary in the `FileOps` glue.

### E. `net/src/unix_socket/` (`SocketHandle` → `Handle<UnixSlot>`)
- Largest surface. Slot storage in `mod.rs` becomes `HandleTable<UnixSlot>`;
  drop `UnixSlot::generation` and `transition_to_free`'s generation bump (lifecycle
  maps cleanly: `create` = `insert`, state transitions = `get_mut`, `close` =
  `remove`). Delete `validate_socket_handle` in favour of `table.get`.
- **Preserve the `slot_for_wq` split:** event-bus keying uses the slot index
  *without* generation validation; that becomes `handle.slot()` + the existing
  `< MAX_UNIX_SOCKETS` bounds check. Data access keeps validating via
  `table.get(handle)`. `PairTable` and backlog `KVecDeque<Handle<UnixSlot>>` are
  unaffected except for the type rename.
- Consumers: `net/src/unix_socket_file_ops.rs` (8 FileOps entry points),
  `core/src/syscall/net_handlers.rs` (the `SocketFd::Unix(..)` enum + every
  `unix_*` syscall handler signature), and all internal `mod.rs` callers.

### F. `drivers/src/virtio_blk.rs` (`DevHandle` → `Handle<DevState>`)
- Registry becomes `HandleTable<DevState>` where
  `DevState { inner: KArc<VirtioBlkInner>, index: u16, write_claimed: bool }`
  (the per-slot capability metadata moves into the stored value). Drop the
  bespoke `DevSlot` array, `next_generation`, and `validate`.
- `blk_device_by_index` becomes a `table.iter()` scan for `index`; `open_writer`
  / `BlockWriteToken::drop` flip `write_claimed` via `table.get_mut`. Generation
  is never bumped (devices are never removed) — harmless and consistent.
- `DevHandle` is an in-kernel token only (never packed into an fd `usize`), so no
  `pack`/`unpack` is needed here — callers pass `Handle<DevState>` directly.
- Consumers: `boot/src/boot_services.rs` and the `drivers/src/tests/virtio_*`
  suites (type rename + drop the `DevHandle` import).

## Risks & mitigations
- **memfd sentinel collision** — the one genuine correctness hazard; handled by
  switching `RegionBacking`/`memfd_handle()` to `Option<Handle<..>>` (also
  removes a latent footgun where a real slot-0 memfd read as "none").
- **unix_socket breadth** — many signatures change, but it is a mechanical type
  rename guarded by the large `net` test suite (socket_*, unix, tcp/udp demux).
- **memfd lock-free `fb_flip`** — preserved by keeping the side atomic arrays and
  relying on `with_fixed_capacity`'s stable slot indices.
- Each subsystem compiles and tests independently; if `virtio_blk`'s
  capability-registry conversion proves to fight `HandleTable`, its fallback is
  encoding-unification only (still deletes the bespoke bit-twiddle) — but the
  full conversion is the intended end state.

## Verification
1. `cargo fmt --all` (per repo policy).
2. `just build` — runs the framekernel/unsafe, `-Zemit-stack-sizes`, and
   soft-float gates; confirms no kernel crate gains `unsafe` and `slopos-ostd`
   stays the sole `unsafe` owner.
3. `just test` — full KTAP run. Targeted re-runs while iterating:
   `just test FILTER='slopos_mm::*,slopos_net::*'`, plus the pipe, ring,
   signalfd, vfs, memfd, unix-socket, and virtio-blk suites are the regression
   net for each migrated subsystem.
4. `just check-test-count` — guards against silently dropping tests.
5. Smoke: `just boot-fast` to confirm compositor `fb_flip` (memfd lock-free
   path) and a shell session (pipes, vnodes, unix sockets) still work end-to-end.

No commits/PRs are created as part of this work; the change lands as a single
coherent working-tree edit per the user's instruction.
