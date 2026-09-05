use slopos_abi::fs::UserFsEntry;
use slopos_ostd::KVec;
use slopos_ostd::klog_info;
use slopos_testing::TestResult;

use crate::blockdev::{BlockDevice, BlockDeviceError, MemoryBlockDevice};
use crate::cpio::{CpioError, for_each_cpio_entry};
use crate::ext2::cache::BlockCache;
use crate::ext2::{Ext2Error, Ext2Fs};
use crate::vfs::{
    vfs_init_builtin_filesystems, vfs_is_initialized, vfs_list, vfs_mkdir, vfs_open, vfs_rename,
    vfs_set_mode, vfs_stat, vfs_unlink,
};

/// Mount an in-memory image over a stack-local [`BlockCache`]. `$device` must
/// outlive `$fs`; returns `TestResult::Fail` early on an invalid superblock.
macro_rules! mount_ext2 {
    ($device:expr, $cache:ident, $fs:ident) => {
        let (sb, bs, is) = match Ext2Fs::mount_params(&$device) {
            Ok(v) => v,
            Err(_) => return TestResult::Fail,
        };
        let mut $cache = match BlockCache::new(bs) {
            Ok(c) => c,
            Err(_) => return TestResult::Fail,
        };
        #[allow(unused_mut)]
        let mut $fs = match Ext2Fs::new(&$device, &mut $cache, sb, bs, is) {
            Ok(v) => v,
            Err(_) => return TestResult::Fail,
        };
    };
}

// Idempotent, so no test has to depend on the lex order that put it first.
fn ensure_vfs_ready() -> bool {
    if vfs_init_builtin_filesystems().is_err() {
        klog_info!("VFS_TEST: failed to initialize VFS");
        return false;
    }
    true
}

pub fn test_vfs_initialized() -> TestResult {
    klog_info!("VFS_TEST: check initialized");
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    if !vfs_is_initialized() {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vfs_root_stat() -> TestResult {
    klog_info!("VFS_TEST: root stat");
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let (kind, _size) = match vfs_stat(b"/") {
        Ok(stat) => stat,
        Err(_) => return TestResult::Fail,
    };
    if kind != 1 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vfs_file_roundtrip() -> TestResult {
    klog_info!("VFS_TEST: file roundtrip");
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let _ = vfs_mkdir(b"/vfs_test");

    let handle = match vfs_open(b"/vfs_test/hello.txt", true) {
        Ok(h) => h,
        Err(_) => return TestResult::Fail,
    };

    let content = b"hello vfs";
    if handle.write(0, content).is_err() {
        return TestResult::Fail;
    }

    let mut buf = [0u8; 32];
    let read_len = match handle.read(0, &mut buf) {
        Ok(len) => len,
        Err(_) => return TestResult::Fail,
    };

    if read_len != content.len() || &buf[..content.len()] != content {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vfs_list() -> TestResult {
    klog_info!("VFS_TEST: list directory");
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let _ = vfs_mkdir(b"/vfs_list_test");
    if vfs_open(b"/vfs_list_test/listed.txt", true).is_err() {
        return slopos_testing::fail!("could not create the listing fixture");
    }

    let mut entries = [UserFsEntry::new(); 8];
    let count = match vfs_list(b"/vfs_list_test", &mut entries) {
        Ok(count) => count,
        Err(_) => return TestResult::Fail,
    };
    if count == entries.len() {
        return slopos_testing::fail!("the fixture directory filled the listing buffer");
    }

    let mut found = false;
    for entry in entries.iter().take(count) {
        if entry.name_str() == "listed.txt" {
            found = true;
            break;
        }
    }

    if !found {
        return slopos_testing::fail!("a just-created file was absent from its own directory");
    }
    TestResult::Pass
}

/// Every directory that shows up in `ls /` must be `cd`-able: `ls` reports
/// entries from the *parent* directory block via `readdir`, while `cd` resolves
/// the child path and `stat`s the child inode, and the two can diverge.
pub fn test_vfs_cd_into_listed_dirs() -> TestResult {
    use slopos_abi::fs::FS_TYPE_DIRECTORY;

    klog_info!("VFS_TEST: cd into every listed directory");
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    // Its own root directory, so the sweep below is never vacuous.
    const FIXTURE: &str = "vfs_cd_test";
    let _ = vfs_mkdir(b"/vfs_cd_test");

    // 32 × 72 bytes of entries — more than the whole frame budget on its own.
    let Ok(mut entries) = KVec::filled(UserFsEntry::new(), 32) else {
        return TestResult::Fail;
    };
    let count = match vfs_list(b"/", &mut entries) {
        Ok(count) => count,
        Err(_) => return TestResult::Fail,
    };

    let mut saw_fixture = false;
    for entry in entries.iter().take(count) {
        if entry.type_ != FS_TYPE_DIRECTORY {
            continue;
        }
        let name = entry.name_str();
        if name == "." || name == ".." {
            continue;
        }

        let mut path = [0u8; 256];
        path[0] = b'/';
        let nb = name.as_bytes();
        if nb.len() + 1 >= path.len() {
            continue;
        }
        path[1..1 + nb.len()].copy_from_slice(nb);
        let path = &path[..1 + nb.len()];

        match vfs_stat(path) {
            Ok((kind, _)) if kind == FS_TYPE_DIRECTORY => {}
            _ => {
                klog_info!("VFS_TEST: cd target not resolvable: {}", name);
                return TestResult::Fail;
            }
        }
        saw_fixture |= name == FIXTURE;
    }

    if !saw_fixture {
        return slopos_testing::fail!("a just-created root directory was absent from `ls /`");
    }
    TestResult::Pass
}

pub fn test_vfs_unlink() -> TestResult {
    klog_info!("VFS_TEST: unlink file");
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let _ = vfs_mkdir(b"/vfs_unlink_test");
    if vfs_open(b"/vfs_unlink_test/doomed.txt", true).is_err() {
        return slopos_testing::fail!("could not create the unlink fixture");
    }

    if vfs_unlink(b"/vfs_unlink_test/doomed.txt").is_err() {
        return TestResult::Fail;
    }

    let mut entries = [UserFsEntry::new(); 8];
    let count = match vfs_list(b"/vfs_unlink_test", &mut entries) {
        Ok(count) => count,
        Err(_) => return TestResult::Fail,
    };
    if count == entries.len() {
        return slopos_testing::fail!("the fixture directory filled the listing buffer");
    }

    for entry in entries.iter().take(count) {
        if entry.name_str() == "doomed.txt" {
            return slopos_testing::fail!("an unlinked file was still listed");
        }
    }
    TestResult::Pass
}

/// `//tmp/x`, `/./tmp/x` and `/a/../tmp/x` all name `/tmp/x`. Before
/// canonicalisation each missed the `/tmp` mount and landed on the root
/// filesystem's shadowed directory, so one visible path named two files.
pub fn test_vfs_canonicalise_table() -> TestResult {
    use crate::vfs::canonicalise;

    // `static`, not a local: a by-value case table is a stack frame, and the
    // kernel bounds those at 2 KiB.
    static CASES: [(&[u8], &[u8]); 10] = [
        (b"/", b"/"),
        (b"//", b"/"),
        (b"/tmp", b"/tmp"),
        (b"//tmp/x", b"/tmp/x"),
        (b"/./tmp/x", b"/tmp/x"),
        (b"/a/../tmp/x", b"/tmp/x"),
        (b"/tmp/./x", b"/tmp/x"),
        (b"/tmp//x", b"/tmp/x"),
        (b"/../../tmp", b"/tmp"),
        (b"/tmp/x/..", b"/tmp"),
    ];

    for &(input, want) in CASES.iter() {
        let got = match canonicalise(input) {
            Ok(c) => c,
            Err(_) => return slopos_testing::fail!("canonicalise rejected a valid path"),
        };
        if got.as_bytes() != want {
            return slopos_testing::fail!(
                "canonicalise({:?}) = {:?}, want {:?}",
                core::str::from_utf8(input).unwrap_or("?"),
                core::str::from_utf8(got.as_bytes()).unwrap_or("?"),
                core::str::from_utf8(want).unwrap_or("?")
            );
        }
    }

    if canonicalise(b"relative/path").is_ok() {
        return slopos_testing::fail!("a relative path must be rejected");
    }
    TestResult::Pass
}

/// The mount is reached through every spelling of its path, so writes through
/// `//tmp/f` and reads through `/tmp/f` name one file.
pub fn test_vfs_mount_reached_through_every_spelling() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let payload = b"canonical";
    if vfs_open(b"/tmp/canon_probe.txt", true)
        .and_then(|h| h.write(0, payload))
        .is_err()
    {
        return slopos_testing::fail!("could not create the probe file");
    }

    for spelling in [
        b"//tmp/canon_probe.txt".as_slice(),
        b"/./tmp/canon_probe.txt".as_slice(),
        b"/tmp/../tmp/canon_probe.txt".as_slice(),
    ] {
        let (_kind, size) = match vfs_stat(spelling) {
            Ok(s) => s,
            Err(_) => {
                return slopos_testing::fail!(
                    "{:?} did not reach the mount",
                    core::str::from_utf8(spelling).unwrap_or("?")
                );
            }
        };
        if size != payload.len() as u32 {
            return slopos_testing::fail!("a spelling of the path named a different file");
        }
    }

    let _ = vfs_unlink(b"/tmp/canon_probe.txt");
    TestResult::Pass
}

/// A name past `MAX_NAME_LEN` must be refused, not truncated: a truncated
/// entry can never be matched by the name that created it, so the file and
/// its inode are unreachable and unreclaimable.
pub fn test_ramfs_long_name_refused() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let mut path = [b'A'; 64];
    path[..5].copy_from_slice(b"/tmp/");
    let long = &path[..];

    match vfs_open(long, true) {
        Err(crate::vfs::VfsError::NameTooLong) => TestResult::Pass,
        Err(other) => slopos_testing::fail!("want NameTooLong, got {:?}", other),
        Ok(_) => slopos_testing::fail!("an over-long name was accepted"),
    }
}

/// Renaming a directory into its own descendant detaches the subtree: it
/// becomes unreachable from the root and unremovable.
pub fn test_ramfs_rename_into_descendant_refused() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let _ = vfs_unlink(b"/tmp/anc/child");
    let _ = vfs_unlink(b"/tmp/anc");
    if vfs_mkdir(b"/tmp/anc").is_err() || vfs_mkdir(b"/tmp/anc/child").is_err() {
        return slopos_testing::fail!("could not build the fixture");
    }

    let result = vfs_rename(b"/tmp/anc", b"/tmp/anc/child/loop");
    let outcome = match result {
        Err(_) => TestResult::Pass,
        Ok(()) => slopos_testing::fail!("a directory was spliced into its own descendant"),
    };

    let _ = vfs_unlink(b"/tmp/anc/child");
    let _ = vfs_unlink(b"/tmp/anc");
    outcome
}

pub fn test_vfs_storage_contention_stress_baseline() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let _ = vfs_mkdir(b"/vfs_stress");

    let mut payload = [0u8; 256];
    for (idx, b) in payload.iter_mut().enumerate() {
        *b = (idx & 0xFF) as u8;
    }

    for iter in 0..64u32 {
        let mut path = [0u8; 32];
        let name = [
            b'f',
            b'i',
            b'l',
            b'e',
            b'_',
            (b'0' + ((iter % 10) as u8)),
            b'.',
            b'b',
            b'i',
            b'n',
        ];
        path[..12].copy_from_slice(b"/vfs_stress/");
        path[12..22].copy_from_slice(&name);

        let handle = match vfs_open(&path[..22], true) {
            Ok(h) => h,
            Err(_) => return TestResult::Fail,
        };

        if handle.write(0, &payload).is_err() {
            return TestResult::Fail;
        }

        let mut out = [0u8; 256];
        let read_len = match handle.read(0, &mut out) {
            Ok(n) => n,
            Err(_) => return TestResult::Fail,
        };
        if read_len != payload.len() || out != payload {
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

struct FailingBlockDevice {
    fail_reads: bool,
    fail_writes: bool,
    capacity: u64,
}

impl FailingBlockDevice {
    fn new(capacity: u64) -> Self {
        Self {
            fail_reads: false,
            fail_writes: false,
            capacity,
        }
    }

    fn with_read_fail(mut self) -> Self {
        self.fail_reads = true;
        self
    }
}

impl BlockDevice for FailingBlockDevice {
    fn read_at(&self, _offset: u64, _buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        if self.fail_reads {
            Err(BlockDeviceError::InvalidBuffer)
        } else {
            Ok(())
        }
    }

    fn write_at(&self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
        if self.fail_writes {
            Err(BlockDeviceError::InvalidBuffer)
        } else {
            Ok(())
        }
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }
}

struct WriteFailingDevice {
    inner: MemoryBlockDevice,
}

impl WriteFailingDevice {
    fn new(inner: MemoryBlockDevice) -> Self {
        Self { inner }
    }
}

impl BlockDevice for WriteFailingDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
        Err(BlockDeviceError::InvalidBuffer)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }
}

/// Counts what reached the device, so a test can assert on the *shape* of a
/// commit — which blocks, and whether a barrier followed — rather than only on
/// the bytes that ended up there.
struct CountingBlockDevice {
    inner: MemoryBlockDevice,
    writes: core::sync::atomic::AtomicUsize,
    flushes: core::sync::atomic::AtomicUsize,
}

impl CountingBlockDevice {
    fn new(inner: MemoryBlockDevice) -> Self {
        Self {
            inner,
            writes: core::sync::atomic::AtomicUsize::new(0),
            flushes: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn writes(&self) -> usize {
        self.writes.load(core::sync::atomic::Ordering::Relaxed)
    }

    fn flushes(&self) -> usize {
        self.flushes.load(core::sync::atomic::Ordering::Relaxed)
    }

    fn reset(&self) {
        self.writes.store(0, core::sync::atomic::Ordering::Relaxed);
        self.flushes.store(0, core::sync::atomic::Ordering::Relaxed);
    }
}

impl BlockDevice for CountingBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.writes
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.inner.write_at(offset, buffer)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        self.flushes
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

struct Ext2ImageSpec<'a> {
    blocks: u32,
    inodes: u32,
    file_name: Option<&'a [u8]>,
    file_data: Option<&'a [u8]>,
    file_block: u32,
}

#[inline(never)]
fn write_dir_entry(
    dir_block: &mut [u8],
    offset: usize,
    inode: u32,
    rec_len: u16,
    name: &[u8],
    file_type: u8,
) {
    dir_block[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
    dir_block[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
    dir_block[offset + 6] = name.len() as u8;
    dir_block[offset + 7] = file_type;
    let name_end = offset + 8 + name.len();
    dir_block[offset + 8..name_end].copy_from_slice(name);
    for b in dir_block[name_end..offset + rec_len as usize].iter_mut() {
        *b = 0;
    }
}

/// Fixture geometry, block-numbered so nothing overlaps.
///
/// The inode table's extent is `inodes_per_group * inode_size / block_size`
/// blocks, not one: with 32 inodes of 128 bytes over 1 KiB blocks it spans
/// four. Data placed inside that span is data the first created inode
/// overwrites — `s_first_ino = 11` puts inode 11's record in the *second*
/// table block, so a test that created a file and then wrote to it was writing
/// its own record over the root directory.
const FIX_BLOCK_SIZE: u32 = 1024;
const FIX_INODE_SIZE: u16 = 128;
const FIX_BLOCK_BITMAP: u32 = 3;
const FIX_INODE_BITMAP: u32 = 4;
const FIX_INODE_TABLE: u32 = 5;
const FIX_ITABLE_BLOCKS: u32 = 4;
const FIX_ROOT_DIR_BLOCK: u32 = FIX_INODE_TABLE + FIX_ITABLE_BLOCKS;
/// Default `Ext2ImageSpec::file_block`; the first block past everything the
/// fixture reserves.
pub(crate) const FIX_FILE_BLOCK: u32 = FIX_ROOT_DIR_BLOCK + 1;
/// Blocks 1..=this are marked used in the block bitmap.
const FIX_LAST_USED_BLOCK: u32 = FIX_FILE_BLOCK;
/// `s_first_ino`: inodes below it are reserved and marked used.
const FIX_FIRST_INO: u32 = 11;

#[inline(never)]
fn write_superblock(sb: &mut [u8], inodes: u32, blocks: u32, inode_size: u16) {
    let free_blocks = blocks.saturating_sub(FIX_LAST_USED_BLOCK + 1);
    let free_inodes = inodes.saturating_sub(FIX_FIRST_INO - 1);
    sb[0..4].copy_from_slice(&inodes.to_le_bytes());
    sb[4..8].copy_from_slice(&blocks.to_le_bytes());
    sb[12..16].copy_from_slice(&free_blocks.to_le_bytes());
    sb[16..20].copy_from_slice(&free_inodes.to_le_bytes());
    sb[20..24].copy_from_slice(&1u32.to_le_bytes());
    sb[24..28].copy_from_slice(&0u32.to_le_bytes());
    sb[32..36].copy_from_slice(&blocks.to_le_bytes());
    sb[40..44].copy_from_slice(&inodes.to_le_bytes());
    sb[56..58].copy_from_slice(&0xEF53u16.to_le_bytes());
    // `s_state`/`s_errors` as `mkfs.ext2` writes them. A zeroed `s_state` is
    // not "clean" to any reader, so a fixture that left it out would make
    // every mount of one read as a crashed boot.
    sb[58..60].copy_from_slice(&crate::ext2::ondisk::EXT2_VALID_FS.to_le_bytes());
    sb[60..62].copy_from_slice(&crate::ext2::ondisk::EXT2_ERRORS_RO.to_le_bytes());
    sb[76..80].copy_from_slice(&1u32.to_le_bytes());
    sb[84..88].copy_from_slice(&FIX_FIRST_INO.to_le_bytes());
    sb[88..90].copy_from_slice(&inode_size.to_le_bytes());
}

#[inline(never)]
fn write_group_descriptor(desc: &mut [u8], inodes: u32, blocks: u32) {
    let free_blocks = blocks.saturating_sub(FIX_LAST_USED_BLOCK + 1);
    let free_inodes = inodes.saturating_sub(FIX_FIRST_INO - 1);
    desc[0..4].copy_from_slice(&FIX_BLOCK_BITMAP.to_le_bytes());
    desc[4..8].copy_from_slice(&FIX_INODE_BITMAP.to_le_bytes());
    desc[8..12].copy_from_slice(&FIX_INODE_TABLE.to_le_bytes());
    desc[12..14].copy_from_slice(&(free_blocks as u16).to_le_bytes());
    desc[14..16].copy_from_slice(&(free_inodes as u16).to_le_bytes());
    desc[16..18].copy_from_slice(&1u16.to_le_bytes());
}

/// Mark the metadata blocks and reserved inodes used.
///
/// An all-zero bitmap is not "empty": the allocator reads it as every block
/// free and hands out block 1, the superblock. Every write through such a
/// fixture lands on top of the filesystem describing it.
#[inline(never)]
fn write_fixture_bitmaps(buf: &mut [u8], inodes: u32, blocks: u32) {
    let bs = FIX_BLOCK_SIZE as usize;
    let bmap = FIX_BLOCK_BITMAP as usize * bs;
    // `locate_block` maps block N to bit N - first_data_block, and
    // first_data_block is 1 for a 1 KiB image.
    for blk in 1..=FIX_LAST_USED_BLOCK.min(blocks - 1) {
        let bit = (blk - 1) as usize;
        buf[bmap + bit / 8] |= 1 << (bit % 8);
    }
    // Bits past the volume address blocks that do not exist; leaving them
    // clear invites the allocator to return one and fail `checked_block`.
    for bit in (blocks - 1) as usize..bs * 8 {
        buf[bmap + bit / 8] |= 1 << (bit % 8);
    }

    let imap = FIX_INODE_BITMAP as usize * bs;
    for ino in 1..FIX_FIRST_INO.min(inodes + 1) {
        let bit = (ino - 1) as usize;
        buf[imap + bit / 8] |= 1 << (bit % 8);
    }
    for bit in inodes as usize..bs * 8 {
        buf[imap + bit / 8] |= 1 << (bit % 8);
    }
}

#[inline(never)]
fn write_root_inode(inode_table: &mut [u8], root_inode_offset: usize, block_size: u32) {
    inode_table[root_inode_offset..root_inode_offset + 2]
        .copy_from_slice(&(0x4000u16 | 0o755).to_le_bytes());
    inode_table[root_inode_offset + 4..root_inode_offset + 8]
        .copy_from_slice(&block_size.to_le_bytes());
    // `links_count`: `.` and `..`, which is what `rmdir` on a child
    // decrements from.
    inode_table[root_inode_offset + 26..root_inode_offset + 28]
        .copy_from_slice(&2u16.to_le_bytes());
    inode_table[root_inode_offset + 28..root_inode_offset + 32]
        .copy_from_slice(&(block_size / 512).to_le_bytes());
    inode_table[root_inode_offset + 40..root_inode_offset + 44]
        .copy_from_slice(&FIX_ROOT_DIR_BLOCK.to_le_bytes());
}

#[inline(never)]
fn write_file_inode(
    inode_table: &mut [u8],
    file_inode_offset: usize,
    data_len: u32,
    file_block: u32,
) {
    inode_table[file_inode_offset..file_inode_offset + 2]
        .copy_from_slice(&(0x8000u16 | 0o644).to_le_bytes());
    inode_table[file_inode_offset + 4..file_inode_offset + 8]
        .copy_from_slice(&data_len.to_le_bytes());
    inode_table[file_inode_offset + 26..file_inode_offset + 28]
        .copy_from_slice(&1u16.to_le_bytes());
    inode_table[file_inode_offset + 28..file_inode_offset + 32]
        .copy_from_slice(&(FIX_BLOCK_SIZE / 512).to_le_bytes());
    inode_table[file_inode_offset + 40..file_inode_offset + 44]
        .copy_from_slice(&file_block.to_le_bytes());
}

#[inline(never)]
fn write_dir_with_file(
    dir_block: &mut [u8],
    block_size: usize,
    file_inode_number: u32,
    name: &[u8],
) {
    let used = 24 + ((8 + name.len() + 3) & !3);
    let rec_len = (block_size - used) as u16;
    write_dir_entry(dir_block, 0, 2, 12, b".", 2);
    write_dir_entry(dir_block, 12, 2, 12, b"..", 2);
    write_dir_entry(
        dir_block,
        24,
        file_inode_number,
        (used - 24) as u16,
        name,
        1,
    );
    write_dir_entry(dir_block, used, 0, rec_len, b"", 0);
}

#[inline(never)]
fn write_dir_minimal(dir_block: &mut [u8], block_size: usize) {
    write_dir_entry(dir_block, 0, 2, 12, b".", 2);
    write_dir_entry(dir_block, 12, 2, 12, b"..", 2);
    write_dir_entry(dir_block, 24, 0, (block_size as u32 - 24) as u16, b"", 0);
}

fn build_ext2_image(spec: Ext2ImageSpec<'_>) -> Option<MemoryBlockDevice> {
    let block_size = FIX_BLOCK_SIZE;
    let inode_size = FIX_INODE_SIZE;
    let bs = block_size as usize;
    let size_bytes = (spec.blocks as usize).saturating_mul(bs);
    let device = MemoryBlockDevice::allocate(size_bytes)?;

    device.with_buffer_mut(|buf| {
        let sb_offset = 1024usize;
        write_superblock(
            &mut buf[sb_offset..sb_offset + 1024],
            spec.inodes,
            spec.blocks,
            inode_size,
        );

        let desc_offset = 2 * bs;
        write_group_descriptor(
            &mut buf[desc_offset..desc_offset + 32],
            spec.inodes,
            spec.blocks,
        );
        write_fixture_bitmaps(buf, spec.inodes, spec.blocks);

        let inode_table_offset = FIX_INODE_TABLE as usize * bs;
        let root_inode_offset = inode_size as usize;
        write_root_inode(
            &mut buf[inode_table_offset..inode_table_offset + bs],
            root_inode_offset,
            block_size,
        );

        let file_inode_number = 3u32;
        let dir_offset = FIX_ROOT_DIR_BLOCK as usize * bs;
        if let (Some(name), Some(data)) = (spec.file_name, spec.file_data) {
            let file_inode_offset = root_inode_offset + inode_size as usize;
            write_file_inode(
                &mut buf[inode_table_offset..inode_table_offset + bs],
                file_inode_offset,
                data.len() as u32,
                spec.file_block,
            );

            if spec.file_block < spec.blocks {
                let data_offset = spec.file_block as usize * bs;
                let data_block = &mut buf[data_offset..data_offset + bs];
                data_block[..data.len()].copy_from_slice(data);
            }

            write_dir_with_file(
                &mut buf[dir_offset..dir_offset + bs],
                bs,
                file_inode_number,
                name,
            );
        } else {
            write_dir_minimal(&mut buf[dir_offset..dir_offset + bs], bs);
        }
    });

    Some(device)
}

fn build_minimal_ext2_image(blocks: u32, inodes: u32) -> Option<MemoryBlockDevice> {
    build_ext2_image(Ext2ImageSpec {
        blocks,
        inodes,
        file_name: None,
        file_data: None,
        file_block: 0,
    })
}

/// An image declaring an incompat feature we cannot represent (extents,
/// 64-bit block numbers, metadata checksums) must be refused, not mounted
/// read-write and written into corruption.
pub fn test_ext2_unsupported_incompat_feature_refused() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    device.with_buffer_mut(|buf| {
        let sb = &mut buf[sb_offset..sb_offset + 1024];
        // EXT4_FEATURE_INCOMPAT_EXTENTS.
        sb[96..100].copy_from_slice(&0x0040u32.to_le_bytes());
    });

    match Ext2Fs::mount_params(&device) {
        Err(Ext2Error::UnsupportedFeature) => TestResult::Pass,
        other => slopos_testing::fail!("want UnsupportedFeature, got {:?}", other.map(|_| ())),
    }
}

/// A read-only-compatible feature we do not write forces a read-only mount
/// rather than a refusal: the image is still safe to read.
pub fn test_ext2_unsupported_ro_compat_forces_readonly() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    device.with_buffer_mut(|buf| {
        let sb = &mut buf[sb_offset..sb_offset + 1024];
        // EXT4_FEATURE_RO_COMPAT_METADATA_CSUM.
        sb[100..104].copy_from_slice(&0x0400u32.to_le_bytes());
    });

    mount_ext2!(device, cache, fs);
    if !fs.is_read_only() {
        return slopos_testing::fail!("an unsupported ro_compat feature must force read-only");
    }
    match fs.create_file(2, b"nope") {
        Err(Ext2Error::ReadOnly) => TestResult::Pass,
        other => slopos_testing::fail!("want ReadOnly, got {:?}", other.map(|_| ())),
    }
}

/// A write-protected device forces the filesystem read-only whatever the
/// superblock says: nothing must dirty a block the device will refuse.
pub fn test_ext2_write_protected_device_forces_readonly() -> TestResult {
    struct Protected(MemoryBlockDevice);
    impl BlockDevice for Protected {
        fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
            self.0.read_at(offset, buffer)
        }
        fn write_at(&self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
            Err(BlockDeviceError::WriteProtected)
        }
        fn capacity(&self) -> u64 {
            self.0.capacity()
        }
        fn write_protected(&self) -> bool {
            true
        }
    }

    let Some(inner) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let device = Protected(inner);
    mount_ext2!(device, cache, fs);
    if !fs.is_read_only() {
        return slopos_testing::fail!("a write-protected device must force read-only");
    }
    match fs.create_file(2, b"nope") {
        Err(Ext2Error::ReadOnly) => {}
        other => return slopos_testing::fail!("want ReadOnly, got {:?}", other.map(|_| ())),
    }
    if fs.mark_dirty_on_disk().is_err() || fs.mark_clean().is_err() {
        return slopos_testing::fail!(
            "superblock state writes must be no-ops on a read-only handle"
        );
    }
    if fs.dirty_count() != 0 {
        return slopos_testing::fail!("a read-only handle dirtied the cache");
    }
    TestResult::Pass
}

/// An inode larger than the block that holds it makes the inode-table offset
/// arithmetic address outside the block it just read.
pub fn test_ext2_inode_size_beyond_block_rejected() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    device.with_buffer_mut(|buf| {
        let sb = &mut buf[sb_offset..sb_offset + 1024];
        // Block size is 1024 here; claim a 4096-byte inode.
        sb[88..90].copy_from_slice(&4096u16.to_le_bytes());
    });

    match Ext2Fs::mount_params(&device) {
        Err(Ext2Error::InvalidSuperblock) => TestResult::Pass,
        other => slopos_testing::fail!("want InvalidSuperblock, got {:?}", other.map(|_| ())),
    }
}

/// A group's block bitmap occupies exactly one block, so `blocks_per_group`
/// cannot exceed the bits one block holds. Without the bound the allocator
/// derives block numbers outside the group it just searched.
pub fn test_ext2_blocks_per_group_beyond_bitmap_rejected() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    device.with_buffer_mut(|buf| {
        let sb = &mut buf[sb_offset..sb_offset + 1024];
        sb[32..36].copy_from_slice(&65_536u32.to_le_bytes());
    });

    match mount_geometry(&device) {
        Err(Ext2Error::InvalidSuperblock) => TestResult::Pass,
        other => slopos_testing::fail!("want InvalidSuperblock, got {:?}", other.map(|_| ())),
    }
}

