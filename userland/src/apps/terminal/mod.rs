//! Terminal emulator — a standalone compositor client that owns a PTY master,
//! spawns `/bin/shell` on the slave, and bridges keystrokes <-> shell output
//! through the kernel line discipline.
//!
//! ```text
//! compositor <-> [terminal] <-> PTY master
//!                               | kernel ldisc (ISIG -> SIGINT -> fg pgrp)
//!                               <-> PTY slave <-> [shell] <-> forked jobs
//! ```
//!
//! Single-threaded `slopfut` `block_on` root: `ProtocolHandle` is `Rc`/`!Send`,
//! so the protocol client and the master fd are never touched off-thread.

pub mod grid;
pub mod input;
mod render;
mod surface;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use slopos_abi::task::TaskPriority;

use crate::ring::{Ring, slopfut};
use crate::syscall::{ShmBuffer, fs, process, tty};

use grid::TerminalGrid;
use input::{CompositorEvent, KeyAction, PointerState, Selection};

const WINDOW_WIDTH: i32 = 640;
const WINDOW_HEIGHT: i32 = 480;
const BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// Read buffer drained from the PTY master per loop turn.
const MASTER_READ_CHUNK: usize = 1024;
/// Output bytes to consume per loop turn before returning to compositor input.
const MASTER_READ_BUDGET: usize = 4 * 1024;
/// Bounded terminal-local input backlog for nonblocking PTY-master writes.
const MASTER_WRITE_QUEUE_CAP: usize = 64 * 1024;
/// Queued input bytes to write per loop turn before returning to events/output.
const MASTER_WRITE_BUDGET: usize = 4 * 1024;

pub fn terminal_user_main() {
    use slopos_windowing::connection;

    // The terminal's stderr is not wired to anything a developer can see;
    // route panic payloads to the kernel serial console instead.
    std::panic::set_hook(Box::new(|info| {
        let _ = tty::write(b"terminal: PANIC: ");
        let msg = std::format!("{info}\n");
        let _ = tty::write(msg.as_bytes());
    }));

    let handle = connection::connect().expect("compositor not running");
    surface::init_handle(handle.clone());

    if !surface::init(WINDOW_WIDTH, WINDOW_HEIGHT) {
        panic!("terminal: surface init failed");
    }
    surface::set_title("Terminal");
    surface::set_app_id("org.slopos.terminal");
    // The I-beam cursor is set on each pointer-enter (see the event loop),
    // where a live focus serial is available to authorize the request.

    // Open the PTY pair; the master arrives as a real owned fd.
    let (master_owned, _slave_num) = match process::openpty() {
        Ok(pair) => pair,
        Err(_) => {
            let _ = tty::write(b"terminal: openpty failed\n");
            return;
        }
    };
    let master_fd = master_owned.into_raw();

    // Resolve the slave peer fd from the master (TIOCGPTPEER).
    let slave_fd = match fs::ioctl_tiocgptpeer(master_fd) {
        Ok(fd) => fd.into_raw(),
        Err(_) => {
            let _ = tty::write(b"terminal: tiocgptpeer failed\n");
            let _ = fs::close_fd_raw(master_fd);
            return;
        }
    };

    // Set the initial winsize from the default geometry so the shell sees a
    // sane TERM size immediately (a Configure later re-pushes the real one).
    let cw = crate::gfx::font::cell_width().max(1);
    let ch = crate::gfx::font::cell_height().max(1);
    let cols = (WINDOW_WIDTH / cw).clamp(1, grid::MAX_COLS as i32) as u16;
    let rows = (WINDOW_HEIGHT / ch).clamp(1, grid::MAX_ROWS as i32) as u16;
    push_winsize(master_fd, rows, cols);

    // Spawn the shell with the slave cloned onto its stdin/stdout/stderr. The
    // child's empty table inherits only those clones — the terminal's own
    // master/slave descriptors are never in the child, so no cloexec juggling
    // is needed. No TASK_FLAG_NEW_PGRP: the shell runs its own setsid()/TIOCSCTTY.
    spawn_shell_on_slave(slave_fd);

    // The terminal no longer needs the slave fd; the shell holds its own
    // clone. Closing this ref keeps the master's EOF / SIGHUP semantics
    // correct when the shell finally exits.
    let _ = fs::close_fd_raw(slave_fd);

    // Master must be non-blocking so the drain loop can read-until-WouldBlock.
    let _ = fs::set_fd_nonblocking(master_fd);

    let compositor_fd = with_client_fd(&handle);

    let mut grid = TerminalGrid::new(rows, cols);

    let result = match Ring::setup(32) {
        Ok(ring) => slopfut::block_on(
            ring,
            event_loop(master_fd, compositor_fd, &handle, &mut grid),
        ),
        Err(_) => {
            let _ = tty::write(b"terminal: ring setup failed\n");
            ExitReason::Error
        }
    };

    let _ = fs::close_fd_raw(master_fd);
    match result {
        ExitReason::ShellGone => {
            let _ = tty::write(b"terminal: shell session ended\n");
        }
        ExitReason::Closed => {
            let _ = tty::write(b"terminal: window closed\n");
        }
        ExitReason::Error => {
            let _ = tty::write(b"terminal: fatal loop error\n");
        }
    }
    std::process::exit(0);
}

