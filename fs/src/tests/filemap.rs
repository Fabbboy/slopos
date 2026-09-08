//! File-backed `mmap` page sets: population, both directions of the coherence
//! rule, the writeback that reaches the device, and the two caps.
//!
//! The fixture is a private ext2 mount over a `MemoryBlockDevice`, not the
//! production singleton, which is already mounted on the root and refuses a
//! second init. A device this module owns is also the only way to ask whether
//! a writeback left the filesystem: a fresh `BlockCache` over the same image
//! answers "is it on the device", which a re-read through the mount's own
//! cache cannot.

use slopos_ostd::process::AccountId;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, Mutex};
use slopos_ostd::{KBox, KVec, lock_class};
use slopos_testing::TestResult;

use super::ScratchProcess;
use crate::blockdev::MemoryBlockDevice;
use crate::ext2::cache::BlockCache;
use crate::ext2::{Ext2Error, Ext2Fs, Ext2Superblock};
use crate::filemap::{self, FileMapError, MAX_INODES_PER_ACCOUNT};
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};

/// 1 KiB blocks, so this is 512 KiB — room for the seventeen 4 KiB files the
/// inode-cap test needs.
const IMAGE_BLOCKS: u32 = 512;

/// **Exactly what the fixture's inode table holds**: four 1 KiB blocks of
/// 128-byte records. A larger `s_inodes_count` leaves the bitmap willing to
/// allocate inode 33, whose record lands in the root directory block and
/// destroys the directory.
const IMAGE_INODES: u32 = 32;

const PAGE: usize = 4096;

/// One page set per registry slot, which is what the inode cap is.
const CAP_FIXTURES: usize = 16;

struct TestMount {
    device: MemoryBlockDevice,
    cache: KBox<BlockCache>,
    /// Boxed so a per-call copy is the only 1 KiB superblock on any frame.
    superblock: KBox<Ext2Superblock>,
    superblock_dirty: bool,
    block_size: u32,
    inode_size: u16,
}

static TEST_MOUNT: Mutex<Option<TestMount>> =
    Mutex::new(None, lock_class!("FILEMAP_TEST_MOUNT", LOCK_LEVEL_RESOURCE));

struct TestFs;

static TEST_FS: TestFs = TestFs;

fn test_fs() -> &'static dyn FileSystem {
    &TEST_FS
}

/// One operation over the fixture mount, publishing the superblock the way the
/// production mount does — a create allocates, and the next call must see it.
#[inline(never)]
fn with_ext2<R>(f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R> {
    let mut guard = TEST_MOUNT.lock().map_err(|_| VfsError::Interrupted)?;
    let mount = guard.as_mut().ok_or(VfsError::IoError)?;
    let (block_size, inode_size, dirty) =
        (mount.block_size, mount.inode_size, mount.superblock_dirty);
    let mut fs = Ext2Fs::new(
        &mount.device,
        &mut mount.cache,
        *mount.superblock,
        block_size,
        inode_size,
    )
    .map_err(|_| VfsError::IoError)?;
    fs.set_superblock_dirty(dirty);
    let result = f(&mut fs).map_err(report_ext2_error);
    *mount.superblock = fs.superblock();
    mount.superblock_dirty = fs.superblock_dirty();
    result
}

/// The fixture collapses every ext2 error into one `VfsError`, so the real
/// variant is logged here: a failing fixture step names its cause.
#[cold]
#[inline(never)]
fn report_ext2_error(e: Ext2Error) -> VfsError {
    slopos_ostd::klog_info!("FILEMAP_FIXTURE: ext2 refused the operation: {:?}", e);
    VfsError::IoError
}

impl FileSystem for TestFs {
    fn name(&self) -> &'static str {
        "ext2-filemap-fixture"
    }

    fn root_inode(&self) -> InodeId {
        2
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        with_ext2(|fs| {
            let mut found: Option<u32> = None;
            fs.for_each_dir_entry(parent as u32, |entry| {
                if entry.name == name {
                    found = Some(entry.inode.raw());
                    false
                } else {
                    true
                }
            })?;
            found.map(|i| i as InodeId).ok_or(Ext2Error::PathNotFound)
        })
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        with_ext2(|fs| {
            let ext2_inode = fs.read_inode(inode as u32)?;
            if ext2_inode.is_directory() {
                return Ok(FileStat::new_directory(inode));
            }
            let mut stat = FileStat::new_file(inode, ext2_inode.size as u64);
            // The seal has to reach the page set: it is what refuses a
            // writable set.
            stat.sealed = ext2_inode.is_immutable();
            Ok(stat)
        })
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        with_ext2(|fs| fs.read_file(inode as u32, offset, buf))
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        with_ext2(|fs| fs.write_file(inode as u32, offset, buf))
    }

    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId> {
        with_ext2(|fs| match file_type {
            FileType::Regular => fs.create_file(parent as u32, name).map(|i| i as InodeId),
            _ => Err(Ext2Error::InvalidInode),
        })
    }

    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        with_ext2(|fs| fs.unlink_entry(parent as u32, name))
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        let mut count = 0usize;
        with_ext2(|fs| {
            fs.for_each_dir_entry_from(inode as u32, offset as u64, |_, entry| {
                count += 1;
                callback(entry.name, entry.inode.raw() as InodeId, FileType::Regular)
            })
        })?;
        Ok(count)
    }

    fn truncate(&self, inode: InodeId, size: u64) -> VfsResult<()> {
        with_ext2(|fs| fs.truncate_file(inode as u32, size))
    }

    fn set_sealed(&self, inode: InodeId) -> VfsResult<()> {
        with_ext2(|fs| fs.set_sealed(inode as u32))
    }

    fn sync(&self) -> VfsResult<()> {
        with_ext2(|fs| fs.sync())
    }
}

