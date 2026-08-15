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
use slopos_ostd::mm::uframe::KeepaliveFrames;
use slopos_ostd::mm::{VmReader, VmWriter};
use slopos_ostd::{TxReclaimToken, ZcNotifToken};

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
    /// The ring owner's account, captured at registration.
    ///
    /// Held rather than re-resolved at keepalive time: a keepalive is taken on
    /// the send path, which a process exit races, and re-resolving would
    /// charge it to `AccountId::NONE` exactly when the pages are least likely
    /// to be released promptly. The row is generation-stamped, so a charge
    /// against an account whose process has gone is a defined no-op rather
    /// than a debit against a stranger.
    account: slopos_ostd::process::AccountId,
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

/// One in-flight zero-copy send (`OP_SEND_ZC`) awaiting its deferred
/// `SLOPRING_CQE_F_NOTIF`. The result CQE is posted at submit; this row keeps
/// the fixed buffer checked out until the driver reclaims the NIC TX descriptor
/// (the `token` flips), at which point the harvest posts `F_NOTIF` and checks
/// the buffer back in. Kept in a side table (not an `InFlight` row) because the
/// token is not `Copy` and the row must **not** be re-probed (that would
/// re-send).
/// The driver→ring "buffer reusable" signal a deferred send waits on. UDP/ICMP
/// use a single-shot generation flip; TCP `MSG_ZEROCOPY` uses a refcounted token
/// that reaches zero only when the bytes are cumulatively ACKed **and** every
/// in-flight (re)transmit DMA is reclaimed.
enum DeferredToken {
    Tx {
        token: TxReclaimToken,
        snapshot: u64,
    },
    Notif {
        token: ZcNotifToken,
    },
}

impl DeferredToken {
    /// Has the buffer become reusable since the send was recorded?
    fn is_ready(&self) -> bool {
        match self {
            DeferredToken::Tx { token, snapshot } => token.is_reclaimed(*snapshot),
            DeferredToken::Notif { token } => token.is_notifiable(),
        }
    }
}

struct DeferredNotif {
    user_data: u64,
    token: DeferredToken,
    buf_index: u16,
}

/// The per-ring buffer registry, owned by [`crate::ring_obj::Ring`] (so it
/// shares the per-ring lock — single writer at a time).
pub struct BufferRegistry {
    fixed: Option<FixedBufferSet>,
    provided: KVec<ProvidedBufRing>,
    /// In-flight zero-copy sends awaiting their deferred `F_NOTIF`. Pre-grown to
    /// the fixed-buffer count at `register_fixed` so [`push_deferred`] never
    /// reallocates (a buffer index can hold at most one in-flight ZC send — a
    /// second `check_out_fixed` is rejected — so the table never exceeds it).
    ///
    /// [`push_deferred`]: Self::push_deferred
    deferred: KVec<DeferredNotif>,
}

