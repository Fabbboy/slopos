use slopos_abi::Errno;
use slopos_abi::event::{KernelEvent, PipeSlot};
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{POLLERR, POLLHUP};
use slopos_kernel_services::driver_runtime::scheduler_is_enabled;
use slopos_ostd::KArc;
use slopos_ostd::process::quota::{AliasOf, FileBacking};
use slopos_ostd::sync::BUS;

use crate::pipe;
use crate::pipe::PipeHandle;

pub struct PipeReadOps;
pub struct PipeWriteOps;

pub static PIPE_READ_OPS: PipeReadOps = PipeReadOps;
pub static PIPE_WRITE_OPS: PipeWriteOps = PipeWriteOps;

/// The read-side event for a pipe handle (data became available to read).
#[inline]
fn read_ev(h: PipeHandle) -> KernelEvent {
    KernelEvent::PipeRead {
        pipe: PipeSlot(h.slot() as u32),
    }
}

/// The write-side event for a pipe handle (buffer space became available).
#[inline]
fn write_ev(h: PipeHandle) -> KernelEvent {
    KernelEvent::PipeWrite {
        pipe: PipeSlot(h.slot() as u32),
    }
}

/// Owner of the pipe's read end: dropping it retires one reader, waking
/// blocked writers on the last-reader edge and freeing the pipe slot once
/// both ends are gone.
#[derive(slopos_ostd::Charged)]
pub(crate) struct PipeReadBacking {
    handle: PipeHandle,
    object_charge: AliasOf,
}

slopos_ostd::charge_audit!(PipeReadBacking);

impl FileBacking for PipeReadBacking {}

impl Drop for PipeReadBacking {
    fn drop(&mut self) {
        pipe_release_reader(self.handle);
    }
}

/// Owner of the pipe's write end — the reader-side EOF edge lives in its
/// `Drop`.
#[derive(slopos_ostd::Charged)]
pub(crate) struct PipeWriteBacking {
    handle: PipeHandle,
    object_charge: AliasOf,
}

slopos_ostd::charge_audit!(PipeWriteBacking);

impl FileBacking for PipeWriteBacking {}

impl Drop for PipeWriteBacking {
    fn drop(&mut self) {
        pipe_release_writer(self.handle);
    }
}

/// Wrap ownership of both ends of a freshly-allocated, primed pipe
/// (readers == writers == 1). Consumes both primed references: on
/// allocation failure they are retired here, freeing the pipe slot —
/// the caller must not free it itself.
pub(crate) fn pipe_backings(
    handle: PipeHandle,
) -> Option<(KArc<dyn FileBacking>, KArc<dyn FileBacking>)> {
    let read: KArc<dyn FileBacking> = match KArc::try_new(PipeReadBacking {
        handle,
        object_charge: AliasOf {
            owner: "the pipe registry row",
        },
    }) {
        Ok(backing) => backing,
        Err(_) => {
            pipe_release_reader(handle);
            pipe_release_writer(handle);
            return None;
        }
    };
    let write: KArc<dyn FileBacking> = match KArc::try_new(PipeWriteBacking {
        handle,
        object_charge: AliasOf {
            owner: "the pipe registry row",
        },
    }) {
        Ok(backing) => backing,
        Err(_) => {
            drop(read);
            pipe_release_writer(handle);
            return None;
        }
    };
    Some((read, write))
}

fn pipe_release_reader(h: PipeHandle) {
    if h == PipeHandle::INVALID {
        return;
    }
    let mut wake_writers = false;
    let should_free = pipe::with_pipe_mut(h, |slot| {
        if slot.readers > 0 {
            slot.readers -= 1;
            if slot.readers == 0 {
                wake_writers = true;
            }
        }
        slot.readers == 0 && slot.writers == 0
    })
    .unwrap_or(false);
    if should_free {
        pipe::free_slot(h);
    }
    if wake_writers {
        BUS.publish(write_ev(h));
    }
}

