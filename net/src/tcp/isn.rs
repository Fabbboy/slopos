//! Initial Sequence Number generator — RFC 6528.
//!
//! ```text
//!     ISN = SipHash-2-4(key, 4-tuple) + (monotonic_ns / 4µs)
//! ```
//!
//! The key is 128 bits drawn once from the kernel CSPRNG, so the output is a
//! keyed PRF of the connection identifier: an observer who collects ISNs
//! recovers nothing about the key and so cannot predict the ISN of a tuple it
//! has not seen.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::TcpTuple;
use super::siphash::siphash24;

const KEY_UNSET: usize = 0;
const KEY_SEEDING: usize = 1;
const KEY_READY: usize = 2;

static ISN_KEY_STATE: AtomicUsize = AtomicUsize::new(KEY_UNSET);
static ISN_KEY0: AtomicU64 = AtomicU64::new(0);
static ISN_KEY1: AtomicU64 = AtomicU64::new(0);

/// The key must be one consistent pair: a second caller racing the seeder must
/// wait rather than read a half-written key and hash under it.
fn isn_key() -> (u64, u64) {
    loop {
        match ISN_KEY_STATE.load(Ordering::Acquire) {
            KEY_READY => {
                return (
                    ISN_KEY0.load(Ordering::Relaxed),
                    ISN_KEY1.load(Ordering::Relaxed),
                );
            }
            KEY_UNSET => {
                if ISN_KEY_STATE
                    .compare_exchange(KEY_UNSET, KEY_SEEDING, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    let k0 = slopos_kernel_services::platform::rng_next();
                    let k1 = slopos_kernel_services::platform::rng_next();
                    ISN_KEY0.store(k0, Ordering::Relaxed);
                    ISN_KEY1.store(k1, Ordering::Relaxed);
                    ISN_KEY_STATE.store(KEY_READY, Ordering::Release);
                    return (k0, k1);
                }
            }
            _ => core::hint::spin_loop(),
        }
    }
}

/// Reset the key to the uninitialized sentinel, so [`super::tcp_reset_all`]
/// callers see a freshly seeded key on the next call.
pub(crate) fn reset_for_tests() {
    ISN_KEY_STATE.store(KEY_UNSET, Ordering::Release);
}

pub(crate) fn generate_isn(tuple: &TcpTuple) -> u32 {
    let (k0, k1) = isn_key();
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
    let h = siphash24(k0, k1, &tuple_bytes);
    // RFC 6528 §3: a 4-microsecond clock drift so a re-used 4-tuple still
    // lands on a different ISN after TIME_WAIT expires.
    let drift = (slopos_kernel_services::clock::monotonic_ns() / 4_000) as u32;
    (h as u32).wrapping_add(drift)
}
