use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{POLLERR, POLLHUP};
use slopos_kernel_services::driver_runtime::{
    block_current_task, finish_wait, prepare_to_wait, scheduler_is_enabled,
};

use crate::pipe;

pub struct PipeReadOps;
pub struct PipeWriteOps;

pub static PIPE_READ_OPS: PipeReadOps = PipeReadOps;
pub static PIPE_WRITE_OPS: PipeWriteOps = PipeWriteOps;

fn pipe_dup_reader(pipe_id: u32) -> Option<usize> {
    if pipe_id == pipe::INVALID_PIPE_ID {
        return None;
    }
    let mut pipe_state = pipe::PIPE_STATE.lock();
    let slot = pipe::slot_mut(&mut pipe_state, pipe_id)?;
    slot.readers = slot.readers.saturating_add(1);
    Some(pipe_id as usize)
}

fn pipe_dup_writer(pipe_id: u32) -> Option<usize> {
    if pipe_id == pipe::INVALID_PIPE_ID {
        return None;
    }
    let mut pipe_state = pipe::PIPE_STATE.lock();
    let slot = pipe::slot_mut(&mut pipe_state, pipe_id)?;
    slot.writers = slot.writers.saturating_add(1);
    Some(pipe_id as usize)
}

fn pipe_release_reader(pipe_id: u32) {
    if pipe_id == pipe::INVALID_PIPE_ID {
        return;
    }
    let mut wake_writers = false;
    {
        let mut pipe_state = pipe::PIPE_STATE.lock();
        if let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) {
            if slot.readers > 0 {
                slot.readers -= 1;
                if slot.readers == 0 {
                    wake_writers = true;
                }
            }
            if slot.readers == 0 && slot.writers == 0 {
                *slot = pipe::PipeSlot::new();
            }
        }
    }
    if wake_writers {
        pipe::writer_wq(pipe_id).wake_all();
    }
}

fn pipe_release_writer(pipe_id: u32) {
    if pipe_id == pipe::INVALID_PIPE_ID {
        return;
    }
    let mut wake_readers = false;
    {
        let mut pipe_state = pipe::PIPE_STATE.lock();
        if let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) {
            if slot.writers > 0 {
                slot.writers -= 1;
                if slot.writers == 0 {
                    wake_readers = true;
                }
            }
            if slot.readers == 0 && slot.writers == 0 {
                *slot = pipe::PipeSlot::new();
            }
        }
    }
    if wake_readers {
        pipe::reader_wq(pipe_id).wake_all();
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
        let pipe_id = handle as u32;
        let is_nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
        let mut local = [0u8; IO_STAGING_SIZE];
        let mut total = 0usize;
        let mut remaining = buf.len();

        loop {
            let mut need_block = false;
            let mut consumed = 0usize;
            let no_writers;
            {
                let mut pipe_state = pipe::PIPE_STATE.lock();
                let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) else {
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
                pipe::writer_wq(pipe_id).wake_one();
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
                pipe::reader_wq(pipe_id).enqueue_current();
                block_current_task();
                finish_wait();
                pipe::reader_wq(pipe_id).remove_current();
                continue;
            }
            return Errno::EAGAIN.as_isize();
        }
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EBADF.as_isize()
    }

    fn release(&self, handle: usize) {
        pipe_release_reader(handle as u32);
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        pipe_dup_reader(handle as u32)
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let pipe_id = handle as u32;
        let mut pipe_state = pipe::PIPE_STATE.lock();
        match pipe::slot_mut(&mut pipe_state, pipe_id) {
            Some(slot) => slot.revents(true, false, events),
            None => POLLERR,
        }
    }

    fn poll_wait(&self, handle: usize) -> bool {
        pipe::reader_wq(handle as u32).enqueue_current()
    }

    fn poll_unwait(&self, handle: usize) {
        pipe::reader_wq(handle as u32).remove_current();
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
        let pipe_id = handle as u32;
        let is_nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
        let buf_len = buf.len();
        let mut total = 0usize;
        let mut local = [0u8; IO_STAGING_SIZE];

        loop {
            let mut need_block = false;
            let can_write;
            {
                let mut pipe_state = pipe::PIPE_STATE.lock();
                let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) else {
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
                    pipe::writer_wq(pipe_id).enqueue_current();
                    block_current_task();
                    finish_wait();
                    pipe::writer_wq(pipe_id).remove_current();
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
                let mut pipe_state = pipe::PIPE_STATE.lock();
                let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) else {
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
                    pipe::reader_wq(pipe_id).wake_one();
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
                pipe::writer_wq(pipe_id).enqueue_current();
                block_current_task();
                finish_wait();
                pipe::writer_wq(pipe_id).remove_current();
                continue;
            }
            return Errno::EAGAIN.as_isize();
        }
    }

    fn release(&self, handle: usize) {
        pipe_release_writer(handle as u32);
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        pipe_dup_writer(handle as u32)
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let pipe_id = handle as u32;
        let mut pipe_state = pipe::PIPE_STATE.lock();
        match pipe::slot_mut(&mut pipe_state, pipe_id) {
            Some(slot) => slot.revents(false, true, events),
            None => POLLERR | POLLHUP,
        }
    }

    fn poll_wait(&self, handle: usize) -> bool {
        pipe::writer_wq(handle as u32).enqueue_current()
    }

    fn poll_unwait(&self, handle: usize) {
        pipe::writer_wq(handle as u32).remove_current();
    }
}
