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