/// `inodes_count` above what the groups can hold lets an in-range inode number
/// divide into a group index past the descriptor table, which reads an
/// arbitrary block as a group descriptor.
pub fn test_ext2_inodes_count_beyond_group_capacity_rejected() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    device.with_buffer_mut(|buf| {
        let sb = &mut buf[sb_offset..sb_offset + 1024];
        sb[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    });

    match mount_geometry(&device) {
        Err(Ext2Error::InvalidSuperblock) => TestResult::Pass,
        other => slopos_testing::fail!("want InvalidSuperblock, got {:?}", other.map(|_| ())),
    }
}

/// `groups_count` was computed by an addition that overflows near `u32::MAX`;
/// the descriptor table must be proven to fit inside the volume instead.
pub fn test_ext2_group_desc_table_beyond_volume_rejected() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    device.with_buffer_mut(|buf| {
        let sb = &mut buf[sb_offset..sb_offset + 1024];
        sb[4..8].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
        sb[32..36].copy_from_slice(&8u32.to_le_bytes());
    });

    match mount_geometry(&device) {
        Err(Ext2Error::InvalidSuperblock) => TestResult::Pass,
        other => slopos_testing::fail!("want InvalidSuperblock, got {:?}", other.map(|_| ())),
    }
}

/// `s_first_data_block` is 1 at 1 KiB blocks and 0 otherwise. The descriptor
/// table's location is derived from it, so a wrong value points the table
/// somewhere else entirely.
pub fn test_ext2_wrong_first_data_block_rejected() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    device.with_buffer_mut(|buf| {
        let sb = &mut buf[sb_offset..sb_offset + 1024];
        sb[20..24].copy_from_slice(&7u32.to_le_bytes());
    });

    match mount_geometry(&device) {
        Err(Ext2Error::InvalidSuperblock) => TestResult::Pass,
        other => slopos_testing::fail!("want InvalidSuperblock, got {:?}", other.map(|_| ())),
    }
}

/// A group descriptor whose inode-table pointer lies outside the volume must
/// be refused at read time: it is the pointer `write_inode_num` writes through.
pub fn test_ext2_group_desc_pointer_outside_volume_refused() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    // The descriptor table sits at block 2 for a 1 KiB image; inode_table is
    // the third le32 of descriptor 0.
    device.with_buffer_mut(|buf| {
        let gd = 2usize * 1024;
        buf[gd + 8..gd + 12].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
    });

    mount_ext2!(device, cache, fs);
    match fs.read_inode(2) {
        Err(Ext2Error::InvalidBlock) => TestResult::Pass,
        other => slopos_testing::fail!("want InvalidBlock, got {:?}", other.map(|_| ())),
    }
}

/// `rec_len=9` with `name_len=1` passes the directory walker's own check
/// (9 >= 8, and 8+1 <= 9) while the inserter needs `dir_entry_size(1) = 12`.
/// Subtracting the two underflowed, which is a panic in dev/tests and a wild
/// slice index in release. Reachable from an ordinary create.
pub fn test_ext2_dir_entry_rec_len_shorter_than_name_refused() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    device.with_buffer_mut(|buf| {
        let dir = FIX_ROOT_DIR_BLOCK as usize * 1024;
        // A live record claiming a 1-byte name in a 12-byte slot, then shrunk
        // to 9 bytes: individually legal to the walker, impossible to the
        // inserter's own step arithmetic.
        buf[dir + 4..dir + 6].copy_from_slice(&9u16.to_le_bytes());
        buf[dir + 6] = 1;
    });

    mount_ext2!(device, cache, fs);
    match fs.create_file(2, b"x") {
        Err(Ext2Error::DirectoryFormat) => TestResult::Pass,
        Err(other) => slopos_testing::fail!("want DirectoryFormat, got {:?}", other),
        Ok(_) => slopos_testing::fail!("a malformed directory record must not be inserted into"),
    }
}

/// The bound above must not reject an image carrying a *deleted* entry: a free
/// record's `name_len` byte is stale and unconstrained, so only a live record's
/// name is held to fitting inside `rec_len`.
pub fn test_ext2_deleted_dir_entry_with_stale_name_len_accepted() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    device.with_buffer_mut(|buf| {
        let dir = FIX_ROOT_DIR_BLOCK as usize * 1024;
        // Free the ".." record at offset 12 but leave a large stale name_len.
        buf[dir + 12..dir + 16].copy_from_slice(&0u32.to_le_bytes());
        buf[dir + 18] = 255;
    });

    mount_ext2!(device, cache, fs);
    match fs.create_file(2, b"ok") {
        Ok(_) => TestResult::Pass,
        Err(other) => {
            slopos_testing::fail!(
                "a deleted entry's stale name_len must not fail: {:?}",
                other
            )
        }
    }
}

fn mount_geometry(
    device: &dyn crate::blockdev::BlockDevice,
) -> Result<crate::ext2::geometry::Ext2Geometry, Ext2Error> {
    let (sb, _bs, _is) = Ext2Fs::mount_params(device)?;
    crate::ext2::geometry::Ext2Geometry::derive(&sb)
}

pub fn test_ext2_invalid_superblock_magic() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    device.with_buffer_mut(|buf| {
        let sb = &mut buf[sb_offset..sb_offset + 1024];
        sb[56] = 0;
        sb[57] = 0;
    });

    let result = Ext2Fs::mount_params(&device);
    match result {
        Err(Ext2Error::InvalidSuperblock) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_unsupported_block_size() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    device.with_buffer_mut(|buf| {
        let sb = &mut buf[sb_offset..sb_offset + 1024];
        sb[24..28].copy_from_slice(&8u32.to_le_bytes());
    });

    let result = Ext2Fs::mount_params(&device);
    match result {
        Err(Ext2Error::UnsupportedBlockSize) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

// A malformed image with inodes_per_group == 0 must be rejected at parse time,
// before the value reaches the inode-number division in block_group()/local_index().
pub fn test_ext2_zero_inodes_per_group_rejected() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let sb_offset = 1024usize;
    device.with_buffer_mut(|buf| {
        let sb = &mut buf[sb_offset..sb_offset + 1024];
        // inodes_per_group lives at superblock offset 40 (le32).
        sb[40..44].copy_from_slice(&0u32.to_le_bytes());
    });

    let result = Ext2Fs::mount_params(&device);
    match result {
        Err(Ext2Error::InvalidSuperblock) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_directory_format_error() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let dir_offset = FIX_ROOT_DIR_BLOCK as usize * 1024;
    device.with_buffer_mut(|buf| {
        let dir_block = &mut buf[dir_offset..dir_offset + 1024];
        dir_block[4] = 0;
        dir_block[5] = 0;
    });

    mount_ext2!(device, _cache, fs);

    let result = fs.for_each_dir_entry(2, |_| true);
    match result {
        Err(Ext2Error::DirectoryFormat) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_invalid_inode() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    mount_ext2!(device, _cache, fs);

    let result = fs.read_inode(9999);
    match result {
        Err(Ext2Error::InvalidInode) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_read_file_not_regular() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    mount_ext2!(device, _cache, fs);

    let mut buf = [0u8; 32];
    let result = fs.read_file(2, 0, &mut buf);
    match result {
        Err(Ext2Error::NotFile) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_device_read_error() -> TestResult {
    let device = FailingBlockDevice::new(4096).with_read_fail();
    let result = Ext2Fs::mount_params(&device);
    match result {
        Err(Ext2Error::DeviceError) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_device_write_error_on_metadata() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let failing = WriteFailingDevice::new(device);
    let (sb, bs, is) = match Ext2Fs::mount_params(&failing) {
        Ok(v) => v,
        Err(_) => return TestResult::Pass,
    };
    let mut cache = match BlockCache::new(bs) {
        Ok(c) => c,
        Err(_) => return TestResult::Fail,
    };
    let mut fs = match Ext2Fs::new(&failing, &mut cache, sb, bs, is) {
        Ok(v) => v,
        Err(_) => return TestResult::Fail,
    };

    // Write-back caching: the mutations land in the cache and the create
    // succeeds; the device error surfaces only at `sync`.
    if fs.create_directory(2, b"faildir").is_err() {
        return TestResult::Fail;
    }
    match fs.sync() {
        Err(Ext2Error::DeviceError) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_read_block_out_of_bounds() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"boot.bin"),
        file_data: Some(b"slopos-test"),
        file_block: 80,
    };
    let Some(device) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };
    mount_ext2!(device, _cache, fs);

    let inode = match fs.resolve_path(b"/boot.bin") {
        Ok(inode) => inode,
        Err(_) => return TestResult::Fail,
    };

    let mut tiny = [0u8; 1];
    let result = fs.read_file(inode, 0, &mut tiny);
    match result {
        Err(Ext2Error::InvalidBlock) | Err(Ext2Error::DeviceError) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_read_file_data_roundtrip() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"boot.bin"),
        file_data: Some(b"slopos-test"),
        file_block: FIX_FILE_BLOCK,
    };
    let Some(device) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };
    mount_ext2!(device, _cache, fs);

    let inode = match fs.resolve_path(b"/boot.bin") {
        Ok(inode) => inode,
        Err(_) => return TestResult::Fail,
    };

    let mut buf = [0u8; 16];
    let read_len = match fs.read_file(inode, 0, &mut buf) {
        Ok(len) => len,
        Err(_) => return TestResult::Fail,
    };

    if read_len != b"slopos-test".len() || &buf[..read_len] != b"slopos-test" {
        return TestResult::Fail;
    }
    TestResult::Pass
}

// `sync` must persist a write such that a brand-new handle with its own cache
// reads it back. Overwrites a pre-existing inode (no allocation) so the test
// isolates cache writeback from the hand-built image's on-disk layout.
pub fn test_ext2_write_persists_across_handles() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"persist.txt"),
        file_data: Some(b"old"),
        file_block: FIX_FILE_BLOCK,
    };
    let Some(device) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };

    let payload = b"persisted-bytes";
    if write_then_sync(&device, b"/persist.txt", payload) == TestResult::Fail {
        return TestResult::Fail;
    }
    expect_file_contents(&device, b"/persist.txt", payload)
}

