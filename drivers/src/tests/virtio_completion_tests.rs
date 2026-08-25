//! VirtIO completion primitive regression tests: `IrqEdgeEvent`, the sleeping
//! `Mutex`, virtqueue descriptor free-list invariants, HPET `period_fs()`, and
//! live virtio-blk I/O after probe.

use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_ok, assert_test, fail, pass};

use slopos_fs::blockdev::{BlockDevice, BlockDeviceIndex};
use slopos_ostd::mm::heap::KVec;
use slopos_ostd::sync::Mutex;

use crate::hpet;
use crate::virtio::queue::Virtqueue;
use crate::virtio::{EdgeWait, IrqEdgeEvent};
use crate::virtio_blk;
use crate::virtio_blk::BlkClaimError;

/// The disposable scratch block device (virtio-disk1), attached only for the
/// test harness. Destructive block tests target THIS index, never disk0 (the
/// live root-fs image).
const SCRATCH: BlockDeviceIndex = BlockDeviceIndex(1);

pub fn test_edge_event_new_not_signaled() -> TestResult {
    let ev = IrqEdgeEvent::new();
    assert_test!(!ev.try_consume(), "new IrqEdgeEvent should not be signaled");
    pass!()
}

pub fn test_edge_event_signal_then_consume() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    assert_test!(
        ev.try_consume(),
        "try_consume should return true after signal"
    );
    pass!()
}

pub fn test_edge_event_double_consume() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    let first = ev.try_consume();
    let second = ev.try_consume();
    assert_test!(first, "first try_consume should succeed");
    assert_test!(!second, "second try_consume should fail (single-shot)");
    pass!()
}

pub fn test_edge_event_reset_clears_signal() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    ev.reset();
    assert_test!(
        !ev.try_consume(),
        "try_consume should fail after reset clears signal"
    );
    pass!()
}

pub fn test_edge_event_signal_after_reset() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    ev.reset();
    ev.signal();
    assert_test!(
        ev.try_consume(),
        "try_consume should succeed after signal-reset-signal"
    );
    pass!()
}

pub fn test_edge_event_multiple_signals() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    ev.signal();
    ev.signal();
    assert_test!(ev.try_consume(), "first consume after triple signal");
    assert_test!(
        !ev.try_consume(),
        "second consume should fail — only one event"
    );
    pass!()
}

pub fn test_edge_event_wait_presignaled() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    assert_eq_test!(
        ev.wait_timeout(5000),
        EdgeWait::Latched,
        "a pre-signaled wait must consume the latched edge without parking"
    );
    pass!()
}

pub fn test_edge_event_wait_timeout() -> TestResult {
    const TIMEOUT_MS: u32 = 1;
    let ev = IrqEdgeEvent::new();

    let Some(owed_ticks) = hpet::ms_to_ticks(TIMEOUT_MS) else {
        assert_eq_test!(
            ev.wait_timeout(TIMEOUT_MS),
            EdgeWait::TimedOut,
            "unsignaled wait should time out"
        );
        return pass!();
    };

    let start = hpet::read_counter();
    let outcome = ev.wait_timeout(TIMEOUT_MS);
    let elapsed_ticks = hpet::read_counter().wrapping_sub(start);

    assert_eq_test!(
        outcome,
        EdgeWait::TimedOut,
        "unsignaled wait should time out"
    );
    assert_test!(
        elapsed_ticks >= owed_ticks,
        "timeout returned after {} HPET ticks, owing {}",
        elapsed_ticks,
        owed_ticks
    );
    pass!()
}

pub fn test_sleep_mutex_lock_unlock() -> TestResult {
    let m = Mutex::new(7u32, lock_class!("test.virtio_mutex1", LOCK_LEVEL_RESOURCE));
    {
        let Ok(mut g) = m.lock() else {
            return fail!("uncontended lock must succeed");
        };
        *g += 1;
    }
    let Ok(g) = m.lock() else {
        return fail!("uncontended relock must succeed");
    };
    assert_eq_test!(*g, 8, "mutated value must persist across lock cycles");
    pass!()
}

pub fn test_sleep_mutex_try_lock_contention() -> TestResult {
    let m = Mutex::new(0u32, lock_class!("test.virtio_mutex2", LOCK_LEVEL_RESOURCE));
    let Ok(g) = m.lock() else {
        return fail!("uncontended lock must succeed");
    };
    assert_test!(
        m.try_lock().is_none(),
        "try_lock must fail while the mutex is held"
    );
    drop(g);
    assert_test!(
        m.try_lock().is_some(),
        "try_lock must succeed after the holder releases"
    );
    pass!()
}

