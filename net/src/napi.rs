use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

/// Per-NIC NAPI instrumentation: budget cap + processed counter.
///
/// No Idle/Scheduled/Polling state machine: a single IRQ producer and a single
/// kthread consumer parked on the waker's `armed` flag make it redundant.
pub struct NapiContext {
    budget: u32,
    processed: AtomicU32,
}

impl NapiContext {
    pub const fn new(budget: u32) -> Self {
        Self {
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
}

// The wait-predicate purity gate (`scripts/check_wait_predicate_purity.sh`)
// forbids calling either NAPI dispatch function inside a
// `wait_event{,_timeout,_until}` closure — predicates must observe state, not
// side-effect.

/// Stored as a raw pointer so the static is const-constructible.
static NAPI_KICK_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// The registered function must drain the NIC RX ring inline
/// (`virtnet_force_napi_poll` or equivalent).
pub fn register_kick(f: fn()) {
    NAPI_KICK_FN.store(slopos_ostd::util::fn_ptr::encode(f), Ordering::Release);
}

/// Run a NAPI burst synchronously on the caller's CPU; a no-op until a driver
/// registers. Lets a task waking from a syscall wait observe the most recent
/// committed used-ring state without waiting for the kthread.
#[inline]
pub fn kick() {
    let ptr = NAPI_KICK_FN.load(Ordering::Acquire);
    if let Some(f) = slopos_ostd::util::fn_ptr::decode(ptr) {
        f();
    }
}

static NAPI_WAKE_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// The registered function must call
/// [`NapiWaker::arm_and_wake`](crate::napi_waker::NapiWaker::arm_and_wake).
pub fn register_wake_napi(f: fn()) {
    NAPI_WAKE_FN.store(slopos_ostd::util::fn_ptr::encode(f), Ordering::Release);
}

/// Wake the netpoll kthread; a no-op until a driver registers. Does not poll
/// synchronously — the kthread drains when scheduled.
#[inline]
pub fn wake_napi() {
    let ptr = NAPI_WAKE_FN.load(Ordering::Acquire);
    if let Some(f) = slopos_ostd::util::fn_ptr::decode(ptr) {
        f();
    }
}
