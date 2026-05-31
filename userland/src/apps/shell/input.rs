use core::cmp;

use crate::ring::{Ring, slopfut};
use crate::syscall::{InputEvent, InputEventData, InputEventType, fs};
use slopos_abi::syscall::{LocalFlags, POLLIN};
use slopos_protocol::types::Event as ProtocolEvent;
use slopos_windowing::ProtocolHandle;
use std::time::{Duration, Instant};

use super::buffers;
use super::buffers::ParsedTokens;
use super::completion;
use slopos_abi::input::POINTER_AXIS_VERTICAL;

use super::display::{
    DISPLAY, shell_console_clear, shell_console_follow_bottom, shell_console_page_down,
    shell_console_page_up, shell_console_scroll_lines, shell_redraw_input, shell_write,
};
use super::history;
use super::parser::shell_parse_line;

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

use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

thread_local! {
    static PROTO_HANDLE: RefCell<Option<ProtocolHandle>> = RefCell::new(None);
}

/// Store the protocol handle for later use by input operations.
pub fn init_handle(handle: ProtocolHandle) {
    PROTO_HANDLE.with(|h| *h.borrow_mut() = Some(handle));
}

fn with_handle<R>(f: impl FnOnce(&ProtocolHandle) -> R) -> R {
    PROTO_HANDLE.with(|h| {
        let h = h.borrow();
        f(h.as_ref().expect("input: no protocol handle"))
    })
}

static PROMPT_COLORS: Mutex<[u8; super::PROMPT_BUF_MAX]> = Mutex::new([0; super::PROMPT_BUF_MAX]);
static PROMPT_COLORS_LEN: AtomicUsize = AtomicUsize::new(0);
static SCROLL_ACCUM: AtomicI32 = AtomicI32::new(0);

// ---------------------------------------------------------------------------
// Deferred event queue: events received during clipboard paste are buffered
// here and drained on the next call to `poll_protocol_events`.
// ---------------------------------------------------------------------------

const DEFERRED_CAPACITY: usize = 16;

struct DeferredQueue {
    buf: [InputEvent; DEFERRED_CAPACITY],
    head: usize,
    tail: usize,
    count: usize,
}

const EMPTY_EVENT: InputEvent = InputEvent {
    event_type: InputEventType::KeyPress,
    _padding: [0; 3],
    timestamp_ms: 0,
    data: InputEventData { data0: 0, data1: 0 },
};

