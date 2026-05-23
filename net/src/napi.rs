use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

/// Per-NIC NAPI instrumentation: budget cap + processed counter.
///
/// Phase 2 stripped the Idle/Scheduled/Polling CAS state machine — with
/// a single IRQ producer (`NapiWaker::arm_and_wake`) and a single
/// kthread consumer (`napi_thread_entry`) parked on the waker, the
/// 3-state CAS is structurally redundant: the waker's `armed: AtomicBool`
/// already ensures one wake per pending event and the kthread's loop
/// shape forbids re-entrancy. Counters are kept for telemetry and the
/// budget controls per-burst RX cap.
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

// =============================================================================
// Driver-agnostic NAPI dispatch
// =============================================================================
//
// Two function pointers are registered by the NIC driver at init:
//
// 1. `register_wake_napi` — async wake. Called by non-IRQ producers
//    (loopback TX) to signal the netpoll kthread without running a
//    synchronous burst. Maps to `NapiWaker::arm_and_wake`.
//
// 2. `register_kick` — sync drain. Called by user-task syscall paths
//    (`socket_connect` retry, `socket_recv` post-wait, `socket_poll_readable`)
//    to run a NAPI burst inline on the caller's CPU. Maps to
//    `virtnet_force_napi_poll`.
//
// The sync `kick` is the user-task side of the threaded-NAPI design:
// the kthread is the primary RX cadence (woken by IRQ
// `arm_and_wake`), but a user task waking from a syscall wait has no
// way to know whether the kthread has caught up with the ring. The
// kick drains anything the kthread has not yet processed, then
// returns. Net effect: a wake from any path observes the most recent
// committed used-ring state.
//
// The wait-predicate purity gate (`scripts/check_wait_predicate_purity.sh`)
// forbids calling either function inside a `wait_event{,_timeout,_until}`
// closure — predicates must observe state, not side-effect.

/// Driver-registered NAPI sync-kick function. Stored as a raw pointer
/// so the static is const-constructible.
static NAPI_KICK_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Register the NAPI sync-kick function. The registered function
/// should call `virtnet_force_napi_poll` or equivalent — drain the
/// NIC RX ring inline.
pub fn register_kick(f: fn()) {
    NAPI_KICK_FN.store(slopos_ostd::util::fn_ptr::encode(f), Ordering::Release);
}

/// Run a NAPI burst synchronously on the caller's CPU. Safe no-op
/// when no NIC driver has registered. Used by user-task syscall
/// paths (`socket_connect` retry, `socket_recv` post-wait,
/// `socket_poll_readable`) to ensure they observe the most recent
/// committed used-ring state without waiting for the kthread.
#[inline]
pub fn kick() {
    let ptr = NAPI_KICK_FN.load(Ordering::Acquire);
    if let Some(f) = slopos_ostd::util::fn_ptr::decode(ptr) {
        f();
    }
}

/// Driver-registered NAPI async-wake function. Stored as a raw
/// pointer so the static is const-constructible.
static NAPI_WAKE_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Register the NAPI async-wake function. Called once during NIC
/// driver init; the registered function should call
/// [`NapiWaker::arm_and_wake`](crate::napi_waker::NapiWaker::arm_and_wake).
pub fn register_wake_napi(f: fn()) {
    NAPI_WAKE_FN.store(slopos_ostd::util::fn_ptr::encode(f), Ordering::Release);
}

/// Wake the netpoll kthread. Safe no-op when no driver has
/// registered (boot-time test fixtures, loopback-only configs).
/// Does NOT run a synchronous poll — the kthread is responsible
/// for draining when scheduled.
#[inline]
pub fn wake_napi() {
    let ptr = NAPI_WAKE_FN.load(Ordering::Acquire);
    if let Some(f) = slopos_ostd::util::fn_ptr::decode(ptr) {
        f();
    }
}
