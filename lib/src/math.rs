/// Absolute value for 32-bit integers.
#[inline(always)]
pub const fn abs_i32(x: i32) -> i32 {
    if x < 0 {
        // Matches C semantics; i32::MIN will overflow just like C's abs.
        -x
    } else {
        x
    }
}

/// Minimum of two 32-bit integers.
#[inline(always)]
pub const fn min_i32(a: i32, b: i32) -> i32 {
    if a < b {
        a
    } else {
        b
    }
}

/// Maximum of two 32-bit integers.
#[inline(always)]
pub const fn max_i32(a: i32, b: i32) -> i32 {
    if a > b {
        a
    } else {
        b
    }
}

/// Minimum of two unsigned 32-bit integers.
#[inline(always)]
pub const fn min_u32(a: u32, b: u32) -> u32 {
    if a < b {
        a
    } else {
        b
    }
}

/// Maximum of two unsigned 32-bit integers.
#[inline(always)]
pub const fn max_u32(a: u32, b: u32) -> u32 {
    if a > b {
        a
    } else {
        b
    }
}
