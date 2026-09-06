//! Mount identity, the mount table's child queries, and the paged listing's
//! mount pass.

use slopos_abi::fs::{FS_TYPE_DIRECTORY, UserFsEntry};
use slopos_ostd::lock_class;
use slopos_ostd::sync::LOCK_LEVEL_RESOURCE;
use slopos_testing::TestResult;

use crate::ramfs::RamFs;
use crate::vfs::traits::{FileSystem, FileType};
use crate::vfs::{
    ListCursor, mount, mount_at, unmount, vfs_init_builtin_filesystems, vfs_list_from, vfs_mkdir,
    vfs_rmdir, with_mount_table,
};

/// Four fixture filesystems, each with its own lock class: a path walk
/// crossing a mount holds one mount's lock while taking the next one's.
static FIXTURE_FS: [RamFs; 4] = [
    RamFs::new_const(lock_class!("RAMFS_MOUNT_TEST_0", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_MOUNT_TEST_1", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_MOUNT_TEST_2", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_MOUNT_TEST_3", LOCK_LEVEL_RESOURCE)),
];

fn ready() -> bool {
    vfs_init_builtin_filesystems().is_ok()
}

/// Fixtures live under `/tmp`, always the RAM mount the boot step puts there,
/// so no test here depends on `/` being writable.
fn ensure_dir(path: &[u8]) -> bool {
    let _ = vfs_mkdir(path);
    mount_at(path).is_some() || crate::vfs::vfs_stat(path).map(|(k, _)| k) == Ok(FS_TYPE_DIRECTORY)
}

fn entry_name(entry: &UserFsEntry) -> &[u8] {
    let end = entry
        .name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(entry.name.len());
    &entry.name[..end]
}

fn page_contains(page: &[UserFsEntry], name: &[u8]) -> bool {
    page.iter().any(|e| entry_name(e) == name)
}

/// A mount id is handed out once. Slot indices are reused the instant a mount
/// is released — which is exactly why the listing cursor may not be an ordinal.
pub fn test_mount_id_is_never_reused() -> TestResult {
    const MP: &[u8] = b"/tmp/mount_id_mp";

    if !ready() {
        return TestResult::Fail;
    }
    if !ensure_dir(MP) {
        return slopos_testing::fail!("could not create the mount point");
    }

    let outcome = (|| -> Result<(), &'static str> {
        mount(MP, &FIXTURE_FS[0], 0).map_err(|_| "first mount failed")?;
        let first = mount_at(MP)
            .ok_or("the first mount is not in the table")?
            .id;
        unmount(MP).map_err(|_| "first unmount failed")?;

        mount(MP, &FIXTURE_FS[1], 0).map_err(|_| "second mount failed")?;
        let second = mount_at(MP)
            .ok_or("the second mount is not in the table")?
            .id;
        let result = if second > first {
            Ok(())
        } else {
            Err("a released mount's identity came back")
        };
        unmount(MP).map_err(|_| "second unmount failed")?;
        result
    })();

    let _ = unmount(MP);
    let _ = vfs_rmdir(MP);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

/// `has_child_mount` answers for a *direct* child only, under either spelling
/// of the parent, and stops answering the moment the mount is released.
pub fn test_mount_table_child_queries() -> TestResult {
    const DIR: &[u8] = b"/tmp/child_q";
    const CHILD: &[u8] = b"/tmp/child_q/leaf";
    const DEEPER: &[u8] = b"/tmp/child_q/leaf/deeper";

    if !ready() || !ensure_dir(DIR) {
        return slopos_testing::fail!("could not create the fixture directory");
    }

    let outcome = (|| -> Result<(), &'static str> {
        mount(CHILD, &FIXTURE_FS[0], 0).map_err(|_| "mount failed")?;
        mount(DEEPER, &FIXTURE_FS[1], 0).map_err(|_| "deep mount failed")?;

        let checks = with_mount_table(|mt| {
            (
                mt.has_child_mount(DIR, b"leaf"),
                mt.has_child_mount(b"/tmp/child_q/", b"leaf"),
                mt.has_child_mount(DIR, b"deeper"),
                mt.has_child_mount(DIR, b"lea"),
            )
        });
        if !checks.0 {
            return Err("a direct child mount was not reported");
        }
        if !checks.1 {
            return Err("a trailing slash on the parent hid its child mount");
        }
        if checks.2 {
            return Err("a grandchild mount was reported as a direct child");
        }
        if checks.3 {
            return Err("a name prefix matched a child mount");
        }

        unmount(DEEPER).map_err(|_| "deep unmount failed")?;
        unmount(CHILD).map_err(|_| "unmount failed")?;
        if with_mount_table(|mt| mt.has_child_mount(DIR, b"leaf")) {
            return Err("a released mount is still reported as a child");
        }
        Ok(())
    })();

    let _ = unmount(DEEPER);
    let _ = unmount(CHILD);
    let _ = vfs_rmdir(DIR);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

/// A listing paged across a mount or unmount neither drops nor repeats an
/// entry. Both halves fail against a cursor keyed on an ordinal.
pub fn test_paged_listing_survives_a_mount_change() -> TestResult {
    if !ready() {
        return TestResult::Fail;
    }
    match (listing_drop_half(), listing_repeat_half()) {
        (Ok(()), Ok(())) => TestResult::Pass,
        (Err(msg), _) | (_, Err(msg)) => slopos_testing::fail!(msg),
    }
}

/// Page one of a three-entry buffer over a freshly created directory: `.`,
/// `..`, and the first child mount in id order.
fn first_page(dir: &[u8], entries: &mut [UserFsEntry; 3]) -> Result<ListCursor, &'static str> {
    let mut cursor = ListCursor::start();
    let n = vfs_list_from(dir, entries, &mut cursor).map_err(|_| "the first page failed")?;
    if n != 3 || entry_name(&entries[2]) != b"m1" {
        return Err("the first page did not end on the lowest-id child mount");
    }
    // Through the ABI word, because that is what userland carries between
    // calls and the packing is part of what is under test.
    Ok(ListCursor::from_abi(cursor.to_abi()))
}

fn listing_drop_half() -> Result<(), &'static str> {
    const DIR: &[u8] = b"/tmp/g9_drop";
    const M1: &[u8] = b"/tmp/g9_drop/m1";
    const M2: &[u8] = b"/tmp/g9_drop/m2";
    const M3: &[u8] = b"/tmp/g9_drop/m3";

    if !ensure_dir(DIR) {
        return Err("could not create the listing fixture");
    }
    let body = (|| -> Result<(), &'static str> {
        mount(M1, &FIXTURE_FS[0], 0).map_err(|_| "m1 mount failed")?;
        mount(M2, &FIXTURE_FS[1], 0).map_err(|_| "m2 mount failed")?;
        mount(M3, &FIXTURE_FS[2], 0).map_err(|_| "m3 mount failed")?;

        let mut entries = [UserFsEntry::new(); 3];
        let mut cursor = first_page(DIR, &mut entries)?;

        unmount(M1).map_err(|_| "m1 unmount failed")?;
        let n =
            vfs_list_from(DIR, &mut entries, &mut cursor).map_err(|_| "the second page failed")?;
        if !page_contains(&entries[..n], b"m2") {
            return Err("a mount was dropped when the set shrank between pages");
        }
        if page_contains(&entries[..n], b"m1") {
            return Err("an unmounted filesystem was still listed");
        }
        Ok(())
    })();

    let _ = unmount(M1);
    let _ = unmount(M2);
    let _ = unmount(M3);
    let _ = vfs_rmdir(DIR);
    body
}

