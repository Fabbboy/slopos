# Persistent Storage

Make a file written on one boot readable on the next, on the root filesystem
and under failure. A file written under `/mnt` and committed — with `fsync`, or
by opening it `O_SYNC` — survives a reboot, and `just test-persist` boots one
image twice and reads the payload back with CI running it. That is the narrow
case:

- **The root is still RAM.** Nothing written to `/` outlives the boot; the disk
  is a secondary mount at `/mnt`, and `root=virtio` is exercised by no recipe,
  test or CI job.

Crash consistency is closed: an image that was never marked clean mounts
read-only rather than being silently trusted, a filesystem found damaged stops
being written to, an unlinked-but-open file keeps its contents until the last
close on both sides of the disk, and the whole of it is held down by a
crash-injection test that cuts the device at every write boundary. What remains
is making the persistent root the real one.

Verity is settled and out of scope here: a trailer makes the device
write-protected and the mount read-only (`MOUNT_RDONLY`, `EROFS`), the shipped
`ext2.img` carries one and `just boot` asserts it with `verity=require`, and
the image the suite writes to is built `VERITY=off`. The one open verity item
is an *upgrade* — persisting the set of blocks a boot rewrote so an image can
be both writable and mostly attested — and it lives in Phase 6 as 6.6, because
nothing before it needs it.

## Why this matters beyond files

Persistence is the enabler under a long tail of features that are currently
either fake or impossible: a keymap choice that survives (`/etc/keymap` is
written today into a ramfs and lost), user data of any kind, a package or
application store, logs that outlive the crash that produced them, a W/L
ledger with somewhere to live other than the boot medium
(`plans/microtransactions.md` Phase 1 chose a Limine module precisely because
there is no writable store), shell history, compositor layout, wallpaper
selection, and every "settings" surface the GUI would otherwise have to
pretend to have.

---

## 1. What exists

The shape, which the code states more precisely:

- `drivers/src/virtio_blk.rs` — one virtio-blk driver, MSI-X, an exclusive
  `BlockWriteToken` write capability, `VIRTIO_BLK_F_FLUSH` negotiated and a
  real `VIRTIO_BLK_T_FLUSH` behind `BlockDevice::flush`. Probe-order indices:
  `disk0` is the root image, `disk1` a blank scratch device the test harness
  attaches. Nothing in this plan needs to change it.
- `fs/src/blockdev.rs` — `BlockDevice` trait (`read_at`/`write_at`/`capacity`/
  `flush`) plus `MemoryBlockDevice` for tests.
- `fs/src/ext2/` — superblock/group-desc/inode parsing with validated geometry
  (`Ext2Geometry` is the only constructor of `GroupIdx`), a 128-entry
  write-back `BlockCache` over `Frame<PageCacheMeta>` with LRU eviction and
  shrinker registration, block/inode bitmap allocation, triple-indirect
  block mapping, directory record walk/append/remove, and `sync()` implementing
  ext2 `data=ordered`: data → barrier → metadata → barrier → superblock free
  counts → barrier.
- **The write surface is closed and every operation is all-or-nothing.**
  `truncate` (partial indirect-tree free, sparse extension, tail zeroing),
  `rename` (new entry written before the old is removed, `..` fixup,
  self-splice refused), `symlink`/`readlink` (fast and slow forms), `set_mode`,
  `rmdir`, and `set_sealed` carried by `EXT2_IMMUTABLE_FL` so the seal survives
  a reboot and reads as one to `lsattr` and `e2fsck`. `links_count`,
  `used_dirs_count`, `i_blocks` (indirect blocks included) and `i_dtime` are
  maintained; `unlink` on a multiply-linked inode drops a link rather than
  freeing it. Offsets and sizes are 64-bit, `i_size_high` round-trips, and a
  file past 4 GiB sets `RO_COMPAT_LARGE_FILE`.
- **A failed operation leaves nothing behind.** `Ext2Fs::transaction` opens an
  RAII scope over the cache: a block found clean rolls back by being dropped, a
  block already dirty by a snapshot taken before the first mutation, and the
  superblock's free counts are restored with them. Eviction prefers untouched
  victims, so the residual is a block the cache had to write back mid-operation
  — which is what a journal, not a wider guard, would close. Snapshots are
  capped (`MAX_UNDO`); a scope that outgrows its undo record fails rather than
  committing a part of itself.
