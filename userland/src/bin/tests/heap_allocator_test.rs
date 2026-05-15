#![feature(restricted_std)]

// Pull in the `slopos-userland` lib crate so its `_start` ELF entry
// point is linked into the binary. Without this `use _` reference, the
// binary has no `_start`, the linker emits an entry of 0x0, and the
// kernel's `do_exec` rejects the ELF as `NoExec`.
use slopos_userland as _;

use slopos_slibc::alloc::RawBuffer;

fn test_alloc_dealloc_basic() -> bool {
    let Some(mut buf) = RawBuffer::new(64) else {
        return false;
    };
    buf.fill_with(|i| (i as u8).wrapping_mul(3));
    buf.verify(|i| (i as u8).wrapping_mul(3))
}

fn test_forward_coalesce() -> bool {
    let Some(a) = RawBuffer::new(64) else {
        return false;
    };
    let Some(b) = RawBuffer::new(64) else {
        return false;
    };
    drop(b);
    drop(a);

    let Some(c) = RawBuffer::new(128) else {
        return false;
    };
    drop(c);
    true
}

fn test_backward_coalesce() -> bool {
    let Some(a) = RawBuffer::new(64) else {
        return false;
    };
    let Some(b) = RawBuffer::new(64) else {
        return false;
    };
    drop(a);
    drop(b);

    let Some(c) = RawBuffer::new(128) else {
        return false;
    };
    drop(c);
    true
}

fn test_format_pattern_stability() -> bool {
    let mut seed: u32 = 0x4D59_5DF4;

    for iter in 0..1000usize {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let size = 32 + (seed as usize % 97);

        let Some(mut buf) = RawBuffer::new(size) else {
            return false;
        };

        let base = (iter as u8).wrapping_mul(13);
        buf.fill_with(|i| base.wrapping_add(i as u8));
        if !buf.verify(|i| base.wrapping_add(i as u8)) {
            return false;
        }
    }

    let Some(mut big) = RawBuffer::new(64 * 1024) else {
        return false;
    };
    big.write_byte(0, 0xA5);
    big.write_byte(64 * 1024 - 1, 0x5A);
    big.read_byte(0) == 0xA5 && big.read_byte(64 * 1024 - 1) == 0x5A
}

fn test_mmap_fallback() -> bool {
    let size = 256 * 1024;
    let Some(mut buf) = RawBuffer::new(size) else {
        return false;
    };
    buf.write_byte(0, 0xC1);
    buf.write_byte(size - 1, 0x1C);
    buf.read_byte(0) == 0xC1 && buf.read_byte(size - 1) == 0x1C
}

fn test_realloc_grow() -> bool {
    let Some(mut p) = RawBuffer::new(32) else {
        return false;
    };
    p.fill_with(|i| i as u8);

    let Some(p) = p.realloc(128) else {
        return false;
    };
    for i in 0..32 {
        if p.read_byte(i) != i as u8 {
            return false;
        }
    }

    let Some(p) = p.realloc(256) else {
        return false;
    };
    for i in 0..32 {
        if p.read_byte(i) != i as u8 {
            return false;
        }
    }
    true
}

fn test_small_recycling() -> bool {
    let Some(a) = RawBuffer::new(64) else {
        return false;
    };
    drop(a);

    let Some(b) = RawBuffer::new(64) else {
        return false;
    };
    drop(b);
    true
}

fn main() {
    // Reports each subtest to the kernel via SYSCALL_TEST_REPORT; exit
    // code is the failure count. Kernel utest runner uses the structured
    // reports for fine-grained roll-up.
    slopos_slibc::test_harness::run(&[
        ("alloc_dealloc_basic", test_alloc_dealloc_basic),
        ("forward_coalesce", test_forward_coalesce),
        ("backward_coalesce", test_backward_coalesce),
        ("format_pattern_stability", test_format_pattern_stability),
        ("mmap_fallback", test_mmap_fallback),
        ("realloc_grow", test_realloc_grow),
        ("small_recycling", test_small_recycling),
    ]);
}
