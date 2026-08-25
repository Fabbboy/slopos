//! `KernelIoToken` — witness that the holder runs at `TaskPriority::KernelIo`.
//! Every deschedule that can last passes a point where a stop or freeze is seen.
//!
//! A freeze parks each thread on its stop queue holding no sleep-queue entry and
//! no runqueue slot: the one state a scheduler reset cannot damage, which is
//! what lets the test fixture reset around these threads instead of killing
//! them. The bounded wait for the acks lives in `sched`.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use slopos_abi::task::{INVALID_TASK_ID, TaskPriority};

use crate::sync::BspToken;
use crate::sync::lock_graph::LockClassKey;
use crate::sync::lock_tracking::LOCK_LEVEL_REGISTRY;
use crate::sync::spin::SpinLock;
use crate::sync::wait_queue::{WaitAbort, WaitQueue};
use crate::task::{SpawnError, SpawnedTaskId};

/// Witness type carried by every `KernelIo`-priority kthread.
///
/// Only [`spawn_kernel_io!`] expansions reach the constructor. `!Send + !Sync`
/// via the `*const ()` marker: the witness holds only for the thread the
/// trampoline handed it to, so it can neither move to another task nor be
/// stored in a global. The `'cpu` lifetime is informational; use `'static` for
/// the long-lived case.
#[derive(Debug)]
pub struct KernelIoToken<'cpu> {
    _not_send: PhantomData<*const ()>,
    _lifetime: PhantomData<&'cpu ()>,
}

impl<'cpu> KernelIoToken<'cpu> {
    /// Construct a fresh witness.
    ///
    /// **Internal — call only from the [`spawn_kernel_io!`] trampoline**,
    /// which runs as the first frame of a task whose priority class the
    /// scheduler has already validated.
    #[doc(hidden)]
    #[inline]
    pub const fn __new_for_trampoline_only() -> Self {
        Self {
            _not_send: PhantomData,
            _lifetime: PhantomData,
        }
    }
}

/// Skips the stop and freeze probes, and sound because it does not block.
#[inline]
pub fn yield_now(_token: &KernelIoToken<'_>) {
    if let Some(yield_fn) = current_yield_backend() {
        yield_fn();
    }
}

pub type YieldBackend = fn();

static YIELD_BACKEND: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the production yield backend. BSP-only via the `&BspToken<'brand>`
/// witness produced by [`crate::sync::run_bsp_init`].
pub fn register_yield_backend<'brand>(_token: &BspToken<'brand>, backend: YieldBackend) {
    let raw = backend as *const () as *mut ();
    let prev = YIELD_BACKEND.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::sync::kernel_io_task::register_yield_backend called twice"
    );
}

