//! The kernel-side ring object: SQ/CQ index state, the in-flight
//! table, and the per-ring serialization lock (SLOPRING § 6, § 9).
//!
//! The cached kernel-owned indices (`sq_head` / `cq_tail`) are the source of
//! truth for control decisions, and are mirrored into the shared page on each
//! advance.

use slopos_abi::ring::{Cqe, RingLayout, SLOPRING_CQ_OVERFLOW};
use slopos_fs::fileio::FdTable;
use slopos_fs::fileio::FileRef;

use crate::region::{RegionError, RingRegion};

/// One armed-but-incomplete SQE — plain data, never a suspended future
/// (SLOPRING § 9). Holds everything needed to re-run the non-blocking probe at
/// harvest time, and nothing that references ring memory.
pub struct InFlight {
    /// Correlation cookie, echoed into the eventual CQE.
    pub user_data: u64,
    pub opcode: u8,
    /// Strong reference to the target open file, resolved once at submit, so
    /// the backing outlives a close or fd-number reuse. `None` for fd-less
    /// rows (`OP_TIMEOUT`; path/fd-number ops never defer).
    pub file: Option<FileRef>,
    /// User VA of the data buffer / msghdr / sockaddr.
    pub addr: u64,
    /// Secondary VA (accept's socklen out-ptr).
    pub addr2: u64,
    /// Buffer byte length.
    pub len: u32,
    /// Per-opcode flags (poll mask, recv/send flags, cancel flags).
    pub op_flags: u32,
    /// File offset (reads/writes) — unused for sockets.
    pub off: u64,
    /// For `OP_TIMEOUT`: absolute deadline in ms (`get_time_ms` clock).
    pub deadline_ms: u64,
    /// [`slopos_abi::ring::SLOPRING_SQE_MULTISHOT`] for this row: the harvest
    /// keeps it in flight and posts an interim `F_MORE` CQE on each yield.
    pub is_multishot: bool,
    /// OP_POLL_ADD multishot edge cache: the last masked-ready `revents`
    /// posted (0 = re-armed, fd currently not-ready). A CQE fires only when
    /// the ready bitset *transitions*, suppressing a level flood.
    pub last_revents: u16,
    /// Provided-buffer group id (`SLOPRING_SQE_BUFFER_SELECT`), 0 if unused.
    pub buf_group: u16,
    /// Registered fixed-buffer index (`SLOPRING_SQE_FIXED_BUFFER`).
    pub buf_index: u16,
    /// The `Sqe.flags` buffer-selection bits, carried so the deferred reprobe
    /// re-applies the same selection.
    pub buf_flags: u8,
}

impl InFlight {
    /// Duplicate a row, aliasing its file reference. The harvest walks a
    /// `snapshot()` clone while removing from the live table; the live row
    /// keeps the authoritative reference, which reaps outside the ring lock.
    pub fn alias(&self) -> InFlight {
        InFlight {
            user_data: self.user_data,
            opcode: self.opcode,
            file: self.file.as_ref().map(FileRef::alias),
            addr: self.addr,
            addr2: self.addr2,
            len: self.len,
            op_flags: self.op_flags,
            off: self.off,
            deadline_ms: self.deadline_ms,
            is_multishot: self.is_multishot,
            last_revents: self.last_revents,
            buf_group: self.buf_group,
            buf_index: self.buf_index,
            buf_flags: self.buf_flags,
        }
    }
}

/// One ring object — created by `ring_setup`, dropped when its last fd
/// closes. Stored in the per-process ring registry (SLOPRING § 9).
pub struct Ring {
    /// The shared SQ/CQ region (owns the `RingMeta` frames).
    pub region: RingRegion,
    /// Computed ABI layout (offsets, masks).
    pub layout: RingLayout,
    /// Kernel-owned SQ consumer cursor (free-running).
    pub sq_head: u32,
    /// Kernel-owned CQ producer cursor (free-running).
    pub cq_tail: u32,
    /// In-flight rows. Bounded by `cq_entries`.
    pub inflight: heapless_vec::InFlightVec,
    /// User VA the region was mapped at (for teardown bookkeeping).
    #[allow(dead_code)]
    pub user_addr: u64,
    /// The owning process — the only one allowed to enter this ring.
    ///
    /// An [`FdTable`] rather than a raw pid because this is a *permission*
    /// key: a recycled id would let whichever process next holds that number
    /// enter a ring it never created and act on the creator's descriptors.
    pub owner: FdTable,
    /// Count of CQEs dropped on overflow (mirrors shared `cq_overflow`).
    pub cq_overflow: u32,
    /// Registered / provided buffer registry (ABI v2 zero-copy path). Shares
    /// the per-ring lock. Heap-boxed so `Ring` stays constructible within the
    /// 2 KiB stack ceiling (Inv. 5').
    pub buffers: slopos_ostd::KBox<crate::buffers::BufferRegistry>,
    /// Rows retired this `with_ring` span, detached under the per-ring lock
    /// but dropped by the caller *after* releasing it: a last `FileRef` drop
    /// can take arbitrary subsystem locks, even re-entering the ring registry.
    pub pending_reap: slopos_ostd::KVec<InFlight>,
}

impl Ring {
    /// Number of unharvested CQEs (`cq_tail - cq_head`, SLOPRING § 8.3).
    pub fn available_cqes(&self, cq_head: u32) -> u32 {
        self.cq_tail.wrapping_sub(cq_head)
    }

