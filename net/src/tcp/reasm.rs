//! Out-of-order reassembly queue (RFC 793 §3.9).
//!
//! A tiny fixed-size store of segments that arrived ahead of `rcv_nxt`.  When
//! the gap ahead of it fills, `drain_contiguous` walks the queue in sequence
//! order and pushes contiguous bytes into the connection's receive buffer.
//!
//! The storage layout (`[u8; OOO_ENTRY_MAX] × OOO_MAX_ENTRIES` inline) is
//! deliberately simple and will be replaced with an interval-merging form
//! in a later phase — this revision only *moves* the existing implementation.

use super::buffer::TcpRecvState;
use super::seq::{seq_ge, seq_gt, seq_le, seq_lt};

/// Maximum number of out-of-order segments buffered per connection.
pub const OOO_MAX_ENTRIES: usize = 8;

/// Maximum payload bytes stored per out-of-order entry (one MSS).
pub const OOO_ENTRY_MAX: usize = 1460;

#[derive(Clone, Copy)]
pub(crate) struct OooEntry {
    pub seq: u32,
    pub len: u16,
    pub data: [u8; OOO_ENTRY_MAX],
}

impl OooEntry {
    pub(crate) const fn empty() -> Self {
        Self {
            seq: 0,
            len: 0,
            data: [0; OOO_ENTRY_MAX],
        }
    }

    pub(crate) fn end_seq(&self) -> u32 {
        self.seq.wrapping_add(self.len as u32)
    }
}

impl core::fmt::Debug for OooEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OooEntry")
            .field("seq", &self.seq)
            .field("len", &self.len)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TcpOooQueue {
    entries: [OooEntry; OOO_MAX_ENTRIES],
    count: usize,
}

impl TcpOooQueue {
    pub const fn new() -> Self {
        Self {
            entries: [OooEntry::empty(); OOO_MAX_ENTRIES],
            count: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn insert(&mut self, seq: u32, data: &[u8]) {
        if data.is_empty() || data.len() > OOO_ENTRY_MAX {
            return;
        }

        // Drop duplicates: skip if we already cover this range.
        for i in 0..self.count {
            let e = &self.entries[i];
            if e.seq == seq && e.len as usize >= data.len() {
                return;
            }
            // If new segment is fully contained within an existing entry, skip.
            if seq_ge(seq, e.seq) && seq_le(seq.wrapping_add(data.len() as u32), e.end_seq()) {
                return;
            }
        }

        if self.count >= OOO_MAX_ENTRIES {
            // Evict the entry with the highest sequence number (furthest from rcv_nxt)
            // to keep near-gap segments which are more likely to complete reassembly.
            let mut evict_idx = 0;
            let mut evict_seq = self.entries[0].seq;
            for i in 1..self.count {
                if seq_gt(self.entries[i].seq, evict_seq) {
                    evict_idx = i;
                    evict_seq = self.entries[i].seq;
                }
            }
            // Only evict if the new entry has a lower seq (closer to gap).
            if seq_ge(seq, evict_seq) {
                return;
            }
            self.entries[evict_idx] = self.entries[self.count - 1];
            self.count -= 1;
        }

        let entry = &mut self.entries[self.count];
        entry.seq = seq;
        entry.len = data.len() as u16;
        entry.data[..data.len()].copy_from_slice(data);
        self.count += 1;
    }

    /// Drain contiguous segments starting at `rcv_nxt` into `recv`.
    /// Returns the total number of bytes drained (for advancing `rcv_nxt`).
    pub fn drain_contiguous(
        &mut self,
        rcv_nxt: u32,
        recv: &mut TcpRecvState,
        now_ms: u64,
    ) -> usize {
        let mut total = 0usize;
        let mut next_seq = rcv_nxt;

        loop {
            let mut found = None;
            for i in 0..self.count {
                let e = &self.entries[i];
                // Exact match or overlapping start (segment starts at or before next_seq
                // but extends past it).
                if e.seq == next_seq {
                    found = Some(i);
                    break;
                }
                // Partial overlap: segment starts before next_seq but data extends beyond.
                if seq_lt(e.seq, next_seq) && seq_gt(e.end_seq(), next_seq) {
                    found = Some(i);
                    break;
                }
            }

            let Some(idx) = found else { break };

            let entry = self.entries[idx];
            let skip = if seq_lt(entry.seq, next_seq) {
                next_seq.wrapping_sub(entry.seq) as usize
            } else {
                0
            };
            let usable = &entry.data[skip..entry.len as usize];
            let wrote = recv.enqueue(usable, now_ms);
            if wrote == 0 {
                break; // Receive buffer full
            }
            total += wrote;
            next_seq = next_seq.wrapping_add(wrote as u32);

            self.count -= 1;
            if idx < self.count {
                self.entries[idx] = self.entries[self.count];
            }
        }

        total
    }

    pub fn clear(&mut self) {
        self.count = 0;
    }
}