/// Build the image and publish the fixture mount, once.
#[inline(never)]
fn ensure_mount() -> bool {
    if TEST_MOUNT.lock().is_ok_and(|g| g.is_some()) {
        return true;
    }
    let Some(device) = super::build_minimal_ext2_image(IMAGE_BLOCKS, IMAGE_INODES) else {
        return false;
    };
    install_mount(device)
}

#[inline(never)]
fn install_mount(device: MemoryBlockDevice) -> bool {
    let Ok((superblock, block_size, inode_size)) = Ext2Fs::mount_params(&device) else {
        return false;
    };
    let Ok(boxed) = KBox::try_new(superblock) else {
        return false;
    };
    let Ok(cache) = BlockCache::new_boxed(block_size) else {
        return false;
    };
    let Ok(mut guard) = TEST_MOUNT.lock() else {
        return false;
    };
    *guard = Some(TestMount {
        device,
        cache,
        superblock: boxed,
        superblock_dirty: false,
        block_size,
        inode_size,
    });
    true
}

/// `len` bytes of a per-file pattern, so a page taken from the wrong offset or
/// the wrong file is visible in the assertion.
#[inline(never)]
fn pattern(len: usize, seed: u8) -> Option<KVec<u8>> {
    let mut buf = KVec::<u8>::zeroed(len).ok()?;
    for (i, b) in buf.as_mut_slice().iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(seed);
    }
    Some(buf)
}

/// Create `name` under the fixture root and fill it with `len` pattern bytes.
#[inline(never)]
fn seed_file(name: &[u8], len: usize, seed: u8) -> Result<InodeId, &'static str> {
    let fs = test_fs();
    let _ = fs.unlink(2, name);
    let inode = fs
        .create(2, name, FileType::Regular)
        .map_err(|_| "create refused")?;
    let data = pattern(len, seed).ok_or("pattern alloc failed")?;
    let mut done = 0usize;
    while done < len {
        match fs.write(inode, done as u64, &data.as_slice()[done..]) {
            Ok(0) => return Err("write took no bytes"),
            Err(_) => return Err("write refused"),
            Ok(n) => done += n,
        }
    }
    Ok(inode)
}

/// Give the fixture's inode and blocks back: the image holds exactly 22
/// allocatable inodes, so a leftover file makes a later test's seeding fail.
fn drop_file(name: &[u8]) {
    let _ = test_fs().unlink(2, name);
}

/// Read straight from the filesystem, bypassing the page set.
#[inline(never)]
fn fs_read(inode: InodeId, offset: u64, len: usize) -> Option<KVec<u8>> {
    let mut buf = KVec::<u8>::zeroed(len).ok()?;
    let mut done = 0usize;
    while done < len {
        match test_fs().read(inode, offset + done as u64, &mut buf.as_mut_slice()[done..]) {
            Ok(0) | Err(_) => return None,
            Ok(n) => done += n,
        }
    }
    Some(buf)
}

/// Read through the `read(2)` path's own chunk step, which is what consults
/// the page set.
#[inline(never)]
fn read_path(inode: InodeId, offset: u64, len: usize) -> Option<KVec<u8>> {
    let mut buf = KVec::<u8>::zeroed(len).ok()?;
    let mut done = 0usize;
    while done < len {
        match crate::vfs_file_ops::read_chunk(
            test_fs(),
            inode,
            offset + done as u64,
            &mut buf.as_mut_slice()[done..],
        ) {
            Ok(0) | Err(_) => return None,
            Ok(n) => done += n,
        }
    }
    Some(buf)
}

/// Read the image through a *fresh* block cache: whatever this sees is on the
/// device, not merely in the mount's cache.
#[inline(never)]
fn device_read(inode: InodeId, offset: u64, len: usize) -> Option<KVec<u8>> {
    let mut buf = KVec::<u8>::zeroed(len).ok()?;
    let guard = TEST_MOUNT.lock().ok()?;
    let mount = guard.as_ref()?;
    read_fresh(
        &mount.device,
        &mount.superblock,
        mount.block_size,
        mount.inode_size,
        inode,
        offset,
        buf.as_mut_slice(),
    )
    .then_some(buf)
}

/// Its own frame: an `Ext2Fs` handle carries a 1 KiB superblock copy, and the
/// caller's frame already holds the mount guard and the staging buffer.
#[inline(never)]
fn read_fresh(
    device: &MemoryBlockDevice,
    superblock: &Ext2Superblock,
    block_size: u32,
    inode_size: u16,
    inode: InodeId,
    offset: u64,
    buf: &mut [u8],
) -> bool {
    let Ok(mut cache) = BlockCache::new_boxed(block_size) else {
        return false;
    };
    let Ok(mut fs) = Ext2Fs::new(device, &mut cache, *superblock, block_size, inode_size) else {
        return false;
    };
    let mut done = 0usize;
    while done < buf.len() {
        match fs.read_file(inode as u32, offset + done as u64, &mut buf[done..]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => done += n,
        }
    }
    true
}

