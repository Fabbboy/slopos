//! Interval-merging OOO reassembly tracker (Phase 5).
//!
//! The [`Assembler`] records which byte ranges of the receive window have
//! arrived out of order.  It stores up to [`ASSEMBLER_MAX_RANGES`] disjoint
//! `(start_seq, end_seq)` intervals, sorted by start sequence number.
//! Overlapping or adjacent inserts are merged in-place.
//!
//! Payload bytes are written directly into the connection's receive ring
//! buffer at their final offset by the caller — the Assembler never touches
//! payload data, only tracks coverage.

use super::seq::{seq_ge, seq_gt, seq_le, seq_lt};

/// Maximum number of disjoint byte ranges tracked per connection.
pub const ASSEMBLER_MAX_RANGES: usize = 16;

/// Interval-merging tracker for out-of-order received byte ranges.
///
/// Each entry is an `(start_seq, end_seq)` half-open interval in absolute
/// TCP sequence-number space.  The array is kept sorted by `start_seq`
/// (wrapping-aware) with the invariant that no two entries overlap or are
/// adjacent — `insert` merges eagerly.
///
/// Size: 16 × 8 + 8 = 136 bytes (down from 11 696 bytes for `TcpOooQueue`).
#[derive(Clone, Copy)]
pub struct Assembler {
    ranges: [(u32, u32); ASSEMBLER_MAX_RANGES],
    count: usize,
}

impl core::fmt::Debug for Assembler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list()
            .entries(self.ranges[..self.count].iter().map(|(s, e)| (*s, *e)))
            .finish()
    }
}

impl Assembler {
    pub const fn new() -> Self {
        Self {
            ranges: [(0, 0); ASSEMBLER_MAX_RANGES],
            count: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Number of tracked disjoint intervals (exposed for tests).
    pub fn range_count(&self) -> usize {
        self.count
    }

    /// Record that bytes `[seq, seq+len)` have been received.
    ///
    /// Merges with any overlapping or adjacent existing intervals.  If the
    /// table is full and the new range does not merge with anything, the
    /// highest-sequence interval is evicted (keeps segments closest to the
    /// reassembly frontier).
    pub fn insert(&mut self, seq: u32, len: usize) {
        if len == 0 {
            return;
        }
        let mut new_start = seq;
        let mut new_end = seq.wrapping_add(len as u32);

        // Scan for overlapping or adjacent intervals and merge.
        let mut i = 0;
        while i < self.count {
            let (s, e) = self.ranges[i];
            // Overlap or adjacent: s <= new_end && new_start <= e
            if seq_le(s, new_end) && seq_le(new_start, e) {
                // Merge.
                if seq_lt(s, new_start) {
                    new_start = s;
                }
                if seq_gt(e, new_end) {
                    new_end = e;
                }
                // Remove entry i by swapping with last and shrinking.
                self.count -= 1;
                if i < self.count {
                    self.ranges[i] = self.ranges[self.count];
                }
                // Don't advance i — re-check the swapped-in entry.
                continue;
            }
            i += 1;
        }

        // Evict if full (no merge happened that freed a slot).
        if self.count >= ASSEMBLER_MAX_RANGES {
            // Evict the highest-seq interval to favour ranges near the gap.
            let mut evict = 0;
            for j in 1..self.count {
                if seq_gt(self.ranges[j].0, self.ranges[evict].0) {
                    evict = j;
                }
            }
            // Only evict if the new range is closer to the gap.
            if seq_ge(new_start, self.ranges[evict].0) {
                return;
            }
            self.count -= 1;
            if evict < self.count {
                self.ranges[evict] = self.ranges[self.count];
            }
        }

        // Insert the merged interval and restore sorted order.
        self.ranges[self.count] = (new_start, new_end);
        self.count += 1;

        // Insertion-sort the last element into place (n ≤ 16).
        let mut j = self.count - 1;
        while j > 0 && seq_gt(self.ranges[j - 1].0, self.ranges[j].0) {
            self.ranges.swap(j - 1, j);
            j -= 1;
        }
    }

    /// Consume contiguous coverage starting at `rcv_nxt`.
    ///
    /// If the lowest interval starts at (or before) `rcv_nxt`, it is removed
    /// and its length past `rcv_nxt` is returned.  The loop repeats for any
    /// chained contiguous intervals (defensive — `insert` merges eagerly so
    /// at most one interval should match).
    ///
    /// The caller is responsible for advancing `head`/`count` on the ring
    /// buffer and updating `rcv_nxt`.
    pub fn drain_contiguous(&mut self, rcv_nxt: u32) -> usize {
        let mut total = 0usize;
        loop {
            if self.count == 0 {
                break;
            }
            let (start, end) = self.ranges[0];
            let expected = rcv_nxt.wrapping_add(total as u32);
            // Allow start <= expected (handles partial overlap / trimming).
            if seq_gt(start, expected) {
                break;
            }
            // Bytes contributed: from expected to end.
            if seq_le(end, expected) {
                // Entirely below rcv_nxt — stale; just remove.
                self.remove_first();
                continue;
            }
            let contributed = end.wrapping_sub(expected) as usize;
            total += contributed;
            self.remove_first();
        }
        total
    }

    /// Return up to 4 SACK blocks from the lowest intervals.
    pub fn sack_blocks(&self) -> ([(u32, u32); 4], u8) {
        let mut blocks = [(0u32, 0u32); 4];
        let n = core::cmp::min(self.count, 4);
        for i in 0..n {
            blocks[i] = self.ranges[i];
        }
        (blocks, n as u8)
    }

    pub fn clear(&mut self) {
        self.count = 0;
    }

    /// Remove the first element and shift everything left.
    fn remove_first(&mut self) {
        if self.count == 0 {
            return;
        }
        self.count -= 1;
        // Shift left to preserve sorted order.
        let mut i = 0;
        while i < self.count {
            self.ranges[i] = self.ranges[i + 1];
            i += 1;
        }
    }
}
