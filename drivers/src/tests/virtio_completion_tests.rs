//! VirtIO completion primitive regression tests.
//!
//! Tests cover the split completion model:
//! - `CompletionEvent`: scheduler-backed blocking for request/response waits
//! - `IrqEdgeEvent`: fast atomic flag for NAPI-style edge notifications
//! - HPET `period_fs()` accessor used for deadline computation
//! - Integration tests through live virtio-blk after probe

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::hpet;
use crate::virtio::{CompletionEvent, IrqEdgeEvent};
use crate::virtio_blk;

// =============================================================================
// 1. IrqEdgeEvent unit tests (pure logic — no hardware)
// =============================================================================

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
    let start = hpet::read_counter();
    let result = ev.wait_timeout_ms(5000);
    let elapsed_ticks = hpet::read_counter().wrapping_sub(start);
    assert_test!(result, "wait should return true when pre-signaled");
    let period = hpet::period_fs() as u64;
    if period > 0 {
        let elapsed_ns = (elapsed_ticks as u128 * period as u128 / 1_000_000) as u64;
        assert_test!(
            elapsed_ns < 1_000_000,
            "pre-signaled wait took {} ns — should be < 1 ms",
            elapsed_ns
        );
    }
    pass!()
}

pub fn test_edge_event_wait_timeout() -> TestResult {
    if !hpet::is_available() {
        let ev = IrqEdgeEvent::new();
        let result = ev.wait_timeout_ms(1);
        assert_test!(!result, "unsignaled wait should timeout");
        return pass!();
    }

    let ev = IrqEdgeEvent::new();
    let start = hpet::read_counter();
    let result = ev.wait_timeout_ms(1);
    let elapsed_ticks = hpet::read_counter().wrapping_sub(start);
    assert_test!(!result, "unsignaled wait(1ms) should timeout");
    let period = hpet::period_fs() as u64;
    if period > 0 {
        let elapsed_ns = (elapsed_ticks as u128 * period as u128 / 1_000_000) as u64;
        assert_test!(
            elapsed_ns >= 500_000,
            "timeout wait took only {} ns — expected >= 0.5 ms",
            elapsed_ns
        );
    }
    pass!()
}

// =============================================================================
// 2. CompletionEvent unit tests
// =============================================================================

pub fn test_completion_event_new_not_signaled() -> TestResult {
    let ev = CompletionEvent::new();
    assert_test!(
        !ev.try_consume(),
        "new CompletionEvent should not be signaled"
    );
    pass!()
}

pub fn test_completion_event_signal_then_consume() -> TestResult {
    let ev = CompletionEvent::new();
    ev.signal();
    assert_test!(
        ev.try_consume(),
        "try_consume should return true after signal"
    );
    pass!()
}

pub fn test_completion_event_double_consume() -> TestResult {
    let ev = CompletionEvent::new();
    ev.signal();
    let first = ev.try_consume();
    let second = ev.try_consume();
    assert_test!(first, "first try_consume should succeed");
    assert_test!(!second, "second try_consume should fail (single-shot)");
    pass!()
}

pub fn test_completion_event_reset_clears_signal() -> TestResult {
    let ev = CompletionEvent::new();
    ev.signal();
    ev.reset();
    assert_test!(
        !ev.try_consume(),
        "try_consume should fail after reset clears signal"
    );
    pass!()
}

pub fn test_completion_event_wait_presignaled() -> TestResult {
    let ev = CompletionEvent::new();
    ev.signal();
    let start = hpet::read_counter();
    let result = ev.wait_timeout_ms(5000);
    let elapsed_ticks = hpet::read_counter().wrapping_sub(start);
    assert_test!(result, "wait should return true when pre-signaled");
    let period = hpet::period_fs() as u64;
    if period > 0 {
        let elapsed_ns = (elapsed_ticks as u128 * period as u128 / 1_000_000) as u64;
        assert_test!(
            elapsed_ns < 1_000_000,
            "pre-signaled wait took {} ns — should be < 1 ms",
            elapsed_ns
        );
    }
    pass!()
}

// =============================================================================
// 3. HPET period_fs() accessor
// =============================================================================

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

// =============================================================================
// 4. Integration: interrupt-driven I/O (CompletionEvent path)
// =============================================================================

pub fn test_virtio_blk_read_interrupt_driven() -> TestResult {
    assert_test!(
        virtio_blk::virtio_blk_is_ready(),
        "virtio-blk must be ready"
    );

    let mut buf = [0u8; 512];
    let ok = virtio_blk::virtio_blk_read(1024, &mut buf);
    assert_test!(ok, "superblock read should succeed via CompletionEvent I/O");
    let magic = u16::from_le_bytes([buf[0x38], buf[0x39]]);
    assert_eq_test!(magic, 0xEF53, "ext2 superblock magic mismatch");
    pass!()
}

pub fn test_virtio_blk_consecutive_reads() -> TestResult {
    assert_test!(
        virtio_blk::virtio_blk_is_ready(),
        "virtio-blk must be ready"
    );

    let mut buf1 = [0u8; 512];
    let mut buf2 = [0u8; 512];
    let ok1 = virtio_blk::virtio_blk_read(0, &mut buf1);
    let ok2 = virtio_blk::virtio_blk_read(512, &mut buf2);
    assert_test!(ok1, "first consecutive read should succeed");
    assert_test!(ok2, "second consecutive read should succeed");
    pass!()
}

pub fn test_virtio_blk_write_readback_interrupt_driven() -> TestResult {
    assert_test!(
        virtio_blk::virtio_blk_is_ready(),
        "virtio-blk must be ready"
    );

    let offset = 8192u64 * 512;
    let pattern: [u8; 512] = {
        let mut p = [0u8; 512];
        for (i, b) in p.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        p
    };

    let ok_write = virtio_blk::virtio_blk_write(offset, &pattern);
    assert_test!(ok_write, "write should succeed via CompletionEvent I/O");

    let mut readback = [0u8; 512];
    let ok_read = virtio_blk::virtio_blk_read(offset, &mut readback);
    assert_test!(ok_read, "readback should succeed via CompletionEvent I/O");
    assert_test!(
        readback == pattern,
        "readback data should match written pattern"
    );
    pass!()
}

// =============================================================================
// Suite registration
// =============================================================================

slopos_testing::define_test_suite!(
    virtio_completion,
    [
        // IrqEdgeEvent unit tests
        test_edge_event_new_not_signaled,
        test_edge_event_signal_then_consume,
        test_edge_event_double_consume,
        test_edge_event_reset_clears_signal,
        test_edge_event_signal_after_reset,
        test_edge_event_multiple_signals,
        test_edge_event_wait_presignaled,
        test_edge_event_wait_timeout,
        // CompletionEvent unit tests
        test_completion_event_new_not_signaled,
        test_completion_event_signal_then_consume,
        test_completion_event_double_consume,
        test_completion_event_reset_clears_signal,
        test_completion_event_wait_presignaled,
        // HPET accessor
        test_hpet_period_fs_nonzero,
        test_hpet_period_fs_matches_full_name,
        // Integration: CompletionEvent-driven I/O
        test_virtio_blk_read_interrupt_driven,
        test_virtio_blk_consecutive_reads,
        test_virtio_blk_write_readback_interrupt_driven,
    ]
);
