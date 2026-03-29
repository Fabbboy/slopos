use core::cmp;
use core::ffi::c_void;
use core::ptr;

use crate::runtime;
use crate::syscall::core as sys_core;
use crate::syscall::{InputEvent, InputEventType, UserPollFd, fs, input};
use slopos_abi::syscall::{LocalFlags, POLLIN};
use std::time::{Duration, Instant};

use super::buffers;
use super::completion;
use slopos_abi::input::POINTER_AXIS_VERTICAL;

use super::display::{
    DISPLAY, shell_console_clear, shell_console_follow_bottom, shell_console_page_down,
    shell_console_page_up, shell_console_scroll_lines, shell_redraw_input, shell_write,
};
use super::history;
use super::parser::{SHELL_MAX_TOKENS, shell_parse_line};

const KEY_PAGE_UP: u8 = 0x80;
const KEY_PAGE_DOWN: u8 = 0x81;
const KEY_UP: u8 = 0x82;
const KEY_DOWN: u8 = 0x83;
const KEY_LEFT: u8 = 0x84;
const KEY_RIGHT: u8 = 0x85;
const KEY_HOME: u8 = 0x86;
const KEY_END: u8 = 0x87;
const KEY_DELETE: u8 = 0x88;

const KEY_SHIFT_LEFT: u8 = 0x94;
const KEY_SHIFT_RIGHT: u8 = 0x95;
const KEY_SHIFT_HOME: u8 = 0x96;
const KEY_SHIFT_END: u8 = 0x97;

const CTRL_A: u8 = 0x01;
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;
const CTRL_E: u8 = 0x05;
const CTRL_K: u8 = 0x0B;
const CTRL_L: u8 = 0x0C;
const CTRL_U: u8 = 0x15;
const CTRL_V: u8 = 0x16;
const CTRL_W: u8 = 0x17;

const MOUSE_LEFT: u8 = 0x01;
const MOUSE_EVENT_BUF_SIZE: usize = 8;

static PROMPT_COLORS: super::SyncUnsafeCell<[u8; super::PROMPT_BUF_MAX]> =
    super::SyncUnsafeCell::new([0; super::PROMPT_BUF_MAX]);
static PROMPT_COLORS_LEN: super::SyncUnsafeCell<usize> = super::SyncUnsafeCell::new(0);
static SCROLL_ACCUM: super::SyncUnsafeCell<i32> = super::SyncUnsafeCell::new(0);

pub fn read_command_line(
    tokens: &mut [*const u8; SHELL_MAX_TOKENS],
    prompt: &[u8],
    prompt_colors: &[u8],
) -> i32 {
    unsafe {
        let colors = &mut *PROMPT_COLORS.get();
        let copy_len = prompt_colors.len().min(super::PROMPT_BUF_MAX);
        colors[..copy_len].copy_from_slice(&prompt_colors[..copy_len]);
        *PROMPT_COLORS_LEN.get() = copy_len;
    }
    buffers::with_line_buf(|buf| {
        runtime::u_memset(buf.as_mut_ptr() as *mut c_void, 0, buf.len());
    });

    // Set TTY to raw mode: shell handles its own rendering / line editing.
    // This mirrors what bash/zsh do on real Linux — cfmakeraw() equivalent.
    let saved_termios = fs::tcgetattr(0).ok();
    if let Some(ref t) = saved_termios {
        let mut raw = *t;
        raw.c_lflag &=
            !(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG | LocalFlags::ECHOE);
        let _ = fs::tcsetattr(0, &raw);
    }

    let result = input_loop(tokens, prompt, 0, 0);

    // Restore canonical mode so child processes (nc, etc.) get line-buffered input.
    if let Some(ref t) = saved_termios {
        let _ = fs::tcsetattr(0, t);
    }

    result
}

fn prompt_colors_slice() -> &'static [u8] {
    unsafe {
        let len = *PROMPT_COLORS_LEN.get();
        let colors: &[u8; super::PROMPT_BUF_MAX] = &*PROMPT_COLORS.get();
        &colors[..len]
    }
}

