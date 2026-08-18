//! Interval-merging out-of-order reassembly tracker.
//!
//! The [`Assembler`] tracks coverage only; payload bytes are written into the
//! connection's receive ring buffer at their final offset by the caller.

use super::seq::{seq_ge, seq_gt, seq_le, seq_lt};

pub const ASSEMBLER_MAX_RANGES: usize = 16;

/// Each entry is an `(start_seq, end_seq)` half-open interval in absolute TCP
/// sequence-number space, sorted by `start_seq` (wrapping-aware); `insert`
/// merges eagerly, so no two entries overlap or are adjacent.
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

    pub fn range_count(&self) -> usize {
        self.count
    }

    /// Record that bytes `[seq, seq+len)` have been received.
    ///
    /// When the table is full and the new range merges with nothing, the
    /// highest-sequence interval is evicted, keeping segments closest to the
    /// reassembly frontier.
    pub fn insert(&mut self, seq: u32, len: usize) {
        if len == 0 {
            return;
        }
        let mut new_start = seq;
        let mut new_end = seq.wrapping_add(len as u32);

        let mut i = 0;
        while i < self.count {
            let (s, e) = self.ranges[i];
            if seq_le(s, new_end) && seq_le(new_start, e) {
                if seq_lt(s, new_start) {
                    new_start = s;
                }
                if seq_gt(e, new_end) {
                    new_end = e;
                }
                self.count -= 1;
                if i < self.count {
                    self.ranges[i] = self.ranges[self.count];
                }
                // Don't advance i — re-check the swapped-in entry.
                continue;
            }
            i += 1;
        }

        if self.count >= ASSEMBLER_MAX_RANGES {
            let mut evict = 0;
            for j in 1..self.count {
                if seq_gt(self.ranges[j].0, self.ranges[evict].0) {
                    evict = j;
                }
            }
            if seq_ge(new_start, self.ranges[evict].0) {
                return;
            }
            self.count -= 1;
            if evict < self.count {
                self.ranges[evict] = self.ranges[self.count];
            }
        }

        self.ranges[self.count] = (new_start, new_end);
        self.count += 1;

        let mut j = self.count - 1;
        while j > 0 && seq_gt(self.ranges[j - 1].0, self.ranges[j].0) {
            self.ranges.swap(j - 1, j);
            j -= 1;
        }
    }

    /// Consume contiguous coverage starting at `rcv_nxt`.
    ///
    /// Returns the number of bytes past `rcv_nxt` now covered; the caller is
    /// responsible for advancing `head`/`count` on the ring buffer and
    /// updating `rcv_nxt`.
    pub fn drain_contiguous(&mut self, rcv_nxt: u32) -> usize {
        let mut total = 0usize;
        loop {
            if self.count == 0 {
                break;
            }
            let (start, end) = self.ranges[0];
            let expected = rcv_nxt.wrapping_add(total as u32);
            if seq_gt(start, expected) {
                break;
            }
            if seq_le(end, expected) {
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

    fn remove_first(&mut self) {
        if self.count == 0 {
            return;
        }
        self.count -= 1;
        let mut i = 0;
        while i < self.count {
            self.ranges[i] = self.ranges[i + 1];
            i += 1;
        }
    }
}
