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

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

/// Refcounted zero-copy notification token for sends whose pinned pages may be
/// DMA'd by the NIC **more than once** before they are reusable — the TCP
/// `MSG_ZEROCOPY` case, where a segment can be (re)transmitted N times and the
/// buffer is reusable only when the bytes are cumulatively ACKed **and** no NIC
/// TX descriptor still references the pages.
///
/// The single-shot [`TxReclaimToken`] (one DMA, one reclaim) cannot express
/// that: a retransmit issues a second DMA of the same pages, so a generation
/// flip on the first reclaim would free the buffer while the second DMA is still
/// reading it. This token instead holds a **reference count**:
///
///  * `new()` starts at `1` — the send-queue chunk's own reference, held until
///    the bytes are cumulatively ACKed (or the connection is torn down).
///  * `acquire()` bumps it for each (re)transmit handed to the driver (one per
///    in-flight NIC TX descriptor).
///  * `release()` drops a NIC reference when the driver reclaims that TX
///    descriptor (`virtnet_clean_tx`).
///  * `mark_acked_and_release()` drops the chunk's own reference on cumulative
///    ACK / teardown.
///
/// Because the chunk holds a reference until ACK, the count stays `>= 1` for the
/// whole retransmit window and reaches `0` **only** once the bytes are ACKed and
/// every outstanding DMA has been reclaimed — exactly the `SLOPRING_CQE_F_NOTIF`
/// "buffer reusable" condition the ring's harvest polls via [`is_notifiable`].
///
/// [`is_notifiable`]: ZcNotifToken::is_notifiable
#[derive(Clone)]
pub struct ZcNotifToken {
    /// Live references: the owning chunk (1) plus one per in-flight NIC DMA.
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

    /// Add a reference for one in-flight NIC DMA (an initial transmit or a
    /// retransmit handed to the driver). Paired with a later [`release`] from the
    /// driver's TX reclaim.
    ///
    /// [`release`]: ZcNotifToken::release
    pub fn acquire(&self) {
        self.refs.fetch_add(1, Ordering::AcqRel);
    }

    /// Drop a NIC-DMA reference — the driver reclaimed one TX descriptor that
    /// referenced the pinned pages. The `Release` orders the driver's prior
    /// used-ring reads before the ring observes the count via its `Acquire`
    /// [`is_notifiable`].
    ///
    /// [`is_notifiable`]: ZcNotifToken::is_notifiable
    pub fn release(&self) {
        // Saturating: the protocol never under-counts, but never underflow.
        let prev = self.refs.load(Ordering::Acquire);
        if prev > 0 {
            self.refs.fetch_sub(1, Ordering::Release);
        }
    }

    /// Drop the send-queue chunk's own reference, on cumulative ACK or teardown.
    /// Semantically distinct from [`release`] (it retires the chunk, not a DMA)
    /// but the same count operation; kept separate so the call sites read clearly
    /// and so the protocol's "exactly one chunk reference" discipline is
    /// auditable.
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
        // A fresh snapshot taken *after* a reclaim is not itself reclaimed until
        // the next signal — the comparison is against the snapshot value, so a
        // reused token never reports a stale reclaim.
        let at = t.snapshot();
        assert!(!t.is_reclaimed(at));
        t.signal_reclaimed();
        assert!(t.is_reclaimed(at));
    }

    #[test]
    fn notif_token_held_until_ack_and_all_dmas_reclaimed() {
        let t = ZcNotifToken::new().unwrap();
        // Fresh chunk: one reference, not notifiable.
        assert!(!t.is_notifiable());
        // Two (re)transmits in flight.
        t.acquire();
        t.acquire();
        assert!(!t.is_notifiable());
        // Both DMAs reclaimed, but the chunk is not yet ACKed → still held.
        t.release();
        t.release();
        assert!(
            !t.is_notifiable(),
            "chunk reference must keep it held until ACK"
        );
        // Cumulative ACK drops the chunk reference → now notifiable.
        t.mark_acked_and_release();
        assert!(t.is_notifiable());
    }

    #[test]
    fn notif_token_ack_before_dma_reclaim_waits_for_reclaim() {
        let t = ZcNotifToken::new().unwrap();
        t.acquire(); // one DMA in flight
        // ACK arrives before the in-flight retransmit's TX descriptor is
        // reclaimed: chunk ref dropped, but the DMA ref keeps it held.
        t.mark_acked_and_release();
        assert!(
            !t.is_notifiable(),
            "must not free the buffer while the NIC may still DMA it"
        );
        t.release(); // driver reclaims the last descriptor
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