#[inline]
fn current_yield_backend() -> Option<YieldBackend> {
    let raw = YIELD_BACKEND.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` was produced by `register_yield_backend` from a function
    // pointer of matching ABI.
    Some(unsafe { core::mem::transmute::<*mut (), YieldBackend>(raw) })
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_yield_backend_for_test() {
    YIELD_BACKEND.store(core::ptr::null_mut(), Ordering::Release);
}

/// A count, not a flag: the first of two overlapping freezes to release must
/// not thaw the second holder's threads.
static FREEZE_DEPTH: AtomicU32 = AtomicU32::new(0);

/// Names *which* freeze a thread's ack is for. A bare bool is ABA-able: a
/// thread preempted before clearing it lets the next freeze read a stale ack.
static FREEZE_EPOCH: AtomicU64 = AtomicU64::new(1);

#[inline]
pub fn kernel_io_freeze_requested() -> bool {
    FREEZE_DEPTH.load(Ordering::Acquire) != 0
}

#[inline]
fn current_freeze_epoch() -> u64 {
    FREEZE_EPOCH.load(Ordering::Acquire)
}

/// Broadcasting on the 0 → 1 edge only keeps a nested freeze from handing every
/// already-parked thread back to the scheduler.
pub fn request_kernel_io_freeze() {
    if FREEZE_DEPTH.fetch_add(1, Ordering::AcqRel) == 0 {
        FREEZE_EPOCH.fetch_add(1, Ordering::AcqRel);
        wake_every_registered_stop();
    }
}

/// Saturating: an unbalanced release that wrapped to `u32::MAX` would discard
/// every work wake for the rest of the boot.
pub fn release_kernel_io_freeze() {
    let mut depth = FREEZE_DEPTH.load(Ordering::Acquire);
    loop {
        if depth == 0 {
            debug_assert!(false, "kernel-io freeze released more times than held");
            return;
        }
        match FREEZE_DEPTH.compare_exchange_weak(
            depth,
            depth - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(observed) => depth = observed,
        }
    }
    if depth == 1 {
        wake_every_registered_stop();
    }
}

/// Panic path only: a test that panics mid-freeze must not leave the threads
/// parked for the rest of the boot.
pub fn clear_kernel_io_freeze_after_panic() {
    if FREEZE_DEPTH.swap(0, Ordering::AcqRel) != 0 {
        wake_every_registered_stop();
    }
}

/// The freeze's second phase. A freeze is cooperative — a thread parks only by
/// running — so a caller whose correctness needs the threads *off the run
/// queues* arms a hold once the freeze's window has had its chance. While a
/// hold is armed no covered task may be linked into a run queue at all.
///
/// A depth, not a flag, for `FREEZE_DEPTH`'s reason.
static HOLD_DEPTH: AtomicU32 = AtomicU32::new(0);

/// The covered ids, mirrored out of `STOP_REGISTRY` so the publish path can ask
/// without taking it: a claim runs under a run-queue lock, and reading the
/// registry there would invert the order `clear_cpu_queues` takes.
static HELD_IDS: [AtomicU32; MAX_KERNEL_IO_STOPS] =
    [const { AtomicU32::new(INVALID_TASK_ID) }; MAX_KERNEL_IO_STOPS];

fn store_held_ids(ids: impl IntoIterator<Item = u32>) {
    let mut ids = ids.into_iter();
    for cell in &HELD_IDS {
        cell.store(ids.next().unwrap_or(INVALID_TASK_ID), Ordering::Release);
    }
}

fn held_ids_snapshot() -> KernelIoTaskIds {
    let mut ids = [INVALID_TASK_ID; MAX_KERNEL_IO_STOPS];
    let mut len = 0usize;
    for cell in &HELD_IDS {
        let id = cell.load(Ordering::Acquire);
        if id != INVALID_TASK_ID {
            ids[len] = id;
            len += 1;
        }
    }
    KernelIoTaskIds { ids, len }
}

/// Ids are published before the depth bump so [`kernel_io_hold_covers`] never
/// answers `true` for a task the release walk will not visit.
///
/// A union with what is already held, never a replacement. The depth counter
/// nests; the id set has to nest with it, or an inner arm taken while a stop is
/// registered-but-unbound drops that id, the outermost disarm never visits it,
/// and the task is stranded in `Held` on no queue, past where the rescue sweep
/// looks. (Nothing deregisters a stop, so that is the only way the registry can
/// answer with less than the incumbent set.)
pub fn arm_kernel_io_hold() {
    let mut merged = held_ids_snapshot();
    for stop in registered_stops().iter().flatten() {
        merged.push_unique(stop.task_id());
    }
    store_held_ids(merged.iter());
    HOLD_DEPTH.fetch_add(1, Ordering::AcqRel);
}

/// Union the registry into an armed hold's cover, without touching the depth.
///
/// [`__spawn_kernel_io`] publishes the stop to the registry *before* binding the
/// spawned id to it, so a hold armed inside that window records the stop with
/// `INVALID_TASK_ID` and covers nothing. This is called from the settle loop,
/// which closes the window only for as long as that loop runs — a spawn landing
/// after the hold has settled is still uncovered, and is the residual recorded
/// in `plans/pipeline-stability.md` §4. Nothing in the tree spawns a kernel-I/O
/// thread off the BSP today, which is why that residual is tolerated rather
/// than fixed here.
///
/// A no-op when no hold is armed, so a caller need not check first.
pub fn refresh_kernel_io_hold() {
    if !kernel_io_hold_armed() {
        return;
    }
    let mut merged = held_ids_snapshot();
    let before = merged.len;
    for stop in registered_stops().iter().flatten() {
        merged.push_unique(stop.task_id());
    }
    if merged.len != before {
        store_held_ids(merged.iter());
    }
}

/// Release one level. `Some` only for the level that takes the depth to zero,
/// carrying the set that level covered so the caller can publish each held task
/// again. Saturating for [`release_kernel_io_freeze`]'s reason, and worse here:
/// a wrapped depth would keep every kernel-I/O thread off every run queue for
/// the rest of the boot.
pub fn disarm_kernel_io_hold() -> Option<KernelIoTaskIds> {
    let held = held_ids_snapshot();
    let mut depth = HOLD_DEPTH.load(Ordering::Acquire);
    loop {
        if depth == 0 {
            debug_assert!(false, "kernel-io hold released more times than held");
            return None;
        }
        match HOLD_DEPTH.compare_exchange_weak(
            depth,
            depth - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(observed) => depth = observed,
        }
    }
    if depth != 1 {
        return None;
    }
    store_held_ids(core::iter::empty());
    Some(held)
}

#[inline]
pub fn kernel_io_hold_armed() -> bool {
    HOLD_DEPTH.load(Ordering::Acquire) != 0
}

#[inline]
pub fn kernel_io_hold_covers(task_id: u32) -> bool {
    if task_id == INVALID_TASK_ID || !kernel_io_hold_armed() {
        return false;
    }
    HELD_IDS
        .iter()
        .any(|cell| cell.load(Ordering::Acquire) == task_id)
}

/// The set an armed hold covers.
#[inline]
pub fn kernel_io_held_ids() -> KernelIoTaskIds {
    held_ids_snapshot()
}

/// Panic path: a test that panics under a hold must not leave the threads off
/// every run queue for the rest of the boot.
pub fn clear_kernel_io_hold_after_panic() -> Option<KernelIoTaskIds> {
    let held = held_ids_snapshot();
    if HOLD_DEPTH.swap(0, Ordering::AcqRel) == 0 {
        return None;
    }
    store_held_ids(core::iter::empty());
    Some(held)
}

/// Arm a hold additionally covering `ids`, handing back the set it found so a
/// test nested inside a real hold can restore exactly that.
///
/// Additive rather than displacing so the panic path stays correct: unwinding
/// runs `KernelIoHold::drop` (which takes the depth 2 -> 1 and republishes
/// nothing) *before* `clear_kernel_io_hold_after_panic` snapshots `HELD_IDS`.
/// A displacing helper would leave that snapshot holding only the test's
/// synthetic ids, and every real kernel-I/O thread stranded in `Held`.
#[cfg(any(test, feature = "test-helpers"))]
pub fn arm_kernel_io_hold_over_for_test(ids: &[u32]) -> KernelIoTaskIds {
    let found = held_ids_snapshot();
    let mut merged = held_ids_snapshot();
    for id in ids {
        merged.push_unique(*id);
    }
    store_held_ids(merged.iter());
    HOLD_DEPTH.fetch_add(1, Ordering::AcqRel);
    found
}

/// Saturating, like [`disarm_kernel_io_hold`]: a test that unwinds past its own
/// disarm, or one whose panic cleanup zeroed the depth first, would otherwise
/// wrap it to `u32::MAX` and refuse every kernel-I/O publication for the rest
/// of the boot.
#[cfg(any(test, feature = "test-helpers"))]
pub fn disarm_kernel_io_hold_for_test(displaced: &KernelIoTaskIds) {
    let mut depth = HOLD_DEPTH.load(Ordering::Acquire);
    while depth != 0 {
        match HOLD_DEPTH.compare_exchange_weak(
            depth,
            depth - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(observed) => depth = observed,
        }
    }
    debug_assert!(depth != 0, "kernel-io hold released more times than held");
    store_held_ids(displaced.iter());
}

/// Cooperative stop signal for one kernel-I/O thread.
///
/// Kernel tasks take no signals and are not killable: they need a stop they
/// can *finish* on — the ext2 flusher's last act is a full sync, which a kill
/// would discard. The queue lives inside the signal so the park, the stop-wake
/// and the freeze gate cannot drift onto separate queues.
pub struct KernelIoStop {
    name: &'static str,
    requested: AtomicBool,
    exited: AtomicBool,
    /// Freeze epoch this thread is parked for, or 0 when it is off the gate.
    frozen_epoch: AtomicU64,
    /// A stalled kthread is otherwise indistinguishable from an idle one.
    laps: AtomicU64,
    task_id: AtomicU32,
    wq: WaitQueue,
}

impl KernelIoStop {
    /// The lock class comes from the caller: these threads serve unrelated
    /// subsystems, and a class minted here would merge all of them.
    pub const fn new(name: &'static str, class: &'static LockClassKey) -> Self {
        Self {
            name,
            requested: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            frozen_epoch: AtomicU64::new(0),
            laps: AtomicU64::new(0),
            task_id: AtomicU32::new(INVALID_TASK_ID),
            wq: WaitQueue::new(class),
        }
    }

    #[inline]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    #[inline]
    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Ask the thread to stop, and wake it: the flag alone leaves a parked
    /// thread that never re-evaluates it. Wakes through a freeze, which a stop
    /// outranks.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
        let _ = self.wq.wake_all();
    }

    /// Skipped under a freeze: the caller's armed edge is the durable state and
    /// this is only a nudge.
    #[inline]
    pub fn wake_for_work(&self) {
        if kernel_io_freeze_requested() {
            return;
        }
        let _ = self.wq.wake_all();
    }

    #[inline]
    pub fn wake_one_for_work(&self) {
        if kernel_io_freeze_requested() {
            return;
        }
        let _ = self.wq.wake_one();
    }

    #[inline]
    pub fn note_exited(&self) {
        self.exited.store(true, Ordering::Release);
    }

    #[inline]
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// For the freeze *now held*; a stale stamp reads false.
    #[inline]
    pub fn is_frozen(&self) -> bool {
        let stamped = self.frozen_epoch.load(Ordering::Acquire);
        stamped != 0 && stamped == current_freeze_epoch()
    }

    #[inline]
    pub fn laps(&self) -> u64 {
        self.laps.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn task_id(&self) -> u32 {
        self.task_id.load(Ordering::Acquire)
    }

    #[inline]
    fn bind_task(&self, task_id: u32) {
        self.task_id.store(task_id, Ordering::Release);
    }

    #[inline]
    fn note_lap(&self) {
        self.laps.fetch_add(1, Ordering::Relaxed);
    }
}

/// Why a kernel-I/O park ended.
#[must_use = "a kernel-I/O park that ignores Stop cannot be shut down"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KthreadWait {
    /// The caller's condition held.
    Ready,
    /// The deadline elapsed, or there was no blocking surface.
    Timeout,
    /// A stop was requested. Finish what must be finished, then return.
    Stop,
}

impl KernelIoToken<'_> {
    /// The stop and freeze probes sit ahead of `condition`, so it is not
    /// evaluated under either: observing predicates only.
    pub fn park<F: FnMut() -> bool>(&self, stop: &KernelIoStop, mut condition: F) -> KthreadWait {
        loop {
            if stop.requested() {
                return KthreadWait::Stop;
            }
            let outcome = stop
                .wq
                .wait_event(|| stop.requested() || kernel_io_freeze_requested() || condition());
            stop.note_lap();
            match Self::classify(stop, outcome) {
                ParkOutcome::Settled(wait) => return wait,
                ParkOutcome::Freeze => {
                    if !hold_frozen(stop) {
                        return KthreadWait::Timeout;
                    }
                }
            }
        }
    }

    /// A freeze restarts the timeout: a thread that spent it parked did no work.
    pub fn park_timeout<F: FnMut() -> bool>(
        &self,
        stop: &KernelIoStop,
        mut condition: F,
        timeout_ms: u64,
    ) -> KthreadWait {
        loop {
            if stop.requested() {
                return KthreadWait::Stop;
            }
            let outcome = stop.wq.wait_event_timeout(
                || stop.requested() || kernel_io_freeze_requested() || condition(),
                timeout_ms,
            );
            stop.note_lap();
            match Self::classify(stop, outcome) {
                ParkOutcome::Settled(wait) => return wait,
                ParkOutcome::Freeze => {
                    if !hold_frozen(stop) {
                        return KthreadWait::Timeout;
                    }
                }
            }
        }
    }

    #[inline]
    fn classify(stop: &KernelIoStop, outcome: Result<(), WaitAbort>) -> ParkOutcome {
        if stop.requested() {
            return ParkOutcome::Settled(KthreadWait::Stop);
        }
        // A kernel task exits by returning, and `Stop` is the only outcome that
        // says so; `Timeout` would spin with the abort still pending.
        if matches!(outcome, Err(WaitAbort::Killed)) {
            return ParkOutcome::Settled(KthreadWait::Stop);
        }
        // Ahead of the error check, or a `park_timeout` expiring as a freeze is
        // raised runs a round of its body inside the holder's window.
        if kernel_io_freeze_requested() {
            return ParkOutcome::Freeze;
        }
        if outcome.is_err() {
            return ParkOutcome::Settled(KthreadWait::Timeout);
        }
        ParkOutcome::Settled(KthreadWait::Ready)
    }
}

enum ParkOutcome {
    Settled(KthreadWait),
    Freeze,
}

/// Unbounded even under `park_timeout`: holding no sleep-queue entry and no
/// runqueue slot leaves a scheduler reset nothing to destroy. Re-stamps because
/// a freeze can end and a new one begin while parked. `false` means no runtime.
fn hold_frozen(stop: &KernelIoStop) -> bool {
    let outcome = loop {
        let epoch = current_freeze_epoch();
        if stop.requested() || !kernel_io_freeze_requested() {
            break Ok(());
        }
        stop.frozen_epoch.store(epoch, Ordering::Release);
        let outcome = stop.wq.wait_event(|| {
            stop.requested() || !kernel_io_freeze_requested() || current_freeze_epoch() != epoch
        });
        if outcome.is_err() {
            break outcome;
        }
    };
    stop.frozen_epoch.store(0, Ordering::Release);
    !matches!(outcome, Err(WaitAbort::NoRuntime))
}

pub const MAX_KERNEL_IO_STOPS: usize = 8;

struct StopRegistry {
    entries: [Option<&'static KernelIoStop>; MAX_KERNEL_IO_STOPS],
    count: usize,
}

static STOP_REGISTRY: SpinLock<StopRegistry> = SpinLock::new(
    StopRegistry {
        entries: [None; MAX_KERNEL_IO_STOPS],
        count: 0,
    },
    crate::lock_class!("STOP_REGISTRY", LOCK_LEVEL_REGISTRY),
);

/// Snapshot so a broadcast runs with the lock released: `wake_all` reaches the
/// scheduler.
fn registered_stops() -> [Option<&'static KernelIoStop>; MAX_KERNEL_IO_STOPS] {
    let registry = STOP_REGISTRY.lock();
    let mut copy: [Option<&'static KernelIoStop>; MAX_KERNEL_IO_STOPS] =
        [None; MAX_KERNEL_IO_STOPS];
    copy[..registry.count].copy_from_slice(&registry.entries[..registry.count]);
    copy
}

fn wake_every_registered_stop() {
    for stop in registered_stops().iter().flatten() {
        let _ = stop.wq.wake_all();
    }
}

fn register_kernel_io_stop(stop: &'static KernelIoStop) -> bool {
    let mut registry = STOP_REGISTRY.lock();
    // A retried spawn hands back the same `'static` stop.
    if registry.entries[..registry.count]
        .iter()
        .flatten()
        .any(|entry| core::ptr::eq(*entry, stop))
    {
        return true;
    }
    if registry.count >= MAX_KERNEL_IO_STOPS {
        return false;
    }
    let idx = registry.count;
    registry.entries[idx] = Some(stop);
    registry.count += 1;
    true
}

