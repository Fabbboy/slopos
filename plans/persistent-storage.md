# Persistent Storage

Make a file written on one boot readable on the next. SlopOS has an ext2
implementation, a virtio-blk driver, a write-back block cache, ordered
writeback and a background flusher, and none of it adds up to persistence:
the root filesystem is a RAM image, so nothing written to `/` outlives the
boot. The disk is opened read-write at `/mnt` on every default boot, which
makes the second problem reachable today rather than only under
`root=virtio` — writes that reach the disk make those blocks unreadable on
the next boot, because the verity trailer's build-time CRCs no longer
describe them.

This plan closes both, then fills the write-path and durability gaps that a
persistent root turns from latent into load-bearing.

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
- `fs/src/ext2_vfs.rs` — the mount singleton, a 5 s background flusher kthread
  with a dirty-count eager-wake threshold and exponential backoff, and the
  `FileSystem` impl.
- `fs/src/vfs/` — mount table (16 entries, longest-prefix, per-component
  re-resolution), lexical canonicalisation, path walk, `FileSystem` trait.
- `fs/src/verity.rs` — a CRC-32-per-block trailer appended by
  `scripts/gen_verity.py`, verified at read time for blocks not written since
  mount.

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

The disk is not idle, though, and two details matter for Phase 1:

- **ext2 is initialised on `disk0` unconditionally**, before and independent of
  the `ROOTFS_IS_RAMFS` branch (`boot/src/boot_services.rs:101-121`). That
  claims the exclusive `BlockWriteToken` and runs `mark_dirty_on_disk`, which
  writes the superblock at mount (`fs/src/ext2_vfs.rs:269-276` →
  `fs/src/ext2/mod.rs:151-153`). `mark_clean` writes it again on an orderly
  shutdown. **The default boot writes to the disk twice.**
- **`/mnt` is mounted writable** (`flags = 0`, `boot_services.rs:124`), and
  nothing reads that field (G13). Anything userland writes under `/mnt` reaches
  the disk through the flusher.

A change that makes a verified device read-only therefore alters the *current
default boot*, not just a hypothetical `root=virtio` one.

### G2 — Verity makes a written block unreadable on the next boot

`ext2_vfs_init_with_device` wraps the device in `build_verified` before
anything else (`fs/src/ext2_vfs.rs:279`). `VerifiedBlockDevice` holds the
trailer's build-time CRC per block and a `written` bitset of blocks excused
from checking. That bitset is in memory only, by construction, and `hashes` is
never mutated. So:

1. Boot 1 writes block *N*. The bit is set; reads pass.
2. Reboot. The bitset is gone. The trailer still carries the *build-time* CRC
   of block *N*.
3. Boot 2 reads block *N*, computes a CRC that cannot match, and returns
   `IntegrityFailure` → `VfsError::IoError`.

The cache reads and writes exactly one aligned, block-sized region
(`fs/src/ext2/cache.rs:114-122`, `:203`, `:225-226`), which is precisely the
condition `read_at` verifies under. So every block the write path dirties
through `BlockCache` fails on the next boot: block and inode bitmaps, group
descriptors, inode-table blocks, indirect blocks, directory blocks and file
data. Reproduced against the shipped image: flipping one byte in block 1 moves
its CRC from `0x845b4943` to `0x01407b0d`, and the replayed verification loop
rejects it.

**Block 0 is the exception, and it is a hole rather than a mitigation.**
Superblock I/O is sub-block — `read_at(1024, &mut [u8; 1024])` /
`write_at(1024, …)` direct to the device, bypassing the cache
(`fs/src/ext2/mod.rs:78-80`, `:147-153`, `:611-618`). Verity's
`(b + 1) * bs <= end` never holds for it, and block 0 is unreachable through
the cache anyway because `BlockNum::is_valid()` treats 0 as absent
(`fs/src/ext2/types.rs:44-46`). So verity gives **zero coverage of the
superblock**, and `mark_dirty_on_disk` rewrites it on every boot of the shipped
verified image without tripping anything.


### G3 — The ext2 write surface has holes the VFS reports as "not supported"

`impl FileSystem for T where T: Ext2VfsBackend` (`fs/src/ext2_vfs.rs`) defines
`name`, `root_inode`, `lookup`, `stat`, `read`, `write`, `create`, `unlink`,
`readdir`, `truncate` (an explicit refusal) and `sync`. Everything else takes
the trait's default:

