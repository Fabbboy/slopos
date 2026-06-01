//! Per-ring registered / provided buffer registry (SLOPRING § 13, ABI v2).
//!
//! Two io_uring-parity buffer mechanisms, both selected from an [`Sqe`] and
//! both backed by [`PinnedUserBuffer`] (pinned user pages accessed volatilely —
//! `ring/` stays `#![forbid(unsafe_code)]`):
//!
//! * **Registered fixed buffers** ([`SLOPRING_SQE_FIXED_BUFFER`], `Sqe.buf_index`)
//!   — `ring_register(RING_REGISTER_BUFFERS)` pins an array of user iovecs once;
//!   ops reference one by index. The per-op page-table walk + staging-Vec
//!   allocation + SMAP user-copy the inline path pays are all gone; a
//!   [`BufBitset`] reserves a buffer while an op holds it, so it cannot be
//!   reused or unregistered mid-flight (the UAF / orphan-reaping guard — the
//!   reservation is released only when the op's terminal CQE / cancel lands,
//!   mirroring the userland reactor's `OpSlot.buf`).
//! * **Provided buffer rings** ([`SLOPRING_SQE_BUFFER_SELECT`], `Sqe.buf_group`)
//!   — `ring_register(RING_REGISTER_PBUF_RING)` pins a userland-managed
//!   `io_uring_buf_ring`; a recv-family op peeks the next published buffer, the
//!   kernel fills it, reports the chosen `bid` in the CQE
//!   ([`SLOPRING_CQE_F_BUFFER`]), and commits the ring head.
//!
//! [`Sqe`]: slopos_abi::ring::Sqe

use slopos_abi::Errno;
use slopos_abi::ring::{
    IouringBuf, RegisterBufRingCmd, SLOPRING_MAX_BUF_GROUPS, SLOPRING_MAX_FIXED_BUFFERS,
    SLOPRING_PBUF_RING_MAX_ENTRIES, SLOPRING_PBUF_RING_TAIL_OFFSET,
};
use slopos_mm::pinned_user_buffer::{PinError, PinnedUserBuffer};
use slopos_ostd::KVec;

/// Reusable staging cap, matching the 4 KiB bound the inline `net_glue` path
/// stages through, so the registered path's transfer size is byte-for-byte
/// comparable to the inline path's.
pub const STAGING_CAP: usize = 4096;

/// Which registered buffer an SQE selects. `None` (neither flag set) is the
/// inline path — unchanged. Built in `opcode.rs` from the SQE flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufSel {
    /// Fixed buffer `index` in the registered set (`SLOPRING_SQE_FIXED_BUFFER`).
    Fixed { index: u16 },
    /// Kernel-picked buffer from provided-ring `group` (`SLOPRING_SQE_BUFFER_SELECT`).
    Provided { group: u16 },
}

/// A peeked provided buffer (not yet committed off the ring).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProvidedBuf {
    pub addr: u64,
    pub len: u32,
    pub bid: u16,
}

fn pin_errno(e: PinError) -> Errno {
    match e {
        PinError::OutOfMemory => Errno::ENOMEM,
        PinError::TooLarge | PinError::InvalidRange => Errno::EINVAL,
        PinError::NotUserAccessible | PinError::NotPresent | PinError::NotAnonymous => {
            Errno::EFAULT
        }
    }
}

/// Fixed-capacity checked-out bitset (one bit per registered fixed buffer, up
/// to [`SLOPRING_MAX_FIXED_BUFFERS`] = 1024 → 16 `u64` words). No heap, no
/// `unsafe`.
struct BufBitset {
    words: [u64; (SLOPRING_MAX_FIXED_BUFFERS as usize) / 64],
}

impl BufBitset {
    const fn new() -> Self {
        Self {
            words: [0; (SLOPRING_MAX_FIXED_BUFFERS as usize) / 64],
        }
    }
    fn set(&mut self, i: usize) {
        self.words[i / 64] |= 1u64 << (i % 64);
    }
    fn clear(&mut self, i: usize) {
        self.words[i / 64] &= !(1u64 << (i % 64));
    }
    fn get(&self, i: usize) -> bool {
        self.words[i / 64] & (1u64 << (i % 64)) != 0
    }
    fn any(&self) -> bool {
        self.words.iter().any(|w| *w != 0)
    }
}

