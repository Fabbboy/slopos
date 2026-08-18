//! Thread-safe protocol connection for compositor communication.
//!
//! The protocol client is confined to the UI thread via `Rc` (`!Send + !Sync`).
//! Background threads that need to post work to the UI thread use [`UiSender`],
//! which is `Send + Sync + Clone`.

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

/// Protocol connection state.
///
/// Confined to the UI thread by `Rc` (`!Send`, `!Sync`). `pending_destroys` is a
/// `RefCell` separate from the client so `Surface::drop` can always queue a
/// deferred destroy while the client is borrowed elsewhere.
pub struct Protocol {
    client: RefCell<Client>,
    pending_destroys: RefCell<PendingDestroys>,
    ui_queue: Arc<UiQueue>,
    compositor_fd: i32,
    wakeup_read_fd: i32,
    /// Write end — closed on drop; `UiQueue` holds a non-owning copy.
    wakeup_write_fd: i32,
    display_format: PixelFormat,
    display_width: u32,
    display_height: u32,
}

/// Handle to the protocol connection; `Rc` confines it to the UI thread.
pub type ProtocolHandle = Rc<Protocol>;

/// Connect to the compositor protocol socket.
///
/// Retries while the compositor is still starting up.
pub fn connect() -> Result<ProtocolHandle, ()> {
    for _ in 0..10 {
        if let Ok(mut client) = Client::connect(b"/run/compositor") {
            let compositor_fd = client.fd();

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
    /// The compositor socket fd; stable for this connection's lifetime.
    #[inline]
    pub fn compositor_fd(&self) -> i32 {
        self.compositor_fd
    }

    /// The read end of the self-pipe written by [`UiSender::post`].
    ///
    /// Exposed so an async consumer can await readiness on it directly.
    #[inline]
    pub fn wakeup_read_fd(&self) -> i32 {
        self.wakeup_read_fd
    }

    /// Get exclusive access to the protocol client.
    ///
    /// Panics on reentrant borrow.
    #[inline]
    pub fn borrow_client(&self) -> core::cell::RefMut<'_, Client> {
        self.client.borrow_mut()
    }

    /// Non-panicking variant of [`borrow_client()`].
    #[inline]
    pub fn try_borrow_client(&self) -> Option<core::cell::RefMut<'_, Client>> {
        self.client.try_borrow_mut().ok()
    }

    /// Queue a deferred destroy for compositor-side objects.
    ///
    /// Called from `Surface::drop`, where the client `RefCell` is already borrowed.
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
    pub fn ui_sender(&self) -> UiSender {
        UiSender {
            queue: Arc::clone(&self.ui_queue),
        }
    }

    /// Execute all closures posted by background threads via [`UiSender`].
    ///
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
    /// Also woken by [`UiSender::post`] from a background thread.
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

        if fds[1].revents & POLLIN != 0 {
            self.drain_wakeup();
        }
    }

    /// Drain the self-pipe so a pending wakeup doesn't fire again next iteration.
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

type UiCallback = Box<dyn FnOnce(&mut Client) + Send>;

/// Push from any thread; drain on the UI thread only.
struct UiQueue {
    pending: Mutex<Vec<UiCallback>>,
    /// Write end of the wakeup pipe; -1 means no pipe.
    wakeup_fd: i32,
}

impl UiQueue {
    fn with_wakeup(wakeup_fd: i32) -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            wakeup_fd,
        }
    }

    fn push(&self, callback: UiCallback) {
        if let Ok(mut q) = self.pending.lock() {
            q.push(callback);
        }
        // EAGAIN (pipe full) is fine: a wakeup is already pending.
        if self.wakeup_fd >= 0 {
            let _ = crate::sys::write(self.wakeup_fd, &[1u8]);
        }
    }

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
/// Posted closures run on the next [`Protocol::drain_ui_queue`].
pub struct UiSender {
    queue: Arc<UiQueue>,
}

impl UiSender {
    /// Post a closure to execute on the UI thread.
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
