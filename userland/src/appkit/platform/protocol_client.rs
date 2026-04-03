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
use core::sync::atomic::{AtomicPtr, Ordering};
use std::boxed::Box;
use std::rc::Rc;
use std::sync::Arc;
use std::vec::Vec;

use slopos_protocol::client::Client;

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
pub fn connect() -> Result<ProtocolHandle, ()> {
    for _ in 0..10 {
        if let Ok(client) = Client::connect(b"/run/compositor") {
            return Ok(Rc::new(Protocol {
                client: RefCell::new(client),
                pending_destroys: RefCell::new(PendingDestroys::new()),
                ui_queue: Arc::new(UiQueue::new()),
            }));
        }
        crate::syscall::core::sleep_ms(200);
    }
    Err(())
}

impl Protocol {
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
    pub fn queue_destroy(&self, toplevel_id: u32, surface_id: u32) {
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
            if toplevel_id != 0 {
                let _ = client.toplevel_destroy(toplevel_id);
            }
            if surface_id != 0 {
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
}

// ---------------------------------------------------------------------------
// Deferred destroy queue
// ---------------------------------------------------------------------------

const PENDING_DESTROY_CAP: usize = 64;

struct PendingDestroys {
    entries: Vec<(u32, u32)>,
}

impl PendingDestroys {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn push(&mut self, toplevel_id: u32, surface_id: u32) {
        if self.entries.len() < PENDING_DESTROY_CAP {
            self.entries.push((toplevel_id, surface_id));
        } else {
            crate::syscall::tty::write(b"warn: destroy queue full, surface leak\n");
        }
    }

    fn take(&mut self) -> Vec<(u32, u32)> {
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

struct CallbackNode {
    callback: UiCallback,
    next: *mut CallbackNode,
}

/// Thread-safe callback queue (lock-free Treiber stack).
///
/// Push from any thread (wait-free CAS), drain from UI thread only.
struct UiQueue {
    head: AtomicPtr<CallbackNode>,
}

// SAFETY: The AtomicPtr itself is Send+Sync. CallbackNode contains a
// Box<dyn FnOnce + Send> which is Send. Push uses CAS (thread-safe),
// drain swaps head to null (exclusive to UI thread).
unsafe impl Send for UiQueue {}
unsafe impl Sync for UiQueue {}

impl UiQueue {
    const fn new() -> Self {
        Self {
            head: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// Push a callback (any thread, wait-free).
    fn push(&self, callback: UiCallback) {
        let node = Box::into_raw(Box::new(CallbackNode {
            callback,
            next: core::ptr::null_mut(),
        }));
        loop {
            let old_head = self.head.load(Ordering::Acquire);
            // SAFETY: `node` is a valid, exclusively owned allocation.
            unsafe { (*node).next = old_head };
            if self
                .head
                .compare_exchange_weak(old_head, node, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
    }

    /// Drain all callbacks, executing each with `&mut Client` (UI thread only).
    fn drain(&self, client: &mut Client) {
        let head = self.head.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if head.is_null() {
            return;
        }

        // Treiber stack is LIFO — reverse to get FIFO order.
        let mut reversed: *mut CallbackNode = core::ptr::null_mut();
        let mut current = head;
        while !current.is_null() {
            // SAFETY: Each node was allocated via Box::into_raw and is uniquely
            // owned after the atomic swap above.
            let next = unsafe { (*current).next };
            unsafe { (*current).next = reversed };
            reversed = current;
            current = next;
        }

        // Execute in submission order.
        current = reversed;
        while !current.is_null() {
            // SAFETY: Reclaim the Box allocation. The node is exclusively ours.
            let node = unsafe { Box::from_raw(current) };
            current = node.next;
            (node.callback)(client);
        }
    }
}

impl Drop for UiQueue {
    fn drop(&mut self) {
        // Drop any un-drained callbacks.
        let mut current = *self.head.get_mut();
        while !current.is_null() {
            let node = unsafe { Box::from_raw(current) };
            current = node.next;
            // callback dropped without executing
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
