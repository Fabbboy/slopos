//! Pre-allocated packet buffer pool.
//!
//! Provides O(1) alloc/release of fixed-size packet buffers. Each slot
//! is backed by a typed [`Frame<PacketMeta>`] page from the kernel
//! frame allocator; a buffer's bytes are reached through the frame's
//! `as_bytes`/`as_bytes_mut` HHDM views, so byte access is
//! borrow-checker-enforced rather than relying on raw-pointer aliasing.
//!
//! # Design rationale
//!
//! Linux uses `kmem_cache` (slab) for `sk_buff` allocation because
//! per-packet `kmalloc` is too slow and fragments the heap under load.
//! A fixed pool gives O(1) alloc/free and predictable memory use. The
//! frames are allocated once at [`init`](PacketPool::init) and recycled
//! for the kernel's lifetime; the free-list lives behind a leaf
//! `SpinLock` (the lock disables IRQs while held and acquires no other
//! lock, so it is safe to take from any context and can never head a
//! lock-ordering cycle).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use slopos_ostd::KVec;
use slopos_ostd::mm::frame::{Frame, PacketMeta};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

/// Usable size of each packet buffer in bytes.
///
/// Covers the maximum Ethernet frame (1518) plus headroom (128) with
/// room to spare. The backing frame is a full 4 KiB page; only the
/// first `BUF_SIZE` bytes are exposed to the network stack.
pub const BUF_SIZE: usize = 2048;

/// Number of pre-allocated buffer slots.
///
/// Each slot is backed by one 4 KiB frame, so the pool reserves
/// `POOL_SIZE * 4 KiB` of physical memory. Tunable: lower it for a
/// smaller footprint, raise it for more in-flight buffers.
pub const POOL_SIZE: usize = 256;

// =============================================================================
// Pool state
// =============================================================================

/// Mutable pool state, guarded by [`PacketPool::inner`].
struct PoolInner {
    /// Per-slot frame storage. `slots[i]` is `Some` while the frame for
    /// slot `i` is resident in the pool, and `None` while that frame is
    /// lent to a live [`PacketBuf`](super::packetbuf::PacketBuf). The
    /// length is however many frames [`init`](PacketPool::init) managed
    /// to allocate (`<= POOL_SIZE`).
    slots: KVec<Option<Frame<PacketMeta>>>,
    /// Free-list of slot indices: `i` is present iff `slots[i]` is
    /// resident and not currently lent out.
    free: KVec<u16>,
}

/// Pre-allocated packet buffer pool.
///
/// `alloc`/`release` operate on bare `u16` slot handles; `acquire`/
/// `restore` additionally move the backing [`Frame<PacketMeta>`] in and
/// out, so a [`PacketBuf`](super::packetbuf::PacketBuf) can own its
/// frame by value and mutate the bytes under a genuine `&mut`.
pub struct PacketPool {
    /// `None` until [`init`](Self::init) populates it.
    inner: SpinLock<Option<PoolInner>>,
    /// Whether [`init`](Self::init) has run.
    initialized: AtomicBool,
    /// Number of free slots — equals `inner.free.len()` while
    /// initialized. Held as a standalone atomic so [`available`] is a
    /// lock-free read.
    count: AtomicUsize,
}

/// The global packet pool singleton.
///
/// Call [`PacketPool::init`] once before any networking code runs.
pub static PACKET_POOL: PacketPool = PacketPool {
    inner: SpinLock::new(None, LOCK_LEVEL_RESOURCE),
    initialized: AtomicBool::new(false),
    count: AtomicUsize::new(0),
};

impl PacketPool {
    /// Allocate the pool's backing frames and build the free-list.
    ///
    /// Idempotent — subsequent calls are no-ops. Allocates up to
    /// [`POOL_SIZE`] frames from the kernel frame allocator; if the
    /// allocator is short, the pool is built with however many frames
    /// were available rather than panicking.
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
                // Allocator exhausted — keep the slots already built.
                None => break,
            }
        }

        let available = free.len();
        *self.inner.lock() = Some(PoolInner { slots, free });
        self.count.store(available, Ordering::Release);
    }

    /// Reserve a free slot, returning its index. The backing frame stays
    /// resident in the pool (use [`acquire`](Self::acquire) to take
    /// ownership of the frame too). Returns `None` if the pool is
    /// exhausted or not yet initialized.
    pub fn alloc(&self) -> Option<u16> {
        let mut guard = self.inner.lock();
        let inner = guard.as_mut()?;
        let slot = inner.free.pop()?;
        self.count.fetch_sub(1, Ordering::Relaxed);
        Some(slot)
    }

    /// Return a slot index reserved by [`alloc`](Self::alloc) to the
    /// free-list. The slot's frame must still be resident.
    pub fn release(&self, slot: u16) {
        let mut guard = self.inner.lock();
        if let Some(inner) = guard.as_mut() {
            inner.free.push(slot).expect("packet pool: release push");
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Reserve a free slot *and* take ownership of its backing frame.
    ///
    /// Returns the slot index plus the moved-out [`Frame<PacketMeta>`],
    /// or `None` if the pool is exhausted / not yet initialized. The
    /// frame is returned to the pool by [`restore`](Self::restore).
    pub(crate) fn acquire(&self) -> Option<(u16, Frame<PacketMeta>)> {
        let mut guard = self.inner.lock();
        let inner = guard.as_mut()?;
        let slot = inner.free.pop()?;
        let frame = inner.slots[slot as usize].take()?;
        self.count.fetch_sub(1, Ordering::Relaxed);
        Some((slot, frame))
    }

    /// Return a frame taken via [`acquire`](Self::acquire) to its slot
    /// and mark the slot free again.
    pub(crate) fn restore(&self, slot: u16, frame: Frame<PacketMeta>) {
        let mut guard = self.inner.lock();
        if let Some(inner) = guard.as_mut() {
            inner.slots[slot as usize] = Some(frame);
            inner.free.push(slot).expect("packet pool: restore push");
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Number of free buffer slots (diagnostic; racy under concurrent
    /// access).
    #[inline]
    pub fn available(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Whether [`init`](Self::init) has been called.
    #[inline]
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }
}
