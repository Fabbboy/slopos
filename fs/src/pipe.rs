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

use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT, POLLPRI};
use slopos_sync::{IrqMutex, IrqMutexGuard, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, WaitQueue};

pub(crate) const MAX_PIPES: usize = 64;
pub(crate) const PIPE_BUFFER_SIZE: usize = 4096;
pub(crate) const INVALID_PIPE_ID: u32 = u32::MAX;

pub(crate) static READER_WQS: [WaitQueue; MAX_PIPES] = [const { WaitQueue::new() }; MAX_PIPES];
pub(crate) static WRITER_WQS: [WaitQueue; MAX_PIPES] = [const { WaitQueue::new() }; MAX_PIPES];

#[inline]
pub(crate) fn reader_wq(pipe_id: u32) -> &'static WaitQueue {
    &READER_WQS[pipe_id as usize]
}

#[inline]
pub(crate) fn writer_wq(pipe_id: u32) -> &'static WaitQueue {
    &WRITER_WQS[pipe_id as usize]
}

pub(crate) struct PipeSlot {
    pub(crate) valid: bool,
    read_pos: usize,
    write_pos: usize,
    pub(crate) len: usize,
    pub(crate) readers: u16,
    pub(crate) writers: u16,
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
            buffer: [0; PIPE_BUFFER_SIZE],
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

/// Allocate a new pipe slot. Returns the pipe ID.
///
/// Takes the allocation bitmap lock briefly to find a free slot, then
/// locks the individual pipe slot to initialize it.
pub(crate) fn alloc_slot() -> Option<u32> {
    let mut alloc = PIPE_ALLOC.lock();
    for (idx, used) in alloc.used.iter_mut().enumerate() {
        if !*used {
            *used = true;
            drop(alloc); // Release alloc lock before taking pipe lock.
            let mut slot = PIPE_SLOTS[idx].lock();
            *slot = PipeSlot::new();
            slot.valid = true;
            return Some(idx as u32);
        }
    }
    None
}

/// Lock a pipe slot by ID, returning a guard if the slot is valid.
///
/// This is the primary accessor for pipe operations. Replaces the old
/// `PIPE_STATE.lock()` + `slot_mut()` pattern with per-pipe locking.
#[inline]
pub(crate) fn lock_slot(pipe_id: u32) -> Option<IrqMutexGuard<'static, PipeSlot>> {
    let idx = pipe_id as usize;
    if idx >= MAX_PIPES {
        return None;
    }
    let guard = PIPE_SLOTS[idx].lock();
    if !guard.valid {
        return None;
    }
    Some(guard)
}

/// Free a pipe slot. Marks it as unused in the allocation bitmap.
pub(crate) fn free_slot(pipe_id: u32) {
    let idx = pipe_id as usize;
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
