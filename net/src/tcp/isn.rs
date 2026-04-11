//! Initial Sequence Number generator — RFC 6528 in spirit.
//!
//! Replaces the old `ISN_COUNTER.fetch_add(64000)` scheme (tracked as
//! `SLOPOS-2026-0007` in the CVSS ledger) with a per-tuple hash that is
//! unpredictable to off-path attackers:
//!
//! ```text
//!     ISN = FNV-mix(4-tuple || boot_secret) + (monotonic_ns / 4µs)
//! ```
//!
//! - `boot_secret` is a 64-bit value seeded once at first call from
//!   [`slopos_utils::clock::monotonic_ns`], which is itself driven by the
//!   HPET/TSC and unpredictable to a remote observer.  A successful race on
//!   two cores during initialization is harmless: whichever write wins is
//!   still a fresh unpredictable secret.
//! - The 4-µs timer drift ensures that retransmitted SYNs against the same
//!   tuple over wall time still yield distinct ISNs, satisfying RFC 6528's
//!   "same tuple → monotonic over time" intent without maintaining any
//!   per-tuple state.
//!
//! This is intentionally **not** a keyed hash — SlopOS does not currently
//! ship a SipHash/Blake3 primitive and adding one for the ISN path alone is
//! overkill for a hobby kernel.  The design is strictly better than the
//! predictable counter it replaces; a future upgrade can swap in a real
//! keyed hash without touching callers.

use core::sync::atomic::{AtomicU64, Ordering};

use super::TcpTuple;

/// Per-boot secret.  A value of `0` means "not yet initialized" — the first
/// caller seeds it.  Loads are `Relaxed` because the only invariant we care
/// about is "any non-zero value is a valid secret".
static ISN_SECRET: AtomicU64 = AtomicU64::new(0);

fn boot_secret() -> u64 {
    let s = ISN_SECRET.load(Ordering::Relaxed);
    if s != 0 {
        return s;
    }
    // Mix the high-resolution clock through the fractional part of the
    // golden ratio (0x9E37_79B9_7F4A_7C15) so that a low nanosecond count
    // near boot still spreads entropy across all 64 bits.
    let seed = slopos_utils::clock::monotonic_ns()
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0x243F_6A88_85A3_08D3);
    // Guarantee non-zero so subsequent loads short-circuit.
    let seed = if seed == 0 { 1 } else { seed };
    ISN_SECRET.store(seed, Ordering::Relaxed);
    seed
}

/// Reset the secret to the uninitialized sentinel.  Called by
/// [`super::tcp_reset_all`] so regression tests (and in-kernel reinit
/// paths) see a freshly seeded secret on the next call.  Cheap enough to
/// leave ungated — costs one relaxed store per invocation.
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

/// Generate an Initial Sequence Number for a connection identified by
/// `tuple`.  See the module-level documentation for the algorithm.
pub(crate) fn generate_isn(tuple: &TcpTuple) -> u32 {
    let mut h = FNV_OFFSET ^ boot_secret();
    // Serialize the 4-tuple in network byte order so two hosts with mirrored
    // endian representations produce comparable values on the wire.
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
    // RFC 6528 §3: add a 4-microsecond clock drift so a re-used 4-tuple
    // still lands on a different ISN after TIME_WAIT expires.
    //
    // monotonic_ns / 4_000 ≈ 4 µs tick resolution.
    let drift = (slopos_utils::clock::monotonic_ns() / 4_000) as u32;
    (h as u32).wrapping_add(drift)
}