| Operation | On ext2 today | Consequence on a persistent root |
|---|---|---|
| `truncate` | `NotSupported` | `open(O_TRUNC)` fails, so `std::fs::write` to an **existing** file fails. Overwriting a file is impossible. |
| `rename` | `NotSupported` | `SYSCALL_RENAME` works only on ramfs. Write-to-temp-then-rename, the one atomic-update idiom, does not exist. |
| `symlink` / `readlink` | `NotSupported` | `fs/src/ext2/symlink.rs` is complete and has **no callers**. |
| `set_mode` | default `Ok(())` — a silent no-op | `chmod` reports success and changes nothing. |
| `set_sealed` | `NotSupported`; `stat` hardcodes `sealed: false` (`ext2_vfs.rs:161`) | **The seal does not exist on disk.** See G4. |
| directory removal | `unlink_entry` refuses with `IsDirectory` (`ext2/mod.rs:501`) | No `rmdir` at all — on ext2 or as a syscall. `slibc`'s `rmdir` is `unlink` (`slibc/std_pal/fs/slopos.rs:679`). `dir::is_dir_empty` has no callers. |

Beyond the table: `links_count` is incremented on `mkdir` and never decremented
(`ext2/mod.rs:475`); `used_dirs_count` in the group descriptor is parsed,
encoded and never maintained (`ext2/ondisk.rs:154,168,178` are its only three
occurrences); `i_dtime` is left zero on unlink (`ext2/mod.rs:530`) where every
other ext2 implementation stamps it; `i_size_high` is never read although
`RO_COMPAT_LARGE_FILE` is in `SUPPORTED_RO_COMPAT`, so since `Inode::size` is a
`u32`, a file above 4 GiB reports the wrong size; and
`Ext2Fs::read_file`/`write_file` take `offset: u32` (`ext2/mod.rs:256-259`,
`:274`) with `ext2_vfs` casting `offset as u32` (`ext2_vfs.rs:165`, `:169`), so
an offset past 4 GiB wraps silently. The inner `ext2::file` functions already
take `u64`, so the truncation is exactly at the `Ext2Fs` boundary.

### G4 — A persistent root has no seal, and the privilege model rests on one

`FileStat::sealed` is the mechanism that stops a task overwriting
`/bin/compositor` and spawning the replacement into that path's grant —
`core/src/exec/grants.rs` says outright that program-identity privilege "is
only as strong as write protection on `/bin`". The seal is set when the
initramfs cpio is unpacked (`fs/src/cpio.rs` → `vfs_set_sealed`), which
happens only on the RamFs path. On `root=virtio` there is no cpio unpack, ext2
cannot store the bit, and `stat` reports `sealed: false` unconditionally. Every
binary under `/bin` is writable by any task that can open it.

This is a security finding. It needs a `CVSS.md` triage entry
once `root=virtio` is a reachable configuration — and it must be closed in the
same change that makes that path the default, not after.

### G5 — Userland cannot ask for durability

There is no `fsync`, no `fdatasync`, no `sync`, and no `O_SYNC`. `SYSCALL_TABLE_SIZE`
is 177 with 57 free slots. `FileSystem::sync` exists and `ext2_vfs_sync` works;
nothing above the VFS calls either except shutdown. The only paths to stable
storage are the 5 s flusher tick, cache eviction under pressure, and
`ext2_vfs_shutdown_sync` on an orderly `kernel_shutdown` / `kernel_reboot`.

An application that writes a config file and the machine loses power one second
later loses the write, with no API it could have used to prevent that.

### G6 — A failed operation leaves partial metadata in the cache

`StaticExt2Vfs::with_fs` carries the admission:

> `TODO(tech-debt)`: a failed op leaves its dirtied blocks cached, so a later
> sync can persist partial metadata — fix is a write-ahead journal.

`create_inode_entry` is the sharp case: it allocates an inode, allocates a
directory data block, writes the new inode, then appends the parent's directory
entry. A failure at the last step leaves an allocated, written, unreferenced
inode in the cache, and the next flush — the flusher's, not the caller's —
writes it to disk. The superblock free counts are correctly rolled back (they
are published only on success), which makes the on-disk bitmaps and the
on-disk counts disagree.

### G7 — Nothing repairs a dirty image, and nothing notices it is dirty

