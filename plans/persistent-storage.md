# Persistent Storage

Make a file written on one boot readable on the next, on the root filesystem
and under failure. **Done.** A boot with a writable disk mounts it at `/` by
default, what is written there outlives the boot, and `just test-persist` boots
one image twice and reads a payload back from `/var` with CI running it. The
initramfs is the fallback for the machine that has no disk
(`just boot-ramonly`) and for a disk that mounted read-only — which is what
keeps `just boot`'s `verity=require` meaning "the disk I run `/sbin/init` from
is attested" while `/` stays a RAM root on that image.

Crash consistency is closed: an image that was never marked clean mounts
read-only rather than being silently trusted, a filesystem found damaged stops
being written to, an unlinked-but-open file keeps its contents until the last
close on both sides of the disk, and the whole of it is held down by a
crash-injection test that cuts the device at every write boundary.

The block layer is no longer one whole-device filesystem: a GPT or MBR table
is parsed, a partition is a `BlockDevice` window, `/dev/vda1` is a name
`root=` accepts and a node userland can read, `statfs(2)` reports the numbers
the allocator enforces, `mount(2)`/`umount2(2)` exist behind a capability, and
`mmap(2)` of a regular file works in both sharing modes with a stated
coherence rule. Verity is no longer read-only-or-nothing: a v2 trailer carries
the set of blocks a boot rewrote, so an image can be writable and still
attested everywhere nobody has written.

What remains is named in §2 and sequenced in §3: the one-mutex-per-mount
serialisation (G5), a write-ahead journal (G6), and a per-process disk charge
(G10). None of them is load-bearing for persistence; each is a scaling or
fairness property.

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
  real `VIRTIO_BLK_T_FLUSH` behind `BlockDevice::flush`, plus a claim-free
  read-only `BlockReader` for a device nobody has claimed. Probe-order
  indices: `disk0` is the root image, `disk1` a blank scratch device the test
  harness attaches.
- `fs/src/blockdev.rs` — `BlockDevice` trait (`read_at`/`write_at`/`capacity`/
  `write_protected`/`flush`/`checkpoint`) plus `MemoryBlockDevice` for tests.
- `fs/src/ext2/` — superblock/group-desc/inode parsing with validated geometry
  (`Ext2Geometry` is the only constructor of `GroupIdx`), a 512-entry
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
- **A directory of any size is listable, and a mount point appears exactly
  once.** `FileSystem::readdir_cookie` pages over an opaque per-filesystem
  cookie — a byte offset into the directory's data on ext2, which an unrelated
  create or unlink does not shift — carried through `UserFsList.cursor` so
  `fs_list` resumes across calls. The synthesised mount-point pass is resumed
  by **mount identity**, a monotonic `u32` stamped at mount and never reused,
  and the filesystem pass drops a name that is a child mount, so the mount
  pass is the single authority for those names across every page.
- `fs/src/ext2_vfs.rs` — the mount singleton, a 5 s background flusher kthread
  with a dirty-count eager-wake threshold and exponential backoff, and the
  `FileSystem` impl.
- `fs/src/vfs/` — mount table (16 entries, per-component re-resolution,
  canonicalised paths, `MOUNT_RDONLY` consulted at every `vfs::ops` mutation,
  one identity per mount), lexical canonicalisation, path walk carrying the
  mount's flags, `FileSystem` trait. One name-length rule holds across both
  roots: `fs::MAX_NAME_LEN` is enforced at every VFS creation path, so a name
  creatable on the disk root is creatable on a RAM root and openable through a
  listing that truncates into `UserFsEntry.name`.
