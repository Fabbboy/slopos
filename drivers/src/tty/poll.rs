//! TTY poll readiness and compositor focus management.

use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT};

use slopos_kernel_services::driver_runtime::scheduler_is_enabled;

use slopos_ostd::sync::BUS;

use super::table::{TTY_SLOTS, tty_input_event, tty_output_event};
use super::{MAX_TTYS, PostLockWork, TtyError, TtyFlags, TtyIndex};

/// Waiters park on both queues so a readiness change in either direction wakes
/// them.
fn poll_register_slot(slot: usize) -> bool {
    let input = BUS.subscribe_current(tty_input_event(slot));
    let output = BUS.subscribe_current(tty_output_event(slot));
    input || output
}

fn poll_unregister_slot(slot: usize) {
    BUS.unsubscribe_current(tty_input_event(slot));
    BUS.unsubscribe_current(tty_output_event(slot));
}

/// Sets `focused_task_id` only, which routes input; the POSIX foreground process
/// group that gates terminal access and job-control signals is independent.
#[must_use]
pub fn set_compositor_focus(task_id: u32) -> Result<(), TtyError> {
    let idx = super::active_tty();
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let mut found = false;
    {
        let mut guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_mut() {
            tty.session.focused_task_id = task_id;
            found = true;
        }
    }
    if !found {
        return Err(TtyError::NotAllocated);
    }
    if scheduler_is_enabled() != 0 {
        BUS.publish(tty_input_event(slot));
    }
    Ok(())
}

#[must_use]
pub fn get_compositor_focus() -> Result<u32, TtyError> {
    let idx = super::active_tty();
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => Ok(tty.session.focused_task_id),
        None => Err(TtyError::NotAllocated),
    }
}

/// Poll readiness for a TTY fd, after draining pending hardware input. `POLLERR`
/// accompanies `POLLHUP` as in Linux's `tty_poll()`, so a program testing
/// write-readiness through `POLLERR` still sees the error.
pub fn poll_events(idx: TtyIndex, requested: u16) -> u16 {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return 0;
    }

    let mut deferred = PostLockWork::new();
    let revents = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(t) => t,
            None => return 0,
        };

        tty.drain_hw_input_locked(&mut deferred);

        let mut revents = 0u16;

        if (requested & POLLIN) != 0
            && (tty.ldisc.has_data()
                || (tty.flags.contains(TtyFlags::PACKET_MODE) && !tty.packet_events.is_empty()))
        {
            revents |= POLLIN;
        }

        if (requested & POLLOUT) != 0
            && !tty.ldisc.is_stopped()
            && !tty.flags.contains(TtyFlags::OUTPUT_STOPPED)
        {
            revents |= POLLOUT;
        }

        if tty.flags.contains(TtyFlags::HUNG_UP)
            || (tty.flags.contains(TtyFlags::PEER_CLOSED) && !tty.ldisc.has_data())
        {
            revents |= POLLHUP | POLLERR;
            if (requested & POLLIN) != 0 {
                revents |= POLLIN;
            }
        }

        revents
    };

    deferred.execute();
    revents
}

pub fn poll_enqueue(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }
    if scheduler_is_enabled() == 0 {
        return false;
    }
    poll_register_slot(slot)
}

pub fn poll_dequeue(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    poll_unregister_slot(slot);
}

/// Enqueue the current task on every named slot's input and output queues, then
/// block once; any one of them waking resumes it. Falls back to a 1 ms timer
/// delay when `slots` is empty or the scheduler is not yet running.
pub fn poll_sleep_on(slots: &[u8]) {
    if scheduler_is_enabled() == 0 {
        slopos_kernel_services::platform::timer_poll_delay_ms(1);
        return;
    }

    if slots.is_empty() {
        slopos_kernel_services::platform::timer_poll_delay_ms(1);
        return;
    }

    // TODO(tech-debt): a wake landing between enqueue and the block CAS is lost, so
    // this waits out the full 100 ms — fix is one `wait_event_timeout` queue, not N.
    let mut registered = 0usize;
    for &slot in slots {
        let s = slot as usize;
        if s < MAX_TTYS && poll_register_slot(s) {
            registered += 1;
        }
    }

    if registered == 0 {
        slopos_kernel_services::platform::timer_poll_delay_ms(1);
        return;
    }

    slopos_kernel_services::driver_runtime::block_current_task_with_timeout(100);

    for &slot in slots {
        let s = slot as usize;
        if s < MAX_TTYS {
            poll_unregister_slot(s);
        }
    }
}

/// Slot-less form: sleeps on every active TTY poll waiter.
pub fn poll_sleep() {
    let mut slots = [0u8; MAX_TTYS];
    let mut count = 0;
    let mut bits = super::table::active_slots_bitmap();
    while bits != 0 {
        let i = bits.trailing_zeros() as usize;
        slots[count] = i as u8;
        count += 1;
        bits &= bits - 1;
    }
    poll_sleep_on(&slots[..count]);
}