`mark_dirty_on_disk` sets `EXT2_ERROR_FS` at mount and `mark_clean` clears it
on an orderly shutdown, which is exactly right. But the next mount **reads that
state and ignores it**: there is no fsck, no orphan-inode list, no
`errors=remount-ro`, no mount-count or last-check bookkeeping. A crash mid-write
produces an image that mounts and is silently trusted.

ext2 upstream's answer to the two cases a crash can leave behind is the orphan
inode list (`s_last_orphan`, threaded through `i_dtime` on each orphan): an
unlinked-but-still-open inode and a partially-completed truncate. SlopOS has
neither the list nor the situations it covers — because `unlink` frees the
blocks immediately regardless of open descriptors, which is itself a POSIX
violation and a use-after-free of the inode number for any fd still holding it.

### G8 — There is no wall clock, so every persisted file is stamped zero

`fs/src/ext2/time.rs::now_unix()` returns 0, and has no callers at all — the
inode construction sites write literal zeros (`ext2/mod.rs:420-423`, `:527-530`,
`ext2/symlink.rs:25-28`), and `write_file` never stamps `mtime`. This is not a
lazy stub: there is no wall-clock source to read. `CLOCK_REALTIME` aliases
monotonic uptime (`core/src/syscall/core_handlers.rs:46-50` returns
`monotonic_ns()` for both clock ids), no `DateAtBootRequest` appears in
`boot/src/limine_protocol.rs` although the pinned crate provides one, and there
is no CMOS/RTC driver.

On a RAM root this is invisible. On a persistent one every file carries
`mtime = ctime = atime = 0`, so any "newer than" comparison — a build tool, a
cache, a sync — is meaningless, and the superblock bookkeeping `e2fsck` reads
(`s_wtime`, `s_mtime`, `s_lastcheck`) cannot be written at all.

### G9 — Directory listings are capped at 64 entries with no cursor

`vfs_list` reads into a fixed `[0u64; 64]` and stops at 64
(`fs/src/vfs/ops.rs:181`, `:187`), always calling `readdir(inode, 0, …)`, and
`syscall_fs_list` has no continuation cookie (`abi/src/fs.rs:5`
`USER_FS_MAX_ENTRIES = 64`). `FileSystem::readdir` takes an offset, so the cap
is purely the VFS/ABI layer. Persistence is exactly what makes a directory
exceed 64 entries; today the 65th file is invisible to `ls` and unlistable by
any means. The ext2 `readdir` offset is a linear entry *index*
(`ext2_vfs.rs:196-210`), so paging over it is O(n²) and unstable across a
concurrent unlink — a real directory cookie is the fix, not a bigger array.

### G10 — Disk space is not a charged resource

`abi/src/quota.rs` enumerates eight `ResourceKind`s and none of them covers
disk blocks or on-disk inodes. With a writable root, any unprivileged process
can write until `Ext2Error::NoSpace` and deny the disk to every other process
and to the kernel's own writes. `statfs` (Phase 6.3) reports free space; it
does not limit it.

### G11 — A partial multi-block write leaks blocks

`ext2::file::write_file` returns early when `ensure_data_block` fails partway
through (`fs/src/ext2/file.rs:70-101`), skipping the `inode.size` update while
the blocks allocated on earlier iterations stay allocated and `inode.blocks`
stays incremented. ENOSPC mid-write therefore leaks blocks and reports no short
count. Same shape as G6, different call path.

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
report free space. The mount table stores a `flags: u32` that no reader
consults (`fs/src/vfs/mount.rs:13`, `:48`) — a read-only mount cannot be
expressed. `MAX_MOUNTS` is 16.

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
`AGENTS.md`, with the one exception 0.5 names. Phases 0–3 and 5 are a
dependency chain; 4 and 6 are parallelisable once 3 lands.

### Phase 0 — A test harness that can observe persistence

The evidence later phases are graded against. Two things block it that the gap
list does not, and both must land first.

- [ ] **0.1** Make the harness exit path flush. `qemu_signal_exit` writes
  `isa-debug-exit` (port 0xF4) and only then calls `power::shutdown`
  (`ktesting/src/qemu_signal.rs:14-17`); the port write terminates the VM, so
  the second call is dead code and `flush_filesystems_for_shutdown`
  (`boot/src/shutdown.rs:31-37`) never runs under the harness. Until this is
  fixed no test can observe a durable write, and every image a test run touches
  is left with `s_state == EXT2_ERROR_FS` because `mark_clean` never runs —
  which would make 0.3's `e2fsck` fail for a reason unrelated to correctness.
  Sync and mark clean *before* the port write.
