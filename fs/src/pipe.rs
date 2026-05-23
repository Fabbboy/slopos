//! Kernel pipe implementation with per-pipe locking.
//!
//! # Locking
//!
//! Each pipe slot has its own [`SpinLock`], so operations on independent
//! pipes never contend. A separate [`PIPE_ALLOC`] lock protects only the
//! allocation bitmap (touched on pipe create/destroy only).
//!
//! Blocking and wakeups go through the kernel event bus keyed by
//! `KernelEvent::PipeRead` / `KernelEvent::PipeWrite`, so wakers and sleepers
//! never hold a pipe lock and a wait-queue lock simultaneously.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT, POLLPRI};
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SpinLock, SpinLockGuard};

pub(crate) use slopos_abi::event::MAX_PIPES;
pub(crate) const PIPE_BUFFER_SIZE: usize = 4096;

/// Bits used for the slot index in the handle encoding.
const SLOT_BITS: u32 = 8;
const SLOT_MASK: usize = (1 << SLOT_BITS) - 1; // 0xFF — supports up to 256 slots

// ---------------------------------------------------------------------------
// PipeHandle — type-safe handle for pipe kernel objects
// ---------------------------------------------------------------------------

/// Opaque handle identifying a kernel pipe slot.
///
/// Encodes a slot index and a generation counter so that stale handles
/// (from a closed pipe whose slot was recycled) are reliably rejected.
/// The encoding is `(generation << SLOT_BITS) | slot_index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct PipeHandle(u32);

impl PipeHandle {
    /// Sentinel value representing no pipe.
    pub const INVALID: Self = Self(u32::MAX);

    pub(crate) fn new(slot: usize, generation: u32) -> Self {
        Self(((generation as usize) << SLOT_BITS | (slot & SLOT_MASK)) as u32)
    }

    pub(crate) fn slot(self) -> usize {
        (self.0 as usize) & SLOT_MASK
    }

    pub(crate) fn generation(self) -> u32 {
        ((self.0 as usize) >> SLOT_BITS) as u32
    }

    /// Convert to usize for storage in OpenFileEntry.handle.
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// Reconstruct from usize stored in OpenFileEntry.handle.
    pub fn from_usize(v: usize) -> Self {
        Self(v as u32)
    }
}

/// Global generation counter for pipe slot allocation.
static PIPE_GENERATION: AtomicU32 = AtomicU32::new(1);

pub(crate) struct PipeSlot {
    pub(crate) valid: bool,
    read_pos: usize,
    write_pos: usize,
    pub(crate) len: usize,
    pub(crate) readers: u16,
    pub(crate) writers: u16,
    pub(crate) generation: u32,
    buffer: [u8; PIPE_BUFFER_SIZE],
}

impl PipeSlot {
    pub(crate) const fn new() -> Self {
        Self {
            valid: false,
            read_pos: 0,
            write_pos: 0,
            len: 0,
            readers: 0,
            writers: 0,
            generation: 0,
            buffer: [0; PIPE_BUFFER_SIZE],
        }
    }

    /// Reset a PipeSlot in place. Field-by-field zero so neither a
    /// fresh `PipeSlot` rvalue (4 KiB) nor a raw-pointer write_bytes
    /// touches the caller's stack — `[u8; PIPE_BUFFER_SIZE]::fill` is
    /// an in-place memset.
    pub(crate) fn reset(&mut self) {
        self.valid = false;
        self.read_pos = 0;
        self.write_pos = 0;
        self.len = 0;
        self.readers = 0;
        self.writers = 0;
        self.generation = 0;
        self.buffer.fill(0);
    }

    /// Atomically read and consume bytes from the pipe buffer.
    ///
    /// Data is consumed from the ring buffer in a single operation while
    /// the caller holds the per-pipe lock. The consumed bytes are copied
    /// into the kernel staging buffer `out`; the caller is responsible for
    /// transferring them to userspace *after* releasing the lock.
    pub(crate) fn read_into(&mut self, out: &mut [u8]) -> usize {
        let mut copied = 0usize;
        while copied < out.len() && self.len > 0 {
            out[copied] = self.buffer[self.read_pos];
            self.read_pos = (self.read_pos + 1) % PIPE_BUFFER_SIZE;
            self.len -= 1;
            copied += 1;
        }
        copied
    }

