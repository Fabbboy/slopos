use core::ffi::c_int;
use core::ptr;

use slopos_abi::fs::UserFsEntry;
use slopos_lib::{klog_info, wl_currency};

use crate::blockdev::{BlockDevice, BlockDeviceError, MemoryBlockDevice};
use crate::ext2::{Ext2Error, Ext2Fs};
use crate::vfs::{
    vfs_init_builtin_filesystems, vfs_is_initialized, vfs_list, vfs_mkdir, vfs_open, vfs_stat,
    vfs_unlink,
};

fn test_vfs_initialized() -> c_int {
    klog_info!("VFS_TEST: check initialized");
    if !vfs_is_initialized() {
        return -1;
    }
    0
}

fn test_vfs_root_stat() -> c_int {
    klog_info!("VFS_TEST: root stat");
    let (kind, _size) = match vfs_stat(b"/") {
        Ok(stat) => stat,
        Err(_) => return -1,
    };
    if kind != 1 {
        return -1;
    }
    0
}

fn test_vfs_file_roundtrip() -> c_int {
    klog_info!("VFS_TEST: file roundtrip");
    if vfs_mkdir(b"/vfs_test").is_err() {
        return -1;
    }

    let handle = match vfs_open(b"/vfs_test/hello.txt", true) {
        Ok(h) => h,
        Err(_) => return -1,
    };

    let content = b"hello vfs";
    if handle.write(0, content).is_err() {
        return -1;
    }

    let mut buf = [0u8; 32];
    let read_len = match handle.read(0, &mut buf) {
        Ok(len) => len,
        Err(_) => return -1,
    };

    if read_len != content.len() || &buf[..content.len()] != content {
        return -1;
    }
    0
}

fn test_vfs_list() -> c_int {
    klog_info!("VFS_TEST: list directory");
    let mut entries = [UserFsEntry::new(); 8];
    let count = match vfs_list(b"/vfs_test", &mut entries) {
        Ok(count) => count,
        Err(_) => return -1,
    };

    let mut found = false;
    for entry in entries.iter().take(count) {
        if entry.name_str() == "hello.txt" {
            found = true;
            break;
        }
    }

    if !found {
        return -1;
    }
    0
}

