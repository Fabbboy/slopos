//! Lock-free TX zero-copy reclaim token (SlopRing `OP_SEND_ZC` /
//! `SLOPRING_CQE_F_NOTIF` — the MSG_ZEROCOPY notification model).
//!
//! The driver crate may not depend on `ring`, and TX-complete reclaim runs in
//! NAPI/IRQ context where posting a CQE is impossible, so the driver only flips
//! a token when it reclaims the TX buffer; the ring's harvest re-poll observes
//! the flip and posts the CQE that tells userland the buffer is reusable.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::KArc;

/// Shared, refcounted reclaim signal for one in-flight zero-copy send: the ring
/// keeps one handle (with the snapshot it took at submit), the driver TX slot
/// keeps the other.
#[derive(Clone)]
pub struct TxReclaimToken {
    /// Monotonic reclaim counter. `is_reclaimed` compares against a snapshot,
    /// so a reused token cannot report a stale reclaim.
    generation: KArc<AtomicU64>,
}

impl TxReclaimToken {
    /// Allocate a fresh token. `None` if the heap refuses.
    pub fn new() -> Option<Self> {
        Some(Self {
            generation: KArc::try_new(AtomicU64::new(0)).ok()?,
        })
    }

    /// Snapshot the reclaim counter — taken on the ring side at submit.
    pub fn snapshot(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Driver-side: the NIC is done with the pinned buffer. The `Release`
    /// orders all prior buffer/used-ring reads before the bump the ring
    /// observes with its `Acquire` `is_reclaimed`.
    pub fn signal_reclaimed(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Ring-side: has the driver reclaimed the buffer since `at` was taken?
    pub fn is_reclaimed(&self, at: u64) -> bool {
        self.generation.load(Ordering::Acquire) != at
    }
}

/// Refcounted zero-copy notification token for sends whose pinned pages may be
/// DMA'd by the NIC **more than once** — the TCP `MSG_ZEROCOPY` case, where a
/// retransmit issues a second DMA of the same pages and the single-shot
/// [`TxReclaimToken`] would free the buffer under it.
///
/// The count is the send-queue chunk's own reference plus one per in-flight NIC
/// DMA, so it reaches `0` only once the bytes are cumulatively ACKed and every
/// outstanding DMA has been reclaimed — the `SLOPRING_CQE_F_NOTIF` "buffer
/// reusable" condition the ring's harvest polls via [`is_notifiable`].
///
/// [`is_notifiable`]: ZcNotifToken::is_notifiable
#[derive(Clone)]
pub struct ZcNotifToken {
    refs: KArc<AtomicUsize>,
}

impl ZcNotifToken {
    /// Allocate a fresh token owning a single reference (the send-queue chunk).
    /// `None` if the heap refuses.
    pub fn new() -> Option<Self> {
        Some(Self {
            refs: KArc::try_new(AtomicUsize::new(1)).ok()?,
        })
    }

    /// Add a reference for one in-flight NIC DMA, paired with a later
    /// [`release`] from the driver's TX reclaim.
    ///
    /// [`release`]: ZcNotifToken::release
    pub fn acquire(&self) {
        self.refs.fetch_add(1, Ordering::AcqRel);
    }

    /// Drop a NIC-DMA reference — the driver reclaimed one TX descriptor. The
    /// `Release` orders the driver's prior used-ring reads before the ring
    /// observes the count via its `Acquire` [`is_notifiable`].
    ///
    /// [`is_notifiable`]: ZcNotifToken::is_notifiable
    pub fn release(&self) {
        // TODO(tech-debt): the load/fetch_sub pair is not atomic — two
        // concurrent releases can both observe `prev > 0` and underflow.
        let prev = self.refs.load(Ordering::Acquire);
        if prev > 0 {
            self.refs.fetch_sub(1, Ordering::Release);
        }
    }

    /// Drop the send-queue chunk's own reference, on cumulative ACK or teardown.
    /// The same count operation as [`release`], kept separate so the "exactly
    /// one chunk reference" discipline is auditable.
    ///
    /// [`release`]: ZcNotifToken::release
    pub fn mark_acked_and_release(&self) {
        self.release();
    }

    /// Ring-side: is the buffer reusable — bytes ACKed (chunk reference dropped)
    /// and every in-flight DMA reclaimed (count back to zero)?
    pub fn is_notifiable(&self) -> bool {
        self.refs.load(Ordering::Acquire) == 0
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
        let at = t.snapshot();
        assert!(!t.is_reclaimed(at));
        t.signal_reclaimed();
        assert!(t.is_reclaimed(at));
    }

    #[test]
    fn notif_token_held_until_ack_and_all_dmas_reclaimed() {
        let t = ZcNotifToken::new().unwrap();
        assert!(!t.is_notifiable());
        t.acquire();
        t.acquire();
        assert!(!t.is_notifiable());
        t.release();
        t.release();
        assert!(
            !t.is_notifiable(),
            "chunk reference must keep it held until ACK"
        );
        t.mark_acked_and_release();
        assert!(t.is_notifiable());
    }

    #[test]
    fn notif_token_ack_before_dma_reclaim_waits_for_reclaim() {
        let t = ZcNotifToken::new().unwrap();
        t.acquire();
        t.mark_acked_and_release();
        assert!(
            !t.is_notifiable(),
            "must not free the buffer while the NIC may still DMA it"
        );
        t.release();
        assert!(t.is_notifiable());
    }

    #[test]
    fn notif_token_clone_shares_count() {
        let chunk = ZcNotifToken::new().unwrap();
        let driver = chunk.clone();
        chunk.acquire();
        driver.release();
        chunk.mark_acked_and_release();
        assert!(driver.is_notifiable(), "the count is shared across clones");
    }
}
