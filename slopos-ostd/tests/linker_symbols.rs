//! Host-side smoke tests for `slopos_ostd::arch::x86_64::linker`.
//!
//! The kernel linker symbols don't exist when `cargo test` runs on the
//! host; the module's `#[cfg(not(target_os = "none"))]` stub provides
//! synthetic non-null addresses backed by a private BSS buffer so the
//! invariants ("range non-empty", "start <= end", "stack top distinct")
//! can be exercised without a kernel ELF.

use slopos_ostd::arch::x86_64::linker;

#[test]
fn text_range_is_non_null_and_ordered() {
    let range = linker::text_range();
    assert!(!range.start.is_null(), "text_start must be non-null");
    assert!(!range.end.is_null(), "text_end must be non-null");
    assert!(
        (range.start as usize) < (range.end as usize),
        "text_start ({:p}) must precede text_end ({:p})",
        range.start,
        range.end,
    );
}

#[test]
fn kernel_image_range_is_non_null_and_ordered() {
    let range = linker::kernel_image_range();
    assert!(!range.start.is_null(), "kernel_start must be non-null");
    assert!(!range.end.is_null(), "kernel_end must be non-null");
    assert!(
        (range.start as usize) < (range.end as usize),
        "kernel_start ({:p}) must precede kernel_end ({:p})",
        range.start,
        range.end,
    );
}

#[test]
fn kernel_stack_top_is_non_null() {
    let p = linker::kernel_stack_top();
    assert!(!p.is_null(), "kernel_stack_top must be non-null");
}

#[test]
fn kernel_image_envelops_or_aliases_text() {
    // The kernel image starts at the same address as `.text` per
    // `link.ld:36` (`_kernel_start = _text_start;`). The host stub
    // mirrors this — both ranges start at the same byte.
    let text = linker::text_range();
    let image = linker::kernel_image_range();
    assert_eq!(text.start as usize, image.start as usize);
    assert!((image.end as usize) >= (text.end as usize));
}

#[test]
fn stack_top_distinct_from_image_anchors() {
    let image = linker::kernel_image_range();
    let stack = linker::kernel_stack_top();
    assert_ne!(stack as usize, image.start as usize);
    assert_ne!(stack as usize, image.end as usize);
}