- **There is a wall clock, and userland can read it.** Limine's
  `DateAtBootRequest` anchors `CLOCK_REALTIME` against the monotonic counter at
  boot (`kernel_services::clock::realtime_ns`); the inode write paths stamp
  `atime`/`ctime`/`mtime` from it, and `syscall_clock_gettime` answers it for
  `CLOCK_REALTIME`, falling back to monotonic only on a boot that established
  no clock. A boot whose bootloader reports no date stamps nothing, rather than
  claiming 1970.
- **A dirty image is not silently trusted.** `s_state` is read once at mount,
  against the superblock as it came off the disk, and an image that was never
  marked clean mounts **read-only** with a loud klog line naming `e2fsck` as
  the repair (`Ext2Fs::mount_read_only_reason` → `ReadOnlyReason`). There is no
  in-kernel fsck and there should not be one. `s_mnt_count`, `s_mtime` and
  `s_wtime` are maintained; `s_lastcheck` deliberately is *not*, because this
  kernel runs no check and stamping it would claim one that never happened.
  `check_overdue` reports `e2fsck`'s own two rules without acting on them.
- **A damaged filesystem stops being written to.** `errors=remount-ro`: an
  operation that finds the image or the device damaged latches the whole mount
  read-only. What counts as damage is a classification
  (`Ext2Error::is_corruption`) that deliberately excludes every error an
  unprivileged caller can induce on demand — `InvalidRange` exists as a
  separate variant precisely so a bad `readdir` cookie or an offset past the
  block map's reach is `EINVAL` to that caller rather than `EROFS` for
  everybody.
- **An unlinked file that is still open keeps its contents.** POSIX's rule, in
  two halves. The in-memory half is an open-inode reference table
  (`fs/src/vfs/orphan.rs`) that decides, under the lock the count is taken
  under, whether `unlink` frees now or defers; the on-disk half is ext2's own
  orphan list, threaded through `s_last_orphan` and each member's `i_dtime`,
  so a crash leaves a list the next mount drains rather than leaked blocks.
  The deferred free runs on the flusher, not at the last close, because a
  descriptor drops from a `Drop` the task-exit path reaches under a preempt
  guard. `rename` over an open file takes the same path.
- **A directory of any size is listable.** `FileSystem::readdir_cookie` pages
  over an opaque per-filesystem cookie — a byte offset into the directory's
  data on ext2, which an unrelated create or unlink does not shift — carried
  through `UserFsList.cursor` so `fs_list` resumes across calls.
- `fs/src/ext2_vfs.rs` — the mount singleton, a 5 s background flusher kthread
  with a dirty-count eager-wake threshold and exponential backoff, and the
  `FileSystem` impl.
- `fs/src/vfs/` — mount table (16 entries, longest-prefix, per-component
  re-resolution, `MOUNT_RDONLY` consulted at every `vfs::ops` mutation),
  lexical canonicalisation, path walk carrying the mount's flags, `FileSystem`
  trait.
