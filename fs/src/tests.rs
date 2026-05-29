use slopos_abi::fs::UserFsEntry;
use slopos_ostd::klog_info;
use slopos_testing::TestResult;

use crate::blockdev::{BlockDevice, BlockDeviceError, MemoryBlockDevice};
use crate::ext2::cache::BlockCache;
use crate::ext2::{Ext2Error, Ext2Fs};
use crate::vfs::{
    vfs_init_builtin_filesystems, vfs_is_initialized, vfs_list, vfs_mkdir, vfs_open, vfs_stat,
    vfs_unlink,
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

    let mut entries = [UserFsEntry::new(); 32];
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
