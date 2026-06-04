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
    surface::set_cursor_shape(slopos_abi::CURSOR_SHAPE_TEXT);

    // Open the PTY pair and a real fd for the master (never the index-based
    // tty_read/tty_write, which hardcode non-blocking and bypass fd tracking).
    let (master_idx, _slave_idx) = match process::openpty() {
        Ok(pair) => pair,
        Err(_) => {
            let _ = tty::write(b"terminal: openpty failed\n");
            return;
        }
    };
    let master_owned = match process::open_tty_fd(master_idx) {
        Ok(fd) => fd,
        Err(_) => {
            let _ = tty::write(b"terminal: open_tty_fd failed\n");
            return;
        }
    };
    // The terminal starts with an empty fd table, so freshly opened fds
    // land in the 0-2 range — exactly the slots the spawn dance dup2s the
    // slave over. Lift both PTY fds above stdio first, or the dup2s would
    // silently close the master and hang up the pair.
    let master_fd = dup_above_stdio(master_owned.into_raw());
    if master_fd < 0 {
        let _ = tty::write(b"terminal: master fd relocation failed\n");
        return;
    }

    // Resolve the slave peer fd from the master (TIOCGPTPEER).
    let slave_fd = match fs::ioctl_tiocgptpeer(master_fd) {
        Ok(fd) => fd.into_raw(),
        Err(_) => {
            let _ = tty::write(b"terminal: tiocgptpeer failed\n");
            let _ = fs::close_fd_raw(master_fd);
            return;
        }
    };
    let slave_fd = dup_above_stdio(slave_fd);
    if slave_fd < 0 {
        let _ = tty::write(b"terminal: slave fd relocation failed\n");
        let _ = fs::close_fd_raw(master_fd);
        return;
    }

    // Close-on-exec: the spawned shell must inherit ONLY the slave copies
    // dup2'd onto fd 0/1/2 (dup2 clears cloexec on the new slots) — never
    // the terminal's own master/slave descriptors. A leaked master ref in
    // the shell would keep the pair alive after the terminal closes it,
    // breaking EOF/SIGHUP semantics.
    let _ = fs::set_fd_cloexec(master_fd);
    let _ = fs::set_fd_cloexec(slave_fd);

    // Set the initial winsize from the default geometry so the shell sees a
    // sane TERM size immediately (a Configure later re-pushes the real one).
    let cw = crate::gfx::font::cell_width().max(1);
    let ch = crate::gfx::font::cell_height().max(1);
    let cols = (WINDOW_WIDTH / cw).clamp(1, grid::MAX_COLS as i32) as u16;
    let rows = (WINDOW_HEIGHT / ch).clamp(1, grid::MAX_ROWS as i32) as u16;
    push_winsize(master_fd, rows, cols);

    // Spawn the shell with the slave as its fd 0/1/2 by transiently dup2'ing
    // it over the terminal's own stdio around the spawn, then restoring.
    // No TASK_FLAG_NEW_PGRP: the shell must run its own setsid()/TIOCSCTTY.
    spawn_shell_on_slave(slave_fd);

    // The terminal no longer needs the slave fd; the shell holds its own
    // references. Closing the last extra ref here keeps the master's EOF /
    // SIGHUP semantics correct when the shell finally exits.
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

/// Spawn `/bin/shell` with the PTY slave as its stdin/stdout/stderr.
///
/// Mirrors the save/restore dup2 pattern (shell exec.rs): point the terminal's
/// own fd 0/1/2 at the slave just long enough for the spawned child to inherit
/// them, then restore the terminal's descriptors. The shell itself performs
/// `setsid()` + `TIOCSCTTY`, so we deliberately omit `TASK_FLAG_NEW_PGRP`.
fn spawn_shell_on_slave(slave_fd: i32) {
    let saved0 = fs::dup(0).map(|f| f.into_raw()).unwrap_or(-1);
    let saved1 = fs::dup(1).map(|f| f.into_raw()).unwrap_or(-1);
    let saved2 = fs::dup(2).map(|f| f.into_raw()).unwrap_or(-1);

    let _ = fs::dup2(slave_fd, 0);
    let _ = fs::dup2(slave_fd, 1);
    let _ = fs::dup2(slave_fd, 2);

    let tid = process::spawn_path_with_attrs(
        b"/bin/shell",
        TaskPriority::Normal,
        slopos_abi::task::TASK_FLAG_USER_MODE,
    );
    if tid <= 0 {
        let _ = tty::write(b"terminal: failed to spawn shell\n");
    }

    // Restore the terminal's own descriptors.
    restore_fd(saved0, 0);
    restore_fd(saved1, 1);
    restore_fd(saved2, 2);
}