/// The page set holds the file's bytes after a populate.
pub fn test_filemap_populates_from_the_file() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"populate", 2 * PAGE, 7) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    let Some(expected) = pattern(2 * PAGE, 7) else {
        return slopos_testing::fail!("pattern alloc failed");
    };

    let (map, paddrs) = match filemap::acquire(test_fs(), inode, 0, 2, true, AccountId::NONE) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("acquire refused: {:?}", e),
    };
    if paddrs.len() != 2 {
        return slopos_testing::fail!("acquire handed back {} pages", paddrs.len());
    }

    let served = read_path(inode, 0, 2 * PAGE);
    filemap::release(map, 1);
    filemap::drain_pending();
    drop_file(b"populate");

    match served {
        Some(got) if got.as_slice() == expected.as_slice() => TestResult::Pass,
        Some(_) => slopos_testing::fail!("the mapped pages do not hold the file's bytes"),
        None => slopos_testing::fail!("reading through the page set failed"),
    }
}

/// A `write(2)` into a mapped range lands in the page set, and a read sees it
/// there while the filesystem still holds the old bytes.
pub fn test_filemap_write_through_is_read_back() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"writethru", PAGE, 11) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    let (map, _paddrs) = match filemap::acquire(test_fs(), inode, 0, 1, true, AccountId::NONE) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("acquire refused: {:?}", e),
    };

    const NEW: &[u8] = b"page-set-authority";
    let wrote = crate::vfs_file_ops::write_chunk(test_fs(), inode, 64, NEW);
    let through = read_path(inode, 64, NEW.len());
    let on_disk = fs_read(inode, 64, NEW.len());
    filemap::release(map, 1);
    filemap::drain_pending();
    drop_file(b"writethru");

    if wrote != Ok(NEW.len()) {
        return slopos_testing::fail!("the write path took {:?} bytes", wrote);
    }
    match through {
        Some(got) if got.as_slice() == NEW => {}
        _ => return slopos_testing::fail!("the read path did not see the stored bytes"),
    }
    match on_disk {
        Some(got) if got.as_slice() == NEW => {
            slopos_testing::fail!("the filesystem was written behind the page set's back")
        }
        Some(_) => TestResult::Pass,
        None => slopos_testing::fail!("reading the filesystem failed"),
    }
}

/// A mapped file's length is the filesystem's, not the page set's: the set
/// holds whole pages, so its last one has a zero-filled tail past EOF that
/// `read(2)` must never serve.
pub fn test_filemap_read_stops_at_eof() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    const LEN: usize = 40;
    let inode = match seed_file(b"shorteof", LEN, 13) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    let (map, _paddrs) = match filemap::acquire(test_fs(), inode, 0, 1, true, AccountId::NONE) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("acquire refused: {:?}", e),
    };

    let mut buf = match KVec::<u8>::zeroed(PAGE) {
        Ok(v) => v,
        Err(_) => return slopos_testing::fail!("staging alloc failed"),
    };
    let first = crate::vfs_file_ops::read_chunk(test_fs(), inode, 0, buf.as_mut_slice());
    let past = crate::vfs_file_ops::read_chunk(test_fs(), inode, LEN as u64, buf.as_mut_slice());
    filemap::release(map, 1);
    filemap::drain_pending();
    drop_file(b"shorteof");

    if first != Ok(LEN) {
        return slopos_testing::fail!(
            "a whole-page read of a {}-byte mapped file answered {:?}",
            LEN,
            first
        );
    }
    if past != Ok(0) {
        return slopos_testing::fail!("a read at EOF answered {:?} instead of 0", past);
    }
    TestResult::Pass
}

/// A removed name takes its page set out of lookup, so a reallocated inode
/// number cannot resolve to the previous file's pages. ext2 inode numbers
/// carry no generation, so the number is the only key there is.
pub fn test_filemap_forget_unkeys_the_inode() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"forgotten", PAGE, 17) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    let (map, _paddrs) = match filemap::acquire(test_fs(), inode, 0, 1, true, AccountId::NONE) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("acquire refused: {:?}", e),
    };
    const STORED: &[u8] = b"stored-before-the-unlink";
    let _ = crate::vfs_file_ops::write_chunk(test_fs(), inode, 0, STORED);
    // The last reference, so the set is queued rather than freed.
    filemap::release(map, 1);

    let sets_queued = filemap::mapped_inode_count();
    filemap::detach_inode(test_fs(), inode);
    let covered = filemap::covers_offset(test_fs(), inode, 0);
    let queued_after = filemap::pending_count();
    let sets_after = filemap::mapped_inode_count();
    // The flush half ran while the blocks were still the inode's.
    let persisted = fs_read(inode, 0, STORED.len());
    drop_file(b"forgotten");

    if covered {
        return slopos_testing::fail!("a forgotten inode still resolves to its page set");
    }
    if sets_after >= sets_queued {
        return slopos_testing::fail!(
            "forget did not release the unreferenced set ({} sets before, {} after)",
            sets_queued,
            sets_after
        );
    }
    if queued_after != 0 {
        return slopos_testing::fail!("a forgotten set was left owing writeback");
    }
    match persisted {
        Some(got) if got.as_slice() == STORED => TestResult::Pass,
        Some(_) => slopos_testing::fail!("the pre-unlink flush lost the stored bytes"),
        None => slopos_testing::fail!("reading the filesystem failed"),
    }
}

