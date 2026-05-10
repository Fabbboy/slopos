use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{POLLERR, POLLHUP};
use slopos_kernel_services::driver_runtime::scheduler_is_enabled;

use crate::pipe;
use crate::pipe::PipeHandle;

pub struct PipeReadOps;
pub struct PipeWriteOps;

pub static PIPE_READ_OPS: PipeReadOps = PipeReadOps;
pub static PIPE_WRITE_OPS: PipeWriteOps = PipeWriteOps;

fn pipe_dup_reader(h: PipeHandle) -> Option<usize> {
    if h == PipeHandle::INVALID {
        return None;
    }
    let mut slot = pipe::lock_slot(h)?;
    slot.readers = slot.readers.saturating_add(1);
    Some(h.as_usize())
}

fn pipe_dup_writer(h: PipeHandle) -> Option<usize> {
    if h == PipeHandle::INVALID {
        return None;
    }
    let mut slot = pipe::lock_slot(h)?;
    slot.writers = slot.writers.saturating_add(1);
    Some(h.as_usize())
}

fn pipe_release_reader(h: PipeHandle) {
    if h == PipeHandle::INVALID {
        return;
    }
    let mut wake_writers = false;
    let should_free = {
        if let Some(mut slot) = pipe::lock_slot(h) {
            if slot.readers > 0 {
                slot.readers -= 1;
                if slot.readers == 0 {
                    wake_writers = true;
                }
            }
            slot.readers == 0 && slot.writers == 0
        } else {
            false
        }
    };
    if should_free {
        pipe::free_slot(h);
    }
    if wake_writers {
        pipe::writer_wq(h).wake_all();
    }
}

fn pipe_release_writer(h: PipeHandle) {
    if h == PipeHandle::INVALID {
        return;
    }
    let mut wake_readers = false;
    let should_free = {
        if let Some(mut slot) = pipe::lock_slot(h) {
            if slot.writers > 0 {
                slot.writers -= 1;
                if slot.writers == 0 {
                    wake_readers = true;
                }
            }
            slot.readers == 0 && slot.writers == 0
        } else {
            false
        }
    };
    if should_free {
        pipe::free_slot(h);
    }
    if wake_readers {
        pipe::reader_wq(h).wake_all();
    }
}

impl FileOps for PipeReadOps {
    fn kind(&self) -> FileKind {
        FileKind::PipeRead
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, flags: u32) -> isize {
        if buf.is_empty() {
            return 0;
        }
        let h = PipeHandle::from_usize(handle);
        let is_nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
        let mut local = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
        let mut total = 0usize;
        let mut remaining = buf.len();

        loop {
            // Snapshot under the slot lock: try to consume data and
            // observe writer count.
            let (consumed, no_writers, slot_gone) = {
                match pipe::lock_slot(h) {
                    Some(mut slot) => {
                        let consumed = if remaining > 0 && slot.len > 0 {
                            let chunk = remaining.min(local.len());
                            slot.read_into(&mut local[..chunk])
                        } else {
                            0
                        };
                        (consumed, slot.writers == 0, false)
                    }
                    None => (0, true, true),
                }
            };

            if slot_gone {
                return if total > 0 {
                    total as isize
                } else {
                    Errno::EBADF.as_isize()
                };
            }

            if consumed > 0 {
                match buf.copy_in(total, &local[..consumed]) {
                    Ok(n) => {
                        total += n;
                        remaining -= n;
                    }
                    Err(_) => {
                        return if total > 0 {
                            total as isize
                        } else {
                            Errno::EFAULT.as_isize()
                        };
                    }
                }
                pipe::writer_wq(h).wake_one();
                continue;
            }

            if total > 0 {
                return total as isize;
            }
            if no_writers {
                return 0;
            }
            if is_nonblock {
                return Errno::EAGAIN.as_isize();
            }
            if scheduler_is_enabled() == 0 {
                return Errno::EAGAIN.as_isize();
            }

            // Block via wait_event so the queue's SpinLock pairs with
            // the producer's wake_one and the Running→Blocked CAS
            // happens under the same lock the wake side acquires —
            // closing the lost-wakeup window without any consumer-side
            // ad-hoc state CAS. The closure re-checks data/EOF under
            // the slot lock so the wake-up condition is observed
            // atomically with respect to the producer's slot store.
            pipe::reader_wq(h).wait_event(|| match pipe::lock_slot(h) {
                Some(slot) => slot.len > 0 || slot.writers == 0,
                // Slot evaporated under us — fall out of the wait so
                // the next iteration's lock_slot returns EBADF.
                None => true,
            });
        }
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EBADF.as_isize()
    }

    fn release(&self, handle: usize) {
        pipe_release_reader(PipeHandle::from_usize(handle));
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        pipe_dup_reader(PipeHandle::from_usize(handle))
    }

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        let h = PipeHandle::from_usize(handle);
        // Register FIRST, then check readiness (Linux pattern).
        let registered = pipe::reader_wq(h).enqueue_current();
        let revents = match pipe::lock_slot(h) {
            Some(slot) => slot.revents(true, false, events),
            None => POLLERR,
        };
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_idx: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let h = PipeHandle::from_usize(handle);
        match pipe::lock_slot(h) {
            Some(slot) => slot.revents(true, false, events),
            None => POLLERR,
        }
    }

    fn poll_wait(&self, handle: usize) -> bool {
        pipe::reader_wq(PipeHandle::from_usize(handle)).enqueue_current()
    }

    fn poll_unwait(&self, handle: usize) {
        pipe::reader_wq(PipeHandle::from_usize(handle)).remove_current();
    }
}