/// Registers the stop before spawning, so no thread reaches its first park
/// unreachable by the broadcasts.
#[doc(hidden)]
pub fn __spawn_kernel_io(
    stop: &'static KernelIoStop,
    entry: crate::task::KernelThreadEntry,
) -> Result<SpawnedTaskId, SpawnError> {
    if !register_kernel_io_stop(stop) {
        crate::klog_info!(
            "kernel-io: stop registry full, refusing to spawn '{}'",
            stop.name()
        );
        return Err(SpawnError::StopRegistryFull);
    }
    match crate::task::spawn_at_priority(stop.name(), entry, TaskPriority::KernelIo) {
        Ok(id) => {
            stop.bind_task(id.as_u32());
            Ok(id)
        }
        Err(err) => {
            stop.note_exited();
            Err(err)
        }
    }
}

/// Spawn a `KernelIo` kthread bound to `stop`, which carries its name. The stop
/// is not optional: a thread without one can be neither stopped at shutdown nor
/// frozen for a test scope, and both failures are silent.
#[macro_export]
macro_rules! spawn_kernel_io {
    ($stop:expr, $entry:path $(,)?) => {{
        fn __slopos_kernel_io_trampoline() {
            let token =
                $crate::sync::kernel_io_task::KernelIoToken::<'static>::__new_for_trampoline_only();
            $entry(token);
        }

        $crate::sync::kernel_io_task::__spawn_kernel_io($stop, __slopos_kernel_io_trampoline)
    }};
}