- [ ] **0.2** The durability API from Phase 2.1/2.2, pulled forward. A utest has
  no way to ask for a flush, so "write then assert it survived" is not
  expressible. Phase 0 and Phase 2 are one unit; 2.1 and 2.2 land here.
- [ ] **0.3** `just check-fs-image` — `e2fsck -fn` on an image a boot wrote to.
  e2fsprogs is already a hard build dependency, so this costs nothing new. An
  image SlopOS wrote that `e2fsck` rejects is a bug in SlopOS, and it is the
  only oracle for on-disk correctness we do not have to write ourselves.
  Advisory until 3.0 lands: the operations Phase 3 adds are exactly the ones
  with a partial-failure window.
- [ ] **0.4** `just test-persist` — a two-boot recipe. Boot the tests ISO, run a
  utest that writes a known payload and syncs, power down, boot the *same image*
  again without rebuilding, assert the payload reads back. The image is
  `fs/assets/ext2-tests.img` and `_iso-tests` rebuilds it every run
  (`justfile:15`, `:83-85`, `:122`), so the recipe must skip that dependency and
  drive `run_tests --no-build --iso … --fs-image …` for both boots.
- [ ] **0.5** Land 0.3 and 0.4 as recipes *not* wired into `just test` or CI
  until Phase 1 makes them pass. A red suite is not a commitable state
  (`AGENTS.md`, Pre-commit); the baseline failure belongs in the commit body.

The expected failure is specific: not a clean per-test `IoError` but a
group-descriptor or inode-table read failing during mount or during `exec` of
`/sbin/init` — a boot failure. The superblock survives, for the reason G2 gives.

### Phase 1 — Decide what verity means on a writable device

G2 is a design contradiction rather than a bug to patch. Four shapes, and the
decision must be made before anything durable is written.

**Option A — verity implies read-only.** A trailer present ⇒ the mount is
read-only; a writable mount refuses to attach a verified device. Smallest
change, keeps the integrity guarantee exactly as strong as it is now, and makes
the shipped read-only-root configuration honest. Costs: the persistent root
gets no integrity checking at all.

**Option B — maintain the hash array live.** Recompute a block's CRC on
writeback and update the trailer, with the array itself journalled so a crash
between the data write and the hash write does not leave the array describing
neither the old nor the new contents. Keeps verification on a writable device
and is what dm-verity deliberately does not do (dm-verity is read-only by
design; the writable analogue is dm-integrity). Costs: the trailer becomes
mutable metadata with its own crash-consistency problem, and the write path
grows a second durability ordering constraint.

**Option C — scope verity to an immutable set of blocks.** Attest only the
`/bin`+`/sbin` content installed at build time; writes elsewhere are unverified
and writes inside the set are refused. Marries verity to the seal (G4): the
same boundary that says "these bytes are attested" says "these bytes may not
change". Costs: it is a block *set*, not a byte range — file data, directory
blocks and inode-table blocks are scattered across groups — so it needs a
per-block attestation bitmap, a rule for mixed blocks (a 4 KiB inode-table
block holds 32 inodes, so freezing one freezes the inodes of mutable files
sharing it), and a build-time ext2 reader in `gen_verity.py`.

**Option D — persist the `written` bitset.** It is already a `KVec<u64>`; write
it into the trailer at `mark_clean` time and treat `EXT2_ERROR_FS` at mount as
"verity off for this boot". Blocks never written stay attested across reboots,
there is no live hash recomputation, and the crash case degrades to today's
behaviour rather than to a false failure. Closer to a day than B's week, and it
keeps integrity on the majority of the image — which A does not.

**Recommendation: A now, D as the first upgrade, C only if restated as a
bitmap.** A is the smallest change that makes the current guarantee true
instead of accidentally false. D buys most of the coverage back cheaply. C as
written is not coherent: `/bin` on ext2 is not a byte range but a scattered set
of blocks, and a 4 KiB inode-table block holds 32 inodes, so freezing one
freezes the inodes of mutable files sharing it — and the bitmaps are shared by
construction. C needs a per-block attestation bitmap and a rule for mixed
blocks, which is most of D plus a build-time ext2 reader.

