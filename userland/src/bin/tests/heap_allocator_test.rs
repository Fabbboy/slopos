#![feature(restricted_std)]

use core::ffi::c_void;

use slopos_slibc::mem::malloc;

fn test_alloc_dealloc_basic() -> bool {
    let ptr = malloc::alloc(64).cast::<u8>();
    if ptr.is_null() {
        return false;
    }

    unsafe {
        for i in 0..64 {
            *ptr.add(i) = (i as u8).wrapping_mul(3);
        }
        for i in 0..64 {
            if *ptr.add(i) != (i as u8).wrapping_mul(3) {
                malloc::dealloc(ptr.cast::<c_void>());
                return false;
            }
        }
    }

    malloc::dealloc(ptr.cast::<c_void>());
    true
}

fn test_forward_coalesce() -> bool {
    let a = malloc::alloc(64).cast::<u8>();
    let b = malloc::alloc(64).cast::<u8>();
    if a.is_null() || b.is_null() {
        if !a.is_null() {
            malloc::dealloc(a.cast::<c_void>());
        }
        if !b.is_null() {
            malloc::dealloc(b.cast::<c_void>());
        }
        return false;
    }

    malloc::dealloc(b.cast::<c_void>());
    malloc::dealloc(a.cast::<c_void>());

    let c = malloc::alloc(128).cast::<u8>();
    if c.is_null() {
        return false;
    }
    malloc::dealloc(c.cast::<c_void>());
    true
}

fn test_backward_coalesce() -> bool {
    let a = malloc::alloc(64).cast::<u8>();
    let b = malloc::alloc(64).cast::<u8>();
    if a.is_null() || b.is_null() {
        if !a.is_null() {
            malloc::dealloc(a.cast::<c_void>());
        }
        if !b.is_null() {
            malloc::dealloc(b.cast::<c_void>());
        }
        return false;
    }

    malloc::dealloc(a.cast::<c_void>());
    malloc::dealloc(b.cast::<c_void>());

    let c = malloc::alloc(128).cast::<u8>();
    if c.is_null() {
        return false;
    }
    malloc::dealloc(c.cast::<c_void>());
    true
}

fn test_format_pattern_stability() -> bool {
    let mut seed: u32 = 0x4D59_5DF4;

    for iter in 0..1000usize {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let size = 32 + (seed as usize % 97);

        let ptr = malloc::alloc(size).cast::<u8>();
        if ptr.is_null() {
            return false;
        }

        unsafe {
            let base = (iter as u8).wrapping_mul(13);
            for i in 0..size {
                *ptr.add(i) = base.wrapping_add(i as u8);
            }
            for i in 0..size {
                if *ptr.add(i) != base.wrapping_add(i as u8) {
                    malloc::dealloc(ptr.cast::<c_void>());
                    return false;
                }
            }
        }

        malloc::dealloc(ptr.cast::<c_void>());
    }

    let big = malloc::alloc(64 * 1024).cast::<u8>();
    if big.is_null() {
        return false;
    }
    unsafe {
        *big = 0xA5;
        *big.add(64 * 1024 - 1) = 0x5A;
        if *big != 0xA5 || *big.add(64 * 1024 - 1) != 0x5A {
            malloc::dealloc(big.cast::<c_void>());
            return false;
        }
    }
    malloc::dealloc(big.cast::<c_void>());
    true
}

fn test_mmap_fallback() -> bool {
    let size = 256 * 1024;
    let ptr = malloc::alloc(size).cast::<u8>();
    if ptr.is_null() {
        return false;
    }
    unsafe {
        *ptr = 0xC1;
        *ptr.add(size - 1) = 0x1C;
        if *ptr != 0xC1 || *ptr.add(size - 1) != 0x1C {
            malloc::dealloc(ptr.cast::<c_void>());
            return false;
        }
    }
    malloc::dealloc(ptr.cast::<c_void>());
    true
}

fn test_realloc_grow() -> bool {
    let p1 = malloc::alloc(32).cast::<u8>();
    if p1.is_null() {
        return false;
    }

    unsafe {
        for i in 0..32 {
            *p1.add(i) = i as u8;
        }
    }

    let p2 = malloc::realloc(p1.cast::<c_void>(), 128).cast::<u8>();
    if p2.is_null() {
        return false;
    }

    unsafe {
        for i in 0..32 {
            if *p2.add(i) != i as u8 {
                malloc::dealloc(p2.cast::<c_void>());
                return false;
            }
        }
    }

    let p3 = malloc::realloc(p2.cast::<c_void>(), 256).cast::<u8>();
    if p3.is_null() {
        return false;
    }

    unsafe {
        for i in 0..32 {
            if *p3.add(i) != i as u8 {
                malloc::dealloc(p3.cast::<c_void>());
                return false;
            }
        }
    }

    malloc::dealloc(p3.cast::<c_void>());
    true
}

fn test_small_recycling() -> bool {
    let a = malloc::alloc(64).cast::<u8>();
    if a.is_null() {
        return false;
    }
    malloc::dealloc(a.cast::<c_void>());

    let b = malloc::alloc(64).cast::<u8>();
    if b.is_null() {
        return false;
    }
    malloc::dealloc(b.cast::<c_void>());
    true
}

fn main() {
    let tests: &[fn() -> bool] = &[
        test_alloc_dealloc_basic,
        test_forward_coalesce,
        test_backward_coalesce,
        test_format_pattern_stability,
        test_mmap_fallback,
        test_realloc_grow,
        test_small_recycling,
    ];

    for test_fn in tests {
        if !test_fn() {
            std::process::exit(1);
        }
    }
    std::process::exit(0);
}
