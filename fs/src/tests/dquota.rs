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
    self as quota, blocks_held, quota_mode, root, set_limit, set_quota_mode, stats,
};
use slopos_testing::{TestResult, fail};

use super::{Ext2ImageSpec, FIX_FILE_BLOCK, ScratchProcess, build_ext2_image};
use crate::blockdev::{BlockDevice, MemoryBlockDevice};
use crate::ext2::Ext2Fs;
use crate::ext2::blockcharge::{BlockCharges, MAX_ROWS};
use crate::ext2::cache::BlockCache;

/// Small enough that the first handful of files reaches it.
const CEILING: u32 = 6;
/// What the late allocator charges in the full-record test. Small because it
/// is a real charge and lands in the headroom gate's measured peak.
const LATE_BLOCKS: u32 = 2;
const NAMES: [&[u8]; 12] = [
    b"q0", b"q1", b"q2", b"q3", b"q4", b"q5", b"q6", b"q7", b"q8", b"q9", b"qa", b"qb",
];

/// The account the mounted body charges to. A static rather than a parameter
/// because the mount helper's frame has no room under the 2 KiB gate.
static ACCOUNT: AtomicU64 = AtomicU64::new(0);
/// The second principal, for a body that switches between them the way a
/// mount does between two processes' calls.
static OTHER: AtomicU64 = AtomicU64::new(0);
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

/// A refund credits the principal that was charged, not whoever ran the free:
/// the unlinker is not credited for a file it did not create, and the creator
/// stops being charged for blocks that no longer exist.
///
/// One mount for both principals: the attribution lives in the mount's block
/// cache, which in production lives for the boot.
pub fn test_quota_diskblocks_refund_follows_the_allocator() -> TestResult {
    let Some(device) = image() else {
        return TestResult::Skipped;
    };
    let Some(creator) = ScratchProcess::new() else {
        return fail!("no scratch process");
    };
    let Some(remover) = ScratchProcess::new() else {
        return fail!("no second scratch process");
    };
    ACCOUNT.store(creator.table().account().raw(), Ordering::Relaxed);
    OTHER.store(remover.table().account().raw(), Ordering::Relaxed);

    match with_mount(&device, foreign_unlink) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn other() -> AccountId {
    AccountId::from_raw(OTHER.load(Ordering::Relaxed))
}

fn foreign_unlink(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let (owner, remover) = (account(), other());
    let owner_before = blocks_held(owner);
    let remover_before = blocks_held(remover);

    let ino = fs.create_file(2, NAMES[0]).map_err(|_| "create")?;
    fs.write_file(ino, 0, b"one block's worth")
        .map_err(|_| "write")?;
    let owned = blocks_held(owner).saturating_sub(owner_before);
    if owned == 0 {
        return Err("the creator was charged nothing, so the refund proves nothing");
    }

    fs.set_account(remover);
    let mine = fs.create_file(2, NAMES[1]).map_err(|_| "create")?;
    fs.write_file(mine, 0, b"one block's worth")
        .map_err(|_| "write")?;
    let held = blocks_held(remover).saturating_sub(remover_before);
    if held == 0 {
        return Err("the remover was charged nothing, so its ledger cannot drift");
    }

    fs.unlink_entry(2, NAMES[0]).map_err(|_| "foreign unlink")?;
    if blocks_held(remover) != remover_before.saturating_add(held) {
        return Err("unlinking a foreign file credited the remover's ledger");
    }
    if blocks_held(owner) != owner_before {
        return Err("the creator stayed charged for blocks that were freed");
    }

    fs.unlink_entry(2, NAMES[1]).map_err(|_| "own unlink")?;
    if blocks_held(remover) != remover_before {
        return Err("deleting its own file did not give the remover its blocks back");
    }
    Ok(())
}

/// A table full of one principal's rows must not strip another's refunds: the
/// next allocator keeps a refundable row.
pub fn test_quota_diskblocks_a_full_record_costs_the_crowd() -> TestResult {
    let Some(crowd) = ScratchProcess::new() else {
        return fail!("no scratch process");
    };
    let Some(late) = ScratchProcess::new() else {
        return fail!("no second scratch process");
    };
    let (crowd, late) = (crowd.table().account(), late.table().account());
    let Ok(mut charges) = BlockCharges::new() else {
        return fail!("the record would not allocate");
    };

    // One operation per row: the pending table holds too few pairs for one
    // operation to publish this many. Recorded but not charged, because the
    // record is what fills up and a fixture must not move the gate's peak.
    for ino in 1..=MAX_ROWS as u32 {
        charges.charge(Some(ino), crowd, 1);
        charges.commit();
    }

    let before = blocks_held(late);
    let ino = MAX_ROWS as u32 + 1;
    if quota::charge_blocks(late, LATE_BLOCKS).is_err() {
        return fail!("the late allocator was refused a block it is entitled to");
    }
    charges.charge(Some(ino), late, LATE_BLOCKS);
    charges.commit();
    charges.free(Some(ino), LATE_BLOCKS);
    charges.commit();

    let after = blocks_held(late);
    if after != before {
        return fail!(
            "a full record left the late allocator holding {} of its own freed blocks (was {})",
            after.saturating_sub(before),
            before
        );
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_quota_diskblocks_ceiling_refuses_and_refunds,
    suite = fs
);
slopos_testing::stest!(
    name = test_quota_diskblocks_release_at_exit_is_not_inheritance,
    suite = fs
);
slopos_testing::stest!(
    name = test_quota_diskblocks_refund_follows_the_allocator,
    suite = fs
);
slopos_testing::stest!(
    name = test_quota_diskblocks_a_full_record_costs_the_crowd,
    suite = fs
);