- [ ] **1.1** Implement the decision. Under A the behaviour must be stated
  exactly, because it changes today's default boot (G1): a trailer-carrying
  device mounts read-only, `Ext2Fs::read_only` is set so `mark_dirty_on_disk`
  does not write the superblock at mount, the flusher is not started, and
  `/mnt` becomes a read-only mount rather than disappearing. A refusal instead
  of a read-only mount would drop `/mnt` from every shipped boot; do not do
  that without saying so.
- [ ] **1.2** Make the mount table's `flags` mean something: a `MOUNT_RDONLY`
  bit that `vfs_open_flags` consults, returning `EROFS`. Today the field is
  stored and never read, so "mounted read-only" is not a state the VFS can be
  in.
- [ ] **1.3** Give `scripts/build_fs_image.sh` a `VERITY=off` path, and use it
  for the image the persistence tests boot. The read-only shipped image keeps
  its trailer.
- [ ] **1.4** Keep one recipe booting a verified read-only image. With A plus
  1.3 plus Phase 5's disk root, no shipped configuration would exercise
  `fs/src/verity.rs` at all, and untested code is code on death row.
- [ ] **1.5** `just test-persist` (0.4) now gets past the second boot; wire it
  and `check-fs-image` into CI.

### Phase 2 — A durability contract userland can invoke

2.1 and 2.2 land in Phase 0, which cannot be built without them; the rest
follows once the write surface exists.

- [ ] **2.1** `fsync(2)` and `fdatasync(2)` as syscalls, from the free slot
  range, `cap(NoneFd)`. Both resolve the fd to its vnode and drive
  `FileSystem::sync`. Per-inode sync is the correct semantic and the eventual
  target; a first cut may sync the whole filesystem, provided the commit says
  so plainly rather than implying a per-file guarantee it does not give. The fd
  resolution must use `ProcessId`/`FdTable`, never a bare `u32` pid —
  `check_process_designator.sh` scans `fs/src/fileio/`.
- [ ] **2.2** `sync(2)`, `cap(NoneSelf)`. Walks the mount table calling
  `FileSystem::sync` on each.
- [ ] **2.2a** Bound the exposure. `CACHED_EXT2` is one global sleeping mutex
  held across all block I/O (`fs/src/ext2_vfs.rs:31`, `:62`), so these two
  syscalls make an unbounded, uncharged, system-wide filesystem stall directly
  userland-reachable: a loop of `fsync` blocks every other process's path walk
  and `exec`. Either rate-limit it, charge it, or state the exposure and the
  trigger for fixing it.
- [ ] **2.3** `O_SYNC` / `O_DSYNC` in `abi/src/fs.rs`, honoured in
  `vfs_file_ops::write` — the per-file "I do not trust the flusher" knob, and
  what `/etc/keymap` should be written with.
- [ ] **2.4** Wire the new numbers into `SYSCALL_TABLE` and re-record
  `CAP_COUNTS` in `core/src/syscall/handlers.rs`. That const histogram is a
  compile-time assert; it will fail until updated, which is the intended
  behaviour.
- [ ] **2.5** `slibc` bindings so `std::fs::File::sync_all` and `sync_data`
  stop being unimplemented.
- [ ] **2.6** Tests: a utest that writes, `fsync`s, and asserts the block
  device saw a flush; a stest that asserts `sync` leaves `dirty_count() == 0`.

### Phase 3 — Close the write surface

Everything here is implementing `FileSystem` methods on ext2 that the ext2
layer below either already supports or nearly does.

- [ ] **3.0** Allocation rollback first, not in Phase 4. Every operation below
  has several writes before its commit point, and 0.3's `e2fsck` grades each
  one. Landing them before the rollback guard means the oracle reports real
  inconsistencies whose fix is deferred to the next phase.

- [ ] **3.1** `truncate` — wire `Ext2Fs` to the existing `ext2::file::truncate`,
  including the free-block accounting and the superblock dirty flag. Unblocks
  `O_TRUNC`, and therefore overwriting any existing file.
- [ ] **3.2** `rename` — `dir::append_dir_entry` + `dir::remove_dir_entry` +
  `dir::update_dotdot` (all present, the last unused) plus the parent
  `links_count` adjustment when a directory moves. Same-filesystem only; the VFS
  already refuses cross-device. Rename-over-existing must be atomic in the sense
  ext2 gives it: the new entry is written before the old is removed, so a crash
  leaves two names for one inode rather than none.