/// `flush` puts the pages back, and a sync makes the image itself carry them.
pub fn test_filemap_flush_reaches_the_device() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"flushed", PAGE, 23) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    let (map, _paddrs) = match filemap::acquire(test_fs(), inode, 0, 1, true, AccountId::NONE) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("acquire refused: {:?}", e),
    };

    const NEW: &[u8] = b"durable-through-the-page-set";
    let _ = crate::vfs_file_ops::write_chunk(test_fs(), inode, 128, NEW);
    let flushed = filemap::flush(map);
    let synced = test_fs().sync();
    let from_device = device_read(inode, 128, NEW.len());
    filemap::release(map, 1);
    filemap::drain_pending();
    drop_file(b"flushed");

    if let Err(e) = flushed {
        return slopos_testing::fail!("flush failed: {:?}", e);
    }
    if synced.is_err() {
        return slopos_testing::fail!("the fixture sync failed");
    }
    match from_device {
        Some(got) if got.as_slice() == NEW => TestResult::Pass,
        Some(_) => slopos_testing::fail!("the flushed bytes never reached the image"),
        None => slopos_testing::fail!("reading the image through a fresh cache failed"),
    }
}

/// The last release queues rather than writing back — it runs under a preempt
/// guard — and `drain_pending` is what completes it.
pub fn test_filemap_release_queues_the_writeback() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"queued", PAGE, 31) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    let (map, _paddrs) = match filemap::acquire(test_fs(), inode, 0, 1, true, AccountId::NONE) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("acquire refused: {:?}", e),
    };

    const NEW: &[u8] = b"queued-writeback";
    let _ = crate::vfs_file_ops::write_chunk(test_fs(), inode, 0, NEW);
    filemap::release(map, 1);

    let queued = filemap::pending_count();
    let sets_queued = filemap::mapped_inode_count();
    let before = fs_read(inode, 0, NEW.len());
    filemap::drain_pending();
    let sets_drained = filemap::mapped_inode_count();
    let after = fs_read(inode, 0, NEW.len());
    drop_file(b"queued");

    if queued == 0 {
        return slopos_testing::fail!("the last release wrote back inline instead of queueing");
    }
    match before {
        Some(got) if got.as_slice() == NEW => {
            return slopos_testing::fail!("the release performed the writeback itself");
        }
        None => return slopos_testing::fail!("reading the filesystem failed"),
        Some(_) => {}
    }
    match after {
        Some(got) if got.as_slice() == NEW => {}
        _ => return slopos_testing::fail!("drain_pending did not write the pages back"),
    }
    if filemap::pending_count() != 0 {
        return slopos_testing::fail!("drain_pending left work queued");
    }
    if sets_drained >= sets_queued {
        return slopos_testing::fail!(
            "drain_pending kept the page set ({} sets before, {} after)",
            sets_queued,
            sets_drained
        );
    }
    TestResult::Pass
}

/// A mapping reaching past the end of the file is refused, because nothing can
/// fault a hole in later.
pub fn test_filemap_refuses_past_eof() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"shortfile", 64, 3) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    let refusal = filemap::acquire(test_fs(), inode, 1, 1, true, AccountId::NONE);
    drop_file(b"shortfile");
    match refusal {
        Err(FileMapError::PastEof) => TestResult::Pass,
        Err(e) => slopos_testing::fail!("wrong refusal for a mapping past EOF: {:?}", e),
        Ok((map, _)) => {
            filemap::release(map, 1);
            filemap::drain_pending();
            slopos_testing::fail!("a mapping past EOF was accepted")
        }
    }
}

/// The page ceiling refuses, as `ENOMEM`.
pub fn test_filemap_page_cap_refuses() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"capped", PAGE, 5) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    let refusal = filemap::acquire(test_fs(), inode, 0, 4096, true, AccountId::NONE);
    drop_file(b"capped");
    match refusal {
        Err(FileMapError::TooManyPages) => {
            if FileMapError::TooManyPages.to_errno() != slopos_abi::Errno::ENOMEM {
                return slopos_testing::fail!("the page ceiling does not report ENOMEM");
            }
            TestResult::Pass
        }
        Err(e) => slopos_testing::fail!("wrong refusal past the page ceiling: {:?}", e),
        Ok((map, _)) => {
            filemap::release(map, 1);
            filemap::drain_pending();
            slopos_testing::fail!("a mapping past the page ceiling was accepted")
        }
    }
}

