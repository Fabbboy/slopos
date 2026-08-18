//! Terminal emulator — a standalone compositor client that owns a PTY master,
//! spawns `/bin/shell` on the slave, and bridges keystrokes <-> shell output
//! through the kernel line discipline.
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
use slopos_terminal_core::damage::{CellDamage, DamageHistory};

const WINDOW_WIDTH: i32 = 640;
const WINDOW_HEIGHT: i32 = 480;
/// Buffers `SoftSurface` cycles. The damage history is sized to match so a
/// recycled slot's age always resolves against a recorded frame.
const SURFACE_BUFFERS: usize = 2;
const BLINK_INTERVAL: Duration = Duration::from_millis(500);

const MASTER_READ_CHUNK: usize = 1024;
/// Output bytes per loop turn before returning to compositor input.
const MASTER_READ_BUDGET: usize = 4 * 1024;
const MASTER_WRITE_QUEUE_CAP: usize = 64 * 1024;
/// Queued input bytes per loop turn before returning to events/output.
const MASTER_WRITE_BUDGET: usize = 4 * 1024;

pub fn terminal_user_main() {
    use slopos_windowing::connection;

    // The terminal's stderr goes nowhere visible, so panics route to the
    // kernel serial console instead.
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

    let (master_owned, _slave_num) = match process::openpty() {
        Ok(pair) => pair,
        Err(_) => {
            let _ = tty::write(b"terminal: openpty failed\n");
            return;
        }
    };
    let master_fd = master_owned.into_raw();

    let slave_fd = match fs::ioctl_tiocgptpeer(master_fd) {
        Ok(fd) => fd.into_raw(),
        Err(_) => {
            let _ = tty::write(b"terminal: tiocgptpeer failed\n");
            let _ = fs::close_fd_raw(master_fd);
            return;
        }
    };

    // The shell needs a sane TERM size before the first Configure arrives.
    let cw = crate::gfx::font::cell_width().max(1);
    let ch = crate::gfx::font::cell_height().max(1);
    let cols = (WINDOW_WIDTH / cw).clamp(1, grid::MAX_COLS as i32) as u16;
    let rows = (WINDOW_HEIGHT / ch).clamp(1, grid::MAX_ROWS as i32) as u16;
    push_winsize(master_fd, rows, cols);

    spawn_shell_on_slave(slave_fd);

    // The shell holds its own clone; dropping this last terminal-side ref is
    // what makes the master's EOF/SIGHUP fire when the shell exits.
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
    /// Master hit EOF / hangup.
    ShellGone,
    Closed,
    Error,
}

/// Font cell `(width, height)` in pixels, each at least 1. The terminal core is
/// font-agnostic, so the app feeds these into the pure selection geometry.
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

/// The compositor socket fd is stable for the process lifetime.
fn with_client_fd(handle: &slopos_windowing::ProtocolHandle) -> i32 {
    handle.borrow_client().fd()
}

/// The child starts with an empty fd table, so the `CloneFd` actions install
/// exactly the slave and no cloexec juggling is needed. `TASK_FLAG_NEW_PGRP` is
/// deliberately omitted: the shell runs its own `setsid()` + `TIOCSCTTY`.
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

