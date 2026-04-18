//! Kernel pipe implementation with per-pipe locking.
//!
//! # Locking
//!
//! Each pipe slot has its own [`IrqMutex`], so operations on independent
//! pipes never contend. A separate [`PIPE_ALLOC`] lock protects only the
//! allocation bitmap (touched on pipe create/destroy only).
//!
//! Wait queues live in separate statics ([`READER_WQS`], [`WRITER_WQS`])
//! indexed by pipe slot, so wakers and sleepers never hold a pipe lock
//! and a wait-queue lock simultaneously.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT, POLLPRI};
use slopos_sync::{IrqMutex, IrqMutexGuard, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, WaitQueue};

pub(crate) const MAX_PIPES: usize = 64;
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

pub(crate) static READER_WQS: [WaitQueue; MAX_PIPES] = [const { WaitQueue::new() }; MAX_PIPES];
pub(crate) static WRITER_WQS: [WaitQueue; MAX_PIPES] = [const { WaitQueue::new() }; MAX_PIPES];

#[inline]
pub(crate) fn reader_wq(handle: PipeHandle) -> &'static WaitQueue {
    &READER_WQS[handle.slot()]
}

#[inline]
pub(crate) fn writer_wq(handle: PipeHandle) -> &'static WaitQueue {
    &WRITER_WQS[handle.slot()]
}

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

    /// Reset a PipeSlot in place without materialising a fresh
    /// `PipeSlot` rvalue on the caller's stack (`*slot = PipeSlot::new()`
    /// would cost 4 KiB).  Every field's default is zero, so a
    /// `write_bytes` zero-fill is the entire reset.
    ///
    /// # Safety
    /// `this` must point to a properly aligned, exclusively-owned
    /// `PipeSlot` slot that the caller guarantees has already released
    /// any logical resources (buffer content, wait queue refs).
    pub(crate) unsafe fn reset_in_place(this: *mut PipeSlot) {
        unsafe {
            core::ptr::write_bytes(this, 0, 1);
        }
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
// Per-pipe locks — each pipe has its own IrqMutex.
// ---------------------------------------------------------------------------

/// Per-pipe locks. Each slot is independently locked.
pub(crate) static PIPE_SLOTS: [IrqMutex<PipeSlot>; MAX_PIPES] =
    [const { IrqMutex::new(PipeSlot::new(), LOCK_LEVEL_RESOURCE) }; MAX_PIPES];

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

static PIPE_ALLOC: IrqMutex<PipeAllocBitmap> =
    IrqMutex::new(PipeAllocBitmap::new(), LOCK_LEVEL_REGISTRY);

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
            // Reset via pointer to avoid a 4 KiB stack rvalue.
            unsafe { PipeSlot::reset_in_place(&mut *slot as *mut PipeSlot) };
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
pub(crate) fn lock_slot(handle: PipeHandle) -> Option<IrqMutexGuard<'static, PipeSlot>> {
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
