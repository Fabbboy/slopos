# Block-Device Ownership & Capability Redesign

Status: **in progress** (started 2026-05-27)

## Why

A kernel test (`test_virtio_blk_write_readback_interrupt_driven`) wrote the
`(i&0xFF)` pattern to a hardcoded LBA (sector 8192) on the **live** virtio-blk
device that backs the root filesystem. On the test image that sector is
`/bin/io_capture_test`'s code block, so the test silently corrupted an on-disk
binary; `exec()`ing it later faulted with a wild instruction (#UD). The
nightly-2026-05-25 bump shifted the userland layout so the binary's block 9
landed exactly on sector 8192, exposing the latent bug.

A band-aid (save/restore the sector) is already committed (`633b3333`). This
plan makes the **entire class** of bug structurally impossible.

## The anti-pattern (three root causes)

1. **Ambient authority** — `virtio_blk_read/write(offset, buf)` are `pub` free
   functions callable from anywhere (`drivers/src/virtio_blk.rs`). The FS gets
   only bare `fn` pointers (`fs/src/ext2_vfs.rs`, wired in
   `boot/src/boot_services.rs`).
2. **Raw integer addressing** — an LBA is a bare `u64`; nothing distinguishes
   "free/scratch" from "owned by a file".
3. **No exclusion + no test isolation** — one device backs both the live FS and
   destructive tests (`scripts/qemu_run.sh` attaches a single `virtio-blk-pci`).

## The principle (from Linux bd_writers, FreeBSD GEOM, Redox schemes, Asterinas OSTD, Theseus MappedPages)

> Authority to write a device is an **owned, typed object you must be granted** —
> never a global name you can invoke, never a raw integer you can fabricate.
> Exclusion is checked once at **handle acquisition**; afterward every write is
> safe by construction.

SlopOS already has the right substrate: `slopos-ostd` is the sole `unsafe`
crate (perfect for Tock-style capability minting), `Frame<M>` typestate,
generation-counter handles (`12a54cdd`) + `MemfdHandle`.

## Layers (each independently shippable, kept green)

### Layer 1 — Own the device (keystone)
Refactor virtio-blk from "single global static + `pub fn` read/write" into an
owned, non-`Clone` `BlockDevice` handle produced by probe. The FS holds the
**only** writable handle. Delete/privatize the ambient `virtio_blk_write`.
Acceptance: no `pub fn` raw write exists; FS owns its device; suite green.

### Layer 5 — Device registry with generation handles (supports Layer 1's `'static`)
Because device tables are `'static`, branded lifetimes don't thread; use an
opaque slot+generation `DevHandle` (like `MemfdHandle`). The registry mints
handles; the FS claims one for exclusive write; stale handles fail loudly.
Acceptance: device access only via `DevHandle`; double-claim of write access
rejected at runtime.

### Layer 4 — Test isolation: scratch device, never the rootfs
Attach a second, empty virtio-blk in the test QEMU invocation. The driver
claims N devices. The test harness hands destructive tests a capability to the
**scratch** device only; the rootfs device's writable handle is never exposed
to the test registry.
Acceptance: destructive block tests target the scratch device; a test cannot
obtain a writable handle to the rootfs device.

### Layer 2 — `RawSectorCap` minted only in `slopos-ostd`
For the legitimately-raw path (mkfs, recovery, the write-readback test), require
`fn raw_write<C: RawSectorCap>(cap: &C, ...)`. `unsafe impl RawSectorCap` is
allowed only inside `slopos-ostd`, aligning capability minting with the existing
`check_unsafe_outside_ostd.sh` gate.
Acceptance: raw writes require a cap minted in ostd; grep-able + gate-enforced.

### Layer 3 — Affine `Extent` tokens (exceed the gold standard)
Writes take an owned, non-`Clone` `Extent` minted only by an allocator that
never overlaps live blocks. A hardcoded LBA stops being an expressible write
target. (Theseus `MappedPages` analogue for disk ranges.)
Acceptance: FS write path takes `&Extent`; raw LBA writes unrepresentable
outside the cap path.

### Integrity backstop — block checksums / rootfs verity
Detect out-of-band corruption at read time (RedoxFS/dm-verity style) so silent
corruption becomes a loud, attributable failure.
Acceptance: tampering with a verified block is detected on read.

## Invariants / gates that MUST stay green at every commit

- `just build` (runs `check_unsafe_outside_ostd.sh`, `check_alloc_dep.sh`,
  `check_stack_sizes.sh` ≤2048B, framekernel gates).
- `just test` → 2458+ passed, 0 failed.
- Only `slopos-ostd` uses `unsafe`; kernel crates route allocation through
  `slopos_ostd` (`KBox`/`KVec`/`KArc`).
- `cargo fmt --all` before every commit.

## Process (per layer)

1. Design subagent (Plan/Explore) pins the exact approach against the code.
2. Implement; `just build` + `just test` green.
3. Code-review subagent(s) review the diff; iterate until no findings ("world
   class").
4. Commit on `develop` (`<area>: <imperative>`), update this doc's status.

## Status by layer

- **Layer 1 (own the device) — DONE.** Ambient `virtio_blk_read/write/...` free
  fns + `CallbackBlockDevice` deleted; FS holds an owned `KBox<dyn BlockDevice>`
  (disk0 `BlockWriteToken`) claimed via `open_writer`.
- **Layer 5 (registry + generation handles) — DONE.** `BLK_REGISTRY` + opaque
  `DevHandle` (slot+generation); `open_writer` exclusive-write claim.
- **Layer 4 (test scratch device) — DONE.** Disposable virtio-disk1 in the test
  harness; destructive tests target it via a capability; rootfs untouched
  (verified). Exclusion-FSM tests added.
- **Layer 2 (RawSectorCap) — SUBSUMED.** There is no longer any raw "write any
  LBA" path to gate: `open_writer`→`BlockWriteToken` *is* the capability, minted
  only from a registry `DevHandle`. A separate `unsafe trait` gate would have no
  consumer. Re-open only if a raw recovery/mkfs path is reintroduced.
- **Layer 3 (affine Extent tokens) — TODO.** `BlockDevice::write_at` still takes
  a raw `u64` offset; wrap in allocator-minted `Extent` tokens so a literal LBA
  is not an expressible write target.
- **Integrity backstop (checksums/verity) — TODO.**

## Progress log

- 2026-05-27: plan written; band-aid + nightly bump + debug tooling committed.
- 2026-05-27: Layers 1/4/5 landed (commits: IRQ-handler generalize; registry
  keystone; review fixups; scratch device + test migration + FSM tests; ambient
  removal + FS capability). 2460/2460 green; rootfs verified untouched by the
  destructive test. Keystone reviewed by subagent (no memory-safety bugs).
