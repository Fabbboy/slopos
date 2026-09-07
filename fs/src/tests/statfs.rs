//! `statfs(2)`: what each filesystem reports about its own capacity.

use slopos_abi::fs::{EXT2_SUPER_MAGIC, RAMFS_MAGIC};
use slopos_ostd::KVec;
use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_testing::TestResult;

use crate::ext2::Ext2Fs;
use crate::ext2::cache::BlockCache;
use crate::ext2_vfs::ext2_stats_of;
use crate::ramfs::RamFs;
use crate::vfs::traits::same_filesystem;
use crate::vfs::{
    MOUNT_RDONLY, VfsError, mount, mount_at, unmount, vfs_open, vfs_statfs, vfs_unlink,
};

use super::{build_minimal_ext2_image, ensure_vfs_ready};

const IMAGE_BLOCKS: u32 = 64;
const IMAGE_INODES: u32 = 32;

/// A reserve no fixture sets, so the subtraction is observable.
const PROBE_RESERVE: u32 = 5;

/// The counts come off the image's own superblock, and `blocks_available`
/// leaves the reserve the allocator enforces out of the answer.
pub fn test_statfs_ext2_matches_the_image_superblock() -> TestResult {
    let Some(device) = build_minimal_ext2_image(IMAGE_BLOCKS, IMAGE_INODES) else {
        return TestResult::Skipped;
    };
    let (superblock, block_size, _inode_size) = match Ext2Fs::mount_params(&device) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("the fixture did not mount: {:?}", e),
    };

    let stats = ext2_stats_of(&superblock, block_size, PROBE_RESERVE, false);
    if stats.magic != EXT2_SUPER_MAGIC {
        return slopos_testing::fail!("magic {:#x}, want ext2's", stats.magic);
    }
    if stats.block_size != block_size {
        return slopos_testing::fail!(
            "block size {}, want the image's {}",
            stats.block_size,
            block_size
        );
    }
    if stats.blocks != u64::from(IMAGE_BLOCKS) || stats.inodes != u64::from(IMAGE_INODES) {
        return slopos_testing::fail!(
            "{} blocks / {} inodes, want {} / {}",
            stats.blocks,
            stats.inodes,
            IMAGE_BLOCKS,
            IMAGE_INODES
        );
    }
    if stats.blocks_free == 0 || stats.blocks_free >= stats.blocks {
        return slopos_testing::fail!(
            "{} free of {} blocks is not a plausible count",
            stats.blocks_free,
            stats.blocks
        );
    }
    if stats.inodes_free == 0 || stats.inodes_free >= stats.inodes {
        return slopos_testing::fail!(
            "{} free of {} inodes is not a plausible count",
            stats.inodes_free,
            stats.inodes
        );
    }
    if stats.blocks_available != stats.blocks_free - u64::from(PROBE_RESERVE) {
        return slopos_testing::fail!(
            "available {} with {} free and a reserve of {}: the reserve was not subtracted",
            stats.blocks_available,
            stats.blocks_free,
            PROBE_RESERVE
        );
    }
    if stats.max_name_len != crate::MAX_NAME_LEN as u32 {
        return slopos_testing::fail!(
            "name limit {}, want the {} the VFS enforces",
            stats.max_name_len,
            crate::MAX_NAME_LEN
        );
    }
    TestResult::Pass
}

/// Its own frame: the payload must not share one with the mount temporaries.
#[inline(never)]
fn block_payload(len: usize) -> Option<KVec<u8>> {
    let mut buf = KVec::new();
    buf.resize(len, b'S').ok()?;
    Some(buf)
}

