//! Global protocol connection for compositor communication.
//!
//! Manages a single `Client` connection per process. The compositor
//! always starts before GUI apps, so `init()` retries a few times
//! to allow for startup delays.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_protocol::client::Client;

static mut PROTOCOL_CLIENT: Option<Client> = None;
static INIT_ATTEMPTED: AtomicBool = AtomicBool::new(false);

/// Connect to the compositor protocol socket.
///
/// With socket activation, the listen socket exists before any app starts,
/// so connect() succeeds on the first try via the kernel backlog. We only
/// retry for the OutputInfo handshake (compositor may still be initializing
/// its framebuffer). 10 retries at 200ms = 2 seconds max.
///
/// Safe to call multiple times; only the first call actually connects.
pub fn init() {
    if INIT_ATTEMPTED.load(Ordering::Relaxed) {
        return;
    }
    INIT_ATTEMPTED.store(true, Ordering::Relaxed);

    // With socket activation, connect() succeeds immediately via kernel backlog.
    // The only wait is for OutputInfo (compositor must accept + respond).
    for _ in 0..10 {
        if let Ok(client) = Client::connect(b"/run/compositor") {
            unsafe {
                addr_of_mut!(PROTOCOL_CLIENT).write(Some(client));
            }
            return;
        }
        crate::syscall::core::sleep_ms(200);
    }
    // Compositor not running -- client stays None, surface creation will fail.
}

/// Borrow the global protocol client.
///
/// # Panics
///
/// Panics if called before a successful `init()`.
#[inline]
pub fn client() -> &'static mut Client {
    unsafe {
        (*addr_of_mut!(PROTOCOL_CLIENT))
            .as_mut()
            .expect("protocol not initialized")
    }
}
