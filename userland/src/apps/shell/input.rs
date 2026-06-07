//! Raw fd0 line editor for the PTY-slave shell.
//!
//! The shell reads bytes from fd 0 (the PTY slave) on the slopfut ring,
//! parses VT100/xterm escape sequences incrementally (with partial state
//! carried across reads), and decodes them into internal key codes so the
//! editor's `match` dispatch is the same regardless of whether a key arrived
//! as a literal byte or a CSI sequence.  All redraw output is ANSI to fd 1.

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

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

// Internal key codes — the dispatch `match` keys.  Decoded from either a raw
// control byte or a parsed escape sequence.  These values are outside the
// printable ASCII range so they never collide with literal input bytes.
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
const READ_CHUNK: usize = 256;

/// Default editor width when `tiocgwinsz(0)` fails (no terminal wired).
const DEFAULT_COLS: usize = 80;

/// Milliseconds to wait disambiguating a bare ESC from the start of a CSI/SS3
/// escape sequence.  A real escape sequence's bytes arrive back-to-back; a
/// lone ESC keypress sees no follow-up byte within this window.
const ESC_TIMEOUT_MS: u64 = 30;

static PROMPT_COLORS: Mutex<[u8; super::PROMPT_BUF_MAX]> = Mutex::new([0; super::PROMPT_BUF_MAX]);
static PROMPT_COLORS_LEN: AtomicUsize = AtomicUsize::new(0);

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

    // Editor terminal width, queried fresh each prompt and re-queried live
    // whenever SIGWINCH fires mid-edit (see the resize listener below).
    let cols = query_cols();

    // Resize listener: SIGWINCH arrives as an in-band ring event raced
    // against stdin, so the editor re-wraps mid-edit instead of keeping
    // stale geometry until the next prompt. Dropping it (end of this call)
    // restores the signal mask before any child command runs.
    let winch = SignalListener::new(sig_bit(SIGWINCH));

    // Set the slave TTY to raw mode: the shell does its own rendering / line
    // editing.  Mirrors bash/zsh's cfmakeraw() on real Linux.
    let saved_termios = fs::tcgetattr(STDIN_FD).ok();
    if let Some(ref t) = saved_termios {
        let mut raw = *t;
        raw.c_lflag &=
            !(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG | LocalFlags::ECHOE);
        let _ = fs::tcsetattr(STDIN_FD, &raw);
    }

    // Enable bracketed paste so pasted text arrives wrapped in \x1b[200~..201~
    // and is inserted literally rather than interpreted as commands.
    let _ = fs::write_slice(1, b"\x1b[?2004h");

    let result = match Ring::setup(16) {
        Ok(ring) => slopfut::block_on(ring, input_loop(tokens, prompt, cols, winch.as_ref())),
        Err(_) => {
            // Ring unavailable: cannot run the async editor.  Yield once and
            // re-prompt (return 0 -> caller `continue`s) rather than wedging.
            crate::syscall::core::yield_now();
            0
        }
    };

    // Restore canonical mode FIRST (ISIG back on), THEN write the
    // bracketed-paste-off sequence: the write can block on a full output
    // queue, and a blocking write in raw mode (ISIG off) cannot be
    // interrupted from the keyboard — the one ordering that can wedge the
    // shell unkillably.
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

// ---------------------------------------------------------------------------
// Incremental escape-sequence parser
// ---------------------------------------------------------------------------

/// One decoded input event from the byte stream.
enum Decoded {
    /// A printable / control byte or a decoded internal key code.
    Key(u8),
    /// A run of literal bytes from a bracketed paste (control chars already
    /// stripped except `\t`).
    Paste(Vec<u8>),
}

/// State machine that consumes the fd0 byte stream and emits [`Decoded`]
/// events.  Partial escape sequences are retained across `feed` calls so a
/// sequence split across two reads parses correctly.
struct EscParser {
    /// Bytes of an in-progress escape sequence (starts with ESC).
    pending: Vec<u8>,
    /// True while inside a bracketed-paste run (`\x1b[200~` seen, `201~`
    /// not yet).
    in_paste: bool,
    /// Accumulated literal paste bytes.
    paste_buf: Vec<u8>,
}

impl EscParser {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            in_paste: false,
            paste_buf: Vec::new(),
        }
    }

    /// True when a bare ESC is buffered with no follow-up yet — the caller
    /// must disambiguate against a short timeout.
    fn awaiting_escape(&self) -> bool {
        !self.pending.is_empty() && !self.in_paste
    }

    /// Resolve a buffered bare ESC (timeout elapsed with no follow-up byte):
    /// emit it as a literal ESC key.
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
        if self.pending.is_empty() {
            if b == ESC {
                self.pending.push(b);
            } else {
                out.push(Decoded::Key(b));
            }
            return;
        }
        // Inside an escape sequence.
        self.pending.push(b);
        self.try_complete(out);
    }

    fn feed_paste_byte(&mut self, b: u8, out: &mut Vec<Decoded>) {
        // The paste terminator is the literal sequence \x1b[201~.  Buffer
        // bytes after ESC in `pending` to detect it; everything else is
        // literal paste content (control chars stripped except \t).
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
                // Not the terminator: the ESC and trailing bytes are literal
                // paste content.  Strip control bytes except \t.
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
        if b == b'\t' || (0x20..=0x7e).contains(&b) {
            self.paste_buf.push(b);
        }
    }

    /// Attempt to recognise a complete escape sequence in `pending`.  On
    /// success, push the decoded key (or enter paste mode) and clear
    /// `pending`.  On a still-incomplete prefix, leave `pending` for the next
    /// byte.  On an unrecognised sequence, drop it.
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
            EscMatch::Partial => {} // wait for more bytes
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