/// One inode past the registry's capacity is refused, as `ENOMEM`, and the
/// sets already held are untouched.
pub fn test_filemap_inode_cap_refuses() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    filemap::drain_pending();
    let held = match hold_every_slot() {
        Ok(h) => h,
        Err(msg) => return slopos_testing::fail!("{}", msg),
    };

    let overflow = match seed_file(b"cap-overflow", PAGE, 99) {
        Ok(inode) => {
            Ok(filemap::acquire(test_fs(), inode, 0, 1, true, AccountId::NONE).map(|(map, _)| map))
        }
        Err(why) => Err(why),
    };

    for map in held.iter() {
        filemap::release(*map, 1);
    }
    filemap::drain_pending();
    drop_cap_files();
    drop_file(b"cap-overflow");

    match overflow {
        Err(why) => slopos_testing::fail!("could not seed the overflow file: {}", why),
        Ok(Err(FileMapError::TooManyInodes)) => TestResult::Pass,
        Ok(Err(e)) => slopos_testing::fail!("wrong refusal past the inode cap: {:?}", e),
        Ok(Ok(map)) => {
            filemap::release(map, 1);
            filemap::drain_pending();
            slopos_testing::fail!("a seventeenth inode was accepted")
        }
    }
}

/// `capNN`, the name the cap fixtures are created under.
fn cap_name(slot: u8) -> [u8; 5] {
    [b'c', b'a', b'p', b'0' + slot / 10, b'0' + slot % 10]
}

/// Acquire a page set on as many distinct inodes as the registry holds. Its
/// own frame: the name buffer and the handle list both live here.
#[inline(never)]
fn hold_every_slot() -> Result<KVec<slopos_mm::vma_region::FileMapRef>, &'static str> {
    let mut held: KVec<slopos_mm::vma_region::FileMapRef> = KVec::new();
    let mut slot = 0u8;
    while held.len() < CAP_FIXTURES {
        let name = cap_name(slot);
        slot += 1;
        let inode = seed_file(&name, PAGE, slot)?;
        match filemap::acquire(test_fs(), inode, 0, 1, true, AccountId::NONE) {
            Ok((map, _)) => {
                if held.push(map).is_err() {
                    filemap::release(map, 1);
                    return Err("handle list alloc failed");
                }
            }
            Err(e) => {
                for map in held.iter() {
                    filemap::release(*map, 1);
                }
                filemap::drain_pending();
                drop_cap_files();
                return Err(match e {
                    FileMapError::TooManyInodes => "the registry refused before its cap",
                    _ => "a cap fixture acquire failed",
                });
            }
        }
    }
    Ok(held)
}

/// The image holds 22 allocatable inodes, so the cap fixtures cannot be left
/// behind: the next test to seed a file would be the one that failed.
fn drop_cap_files() {
    for slot in 0..CAP_FIXTURES as u8 {
        drop_file(&cap_name(slot));
    }
}

/// A principal may hold only its share of the registry, and the frames it
/// holds are charged to it. Without both halves one process cornered all
/// sixteen slots at no cost to its own budget.
pub fn test_filemap_share_bounds_one_principal() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    filemap::drain_pending();
    let Some(first) = ScratchProcess::new() else {
        return TestResult::Skipped;
    };
    let Some(second) = ScratchProcess::new() else {
        return TestResult::Skipped;
    };
    let verdict = share_probe(first.table().account(), second.table().account());
    filemap::drain_pending();
    drop_share_files();
    match verdict {
        Ok(()) => TestResult::Pass,
        Err(why) => slopos_testing::fail!("{}", why),
    }
}

/// A set outlives the principal that grew it whenever another process is
/// still mapping it. The holder is charged for it instead, so unreclaimable
/// frames cannot end up charged to a retired account.
pub fn test_filemap_orphaned_set_is_adopted_by_its_holder() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    filemap::drain_pending();
    let Some(grower) = ScratchProcess::new() else {
        return TestResult::Skipped;
    };
    let Some(heir) = ScratchProcess::new() else {
        return TestResult::Skipped;
    };
    let heir_account = heir.table().account();
    let verdict = adopt_probe(grower, heir_account);
    filemap::drain_pending();
    drop_file(b"adopted");
    match verdict {
        Ok(()) => {
            if pinned_pages(heir_account) != 0 {
                return slopos_testing::fail!("the adopted charge outlived the page set");
            }
            TestResult::Pass
        }
        Err(why) => slopos_testing::fail!("{}", why),
    }
}

#[inline(never)]
fn adopt_probe(grower: ScratchProcess, heir: AccountId) -> Result<(), &'static str> {
    let inode = seed_file(b"adopted", PAGE, 7)?;
    let grower_account = grower.table().account();
    let (map, _) = filemap::acquire(test_fs(), inode, 0, 1, true, grower_account)
        .map_err(|_| "the fixture acquire was refused")?;
    if pinned_pages(grower_account) == 0 {
        filemap::release(map, 1);
        return Err("the frames were not charged to the principal that grew the set");
    }
    // The set survives on this handle's reference; its owner does not.
    drop(grower);

    let adopted = filemap::retain(map, 1, false, heir) && pinned_pages(heir) == 1;
    filemap::release(map, 1);
    filemap::release(map, 1);
    if !adopted {
        return Err("the holder did not adopt a set whose owner had exited");
    }
    Ok(())
}

/// `shrNN`, the name the share fixtures are created under.
fn share_name(slot: u8) -> [u8; 5] {
    [b's', b'h', b'r', b'0' + slot / 10, b'0' + slot % 10]
}

fn drop_share_files() {
    for slot in 0..=(2 * MAX_INODES_PER_ACCOUNT as u8) {
        drop_file(&share_name(slot));
    }
}

