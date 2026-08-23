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
// forbids calling the NAPI dispatch function inside a
// `wait_event{,_timeout,_until}` closure — predicates must observe state, not
// side-effect.
//
// There is deliberately no synchronous-drain counterpart to `wake_napi`: a
// caller that drains on its own behalf is compensating for the netpoll kthread
// not running.

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
