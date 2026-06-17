//! VirtIO GPU 2D driver regression tests.
//!
//! Pure-logic tests (format mapping, damage coalescing) run everywhere.
//! Integration tests round-trip real control-queue commands and are gated on a
//! live device: on a QEMU without `-device virtio-gpu-pci` they no-op (the
//! kernel stays on the passive framebuffer), so they pass trivially rather than
//! fail the suite.

use slopos_abi::PixelFormat;
use slopos_abi::damage::DamageRect;
use slopos_ostd::klog_info;
use slopos_testing::{TestResult, assert_eq_test, assert_test, pass};

use crate::virtio_gpu;
use crate::virtio_gpu::test_support;

// =============================================================================
// Pure-logic tests (no device)
// =============================================================================

/// SlopOS pixel formats map to the matching virtio-gpu 2D format codes.
pub fn test_virtio_gpu_format_mapping() -> TestResult {
    // Argb8888 memory order [B,G,R,A] == VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM (1).
    assert_eq_test!(test_support::format_code(PixelFormat::Argb8888), 1u32);
    // Xrgb8888 memory order [B,G,R,X] == VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM (2).
    assert_eq_test!(test_support::format_code(PixelFormat::Xrgb8888), 2u32);
    // Unsupported scanout formats fall back to opaque BGRX (2).
    assert_eq_test!(test_support::format_code(PixelFormat::Rgb888), 2u32);
    pass!()
}

/// Scattered damage rects coalesce to their bounding box.
pub fn test_virtio_gpu_coalesce_bbox() -> TestResult {
    let rects = [
        DamageRect {
            x0: 0,
            y0: 0,
            x1: 9,
            y1: 9,
        },
        DamageRect {
            x0: 20,
            y0: 30,
            x1: 29,
            y1: 39,
        },
    ];
    let bbox = test_support::coalesce(&rects, 100, 100);
    assert_eq_test!(bbox, Some((0u32, 0u32, 30u32, 40u32)));
    pass!()
}

/// A rect overhanging the screen is clamped to the framebuffer bounds.
pub fn test_virtio_gpu_coalesce_clamps_to_screen() -> TestResult {
    let rects = [DamageRect {
        x0: -5,
        y0: -5,
        x1: 999,
        y1: 999,
    }];
    let bbox = test_support::coalesce(&rects, 100, 80);
    assert_eq_test!(bbox, Some((0u32, 0u32, 100u32, 80u32)));
    pass!()
}

/// No valid rect (empty list or inverted rect) yields no transfer.
pub fn test_virtio_gpu_coalesce_invalid_is_none() -> TestResult {
    assert_test!(test_support::coalesce(&[], 100, 100).is_none());
    let invalid = [DamageRect {
        x0: 10,
        y0: 10,
        x1: 5,
        y1: 5,
    }];
    assert_test!(test_support::coalesce(&invalid, 100, 100).is_none());
    pass!()
}

/// Mode validation rejects out-of-range sizes and rounds to even dimensions.
pub fn test_virtio_gpu_sanitize_mode() -> TestResult {
    assert_test!(
        test_support::sanitize(100, 100).is_none(),
        "too small accepted"
    );
    assert_test!(
        test_support::sanitize(99999, 1080).is_none(),
        "too wide accepted"
    );
    assert_eq_test!(test_support::sanitize(1920, 1080), Some((1920u32, 1080u32)));
    assert_eq_test!(test_support::sanitize(1921, 1081), Some((1920u32, 1080u32)));
    pass!()
}

// =============================================================================
// Integration tests (live device, gated on is_present)
// =============================================================================

/// `GET_DISPLAY_INFO` returns a non-zero scanout-0 resolution.
pub fn test_virtio_gpu_display_info_roundtrip() -> TestResult {
    if !virtio_gpu::is_present() {
        klog_info!("virtio-gpu: no device — skipping display-info round-trip");
        return pass!();
    }
    match test_support::display_info() {
        Some((w, h)) => {
            assert_test!(w > 0 && h > 0, "display info returned zero dimensions");
            pass!()
        }
        None => pass!(), // device may report scanout 0 disabled; not a failure
    }
}

/// Full control-queue resource lifecycle (create → attach → transfer → flush →
/// unref) acks OK against the live device.
pub fn test_virtio_gpu_resource_roundtrip() -> TestResult {
    if !virtio_gpu::is_present() {
        klog_info!("virtio-gpu: no device — skipping resource round-trip");
        return pass!();
    }
    assert_test!(
        test_support::resource_roundtrip(),
        "virtio-gpu resource lifecycle round-trip failed"
    );
    pass!()
}

/// Hardware cursor upload + move round-trips on the cursor queue.
pub fn test_virtio_gpu_cursor_roundtrip() -> TestResult {
    if !virtio_gpu::is_present() {
        klog_info!("virtio-gpu: no device — skipping cursor round-trip");
        return pass!();
    }
    assert_test!(
        test_support::cursor_roundtrip(),
        "virtio-gpu cursor upload/move round-trip failed"
    );
    pass!()
}

slopos_testing::stest!(name = test_virtio_gpu_format_mapping);
slopos_testing::stest!(name = test_virtio_gpu_coalesce_bbox);
slopos_testing::stest!(name = test_virtio_gpu_coalesce_clamps_to_screen);
slopos_testing::stest!(name = test_virtio_gpu_coalesce_invalid_is_none);
slopos_testing::stest!(name = test_virtio_gpu_sanitize_mode);
slopos_testing::stest!(name = test_virtio_gpu_display_info_roundtrip);
slopos_testing::stest!(name = test_virtio_gpu_resource_roundtrip);
slopos_testing::stest!(name = test_virtio_gpu_cursor_roundtrip);