fn listing_repeat_half() -> Result<(), &'static str> {
    const DIR: &[u8] = b"/tmp/g9_rep";
    const M0: &[u8] = b"/tmp/g9_rep/m0";
    const M1: &[u8] = b"/tmp/g9_rep/m1";
    const M2: &[u8] = b"/tmp/g9_rep/m2";
    const M3: &[u8] = b"/tmp/g9_rep/m3";

    if !ensure_dir(DIR) {
        return Err("could not create the listing fixture");
    }
    let body = (|| -> Result<(), &'static str> {
        mount(M0, &FIXTURE_FS[0], 0).map_err(|_| "m0 mount failed")?;
        mount(M1, &FIXTURE_FS[1], 0).map_err(|_| "m1 mount failed")?;
        mount(M2, &FIXTURE_FS[2], 0).map_err(|_| "m2 mount failed")?;
        // Frees the slot ahead of m1's, which the next mount takes.
        unmount(M0).map_err(|_| "m0 unmount failed")?;

        let mut entries = [UserFsEntry::new(); 3];
        let mut cursor = first_page(DIR, &mut entries)?;

        mount(M3, &FIXTURE_FS[3], 0).map_err(|_| "m3 mount failed")?;
        let n =
            vfs_list_from(DIR, &mut entries, &mut cursor).map_err(|_| "the second page failed")?;
        if page_contains(&entries[..n], b"m1") {
            return Err("a mount was listed twice when the set grew between pages");
        }
        if !page_contains(&entries[..n], b"m2") {
            return Err("a mount was dropped when the set grew between pages");
        }
        Ok(())
    })();

    let _ = unmount(M1);
    let _ = unmount(M2);
    let _ = unmount(M3);
    let _ = vfs_rmdir(DIR);
    body
}