    /// `true` iff the CQ has no free slot (SLOPRING § 11).
    pub fn cq_full(&self, cq_head: u32) -> bool {
        self.cq_tail.wrapping_sub(cq_head) >= self.layout.cq_entries
    }

    /// Read the user-owned `cq_head` index (volatile acquire).
    pub fn read_cq_head(&self) -> Result<u32, RegionError> {
        self.region
            .load_u32_acquire(self.layout.cq_off_head as usize)
    }

    /// Read the user-owned `sq_tail` index (volatile acquire).
    pub fn read_sq_tail(&self) -> Result<u32, RegionError> {
        self.region
            .load_u32_acquire(self.layout.sq_off_tail as usize)
    }

    /// Publish the kernel's `sq_head` into the shared page (release).
    pub fn publish_sq_head(&self) -> Result<(), RegionError> {
        self.region
            .store_u32_release(self.layout.sq_off_head as usize, self.sq_head)
    }

    /// Publish the kernel's `cq_tail` into the shared page (release).
    pub fn publish_cq_tail(&self) -> Result<(), RegionError> {
        self.region
            .store_u32_release(self.layout.cq_off_tail as usize, self.cq_tail)
    }

    /// Post a completion with explicit CQE `flags`. Returns `false` when the
    /// CQ is full — the overflow counter is bumped instead, and the caller
    /// decides whether that is acceptable (ownership ops reserve a slot
    /// first, SLOPRING § 11).
    pub fn post_cqe(
        &mut self,
        user_data: u64,
        res: i32,
        cqe_flags: u32,
    ) -> Result<bool, RegionError> {
        let cq_head = self.read_cq_head()?;
        if self.cq_full(cq_head) {
            self.cq_overflow = self.cq_overflow.wrapping_add(1);
            // Latch the sticky flag *before* publishing the counter, so a
            // reader that sees a bumped count never observes it still clear.
            // Single writer per ring (the per-ring lock), so a plain store of
            // the bit is correct — no load+OR+store needed.
            self.region
                .store_u32_release(self.layout.cq_off_flags as usize, SLOPRING_CQ_OVERFLOW)?;
            self.region
                .store_u32_release(self.layout.cq_off_overflow as usize, self.cq_overflow)?;
            return Ok(false);
        }
        let idx = self.cq_tail & (self.layout.cq_entries - 1);
        let cqe = Cqe {
            user_data,
            res,
            flags: cqe_flags,
        };
        self.region
            .copy_in(self.layout.cqe_off(idx) as usize, &cqe.to_bytes())?;
        self.cq_tail = self.cq_tail.wrapping_add(1);
        self.publish_cq_tail()?;
        Ok(true)
    }
}

/// A fixed-capacity vector of in-flight rows, sized at ring setup.
pub mod heapless_vec {
    use super::InFlight;
    use slopos_ostd::KVec;

    /// Capacity-bounded in-flight table; never grows past `cap`, and excess
    /// submissions complete `-EAGAIN` (SLOPRING § 9).
    pub struct InFlightVec {
        rows: KVec<InFlight>,
        cap: usize,
    }

    impl InFlightVec {
        pub fn with_capacity(cap: usize) -> Self {
            Self {
                rows: KVec::with_capacity(cap).expect("ring: inflight alloc"),
                cap,
            }
        }

        pub fn is_full(&self) -> bool {
            self.rows.len() >= self.cap
        }

        #[allow(dead_code)]
        pub fn len(&self) -> usize {
            self.rows.len()
        }

        #[allow(dead_code)]
        pub fn is_empty(&self) -> bool {
            self.rows.is_empty()
        }

        /// Push a row; returns `false` if at capacity (caller completes
        /// the SQE inline with `-EAGAIN`).
        pub fn push(&mut self, row: InFlight) -> bool {
            if self.is_full() {
                return false;
            }
            self.rows.push(row).is_ok()
        }

        /// Remove the row at index `i` (swap-remove order is fine — the
        /// table is unordered).
        pub fn remove_at(&mut self, i: usize) -> Option<InFlight> {
            if i >= self.rows.len() {
                return None;
            }
            Some(self.rows.swap_remove(i))
        }

        /// Snapshot the rows (aliasing each file reference) for harvest
        /// re-probing, so the harvest walks a stable set while removing from
        /// the live table. An aliased ref is never the last, so dropping the
        /// snapshot under the ring lock is safe.
        pub fn snapshot(&self) -> KVec<InFlight> {
            let mut out = KVec::with_capacity(self.rows.len()).expect("ring: inflight snapshot");
            for r in self.rows.iter() {
                out.push(r.alias()).expect("ring: inflight snapshot push");
            }
            out
        }

        pub fn find_user_data(&self, user_data: u64) -> Option<usize> {
            self.rows.iter().position(|r| r.user_data == user_data)
        }

        /// Update the OP_POLL_ADD edge cache on the *live* row matching
        /// `user_data`: the harvest walks a `snapshot()` clone, so a write to
        /// the snapshot row would be lost.
        pub fn set_last_revents(&mut self, user_data: u64, v: u16) {
            if let Some(i) = self.rows.iter().position(|r| r.user_data == user_data) {
                self.rows[i].last_revents = v;
            }
        }

        pub fn iter(&self) -> impl Iterator<Item = &InFlight> + '_ {
            self.rows.iter()
        }
    }
}