impl BufferRegistry {
    pub const fn new() -> Self {
        Self {
            fixed: None,
            provided: KVec::new(),
            deferred: KVec::new(),
        }
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
    pub fn register_fixed(
        &mut self,
        process: slopos_ostd::process::ProcessId,
        iovecs: &[(u64, u32)],
    ) -> Result<(), Errno> {
        if self.fixed.is_some() {
            return Err(Errno::EEXIST);
        }
        if iovecs.is_empty() || iovecs.len() > SLOPRING_MAX_FIXED_BUFFERS as usize {
            return Err(Errno::EINVAL);
        }
        let mut pins = KVec::with_capacity(iovecs.len()).map_err(|_| Errno::ENOMEM)?;
        for &(addr, len) in iovecs {
            let pin = PinnedUserBuffer::pin(process, addr, len as usize, process.account())
                .map_err(pin_errno)?;
            pins.push(pin).map_err(|_| Errno::ENOMEM)?;
        }
        // Pre-grow the deferred-notify side table to the buffer count so
        // `push_deferred` (after a zero-copy submit that cannot be undone) never
        // reallocates — at most one in-flight ZC send per buffer index.
        let deferred = KVec::with_capacity(pins.len()).map_err(|_| Errno::ENOMEM)?;
        self.fixed = Some(FixedBufferSet {
            pins,
            checked_out: BufBitset::new(),
            account: process.account(),
        });
        self.deferred = deferred;
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

    /// A volatile [`VmReader`] over fixed buffer `index`'s first `len` bytes
    /// (capped at the pin length) — the single-direct-copy send source. The net
    /// leaf pulls bytes straight from the pinned pages into the socket buffer
    /// with no intermediate kernel scratch.
    pub fn fixed_reader(&self, index: u16, len: usize) -> Result<VmReader<'_>, Errno> {
        let set = self.fixed.as_ref().ok_or(Errno::EINVAL)?;
        let pin = set.pins.get(index as usize).ok_or(Errno::EINVAL)?;
        let n = len.min(pin.len());
        pin.reader(0, n).ok_or(Errno::EFAULT)
    }

    /// A volatile [`VmWriter`] over the whole of fixed buffer `index` — the
    /// single-direct-copy recv sink. The net leaf fills the pinned pages
    /// directly from the socket buffer with no intermediate kernel scratch.
    pub fn fixed_writer(&self, index: u16) -> Result<VmWriter<'_>, Errno> {
        let set = self.fixed.as_ref().ok_or(Errno::EINVAL)?;
        let pin = set.pins.get(index as usize).ok_or(Errno::EINVAL)?;
        let cap = pin.len();
        pin.writer(0, cap).ok_or(Errno::EFAULT)
    }

    /// Cap on a fixed buffer's usable length.
    pub fn fixed_len_of(&self, index: u16) -> Result<usize, Errno> {
        let set = self.fixed.as_ref().ok_or(Errno::EINVAL)?;
        Ok(set.pins.get(index as usize).ok_or(Errno::EINVAL)?.len())
    }

    /// Coalesced physical `(paddr, len)` runs over the first `len` bytes of fixed
    /// buffer `index` — the scatter-gather payload runs a NIC DMAs straight from
    /// the pinned pages (`OP_SEND_ZC` zero-copy path).
    pub fn fixed_io_slices(&self, index: u16, len: usize) -> Result<KVec<(u64, u32)>, Errno> {
        let set = self.fixed.as_ref().ok_or(Errno::EINVAL)?;
        let pin = set.pins.get(index as usize).ok_or(Errno::EINVAL)?;
        let n = len.min(pin.len());
        let slices = pin.io_slices_len(n);
        let mut out = KVec::with_capacity(slices.len()).map_err(|_| Errno::ENOMEM)?;
        for s in slices.iter() {
            out.push((s.paddr, s.len)).map_err(|_| Errno::ENOMEM)?;
        }
        Ok(out)
    }

    /// An **independent** owning ref on every backing page of fixed buffer
    /// `index`, handed to the driver so the pinned pages survive a ring/process
    /// teardown that drops this registry while the NIC is still DMAing them
    /// (the use-after-free guard; the registry's own pin + `checked_out` guard
    /// only the explicit unregister syscall). `None` if out of range / alloc.
    /// The keepalive carries its own `PinnedBytes` charge, independent of the
    /// registered buffer's: it outlives the ring by design, so sharing one
    /// would refund at teardown while the NIC still held the pages.
    pub fn fixed_keepalive(&self, index: u16) -> Option<KeepaliveFrames> {
        let set = self.fixed.as_ref()?;
        let pin = set.pins.get(index as usize)?;
        pin.keepalive_frames(set.account)
    }

    /// In-page byte offset of fixed buffer `index`'s data within its first
    /// backing page — paired with [`fixed_keepalive`](Self::fixed_keepalive) so
    /// the TCP `MSG_ZEROCOPY` send queue can re-derive a segment's DMA runs at an
    /// arbitrary offset on every (re)transmit. `None` if out of range.
    pub fn fixed_base_off(&self, index: u16) -> Option<usize> {
        let set = self.fixed.as_ref()?;
        Some(set.pins.get(index as usize)?.base_off())
    }