impl DeferredQueue {
    const fn new() -> Self {
        Self {
            buf: [EMPTY_EVENT; DEFERRED_CAPACITY],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    fn push(&mut self, evt: InputEvent) {
        if self.count >= DEFERRED_CAPACITY {
            return; // full -- drop oldest-style would complicate things; just drop newest
        }
        self.buf[self.tail] = evt;
        self.tail = (self.tail + 1) % DEFERRED_CAPACITY;
        self.count += 1;
    }

    fn pop(&mut self) -> Option<InputEvent> {
        if self.count == 0 {
            return None;
        }
        let evt = self.buf[self.head];
        self.head = (self.head + 1) % DEFERRED_CAPACITY;
        self.count -= 1;
        Some(evt)
    }
}

static DEFERRED: Mutex<DeferredQueue> = Mutex::new(DeferredQueue::new());

fn with_deferred<R>(f: impl FnOnce(&mut DeferredQueue) -> R) -> R {
    f(&mut DEFERRED.lock().unwrap())
}

pub fn read_command_line(tokens: &mut ParsedTokens, prompt: &[u8], prompt_colors: &[u8]) -> i32 {
    {
        let mut colors = PROMPT_COLORS.lock().unwrap();
        let copy_len = prompt_colors.len().min(super::PROMPT_BUF_MAX);
        colors[..copy_len].copy_from_slice(&prompt_colors[..copy_len]);
        PROMPT_COLORS_LEN.store(copy_len, Ordering::Relaxed);
    }
    buffers::with_line_buf(|buf| {
        buf.fill(0);
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

    // The prompt's idle wait + clipboard-paste spin now ride the slopfut
    // runtime: a `select2(poll_add(protocol_fd, POLLIN), sleep_ms(blink))`
    // replaces the `poll(protocol_fd, remaining_blink)` and the paste loop's
    // `yield_now` busy-spin becomes a `poll_add` await. The whole interactive
    // loop is the `block_on` root.
    let result = match Ring::setup(16) {
        Ok(ring) => slopfut::block_on(ring, input_loop(tokens, prompt, 0, 0)),
        Err(_) => {
            // Ring unavailable: cannot run the async editor. Yield once and
            // re-prompt (return 0 → caller `continue`s the prompt loop)
            // rather than wedging or busy-spinning.
            crate::syscall::core::yield_now();
            0
        }
    };

    // Restore canonical mode so child processes (nc, etc.) get line-buffered input.
    if let Some(ref t) = saved_termios {
        let _ = fs::tcsetattr(0, t);
    }

    result
}

fn prompt_colors_snapshot() -> ([u8; super::PROMPT_BUF_MAX], usize) {
    let colors = *PROMPT_COLORS.lock().unwrap();
    let len = PROMPT_COLORS_LEN.load(Ordering::Relaxed);
    (colors, len)
}

/// Milliseconds until the next cursor-blink flip, given how long has elapsed
/// since the last flip and the blink interval. Clamped to >= 1ms so the blink
/// timer always makes forward progress. Pure for host-side testing.
fn blink_remaining_ms(elapsed: Duration, interval: Duration) -> u64 {
    let remaining = interval.saturating_sub(elapsed);
    (remaining.as_millis() as u64).max(1)
}

async fn input_loop(
    tokens: &mut ParsedTokens,
    prompt: &[u8],
    initial_len: usize,
    initial_cursor_pos: usize,
) -> i32 {
    use super::display::InputSelection;

    const BLINK_INTERVAL: Duration = Duration::from_millis(500);

    // Cache the compositor socket fd once. The connection's lifetime
    // matches the shell process, so the fd is stable for the duration
    // of read_input.
    let protocol_fd = with_handle(|h| h.borrow_client().fd());

    // `'restart` replaces the old self-recursion (CTRL_L / tab show-matches):
    // those used to `return input_loop(...)` to re-run the editor with an
    // updated len/cursor; an `async fn` cannot tail-recurse (infinitely-
    // sized future), so they now reset editor state and `continue 'restart`.
    let mut len = initial_len;
    let mut cursor_pos = initial_cursor_pos;
    'restart: loop {
        let mut line_row = super::display::shell_console_get_cursor().1;

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
            let count = poll_protocol_events(&mut events);
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
                            let delta = events[i].axis_value_v120();
                            let prev = SCROLL_ACCUM.fetch_add(delta, Ordering::Relaxed);
                            let new_accum = prev + delta;
                            let lines = new_accum / 120;
                            SCROLL_ACCUM.store(new_accum % 120, Ordering::Relaxed);
                            if lines != 0 {
                                shell_console_scroll_lines(lines);
                            }
                        }
                    }
                    InputEventType::CloseRequest => {
                        std::process::exit(0);
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

            let rc = match key_event {
                Some(c) => c as i64,
                None => -1,
            };
            if rc < 0 {
                let now = Instant::now();
                let elapsed = now.duration_since(last_blink);
                if elapsed >= BLINK_INTERVAL {
                    cursor_visible = !cursor_visible;
                    last_blink = now;
                    rd!();
                    continue;
                }
                if !mouse_acted {
                    // Sleep until either the compositor socket has data or the
                    // next blink flip is due. `select2` over an `OP_POLL_ADD`
                    // (compositor readiness) and an `OP_TIMEOUT` (blink deadline)
                    // replaces the `poll(protocol_fd, remaining_blink)` wakeup;
                    // the synchronous event drain above still does the reading.
                    let remaining_ms = blink_remaining_ms(elapsed, BLINK_INTERVAL);
                    let wake = slopfut::poll_add(protocol_fd, POLLIN);
                    let blink = Box::pin(slopfut::time::sleep_ms(remaining_ms));
                    let _ = slopfut::select2(wake, blink).await;
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

            if !DISPLAY.follow.load(std::sync::atomic::Ordering::Relaxed) {
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
                    let new_len = buffers::with_line_buf(|buf| {
                        history::navigate_up(&snapshot[..len], len, buf)
                    });
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
                    // Re-run the editor with the current buffer (was a tail-call
                    // `return input_loop(...)`; the async fn restarts instead).
                    continue 'restart;
                }

                CTRL_C => {
                    if sel.is_active() {
                        let (lo, hi) = sel.ordered();
                        let hi = hi.min(len);
                        if lo < hi {
                            buffers::with_line_buf(|buf| {
                                with_handle(|h| {
                                    let _ = h.borrow_client().clipboard_copy(&buf[lo..hi]);
                                });
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
                    let pasted = protocol_clipboard_paste(protocol_fd, &mut paste_buf).await;
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

                        // Re-run the editor (was a tail-call `return
                        // input_loop(...)`; the async fn restarts instead).
                        continue 'restart;
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

        // The inner loop broke on Enter: assemble the line and return the
        // token count. A `return` exits `'restart` and the whole async fn;
        // the CTRL_L / tab-completion arms `continue 'restart` to re-edit.
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

        tokens.clear();
        buffers::with_expand_buf(|expand_buf| {
            shell_parse_line(&expand_buf[..expanded_len], tokens)
        });
        return tokens.count() as i32;
    }
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
    let (pc_buf, pc_len) = prompt_colors_snapshot();
    buffers::with_line_buf(|buf| {
        shell_redraw_input(
            line_row,
            prompt,
            &pc_buf[..pc_len],
            &buf[..len],
            cursor_pos,
            cursor_visible,
            selection,
        );
    });
}

/// Poll protocol events from the compositor and convert them to InputEvents.
///
/// Drains the deferred queue first (events stashed during clipboard paste),
/// then polls fresh events from the compositor socket.
pub(crate) fn poll_protocol_events(events: &mut [InputEvent]) -> usize {
    let mut count = 0usize;

    // Phase 1: drain deferred events that were buffered during paste.
    with_deferred(|q| {
        while count < events.len() {
            match q.pop() {
                Some(evt) => {
                    events[count] = evt;
                    count += 1;
                }
                None => break,
            }
        }
    });

    // Phase 2: poll the compositor socket for fresh events.
    with_handle(|h| {
        let mut client = h.borrow_client();
        while count < events.len() {
            match client.poll_event() {
                Ok(Some(evt)) => {
                    if let Some(input_evt) = protocol_event_to_input_event(&evt) {
                        events[count] = input_evt;
                        count += 1;
                    }
                }
                _ => break,
            }
        }
    });
    count
}

/// Convert a compositor protocol event into a kernel InputEvent.
fn protocol_event_to_input_event(evt: &ProtocolEvent) -> Option<InputEvent> {
    match evt {
        ProtocolEvent::Key {
            time,
            scancode,
            ascii,
            pressed,
            ..
        } => Some(InputEvent::key(
            if *pressed {
                InputEventType::KeyPress
            } else {
                InputEventType::KeyRelease
            },
            *scancode as u8,
            *ascii as u8,
            *time as u64,
        )),
        ProtocolEvent::PointerMotion { time, x, y } => {
            Some(InputEvent::pointer_motion(*x, *y, *time as u64))
        }
        ProtocolEvent::PointerButton {
            time,
            button,
            pressed,
            ..
        } => Some(InputEvent::pointer_button(
            *pressed,
            *button as u8,
            *time as u64,
        )),
        ProtocolEvent::PointerEnter { x, y, .. } => {
            Some(InputEvent::pointer_enter_leave(true, *x, *y, 0))
        }
        ProtocolEvent::PointerLeave { .. } => Some(InputEvent::pointer_enter_leave(false, 0, 0, 0)),
        ProtocolEvent::PointerAxis { axis, value, time } => {
            Some(InputEvent::pointer_axis(*axis, *value, *time as u64))
        }
        ProtocolEvent::Configure { width, height, .. } => {
            Some(InputEvent::configure(*width, *height, 0))
        }
        ProtocolEvent::Close { .. } => Some(InputEvent::close_request(0)),
        _ => None,
    }
}

/// Request clipboard paste from the compositor and wait for the result.
///
/// Non-paste events received while waiting are pushed into the module-level
/// `DeferredQueue` so they are replayed on the next `poll_protocol_events`
/// call -- no events are lost regardless of type.
async fn protocol_clipboard_paste(protocol_fd: i32, buf: &mut [u8]) -> usize {
    // Request the paste; the borrow is dropped before any await so the
    // protocol client is never borrowed across a suspension point.
    let requested = with_handle(|h| h.borrow_client().clipboard_paste().is_ok());
    if !requested {
        return 0;
    }
    for _ in 0..100 {
        // Drain one event with a short borrow.
        let polled = with_handle(|h| h.borrow_client().poll_event());
        match polled {
            Ok(Some(ProtocolEvent::PasteResult(cb))) => {
                let copy = (cb.len as usize).min(buf.len());
                buf[..copy].copy_from_slice(&cb.data[..copy]);
                return copy;
            }
            Ok(Some(other)) => {
                if let Some(evt) = protocol_event_to_input_event(&other) {
                    with_deferred(|q| q.push(evt));
                }
            }
            Ok(None) => {
                // Park on compositor readiness instead of busy-yielding:
                // the PasteResult arrives as a socket message, so an
                // `OP_POLL_ADD` wakeup replaces the old `yield_now` spin.
                let _ = slopfut::poll_add(protocol_fd, POLLIN).await;
            }
            Err(_) => return 0,
        }
    }
    0
}
