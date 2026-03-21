//! Kernel pipe implementation.
//!
//! # Locking
//!
//! Pipe data (ring buffer, reader/writer counts) is protected by [`PIPE_STATE`].
//! Wait queues live in separate statics ([`READER_WQS`], [`WRITER_WQS`]) indexed
//! by pipe slot, so wakers and sleepers never hold `PIPE_STATE` and a wait-queue
//! lock simultaneously.

use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT, POLLPRI};
use slopos_sync::{IrqMutex, WaitQueue};

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

pub(crate) struct PipeState {
    pub(crate) slots: [PipeSlot; MAX_PIPES],
}

// SAFETY: PipeState is only accessed through the PIPE_STATE IrqMutex.
unsafe impl Send for PipeState {}

impl PipeState {
    const fn new() -> Self {
        Self {
            slots: [const { PipeSlot::new() }; MAX_PIPES],
        }
    }
}

pub(crate) static PIPE_STATE: IrqMutex<PipeState> = IrqMutex::new(PipeState::new());

pub(crate) fn alloc_slot() -> Option<u32> {
    let mut state = PIPE_STATE.lock();
    for (idx, slot) in state.slots.iter_mut().enumerate() {
        if !slot.valid {
            *slot = PipeSlot::new();
            slot.valid = true;
            return Some(idx as u32);
        }
    }
    None
}

pub(crate) fn slot_mut(state: &mut PipeState, pipe_id: u32) -> Option<&mut PipeSlot> {
    let idx = pipe_id as usize;
    if idx >= MAX_PIPES {
        return None;
    }
    let slot = &mut state.slots[idx];
    if !slot.valid {
        return None;
    }
    Some(slot)
}