fn pinned_pages(account: AccountId) -> u32 {
    slopos_ostd::process::quota::stats(account, slopos_abi::quota::ResourceKind::PinnedBytes)
        .map(|stats| stats.used)
        .unwrap_or(0)
}

/// A fresh file of `pages` pages, with its first `take` pages mapped on
/// `account`'s behalf.
#[inline(never)]
fn seed_and_acquire(
    slot: u8,
    pages: usize,
    take: u32,
    account: AccountId,
) -> Result<Result<(slopos_mm::vma_region::FileMapRef, InodeId), FileMapError>, &'static str> {
    let name = share_name(slot);
    let inode = seed_file(&name, pages * PAGE, slot + 1)?;
    Ok(filemap::acquire(test_fs(), inode, 0, take, true, account).map(|(map, _)| (map, inode)))
}

/// Take one principal to its share, then check what it may no longer do and
/// that the other principal still can. Its own frame: the handle list and the
/// name buffers live here.
#[inline(never)]
fn share_probe(one: AccountId, two: AccountId) -> Result<(), &'static str> {
    let share = MAX_INODES_PER_ACCOUNT;
    let baseline = pinned_pages(one);
    let mut held: KVec<slopos_mm::vma_region::FileMapRef> = KVec::new();
    let mut verdict: Result<(), &'static str> = Ok(());
    // Two pages long, mapped one page deep, so the other principal has a
    // growth to ask for further down.
    let mut shared: InodeId = 0;

    for slot in 0..share {
        let pages = if slot == 0 { 2 } else { 1 };
        match seed_and_acquire(slot as u8, pages, 1, one) {
            Ok(Ok((map, inode))) => {
                if slot == 0 {
                    shared = inode;
                }
                if held.push(map).is_err() {
                    filemap::release(map, 1);
                    verdict = Err("handle list alloc failed");
                    break;
                }
            }
            Ok(Err(_)) => {
                verdict = Err("a principal was refused inside its own share");
                break;
            }
            Err(why) => {
                verdict = Err(why);
                break;
            }
        }
    }

    if verdict.is_ok() && pinned_pages(one) != baseline.saturating_add(share as u32) {
        verdict = Err("the frames were not charged to the mapping principal");
    }

    // The other principal's own share, which the registry still has slots for.
    if verdict.is_ok() {
        for slot in 0..share {
            match seed_and_acquire(share as u8 + 1 + slot as u8, 1, 1, two) {
                Ok(Ok((map, _))) => {
                    if held.push(map).is_err() {
                        filemap::release(map, 1);
                        verdict = Err("handle list alloc failed");
                        break;
                    }
                }
                Ok(Err(_)) => {
                    verdict = Err("a second principal was denied a slot the registry still had");
                    break;
                }
                Err(why) => {
                    verdict = Err(why);
                    break;
                }
            }
        }
    }

    // Re-homing a set costs a slot exactly as claiming a fresh one does.
    if verdict.is_ok() {
        match filemap::acquire(test_fs(), shared, 0, 2, true, two) {
            Err(FileMapError::TooManyInodes) => {}
            Err(e) => {
                verdict = Err(match e {
                    FileMapError::TooManyPages => "the slot share answered the page refusal",
                    _ => "wrong refusal re-homing past a principal's slot share",
                });
            }
            Ok((map, _)) => {
                filemap::release(map, 1);
                verdict = Err("re-homing took a set past the slot share");
            }
        }
    }

    // One past its own share, on a slot the registry still has free.
    if verdict.is_ok() {
        match seed_and_acquire(share as u8, 1, 1, one) {
            Ok(Err(FileMapError::TooManyInodes)) => {}
            Ok(Err(e)) => {
                verdict = Err(match e {
                    FileMapError::TooManyPages => "the slot share answered the page refusal",
                    _ => "wrong refusal past a principal's slot share",
                });
            }
            Ok(Ok((map, _))) => {
                filemap::release(map, 1);
                verdict = Err("a principal took more than its share of the slots");
            }
            Err(why) => verdict = Err(why),
        }
    }

    for map in held.iter() {
        filemap::release(*map, 1);
    }
    filemap::drain_pending();
    if verdict.is_ok() && pinned_pages(one) != baseline {
        verdict = Err("the frame charge outlived the page set");
    }
    verdict
}

/// A `write(2)` crossing the trailing edge of a mapped range must not come
/// back short: the loop in `FileOps::write` continues past the boundary.
pub fn test_filemap_write_past_the_mapped_edge_is_whole() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"crossedge", 2 * PAGE, 41) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    // Page 0 only, so the second half of the write lands past the coverage.
    let (map, _paddrs) = match filemap::acquire(test_fs(), inode, 0, 1, true, AccountId::NONE) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("acquire refused: {:?}", e),
    };
    let Some(vnode) = crate::vfs_file_ops::vnode_handle_for_tests(test_fs(), inode) else {
        return slopos_testing::fail!("could not open a vnode over the fixture");
    };

    let payload = match pattern(PAGE, 91) {
        Some(p) => p,
        None => return slopos_testing::fail!("payload alloc failed"),
    };
    let wrote = {
        let source = slopos_abi::io::KernelIoBufRef::new(payload.as_slice());
        slopos_abi::file_ops::FileOps::write(
            &crate::vfs_file_ops::VFS_FILE_OPS,
            vnode,
            &source,
            PAGE as u64 / 2,
            0,
        )
    };
    let readback = read_path(inode, PAGE as u64 / 2, PAGE);
    crate::vfs_file_ops::drop_vnode_for_tests(vnode);
    filemap::release(map, 1);
    filemap::drain_pending();
    drop_file(b"crossedge");

    if wrote != PAGE as isize {
        return slopos_testing::fail!(
            "a {}-byte write across the mapped edge returned {}",
            PAGE,
            wrote
        );
    }
    match readback {
        Some(got) if got.as_slice() == payload.as_slice() => TestResult::Pass,
        Some(_) => slopos_testing::fail!("the bytes across the edge did not all land"),
        None => slopos_testing::fail!("reading the written range failed"),
    }
}