/// True if `s` is a prefix of the bracketed-paste end sequence `\x1b[201~`.
fn could_be_paste_end(s: &[u8]) -> bool {
    let end = b"\x1b[201~";
    s.len() <= end.len() && end.starts_with(s)
}

fn is_paste_end(s: &[u8]) -> bool {
    s == b"\x1b[201~"
}

/// Decode a buffered escape sequence into an internal key code.  Mirrors the
/// terminal emulator's encoder: CSI arrows/Home/End and the `~`-terminated
/// editing keys, plus SS3 arrows (`\x1bO…`) for completeness.
fn decode_escape(p: &[u8]) -> EscMatch {
    if p.len() == 1 {
        // Only ESC so far.
        return EscMatch::Partial;
    }
    match p[1] {
        b'[' | b'O' => {}
        _ => return EscMatch::Invalid,
    }
    if p.len() == 2 {
        return EscMatch::Partial;
    }
    // CSI/SS3 final byte forms (no parameters): \x1b[A etc.
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
    // Parameterised `\x1b[<n>~` editing keys.
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

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

/// A long-lived `SignalListener::recv` future, re-armed only after it
/// resolves. Holding it across `next_decoded` calls (instead of building a
/// fresh one per wait) means losing a select race never cancels it — a
/// completed-but-unobserved resize is reported on the next wait instead of
/// being discarded with the dropped future.
type WinchFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = u32> + 'a>>;

async fn input_loop(
    tokens: &mut ParsedTokens,
    prompt: &[u8],
    cols: usize,
    winch: Option<&SignalListener>,
) -> i32 {
    let mut cols = cols;
    let mut parser = EscParser::new();
    let mut winch_fut: Option<WinchFuture<'_>> =
        winch.map(|w| -> WinchFuture<'_> { Box::pin(w.recv()) });
    // Decoded events not yet dispatched (one read can yield several).
    let mut queue: std::collections::VecDeque<Decoded> = std::collections::VecDeque::new();

    // Region-relative row the cursor currently sits on (see `redraw`). The
    // REPL pre-printed the prompt, so the first region starts at its first
    // row with the cursor resting after it.
    let mut cur_row = row_of_offset(prompt.len(), cols.max(1));

    'restart: loop {
        let mut len = 0usize;
        let mut cursor_pos = 0usize;

        redraw(prompt, len, cursor_pos, cols, &mut cur_row);

        loop {
            let decoded = match queue.pop_front() {
                Some(d) => d,
                None => {
                    if let Wake::Winch =
                        next_decoded(&mut parser, &mut queue, winch_fut.as_mut()).await
                    {
                        // The resolved listener is spent: re-arm it before
                        // anything else awaits, then refresh the width and
                        // re-wrap the edit region under the new geometry.
                        if let (Some(slot), Some(w)) = (winch_fut.as_mut(), winch) {
                            *slot = Box::pin(w.recv());
                        }
                        cols = query_cols();
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                        continue;
                    }
                    match queue.pop_front() {
                        Some(d) => d,
                        None => continue,
                    }
                }
            };

            let c = match decoded {
                Decoded::Paste(text) => {
                    insert_text(&text, text.len(), &mut len, &mut cursor_pos);
                    redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    continue;
                }
                Decoded::Key(c) => c,
            };

            match c {
                b'\n' | b'\r' => {
                    // Finish below the LAST wrapped row of the input, not
                    // wherever the cursor happens to sit mid-region.
                    let end_row = row_of_offset(prompt.len() + len, cols.max(1));
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
                        cursor_pos -= 1;
                        redraw(prompt, len, cursor_pos, cols, &mut cur_row);
                    }
                }

                KEY_RIGHT => {
                    if cursor_pos < len {
                        cursor_pos += 1;
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

                // Page Up / Page Down: the terminal owns scrollback now, so
                // these are no-ops at the editor level.
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
                    // Re-run the editor with a fresh blank line on a cleared
                    // screen (the async fn restarts; it cannot tail-recurse).
                    continue 'restart;
                }

                CTRL_C => {
                    let end_row = row_of_offset(prompt.len() + len, cols.max(1));
                    emit_cursor_move(end_row.saturating_sub(cur_row), b'B');
                    shell_write(b"^C\n");
                    history::reset_cursor();
                    return 0;
                }

                CTRL_D => {
                    if len == 0 {
                        return -1;
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
                        let end_row = row_of_offset(prompt.len() + len, cols.max(1));
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
                        // Re-emit prompt + buffer on a fresh line and keep
                        // editing the current buffer.
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

        // The inner loop broke on Enter: assemble the line, parse it, and
        // return the token count.
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

/// Why `next_decoded` returned: stdin made progress (bytes decoded, ESC
/// flushed, or a transient state the caller re-arms on), or the terminal
/// was resized and the caller must re-query its width.
enum Wake {
    Input,
    Winch,
}

/// Editor terminal width. Falls back to 80 columns when no terminal is
/// wired (`tiocgwinsz` fails or reports a zero width).
fn query_cols() -> usize {
    match fs::tiocgwinsz(STDIN_FD) {
        Ok(ws) if ws.ws_col != 0 => ws.ws_col as usize,
        _ => DEFAULT_COLS,
    }
}

/// Resolve a buffered lone ESC as a literal keypress (the disambiguation
/// timer expired with no follow-up byte).
fn flush_pending_escape(parser: &mut EscParser, queue: &mut std::collections::VecDeque<Decoded>) {
    let mut out = Vec::new();
    parser.flush_escape(&mut out);
    for d in out {
        queue.push_back(d);
    }
}

/// Read the next batch of decoded events into `queue`, blocking on fd 0.
///
/// A bare ESC buffered in the parser is disambiguated with a short timeout: if
/// no follow-up byte arrives, it resolves as a literal ESC keypress.  When a
/// resize listener is wired, it is raced as the FIRST select arm against
/// non-consuming waits only (fd readiness / timer), and reported as
/// [`Wake::Winch`]:
///
/// - first arm: a resize landing in the same reactor batch as input
///   readiness resolves the select as Winch instead of being the dropped
///   loser whose already-drained signal would be discarded with it;
/// - persistent future (see [`WinchFuture`]) + non-consuming peers: losing
///   an arm never cancels in-flight state that already consumed bytes or a
///   signal, so neither input nor resizes can be lost to a select race.
async fn next_decoded(
    parser: &mut EscParser,
    queue: &mut std::collections::VecDeque<Decoded>,
    winch: Option<&mut WinchFuture<'_>>,
) -> Wake {
    let buf = vec![0u8; READ_CHUNK];

    let result = if parser.awaiting_escape() {
        // ESC pending: wait for follow-up readiness against a short timeout.
        // If the timer wins, the ESC was a lone keypress.
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
        // No pending escape: park on fd0 readiness, then read.
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
        // Read of zero bytes = the PTY master hung up (the terminal closed
        // it).  End of input: exit cleanly rather than busy-looping.  A
        // deliberate Ctrl-D keypress arrives as the byte 0x04 in the stream
        // and is handled by the editor's CTRL_D arm, not this path.
        let _ = crate::syscall::tty::write(b"shell: stdin EOF, exiting\n");
        std::process::exit(0);
    }
    if r.res < 0 {
        // Transient read error: leave the queue empty so the caller re-arms.
        return Wake::Input;
    }
    let n = r.res as usize;
    let mut out = Vec::new();
    parser.feed(&r.buf[..n.min(r.buf.len())], &mut out);
    for d in out {
        queue.push_back(d);
    }
    Wake::Input
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

/// Cursor row within the edit region after printing `offset` cells, under
/// the terminal's deferred autowrap: an exactly-full row leaves the cursor
/// resting on that row (wrap pending), not on the next one.
fn row_of_offset(offset: usize, cols: usize) -> usize {
    if offset == 0 { 0 } else { (offset - 1) / cols }
}

/// Redraw the edit region in place. Inputs wider than the terminal wrap
/// across rows, so the redraw is region-based: move to the region's first
/// row, erase everything below, reprint prompt + buffer (the terminal
/// wraps), then reposition the cursor. `cur_row` tracks the row (within
/// the region) the cursor was left on by the previous redraw.
fn redraw(prompt: &[u8], len: usize, cursor_pos: usize, cols: usize, cur_row: &mut usize) {
    use super::display::shell_write_idx;
    let cols = cols.max(1);

    // Column 0 of the region's first row, then wipe the previous render —
    // including every wrapped row below.
    shell_write(b"\r");
    emit_cursor_move(*cur_row, b'A');
    shell_write(b"\x1b[J");

    // Colored prompt: emit per-color runs (same palette mapping as the
    // top-level prompt writer).
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

    // Buffer contents (default color).
    buffers::with_line_buf(|buf| {
        shell_write(&buf[..len]);
    });

    // Reposition: back to the region start, then down/right to the target.
    // Mid-content offsets use committed-wrap coordinates (a row-boundary
    // offset sits at column 0 of the next row); the end-of-content offset
    // uses the deferred resting position (last column of the full row).
    let total = prompt.len() + len;
    let end_row = row_of_offset(total, cols);
    let offset = prompt.len() + cursor_pos;
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
