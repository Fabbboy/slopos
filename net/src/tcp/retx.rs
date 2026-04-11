//! Retransmission queue for in-flight TCP segments.
//!
//! Tracks every segment that has been put on the wire but not yet
//! cumulatively acknowledged by the peer.  Feeds three things:
//!
//! 1. **RTT sampling.**  When an ACK arrives, the queue reports the
//!    earliest-sent byte that was just acked so the [`super::rtt::RttEstimator`]
//!    can compute `now - sent_at` for that segment (subject to Karn's
//!    rule: retransmitted segments are skipped).
//! 2. **Fast retransmit.**  The [`super::pcb::DataState`] counts
//!    duplicate ACKs; on the third one it calls [`RetxQueue::oldest_unacked`]
//!    to find the byte at `snd_una` and retransmits exactly that segment
//!    before the RTO fires.
//! 3. **RTO-driven retransmit.**  When the retransmission timer expires,
//!    [`RetxQueue::mark_all_for_retransmit`] flags the whole in-flight
//!    queue; the next `poll_transmit` starts at `snd_una` again.
//!
//! ## Storage
//!
//! An inline `[RetxEntry; CAP]` with `CAP = 16`.  16 * MSS ≈ 23 KB of
//! in-flight data is the practical upper bound for a 32 KB send buffer
//! with NewReno's default cwnd, so 16 slots is generous without burning
//! memory.  A full queue causes new sends to block until the oldest slot
//! is acked — i.e. the congestion-control loop catches up.

use crate::tcp::seq::SeqNum;

/// Maximum number of in-flight retransmit entries tracked per connection.
pub const RETX_CAPACITY: usize = 16;

/// One in-flight segment's metadata.  The payload itself lives in the
/// send ring buffer (`TcpSendState::buf`) — this entry only remembers
/// the sequence-space slice it covers plus the wall time at which it
/// was first transmitted.
#[derive(Clone, Copy, Debug, Default)]
pub struct RetxEntry {
    /// First sequence number covered by this entry.
    pub seq: SeqNum,
    /// Byte length of the segment (data only; SYN/FIN are tracked
    /// separately via the state-machine `snd_nxt` advances).
    pub len: u32,
    /// Timestamp (`now_ms`) when this segment was first transmitted.
    /// Never updated on retransmit — needed for Karn's algorithm.
    pub first_send_ms: u64,
    /// `true` if this entry has been retransmitted at least once;
    /// Karn's rule says we can't sample RTT from its ACK.
    pub retransmitted: bool,
}

/// The per-connection retransmit queue.
///
/// Invariant: entries are stored in emit order (== sequence order, modulo
/// wrap) contiguously from index `0` to `len-1`.  Removing the head
/// shifts the remainder down — since `CAP` is only 16, the `O(n)` cost
/// is negligible.
#[derive(Clone, Copy, Debug)]
pub struct RetxQueue {
    entries: [RetxEntry; RETX_CAPACITY],
    len: u8,
    /// Byte total of all entries currently in the queue.  Maintained
    /// incrementally by [`RetxQueue::push_sent`] / [`RetxQueue::on_ack`]
    /// so callers can assert
    /// `inflight_bytes() == snd_nxt - snd_una`.
    inflight: u32,
}

impl Default for RetxQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl RetxQueue {
    /// Create an empty queue.  Usable from `const` context.
    pub const fn new() -> Self {
        Self {
            entries: [RetxEntry {
                seq: SeqNum::ZERO,
                len: 0,
                first_send_ms: 0,
                retransmitted: false,
            }; RETX_CAPACITY],
            len: 0,
            inflight: 0,
        }
    }

    /// Current number of entries in the queue.
    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// True iff no segments are currently in flight.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of free slots remaining.
    #[inline]
    pub fn capacity_remaining(&self) -> usize {
        RETX_CAPACITY - self.len as usize
    }

    /// Total bytes in flight.  Must equal `snd_nxt - snd_una` in the
    /// containing `DataState`; an [`assert!`] in the state machine
    /// verifies this after every transition.
    #[inline]
    pub fn inflight_bytes(&self) -> u32 {
        self.inflight
    }

    /// Read-only view of the oldest unacked segment, or `None` if the
    /// queue is empty.  Used by fast retransmit to find the segment
    /// that should be resent when three duplicate ACKs arrive.
    pub fn oldest_unacked(&self) -> Option<&RetxEntry> {
        if self.len == 0 {
            None
        } else {
            Some(&self.entries[0])
        }
    }

