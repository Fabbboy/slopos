//! The per-principal disk-block ceiling.
//!
//! ext2's own `s_r_blocks_count` is a system floor: it keeps a reserve for
//! `/sbin/init` and the kernel's writes, and says nothing about how the space
//! *above* it is shared. These tests cover the other half — one process cannot
//! take all of it, what it took comes back when it frees, and what it still
//! holds at exit is released rather than inherited.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::quota::{QuotaMode, ResourceKind};
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{
    blocks_held, quota_mode, root, set_limit, set_quota_mode, stats,
};
use slopos_testing::{TestResult, fail};

use super::{Ext2ImageSpec, FIX_FILE_BLOCK, ScratchProcess, build_ext2_image};
use crate::blockdev::{BlockDevice, MemoryBlockDevice};
use crate::ext2::Ext2Fs;
use crate::ext2::cache::BlockCache;

/// Small enough that the first handful of files reaches it.
const CEILING: u32 = 6;
const NAMES: [&[u8]; 12] = [
    b"q0", b"q1", b"q2", b"q3", b"q4", b"q5", b"q6", b"q7", b"q8", b"q9", b"qa", b"qb",
];

/// The account the mounted body charges to. A static rather than a parameter
/// because the mount helper's frame has no room under the 2 KiB gate.
static ACCOUNT: AtomicU64 = AtomicU64::new(0);
/// Blocks the body left held, read back after it returns.
static HELD: AtomicU64 = AtomicU64::new(0);

fn image() -> Option<MemoryBlockDevice> {
    build_ext2_image(Ext2ImageSpec {
        blocks: 512,
        inodes: 32,
        file_name: None,
        file_data: None,
        file_block: FIX_FILE_BLOCK,
    })
}

#[inline(never)]
fn with_mount(
    device: &dyn BlockDevice,
    body: fn(&mut Ext2Fs<'_>) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let (sb, bs, is) = Ext2Fs::mount_params(device).map_err(|_| "mount_params")?;
    let mut cache = BlockCache::new_boxed(bs).map_err(|_| "cache")?;
    let mut fs = Ext2Fs::new(device, &mut cache, sb, bs, is).map_err(|_| "mount")?;
    fs.set_account(account());
    body(&mut fs)
}

fn account() -> AccountId {
    AccountId::from_raw(ACCOUNT.load(Ordering::Relaxed))
}

/// Fill files until the ceiling refuses, then give every block back. `used`
/// returning to exactly its pre-test value is the observable form: an
/// under-refund leaves it high, a double refund low.
pub fn test_quota_diskblocks_ceiling_refuses_and_refunds() -> TestResult {
    let Some(device) = image() else {
        return TestResult::Skipped;
    };
    let Some(scratch) = ScratchProcess::new() else {
        return fail!("no scratch process");
    };
    let account = scratch.table().account();
    ACCOUNT.store(account.raw(), Ordering::Relaxed);

    let baseline = stats(account, ResourceKind::DiskBlocks).map_or(0, |s| s.used);
    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    set_limit(account, ResourceKind::DiskBlocks, baseline + CEILING);

    let outcome = with_mount(&device, fill_then_free);
    let after = stats(account, ResourceKind::DiskBlocks).map_or(0, |s| s.used);
    let denials = stats(account, ResourceKind::DiskBlocks).map_or(0, |s| s.denials);
    set_quota_mode(restore);

    if let Err(msg) = outcome {
        return fail!("{}", msg);
    }
    if denials == 0 {
        return fail!("the ceiling was reached without counting a denial");
    }
    if after != baseline {
        return fail!(
            "used={} after freeing everything, want the {} it started at",
            after,
            baseline
        );
    }
    TestResult::Pass
}

fn fill_then_free(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let account = account();
    let held_before = blocks_held(account);
    let mut created = 0usize;
    let mut refused = false;
    for name in NAMES {
        let Ok(ino) = fs.create_file(2, name) else {
            refused = true;
            break;
        };
        created += 1;
        if fs.write_file(ino, 0, b"one block's worth").is_err() {
            refused = true;
            break;
        }
    }
    if !refused {
        return Err("the ceiling never refused a block");
    }
    let peak = blocks_held(account).saturating_sub(held_before);
    if peak > CEILING {
        return Err("more blocks are held than the ceiling permits");
    }
    if peak == 0 {
        return Err("the fixture charged nothing, so the ceiling proved nothing");
    }
    for name in NAMES.iter().take(created) {
        fs.unlink_entry(2, name).map_err(|_| "unlink")?;
    }
    if blocks_held(account) != held_before {
        return Err("freeing every file did not give every block back");
    }
    Ok(())
}

/// What a process still holds at exit is released, not inherited. Without the
/// release the charge migrates one hop up the chain, so the root ends a boot
/// billed for every block every exited process allocated.
pub fn test_quota_diskblocks_release_at_exit_is_not_inheritance() -> TestResult {
    let Some(device) = image() else {
        return TestResult::Skipped;
    };
    let Some(scratch) = ScratchProcess::new() else {
        return fail!("no scratch process");
    };
    let account = scratch.table().account();
    ACCOUNT.store(account.raw(), Ordering::Relaxed);
    HELD.store(0, Ordering::Relaxed);

    if let Err(msg) = with_mount(&device, fill_and_keep) {
        return fail!("{}", msg);
    }
    let held = HELD.load(Ordering::Relaxed) as u32;
    if held == 0 {
        return fail!("the fixture held no blocks, so the release proves nothing");
    }
    let root_before = stats(root(), ResourceKind::DiskBlocks).map_or(0, |s| s.used);
    drop(scratch);
    let root_after = stats(root(), ResourceKind::DiskBlocks).map_or(0, |s| s.used);

    if root_after != root_before.saturating_sub(held) {
        return fail!(
            "the root holds {} after the exit, want {} — the charge was inherited",
            root_after,
            root_before.saturating_sub(held)
        );
    }
    TestResult::Pass
}

fn fill_and_keep(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let account = account();
    for name in NAMES.iter().take(3) {
        let ino = fs.create_file(2, name).map_err(|_| "create")?;
        fs.write_file(ino, 0, b"kept").map_err(|_| "write")?;
    }
    HELD.store(u64::from(blocks_held(account)), Ordering::Relaxed);
    Ok(())
}

slopos_testing::stest!(
    name = test_quota_diskblocks_ceiling_refuses_and_refunds,
    suite = fs
);
slopos_testing::stest!(
    name = test_quota_diskblocks_release_at_exit_is_not_inheritance,
    suite = fs
);