enum ExitReason {
    /// Master hit EOF / hangup — the shell session ended.
    ShellGone,
    /// Compositor asked the window to close.
    Closed,
    /// A fatal setup error inside the loop.
    Error,
}

/// Current font cell metrics `(width, height)` in pixels, each at least 1.
/// The terminal core is font-agnostic, so the app reads its glyph-atlas
/// metrics here and passes them into the pure selection geometry.
fn cell_metrics() -> (i32, i32) {
    (
        crate::gfx::font::cell_width().max(1),
        crate::gfx::font::cell_height().max(1),
    )
}

/// Push a TIOCSWINSZ to the master so the kernel SIGWINCHes the slave fg pgrp.
fn push_winsize(master_fd: i32, rows: u16, cols: u16) {
    let ws = slopos_abi::syscall::UserWinsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let _ = fs::tiocswinsz(master_fd, &ws);
}

/// Borrow the compositor socket fd from the protocol handle (stable for the
/// process lifetime).
fn with_client_fd(handle: &slopos_windowing::ProtocolHandle) -> i32 {
    handle.borrow_client().fd()
}

/// Spawn `/bin/shell` with the PTY slave cloned onto its stdin/stdout/stderr.
///
/// The child starts with an empty fd table, so the three `CloneFd` actions
/// install exactly the slave — the terminal's own descriptors are never
/// inherited. The shell performs its own `setsid()` + `TIOCSCTTY`, so we
/// deliberately omit `TASK_FLAG_NEW_PGRP`.
fn spawn_shell_on_slave(slave_fd: i32) {
    let actions = [
        process::clone_fd(slave_fd, 0),
        process::clone_fd(slave_fd, 1),
        process::clone_fd(slave_fd, 2),
    ];
    let tid = process::spawn_path_with_actions(
        b"/bin/shell",
        &[],
        TaskPriority::Normal,
        slopos_abi::task::TASK_FLAG_USER_MODE,
        &actions,
        0,
    );
    if tid <= 0 {
        let _ = tty::write(b"terminal: failed to spawn shell\n");
    }
}

