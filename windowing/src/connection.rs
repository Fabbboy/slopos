//! Thread-safe protocol connection for compositor communication.
//!
//! The protocol client is confined to the UI thread via `Rc` (`!Send + !Sync`).
//! Background threads that need to post work to the UI thread use [`UiSender`],
//! which is `Send + Sync + Clone`.
//!
//! Architecture:
//! - [`connect()`] opens the socket and returns an `Rc<Protocol>` handle.
//! - The handle stays on the UI thread — the compiler prevents sending it.
//! - [`Protocol::ui_sender()`] creates a cross-thread posting handle.
//! - The event loop calls [`Protocol::drain_ui_queue()`] each iteration.

use core::cell::RefCell;
use std::boxed::Box;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use slopos_abi::handle::{DisplayHandle, HasDisplayHandle, RawDisplayHandle};
use slopos_abi::pixel::PixelFormat;
use slopos_abi::syscall::posix::POLLIN;
use slopos_abi::syscall::types::UserPollFd;
use slopos_protocol::client::Client;
use slopos_protocol::types::{SurfaceId, ToplevelId};

// ---------------------------------------------------------------------------
// Protocol handle (UI-thread-confined via Rc)
// ---------------------------------------------------------------------------

/// Protocol connection state.
///
/// Confined to the UI thread by `Rc` (`!Send`, `!Sync`). Two separate `RefCell`s
/// ensure that `Surface::drop` can always queue a deferred destroy even while the
/// client `RefCell` is borrowed elsewhere.
pub struct Protocol {
    client: RefCell<Client>,
    pending_destroys: RefCell<PendingDestroys>,
    ui_queue: Arc<UiQueue>,
    /// Cached compositor socket fd (stable for the lifetime of the connection).
    compositor_fd: i32,
    /// Read end of the self-pipe used by [`wait_events`] to wake from `poll()`.
    wakeup_read_fd: i32,
    /// Write end — closed on drop (the UiQueue also holds a copy via `wakeup_fd`).
    wakeup_write_fd: i32,
    /// Cached display pixel format (set during `connect()`).
    display_format: PixelFormat,
    /// Cached display width in pixels (set during `connect()`).
    display_width: u32,
    /// Cached display height in pixels (set during `connect()`).
    display_height: u32,
}

/// Handle to the protocol connection.
///
/// Clone freely on the UI thread. Cannot cross thread boundaries — `Rc` is
/// `!Send` and `!Sync`, so the compiler rejects any attempt to share it.
pub type ProtocolHandle = Rc<Protocol>;

/// Connect to the compositor protocol socket.
///
/// Returns an `Rc<Protocol>` confined to the calling thread. Retries up to 10
/// times with 200ms delays to allow for compositor startup.
///
/// Internally creates a self-pipe so that [`Protocol::wait_events`] can be
/// woken by [`UiSender::post`] from background threads.
pub fn connect() -> Result<ProtocolHandle, ()> {
    for _ in 0..10 {
        if let Ok(mut client) = Client::connect(b"/run/compositor") {
            let compositor_fd = client.fd();

            // Eagerly sync display info before wrapping in RefCell.
            // If the compositor hasn't sent OutputInfo yet, ensure_output_info
            // retries internally for up to 10 seconds.
            if client.ensure_output_info().is_err() {
                crate::sys::sleep_ms(200);
                continue;
            }
            let display_format =
                PixelFormat::from_u32(client.display_format()).unwrap_or(PixelFormat::Argb8888);
            let display_width = client.display_width();
            let display_height = client.display_height();

            // Self-pipe: lets UiSender::post() wake a poll()-sleeping UI thread.
            let (wakeup_read_fd, wakeup_write_fd) =
                crate::sys::pipe2(slopos_abi::syscall::posix::O_NONBLOCK as u32)?;

            return Ok(Rc::new(Protocol {
                client: RefCell::new(client),
                pending_destroys: RefCell::new(PendingDestroys::new()),
                ui_queue: Arc::new(UiQueue::with_wakeup(wakeup_write_fd)),
                compositor_fd,
                wakeup_read_fd,
                wakeup_write_fd,
                display_format,
                display_width,
                display_height,
            }));
        }
        crate::sys::sleep_ms(200);
    }
    Err(())
}