fn input_loop(
    tokens: &mut [*const u8; SHELL_MAX_TOKENS],
    prompt: &[u8],
    mut len: usize,
    mut cursor_pos: usize,
) -> i32 {
    use super::display::InputSelection;

    let mut line_row = super::display::shell_console_get_cursor().1;

    const BLINK_INTERVAL: Duration = Duration::from_millis(500);
    let mut cursor_visible = true;
    let mut last_blink = Instant::now();

    let mut sel = InputSelection::NONE;
    let mut mouse_dragging = false;
    let mut prev_left_pressed = false;
    let mut has_pointer_focus = false;
    let mut last_ptr_x: i32 = 0;
    let mut last_ptr_y: i32 = 0;
    let mut button_state: u8 = 0;

    macro_rules! rd {
        () => {
            redraw(line_row, prompt, len, cursor_pos, cursor_visible, &sel)
        };
    }

    rd!();

    loop {
        line_row = super::display::shell_console_get_cursor().1;

        let mut events = [InputEvent::default(); MOUSE_EVENT_BUF_SIZE];
        let count = input::poll_batch(&mut events) as usize;
        let mut key_event: Option<u8> = None;
        for i in 0..count.min(MOUSE_EVENT_BUF_SIZE) {
            match events[i].event_type {
                InputEventType::KeyPress => {
                    let ascii = events[i].key_ascii();
                    if ascii != 0 && key_event.is_none() {
                        key_event = Some(ascii);
                    }
                }
                InputEventType::PointerMotion | InputEventType::PointerEnter => {
                    last_ptr_x = events[i].pointer_x();
                    last_ptr_y = events[i].pointer_y();
                    has_pointer_focus = true;
                }
                InputEventType::PointerLeave => {
                    has_pointer_focus = false;
                }
                InputEventType::PointerButtonPress => {
                    button_state |= events[i].pointer_button_code();
                }
                InputEventType::PointerButtonRelease => {
                    button_state &= !events[i].pointer_button_code();
                }
                InputEventType::Configure => {
                    let new_w = events[i].configure_width() as i32;
                    let new_h = events[i].configure_height() as i32;
                    super::display::shell_console_resize(new_w, new_h);
                }
                InputEventType::PointerAxis => {
                    if events[i].axis_id() == POINTER_AXIS_VERTICAL {
                        let accum = unsafe { &mut *SCROLL_ACCUM.get() };
                        *accum += events[i].axis_value_v120();
                        let lines = *accum / 120;
                        *accum %= 120;
                        if lines != 0 {
                            // Negative value = scroll up (show history),
                            // positive = scroll down (show newer).
                            shell_console_scroll_lines(lines);
                        }
                    }
                }
                _ => {}
            }
        }

        let mut mouse_acted = false;
        let left_pressed = has_pointer_focus && (button_state & MOUSE_LEFT) != 0;
        let newly_pressed = left_pressed && !prev_left_pressed;
        let newly_released = !left_pressed && prev_left_pressed;

        if newly_pressed {
            if is_on_input_row(last_ptr_y, line_row) {
                if let Some(off) = pixel_to_input_offset(last_ptr_x, prompt.len(), len) {
                    sel = InputSelection {
                        start: off,
                        end: off,
                    };
                    mouse_dragging = true;
                    cursor_visible = true;
                    last_blink = Instant::now();
                    mouse_acted = true;
                }
            } else {
                mouse_dragging = false;
                if sel.is_active() {
                    sel = InputSelection::NONE;
                    mouse_acted = true;
                }
            }
        } else if mouse_dragging && left_pressed {
            if is_on_input_row(last_ptr_y, line_row) {
                if let Some(off) = pixel_to_input_offset(last_ptr_x, prompt.len(), len) {
                    if off != sel.end {
                        sel.end = off;
                        mouse_acted = true;
                    }
                }
            }
        }

        if newly_released && mouse_dragging {
            mouse_dragging = false;
            if !sel.is_active() {
                sel = InputSelection::NONE;
            }
            mouse_acted = true;
        }
        prev_left_pressed = left_pressed;
        if mouse_acted {
            rd!();
        }

        let rc = if DISPLAY.enabled.get() {
            match key_event {
                Some(c) => c as i64,
                None => -1,
            }
        } else {
            let mut pfds = [UserPollFd {
                fd: 0,
                events: POLLIN,
                revents: 0,
            }];
            let _ = fs::poll(&mut pfds, 0);
            if (pfds[0].revents & POLLIN) != 0 {
                let mut ch = [0u8; 1];
                match fs::read_slice(0, &mut ch) {
                    Ok(1) => ch[0] as i64,
                    _ => -1,
                }
            } else {
                -1i64
            }
        };
        if rc < 0 {
            let now = Instant::now();
            if now.duration_since(last_blink) >= BLINK_INTERVAL {
                cursor_visible = !cursor_visible;
                last_blink = now;
                rd!();
            }
            if !mouse_acted {
                sys_core::yield_now();
            }
            continue;
        }
        let c = rc as u8;

        cursor_visible = true;
        last_blink = Instant::now();

        if c == KEY_PAGE_UP {
            shell_console_page_up();
            continue;
        }
        if c == KEY_PAGE_DOWN {
            shell_console_page_down();
            continue;
        }

        if DISPLAY.enabled.get() && !DISPLAY.follow.get() {
            shell_console_follow_bottom();
        }

        let preserves_selection = core::matches!(
            c,
            KEY_SHIFT_LEFT
                | KEY_SHIFT_RIGHT
                | KEY_SHIFT_HOME
                | KEY_SHIFT_END
                | CTRL_C
                | CTRL_V
                | KEY_PAGE_UP
                | KEY_PAGE_DOWN
        );
        if !preserves_selection && sel.is_active() {
            sel = InputSelection::NONE;
            mouse_dragging = false;
        }

        match c {
            b'\n' | b'\r' => {
                sel = InputSelection::NONE;
                redraw(line_row, prompt, len, cursor_pos, true, &sel);
                super::display::shell_echo_char(b'\n');
                buffers::with_line_buf(|buf| {
                    history::push(buf, len);
                });
                history::reset_cursor();
                break;
            }

            b'\x08' | 0x7f => {
                if sel.is_active() {
                    delete_selection(&mut sel, &mut len, &mut cursor_pos);
                    rd!();
                } else if cursor_pos > 0 {
                    delete_char_before_cursor(&mut len, &mut cursor_pos);
                    rd!();
                }
            }

            KEY_DELETE => {
                if sel.is_active() {
                    delete_selection(&mut sel, &mut len, &mut cursor_pos);
                    rd!();
                } else if cursor_pos < len {
                    delete_char_at_cursor(&mut len, cursor_pos);
                    rd!();
                }
            }

            KEY_UP => {
                let mut snapshot = [0u8; 256];
                buffers::with_line_buf(|buf| {
                    snapshot[..len].copy_from_slice(&buf[..len]);
                });
                let new_len =
                    buffers::with_line_buf(|buf| history::navigate_up(&snapshot[..len], len, buf));
                if let Some(nl) = new_len {
                    len = nl;
                    cursor_pos = nl;
                    rd!();
                }
            }

            KEY_DOWN => {
                let new_len = buffers::with_line_buf(|buf| history::navigate_down(buf));
                if let Some(nl) = new_len {
                    len = nl;
                    cursor_pos = nl;
                    rd!();
                }
            }

            KEY_LEFT => {
                if cursor_pos > 0 {
                    cursor_pos -= 1;
                    rd!();
                }
            }

            KEY_RIGHT => {
                if cursor_pos < len {
                    cursor_pos += 1;
                    rd!();
                }
            }

            KEY_SHIFT_LEFT => {
                if cursor_pos > 0 {
                    if !sel.is_active() {
                        sel.start = cursor_pos;
                    }
                    cursor_pos -= 1;
                    sel.end = cursor_pos;
                    rd!();
                }
            }

            KEY_SHIFT_RIGHT => {
                if cursor_pos < len {
                    if !sel.is_active() {
                        sel.start = cursor_pos;
                    }
                    cursor_pos += 1;
                    sel.end = cursor_pos;
                    rd!();
                }
            }

            KEY_SHIFT_HOME => {
                if cursor_pos != 0 {
                    if !sel.is_active() {
                        sel.start = cursor_pos;
                    }
                    cursor_pos = 0;
                    sel.end = 0;
                    rd!();
                }
            }

            KEY_SHIFT_END => {
                if cursor_pos != len {
                    if !sel.is_active() {
                        sel.start = cursor_pos;
                    }
                    cursor_pos = len;
                    sel.end = len;
                    rd!();
                }
            }

            KEY_HOME | CTRL_A => {
                if cursor_pos != 0 {
                    cursor_pos = 0;
                    rd!();
                }
            }

            KEY_END | CTRL_E => {
                if cursor_pos != len {
                    cursor_pos = len;
                    rd!();
                }
            }

            CTRL_K => {
                if cursor_pos < len {
                    buffers::with_line_buf(|buf| {
                        for i in cursor_pos..len {
                            buf[i] = 0;
                        }
                    });
                    len = cursor_pos;
                    rd!();
                }
            }

            CTRL_U => {
                if cursor_pos > 0 {
                    let shift = len - cursor_pos;
                    buffers::with_line_buf(|buf| {
                        for i in 0..shift {
                            buf[i] = buf[cursor_pos + i];
                        }
                        for i in shift..len {
                            buf[i] = 0;
                        }
                    });
                    len = shift;
                    cursor_pos = 0;
                    rd!();
                }
            }

            CTRL_W => {
                if cursor_pos > 0 {
                    let old_cursor = cursor_pos;
                    let mut new_cursor = cursor_pos;
                    buffers::with_line_buf(|buf| {
                        while new_cursor > 0 && buf[new_cursor - 1] == b' ' {
                            new_cursor -= 1;
                        }
                        while new_cursor > 0 && buf[new_cursor - 1] != b' ' {
                            new_cursor -= 1;
                        }
                        let tail = len - old_cursor;
                        for i in 0..tail {
                            buf[new_cursor + i] = buf[old_cursor + i];
                        }
                        for i in new_cursor + tail..len {
                            buf[i] = 0;
                        }
                    });
                    len -= old_cursor - new_cursor;
                    cursor_pos = new_cursor;
                    rd!();
                }
            }

            CTRL_L => {
                shell_write(b"\x1B[2J\x1B[H");
                shell_console_clear();
                shell_write(prompt);
                return input_loop(tokens, prompt, len, cursor_pos);
            }

            CTRL_C => {
                if sel.is_active() {
                    let (lo, hi) = sel.ordered();
                    let hi = hi.min(len);
                    if lo < hi {
                        buffers::with_line_buf(|buf| {
                            input::clipboard_copy(&buf[lo..hi]);
                        });
                    }
                    sel = InputSelection::NONE;
                    rd!();
                    continue;
                }
                shell_write(b"^C\n");
                history::reset_cursor();
                return 0;
            }

            CTRL_V => {
                if sel.is_active() {
                    delete_selection(&mut sel, &mut len, &mut cursor_pos);
                }
                let mut paste_buf = [0u8; 256];
                let pasted = input::clipboard_paste(&mut paste_buf);
                if pasted > 0 {
                    let mut filtered = [0u8; 256];
                    let mut flen = 0;
                    for &b in &paste_buf[..pasted] {
                        if (0x20..=0x7E).contains(&b) {
                            filtered[flen] = b;
                            flen += 1;
                        }
                    }
                    if flen > 0 {
                        insert_text(&filtered, flen, &mut len, &mut cursor_pos);
                        rd!();
                    }
                }
            }

            CTRL_D => {
                if len == 0 {
                    return -1;
                }
                if cursor_pos < len {
                    delete_char_at_cursor(&mut len, cursor_pos);
                    rd!();
                }
            }

            0x09 => {
                let cwd = super::cwd_bytes();
                let comp = buffers::with_line_buf(|buf| {
                    completion::try_complete(buf, len, cursor_pos, &cwd)
                });

                if comp.show_matches {
                    shell_write(b"\n");
                    shell_write(&comp.matches_buf[..comp.matches_len]);
                    shell_write(b"\n");
                    shell_write(prompt);

                    if comp.insertion_len > 0 {
                        insert_text(
                            &comp.insertion,
                            comp.insertion_len,
                            &mut len,
                            &mut cursor_pos,
                        );
                    }

                    return input_loop(tokens, prompt, len, cursor_pos);
                } else if comp.insertion_len > 0 {
                    insert_text(
                        &comp.insertion,
                        comp.insertion_len,
                        &mut len,
                        &mut cursor_pos,
                    );
                    rd!();
                }
            }

            0x20..=0x7E => {
                if sel.is_active() {
                    delete_selection(&mut sel, &mut len, &mut cursor_pos);
                }
                let max_len = buffers::with_line_buf(|buf| buf.len());
                if len + 1 < max_len {
                    buffers::with_line_buf(|buf| {
                        let mut i = len;
                        while i > cursor_pos {
                            buf[i] = buf[i - 1];
                            i -= 1;
                        }
                        buf[cursor_pos] = c;
                    });
                    len += 1;
                    cursor_pos += 1;
                    rd!();
                }
            }

            _ => {}
        }
    }

    buffers::with_line_buf(|buf| {
        let capped = cmp::min(len, buf.len() - 1);
        buf[capped] = 0;
    });

    let expanded_len = buffers::with_line_buf(|line_buf| {
        let line_len = line_buf
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(line_buf.len());
        buffers::with_expand_buf(|expand_buf| {
            super::parser::expand_variables(line_buf, line_len, expand_buf)
        })
    });

    *tokens = [ptr::null(); SHELL_MAX_TOKENS];
    buffers::with_expand_buf(|expand_buf| shell_parse_line(&expand_buf[..expanded_len], tokens))
}