/// The `block_on` root: select over compositor readiness, master readiness,
/// and a cursor-blink timer; on each wake drain both fds, render if dirty.
async fn event_loop(
    master_fd: i32,
    compositor_fd: i32,
    handle: &slopos_windowing::ProtocolHandle,
    grid: &mut TerminalGrid,
) -> ExitReason {
    use slopos_abi::syscall::POLLIN;

    let mut selection = Selection::NONE;
    let mut ptr = PointerState::new();
    let mut mods: u8 = 0;
    let mut cursor_on = true;
    let mut last_blink = Instant::now();
    let mut pending_writes = MasterWriteQueue::new();
    // Destination memfd handed to the compositor between a PasteReady and its
    // PasteResult (the receiver-provides-the-buffer paste handshake).
    let mut pending_paste: Option<ShmBuffer> = None;

    // Initial paint.
    render::render(grid, &selection, cursor_on);

    loop {
        // --- Drain all pending compositor events synchronously. ---
        let mut want_render = false;
        loop {
            let polled = handle.borrow_client().poll_event();
            let evt = match polled {
                Ok(Some(evt)) => evt,
                _ => break,
            };
            // The terminal polls the socket directly, so it routes buffer
            // releases to its renderer itself; otherwise both buffers stay in
            // flight and the surface falls back to single-buffer updates.
            if let slopos_protocol::types::Event::BufferRelease { buffer_id, .. } = &evt {
                surface::release_buffer(*buffer_id);
                continue;
            }
            match input::classify(&evt) {
                CompositorEvent::Key(ascii, scancode, codepoint) => {
                    match input::encode_key(ascii, scancode, codepoint, mods) {
                        KeyAction::ToMaster(bytes) => {
                            let action = bytes.as_bytes().to_vec();
                            if is_priority_keyboard_control(bytes.as_bytes()) {
                                if !pending_writes.write_priority_action(master_fd, action) {
                                    // Drop the whole control action rather than truncating it.
                                }
                            } else if !pending_writes.enqueue_action_back(action) {
                                // Drop the whole key action rather than truncating it.
                            }
                            // Any keypress cancels a scrollback view.
                            if grid.viewing_history() {
                                grid.scroll_view_down(usize::MAX);
                                want_render = true;
                            }
                        }
                        KeyAction::ScrollUp(n) => {
                            grid.scroll_view_up(n);
                            want_render = true;
                        }
                        KeyAction::ScrollDown(n) => {
                            grid.scroll_view_down(n);
                            want_render = true;
                        }
                        KeyAction::CopySelection => {
                            // Keyboard copy keeps the selection highlighted
                            // and never touches the master (no SIGINT).
                            if selection.is_active() {
                                copy_selection(handle, grid, &selection);
                            }
                        }
                        KeyAction::RequestPaste => {
                            // The compositor replies with PasteResult, which
                            // the Paste arm below writes to the master.
                            let _ = handle.borrow_client().clipboard_paste();
                        }
                        KeyAction::None => {}
                    }
                }
                CompositorEvent::Modifiers(m) => {
                    mods = m;
                }
                CompositorEvent::Resize(w, h) => {
                    if surface::resize(w as u32, h as u32) {
                        let cw = crate::gfx::font::cell_width().max(1);
                        let ch = crate::gfx::font::cell_height().max(1);
                        let cols = (w / cw).clamp(1, grid::MAX_COLS as i32) as u16;
                        let rows = (h / ch).clamp(1, grid::MAX_ROWS as i32) as u16;
                        // A width change reflows scrollback (rejoining wrapped
                        // lines and re-wrapping at the new width); pass the
                        // selection endpoints so the grid remaps them through
                        // the reflow and a copy survives the resize. A height
                        // change leaves the absolute line numbering — and so the
                        // selection — valid, untouched.
                        let mut pts = selection.endpoints();
                        let outcome = grid.resize(rows, cols, &mut pts);
                        if outcome.reflowed && selection.is_active() {
                            match (pts[0], pts[1]) {
                                (Some(a), Some(h)) => selection.set_endpoints(a, h),
                                // An endpoint's content was evicted from the
                                // re-wrapped history: clear rather than copy a
                                // stale range.
                                _ => selection.clear(),
                            }
                        }
                        push_winsize(master_fd, rows, cols);
                        want_render = true;
                    }
                }
                CompositorEvent::Close => {
                    return ExitReason::Closed;
                }
                CompositorEvent::PointerMotion(x, y) | CompositorEvent::PointerEnter(x, y) => {
                    // Set the I-beam on enter, when the focus serial that
                    // authorizes the cursor request is current.
                    if matches!(evt, slopos_protocol::types::Event::PointerEnter { .. }) {
                        surface::set_cursor_shape(slopos_abi::CURSOR_SHAPE_TEXT);
                    }
                    ptr.last_x = x;
                    ptr.last_y = y;
                    ptr.has_focus = true;
                    let (cw, ch) = cell_metrics();
                    if input::update_selection(&mut ptr, &mut selection, grid, cw, ch) {
                        want_render = true;
                    }
                }
                CompositorEvent::PointerLeave => {
                    ptr.has_focus = false;
                }
                CompositorEvent::Scroll(value_v120) => {
                    let lines = input::wheel_scroll_lines(value_v120);
                    if lines < 0 {
                        grid.scroll_view_up((-lines) as usize);
                        want_render = true;
                    } else if lines > 0 {
                        grid.scroll_view_down(lines as usize);
                        want_render = true;
                    }
                }
                CompositorEvent::PointerButton { pressed, code } => {
                    if pressed {
                        ptr.button_state |= code;
                    } else {
                        ptr.button_state &= !code;
                    }
                    let (cw, ch) = cell_metrics();
                    if input::update_selection(&mut ptr, &mut selection, grid, cw, ch) {
                        want_render = true;
                    }
                    // On release with a non-collapsed selection, copy it to
                    // the compositor clipboard (a bare click left it inactive).
                    if !pressed && selection.is_active() {
                        copy_selection(handle, grid, &selection);
                    }
                }
                CompositorEvent::PasteReady(len) => {
                    // The compositor told us the clipboard size; hand it a
                    // destination memfd of that size to copy into. Drop any
                    // stale pending buffer first.
                    pending_paste = None;
                    if len > 0 {
                        if let Ok(dst) = ShmBuffer::create(len as usize) {
                            if handle.borrow_client().clipboard_read(dst.fd(), len).is_ok() {
                                pending_paste = Some(dst);
                            }
                        }
                    }
                }
                CompositorEvent::PasteResult(len) => {
                    // The destination memfd now holds `len` clipboard bytes.
                    if let Some(dst) = pending_paste.take() {
                        let n = (len as usize).min(dst.size());
                        write_paste(
                            master_fd,
                            &mut pending_writes,
                            &dst.as_slice()[..n],
                            grid.bracketed_paste(),
                        );
                    }
                }
                CompositorEvent::Ignored => {}
            }
        }

        pending_writes.drain(master_fd, MASTER_WRITE_BUDGET);

        // --- Drain the PTY master (read until WouldBlock). ---
        match drain_master(master_fd, grid) {
            MasterDrain::Eof => return ExitReason::ShellGone,
            MasterDrain::Data => want_render = true,
            MasterDrain::Idle => {}
        }

        if grid.take_dirty() || want_render {
            render::render(grid, &selection, cursor_on);
        }

        // --- Sleep until the next wake: compositor data, master data, or the
        // next cursor-blink flip. ---
        let elapsed = last_blink.elapsed();
        if elapsed >= BLINK_INTERVAL {
            cursor_on = !cursor_on;
            last_blink = Instant::now();
            render::render(grid, &selection, cursor_on);
            continue;
        }
        let blink_ms = (BLINK_INTERVAL - elapsed).as_millis().max(1) as u64;
        let comp = slopfut::poll_add(compositor_fd, POLLIN);
        let master_mask = if pending_writes.is_empty() {
            POLLIN
        } else {
            POLLIN | slopos_abi::syscall::POLLOUT
        };
        let mst = slopfut::poll_add(master_fd, master_mask);
        let blink = Box::pin(slopfut::time::sleep_ms(blink_ms));
        let _ = slopfut::select3(comp, mst, blink).await;
    }
}

