//! Per-CPU object magazine.
//!
//! Each [`super::allocator::SlabAllocator<SIZE>`] holds one [`Magazine`]
//! per CPU through a `CpuLocal<Magazine>`. The fast path (`pop` /
//! `push`) is lock-free; the only synchronisation cost is the
//! `IrqPreemptGuard` the caller already takes for per-CPU access.
//!
//! Slots are stored as `usize` (raw pointer bits) so the magazine is
//! naturally `Send + Sync` without an `unsafe impl` (`mm` is
//! `#![forbid(unsafe_code)]`). The `NonNull<u8>` shape is recovered
//! only at the API boundary.

use core::ptr::NonNull;

/// Object-cache capacity per CPU per size class.
pub(crate) const MAGAZINE_CAPACITY: usize = 32;

/// One slot. `repr(transparent)` over `usize` so the layout is
/// identical to a raw pointer slot, but `usize` is naturally `Send +
/// Sync`, sparing the magazine its own marker.
#[repr(transparent)]
#[derive(Copy, Clone)]
struct Slot(usize);

impl Slot {
    const NULL: Self = Self(0);

    #[inline]
    fn from_ptr(p: NonNull<u8>) -> Self {
        Self(p.as_ptr() as usize)
    }

    #[inline]
    fn as_ptr(self) -> Option<NonNull<u8>> {
        NonNull::new(self.0 as *mut u8)
    }
}

/// A fixed-size stack of cached object pointers. `pop` / `push` are
/// single-threaded (per-CPU) operations gated by an
/// [`slopos_ostd::sync::IrqPreemptGuard`] at the call site.
#[repr(C)]
pub(crate) struct Magazine {
    slots: [Slot; MAGAZINE_CAPACITY],
    len: u32,
}

impl Magazine {
    pub(crate) const fn new() -> Self {
        Self {
            slots: [Slot::NULL; MAGAZINE_CAPACITY],
            len: 0,
        }
    }

    #[inline]
    pub(crate) fn count(&self) -> usize {
        self.len as usize
    }

    /// Pop the top object pointer, if any.
    #[inline]
    pub(crate) fn pop(&mut self) -> Option<NonNull<u8>> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let slot = self.slots[self.len as usize];
        self.slots[self.len as usize] = Slot::NULL;
        slot.as_ptr()
    }

    /// Push an object pointer. Returns `false` if the magazine is
    /// full (caller drains and retries).
    #[inline]
    pub(crate) fn push(&mut self, ptr: NonNull<u8>) -> bool {
        if (self.len as usize) >= MAGAZINE_CAPACITY {
            return false;
        }
        self.slots[self.len as usize] = Slot::from_ptr(ptr);
        self.len += 1;
        true
    }
}