/// Convert a pixel x-coordinate to a character offset within the input buffer.
/// Returns `None` if the click is outside the input area (e.g. on the prompt).
fn pixel_to_input_offset(px: i32, prompt_len: usize, input_len: usize) -> Option<usize> {
    let col = px / crate::gfx::font::cell_width();
    if col < 0 {
        return None;
    }
    let col = col as usize;
    if col < prompt_len {
        return Some(0);
    }
    let offset = col - prompt_len;
    Some(offset.min(input_len))
}

/// Check whether a pixel y-coordinate falls on the current input line row.
fn is_on_input_row(py: i32, line_row: i32) -> bool {
    let row = py / crate::gfx::font::cell_height();
    row == line_row
}

fn delete_selection(
    sel: &mut super::display::InputSelection,
    len: &mut usize,
    cursor_pos: &mut usize,
) {
    let (lo, hi) = sel.ordered();
    if lo >= hi || lo >= *len {
        *sel = super::display::InputSelection::NONE;
        return;
    }
    let hi = hi.min(*len);
    let removed = hi - lo;
    buffers::with_line_buf(|buf| {
        for i in lo..*len - removed {
            buf[i] = buf[i + removed];
        }
        for i in *len - removed..*len {
            buf[i] = 0;
        }
    });
    *len -= removed;
    *cursor_pos = lo;
    *sel = super::display::InputSelection::NONE;
}