- [ ] **3.3** `symlink` / `readlink` — call the complete-and-unused
  `ext2/symlink.rs`. Fast symlinks (≤ 60 bytes, stored in `i_block`) are already
  handled there.
- [ ] **3.4** `set_mode` — write `i_mode`'s permission bits through instead of
  returning `Ok(())`.
- [ ] **3.5** `set_sealed` + `stat().sealed` on ext2, using an inode flag.
  `EXT2_IMMUTABLE_FL` (0x10) is the carrier: it is what the bit means,
  `lsattr`/`chattr` show it, and `e2fsck` accepts it. One-way, as the trait
  requires. `Inode` parses and encodes `i_flags` at offset 32 already; nothing
  sets it.
- [ ] **3.5a** Stamp the flag at build time. The kernel half is useless alone:
  `build_fs_image.sh` writes each binary and sets `mode 0100755` via `debugfs
  set_inode_field` (`:59-60`) with no `flags` stamp, so after 3.5 every shipped
  binary still reports `sealed: false`. Add the `flags 0x10` stamp for `/bin`
  and `/sbin`. **3.5 + 3.5a together close G4**, and both must land before the
  disk becomes the default root.
- [ ] **3.6** `rmdir` — an ext2 directory removal using `dir::is_dir_empty`
  (present, unused), decrementing the parent's `links_count` and the group's
  `used_dirs_count`. A `rmdir(2)` syscall, or `unlink` on a directory
  dispatching to it, per what `slibc` expects.
- [ ] **3.7** Maintain `used_dirs_count` on create and remove; stamp `i_dtime`
  on unlink.
- [ ] **3.8** 64-bit offsets: `Ext2Fs::read_file`/`write_file` take `u64`, and
  `i_size_high` is read and written for regular files. Not a signature edit —
  it ripples through `ext2::file`, the block-mapping layer, `i_size_high`
  round-tripping, the `ext2_vfs` casts, and whatever in `fileio` passes offsets.
- [ ] **3.9** A wall clock, and `now_unix()` wired into the inode write paths
  (G8). Limine's `DateAtBootRequest` plus the monotonic offset gives a real
  `CLOCK_REALTIME`; without it 4.5 cannot be implemented and every persisted
  file stays stamped zero.
- [ ] **3.10** A `readdir` cursor and an ABI that can page (G9), so a directory
  with more than 64 entries is listable. A stable cookie, not a linear index.
- [ ] **3.11** Per-operation tests against a `MemoryBlockDevice` image in the
  existing `fs/src/tests.rs` style, plus at least one `e2fsck`-validated
  round-trip through `just check-fs-image`. Watch the 2 KiB stack gate: 3.1, 3.2
  and 3.6 each hold a parent inode, a child inode, a cache and an `Ext2Fs` live
  at once, and `rename` needs two resolved paths — `CanonPath` alone is 256
  bytes plus a 256-byte component array (`fs/src/vfs/canon.rs:15,39,46`). Expect
  this to shape the signatures, not just the tests.

### Phase 4 — Crash consistency

- [ ] **4.1** *(moved to 3.0)* Kill the `with_fs` TODO with an RAII guard that
  rolls back every allocation an operation made if it does not reach its commit
  point, and invalidate the cache entries a failed operation dirtied so the
  flusher cannot publish them. Covers `create_inode_entry` and the G11
  mid-write leak. The guard's `Drop` must be panic-free —
  `check_drop_panic_free.sh` scans `fs/`. Asterinas landed the same shape for
  the same defect (the *concept*, not their code).
- [ ] **4.2** Refuse to mount an image whose `s_state` says `EXT2_ERROR_FS`
  read-write. Options are read-only mount, or mount and repair. There is no
  in-kernel fsck and there should not be one yet: read-only plus a loud klog
  line is the honest behaviour, and `e2fsck` on the host is the repair tool.
- [ ] **4.3** `errors=remount-ro`: a device error or a metadata inconsistency
  during operation flips the mount read-only rather than continuing to write
  into a filesystem already known to be damaged.