/// The `block_on` root: selects over compositor readiness, master readiness and
/// the cursor-blink timer.
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
    let mut history = DamageHistory::new(SURFACE_BUFFERS);
    // Reported but not yet presented. Only a present clears it, so a skipped
    // frame carries its damage forward rather than losing it.
    let mut pending = CellDamage::new();
    pending.set_rows(grid.rows as usize);
    let mut pending_writes = MasterWriteQueue::new();
    // Destination memfd handed to the compositor between a PasteReady and its
    // PasteResult: the receiver provides the buffer.
    let mut pending_paste: Option<ShmBuffer> = None;

    // The first frame has no buffer contents to build on.
    let _ = grid.take_damage();
    render_full(grid, &selection, cursor_on, &mut history, &mut pending);

    loop {
        // Set by app-owned state the grid cannot report damage for.
        let mut repaint_all = false;
        loop {
            let polled = handle.borrow_client().poll_event();
            let evt = match polled {
                Ok(Some(evt)) => evt,
                _ => break,
            };
            // The terminal polls the socket directly, so it must route buffer
            // releases itself or both buffers stay in flight and the surface
            // falls back to single-buffer updates.
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
                            }
                        }
                        KeyAction::ScrollUp(n) => {
                            grid.scroll_view_up(n);
                        }
                        KeyAction::ScrollDown(n) => {
                            grid.scroll_view_down(n);
                        }
                        KeyAction::CopySelection => {
                            // Keyboard copy never touches the master, so it
                            // cannot raise SIGINT.
                            if selection.is_active() {
                                copy_selection(handle, grid, &selection);
                            }
                        }
                        KeyAction::RequestPaste => {
                            // The compositor answers asynchronously; the paste
                            // arms below carry it to the master.
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
                        // A width change re-wraps scrollback, so the selection
                        // endpoints go through the reflow to stay valid. A
                        // height change leaves absolute line numbering intact.
                        let mut pts = selection.endpoints();
                        let outcome = grid.resize(rows, cols, &mut pts);
                        if outcome.reflowed && selection.is_active() {
                            match (pts[0], pts[1]) {
                                (Some(a), Some(h)) => selection.set_endpoints(a, h),
                                // An endpoint was evicted from the re-wrapped
                                // history; a stale range must not be copied.
                                _ => selection.clear(),
                            }
                        }
                        push_winsize(master_fd, rows, cols);
                        // New buffers: nothing recorded describes them.
                        history.clear();
                        pending.set_rows(grid.rows as usize);
                        let _ = grid.take_damage();
                        render_full(grid, &selection, cursor_on, &mut history, &mut pending);
                    }
                }
                CompositorEvent::Close => {
                    return ExitReason::Closed;
                }
                CompositorEvent::PointerMotion(x, y) | CompositorEvent::PointerEnter(x, y) => {
                    // Only on enter is the focus serial that authorizes a
                    // cursor request current.
                    if matches!(evt, slopos_protocol::types::Event::PointerEnter { .. }) {
                        surface::set_cursor_shape(slopos_abi::CURSOR_SHAPE_TEXT);
                    }
                    ptr.last_x = x;
                    ptr.last_y = y;
                    ptr.has_focus = true;
                    let (cw, ch) = cell_metrics();
                    if input::update_selection(&mut ptr, &mut selection, grid, cw, ch) {
                        repaint_all = true;
                    }
                }
                CompositorEvent::PointerLeave => {
                    ptr.has_focus = false;
                }
                CompositorEvent::Scroll(value_v120) => {
                    let lines = input::wheel_scroll_lines(value_v120);
                    if lines < 0 {
                        grid.scroll_view_up((-lines) as usize);
                    } else if lines > 0 {
                        grid.scroll_view_down(lines as usize);
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
                        repaint_all = true;
                    }
                    // A bare click leaves the selection inactive, so only a
                    // real drag reaches the clipboard here.
                    if !pressed && selection.is_active() {
                        copy_selection(handle, grid, &selection);
                    }
                }
                CompositorEvent::PasteReady(len) => {
                    // `len` is the clipboard size; hand back a destination
                    // memfd of exactly that size to copy into.
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

        // Output damages the cells it writes, so nothing extra is needed here.
        match drain_master(master_fd, grid) {
            MasterDrain::Eof => return ExitReason::ShellGone,
            MasterDrain::Data | MasterDrain::Idle => {}
        }

        if repaint_all {
            // Selection shading is app state, so the grid records no damage
            // for it and the whole view has to repaint.
            pending.add_all(grid.cols);
        }
        pending.union(&grid.take_damage());
        present_pending(grid, &selection, cursor_on, &mut history, &mut pending);

        let elapsed = last_blink.elapsed();
        if elapsed >= BLINK_INTERVAL {
            cursor_on = !cursor_on;
            last_blink = Instant::now();
            // A blink inverts one cell; damaging only it keeps the frame
            // proportional to what changed.
            grid.damage_cursor();
            pending.union(&grid.take_damage());
            present_pending(grid, &selection, cursor_on, &mut history, &mut pending);
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

/// For the first frame and after a resize, where the buffers hold nothing a
/// partial repaint could build on.
fn render_full(
    grid: &TerminalGrid,
    selection: &Selection,
    cursor_on: bool,
    history: &mut DamageHistory,
    pending: &mut CellDamage,
) {
    render::render_full(grid, selection, cursor_on);
    pending.clear();
    // Only one slot was fully painted; no recorded frame describes what the
    // others still hold.
    history.clear();
    let mut all = CellDamage::new();
    all.set_rows(grid.rows as usize);
    all.add_all(grid.cols);
    history.push(all);
}

/// `pending` is cleared only once the frame is actually committed, so damage
/// can never be forgotten without having been shown.
fn present_pending(
    grid: &TerminalGrid,
    selection: &Selection,
    cursor_on: bool,
    history: &mut DamageHistory,
    pending: &mut CellDamage,
) {
    if pending.is_empty() {
        return;
    }
    // The target slot holds an older frame, so it needs this frame's damage
    // plus everything painted since it was last presented.
    match history.resolve(surface::buffer_age(), pending) {
        Some(repaint) => {
            render::render_damage(grid, selection, cursor_on, &repaint);
            history.push(core::mem::replace(pending, {
                let mut d = CellDamage::new();
                d.set_rows(grid.rows as usize);
                d
            }));
        }
        None => render_full(grid, selection, cursor_on, history, pending),
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

/// Reads until `WouldBlock`. Every byte is mirrored to the kernel console so
/// `just boot-log` keeps showing shell output.
fn drain_master(master_fd: i32, grid: &mut TerminalGrid) -> MasterDrain {
    let mut buf = [0u8; MASTER_READ_CHUNK];
    let mut got_any = false;
    let mut total = 0usize;
    loop {
        match fs::read_slice(master_fd, &mut buf) {
            Ok(0) => {
                return MasterDrain::Eof;
            }
            Ok(n) => {
                got_any = true;
                total += n;
                let _ = tty::write(&buf[..n]);
                for &b in &buf[..n] {
                    grid.process_byte(b);
                }
                if total >= MASTER_READ_BUDGET {
                    return MasterDrain::Data;
                }
                if n < buf.len() {
                    // A short read still needs one more turn to confirm
                    // WouldBlock, so no bytes are left buffered.
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
                // EBADF after close, EIO: either way the session is over.
                let msg = std::format!("terminal: master read error {e:?}\n");
                let _ = tty::write(msg.as_bytes());
                return MasterDrain::Eof;
            }
        }
    }
}

/// Hard ceiling on a single clipboard copy (16 MiB), matching the compositor.
const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

/// Copies the full selection with no truncation: the memfd is sized to it
/// before `collect_selection` fills it.
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
        // The compositor dups the fd via SCM_RIGHTS, so dropping `shm` here
        // leaves the clipboard backing alive on its side.
        let _ = handle.borrow_client().clipboard_copy(shm.fd(), n as u32);
    }
}

/// Bracketed-paste markers are added only when the slave-side application
/// enabled DECSET 2004; a raw `cat` must not see them. `sanitize_paste` strips
/// every control byte first, so the bracket cannot be escaped and a clipboard
/// can never type Ctrl+C.
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
