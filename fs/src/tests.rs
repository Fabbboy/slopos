use core::ffi::c_int;

use slopos_abi::fs::UserFsEntry;
use slopos_lib::klog_info;

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

pub fn run_ext2_tests() -> c_int {
    klog_info!("VFS_TEST: running suite");

    // Ensure VFS is initialized before running tests.
    // This is necessary because tests may run before the services boot phase.
    if let Err(_) = vfs_init_builtin_filesystems() {
        klog_info!("VFS_TEST: failed to initialize VFS");
        return 0;
    }

    let mut passed = 0;

    if test_vfs_initialized() == 0 {
        passed += 1;
    }
    if test_vfs_root_stat() == 0 {
        passed += 1;
    }
    if test_vfs_file_roundtrip() == 0 {
        passed += 1;
    }
    if test_vfs_list() == 0 {
        passed += 1;
    }
    if test_vfs_unlink() == 0 {
        passed += 1;
    }

    klog_info!("VFS_TEST: {passed}/5 passed");
    passed
}
