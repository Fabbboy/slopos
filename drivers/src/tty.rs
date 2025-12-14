
use core::ffi::c_int;
use core::ptr;

use slopos_lib::cpu;

use crate::keyboard;
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

static mut TTY_WAIT_QUEUE: TtyWaitQueue = TtyWaitQueue {
    tasks: [ptr::null_mut(); TTY_MAX_WAITERS],
    head: 0,
    tail: 0,
    count: 0,
};

unsafe extern "C" {
    fn scheduler_register_idle_wakeup_callback(cb: extern "C" fn() -> c_int);
    fn scheduler_is_enabled() -> c_int;
    fn task_is_blocked(task: *mut Task) -> c_int;
    fn unblock_task(task: *mut Task) -> c_int;

    fn serial_poll_receive(port: u16);
    fn serial_buffer_pending(port: u16) -> c_int;
    fn serial_buffer_read(port: u16, out: *mut u8) -> c_int;
}

#[inline]
fn tty_cpu_relax() {
    cpu::pause();
}

#[inline]
fn tty_service_serial_input() {
    unsafe { serial_poll_receive(COM1_BASE) };
}

fn tty_input_available() -> c_int {
    tty_service_serial_input();
    if keyboard::keyboard_has_input() != 0 {
        return 1;
    }
    if unsafe { serial_buffer_pending(COM1_BASE) } != 0 {
        return 1;
    }
    0
}

extern "C" fn tty_input_available_cb() -> c_int {
    tty_input_available()
}

fn tty_register_idle_callback() {
    static mut REGISTERED: bool = false;
    unsafe {
        if REGISTERED {
            return;
        }
        scheduler_register_idle_wakeup_callback(tty_input_available_cb);
        REGISTERED = true;
    }
}

fn tty_wait_queue_pop() -> *mut Task {
    unsafe {
        if TTY_WAIT_QUEUE.count == 0 {
            return ptr::null_mut();
        }
        let task = TTY_WAIT_QUEUE.tasks[TTY_WAIT_QUEUE.tail];
        TTY_WAIT_QUEUE.tail = (TTY_WAIT_QUEUE.tail + 1) % TTY_MAX_WAITERS;
        TTY_WAIT_QUEUE.count = TTY_WAIT_QUEUE.count.saturating_sub(1);
        task
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tty_notify_input_ready() {
    if unsafe { scheduler_is_enabled() } == 0 {
        return;
    }

    cpu::disable_interrupts();
    let mut tasko_wake: *mut Task = ptr::null_mut();

    unsafe {
        while TTY_WAIT_QUEUE.count > 0 {
            let candidate = tty_wait_queue_pop();
            if candidate.is_null() {
                continue;
            }
            if task_is_blocked(candidate) == 0 {
                continue;
            }
            tasko_wake = candidate;
            break;
        }
    }

    cpu::enable_interrupts();

    if !tasko_wake.is_null() {
        unsafe {
            let _ = unblock_task(tasko_wake);
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
    unsafe {
        if serial_buffer_read(COM1_BASE, &mut raw as *mut u8) != 0 {
            if raw == b'\r' {
                raw = b'\n';
            } else if raw == 0x7F {
                raw = b'\x08';
            }
            *out_char = raw;
            return true;
        }
    }
    false
}

fn tty_block_until_input_ready() {
    loop {
        if tty_input_available() != 0 {
            break;
        }
        tty_service_serial_input();
        if unsafe { scheduler_is_enabled() } != 0 {
            unsafe { yield_() };
        } else {
            tty_cpu_relax();
        }
    }
}

#[inline]
fn serial_putc(port: u16, c: u8) {
    let s = [c];
    let text = core::str::from_utf8(&s).unwrap_or("");
    serial::write_str(text);
    let _ = port; // keep signature parity
}

unsafe extern "C" {
    fn yield_();
}

#[unsafe(no_mangle)]
pub extern "C" fn tty_read_line(buffer: *mut u8, buffer_size: usize) -> usize {
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

        let port = COM1_BASE;

        if c == b'\n' || c == b'\r' {
            unsafe {
                *buffer.add(pos) = 0;
            }
            serial_putc(port, b'\n');
            return pos;
        }

        if c == b'\x08' {
            if pos > 0 {
                pos -= 1;
                serial_putc(port, b'\x08');
                serial_putc(port, b' ');
                serial_putc(port, b'\x08');
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
            serial_putc(port, c);
            continue;
        }

        if is_control_char(c) {
            continue;
        }

        unsafe {
            *buffer.add(pos) = c;
        }
        pos += 1;
        serial_putc(port, c);
    }
}