    /// Record an in-flight zero-copy send awaiting its deferred `F_NOTIF`. The
    /// fixed buffer stays checked out (held by this entry) until the driver
    /// reclaims the NIC descriptor and [`take_reclaimed`] retires it. Infallible
    /// — the table was pre-grown at `register_fixed` (see [`deferred`]).
    ///
    /// [`take_reclaimed`]: Self::take_reclaimed
    /// [`deferred`]: Self::deferred
    pub fn push_deferred(
        &mut self,
        user_data: u64,
        token: TxReclaimToken,
        snapshot: u64,
        buf_index: u16,
    ) {
        let _ = self.deferred.push(DeferredNotif {
            user_data,
            token: DeferredToken::Tx { token, snapshot },
            buf_index,
        });
    }

    /// Record an in-flight TCP `MSG_ZEROCOPY` send awaiting its deferred
    /// `F_NOTIF`. Like [`push_deferred`](Self::push_deferred) but keyed on the
    /// refcounted [`ZcNotifToken`] — the buffer stays checked out until the bytes
    /// are cumulatively ACKed and every retransmit DMA is reclaimed (the count
    /// reaches zero). Infallible (the table was pre-grown; one in-flight ZC send
    /// per buffer index, since the index stays checked out until `F_NOTIF`).
    pub fn push_deferred_notif(&mut self, user_data: u64, token: ZcNotifToken, buf_index: u16) {
        let _ = self.deferred.push(DeferredNotif {
            user_data,
            token: DeferredToken::Notif { token },
            buf_index,
        });
    }

    /// `true` iff any zero-copy send is in flight awaiting its deferred
    /// `F_NOTIF` — the harvest uses this to decide whether to drive a device TX
    /// reclaim before checking the tokens.
    pub fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }

    /// Drain every deferred zero-copy send whose driver has reclaimed the NIC TX
    /// descriptor since submit (`token.is_reclaimed(snapshot)`), returning the
    /// `(user_data, buf_index)` of each so the caller can post `F_NOTIF` and
    /// `check_in_fixed` **after** this `&mut self` borrow ends (the harvest
    /// double-borrow fix).
    pub fn take_reclaimed(&mut self) -> KVec<(u64, u16)> {
        let mut out: KVec<(u64, u16)> = KVec::new();
        let mut i = 0;
        while i < self.deferred.len() {
            if self.deferred[i].token.is_ready() {
                let entry = self.deferred.swap_remove(i);
                let _ = out.push((entry.user_data, entry.buf_index));
                // swap_remove moved a new element into `i`; re-check it.
            } else {
                i += 1;
            }
        }
        out
    }

    // ----- provided buffer rings (RING_REGISTER_PBUF_RING) -----------------

    fn provided_idx(&self, group: u16) -> Option<usize> {
        self.provided.iter().position(|r| r.gid == group)
    }

    /// Pin and register a provided buffer ring for `group`.
    pub fn register_provided(
        &mut self,
        process: slopos_ostd::process::ProcessId,
        cmd: &RegisterBufRingCmd,
    ) -> Result<(), Errno> {
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
        let ring_pin = PinnedUserBuffer::pin(process, cmd.ring_addr, ring_bytes, process.account())
            .map_err(pin_errno)?;
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

    /// Transiently pin a kernel-picked provided buffer at user `addr` for the
    /// duration of one recv op (no per-op reservation: a provided buffer is
    /// touched only within the op). The caller builds a [`VmWriter`] over the
    /// returned pin, fills it directly from the socket, then [`commit_provided`]
    /// on success — the single-direct-copy provided-buffer recv path.
    ///
    /// [`commit_provided`]: Self::commit_provided
    pub fn provided_pin(
        process: slopos_ostd::process::ProcessId,
        addr: u64,
        len: usize,
    ) -> Result<PinnedUserBuffer, Errno> {
        PinnedUserBuffer::pin(process, addr, len, process.account()).map_err(pin_errno)
    }

    // ----- test-only injection (no live process VM) ------------------------

    /// Test hook: install a fixed-buffer set from pre-built (fabricated) pins.
    #[cfg(feature = "test-hooks")]
    pub fn register_fixed_for_test(&mut self, pins: KVec<PinnedUserBuffer>) {
        self.fixed = Some(FixedBufferSet {
            pins,
            checked_out: BufBitset::new(),
            // A fabricated set belongs to no process; a charge against no
            // account debits and refunds nothing.
            account: slopos_ostd::process::AccountId::NONE,
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
