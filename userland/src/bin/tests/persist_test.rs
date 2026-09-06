#![feature(restricted_std)]

use slopos_userland as _;

use slopos_abi::syscall::{BOOT_FLAG_ROOT_PERSISTENT, UserSysInfo};
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};

/// Where a durable write goes. `/var` on the disk root, which is where a
/// persistent thing belongs; `/mnt` when `/` is RAM and the disk is the
/// secondary mount, because a RAM `/var` persists nothing.
fn durable_dir() -> &'static str {
    if root_persists() { "/var" } else { "/mnt" }
}

/// Whether `/` is backed by a block device. A RAM root answers every syscall
/// this test makes identically — including a successful `fsync` — so nothing
/// short of asking the kernel distinguishes them.
fn root_persists() -> bool {
    let mut info = UserSysInfo::default();
    if slopos_userland::syscall::core::sys_info(&mut info) != 0 {
        return false;
    }
    info.boot_flags & BOOT_FLAG_ROOT_PERSISTENT != 0
}

fn probe_path() -> String {
    format!("{}/persist-probe", durable_dir())
}

/// Self-detecting, so one ISO serves both boots of `just test-persist`.
fn persist_roundtrip() -> bool {
    let probe = probe_path();
    let payload: &[u8] = b"slopos-persist-v1\n";
    match File::open(&probe) {
        Ok(mut f) => {
            let mut got = Vec::new();
            if f.read_to_end(&mut got).is_err() {
                println!("PERSIST: probe unreadable");
                return false;
            }
            if got == payload {
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
            let mut f = match File::create(&probe) {
                Ok(f) => f,
                Err(e) => {
                    println!("PERSIST: create failed: {e}");
                    return false;
                }
            };
            if f.write_all(payload).is_err() {
                println!("PERSIST: write failed");
                return false;
            }
            // Without this the payload sits in the block cache.
            if let Err(e) = f.sync_all() {
                println!("PERSIST: sync_all failed: {e}");
                return false;
            }
            drop(f);

            match fs::read(&probe) {
                Ok(got) if got == payload => {
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
/// A fresh name per boot, because `File::create` implies `O_TRUNC` and a
/// re-run on a persistent image must not depend on overwriting.
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
/// overwrite still runs on every boot.
fn unique_probe_path(tag: &str) -> String {
    let dir = durable_dir();
    for n in 0..64u32 {
        let candidate = format!("{dir}/{tag}-probe{n}");
        if fs::metadata(&candidate).is_err() {
            return candidate;
        }
    }
    format!("{dir}/{tag}-probe-overflow")
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

/// The root the test image boots is the disk. A RAM root passes every other
/// test here and persists nothing, so without this the suite would report a
/// regression to a RAM root as green.
fn the_root_is_the_disk() -> bool {
    if root_persists() {
        return true;
    }
    println!("PERSIST: / is not disk-backed — a write to it does not survive a reboot");
    false
}

/// `/etc` is the settings root and must be writable on whichever root booted:
/// `/etc/keymap` is written by `/bin/keymap` and read by `init` on the next
/// boot, which is the whole point of persisting it.
fn etc_is_writable() -> bool {
    let probe = "/etc/.persist-probe";
    if let Err(e) = fs::write(probe, b"x") {
        println!("ETC: /etc is not writable: {e}");
        return false;
    }
    let ok = fs::read(probe).map(|got| got == b"x").unwrap_or(false);
    let _ = fs::remove_file(probe);
    if !ok {
        println!("ETC: probe did not read back");
    }
    ok
}

/// The block reserve (`s_r_blocks_count`) is what stops an unprivileged writer
/// denying the disk to `/sbin/init`.
///
/// Every utest binary runs with `TASK_FLAG_SYSTEM`, which is exactly the
/// entitlement the reserve waives, so this process cannot be the filler. It
/// re-spawns itself without that flag as the [`FILL_ARG`] child, which writes
/// until refused and exits **leaving the filler in place**. The discriminating
/// observation is then this privileged parent's write: with a reserve it
/// succeeds into the blocks the child was refused; on a disk that simply
/// filled, free is zero and it fails. Only after that is the filler removed.
fn the_disk_reserve_refuses_an_unprivileged_filler() -> bool {
    use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
    use slopos_userland::syscall::process;

    if !root_persists() {
        return true;
    }
    let _ = fs::remove_file(FILLER_PATH);
    let stdio = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    let argv0 = b"persist_test\0";
    let argv = [argv0.as_ptr(), FILL_ARG.as_ptr()];
    let tid = process::spawn_path_with_actions(
        b"/bin/persist_test",
        &argv,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &stdio,
        0,
    );
    if tid <= 0 {
        println!("RESERVE: could not spawn the unprivileged filler ({tid})");
        return false;
    }
    let rc = process::waitpid(tid as u32);
    if rc != 0 {
        println!("RESERVE: the unprivileged filler exited {rc}");
        let _ = fs::remove_file(FILLER_PATH);
        return false;
    }

    let after = unique_probe_path("reserve-after");
    let ok = match fs::write(&after, b"reserve\n") {
        Ok(()) => true,
        Err(e) => {
            println!("RESERVE: nothing was held back for a privileged writer: {e}");
            false
        }
    };
    let _ = fs::remove_file(&after);
    let _ = fs::remove_file(FILLER_PATH);
    ok
}

const FILL_ARG: &[u8] = b"--fill-until-refused\0";
const FILLER_PATH: &str = "/var/reserve-filler";

/// The unprivileged half: fill the volume until `ENOSPC` and exit 0 with the
/// filler still on disk, so the parent can ask whether anything was held back.
/// Something must have been written first, or a refusal for an unrelated
/// reason reads as the reserve working. Bounded so a reserve that never
/// refuses ends this process rather than the run.
fn fill_until_refused() -> i32 {
    let Ok(mut f) = File::create(FILLER_PATH) else {
        println!("RESERVE(child): create failed");
        return 2;
    };
    let chunk = [0xABu8; 64 * 1024];
    let mut written = 0u64;
    let mut refused = false;
    while written < 256 * 1024 * 1024 {
        match f.write_all(&chunk) {
            Ok(()) => written += chunk.len() as u64,
            Err(_) => {
                refused = true;
                break;
            }
        }
    }
    drop(f);
    if !refused {
        println!("RESERVE(child): wrote {written} bytes and was never refused");
        return 3;
    }
    if written == 0 {
        println!("RESERVE(child): refused before writing anything");
        return 4;
    }
    0
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--fill-until-refused") {
        std::process::exit(fill_until_refused());
    }
    slopos_slibc::test_harness::run(&[
        ("the_root_is_the_disk", the_root_is_the_disk),
        ("persist_roundtrip", persist_roundtrip),
        ("sync_all_mounts", sync_all_mounts),
        (
            "fsync_commits_the_written_file",
            fsync_commits_the_written_file,
        ),
        ("o_sync_write_needs_no_fsync", o_sync_write_needs_no_fsync),
        ("etc_is_writable", etc_is_writable),
        (
            "the_disk_reserve_refuses_an_unprivileged_filler",
            the_disk_reserve_refuses_an_unprivileged_filler,
        ),
    ]);
}