pub fn test_sleep_mutex_relock_after_try() -> TestResult {
    let m = Mutex::new(1u32, lock_class!("test.virtio_mutex3", LOCK_LEVEL_RESOURCE));
    {
        let mut g = match m.try_lock() {
            Some(g) => g,
            None => return fail!("try_lock on a fresh mutex must succeed"),
        };
        *g = 2;
    }
    let Ok(g) = m.lock() else {
        return fail!("uncontended lock must succeed");
    };
    assert_eq_test!(*g, 2, "lock after try_lock must observe the mutation");
    pass!()
}

pub fn test_virtqueue_unready_alloc_none() -> TestResult {
    let mut q = Virtqueue::new();
    assert_eq_test!(q.free_count(), 0, "fresh queue advertises no descriptors");
    assert_test!(
        q.alloc_desc().is_none(),
        "alloc_desc on an unready queue must return None"
    );
    q.free_desc(3);
    assert_eq_test!(
        q.free_count(),
        0,
        "free_desc of an out-of-range index must be a no-op"
    );
    pass!()
}

pub fn test_hpet_period_fs_nonzero() -> TestResult {
    assert_test!(hpet::is_available(), "HPET must be available for this test");
    let period = hpet::period_fs();
    assert_test!(period > 0, "period_fs should be > 0 when HPET is init'd");
    assert_test!(
        period <= 100_000_000,
        "period_fs {} exceeds HPET spec max",
        period
    );
    pass!()
}

pub fn test_hpet_period_fs_matches_full_name() -> TestResult {
    assert_eq_test!(
        hpet::period_fs(),
        hpet::period_femtoseconds(),
        "period_fs and period_femtoseconds should return the same value"
    );
    pass!()
}

pub fn test_virtio_blk_read_interrupt_driven() -> TestResult {
    // Reads target disk0 (the root-fs image), which carries the ext2 superblock.
    let Some(disk0) = virtio_blk::blk_device_by_index(BlockDeviceIndex(0)) else {
        return fail!("root-fs block device (disk0) not present");
    };
    assert_test!(virtio_blk::blk_is_ready(disk0), "virtio-blk must be ready");

    let mut buf = [0u8; 512];
    let ok = virtio_blk::blk_read(disk0, 1024, &mut buf);
    assert_test!(ok, "superblock read should succeed via IRQ-driven I/O");
    let magic = u16::from_le_bytes([buf[0x38], buf[0x39]]);
    assert_eq_test!(magic, 0xEF53, "ext2 superblock magic mismatch");
    pass!()
}

pub fn test_virtio_blk_consecutive_reads() -> TestResult {
    let Some(disk0) = virtio_blk::blk_device_by_index(BlockDeviceIndex(0)) else {
        return fail!("root-fs block device (disk0) not present");
    };
    assert_test!(virtio_blk::blk_is_ready(disk0), "virtio-blk must be ready");

    let mut buf1 = [0u8; 512];
    let mut buf2 = [0u8; 512];
    let ok1 = virtio_blk::blk_read(disk0, 0, &mut buf1);
    let ok2 = virtio_blk::blk_read(disk0, 512, &mut buf2);
    assert_test!(ok1, "first consecutive read should succeed");
    assert_test!(ok2, "second consecutive read should succeed");
    pass!()
}

pub fn test_virtio_blk_write_readback_interrupt_driven() -> TestResult {
    // Destructive round-trip against the disposable scratch device (disk1),
    // never the live root-fs image (disk0): the scratch disk is recreated blank
    // each run and the write capability is exclusive, so no save/restore is
    // needed.
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    let offset = 8192u64 * 512;
    let pattern: [u8; 512] = {
        let mut p = [0u8; 512];
        for (i, b) in p.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        p
    };

    assert_test!(
        token.write_at(offset, &pattern).is_ok(),
        "write should succeed via IRQ-driven I/O"
    );
    let mut readback = [0u8; 512];
    assert_test!(
        token.read_at(offset, &mut readback).is_ok(),
        "readback should succeed via IRQ-driven I/O"
    );
    assert_test!(
        readback == pattern,
        "readback data should match written pattern"
    );
    pass!()
}

