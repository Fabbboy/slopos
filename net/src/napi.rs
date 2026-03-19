use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NapiState {
    Idle = 0,
    Scheduled = 1,
    Polling = 2,
}

pub struct NapiContext {
    state: AtomicU8,
    budget: u32,
    processed: AtomicU32,
}

impl NapiContext {
    pub const fn new(budget: u32) -> Self {
        Self {
            state: AtomicU8::new(NapiState::Idle as u8),
            budget,
            processed: AtomicU32::new(0),
        }
    }

    #[inline]
    pub fn budget(&self) -> u32 {
        self.budget
    }

    #[inline]
    pub fn processed(&self) -> u32 {
        self.processed.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn add_processed(&self, count: u32) {
        self.processed.fetch_add(count, Ordering::Relaxed);
    }

    #[inline]
    pub fn state(&self) -> NapiState {
        match self.state.load(Ordering::Acquire) {
            1 => NapiState::Scheduled,
            2 => NapiState::Polling,
            _ => NapiState::Idle,
        }
    }

    #[inline]
    pub fn is_scheduled(&self) -> bool {
        matches!(self.state(), NapiState::Scheduled)
    }

    pub fn schedule(&self) -> bool {
        self.state
            .compare_exchange(
                NapiState::Idle as u8,
                NapiState::Scheduled as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub fn begin_poll(&self) -> bool {
        self.state
            .compare_exchange(
                NapiState::Scheduled as u8,
                NapiState::Polling as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub fn complete(&self) {
        self.state.store(NapiState::Idle as u8, Ordering::Release);
    }
}

// =============================================================================
// NAPI kick — driver-agnostic poll trigger
// =============================================================================
//
// The socket layer needs to trigger packet processing (e.g., after waking from
// a blocking connect/recv) without naming a specific NIC driver.  The driver
// registers its poll function at init time; the socket layer calls `kick()`
// to invoke it.

/// Registered NAPI kick function.  Stored as a raw pointer so the static is
/// const-constructible without `Option<fn()>` (which is non-null-optimised but
/// not const-constructible in a `static`).
static NAPI_KICK_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Register the NAPI kick function.
///
/// Called once during NIC driver init (e.g., `virtio_net::init`).
/// The function should schedule + execute a NAPI poll cycle.
pub fn register_kick(f: fn()) {
    NAPI_KICK_FN.store(f as *mut (), Ordering::Release);
}

/// Trigger a NAPI poll cycle if a kick function has been registered.
///
/// Safe no-op if no driver has registered yet.
#[inline]
pub fn kick() {
    let ptr = NAPI_KICK_FN.load(Ordering::Acquire);
    if !ptr.is_null() {
        // SAFETY: `ptr` was stored via `register_kick` from a valid `fn()`.
        let f: fn() = unsafe { core::mem::transmute(ptr) };
        f();
    }
}