impl HasDisplayHandle for Protocol {
    fn display_handle(&self) -> DisplayHandle<'_> {
        DisplayHandle::new(RawDisplayHandle {
            fd: self.compositor_fd,
            format: self.display_format,
            width: self.display_width,
            height: self.display_height,
        })
    }
}

impl Protocol {
    /// The compositor socket file descriptor.
    ///
    /// Stable for the lifetime of this connection.
    #[inline]
    pub fn compositor_fd(&self) -> i32 {
        self.compositor_fd
    }

    /// The read end of the self-pipe written by [`UiSender::post`].
    ///
    /// Stable for the lifetime of this connection. Exposed so an async
    /// consumer can await readiness on it directly (the sync fallback uses
    /// it internally in [`wait_events`]).
    #[inline]
    pub fn wakeup_read_fd(&self) -> i32 {
        self.wakeup_read_fd
    }

    /// Get exclusive access to the protocol client.
    ///
    /// Panics on reentrant borrow (a logic bug in single-threaded code).
    #[inline]
    pub fn borrow_client(&self) -> core::cell::RefMut<'_, Client> {
        self.client.borrow_mut()
    }

    /// Non-panicking variant of [`borrow_client()`].
    ///
    /// Returns `None` if the client `RefCell` is already borrowed (e.g. during
    /// `Surface::drop` from within a client call).
    #[inline]
    pub fn try_borrow_client(&self) -> Option<core::cell::RefMut<'_, Client>> {
        self.client.try_borrow_mut().ok()
    }

    /// Queue a deferred destroy for compositor-side objects.
    ///
    /// Uses a separate `RefCell` from the client, so this never conflicts with
    /// an active client borrow. Called from `Surface::drop` when the client
    /// `RefCell` is already borrowed.
    pub fn queue_destroy(&self, toplevel_id: ToplevelId, surface_id: SurfaceId) {
        if let Ok(mut q) = self.pending_destroys.try_borrow_mut() {
            q.push(toplevel_id, surface_id);
        }
    }

    /// Send any deferred destroy requests queued by `Surface::drop`.
    ///
    /// Call at the top of the event loop before any other client operations.
    pub fn flush_pending_destroys(&self) {
        let snapshot = {
            let Ok(mut q) = self.pending_destroys.try_borrow_mut() else {
                return;
            };
            q.take()
        };

        if snapshot.is_empty() {
            return;
        }

        let Some(mut client) = self.try_borrow_client() else {
            // Re-queue so we retry next iteration.
            if let Ok(mut q) = self.pending_destroys.try_borrow_mut() {
                for &(toplevel_id, surface_id) in &snapshot {
                    q.push(toplevel_id, surface_id);
                }
            }
            return;
        };

        for (toplevel_id, surface_id) in snapshot {
            if toplevel_id != ToplevelId::NONE {
                let _ = client.toplevel_destroy(toplevel_id);
            }
            if surface_id != SurfaceId::NONE {
                let _ = client.surface_destroy(surface_id);
            }
        }
    }

    /// Create a sender handle for posting work from background threads.
    ///
    /// The returned `UiSender` is `Send + Sync + Clone` — pass it to any thread.
    /// Posted closures execute on the next [`drain_ui_queue()`] call.
    pub fn ui_sender(&self) -> UiSender {
        UiSender {
            queue: Arc::clone(&self.ui_queue),
        }
    }

    /// Execute all closures posted by background threads via [`UiSender`].
    ///
    /// Zero cost when the queue is empty (single atomic load).
    /// Call at the top of the event loop, after `flush_pending_destroys`.
    pub fn drain_ui_queue(&self) {
        let mut client = match self.try_borrow_client() {
            Some(c) => c,
            None => return,
        };
        self.ui_queue.drain(&mut client);
    }

    /// Sleep until the compositor sends data or `timeout_ms` elapses.
    ///
    /// Polls both the compositor socket and the internal wakeup pipe, so
    /// [`UiSender::post`] from a background thread also wakes this call.
    ///
    /// * `timeout_ms > 0` — sleep up to that many milliseconds.
    /// * `timeout_ms == 0` — non-blocking check (returns immediately).
    /// * `timeout_ms < 0` — sleep indefinitely until an event arrives.
    pub fn wait_events(&self, timeout_ms: i64) {
        let mut fds = [
            UserPollFd {
                fd: self.compositor_fd,
                events: POLLIN,
                revents: 0,
            },
            UserPollFd {
                fd: self.wakeup_read_fd,
                events: POLLIN,
                revents: 0,
            },
        ];
        let _ = crate::sys::poll(&mut fds, timeout_ms);

        // Drain the wakeup pipe so it doesn't fire again next iteration.
        if fds[1].revents & POLLIN != 0 {
            self.drain_wakeup();
        }
    }

    /// Drain the self-pipe so a pending wakeup doesn't fire again next
    /// iteration. Reads one chunk at a time until the (non-blocking) pipe
    /// is empty. Call after the wakeup fd reports readable.
    pub fn drain_wakeup(&self) {
        let mut drain = [0u8; 64];
        while crate::sys::read(self.wakeup_read_fd, &mut drain) > 0 {}
    }
}

