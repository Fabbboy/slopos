//! Raw fd0 line editor for the PTY-slave shell.
//!
//! Bytes come off fd 0 (the PTY slave) on the slopfut ring and are decoded into
//! internal key codes, so the editor's `match` dispatch is the same whether a
//! key arrived as a literal byte or a CSI sequence. Redraws are ANSI to fd 1.

use core::cmp;

use crate::ring::{Ring, slopfut};
use crate::syscall::fs;
use slopfut::signal::SignalListener;
use slopos_abi::signal::{SIGWINCH, sig_bit};
use slopos_abi::syscall::{LocalFlags, POLLIN};

use super::buffers;
use super::buffers::ParsedTokens;
use super::completion;
use super::display::shell_write;
use super::history;
use super::parser::shell_parse_line;

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

// Internal key codes, outside the printable ASCII range so they never collide
// with literal input bytes.
const KEY_PAGE_UP: u8 = 0x80;
const KEY_PAGE_DOWN: u8 = 0x81;
const KEY_UP: u8 = 0x82;
const KEY_DOWN: u8 = 0x83;
const KEY_LEFT: u8 = 0x84;
const KEY_RIGHT: u8 = 0x85;
const KEY_HOME: u8 = 0x86;
const KEY_END: u8 = 0x87;
const KEY_DELETE: u8 = 0x88;

const CTRL_A: u8 = 0x01;
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;
const CTRL_E: u8 = 0x05;
const CTRL_K: u8 = 0x0B;
const CTRL_L: u8 = 0x0C;
const CTRL_U: u8 = 0x15;
const CTRL_W: u8 = 0x17;

const ESC: u8 = 0x1b;

const STDIN_FD: i32 = 0;

/// One byte per `read(2)`: fd 0 is shared with every command the shell
/// launches, so anything read ahead of the newline would be dropped on the floor
/// or stolen from the program about to run.
const READ_CHUNK: usize = 1;

/// Default editor width when `tiocgwinsz(0)` fails (no terminal wired).
const DEFAULT_COLS: usize = 80;

/// Window for telling a bare ESC from the start of a CSI/SS3 sequence: a real
/// sequence's bytes arrive back-to-back, a lone ESC keypress has no follow-up.
const ESC_TIMEOUT_MS: u64 = 30;

static PROMPT_COLORS: Mutex<[u8; super::PROMPT_BUF_MAX]> = Mutex::new([0; super::PROMPT_BUF_MAX]);
static PROMPT_COLORS_LEN: AtomicUsize = AtomicUsize::new(0);

/// Events decoded from bytes already taken off fd 0 but not yet dispatched.
/// Those bytes are off the descriptor, so dropping them at the end of a prompt
/// would lose input no other reader can recover. It never holds bytes the editor
/// has not read — that is what leaves type-ahead for the command about to run.
static PENDING_EVENTS: Mutex<VecDeque<Decoded>> = Mutex::new(VecDeque::new());

/// Consecutive failures to build the editor's ring.  Bounded so a permanently
/// broken ring ends the shell instead of spinning on the prompt.
static RING_FAILURES: AtomicUsize = AtomicUsize::new(0);
const RING_FAILURE_LIMIT: usize = 3;

pub enum LineOutcome {
    /// A parsed command line holding this many tokens.
    Ready(usize),
    /// Nothing to run: a blank line, or an editing action that consumed it.
    Empty,
    /// The line outgrew the editor's buffer. Refused rather than truncated: a
    /// shortened command line means something other than what was typed.
    TooLong,
    /// End of input — Ctrl+D on an empty line, or the terminal hung up.
    Eof,
    /// Ctrl+C: the line was abandoned.
    Interrupted,
}

