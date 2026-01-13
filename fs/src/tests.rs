use core::ffi::c_int;

use slopos_abi::fs::{USER_FS_OPEN_CREAT, USER_FS_OPEN_READ, USER_FS_OPEN_WRITE, UserFsEntry};
use slopos_lib::klog_info;

use crate::ext2_image::EXT2_IMAGE;
use crate::ext2_state::{
    ext2_init_with_image, ext2_list, ext2_mkdir, ext2_open, ext2_read, ext2_stat, ext2_unlink,
    ext2_write,
};
fn as_c(path: &[u8]) -> *const i8 {
    path.as_ptr() as *const i8
}

fn test_ext2_init() -> c_int {
    klog_info!("EXT2_TEST: init image");
    if ext2_init_with_image(EXT2_IMAGE) != 0 {
        return -1;
    }
    0
}

fn test_ext2_root() -> c_int {
    klog_info!("EXT2_TEST: root stat");
    let (kind, _size) = match ext2_stat(as_c(b"/\0")) {
        Ok(stat) => stat,
        Err(_) => return -1,
    };
    if kind != 1 {
        return -1;
    }
    0
}

fn test_ext2_file_roundtrip() -> c_int {
    klog_info!("EXT2_TEST: file roundtrip");
    if ext2_mkdir(as_c(b"/itests\0")).is_err() {
        return -1;
    }
    let flags = USER_FS_OPEN_READ | USER_FS_OPEN_WRITE | USER_FS_OPEN_CREAT;
    let inode = match ext2_open(as_c(b"/itests/hello.txt\0"), flags) {
        Ok(inode) => inode,
        Err(_) => return -1,
    };
    let content = b"hello ext2";
    if ext2_write(inode, 0, content).is_err() {
        return -1;
    }
    let mut buf = [0u8; 32];
    let read_len = match ext2_read(inode, 0, &mut buf) {
        Ok(len) => len,
        Err(_) => return -1,
    };
    if read_len != content.len() || &buf[..content.len()] != content {
        return -1;
    }
    0
}

fn test_ext2_list() -> c_int {
    klog_info!("EXT2_TEST: list directory");
    let mut entries = [UserFsEntry::new(); 8];
    let count = match ext2_list(as_c(b"/itests\0"), &mut entries) {
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

fn test_ext2_unlink() -> c_int {
    klog_info!("EXT2_TEST: unlink file");
    if ext2_unlink(as_c(b"/itests/hello.txt\0")).is_err() {
        return -1;
    }
    let mut entries = [UserFsEntry::new(); 8];
    let count = match ext2_list(as_c(b"/itests\0"), &mut entries) {
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
    klog_info!("EXT2_TEST: running suite");
    let mut passed = 0;

    if test_ext2_init() == 0 {
        passed += 1;
    }
    if test_ext2_root() == 0 {
        passed += 1;
    }
    if test_ext2_file_roundtrip() == 0 {
        passed += 1;
    }
    if test_ext2_list() == 0 {
        passed += 1;
    }
    if test_ext2_unlink() == 0 {
        passed += 1;
    }

    klog_info!("EXT2_TEST: {passed}/5 passed");
    passed
}
