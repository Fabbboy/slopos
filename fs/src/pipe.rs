//! Kernel pipe implementation.
//!
//! # Storage
//!
//! Pipes live in a single [`HandleTable`] behind one [`SpinLock`]. Each
//! pipe is a [`Pipe`] owning a heap ring buffer; the table mints a
//! generation-checked handle per pipe, so a handle left over from a closed
//! pipe whose slot was recycled resolves to a typed error (and the caller
//! reports `EBADF`) rather than aliasing the recycled pipe.
//!
//! A single table lock (rather than one lock per pipe) keeps the moving of
//! a non-`Copy` [`Pipe`] in and out of shared storage in safe Rust. Pipe
//! operations are short — a ring-buffer memcpy performed while the lock is
//! held, with the lock always released before any wait or wake — and the
//! table is capped at [`MAX_PIPES`] entries, so contention is negligible.
//!
//! # Blocking and wakeups
//!
//! Blocking and wakeups go through the kernel event bus keyed by
//! `KernelEvent::PipeRead` / `KernelEvent::PipeWrite`. The accessors below
//! never let a table guard escape their closure, so a sleeper or waker
//! never holds the table lock across a wait-queue operation.

use slopos_abi::quota::ObjectRow;
use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT, POLLPRI};
use slopos_ostd::KVec;
use slopos_ostd::handle::{Handle, HandleTable};
use slopos_ostd::lock_class;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{Charge, try_charge};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

pub(crate) use slopos_abi::event::MAX_PIPES;
pub(crate) const PIPE_BUFFER_SIZE: usize = 4096;

/// Bits reserved for the slot index in the packed handle encoding; the
/// remaining bits hold the generation (see [`Handle::pack`]). 8 bits cover
/// MAX_PIPES (≤ 256) slots.
const SLOT_BITS: u32 = 8;

// ---------------------------------------------------------------------------
// PipeHandle — packed, FD-facing handle for pipe kernel objects
// ---------------------------------------------------------------------------

/// Opaque handle identifying a kernel pipe.
///
/// The file layer stores a pipe reference inside a single `usize`
/// (`OpenFile::handle`) shared by every file backend, so the pipe handle
/// must pack into that width. The encoding is
/// `(generation << SLOT_BITS) | slot_index`: the low 8 bits hold the slot
/// (which also keys the event bus), and the upper 56 bits hold the
/// generation. That is a far larger generation space than a pipe can
/// exhaust, so a recycled slot is always detectable as stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct PipeHandle(u64);

impl PipeHandle {
    /// Sentinel value representing no pipe.
    pub const INVALID: Self = Self(u64::MAX);

    /// Pack an internal [`Handle`] into the FD-facing encoding.
    pub(crate) fn pack(h: Handle<Pipe>) -> Self {
        Self(h.pack(SLOT_BITS) as u64)
    }

    /// Rebuild the internal [`Handle`], or `None` if this is the sentinel
    /// or names an out-of-range slot.
    pub(crate) fn to_internal(self) -> Option<Handle<Pipe>> {
        if self == Self::INVALID {
            return None;
        }
        let h = Handle::unpack(self.0 as usize, SLOT_BITS);
        if h.slot() as usize >= MAX_PIPES {
            return None;
        }
        Some(h)
    }

    /// The slot index — also the event-bus key for this pipe.
    pub(crate) fn slot(self) -> usize {
        Handle::<Pipe>::unpack(self.0 as usize, SLOT_BITS).slot() as usize
    }

    /// Convert to `usize` for storage in `OpenFile::handle`.
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// Reconstruct from a `usize` stored in `OpenFile::handle`.
    pub fn from_usize(v: usize) -> Self {
        Self(v as u64)
    }
}