/// Multi-sector batching: a sector-aligned span larger than one sector must
/// round-trip through the chained-descriptor path (single request per
/// bounce-page chunk instead of one request per sector), and unaligned
/// sub-spans must read back correctly through the head/middle/tail split.
pub fn test_virtio_blk_multisector_write_readback() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    // Sector-aligned offset clear of the single-sector test's region and
    // inside the 8 MiB scratch image.
    // Heap buffers: 3772 bytes in one frame would step past the 4 KiB guard
    // page, which `stack-probes: none` gives no chance to catch.
    const SPAN: usize = 3 * 512;
    let offset = 2048u64 * 512;
    let mut pattern = assert_ok!(KVec::<u8>::zeroed(SPAN), "pattern buffer");
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = ((i * 7) ^ (i >> 8)) as u8;
    }

    assert_test!(
        token.write_at(offset, &pattern).is_ok(),
        "multi-sector write should succeed"
    );

    let mut readback = assert_ok!(KVec::<u8>::zeroed(SPAN), "readback buffer");
    assert_test!(
        token.read_at(offset, &mut readback).is_ok(),
        "multi-sector readback should succeed"
    );
    assert_test!(
        readback[..] == pattern[..],
        "multi-sector readback should match the written pattern"
    );

    // Unaligned sub-span crossing two sector boundaries: exercises the
    // partial-head + aligned-middle + partial-tail split.
    let mut sub = assert_ok!(KVec::<u8>::zeroed(700), "sub-span buffer");
    assert_test!(
        token.read_at(offset + 100, &mut sub).is_ok(),
        "unaligned sub-span read should succeed"
    );
    assert_test!(
        sub[..] == pattern[100..800],
        "unaligned sub-span must match the pattern slice"
    );
    pass!()
}

/// The durability barrier must complete promptly through the scheduler-backed
/// wait.
pub fn test_virtio_blk_flush_completes() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    // Sector 4000: inside the scratch image, disjoint from the other tests.
    let pattern = [0xA5u8; 512];
    assert_test!(
        token.write_at(4000 * 512, &pattern).is_ok(),
        "write before flush should succeed"
    );
    assert_test!(
        token.flush().is_ok(),
        "flush barrier should complete without timing out"
    );
    pass!()
}

/// The exclusive-write capability FSM: a device admits at most one live
/// [`BlockWriteToken`]; dropping it releases the claim.
pub fn test_block_device_exclusive_write_claim() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };

    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("first open_writer should succeed: {:?}", e),
    };

    assert_test!(
        matches!(
            virtio_blk::open_writer(handle),
            Err(BlkClaimError::AlreadyClaimed)
        ),
        "second open_writer must return AlreadyClaimed while a token is live"
    );

    drop(token);
    assert_test!(
        virtio_blk::open_writer(handle).is_ok(),
        "exclusive claim must be re-acquirable after the token is dropped"
    );
    pass!()
}

/// Device lookup by stable probe-order index, and registry bounds.
pub fn test_block_device_lookup_bounds() -> TestResult {
    assert_test!(
        virtio_blk::blk_device_by_index(BlockDeviceIndex(0)).is_some(),
        "disk0 (root fs) must be present"
    );
    assert_test!(
        virtio_blk::blk_device_by_index(SCRATCH).is_some(),
        "disk1 (scratch) must be present in the test harness"
    );
    assert_test!(
        virtio_blk::blk_device_by_index(BlockDeviceIndex(99)).is_none(),
        "an out-of-range index must resolve to None"
    );
    assert_test!(
        virtio_blk::blk_device_count() >= 2,
        "at least the root-fs and scratch devices must be claimed"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_edge_event_new_not_signaled,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_signal_then_consume,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_double_consume,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_reset_clears_signal,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_signal_after_reset,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_multiple_signals,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_wait_presignaled,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_wait_timeout,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_sleep_mutex_lock_unlock,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_sleep_mutex_try_lock_contention,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_sleep_mutex_relock_after_try,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtqueue_unready_alloc_none,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_hpet_period_fs_nonzero,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_hpet_period_fs_matches_full_name,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_read_interrupt_driven,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_consecutive_reads,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_write_readback_interrupt_driven,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_multisector_write_readback,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_flush_completes,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_block_device_exclusive_write_claim,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_block_device_lookup_bounds,
    suite = virtio_completion
);
