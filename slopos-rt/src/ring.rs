//! `slibc-ring` — the userland SlopRing runtime.
//!
//! Mirrors `liburing`'s shape: [`Ring::setup`] creates a ring and maps
//! its SQ/CQ, [`Ring::get_sqe`] / [`Ring::submit`] fill and publish
//! submissions, and [`Ring::wait_completion`] / [`Ring::poll_completion`]
//! harvest. This is **userland** — async lives here, never in the kernel
//! (AD-8/AD-9). The kernel side is the strictly-synchronous `ring/`
//! crate driven by the two `ring_*` syscalls.
//!
//! The runtime reads/writes the shared region directly through its own
//! mapping. Because the *kernel* reads this same memory volatilely
//! (SLOPRING § 5.3), userland uses the matching acquire/release ordering
//! on the head/tail indices so the SPSC contract (SLOPRING § 4.2) holds.

use core::sync::atomic::{Ordering, fence};

use slopos_abi::ring::{Cqe, RingParams, SLOPRING_CQ_OVERFLOW, Sqe};

use crate::sys::ring::{ring_enter, ring_setup};

/// Errors the runtime surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    /// `ring_setup` failed (negated errno in `.0`).
    Setup(i32),
    /// `ring_enter` failed (negated errno in `.0`).
    Enter(i32),
    /// The SQ is full — retry after harvesting / submitting.
    SqFull,
}

/// A userland handle to a SlopRing. Owns the ring fd and the mapped
/// region.
pub struct Ring {
    fd: i32,
    params: RingParams,
    base: u64,
}

impl Ring {
    /// Create a ring with `entries` SQ slots and map it.
    pub fn setup(entries: u32) -> Result<Self, RingError> {
        let mut params = RingParams::ZERO;
        let fd = ring_setup(entries, &mut params);
        if fd < 0 {
            return Err(RingError::Setup(fd));
        }
        Ok(Self {
            fd,
            base: params.region_addr,
            params,
        })
    }

    /// The ring fd.
    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// Number of SQ slots.
    pub fn sq_entries(&self) -> u32 {
        self.params.sq_entries
    }

    /// `true` if the kernel has dropped at least one completion because
    /// the CQ was full (the shared `SLOPRING_CQ_OVERFLOW` flag). Userland
    /// should harvest aggressively and treat in-flight ops as possibly
    /// lost when this is set. The companion count is
    /// [`Ring::cq_overflow_count`].
    ///
    /// This is a **sticky one-way latch**: once set it stays set for the
    /// ring's lifetime and is cleared only by creating a fresh ring
    /// (`ring_setup`). The dropped completions are unrecoverable, so the
    /// flag is a permanent "this ring lost data" signal, not a transient
    /// edge. Use [`Ring::cq_overflow_count`] for the running drop count.
    pub fn cq_overflow(&self) -> bool {
        (self.load_acq(self.params.cq_off_flags) & SLOPRING_CQ_OVERFLOW) != 0
    }

    /// Number of completions the kernel dropped on a full CQ (the shared
    /// `cq_off_overflow` counter, monotonically increasing modulo 2^32).
    pub fn cq_overflow_count(&self) -> u32 {
        self.load_acq(self.params.cq_off_overflow)
    }

    // -- raw index access (volatile, ordered) --

    #[inline]
    fn idx_ptr(&self, off: u32) -> *mut u32 {
        (self.base + off as u64) as *mut u32
    }

    #[inline]
    fn load_acq(&self, off: u32) -> u32 {
        // SAFETY: `off` is one of the control offsets the kernel wrote
        // into `params`; the region is mapped read+write for this
        // process for the ring's lifetime.
        let v = unsafe { core::ptr::read_volatile(self.idx_ptr(off)) };
        fence(Ordering::Acquire);
        v
    }

    #[inline]
    fn store_rel(&self, off: u32, val: u32) {
        fence(Ordering::Release);
        // SAFETY: see `load_acq`.
        unsafe { core::ptr::write_volatile(self.idx_ptr(off), val) }
    }

    fn sq_tail(&self) -> u32 {
        self.load_acq(self.params.sq_off_tail)
    }
    fn sq_head(&self) -> u32 {
        self.load_acq(self.params.sq_off_head)
    }
    fn cq_tail(&self) -> u32 {
        self.load_acq(self.params.cq_off_tail)
    }
    fn cq_head(&self) -> u32 {
        self.load_acq(self.params.cq_off_head)
    }

