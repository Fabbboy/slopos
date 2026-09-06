# Persistent Storage

Make a file written on one boot readable on the next, on the root filesystem
and under failure. **Done for the root.** A boot with a writable disk mounts it
at `/` by default, what is written there outlives the boot, and
`just test-persist` boots one image twice and reads a payload back from
`/var` with CI running it. The initramfs is the fallback for the machine that
has no disk (`just boot-ramonly`) and for a disk that mounted read-only — which
is what keeps `just boot`'s `verity=require` meaning "the disk I run
`/sbin/init` from is attested" while `/` stays a RAM root on that image.

Crash consistency is closed: an image that was never marked clean mounts
read-only rather than being silently trusted, a filesystem found damaged stops
being written to, an unlinked-but-open file keeps its contents until the last
close on both sides of the disk, and the whole of it is held down by a
crash-injection test that cuts the device at every write boundary.

Verity is settled and out of scope here: a trailer makes the device
write-protected and the mount read-only (`MOUNT_RDONLY`, `EROFS`), the shipped
`ext2.img` carries one and `just boot` asserts it with `verity=require`, and
the image the suite writes to is built `VERITY=off`. The one open verity item
is an *upgrade* — persisting the set of blocks a boot rewrote so an image can
be both writable and mostly attested — and it lives in Phase 6 as 6.6, because
nothing before it needs it.

What remains is Phase 6: block-layer breadth, and the one-mutex-per-mount
serialisation (G5) that the disk root now makes the ordinary case.

## Why this matters beyond files

Persistence is the enabler under a long tail of features that were fake or
impossible while `/` was RAM: a keymap choice that survives (`/etc/keymap` now
lands on the disk root and `init` re-applies it), user data of any kind, a
package or application store, logs that outlive the crash that produced them,
a W/L ledger with somewhere to live other than the boot medium
(`plans/microtransactions.md` Phase 1 chose a Limine module precisely because
there was no writable store), shell history, compositor layout, wallpaper
selection, and every "settings" surface the GUI would otherwise have to
pretend to have. `/etc` and `/var` exist on both roots for exactly this, and
`BOOT_FLAG_ROOT_PERSISTENT` is how a program tells a root that persists from
one whose successful `fsync` still loses the data at power-off.

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
- **The disk is the root.** `root=auto` picks a *writable* disk over the
  initramfs (`boot_step_rootfs_init`); the attach verdict is computed once and
  memoised, because it claims the device's exclusive write capability. A
  read-only disk falls back to the initramfs exactly as no disk does — nothing
  written to such a root survives, so preferring it buys no persistence and
  costs a writable `/` — and is mounted at `/mnt` instead. `root=virtio` still
  forces it, honestly read-only. `BOOT_FLAG_ROOT_PERSISTENT` says which case a
  boot is, and the persist test asserts the disk case on the tests image.
- **The build no longer wipes the disk.** `PRESERVE_FS_IMAGE=1` refreshes a
  writable image's binaries in place via `debugfs` and keeps everything the
  guest wrote; a stamp of the binaries and assets skips the work when nothing
  changed. A verified image, a wrong-sized one, and one `e2fsck` rejects all
  fall back to `mkfs`, because refreshing binaries into a damaged filesystem
  propagates the damage into the next boot's `/sbin/init`.
- **Disk space is held in reserve.** ext2's own `s_r_blocks_count` is
  enforced in `allocate_block_near`, and the same ratio is applied to the
  inode table because ext2 carries no `s_r_inodes_count` and an inode
  exhaustion denies `/sbin/init` a file with every block still free. Kernel
  threads and `TASK_FLAG_SYSTEM` spend into the reserve; everyone else gets
  `ENOSPC` while it holds. The reserve lives on `Ext2Geometry`, read once at
  mount, not on `Superblock` — that struct is copied onto every operation's
  frame and into every transaction snapshot, and the 2 KiB stack gate refused
  the first version that put it there. Set with `mke2fs -m` / `tune2fs -m`;
  `dumpe2fs` reports the number the kernel enforces.
- **The grant directories are sealed.** A sealed binary could not be
  overwritten, but its *directory* could be renamed aside and a fresh
  `/bin/halt` planted under the path the grant table is keyed on. `/bin` and
  `/sbin` now carry the seal on both roots (`EXT2_IMMUTABLE_FL` on disk, set
  after the cpio unpack on ramfs), and ramfs enforces a parent's seal on
  create, unlink and rename as ext2 always had. `spawn_privilege_test`'s
  `grant_directories_are_sealed` fails with either half reverted.
- **A blocking primitive is not re-entered by its own wake.** The scheduler's
  deferred-reschedule and trap-exit paths skip a current task that is `Ready`
  as well as one that is `Blocked`: a wake that lands between the Blocked-CAS
  and the deschedule enqueues the task still running, and a `schedule()` from
  either path then dequeued the caller as its own successor and spun on its
  own `on_cpu` flag forever. Reachable on a RAM root in principle, but the
  disk root is what made it ordinary — every `exec` parks on a virtio
  completion — and it stalled one run in four until it was found. `exec`
  stages an ELF in 64 KiB chunks rather than 4 KiB, because on ext2 each
  chunk is a `CACHED_EXT2` acquisition and a device round trip, and the block
  cache is 512 entries rather than 128 so a shell binary fits it.

## 2. The gaps

Grouped by subsystem; the phase list below sequences them.

### G1 — Closed: the disk is the root, and the path is exercised