pub fn read_command_line(
    tokens: &mut ParsedTokens,
    prompt: &[u8],
    prompt_colors: &[u8],
) -> LineOutcome {
    {
        let mut colors = PROMPT_COLORS.lock().unwrap();
        let copy_len = prompt_colors.len().min(super::PROMPT_BUF_MAX);
        colors[..copy_len].copy_from_slice(&prompt_colors[..copy_len]);
        PROMPT_COLORS_LEN.store(copy_len, Ordering::Relaxed);
    }
    buffers::with_line_buf(|buf| {
        buf.fill(0);
    });

    let cols = query_cols();

    // Dropping the listener at the end of this call restores the signal mask
    // before any child command runs.
    let winch = SignalListener::new(sig_bit(SIGWINCH));

    // Raw mode: the shell does its own rendering and line editing.
    let saved_termios = fs::tcgetattr(STDIN_FD).ok();
    if let Some(ref t) = saved_termios {
        let mut raw = *t;
        raw.c_lflag &=
            !(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG | LocalFlags::ECHOE);
        let _ = fs::tcsetattr(STDIN_FD, &raw);
    }

    // Bracketed paste on: pasted text arrives wrapped in \x1b[200~..201~ and is
    // inserted literally rather than interpreted as commands.
    let _ = fs::write_slice(1, b"\x1b[?2004h");

    let result = match Ring::setup(16) {
        Ok(ring) => slopfut::block_on(ring, input_loop(tokens, prompt, cols, winch.as_ref())),
        Err(_) => {
            crate::syscall::core::yield_now();
            if RING_FAILURES.fetch_add(1, Ordering::Relaxed) + 1 >= RING_FAILURE_LIMIT {
                super::display::shell_error(b"sh: cannot start the line editor\n");
                LineOutcome::Eof
            } else {
                LineOutcome::Interrupted
            }
        }
    };
    if matches!(result, LineOutcome::Ready(_) | LineOutcome::Empty) {
        RING_FAILURES.store(0, Ordering::Relaxed);
    }

    // Restore canonical mode (ISIG back on) before the paste-off write: that
    // write can block on a full output queue, and a blocking write with ISIG off
    // cannot be interrupted from the keyboard.
    if let Some(ref t) = saved_termios {
        let _ = fs::tcsetattr(STDIN_FD, t);
    }
    let _ = fs::write_slice(1, b"\x1b[?2004l");

    result
}

fn prompt_colors_snapshot() -> ([u8; super::PROMPT_BUF_MAX], usize) {
    let colors = *PROMPT_COLORS.lock().unwrap();
    let len = PROMPT_COLORS_LEN.load(Ordering::Relaxed);
    (colors, len)
}

enum Decoded {
    /// A printable / control byte or a decoded internal key code.
    Key(u8),
    /// A complete multi-byte UTF-8 character (`seq[..len]`), decoded here so raw
    /// continuation bytes can never alias the internal key-code space (0x80..).
    Char([u8; 4], usize),
    /// A run of literal bytes from a bracketed paste (control chars already
    /// stripped except `\t`).
    Paste(Vec<u8>),
}

/// Consumes the fd0 byte stream and emits [`Decoded`] events. Partial escape
/// sequences are retained across `feed` calls, so a sequence split across two
/// reads still parses.
struct EscParser {
    /// Bytes of an in-progress escape sequence (starts with ESC).
    pending: Vec<u8>,
    /// True while inside a bracketed-paste run (`\x1b[200~` seen, `201~`
    /// not yet).
    in_paste: bool,
    paste_buf: Vec<u8>,
    /// Last paste byte was `\r`, so a following `\n` is the other half of one
    /// CRLF terminator rather than a second line break.
    paste_last_cr: bool,
    utf8: [u8; 4],
    utf8_len: usize,
    /// Total bytes the current UTF-8 sequence needs (0 = none pending).
    utf8_need: usize,
}