impl FileOps for PipeWriteOps {
    fn kind(&self) -> FileKind {
        FileKind::PipeWrite
    }

    fn read(&self, _handle: usize, _buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        Errno::EBADF.as_isize()
    }

    fn write(&self, handle: usize, buf: &dyn IoBufRead, _offset: u64, flags: u32) -> isize {
        if buf.is_empty() {
            return 0;
        }
        let h = PipeHandle::from_usize(handle);
        let is_nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
        let buf_len = buf.len();
        let mut total = 0usize;
        let mut local = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };

        // The wait condition is "buffer has room OR peer is gone": we
        // can make forward progress in either case (push more bytes,
        // or report EPIPE). Used by both the pre-stage block (buffer
        // full) and the post-stage block (buffer filled mid-write).
        let drain_or_close = || match pipe::lock_slot(h) {
            Some(slot) => slot.len < pipe::PIPE_BUFFER_SIZE || slot.readers == 0,
            None => true,
        };

        loop {
            // Snapshot under the slot lock.
            let (can_write, no_readers, slot_gone) = match pipe::lock_slot(h) {
                Some(slot) => (slot.len < pipe::PIPE_BUFFER_SIZE, slot.readers == 0, false),
                None => (false, true, true),
            };

            if slot_gone {
                return if total > 0 {
                    total as isize
                } else {
                    Errno::EBADF.as_isize()
                };
            }
            if no_readers {
                return if total > 0 {
                    total as isize
                } else {
                    Errno::EPIPE.as_isize()
                };
            }

            if !can_write {
                if total >= buf_len {
                    return total as isize;
                }
                if is_nonblock {
                    return if total > 0 {
                        total as isize
                    } else {
                        Errno::EAGAIN.as_isize()
                    };
                }
                if scheduler_is_enabled() == 0 {
                    return Errno::EAGAIN.as_isize();
                }
                pipe::writer_wq(h).wait_event(drain_or_close);
                continue;
            }

            if total >= buf_len {
                return total as isize;
            }
            let chunk = (buf_len - total).min(local.len());
            let staged = match buf.copy_out(total, &mut local[..chunk]) {
                Ok(n) => n,
                Err(_) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        Errno::EFAULT.as_isize()
                    };
                }
            };
            if staged == 0 {
                return total as isize;
            }

            // Push the staged bytes under the slot lock and wake one
            // reader if anything was buffered. The slot lock pairs
            // with the reader's wait_event closure (which also takes
            // the slot lock) — the reader's condition observation
            // happens-before its WQ-lock acquire, and the WQ lock-pair
            // gives the cross-CPU visibility for the writer's update.
            {
                let Some(mut slot) = pipe::lock_slot(h) else {
                    return if total > 0 {
                        total as isize
                    } else {
                        Errno::EBADF.as_isize()
                    };
                };

                if slot.readers == 0 {
                    return if total > 0 {
                        total as isize
                    } else {
                        Errno::EPIPE.as_isize()
                    };
                }

                let written = slot.write_from(&local[..staged]);
                total += written;
                if written > 0 {
                    pipe::reader_wq(h).wake_one();
                }
            }

            if total >= buf_len {
                return total as isize;
            }
            if is_nonblock {
                return if total > 0 {
                    total as isize
                } else {
                    Errno::EAGAIN.as_isize()
                };
            }
            if scheduler_is_enabled() == 0 {
                return Errno::EAGAIN.as_isize();
            }
            // Buffer drained partially but the whole staged batch
            // didn't fit — wait for room and resume the loop.
            pipe::writer_wq(h).wait_event(drain_or_close);
        }
    }

    fn release(&self, handle: usize) {
        pipe_release_writer(PipeHandle::from_usize(handle));
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        pipe_dup_writer(PipeHandle::from_usize(handle))
    }

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        let h = PipeHandle::from_usize(handle);
        // Register FIRST, then check readiness (Linux pattern).
        let registered = pipe::writer_wq(h).enqueue_current();
        let revents = match pipe::lock_slot(h) {
            Some(slot) => slot.revents(false, true, events),
            None => POLLERR | POLLHUP,
        };
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_idx: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let h = PipeHandle::from_usize(handle);
        match pipe::lock_slot(h) {
            Some(slot) => slot.revents(false, true, events),
            None => POLLERR | POLLHUP,
        }
    }

    fn poll_wait(&self, handle: usize) -> bool {
        pipe::writer_wq(PipeHandle::from_usize(handle)).enqueue_current()
    }

    fn poll_unwait(&self, handle: usize) {
        pipe::writer_wq(PipeHandle::from_usize(handle)).remove_current();
    }
}