impl Drop for Protocol {
    fn drop(&mut self) {
        crate::sys::close(self.wakeup_read_fd);
        crate::sys::close(self.wakeup_write_fd);
    }
}

// ---------------------------------------------------------------------------
// Deferred destroy queue
// ---------------------------------------------------------------------------

const PENDING_DESTROY_CAP: usize = 64;

struct PendingDestroys {
    entries: Vec<(ToplevelId, SurfaceId)>,
}

impl PendingDestroys {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn push(&mut self, toplevel_id: ToplevelId, surface_id: SurfaceId) {
        if self.entries.len() < PENDING_DESTROY_CAP {
            self.entries.push((toplevel_id, surface_id));
        } else {
            crate::sys::tty_write(b"warn: destroy queue full, surface leak\n");
        }
    }

    fn take(&mut self) -> Vec<(ToplevelId, SurfaceId)> {
        core::mem::take(&mut self.entries)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// UiSender — cross-thread posting (Send + Sync + Clone)
// ---------------------------------------------------------------------------

type UiCallback = Box<dyn FnOnce(&mut Client) + Send>;

/// Thread-safe callback queue.
///
/// Push from any thread (briefly holds the mutex), drain from UI
/// thread only. Holds the write end of a self-pipe so that
/// [`UiSender::post`] can wake a UI thread sleeping in
/// [`Protocol::wait_events`].
///
/// UI callback queues exhibit very low contention — a `Mutex<Vec<_>>`
/// keeps the implementation `unsafe`-free without measurable cost.
struct UiQueue {
    pending: Mutex<Vec<UiCallback>>,
    /// Write end of the wakeup pipe. -1 means no pipe (should not happen in
    /// practice but keeps the default constructor safe).
    wakeup_fd: i32,
}

impl UiQueue {
    /// Create a queue with a wakeup pipe write end.
    fn with_wakeup(wakeup_fd: i32) -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            wakeup_fd,
        }
    }

    /// Push a callback (any thread).
    ///
    /// After the callback is enqueued, writes one byte to the wakeup
    /// pipe so that a UI thread sleeping in [`Protocol::wait_events`]
    /// wakes up.
    fn push(&self, callback: UiCallback) {
        if let Ok(mut q) = self.pending.lock() {
            q.push(callback);
        }
        // Wake the UI thread. EAGAIN (pipe full) is fine — the wakeup is
        // already pending. Any other error is harmless (best-effort).
        if self.wakeup_fd >= 0 {
            let _ = crate::sys::write(self.wakeup_fd, &[1u8]);
        }
    }

    /// Drain all callbacks, executing each with `&mut Client` (UI thread only).
    fn drain(&self, client: &mut Client) {
        let pending = {
            let Ok(mut q) = self.pending.lock() else {
                return;
            };
            core::mem::take(&mut *q)
        };
        for cb in pending {
            cb(client);
        }
    }
}

/// Handle for posting work to the UI thread from any thread.
///
/// `Send + Sync + Clone` — safe to move to background threads.
/// Posted closures receive `&mut Client` and execute on the next event loop
/// iteration when the UI thread calls `Protocol::drain_ui_queue()`.
pub struct UiSender {
    queue: Arc<UiQueue>,
}

impl UiSender {
    /// Post a closure to execute on the UI thread.
    ///
    /// Returns immediately. The closure runs on the next event loop iteration.
    pub fn post(&self, f: impl FnOnce(&mut Client) + Send + 'static) {
        self.queue.push(Box::new(f));
    }
}

impl Clone for UiSender {
    fn clone(&self) -> Self {
        Self {
            queue: Arc::clone(&self.queue),
        }
    }
}