/// One `RING_REGISTER_BUFFERS` registration: the pinned iovecs + their
/// in-flight reservation bitset.
struct FixedBufferSet {
    pins: KVec<PinnedUserBuffer>,
    checked_out: BufBitset,
}

/// One registered provided buffer ring (the pinned userland `io_uring_buf_ring`
/// for one `buf_group`).
struct ProvidedBufRing {
    gid: u16,
    ring_pin: PinnedUserBuffer,
    mask: u32,
    /// Kernel-owned consumer cursor (peek reads at `head`, commit advances it).
    head: u32,
    #[allow(dead_code)]
    flags: u16,
}

impl ProvidedBufRing {
    /// Volatile read of the user-published producer `tail` (overlaps
    /// `bufs[0].resv`).
    fn read_tail(&self) -> Result<u32, Errno> {
        let mut b = [0u8; 2];
        self.ring_pin
            .copy_out(SLOPRING_PBUF_RING_TAIL_OFFSET, &mut b)
            .map_err(|_| Errno::EFAULT)?;
        Ok(u16::from_le_bytes(b) as u32)
    }

    /// Peek the buffer at `head` without advancing — `None` if the ring is
    /// empty (`head == tail`).
    fn peek(&self) -> Result<Option<ProvidedBuf>, Errno> {
        let tail = self.read_tail()?;
        // u16 producer cursor: compare modulo 2^16 (head only ever trails tail
        // by at most `entries`, well under 2^16).
        if (tail & 0xFFFF) == (self.head & 0xFFFF) {
            return Ok(None);
        }
        let idx = (self.head & self.mask) as usize;
        let off = idx * core::mem::size_of::<IouringBuf>();
        let mut b = [0u8; 16];
        self.ring_pin
            .copy_out(off, &mut b)
            .map_err(|_| Errno::EFAULT)?;
        let buf = IouringBuf::from_bytes(&b);
        Ok(Some(ProvidedBuf {
            addr: buf.addr,
            len: buf.len,
            bid: buf.bid,
        }))
    }

    fn commit(&mut self) {
        self.head = self.head.wrapping_add(1);
    }
}

/// The per-ring buffer registry, owned by [`crate::ring_obj::Ring`] (so it
/// shares the per-ring lock — single writer at a time).
pub struct BufferRegistry {
    fixed: Option<FixedBufferSet>,
    provided: KVec<ProvidedBufRing>,
    /// Reusable staging buffer (allocated once, on first registered-buffer op).
    scratch: KVec<u8>,
}

impl BufferRegistry {
    pub const fn new() -> Self {
        Self {
            fixed: None,
            provided: KVec::new(),
            scratch: KVec::new(),
        }
    }

    fn ensure_scratch(&mut self) -> Result<(), Errno> {
        if self.scratch.len() < STAGING_CAP {
            self.scratch = KVec::zeroed(STAGING_CAP).map_err(|_| Errno::ENOMEM)?;
        }
        Ok(())
    }