/// A `statfs` that never moves is indistinguishable from a constant: a create
/// and a one-block write must show in both counts.
pub fn test_statfs_ext2_free_counts_follow_a_write() -> TestResult {
    let Some(device) = build_minimal_ext2_image(IMAGE_BLOCKS, IMAGE_INODES) else {
        return TestResult::Skipped;
    };
    let (superblock, block_size, inode_size) = match Ext2Fs::mount_params(&device) {
        Ok(v) => v,
        Err(e) => return slopos_testing::fail!("the fixture did not mount: {:?}", e),
    };
    let Ok(mut cache) = BlockCache::new_boxed(block_size) else {
        return TestResult::Skipped;
    };
    let Ok(mut fs) = Ext2Fs::new(&device, &mut cache, superblock, block_size, inode_size) else {
        return slopos_testing::fail!("the fixture did not mount");
    };
    let Some(payload) = block_payload(block_size as usize) else {
        return TestResult::Skipped;
    };

    let before = ext2_stats_of(&fs.superblock(), block_size, 0, false);
    let Ok(ino) = fs.create_file(2, b"statfs.bin") else {
        return slopos_testing::fail!("create failed on a healthy fixture");
    };
    if let Err(e) = fs.write_file(ino, 0, &payload) {
        return slopos_testing::fail!("write failed on a healthy fixture: {:?}", e);
    }
    let after = ext2_stats_of(&fs.superblock(), block_size, 0, false);

    if after.blocks_free >= before.blocks_free {
        return slopos_testing::fail!(
            "{} free blocks before a {}-byte write, {} after",
            before.blocks_free,
            payload.len(),
            after.blocks_free
        );
    }
    if after.inodes_free >= before.inodes_free {
        return slopos_testing::fail!(
            "{} free inodes before a create, {} after",
            before.inodes_free,
            after.inodes_free
        );
    }
    if after.blocks_available > after.blocks_free {
        return slopos_testing::fail!("available exceeds free");
    }
    TestResult::Pass
}

/// ramfs answers real inode totals and refuses to invent a block count: its
/// capacity is the kernel heap's, which is not this filesystem's to claim.
pub fn test_statfs_ramfs_reports_inode_totals() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let before = match vfs_statfs(b"/tmp") {
        Ok((stats, _flags)) => stats,
        Err(e) => return slopos_testing::fail!("statfs(/tmp) failed: {:?}", e),
    };
    if before.magic != RAMFS_MAGIC {
        return slopos_testing::fail!("magic {:#x}, want ramfs's", before.magic);
    }
    if before.inodes == 0 || before.inodes_free == 0 || before.inodes_free > before.inodes {
        return slopos_testing::fail!(
            "{} free of {} inodes is not a plausible count",
            before.inodes_free,
            before.inodes
        );
    }
    if before.blocks != 0 || before.blocks_free != 0 || before.blocks_available != 0 {
        return slopos_testing::fail!("a heap-backed filesystem claimed a block count");
    }
    if before.block_size == 0 {
        return slopos_testing::fail!("a zero block size makes every byte count a division by 0");
    }
    if before.max_name_len != crate::MAX_NAME_LEN as u32 {
        return slopos_testing::fail!(
            "name limit {}, want {}",
            before.max_name_len,
            crate::MAX_NAME_LEN
        );
    }

    if vfs_open(b"/tmp/statfs_probe.txt", true).is_err() {
        return slopos_testing::fail!("could not create the probe file");
    }
    let after = match vfs_statfs(b"/tmp") {
        Ok((stats, _flags)) => stats,
        Err(e) => return slopos_testing::fail!("statfs(/tmp) failed after a create: {:?}", e),
    };
    let _ = vfs_unlink(b"/tmp/statfs_probe.txt");
    if after.inodes_free >= before.inodes_free {
        return slopos_testing::fail!(
            "{} free inodes before a create, {} after",
            before.inodes_free,
            after.inodes_free
        );
    }
    TestResult::Pass
}

/// devfs has no capacity, so it refuses rather than reporting zeros — a
/// struct of zeros is an answer, and it claims a full filesystem.
pub fn test_statfs_devfs_refuses() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    match vfs_statfs(b"/dev") {
        Err(VfsError::NotSupported) => TestResult::Pass,
        Ok(_) => slopos_testing::fail!("devfs answered a capacity it does not have"),
        Err(e) => slopos_testing::fail!("want NotSupported, got {:?}", e),
    }
}

/// Its own instance and lock class, because the walk to it holds the root
/// mount's lock while taking this one's.
static RDONLY_PROBE_FS: RamFs =
    RamFs::new_const(lock_class!("STATFS_RDONLY_PROBE", LOCK_LEVEL_RESOURCE));

/// A read-only mount must be visible in the flags `statfs` reports, or
/// userland discovers it only when a write fails.
pub fn test_statfs_read_only_mount_reports_the_flag() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    if let Err(e) = mount(b"/statfs_rdonly", &RDONLY_PROBE_FS, MOUNT_RDONLY) {
        return slopos_testing::fail!("could not mount the probe: {:?}", e);
    }
    let outcome = vfs_statfs(b"/statfs_rdonly");
    let _ = unmount(b"/statfs_rdonly");

    match outcome {
        Ok((stats, flags)) => {
            if flags & MOUNT_RDONLY == 0 {
                return slopos_testing::fail!("a read-only mount reported flags {:#x}", flags);
            }
            if stats.magic != RAMFS_MAGIC {
                return slopos_testing::fail!("the flags came from another mount");
            }
            TestResult::Pass
        }
        Err(e) => slopos_testing::fail!("statfs on the probe mount failed: {:?}", e),
    }
}

