//! Global protocol connection for compositor communication.
//!
//! Manages a single `Client` connection per process.  The compositor
//! always starts before GUI apps, so `init()` retries a few times
//! to allow for startup delays.
//!
//! Architecture (Wayland-style):
//! - `init()` only opens the socket — no blocking handshake.
//! - Display geometry is received lazily via `Client::ensure_output_info()`
//!   the first time it is needed (surface creation).  By that point the
//!   compositor has had time to accept the connection and push the event.

use core::cell::RefCell;

use slopos_protocol::client::Client;

/// Per-process global protocol client.
///
/// Uses `RefCell` for interior mutability with runtime borrow checking,
/// which is safe because SlopOS user processes are single-threaded.
/// `RefCell` dynamically enforces the single-mutable-borrow invariant
/// that raw `UnsafeCell` + `*mut` cannot.
static PROTOCOL_CLIENT: CellWrapper = CellWrapper(RefCell::new(None));

/// Wrapper to make `RefCell` usable in a `static`.
///
/// SAFETY: SlopOS user processes are single-threaded — `RefCell` is
/// never accessed from multiple threads.
struct CellWrapper(RefCell<Option<Client>>);
unsafe impl Sync for CellWrapper {}

/// Connect to the compositor protocol socket (non-blocking).
///
/// With socket activation, the listen socket exists before any app starts,
/// so connect() succeeds on the first try via the kernel backlog.
/// No OutputInfo handshake happens here — that is deferred to the first
/// `Surface::new()` call via `Client::ensure_output_info()`.
///
/// Safe to call multiple times; only the first call actually connects.
pub fn init() {
    if PROTOCOL_CLIENT.0.borrow().is_some() {
        return;
    }

    for _ in 0..10 {
        if let Ok(client) = Client::connect(b"/run/compositor") {
            *PROTOCOL_CLIENT.0.borrow_mut() = Some(client);
            return;
        }
        crate::syscall::core::sleep_ms(200);
    }
    // Compositor not running — client stays None, surface creation will fail.
}

/// Get an exclusive borrow of the global protocol client.
///
/// Returns a `RefMut` guard that dynamically enforces single-borrow.
/// Panics if called reentrantly (which would be a logic bug in
/// single-threaded code) or before a successful `init()`.
#[inline]
pub fn client() -> core::cell::RefMut<'static, Client> {
    core::cell::RefMut::map(PROTOCOL_CLIENT.0.borrow_mut(), |opt| {
        opt.as_mut().expect("protocol not initialized")
    })
}

/// Non-panicking variant of [`client()`].
///
/// Returns `None` if the protocol client was never initialized or if
/// the `RefCell` is already borrowed (e.g. during Drop from within
/// a client call).
#[inline]
pub fn try_client() -> Option<core::cell::RefMut<'static, Client>> {
    let borrow = PROTOCOL_CLIENT.0.try_borrow_mut().ok()?;
    if borrow.is_none() {
        return None;
    }
    Some(core::cell::RefMut::map(borrow, |opt| opt.as_mut().unwrap()))
}

/// Returns `true` if the protocol client is connected.
#[inline]
pub fn is_initialized() -> bool {
    PROTOCOL_CLIENT
        .0
        .try_borrow()
        .map(|b| b.is_some())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Deferred destroy queue
// ---------------------------------------------------------------------------

/// Maximum number of deferred destroy requests that can be queued.
const PENDING_DESTROY_CAP: usize = 32;

struct PendingDestroy {
    entries: [(u32, u32); PENDING_DESTROY_CAP], // (toplevel_id, surface_id)
    count: usize,
}

static PENDING_DESTROYS: PendingDestroyCellWrapper =
    PendingDestroyCellWrapper(RefCell::new(PendingDestroy {
        entries: [(0, 0); PENDING_DESTROY_CAP],
        count: 0,
    }));

/// Wrapper to make `RefCell<PendingDestroy>` usable in a `static`.
///
/// SAFETY: same single-threaded guarantee as `CellWrapper` above.
struct PendingDestroyCellWrapper(RefCell<PendingDestroy>);
unsafe impl Sync for PendingDestroyCellWrapper {}

/// Queue a deferred destroy for compositor-side toplevel + surface objects.
///
/// Called from `Surface::drop` when the protocol client RefCell is already
/// borrowed. The request will be flushed at the top of the next event loop
/// iteration via [`flush_pending_destroys`].
pub fn queue_destroy(toplevel_id: u32, surface_id: u32) {
    if let Ok(mut q) = PENDING_DESTROYS.0.try_borrow_mut() {
        let idx = q.count;
        if idx < PENDING_DESTROY_CAP {
            q.entries[idx] = (toplevel_id, surface_id);
            q.count = idx + 1;
        }
    }
}

/// Send any deferred destroy requests that were queued by `Surface::drop`.
///
/// Must be called from a context where the protocol client RefCell is NOT
/// already borrowed (i.e. at the top of the event loop, before any other
/// client operations).
pub fn flush_pending_destroys() {
    // Take the pending list with a short borrow.
    let snapshot = {
        let Ok(mut q) = PENDING_DESTROYS.0.try_borrow_mut() else {
            return;
        };
        let n = q.count;
        if n == 0 {
            return;
        }
        let snap = (q.entries, n);
        q.count = 0;
        snap
    };

    // Separate scope: borrow the client and send destroy messages.
    let Some(mut client) = try_client() else {
        // Client unavailable — re-queue so we retry next iteration.
        if let Ok(mut q) = PENDING_DESTROYS.0.try_borrow_mut() {
            let (entries, n) = snapshot;
            for i in 0..n {
                let idx = q.count;
                if idx < PENDING_DESTROY_CAP {
                    q.entries[idx] = entries[i];
                    q.count = idx + 1;
                }
            }
        }
        return;
    };

    let (entries, n) = snapshot;
    for i in 0..n {
        let (toplevel_id, surface_id) = entries[i];
        if toplevel_id != 0 {
            let _ = client.toplevel_destroy(toplevel_id);
        }
        if surface_id != 0 {
            let _ = client.surface_destroy(surface_id);
        }
    }
}
