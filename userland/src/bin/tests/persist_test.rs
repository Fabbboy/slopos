#![feature(restricted_std)]

use slopos_userland as _;

use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};

// `/` is a RAM root under `root=auto`; only the ext2 mount can persist.
const PROBE: &str = "/mnt/persist-probe";
const PAYLOAD: &[u8] = b"slopos-persist-v1\n";

/// Self-detecting, so one ISO serves both boots of `just test-persist`.
fn persist_roundtrip() -> bool {
    match File::open(PROBE) {
        Ok(mut f) => {
            let mut got = Vec::new();
            if f.read_to_end(&mut got).is_err() {
                println!("PERSIST: probe unreadable");
                return false;
            }
            if got == PAYLOAD {
                println!("PERSIST: verified");
                true
            } else {
                println!("PERSIST: payload mismatch ({} bytes)", got.len());
                false
            }
        }
        // Only an absent probe means "first boot"; seeding over any other
        // error would report the failure this test exists to catch as a pass.
        Err(e) if e.kind() != ErrorKind::NotFound => {
            println!("PERSIST: probe unopenable: {e}");
            false
        }
        Err(_) => {
            let mut f = match File::create(PROBE) {
                Ok(f) => f,
                Err(e) => {
                    println!("PERSIST: create failed: {e}");
                    return false;
                }
            };
            if f.write_all(PAYLOAD).is_err() {
                println!("PERSIST: write failed");
                return false;
            }
            // Without this the payload sits in the block cache.
            if let Err(e) = f.sync_all() {
                println!("PERSIST: sync_all failed: {e}");
                return false;
            }
            drop(f);

            match fs::read(PROBE) {
                Ok(got) if got == PAYLOAD => {
                    println!("PERSIST: seeded");
                    true
                }
                _ => {
                    println!("PERSIST: seeded probe did not read back");
                    false
                }
            }
        }
    }
}

fn sync_all_mounts() -> bool {
    slopos_userland::syscall::fs::sync().is_ok()
}

/// `fsync` commits the inode rather than the mount, so it must still commit
/// *this* inode: a payload written and fsync'd has to read back, and the call
/// must succeed on a filesystem with nothing else dirty.
///
/// A fresh name per boot, because `File::create` implies `O_TRUNC` and ext2
/// refuses that until the write surface is finished (plan 3.1). Re-running
/// this on a persistent image must not depend on overwriting.
fn fsync_commits_the_written_file() -> bool {
    let path = unique_probe_path("fsync");
    let payload = b"fsync-scope-v1\n";
    let mut f = match File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            println!("FSYNC: create failed: {e}");
            return false;
        }
    };
    if f.write_all(payload).is_err() {
        println!("FSYNC: write failed");
        return false;
    }
    if let Err(e) = f.sync_data() {
        println!("FSYNC: sync_data failed: {e}");
        return false;
    }
    if let Err(e) = f.sync_all() {
        println!("FSYNC: sync_all failed: {e}");
        return false;
    }
    // Idempotent: a second commit of an already-clean inode is not an error.
    if f.sync_all().is_err() {
        println!("FSYNC: repeat sync_all failed");
        return false;
    }
    drop(f);

    match fs::read(&path) {
        Ok(got) if got == payload => true,
        Ok(got) => {
            println!("FSYNC: payload mismatch ({} bytes)", got.len());
            false
        }
        Err(e) => {
            println!("FSYNC: read back failed: {e}");
            false
        }
    }
}

/// A name no previous boot of this image used, so a test that cannot yet
/// overwrite still runs on every boot. `/mnt` is the writable mount.
fn unique_probe_path(tag: &str) -> String {
    for n in 0..64u32 {
        let candidate = format!("/mnt/{tag}-probe{n}");
        if fs::metadata(&candidate).is_err() {
            return candidate;
        }
    }
    format!("/mnt/{tag}-probe-overflow")
}

/// No `fsync` call anywhere in this path: `O_SYNC` alone must put the bytes on
/// the device.
fn o_sync_write_needs_no_fsync() -> bool {
    use std::ffi::CString;

    let path = unique_probe_path("osync");
    let cpath = match CString::new(path.as_str()) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let payload = b"o-sync-v1\n";
    if let Err(e) = slopos_userland::syscall::fs::write_durable(&cpath, payload) {
        println!("OSYNC: durable write failed: {e:?}");
        return false;
    }
    match fs::read(&path) {
        Ok(got) if got == payload => true,
        _ => {
            println!("OSYNC: probe did not read back");
            false
        }
    }
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("persist_roundtrip", persist_roundtrip),
        ("sync_all_mounts", sync_all_mounts),
        (
            "fsync_commits_the_written_file",
            fsync_commits_the_written_file,
        ),
        ("o_sync_write_needs_no_fsync", o_sync_write_needs_no_fsync),
    ]);
}