- [ ] **4.4** Orphan inodes. `unlink` on an inode with open descriptors must
  detach the name and defer the free until the last close (POSIX), which needs a
  refcount the VFS does not currently keep. The on-disk half is ext2's own
  mechanism: thread the inode onto `s_last_orphan` via `i_dtime`, so a crash
  leaves a list the next `e2fsck` drains rather than leaked blocks. Do the
  in-memory refcount first; the on-disk list is only meaningful once the window
  it protects exists.
- [ ] **4.5** Superblock bookkeeping: `s_mnt_count`, `s_lastcheck`, `s_wtime`,
  `s_mtime` — the fields `e2fsck` reads and reports on. Blocked on the wall
  clock (3.9); three of the four are timestamps.
- [ ] **4.6** Crash-injection testing. A `FaultyBlockDevice` wrapper that fails
  or drops writes after *N* operations, in the `MemoryBlockDevice` style, driven
  from stests: perform an operation, cut the device at each write boundary,
  remount, assert the image is one of the two legal states. Bounded black-box
  crash testing after CrashMonkey/B³ (OSDI '18): a bounded search over short
  workloads finds real crash-consistency bugs at a scale a kernel test harness
  can host. `dm-flakey` is the mechanism-level model for `FaultyBlockDevice`.

### Phase 5 — Make the persistent root the real one

Ordered after Phase 3 because `root=virtio` without the seal (3.5) is a
privilege-escalation surface, and without `truncate` (3.1) is a root that
cannot overwrite a file.

- [ ] **5.0** Decide what a writable root does to the test harness.
  `scripts/qemu_run.sh:207-213` records why `disk1` exists: destructive tests
  target the scratch device, never the live root image, so a buggy test cannot
  corrupt an on-disk binary — the incident it names happened. A writable,
  persistent `/` makes every filesystem test a mutation of the image the next
  boot runs `/sbin/init` from, and CI becomes order-dependent (`AGENTS.md`
  already documents the `'*ext2_aaa*'` ordering coupling). Either regenerate the
  tests image per CI run, or give the persistence test a preserved copy of its
  own.
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
- [ ] **5.4** `just test-persist` (0.1) joins CI.
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
- **Syscall classification.** Every new syscall needs a `cap(...)` clause and
  a re-recorded `CAP_COUNTS` (a compile-time assert, so it fails loudly). A
  filesystem syscall that can reach a power primitive also moves
  `check_authority_reachability.sh`.
- **Test count.** New stests/utests move `check_test_count.sh`; measure the
  new baseline with `TEST_COUNT_BASELINE=0 scripts/check_test_count.sh`.
- **Gates beyond the obvious ones.** `check_drop_panic_free.sh` (3.0's rollback
  guard is a new `Drop` in `fs/`, which that gate scans and for which it already
  carries a named exception), `check_process_designator.sh` (2.1 adds entry
  points under `fs/src/fileio/`), `check_quota_headroom.sh` (5.5's new
  `ResourceKind`), `check_sched_spread.sh` (Phase 5 changes when the flusher
  kthread is placed).
- **Licensing.** ext2's on-disk layout, feature bits, `errno` values and
  `s_last_orphan` semantics are interface facts and free to use. Upstream
  *prose* — a Linux or Asterinas comment block — is not, and neither is
  anything from a GPL-2.0-only tree. Cite the specification or the documented
  behaviour, never an implementation file. The sharpest exposure is any
  build-time ext2 reader (Phase 1 Option C, `gen_verity.py`): superblock-parsing
  code is exactly what gets pasted from a GPL-2.0-only tree. Layout constants
  only.

## 5. Effort and sequencing

Phase 0 folds in Phase 2.1/2.2 and the harness fix — three days, not one.
Phase 1 under Option A is a day; D is another day; B or C is a week. Phase 3 is
the bulk, and 3.8 (`u32` → `u64`) is an ext2-wide API change rather than the
one-line edit it reads as. Phase 4 is a week, of which 4.4 is most: deferred
inode free needs a VFS-level open count the tree does not have, so it touches
fd lifetime rather than only ext2. Phase 5 is a week once 3 is green, not two
days — the disk root by default moves four ratchets (`check_test_count.sh`,
`check_lockdep_headroom.sh`, `check_sched_spread.sh`,
`check_authority_reachability.sh`), each needing a re-measure and a commit
message explaining the delta. Phase 6 is unbounded and last.

The minimum that delivers "a file survives a reboot": 0 → 1 → 3.0 → 3.1 → 3.5
→ 3.5a → 5. Everything else is what makes it trustworthy.