/// A chunk that *starts below* a mapped range must not run into it: the bytes
/// inside the coverage have to reach the page set, or no mapper can see them.
pub fn test_filemap_write_from_below_reaches_the_set() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"frombelow", 2 * PAGE, 43) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    // The *second* page only: the write below it must stop at the boundary.
    let (map, _paddrs) = match filemap::acquire(test_fs(), inode, 1, 1, true, AccountId::NONE) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("acquire refused: {:?}", e),
    };
    let Some(vnode) = crate::vfs_file_ops::vnode_handle_for_tests(test_fs(), inode) else {
        return slopos_testing::fail!("could not open a vnode over the fixture");
    };

    const SPAN: usize = 200;
    const BELOW: usize = 96;
    let offset = PAGE as u64 - BELOW as u64;
    let payload = match pattern(SPAN, 77) {
        Some(p) => p,
        None => return slopos_testing::fail!("payload alloc failed"),
    };
    let wrote = {
        let source = slopos_abi::io::KernelIoBufRef::new(payload.as_slice());
        slopos_abi::file_ops::FileOps::write(
            &crate::vfs_file_ops::VFS_FILE_OPS,
            vnode,
            &source,
            offset,
            0,
        )
    };
    // Straight from the set, so this fails if the bytes went to the filesystem.
    let mut seen = match KVec::<u8>::zeroed(SPAN - BELOW) {
        Ok(v) => v,
        Err(_) => return slopos_testing::fail!("staging alloc failed"),
    };
    let served = filemap::read_through(test_fs(), inode, PAGE as u64, seen.as_mut_slice());
    crate::vfs_file_ops::drop_vnode_for_tests(vnode);
    filemap::release(map, 1);
    filemap::drain_pending();
    drop_file(b"frombelow");

    if wrote != SPAN as isize {
        return slopos_testing::fail!("a {}-byte write returned {}", SPAN, wrote);
    }
    if served != Some(SPAN - BELOW) {
        return slopos_testing::fail!("the page set served {:?} of the covered tail", served);
    }
    if seen.as_slice() != &payload.as_slice()[BELOW..] {
        return slopos_testing::fail!("the covered part of the write bypassed the page set");
    }
    TestResult::Pass
}

/// `msync(MS_ASYNC)` on a mapping of a removed file must not queue: nothing
/// would pick the entry up, and the flusher's park predicate reads that count.
pub fn test_filemap_queue_flush_on_a_forgotten_set_is_refused() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"asyncgone", PAGE, 47) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    let (map, _paddrs) = match filemap::acquire(test_fs(), inode, 0, 1, true, AccountId::NONE) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("acquire refused: {:?}", e),
    };
    // The mapping stays, so the set survives being forgotten.
    filemap::forget_inode(test_fs(), inode);
    let queued = filemap::queue_flush(map);
    let pending = filemap::pending_count();
    // `flush` on the same handle is not an argument error either.
    let flushed = filemap::flush(map);
    filemap::release(map, 1);
    let after_release = filemap::pending_count();
    filemap::drain_pending();
    drop_file(b"asyncgone");

    if !queued {
        return slopos_testing::fail!("queue_flush reported a stale handle for a live set");
    }
    if pending != 0 {
        return slopos_testing::fail!(
            "a forgotten set took a queue entry nothing can complete ({} pending)",
            pending
        );
    }
    if flushed.is_err() {
        return slopos_testing::fail!("msync of a removed file's mapping failed: {:?}", flushed);
    }
    if after_release != 0 {
        return slopos_testing::fail!("releasing a forgotten set queued work ({})", after_release);
    }
    TestResult::Pass
}

/// A read-only shared mapping must not arm the writeback: rewriting every page
/// of an unmodified file stamps its timestamps and un-attests its blocks.
pub fn test_filemap_readonly_mapping_does_not_arm_writeback() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"readonlym", PAGE, 59) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    let (map, _paddrs) = match filemap::acquire(test_fs(), inode, 0, 1, false, AccountId::NONE) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("acquire refused: {:?}", e),
    };
    // One page-sized reference, as a read-only mapping takes.
    if !filemap::retain(map, 1, false, AccountId::NONE) {
        return slopos_testing::fail!("retain reported a stale handle");
    }
    filemap::release(map, 1);
    // Move the file *behind* the set, so a writeback is visible as a revert.
    const LATER: &[u8] = b"written-after-the-mapping";
    if test_fs().write(inode, 0, LATER).is_err() {
        return slopos_testing::fail!("writing behind the page set failed");
    }
    filemap::release(map, 1);
    filemap::drain_pending();
    let survivor = fs_read(inode, 0, LATER.len());
    drop_file(b"readonlym");

    match survivor {
        Some(got) if got.as_slice() == LATER => TestResult::Pass,
        Some(_) => {
            slopos_testing::fail!("a read-only mapping's release rewrote the file")
        }
        None => slopos_testing::fail!("reading the filesystem failed"),
    }
}