struct QueuedWrite {
    bytes: Vec<u8>,
    written: usize,
}

struct MasterWriteQueue {
    actions: VecDeque<QueuedWrite>,
    queued_bytes: usize,
}

impl MasterWriteQueue {
    fn new() -> Self {
        Self {
            actions: VecDeque::new(),
            queued_bytes: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.queued_bytes == 0
    }

    fn remaining_capacity(&self) -> usize {
        MASTER_WRITE_QUEUE_CAP.saturating_sub(self.queued_bytes)
    }

    fn enqueue_action_back(&mut self, data: Vec<u8>) -> bool {
        self.enqueue_action(data, false, 0)
    }

    fn enqueue_action_front(&mut self, data: Vec<u8>, written: usize) -> bool {
        self.enqueue_action(data, true, written)
    }

    fn enqueue_action(&mut self, data: Vec<u8>, front: bool, written: usize) -> bool {
        if written > data.len() {
            return false;
        }
        let remaining = data.len() - written;
        if remaining == 0 {
            return true;
        }
        if remaining > self.remaining_capacity() {
            return false;
        }
        let action = QueuedWrite {
            bytes: data,
            written,
        };
        if front {
            self.actions.push_front(action);
        } else {
            self.actions.push_back(action);
        }
        self.queued_bytes += remaining;
        true
    }

    fn write_priority_action(&mut self, fd: i32, data: Vec<u8>) -> bool {
        if data.len() > self.remaining_capacity() {
            return false;
        }
        if !self.is_empty() {
            return self.enqueue_action_front(data, 0);
        }
        match fs::write_slice(fd, &data) {
            Ok(n) if n == data.len() => true,
            Ok(n) => self.enqueue_action_front(data, n),
            Err(e) if e == crate::syscall::SyscallError::EAGAIN => {
                self.enqueue_action_front(data, 0)
            }
            Err(e) => {
                let msg = std::format!("terminal: priority write error {e:?}\n");
                let _ = tty::write(msg.as_bytes());
                self.enqueue_action_front(data, 0)
            }
        }
    }

    fn drain(&mut self, fd: i32, budget: usize) {
        let mut written_total = 0usize;
        loop {
            let Some(action) = self.actions.front_mut() else {
                break;
            };
            let limit = budget.saturating_sub(written_total);
            if limit == 0 {
                break;
            }
            let remaining = &action.bytes[action.written..];
            let chunk_len = remaining.len().min(limit);
            match fs::write_slice(fd, &remaining[..chunk_len]) {
                Ok(0) => break,
                Ok(n) => {
                    written_total += n;
                    action.written += n;
                    self.queued_bytes = self.queued_bytes.saturating_sub(n);
                    if action.written == action.bytes.len() {
                        let _ = self.actions.pop_front();
                    }
                }
                Err(e) if e == crate::syscall::SyscallError::EAGAIN => break,
                Err(e) => {
                    let msg = std::format!("terminal: queued write error {e:?}\n");
                    let _ = tty::write(msg.as_bytes());
                    self.actions.clear();
                    self.queued_bytes = 0;
                    break;
                }
            }
        }
    }
}

fn is_priority_keyboard_control(bytes: &[u8]) -> bool {
    matches!(bytes, [0x03] | [0x1C] | [0x1A] | [0x11] | [0x13])
}

enum MasterDrain {
    Data,
    Idle,
    Eof,
}

/// Read from the master until `WouldBlock`, feeding bytes to the interpreter.
/// Every byte read is mirrored via `tty::write` (SYSCALL_WRITE -> kernel
/// console) so `just boot-log` keeps showing shell output.
fn drain_master(master_fd: i32, grid: &mut TerminalGrid) -> MasterDrain {
    let mut buf = [0u8; MASTER_READ_CHUNK];
    let mut got_any = false;
    let mut total = 0usize;
    loop {
        match fs::read_slice(master_fd, &mut buf) {
            Ok(0) => {
                // EOF / hangup: shell session gone.
                return MasterDrain::Eof;
            }
            Ok(n) => {
                got_any = true;
                total += n;
                // Serial-debug mirror (single call site).
                let _ = tty::write(&buf[..n]);
                for &b in &buf[..n] {
                    grid.process_byte(b);
                }
                if total >= MASTER_READ_BUDGET {
                    return MasterDrain::Data;
                }
                if n < buf.len() {
                    // Short read: likely drained. Loop once more to confirm
                    // WouldBlock so we never leave bytes buffered.
                    continue;
                }
            }
            Err(e) if e == crate::syscall::SyscallError::EAGAIN => {
                return if got_any {
                    MasterDrain::Data
                } else {
                    MasterDrain::Idle
                };
            }
            Err(e) => {
                // Any other error (EBADF after close, EIO) ends the session.
                let msg = std::format!("terminal: master read error {e:?}\n");
                let _ = tty::write(msg.as_bytes());
                return MasterDrain::Eof;
            }
        }
    }
}

/// Hard ceiling on a single clipboard copy (16 MiB), matching the compositor.
const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

/// Copy the current selection to the compositor clipboard via a memfd. The
/// full selection is captured (no truncation): a buffer sized to the selection
/// is filled by `collect_selection`, then its fd is handed to the compositor.
fn copy_selection(
    handle: &slopos_windowing::ProtocolHandle,
    grid: &TerminalGrid,
    selection: &Selection,
) {
    let bound = input::selection_byte_bound(grid, selection).min(MAX_CLIPBOARD_BYTES);
    if bound == 0 {
        return;
    }
    let Ok(mut shm) = ShmBuffer::create(bound) else {
        return;
    };
    let n = input::collect_selection(grid, selection, shm.as_mut_slice());
    if n > 0 {
        // The compositor dups the fd via SCM_RIGHTS, so dropping `shm` after
        // the send leaves the clipboard backing alive on the compositor side.
        let _ = handle.borrow_client().clipboard_copy(shm.fd(), n as u32);
    }
}

/// Write pasted content to the master, wrapped in bracketed-paste markers
/// only when the slave-side application enabled DECSET 2004 (the shell's
/// line editor does; a raw `cat` must not see the markers). The payload is
/// sanitized first — no control byte survives, so the bracket cannot be
/// escaped and a clipboard can never type Ctrl+C (see `sanitize_paste`).
fn write_paste(
    master_fd: i32,
    pending_writes: &mut MasterWriteQueue,
    data: &[u8],
    bracketed: bool,
) {
    if data.is_empty() {
        return;
    }
    // Sanitizing never grows the payload (it only drops/normalizes bytes).
    let mut clean = std::vec![0u8; data.len()];
    let n = input::sanitize_paste(data, &mut clean);
    if n == 0 {
        return;
    }
    let markers_len = if bracketed {
        b"\x1b[200~".len() + b"\x1b[201~".len()
    } else {
        0
    };
    let action_len = n.saturating_add(markers_len);
    if action_len == 0 || action_len > pending_writes.remaining_capacity() {
        return;
    }
    let mut action = Vec::with_capacity(action_len);
    if bracketed {
        action.extend_from_slice(b"\x1b[200~");
    }
    action.extend_from_slice(&clean[..n]);
    if bracketed {
        action.extend_from_slice(b"\x1b[201~");
    }
    if !pending_writes.enqueue_action_back(action) {
        // Drop the whole paste action rather than truncating it.
    }
    pending_writes.drain(master_fd, MASTER_WRITE_BUDGET);
}
