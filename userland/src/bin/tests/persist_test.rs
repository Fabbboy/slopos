#![feature(restricted_std)]

use slopos_userland as _;

use slopos_abi::fs::{O_RDONLY, O_RDWR};
use slopos_abi::syscall::posix::{MAP_PRIVATE, MAP_SHARED, PROT_READ, PROT_WRITE};
use slopos_abi::syscall::{BOOT_FLAG_ROOT_PERSISTENT, MS_SYNC, UserSysInfo};
use slopos_userland::syscall::fs as fs_syscall;
use slopos_userland::syscall::memory;
use std::ffi::c_char;
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

/// `statfs(2)` and `fstatfs(2)` must describe the same filesystem: a caller
/// sizes a write from one form and performs it through the other.
///
/// The probe lives under `/etc` so the descriptor and `/` name one filesystem
/// on a RAM root too, where `durable_dir()` would be a second mount.
fn statfs_agrees_with_fstatfs() -> bool {
    let probe = "/etc/.statfs-probe\0";
    if let Err(e) = fs::write(&probe[..probe.len() - 1], b"x") {
        println!("STATFS: probe create failed: {e}");
        return false;
    }
    let by_path = fs_syscall::statfs_path(b"/\0".as_ptr() as *const c_char);
    let fd = fs_syscall::open_path(probe.as_ptr() as *const c_char, O_RDONLY);
    let by_fd = fd.and_then(|fd| fs_syscall::fstatfs(fd.raw()));
    let _ = fs::remove_file(&probe[..probe.len() - 1]);

    let (by_path, by_fd) = match (by_path, by_fd) {
        (Ok(p), Ok(f)) => (p, f),
        (Err(e), _) => {
            println!("STATFS: statfs(\"/\") failed: {e}");
            return false;
        }
        (_, Err(e)) => {
            println!("STATFS: fstatfs failed: {e}");
            return false;
        }
    };

    if by_path.f_bsize == 0 {
        println!("STATFS: f_bsize is zero — a caller cannot size anything from that");
        return false;
    }
    // Free counts are live; the filesystem's identity and geometry are not.
    if (
        by_path.f_type,
        by_path.f_bsize,
        by_path.f_blocks,
        by_path.f_files,
    ) != (by_fd.f_type, by_fd.f_bsize, by_fd.f_blocks, by_fd.f_files)
    {
        println!(
            "STATFS: path and fd disagree: type {:#x}/{:#x} bsize {}/{} blocks {}/{} files {}/{}",
            by_path.f_type,
            by_fd.f_type,
            by_path.f_bsize,
            by_fd.f_bsize,
            by_path.f_blocks,
            by_fd.f_blocks,
            by_path.f_files,
            by_fd.f_files,
        );
        return false;
    }
    if by_path.f_bavail > by_path.f_bfree {
        println!("STATFS: f_bavail exceeds f_bfree — the reserve was not subtracted");
        return false;
    }
    true
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

/// A `MAP_SHARED` mapping and `read(2)` agree in both directions: the mapping
/// shows the file's bytes, and a store through it is what a later `read(2)`
/// returns.
fn mmap_shared_file_is_coherent_with_read() -> bool {
    const BODY: &[u8] = b"mmap-shared-original-contents-0123456789";
    const STORED: &[u8] = b"STORED";

    let path = unique_probe_path("mmap-shared");
    if let Err(e) = fs::write(&path, BODY) {
        println!("MMAP: create failed: {e}");
        return false;
    }
    let mut path_z = path.clone();
    path_z.push('\0');
    let Ok(fd) = fs_syscall::open_path(path_z.as_ptr() as *const c_char, O_RDWR) else {
        println!("MMAP: open failed");
        return false;
    };

    let base = memory::mmap(
        0,
        BODY.len() as u64,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd.raw() as i64,
        0,
    );
    if base == 0 || (base as i64) < 0 {
        println!("MMAP: MAP_SHARED of a regular file was refused ({base:#x})");
        return false;
    }

    let mapped_matches = read_mapping(base, BODY.len()) == BODY;
    store_through(base, STORED);
    let synced = memory::msync(base, BODY.len() as u64, MS_SYNC);
    let _ = memory::munmap(base, BODY.len() as u64);
    let _ = fs_syscall::close_fd(fd);

    let readback = fs::read(&path).unwrap_or_default();
    let _ = fs::remove_file(&path);

    if !mapped_matches {
        println!("MMAP: the mapping did not show the file's bytes");
        return false;
    }
    if synced != 0 {
        println!("MMAP: msync(MS_SYNC) failed: {synced}");
        return false;
    }
    if readback.len() != BODY.len() {
        println!(
            "MMAP: read back {} bytes, expected {}",
            readback.len(),
            BODY.len()
        );
        return false;
    }
    if &readback[..STORED.len()] != STORED {
        println!("MMAP: read(2) did not see the bytes stored through the mapping");
        return false;
    }
    if readback[STORED.len()..] != BODY[STORED.len()..] {
        println!("MMAP: the store clobbered bytes outside its own range");
        return false;
    }
    true
}

/// A `MAP_PRIVATE` mapping is populated from the same authority, and a store
/// through it never reaches the file.
fn mmap_private_file_keeps_its_store_private() -> bool {
    const BODY: &[u8] = b"mmap-private-original-contents-abcdefghij";
    const STORED: &[u8] = b"PRIVATE";

    let path = unique_probe_path("mmap-private");
    if let Err(e) = fs::write(&path, BODY) {
        println!("MMAP: create failed: {e}");
        return false;
    }
    let mut path_z = path.clone();
    path_z.push('\0');
    let Ok(fd) = fs_syscall::open_path(path_z.as_ptr() as *const c_char, O_RDWR) else {
        println!("MMAP: open failed");
        return false;
    };

    let base = memory::mmap(
        0,
        BODY.len() as u64,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE,
        fd.raw() as i64,
        0,
    );
    if base == 0 || (base as i64) < 0 {
        println!("MMAP: MAP_PRIVATE of a regular file was refused ({base:#x})");
        return false;
    }

    let mapped_matches = read_mapping(base, BODY.len()) == BODY;
    store_through(base, STORED);
    let stored_visible = read_mapping(base, STORED.len()) == STORED;
    let _ = memory::munmap(base, BODY.len() as u64);
    let _ = fs_syscall::close_fd(fd);

    let readback = fs::read(&path).unwrap_or_default();
    let _ = fs::remove_file(&path);

    if !mapped_matches {
        println!("MMAP: the private mapping did not show the file's bytes");
        return false;
    }
    if !stored_visible {
        println!("MMAP: a store through a private mapping was not readable back");
        return false;
    }
    if readback != BODY {
        println!("MMAP: a private store reached the file");
        return false;
    }
    true
}

/// A read-only descriptor cannot be turned into write access to the file:
/// neither by a shared writable `mmap` nor by `mprotect` afterwards. A private
/// writable mapping stays legal, since its store never reaches the file.
fn mmap_shared_write_needs_a_writable_descriptor() -> bool {
    const BODY: &[u8] = b"mmap-mode-gate-contents";

    let path = unique_probe_path("mmap-mode");
    if let Err(e) = fs::write(&path, BODY) {
        println!("MMAP: create failed: {e}");
        return false;
    }
    let mut path_z = path.clone();
    path_z.push('\0');
    let Ok(fd) = fs_syscall::open_path(path_z.as_ptr() as *const c_char, O_RDONLY) else {
        println!("MMAP: open failed");
        return false;
    };

    let len = BODY.len() as u64;
    let raw = fd.raw() as i64;
    let mut ok = true;

    let refused = memory::mmap(0, len, PROT_READ | PROT_WRITE, MAP_SHARED, raw, 0);
    if refused != 0 && (refused as i64) > 0 {
        println!("MMAP: a read-only descriptor mapped shared-writable");
        let _ = memory::munmap(refused, len);
        ok = false;
    }

    let private = memory::mmap(0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE, raw, 0);
    if private == 0 || (private as i64) < 0 {
        println!("MMAP: a private writable mapping of a read-only fd was refused ({private:#x})");
        ok = false;
    } else {
        if read_mapping(private, BODY.len()) != BODY {
            println!("MMAP: the private mapping did not show the file's bytes");
            ok = false;
        }
        let _ = memory::munmap(private, len);
    }

    let shared_ro = memory::mmap(0, len, PROT_READ, MAP_SHARED, raw, 0);
    if shared_ro == 0 || (shared_ro as i64) < 0 {
        println!("MMAP: a read-only shared mapping was refused ({shared_ro:#x})");
        ok = false;
    } else {
        if memory::mprotect(shared_ro, len, PROT_READ | PROT_WRITE) == 0 {
            println!("MMAP: mprotect widened a shared file mapping to writable");
            ok = false;
        }
        let _ = memory::munmap(shared_ro, len);
    }

    let _ = fs_syscall::close_fd(fd);
    let _ = fs::remove_file(&path);
    ok
}

/// A truncation under a live mapping is not undone by that mapping's
/// writeback: the page set is unkeyed before the size changes.
fn truncate_under_a_mapping_stays_truncated() -> bool {
    const BODY: &[u8] = b"mmap-truncate-original-contents";

    let path = unique_probe_path("mmap-trunc");
    if let Err(e) = fs::write(&path, BODY) {
        println!("MMAP: create failed: {e}");
        return false;
    }
    let mut path_z = path.clone();
    path_z.push('\0');
    let Ok(fd) = fs_syscall::open_path(path_z.as_ptr() as *const c_char, O_RDWR) else {
        println!("MMAP: open failed");
        return false;
    };

    let len = BODY.len() as u64;
    let base = memory::mmap(
        0,
        len,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd.raw() as i64,
        0,
    );
    if base == 0 || (base as i64) < 0 {
        println!("MMAP: MAP_SHARED of a regular file was refused ({base:#x})");
        let _ = fs_syscall::close_fd(fd);
        let _ = fs::remove_file(&path);
        return false;
    }
    store_through(base, b"CLOBBER");

    let truncated = fs_syscall::truncate_path(path_z.as_ptr() as *const c_char, 0);
    let synced = memory::msync(base, len, MS_SYNC);
    let _ = memory::munmap(base, len);
    let _ = fs_syscall::close_fd(fd);
    let _ = fs_syscall::sync();

    let readback = fs::read(&path).unwrap_or_default();
    let _ = fs::remove_file(&path);

    if truncated.is_err() {
        println!("MMAP: truncate of a mapped file failed");
        return false;
    }
    if synced != 0 {
        println!("MMAP: msync after a truncate failed: {synced}");
        return false;
    }
    if !readback.is_empty() {
        println!(
            "MMAP: a mapping's writeback resurrected {} bytes over a truncate",
            readback.len()
        );
        return false;
    }
    true
}

fn read_mapping(base: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let p = (base + i as u64) as *const u8;
        out.push(unsafe { p.read_volatile() });
    }
    out
}

fn store_through(base: u64, data: &[u8]) {
    for (i, byte) in data.iter().enumerate() {
        let p = (base + i as u64) as *mut u8;
        unsafe { p.write_volatile(*byte) };
    }
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
        ("statfs_agrees_with_fstatfs", statfs_agrees_with_fstatfs),
        (
            "the_disk_reserve_refuses_an_unprivileged_filler",
            the_disk_reserve_refuses_an_unprivileged_filler,
        ),
        (
            "mmap_shared_file_is_coherent_with_read",
            mmap_shared_file_is_coherent_with_read,
        ),
        (
            "mmap_private_file_keeps_its_store_private",
            mmap_private_file_keeps_its_store_private,
        ),
        (
            "mmap_shared_write_needs_a_writable_descriptor",
            mmap_shared_write_needs_a_writable_descriptor,
        ),
        (
            "truncate_under_a_mapping_stays_truncated",
            truncate_under_a_mapping_stays_truncated,
        ),
    ]);
}
