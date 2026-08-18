//! SACK-aware send map for in-flight TCP segments (RFC 6675).
//!
//! Per-segment state drives selective retransmission: SACK coverage confirms
//! entries, DupThresh confirmations past a hole declare it `Lost`, and an RTO
//! marks everything `Lost` rather than rewinding `snd_nxt`.
//!
//! Storage is an inline `[SendMapEntry; 32]` — 32 × MSS ≈ 46 KB in flight,
//! sized for a 64 KB send window. A full map blocks new sends until a
//! cumulative ACK frees the oldest slot.

use crate::tcp::seq::SeqNum;

/// Maximum number of in-flight entries tracked per connection.
pub const SENDMAP_CAPACITY: usize = 32;

/// DupThresh: number of SACKed entries past a hole to declare it lost.
const DUP_THRESH: usize = 3;

/// Lifecycle state of a single in-flight segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, slopos_ostd::Zeroable)]
#[repr(u8)]
pub enum SegmentState {
    /// Sent, not yet acknowledged or SACKed by the peer. **Discriminant 0**: a
    /// zero byte must be a valid `InFlight`, which [`SendMap`]'s zero-fill
    /// relies on.
    InFlight = 0,
    /// Covered by a SACK block from the peer.
    SackConfirmed = 1,
    /// Declared lost: ≥ DupThresh SACKed entries exist past this hole, or an
    /// RTO fired.
    Lost = 2,
    /// Was `Lost`, has been retransmitted, now in flight again.
    Retransmitted = 3,
}

impl Default for SegmentState {
    fn default() -> Self {
        Self::InFlight
    }
}

/// One in-flight segment's metadata; the payload lives in the send ring buffer.
#[derive(Clone, Copy, Debug, slopos_ostd::Zeroable)]
pub struct SendMapEntry {
    /// First sequence number covered by this entry.
    pub seq: SeqNum,
    /// Byte length of the segment (data only; SYN/FIN tracked separately).
    pub len: u32,
    /// Timestamp (`now_ms`) when this segment was first transmitted.
    /// Never updated on retransmit — needed for RTT sampling.
    pub first_send_ms: u64,
    pub state: SegmentState,
}

impl Default for SendMapEntry {
    fn default() -> Self {
        Self {
            seq: SeqNum::ZERO,
            len: 0,
            first_send_ms: 0,
            state: SegmentState::InFlight,
        }
    }
}

/// Per-connection send map tracking every unacknowledged segment.
///
/// Invariant: entries are stored in emit order (== sequence order) from index
/// `0` to `len-1`; removing the head shifts the remainder down.
#[derive(Clone, Copy, Debug, slopos_ostd::Zeroable)]
pub struct SendMap {
    entries: [SendMapEntry; SENDMAP_CAPACITY],
    len: u8,
}

impl Default for SendMap {
    fn default() -> Self {
        Self::new()
    }
}