fn test_vfs_unlink() -> c_int {
    klog_info!("VFS_TEST: unlink file");
    if vfs_unlink(b"/vfs_test/hello.txt").is_err() {
        return -1;
    }

    let mut entries = [UserFsEntry::new(); 8];
    let count = match vfs_list(b"/vfs_test", &mut entries) {
        Ok(count) => count,
        Err(_) => return -1,
    };

    for entry in entries.iter().take(count) {
        if entry.name_str() == "hello.txt" {
            return -1;
        }
    }
    0
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

    fn write_at(&mut self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
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

    fn write_at(&mut self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
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

    unsafe {
        ptr::write_bytes(device.as_mut_ptr(), 0, size_bytes);
    }

    let sb_offset = 1024usize;
    let sb = unsafe { core::slice::from_raw_parts_mut(device.as_mut_ptr().add(sb_offset), 1024) };

    fn write_u32(buf: &mut [u8], offset: usize, value: u32) {
        buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn write_u16(buf: &mut [u8], offset: usize, value: u16) {
        buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    write_u32(sb, 0, spec.inodes);
    write_u32(sb, 4, spec.blocks);
    write_u32(sb, 12, 8);
    write_u32(sb, 16, 8);
    write_u32(sb, 20, 1);
    write_u32(sb, 24, 0);
    write_u32(sb, 32, blocks_per_group);
    write_u32(sb, 40, inodes_per_group);
    write_u16(sb, 56, 0xEF53);
    write_u32(sb, 76, 1);
    write_u32(sb, 84, 11);
    write_u16(sb, 88, inode_size);

    let desc_offset = 2 * block_size as usize;
    let desc = unsafe { core::slice::from_raw_parts_mut(device.as_mut_ptr().add(desc_offset), 32) };

    write_u32(desc, 0, 3);
    write_u32(desc, 4, 4);
    write_u32(desc, 8, 5);
    write_u16(desc, 12, 8);
    write_u16(desc, 14, 8);
    write_u16(desc, 16, 1);

    let inode_table_offset = 5 * block_size as usize;
    let inode_table = unsafe {
        core::slice::from_raw_parts_mut(device.as_mut_ptr().add(inode_table_offset), 1024)
    };

    let root_inode_offset = 128;
    inode_table[root_inode_offset..root_inode_offset + 2].copy_from_slice(&0x4000u16.to_le_bytes());
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
            let data_block = unsafe {
                core::slice::from_raw_parts_mut(device.as_mut_ptr().add(data_offset), 1024)
            };
            data_block[..data.len()].copy_from_slice(data);
        }

        let dir_offset = 6 * block_size as usize;
        let dir_block =
            unsafe { core::slice::from_raw_parts_mut(device.as_mut_ptr().add(dir_offset), 1024) };

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
        let dir_block =
            unsafe { core::slice::from_raw_parts_mut(device.as_mut_ptr().add(dir_offset), 1024) };

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

fn test_ext2_invalid_superblock_magic() -> c_int {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return 0;
    };
    let sb_offset = 1024usize;
    unsafe {
        let sb = core::slice::from_raw_parts_mut(device.as_mut_ptr().add(sb_offset), 1024);
        sb[56] = 0;
        sb[57] = 0;
    }

    let result = Ext2Fs::init_internal(&mut device);
    match result {
        Err(Ext2Error::InvalidSuperblock) => 0,
        _ => -1,
    }
}

fn test_ext2_unsupported_block_size() -> c_int {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return 0;
    };
    let sb_offset = 1024usize;
    unsafe {
        let sb = core::slice::from_raw_parts_mut(device.as_mut_ptr().add(sb_offset), 1024);
        sb[24..28].copy_from_slice(&8u32.to_le_bytes());
    }

    let result = Ext2Fs::init_internal(&mut device);
    match result {
        Err(Ext2Error::UnsupportedBlockSize) => 0,
        _ => -1,
    }
}

fn test_ext2_directory_format_error() -> c_int {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return 0;
    };
    let dir_offset = 6 * 1024usize;
    unsafe {
        let dir_block = core::slice::from_raw_parts_mut(device.as_mut_ptr().add(dir_offset), 1024);
        dir_block[4] = 0;
        dir_block[5] = 0;
    }

    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return -1,
    };

    let result = fs.for_each_dir_entry(2, |_| true);
    match result {
        Err(Ext2Error::DirectoryFormat) => 0,
        _ => -1,
    }
}

fn test_ext2_invalid_inode() -> c_int {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return 0;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return -1,
    };

    let result = fs.read_inode(9999);
    match result {
        Err(Ext2Error::InvalidInode) => 0,
        _ => -1,
    }
}

fn test_ext2_read_file_not_regular() -> c_int {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return 0;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return -1,
    };

    let mut buf = [0u8; 32];
    let result = fs.read_file(2, 0, &mut buf);
    match result {
        Err(Ext2Error::NotFile) => 0,
        _ => -1,
    }
}

fn test_ext2_device_read_error() -> c_int {
    let mut device = FailingBlockDevice::new(4096).with_read_fail();
    let result = Ext2Fs::init_internal(&mut device);
    match result {
        Err(Ext2Error::DeviceError) => 0,
        _ => -1,
    }
}

fn test_ext2_device_write_error_on_metadata() -> c_int {
    let Some(device) = build_minimal_ext2_image(64, 32) else {
        return 0;
    };
    let mut failing = WriteFailingDevice::new(device);
    let mut fs = match Ext2Fs::init_internal(&mut failing) {
        Ok(fs) => fs,
        Err(_) => return 0,
    };

    wl_currency::reset();
    let result = fs.create_directory(2, b"faildir");
    let balance = wl_currency::check_balance();
    match result {
        Err(Ext2Error::DeviceError) => {
            if balance < 0 {
                0
            } else {
                -1
            }
        }
        _ => -1,
    }
}

fn test_ext2_read_block_out_of_bounds() -> c_int {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"boot.bin"),
        file_data: Some(b"slopos-test"),
        file_block: 80,
    };
    let Some(mut device) = build_ext2_image(spec) else {
        return 0;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return -1,
    };

    let inode = match fs.resolve_path(b"/boot.bin") {
        Ok(inode) => inode,
        Err(_) => return -1,
    };

    let result = fs.read_file(inode, 0, &mut [0u8; 1]);
    match result {
        Err(Ext2Error::InvalidBlock) | Err(Ext2Error::DeviceError) => 0,
        _ => -1,
    }
}

