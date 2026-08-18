//! Initial Sequence Number generator — RFC 6528 in spirit.
//!
//! ```text
//!     ISN = FNV-mix(4-tuple || boot_secret) + (monotonic_ns / 4µs)
//! ```
//!
//! `boot_secret` is seeded once from [`slopos_kernel_services::clock::monotonic_ns`],
//! which the HPET/TSC drives; a race between two cores is harmless, since
//! whichever write wins is still a fresh unpredictable secret.
//!
//! Intentionally **not** a keyed hash — SlopOS ships no SipHash/Blake3
//! primitive.

use core::sync::atomic::{AtomicU64, Ordering};

use super::TcpTuple;

/// Per-boot secret; `0` means "not yet initialized" and the first caller
/// seeds it.  Loads are `Relaxed`: any non-zero value is a valid secret.
static ISN_SECRET: AtomicU64 = AtomicU64::new(0);

fn boot_secret() -> u64 {
    let s = ISN_SECRET.load(Ordering::Relaxed);
    if s != 0 {
        return s;
    }
    // Mixing through the golden ratio spreads a low near-boot nanosecond
    // count across all 64 bits.
    let seed = slopos_kernel_services::clock::monotonic_ns()
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0x243F_6A88_85A3_08D3);
    let seed = if seed == 0 { 1 } else { seed };
    ISN_SECRET.store(seed, Ordering::Relaxed);
    seed
}

/// Reset the secret to the uninitialized sentinel, so [`super::tcp_reset_all`]
/// callers see a freshly seeded secret on the next call.
pub(crate) fn reset_for_tests() {
    ISN_SECRET.store(0, Ordering::Relaxed);
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

#[inline]
fn fnv_mix(mut h: u64, byte: u8) -> u64 {
    h ^= byte as u64;
    h.wrapping_mul(FNV_PRIME)
}

pub(crate) fn generate_isn(tuple: &TcpTuple) -> u32 {
    let mut h = FNV_OFFSET ^ boot_secret();
    // Network byte order, so mirrored-endian hosts hash the same tuple alike.
    let tuple_bytes: [u8; 12] = [
        tuple.local_ip[0],
        tuple.local_ip[1],
        tuple.local_ip[2],
        tuple.local_ip[3],
        (tuple.local_port >> 8) as u8,
        tuple.local_port as u8,
        tuple.remote_ip[0],
        tuple.remote_ip[1],
        tuple.remote_ip[2],
        tuple.remote_ip[3],
        (tuple.remote_port >> 8) as u8,
        tuple.remote_port as u8,
    ];
    for b in tuple_bytes {
        h = fnv_mix(h, b);
    }
    // RFC 6528 §3: a 4-microsecond clock drift so a re-used 4-tuple still
    // lands on a different ISN after TIME_WAIT expires.
    let drift = (slopos_kernel_services::clock::monotonic_ns() / 4_000) as u32;
    (h as u32).wrapping_add(drift)
}