fn delete_char_before_cursor(len: &mut usize, cursor_pos: &mut usize) {
    buffers::with_line_buf(|buf| delete_char_before_cursor_in_buf(buf, *cursor_pos, *len));
    *len = len.saturating_sub(1);
    *cursor_pos -= 1;
}

fn delete_char_at_cursor(len: &mut usize, cursor_pos: usize) {
    buffers::with_line_buf(|buf| delete_char_at_cursor_in_buf(buf, cursor_pos, *len));
    *len = len.saturating_sub(1);
}

fn delete_char_before_cursor_in_buf(buf: &mut [u8; 256], cursor_pos: usize, len: usize) {
    for i in cursor_pos - 1..len.saturating_sub(1) {
        buf[i] = buf[i + 1];
    }
    if len > 0 {
        buf[len - 1] = 0;
    }
}

fn delete_char_at_cursor_in_buf(buf: &mut [u8; 256], cursor_pos: usize, len: usize) {
    for i in cursor_pos..len.saturating_sub(1) {
        buf[i] = buf[i + 1];
    }
    if len > 0 {
        buf[len - 1] = 0;
    }
}

fn insert_text(text: &[u8], text_len: usize, len: &mut usize, cursor_pos: &mut usize) {
    let max_len = buffers::with_line_buf(|buf| buf.len());
    let available = max_len.saturating_sub(*len + 1);
    let insert_len = text_len.min(available);
    if insert_len == 0 {
        return;
    }

    buffers::with_line_buf(|buf| {
        let mut i = *len;
        while i > *cursor_pos {
            if i - 1 + insert_len < max_len {
                buf[i - 1 + insert_len] = buf[i - 1];
            }
            i -= 1;
        }
        for i in 0..insert_len {
            buf[*cursor_pos + i] = text[i];
        }
    });
    *len += insert_len;
    *cursor_pos += insert_len;
}

fn redraw(
    line_row: i32,
    prompt: &[u8],
    len: usize,
    cursor_pos: usize,
    cursor_visible: bool,
    selection: &super::display::InputSelection,
) {
    buffers::with_line_buf(|buf| {
        shell_redraw_input(
            line_row,
            prompt,
            prompt_colors_slice(),
            &buf[..len],
            cursor_pos,
            cursor_visible,
            selection,
        );
    });
}