impl EscParser {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            in_paste: false,
            paste_buf: Vec::new(),
            paste_last_cr: false,
            utf8: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
        }
    }

    /// True when a bare ESC is buffered with no follow-up yet — the caller
    /// must disambiguate against a short timeout.
    fn awaiting_escape(&self) -> bool {
        !self.pending.is_empty() && !self.in_paste
    }

    /// Resolve a buffered bare ESC — the timeout elapsed with no follow-up
    /// byte — as a literal ESC key.
    fn flush_escape(&mut self, out: &mut Vec<Decoded>) {
        if self.awaiting_escape() {
            self.pending.clear();
            out.push(Decoded::Key(ESC));
        }
    }

    fn feed(&mut self, bytes: &[u8], out: &mut Vec<Decoded>) {
        for &b in bytes {
            self.feed_byte(b, out);
        }
    }

    fn feed_byte(&mut self, b: u8, out: &mut Vec<Decoded>) {
        if self.in_paste {
            self.feed_paste_byte(b, out);
            return;
        }
        if !self.pending.is_empty() {
            self.pending.push(b);
            self.try_complete(out);
            return;
        }
        if self.utf8_need > 0 {
            if is_utf8_continuation(b) {
                self.utf8[self.utf8_len] = b;
                self.utf8_len += 1;
                if self.utf8_len == self.utf8_need {
                    let n = self.utf8_need;
                    self.utf8_need = 0;
                    self.utf8_len = 0;
                    // Overlongs and surrogates are dropped, not shown as text.
                    if core::str::from_utf8(&self.utf8[..n]).is_ok() {
                        out.push(Decoded::Char(self.utf8, n));
                    }
                }
                return;
            }
            self.utf8_need = 0;
            self.utf8_len = 0;
        }
        match b {
            ESC => self.pending.push(b),
            // UTF-8 lead bytes (2/3/4-byte sequences).
            0xC2..=0xF4 => {
                self.utf8[0] = b;
                self.utf8_len = 1;
                self.utf8_need = match b {
                    0xC2..=0xDF => 2,
                    0xE0..=0xEF => 3,
                    _ => 4,
                };
            }
            // Stray continuation bytes and invalid leads: dropped, since 0x80..
            // is the editor's internal key-code space.
            0x80..=0xC1 | 0xF5..=0xFF => {}
            _ => out.push(Decoded::Key(b)),
        }
    }

    fn feed_paste_byte(&mut self, b: u8, out: &mut Vec<Decoded>) {
        // The paste terminator is the literal sequence \x1b[201~.
        if !self.pending.is_empty() {
            self.pending.push(b);
            if is_paste_end(&self.pending) {
                self.in_paste = false;
                self.pending.clear();
                let run = core::mem::take(&mut self.paste_buf);
                if !run.is_empty() {
                    out.push(Decoded::Paste(run));
                }
            } else if !could_be_paste_end(&self.pending) {
                let drained = core::mem::take(&mut self.pending);
                for c in drained {
                    self.push_paste_literal(c);
                }
            }
            return;
        }
        if b == ESC {
            self.pending.push(b);
        } else {
            self.push_paste_literal(b);
        }
    }

    fn push_paste_literal(&mut self, b: u8) {
        // C0 controls and DEL are stripped, except the line terminators: those
        // carry the structure of a pasted script.
        let was_cr = core::mem::replace(&mut self.paste_last_cr, b == b'\r');
        match b {
            b'\r' => self.paste_buf.push(b'\n'),
            // The `\n` half of a CRLF pair; its `\r` already ended the line.
            b'\n' if was_cr => {}
            b'\n' => self.paste_buf.push(b'\n'),
            b'\t' | 0x20..=0x7e => self.paste_buf.push(b),
            _ if b >= 0x80 => self.paste_buf.push(b),
            _ => {}
        }
    }

    fn try_complete(&mut self, out: &mut Vec<Decoded>) {
        let p = &self.pending;
        // Bracketed paste start.
        if p.as_slice() == b"\x1b[200~" {
            self.in_paste = true;
            self.pending.clear();
            self.paste_buf.clear();
            return;
        }
        match decode_escape(p) {
            EscMatch::Complete(code) => {
                self.pending.clear();
                out.push(Decoded::Key(code));
            }
            EscMatch::Partial => {}
            EscMatch::Invalid => {
                self.pending.clear();
            }
        }
    }
}