/// A real directory entry a mount covers appears exactly once across every
/// page, as a directory.
pub fn test_mount_shadowed_name_lists_once() -> TestResult {
    const DIR: &[u8] = b"/tmp/g9_shadow";
    const COVERED: &[u8] = b"/tmp/g9_shadow/covered";

    if !ready() || !ensure_dir(DIR) || !ensure_dir(COVERED) {
        return slopos_testing::fail!("could not create the shadowing fixture");
    }

    let outcome = (|| -> Result<(), &'static str> {
        mount(COVERED, &FIXTURE_FS[0], 0).map_err(|_| "mount failed")?;

        // One entry per page, so the shadowed name and the mount entry cannot
        // land on the same page and be de-duplicated there.
        let mut entries = [UserFsEntry::new(); 1];
        let mut cursor = ListCursor::start();
        let mut seen = 0usize;
        let mut pages = 0usize;

        while !cursor.is_end() {
            let n = vfs_list_from(DIR, &mut entries, &mut cursor).map_err(|_| "a page failed")?;
            for entry in entries.iter().take(n) {
                if entry_name(entry) != b"covered" {
                    continue;
                }
                seen += 1;
                if entry.type_ != FS_TYPE_DIRECTORY {
                    return Err("a mount point did not list as a directory");
                }
            }
            cursor = ListCursor::from_abi(cursor.to_abi());
            pages += 1;
            if pages > 16 {
                return Err("the paged listing did not terminate");
            }
        }

        if seen != 1 {
            return Err("a name shadowed by a mount was not listed exactly once");
        }
        Ok(())
    })();

    let _ = unmount(COVERED);
    let _ = vfs_rmdir(COVERED);
    let _ = vfs_rmdir(DIR);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

/// The pool `mount(2)` hands `fstype="ramfs"` out of exhausts at its size,
/// recovers on release, and resets a released instance.
pub fn test_ramfs_mount_pool_exhausts_and_recovers() -> TestResult {
    use crate::vfs::init::{RAMFS_POOL_LEN, vfs_ramfs_pool_claim, vfs_ramfs_pool_release};

    let mut claimed: [Option<&'static RamFs>; RAMFS_POOL_LEN] = [None; RAMFS_POOL_LEN];
    for slot in claimed.iter_mut() {
        *slot = vfs_ramfs_pool_claim();
    }

    let outcome = (|| -> Result<(), &'static str> {
        for slot in claimed.iter() {
            if slot.is_none() {
                return Err("the pool refused an instance while it had room");
            }
        }
        if vfs_ramfs_pool_claim().is_some() {
            return Err("the pool handed out more instances than it holds");
        }

        let first = claimed[0].ok_or("the pool is empty")?;
        first
            .create(first.root_inode(), b"stale", FileType::Regular)
            .map_err(|_| "could not write to a pooled instance")?;

        let instance: &'static dyn FileSystem = first;
        if !vfs_ramfs_pool_release(instance, false) {
            return Err("the pool did not recognise its own instance");
        }
        let again = vfs_ramfs_pool_claim().ok_or("a released instance did not come back")?;
        claimed[0] = Some(again);

        if again.lookup(again.root_inode(), b"stale").is_ok() {
            return Err("a re-claimed instance still held the previous mount's file");
        }
        Ok(())
    })();

    for slot in claimed.iter().flatten() {
        let instance: &'static dyn FileSystem = *slot;
        let _ = vfs_ramfs_pool_release(instance, false);
    }
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

slopos_testing::stest!(name = test_mount_id_is_never_reused, suite = fs);
slopos_testing::stest!(name = test_mount_table_child_queries, suite = fs);
slopos_testing::stest!(
    name = test_paged_listing_survives_a_mount_change,
    suite = fs
);
slopos_testing::stest!(name = test_mount_shadowed_name_lists_once, suite = fs);
slopos_testing::stest!(
    name = test_ramfs_mount_pool_exhausts_and_recovers,
    suite = fs
);
