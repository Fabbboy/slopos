---
name: canonical-handle-unification
description: All fd-like subsystems use slopos_ostd Handle<T>/HandleTable<T>; one encoding via Handle::pack/unpack
metadata:
  type: project
---

As of 2026-06-13 every slot+generation handle in the kernel routes through the
ONE canonical primitive `slopos_ostd::handle::{Handle<T>, HandleTable<T>}`
(`slopos-ostd/src/handle.rs`). There is a single integer-encoding surface:
`Handle::pack(slot_bits)` / `Handle::unpack(raw, slot_bits)` — added during this
migration. No subsystem hand-rolls `(generation << SLOT_BITS) | slot` anymore.

Migrated (was 7 independent reimplementations, ~500 LoC of bespoke slot
arrays + generation counters + validate fns, all deleted):
- `fs/src/pipe.rs` (PipeHandle keeps an INVALID sentinel; packing delegates)
- `signalfd/src/registry.rs`, `ring/src/registry.rs` (use bare `Handle<T>`)
- `fs/src/vfs_file_ops.rs` → `HandleTable<OpenVnode>`
- `mm/src/memfd.rs` → `HandleTable<MemfdObject>`; `MemfdHandle = Handle<MemfdObject>`
- `net/src/unix_socket/` → `HandleTable<UnixSlot>`; `SocketHandle` newtype over `Handle<UnixSlot>`
- `drivers/src/virtio_blk.rs` → `HandleTable<DevState>`; `DevHandle = Handle<DevState>`

**Convention for any NEW fd-like / slot-table subsystem:** store values in a
`HandleTable<T>` (lazily-init `SpinLock<Option<HandleTable<T>>>` + a
`with_registry` helper is the established idiom — see signalfd/memfd/virtio_blk);
expose a thin domain newtype or bare `Handle<T>`; pack to the fd-layer `usize`
(`OpenFile.handle`) only at the FileOps/syscall boundary via `Handle::pack`.
Never hand-roll generation checking — `table.get/get_mut/remove` do it.

Gotchas learned: (1) `HandleTable`'s first handle is slot 0 / gen 0, so an
in-band zero "none" sentinel collides — memfd's old `NONE == 0` was replaced
with `Option<Handle<..>>` in `RegionBacking::SharedMemfd`/`memfd_handle()`.
(2) The fd-layer `usize` is the cross-crate currency; convert raw↔typed once at
the boundary, keep the typed handle internally. Related: [[memfd-migration]] if
written, and the FileOps dispatch in `fs/src/fileio/mod.rs`.