enum EscMatch {
    Complete(u8),
    Partial,
    Invalid,
}

fn could_be_paste_end(s: &[u8]) -> bool {
    let end = b"\x1b[201~";
    s.len() <= end.len() && end.starts_with(s)
}

fn is_paste_end(s: &[u8]) -> bool {
    s == b"\x1b[201~"
}

/// Decode a buffered escape sequence into an internal key code: CSI arrows /
/// Home / End, the `~`-terminated editing keys, and SS3 arrows (`\x1bO…`).
fn decode_escape(p: &[u8]) -> EscMatch {
    if p.len() == 1 {
        return EscMatch::Partial;
    }
    match p[1] {
        b'[' | b'O' => {}
        _ => return EscMatch::Invalid,
    }
    if p.len() == 2 {
        return EscMatch::Partial;
    }
    match p[2] {
        b'A' => return EscMatch::Complete(KEY_UP),
        b'B' => return EscMatch::Complete(KEY_DOWN),
        b'C' => return EscMatch::Complete(KEY_RIGHT),
        b'D' => return EscMatch::Complete(KEY_LEFT),
        b'H' => return EscMatch::Complete(KEY_HOME),
        b'F' => return EscMatch::Complete(KEY_END),
        b'0'..=b'9' => {} // parameterised: needs a trailing '~'
        _ => return EscMatch::Invalid,
    }
    if *p.last().unwrap() == b'~' {
        return match &p[2..p.len() - 1] {
            b"3" => EscMatch::Complete(KEY_DELETE),
            b"5" => EscMatch::Complete(KEY_PAGE_UP),
            b"6" => EscMatch::Complete(KEY_PAGE_DOWN),
            b"1" => EscMatch::Complete(KEY_HOME),
            b"4" => EscMatch::Complete(KEY_END),
            _ => EscMatch::Invalid,
        };
    }
    // Still accumulating digits before the '~'.
    if p.len() < 8 {
        EscMatch::Partial
    } else {
        EscMatch::Invalid
    }
}

/// A long-lived `SignalListener::recv` future, re-armed only after it resolves:
/// losing a select race must not cancel it, or a completed-but-unobserved resize
/// would be discarded with the dropped future.
type WinchFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = u32> + 'a>>;