/// Reverse registration order, so a thread that feeds an earlier one drains
/// first. Only asks: the bounded wait needs a scheduler.
pub fn request_kernel_io_stop_all() -> usize {
    let stops = registered_stops();
    let mut asked = 0usize;
    for stop in stops.iter().rev().flatten() {
        stop.request();
        asked += 1;
    }
    asked
}

pub fn kernel_io_stops_pending() -> usize {
    let registry = STOP_REGISTRY.lock();
    registry.entries[..registry.count]
        .iter()
        .flatten()
        .filter(|stop| !stop.has_exited())
        .count()
}

pub fn kernel_io_unfrozen_pending() -> usize {
    let registry = STOP_REGISTRY.lock();
    registry.entries[..registry.count]
        .iter()
        .flatten()
        .filter(|stop| !stop.has_exited() && !stop.is_frozen())
        .count()
}

pub fn for_each_unstopped_kernel_io(mut report: impl FnMut(&'static str)) {
    let registry = STOP_REGISTRY.lock();
    for stop in registry.entries[..registry.count].iter().flatten() {
        if !stop.has_exited() {
            report(stop.name());
        }
    }
}

pub fn for_each_unfrozen_kernel_io(mut report: impl FnMut(&'static str)) {
    let registry = STOP_REGISTRY.lock();
    for stop in registry.entries[..registry.count].iter().flatten() {
        if !stop.has_exited() && !stop.is_frozen() {
            report(stop.name());
        }
    }
}

/// `slot` indexes the append-only registry, so it is stable across calls and
/// usable as a key for a lap snapshot.
pub fn for_each_unfrozen_kernel_io_detail(mut report: impl FnMut(usize, &'static str, u64)) {
    let registry = STOP_REGISTRY.lock();
    for (slot, stop) in registry.entries[..registry.count].iter().enumerate() {
        let Some(stop) = stop else { continue };
        if !stop.has_exited() && !stop.is_frozen() {
            report(slot, stop.name(), stop.laps());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezeWait {
    Done,
    Poll,
    /// The window closed with threads still arriving: start a fresh one.
    Extend,
    /// A whole window passed and not one thread reached the gate.
    GiveUpStalled,
    /// Threads were still arriving at the absolute cap.
    GiveUpCapped,
}

/// Pure so the wait's policy is testable without a clock or a registry.
///
/// Arm order is load-bearing: completion outranks the cap, and the cap outranks
/// a window that has not closed.
pub const fn freeze_wait_verdict(
    pending_now: usize,
    pending_at_window_start: usize,
    window_elapsed_ms: u64,
    total_elapsed_ms: u64,
    window_ms: u64,
    cap_ms: u64,
) -> FreezeWait {
    if pending_now == 0 {
        return FreezeWait::Done;
    }
    if total_elapsed_ms >= cap_ms {
        return FreezeWait::GiveUpCapped;
    }
    if window_elapsed_ms < window_ms {
        return FreezeWait::Poll;
    }
    if pending_now < pending_at_window_start {
        return FreezeWait::Extend;
    }
    FreezeWait::GiveUpStalled
}

/// Snapshotted in one pass so a caller can test membership under its own lock.
/// This, not a priority comparison, is what "infrastructure" means: preserved
/// exactly when also freezable and stoppable.
pub struct KernelIoTaskIds {
    ids: [u32; MAX_KERNEL_IO_STOPS],
    len: usize,
}

impl KernelIoTaskIds {
    #[inline]
    pub const fn empty() -> Self {
        Self {
            ids: [INVALID_TASK_ID; MAX_KERNEL_IO_STOPS],
            len: 0,
        }
    }

    #[inline]
    pub fn contains(&self, task_id: u32) -> bool {
        task_id != INVALID_TASK_ID && self.ids[..self.len].contains(&task_id)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.ids[..self.len].iter().copied()
    }

    /// Ignores [`INVALID_TASK_ID`] and duplicates, and saturates rather than
    /// panicking: the set is a *cover*, and a nested arm that overflowed it
    /// would drop an id the release walk must still visit.
    #[inline]
    fn push_unique(&mut self, task_id: u32) {
        if task_id == INVALID_TASK_ID || self.contains(task_id) || self.len == self.ids.len() {
            return;
        }
        self.ids[self.len] = task_id;
        self.len += 1;
    }
}

pub fn kernel_io_task_ids() -> KernelIoTaskIds {
    let mut ids = [INVALID_TASK_ID; MAX_KERNEL_IO_STOPS];
    let mut len = 0usize;
    for stop in registered_stops().iter().flatten() {
        let id = stop.task_id();
        if id != INVALID_TASK_ID {
            ids[len] = id;
            len += 1;
        }
    }
    KernelIoTaskIds { ids, len }
}

pub fn for_each_kernel_io_stop(mut visit: impl FnMut(&'static KernelIoStop)) {
    let stops = registered_stops();
    for stop in stops.iter().flatten() {
        visit(stop);
    }
}
