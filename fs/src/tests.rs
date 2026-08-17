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

/// Test helper: mount an in-memory image over an owned, stack-local
/// [`BlockCache`] and bind a [`Ext2Fs`] handle. Mirrors how the production VFS
/// bridge borrows a persistent cache, but here the cache is owned by the test
/// frame. `$device` must outlive `$fs`. Returns `TestResult::Fail` early if the
/// superblock is invalid.
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
        let mut $fs = Ext2Fs::new(&$device, &mut $cache, sb, bs, is);
    };
}

pub fn test_vfs_initialized() -> TestResult {
    klog_info!("VFS_TEST: check initialized");
    if !vfs_is_initialized() {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vfs_root_stat() -> TestResult {
    klog_info!("VFS_TEST: root stat");
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
    if vfs_mkdir(b"/vfs_test").is_err() {
        return TestResult::Fail;
    }

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
    let mut entries = [UserFsEntry::new(); 8];
    let count = match vfs_list(b"/vfs_test", &mut entries) {
        Ok(count) => count,
        Err(_) => return TestResult::Fail,
    };

    let mut found = false;
    for entry in entries.iter().take(count) {
        if entry.name_str() == "hello.txt" {
            found = true;
            break;
        }
    }

    if !found {
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Regression: every directory that shows up in `ls /` must be `cd`-able.
///
/// `ls` reports directory entries via `readdir` (which only reads the *parent*
/// directory block), while `cd` resolves the child path and `stat`s the child
/// inode. This test mirrors that exact sequence — list `/`, then `vfs_stat`
/// each listed directory by its full path — so a divergence between "listed"
/// and "resolvable" is caught here instead of only surfacing interactively.
pub fn test_vfs_cd_into_listed_dirs() -> TestResult {
    use slopos_abi::fs::FS_TYPE_DIRECTORY;

    klog_info!("VFS_TEST: cd into every listed directory");

    // 32 × 72 bytes of entries — more than the whole frame budget on its own.
    let Ok(mut entries) = KVec::filled(UserFsEntry::new(), 32) else {
        return TestResult::Fail;
    };
    let count = match vfs_list(b"/", &mut entries) {
        Ok(count) => count,
        Err(_) => return TestResult::Fail,
    };

    let mut dirs_seen = 0u32;
    for entry in entries.iter().take(count) {
        if entry.type_ != FS_TYPE_DIRECTORY {
            continue;
        }
        let name = entry.name_str();
        if name == "." || name == ".." {
            continue;
        }

        // Build "/<name>" exactly as the shell does for an absolute `cd`.
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
        dirs_seen += 1;
    }

    if dirs_seen == 0 {
        // No subdirectories to exercise (e.g. ramfs root): nothing to assert.
        return TestResult::Skipped;
    }
    TestResult::Pass
}

pub fn test_vfs_unlink() -> TestResult {
    klog_info!("VFS_TEST: unlink file");
    if vfs_unlink(b"/vfs_test/hello.txt").is_err() {
        return TestResult::Fail;
    }

    let mut entries = [UserFsEntry::new(); 8];
    let count = match vfs_list(b"/vfs_test", &mut entries) {
        Ok(count) => count,
        Err(_) => return TestResult::Fail,
    };

    for entry in entries.iter().take(count) {
        if entry.name_str() == "hello.txt" {
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_vfs_storage_contention_stress_baseline() -> TestResult {
    if vfs_mkdir(b"/vfs_stress").is_err() {
        return TestResult::Fail;
    }

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

#[inline(never)]
fn write_superblock(sb: &mut [u8], inodes: u32, blocks: u32, inode_size: u16) {
    sb[0..4].copy_from_slice(&inodes.to_le_bytes());
    sb[4..8].copy_from_slice(&blocks.to_le_bytes());
    sb[12..16].copy_from_slice(&8u32.to_le_bytes());
    sb[16..20].copy_from_slice(&8u32.to_le_bytes());
    sb[20..24].copy_from_slice(&1u32.to_le_bytes());
    sb[24..28].copy_from_slice(&0u32.to_le_bytes());
    sb[32..36].copy_from_slice(&blocks.to_le_bytes()); // blocks_per_group == blocks
    sb[40..44].copy_from_slice(&inodes.to_le_bytes()); // inodes_per_group == inodes
    sb[56..58].copy_from_slice(&0xEF53u16.to_le_bytes());
    sb[76..80].copy_from_slice(&1u32.to_le_bytes());
    sb[84..88].copy_from_slice(&11u32.to_le_bytes());
    sb[88..90].copy_from_slice(&inode_size.to_le_bytes());
}

#[inline(never)]
fn write_group_descriptor(desc: &mut [u8]) {
    desc[0..4].copy_from_slice(&3u32.to_le_bytes());
    desc[4..8].copy_from_slice(&4u32.to_le_bytes());
    desc[8..12].copy_from_slice(&5u32.to_le_bytes());
    desc[12..14].copy_from_slice(&8u16.to_le_bytes());
    desc[14..16].copy_from_slice(&8u16.to_le_bytes());
    desc[16..18].copy_from_slice(&1u16.to_le_bytes());
}

#[inline(never)]
fn write_root_inode(inode_table: &mut [u8], root_inode_offset: usize, block_size: u32) {
    inode_table[root_inode_offset..root_inode_offset + 2].copy_from_slice(&0x4000u16.to_le_bytes());
    inode_table[root_inode_offset + 4..root_inode_offset + 8]
        .copy_from_slice(&block_size.to_le_bytes());
    inode_table[root_inode_offset + 28..root_inode_offset + 32]
        .copy_from_slice(&2u32.to_le_bytes());
    inode_table[root_inode_offset + 40..root_inode_offset + 44]
        .copy_from_slice(&6u32.to_le_bytes());
}

#[inline(never)]
fn write_file_inode(
    inode_table: &mut [u8],
    file_inode_offset: usize,
    data_len: u32,
    file_block: u32,
) {
    inode_table[file_inode_offset..file_inode_offset + 2].copy_from_slice(&0x8000u16.to_le_bytes());
    inode_table[file_inode_offset + 4..file_inode_offset + 8]
        .copy_from_slice(&data_len.to_le_bytes());
    inode_table[file_inode_offset + 28..file_inode_offset + 32]
        .copy_from_slice(&1u32.to_le_bytes());
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
    let block_size = 1024u32;
    let inode_size = 128u16;
    let blocks_per_group = spec.blocks;
    let inodes_per_group = spec.inodes;
    let size_bytes = (spec.blocks as usize).saturating_mul(block_size as usize);
    let device = MemoryBlockDevice::allocate(size_bytes)?;

    let _ = (blocks_per_group, inodes_per_group); // values inlined into helpers
    device.with_buffer_mut(|buf| {
        let sb_offset = 1024usize;
        write_superblock(
            &mut buf[sb_offset..sb_offset + 1024],
            spec.inodes,
            spec.blocks,
            inode_size,
        );

        let desc_offset = 2 * block_size as usize;
        write_group_descriptor(&mut buf[desc_offset..desc_offset + 32]);

        let inode_table_offset = 5 * block_size as usize;
        let root_inode_offset = 128;
        write_root_inode(
            &mut buf[inode_table_offset..inode_table_offset + 1024],
            root_inode_offset,
            block_size,
        );

        let file_inode_number = 3u32;
        if let (Some(name), Some(data)) = (spec.file_name, spec.file_data) {
            let file_inode_offset = root_inode_offset + inode_size as usize;
            write_file_inode(
                &mut buf[inode_table_offset..inode_table_offset + 1024],
                file_inode_offset,
                data.len() as u32,
                spec.file_block,
            );

            if spec.file_block < spec.blocks {
                let data_offset = spec.file_block as usize * block_size as usize;
                let data_block = &mut buf[data_offset..data_offset + 1024];
                data_block[..data.len()].copy_from_slice(data);
            }

            let dir_offset = 6 * block_size as usize;
            write_dir_with_file(
                &mut buf[dir_offset..dir_offset + 1024],
                block_size as usize,
                file_inode_number,
                name,
            );
        } else {
            let dir_offset = 6 * block_size as usize;
            write_dir_minimal(&mut buf[dir_offset..dir_offset + 1024], block_size as usize);
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
    let dir_offset = 6 * 1024usize;
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
    let mut fs = Ext2Fs::new(&failing, &mut cache, sb, bs, is);

    // With write-back caching, the metadata mutations from `create_directory`
    // land in the cache and the operation itself succeeds — the device error
    // surfaces only when those dirty blocks are written back at `sync`.
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
        file_block: 7,
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

// Regression + durability: `sync` must persist a write to the backing device
// such that a brand-new handle with its OWN cache reads it back. Overwrites a
// pre-existing file inode (no allocation) so the test isolates cache-writeback,
// not the on-disk layout of the hand-built minimal image.
pub fn test_ext2_write_persists_across_handles() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"persist.txt"),
        file_data: Some(b"old"),
        file_block: 7,
    };
    let Some(device) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };

    let payload = b"persisted-bytes";
    {
        mount_ext2!(device, _c1, fs);
        let ino = match fs.resolve_path(b"/persist.txt") {
            Ok(ino) => ino,
            Err(_) => return TestResult::Fail,
        };
        if fs.write_file(ino, 0, payload).is_err() {
            return TestResult::Fail;
        }
        // Durability barrier: ordered writeback to the device.
        if fs.sync().is_err() {
            return TestResult::Fail;
        }
    }

    // Fresh handle, fresh cache: must see the persisted bytes on disk.
    mount_ext2!(device, _c2, fs2);
    let ino = match fs2.resolve_path(b"/persist.txt") {
        Ok(ino) => ino,
        Err(_) => return TestResult::Fail,
    };
    let mut buf = [0u8; 32];
    let read_len = match fs2.read_file(ino, 0, &mut buf) {
        Ok(len) => len,
        Err(_) => return TestResult::Fail,
    };
    if read_len != payload.len() || &buf[..read_len] != payload {
        return TestResult::Fail;
    }
    TestResult::Pass
}

// Write-back semantics: a write only DIRTIES the cache; the device is not
// touched until `sync`. We prove this by writing through one handle WITHOUT
// syncing, then confirming a fresh handle (own cache) does NOT see the change.
pub fn test_ext2_writeback_is_deferred_until_sync() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"defer.txt"),
        file_data: Some(b"old"),
        file_block: 7,
    };
    let Some(device) = build_ext2_image(spec) else {
        return TestResult::Pass;
    };

    {
        mount_ext2!(device, _c1, fs);
        let ino = match fs.resolve_path(b"/defer.txt") {
            Ok(ino) => ino,
            Err(_) => return TestResult::Fail,
        };
        if fs.write_file(ino, 0, b"NEW").is_err() {
            return TestResult::Fail;
        }
        // The write must be visible as dirty in the cache...
        if fs.dirty_count() == 0 {
            return TestResult::Fail;
        }
        // ...but we deliberately drop the handle WITHOUT syncing. Because the
        // cache is stack-local to this scope it is discarded — modelling a
        // crash before writeback. (In production the cache is persistent, so
        // the data would survive in-memory; here we test the device path.)
    }

    // Fresh handle reading the device must still see the OLD bytes.
    mount_ext2!(device, _c2, fs2);
    let ino = match fs2.resolve_path(b"/defer.txt") {
        Ok(ino) => ino,
        Err(_) => return TestResult::Fail,
    };
    let mut buf = [0u8; 8];
    let n = match fs2.read_file(ino, 0, &mut buf) {
        Ok(n) => n,
        Err(_) => return TestResult::Fail,
    };
    if &buf[..n] != b"old" {
        return TestResult::Fail;
    }
    TestResult::Pass
}

// Cross-call cache reuse: two operations on the SAME persistent cache see each
// other's writes immediately (write-back coherency), and a clean read leaves
// no dirty blocks.
pub fn test_ext2_cache_reuse_within_handle() -> TestResult {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"reuse.txt"),
        file_data: Some(b"old"),
        file_block: 7,
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
    // Read back through the SAME cache before any sync — must observe the write.
    let mut buf = [0u8; 16];
    let n = match fs.read_file(ino, 0, &mut buf) {
        Ok(n) => n,
        Err(_) => return TestResult::Fail,
    };
    if &buf[..n.min(payload.len())] != payload {
        return TestResult::Fail;
    }
    // After sync, the cache is clean.
    if fs.sync().is_err() {
        return TestResult::Fail;
    }
    if fs.dirty_count() != 0 {
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

fn ext2_tests_init() -> bool {
    if let Err(_) = vfs_init_builtin_filesystems() {
        klog_info!("VFS_TEST: failed to initialize VFS");
        return false;
    }
    true
}

// VFS initialisation is performed once on first test invocation; lex sort
// of test names guarantees `test_ext2_aaa_init` runs before any other
// `test_ext2_*` / `test_vfs_*` entry in this file.
static EXT2_VFS_READY: slopos_ostd::sync::StateFlag = slopos_ostd::sync::StateFlag::new();

fn ensure_ext2_vfs_ready() -> bool {
    if EXT2_VFS_READY.is_active() {
        return true;
    }
    if !ext2_tests_init() {
        return false;
    }
    EXT2_VFS_READY.set_active();
    true
}

fn test_ext2_aaa_init() -> TestResult {
    if ensure_ext2_vfs_ready() {
        TestResult::Pass
    } else {
        TestResult::Skipped
    }
}

// ---------------------------------------------------------------------------
// Block-integrity (verity) tests — see fs/src/verity.rs
// ---------------------------------------------------------------------------

/// Build an in-memory image with a verity trailer (`n` blocks of `bs` bytes)
/// and wrap it in a `VerifiedBlockDevice`. If `corrupt_block` is set, that
/// block's data is flipped AFTER its hash is recorded, so the stored hash no
/// longer matches the on-disk bytes (simulating corruption/drift).
fn build_verity_device(
    bs: usize,
    n: usize,
    corrupt_block: Option<usize>,
) -> Option<slopos_ostd::KBox<dyn BlockDevice + Send + Sync>> {
    use crate::verity::crc32;
    let total = n * bs + n * 4 + 32;
    let dev = MemoryBlockDevice::allocate(total)?;
    dev.with_buffer_mut(|img| {
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
    });
    let boxed: slopos_ostd::KBox<dyn BlockDevice + Send + Sync> =
        slopos_ostd::KBox::try_new(dev).ok()?;
    Some(crate::verity::build_verified(boxed))
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

/// A clean (untampered) image verifies on read.
fn test_verity_clean_read_passes() -> TestResult {
    let bs = 512usize;
    let Some(dev) = build_verity_device(bs, 4, None) else {
        return TestResult::Fail;
    };
    let mut buf = [0u8; 512];
    // Every block reads back successfully through the verifier.
    for b in 0..4u64 {
        if dev.read_at(b * bs as u64, &mut buf).is_err() {
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// A corrupted block fails the read loudly with `IntegrityFailure` instead of
/// returning bad bytes — the structural defense against the io_capture class.
fn test_verity_corruption_detected() -> TestResult {
    let bs = 512usize;
    let Some(dev) = build_verity_device(bs, 4, Some(2)) else {
        return TestResult::Fail;
    };
    let mut buf = [0u8; 512];
    // Block 0 and 1 are clean → ok; block 2 is corrupted → IntegrityFailure.
    if dev.read_at(0, &mut buf).is_err() || dev.read_at(bs as u64, &mut buf).is_err() {
        return TestResult::Fail;
    }
    match dev.read_at(2 * bs as u64, &mut buf) {
        Err(BlockDeviceError::IntegrityFailure) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

/// After a block is written, it is "re-blessed" (the FS owns its mutable
/// blocks) and no longer verified — so a subsequent read of a corrupted-but-
/// written block does not fail.
fn test_verity_written_block_skips_verification() -> TestResult {
    let bs = 512usize;
    let Some(dev) = build_verity_device(bs, 4, Some(2)) else {
        return TestResult::Fail;
    };
    // Write block 2 (marks it written / re-blesses it).
    let payload = [0xABu8; 512];
    if dev.write_at(2 * bs as u64, &payload).is_err() {
        return TestResult::Fail;
    }
    // Now reading block 2 must NOT fail integrity (it's owned/mutable).
    let mut buf = [0u8; 512];
    if dev.read_at(2 * bs as u64, &mut buf).is_err() {
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Only blocks FULLY contained in a read are verified. A sub-block read of a
/// corrupted block must NOT fail (it never fully covers the block) — this is
/// the property the sub-block superblock read relies on.
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
/// block mid-buffer (verifies the loop iterates over every covered block).
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

/// A registered process with an empty descriptor table, released on drop.
///
/// A table lives in its process's own registry slot, so there is no table to
/// be had without a process. These tests used to mint one against a made-up
/// pid; that is precisely how a table ends up bound to an id nothing owns,
/// which is the confusion the re-key removes.
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

    /// The descriptor table this process owns.
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

    // The writable ext2 root is mounted by `test_ext2_aaa_init` (lex-first);
    // this test runs after it. Use a private directory so it never collides
    // with `test_vfs_*`, whose `mkdir` asserts first-creation.
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

    // Clone fd 2 → install a shared alias at fd 7.
    let outcome = (|| {
        let cloned = fileio_clone_file_ref(table, 2)?;
        if fileio_install_file_ref_at(table, 7, cloned, false) != 7 {
            return None;
        }
        // Move fd 2 → fd 9; fd 2 is emptied by the take.
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
slopos_testing::stest!(name = test_verity_written_block_skips_verification);
slopos_testing::stest!(name = test_verity_partial_read_skips);
slopos_testing::stest!(name = test_verity_multiblock_span_detects);

slopos_testing::stest!(name = test_ext2_aaa_init);
slopos_testing::stest!(name = test_vfs_initialized);
slopos_testing::stest!(name = test_vfs_root_stat);
slopos_testing::stest!(name = test_vfs_file_roundtrip);
slopos_testing::stest!(name = test_vfs_list);
slopos_testing::stest!(name = test_vfs_cd_into_listed_dirs);
slopos_testing::stest!(name = test_vfs_unlink);
slopos_testing::stest!(name = test_vfs_storage_contention_stress_baseline);
slopos_testing::stest!(name = test_ext2_invalid_superblock_magic);
slopos_testing::stest!(name = test_ext2_unsupported_block_size);
slopos_testing::stest!(name = test_ext2_zero_inodes_per_group_rejected);
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
slopos_testing::stest!(name = test_ext2_path_resolution_not_found);
slopos_testing::stest!(name = test_ext2_remove_path_not_file);

// --- initramfs cpio (newc) parser -----------------------------------------

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

/// A process may hold `FILEIO_MAX_OPEN_FILES` descriptors and no more.
///
/// The bound is the length of the table the process was given, so this also
/// pins that the heap-backed array is built at its full size rather than
/// growing under whatever lock happens to be held when a descriptor arrives.
pub fn test_fileio_open_file_limit() -> TestResult {
    use crate::fileio::{FILEIO_MAX_OPEN_FILES, file_open_for_process};
    use slopos_abi::fs::O_RDONLY;

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

/// The account row is what refuses, and the errno is the one the call site
/// already returned for a full table.
///
/// Drives a process to its `FdSlot` ceiling with the ceiling set *below* the
/// table length, so the refusal provably comes from the quota rather than from
/// running out of array.
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

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(account, ResourceKind::FdSlot, CEILING);

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
    if s.used != CEILING {
        return slopos_testing::fail!("used={} after filling to {}", s.used, CEILING);
    }
    if s.denials == 0 {
        return slopos_testing::fail!("a refusal nobody counted is a silent denial");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_quota_fdslot_drive_to_full);

/// A refused open refunds exactly once, and closing gives every unit back.
///
/// The failure this exists to catch is the one the token design is built
/// around: a mint that fails after the debit, leaving the row permanently
/// short and the process permanently denied. `used` returning to its exact
/// pre-test value is the observable form of "refunded exactly once" — an
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

    // Refuse repeatedly: a refusal that under-refunds shows up as a row that
    // drifts, which one refusal could hide.
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

/// One process at its ceiling does not deny another.
///
/// The property the whole subsystem exists for, and the one a global table
/// bound cannot provide: with a shared array, the first process to fill it
/// denies everyone. Here the exhausted process is refused and its neighbour —
/// whose account is a sibling row — opens normally.
pub fn test_quota_fdslot_cross_process_isolation() -> TestResult {
    use crate::fileio::file_open_for_process;
    use slopos_abi::fs::O_RDONLY;
    use slopos_abi::quota::{QuotaMode, ResourceKind};
    use slopos_ostd::process::quota::{quota_mode, set_limit, set_quota_mode};

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

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(greedy_table.account(), ResourceKind::FdSlot, CEILING);

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

/// The ledger agrees with itself immediately after an exhaustion run.
///
/// Named `zzz` so it sorts after the three tests above: the audit is only
/// interesting on a ledger that has just been driven to its ceilings and
/// unwound, which is what those leave behind. This is the runtime form of the
/// equality invariant and the only mechanism that can see a forgotten or
/// unwinder-skipped charge — the type system guarantees the token is unique,
/// never that the number matches reality.
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
    let _ = vfs_mkdir(b"/fileio_test");
    let handle = vfs_open(path, true).ok()?;
    handle.write(0, b"x").ok()?;
    Some(path)
}

/// A sealed inode refuses every mutation, and an unsealed one in the same
/// mount refuses none of them.
///
/// Both files live in `/tmp`, one filesystem instance, so this cannot pass by
/// accident on a per-filesystem flag.
pub fn test_sealed_inode_refuses_mutation() -> TestResult {
    use crate::vfs::{VfsError, VfsOpenFlags, vfs_open_flags, vfs_set_sealed};

    const SEALED: &[u8] = b"/tmp/seal_sealed";
    const PLAIN: &[u8] = b"/tmp/seal_plain";
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

    // Every mutator refuses on the sealed inode.
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
    // The open that would have produced a writable handle never returns one.
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

    // The unsealed neighbour in the same mount accepts all of it.
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

    match vfs_set_sealed(b"/dev/null") {
        Err(VfsError::NotSupported) => TestResult::Pass,
        other => slopos_testing::fail!("devfs answered {:?}, want NotSupported", other),
    }
}

slopos_testing::stest!(name = test_sealed_inode_refuses_mutation);
slopos_testing::stest!(name = test_set_sealed_defaults_closed);

/// A process at its `ObjectRow` ceiling is refused with `ENFILE`, and every
/// row it held comes back on close.
///
/// `ObjectRow` counts registry rows and their backing objects — the vnode, the
/// socket endpoint, the pipe — which are the system-wide tables an
/// unprivileged process could previously fill on its own. The errno is
/// deliberately `ENFILE` ("too many open files in *system*") rather than
/// `EMFILE`: what has been exhausted is an object table, not this process's
/// descriptor numbers, and userland backs off differently for each.
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
    set_limit(account, ResourceKind::FdSlot, baseline + CEILING + 64);

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

/// One process at its `ObjectRow` ceiling does not deny another.
pub fn test_quota_objectrow_cross_process_isolation() -> TestResult {
    use crate::fileio::file_open_for_process;
    use slopos_abi::fs::O_RDONLY;
    use slopos_abi::quota::{QuotaMode, ResourceKind};
    use slopos_ostd::process::quota::{quota_mode, set_limit, set_quota_mode};

    const CEILING: u32 = 3;

    let Some(path) = scratch_file(b"/fileio_test/quota_obj_iso.txt") else {
        return TestResult::Fail;
    };
    let (Some(greedy), Some(neighbour)) = (ScratchProcess::new(), ScratchProcess::new()) else {
        return TestResult::Fail;
    };
    let greedy_table = greedy.table();
    let neighbour_table = neighbour.table();

    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(greedy_table.account(), ResourceKind::ObjectRow, CEILING);

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

/// Reclaiming from the block cache leaves every surviving block findable.
///
/// The index maps block number to slot, and reclaim moves slots — so a repair
/// that missed an entry would leave a live cached block unreachable under its
/// own number, and the next read of it would fault in a second copy while the
/// first sat pinned and invisible. Worse, a *stale* entry would hand back
/// another block's bytes under the number that was asked for.
///
/// Written against the cache directly rather than through the VFS: the failure
/// is in the index, and a read through the VFS would silently re-fault the
/// block and report success.
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

    // Re-dirty the *last* block, so the scan (which walks from the end) has to
    // skip it and the entries it does release come from the middle. That is
    // what makes `swap_remove` actually move a surviving entry into a lower
    // slot -- without it every removal is a pop, nothing moves, and the index
    // repair this test exists for is never exercised.
    {
        let Ok(mut tail) = cache.get(BlockNum(BLOCKS), &device) else {
            return TestResult::Fail;
        };
        tail.data_mut()[0] = BLOCKS as u8;
    }

    // Reclaim nearly everything. The cache pre-allocates `CACHE_ENTRIES`
    // slots, so a small request is satisfied entirely from the empty trailing
    // ones and never disturbs a live block -- the case that must work is the
    // one where the scan reaches the populated slots and has to move them.
    let before = cache.reclaimable();
    slopos_testing::assert_test!(before > 0, "a flushed cache reported nothing reclaimable");
    let released = cache.shrink_clean(before);
    slopos_testing::assert_test!(released > 0, "shrink_clean released nothing");

    // Every block must still read back its own stamp: a survivor through the
    // index, an evicted one through a re-read from the device.
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