fn restore_fd(saved: i32, slot: i32) {
    if saved >= 0 {
        let _ = fs::dup2(saved, slot);
        let _ = fs::close_fd_raw(saved);
    } else {
        // The slot was empty before the spawn dance — return it to empty
        // so the terminal does not keep stray slave references alive.
        let _ = fs::close_fd_raw(slot);
    }
}

/// Re-home `fd` above the stdio range (>= 3), closing the low-numbered
/// originals. `dup` allocates the lowest free slot, so walking upward
/// terminates after at most three hops.
fn dup_above_stdio(fd: i32) -> i32 {
    let mut cur = fd;
    let mut low = [-1i32; 3];
    let mut n = 0;
    while (0..3).contains(&cur) {
        low[n] = cur;
        n += 1;
        cur = match fs::dup(cur) {
            Ok(f) => f.into_raw(),
            Err(_) => break,
        };
    }
    for &l in &low[..n] {
        let _ = fs::close_fd_raw(l);
    }
    if (0..3).contains(&cur) { -1 } else { cur }
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
            match input::classify(&evt) {
                CompositorEvent::Key(ascii, scancode) => {
                    match input::encode_key(ascii, scancode, mods) {
                        KeyAction::ToMaster(bytes) => {
                            let _ = fs::write_slice(master_fd, bytes.as_bytes());
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
                        grid.resize(rows, cols);
                        // Resize rebuilds the buffers (and drops scrollback on
                        // a width change), so any anchored selection is no
                        // longer meaningful — clear it rather than copy stale
                        // coordinates.
                        selection.clear();
                        push_winsize(master_fd, rows, cols);
                        want_render = true;
                    }
                }
                CompositorEvent::Close => {
                    return ExitReason::Closed;
                }
                CompositorEvent::PointerMotion(x, y) | CompositorEvent::PointerEnter(x, y) => {
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
                        write_paste(master_fd, &dst.as_slice()[..n], grid.bracketed_paste());
                    }
                }
                CompositorEvent::Ignored => {}
            }
        }

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
        let mst = slopfut::poll_add(master_fd, POLLIN);
        let blink = Box::pin(slopfut::time::sleep_ms(blink_ms));
        let _ = slopfut::select3(comp, mst, blink).await;
    }
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
    loop {
        match fs::read_slice(master_fd, &mut buf) {
            Ok(0) => {
                // EOF / hangup: shell session gone.
                return MasterDrain::Eof;
            }
            Ok(n) => {
                got_any = true;
                // Serial-debug mirror (single call site).
                let _ = tty::write(&buf[..n]);
                for &b in &buf[..n] {
                    grid.process_byte(b);
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
fn write_paste(master_fd: i32, data: &[u8], bracketed: bool) {
    if data.is_empty() {
        return;
    }
    // Sanitizing never grows the payload (it only drops/normalizes bytes).
    let mut clean = std::vec![0u8; data.len()];
    let n = input::sanitize_paste(data, &mut clean);
    if n == 0 {
        return;
    }
    if bracketed {
        let _ = fs::write_slice(master_fd, b"\x1b[200~");
    }
    let _ = fs::write_slice(master_fd, &clean[..n]);
    if bracketed {
        let _ = fs::write_slice(master_fd, b"\x1b[201~");
    }
}
