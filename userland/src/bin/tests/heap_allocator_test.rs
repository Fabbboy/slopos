#![feature(restricted_std)]

// Pull in the `slopos-userland` lib crate so its `_start` ELF entry
// point is linked into the binary. Without this `use _` reference, the
// binary has no `_start`, the linker emits an entry of 0x0, and the
// kernel's `do_exec` rejects the ELF as `NoExec`.
use slopos_userland as _;

use slopos_slibc::alloc::RawBuffer;

fn test_simd_fill_survives_demand_fault() -> bool {
    // A non-zero `[u32; 3]` fill (the terminal grid's Cell layout) compiles
    // to an AVX `vmovups` memset. On FRESH pages the fill hits demand faults
    // mid-SIMD; a kernel that fails to preserve user vector state across a
    // page fault corrupts the in-flight store, leaving stale zeros in the
    // buffer (the "garbage glyphs after resize" terminal bug). Each ~3.6 MB
    // round forces fresh, never-faulted pages.
    const FILL: [u32; 3] = [0x20, 0xe6e6e6, 0x1e1e1e];
    for _ in 0..20 {
        let mut v: Vec<[u32; 3]> = Vec::new();
        v.resize(300_000, FILL);
        if v.iter().any(|e| *e != FILL) {
            return false;
        }
        drop(v);
    }
    true
}

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

fn test_mass_free_then_realloc() -> bool {
    // Fill the arena with a batch of sub-threshold buffers, free the
    // whole batch (coalescing it back into segment-spanning chunks),
    // then allocate and touch fresh chunks. Catches stale bookkeeping
    // that survives a mass free — the access pattern of a terminal
    // grid resize.
    let mut batch = Vec::new();
    for round in 0..16usize {
        let Some(mut buf) = RawBuffer::new(32 * 1024) else {
            return false;
        };
        let tag = round as u8;
        buf.fill_with(|i| tag.wrapping_add(i as u8));
        batch.push(buf);
    }
    for (round, buf) in batch.iter().enumerate() {
        let tag = round as u8;
        if !buf.verify(|i| tag.wrapping_add(i as u8)) {
            return false;
        }
    }
    drop(batch);

    for round in 0..32usize {
        let Some(mut buf) = RawBuffer::new(4 * 1024) else {
            return false;
        };
        let tag = (round as u8).wrapping_mul(31);
        buf.fill_with(|i| tag.wrapping_add(i as u8));
        if !buf.verify(|i| tag.wrapping_add(i as u8)) {
            return false;
        }
    }
    true
}

fn test_segment_release() -> bool {
    use slopos_slibc::mem::malloc::heap_stats;

    // A batch larger than one segment forces the arena to grow; once
    // every chunk is freed the extra segments must be munmapped (the
    // last default-sized one stays resident as the warm arena).
    let before = heap_stats();
    let mut batch = Vec::new();
    for round in 0..64usize {
        let Some(mut buf) = RawBuffer::new(32 * 1024) else {
            return false;
        };
        let tag = round as u8;
        buf.fill_with(|i| tag.wrapping_mul(7).wrapping_add(i as u8));
        batch.push(buf);
    }
    let peak = heap_stats();
    if peak.arena_size <= before.arena_size {
        return false;
    }

    drop(batch);
    let after = heap_stats();
    after.arena_size < peak.arena_size
}

fn test_direct_registry() -> bool {
    use slopos_slibc::mem::malloc::heap_stats;

    // Threshold-sized allocations get a dedicated mapping tracked in
    // the direct registry; the count must follow create and free.
    let base = heap_stats().direct_count;
    let size = 256 * 1024;
    let Some(mut buf) = RawBuffer::new(size) else {
        return false;
    };
    buf.write_byte(0, 0xAB);
    buf.write_byte(size - 1, 0xCD);
    if heap_stats().direct_count != base + 1 {
        return false;
    }
    if buf.read_byte(0) != 0xAB || buf.read_byte(size - 1) != 0xCD {
        return false;
    }
    drop(buf);
    heap_stats().direct_count == base
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
        ("mass_free_then_realloc", test_mass_free_then_realloc),
        ("segment_release", test_segment_release),
        ("direct_registry", test_direct_registry),
        (
            "simd_fill_survives_demand_fault",
            test_simd_fill_survives_demand_fault,
        ),
    ]);
}