/// The registry self-heals: a set queued by a caller with nobody to drain it
/// must not hold its slot against the next mapping, or a boot whose root runs
/// no ext2 flusher answers `ENOMEM` to file `mmap` forever.
pub fn test_filemap_acquire_drains_a_queued_release() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let first = match seed_file(b"selfheal1", PAGE, 61) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    let (map, _paddrs) = match filemap::acquire(test_fs(), first, 0, 1, true, AccountId::NONE) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("acquire refused: {:?}", e),
    };
    // Exactly what a process exit leaves behind: queued, with no flusher.
    filemap::release(map, 1);
    if filemap::pending_count() == 0 {
        return slopos_testing::fail!("the release did not queue");
    }

    let second = match seed_file(b"selfheal2", PAGE, 63) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the second file: {}", why),
    };
    let acquired = filemap::acquire(test_fs(), second, 0, 1, true, AccountId::NONE);
    let pending_after = filemap::pending_count();
    if let Ok((map2, _)) = acquired {
        filemap::release(map2, 1);
    }
    filemap::drain_pending();
    drop_file(b"selfheal1");
    drop_file(b"selfheal2");

    if acquired.is_err() {
        return slopos_testing::fail!("the second acquire failed: {:?}", acquired.err());
    }
    if pending_after != 0 {
        return slopos_testing::fail!("acquire left {} queued release(s) behind it", pending_after);
    }
    TestResult::Pass
}

/// A sealed inode refuses a writable page set. `mmap(2)`'s descriptor gate is
/// the primary check; the set enforces it itself because it is what every
/// other reader's `read(2)` is routed through.
pub fn test_filemap_sealed_inode_refuses_a_writable_set() -> TestResult {
    if !ensure_mount() {
        return slopos_testing::fail!("could not build the fixture image");
    }
    let inode = match seed_file(b"sealedmap", PAGE, 67) {
        Ok(i) => i,
        Err(why) => return slopos_testing::fail!("could not seed the fixture file: {}", why),
    };
    if test_fs().set_sealed(inode).is_err() {
        return slopos_testing::fail!("sealing the fixture failed");
    }

    let writable = filemap::acquire(test_fs(), inode, 0, 1, true, AccountId::NONE);
    let readable = filemap::acquire(test_fs(), inode, 0, 1, false, AccountId::NONE);
    if let Ok((map, _)) = readable {
        filemap::release(map, 1);
    }
    if let Ok((map, _)) = writable {
        filemap::release(map, 1);
    }
    filemap::drain_pending();

    match writable {
        Err(FileMapError::WriteRefused) => {
            if FileMapError::WriteRefused.to_errno() != slopos_abi::Errno::EACCES {
                return slopos_testing::fail!("a refused writable mapping is not EACCES");
            }
        }
        Err(e) => return slopos_testing::fail!("wrong refusal for a sealed inode: {:?}", e),
        Ok(_) => return slopos_testing::fail!("a sealed inode accepted a writable page set"),
    }
    if readable.is_err() {
        return slopos_testing::fail!("a sealed inode refused a read-only page set");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_filemap_populates_from_the_file, suite = fs);
slopos_testing::stest!(name = test_filemap_write_through_is_read_back, suite = fs);
slopos_testing::stest!(name = test_filemap_flush_reaches_the_device, suite = fs);
slopos_testing::stest!(name = test_filemap_release_queues_the_writeback, suite = fs);
slopos_testing::stest!(name = test_filemap_refuses_past_eof, suite = fs);
slopos_testing::stest!(name = test_filemap_page_cap_refuses, suite = fs);
slopos_testing::stest!(name = test_filemap_inode_cap_refuses, suite = fs);
slopos_testing::stest!(name = test_filemap_read_stops_at_eof, suite = fs);
slopos_testing::stest!(name = test_filemap_forget_unkeys_the_inode, suite = fs);
slopos_testing::stest!(
    name = test_filemap_write_past_the_mapped_edge_is_whole,
    suite = fs
);
slopos_testing::stest!(
    name = test_filemap_write_from_below_reaches_the_set,
    suite = fs
);
slopos_testing::stest!(
    name = test_filemap_queue_flush_on_a_forgotten_set_is_refused,
    suite = fs
);
slopos_testing::stest!(
    name = test_filemap_readonly_mapping_does_not_arm_writeback,
    suite = fs
);
slopos_testing::stest!(
    name = test_filemap_acquire_drains_a_queued_release,
    suite = fs
);
slopos_testing::stest!(
    name = test_filemap_sealed_inode_refuses_a_writable_set,
    suite = fs
);
slopos_testing::stest!(name = test_filemap_share_bounds_one_principal, suite = fs);

slopos_testing::stest!(
    name = test_filemap_orphaned_set_is_adopted_by_its_holder,
    suite = fs
);
