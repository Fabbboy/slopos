//! Lock-free TX zero-copy reclaim token (SlopRing `OP_SEND_ZC` /
//! `SLOPRING_CQE_F_NOTIF` — the MSG_ZEROCOPY notification model).
//!
//! A zero-copy send hands the NIC driver pinned user pages to DMA directly, so
//! the kernel must hold those pages pinned until the NIC is done with them.
//! Two constraints shape the signal:
//!
//!  * The driver crate must **not** depend on `ring` (layering), and TX-complete
//!    reclaim runs in NAPI/IRQ context where posting a CQE is impossible.
//!  * SlopRing has no async IRQ-driven CQE path; completions are harvested by
//!    re-polling in `ring_enter`.
//!
//! So the driver only **flips a token** when it reclaims the TX buffer from the
//! used ring; the ring's harvest re-poll observes the flip and posts the
//! `SLOPRING_CQE_F_NOTIF` CQE that tells userland the buffer is reusable. This
//! is the entire driver→ring completion signal: a single lock-free atomic bump,
//! no new async machinery. The shared counter lives behind a [`KArc`] so the
//! submitting op (ring side) and the driver TX slot can both hold it; the last
//! holder drops it.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::KArc;

/// Shared, refcounted reclaim signal for one in-flight zero-copy send. Cheap to
/// `Clone` (bumps the `KArc`); the ring keeps one handle (with the snapshot it
/// took at submit) and the driver TX slot keeps another.
#[derive(Clone)]
pub struct TxReclaimToken {
    /// Monotonic reclaim counter. The ring snapshots it at submit; the driver
    /// `fetch_add`s it once on reclaim. `is_reclaimed` compares against the
    /// snapshot, so it is robust even if the same token is reused.
    generation: KArc<AtomicU64>,
}

impl TxReclaimToken {
    /// Allocate a fresh token (counter starts at 0). `None` if the heap refuses.
    pub fn new() -> Option<Self> {
        Some(Self {
            generation: KArc::try_new(AtomicU64::new(0)).ok()?,
        })
    }

    /// Snapshot the current reclaim counter — taken on the ring side at submit.
    /// A later `is_reclaimed(snapshot)` returning `true` means the driver has
    /// reclaimed the buffer since.
    pub fn snapshot(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Driver-side: the NIC is done with the pinned buffer (popped from the TX
    /// used ring). The `Release` orders all prior buffer/used-ring reads before
    /// the bump the ring observes with its `Acquire` `is_reclaimed`.
    pub fn signal_reclaimed(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Ring-side: has the driver reclaimed the buffer since `at` was taken?
    pub fn is_reclaimed(&self, at: u64) -> bool {
        self.generation.load(Ordering::Acquire) != at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_then_reclaim_observed() {
        let t = TxReclaimToken::new().unwrap();
        let at = t.snapshot();
        assert!(
            !t.is_reclaimed(at),
            "fresh token must not read as reclaimed"
        );
        t.signal_reclaimed();
        assert!(t.is_reclaimed(at), "post-signal must read as reclaimed");
    }

    #[test]
    fn clone_shares_the_counter() {
        let driver = TxReclaimToken::new().unwrap();
        let ring = driver.clone();
        let at = ring.snapshot();
        driver.signal_reclaimed();
        assert!(
            ring.is_reclaimed(at),
            "a signal on one handle is visible on the clone"
        );
    }

    #[test]
    fn snapshot_after_reclaim_is_stable() {
        let t = TxReclaimToken::new().unwrap();
        t.signal_reclaimed();
        // A fresh snapshot taken *after* a reclaim is not itself reclaimed until
        // the next signal — the comparison is against the snapshot value, so a
        // reused token never reports a stale reclaim.
        let at = t.snapshot();
        assert!(!t.is_reclaimed(at));
        t.signal_reclaimed();
        assert!(t.is_reclaimed(at));
    }
}