impl SendMap {
    pub const fn new() -> Self {
        Self {
            entries: [SendMapEntry {
                seq: SeqNum::ZERO,
                len: 0,
                first_send_ms: 0,
                state: SegmentState::InFlight,
            }; SENDMAP_CAPACITY],
            len: 0,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn capacity_remaining(&self) -> usize {
        SENDMAP_CAPACITY - self.len as usize
    }

    /// Sum of ALL entry lengths regardless of state.
    /// Invariant target: `total_bytes() == snd_nxt - snd_una - fin_offset`.
    pub fn total_bytes(&self) -> u32 {
        let mut total = 0u32;
        for i in 0..self.len as usize {
            total = total.saturating_add(self.entries[i].len);
        }
        total
    }

    /// RFC 6675 "pipe" estimate: bytes believed to be in the network. Counts
    /// only `InFlight` and `Retransmitted` entries.
    pub fn pipe(&self) -> u32 {
        let mut p = 0u32;
        for i in 0..self.len as usize {
            match self.entries[i].state {
                SegmentState::InFlight | SegmentState::Retransmitted => {
                    p = p.saturating_add(self.entries[i].len);
                }
                _ => {}
            }
        }
        p
    }

    pub fn has_lost(&self) -> bool {
        for i in 0..self.len as usize {
            if self.entries[i].state == SegmentState::Lost {
                return true;
            }
        }
        false
    }

    pub fn next_lost(&self) -> Option<&SendMapEntry> {
        for i in 0..self.len as usize {
            if self.entries[i].state == SegmentState::Lost {
                return Some(&self.entries[i]);
            }
        }
        None
    }

    /// Record a segment put on the wire. Returns `Err(())` if the map is full.
    pub fn push_sent(&mut self, seq: SeqNum, len: u32, now_ms: u64) -> Result<(), ()> {
        if self.len as usize >= SENDMAP_CAPACITY {
            return Err(());
        }
        let idx = self.len as usize;
        self.entries[idx] = SendMapEntry {
            seq,
            len,
            first_send_ms: now_ms,
            state: SegmentState::InFlight,
        };
        self.len += 1;
        Ok(())
    }

    /// Process a cumulative ACK covering everything up to (but not including)
    /// `up_to`, removing fully-acked entries from the head.
    ///
    /// RTT samples are only taken from `InFlight` entries — retransmitted,
    /// SACK-confirmed and lost entries are ineligible.
    pub fn on_cumulative_ack(&mut self, up_to: SeqNum) -> AckOutcome {
        if self.len == 0 {
            return AckOutcome::default();
        }

        let mut bytes_freed = 0u32;
        let mut rtt_sample_origin: Option<u64> = None;
        let mut drop_count = 0usize;

        for i in 0..self.len as usize {
            let e = &self.entries[i];
            let end = e.seq + e.len;
            if up_to >= end {
                bytes_freed = bytes_freed.saturating_add(e.len);
                if drop_count == 0 && e.state == SegmentState::InFlight {
                    rtt_sample_origin = Some(e.first_send_ms);
                }
                drop_count += 1;
            } else if up_to > e.seq {
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
                break;
            }
        }

        if drop_count > 0 {
            for i in 0..(self.len as usize - drop_count) {
                self.entries[i] = self.entries[i + drop_count];
            }
            self.len -= drop_count as u8;
        }

        AckOutcome {
            bytes_freed,
            rtt_sample_origin_ms: rtt_sample_origin,
            entries_removed: drop_count as u8,
        }
    }

    /// Apply SACK blocks from the peer, then run loss detection.
    ///
    /// Only entries **fully** covered by a block are marked `SackConfirmed`.
    /// An `InFlight` entry with ≥ `DUP_THRESH` `SackConfirmed` entries after it
    /// is marked `Lost` (RFC 6675 §4). Returns `true` on a new loss.
    pub fn apply_sack_blocks(&mut self, blocks: &[(u32, u32)], count: u8) -> bool {
        let n = core::cmp::min(count as usize, blocks.len());
        if n == 0 || self.len == 0 {
            return false;
        }

        for bi in 0..n {
            let (left, right) = blocks[bi];
            if right <= left {
                continue;
            }
            for i in 0..self.len as usize {
                let e = &self.entries[i];
                if e.state == SegmentState::SackConfirmed || e.state == SegmentState::Lost {
                    continue;
                }
                let e_end = (e.seq + e.len).raw();
                if seq_le(left, e.seq.raw()) && seq_ge(right, e_end) {
                    self.entries[i].state = SegmentState::SackConfirmed;
                }
            }
        }

        let mut any_new_loss = false;
        for i in 0..self.len as usize {
            if self.entries[i].state != SegmentState::InFlight {
                continue;
            }
            let mut sack_count = 0usize;
            for j in (i + 1)..self.len as usize {
                if self.entries[j].state == SegmentState::SackConfirmed {
                    sack_count += 1;
                }
            }
            if sack_count >= DUP_THRESH {
                self.entries[i].state = SegmentState::Lost;
                any_new_loss = true;
            }
        }

        any_new_loss
    }

    /// RTO path: mark every entry as `Lost` so the transmit loop re-sends them
    /// selectively instead of doing go-back-N.
    pub fn mark_all_lost(&mut self) {
        for i in 0..self.len as usize {
            self.entries[i].state = SegmentState::Lost;
        }
    }

    pub fn mark_retransmitted(&mut self, seq: SeqNum) {
        for i in 0..self.len as usize {
            if self.entries[i].seq == seq && self.entries[i].state == SegmentState::Lost {
                self.entries[i].state = SegmentState::Retransmitted;
                return;
            }
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }
}

#[inline]
fn seq_le(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

#[inline]
fn seq_ge(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
}

/// Result of applying a cumulative ACK to the send map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AckOutcome {
    pub bytes_freed: u32,
    /// First transmission of the oldest-freed `InFlight` entry, or `None` if no
    /// eligible entry was freed.
    pub rtt_sample_origin_ms: Option<u64>,
    pub entries_removed: u8,
}