    /// Record that a segment of `len` bytes starting at `seq` has been
    /// put on the wire at time `now_ms`.  Returns `Err(())` if the
    /// queue is full — callers should treat that as a transient back
    /// pressure signal and defer the send.
    pub fn push_sent(&mut self, seq: SeqNum, len: u32, now_ms: u64) -> Result<(), ()> {
        if self.len as usize >= RETX_CAPACITY {
            return Err(());
        }
        let idx = self.len as usize;
        self.entries[idx] = RetxEntry {
            seq,
            len,
            first_send_ms: now_ms,
            retransmitted: false,
        };
        self.len += 1;
        self.inflight = self.inflight.saturating_add(len);
        Ok(())
    }

    /// Process a cumulative ACK covering everything up to (but not
    /// including) `up_to`.  Returns the [`AckOutcome`] describing
    /// which byte range was freed and whether the oldest freed entry
    /// was eligible for an RTT sample (Karn's rule).
    pub fn on_ack(&mut self, up_to: SeqNum) -> AckOutcome {
        if self.len == 0 {
            return AckOutcome::default();
        }

        let mut bytes_freed = 0u32;
        let mut rtt_sample_origin: Option<u64> = None;
        let mut rtt_sampleable = false;
        let mut drop_count = 0usize;

        for i in 0..self.len as usize {
            let e = &self.entries[i];
            let end = e.seq + e.len;
            // Fully acked entry: up_to >= end (in wrapping-cmp semantics).
            if up_to >= end {
                bytes_freed = bytes_freed.saturating_add(e.len);
                if drop_count == 0 && !e.retransmitted {
                    rtt_sample_origin = Some(e.first_send_ms);
                    rtt_sampleable = true;
                }
                drop_count += 1;
            } else if up_to > e.seq {
                // Partial ACK within an entry: shrink its head.  Only
                // valid for the first entry — if a mid-queue entry
                // gets partial ACK it means the queue is out of order,
                // which is impossible under TCP's cumulative ACK
                // semantics.
                debug_assert!(i == drop_count, "partial ACK outside queue head");
                if i == drop_count {
                    let delta = up_to - e.seq;
                    let e_mut = &mut self.entries[i];
                    e_mut.seq = e_mut.seq + delta;
                    e_mut.len -= delta;
                    bytes_freed = bytes_freed.saturating_add(delta);
                }
                break;
            } else {
                // up_to <= e.seq: entry untouched; everything after is too.
                break;
            }
        }

        // Shift the tail down over the fully-dropped entries.
        if drop_count > 0 {
            for i in 0..(self.len as usize - drop_count) {
                self.entries[i] = self.entries[i + drop_count];
            }
            self.len -= drop_count as u8;
        }

        self.inflight = self.inflight.saturating_sub(bytes_freed);

        AckOutcome {
            bytes_freed,
            rtt_sample_origin_ms: if rtt_sampleable {
                rtt_sample_origin
            } else {
                None
            },
            entries_removed: drop_count as u8,
        }
    }

    /// Called from the RTO timeout path: the whole queue needs to be
    /// resent.  Marks every entry as retransmitted (disqualifying them
    /// from future RTT samples per Karn) but leaves the data in place
    /// so `poll_transmit` can walk it from the head again.
    pub fn mark_all_for_retransmit(&mut self) {
        for i in 0..self.len as usize {
            self.entries[i].retransmitted = true;
        }
    }

    /// Mark the oldest unacked entry as retransmitted (fast retransmit).
    /// Noop if empty.
    pub fn mark_oldest_retransmitted(&mut self) {
        if self.len > 0 {
            self.entries[0].retransmitted = true;
        }
    }

    /// Drop everything.  Used on connection close and test reset.
    pub fn clear(&mut self) {
        self.len = 0;
        self.inflight = 0;
    }
}

/// Result of applying a cumulative ACK to the retransmit queue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AckOutcome {
    /// Number of bytes freed from the inflight pool by this ACK.
    pub bytes_freed: u32,
    /// Timestamp of the oldest-freed entry's first transmission, or
    /// `None` if the entry was retransmitted (Karn) or no entry was freed.
    /// The RTT estimator samples `now - this` when present.
    pub rtt_sample_origin_ms: Option<u64>,
    /// Number of entries popped from the head of the queue.
    pub entries_removed: u8,
}
