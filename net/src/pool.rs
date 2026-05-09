//! Pre-allocated packet buffer pool with lock-free allocation.
//!
//! Provides O(1) alloc/release from any context (including interrupts) via
//! a Treiber stack with ABA-safe tagged pointers.  The backing storage is a
//! static 512 KiB array in BSS, 64-byte aligned for cache-line friendliness.
//!
//! # Design rationale
//!
//! Linux uses `kmem_cache` (slab) for `sk_buff` allocation because per-packet
//! `kmalloc` is too slow and causes heap fragmentation under load.  A fixed pool
//! gives O(1) alloc/free, predictable memory usage, and cache-friendly layout.
//! The lock-free Treiber stack avoids disabling interrupts on the alloc/release
//! hot path, using a version-tagged CAS to prevent ABA races.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};

/// Size of each packet buffer slot in bytes.
///
/// Covers maximum Ethernet frame (1518) plus headroom (128) with room to spare.
pub const BUF_SIZE: usize = 2048;

/// Number of pre-allocated buffer slots.
pub const POOL_SIZE: usize = 256;

/// Cache-line alignment for each slot (documentation constant).
pub const CACHE_LINE_ALIGN: usize = 64;

/// Sentinel value: end of freelist / pool exhausted.
const FREELIST_EMPTY: u16 = u16::MAX;

// =============================================================================
// Static backing storage
// =============================================================================

/// Raw buffer storage — 256 slots × 2048 bytes, 64-byte aligned.
///
/// Lives in BSS (zero-initialized, 512 KiB).  Interior mutability via
/// `UnsafeCell` is sound because the pool's allocation discipline guarantees
/// that each slot is owned by at most one [`PacketBuf`](super::packetbuf::PacketBuf)
/// at any time.
#[repr(C, align(64))]
struct PoolStorage {
    slots: UnsafeCell<[[u8; BUF_SIZE]; POOL_SIZE]>,
}

// SAFETY: Slot access is serialized by the pool ownership model.
// A slot is accessed exclusively by its owning PacketBuf (move-only, no Clone).
unsafe impl Sync for PoolStorage {}

static POOL_STORAGE: PoolStorage = PoolStorage {
    slots: UnsafeCell::new([[0u8; BUF_SIZE]; POOL_SIZE]),
};

// =============================================================================
// Pool metadata
// =============================================================================

/// Lock-free packet buffer pool.
///
/// Uses a Treiber stack (atomic CAS on a tagged head pointer) for O(1)
/// allocation and deallocation from any context, including interrupt handlers.
///
/// The head is a packed `u32`: bits `[15:0]` = slot index (or [`FREELIST_EMPTY`]),
/// bits `[31:16]` = version counter (ABA prevention).  The version wraps at
/// 65 536 which is sufficient for a hobby OS.
pub struct PacketPool {
    /// Tagged head pointer: `(version << 16) | index`.
    head: AtomicU32,
    /// Per-slot next-free pointer, forming the intrusive freelist.
    next: [AtomicU16; POOL_SIZE],
    /// Number of currently available (free) slots.
    count: AtomicUsize,
    /// Whether [`init`](PacketPool::init) has been called.
    initialized: AtomicBool,
}

// SAFETY: All fields use atomic types — no unsynchronized shared mutation.
unsafe impl Send for PacketPool {}
unsafe impl Sync for PacketPool {}

/// The global packet pool singleton.
///
/// Call [`PacketPool::init`] once at kernel boot before any networking code runs.
pub static PACKET_POOL: PacketPool = PacketPool {
    head: AtomicU32::new(FREELIST_EMPTY as u32),
    next: [const { AtomicU16::new(0) }; POOL_SIZE],
    count: AtomicUsize::new(0),
    initialized: AtomicBool::new(false),
};

impl PacketPool {
    /// Initialize the pool's freelist.
    ///
    /// Builds a linked list of free slots: `0 → 1 → 2 → … → 255 → ∅`.
    /// Must be called exactly once before networking starts.  Subsequent calls
    /// are harmless no-ops.
    pub fn init(&self) {
        if self.initialized.swap(true, Ordering::AcqRel) {
            return;
        }

        // Build freelist: each slot points to the next; last slot points to EMPTY.
        for i in 0..POOL_SIZE {
            let next = if i + 1 < POOL_SIZE {
                (i + 1) as u16
            } else {
                FREELIST_EMPTY
            };
            self.next[i].store(next, Ordering::Relaxed);
        }

        // Head = slot 0, version 0.  Release ordering makes all prior stores
        // (the next[] chain) visible to any thread that observes this write.
        self.head.store(0, Ordering::Release);
        self.count.store(POOL_SIZE, Ordering::Release);
    }

    /// Allocate a buffer slot.
    ///
    /// Returns `Some(slot_index)` on success, `None` if the pool is exhausted.
    /// O(1) amortized.  Safe from interrupt context (lock-free CAS loop).
    pub fn alloc(&self) -> Option<u16> {
        loop {
            let old = self.head.load(Ordering::Acquire);
            let idx = (old & 0xFFFF) as u16;
            if idx == FREELIST_EMPTY {
                return None;
            }
            let ver = old >> 16;
            let next_idx = self.next[idx as usize].load(Ordering::Relaxed);
            let new = (ver.wrapping_add(1) << 16) | (next_idx as u32);
            if self
                .head
                .compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                self.count.fetch_sub(1, Ordering::Relaxed);
                return Some(idx);
            }
            core::hint::spin_loop();
        }
    }

    /// Return a buffer slot to the pool.
    ///
    /// Called by [`PacketBuf::drop`](super::packetbuf::PacketBuf).  The slot must
    /// have been previously allocated from this pool.  O(1), lock-free.
    ///
    /// The caller must not access the slot's data after calling `release`.
    pub fn release(&self, slot: u16) {
        debug_assert!(
            (slot as usize) < POOL_SIZE,
            "release: slot index {} out of bounds",
            slot
        );
        loop {
            let old = self.head.load(Ordering::Acquire);
            let old_idx = (old & 0xFFFF) as u16;
            let ver = old >> 16;
            self.next[slot as usize].store(old_idx, Ordering::Relaxed);
            let new = (ver.wrapping_add(1) << 16) | (slot as u32);
            if self
                .head
                .compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                self.count.fetch_add(1, Ordering::Relaxed);
                return;
            }
            core::hint::spin_loop();
        }
    }

    /// Number of free buffer slots (diagnostic, racy under concurrent access).
    #[inline]
    pub fn available(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Whether the pool has been initialized.
    #[inline]
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Raw pointer to the first byte of slot `slot`.
    ///
    /// The returned pointer is valid for `BUF_SIZE` bytes.  The caller must
    /// own the slot (allocated and not yet released) and ensure no aliasing
    /// mutable references exist before dereferencing.
    #[inline]
    pub(crate) fn slot_data(&self, slot: u16) -> *mut u8 {
        debug_assert!((slot as usize) < POOL_SIZE);
        // SAFETY: UnsafeCell grants interior mutability.  Pointer arithmetic
        // is in-bounds because slot < POOL_SIZE and each slot is BUF_SIZE bytes.
        unsafe { (POOL_STORAGE.slots.get() as *mut u8).add(slot as usize * BUF_SIZE) }
    }
}
