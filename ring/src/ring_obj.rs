//! The kernel-side ring object: SQ/CQ index state, the in-flight
//! table, and the per-ring serialization lock (SLOPRING § 6, § 9).
//!
//! All plain safe-Rust data. The object owns its [`RingRegion`] and the
//! cached copies of the kernel-owned indices (`sq_head` / `cq_tail`),
//! which are the source of truth for control decisions and mirrored
//! into the shared page on each advance.

use slopos_abi::ring::{Cqe, RingLayout, SLOPRING_CQ_OVERFLOW};

use crate::region::{RegionError, RingRegion};

/// One armed-but-incomplete SQE — plain data, never a suspended future
/// (SLOPRING § 9). Holds the bytes needed to re-run the non-blocking
/// probe at harvest time and nothing that references ring memory.
#[derive(Clone, Copy, Debug)]
pub struct InFlight {
    /// Correlation cookie, echoed into the eventual CQE.
    pub user_data: u64,
    /// `OP_*` opcode.
    pub opcode: u8,
    /// Target fd resolved at submit (re-validated each probe).
    pub fd: i32,
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
    /// Armed-multishot flag (carries [`slopos_abi::ring::SLOPRING_SQE_MULTISHOT`]
    /// for the row). When set, the harvest keeps the row in flight and
    /// posts an interim `F_MORE` CQE on each yield instead of removing it.
    pub is_multishot: bool,
    /// OP_POLL_ADD multishot edge cache: the last masked-ready `revents`
    /// posted (0 = re-armed, fd currently not-ready). A CQE fires only
    /// when the ready bitset *transitions*, suppressing the level flood
    /// SlopRing's caller-as-waiter model would otherwise produce.
    pub last_revents: u16,
}

/// One ring object — created by `ring_setup`, dropped when its last fd
/// closes. Stored in the per-process ring registry (SLOPRING § 9).
pub struct Ring {
    /// The shared SQ/CQ region (owns the `RingMeta` frames).
    pub region: RingRegion,
    /// Computed ABI layout (offsets, masks).
    pub layout: RingLayout,
    /// Kernel-owned SQ consumer cursor (free-running). Mirrored into the
    /// shared `sq_head` on each advance.
    pub sq_head: u32,
    /// Kernel-owned CQ producer cursor (free-running). Mirrored into the
    /// shared `cq_tail` on each post.
    pub cq_tail: u32,
    /// In-flight rows. Bounded by `cq_entries`.
    pub inflight: heapless_vec::InFlightVec,
    /// User VA the region was mapped at (for teardown bookkeeping).
    #[allow(dead_code)]
    pub user_addr: u64,
    /// Owning process id (the only process allowed to enter this ring).
    pub owner_pid: u32,
    /// Count of CQEs dropped on overflow (mirrors shared `cq_overflow`).
    pub cq_overflow: u32,
}

impl Ring {
    /// Number of unharvested CQEs (`cq_tail - cq_head`), the Linux
    /// `available_cqes` definition (SLOPRING § 8.3).
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

    /// Post a completion with explicit CQE `flags`. If the CQ has a free
    /// slot, write the CQE and advance `cq_tail`; otherwise increment the
    /// overflow counter and return `false` (the caller decides whether
    /// that is acceptable — ownership ops reserve a slot first,
    /// SLOPRING § 11). `cqe_flags` carries
    /// [`slopos_abi::ring::SLOPRING_CQE_F_MORE`] for armed multishot
    /// interim completions (0 for oneshot / terminal CQEs).
    pub fn post_cqe(
        &mut self,
        user_data: u64,
        res: i32,
        cqe_flags: u32,
    ) -> Result<bool, RegionError> {
        let cq_head = self.read_cq_head()?;
        if self.cq_full(cq_head) {
            self.cq_overflow = self.cq_overflow.wrapping_add(1);
            // Raise the sticky CQ-overflow flag *before* publishing the
            // counter, so a userland reader that sees a bumped count never
            // observes the flag still clear. post_cqe is the single writer
            // per ring (it runs under the per-ring lock), so a plain store
            // of the latched bit is correct — no load+OR+store needed.
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
            // F_MORE (and, in Phase 4, the provided-buffer bits) are set by
            // the caller for armed multishot interim completions.
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

    /// Capacity-bounded in-flight table. Uses `KVec` for storage but
    /// never grows past `cap` (SLOPRING § 9 — excess → `-EAGAIN`).
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

        /// Snapshot the rows into an owned vec for harvest re-probing
        /// without holding the ring lock across blocking.
        pub fn snapshot(&self) -> KVec<InFlight> {
            let mut out = KVec::with_capacity(self.rows.len()).expect("ring: inflight snapshot");
            for r in self.rows.iter() {
                out.push(*r).expect("ring: inflight snapshot push");
            }
            out
        }

        /// Find the index of the first row matching `user_data`.
        pub fn find_user_data(&self, user_data: u64) -> Option<usize> {
            self.rows.iter().position(|r| r.user_data == user_data)
        }

        /// Update the OP_POLL_ADD multishot edge cache (`last_revents`) of
        /// the live row matching `user_data`. The harvest walks a
        /// `snapshot()` clone, so the edge-transition decision must write
        /// back to the *live* row through here — a write to the snapshot
        /// row would be lost.
        pub fn set_last_revents(&mut self, user_data: u64, v: u16) {
            if let Some(i) = self.rows.iter().position(|r| r.user_data == user_data) {
                self.rows[i].last_revents = v;
            }
        }

        /// Iterate the rows.
        pub fn iter(&self) -> impl Iterator<Item = &InFlight> + '_ {
            self.rows.iter()
        }
    }
}