- **A crash is testable.** `FaultyBlockDevice` (`fs/src/tests.rs`) acknowledges
  writes and drops them after *N* operations, in the `dm-flakey` style — the
  harder half of the two, because a dropped write is what a power cut looks
  like from inside the kernel and nothing is told. The crash test cuts the
  device at *every* write boundary of a create+write+sync workload and asserts
  the survivor is one of the two legal states, bounded black-box crash testing
  after CrashMonkey/B³ (OSDI '18).
- `fs/src/verity.rs` — a CRC-32-per-block trailer appended by
  `scripts/gen_verity.py`, sector-padded so a sector-granular capacity still
  reaches it. A trailer is recognised only beyond the filesystem's own extent
  (`build_verified` takes the superblock's block count), a verified device is
  write-protected, and a trailer that is present but unusable refuses the
  mount. `Ext2Fs::read_only_for` is the one rule for whether a handle may
  mutate; a read-only mount writes no superblock state and starts no flusher.
- **Durability is invocable and inode-granular.** `fsync(2)`, `fdatasync(2)`
  and `sync(2)` are syscalls 177–179, `std::fs::File::sync_all`/`sync_data`
  reach them, and `O_SYNC`/`O_DSYNC` are honoured per descriptor in
  `fileio::write_open_file`. `FileSystem::sync_inode` commits one inode — its
  data, the allocation state reaching it, a barrier, then its on-disk record —
  and defaults to whole-filesystem `sync` for filesystems with no finer
  writeback. Two invariants hold it together: a `BlockCache` entry records
  whose data it is (`BlockOwner`), with directory and symlink contents owned by
  their inode so a shared inode-table block is never published ahead of the
  blocks it names; and every barrier decision reads `unbarriered_writes` rather
  than what the caller wrote, because eviction writes back and clears the dirty
  bit without a barrier, so a clean cache is not a durable one.

## 2. The gaps

Grouped by subsystem; the phase list below sequences them.

### G1 — The root filesystem is RAM, and the disk-as-root path is unexercised

`boot_step_rootfs_init` unpacks the initramfs into RamFs and sets
`ROOTFS_IS_RAMFS` (`boot/src/boot_services.rs:53-57`, `:93`);
`boot_step_fs_init` sees that flag and mounts ext2 at `/mnt` as a secondary
instead of at `/` (`:122-131`). `root=auto` — the default, and what every
`just` recipe boots (`justfile:52`) — takes that branch whenever a Limine
initramfs module is present. `root=virtio` is named by no recipe, no test and
no CI job; it appears only in the cmdline parser
(`boot/src/early_init.rs:658-666`).

So nothing in the *root* filesystem persists, and the code path that would is
unexercised.

Two facts about the disk shape Phase 5:

- **ext2 is initialised on `disk0` unconditionally**, before and independent of
  the `ROOTFS_IS_RAMFS` branch (`boot_step_fs_init`). On a writable image that
  claims the exclusive `BlockWriteToken` and runs `mark_dirty_on_disk`, so the
  default boot of the *tests* image writes the superblock twice (mount and
  orderly shutdown). The shipped `ext2.img` is verified, so its default boot
  writes nothing.
- **Verity does not cover the superblock.** Superblock I/O is sub-block —
  `read_at(1024, …)` direct to the device, bypassing the cache — and only a
  block fully contained in a read is verified. On a verified device this is
  moot, since the device refuses the write, but 6.6 has to say what it means
  for block 0.

### G2 — A verified image cannot be a writable one

Settled by construction rather than open: a verity trailer makes the device
write-protected, so the "written on boot 1, unverifiable on boot 2" case
cannot arise. The cost is that the persistent root gets no integrity checking
at all, and the images that persist (`ext2-tests.img` today, the root in
Phase 5) are built `VERITY=off`. Buying most of that coverage back is 6.6.


### G5 — A sync stalls every user of its mount

`CACHED_EXT2` is one global sleeping mutex held across all of ext2's block I/O,
so the caller that wins it holds it for a whole writeback pass while every path
walk and `exec` on that mount waits. `sync(2)` is unprivileged and takes no
fd, which makes that stall userland-reachable. Concurrent callers do not
multiply the cost — the second one in finds nothing dirty and nothing
unbarriered and returns without touching the device — but nothing bounds the
wait behind the pass in flight. A lock finer than one per mount is the fix,
which makes this the page-cache work in 6.5 rather than a sync change.

Two narrower limits sit alongside it:

- **`fdatasync` commits exactly what `fsync` does.** An ext2 record carries the
  block pointers, the size and the timestamps in one 128-byte struct, so no
  write commits the first two without the third. The split becomes real when a
  timestamp alone can dirty a record, which the wall clock now makes possible;
  nothing yet asks for it.
- **`fsync` does not commit the directory entry**, per POSIX. Doing so needs a
  descriptor on a directory, which `open` cannot return.

### G6 — A rollback cannot retract a block the cache already evicted

`Ext2Fs::transaction` undoes an operation's cache state, so the bitmaps, the
inode records and the superblock's free counts move together or not at all.
What it cannot undo is a block eviction wrote back *before* the failure:
`find_or_evict` makes a touched block the victim of last resort, so reaching
this needs an operation whose working set exceeds the 128-entry cache, but
nothing bounds it.

The same is true of a scope that outgrows its snapshot budget: it fails rather
than committing part of itself, which is correct but leaves a large operation
un-performable rather than merely slow. Both want a write-ahead journal, and
neither is closed: Phase 4 bounded the *consequences* of a crash rather than
this residual, and a journal remains the only thing that retracts a block
eviction already put on the device.

### G7 — Closed: a dirty image is refused, and an orphan is recoverable

Was: nothing repaired a dirty image and nothing noticed it was dirty. Now §1
records the mount-time `s_state` rule, `errors=remount-ro`, the superblock
bookkeeping, and the orphan list on both sides. What is deliberately *not*
here is an in-kernel fsck: read-only plus a klog line is the honest behaviour,
and `e2fsck` on the host is the repair tool.

One residual, narrower than the gap it replaces. The window between a path
walk resolving an inode and `open` installing its reference is closed by
re-resolving the path once the reference is held, which catches the name being
gone or now denoting something else; an inode reallocated *and* re-bound to the
same path in between still passes. Closing that needs an inode cache keyed by
`(filesystem, inode)` and a parent lock held across the walk, which is 6.5's
territory.

### G8 — Closed: `CLOCK_REALTIME` answers the wall clock

`syscall_clock_gettime` reads `realtime_ns()`, and the superblock timestamps
`e2fsck` reports are written. See §1.

### G9 — A paged listing's mount-point pass is keyed on an ordinal

A directory of any size is listable: `readdir_cookie` pages over a
per-filesystem cookie — a byte offset on ext2, which an unrelated create or
unlink does not shift — carried across syscalls in `UserFsList.cursor`.

The synthesised mount-point entries the VFS appends are the weak half. They are
resumed by *ordinal* (`ListCursor::mounts_done`), so a mount or unmount between
two pages shifts the sequence and can drop or repeat one; and a real directory
entry shadowed by a mount is only de-duplicated within a page, so a name listed
early can appear again as a synthesised mount later. Both need the mount table
keyed by identity rather than position, which is 6.4's territory — that is when
userland can mount, and therefore when the race becomes reachable on purpose.

### G10 — Disk space is not a charged resource

`abi/src/quota.rs` enumerates eight `ResourceKind`s and none of them covers
disk blocks or on-disk inodes. With a writable root, any unprivileged process
can write until `Ext2Error::NoSpace` and deny the disk to every other process
and to the kernel's own writes. `statfs` (Phase 6.3) reports free space; it
does not limit it.

### G12 — The build wipes the disk on every kernel build

`just build` depends on `_fs-image`, and `scripts/build_fs_image.sh` starts
with `rm -f "$IMAGE_PATH"` and a fresh `mkfs.ext2`. So a developer iterating on
the kernel loses disk contents on every build, and persistence is observable
only by booting twice with no build in between. There is no recipe that
preserves the image, and no test that boots twice.

### G13 — The block layer sees one whole-device filesystem

No partition-table parsing (GPT or MBR), so an image is always "the filesystem
starts at byte 0". No block-device nodes in `/dev`, so `root=/dev/vda1` cannot
be spelled and `root=` selects by driver name only. No `statfs`, so nothing can
report free space. `MAX_MOUNTS` is 16, and `MOUNT_RDONLY` is the only mount
flag with a reader.

The name-length limits differ between the two roots that Phase 5 makes
interchangeable: `fs::MAX_NAME_LEN = 32` is enforced by ramfs
(`fs/src/ramfs/mod.rs:111`) and cpio but *not* on the ext2 path, which accepts
ext2's 255 (`ext2/mod.rs:374`). A long name on disk is reachable — the path
walk applies no per-component limit — but the same `create()` succeeds on ext2
and fails `ENAMETOOLONG` on ramfs, and `vfs_list` truncates into
`UserFsEntry.name: [u8; 64]`, so a listed name can be one that cannot be opened.

### G14 — No file-backed mmap

`syscall_mmap` rejects any fd whose kind is not `FileKind::Memfd`
(`core/src/syscall/memory_handlers.rs:26-32`), so `MAP_SHARED`/`MAP_PRIVATE`
file backing does not exist: no demand-paged executables off the persistent
root, no mmap'd data files, and no answer yet for how a page cache would stay
coherent with the 128-entry ext2 `BlockCache`.

---

## 3. Plan

Each phase ends green on `just test` plus the pre-commit gate sequence in
`AGENTS.md`. Phases 5 and 6 are independent of each other. Numbering starts at
5 because the earlier phases are done and their results are §1; the numbers are
kept so commit messages and issue references to them still resolve.

### Phase 5 — Make the persistent root the real one

The write surface and the on-disk seal are in place, so `root=virtio` is no
longer a privilege-escalation surface nor a root that cannot overwrite a
file. What remains is making it the default and paying for that.

- [ ] **5.0** Decide what a writable root does to the test harness.
  `scripts/qemu_run.sh` records why `disk1` exists: destructive tests target
  the scratch device, never the live root image, so a buggy test cannot
  corrupt an on-disk binary — the incident it names happened. A writable,
  persistent `/` makes every filesystem test a mutation of the image the next
  boot runs `/sbin/init` from, and CI becomes order-dependent (`AGENTS.md`
  already documents the `'*ext2_aaa*'` ordering coupling). Either regenerate the
  tests image per CI run, or give the persistence test a preserved copy of its
  own. The harness already attaches the shipped verified `ext2.img` as a
  snapshot `disk2` for `test_verity_artifact_*`; a writable root is the
  moment that image and the tests image stop being interchangeable, and the
  `verity=require` default in `just boot` must keep meaning "the disk I run
  `/sbin/init` from is attested".
- [ ] **5.1** Stop clobbering the image: `_fs-image` rebuilds only when the
  binaries changed, and a `PRESERVE_FS_IMAGE=1` path that updates `/bin` in
  place via `debugfs` rather than `mkfs`. A developer iterating on the kernel
  must not lose disk state.
- [ ] **5.2** `/etc` exists on both roots, created by `build_fs_image.sh` and
  `gen_initramfs.py` — today neither creates it, and `/etc/keymap` only works
  because ramfs auto-creates parents.
- [ ] **5.3** A boot with a disk mounts it read-write at `/` and keeps the
  initramfs as the fallback for the no-disk case (`just boot-ramonly` is the
  existing proof of that path and must stay green). The `root=` knob keeps its
  three values; what changes is which one `auto` picks when a disk is present
  and healthy.
- [ ] **5.4** Extend `just test-persist` to the disk root: today it writes under
  `/mnt`, which is where the disk is mounted while `/` is RAM.
- [ ] **5.5** Disk-space accounting (G10) before the root is writable: a
  `ResourceKind` charged in `allocate_block`/`allocate_inode`, so one process
  cannot fill the disk and deny it to `/sbin/init`. Moves
  `check_quota_headroom.sh` — `KIND_COUNT` and every per-kind row in the gate
  file change.
- [ ] **5.6** A security sweep of the newly-reachable configuration per the
  `CVSS.md` workflow: the seal, the exec path, `/bin` write protection, disk
  exhaustion, and whatever else "the root filesystem is now writable by
  userland" newly exposes.

### Phase 6 — Block layer breadth

Independent of 2–5; do it when a second device or a real disk demands it.

- [ ] **6.1** GPT parsing, MBR as fallback, exposing partitions as
  `BlockDevice`s over an offset window of the parent. Required before SlopOS
  can read a disk anything else wrote.
- [ ] **6.2** Block-device nodes in devfs (`/dev/vda`, `/dev/vda1`), so `root=`
  can name a device the way every other kernel spells it.
- [ ] **6.3** `statfs(2)` — free blocks and inodes are already in the
  superblock and the group descriptors.
- [ ] **6.4** `mount(2)` / `umount(2)`, gated behind a capability. The mount
  table already exists; what is missing is a syscall and an authority for it.
  `MAX_MOUNTS` is 16, and it becomes a user-visible limit the moment userland
  can mount.
- [ ] **6.5** File-backed `mmap` (G14), with a stated answer for page-cache /
  `BlockCache` coherence.
- [ ] **6.6** Verity on a writable image: persist the set of rewritten blocks.
  The trailer is immutable and the device write-protected today, which is
  what makes the guarantee simple to state; the upgrade is to record, at
  `mark_clean`, which blocks this boot rewrote, and to treat `EXT2_ERROR_FS`
  at the next mount as "verity off for this boot". Blocks never written stay
  attested across reboots, there is no live hash recomputation, and the crash
  case degrades to unverified rather than to a false integrity failure. It
  needs a stated answer for block 0 (the superblock is written sub-block and
  never verified — G1), a trailer version bump `read_header` refuses today,
  and a rule for `gen_verity.py`'s padding, which sits inside the region a
  bitset would describe. The alternative — recomputing a block's CRC on
  writeback and journalling the array, as dm-integrity does — is a week to
  this one's day and makes the trailer a second crash-consistency problem.

---

## 4. Constraints this work must respect

- **Unsafe surface.** All of `fs/` is `#![forbid(unsafe_code)]`. Nothing here
  needs `slopos-ostd` changes; if something appears to, that is a signal the
  design took a wrong turn.
- **Allocation.** `KBox`/`KVec`/`KArc`/`KBTreeMap` only. The block cache
  already allocates through `Frame<PageCacheMeta>` and registers a shrinker;
  new caches must do the same or they are memory the reclaim path cannot see.
- **Stack frames ≤ 2 KiB.** `fs/src/tests.rs` already carries
  `#[inline(never)]` helpers specifically because two `Ext2Fs` plus two
  `BlockCache` handles in one frame exceed the gate. New ext2 operations with
  several live handles will hit this; build in place via `KBox::try_init`.
- **Lock order.** `CACHED_EXT2` is a *sleeping* mutex held across block I/O.
  `Ext2CacheReclaim` uses `try_lock` only, for the reason its comment gives.
  Any new lock is a new lockdep class and moves `check_lockdep_headroom.sh` —
  re-measure with `--emit-allowlist`, never hand-edit the gate file.
- **No spinning lock may span a filesystem call.** A `SpinLock` holds
  preemption off, and any write that has to allocate parks on the virtio
  completion, so the scheduler's `assert_not_blocking_while_atomic` fires. A
  lock held across `FileOps::read`/`write`/`sync` must be a sleeping `Mutex`
  (`OpenFile.position_lock` is one for exactly this reason), and its acquire is
  fallible — a killed task returns `EINTR` rather than proceeding unserialised.
- **Syscall classification.** Every new syscall needs a `cap(...)` clause and
  a re-recorded `CAP_COUNTS` (a compile-time assert, so it fails loudly). A
  filesystem syscall that can reach a power primitive also moves
  `check_authority_reachability.sh`.
- **Test count.** New stests/utests move `check_test_count.sh`; measure the
  new baseline with `TEST_COUNT_BASELINE=0 scripts/check_test_count.sh`.
- **Gates beyond the obvious ones.** `check_drop_panic_free.sh` (the rollback
  guard's `Drop` in `fs/` is one that gate scans; every step it takes is a
  field assignment, an infallible `BlockCache` method, or a device write whose
  result is deliberately discarded — a destructor has nowhere to report one),
  `check_process_designator.sh` (any new entry point under `fs/src/fileio/`),
  `check_quota_headroom.sh` (5.5's new `ResourceKind`; also moves on every
  added utest, because each is one more charged process),
  `check_sched_spread.sh` (Phase 5 changes when the flusher kthread is placed).
- **Licensing.** ext2's on-disk layout, feature bits, `errno` values and
  `s_last_orphan` semantics are interface facts and free to use. Upstream
  *prose* — a Linux or Asterinas comment block — is not, and neither is
  anything from a GPL-2.0-only tree. Cite the specification or the documented
  behaviour, never an implementation file. The sharpest exposure is any
  build-time ext2 reader (`gen_verity.py` already reads the superblock's
  block size; 6.6 would read more): superblock-parsing code is exactly what
  gets pasted from a GPL-2.0-only tree. Layout constants only.

## 5. Effort and sequencing

Phase 5 is a week, not two days — the disk root by default moves four ratchets
(`check_test_count.sh`, `check_lockdep_headroom.sh`, `check_sched_spread.sh`,
`check_authority_reachability.sh`), each needing a re-measure and a commit
message explaining the delta. Phase 6 is unbounded and last.

A file already survives a reboot on `/mnt` — `just test-persist` proves it, on
an image that is honestly unverified rather than one that only looked verified,
and it now survives a *crash* as well as an orderly shutdown. Phase 5 alone is
what moves it to `/`.
