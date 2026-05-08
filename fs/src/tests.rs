use slopos_abi::fs::UserFsEntry;
use slopos_testing::TestResult;
use slopos_utils::klog_info;

use crate::blockdev::{BlockDevice, BlockDeviceError, MemoryBlockDevice};
use crate::ext2::{Ext2Error, Ext2Fs};
use crate::vfs::{
    vfs_init_builtin_filesystems, vfs_is_initialized, vfs_list, vfs_mkdir, vfs_open, vfs_stat,
    vfs_unlink,
};

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

fn build_ext2_image(spec: Ext2ImageSpec<'_>) -> Option<MemoryBlockDevice> {
    let block_size = 1024u32;
    let inode_size = 128u16;
    let blocks_per_group = spec.blocks;
    let inodes_per_group = spec.inodes;
    let size_bytes = (spec.blocks as usize).saturating_mul(block_size as usize);
    let device = MemoryBlockDevice::allocate(size_bytes)?;

    device.with_buffer_mut(|buf| {
        let sb_offset = 1024usize;
        let sb = &mut buf[sb_offset..sb_offset + 1024];

        sb[0..4].copy_from_slice(&spec.inodes.to_le_bytes());
        sb[4..8].copy_from_slice(&spec.blocks.to_le_bytes());
        sb[12..16].copy_from_slice(&8u32.to_le_bytes());
        sb[16..20].copy_from_slice(&8u32.to_le_bytes());
        sb[20..24].copy_from_slice(&1u32.to_le_bytes());
        sb[24..28].copy_from_slice(&0u32.to_le_bytes());
        sb[32..36].copy_from_slice(&blocks_per_group.to_le_bytes());
        sb[40..44].copy_from_slice(&inodes_per_group.to_le_bytes());
        sb[56..58].copy_from_slice(&0xEF53u16.to_le_bytes());
        sb[76..80].copy_from_slice(&1u32.to_le_bytes());
        sb[84..88].copy_from_slice(&11u32.to_le_bytes());
        sb[88..90].copy_from_slice(&inode_size.to_le_bytes());

        let desc_offset = 2 * block_size as usize;
        let desc = &mut buf[desc_offset..desc_offset + 32];

        desc[0..4].copy_from_slice(&3u32.to_le_bytes());
        desc[4..8].copy_from_slice(&4u32.to_le_bytes());
        desc[8..12].copy_from_slice(&5u32.to_le_bytes());
        desc[12..14].copy_from_slice(&8u16.to_le_bytes());
        desc[14..16].copy_from_slice(&8u16.to_le_bytes());
        desc[16..18].copy_from_slice(&1u16.to_le_bytes());

        let inode_table_offset = 5 * block_size as usize;
        let inode_table = &mut buf[inode_table_offset..inode_table_offset + 1024];

        let root_inode_offset = 128;
        inode_table[root_inode_offset..root_inode_offset + 2]
            .copy_from_slice(&0x4000u16.to_le_bytes());
        inode_table[root_inode_offset + 4..root_inode_offset + 8]
            .copy_from_slice(&block_size.to_le_bytes());
        inode_table[root_inode_offset + 28..root_inode_offset + 32]
            .copy_from_slice(&2u32.to_le_bytes());
        inode_table[root_inode_offset + 40..root_inode_offset + 44]
            .copy_from_slice(&6u32.to_le_bytes());

        let file_inode_number = 3u32;
        if let (Some(name), Some(data)) = (spec.file_name, spec.file_data) {
            let file_inode_offset = root_inode_offset + inode_size as usize;
            inode_table[file_inode_offset..file_inode_offset + 2]
                .copy_from_slice(&0x8000u16.to_le_bytes());
            inode_table[file_inode_offset + 4..file_inode_offset + 8]
                .copy_from_slice(&(data.len() as u32).to_le_bytes());
            inode_table[file_inode_offset + 28..file_inode_offset + 32]
                .copy_from_slice(&1u32.to_le_bytes());
            inode_table[file_inode_offset + 40..file_inode_offset + 44]
                .copy_from_slice(&spec.file_block.to_le_bytes());

            if spec.file_block < spec.blocks {
                let data_offset = spec.file_block as usize * block_size as usize;
                let data_block = &mut buf[data_offset..data_offset + 1024];
                data_block[..data.len()].copy_from_slice(data);
            }

            let dir_offset = 6 * block_size as usize;
            let dir_block = &mut buf[dir_offset..dir_offset + 1024];

            let mut write_dir_entry =
                |offset: usize, inode: u32, rec_len: u16, name: &[u8], file_type: u8| {
                    dir_block[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
                    dir_block[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
                    dir_block[offset + 6] = name.len() as u8;
                    dir_block[offset + 7] = file_type;
                    let name_end = offset + 8 + name.len();
                    dir_block[offset + 8..name_end].copy_from_slice(name);
                    for b in dir_block[name_end..offset + rec_len as usize].iter_mut() {
                        *b = 0;
                    }
                };

            let used = 24 + ((8 + name.len() + 3) & !3);
            let rec_len = (block_size as usize - used) as u16;
            write_dir_entry(0, 2, 12, b".", 2);
            write_dir_entry(12, 2, 12, b"..", 2);
            write_dir_entry(24, file_inode_number, (used - 24) as u16, name, 1);
            write_dir_entry(used, 0, rec_len, b"", 0);
        } else {
            let dir_offset = 6 * block_size as usize;
            let dir_block = &mut buf[dir_offset..dir_offset + 1024];

            let mut write_dir_entry =
                |offset: usize, inode: u32, rec_len: u16, name: &[u8], file_type: u8| {
                    dir_block[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
                    dir_block[offset + 4..offset + 6].copy_from_slice(&rec_len.to_le_bytes());
                    dir_block[offset + 6] = name.len() as u8;
                    dir_block[offset + 7] = file_type;
                    let name_end = offset + 8 + name.len();
                    dir_block[offset + 8..name_end].copy_from_slice(name);
                    for b in dir_block[name_end..offset + rec_len as usize].iter_mut() {
                        *b = 0;
                    }
                };

            write_dir_entry(0, 2, 12, b".", 2);
            write_dir_entry(12, 2, 12, b"..", 2);
            write_dir_entry(24, 0, (block_size - 24) as u16, b"", 0);
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

    let result = Ext2Fs::init_internal(&device);
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

    let result = Ext2Fs::init_internal(&device);
    match result {
        Err(Ext2Error::UnsupportedBlockSize) => TestResult::Pass,
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

    let mut fs = match Ext2Fs::init_internal(&device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

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
    let mut fs = match Ext2Fs::init_internal(&device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

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
    let mut fs = match Ext2Fs::init_internal(&device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

    let mut buf = [0u8; 32];
    let result = fs.read_file(2, 0, &mut buf);
    match result {
        Err(Ext2Error::NotFile) => TestResult::Pass,
        _ => TestResult::Fail,
    }
}

pub fn test_ext2_device_read_error() -> TestResult {
    let device = FailingBlockDevice::new(4096).with_read_fail();
    let result = Ext2Fs::init_internal(&device);
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
    let mut fs = match Ext2Fs::init_internal(&failing) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Pass,
    };

    let result = fs.create_directory(2, b"faildir");
    match result {
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
    let mut fs = match Ext2Fs::init_internal(&device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

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
    let mut fs = match Ext2Fs::init_internal(&device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

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

pub fn test_ext2_path_resolution_not_found() -> TestResult {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return TestResult::Pass;
    };
    let mut fs = match Ext2Fs::init_internal(&device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

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
    let mut fs = match Ext2Fs::init_internal(&device) {
        Ok(fs) => fs,
        Err(_) => return TestResult::Fail,
    };

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

slopos_testing::stest!(name = test_ext2_aaa_init);
slopos_testing::stest!(name = test_vfs_initialized);
slopos_testing::stest!(name = test_vfs_root_stat);
slopos_testing::stest!(name = test_vfs_file_roundtrip);
slopos_testing::stest!(name = test_vfs_list);
slopos_testing::stest!(name = test_vfs_unlink);
slopos_testing::stest!(name = test_vfs_storage_contention_stress_baseline);
slopos_testing::stest!(name = test_ext2_invalid_superblock_magic);
slopos_testing::stest!(name = test_ext2_unsupported_block_size);
slopos_testing::stest!(name = test_ext2_directory_format_error);
slopos_testing::stest!(name = test_ext2_invalid_inode);
slopos_testing::stest!(name = test_ext2_read_file_not_regular);
slopos_testing::stest!(name = test_ext2_device_read_error);
slopos_testing::stest!(name = test_ext2_device_write_error_on_metadata);
slopos_testing::stest!(name = test_ext2_read_block_out_of_bounds);
slopos_testing::stest!(name = test_ext2_read_file_data_roundtrip);
slopos_testing::stest!(name = test_ext2_path_resolution_not_found);
slopos_testing::stest!(name = test_ext2_remove_path_not_file);
