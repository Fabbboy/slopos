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

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_protocol::client::Client;

/// Interior-mutable cell that is `Sync`.
///
/// SAFETY invariant: SlopOS user processes are single-threaded —
/// there is no concurrent access to the inner value.
struct ClientCell(UnsafeCell<Option<Client>>);
unsafe impl Sync for ClientCell {}

static PROTOCOL_CLIENT: ClientCell = ClientCell(UnsafeCell::new(None));

/// Guard flag with Acquire/Release ordering so the `Client` written
/// inside `init()` is visible to any later `client()` call.
static INIT_DONE: AtomicBool = AtomicBool::new(false);

/// Connect to the compositor protocol socket (non-blocking).
///
/// With socket activation, the listen socket exists before any app starts,
/// so connect() succeeds on the first try via the kernel backlog.
/// No OutputInfo handshake happens here — that is deferred to the first
/// `Surface::new()` call via `Client::ensure_output_info()`.
///
/// Safe to call multiple times; only the first call actually connects.
pub fn init() {
    if INIT_DONE.load(Ordering::Acquire) {
        return;
    }

    for _ in 0..10 {
        if let Ok(client) = Client::connect(b"/run/compositor") {
            // SAFETY: single-threaded process; no concurrent access.
            unsafe {
                *PROTOCOL_CLIENT.0.get() = Some(client);
            }
            INIT_DONE.store(true, Ordering::Release);
            return;
        }
        crate::syscall::core::sleep_ms(200);
    }
    // Compositor not running — client stays None, surface creation will fail.
}

/// Borrow the global protocol client.
///
/// # Panics
///
/// Panics if called before a successful `init()`.
#[inline]
pub fn client() -> &'static mut Client {
    // SAFETY: single-threaded process; init() completes before any client() call.
    // The Acquire load on INIT_DONE ensures the Client write is visible.
    unsafe {
        (*PROTOCOL_CLIENT.0.get())
            .as_mut()
            .expect("protocol not initialized")
    }
}