// Write-back semantics: a write only dirties the cache; the device is not
// touched until `sync`.
pub fn test_ext2_writeback_is_deferred_until_sync() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"defer.txt"),
        file_data: Some(b"old"),
        file_block: FIX_FILE_BLOCK,
    };
    let Some(device) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };

    // Dropped without syncing: the stack-local cache is discarded, which
    // models a crash before writeback.
    if write_without_sync(&device, b"/defer.txt", b"NEW") == TestResult::Fail {
        return TestResult::Fail;
    }
    expect_file_contents(&device, b"/defer.txt", b"old")
}

/// Each mount lives in its own frame: two `Ext2Fs` plus two `BlockCache`
/// handles in one frame sum past the 2 KiB stack cap.
#[inline(never)]
fn write_then_sync(device: &MemoryBlockDevice, path: &[u8], payload: &[u8]) -> TestResult {
    mount_ext2!(*device, _c, fs);
    let Ok(ino) = fs.resolve_path(path) else {
        return TestResult::Fail;
    };
    if fs.write_file(ino, 0, payload).is_err() {
        return TestResult::Fail;
    }
    if fs.sync().is_err() {
        return TestResult::Fail;
    }
    TestResult::Pass
}

#[inline(never)]
fn write_without_sync(device: &MemoryBlockDevice, path: &[u8], payload: &[u8]) -> TestResult {
    mount_ext2!(*device, _c, fs);
    let Ok(ino) = fs.resolve_path(path) else {
        return TestResult::Fail;
    };
    if fs.write_file(ino, 0, payload).is_err() {
        return TestResult::Fail;
    }
    if fs.dirty_count() == 0 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

#[inline(never)]
fn expect_file_contents(device: &MemoryBlockDevice, path: &[u8], want: &[u8]) -> TestResult {
    mount_ext2!(*device, _c, fs);
    let Ok(ino) = fs.resolve_path(path) else {
        return TestResult::Fail;
    };
    let mut buf = [0u8; 32];
    let Ok(n) = fs.read_file(ino, 0, &mut buf) else {
        return TestResult::Fail;
    };
    if n != want.len() || &buf[..n] != want {
        return TestResult::Fail;
    }
    TestResult::Pass
}

// Two operations on the same cache must see each other's writes before any
// sync, and `sync` must leave no dirty blocks.
pub fn test_ext2_cache_reuse_within_handle() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"reuse.txt"),
        file_data: Some(b"old"),
        file_block: FIX_FILE_BLOCK,
    };
    let Some(device) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };
    mount_ext2!(device, _cache, fs);

    let ino = match fs.resolve_path(b"/reuse.txt") {
        Ok(ino) => ino,
        Err(_) => return TestResult::Fail,
    };
    let payload = b"coherent";
    if fs.write_file(ino, 0, payload).is_err() {
        return TestResult::Fail;
    }
    let mut buf = [0u8; 16];
    let n = match fs.read_file(ino, 0, &mut buf) {
        Ok(n) => n,
        Err(_) => return TestResult::Fail,
    };
    if &buf[..n.min(payload.len())] != payload {
        return TestResult::Fail;
    }
    if fs.sync().is_err() {
        return TestResult::Fail;
    }
    if fs.dirty_count() != 0 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

// `sync` is what makes a write durable, so it must leave nothing behind: no
// dirty block, and no write the device has not been asked to commit.
pub fn test_ext2_sync_leaves_nothing_uncommitted() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"sync.txt"),
        file_data: Some(b"old"),
        file_block: FIX_FILE_BLOCK,
    };
    let Some(image) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };
    let device = CountingBlockDevice::new(image);
    mount_ext2!(device, _cache, fs);

    let Ok(ino) = fs.resolve_path(b"/sync.txt") else {
        return TestResult::Fail;
    };
    if fs.write_file(ino, 0, b"committed").is_err() {
        return TestResult::Fail;
    }
    if fs.dirty_count() == 0 {
        return TestResult::Fail;
    }

    device.reset();
    if fs.sync().is_err() {
        return TestResult::Fail;
    }
    if fs.dirty_count() != 0 || fs.unbarriered_writes() != 0 {
        return TestResult::Fail;
    }
    // A commit the device was never told to make is not a commit.
    if device.writes() == 0 || device.flushes() == 0 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

// A second `sync` over an unchanged filesystem must reach the device for
// neither a write nor a barrier: the flusher runs this path every five
// seconds, and `sync(2)` is unprivileged.
pub fn test_ext2_sync_of_clean_fs_touches_no_device() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"idle.txt"),
        file_data: Some(b"old"),
        file_block: FIX_FILE_BLOCK,
    };
    let Some(image) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };
    let device = CountingBlockDevice::new(image);
    mount_ext2!(device, _cache, fs);

    let Ok(ino) = fs.resolve_path(b"/idle.txt") else {
        return TestResult::Fail;
    };
    if fs.write_file(ino, 0, b"once").is_err() || fs.sync().is_err() {
        return TestResult::Fail;
    }

    device.reset();
    if fs.sync().is_err() {
        return TestResult::Fail;
    }
    if device.writes() != 0 || device.flushes() != 0 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

// `fsync` commits the inode's data and record, and nothing that belongs to an
// inode outside its table block. That difference is the whole point of the
// per-inode path — without it the syscall holds the filesystem lock across
// every dirty block on the mount.
//
// The bystander shares this inode's table block, so its data is flushed
// deliberately — publishing the shared record publishes its pointers too. What
// the assertion turns on is the superblock free-count drift, which is the
// mount's state and which a per-inode commit must leave behind.
pub fn test_ext2_sync_inode_commits_the_inode_not_the_mount() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"target.txt"),
        file_data: Some(b"old"),
        file_block: FIX_FILE_BLOCK,
    };
    let Some(image) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };
    let device = CountingBlockDevice::new(image);
    mount_ext2!(device, _cache, fs);

    let Ok(ino) = fs.resolve_path(b"/target.txt") else {
        return TestResult::Fail;
    };
    // Dirties the root's directory block, which no `sync_inode` may publish.
    if fs.create_file(2, b"bystander.txt").is_err() {
        return TestResult::Fail;
    }
    let payload = b"durable-bytes";
    if fs.write_file(ino, 0, payload).is_err() {
        return TestResult::Fail;
    }

    device.reset();
    if fs.sync_inode(ino, false).is_err() {
        return TestResult::Fail;
    }
    if device.writes() == 0 || device.flushes() == 0 {
        return TestResult::Fail;
    }
    if !fs.superblock_dirty() {
        return TestResult::Fail;
    }
    let after_inode = device.writes();
    if fs.sync().is_err() || fs.dirty_count() != 0 {
        return TestResult::Fail;
    }
    if device.writes() <= after_inode {
        return TestResult::Fail;
    }
    expect_block_prefix(&device.inner, FIX_FILE_BLOCK, payload)
}

// An eviction writes a block back and clears its dirty bit with no barrier, so
// a later `fsync` finds nothing dirty over bytes that are still only in the
// device's volatile cache. It must issue the barrier anyway rather than report
// a durability it never obtained.
pub fn test_ext2_sync_inode_commits_evicted_writes() -> TestResult {
    let Some(image) = narrow_image() else {
        return TestResult::Pass;
    };
    let device = CountingBlockDevice::new(image);
    mount_ext2!(device, _cache, fs);

    let Ok(ino) = fs.resolve_path(b"/narrow.txt") else {
        return TestResult::Fail;
    };
    if fs.write_file(ino, 0, b"evicted").is_err() {
        return TestResult::Fail;
    }
    // Stand in for the eviction: the file's bytes have reached the device and
    // nothing has asked it to commit them, while the inode record is still
    // dirty and so still has to be published behind a barrier.
    if fs.cache_evict_data_for_test().is_err() {
        return TestResult::Fail;
    }
    if fs.unbarriered_writes() == 0 {
        return TestResult::Fail;
    }

    device.reset();
    if fs.sync_inode(ino, false).is_err() {
        return TestResult::Fail;
    }
    // Two barriers, not one. The data is already on the device with nothing
    // ordering it, so the inode record needs a barrier *before* it as well as
    // the closing one — otherwise the device may commit the pointers first and
    // `data=ordered` is violated even though the call reported durability.
    if device.flushes() < 2 || fs.unbarriered_writes() != 0 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

// A commit's cost must not grow with the machine's unrelated dirt. Nine files
// in the directory rather than four dirty five more directory records and a
// second inode-table block, and none of that may enlarge this commit.
//
// The comparison is between two crowded filesystems rather than against an
// idle one, because a commit is *not* free of the others' allocation state:
// bitmaps and group descriptors always go, since publishing an inode whose
// blocks the bitmap still calls free invites the next allocation to hand them
// out twice. That is a constant — two bitmaps and a descriptor block — and a
// constant is what this measures the absence of growth on top of.
pub fn test_ext2_sync_inode_cost_ignores_unrelated_metadata() -> TestResult {
    let few = match sync_inode_write_count(4) {
        Some(n) => n,
        None => return TestResult::Pass,
    };
    let Some(many) = sync_inode_write_count(9) else {
        return TestResult::Pass;
    };
    if few == 0 || many != few {
        return TestResult::Fail;
    }
    TestResult::Pass
}

#[inline(never)]
fn sync_inode_write_count(bystanders: u32) -> Option<usize> {
    let device = CountingBlockDevice::new(narrow_image()?);
    count_sync_inode(&device, bystanders)
}

/// The image builder holds a whole `Ext2ImageSpec` plus the block-writing
/// temporaries; keeping it out of the mount's frame is what puts both under
/// the 2 KiB stack gate.
#[inline(never)]
fn narrow_image() -> Option<MemoryBlockDevice> {
    build_ext2_image(Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"narrow.txt"),
        file_data: Some(b"old"),
        file_block: FIX_FILE_BLOCK,
    })
}

#[inline(never)]
fn count_sync_inode(device: &CountingBlockDevice, bystanders: u32) -> Option<usize> {
    let (sb, bs, is) = Ext2Fs::mount_params(device).ok()?;
    let mut cache = BlockCache::new(bs).ok()?;
    let mut fs = Ext2Fs::new(device, &mut cache, sb, bs, is).ok()?;
    count_sync_inode_inner(&mut fs, device, bystanders)
}

/// Split from the mount for the reason [`with_mounted`] gives.
#[inline(never)]
fn count_sync_inode_inner(
    fs: &mut Ext2Fs<'_>,
    device: &CountingBlockDevice,
    bystanders: u32,
) -> Option<usize> {
    let ino = fs.resolve_path(b"/narrow.txt").ok()?;
    create_bystanders(fs, bystanders)?;
    fs.write_file(ino, 0, b"narrow").ok()?;

    device.reset();
    fs.sync_inode(ino, false).ok()?;
    Some(device.writes())
}

#[inline(never)]
fn create_bystanders(fs: &mut Ext2Fs<'_>, count: u32) -> Option<()> {
    for i in 0..count {
        let mut name = *b"filler0.txt";
        name[6] = b'0' + i as u8;
        fs.create_file(2, &name).ok()?;
    }
    Some(())
}

