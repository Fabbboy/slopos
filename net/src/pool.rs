//! Pre-allocated packet buffer pool.
//!
//! Each slot is backed by a typed [`Frame<PacketMeta>`] page, allocated once at
//! [`init`](PacketPool::init) and recycled for the kernel's lifetime. The
//! free-list sits behind a leaf `SpinLock` that acquires no other lock, so it
//! can be taken from any context.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use slopos_ostd::lock_class;

use slopos_ostd::KVec;
use slopos_ostd::mm::frame::{Frame, PacketMeta};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

/// Usable prefix of each 4 KiB backing frame: max Ethernet frame (1518) plus
/// headroom.
pub const BUF_SIZE: usize = 2048;

/// Number of pre-allocated slots; each reserves one 4 KiB frame.
pub const POOL_SIZE: usize = 256;

struct PoolInner {
    /// `slots[i]` is `None` while that frame is lent to a live
    /// [`PacketBuf`](super::packetbuf::PacketBuf); length is however many
    /// frames [`init`](PacketPool::init) obtained (`<= POOL_SIZE`).
    slots: KVec<Option<Frame<PacketMeta>>>,
    /// `i` is present iff `slots[i]` is resident and not lent out.
    free: KVec<u16>,
}

/// Pre-allocated packet buffer pool.
///
/// `alloc`/`release` hand out bare `u16` slot handles; `acquire`/`restore`
/// additionally move the backing [`Frame<PacketMeta>`] out and back, so a
/// [`PacketBuf`](super::packetbuf::PacketBuf) can own its frame by value.
pub struct PacketPool {
    /// `None` until [`init`](Self::init) populates it.
    inner: SpinLock<Option<PoolInner>>,
    initialized: AtomicBool,
    /// Mirrors `inner.free.len()` so `available` is a lock-free read.
    count: AtomicUsize,
}

/// Global packet pool; [`PacketPool::init`] must run before any networking code.
pub static PACKET_POOL: PacketPool = PacketPool {
    inner: SpinLock::new(None, lock_class!("PACKET_POOL", LOCK_LEVEL_RESOURCE)),
    initialized: AtomicBool::new(false),
    count: AtomicUsize::new(0),
};

impl PacketPool {
    /// Allocate the backing frames and build the free-list.
    ///
    /// Idempotent. Builds with however many of the [`POOL_SIZE`] frames the
    /// allocator supplies rather than panicking when it is short.
    pub fn init(&self) {
        if self.initialized.swap(true, Ordering::AcqRel) {
            return;
        }

        let mut slots: KVec<Option<Frame<PacketMeta>>> =
            KVec::with_capacity(POOL_SIZE).expect("packet pool: slots alloc");
        let mut free: KVec<u16> =
            KVec::with_capacity(POOL_SIZE).expect("packet pool: free-list alloc");

        for i in 0..POOL_SIZE {
            match Frame::<PacketMeta>::alloc() {
                Some(frame) => {
                    slots.push(Some(frame)).expect("packet pool: push slot");
                    free.push(i as u16).expect("packet pool: push free index");
                }
                None => break,
            }
        }

        let available = free.len();
        *self.inner.lock() = Some(PoolInner { slots, free });
        self.count.store(available, Ordering::Release);
    }

    /// Reserve a free slot, leaving its frame resident (see
    /// [`acquire`](Self::acquire)). `None` if exhausted or uninitialized.
    pub fn alloc(&self) -> Option<u16> {
        let mut guard = self.inner.lock();
        let inner = guard.as_mut()?;
        let slot = inner.free.pop()?;
        self.count.fetch_sub(1, Ordering::Relaxed);
        Some(slot)
    }

    /// Return an [`alloc`](Self::alloc)ed slot to the free-list; its frame must
    /// still be resident.
    pub fn release(&self, slot: u16) {
        let mut guard = self.inner.lock();
        if let Some(inner) = guard.as_mut() {
            inner.free.push(slot).expect("packet pool: release push");
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Reserve a free slot *and* move out its backing frame, which
    /// [`restore`](Self::restore) returns. `None` if exhausted or
    /// uninitialized.
    pub(crate) fn acquire(&self) -> Option<(u16, Frame<PacketMeta>)> {
        let mut guard = self.inner.lock();
        let inner = guard.as_mut()?;
        let slot = inner.free.pop()?;
        let frame = inner.slots[slot as usize].take()?;
        self.count.fetch_sub(1, Ordering::Relaxed);
        Some((slot, frame))
    }

    /// Return an [`acquire`](Self::acquire)d frame to its slot and free the slot.
    pub(crate) fn restore(&self, slot: u16, frame: Frame<PacketMeta>) {
        let mut guard = self.inner.lock();
        if let Some(inner) = guard.as_mut() {
            inner.slots[slot as usize] = Some(frame);
            inner.free.push(slot).expect("packet pool: restore push");
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Free-slot count; racy under concurrent access.
    #[inline]
    pub fn available(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }
}