    /// Fill SQE slot at the current `sq_tail` with `sqe` and bump the
    /// local tail (not yet published — call [`Ring::submit`] /
    /// [`Ring::submit_and_wait`] to publish + doorbell). Returns
    /// `SqFull` if no slot is free.
    pub fn push_sqe(&mut self, sqe: &Sqe) -> Result<(), RingError> {
        let tail = self.sq_tail();
        let head = self.sq_head();
        if tail.wrapping_sub(head) >= self.params.sq_entries {
            return Err(RingError::SqFull);
        }
        let idx = tail & (self.params.sq_entries - 1);
        let off = self.params.sq_off_array + idx * core::mem::size_of::<Sqe>() as u32;
        let bytes = sqe.to_bytes();
        // SAFETY: `off` indexes within the mapped SQE array (idx masked
        // to sq_entries-1); the region is writable for this process.
        unsafe {
            let dst = (self.base + off as u64) as *mut u8;
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        }
        // Publish the new tail (release) so the kernel's acquire load of
        // sq_tail sees the SQE body writes above.
        self.store_rel(self.params.sq_off_tail, tail.wrapping_add(1));
        Ok(())
    }

    /// Submit all pending SQEs (doorbell) without waiting. Returns the
    /// submission count reported by the kernel.
    pub fn submit(&mut self) -> Result<u32, RingError> {
        self.enter(self.sq_entries(), 0)
    }

    /// Submit and block until at least `min_complete` CQEs are ready.
    pub fn submit_and_wait(&mut self, min_complete: u32) -> Result<u32, RingError> {
        self.enter(self.sq_entries(), min_complete)
    }

    fn enter(&mut self, to_submit: u32, min_complete: u32) -> Result<u32, RingError> {
        let rc = ring_enter(self.fd, to_submit, min_complete, 0);
        if rc < 0 {
            return Err(RingError::Enter(rc));
        }
        Ok(rc as u32)
    }

    /// Non-blocking harvest of one CQE. Returns `None` if the CQ is
    /// empty. Inline completions are visible here; deferred (was-blocking)
    /// completions require a blocking [`Ring::submit_and_wait`] /
    /// [`Ring::wait_completion`] to progress (SLOPRING § 7.1).
    pub fn poll_completion(&mut self) -> Option<Cqe> {
        let tail = self.cq_tail();
        let head = self.cq_head();
        if head == tail {
            return None;
        }
        let idx = head & (self.params.cq_entries - 1);
        let off = self.params.cq_off_array + idx * core::mem::size_of::<Cqe>() as u32;
        let mut bytes = [0u8; 16];
        // SAFETY: `off` indexes within the mapped CQE array; region is
        // readable for this process.
        unsafe {
            let src = (self.base + off as u64) as *const u8;
            core::ptr::copy_nonoverlapping(src, bytes.as_mut_ptr(), bytes.len());
        }
        // Consume: advance cq_head (release) so the kernel can reuse the
        // slot.
        self.store_rel(self.params.cq_off_head, head.wrapping_add(1));
        Some(Cqe::from_bytes(&bytes))
    }

    /// Block until at least one CQE is available, then return it. Drives
    /// deferred completions via a blocking `ring_enter`.
    pub fn wait_completion(&mut self) -> Result<Cqe, RingError> {
        loop {
            if let Some(cqe) = self.poll_completion() {
                return Ok(cqe);
            }
            // Nothing ready — block once to drive deferred completions.
            self.enter(0, 1)?;
        }
    }
}

impl Drop for Ring {
    /// Release the ring: unmap the shared region, then close the ring fd
    /// (which drops the kernel-side `Ring` and its `Frame<RingMeta>`
    /// refs). Without this, every `Ring::setup` would leak a mapping +
    /// an fd for the process's lifetime.
    fn drop(&mut self) {
        let bytes = self.params.region_bytes;
        if self.base != 0 && bytes != 0 {
            let _ = crate::sys::memory::munmap(self.base, bytes);
        }
        if self.fd >= 0 {
            let _ = crate::sys::memory::close(self.fd);
        }
    }
}