Was: `root=auto` took the initramfs whenever a module was present, and
`root=virtio` was named by no recipe, test or CI job. Now §1 records the
default, the read-only fallback, and the persist test that asserts it. The
unexercised path was also a broken one — the scheduler self-wait §1 describes
stalled a quarter of disk-root runs and had never been seen because nothing
ran that way.

One fact carries forward to 6.6: **verity does not cover the superblock.**
Superblock I/O is sub-block — `read_at(1024, …)` direct to the device,
bypassing the cache — and only a block fully contained in a read is verified.
On a verified device this is moot, since the device refuses the write, but 6.6
has to say what it means for block 0.

### G2 — A verified image cannot be a writable one

Settled by construction rather than open: a verity trailer makes the device
write-protected, so the "written on boot 1, unverifiable on boot 2" case
cannot arise. The cost is that the persistent root gets no integrity checking
at all: the image that persists is built `VERITY=off`, and a verified disk is
not chosen as the root. Buying most of that coverage back is 6.6.


### G5 — A sync stalls every user of its mount

`CACHED_EXT2` is one global sleeping mutex held across all of ext2's block I/O,
so the caller that wins it holds it for a whole writeback pass while every path
walk and `exec` on that mount waits. `sync(2)` is unprivileged and takes no
fd, which makes that stall userland-reachable. Concurrent callers do not
multiply the cost — the second one in finds nothing dirty and nothing
unbarriered and returns without touching the device — but nothing bounds the
wait behind the pass in flight. A lock finer than one per mount is the fix,
which makes this the page-cache work in 6.5 rather than a sync change.

With the disk as the root this is the ordinary case, not an edge: every
`exec` on the machine reads its ELF under that lock. Phase 5 took the two
cheap wins — `EXEC_READ_CHUNK` at 64 KiB so a spawn is a handful of
acquisitions rather than one per block, and a 512-entry block cache so a
shell binary is not evicted while it is being read — and measured the suite
green 21 runs out of 21 afterwards. Neither is the fix; both are what made
the scheduler bug above visible as a bug rather than as slowness.

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

### G10 — Closed as a reserve, open as a per-process charge

Was: any unprivileged process could write until `NoSpace` and deny the disk to
`/sbin/init` and the kernel's own writes. Now §1 records ext2's own answer:
`s_r_blocks_count` and its inode-table ratio, spendable only by kernel threads
and `TASK_FLAG_SYSTEM`. That is a *system* floor, not a *per-process* ceiling
— one process can still consume everything above the reserve and deny the
disk to every other unprivileged process. A `ResourceKind` charged in the
allocator is what closes that half, and it was deliberately not done here:
the reserve is what the plan's own threat ("deny it to `/sbin/init`") needed,
it is the answer every ext2 implementation already agrees on, and a ninth
kind moves every row of `check_quota_headroom.sh`'s gate file on the same
commit as the disk root. It belongs with `statfs` in 6.3, where the number a
process is charged against becomes one it can read.

### G12 — Closed: the build preserves a writable image

See §1. `PRESERVE_FS_IMAGE=1` is opt-in rather than the default because the
*tests* image must be regenerated per run: a writable, persistent `/` makes
every filesystem test a mutation of the image the next boot runs `/sbin/init`
from, and CI is order-independent only if each run starts from a fresh one.
The harness already regenerates the scratch and verified disks per run; the
tests image now joins them.

### G13 — The block layer sees one whole-device filesystem

No partition-table parsing (GPT or MBR), so an image is always "the filesystem
starts at byte 0". No block-device nodes in `/dev`, so `root=/dev/vda1` cannot
be spelled and `root=` selects by driver name only. No `statfs`, so nothing can
report free space. `MAX_MOUNTS` is 16, and `MOUNT_RDONLY` is the only mount
flag with a reader.

The name-length limits differ between the two roots, which are now
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
`AGENTS.md`. Numbering starts at 6 because the earlier phases are done and
their results are §1; the numbers are kept so commit messages and issue
references to them still resolve.

### Phase 6 — Block layer breadth

Do it when a second device or a real disk demands it.

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
  `check_quota_headroom.sh` (a per-process disk `ResourceKind` in 6.3 moves
  `KIND_COUNT` and every per-kind row; it also moves on every added utest,
  because each is one more charged process).
- **Licensing.** ext2's on-disk layout, feature bits, `errno` values and
  `s_last_orphan` semantics are interface facts and free to use. Upstream
  *prose* — a Linux or Asterinas comment block — is not, and neither is
  anything from a GPL-2.0-only tree. Cite the specification or the documented
  behaviour, never an implementation file. The sharpest exposure is any
  build-time ext2 reader (`gen_verity.py` already reads the superblock's
  block size; 6.6 would read more): superblock-parsing code is exactly what
  gets pasted from a GPL-2.0-only tree. Layout constants only.

## 5. Effort and sequencing

Phase 6 is unbounded and last. 6.5 is the one item the disk root made urgent
rather than optional: G5 is now every `exec`'s path, and the two mitigations
Phase 5 took are a bound on the symptom, not the fix.

A file survives a reboot on `/` — `just test-persist` proves it from `/var`,
on an image that is honestly unverified rather than one that only looked
verified, and it survives a *crash* as well as an orderly shutdown. Phase 5
ran a week as estimated, and most of it went on the two things nobody had
measured: a scheduler self-wait the unexercised path had been hiding, and a
directory that the seal on its contents had never protected.