fn test_ext2_read_file_data_roundtrip() -> c_int {
    let spec = Ext2ImageSpec {
        blocks: 64,
        inodes: 32,
        file_name: Some(b"boot.bin"),
        file_data: Some(b"slopos-test"),
        file_block: 7,
    };
    let Some(mut device) = build_ext2_image(spec) else {
        return 0;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return -1,
    };

    let inode = match fs.resolve_path(b"/boot.bin") {
        Ok(inode) => inode,
        Err(_) => return -1,
    };

    let mut buf = [0u8; 16];
    let read_len = match fs.read_file(inode, 0, &mut buf) {
        Ok(len) => len,
        Err(_) => return -1,
    };

    if read_len != b"slopos-test".len() || &buf[..read_len] != b"slopos-test" {
        return -1;
    }
    0
}

fn test_ext2_path_resolution_not_found() -> c_int {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return 0;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return -1,
    };

    let result = fs.resolve_path(b"/nope/file.txt");
    match result {
        Err(Ext2Error::PathNotFound) => 0,
        _ => -1,
    }
}

fn test_ext2_remove_path_not_file() -> c_int {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return 0;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return -1,
    };

    let result = fs.remove_path(b"/");
    match result {
        Err(Ext2Error::PathNotFound) => 0,
        _ => -1,
    }
}

fn test_ext2_wl_currency_on_error() -> c_int {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return 0;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return -1,
    };

    wl_currency::reset();
    let result = fs.read_inode(0);
    let balance = wl_currency::check_balance();
    if result.is_err() && balance < 0 {
        0
    } else {
        -1
    }
}

fn test_ext2_wl_currency_on_success() -> c_int {
    let Some(mut device) = build_minimal_ext2_image(64, 32) else {
        return 0;
    };
    let mut fs = match Ext2Fs::init_internal(&mut device) {
        Ok(fs) => fs,
        Err(_) => return -1,
    };

    wl_currency::reset();
    let result = fs.read_inode(2);
    let balance = wl_currency::check_balance();
    if result.is_ok() && balance > 0 { 0 } else { -1 }
}

pub fn run_ext2_tests() -> c_int {
    run_ext2_tests_summary().0
}

pub fn run_ext2_tests_summary() -> (c_int, u32) {
    klog_info!("VFS_TEST: running suite");

    // Ensure VFS is initialized before running tests.
    // This is necessary because tests may run before the services boot phase.
    if let Err(_) = vfs_init_builtin_filesystems() {
        klog_info!("VFS_TEST: failed to initialize VFS");
        return (0, 0);
    }

    let mut passed = 0u32;
    let mut total = 0u32;

    slopos_lib::run_test!(passed, total, test_vfs_initialized);
    slopos_lib::run_test!(passed, total, test_vfs_root_stat);
    slopos_lib::run_test!(passed, total, test_vfs_file_roundtrip);
    slopos_lib::run_test!(passed, total, test_vfs_list);
    slopos_lib::run_test!(passed, total, test_vfs_unlink);

    slopos_lib::run_test!(passed, total, test_ext2_invalid_superblock_magic);
    slopos_lib::run_test!(passed, total, test_ext2_unsupported_block_size);
    slopos_lib::run_test!(passed, total, test_ext2_directory_format_error);
    slopos_lib::run_test!(passed, total, test_ext2_invalid_inode);
    slopos_lib::run_test!(passed, total, test_ext2_read_file_not_regular);
    slopos_lib::run_test!(passed, total, test_ext2_device_read_error);
    slopos_lib::run_test!(passed, total, test_ext2_device_write_error_on_metadata);
    slopos_lib::run_test!(passed, total, test_ext2_read_block_out_of_bounds);
    slopos_lib::run_test!(passed, total, test_ext2_read_file_data_roundtrip);
    slopos_lib::run_test!(passed, total, test_ext2_path_resolution_not_found);
    slopos_lib::run_test!(passed, total, test_ext2_remove_path_not_file);
    slopos_lib::run_test!(passed, total, test_ext2_wl_currency_on_error);
    slopos_lib::run_test!(passed, total, test_ext2_wl_currency_on_success);

    klog_info!("VFS_TEST: {passed}/{total} passed");
    (passed as c_int, total)
}
