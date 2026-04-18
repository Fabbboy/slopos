use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{POLLERR, POLLHUP};
use slopos_kernel_services::driver_runtime::{
    block_current_task, finish_wait, prepare_to_wait, scheduler_is_enabled,
};

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
        let mut local = match slopos_alloc::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
        let mut total = 0usize;
        let mut remaining = buf.len();

        loop {
            let mut need_block = false;
            let mut consumed = 0usize;
            let no_writers;
            {
                let Some(mut slot) = pipe::lock_slot(h) else {
                    return if total > 0 {
                        total as isize
                    } else {
                        Errno::EBADF.as_isize()
                    };
                };

                if remaining > 0 && slot.len > 0 {
                    let chunk = remaining.min(local.len());
                    consumed = slot.read_into(&mut local[..chunk]);
                }

                no_writers = slot.writers == 0;
                if consumed == 0
                    && total == 0
                    && !no_writers
                    && !is_nonblock
                    && scheduler_is_enabled() != 0
                {
                    need_block = true;
                }
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

            if need_block {
                prepare_to_wait();
                pipe::reader_wq(h).enqueue_current();
                block_current_task();
                finish_wait();
                pipe::reader_wq(h).remove_current();
                continue;
            }
            return Errno::EAGAIN.as_isize();
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
        let mut local = match slopos_alloc::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };

        loop {
            let mut need_block = false;
            let can_write;
            {
                let Some(slot) = pipe::lock_slot(h) else {
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

                can_write = slot.len < pipe::PIPE_BUFFER_SIZE;
            }

            if !can_write {
                if total >= buf_len {
                    return total as isize;
                }
                if is_nonblock && total > 0 {
                    return total as isize;
                }
                if is_nonblock {
                    return Errno::EAGAIN.as_isize();
                }
                if scheduler_is_enabled() != 0 {
                    prepare_to_wait();
                    pipe::writer_wq(h).enqueue_current();
                    block_current_task();
                    finish_wait();
                    pipe::writer_wq(h).remove_current();
                    continue;
                }
                return Errno::EAGAIN.as_isize();
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
            if is_nonblock && total > 0 {
                return total as isize;
            }
            if is_nonblock {
                return Errno::EAGAIN.as_isize();
            }
            if scheduler_is_enabled() != 0 {
                need_block = true;
            }

            if need_block {
                prepare_to_wait();
                pipe::writer_wq(h).enqueue_current();
                block_current_task();
                finish_wait();
                pipe::writer_wq(h).remove_current();
                continue;
            }
            return Errno::EAGAIN.as_isize();
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
