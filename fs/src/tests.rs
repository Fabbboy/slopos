use core::ffi::c_int;
use core::ptr;

use slopos_drivers::serial_println;

use crate::ramfs::{
    ramfs_create_directory, ramfs_create_file, ramfs_find_node, ramfs_get_root,
    ramfs_list_directory, ramfs_node_release, ramfs_read_file, ramfs_release_list,
    ramfs_write_file, ramfs_node_t, RAMFS_TYPE_DIRECTORY, RAMFS_TYPE_FILE,
};

fn as_c(path: &[u8]) -> *const i8 {
    path.as_ptr() as *const i8
}

fn expect_dir(path: &[u8]) -> bool {
    let node = ramfs_find_node(as_c(path));
    if node.is_null() {
        return false;
    }
    let ok = unsafe { (*node).type_ == RAMFS_TYPE_DIRECTORY };
    ramfs_node_release(node);
    ok
}

#[allow(dead_code)]
fn expect_file(path: &[u8]) -> bool {
    let node = ramfs_find_node(as_c(path));
    if node.is_null() {
        return false;
    }
    let ok = unsafe { (*node).type_ == RAMFS_TYPE_FILE };
    ramfs_node_release(node);
    ok
}

fn test_root_node() -> c_int {
    serial_println!("RAMFS_TEST: Verifying root node");
    let root = ramfs_get_root();
    if root.is_null() {
        return -1;
    }
    unsafe {
        if (*root).type_ != RAMFS_TYPE_DIRECTORY || !(*root).parent.is_null() {
            return -1;
        }
    }
    0
}

fn test_file_roundtrip() -> c_int {
    serial_println!("RAMFS_TEST: file roundtrip");
    let dir = ramfs_create_directory(as_c(b"/itests\0"));
    if dir.is_null() || unsafe { (*dir).type_ } != RAMFS_TYPE_DIRECTORY {
        return -1;
    }
    let file = ramfs_create_file(as_c(b"/itests/hello.txt\0"), b"hello".as_ptr() as *const _, 5);
    if file.is_null() {
        return -1;
    }
    let mut buf = [0u8; 16];
    let mut read_len: usize = 0;
    if ramfs_read_file(
        as_c(b"/itests/hello.txt\0"),
        buf.as_mut_ptr() as *mut _,
        buf.len(),
        &mut read_len as *mut usize,
    ) != 0
        || read_len != 5
        || &buf[..5] != b"hello"
    {
        return -1;
    }
    0
}

fn test_write_updates_file() -> c_int {
    serial_println!("RAMFS_TEST: overwrite path");
    let content = b"goodbye world";
    if ramfs_write_file(
        as_c(b"/itests/hello.txt\0"),
        content.as_ptr() as *const _,
        content.len(),
    ) != 0
    {
        return -1;
    }
    let node = ramfs_find_node(as_c(b"/itests/hello.txt\0"));
    if node.is_null() {
        return -1;
    }
    let size = unsafe { (*node).size };
    ramfs_node_release(node);
    if size != content.len() {
        return -1;
    }
    let mut buf = [0u8; 32];
    let mut read_len = 0usize;
    if ramfs_read_file(
        as_c(b"/itests/hello.txt\0"),
        buf.as_mut_ptr() as *mut _,
        buf.len(),
        &mut read_len as *mut usize,
    ) != 0
        || read_len != content.len()
        || &buf[..content.len()] != content
    {
        return -1;
    }
    0
}

fn test_nested_directories() -> c_int {
    serial_println!("RAMFS_TEST: nested traversal");
    let nested_dir = ramfs_create_directory(as_c(b"/itests/nested\0"));
    if nested_dir.is_null() || unsafe { (*nested_dir).type_ } != RAMFS_TYPE_DIRECTORY {
        return -1;
    }
    let nested_file = ramfs_create_file(
        as_c(b"/itests/nested/file.txt\0"),
        b"nested data".as_ptr() as *const _,
        "nested data".len(),
    );
    if nested_file.is_null() {
        return -1;
    }

    let via_dot = ramfs_find_node(as_c(b"/itests/nested/./file.txt\0"));
    if via_dot != nested_file {
        return -1;
    }
    if expect_dir(b"/itests/nested/../nested\0") == false {
        return -1;
    }
    ramfs_node_release(nested_file);
    0
}

fn test_list_directory() -> c_int {
    serial_println!("RAMFS_TEST: list directory");
    let mut entries_ptr: *mut *mut ramfs_node_t = ptr::null_mut();
    let mut count: c_int = 0;
    if ramfs_list_directory(
        as_c(b"/itests\0"),
        &mut entries_ptr as *mut *mut *mut ramfs_node_t,
        &mut count as *mut c_int,
    ) != 0
    {
        return -1;
    }

    let entries = entries_ptr;
    let mut found_file = false;
    let mut found_nested = false;
    for idx in 0..(count as isize) {
        let entry = unsafe { *entries.offset(idx) };
        if entry.is_null() {
            continue;
        }
        let name = unsafe {
            let ptr = (*entry).name as *const u8;
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            core::slice::from_raw_parts(ptr, len)
        };
        if name == b"hello.txt" {
            found_file = true;
        }
        if name == b"nested" {
            found_nested = true;
        }
    }
    if !entries.is_null() {
        ramfs_release_list(entries, count);
    }
    if !found_file || !found_nested {
        return -1;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn run_ramfs_tests() -> c_int {
    serial_println!("RAMFS_TEST: running suite");
    let mut passed = 0;

    if test_root_node() == 0 {
        passed += 1;
    }
    if test_file_roundtrip() == 0 {
        passed += 1;
    }
    if test_write_updates_file() == 0 {
        passed += 1;
    }
    if test_nested_directories() == 0 {
        passed += 1;
    }
    if test_list_directory() == 0 {
        passed += 1;
    }

    serial_println!("RAMFS_TEST: {passed}/5 passed");
    passed
}