/// A single kernel pipe: a heap-backed ring buffer plus reader/writer
/// reference counts.
pub(crate) struct Pipe {
    read_pos: usize,
    write_pos: usize,
    pub(crate) len: usize,
    pub(crate) readers: u16,
    pub(crate) writers: u16,
    buffer: KVec<u8>,
    /// The registry row and its buffer, charged to the `pipe2` caller.
    ///
    /// Here rather than in either backing, because a pipe has **two**
    /// backings releasing into **one** slot: a charge in each would refund
    /// twice, and a charge in one would refund while the object was still
    /// alive behind the other. The row is freed exactly once, when both ends
    /// are gone, so it is the only place the lifetime of the charge and the
    /// lifetime of the object coincide.
    #[expect(dead_code, reason = "held for ownership; dropping it is the refund")]
    object_charge: Charge<ObjectRow>,
}

impl Pipe {
    fn new(buffer: KVec<u8>, object_charge: Charge<ObjectRow>) -> Self {
        Self {
            read_pos: 0,
            write_pos: 0,
            len: 0,
            readers: 0,
            writers: 0,
            buffer,
            object_charge,
        }
    }

    /// Read and consume bytes from the pipe buffer.
    ///
    /// Data is consumed from the ring buffer while the caller holds the
    /// table lock. The consumed bytes are copied into the kernel staging
    /// buffer `out`; the caller transfers them to userspace *after*
    /// releasing the lock.
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
// Pipe table — one HandleTable behind one lock.
// ---------------------------------------------------------------------------

/// All live pipes. A growable [`HandleTable`] capped at [`MAX_PIPES`] live
/// entries by [`alloc_slot`]; the cap keeps slot indices within the
/// [`SLOT_BITS`]-wide field of [`PipeHandle`] and the event-bus key.
static PIPE_TABLE: SpinLock<HandleTable<Pipe>> = SpinLock::new(
    HandleTable::new(),
    lock_class!("PIPE_TABLE", LOCK_LEVEL_RESOURCE),
);

/// Allocate a new pipe, returning its packed [`PipeHandle`].
///
/// The ring buffer is allocated before the table lock is taken so the
/// locked region stays allocation-light. Returns `None` if the pipe table
/// is full or the buffer allocation fails.
pub(crate) fn alloc_slot(account: AccountId) -> Option<PipeHandle> {
    let buffer = KVec::<u8>::zeroed(PIPE_BUFFER_SIZE).ok()?;
    // Charged before the table lock: a refusal must not unwind under it.
    let reservation = try_charge::<ObjectRow>(account, 1).ok()?;
    let pipe = Pipe::new(buffer, Charge::commit(reservation));
    let mut table = PIPE_TABLE.lock();
    if table.len() >= MAX_PIPES {
        return None;
    }
    let handle = table.insert(pipe).ok()?;
    Some(PipeHandle::pack(handle))
}

/// Run `f` with shared access to the pipe named by `handle`. Returns
/// `None` if the handle is stale, recycled, or invalid.
pub(crate) fn with_pipe<R>(handle: PipeHandle, f: impl FnOnce(&Pipe) -> R) -> Option<R> {
    let internal = handle.to_internal()?;
    let table = PIPE_TABLE.lock();
    let pipe = table.get(internal).ok()?;
    Some(f(pipe))
}

/// Run `f` with mutable access to the pipe named by `handle`. Returns
/// `None` if the handle is stale, recycled, or invalid. The table lock is
/// always released when `f` returns, so callers must perform any wait or
/// wake outside this closure.
pub(crate) fn with_pipe_mut<R>(handle: PipeHandle, f: impl FnOnce(&mut Pipe) -> R) -> Option<R> {
    let internal = handle.to_internal()?;
    let mut table = PIPE_TABLE.lock();
    let pipe = table.get_mut(internal).ok()?;
    Some(f(pipe))
}

/// Free a pipe, dropping its ring buffer and bumping the slot generation
/// so any surviving handle to that slot becomes stale.
pub(crate) fn free_slot(handle: PipeHandle) {
    if let Some(internal) = handle.to_internal() {
        let mut table = PIPE_TABLE.lock();
        let _ = table.remove(internal);
    }
}
