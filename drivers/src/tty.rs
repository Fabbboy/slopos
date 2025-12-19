use core::ffi::c_int;
use core::ptr;

use spin::Mutex;
use slopos_lib::cpu;

use crate::keyboard;
use crate::scheduler_callbacks::{
    call_get_current_task, call_register_idle_wakeup_callback, call_scheduler_is_enabled,
    call_task_is_blocked, call_unblock_task, call_yield,
};
use crate::serial;
use crate::syscall_types::Task;

const TTY_MAX_WAITERS: usize = 32;
const COM1_BASE: u16 = 0x3F8;

#[repr(C)]
struct TtyWaitQueue {
    tasks: [*mut Task; TTY_MAX_WAITERS],
    head: usize,
    tail: usize,
    count: usize,
}

// SAFETY: The wait queue only stores task pointers managed by the scheduler,
// and access is synchronized through the TTY_WAIT_QUEUE mutex.
unsafe impl Send for TtyWaitQueue {}

static TTY_WAIT_QUEUE: Mutex<TtyWaitQueue> = Mutex::new(TtyWaitQueue {
    tasks: [ptr::null_mut(); TTY_MAX_WAITERS],
    head: 0,
    tail: 0,
    count: 0,
});

use crate::serial::{serial_buffer_pending, serial_buffer_read, serial_poll_receive};

#[inline]
fn tty_cpu_relax() {
    cpu::pause();
}

#[inline]
fn tty_service_serial_input() {
    serial_poll_receive(COM1_BASE);
}

fn tty_input_available() -> c_int {
    tty_service_serial_input();
    if keyboard::keyboard_has_input() != 0 {
        return 1;
    }
    if serial_buffer_pending(COM1_BASE) != 0 {
        return 1;
    }
    0
}

fn tty_input_available_cb() -> c_int {
    tty_input_available()
}

fn tty_register_idle_callback() {
    static mut REGISTERED: bool = false;
    unsafe {
        if REGISTERED {
            return;
        }
        call_register_idle_wakeup_callback(Some(tty_input_available_cb));
        REGISTERED = true;
    }
}

fn tty_wait_queue_push(task: *mut Task) -> bool {
    if task.is_null() {
        return false;
    }
    let mut queue = TTY_WAIT_QUEUE.lock();
    if queue.count >= TTY_MAX_WAITERS {
        return false;
    }
    let head = queue.head;
    queue.tasks[head] = task;
    queue.head = (head + 1) % TTY_MAX_WAITERS;
    queue.count = queue.count.saturating_add(1);
    true
}

fn tty_wait_queue_pop() -> *mut Task {
    let mut queue = TTY_WAIT_QUEUE.lock();
    if queue.count == 0 {
        return ptr::null_mut();
    }
    let task = queue.tasks[queue.tail];
    queue.tail = (queue.tail + 1) % TTY_MAX_WAITERS;
    queue.count = queue.count.saturating_sub(1);
    task
}
pub fn tty_notify_input_ready() {
    if unsafe { call_scheduler_is_enabled() } == 0 {
        return;
    }

    cpu::disable_interrupts();
    let mut tasko_wake: *mut Task = ptr::null_mut();

    loop {
        let candidate = tty_wait_queue_pop();
        if candidate.is_null() {
            break;
        }
        if unsafe {
            !call_task_is_blocked(candidate as *const Task as *const core::ffi::c_void)
        } {
            continue;
        }
        tasko_wake = candidate;
        break;
    }

    cpu::enable_interrupts();

    if !tasko_wake.is_null() {
        unsafe {
            let _ = call_unblock_task(tasko_wake as *mut core::ffi::c_void);
        }
    }
}

#[inline]
fn is_printable(c: u8) -> bool {
    (c >= 0x20 && c <= 0x7E) || c == b'\t'
}

#[inline]
fn is_control_char(c: u8) -> bool {
    (c <= 0x1F) || c == 0x7F
}

fn tty_dequeue_input_char(out_char: &mut u8) -> bool {
    tty_service_serial_input();

    if keyboard::keyboard_has_input() != 0 {
        *out_char = keyboard::keyboard_getchar();
        return true;
    }

    tty_service_serial_input();

    let mut raw = 0u8;
    if serial_buffer_read(COM1_BASE, &mut raw as *mut u8) == 0 {
        if raw == b'\r' {
            raw = b'\n';
        } else if raw == 0x7F {
            raw = b'\x08';
        }
        *out_char = raw;
        return true;
    }
    false
}

fn tty_block_until_input_ready() {
    loop {
        if tty_input_available() != 0 {
            break;
        }
        tty_service_serial_input();
        if unsafe { call_scheduler_is_enabled() } != 0 {
            let task = unsafe { call_get_current_task() } as *mut Task;
            if !task.is_null() {
                let _ = tty_wait_queue_push(task);
            }
            unsafe { call_yield() };
        } else {
            tty_cpu_relax();
        }
    }
}

#[inline]
fn serial_putc(c: u8) {
    serial::serial_putc_com1(c);
}
pub fn tty_read_line(buffer: *mut u8, buffer_size: usize) -> usize {
    if buffer.is_null() || buffer_size == 0 {
        return 0;
    }

    tty_register_idle_callback();

    if buffer_size < 2 {
        unsafe { *buffer = 0 };
        return 0;
    }

    let mut pos = 0usize;
    let max_pos = buffer_size - 1;

    loop {
        let mut c = 0u8;
        if !tty_dequeue_input_char(&mut c) {
            tty_block_until_input_ready();
            continue;
        }

        if c == b'\n' || c == b'\r' {
            unsafe {
                *buffer.add(pos) = 0;
            }
            serial_putc(b'\n');
            return pos;
        }

        if c == b'\x08' {
            if pos > 0 {
                pos -= 1;
                serial_putc(b'\x08');
                serial_putc(b' ');
                serial_putc(b'\x08');
            }
            continue;
        }

        if pos >= max_pos {
            continue;
        }

        if is_printable(c) {
            unsafe {
                *buffer.add(pos) = c;
            }
            pos += 1;
            serial_putc(c);
            continue;
        }

        if is_control_char(c) {
            continue;
        }

        unsafe {
            *buffer.add(pos) = c;
        }
        pos += 1;
        serial_putc(c);
    }
}