    /// `true` iff any registered buffer (fixed or provided) exists — used to
    /// advertise the feature only when relevant (informational).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.fixed.is_none() && self.provided.is_empty()
    }

    // ----- registered fixed buffers (RING_REGISTER_BUFFERS) ----------------

    /// Pin and register the fixed-buffer set. Rejects a double-register, an
    /// empty / oversized count, or any non-anonymous / unmapped iovec.
    pub fn register_fixed(&mut self, pid: u32, iovecs: &[(u64, u32)]) -> Result<(), Errno> {
        if self.fixed.is_some() {
            return Err(Errno::EEXIST);
        }
        if iovecs.is_empty() || iovecs.len() > SLOPRING_MAX_FIXED_BUFFERS as usize {
            return Err(Errno::EINVAL);
        }
        let mut pins = KVec::with_capacity(iovecs.len()).map_err(|_| Errno::ENOMEM)?;
        for &(addr, len) in iovecs {
            let pin = PinnedUserBuffer::pin(pid, addr, len as usize).map_err(pin_errno)?;
            pins.push(pin).map_err(|_| Errno::ENOMEM)?;
        }
        self.fixed = Some(FixedBufferSet {
            pins,
            checked_out: BufBitset::new(),
        });
        Ok(())
    }

    /// Unregister the fixed-buffer set. `-EBUSY` if any buffer is checked out
    /// (an op holds it in flight); `-EINVAL` if none registered.
    pub fn unregister_fixed(&mut self) -> Result<(), Errno> {
        match self.fixed.as_ref() {
            None => Err(Errno::EINVAL),
            Some(set) if set.checked_out.any() => Err(Errno::EBUSY),
            Some(_) => {
                self.fixed = None; // drops pins → releases the page refs
                Ok(())
            }
        }
    }

    fn fixed_len(&self) -> usize {
        self.fixed.as_ref().map_or(0, |s| s.pins.len())
    }

    /// Reserve fixed buffer `index` for an in-flight op. `-EINVAL` if no set /
    /// out of range, `-EBUSY` if already held.
    pub fn check_out_fixed(&mut self, index: u16) -> Result<(), Errno> {
        let i = index as usize;
        let set = self.fixed.as_mut().ok_or(Errno::EINVAL)?;
        if i >= set.pins.len() {
            return Err(Errno::EINVAL);
        }
        if set.checked_out.get(i) {
            return Err(Errno::EBUSY);
        }
        set.checked_out.set(i);
        Ok(())
    }

    /// Release a fixed-buffer reservation (idempotent / bounds-safe — safe to
    /// call on a stale index after a concurrent unregister race).
    pub fn check_in_fixed(&mut self, index: u16) {
        let i = index as usize;
        if let Some(set) = self.fixed.as_mut()
            && i < set.pins.len()
        {
            set.checked_out.clear(i);
        }
    }

    /// `true` iff fixed buffer `index` is in range (no reservation change).
    pub fn fixed_in_bounds(&self, index: u16) -> bool {
        (index as usize) < self.fixed_len()
    }

    /// Stage fixed buffer `index`'s first `len` bytes (capped at
    /// [`STAGING_CAP`]) into the reusable scratch via one volatile copy, and
    /// return the staged slice for the net primitive. No allocation, no SMAP.
    pub fn stage_fixed_out(&mut self, index: u16, len: usize) -> Result<&[u8], Errno> {
        self.ensure_scratch()?;
        let set = self.fixed.as_ref().ok_or(Errno::EINVAL)?;
        let pin = set.pins.get(index as usize).ok_or(Errno::EINVAL)?;
        let n = len.min(pin.len()).min(STAGING_CAP);
        pin.copy_out(0, &mut self.scratch[..n])
            .map_err(|_| Errno::EFAULT)?;
        Ok(&self.scratch[..n])
    }

    /// Borrow the reusable scratch (first `cap` bytes, capped at
    /// [`STAGING_CAP`]) as a recv sink for the net primitive.
    pub fn recv_scratch(&mut self, cap: usize) -> Result<&mut [u8], Errno> {
        self.ensure_scratch()?;
        let n = cap.min(STAGING_CAP);
        Ok(&mut self.scratch[..n])
    }

    /// Publish `n` recv'd bytes from the scratch into fixed buffer `index` via
    /// one volatile copy.
    pub fn publish_fixed_in(&self, index: u16, n: usize) -> Result<(), Errno> {
        let set = self.fixed.as_ref().ok_or(Errno::EINVAL)?;
        let pin = set.pins.get(index as usize).ok_or(Errno::EINVAL)?;
        let n = n.min(pin.len());
        pin.copy_in(0, &self.scratch[..n])
            .map_err(|_| Errno::EFAULT)?;
        Ok(())
    }

    /// Cap on a fixed buffer's usable length (for clamping recv `cap`).
    pub fn fixed_len_of(&self, index: u16) -> Result<usize, Errno> {
        let set = self.fixed.as_ref().ok_or(Errno::EINVAL)?;
        Ok(set.pins.get(index as usize).ok_or(Errno::EINVAL)?.len())
    }

    // ----- provided buffer rings (RING_REGISTER_PBUF_RING) -----------------

    fn provided_idx(&self, group: u16) -> Option<usize> {
        self.provided.iter().position(|r| r.gid == group)
    }

    /// Pin and register a provided buffer ring for `group`.
    pub fn register_provided(&mut self, pid: u32, cmd: &RegisterBufRingCmd) -> Result<(), Errno> {
        if cmd.buf_group == 0 || cmd.buf_group > SLOPRING_MAX_BUF_GROUPS {
            return Err(Errno::EINVAL);
        }
        if cmd.ring_entries == 0
            || !cmd.ring_entries.is_power_of_two()
            || cmd.ring_entries > SLOPRING_PBUF_RING_MAX_ENTRIES
        {
            return Err(Errno::EINVAL);
        }
        if self.provided_idx(cmd.buf_group).is_some() {
            return Err(Errno::EEXIST);
        }
        let ring_bytes = cmd.ring_entries as usize * core::mem::size_of::<IouringBuf>();
        let ring_pin = PinnedUserBuffer::pin(pid, cmd.ring_addr, ring_bytes).map_err(pin_errno)?;
        self.provided
            .push(ProvidedBufRing {
                gid: cmd.buf_group,
                ring_pin,
                mask: cmd.ring_entries - 1,
                head: 0,
                flags: cmd.flags,
            })
            .map_err(|_| Errno::ENOMEM)?;
        Ok(())
    }

    /// Unregister the provided ring for `group`. The caller must first confirm
    /// no in-flight op references `group` (`-EBUSY` otherwise).
    pub fn unregister_provided(&mut self, group: u16) -> Result<(), Errno> {
        let idx = self.provided_idx(group).ok_or(Errno::EINVAL)?;
        let _ = self.provided.swap_remove(idx); // drops the ring pin
        Ok(())
    }

    /// `true` iff `group` names a registered provided ring.
    pub fn provided_exists(&self, group: u16) -> bool {
        self.provided_idx(group).is_some()
    }

    /// Peek the next published buffer for `group` without consuming it.
    /// `Ok(None)` ⇒ ring empty (caller posts `-ENOBUFS`). `-EINVAL` ⇒ no such
    /// group.
    pub fn peek_provided(&self, group: u16) -> Result<Option<ProvidedBuf>, Errno> {
        let idx = self.provided_idx(group).ok_or(Errno::EINVAL)?;
        self.provided[idx].peek()
    }

    /// Commit (consume) the peeked buffer for `group` — advances the ring head
    /// after a successful fill.
    pub fn commit_provided(&mut self, group: u16) {
        if let Some(idx) = self.provided_idx(group) {
            self.provided[idx].commit();
        }
    }

    /// Deliver `n` recv'd scratch bytes into the peeked provided buffer at user
    /// `addr` and consume it off the ring. The buffer is committed
    /// unconditionally (the socket data was already consumed, so the slot is
    /// spent) — a copy fault still consumes it, reporting `-EFAULT`, like
    /// `recvfrom`'s consume-then-fault. Pins `addr` transiently (no per-op
    /// reservation needed: a provided buffer is touched only within this call).
    pub fn publish_provided_in(
        &mut self,
        pid: u32,
        group: u16,
        addr: u64,
        n: usize,
    ) -> Result<(), Errno> {
        self.commit_provided(group);
        if n == 0 {
            return Ok(());
        }
        let pin = PinnedUserBuffer::pin(pid, addr, n).map_err(pin_errno)?;
        pin.copy_in(0, &self.scratch[..n])
            .map_err(|_| Errno::EFAULT)?;
        Ok(())
    }

    // ----- test-only injection (no live process VM) ------------------------

    /// Test hook: install a fixed-buffer set from pre-built (fabricated) pins.
    #[cfg(feature = "test-hooks")]
    pub fn register_fixed_for_test(&mut self, pins: KVec<PinnedUserBuffer>) {
        self.fixed = Some(FixedBufferSet {
            pins,
            checked_out: BufBitset::new(),
        });
    }

    /// Test hook: install a provided ring from a pre-built (and pre-populated)
    /// ring pin. `entries` must be a power of two.
    #[cfg(feature = "test-hooks")]
    pub fn register_provided_for_test(
        &mut self,
        gid: u16,
        ring_pin: PinnedUserBuffer,
        entries: u32,
    ) {
        let _ = self.provided.push(ProvidedBufRing {
            gid,
            ring_pin,
            mask: entries - 1,
            head: 0,
            flags: 0,
        });
    }
}

impl Default for BufferRegistry {
    fn default() -> Self {
        Self::new()
    }
}