- **A crash is testable.** `FaultyBlockDevice` (`fs/src/tests.rs`) acknowledges
  writes and drops them after *N* operations, in the `dm-flakey` style — the
  harder half of the two, because a dropped write is what a power cut looks
  like from inside the kernel and nothing is told. The crash test cuts the
  device at *every* write boundary of a create+write+sync workload and asserts
  the survivor is one of the two legal states, bounded black-box crash testing
  after CrashMonkey/B³ (OSDI '18).
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
  changed. A wrong-sized image and one `e2fsck` rejects both fall back to
  `mkfs`, because refreshing binaries into a damaged filesystem propagates the
  damage into the next boot's `/sbin/init`. A v1 (write-protected) trailer
  forces a rebuild; a v2 one is recomputed from the refreshed bytes, which is
  what makes `VERITY=rw` compatible with an in-place refresh.
- **Disk space is held in reserve.** ext2's own `s_r_blocks_count` is
  enforced in `allocate_block_near`, and the same ratio is applied to the
  inode table because ext2 carries no `s_r_inodes_count` and an inode
  exhaustion denies `/sbin/init` a file with every block still free. Kernel
  threads and `TASK_FLAG_SYSTEM` spend into the reserve; everyone else gets
  `ENOSPC` while it holds. The reserve lives on `Ext2Geometry`, read once at
  mount, not on `Superblock` — that struct is copied onto every operation's
  frame and into every transaction snapshot, and the 2 KiB stack gate refused
  the first version that put it there. Set with `mke2fs -m` / `tune2fs -m`;
  `dumpe2fs` reports the number the kernel enforces, and `statfs(2)` reports
  the same number to userland.
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
- **A partition table is parsed, and a partition is a device.**
  `fs/src/partition.rs` reads GPT per the UEFI specification §5.3 — header at
  LBA 1, backup at the last logical block, header CRC32 over `header_size`
  bytes with the field zeroed, entry-array CRC32, `my_lba` agreeing with where
  the header was read from — and falls back to the backup copy before falling
  back to MBR. Structural limits (entry count, entry size, array bytes,
  revision) abort the parse; a single malformed entry is dropped with a klog
  note so one bad row does not lose a usable table. A GPT that was *claimed*
  and did not validate never degrades to "no table", so a stale whole-device
  filesystem is not handed to the mount. MBR reads the LBA fields only, treats
  `0xEE` as protective, and skips extended partitions. A device with neither
  signature is the whole-device case every existing image is, which is what
  keeps `just boot`, `just test`, `just test-persist` and `just boot-ramonly`
  unchanged. `PartitionDevice` is a byte window that refuses a start which is
  not sector-aligned — a misaligned window turns every filesystem write into a
  virtio read-modify-write over bytes outside the partition — and
  `SharedBlockDevice` is the whole-device delegate, so the one exclusive write
  claim can back both the mount and a `/dev` node.
- **`root=` names a device the way every other kernel spells it.** The cmdline
  parse is tokenised rather than a substring search, and accepts `auto`,
  `initramfs`, `virtio`, `/dev/vdX[N]` and `vdX[N]`. A named device or
  partition that is absent degrades exactly as no disk does — loud klog,
  initramfs fallback, never a panic. `/dev/vd*` nodes come from a runtime
  registry in devfs (`devfs_register_block_device`), report
  `FileType::BlockDevice` with the device's capacity as their size, and are
  **read-only**: the write capability is exclusive and held by the mount for
  the kernel's lifetime, and a device-level write behind the ext2 block cache
  would corrupt a live filesystem. A *read* is gated too, on the entitlement
  the block reserve already asks for (a kernel thread or `TASK_FLAG_SYSTEM`):
  a raw device read bypasses every filesystem permission check above it, ext2
  does not zero a freed block, and `unlink` is this kernel's only primitive
  for making something unreadable — so an ungated node would hand any process
  the contents of every deleted file and of every partition the mount
  namespace does not show. A read through a node sees the device, not the
  mount's dirty cache; that is stated on the node.
- **Free space is reportable.** `statfs(2)` and `fstatfs(2)` fill a
  `UserStatfs` byte-identical to Linux x86-64's `struct statfs`, so a port
  needs no translation table. ext2 answers from the live superblock and
  subtracts the reserve the allocator actually enforces from `f_bavail`,
  because a caller cannot spend it and the very next `write` would contradict
  the number. ramfs answers honest inode totals and zero blocks — an
  unbounded heap-backed filesystem has no block count to report — and devfs
  inherits the trait's refusal (`EOPNOTSUPP`) rather than a struct of zeros
  claiming an empty disk. `f_flags` folds the mount's `MOUNT_RDONLY` and the
  filesystem's own runtime read-only latch, which the mount flags never learn
  about.
- **Userland can mount, with authority.** `mount(2)` and `umount2(2)` sit
  behind `Capability::Mount`, conferred by `TASK_FLAG_MOUNT` (granted to no
  shipped program; `TASK_FLAG_SYSTEM` implies it, so init can assemble the
  namespace it boots into) and checked by the dispatcher before the handler
  runs. The mountable set is closed, because `mount(2)` cannot conjure a
  `&'static dyn FileSystem`: four pooled ramfs instances, each with its own
  lock class because a path walk crossing a mount holds one mount's lock while
  taking the next one's; the devfs singleton, at a second path; and the one
  ext2 instance the boot step attached, which refuses a `source` rather than
  pretending a second ext2 could exist. `/` is refused (`EBUSY`) — every open
  descriptor and every cached resolution names the filesystem underneath it —
  and so is any target that equals or prefixes a path in the program-identity
  grant table: program privilege is keyed on a path, so a mount over `/bin`
  would confer `TASK_FLAG_POWER` on whatever the mount contains. Sealing the
  inodes, which Phase 5 did, does not cover that: a mount is a namespace
  change over the path, not a change to the file.
  `umount2` answers `EBUSY` while an open reference names the filesystem,
  `MNT_DETACH` overrides that, and the lazy form is cheap here precisely
  because every filesystem is a `static`: the surviving reference holds a
  `&'static dyn FileSystem` and stays readable after the name is gone. A
  detached pool instance is retired rather than reset — resetting it under a
  reader would hand that reader an empty filesystem — and a later claim
  reclaims it once the last reference has gone.
- **A file can be mapped.** `mmap(2)` accepts a regular file in both sharing
  modes and `msync(2)` commits one. Population is eager, at `mmap` time: the
  #PF handler runs on IST4 with interrupts off, so it can neither take
  `CACHED_EXT2` nor park on a virtio completion, and a file page can therefore
  never be faulted in — which is also why a mapping reaching past EOF is
  refused instead of deferring a `SIGBUS` this kernel has no path to deliver.
  **The coherence rule** (`fs/src/filemap.rs`): a per-inode page set is the
  authority for the pages it holds while a shared mapping is live;
  `read(2)`/`write(2)` route through it for the ranges it covers, and
  writeback goes out through `FileSystem::write`, so it inherits the
  filesystem's own ordering. Nothing maps an ext2 `BlockCache` frame, so that
  cache keeps its one-handle-per-frame invariant, its LRU eviction, its
  rollback snapshots and its `data=ordered` barriers; the cost is one 4 KiB
  copy per mapped page, against an alternative that would need
  `AnyUFrameMeta` on a page-cache meta, a pin protocol shrinking a 512-entry
  cache to nothing under a large mapping, and would publish user bytes at
  eviction time without a barrier. A mapping's write access is authorised
  once, at `mmap`, by the descriptor's open mode: `open(2)` is where the seal
  and the read-only mount are checked, so the mode is the proxy for both, and
  `mprotect` refuses to widen a file mapping to writable because the
  descriptor that authorised it is not reachable from there. Every page of a
  set a *writable* shared mapping reached is written back, because a user
  store sets the CPU's PTE dirty bit and nothing here harvests it; a
  read-only mapping arms nothing, so it cannot rewrite an unmodified file or
  un-attest its blocks. A set is unkeyed before a name is removed
  (`detach_inode`) and before a truncation changes the size, so a reallocated
  inode number cannot resolve to the previous file's pages and a truncate
  cannot be undone by a writeback; a store through a mapping of a removed
  file is no longer written back, because the blocks may already belong to
  something else. `MAP_PRIVATE` is an eager copy from the same authority, so
  a private mapping cannot see bytes a shared mapper has superseded — and it
  stays legal on a read-only descriptor, because the store never reaches the
  file.
- **An image can be writable and attested at once.** `fs/src/verity.rs` — a
  CRC-32-per-block trailer appended by `scripts/gen_verity.py`, sector-padded
  so a sector-granular capacity still reaches it, recognised only beyond the
  filesystem's own extent. A **v1** trailer write-protects the device, which is
  what keeps "verified" meaning what it says for the shipped image. A **v2**
  trailer adds a persisted bitmap of rewritten blocks between the hash array
  and the header, with its CRC in the header's previously-unread `reserved`
  field: a write un-attests every block it touches — partial writes included,
  because a partly-rewritten block is one no hash describes — and forwards,
  and a read verifies only a block that is fully contained in it and still
  attested. Four rules make that safe rather than merely convenient. The block
  holding the superblock is permanently unattested, which is this plan's
  answer to G1: superblock I/O is sub-block and never verified anyway, and
  `stamp_not_clean` rewrites it on every mount. The bitmap is checkpointed
  **before** the filesystem marks itself clean, so a crash in between leaves
  the image unclean. A mount that finds an unclean image trusts *nothing* for
  that boot — verity off, not a false integrity failure. And a bitmap whose
  CRC does not match is treated the same way, because a torn bitmap write is a
  crash rather than an attack, and neither refuses the mount. `VERITY=rw`
  builds one; `fs/assets/ext2-persist.img` uses it, and two boots of that
  image report `4095 of 4096 blocks still attested, device writable` and then
  `3846 of 4096` — the rewritten set persisted across the reboot, with every
  untouched block still verified on read.
- **Filesystem identity is an address, not a fat pointer.** `ptr::eq` on a
  `&dyn FileSystem` compares the vtable too, and the coercion in one crate
  produces a different vtable from the same instance coerced in another, so a
  cross-crate comparison reported two references to one filesystem as
  different. `vfs::traits::same_filesystem` is the one rule; it is what the
  open-inode table, the cross-device rename check, the re-resolution check and
  the mount pool all use.

## 2. The gaps

Grouped by subsystem; the phase list below sequences them.

### G1 — Closed: verity states what it covers, and what it does not

Superblock I/O is sub-block — `read_at(1024, …)` direct to the device,
bypassing the cache — and only a block fully contained in a read is verified.
A v2 trailer therefore marks the block holding the superblock permanently
unattested, at build time and at mount, rather than pretending otherwise. Sub-
block reads elsewhere remain unverified by construction; there are none outside
the superblock path.

### G2 — Closed: a verified image can be a writable one

Was: a verity trailer made the device write-protected, so the image that
persists got no integrity checking at all. Now a v2 trailer records what a boot
rewrote and keeps everything else attested across reboots; §1 records the
ordering that makes the crash case degrade to unverified rather than to a false
failure. The shipped `ext2.img` deliberately stays v1: it is read-only by
design, and `just boot`'s `verity=require` is an assertion about *that* image.

### G5 — A sync stalls every user of its mount

`CACHED_EXT2` is one global sleeping mutex held across all of ext2's block I/O,
so the caller that wins it holds it for a whole writeback pass while every path
walk and `exec` on that mount waits. `sync(2)` is unprivileged and takes no
fd, which makes that stall userland-reachable. Concurrent callers do not
multiply the cost — the second one in finds nothing dirty and nothing
unbarriered and returns without touching the device — but nothing bounds the
wait behind the pass in flight. A lock finer than one per mount is the fix.

**Not closed by the page cache, and the plan was wrong to expect it to be.**
File mappings serve their resident pages without taking `CACHED_EXT2` at all,
so a mapped read is off that lock — but population, writeback and every
`read(2)`/`write(2)` miss still go through it, and `exec` still stages an ELF
through it. What would close G5 is per-inode and per-cache-bucket locking
inside ext2, which is a rewrite of `Ext2Fs`'s borrow structure rather than a
consequence of anything in Phase 6.

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
this needs an operation whose working set exceeds the 512-entry cache, but
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
same path in between still passes. File mappings are no longer exposed to that
window — a page set is unkeyed before the name that could free its inode is
removed — but the descriptor path still needs an inode cache keyed by
`(filesystem, inode)` and a parent lock held across the walk.

### G8 — Closed: `CLOCK_REALTIME` answers the wall clock

`syscall_clock_gettime` reads `realtime_ns()`, and the superblock timestamps
`e2fsck` reports are written. See §1.

### G9 — Closed: a paged listing resumes on identity

Was: the synthesised mount-point entries were resumed by *ordinal*, so a mount
or unmount between two pages could drop or repeat one, and a real directory
entry shadowed by a mount was de-duplicated only within a page. Now a mount
carries a monotonic `u32` identity that is never reused, the cursor's mount
phase resumes after the last identity it emitted, and the filesystem pass drops
a name that is a child mount so the mount pass is the single authority for it.
The regression test fails against an ordinal cursor.

### G10 — Closed as a reserve, open as a per-process charge

Was: any unprivileged process could write until `NoSpace` and deny the disk to
`/sbin/init` and the kernel's own writes. Now §1 records ext2's own answer:
`s_r_blocks_count` and its inode-table ratio, spendable only by kernel threads
and `TASK_FLAG_SYSTEM`, and `statfs(2)` reports the number that is actually
enforced. That is a *system* floor, not a *per-process* ceiling — one process
can still consume everything above the reserve and deny the disk to every
other unprivileged process. A `ResourceKind` charged in the allocator is what
closes that half, and a ninth kind moves every row of
`check_quota_headroom.sh`'s gate file, so it wants its own commit.

### G12 — Closed: the build preserves a writable image

See §1. `PRESERVE_FS_IMAGE=1` is opt-in rather than the default because the
*tests* image must be regenerated per run: a writable, persistent `/` makes
every filesystem test a mutation of the image the next boot runs `/sbin/init`
from, and CI is order-independent only if each run starts from a fresh one.
The harness already regenerates the scratch and verified disks per run; the
tests image now joins them.

### G13 — Closed: the block layer sees partitions, devices and free space

Was: no partition-table parsing, no block-device nodes, no `statfs`, and a
name-length limit that differed between the two roots. §1 records all four.
What is deliberately left:

- **A `/dev/vd*` node is read-only and its reads are privileged.** One
  exclusive write claim exists per device and the mount holds it; a
  shared-writer arbiter would be a way to corrupt a live filesystem from
  userland. Reads need a kernel thread or `TASK_FLAG_SYSTEM`, for the reason
  §1 gives. `root=/dev/vda1` is spellable regardless, which was the stated
  goal.
- **No block ioctls.** `syscall_ioctl` is TTY-only and `FileOps::ioctl` is
  reached from nothing, so `BLKGETSIZE64` has no route. `lseek(SEEK_END)` and
  `statfs` answer the same questions today; `fstat` on a node ≥ 4 GiB still
  truncates its size, because `UserFsStat.size` is a `u32`.
- **`MAX_MOUNTS` is 16 and now user-visible.** A full table is `ENOSPC`, as is
  an exhausted ramfs pool: both are a fixed kernel table with no room, where
  `EMFILE` would name a per-process ceiling neither of them is.
- **A name longer than `MAX_NAME_LEN` on a foreign image** stays readable — the
  path walk applies no per-component limit — but lists truncated. Only
  *creation* is capped.

### G14 — Closed: a file can be mapped, with a stated coherence rule

Was: `syscall_mmap` rejected any fd whose kind was not `FileKind::Memfd`. §1
records the design and the rule. The residuals are stated rather than hidden:

- **Population is eager**, so a mapping costs its whole range up front and
  `mmap` of a large file is a large read. Demand paging a file needs a fault
  context that can sleep, which IST4 with interrupts off is not.
- **A mapping past EOF is refused** rather than faulting `SIGBUS` on the hole,
  and the mapped length cannot grow the file.
- **Dirtiness is not tracked per page.** Nothing harvests the PTE dirty bit,
  so every page of a set a *writable* shared mapping reached is written back.
  A read-only mapping arms no writeback at all.
- **The registry is bounded** at 16 inodes and 1024 pages, global and charged
  to nobody: the seventeenth mapped inode is `ENOMEM`, so one process holding
  sixteen mappings denies file `mmap` to every other while it holds them.
  Queued frees self-heal — `acquire` drains them at its head and a release
  that can sleep frees inline — so the denial lasts only as long as the
  mappings do; charging the pages to the mapper is Phase 7.3's job and is
  tracked as `SLOPOS-2026-0054`.
- **`exec` still stages**, so executables are not demand-paged off the disk.

---

## 3. Plan

Phases 0–6 are done and their results are §1. What is left is not a phase: it
is three independent pieces of work, each with its own reason to wait.

### Phase 7 — the scaling residuals

Do each when a measurement, not a plan, asks for it.

- [ ] **7.1** G5: a lock finer than one per mount inside ext2 — per-inode for
  the record and its blocks, per-bucket for the cache index — so a writeback
  pass stops holding every path walk and every `exec` on the mount behind it.
  The two Phase 5 mitigations (`EXEC_READ_CHUNK` at 64 KiB, a 512-entry cache)
  bound the symptom; file mappings take their resident reads off the lock
  entirely; neither is the fix.
- [ ] **7.2** G6: a write-ahead journal, which is the only thing that retracts
  a block an eviction already put on the device, and the only thing that makes
  an operation larger than the undo budget performable rather than refused.
- [ ] **7.3** G10's open half: a per-process disk `ResourceKind` charged in the
  allocator, so one process cannot consume everything above the system
  reserve. It moves `KIND_COUNT` and every per-kind row of the quota gate, so
  it belongs in a commit of its own.

Each ends green on `just test` plus the pre-commit gate sequence in
`AGENTS.md`.

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
  `BlockCache` handles in one frame exceed the gate, and the mount syscall's
  argument decoding is split for the same reason. New ext2 operations with
  several live handles will hit this; build in place via `KBox::try_init`.
- **Lock order.** `CACHED_EXT2` is a *sleeping* mutex held across block I/O.
  `Ext2CacheReclaim` uses `try_lock` only, for the reason its comment gives.
  `MOUNT_TABLE` is an `IrqRwLock` at registry level and must never be held
  across a filesystem call. The file-mapping order is
  `FILEMAP_IO → FILEMAP → CACHED_EXT2` and never the reverse. Any new lock is
  a new lockdep class and moves `check_lockdep_headroom.sh` — re-measure with
  `--emit-allowlist`, never hand-edit the gate file.
- **No spinning lock may span a filesystem call.** A `SpinLock` holds
  preemption off, and any write that has to allocate parks on the virtio
  completion, so the scheduler's `assert_not_blocking_while_atomic` fires. A
  lock held across `FileOps::read`/`write`/`sync` must be a sleeping `Mutex`
  (`OpenFile.position_lock` is one for exactly this reason), and its acquire is
  fallible — a killed task returns `EINTR` rather than proceeding unserialised.
- **Nothing on the fault path may sleep.** The #PF handler runs on IST4 with
  interrupts off; a file page is populated at `mmap` time for that reason
  alone. Anything reached from process teardown (a mapping release, a
  descriptor drop) runs under a preempt guard and may neither block nor
  allocate.
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
  `check_frame_ownership.sh` (a page handed from a claim into a PTE),
  `check_quota_headroom.sh` (a per-process disk `ResourceKind` in 7.3 moves
  `KIND_COUNT` and every per-kind row; it also moves on every added utest,
  because each is one more charged process).
- **Licensing.** ext2's on-disk layout, feature bits, `errno` values and
  `s_last_orphan` semantics are interface facts and free to use, as are GPT's
  header fields, its CRC32 polynomial and Linux's `struct statfs` layout and
  `MS_*`/`MNT_*` values. Upstream *prose* — a Linux or Asterinas comment block
  — is not, and neither is anything from a GPL-2.0-only tree. Cite the
  specification or the documented behaviour, never an implementation file. The
  sharpest exposure is any build-time reader (`gen_verity.py` parses the ext2
  superblock and its own trailer): superblock-parsing code is exactly what gets
  pasted from a GPL-2.0-only tree. Layout constants only.

## 5. Effort and sequencing

Phase 6 ran as one pass across six independent slices — partitions and device
nodes, `statfs`, `mount`, file mappings, verity-on-writable — with the shared
ABI, the capability and the dispatch table decided up front so the slices did
not have to negotiate them. What cost the most was not any of the six: it was
two bugs the new code only made visible. `ptr::eq` on a `&dyn FileSystem`
compares vtables, so every cross-crate filesystem-identity comparison in the
tree had been one coercion away from answering wrongly; the mount pool was
simply the first caller to depend on it. And a page set keyed on
`(filesystem, inode)` outlived the inode, so a freed inode number reallocated
to the next file resolved to the previous file's pages — found by a userland
test whose two cases ran in the wrong order, which is the argument for having
written it.

Two review rounds then found what the tests did not, and the pattern is worth
keeping: every finding was a *new way to reach an old object* that did not
re-ask the question the old way answered. `mmap` reached an inode without the
descriptor's open mode, so it walked around the seal; `mprotect` reached the
mapping without the descriptor at all, so it walked around the fix for the
first one; a `/dev` node reached the device without any check, so it walked
around `unlink`; and a mount reached `/bin` without touching the inodes Phase
5 had sealed. None of them is a bug in the mechanism they bypassed. The
questions to ask of the next new path are therefore: which gate does the old
path pass through, is that gate reachable from here, and if not, what refuses
instead.

A file survives a reboot on `/` — `just test-persist` proves it from `/var`,
and it survives a *crash* as well as an orderly shutdown. An image can now be
writable and attested at the same time, proven by two boots of one v2 image
reporting the rewritten set persisted across the reboot: 4095 of 4096 blocks
attested on the first, 983 on the second, with every block nobody wrote still
verified on read. What is left in §3 is throughput and fairness, not
persistence.
