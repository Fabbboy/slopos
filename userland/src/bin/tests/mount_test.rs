#![feature(restricted_std)]

use slopos_userland as _;

use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::fs as fs_syscall;
use std::ffi::c_char;
use std::fs::{self, File};
use std::io::{Read, Write};

/// Mounted under `/tmp`, which is a ramfs the boot step already put there, so
/// the test needs no writable disk to create its mount point.
const MOUNT_POINT: &str = "/tmp/mount_test_mp";
const MOUNT_POINT_C: &[u8] = b"/tmp/mount_test_mp\0";

fn umount_mount_point() {
    let _ = fs_syscall::umount2(MOUNT_POINT_C.as_ptr() as *const c_char, 0);
}

/// How many entries of `/tmp` carry the mount point's name. Exactly one: the
/// covered directory and the synthesised mount entry are the same name.
fn mount_point_appearances() -> usize {
    let Ok(rd) = fs::read_dir("/tmp") else {
        return usize::MAX;
    };
    rd.flatten()
        .filter(|e| e.file_name().to_str() == Some("mount_test_mp"))
        .count()
}

fn ramfs_mount_roundtrip() -> bool {
    let _ = fs::create_dir(MOUNT_POINT);
    if !fs::metadata(MOUNT_POINT)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        println!("mount_test: could not create the mount point");
        return false;
    }

    if let Err(e) = fs_syscall::mount(b"", MOUNT_POINT.as_bytes(), b"ramfs", 0) {
        println!("mount_test: mount of a ramfs failed: {e}");
        let _ = fs::remove_dir(MOUNT_POINT);
        return false;
    }

    let mut ok = true;
    let probe = "/tmp/mount_test_mp/through-the-mount";
    let payload: &[u8] = b"mounted\n";

    match File::create(probe) {
        Ok(mut f) => {
            if f.write_all(payload).is_err() {
                println!("mount_test: write through the mount failed");
                ok = false;
            }
        }
        Err(e) => {
            println!("mount_test: create through the mount failed: {e}");
            ok = false;
        }
    }

    if ok {
        match File::open(probe) {
            Ok(mut f) => {
                let mut got = Vec::new();
                if f.read_to_end(&mut got).is_err() || got != payload {
                    println!(
                        "mount_test: read back {} bytes, want {}",
                        got.len(),
                        payload.len()
                    );
                    ok = false;
                }
            }
            Err(e) => {
                println!("mount_test: reopen through the mount failed: {e}");
                ok = false;
            }
        }
    }

    let seen = mount_point_appearances();
    if seen != 1 {
        println!("mount_test: the mount point appeared {seen} times in /tmp, want 1");
        ok = false;
    }

    // A live descriptor is `EBUSY`, so nothing may still hold the probe here.
    if let Err(e) = fs_syscall::umount2(MOUNT_POINT_C.as_ptr() as *const c_char, 0) {
        println!("mount_test: umount2 failed: {e}");
        umount_mount_point();
        let _ = fs::remove_dir(MOUNT_POINT);
        return false;
    }

    // The directory underneath was empty, so the file went with the mount.
    if File::open(probe).is_ok() {
        println!("mount_test: a file survived the unmount of its filesystem");
        ok = false;
    }
    if mount_point_appearances() != 1 {
        println!("mount_test: the plain directory vanished with the mount");
        ok = false;
    }

    let _ = fs::remove_dir(MOUNT_POINT);
    ok
}

/// The root mount is boot's: every open descriptor names the filesystem
/// underneath it.
fn umount_root_refused() -> bool {
    match fs_syscall::umount2(b"/\0".as_ptr() as *const c_char, 0) {
        Err(e) if e == SyscallError::EBUSY => true,
        Err(e) => {
            println!("mount_test: umount2(\"/\") gave {e}, want EBUSY");
            false
        }
        Ok(()) => {
            println!("mount_test: umount2(\"/\") succeeded");
            false
        }
    }
}

/// `mount(2)` cannot conjure a filesystem: the mountable set is closed.
fn unsupported_fstype_refused() -> bool {
    let _ = fs::create_dir(MOUNT_POINT);
    let result = fs_syscall::mount(b"", MOUNT_POINT.as_bytes(), b"nosuchfs", 0);
    let ok = match result {
        Err(e) if e == SyscallError::ENODEV => true,
        Err(e) => {
            println!("mount_test: an unsupported fstype gave {e}, want ENODEV");
            false
        }
        Ok(()) => {
            println!("mount_test: an unsupported fstype mounted");
            umount_mount_point();
            false
        }
    };
    let _ = fs::remove_dir(MOUNT_POINT);
    ok
}

/// The VFS caps a created name at `fs::MAX_NAME_LEN` (32) whatever filesystem
/// is underneath. Runs on `/` — the disk on the tests image, which by itself
/// would accept ext2's 255.
fn long_name_refused_on_the_root() -> bool {
    let dir = "/var/mount_test_names";
    let too_long = format!("{dir}/{}", "n".repeat(33));
    let longest = format!("{dir}/{}", "n".repeat(32));

    // Tolerates its own leavings: the disk root persists, so a boot that was
    // cut short must not make the next one fail on a directory that exists.
    let _ = fs::remove_dir(&longest);
    let _ = fs::create_dir(dir);
    if !fs::metadata(dir).map(|m| m.is_dir()).unwrap_or(false) {
        println!("mount_test: could not create the name fixture directory");
        return false;
    }

    let mut ok = true;
    if fs::create_dir(&too_long).is_ok() {
        println!("mount_test: a 33-byte name was accepted");
        ok = false;
    }
    if let Err(e) = fs::create_dir(&longest) {
        println!("mount_test: the 32-byte limit itself was refused: {e}");
        ok = false;
    }

    let _ = fs::remove_dir(&longest);
    let _ = fs::remove_dir(dir);
    ok
}

/// Refused even though this binary holds the mount capability: every
/// program-identity grant is keyed on a path under `/bin`, so covering it with
/// a caller-written filesystem would hand the caller those privileges.
fn mount_over_bin_refused() -> bool {
    match fs_syscall::mount(b"", b"/bin", b"ramfs", 0) {
        Err(e) if e == SyscallError::EPERM => true,
        Err(e) => {
            println!("mount_test: mount over /bin gave {e}, want EPERM");
            false
        }
        Ok(()) => {
            println!("mount_test: /bin was covered by a caller-supplied ramfs");
            let _ = fs_syscall::umount2(b"/bin\0".as_ptr() as *const c_char, 0);
            false
        }
    }
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("ramfs_mount_roundtrip", ramfs_mount_roundtrip),
        ("umount_root_refused", umount_root_refused),
        ("unsupported_fstype_refused", unsupported_fstype_refused),
        (
            "long_name_refused_on_the_root",
            long_name_refused_on_the_root,
        ),
        ("mount_over_bin_refused", mount_over_bin_refused),
    ]);
}