fn pipe_release_writer(h: PipeHandle) {
    if h == PipeHandle::INVALID {
        return;
    }
    let mut wake_readers = false;
    let should_free = pipe::with_pipe_mut(h, |slot| {
        if slot.writers > 0 {
            slot.writers -= 1;
            if slot.writers == 0 {
                wake_readers = true;
            }
        }
        slot.readers == 0 && slot.writers == 0
    })
    .unwrap_or(false);
    if should_free {
        pipe::free_slot(h);
    }
    if wake_readers {
        BUS.publish(read_ev(h));
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
        // Sized to the request, capped at the staging bound: a shell reading a
        // script one byte at a time must not cost a 4 KiB kernel allocation per
        // byte. Larger requests still iterate the loop below at IO_STAGING_SIZE.
        let mut local = match slopos_ostd::KVec::<u8>::zeroed(buf.len().min(IO_STAGING_SIZE)) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
        let mut total = 0usize;
        let mut remaining = buf.len();

        loop {
            // Snapshot under the table lock: try to consume data and
            // observe writer count.
            let (consumed, no_writers, slot_gone) = pipe::with_pipe_mut(h, |slot| {
                let consumed = if remaining > 0 && slot.len > 0 {
                    let chunk = remaining.min(local.len());
                    slot.read_into(&mut local[..chunk])
                } else {
                    0
                };
                (consumed, slot.writers == 0)
            })
            .map_or((0, true, true), |(consumed, no_writers)| {
                (consumed, no_writers, false)
            });

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
                BUS.publish_one(write_ev(h));
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
            if BUS
                .subscribe(read_ev(h))
                .wait_event(|| {
                    // Slot evaporated under us → fall out of the wait so the
                    // next iteration's lookup reports EBADF.
                    pipe::with_pipe(h, |slot| slot.len > 0 || slot.writers == 0).unwrap_or(true)
                })
                .is_err()
            {
                // Nothing has been transferred — the short-count return above
                // already took that case.
                return Errno::EINTR.as_isize();
            }
        }
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EBADF.as_isize()
    }

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        let h = PipeHandle::from_usize(handle);
        // Register FIRST, then check readiness (Linux pattern).
        let registered = BUS.subscribe_current(read_ev(h));
        let revents =
            pipe::with_pipe(h, |slot| slot.revents(true, false, events)).unwrap_or(POLLERR);
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_token: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let h = PipeHandle::from_usize(handle);
        pipe::with_pipe(h, |slot| slot.revents(true, false, events)).unwrap_or(POLLERR)
    }

    fn poll_wait(&self, handle: usize) -> bool {
        BUS.subscribe_current(read_ev(PipeHandle::from_usize(handle)))
    }

    fn poll_unwait(&self, handle: usize) {
        BUS.unsubscribe_current(read_ev(PipeHandle::from_usize(handle)));
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
        let drain_or_close = || {
            pipe::with_pipe(h, |slot| {
                slot.len < pipe::PIPE_BUFFER_SIZE || slot.readers == 0
            })
            .unwrap_or(true)
        };

        loop {
            // Snapshot under the table lock.
            let (can_write, no_readers, slot_gone) = pipe::with_pipe(h, |slot| {
                (slot.len < pipe::PIPE_BUFFER_SIZE, slot.readers == 0)
            })
            .map_or((false, true, true), |(can_write, no_readers)| {
                (can_write, no_readers, false)
            });

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
                if BUS
                    .subscribe(write_ev(h))
                    .wait_event(drain_or_close)
                    .is_err()
                {
                    return if total > 0 {
                        total as isize
                    } else {
                        Errno::EINTR.as_isize()
                    };
                }
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

            // Push the staged bytes under the slot lock, *release the
            // slot lock*, and only then call `wake_one()` on the reader
            // wait-queue.
            //
            // The order matters: `wake_one()` acquires the wait-queue's
            // internal SpinLock, and `PipeReadOps::read`'s waiter goes
            // through `wait_event` whose closure (re-)acquires this
            // same pipe slot lock under the wait-queue's SpinLock.
            // If we called `wake_one()` while still holding the slot
            // lock here, the two paths would form a classical AB-BA
            // pair (PS → WQ here, WQ → PS in the waiter), and on TCG /
            // any sufficiently spread-out interleaving two CPUs would
            // ticket-lock each other into a permanent freeze with
            // interrupts disabled.
            //
            // Note: the kernel relies on the WaitQueue protocol calling
            // `condition()` outside its internal SpinLock (see
            // `slopos_ostd::sync::wait_queue::WaitQueue::wait_event`),
            // so even if a future change were to add another
            // wake-under-data-lock site, the AB-BA would no longer be
            // expressible. This release-before-wake here is defence
            // in depth: producers must not retain the data lock across
            // a wake call.
            enum PushOutcome {
                Wrote {
                    written: usize,
                    no_readers_after: bool,
                },
                NoReaders,
                Gone,
            }
            let outcome = pipe::with_pipe_mut(h, |slot| {
                if slot.readers == 0 {
                    return PushOutcome::NoReaders;
                }
                let written = slot.write_from(&local[..staged]);
                PushOutcome::Wrote {
                    written,
                    no_readers_after: slot.readers == 0,
                }
            })
            .unwrap_or(PushOutcome::Gone);

            let (written, no_readers_after) = match outcome {
                PushOutcome::Wrote {
                    written,
                    no_readers_after,
                } => {
                    total += written;
                    (written, no_readers_after)
                }
                PushOutcome::NoReaders => {
                    return if total > 0 {
                        total as isize
                    } else {
                        Errno::EPIPE.as_isize()
                    };
                }
                PushOutcome::Gone => {
                    return if total > 0 {
                        total as isize
                    } else {
                        Errno::EBADF.as_isize()
                    };
                }
            };
            if written > 0 && !no_readers_after {
                BUS.publish_one(read_ev(h));
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
            if BUS
                .subscribe(write_ev(h))
                .wait_event(drain_or_close)
                .is_err()
            {
                return if total > 0 {
                    total as isize
                } else {
                    Errno::EINTR.as_isize()
                };
            }
        }
    }

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        let h = PipeHandle::from_usize(handle);
        // Register FIRST, then check readiness (Linux pattern).
        let registered = BUS.subscribe_current(write_ev(h));
        let revents = pipe::with_pipe(h, |slot| slot.revents(false, true, events))
            .unwrap_or(POLLERR | POLLHUP);
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_token: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let h = PipeHandle::from_usize(handle);
        pipe::with_pipe(h, |slot| slot.revents(false, true, events)).unwrap_or(POLLERR | POLLHUP)
    }

    fn poll_wait(&self, handle: usize) -> bool {
        BUS.subscribe_current(write_ev(PipeHandle::from_usize(handle)))
    }

    fn poll_unwait(&self, handle: usize) {
        BUS.unsubscribe_current(write_ev(PipeHandle::from_usize(handle)));
    }
}