async fn input_loop(
    tokens: &mut ParsedTokens,
    prompt: &[u8],
    cols: usize,
    winch: Option<&SignalListener>,
) -> LineOutcome {
    let mut cols = cols;
    let mut parser = EscParser::new();
    let mut winch_fut: Option<WinchFuture<'_>> =
        winch.map(|w| -> WinchFuture<'_> { Box::pin(w.recv()) });
    let mut queue: VecDeque<Decoded> = std::mem::take(&mut *PENDING_EVENTS.lock().unwrap());
    let mut overflowed = false;

    // Region-relative cursor row (see `redraw`): the REPL pre-printed the
    // prompt, so the first region starts at its row, cursor resting after it.
    let mut cur_row = row_of_offset(prompt.len(), cols.max(1));

    'restart: loop {
        let mut len = 0usize;
        let mut cursor_pos = 0usize;

        redraw(prompt, len, cursor_pos, cols, &mut cur_row);

        loop {
            let decoded = match queue.pop_front() {
                Some(d) => d,
                None => {
                    match next_decoded(&mut parser, &mut queue, winch_fut.as_mut()).await {
                        Wake::Winch => {
                            // The resolved listener is spent: re-arm it before
                            // anything else awaits.
                            if let (Some(slot), Some(w)) = (winch_fut.as_mut(), winch) {
                                *slot = Box::pin(w.recv());
                            }
                            cols = query_cols();
                            redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                            continue;
                        }
                        Wake::Eof => return LineOutcome::Eof,
                        Wake::Input => {}
                    }
                    match queue.pop_front() {
                        Some(d) => d,
                        None => continue,
                    }
                }
            };

            let c = match decoded {
                Decoded::Paste(text) => {
                    // A pasted line break ends a command, exactly as typing one
                    // would; the remainder is queued for the next prompt.
                    if let Some(nl) = text.iter().position(|&b| b == b'\n') {
                        let rest = text[nl + 1..].to_vec();
                        if !rest.is_empty() {
                            queue.push_front(Decoded::Paste(rest));
                        }
                        queue.push_front(Decoded::Key(b'\n'));
                        let head = &text[..nl];
                        overflowed |= !insert_text(head, head.len(), &mut len, &mut cursor_pos);
                    } else {
                        overflowed |= !insert_text(&text, text.len(), &mut len, &mut cursor_pos);
                    }
                    redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    continue;
                }
                Decoded::Char(seq, n) => {
                    overflowed |= !insert_text(&seq, n, &mut len, &mut cursor_pos);
                    redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    continue;
                }
                Decoded::Key(c) => c,
            };

            match c {
                b'\n' | b'\r' => {
                    // Finish below the LAST wrapped row of the input, not
                    // wherever the cursor happens to sit mid-region.
                    let end_row = row_of_offset(prompt.len() + cells_upto(len), cols.max(1));
                    emit_cursor_move(end_row.saturating_sub(cur_row), b'B');
                    super::display::shell_echo_char(b'\n');
                    buffers::with_line_buf(|buf| {
                        history::push(buf, len);
                    });
                    history::reset_cursor();
                    break;
                }

                b'\x08' | 0x7f => {
                    if cursor_pos > 0 {
                        delete_char_before_cursor(&mut len, &mut cursor_pos);
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                KEY_DELETE => {
                    if cursor_pos < len {
                        delete_char_at_cursor(&mut len, cursor_pos);
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
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
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                KEY_DOWN => {
                    let new_len = buffers::with_line_buf(|buf| history::navigate_down(buf));
                    if let Some(nl) = new_len {
                        len = nl;
                        cursor_pos = nl;
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                KEY_LEFT => {
                    if cursor_pos > 0 {
                        cursor_pos = buffers::with_line_buf(|buf| prev_char_start(buf, cursor_pos));
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                KEY_RIGHT => {
                    if cursor_pos < len {
                        cursor_pos =
                            buffers::with_line_buf(|buf| next_char_end(buf, cursor_pos, len));
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                KEY_HOME | CTRL_A => {
                    if cursor_pos != 0 {
                        cursor_pos = 0;
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                KEY_END | CTRL_E => {
                    if cursor_pos != len {
                        cursor_pos = len;
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                // The terminal owns scrollback, so these are editor no-ops.
                KEY_PAGE_UP | KEY_PAGE_DOWN => {}

                CTRL_K => {
                    if cursor_pos < len {
                        buffers::with_line_buf(|buf| {
                            for i in cursor_pos..len {
                                buf[i] = 0;
                            }
                        });
                        len = cursor_pos;
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
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
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
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
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                CTRL_L => {
                    shell_write(b"\x1b[2J\x1b[H");
                    cur_row = 0;
                    // Restart with a blank line on the cleared screen; an async
                    // fn cannot tail-recurse.
                    continue 'restart;
                }

                CTRL_C => {
                    let end_row = row_of_offset(prompt.len() + cells_upto(len), cols.max(1));
                    emit_cursor_move(end_row.saturating_sub(cur_row), b'B');
                    shell_write(b"^C\n");
                    history::reset_cursor();
                    stash(&mut queue);
                    return LineOutcome::Interrupted;
                }

                CTRL_D => {
                    if len == 0 {
                        // Echo the `exit` the user would have typed instead.
                        shell_write(b"exit\n");
                        stash(&mut queue);
                        return LineOutcome::Eof;
                    }
                    if cursor_pos < len {
                        delete_char_at_cursor(&mut len, cursor_pos);
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                0x09 => {
                    let cwd = super::cwd_bytes();
                    let comp = buffers::with_line_buf(|buf| {
                        completion::try_complete(buf, len, cursor_pos, &cwd)
                    });

                    if comp.show_matches {
                        let end_row = row_of_offset(prompt.len() + cells_upto(len), cols.max(1));
                        emit_cursor_move(end_row.saturating_sub(cur_row), b'B');
                        shell_write(b"\n");
                        shell_write(&comp.matches_buf[..comp.matches_len]);
                        shell_write(b"\n");
                        cur_row = 0;

                        if comp.insertion_len > 0 {
                            insert_text(
                                &comp.insertion,
                                comp.insertion_len,
                                &mut len,
                                &mut cursor_pos,
                            );
                        }
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    } else if comp.insertion_len > 0 {
                        insert_text(
                            &comp.insertion,
                            comp.insertion_len,
                            &mut len,
                            &mut cursor_pos,
                        );
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                0x20..=0x7e => {
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
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                _ => {}
            }
        }

        stash(&mut queue);

        if overflowed {
            return LineOutcome::TooLong;
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
            let text = super::parser::strip_comment(&line_buf[..line_len]);
            buffers::with_expand_buf(|expand_buf| {
                super::parser::expand_variables(text, text.len(), expand_buf)
            })
        });

        tokens.clear();
        buffers::with_expand_buf(|expand_buf| {
            shell_parse_line(&expand_buf[..expanded_len], tokens)
        });
        return match tokens.count() {
            0 => LineOutcome::Empty,
            n => LineOutcome::Ready(n),
        };
    }
}

/// Park still-undispatched events for the next prompt. See [`PENDING_EVENTS`].
fn stash(queue: &mut VecDeque<Decoded>) {
    if queue.is_empty() {
        return;
    }
    let mut pending = PENDING_EVENTS.lock().unwrap();
    pending.append(queue);
}

/// Why `next_decoded` returned: stdin made progress, the terminal was resized
/// and the caller must re-query its width, or input ended.
enum Wake {
    Input,
    Winch,
    Eof,
}

fn query_cols() -> usize {
    match fs::tiocgwinsz(STDIN_FD) {
        Ok(ws) if ws.ws_col != 0 => ws.ws_col as usize,
        _ => DEFAULT_COLS,
    }
}

fn flush_pending_escape(parser: &mut EscParser, queue: &mut std::collections::VecDeque<Decoded>) {
    let mut out = Vec::new();
    parser.flush_escape(&mut out);
    for d in out {
        queue.push_back(d);
    }
}

/// Read the next batch of decoded events into `queue`, blocking on fd 0. A bare
/// ESC buffered in the parser resolves as a literal keypress if no follow-up
/// byte arrives within the timeout.
///
/// The resize listener is raced as the *first* select arm, against non-consuming
/// waits only (fd readiness / timer): a resize landing in the same reactor batch
/// as input readiness then wins rather than being dropped with its already
/// drained signal, and losing an arm never cancels state that consumed bytes.
async fn next_decoded(
    parser: &mut EscParser,
    queue: &mut std::collections::VecDeque<Decoded>,
    winch: Option<&mut WinchFuture<'_>>,
) -> Wake {
    let buf = vec![0u8; READ_CHUNK];

    let result = if parser.awaiting_escape() {
        use slopfut::{Either2, Either3};
        let ready = Box::pin(slopfut::poll_add(STDIN_FD, POLLIN));
        let timer = Box::pin(slopfut::time::sleep_ms(ESC_TIMEOUT_MS));
        let input_ready = match winch {
            Some(w) => match slopfut::select3(w, ready, timer).await {
                Either3::A(_) => return Wake::Winch,
                Either3::B(_) => true,
                Either3::C(()) => false,
            },
            None => match slopfut::select2(ready, timer).await {
                Either2::A(_) => true,
                Either2::B(()) => false,
            },
        };
        if input_ready {
            Some(slopfut::read(STDIN_FD, buf, READ_CHUNK as u32).await)
        } else {
            flush_pending_escape(parser, queue);
            None
        }
    } else {
        use slopfut::Either2;
        let ready = Box::pin(slopfut::poll_add(STDIN_FD, POLLIN));
        match winch {
            Some(w) => match slopfut::select2(w, ready).await {
                Either2::A(_) => return Wake::Winch,
                Either2::B(_) => Some(slopfut::read(STDIN_FD, buf, READ_CHUNK as u32).await),
            },
            None => {
                let _ = ready.await;
                Some(slopfut::read(STDIN_FD, buf, READ_CHUNK as u32).await)
            }
        }
    };

    let Some(r) = result else { return Wake::Input };
    if r.res == 0 {
        // Zero bytes means the PTY master hung up; a deliberate Ctrl-D arrives
        // as 0x04 and is handled by the CTRL_D arm. Reported rather than exiting
        // here, so the REPL exits with the status of the last command it ran.
        return Wake::Eof;
    }
    if r.res < 0 {
        // Only a retryable error is worth re-arming for; anything else would
        // spin the editor at full speed against a descriptor that never works.
        let errno = -r.res as i32;
        let retryable = errno == crate::syscall::SyscallError::EAGAIN.errno()
            || errno == crate::syscall::SyscallError::EINTR.errno();
        return if retryable { Wake::Input } else { Wake::Eof };
    }
    let n = r.res as usize;
    let mut out = Vec::new();
    parser.feed(&r.buf[..n.min(r.buf.len())], &mut out);
    for d in out {
        queue.push_back(d);
    }
    Wake::Input
}

// The line buffer holds UTF-8 bytes, the cursor always rests on a character
// boundary, and the terminal renders one cell per character.

/// True for a UTF-8 continuation byte (`0b10xx_xxxx`).
#[inline]
fn is_utf8_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// Byte offset where the character left of `pos` starts (`pos > 0`).
fn prev_char_start(buf: &[u8; 256], pos: usize) -> usize {
    let mut i = pos - 1;
    while i > 0 && is_utf8_continuation(buf[i]) {
        i -= 1;
    }
    i
}

/// Byte offset just past the character starting at `pos` (`pos < len`).
fn next_char_end(buf: &[u8; 256], pos: usize, len: usize) -> usize {
    let mut i = pos + 1;
    while i < len && is_utf8_continuation(buf[i]) {
        i += 1;
    }
    i
}

/// Display cells occupied by the first `n` buffer bytes.
fn cells_upto(n: usize) -> usize {
    buffers::with_line_buf(|buf| {
        buf[..n]
            .iter()
            .filter(|&&b| !is_utf8_continuation(b))
            .count()
    })
}

fn delete_byte_range(len: &mut usize, start: usize, end: usize) {
    let removed = end - start;
    buffers::with_line_buf(|buf| {
        for i in start..*len - removed {
            buf[i] = buf[i + removed];
        }
        for i in *len - removed..*len {
            buf[i] = 0;
        }
    });
    *len -= removed;
}

/// Delete the whole character left of the cursor (`cursor_pos > 0`).
fn delete_char_before_cursor(len: &mut usize, cursor_pos: &mut usize) {
    let start = buffers::with_line_buf(|buf| prev_char_start(buf, *cursor_pos));
    delete_byte_range(len, start, *cursor_pos);
    *cursor_pos = start;
}

/// Delete the whole character under the cursor (`cursor_pos < len`).
fn delete_char_at_cursor(len: &mut usize, cursor_pos: usize) {
    let end = buffers::with_line_buf(|buf| next_char_end(buf, cursor_pos, *len));
    delete_byte_range(len, cursor_pos, end);
}

/// Insert `text` at the cursor. Returns `false` when the line buffer was too
/// full to take all of it, so the caller can refuse the line rather than run a
/// truncated version of what the user typed.
fn insert_text(text: &[u8], text_len: usize, len: &mut usize, cursor_pos: &mut usize) -> bool {
    let max_len = buffers::with_line_buf(|buf| buf.len());
    let available = max_len.saturating_sub(*len + 1);
    let mut insert_len = text_len.min(available);
    // Never cut a multi-byte character in half at the capacity limit.
    while insert_len > 0 && insert_len < text_len && is_utf8_continuation(text[insert_len]) {
        insert_len -= 1;
    }
    if insert_len == 0 {
        return text_len == 0;
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
    insert_len == text_len
}

/// Cursor row within the edit region after printing `offset` cells, under
/// the terminal's deferred autowrap: an exactly-full row leaves the cursor
/// resting on that row (wrap pending), not on the next one.
fn row_of_offset(offset: usize, cols: usize) -> usize {
    if offset == 0 { 0 } else { (offset - 1) / cols }
}

/// Redraw the edit region in place. Inputs wider than the terminal wrap across
/// rows, so the redraw is region-based: move to the region's first row, erase
/// everything below, reprint prompt + buffer, then reposition the cursor.
/// `cur_row` is the row within the region the previous redraw left it on.
fn redraw(prompt: &[u8], len: usize, cursor_pos: usize, cols: usize, cur_row: &mut usize) {
    use super::display::shell_write_idx;
    let cols = cols.max(1);

    // Column 0 of the region's first row, then wipe every rendered row below.
    shell_write(b"\r");
    emit_cursor_move(*cur_row, b'A');
    shell_write(b"\x1b[J");

    // Colored prompt: emit per-color runs.
    let (pc_buf, pc_len) = prompt_colors_snapshot();
    let mut i = 0;
    while i < prompt.len() {
        let color = if i < pc_len { pc_buf[i] } else { 0 };
        let start = i;
        while i < prompt.len() && (i >= pc_len || pc_buf[i] == color) {
            i += 1;
        }
        shell_write_idx(&prompt[start..i], color);
    }

    buffers::with_line_buf(|buf| {
        shell_write(&buf[..len]);
    });

    // Mid-content offsets use committed-wrap coordinates (a row-boundary offset
    // sits at column 0 of the next row); the end-of-content offset uses the
    // deferred resting position (last column of the full row). All geometry is
    // in display cells, not bytes.
    let total = prompt.len() + cells_upto(len);
    let end_row = row_of_offset(total, cols);
    let offset = prompt.len() + cells_upto(cursor_pos);
    let (target_row, target_col) = if offset == total {
        let col = if total == 0 {
            0
        } else if total % cols == 0 {
            cols - 1
        } else {
            total % cols
        };
        (end_row, col)
    } else {
        (offset / cols, offset % cols)
    };

    shell_write(b"\r");
    emit_cursor_move(end_row, b'A');
    emit_cursor_move(target_row, b'B');
    emit_cursor_move(target_col, b'C');
    *cur_row = target_row;
}

/// Emit a `\x1b[<n><dir>` relative cursor move (`A` up, `B` down,
/// `C` forward); zero distance emits nothing.
fn emit_cursor_move(n: usize, dir: u8) {
    if n == 0 {
        return;
    }
    let mut seq = [0u8; 16];
    seq[0] = ESC;
    seq[1] = b'[';
    let mut pos = 2usize;
    pos += write_usize_decimal(&mut seq[pos..], n);
    seq[pos] = dir;
    shell_write(&seq[..pos + 1]);
}

fn write_usize_decimal(buf: &mut [u8], mut value: usize) -> usize {
    if value == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut digits = [0u8; 20];
    let mut count = 0usize;
    while value > 0 {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }
    for i in 0..count {
        buf[i] = digits[count - 1 - i];
    }
    count
}