/// Stage the probe path off the caller's frame: `/tmp/` plus a name one byte
/// past the limit, with `len` of it filled.
#[inline(never)]
fn tmp_name_path(len: usize) -> Option<KVec<u8>> {
    let mut path = KVec::new();
    path.extend_from_slice(b"/tmp/").ok()?;
    for _ in 0..len {
        path.push(b'n').ok()?;
    }
    Some(path)
}

/// The reported limit must be the one creation enforces: exactly that many
/// bytes creatable, one more refused.
pub fn test_statfs_name_limit_is_the_one_creation_enforces() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let limit = match vfs_statfs(b"/tmp") {
        Ok((stats, _flags)) => stats.max_name_len as usize,
        Err(e) => return slopos_testing::fail!("statfs(/tmp) failed: {:?}", e),
    };
    let Some(at_limit) = tmp_name_path(limit) else {
        return TestResult::Skipped;
    };
    let Some(past_limit) = tmp_name_path(limit + 1) else {
        return TestResult::Skipped;
    };

    if let Err(e) = vfs_open(&at_limit, true) {
        return slopos_testing::fail!("a {}-byte name was refused: {:?}", limit, e);
    }
    let refused = vfs_open(&past_limit, true);
    let _ = vfs_unlink(&at_limit);

    match refused {
        Err(VfsError::NameTooLong) => TestResult::Pass,
        Ok(_) => {
            let _ = vfs_unlink(&past_limit);
            slopos_testing::fail!("a name past the reported limit of {} was created", limit)
        }
        Err(e) => slopos_testing::fail!("want NameTooLong past the limit, got {:?}", e),
    }
}

/// Every mounted filesystem must be a distinct instance under
/// [`same_filesystem`], which compares addresses only.
///
/// These three number their inodes independently — devfs's 1-6 collide with
/// ext2's reserved inodes — so an alias would make the open-vnode table, the
/// cross-device rename check and re-resolution take one filesystem's inode
/// for another's.
pub fn test_statfs_mounted_filesystems_are_distinct_instances() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let paths: [&[u8]; 3] = [b"/", b"/tmp", b"/dev"];
    let mut mounted: [Option<&'static dyn crate::vfs::FileSystem>; 3] = [None; 3];
    for (slot, path) in mounted.iter_mut().zip(paths.iter()) {
        match mount_at(path) {
            Some(m) => *slot = Some(m.fs),
            None => {
                return slopos_testing::fail!(
                    "{:?} is not a mount point",
                    core::str::from_utf8(path).unwrap_or("?")
                );
            }
        }
    }

    for (i, a) in mounted.iter().enumerate() {
        let Some(a) = a else { continue };
        // Reflexive, or "pairwise distinct" would also hold for a relation
        // that answered `false` for everything.
        if !same_filesystem(*a, *a) {
            return slopos_testing::fail!(
                "{:?} is not the same filesystem as itself",
                core::str::from_utf8(paths[i]).unwrap_or("?")
            );
        }
        for (j, b) in mounted.iter().enumerate().skip(i + 1) {
            let Some(b) = b else { continue };
            if same_filesystem(*a, *b) {
                return slopos_testing::fail!(
                    "{:?} and {:?} are the same instance",
                    core::str::from_utf8(paths[i]).unwrap_or("?"),
                    core::str::from_utf8(paths[j]).unwrap_or("?")
                );
            }
        }
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_statfs_ext2_matches_the_image_superblock);
slopos_testing::stest!(name = test_statfs_ext2_free_counts_follow_a_write);
slopos_testing::stest!(name = test_statfs_ramfs_reports_inode_totals);
slopos_testing::stest!(name = test_statfs_devfs_refuses);
slopos_testing::stest!(name = test_statfs_read_only_mount_reports_the_flag);
slopos_testing::stest!(name = test_statfs_name_limit_is_the_one_creation_enforces);
slopos_testing::stest!(name = test_statfs_mounted_filesystems_are_distinct_instances);