    pub(crate) fn write_from(&mut self, input: &[u8]) -> usize {
        let mut written = 0usize;
        while written < input.len() && self.len < PIPE_BUFFER_SIZE {
            self.buffer[self.write_pos] = input[written];
            self.write_pos = (self.write_pos + 1) % PIPE_BUFFER_SIZE;
            self.len += 1;
            written += 1;
        }
        written
    }

    pub(crate) fn revents(&self, is_read_end: bool, is_write_end: bool, events: u16) -> u16 {
        let mut revents = 0u16;

        if is_read_end {
            if self.len > 0 {
                revents |= events & (POLLIN | POLLPRI);
            }
            if self.writers == 0 {
                revents |= POLLHUP;
                if (events & POLLIN) != 0 {
                    revents |= POLLIN;
                }
            }
        }

        if is_write_end {
            if self.readers == 0 {
                revents |= POLLERR | POLLHUP;
            } else if self.len < PIPE_BUFFER_SIZE {
                revents |= events & POLLOUT;
            }
        }

        revents
    }
}

// ---------------------------------------------------------------------------
// Per-pipe locks — each pipe has its own SpinLock.
// ---------------------------------------------------------------------------

/// Per-pipe locks. Each slot is independently locked.
pub(crate) static PIPE_SLOTS: [SpinLock<PipeSlot>; MAX_PIPES] =
    [const { SpinLock::new(PipeSlot::new(), LOCK_LEVEL_RESOURCE) }; MAX_PIPES];

/// Allocation bitmap — only locked during pipe create/destroy.
struct PipeAllocBitmap {
    used: [bool; MAX_PIPES],
}

impl PipeAllocBitmap {
    const fn new() -> Self {
        Self {
            used: [false; MAX_PIPES],
        }
    }
}

static PIPE_ALLOC: SpinLock<PipeAllocBitmap> =
    SpinLock::new(PipeAllocBitmap::new(), LOCK_LEVEL_REGISTRY);

/// Allocate a new pipe slot. Returns a [`PipeHandle`] encoding the slot
/// index and a monotonic generation counter for stale-handle detection.
///
/// Takes the allocation bitmap lock briefly to find a free slot, then
/// locks the individual pipe slot to initialize it.
pub(crate) fn alloc_slot() -> Option<PipeHandle> {
    let mut alloc = PIPE_ALLOC.lock();
    for (idx, used) in alloc.used.iter_mut().enumerate() {
        if !*used {
            *used = true;
            drop(alloc); // Release alloc lock before taking pipe lock.
            let gn = PIPE_GENERATION.fetch_add(1, Ordering::Relaxed);
            let mut slot = PIPE_SLOTS[idx].lock();
            slot.reset();
            slot.valid = true;
            slot.generation = gn;
            return Some(PipeHandle::new(idx, gn));
        }
    }
    None
}

/// Lock a pipe slot by handle, returning a guard if the slot is valid
/// and the generation matches (stale-handle detection).
///
/// This is the primary accessor for pipe operations.
#[inline]
pub(crate) fn lock_slot(handle: PipeHandle) -> Option<SpinLockGuard<'static, PipeSlot>> {
    let idx = handle.slot();
    if idx >= MAX_PIPES {
        return None;
    }
    let guard = PIPE_SLOTS[idx].lock();
    if !guard.valid || guard.generation != handle.generation() {
        return None;
    }
    Some(guard)
}

/// Free a pipe slot. Marks it as unused in the allocation bitmap.
pub(crate) fn free_slot(handle: PipeHandle) {
    let idx = handle.slot();
    if idx >= MAX_PIPES {
        return;
    }
    // Invalidate the slot first, then release the alloc bitmap.
    {
        let mut slot = PIPE_SLOTS[idx].lock();
        slot.valid = false;
    }
    let mut alloc = PIPE_ALLOC.lock();
    alloc.used[idx] = false;
}