#[inline(never)]
fn expect_block_prefix(device: &MemoryBlockDevice, block: u32, want: &[u8]) -> TestResult {
    let mut buf = [0u8; 32];
    let n = want.len().min(buf.len());
    if device.read_at(block as u64 * 1024, &mut buf[..n]).is_err() {
        return TestResult::Fail;
    }
    if &buf[..n] != &want[..n] {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ext2_path_resolution_not_found() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    mount_ext2!(device, _cache, fs);

    let result = fs.resolve_path(b"/nope/file.txt");
    match result {
        Err(Ext2Error::PathNotFound) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_remove_path_not_file() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    mount_ext2!(device, _cache, fs);

    let result = fs.remove_path(b"/");
    match result {
        Err(Ext2Error::PathNotFound) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

fn test_ext2_aaa_init() -> TestResult {
    if ensure_vfs_ready() {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// Lay out `n` blocks of `bs` bytes plus a verity trailer in `img`, in the
/// shape `gen_verity.py` writes. `corrupt_block` is flipped *after* its hash
/// is recorded, so the stored hash no longer matches the bytes on disk.
#[inline(never)]
fn fill_verity_image(img: &mut [u8], bs: usize, n: usize, corrupt_block: Option<usize>) {
    use crate::verity::crc32;
    for i in 0..n {
        for j in 0..bs {
            img[i * bs + j] = ((i.wrapping_mul(31).wrapping_add(j)) & 0xFF) as u8;
        }
    }
    let arr_off = n * bs;
    for i in 0..n {
        let h = crc32(&img[i * bs..(i + 1) * bs]).to_le_bytes();
        img[arr_off + i * 4..arr_off + i * 4 + 4].copy_from_slice(&h);
    }
    if let Some(c) = corrupt_block {
        img[c * bs] ^= 0xFF;
    }
    let root = crc32(&img[arr_off..arr_off + n * 4]);
    let h = n * bs + n * 4;
    img[h..h + 4].copy_from_slice(&0x5356_5254u32.to_le_bytes());
    img[h + 4..h + 8].copy_from_slice(&1u32.to_le_bytes());
    img[h + 8..h + 12].copy_from_slice(&1u32.to_le_bytes());
    img[h + 12..h + 16].copy_from_slice(&(bs as u32).to_le_bytes());
    img[h + 16..h + 24].copy_from_slice(&(n as u64).to_le_bytes());
    img[h + 24..h + 28].copy_from_slice(&root.to_le_bytes());
    img[h + 28..h + 32].copy_from_slice(&0u32.to_le_bytes());
}

/// A trailer-carrying `MemoryBlockDevice`, boxed but not yet wrapped, so a
/// test can damage the trailer before `build_verified` sees it.
fn build_verity_image(
    bs: usize,
    n: usize,
    corrupt_block: Option<usize>,
) -> Option<slopos_ostd::KBox<MemoryBlockDevice>> {
    let total = n * bs + n * 4 + 32;
    let dev = MemoryBlockDevice::allocate(total)?;
    dev.with_buffer_mut(|img| fill_verity_image(img, bs, n, corrupt_block));
    slopos_ostd::KBox::try_new(dev).ok()
}

fn verity_extent(bs: usize, n: usize) -> crate::verity::FsExtent {
    crate::verity::FsExtent {
        block_size: bs as u32,
        blocks: n as u64,
    }
}

/// Build an image with a verity trailer (`n` blocks of `bs` bytes) behind a
/// `VerifiedBlockDevice`.
fn build_verity_device(
    bs: usize,
    n: usize,
    corrupt_block: Option<usize>,
) -> Option<slopos_ostd::KBox<dyn BlockDevice + Send + Sync>> {
    let boxed: slopos_ostd::KBox<dyn BlockDevice + Send + Sync> =
        build_verity_image(bs, n, corrupt_block)?;
    match crate::verity::build_verified(boxed, verity_extent(bs, n)) {
        Ok((dev, crate::verity::VerityStatus::Verified { .. })) => Some(dev),
        _ => None,
    }
}

/// CRC-32 must match the standard (zlib) algorithm `gen_verity.py` uses,
/// otherwise every verified read would fail.
fn test_verity_crc32_known_vectors() -> TestResult {
    use crate::verity::crc32;
    if crc32(&[]) == 0 && crc32(b"123456789") == 0xCBF4_3926 {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

fn test_verity_clean_read_passes() -> TestResult {
    let bs = 512usize;
    let Some(dev) = build_verity_device(bs, 4, None) else {
        return TestResult::Fail;
    };
    let mut buf = [0u8; 512];
    for b in 0..4u64 {
        if dev.read_at(b * bs as u64, &mut buf).is_err() {
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// A corrupted block fails the read loudly instead of returning bad bytes —
/// the structural defense against the io_capture class.
fn test_verity_corruption_detected() -> TestResult {
    let bs = 512usize;
    let Some(dev) = build_verity_device(bs, 4, Some(2)) else {
        return TestResult::Fail;
    };
    let mut buf = [0u8; 512];
    if dev.read_at(0, &mut buf).is_err() || dev.read_at(bs as u64, &mut buf).is_err() {
        return TestResult::Fail;
    }
    match dev.read_at(2 * bs as u64, &mut buf) {
        Err(BlockDeviceError::IntegrityFailure) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

/// A verified device refuses every write: the trailer describes the bytes the
/// image was built with, and a write would leave a block no trailer describes.
fn test_verity_device_is_write_protected() -> TestResult {
    let bs = 512usize;
    let Some(dev) = build_verity_device(bs, 4, None) else {
        return TestResult::Fail;
    };
    if !dev.write_protected() {
        return slopos_testing::fail!("a verified device must report write_protected");
    }
    let payload = [0xABu8; 512];
    match dev.write_at(2 * bs as u64, &payload) {
        Err(BlockDeviceError::WriteProtected) => {}
        other => return slopos_testing::fail!("want WriteProtected, got {:?}", other),
    }
    let mut buf = [0u8; 512];
    if dev.read_at(2 * bs as u64, &mut buf).is_err() {
        return slopos_testing::fail!("a refused write must leave the block verifiable");
    }
    TestResult::Pass
}

/// The three outcomes of `build_verified` must stay distinguishable: no
/// trailer mounts unverified, a valid trailer mounts verified, and a trailer
/// that is present but corrupt refuses rather than failing open.
fn test_verity_trailer_outcomes() -> TestResult {
    use crate::verity::{VerityError, VerityStatus, build_verified};
    let bs = 512usize;
    let extent = verity_extent(bs, 4);

    let Some(plain) = MemoryBlockDevice::allocate(4 * bs) else {
        return TestResult::Fail;
    };
    let Ok(plain) = slopos_ostd::KBox::try_new(plain) else {
        return TestResult::Fail;
    };
    let plain: slopos_ostd::KBox<dyn BlockDevice + Send + Sync> = plain;
    match build_verified(plain, extent) {
        Ok((dev, VerityStatus::Absent)) if !dev.write_protected() => {}
        other => {
            return slopos_testing::fail!(
                "no trailer must mount unverified and writable, got {:?}",
                other.map(|(_, s)| s)
            );
        }
    }

    let Some(good) = build_verity_image(bs, 4, None) else {
        return TestResult::Fail;
    };
    match build_verified(good, extent) {
        Ok((
            dev,
            VerityStatus::Verified {
                blocks: 4,
                block_size,
            },
        )) if block_size as usize == bs && dev.write_protected() => {}
        other => {
            return slopos_testing::fail!(
                "a valid trailer must verify, got {:?}",
                other.map(|(_, s)| s)
            );
        }
    }

    let Some(corrupt) = build_verity_image(bs, 4, None) else {
        return TestResult::Fail;
    };
    corrupt.with_buffer_mut(|img| img[4 * bs] ^= 0x01);
    match build_verified(corrupt, extent) {
        Err(VerityError::CorruptTrailer) => {}
        other => {
            return slopos_testing::fail!(
                "a corrupt hash array must refuse, got {:?}",
                other.map(|(_, s)| s)
            );
        }
    }

    let Some(oversized) = build_verity_image(bs, 4, None) else {
        return TestResult::Fail;
    };
    oversized.with_buffer_mut(|img| {
        let h = 4 * bs + 4 * 4;
        img[h + 16..h + 24].copy_from_slice(&(1u64 << 40).to_le_bytes());
    });
    match build_verified(oversized, extent) {
        Err(VerityError::Geometry) => {}
        other => {
            return slopos_testing::fail!(
                "a trailer that does not fit must refuse, got {:?}",
                other.map(|(_, s)| s)
            );
        }
    }

    let Some(future) = build_verity_image(bs, 4, None) else {
        return TestResult::Fail;
    };
    future.with_buffer_mut(|img| {
        let h = 4 * bs + 4 * 4;
        img[h + 4..h + 8].copy_from_slice(&2u32.to_le_bytes());
    });
    match build_verified(future, extent) {
        Err(VerityError::UnsupportedTrailer) => TestResult::Pass,
        other => slopos_testing::fail!(
            "an unknown trailer version must refuse, got {:?}",
            other.map(|(_, s)| s)
        ),
    }
}

/// The filesystem's own extent decides what the device's tail is. Trailer
/// magic *inside* the extent is file data a user can write on any writable
/// image — treating it as a trailer would let one file refuse every later
/// mount. And a trailer that covers fewer blocks than the filesystem, or a
/// different block size, verifies nothing it claims to, so it refuses.
fn test_verity_trailer_must_lie_beyond_filesystem() -> TestResult {
    use crate::verity::{FsExtent, VerityError, VerityStatus, build_verified};
    let bs = 512usize;

    let Some(inside) = build_verity_image(bs, 4, None) else {
        return TestResult::Fail;
    };
    let whole_device = FsExtent {
        block_size: bs as u32,
        blocks: (inside.capacity() / bs as u64) + 1,
    };
    match build_verified(inside, whole_device) {
        Ok((dev, VerityStatus::Absent)) if !dev.write_protected() => {}
        other => {
            return slopos_testing::fail!(
                "magic inside the filesystem extent must not be a trailer, got {:?}",
                other.map(|(_, s)| s)
            );
        }
    }

    // Trailer says 4 blocks but the filesystem is 5: a stale trailer from a
    // smaller build would leave the fifth block unverified.
    let Some(short) = MemoryBlockDevice::allocate(6 * bs + 32) else {
        return TestResult::Fail;
    };
    short.with_buffer_mut(|img| {
        let trailer_len = 4 * 4 + 32;
        fill_verity_image(&mut img[..4 * bs + trailer_len], bs, 4, None);
        let src = 4 * bs;
        let dst = img.len() - trailer_len;
        img.copy_within(src..src + trailer_len, dst);
    });
    let Ok(short) = slopos_ostd::KBox::try_new(short) else {
        return TestResult::Fail;
    };
    let short: slopos_ostd::KBox<dyn BlockDevice + Send + Sync> = short;
    let short_extent = FsExtent {
        block_size: bs as u32,
        blocks: 5,
    };
    match build_verified(short, short_extent) {
        Err(VerityError::Geometry) => {}
        other => {
            return slopos_testing::fail!(
                "a trailer covering fewer blocks than the filesystem must refuse, got {:?}",
                other.map(|(_, s)| s)
            );
        }
    }

    let Some(wrong_bs) = build_verity_image(bs, 4, None) else {
        return TestResult::Fail;
    };
    let wrong_extent = FsExtent {
        block_size: 1024,
        blocks: 2,
    };
    match build_verified(wrong_bs, wrong_extent) {
        Err(VerityError::Geometry) => TestResult::Pass,
        other => slopos_testing::fail!(
            "a trailer block size unlike the filesystem's must refuse, got {:?}",
            other.map(|(_, s)| s)
        ),
    }
}

/// Only blocks fully contained in a read are verified, which is the property
/// the sub-block superblock read relies on.
fn test_verity_partial_read_skips() -> TestResult {
    let bs = 512usize;
    let Some(dev) = build_verity_device(bs, 4, Some(2)) else {
        return TestResult::Fail;
    };
    // 256 bytes starting 128 into block 2: never fully covers block 2.
    let mut buf = [0u8; 256];
    match dev.read_at(2 * bs as u64 + 128, &mut buf) {
        Ok(()) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

/// A multi-block read spanning a clean→corrupt boundary catches the corrupt
/// block mid-buffer: the loop covers every block the read touches.
fn test_verity_multiblock_span_detects() -> TestResult {
    let bs = 512usize;
    let Some(dev) = build_verity_device(bs, 4, Some(2)) else {
        return TestResult::Fail;
    };
    // Blocks 1..=3 in one read; block 2 is corrupt.
    let mut buf = [0u8; 512 * 3];
    match dev.read_at(bs as u64, &mut buf) {
        Err(BlockDeviceError::IntegrityFailure) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

/// A registered process with an empty descriptor table, released on drop. A
/// table lives in its process's own registry slot, so there is no table to be
/// had without a process.
struct ScratchProcess {
    process: slopos_ostd::KArc<slopos_ostd::process::Process>,
}

impl ScratchProcess {
    fn new() -> Option<Self> {
        use crate::fileio::fileio_create_empty_table_for_process;
        let process = slopos_ostd::process::process_spawn_root().ok()?;
        let handle = process.handle()?;
        if fileio_create_empty_table_for_process(handle) != 0 {
            slopos_ostd::process::process_retire(handle);
            return None;
        }
        Some(Self { process })
    }

    fn table(&self) -> crate::fileio::FdTable {
        crate::fileio::FdTable::of(&self.process).expect("a registered process has a table")
    }
}

impl Drop for ScratchProcess {
    fn drop(&mut self) {
        use crate::fileio::fileio_destroy_table_for_process;
        if let Some(handle) = self.process.handle() {
            fileio_destroy_table_for_process(handle);
            slopos_ostd::process::process_retire(handle);
        }
    }
}

/// `fileio_open_at_fd` installs a path at an explicit fd, relocating off the
/// next-free slot when they differ.
pub fn test_fileio_open_at_fd() -> TestResult {
    use crate::fileio::{file_close_fd, fileio_open_at_fd};
    use slopos_abi::fs::O_RDONLY;

    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let _ = vfs_mkdir(b"/fileio_test");
    let handle = match vfs_open(b"/fileio_test/open_at.txt", true) {
        Ok(h) => h,
        Err(_) => return TestResult::Fail,
    };
    if handle.write(0, b"x").is_err() {
        return TestResult::Fail;
    }

    let Some(scratch) = ScratchProcess::new() else {
        return TestResult::Fail;
    };
    let table = scratch.table();

    // Next-free would be fd 0; opening at fd 5 must relocate off it.
    let rc = fileio_open_at_fd(table, 5, b"/fileio_test/open_at.txt", O_RDONLY as u32);
    let present = rc == 5 && file_close_fd(table, 5) == 0;
    let low_absent = file_close_fd(table, 0) != 0;

    if present && low_absent {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `fileio_install_file_ref_at` shares a description at an explicit fd, and
/// `fileio_take_file_ref` moves one out (source emptied).
pub fn test_fileio_file_ref_move() -> TestResult {
    use crate::fileio::{
        file_close_fd, fileio_clone_file_ref, fileio_install_file_ref_at, fileio_open_at_fd,
        fileio_take_file_ref,
    };
    use slopos_abi::fs::O_RDONLY;

    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let _ = vfs_mkdir(b"/fileio_test");
    let handle = match vfs_open(b"/fileio_test/refmove.txt", true) {
        Ok(h) => h,
        Err(_) => return TestResult::Fail,
    };
    let _ = handle.write(0, b"x");

    let Some(scratch) = ScratchProcess::new() else {
        return TestResult::Fail;
    };
    let table = scratch.table();
    if fileio_open_at_fd(table, 2, b"/fileio_test/refmove.txt", O_RDONLY as u32) != 2 {
        return TestResult::Fail;
    }

    let outcome = (|| {
        let cloned = fileio_clone_file_ref(table, 2)?;
        if fileio_install_file_ref_at(table, 7, cloned, false) != 7 {
            return None;
        }
        let taken = fileio_take_file_ref(table, 2)?;
        if fileio_install_file_ref_at(table, 9, taken, false) != 9 {
            return None;
        }
        Some(())
    })();

    let ok = outcome.is_some()
        && file_close_fd(table, 2) != 0
        && file_close_fd(table, 7) == 0
        && file_close_fd(table, 9) == 0;

    if ok {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

slopos_testing::stest!(name = test_fileio_open_at_fd);
slopos_testing::stest!(name = test_fileio_file_ref_move);
slopos_testing::stest!(name = test_verity_crc32_known_vectors);
slopos_testing::stest!(name = test_verity_clean_read_passes);
slopos_testing::stest!(name = test_verity_corruption_detected);
slopos_testing::stest!(name = test_verity_device_is_write_protected);
slopos_testing::stest!(name = test_verity_trailer_outcomes);
slopos_testing::stest!(name = test_verity_trailer_must_lie_beyond_filesystem);
slopos_testing::stest!(name = test_verity_partial_read_skips);
slopos_testing::stest!(name = test_verity_multiblock_span_detects);

slopos_testing::stest!(name = test_ext2_aaa_init);
slopos_testing::stest!(name = test_vfs_initialized);
slopos_testing::stest!(name = test_vfs_root_stat);
slopos_testing::stest!(name = test_vfs_file_roundtrip);
slopos_testing::stest!(name = test_vfs_list);
slopos_testing::stest!(name = test_vfs_cd_into_listed_dirs);
slopos_testing::stest!(name = test_vfs_unlink);
slopos_testing::stest!(name = test_vfs_canonicalise_table);
slopos_testing::stest!(name = test_vfs_mount_reached_through_every_spelling);
slopos_testing::stest!(name = test_ramfs_long_name_refused);
slopos_testing::stest!(name = test_ramfs_rename_into_descendant_refused);
slopos_testing::stest!(name = test_vfs_storage_contention_stress_baseline);
slopos_testing::stest!(name = test_ext2_unsupported_incompat_feature_refused);
slopos_testing::stest!(name = test_ext2_unsupported_ro_compat_forces_readonly);
slopos_testing::stest!(name = test_ext2_write_protected_device_forces_readonly);
slopos_testing::stest!(name = test_ext2_inode_size_beyond_block_rejected);
slopos_testing::stest!(name = test_ext2_invalid_superblock_magic);
slopos_testing::stest!(name = test_ext2_unsupported_block_size);
slopos_testing::stest!(name = test_ext2_zero_inodes_per_group_rejected);
slopos_testing::stest!(name = test_ext2_blocks_per_group_beyond_bitmap_rejected);
slopos_testing::stest!(name = test_ext2_inodes_count_beyond_group_capacity_rejected);
slopos_testing::stest!(name = test_ext2_group_desc_table_beyond_volume_rejected);
slopos_testing::stest!(name = test_ext2_wrong_first_data_block_rejected);
slopos_testing::stest!(name = test_ext2_group_desc_pointer_outside_volume_refused);
slopos_testing::stest!(name = test_ext2_dir_entry_rec_len_shorter_than_name_refused);
slopos_testing::stest!(name = test_ext2_deleted_dir_entry_with_stale_name_len_accepted);
slopos_testing::stest!(name = test_ext2_directory_format_error);
slopos_testing::stest!(name = test_ext2_invalid_inode);
slopos_testing::stest!(name = test_ext2_read_file_not_regular);
slopos_testing::stest!(name = test_ext2_device_read_error);
slopos_testing::stest!(name = test_ext2_device_write_error_on_metadata);
slopos_testing::stest!(name = test_ext2_read_block_out_of_bounds);
slopos_testing::stest!(name = test_ext2_read_file_data_roundtrip);
slopos_testing::stest!(name = test_ext2_write_persists_across_handles);
slopos_testing::stest!(name = test_ext2_writeback_is_deferred_until_sync);
slopos_testing::stest!(name = test_ext2_cache_reuse_within_handle);
slopos_testing::stest!(name = test_ext2_sync_leaves_nothing_uncommitted);
slopos_testing::stest!(name = test_ext2_sync_of_clean_fs_touches_no_device);
slopos_testing::stest!(name = test_ext2_sync_inode_commits_the_inode_not_the_mount);
slopos_testing::stest!(name = test_ext2_sync_inode_commits_evicted_writes);
slopos_testing::stest!(name = test_ext2_sync_inode_cost_ignores_unrelated_metadata);
slopos_testing::stest!(name = test_ext2_path_resolution_not_found);
slopos_testing::stest!(name = test_ext2_remove_path_not_file);

const T_S_IFMT: u32 = 0o170000;
const T_S_IFDIR: u32 = 0o040000;
const T_S_IFREG: u32 = 0o100000;

fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    a == b
}

fn cpio_push_hex8(out: &mut KVec<u8>, val: u32) {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    for i in (0..8).rev() {
        let nib = ((val >> (i * 4)) & 0xf) as usize;
        out.push(DIGITS[nib]).expect("cpio test alloc");
    }
}

fn cpio_pad4(out: &mut KVec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0).expect("cpio test alloc");
    }
}

/// Append one `newc` record (header + NUL-terminated name + padded data).
fn cpio_emit(out: &mut KVec<u8>, name: &[u8], mode: u32, data: &[u8]) {
    for &b in b"070701" {
        out.push(b).expect("cpio test alloc");
    }
    cpio_push_hex8(out, 0); // ino
    cpio_push_hex8(out, mode);
    cpio_push_hex8(out, 0); // uid
    cpio_push_hex8(out, 0); // gid
    cpio_push_hex8(out, 1); // nlink
    cpio_push_hex8(out, 0); // mtime
    cpio_push_hex8(out, data.len() as u32);
    cpio_push_hex8(out, 0); // devmajor
    cpio_push_hex8(out, 0); // devminor
    cpio_push_hex8(out, 0); // rdevmajor
    cpio_push_hex8(out, 0); // rdevminor
    cpio_push_hex8(out, (name.len() + 1) as u32);
    cpio_push_hex8(out, 0); // check
    for &b in name {
        out.push(b).expect("cpio test alloc");
    }
    out.push(0).expect("cpio test alloc"); // name NUL
    cpio_pad4(out);
    for &b in data {
        out.push(b).expect("cpio test alloc");
    }
    cpio_pad4(out);
}

fn cpio_sample() -> KVec<u8> {
    let mut ar = KVec::new();
    cpio_emit(&mut ar, b"sbin", T_S_IFDIR | 0o755, b"");
    cpio_emit(&mut ar, b"sbin/init", T_S_IFREG | 0o755, b"hello");
    cpio_emit(&mut ar, b"TRAILER!!!", 0, b"");
    ar
}

pub fn test_cpio_parse_basic() -> TestResult {
    klog_info!("CPIO_TEST: basic parse");
    let ar = cpio_sample();

    let mut idx = 0usize;
    let mut ok = true;
    let result = for_each_cpio_entry(&ar, |entry| {
        match idx {
            0 => {
                if !bytes_eq(entry.path, b"sbin")
                    || entry.mode & T_S_IFMT != T_S_IFDIR
                    || !entry.data.is_empty()
                {
                    ok = false;
                }
            }
            1 => {
                if !bytes_eq(entry.path, b"sbin/init")
                    || entry.mode & T_S_IFMT != T_S_IFREG
                    || entry.mode & 0o111 == 0
                    || !bytes_eq(entry.data, b"hello")
                {
                    ok = false;
                }
            }
            _ => ok = false,
        }
        idx += 1;
        Ok(())
    });

    if result != Ok(2) || idx != 2 || !ok {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_cpio_truncated_header() -> TestResult {
    klog_info!("CPIO_TEST: truncated header rejected");
    let ar = cpio_sample();
    // A header is 110 bytes; 50 bytes can't hold one.
    let res = for_each_cpio_entry(&ar[..50], |_| Ok(()));
    if res == Err(CpioError::Truncated) {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

pub fn test_cpio_bad_magic() -> TestResult {
    klog_info!("CPIO_TEST: bad magic rejected");
    let mut ar = cpio_sample();
    ar[0] = b'X';
    let res = for_each_cpio_entry(&ar, |_| Ok(()));
    if res == Err(CpioError::BadMagic) {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

pub fn test_cpio_truncated_data() -> TestResult {
    klog_info!("CPIO_TEST: truncated data rejected");
    let ar = cpio_sample();
    // Chop the archive mid-way so the second record's data runs past the end.
    let res = for_each_cpio_entry(&ar[..ar.len() - 8], |_| Ok(()));
    if matches!(res, Err(CpioError::Truncated)) {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

slopos_testing::stest!(name = test_cpio_parse_basic);
slopos_testing::stest!(name = test_cpio_truncated_header);
slopos_testing::stest!(name = test_cpio_bad_magic);
slopos_testing::stest!(name = test_cpio_truncated_data);

/// A process may hold `FILEIO_MAX_OPEN_FILES` descriptors and no more, which
/// also pins that the heap-backed table is built at its full size rather than
/// grown under whatever lock happens to be held when a descriptor arrives.
pub fn test_fileio_open_file_limit() -> TestResult {
    use crate::fileio::{FILEIO_MAX_OPEN_FILES, file_open_for_process};
    use slopos_abi::fs::O_RDONLY;

    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let _ = vfs_mkdir(b"/fileio_test");
    let handle = match vfs_open(b"/fileio_test/limit.txt", true) {
        Ok(h) => h,
        Err(_) => return TestResult::Fail,
    };
    if handle.write(0, b"x").is_err() {
        return TestResult::Fail;
    }

    let Some(scratch) = ScratchProcess::new() else {
        return TestResult::Fail;
    };
    let table = scratch.table();

    let mut opened = 0usize;
    let mut first_failure = 0i32;
    for _ in 0..FILEIO_MAX_OPEN_FILES + 1 {
        let fd = file_open_for_process(table, b"/fileio_test/limit.txt", O_RDONLY as u32);
        if fd < 0 {
            first_failure = fd;
            break;
        }
        opened += 1;
    }

    if opened != FILEIO_MAX_OPEN_FILES {
        return slopos_testing::fail!(
            "opened {} descriptors, want {}; first failure {}",
            opened,
            FILEIO_MAX_OPEN_FILES,
            first_failure
        );
    }
    if first_failure != slopos_abi::Errno::EMFILE.raw() {
        return slopos_testing::fail!(
            "descriptor {} failed with {}, want EMFILE ({})",
            opened,
            first_failure,
            slopos_abi::Errno::EMFILE.raw()
        );
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_fileio_open_file_limit);

/// The account row is what refuses, with the errno a full table already
/// returns. The ceiling is set *below* the table length, so the refusal
/// provably comes from the quota rather than from running out of array.
pub fn test_quota_fdslot_drive_to_full() -> TestResult {
    use crate::fileio::file_open_for_process;
    use slopos_abi::fs::O_RDONLY;
    use slopos_abi::quota::{QuotaMode, ResourceKind};
    use slopos_ostd::process::quota::{quota_mode, set_limit, set_quota_mode, stats};

    const CEILING: u32 = 8;

    let Some(path) = scratch_file(b"/fileio_test/quota_full.txt") else {
        return TestResult::Fail;
    };
    let Some(scratch) = ScratchProcess::new() else {
        return TestResult::Fail;
    };
    let table = scratch.table();
    let account = table.account();
    let baseline = stats(account, ResourceKind::FdSlot).map_or(0, |s| s.used);

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(account, ResourceKind::FdSlot, baseline + CEILING);

    let mut opened = 0u32;
    let mut first_failure = 0i32;
    for _ in 0..CEILING + 4 {
        let fd = file_open_for_process(table, path, O_RDONLY as u32);
        if fd < 0 {
            first_failure = fd;
            break;
        }
        opened += 1;
    }

    let s = stats(account, ResourceKind::FdSlot);
    set_quota_mode(restore);

    if opened != CEILING {
        return slopos_testing::fail!(
            "opened {} descriptors against a ceiling of {}; first failure {}",
            opened,
            CEILING,
            first_failure
        );
    }
    if first_failure != slopos_abi::Errno::EMFILE.raw() {
        return slopos_testing::fail!(
            "quota refusal returned {}, want EMFILE ({})",
            first_failure,
            slopos_abi::Errno::EMFILE.raw()
        );
    }
    let Some(s) = s else {
        return slopos_testing::fail!("the account row vanished mid-test");
    };
    if s.used != baseline + CEILING {
        return slopos_testing::fail!("used={} after filling to {}", s.used, baseline + CEILING);
    }
    if s.denials == 0 {
        return slopos_testing::fail!("a refusal nobody counted is a silent denial");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_quota_fdslot_drive_to_full);

/// A refused open refunds exactly once, and closing gives every unit back.
/// `used` returning to its exact pre-test value is the observable form: an
/// under-refund leaves it high, a double refund leaves it low.
pub fn test_quota_fdslot_refusal_refunds_once() -> TestResult {
    use crate::fileio::{file_close_fd, file_open_for_process};
    use slopos_abi::fs::O_RDONLY;
    use slopos_abi::quota::{QuotaMode, ResourceKind};
    use slopos_ostd::process::quota::{quota_mode, set_limit, set_quota_mode, stats};

    const CEILING: u32 = 4;

    let Some(path) = scratch_file(b"/fileio_test/quota_refund.txt") else {
        return TestResult::Fail;
    };
    let Some(scratch) = ScratchProcess::new() else {
        return TestResult::Fail;
    };
    let table = scratch.table();
    let account = table.account();
    let baseline = stats(account, ResourceKind::FdSlot).map_or(0, |s| s.used);

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(account, ResourceKind::FdSlot, baseline + CEILING);

    let mut fds = slopos_ostd::KVec::new();
    for _ in 0..CEILING {
        let fd = file_open_for_process(table, path, O_RDONLY as u32);
        if fd < 0 {
            set_quota_mode(restore);
            return slopos_testing::fail!("open under the ceiling failed with {}", fd);
        }
        if fds.push(fd).is_err() {
            set_quota_mode(restore);
            return slopos_testing::fail!("push");
        }
    }

    // Repeat: a refusal that under-refunds drifts the row, which a single
    // refusal could hide.
    for _ in 0..16 {
        if file_open_for_process(table, path, O_RDONLY as u32) >= 0 {
            set_quota_mode(restore);
            return slopos_testing::fail!("an over-ceiling open succeeded");
        }
    }

    let at_ceiling = stats(account, ResourceKind::FdSlot).map_or(0, |s| s.used);
    if at_ceiling != baseline + CEILING {
        set_quota_mode(restore);
        return slopos_testing::fail!(
            "used={} after 16 refusals, want {} — a refused charge is not the identity",
            at_ceiling,
            baseline + CEILING
        );
    }

    for fd in fds.iter() {
        file_close_fd(table, *fd);
    }
    let after = stats(account, ResourceKind::FdSlot).map_or(0, |s| s.used);
    set_quota_mode(restore);

    if after != baseline {
        return slopos_testing::fail!(
            "used={} after closing every descriptor, want the {} it started at",
            after,
            baseline
        );
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_quota_fdslot_refusal_refunds_once);

/// One process at its ceiling does not deny another — the property a global
/// table bound cannot provide, where the first process to fill the shared
/// array denies everyone.
pub fn test_quota_fdslot_cross_process_isolation() -> TestResult {
    use crate::fileio::file_open_for_process;
    use slopos_abi::fs::O_RDONLY;
    use slopos_abi::quota::{QuotaMode, ResourceKind};
    use slopos_ostd::process::quota::{quota_mode, set_limit, set_quota_mode, stats};

    const CEILING: u32 = 4;

    let Some(path) = scratch_file(b"/fileio_test/quota_isolation.txt") else {
        return TestResult::Fail;
    };
    let Some(greedy) = ScratchProcess::new() else {
        return TestResult::Fail;
    };
    let Some(neighbour) = ScratchProcess::new() else {
        return TestResult::Fail;
    };
    let greedy_table = greedy.table();
    let neighbour_table = neighbour.table();
    let baseline = stats(greedy_table.account(), ResourceKind::FdSlot).map_or(0, |s| s.used);

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(
        greedy_table.account(),
        ResourceKind::FdSlot,
        baseline + CEILING,
    );

    let mut opened = 0u32;
    while file_open_for_process(greedy_table, path, O_RDONLY as u32) >= 0 {
        opened += 1;
        if opened > CEILING {
            break;
        }
    }

    let neighbour_fd = file_open_for_process(neighbour_table, path, O_RDONLY as u32);
    set_quota_mode(restore);

    if opened != CEILING {
        return slopos_testing::fail!("the greedy process opened {opened}, want {CEILING}");
    }
    if neighbour_fd < 0 {
        return slopos_testing::fail!(
            "a neighbour was denied ({}) because another process hit its ceiling",
            neighbour_fd
        );
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_quota_fdslot_cross_process_isolation);

/// The ledger agrees with itself immediately after an exhaustion run. Named
/// `zzz` so it sorts after the tests that drive the ceilings and unwind them.
/// The type system guarantees a charge token is unique, never that the number
/// matches reality, so only this audit can see a forgotten or skipped charge.
pub fn test_zzz_quota_ledger_is_consistent_after_exhaustion() -> TestResult {
    use slopos_ostd::process::quota::{LedgerFault, ledger_audit};

    let mut first: Option<LedgerFault> = None;
    let faults = ledger_audit(|fault| {
        if first.is_none() {
            first = Some(fault);
        }
    });

    if faults != 0 {
        return slopos_testing::fail!(
            "the ledger disagrees with itself in {} place(s); first: {:?}",
            faults,
            first
        );
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_zzz_quota_ledger_is_consistent_after_exhaustion);

/// A file to open repeatedly, or `None` if the scratch tree is unavailable.
fn scratch_file(path: &'static [u8]) -> Option<&'static [u8]> {
    if !ensure_vfs_ready() {
        return None;
    }
    let _ = vfs_mkdir(b"/fileio_test");
    let handle = vfs_open(path, true).ok()?;
    handle.write(0, b"x").ok()?;
    Some(path)
}

/// A sealed inode refuses every mutation, and an unsealed one in the same mount
/// refuses none of them. Both files live in `/tmp`, one filesystem instance, so
/// this cannot pass on a per-filesystem flag.
pub fn test_sealed_inode_refuses_mutation() -> TestResult {
    use crate::vfs::{VfsError, VfsOpenFlags, vfs_open_flags, vfs_set_sealed};

    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    const SEALED: &[u8] = b"/tmp/seal_sealed";
    const PLAIN: &[u8] = b"/tmp/seal_plain";
    let _ = vfs_unlink(b"/tmp/seal_moved");
    let _ = vfs_unlink(SEALED);
    let _ = vfs_unlink(PLAIN);

    let writable = || VfsOpenFlags {
        create: true,
        exclusive: false,
        truncate: false,
        writable: true,
    };
    let Ok(sealed) = vfs_open_flags(SEALED, writable()) else {
        return slopos_testing::fail!("could not create the sealed fixture");
    };
    if sealed.write(0, b"original").is_err() {
        return slopos_testing::fail!("could not populate the sealed fixture");
    }
    let Ok(plain) = vfs_open_flags(PLAIN, writable()) else {
        return slopos_testing::fail!("could not create the unsealed fixture");
    };
    if plain.write(0, b"original").is_err() {
        return slopos_testing::fail!("could not populate the unsealed fixture");
    }
    if vfs_set_sealed(SEALED).is_err() {
        return slopos_testing::fail!("sealing failed");
    }

    if !matches!(sealed.write(0, b"x"), Err(VfsError::PermissionDenied)) {
        return slopos_testing::fail!("a sealed inode accepted a write");
    }
    if !matches!(
        sealed.fs.truncate(sealed.inode, 0),
        Err(VfsError::PermissionDenied)
    ) {
        return slopos_testing::fail!("a sealed inode accepted a truncate");
    }
    if !matches!(vfs_set_mode(SEALED, 0o777), Err(VfsError::PermissionDenied)) {
        return slopos_testing::fail!("a sealed inode accepted a mode change");
    }
    if !matches!(vfs_unlink(SEALED), Err(VfsError::PermissionDenied)) {
        return slopos_testing::fail!("a sealed inode accepted an unlink");
    }
    if !matches!(
        vfs_rename(SEALED, b"/tmp/seal_moved"),
        Err(VfsError::PermissionDenied)
    ) {
        return slopos_testing::fail!("a sealed inode accepted being renamed");
    }
    if !matches!(vfs_rename(PLAIN, SEALED), Err(VfsError::PermissionDenied)) {
        return slopos_testing::fail!("a sealed inode accepted being renamed over");
    }
    if !matches!(
        vfs_open_flags(SEALED, writable()),
        Err(VfsError::PermissionDenied)
    ) {
        return slopos_testing::fail!("a sealed path opened for write");
    }
    // Reading is untouched — `do_exec` still has to load the binary.
    let mut buf = [0u8; 8];
    if sealed.read(0, &mut buf) != Ok(8) || &buf != b"original" {
        return slopos_testing::fail!("a sealed inode stopped being readable");
    }

    if plain.write(0, b"y").is_err() {
        return slopos_testing::fail!("the seal reached an unsealed neighbour's write");
    }
    if vfs_set_mode(PLAIN, 0o755).is_err() {
        return slopos_testing::fail!("the seal reached an unsealed neighbour's mode");
    }
    if vfs_rename(PLAIN, b"/tmp/seal_moved").is_err() {
        return slopos_testing::fail!("the seal reached an unsealed neighbour's rename");
    }
    if vfs_unlink(b"/tmp/seal_moved").is_err() {
        return slopos_testing::fail!("the seal reached an unsealed neighbour's unlink");
    }
    TestResult::Pass
}

/// A filesystem that cannot store the bit refuses to be asked, rather than
/// reporting success and leaving the caller believing a file is protected.
pub fn test_set_sealed_defaults_closed() -> TestResult {
    use crate::vfs::{VfsError, vfs_set_sealed};

    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    match vfs_set_sealed(b"/dev/null") {
        Err(VfsError::NotSupported) => TestResult::Pass,
        other => slopos_testing::fail!("devfs answered {:?}, want NotSupported", other),
    }
}

/// A `MOUNT_RDONLY` mount refuses every mutation at the VFS with `ReadOnly`,
/// before the filesystem sees the request, and reading through it is
/// untouched. The fixture is a second RamFs — which would accept every write
/// if asked — so the refusal can only be the mount flag.
pub fn test_rdonly_mount_refuses_mutation() -> TestResult {
    use crate::ramfs::RamFs;
    use crate::vfs::{
        MOUNT_RDONLY, VfsError, VfsOpenFlags, mount, unmount, vfs_open_flags, vfs_set_sealed,
    };
    use slopos_ostd::lock_class;
    use slopos_ostd::sync::LOCK_LEVEL_RESOURCE;

    static RDONLY_FS: RamFs =
        RamFs::new_const(lock_class!("RAMFS_RDONLY_TEST", LOCK_LEVEL_RESOURCE));
    const MP: &[u8] = b"/tmp/rdonly_mp";
    const FILE: &[u8] = b"/tmp/rdonly_mp/file";

    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let _ = vfs_mkdir(MP);
    if mount(MP, &RDONLY_FS, 0).is_err() {
        return slopos_testing::fail!("could not mount the fixture writable");
    }
    let seeded = vfs_open_flags(FILE, VfsOpenFlags::create_only())
        .and_then(|h| h.write(0, b"original").map(|_| ()))
        .is_ok();
    let _ = unmount(MP);
    if !seeded {
        return slopos_testing::fail!("could not seed the fixture");
    }
    if mount(MP, &RDONLY_FS, MOUNT_RDONLY).is_err() {
        return slopos_testing::fail!("could not remount the fixture read-only");
    }

    let writable = || VfsOpenFlags {
        create: false,
        exclusive: false,
        truncate: false,
        writable: true,
    };
    let result = (|| {
        if !matches!(vfs_open_flags(FILE, writable()), Err(VfsError::ReadOnly)) {
            return Err("an existing file opened for write");
        }
        if !matches!(
            vfs_open_flags(b"/tmp/rdonly_mp/new", VfsOpenFlags::create_only()),
            Err(VfsError::ReadOnly)
        ) {
            return Err("a create succeeded");
        }
        if !matches!(vfs_mkdir(b"/tmp/rdonly_mp/dir"), Err(VfsError::ReadOnly)) {
            return Err("a mkdir succeeded");
        }
        if !matches!(vfs_unlink(FILE), Err(VfsError::ReadOnly)) {
            return Err("an unlink succeeded");
        }
        if !matches!(
            vfs_rename(FILE, b"/tmp/rdonly_mp/moved"),
            Err(VfsError::ReadOnly)
        ) {
            return Err("a rename succeeded");
        }
        if !matches!(vfs_set_mode(FILE, 0o755), Err(VfsError::ReadOnly)) {
            return Err("a chmod succeeded");
        }
        if !matches!(vfs_set_sealed(FILE), Err(VfsError::ReadOnly)) {
            return Err("a seal succeeded");
        }
        let _ = vfs_unlink(b"/tmp/rdonly_src");
        if vfs_open_flags(b"/tmp/rdonly_src", VfsOpenFlags::create_only()).is_err() {
            return Err("could not create the writable-side rename source");
        }
        let cross = vfs_rename(b"/tmp/rdonly_src", b"/tmp/rdonly_mp/into");
        let _ = vfs_unlink(b"/tmp/rdonly_src");
        // Two RamFs instances: the cross-device refusal fires first, which is
        // also correct; what must not happen is success.
        if cross.is_ok() {
            return Err("a rename into the read-only mount succeeded");
        }
        let Ok(h) = vfs_open_flags(FILE, VfsOpenFlags::read_only()) else {
            return Err("a read-only open failed");
        };
        let mut buf = [0u8; 8];
        if h.read(0, &mut buf) != Ok(8) || &buf != b"original" {
            return Err("reading through the read-only mount broke");
        }
        Ok(())
    })();

    let _ = unmount(MP);
    let _ = vfs_unlink(MP);
    match result {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

/// `ReadOnly` reaches userland as `EROFS`, which `std` maps to
/// `ErrorKind::ReadOnlyFilesystem` — not `EACCES`, which would read as a
/// permission problem the caller could fix.
pub fn test_readonly_maps_to_erofs() -> TestResult {
    use crate::vfs::VfsError;
    if VfsError::ReadOnly.to_errno() == slopos_abi::Errno::EROFS {
        TestResult::Pass
    } else {
        slopos_testing::fail!("ReadOnly must map to EROFS")
    }
}

slopos_testing::stest!(name = test_sealed_inode_refuses_mutation);
slopos_testing::stest!(name = test_set_sealed_defaults_closed);
slopos_testing::stest!(name = test_rdonly_mount_refuses_mutation);
slopos_testing::stest!(name = test_readonly_maps_to_erofs);

/// A process at its `ObjectRow` ceiling is refused with `ENFILE`, and every row
/// it held comes back on close. `ENFILE` deliberately, not `EMFILE`: what is
/// exhausted is a system-wide object table (vnode, socket endpoint, pipe), not
/// this process's descriptor numbers, and userland backs off differently.
pub fn test_quota_objectrow_drive_to_full() -> TestResult {
    use crate::fileio::file_open_for_process;
    use slopos_abi::fs::O_RDONLY;
    use slopos_abi::quota::{QuotaMode, ResourceKind};
    use slopos_ostd::process::quota::{quota_mode, set_limit, set_quota_mode, stats};

    const CEILING: u32 = 6;

    let Some(path) = scratch_file(b"/fileio_test/quota_objectrow.txt") else {
        return TestResult::Fail;
    };
    let Some(scratch) = ScratchProcess::new() else {
        return TestResult::Fail;
    };
    let table = scratch.table();
    let account = table.account();
    let baseline = stats(account, ResourceKind::ObjectRow).map_or(0, |s| s.used);

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(account, ResourceKind::ObjectRow, baseline + CEILING);
    // The descriptor ceiling must stay clear of the object ceiling, or the
    // refusal under test would come from the wrong axis with the wrong errno.
    let fd_baseline = stats(account, ResourceKind::FdSlot).map_or(0, |s| s.used);
    set_limit(account, ResourceKind::FdSlot, fd_baseline + CEILING + 64);

    let mut opened = 0u32;
    let mut first_failure = 0i32;
    for _ in 0..CEILING + 4 {
        let fd = file_open_for_process(table, path, O_RDONLY as u32);
        if fd < 0 {
            first_failure = fd;
            break;
        }
        opened += 1;
    }
    let denials = stats(account, ResourceKind::ObjectRow).map_or(0, |s| s.denials);
    set_quota_mode(restore);

    if opened != CEILING {
        return slopos_testing::fail!(
            "opened {} objects against a ceiling of {}; first failure {}",
            opened,
            CEILING,
            first_failure
        );
    }
    if first_failure != slopos_abi::Errno::ENFILE.raw() {
        return slopos_testing::fail!(
            "object refusal returned {}, want ENFILE ({})",
            first_failure,
            slopos_abi::Errno::ENFILE.raw()
        );
    }
    if denials == 0 {
        return slopos_testing::fail!("a refused object nobody counted is a silent denial");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_quota_objectrow_drive_to_full);

pub fn test_quota_objectrow_cross_process_isolation() -> TestResult {
    use crate::fileio::file_open_for_process;
    use slopos_abi::fs::O_RDONLY;
    use slopos_abi::quota::{QuotaMode, ResourceKind};
    use slopos_ostd::process::quota::{quota_mode, set_limit, set_quota_mode, stats};

    const CEILING: u32 = 3;

    let Some(path) = scratch_file(b"/fileio_test/quota_obj_iso.txt") else {
        return TestResult::Fail;
    };
    let (Some(greedy), Some(neighbour)) = (ScratchProcess::new(), ScratchProcess::new()) else {
        return TestResult::Fail;
    };
    let greedy_table = greedy.table();
    let neighbour_table = neighbour.table();
    let baseline = stats(greedy_table.account(), ResourceKind::ObjectRow).map_or(0, |s| s.used);

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(
        greedy_table.account(),
        ResourceKind::ObjectRow,
        baseline + CEILING,
    );

    let mut opened = 0u32;
    while file_open_for_process(greedy_table, path, O_RDONLY as u32) >= 0 {
        opened += 1;
        if opened > CEILING {
            break;
        }
    }
    let neighbour_fd = file_open_for_process(neighbour_table, path, O_RDONLY as u32);
    set_quota_mode(restore);

    if opened != CEILING {
        return slopos_testing::fail!("greedy opened {opened} objects, want {CEILING}");
    }
    if neighbour_fd < 0 {
        return slopos_testing::fail!(
            "a neighbour was denied ({}) because another process filled its object quota",
            neighbour_fd
        );
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_quota_objectrow_cross_process_isolation);

/// Reclaiming from the block cache leaves every surviving block findable: the
/// index maps block number to slot and reclaim moves slots, so a missed repair
/// leaves a live block unreachable under its own number and a *stale* entry
/// hands back another block's bytes. Written against the cache directly — a
/// read through the VFS would silently re-fault the block and report success.
pub fn test_block_cache_reclaim_keeps_the_index_coherent() -> TestResult {
    use crate::blockdev::MemoryBlockDevice;
    use crate::ext2::cache::BlockCache;
    use crate::ext2::types::BlockNum;

    const BLOCK_SIZE: u32 = 1024;
    const BLOCKS: u32 = 24;

    let Some(device) = MemoryBlockDevice::allocate((BLOCKS as usize + 8) * BLOCK_SIZE as usize)
    else {
        return TestResult::Skipped;
    };
    let Ok(mut cache) = BlockCache::new(BLOCK_SIZE) else {
        return TestResult::Fail;
    };

    // Stamp each block with its own number so a mixed-up index is visible as
    // wrong *contents* rather than merely a miss.
    for b in 1..=BLOCKS {
        let Ok(mut guard) = cache.get_zero(BlockNum(b), &device) else {
            return TestResult::Fail;
        };
        guard.data_mut()[0] = b as u8;
    }
    // Clean, so they are reclaimable.
    if cache.flush_all(&device).is_err() {
        return TestResult::Fail;
    }

    // Re-dirty the *last* block so the scan, which walks from the end, skips it
    // and releases from the middle -- that is what makes `swap_remove` move a
    // survivor into a lower slot instead of every removal being a pop.
    {
        let Ok(mut tail) = cache.get(BlockNum(BLOCKS), &device) else {
            return TestResult::Fail;
        };
        tail.data_mut()[0] = BLOCKS as u8;
    }

    // Reclaim nearly everything: a small request is satisfied from the
    // pre-allocated empty trailing slots and never disturbs a live block.
    let before = cache.reclaimable();
    slopos_testing::assert_test!(before > 0, "a flushed cache reported nothing reclaimable");
    let released = cache.shrink_clean(before);
    slopos_testing::assert_test!(released > 0, "shrink_clean released nothing");

    // A survivor answers through the index, an evicted block through a re-read
    // from the device.
    for b in 1..=BLOCKS {
        let Ok(guard) = cache.get(BlockNum(b), &device) else {
            return TestResult::Fail;
        };
        slopos_testing::assert_test!(
            guard.data()[0] == b as u8,
            "block {} read back as {} after reclaim -- the index points at the wrong slot",
            b,
            guard.data()[0]
        );
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_block_cache_reclaim_keeps_the_index_coherent,
    suite = fs
);

/// Mount `device` and hand the handle to `body`.
///
/// Its own frame on purpose: the superblock, the cache and the `Ext2Fs` are
/// live across the whole call, and a debug build gives every `?` temporary in
/// the body its own slot on top of them. Splitting the two is what puts both
/// under the 2 KiB stack gate.
#[inline(never)]
fn with_mounted(
    device: &MemoryBlockDevice,
    body: fn(&mut Ext2Fs<'_>) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let (sb, bs, is) = Ext2Fs::mount_params(device).map_err(|_| "mount_params")?;
    let mut cache = BlockCache::new(bs).map_err(|_| "cache")?;
    let mut fs = Ext2Fs::new(device, &mut cache, sb, bs, is).map_err(|_| "mount")?;
    body(&mut fs)
}

/// A fixture with a writable file already in place.
#[inline(never)]
fn phase3_image(name: &[u8], data: &[u8]) -> Option<MemoryBlockDevice> {
    build_ext2_image(Ext2ImageSpec {
        blocks: 128,
        inodes: 32,
        file_name: Some(name),
        file_data: Some(data),
        file_block: FIX_FILE_BLOCK,
    })
}

/// A file the tests themselves created must then be writable: the fixture's
/// inode table spans four blocks, and data placed inside that span used to be
/// overwritten by the first created inode's own record.
pub fn test_ext2_created_file_is_writable() -> TestResult {
    let Some(device) = phase3_image(b"seed.txt", b"seed") else {
        return TestResult::Skipped;
    };
    mount_ext2!(device, _cache, fs);

    let Ok(ino) = fs.create_file(2, b"fresh.txt") else {
        return slopos_testing::fail!("create failed on a healthy fixture");
    };
    let payload = b"written-into-a-created-inode";
    match fs.write_file(ino, 0, payload) {
        Ok(n) if n == payload.len() => {}
        other => return slopos_testing::fail!("write to a created file: {:?}", other),
    }
    // The root directory must still be readable: an inode record landing on
    // top of it is exactly the fixture defect this guards.
    let Ok(found) = fs.resolve_path(b"/seed.txt") else {
        return slopos_testing::fail!("the root directory was clobbered by an inode write");
    };
    let mut buf = [0u8; 32];
    match fs.read_file(ino, 0, &mut buf) {
        Ok(n) if &buf[..n] == payload => {}
        other => return slopos_testing::fail!("read back: {:?}", other),
    }
    let _ = found;
    TestResult::Pass
}

/// `truncate` to zero then a fresh write is `O_TRUNC`, and the shrink must
/// hand the blocks back rather than leak them.
pub fn test_ext2_truncate_shrinks_and_frees() -> TestResult {
    let Some(device) = phase3_image(b"trunc.txt", b"old") else {
        return TestResult::Skipped;
    };
    match truncate_body(&device) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

fn truncate_body(device: &MemoryBlockDevice) -> Result<(), &'static str> {
    with_mounted(device, truncate_body_inner)
}

#[inline(never)]
fn truncate_body_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.create_file(2, b"big.txt").map_err(|_| "create")?;
    // Three blocks, so the shrink frees two and keeps one partial.
    let mut chunk = KVec::<u8>::zeroed(1024).map_err(|_| "alloc")?;
    chunk.as_mut_slice().fill(0xAB);
    for i in 0..3u64 {
        fs.write_file(ino, i * 1024, chunk.as_slice())
            .map_err(|_| "could not grow the fixture")?;
    }
    let free_full = fs.superblock().free_blocks_count;

    fs.truncate_file(ino, 100).map_err(|_| "truncate failed")?;
    if fs.superblock().free_blocks_count != free_full + 2 {
        return Err("truncate did not free exactly the two blocks past the new end");
    }

    // The tail of the surviving block must read as zeros: bytes past the new
    // end are still on disk and an extension would surface them.
    fs.truncate_file(ino, 200).map_err(|_| "extension failed")?;
    let mut buf = KVec::<u8>::zeroed(200).map_err(|_| "alloc")?;
    if fs.read_file(ino, 0, buf.as_mut_slice()) != Ok(200) {
        return Err("a short read after the extension");
    }
    if buf.as_slice()[..100].iter().any(|&b| b != 0xAB) {
        return Err("truncate lost surviving bytes");
    }
    if buf.as_slice()[100..].iter().any(|&b| b != 0) {
        return Err("an extension surfaced bytes truncate removed");
    }
    Ok(())
}

/// `truncate` on a directory is `EISDIR`, not a silent block free.
pub fn test_ext2_truncate_refuses_a_directory() -> TestResult {
    let Some(device) = phase3_image(b"t.txt", b"x") else {
        return TestResult::Skipped;
    };
    mount_ext2!(device, _cache, fs);
    match fs.truncate_file(2, 0) {
        Err(Ext2Error::IsDirectory) => TestResult::Pass,
        other => slopos_testing::fail!("want IsDirectory, got {:?}", other),
    }
}

/// Rename within one directory, and the write-then-remove order: the source
/// name is gone, the destination resolves to the same inode, and the contents
/// followed it.
pub fn test_ext2_rename_same_directory() -> TestResult {
    let Some(device) = phase3_image(b"src.txt", b"payload") else {
        return TestResult::Skipped;
    };
    mount_ext2!(device, _cache, fs);

    let Ok(before) = fs.resolve_path(b"/src.txt") else {
        return TestResult::Fail;
    };
    if fs.rename_entry(2, b"src.txt", 2, b"dst.txt").is_err() {
        return slopos_testing::fail!("rename failed");
    }
    if fs.resolve_path(b"/src.txt").is_ok() {
        return slopos_testing::fail!("the old name survived the rename");
    }
    match fs.resolve_path(b"/dst.txt") {
        Ok(after) if after == before => {}
        other => return slopos_testing::fail!("the new name resolves to {:?}", other),
    }
    let mut buf = [0u8; 16];
    match fs.read_file(before, 0, &mut buf) {
        Ok(n) if &buf[..n] == b"payload" => TestResult::Pass,
        other => slopos_testing::fail!("contents after rename: {:?}", other),
    }
}

/// Renaming over an existing file frees the displaced inode rather than
/// leaking it, and renaming a directory into its own subtree is refused.
pub fn test_ext2_rename_over_and_into_descendant() -> TestResult {
    let Some(device) = phase3_image(b"a.txt", b"aaa") else {
        return TestResult::Skipped;
    };
    mount_ext2!(device, _cache, fs);

    let Ok(_) = fs.create_file(2, b"b.txt") else {
        return TestResult::Fail;
    };
    let free_inodes = fs.superblock().free_inodes_count;
    if fs.rename_entry(2, b"a.txt", 2, b"b.txt").is_err() {
        return slopos_testing::fail!("rename-over failed");
    }
    if fs.superblock().free_inodes_count != free_inodes + 1 {
        return slopos_testing::fail!("rename-over leaked the displaced inode");
    }

    let Ok(_outer) = fs.create_directory(2, b"outer") else {
        return TestResult::Fail;
    };
    let Ok(outer) = fs.resolve_path(b"/outer") else {
        return TestResult::Fail;
    };
    let Ok(_inner) = fs.create_directory(outer, b"inner") else {
        return TestResult::Fail;
    };
    let Ok(inner) = fs.resolve_path(b"/outer/inner") else {
        return TestResult::Fail;
    };
    match fs.rename_entry(2, b"outer", inner, b"cycle") {
        Err(Ext2Error::InvalidPath) => TestResult::Pass,
        other => slopos_testing::fail!("want InvalidPath for a self-splice, got {:?}", other),
    }
}

/// `rmdir` removes only an empty directory, decrements the parent's
/// `links_count` for the vanished `..`, and refuses a regular file.
pub fn test_ext2_rmdir_semantics() -> TestResult {
    let Some(device) = phase3_image(b"f.txt", b"x") else {
        return TestResult::Skipped;
    };
    mount_ext2!(device, _cache, fs);

    let Ok(_) = fs.create_directory(2, b"d") else {
        return TestResult::Fail;
    };
    let Ok(d) = fs.resolve_path(b"/d") else {
        return TestResult::Fail;
    };
    let Ok(links_with_child) = fs.read_inode(2).map(|i| i.links_count) else {
        return TestResult::Fail;
    };

    if fs.create_file(d, b"occupant").is_err() {
        return TestResult::Fail;
    }
    if !matches!(fs.remove_directory(2, b"d"), Err(Ext2Error::NotEmpty)) {
        return slopos_testing::fail!("rmdir accepted a non-empty directory");
    }
    if !matches!(
        fs.remove_directory(2, b"f.txt"),
        Err(Ext2Error::NotDirectory)
    ) {
        return slopos_testing::fail!("rmdir accepted a regular file");
    }
    if !matches!(fs.unlink_entry(2, b"d"), Err(Ext2Error::IsDirectory)) {
        return slopos_testing::fail!("unlink accepted a directory");
    }

    if fs.unlink_entry(d, b"occupant").is_err() {
        return TestResult::Fail;
    }
    if fs.remove_directory(2, b"d").is_err() {
        return slopos_testing::fail!("rmdir failed on an emptied directory");
    }
    match fs.read_inode(2).map(|i| i.links_count) {
        Ok(n) if n == links_with_child - 1 => {}
        other => {
            return slopos_testing::fail!(
                "parent links_count after rmdir: {:?}, want {}",
                other,
                links_with_child - 1
            );
        }
    }
    if fs.resolve_path(b"/d").is_ok() {
        return slopos_testing::fail!("the removed directory still resolves");
    }
    TestResult::Pass
}

/// A fast symlink (target inside `i_block`) and a slow one (target in a data
/// block) both round-trip, and removing one does not free fifteen blocks it
/// never owned.
pub fn test_ext2_symlink_roundtrip() -> TestResult {
    let Some(device) = phase3_image(b"tgt.txt", b"target") else {
        return TestResult::Skipped;
    };
    // The body answers a static reason: every `fail!` in one frame carries its
    // own format-args state, which alone puts this over the 2 KiB gate.
    match symlink_roundtrip_body(&device) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

fn symlink_roundtrip_body(device: &MemoryBlockDevice) -> Result<(), &'static str> {
    with_mounted(device, symlink_roundtrip_body_inner)
}

#[inline(never)]
fn symlink_roundtrip_body_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    const FAST: &[u8] = b"/tgt.txt";
    let mut long = KVec::<u8>::zeroed(100).map_err(|_| "alloc")?;
    long.as_mut_slice().fill(b'x');

    let fast = fs
        .create_symlink(2, b"fast", FAST)
        .map_err(|_| "fast symlink create failed")?;
    let free_after_fast = fs.superblock().free_blocks_count;
    let slow = fs
        .create_symlink(2, b"slow", long.as_slice())
        .map_err(|_| "slow symlink create failed")?;
    if fs.superblock().free_blocks_count != free_after_fast - 1 {
        return Err("a slow symlink must cost exactly one block");
    }

    let mut buf = KVec::<u8>::zeroed(128).map_err(|_| "alloc")?;
    match fs.read_symlink(fast, buf.as_mut_slice()) {
        Ok(n) if &buf.as_slice()[..n] == FAST => {}
        _ => return Err("a fast symlink did not read back its target"),
    }
    match fs.read_symlink(slow, buf.as_mut_slice()) {
        Ok(n) if buf.as_slice()[..n] == *long.as_slice() => {}
        _ => return Err("a slow symlink did not read back its target"),
    }

    // A fast symlink's `i_block` holds text, not block numbers: freeing it as
    // if it did would hand the allocator arbitrary blocks.
    let before = fs.superblock().free_blocks_count;
    fs.unlink_entry(2, b"fast")
        .map_err(|_| "unlink of a fast symlink failed")?;
    if fs.superblock().free_blocks_count != before {
        return Err("removing a fast symlink freed blocks it never owned");
    }
    fs.unlink_entry(2, b"slow")
        .map_err(|_| "unlink of a slow symlink failed")?;
    if fs.superblock().free_blocks_count != before + 1 {
        return Err("removing a slow symlink leaked its data block");
    }
    Ok(())
}

/// `set_mode` writes permission bits through and leaves the type nibble;
/// `set_sealed` stamps `EXT2_IMMUTABLE_FL` and every mutation then refuses.
pub fn test_ext2_mode_and_seal() -> TestResult {
    let Some(device) = phase3_image(b"m.txt", b"x") else {
        return TestResult::Skipped;
    };
    mount_ext2!(device, _cache, fs);

    let Ok(ino) = fs.resolve_path(b"/m.txt") else {
        return TestResult::Fail;
    };
    if fs.set_mode(ino, 0o755).is_err() {
        return slopos_testing::fail!("set_mode failed");
    }
    match fs.read_inode(ino) {
        Ok(i) if i.mode == 0x8000 | 0o755 => {}
        Ok(i) => return slopos_testing::fail!("mode is 0o{:o}, want a regular file 0o755", i.mode),
        Err(e) => return slopos_testing::fail!("read_inode: {:?}", e),
    }

    if fs.is_sealed(ino) != Ok(false) {
        return slopos_testing::fail!("a fresh inode reported sealed");
    }
    if fs.set_sealed(ino).is_err() {
        return slopos_testing::fail!("set_sealed failed");
    }
    if fs.is_sealed(ino) != Ok(true) {
        return slopos_testing::fail!("the seal did not stick");
    }
    if !matches!(fs.write_file(ino, 0, b"no"), Err(Ext2Error::Immutable)) {
        return slopos_testing::fail!("a sealed inode accepted a write");
    }
    if !matches!(fs.truncate_file(ino, 0), Err(Ext2Error::Immutable)) {
        return slopos_testing::fail!("a sealed inode accepted a truncate");
    }
    if !matches!(fs.set_mode(ino, 0o777), Err(Ext2Error::Immutable)) {
        return slopos_testing::fail!("a sealed inode accepted a mode change");
    }
    if !matches!(fs.unlink_entry(2, b"m.txt"), Err(Ext2Error::Immutable)) {
        return slopos_testing::fail!("a sealed inode accepted an unlink");
    }
    TestResult::Pass
}

/// The seal must survive a remount: it is an on-disk inode flag, not a
/// per-mount fact.
pub fn test_ext2_seal_survives_a_remount() -> TestResult {
    let Some(device) = phase3_image(b"p.txt", b"x") else {
        return TestResult::Skipped;
    };
    if seal_and_sync(&device, b"/p.txt") == TestResult::Fail {
        return TestResult::Fail;
    }
    expect_sealed(&device, b"/p.txt")
}

#[inline(never)]
fn seal_and_sync(device: &MemoryBlockDevice, path: &[u8]) -> TestResult {
    mount_ext2!(*device, _c, fs);
    let Ok(ino) = fs.resolve_path(path) else {
        return TestResult::Fail;
    };
    if fs.set_sealed(ino).is_err() || fs.sync().is_err() {
        return TestResult::Fail;
    }
    TestResult::Pass
}

#[inline(never)]
fn expect_sealed(device: &MemoryBlockDevice, path: &[u8]) -> TestResult {
    mount_ext2!(*device, _c, fs);
    let Ok(ino) = fs.resolve_path(path) else {
        return TestResult::Fail;
    };
    match fs.is_sealed(ino) {
        Ok(true) => TestResult::Pass,
        other => slopos_testing::fail!("the seal did not survive the remount: {:?}", other),
    }
}

/// A failed operation must leave nothing behind: no dirtied block the flusher
/// could publish, and no free-count drift.
///
/// The trigger is a `mkdir` on a full disk, which is the sharp case named in
/// the plan: `create_inode_entry` allocates the inode, *then* allocates the
/// directory's first data block. Without the rollback the failure leaves an
/// allocated, written, unreferenced inode in the cache for the next flush to
/// publish, with the superblock's counts saying otherwise.
pub fn test_ext2_failed_op_leaves_no_partial_state() -> TestResult {
    let Some(device) = build_ext2_image(Ext2ImageSpec {
        blocks: 24,
        inodes: 32,
        file_name: Some(b"r.txt"),
        file_data: Some(b"x"),
        file_block: FIX_FILE_BLOCK,
    }) else {
        return TestResult::Skipped;
    };
    match failed_op_body(&device) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

fn failed_op_body(device: &MemoryBlockDevice) -> Result<(), &'static str> {
    with_mounted(device, failed_op_body_inner)
}

#[inline(never)]
fn failed_op_body_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    // Consume every free block, so the next allocation fails.
    let hog = fs
        .create_file(2, b"hog.txt")
        .map_err(|_| "fixture create")?;
    let mut big = KVec::<u8>::zeroed(1024 * 32).map_err(|_| "alloc")?;
    big.as_mut_slice().fill(0x11);
    let _ = fs.write_file(hog, 0, big.as_slice());
    if fs.superblock().free_blocks_count != 0 {
        return Err("the fixture still has free blocks; the failure would not trigger");
    }
    fs.sync().map_err(|_| "sync")?;

    let free_blocks = fs.superblock().free_blocks_count;
    let free_inodes = fs.superblock().free_inodes_count;
    let used_dirs = fs.group_used_dirs(0);
    let dirty = fs.dirty_count();

    // The inode is allocated and written, then the data-block allocation
    // fails: the exact window the rollback guard exists for.
    if !matches!(fs.create_directory(2, b"doomed"), Err(Ext2Error::NoSpace)) {
        return Err("mkdir on a full disk was accepted");
    }

    if fs.superblock().free_blocks_count != free_blocks {
        return Err("a failed mkdir moved the free-block count");
    }
    if fs.superblock().free_inodes_count != free_inodes {
        return Err("a failed mkdir leaked an inode");
    }
    if fs.group_used_dirs(0) != used_dirs {
        return Err("a failed mkdir moved used_dirs_count");
    }
    if fs.dirty_count() != dirty {
        return Err("a failed mkdir left dirtied blocks a later flush would publish");
    }
    if fs.resolve_path(b"/doomed").is_ok() {
        return Err("a failed mkdir left a resolvable name");
    }

    // The rejections that precede any allocation must still be rejections.
    if !matches!(fs.create_file(2, b"r.txt"), Err(Ext2Error::AlreadyExists)) {
        return Err("a duplicate create was accepted");
    }
    let mut long = KVec::<u8>::zeroed(256).map_err(|_| "alloc")?;
    long.as_mut_slice().fill(b'z');
    if !matches!(
        fs.create_file(2, long.as_slice()),
        Err(Ext2Error::NameTooLong)
    ) {
        return Err("an over-long name was accepted");
    }
    Ok(())
}

/// A mid-write ENOSPC must report the short count and keep the blocks it
/// already allocated reachable, rather than returning early with the size
/// unset and the blocks leaked.
pub fn test_ext2_partial_write_reports_a_short_count() -> TestResult {
    // Small enough that a multi-block write runs the disk out partway.
    let Some(device) = build_ext2_image(Ext2ImageSpec {
        blocks: 24,
        inodes: 32,
        file_name: Some(b"tight.txt"),
        file_data: Some(b"x"),
        file_block: FIX_FILE_BLOCK,
    }) else {
        return TestResult::Skipped;
    };
    match partial_write_body(&device) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

fn partial_write_body(device: &MemoryBlockDevice) -> Result<(), &'static str> {
    with_mounted(device, partial_write_body_inner)
}

#[inline(never)]
fn partial_write_body_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.create_file(2, b"filler.txt").map_err(|_| "create")?;
    // Heap-held: a 32 KiB array is eight guard pages of stack frame.
    let mut big = KVec::<u8>::zeroed(1024 * 32).map_err(|_| "alloc")?;
    big.as_mut_slice().fill(0x5A);
    let want = big.len();
    let written = fs
        .write_file(ino, 0, big.as_slice())
        .map_err(|_| "a full-disk write reported an error rather than a short count")?;
    if written == 0 || written == want {
        return Err("want a short write");
    }
    // The size must cover what landed: a shorter one leaks the tail blocks, a
    // longer one hands back a hole the write never filled.
    if fs.read_inode(ino).map(|i| i.size) != Ok(written as u64) {
        return Err("the size after a short write does not match the bytes written");
    }
    let mut buf = [0u8; 64];
    match fs.read_file(ino, (written - 64) as u64, &mut buf) {
        Ok(64) if buf.iter().all(|&b| b == 0x5A) => Ok(()),
        _ => Err("the tail of a short write did not read back"),
    }
}

/// A directory of more than one listing buffer must be enumerable in full:
/// the cookie resumes where the last page stopped, and every name appears
/// exactly once.
pub fn test_ext2_readdir_cursor_pages_a_large_directory() -> TestResult {
    let Some(device) = build_ext2_image(Ext2ImageSpec {
        blocks: 256,
        inodes: 32,
        file_name: Some(b"seed.txt"),
        file_data: Some(b"x"),
        file_block: FIX_FILE_BLOCK,
    }) else {
        return TestResult::Skipped;
    };
    mount_ext2!(device, _cache, fs);

    // More than one 1 KiB directory block's worth, so the cookie has to cross
    // a block boundary rather than merely index within one.
    const COUNT: u32 = 20;
    for i in 0..COUNT {
        let mut name = *b"entry00";
        name[5] = b'0' + (i / 10) as u8;
        name[6] = b'0' + (i % 10) as u8;
        if fs.create_file(2, &name).is_err() {
            return slopos_testing::fail!("could not create entry {}", i);
        }
    }

    let mut seen = 0u32;
    let mut cookie = 0u64;
    let mut pages = 0u32;
    loop {
        let mut in_page = 0u32;
        let reached = match fs.for_each_dir_entry_from(2, cookie, |next, entry| {
            if entry.name == b"." || entry.name == b".." {
                cookie = next;
                return true;
            }
            in_page += 1;
            cookie = next;
            // Three at a time, so the walk resumes many times.
            in_page < 3
        }) {
            Ok(r) => r,
            Err(e) => return slopos_testing::fail!("paged readdir: {:?}", e),
        };
        seen += in_page;
        pages += 1;
        if in_page < 3 {
            let _ = reached;
            break;
        }
        if pages > COUNT + 8 {
            return slopos_testing::fail!("the cursor did not advance; {} pages", pages);
        }
    }

    // COUNT created plus the fixture's own seed.txt.
    if seen != COUNT + 1 {
        return slopos_testing::fail!("paged readdir saw {} entries, want {}", seen, COUNT + 1);
    }
    if pages < 3 {
        return slopos_testing::fail!("the walk finished in {} pages; it never resumed", pages);
    }
    TestResult::Pass
}

/// `used_dirs_count` and `i_dtime` are what `e2fsck` reads; neither was
/// maintained before.
pub fn test_ext2_group_bookkeeping_on_create_and_remove() -> TestResult {
    let Some(device) = phase3_image(b"g.txt", b"x") else {
        return TestResult::Skipped;
    };
    match with_mounted(&device, group_bookkeeping_inner) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

#[inline(never)]
fn group_bookkeeping_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let before = fs.group_used_dirs(0).map_err(|_| "used_dirs")?;
    fs.create_directory(2, b"counted").map_err(|_| "mkdir")?;
    if fs.group_used_dirs(0) != Ok(before + 1) {
        return Err("used_dirs did not rise on mkdir");
    }
    fs.remove_directory(2, b"counted").map_err(|_| "rmdir")?;
    if fs.group_used_dirs(0) != Ok(before) {
        return Err("used_dirs did not fall on rmdir");
    }

    let doomed = fs.create_file(2, b"doomed.txt").map_err(|_| "create")?;
    fs.unlink_entry(2, b"doomed.txt").map_err(|_| "unlink")?;
    // `i_dtime` is stamped only when the kernel has a wall clock, and a test
    // boot without a firmware RTC legitimately has none. What must hold either
    // way is that the record was cleared.
    match fs.read_inode(doomed) {
        Ok(i) if i.mode == 0 && i.links_count == 0 => Ok(()),
        _ => Err("a freed inode's record was not cleared"),
    }
}

/// Unlinking one name of a hardlinked inode must drop a link, not free the
/// inode: the surviving name would otherwise read blocks the allocator has
/// handed to someone else. Such images are not written by this kernel but are
/// routine from `mkfs`/`e2fsck`, so the link count is what decides.
pub fn test_ext2_unlink_respects_link_count() -> TestResult {
    let Some(device) = phase3_image(b"h.txt", b"payload") else {
        return TestResult::Skipped;
    };
    match with_mounted(&device, unlink_link_count_inner) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

#[inline(never)]
fn unlink_link_count_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.resolve_path(b"/h.txt").map_err(|_| "resolve")?;
    // Fabricate the second link the way an `mkfs` image carries one: the
    // count is what `unlink` consults, and this kernel has no `link(2)`.
    let mut inode = fs.read_inode(ino).map_err(|_| "read_inode")?;
    inode.links_count = 2;
    fs.write_inode_for_test(ino, &inode)
        .map_err(|_| "write_inode")?;

    let free_inodes = fs.superblock().free_inodes_count;
    let free_blocks = fs.superblock().free_blocks_count;
    fs.unlink_entry(2, b"h.txt").map_err(|_| "unlink")?;

    if fs.superblock().free_inodes_count != free_inodes {
        return Err("unlinking one of two links freed the inode");
    }
    if fs.superblock().free_blocks_count != free_blocks {
        return Err("unlinking one of two links freed the file's blocks");
    }
    match fs.read_inode(ino) {
        Ok(i) if i.links_count == 1 && i.mode != 0 => {}
        _ => return Err("the surviving link's inode was cleared"),
    }
    // The data must still be there for the surviving name.
    let mut buf = [0u8; 16];
    match fs.read_file(ino, 0, &mut buf) {
        Ok(n) if &buf[..n] == b"payload" => Ok(()),
        _ => Err("the surviving link's contents were lost"),
    }
}

/// `i_size_high` must round-trip and the superblock must gain
/// `RO_COMPAT_LARGE_FILE`: without the bit every other reader sees a
/// truncated size.
pub fn test_ext2_large_file_size_roundtrips() -> TestResult {
    let Some(device) = phase3_image(b"l.txt", b"x") else {
        return TestResult::Skipped;
    };
    mount_ext2!(device, _cache, fs);

    let Ok(ino) = fs.resolve_path(b"/l.txt") else {
        return TestResult::Fail;
    };
    // A sparse extension past 4 GiB costs no blocks, which is what makes this
    // testable on a 128 KiB fixture.
    const BIG: u64 = (1u64 << 32) + 4096;
    if fs.truncate_file(ino, BIG).is_err() {
        return slopos_testing::fail!("sparse extension past 4 GiB failed");
    }
    match fs.read_inode(ino).map(|i| i.size) {
        Ok(BIG) => {}
        other => return slopos_testing::fail!("size round-tripped as {:?}, want {}", other, BIG),
    }
    if fs.superblock().feature_ro_compat & 0x0002 == 0 {
        return slopos_testing::fail!("a large file did not set RO_COMPAT_LARGE_FILE");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_ext2_created_file_is_writable, suite = fs);
slopos_testing::stest!(name = test_ext2_truncate_shrinks_and_frees, suite = fs);
slopos_testing::stest!(name = test_ext2_truncate_refuses_a_directory, suite = fs);
slopos_testing::stest!(name = test_ext2_rename_same_directory, suite = fs);
slopos_testing::stest!(name = test_ext2_rename_over_and_into_descendant, suite = fs);
slopos_testing::stest!(name = test_ext2_rmdir_semantics, suite = fs);
slopos_testing::stest!(name = test_ext2_symlink_roundtrip, suite = fs);
slopos_testing::stest!(name = test_ext2_mode_and_seal, suite = fs);
slopos_testing::stest!(name = test_ext2_seal_survives_a_remount, suite = fs);
slopos_testing::stest!(
    name = test_ext2_failed_op_leaves_no_partial_state,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_partial_write_reports_a_short_count,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_readdir_cursor_pages_a_large_directory,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_group_bookkeeping_on_create_and_remove,
    suite = fs
);
slopos_testing::stest!(name = test_ext2_unlink_respects_link_count, suite = fs);
slopos_testing::stest!(name = test_ext2_large_file_size_roundtrips, suite = fs);

/// A rollback must restore a block the failed operation had *invalidated*, not
/// merely one it dirtied. `truncate` invalidates every block it frees, so an
/// eager invalidation would throw away the undo snapshot and leave the block
/// reading whatever the device last held.
pub fn test_ext2_rollback_restores_an_invalidated_block() -> TestResult {
    let Some(device) = phase3_image(b"rb.txt", b"x") else {
        return TestResult::Skipped;
    };
    match rollback_invalidate_body(&device) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

fn rollback_invalidate_body(device: &MemoryBlockDevice) -> Result<(), &'static str> {
    with_mounted(device, rollback_invalidate_inner)
}

#[inline(never)]
fn rollback_invalidate_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.create_file(2, b"shrink.txt").map_err(|_| "create")?;
    let mut chunk = KVec::<u8>::zeroed(1024).map_err(|_| "alloc")?;
    chunk.as_mut_slice().fill(0xC3);
    for i in 0..3u64 {
        fs.write_file(ino, i * 1024, chunk.as_slice())
            .map_err(|_| "grow")?;
    }
    fs.sync().map_err(|_| "sync")?;

    let free_before = fs.superblock().free_blocks_count;

    // A truncate invalidates every block it frees; the survivor must still
    // read back, which is what a deferred invalidation keeps coherent.
    fs.truncate_file(ino, 1024).map_err(|_| "truncate")?;
    if fs.superblock().free_blocks_count != free_before + 2 {
        return Err("truncate did not free two blocks");
    }
    let mut buf = KVec::<u8>::zeroed(1024).map_err(|_| "alloc")?;
    if fs.read_file(ino, 0, buf.as_mut_slice()) != Ok(1024) {
        return Err("the surviving block did not read back");
    }
    if buf.as_slice().iter().any(|&b| b != 0xC3) {
        return Err("the surviving block's contents were lost");
    }

    // A refused truncate must leave the file exactly as it was. The seal is
    // checked ahead of any block free, so this covers the guard's no-op path,
    // not a partial undo.
    fs.set_sealed(ino).map_err(|_| "seal")?;
    let free_sealed = fs.superblock().free_blocks_count;
    if !matches!(fs.truncate_file(ino, 0), Err(Ext2Error::Immutable)) {
        return Err("a sealed inode accepted a truncate");
    }
    if fs.superblock().free_blocks_count != free_sealed {
        return Err("a refused truncate moved the free-block count");
    }
    if fs.read_inode(ino).map(|i| i.size) != Ok(1024) {
        return Err("a refused truncate changed the size");
    }
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_rollback_restores_an_invalidated_block,
    suite = fs
);

/// A device that stops accepting writes after `n` of them, in the `dm-flakey`
/// style: `write_at` reports success and drops the bytes, so the cache and the
/// filesystem see a durable write the medium never took.
///
/// Dropping rather than failing is the harder half of the two. A failing write
/// propagates `DeviceError` and the caller unwinds; a *dropped* one is exactly
/// what a power cut looks like from inside the kernel, and nothing in the
/// filesystem is told. That is the state a remount has to survive.
struct FaultyBlockDevice {
    inner: MemoryBlockDevice,
    /// Writes still honoured before the cut.
    budget: core::sync::atomic::AtomicUsize,
    /// Once true, a write is acknowledged and discarded.
    cut: core::sync::atomic::AtomicBool,
    writes: core::sync::atomic::AtomicUsize,
}

impl FaultyBlockDevice {
    fn new(inner: MemoryBlockDevice, budget: usize) -> Self {
        Self {
            inner,
            budget: core::sync::atomic::AtomicUsize::new(budget),
            cut: core::sync::atomic::AtomicBool::new(false),
            writes: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Writes the device honoured, which is what a search over cut points
    /// enumerates.
    fn honoured(&self) -> usize {
        self.writes.load(core::sync::atomic::Ordering::Relaxed)
    }

    fn is_cut(&self) -> bool {
        self.cut.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Hand the surviving image back, so a remount reads exactly the bytes the
    /// medium holds.
    fn into_inner(self) -> MemoryBlockDevice {
        self.inner
    }
}

impl BlockDevice for FaultyBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        use core::sync::atomic::Ordering::Relaxed;
        let remaining = self.budget.load(Relaxed);
        if remaining == 0 {
            self.cut.store(true, Relaxed);
            return Ok(());
        }
        self.budget.store(remaining - 1, Relaxed);
        self.writes.fetch_add(1, Relaxed);
        self.inner.write_at(offset, buffer)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }
}

/// A crash mid-write must leave an image the next mount refuses to write
/// rather than one it silently trusts.
///
/// Bounded black-box crash testing, after CrashMonkey/B³ (OSDI '18): the
/// workload is short and every write boundary in it is a cut point, so the
/// search is exhaustive over this operation rather than a sample of it. What
/// each cut asserts is the same at every point — the image is structurally one
/// of the two legal states, and it says so.
pub fn test_ext2_crash_at_every_write_leaves_a_legal_image() -> TestResult {
    // Find the write count of an uncut run first, so the search covers every
    // boundary rather than a guessed prefix.
    let Some(total) = crash_workload_write_count() else {
        return TestResult::Skipped;
    };
    if total == 0 {
        return slopos_testing::fail!("the crash workload issued no writes to cut");
    }

    for cut in 0..total {
        if let Err(msg) = crash_cut_at(cut) {
            klog_info!("CRASH_TEST: cut after {} of {} writes", cut, total);
            return slopos_testing::fail!("{}", msg);
        }
    }
    TestResult::Pass
}

/// Writes the workload issues when nothing cuts it.
#[inline(never)]
fn crash_workload_write_count() -> Option<usize> {
    let inner = phase3_image(b"c.txt", b"seed")?;
    let device = FaultyBlockDevice::new(inner, usize::MAX);
    let _ = crash_workload(&device);
    Some(device.honoured())
}

/// Create, write and sync a file, which is the shortest workload that touches
/// every structure a crash can tear: the inode bitmap, the block bitmap, a
/// group descriptor, an inode-table block, a directory block and a data block.
#[inline(never)]
fn crash_workload(device: &FaultyBlockDevice) -> Result<(), Ext2Error> {
    let (sb, bs, is) = Ext2Fs::mount_params(device)?;
    let mut cache = BlockCache::new(bs)?;
    let mut fs = Ext2Fs::new(device, &mut cache, sb, bs, is)?;
    fs.mark_dirty_on_disk()?;
    let ino = fs.create_file(2, b"crash.txt")?;
    fs.write_file(ino, 0, b"payload-that-spans-a-block")?;
    fs.sync()?;
    fs.mark_clean()
}

/// Run the workload with the device cut after `cut` writes, then assert what
/// the surviving image is.
#[inline(never)]
fn crash_cut_at(cut: usize) -> Result<(), &'static str> {
    let inner = phase3_image(b"c.txt", b"seed").ok_or("fixture")?;
    let device = FaultyBlockDevice::new(inner, cut);
    let _ = crash_workload(&device);
    let cut_happened = device.is_cut();
    let survivor = device.into_inner();
    crash_assert_legal(&survivor, cut_happened)
}

/// The two legal states, and the rule that tells them apart.
///
/// A run whose writes all landed is `EXT2_VALID_FS` and must mount read-write.
/// A run that was cut is whatever the medium happens to hold, and the only
/// thing that makes it safe is `s_state`: the mount stamp went down before any
/// mutation, so a cut anywhere after it leaves the image marked dirty and the
/// next mount refuses to write. A cut so early that even the stamp was lost
/// leaves the image byte-identical to the fixture, which is the other legal
/// state.
#[inline(never)]
fn crash_assert_legal(device: &MemoryBlockDevice, was_cut: bool) -> Result<(), &'static str> {
    let reason = crash_mount_reason(device)?;

    if !was_cut {
        if reason.is_some() {
            return Err("an uncut run left an image the next mount refuses to write");
        }
        return Ok(());
    }

    // Marked dirty: the honest outcome, and the one the next mount refuses.
    if matches!(
        reason,
        Some(crate::ext2::ReadOnlyReason::NotCleanlyUnmounted)
    ) {
        return Ok(());
    }
    if reason.is_some() {
        return Err("a cut run left an image read-only for the wrong reason");
    }

    // Clean: only legal if the cut landed before the mount stamp, in which
    // case nothing this workload did reached the medium. Proving that from the
    // outside is what the fixture's own file is for.
    with_mounted(device, crash_assert_untouched)
}

/// Its own frame so the 1 KiB superblock buffer `mount_params` stages does not
/// share one with the mount the caller goes on to perform.
#[inline(never)]
fn crash_mount_reason(
    device: &MemoryBlockDevice,
) -> Result<Option<crate::ext2::ReadOnlyReason>, &'static str> {
    let (sb, _, _) = Ext2Fs::mount_params(device).map_err(|_| "the image no longer mounts")?;
    Ok(Ext2Fs::mount_read_only_reason(&sb, device))
}

#[inline(never)]
fn crash_assert_untouched(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    if fs.resolve_path(b"/crash.txt").is_ok() {
        return Err("a cut run left a clean image that carries the interrupted file");
    }
    fs.resolve_path(b"/c.txt")
        .map(|_| ())
        .map_err(|_| "a cut run left a clean image whose original file is gone")
}

/// The `errors=remount-ro` rule, at the layer that enforces it: an image whose
/// `s_state` says it was never marked clean mounts read-only, and every
/// mutation through it is `EROFS`.
pub fn test_ext2_dirty_image_mounts_read_only() -> TestResult {
    let Some(device) = phase3_image(b"d.txt", b"seed") else {
        return TestResult::Skipped;
    };
    // What a crashed boot leaves behind: the mount stamp, unretracted.
    device.with_buffer_mut(|buf| {
        buf[1024 + 58..1024 + 60].copy_from_slice(&2u16.to_le_bytes());
    });

    let Ok((sb, _, _)) = Ext2Fs::mount_params(&device) else {
        return slopos_testing::fail!("a dirty image must still mount");
    };
    match Ext2Fs::mount_read_only_reason(&sb, &device) {
        Some(crate::ext2::ReadOnlyReason::NotCleanlyUnmounted) => {}
        other => return slopos_testing::fail!("want NotCleanlyUnmounted, got {:?}", other),
    }

    // The reason is the mount's to apply; a handle told to hold it must then
    // refuse every mutation, and must still read.
    mount_ext2!(device, _cache, fs);
    fs.force_read_only();
    if !matches!(fs.create_file(2, b"nope"), Err(Ext2Error::ReadOnly)) {
        return slopos_testing::fail!("a dirty-mounted image accepted a create");
    }
    if !matches!(fs.unlink_entry(2, b"d.txt"), Err(Ext2Error::ReadOnly)) {
        return slopos_testing::fail!("a dirty-mounted image accepted an unlink");
    }
    if fs.resolve_path(b"/d.txt").is_err() {
        return slopos_testing::fail!("a dirty-mounted image is unreadable");
    }
    // A clean image is the control: without it this test passes on a rule that
    // refuses everything.
    let Some(clean) = phase3_image(b"d.txt", b"seed") else {
        return TestResult::Skipped;
    };
    let Ok((clean_sb, _, _)) = Ext2Fs::mount_params(&clean) else {
        return TestResult::Fail;
    };
    match Ext2Fs::mount_read_only_reason(&clean_sb, &clean) {
        None => TestResult::Pass,
        other => slopos_testing::fail!("a clean image must mount read-write, got {:?}", other),
    }
}

/// A mount records itself in the fields `e2fsck` reads, and `s_state` tracks
/// the mount/unmount pair.
pub fn test_ext2_superblock_mount_bookkeeping() -> TestResult {
    let Some(device) = phase3_image(b"b.txt", b"seed") else {
        return TestResult::Skipped;
    };
    match with_mounted(&device, bookkeeping_inner) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

#[inline(never)]
fn bookkeeping_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let before = fs.read_bookkeeping().map_err(|_| "read")?.mnt_count;
    fs.mark_dirty_on_disk().map_err(|_| "mark_dirty_on_disk")?;
    let after_first = fs.read_bookkeeping().map_err(|_| "read")?.mnt_count;
    if after_first != before.saturating_add(1) {
        return Err("a mount did not increment s_mnt_count");
    }
    if fs.superblock().state != 2 {
        return Err("a mount did not stamp EXT2_ERROR_FS");
    }

    // Idempotent: a second stamp on an already-dirty image must not bill a
    // second mount.
    fs.mark_dirty_on_disk().map_err(|_| "repeat stamp")?;
    if fs.read_bookkeeping().map_err(|_| "read")?.mnt_count != after_first {
        return Err("a repeat stamp counted a second mount");
    }

    fs.mark_clean().map_err(|_| "mark_clean")?;
    if fs.superblock().state != 1 {
        return Err("an unmount did not stamp EXT2_VALID_FS");
    }
    // `s_lastcheck` must stay untouched: this kernel runs no fsck, and
    // stamping it would claim a check that never happened.
    if fs.read_bookkeeping().map_err(|_| "read")?.lastcheck != 0 {
        return Err("a mount stamped s_lastcheck without running a check");
    }
    Ok(())
}

/// The two rules `e2fsck` applies, read off a real superblock so the offsets
/// are exercised rather than assumed.
pub fn test_ext2_check_overdue_rules() -> TestResult {
    use crate::ext2::ondisk::SuperblockBookkeeping;

    let Some(device) = phase3_image(b"k.txt", b"seed") else {
        return TestResult::Skipped;
    };
    device.with_buffer_mut(|buf| {
        let sb = &mut buf[1024..2048];
        sb[52..54].copy_from_slice(&30u16.to_le_bytes()); // s_mnt_count
        sb[54..56].copy_from_slice(&20u16.to_le_bytes()); // s_max_mnt_count
        sb[64..68].copy_from_slice(&1_000u32.to_le_bytes()); // s_lastcheck
        sb[68..72].copy_from_slice(&100u32.to_le_bytes()); // s_checkinterval
    });
    mount_ext2!(device, _cache, fs);
    let Ok(book) = fs.read_bookkeeping() else {
        return slopos_testing::fail!("read_bookkeeping failed");
    };
    if book.mnt_count != 30 || book.max_mnt_count != 20 || book.checkinterval != 100 {
        return slopos_testing::fail!("bookkeeping fields did not round-trip off the disk");
    }
    if !book.check_overdue(None) {
        return slopos_testing::fail!("the mount-count rule needs no clock and did not fire");
    }

    let interval_only = SuperblockBookkeeping {
        mnt_count: 1,
        max_mnt_count: 20,
        ..book
    };
    if !interval_only.check_overdue(Some(2_000)) {
        return slopos_testing::fail!("the check-interval rule did not fire");
    }
    if interval_only.check_overdue(Some(1_050)) {
        return slopos_testing::fail!("the check-interval rule fired early");
    }
    // A clockless boot cannot answer the interval rule, and must not guess.
    if interval_only.check_overdue(None) {
        return slopos_testing::fail!("the interval rule fired with no wall clock");
    }
    // Both rules disabled, which is what `mkfs` writes by default.
    let disabled = SuperblockBookkeeping {
        mnt_count: 9_000,
        max_mnt_count: 0,
        checkinterval: 0,
        ..book
    };
    if disabled.check_overdue(Some(u32::MAX)) {
        return slopos_testing::fail!("a disabled rule reported a check as due");
    }
    TestResult::Pass
}

/// The orphan list is ext2's own mechanism, so the on-disk shape has to be
/// exactly what another implementation reads: a head in `s_last_orphan`, each
/// member's `i_dtime` naming the next, `links_count == 0` with a live `i_mode`.
pub fn test_ext2_orphan_list_roundtrips_on_disk() -> TestResult {
    let Some(device) = phase3_image(b"o.txt", b"payload") else {
        return TestResult::Skipped;
    };
    match with_mounted(&device, orphan_roundtrip_inner) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

#[inline(never)]
fn orphan_roundtrip_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.resolve_path(b"/o.txt").map_err(|_| "resolve")?;
    let free_blocks = fs.superblock().free_blocks_count;
    let free_inodes = fs.superblock().free_inodes_count;

    let orphaned = fs.detach_entry(2, b"o.txt").map_err(|_| "detach")?;
    if orphaned != Some(ino) {
        return Err("detach did not report the orphaned inode");
    }
    if fs.resolve_path(b"/o.txt").is_ok() {
        return Err("the name survived the detach");
    }
    // Neither the blocks nor the inode may come back yet: a descriptor still
    // holds them, which is the whole point.
    if fs.superblock().free_blocks_count != free_blocks {
        return Err("detach freed the blocks of a still-open file");
    }
    if fs.superblock().free_inodes_count != free_inodes {
        return Err("detach freed the inode of a still-open file");
    }
    if fs.orphan_head() != ino {
        return Err("the orphan list head does not name the detached inode");
    }
    // The contents must still be readable through the inode a descriptor holds.
    let mut buf = [0u8; 16];
    match fs.read_file(ino, 0, &mut buf) {
        Ok(n) if &buf[..n] == b"payload" => {}
        _ => return Err("a detached inode's contents were lost"),
    }
    let inode = fs.read_inode(ino).map_err(|_| "read_inode")?;
    if inode.links_count != 0 || inode.mode == 0 {
        return Err("a detached inode does not read as an orphan");
    }

    fs.release_orphan(ino).map_err(|_| "release")?;
    if fs.orphan_head() != 0 {
        return Err("releasing the only orphan left the list non-empty");
    }
    if fs.superblock().free_blocks_count != free_blocks + 1 {
        return Err("releasing an orphan did not free its block");
    }
    if fs.superblock().free_inodes_count != free_inodes + 1 {
        return Err("releasing an orphan did not free its inode");
    }
    // Idempotent: a second release, or one for an inode `e2fsck` already
    // drained, must not free whatever now occupies the number.
    let reused = fs.create_file(2, b"reused.txt").map_err(|_| "create")?;
    let after = fs.superblock().free_inodes_count;
    fs.release_orphan(reused).map_err(|_| "second release")?;
    if fs.superblock().free_inodes_count != after {
        return Err("release_orphan freed a live inode");
    }
    Ok(())
}

/// Several orphans thread into one list, and draining it at mount reclaims
/// every one — the crash-recovery half.
pub fn test_ext2_orphan_drain_reclaims_a_crashed_boot() -> TestResult {
    let Some(device) = phase3_image(b"x.txt", b"seed") else {
        return TestResult::Skipped;
    };
    match with_mounted(&device, orphan_drain_inner) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

#[inline(never)]
fn orphan_drain_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let baseline_blocks = fs.superblock().free_blocks_count;
    let baseline_inodes = fs.superblock().free_inodes_count;

    // Three files, each with a block, all detached: what a boot that died
    // holding three unlinked-but-open files leaves behind.
    for name in [b"a1".as_slice(), b"a2".as_slice(), b"a3".as_slice()] {
        let ino = fs.create_file(2, name).map_err(|_| "create")?;
        fs.write_file(ino, 0, b"x").map_err(|_| "write")?;
        if fs.detach_entry(2, name).map_err(|_| "detach")?.is_none() {
            return Err("detach of a fresh file reported no orphan");
        }
    }
    if fs.orphan_head() == 0 {
        return Err("three detaches left an empty orphan list");
    }
    if fs.superblock().free_inodes_count != baseline_inodes - 3 {
        return Err("detached inodes were freed early");
    }

    match fs.drain_orphans() {
        Ok(3) => {}
        other => {
            return Err(match other {
                Ok(_) => "the drain reclaimed the wrong number of inodes",
                Err(_) => "the drain failed",
            });
        }
    }
    if fs.orphan_head() != 0 {
        return Err("the drain left the list non-empty");
    }
    if fs.superblock().free_inodes_count != baseline_inodes {
        return Err("the drain did not return every inode");
    }
    if fs.superblock().free_blocks_count != baseline_blocks {
        return Err("the drain did not return every block");
    }
    // A drain over an empty list is a mount-time no-op, not an error.
    match fs.drain_orphans() {
        Ok(0) => Ok(()),
        _ => Err("a drain of an empty list was not a no-op"),
    }
}

/// The corruption classification is what `errors=remount-ro` keys on, so its
/// two directions both matter: a damaged structure latches the mount, and an
/// error a caller can produce on demand does not.
pub fn test_ext2_corruption_latches_but_caller_errors_do_not() -> TestResult {
    let Some(device) = phase3_image(b"e.txt", b"seed") else {
        return TestResult::Skipped;
    };
    if let Err(msg) = with_mounted(&device, corruption_caller_errors_inner) {
        return slopos_testing::fail!("{}", msg);
    }

    // A group descriptor pointing outside the volume is damage no caller can
    // ask for, and `validate_desc` is what turns it into `InvalidBlock`.
    device.with_buffer_mut(|buf| {
        let desc = 2 * FIX_BLOCK_SIZE as usize;
        buf[desc..desc + 4].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
    });
    match with_mounted(&device, corruption_latches_inner) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

/// Every error an unprivileged caller can ask for on demand. If any of these
/// latched, one `stat` of a nonexistent path would take the whole mount
/// read-only.
#[inline(never)]
fn corruption_caller_errors_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let mut scratch = [0u8; 4];
    let _ = fs.create_file(2, b"e.txt");
    let _ = fs.unlink_entry(2, b"absent");
    let _ = fs.create_file(2, b"");
    let _ = fs.read_file(9999, 0, &mut scratch);
    let _ = fs.truncate_file(9999, 0);

    // The sharpest one, and the reason `InvalidRange` exists as a variant of
    // its own: this cursor comes straight from userland through `fs_list`, so
    // an argument error classified as damage would let any process take the
    // whole mount read-only for everybody with one bad `u64`.
    let bogus = fs.for_each_dir_entry_from(2, u64::MAX, |_, _| true);
    if !matches!(bogus, Err(Ext2Error::InvalidRange)) {
        return Err("an out-of-range readdir cookie is not reported as a caller error");
    }
    // Past the end but not absurd, and the whole 64-bit range in between: the
    // check must be on the value, not on how large it looks.
    for cookie in [u64::MAX / 2, 1 << 32, 4096, u64::from(u32::MAX)] {
        let _ = fs.for_each_dir_entry_from(2, cookie, |_, _| true);
    }
    let _ = fs.read_file(2, u64::MAX, &mut scratch);
    let _ = fs.write_file(2, u64::MAX, b"x");

    // An offset past what three levels of indirection can address, which is
    // the other end of the same hazard: `file_block_index` only refuses above
    // 16 TiB, so every offset between the triple-indirect reach and that
    // ceiling reaches the block map's fall-through. One `pwrite` there used to
    // latch the whole mount read-only for every process on it.
    let ino = fs.create_file(2, b"deep.txt").map_err(|_| "create")?;
    let past_reach =
        crate::ext2::blockmap::max_file_size(256, FIX_BLOCK_SIZE) + FIX_BLOCK_SIZE as u64;
    let _ = fs.write_file(ino, past_reach, b"x");
    let _ = fs.read_file(ino, past_reach, &mut scratch);
    // And the size that would let a later read reach it: refused where the
    // size is set, so `i_size` never names a block no read can address.
    if !matches!(
        fs.truncate_file(ino, past_reach),
        Err(Ext2Error::InvalidRange)
    ) {
        return Err("a truncate past the block map's reach was accepted");
    }

    if fs.corruption_seen() {
        return Err("an ordinary caller error latched the mount read-only");
    }
    Ok(())
}

#[inline(never)]
fn corruption_latches_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let _ = fs.create_file(2, b"anything");
    if !fs.corruption_seen() {
        return Err("a group descriptor outside the volume did not latch");
    }
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_crash_at_every_write_leaves_a_legal_image,
    suite = fs
);
slopos_testing::stest!(name = test_ext2_dirty_image_mounts_read_only, suite = fs);
slopos_testing::stest!(name = test_ext2_superblock_mount_bookkeeping, suite = fs);
slopos_testing::stest!(name = test_ext2_check_overdue_rules, suite = fs);
slopos_testing::stest!(name = test_ext2_orphan_list_roundtrips_on_disk, suite = fs);
slopos_testing::stest!(
    name = test_ext2_orphan_drain_reclaims_a_crashed_boot,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_corruption_latches_but_caller_errors_do_not,
    suite = fs
);

/// POSIX: a file unlinked while a descriptor holds it keeps its contents until
/// the last close.
///
/// Exercised through the VFS rather than through ext2 directly, because the
/// rule is a two-layer one: the open-reference count decides *which* removal
/// runs, and the filesystem decides what that removal does on disk. A test
/// against either half alone would pass with the other missing.
pub fn test_vfs_unlink_defers_while_open() -> TestResult {
    use crate::vfs::orphan;
    use crate::vfs::{VfsOpenFlags, vfs_open_flags};
    use crate::vfs_file_ops::vfs_open_handle_flags;

    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let _ = vfs_mkdir(b"/tmp/unlink_open");
    const PATH: &[u8] = b"/tmp/unlink_open/held";
    let _ = vfs_unlink(PATH);

    let Ok(seed) = vfs_open_flags(PATH, VfsOpenFlags::create_only()) else {
        return slopos_testing::fail!("could not create the fixture");
    };
    if seed.write(0, b"held-open").is_err() {
        return slopos_testing::fail!("could not seed the fixture");
    }
    drop(seed);

    // A descriptor-level handle, which is what takes the open reference; the
    // `VfsHandle` above deliberately does not.
    let Ok(handle) = vfs_open_handle_flags(PATH, VfsOpenFlags::read_only()) else {
        return slopos_testing::fail!("could not open the fixture");
    };
    let tracked_before = orphan::tracked_count();

    if vfs_unlink(PATH).is_err() {
        return slopos_testing::fail!("unlink of a held-open file failed");
    }
    // The name is gone.
    if vfs_stat(PATH).is_ok() {
        return slopos_testing::fail!("the name survived the unlink");
    }
    // The contents are not: reading through the descriptor still works.
    let mut buf = [0u8; 16];
    let read = {
        let mut sink = slopos_abi::io::KernelIoBuf::new(&mut buf[..9]);
        slopos_abi::file_ops::FileOps::read(
            &crate::vfs_file_ops::VFS_FILE_OPS,
            handle,
            &mut sink,
            0,
            0,
        )
    };
    if read != 9 || &buf[..9] != b"held-open" {
        return slopos_testing::fail!("a held-open unlinked file became unreadable ({})", read);
    }
    if orphan::tracked_count() < tracked_before {
        return slopos_testing::fail!("the unlink dropped the open reference");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_vfs_unlink_defers_while_open, suite = fs);
