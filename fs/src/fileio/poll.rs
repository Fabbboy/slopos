use super::*;

pub fn file_poll_register_pipes(process_id: u32, fds: &[(c_int, u16)]) -> usize {
    let mut registered = 0usize;
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return;
        };
        if !table.in_use {
            return;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        for &(fd, events) in fds {
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                continue;
            };
            if desc.pipe_id == pipe::INVALID_PIPE_ID {
                continue;
            }
            if desc.pipe_read_end && (events & POLLIN) != 0 {
                if pipe::reader_wq(desc.pipe_id).enqueue_current() {
                    registered += 1;
                }
            }
            if desc.pipe_write_end && (events & POLLOUT) != 0 {
                if pipe::writer_wq(desc.pipe_id).enqueue_current() {
                    registered += 1;
                }
            }
        }
        drop(guard);
    });
    registered
}

pub fn file_poll_unregister_pipes(process_id: u32, fds: &[(c_int, u16)]) {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return;
        };
        if !table.in_use {
            return;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        for &(fd, events) in fds {
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                continue;
            };
            if desc.pipe_id == pipe::INVALID_PIPE_ID {
                continue;
            }
            if desc.pipe_read_end && (events & POLLIN) != 0 {
                pipe::reader_wq(desc.pipe_id).remove_current();
            }
            if desc.pipe_write_end && (events & POLLOUT) != 0 {
                pipe::writer_wq(desc.pipe_id).remove_current();
            }
        }
        drop(guard);
    });
}

pub fn file_poll_register_fd(process_id: u32, fd: c_int, events: u16) -> PollRegInfo {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return PollRegInfo::NONE;
        };
        if !table.in_use {
            return PollRegInfo::NONE;
        }

        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return PollRegInfo::NONE;
        };

        if let Some(tty_idx) = desc.tty_index {
            drop(guard);
            let ok = tty::poll_enqueue(tty_idx);
            return PollRegInfo {
                kind: PollRegKind::Tty(tty_idx),
                registered: ok,
            };
        }

        if desc.socket_idx != INVALID_SOCKET_IDX {
            let sock_idx = desc.socket_idx;
            drop(guard);
            let ok = if (events & POLLIN) != 0 {
                socket::poll_enqueue_recv(sock_idx)
            } else {
                false
            };
            return PollRegInfo {
                kind: PollRegKind::Socket(sock_idx),
                registered: ok,
            };
        }

        drop(guard);
        PollRegInfo::NONE
    })
}

pub fn file_poll_unregister_fd(reg: &PollRegInfo) {
    if !reg.registered {
        return;
    }
    match reg.kind {
        PollRegKind::Tty(tty_idx) => {
            tty::poll_dequeue(tty_idx);
        }
        PollRegKind::Socket(sock_idx) => {
            socket::poll_dequeue_recv(sock_idx);
        }
        PollRegKind::None => {}
    }
}

pub fn file_poll_fd(process_id: u32, fd: c_int, events: u16) -> u16 {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return POLLNVAL;
        };
        if !table.in_use {
            return POLLNVAL;
        }

        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return POLLNVAL;
        };

        if desc.pipe_id != pipe::INVALID_PIPE_ID {
            let mut pipe_state = pipe::PIPE_STATE.lock();
            let revents = match pipe::slot_mut(&mut pipe_state, desc.pipe_id) {
                Some(slot) => slot.revents(desc.pipe_read_end, desc.pipe_write_end, events),
                None => POLLERR,
            };
            drop(guard);
            return revents;
        }

        if desc.socket_idx != INVALID_SOCKET_IDX {
            let socket_idx = desc.socket_idx;
            let readable = socket::poll_readable(socket_idx) as u16;
            let writable = socket::poll_writable(socket_idx) as u16;
            let mut revents = 0u16;
            if (events & POLLIN) != 0 {
                if (readable & 1) != 0 {
                    revents |= POLLIN;
                }
                revents |= readable & (POLLIN | POLLERR | POLLHUP);
            }
            if (events & POLLOUT) != 0 {
                if (writable & 1) != 0 {
                    revents |= POLLOUT;
                }
                revents |= writable & (POLLOUT | POLLERR | POLLHUP);
            }
            drop(guard);
            return revents;
        }

        if let Some(tty_idx) = desc.tty_index {
            let revents = tty::poll_events(tty_idx, events);
            drop(guard);
            return revents;
        }

        let mut revents = 0u16;
        if (events & POLLIN) != 0 {
            revents |= POLLIN;
        }
        if (events & POLLOUT) != 0 {
            revents |= POLLOUT;
        }
        drop(guard);
        revents
    })
}
