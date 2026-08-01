//! Ticket-lock spinning mutex / rwlock primitives.
//!
//! [`SpinLock<T>`] disables both interrupts and preemption while held — the
//! workhorse mutex for kernel data accessed from both task and interrupt
//! contexts. Internally it is a **ticket lock**: each acquirer takes a
//! monotonically-increasing ticket and spins until `now_serving` matches,
//! guaranteeing FIFO acquisition order even under heavy SMP contention.
//!
//! [`PreemptMutex<T>`] is the same shape but only disables preemption (does
//! not save/restore RFLAGS). Use it when the lock is never taken from an
//! IRQ handler.
//!
//! [`IrqRwLock<T>`] is a writer-preferring reader-writer lock that disables
//! interrupts while held. New readers yield to a queued writer to prevent
//! writer starvation.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

use core::sync::atomic::AtomicUsize;

use crate::cpu::preempt::PreemptGuard;
use crate::cpu::x86_64 as cpu;
use crate::mm::AllocError;
use crate::mm::init::{Init, init_from_closure};
use crate::sync::lock_tracking;

// =============================================================================
// Contended-spin relax hook
// =============================================================================
//
// A `SpinLock` waiter spins with interrupts disabled, so it cannot service
// a TLB-shootdown IPI while it waits. If the lock holder is itself waiting
// for this CPU's shootdown ack (e.g. a munmap/COW path flushing remote TLBs
// while holding a per-process VM lock), the two CPUs deadlock: the holder
// waits for an ack the spinner can never deliver. The relax hook closes
// this class structurally — the TLB subsystem registers a callback that
// polls and services this CPU's pending shootdown queue, and every
// IRQs-off contended spin invokes it between PAUSE rounds. A waiter can
// therefore always ack, regardless of which lock it is spinning on.
// (The paravirt-Linux analogue: make lock waits productive instead of
// demanding the holder release first.)

/// Registered relax callback, encoded as a `fn()` address. 0 = none.
static SPIN_RELAX_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Per-CPU re-entrancy latch: the hook itself takes a (TLB queue) SpinLock;
/// a contended acquire inside the hook must spin plainly rather than
/// recurse into the hook without bound.
static SPIN_RELAX_IN_HOOK: [AtomicBool; crate::cpu::x86_64::pcr::MAX_CPUS] = {
    const FALSE: AtomicBool = AtomicBool::new(false);
    [FALSE; crate::cpu::x86_64::pcr::MAX_CPUS]
};

/// Register the contended-spin relax callback. One-shot at boot (the TLB
/// subsystem's init); later calls overwrite, which is harmless — the slot
/// is only ever set to the same function.
pub fn register_spin_relax_hook(hook: fn()) {
    SPIN_RELAX_HOOK.store(hook as usize, Ordering::Release);
}

/// Fire the relax hook once, with per-CPU re-entrancy suppression. Called
/// from IRQs-off contended spin loops; the hook must be safe in that
/// context (the TLB service path only takes its own per-CPU queue lock and
/// issues local INVLPG/CR3 flushes).
#[inline]
fn spin_relax_fire() {
    let raw = SPIN_RELAX_HOOK.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let cpu_id = crate::cpu::x86_64::pcr::get_current_cpu();
    if cpu_id >= SPIN_RELAX_IN_HOOK.len() {
        return;
    }
    if SPIN_RELAX_IN_HOOK[cpu_id].swap(true, Ordering::Relaxed) {
        return; // already servicing on this CPU — plain spin
    }
    if let Some(hook) = crate::util::fn_ptr::fn_ptr_decode_opt::<fn()>(raw as *mut ()) {
        hook();
    }
    SPIN_RELAX_IN_HOOK[cpu_id].store(false, Ordering::Relaxed);
}

/// Ticket-lock mutex that disables interrupts AND preemption while held.
/// Essential for kernel data accessed from both normal and interrupt contexts.
///
/// Uses a **ticket lock** internally for FIFO fairness: each acquirer takes a
/// monotonically-increasing ticket and spins until `now_serving` matches. This
/// guarantees that CPUs acquire the lock in the order they requested it,
/// eliminating starvation under SMP contention.
///
/// Supports poisoning semantics for panic recovery: after a panic-time
/// force-unlock via `poison_unlock()`, the mutex is marked poisoned.
/// Callers can check `is_poisoned()` to determine if the protected data
/// may be in an inconsistent state and needs reinitialization.
#[repr(C)]
pub struct SpinLock<T> {
    core: LockCore,
    data: UnsafeCell<T>,
}

/// The ticket machinery, split out from the payload so it has one layout
/// for every `T`.
///
/// `#[repr(C)]` and offset zero: [`lock_tracking::push_lock`] and the
/// watchdog's wait-for graph both name a lock by address, and this makes
/// `&SpinLock<T>` and `&LockCore` the same address, so the two agree. It
/// also lets the poison callback be one function rather than one
/// monomorphisation per `T` — 93 of them in the dev kernel.
#[repr(C)]
pub struct LockCore {
    /// Monotonically-increasing ticket counter. Each `lock()` call takes the
    /// next ticket via `fetch_add(1)`. Wraps at `u16::MAX` — equality checks
    /// handle wrap-around correctly.
    next_ticket: AtomicU16,
    /// The ticket currently being served. Incremented by `fetch_add(1)` on
    /// unlock. A waiter spins until `now_serving == my_ticket`.
    now_serving: AtomicU16,
    /// `(cpu << 16) | ticket` of the acquirer, or [`NO_HOLDER`].
    ///
    /// Written once on acquisition and never cleared. Clearing it on
    /// release would need to be ordered against the `now_serving` bump,
    /// and either order has a window in which the field lies. Validating
    /// it against `now_serving` instead — see [`LockCore::holder_cpu`] —
    /// has no window to get wrong.
    holder: AtomicU32,
    poisoned: AtomicBool,
    /// Lock ordering level for deadlock prevention. Acquiring a lock at
    /// level N while holding a lock at level >= N is a violation.
    level: u8,
}

/// `holder` for a lock nobody has taken. Not zero: zero decodes as "CPU 0
/// holds it with ticket 0", which every freshly-constructed lock would
/// claim.
const NO_HOLDER: u32 = u32::MAX;

/// Spin iterations with `now_serving` unchanged before the spinner reports
/// itself.
///
/// The bound is on *progress*, not on time: a queue that is draining is
/// contention, however slow, and only a `now_serving` that has stopped
/// moving means the holder itself is stuck.
const SPIN_STALL_ROUNDS: u32 = 1_000_000;

impl LockCore {
    #[inline]
    const fn new(level: u8) -> Self {
        Self {
            next_ticket: AtomicU16::new(0),
            now_serving: AtomicU16::new(0),
            holder: AtomicU32::new(NO_HOLDER),
            poisoned: AtomicBool::new(false),
            level,
        }
    }

    /// Record this CPU as the holder of `ticket` and register the
    /// acquisition with the lock-order validator.
    ///
    /// Out of line and non-generic for the same reason as
    /// [`LockCore::await_ticket`]: this runs on the uncontended path, so it
    /// is inlined into all ~850 `lock()` call sites, and at `-O3` its
    /// register pressure lands in each caller's frame.
    #[inline(never)]
    fn acquired(&self, ticket: u16) {
        let cpu = crate::cpu::x86_64::pcr::get_current_cpu() as u32;
        self.holder
            .store((cpu << 16) | ticket as u32, Ordering::Relaxed);
        // SAFETY: the caller holds the lock and `self` outlives the guard
        // it is about to build.
        unsafe {
            lock_tracking::push_lock(
                self as *const _ as *const (),
                lock_core_poison_fn,
                self.level,
            );
        }
    }

    /// The CPU holding this lock, if the recorded holder can be believed.
    ///
    /// Both conjuncts are load-bearing. `next_ticket != now_serving` says
    /// the lock is held at all — a lock that was released still carries its
    /// last holder. `holder`'s ticket matching `now_serving` says the
    /// recorded winner is the one being served — without it a reader sees
    /// the *previous* holder in the window between a releaser's
    /// `fetch_add` and the next winner's store.
    #[inline]
    fn holder_cpu(&self) -> Option<usize> {
        let serving = self.now_serving.load(Ordering::Acquire);
        if self.next_ticket.load(Ordering::Acquire) == serving {
            return None;
        }
        let holder = self.holder.load(Ordering::Relaxed);
        if holder == NO_HOLDER || (holder & 0xFFFF) as u16 != serving {
            return None;
        }
        Some((holder >> 16) as usize)
    }

    /// Spin until `my_ticket` is served.
    ///
    /// Out of line and non-generic. `SpinLock::lock` is inlined at every
    /// one of its ~850 call sites, and at `-O3` the whole body lands in
    /// each caller's frame — `check_stack_sizes.sh --variant release`
    /// exists to catch exactly that fusion. Keeping the contended path
    /// here leaves the inlined part to the uncontended fast path.
    ///
    /// A spinner is the sharpest detector of its own wedge: it is
    /// executing, it knows which lock it wants, and a ticket lock
    /// distinguishes the two cases a test-and-set lock cannot —
    /// `now_serving` advancing is contention, however slow, and
    /// `now_serving` frozen means the holder itself is stuck. The peer
    /// watchdog can make neither distinction and arrives seconds later
    /// naming only the victim.
    #[cold]
    #[inline(never)]
    fn await_ticket(&self, my_ticket: u16) {
        let mut published = false;
        let mut last_serving = self.now_serving.load(Ordering::Relaxed);
        let mut rounds: u32 = 0;
        loop {
            let serving = self.now_serving.load(Ordering::Acquire);
            if serving == my_ticket {
                break;
            }
            if !published {
                published = crate::watchdog::begin_wait(self as *const LockCore as u64);
            }
            if serving == last_serving {
                rounds = rounds.wrapping_add(1);
                if rounds >= SPIN_STALL_ROUNDS {
                    rounds = 0;
                    report_spin_stall(self, my_ticket);
                }
            } else {
                last_serving = serving;
                rounds = 0;
            }
            if published {
                crate::watchdog::publish_wait_holder(self.holder_cpu());
            }
            // We spin with IRQs masked; service any pending TLB shootdown
            // so a lock holder waiting for this CPU's ack can make progress
            // (see the relax-hook block comment above).
            spin_relax_fire();
            // Proportional backoff: the further away our ticket is from
            // now_serving, the more PAUSE iterations we issue per check.
            let distance = my_ticket.wrapping_sub(serving) as u32;
            for _ in 0..distance.min(64) {
                spin_loop();
            }
        }
        if published {
            crate::watchdog::end_wait();
        }
    }

    /// Release one holder, as a guard's `Drop` would.
    ///
    /// Storing `next_ticket` instead would jump `now_serving` past every
    /// queued waiter, and none of their tickets would ever be served —
    /// turning an abandoned lock into a permanently wedged one, which is
    /// the very failure the watchdog exists to report.
    fn release_one(&self) {
        let mut serving = self.now_serving.load(Ordering::Relaxed);
        loop {
            if self.next_ticket.load(Ordering::Relaxed) == serving {
                return;
            }
            match self.now_serving.compare_exchange_weak(
                serving,
                serving.wrapping_add(1),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(current) => serving = current,
            }
        }
    }
}

// SAFETY: SpinLock provides exclusive access through ticket-lock acquisition with
// interrupts and preemption disabled, making it safe to share across contexts.
unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

pub struct SpinLockGuard<'a, T> {
    mutex: &'a SpinLock<T>,
    saved_flags: u64,
    _preempt: PreemptGuard,
}

pub struct PreemptMutex<T> {
    next_ticket: AtomicU16,
    now_serving: AtomicU16,
    level: u8,
    data: UnsafeCell<T>,
}

// SAFETY: PreemptMutex provides exclusive access via ticket lock with
// preemption disabled.
unsafe impl<T: Send> Send for PreemptMutex<T> {}
unsafe impl<T: Send> Sync for PreemptMutex<T> {}

pub struct PreemptMutexGuard<'a, T> {
    mutex: &'a PreemptMutex<T>,
    _preempt: PreemptGuard,
}

impl<T> SpinLock<T> {
    #[inline]
    pub const fn new(data: T, level: u8) -> Self {
        Self {
            core: LockCore::new(level),
            data: UnsafeCell::new(data),
        }
    }

    /// In-place [`Init`] recipe: build the lock fields directly into
    /// the heap slot, threading the caller's `data_init` recipe through
    /// to the inner `UnsafeCell<T>`. Lets large `T` (e.g. a 256-slot
    /// timer wheel) avoid materialising on the caller's stack between
    /// allocation and the `SpinLock::new(data, ...)` call. Used via
    /// `KBox::try_init(SpinLock::init_with(level, T::init_default()))`.
    pub fn init_with<E>(level: u8, data_init: impl Init<T, E>) -> impl Init<Self, E>
    where
        E: From<AllocError>,
    {
        // SAFETY: the closure writes every field of `slot`. Atomic-init
        // and `UnsafeCell::new` shape are fixed by the `SpinLock`
        // layout — we replicate the byte pattern of `Self::new(data, ..)`
        // by hand so no `Self` rvalue ever materialises on the stack.
        // `data_init.__init` writes the inner `T` directly into the
        // same heap slot via `addr_of_mut!((*slot).data) as *mut T`.
        unsafe {
            init_from_closure(move |slot: *mut Self| -> Result<(), E> {
                addr_of_mut!((*slot).core).write(LockCore::new(level));
                let data_ptr = addr_of_mut!((*slot).data) as *mut T;
                data_init.__init(data_ptr)?;
                Ok(())
            })
        }
    }

    /// Returns the lock ordering level.
    #[inline]
    pub const fn level(&self) -> u8 {
        self.core.level
    }

    /// Force unlock the mutex without proper guard handling.
    ///
    /// # Safety
    /// This is ONLY safe to call after a panic recovery via longjmp, when we know
    /// the lock might be held but the guard was lost.
    #[inline]
    pub unsafe fn force_unlock(&self) {
        self.core.release_one();
    }

    /// Force unlock the mutex AND mark it as poisoned.
    ///
    /// # Safety
    /// Same safety requirements as `force_unlock()`. Use in panic recovery
    /// paths to signal that the protected data may be inconsistent.
    #[inline]
    pub unsafe fn poison_unlock(&self) {
        self.core.poisoned.store(true, Ordering::Release);
        self.core.release_one();
    }

    /// Returns true if this mutex was force-unlocked during panic recovery.
    #[inline]
    pub fn is_poisoned(&self) -> bool {
        self.core.poisoned.load(Ordering::Acquire)
    }

    /// Clear the poisoned state after the protected data has been reinitialized.
    #[inline]
    pub fn clear_poison(&self) {
        self.core.poisoned.store(false, Ordering::Release);
    }

    /// Check if the lock is currently held (or has waiters).
    #[inline]
    pub fn is_locked(&self) -> bool {
        let next = self.core.next_ticket.load(Ordering::Relaxed);
        let serving = self.core.now_serving.load(Ordering::Relaxed);
        next != serving
    }

    /// The CPU the ticket state says holds this lock, for tests.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn holder_cpu_for_test(&self) -> Option<usize> {
        self.core.holder_cpu()
    }

    /// Take a ticket without building a guard, leaving the lock held by
    /// nobody — the state a panic leaves behind.
    ///
    /// Test-only. Leaking a real guard would work too, but it would also
    /// leak the interrupt flags and preempt count the guard restores, which
    /// silently stops the CPU this test runs on from ever ticking again.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn abandon_for_test(&self) {
        self.core.next_ticket.fetch_add(1, Ordering::Relaxed);
    }

    /// Release one holder of a lock whose guard was lost, as the
    /// panic-recovery path does. Test-only, and safe because
    /// [`LockCore::release_one`] is a no-op on a lock that is already free.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn release_leaked_guard_for_test(&self) {
        self.core.release_one();
    }

    /// Get a raw pointer to the protected data without taking the lock.
    ///
    /// # Safety
    /// The caller must ensure that accessing the data through this pointer
    /// is safe. Typical use: reading naturally-aligned fields that are
    /// written atomically.
    #[inline]
    pub unsafe fn as_ptr(&self) -> *const T {
        self.data.get() as *const T
    }

    /// Read a naturally-aligned field of the protected data without
    /// taking the lock. The closure `f` is restricted to receiving a
    /// shared reference to the inner `T`; correctness relies on the
    /// caller's discipline that every field accessed through `f` is a
    /// naturally-aligned scalar (u32 / pointer / atomic) which is only
    /// written under the lock, so a plain load is tear-free on x86-64.
    /// Composite (multi-word) fields MUST re-acquire the lock via
    /// [`Self::lock`] instead.
    ///
    /// Folds the one `unsafe { &*self.as_ptr() }` reborrow interior to
    /// OSTD so consumer crates' lock-free field-peek helpers stay in
    /// safe Rust.
    #[inline]
    pub fn read_atomic_field<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        // SAFETY: the closure contract above restricts `f` to tear-free
        // reads of naturally-aligned fields that are only written under
        // the lock; the resulting shared borrow does not outlive `f`'s
        // execution.
        let inner: &T = unsafe { &*self.data.get() };
        f(inner)
    }

    #[inline]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        // Take a ticket. fetch_add wraps at u16::MAX → 0; equality checks are
        // wrap-safe so this is correct for any number of acquisitions.
        let my_ticket = self.core.next_ticket.fetch_add(1, Ordering::Relaxed);
        if self.core.now_serving.load(Ordering::Acquire) != my_ticket {
            self.core.await_ticket(my_ticket);
        }
        self.core.acquired(my_ticket);

        SpinLockGuard {
            mutex: self,
            saved_flags,
            _preempt: preempt,
        }
    }

    #[inline]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        let current = self.core.now_serving.load(Ordering::Relaxed);
        if self
            .core
            .next_ticket
            .compare_exchange(
                current,
                current.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            // The `try_lock`-only locks are the ones the reporter itself
            // takes; skipping the holder here would leave a hole in the
            // graph exactly where the diagnostics run.
            self.core.acquired(current);
            Some(SpinLockGuard {
                mutex: self,
                saved_flags,
                _preempt: preempt,
            })
        } else {
            cpu::restore_flags(saved_flags);
            drop(preempt);
            None
        }
    }
}

/// Report a spin whose queue has stopped moving.
///
/// `#[cold]` and never inlined, with no `format_args!` at the call site:
/// `SpinLock::lock` is the most-monomorphised function in the kernel and
/// its frame is held to 2048 bytes by `check_stack_sizes.sh`. Non-generic
/// for the same reason.
#[cold]
#[inline(never)]
fn report_spin_stall(core: &LockCore, my_ticket: u16) {
    use crate::watchdog::{dump_wait_chain, nmi_emit, nmi_emit_dec, nmi_emit_hex, nmi_emit_line};

    let me = crate::cpu::x86_64::pcr::get_current_cpu();
    nmi_emit("SPINSTALL: cpu ");
    nmi_emit_dec(me as u64);
    nmi_emit(" waiting on lock ");
    nmi_emit_hex(core as *const LockCore as u64);
    nmi_emit(" ticket ");
    nmi_emit_dec(my_ticket as u64);
    nmi_emit(" serving ");
    nmi_emit_dec(core.now_serving.load(Ordering::Relaxed) as u64);
    nmi_emit(" holder ");
    match core.holder_cpu() {
        Some(cpu) => nmi_emit_dec(cpu as u64),
        None => nmi_emit("unknown"),
    }
    nmi_emit_line("");
    dump_wait_chain(me);
}

impl<'a, T> Deref for SpinLockGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: We hold the lock; exclusive access guaranteed.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T> DerefMut for SpinLockGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: We hold the lock; exclusive access guaranteed.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, T> Drop for SpinLockGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Lock is currently held by this guard.
        unsafe {
            lock_tracking::pop_lock(&self.mutex.core as *const _ as *const ());
        }
        // `holder` is deliberately left naming this CPU. A reader believes
        // it only while `holder`'s ticket equals `now_serving`, which this
        // bump makes false — so there is no window in which a stale holder
        // is trusted, and no clear to order against the release.
        self.mutex.core.now_serving.fetch_add(1, Ordering::Release);
        cpu::restore_flags(self.saved_flags);
    }
}

/// Poison-unlock callback for lock tracking.
///
/// # Safety
/// `addr` must point to a live `LockCore`, which `push_lock` is only ever
/// handed by [`SpinLock::lock`] and [`SpinLock::try_lock`].
unsafe fn lock_core_poison_fn(addr: *const ()) {
    // SAFETY: caller certifies addr is a live LockCore.
    let core = unsafe { &*(addr as *const LockCore) };
    core.poisoned.store(true, Ordering::Release);
    core.release_one();
}

impl<T> PreemptMutex<T> {
    #[inline]
    pub const fn new(data: T, level: u8) -> Self {
        Self {
            next_ticket: AtomicU16::new(0),
            now_serving: AtomicU16::new(0),
            level,
            data: UnsafeCell::new(data),
        }
    }

    #[inline]
    pub fn lock(&self) -> PreemptMutexGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let my_ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);

        loop {
            let serving = self.now_serving.load(Ordering::Acquire);
            if serving == my_ticket {
                break;
            }
            let distance = my_ticket.wrapping_sub(serving) as u32;
            for _ in 0..distance.min(64) {
                spin_loop();
            }
        }

        // SAFETY: Preemption is disabled, self is a static lock.
        unsafe {
            lock_tracking::push_lock(
                self as *const _ as *const (),
                preempt_mutex_poison_fn::<T>,
                self.level,
            );
        }

        PreemptMutexGuard {
            mutex: self,
            _preempt: preempt,
        }
    }

    #[inline]
    pub fn try_lock(&self) -> Option<PreemptMutexGuard<'_, T>> {
        let preempt = PreemptGuard::new();
        let current = self.now_serving.load(Ordering::Relaxed);
        if self
            .next_ticket
            .compare_exchange(
                current,
                current.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            // SAFETY: Preemption is disabled, self is a static lock.
            unsafe {
                lock_tracking::push_lock(
                    self as *const _ as *const (),
                    preempt_mutex_poison_fn::<T>,
                    self.level,
                );
            }
            Some(PreemptMutexGuard {
                mutex: self,
                _preempt: preempt,
            })
        } else {
            drop(preempt);
            None
        }
    }
}

impl<'a, T> Deref for PreemptMutexGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: We hold the lock.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T> DerefMut for PreemptMutexGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: We hold the lock.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, T> Drop for PreemptMutexGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Lock is currently held by this guard.
        unsafe {
            lock_tracking::pop_lock(self.mutex as *const _ as *const ());
        }
        self.mutex.now_serving.fetch_add(1, Ordering::Release);
    }
}

/// Poison-unlock callback for PreemptMutex lock tracking.
///
/// # Safety
/// `addr` must point to a live `PreemptMutex<T>`.
unsafe fn preempt_mutex_poison_fn<T>(addr: *const ()) {
    // SAFETY: caller certifies addr is a valid PreemptMutex<T>.
    let mutex = unsafe { &*(addr as *const PreemptMutex<T>) };
    // One holder, as a guard's `Drop` releases. Storing `next_ticket`
    // would strand every already-queued waiter on a ticket that is never
    // served.
    let mut serving = mutex.now_serving.load(Ordering::Relaxed);
    loop {
        if mutex.next_ticket.load(Ordering::Relaxed) == serving {
            return;
        }
        match mutex.now_serving.compare_exchange_weak(
            serving,
            serving.wrapping_add(1),
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(current) => serving = current,
        }
    }
}

// =============================================================================
// IrqRwLock - Reader-Writer Lock with IRQ disable
// =============================================================================

/// A **writer-preferring** reader-writer lock that disables interrupts while held.
/// Multiple readers can hold the lock simultaneously, but writers get exclusive access.
/// When a writer is waiting, new readers yield to prevent writer starvation.
pub struct IrqRwLock<T> {
    /// State: 0 = unlocked, -1 = write-locked, >0 = number of readers
    state: core::sync::atomic::AtomicI32,
    /// Number of writers waiting for access. When > 0, new readers yield
    /// to prevent writer starvation under continuous read traffic.
    writer_waiting: AtomicU32,
    level: u8,
    data: UnsafeCell<T>,
}

// SAFETY: IrqRwLock provides synchronized access through atomic operations with
// interrupts disabled, making it safe to share across contexts.
unsafe impl<T: Send> Send for IrqRwLock<T> {}
unsafe impl<T: Send + Sync> Sync for IrqRwLock<T> {}

/// Guard for read access to IrqRwLock data.
pub struct IrqRwLockReadGuard<'a, T> {
    lock: &'a IrqRwLock<T>,
    saved_flags: u64,
    _preempt: PreemptGuard,
}

/// Guard for write access to IrqRwLock data.
pub struct IrqRwLockWriteGuard<'a, T> {
    lock: &'a IrqRwLock<T>,
    saved_flags: u64,
    _preempt: PreemptGuard,
}

impl<T> IrqRwLock<T> {
    /// Create a new IrqRwLock protecting the given data.
    #[inline]
    pub const fn new(data: T, level: u8) -> Self {
        Self {
            state: core::sync::atomic::AtomicI32::new(0),
            writer_waiting: AtomicU32::new(0),
            level,
            data: UnsafeCell::new(data),
        }
    }

    /// Acquire read access. Multiple readers can hold the lock simultaneously.
    /// Blocks if a writer holds the lock or if writers are waiting (writer preference).
    #[inline]
    pub fn read(&self) -> IrqRwLockReadGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        loop {
            let state = self.state.load(Ordering::Relaxed);
            if state >= 0 && self.writer_waiting.load(Ordering::Relaxed) == 0 {
                if self
                    .state
                    .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    // SAFETY: Preemption is disabled, self is a static lock.
                    unsafe {
                        lock_tracking::push_lock(
                            self as *const _ as *const (),
                            irq_rwlock_poison_fn::<T>,
                            self.level,
                        );
                    }
                    return IrqRwLockReadGuard {
                        lock: self,
                        saved_flags,
                        _preempt: preempt,
                    };
                }
            }
            // IRQs-off spin: keep servicing pending TLB shootdowns.
            spin_relax_fire();
            spin_loop();
        }
    }

    /// Try to acquire read access without blocking.
    #[inline]
    pub fn try_read(&self) -> Option<IrqRwLockReadGuard<'_, T>> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        let state = self.state.load(Ordering::Relaxed);
        if state >= 0 && self.writer_waiting.load(Ordering::Relaxed) == 0 {
            if self
                .state
                .compare_exchange(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                // SAFETY: Preemption is disabled, self is a static lock.
                unsafe {
                    lock_tracking::push_lock(
                        self as *const _ as *const (),
                        irq_rwlock_poison_fn::<T>,
                        self.level,
                    );
                }
                return Some(IrqRwLockReadGuard {
                    lock: self,
                    saved_flags,
                    _preempt: preempt,
                });
            }
        }
        cpu::restore_flags(saved_flags);
        drop(preempt);
        None
    }

    /// Acquire write access. Only one writer, no readers. Signals intent so
    /// new readers yield (writer preference).
    #[inline]
    pub fn write(&self) -> IrqRwLockWriteGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        self.writer_waiting.fetch_add(1, Ordering::Relaxed);

        loop {
            if self
                .state
                .compare_exchange_weak(0, -1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                self.writer_waiting.fetch_sub(1, Ordering::Relaxed);
                // SAFETY: Preemption is disabled, self is a static lock.
                unsafe {
                    lock_tracking::push_lock(
                        self as *const _ as *const (),
                        irq_rwlock_poison_fn::<T>,
                        self.level,
                    );
                }
                return IrqRwLockWriteGuard {
                    lock: self,
                    saved_flags,
                    _preempt: preempt,
                };
            }
            // IRQs-off spin: keep servicing pending TLB shootdowns.
            spin_relax_fire();
            spin_loop();
        }
    }

    /// Try to acquire write access without blocking.
    #[inline]
    pub fn try_write(&self) -> Option<IrqRwLockWriteGuard<'_, T>> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        if self
            .state
            .compare_exchange(0, -1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            // SAFETY: Preemption is disabled, self is a static lock.
            unsafe {
                lock_tracking::push_lock(
                    self as *const _ as *const (),
                    irq_rwlock_poison_fn::<T>,
                    self.level,
                );
            }
            return Some(IrqRwLockWriteGuard {
                lock: self,
                saved_flags,
                _preempt: preempt,
            });
        }
        cpu::restore_flags(saved_flags);
        drop(preempt);
        None
    }
}

impl<'a, T> Deref for IrqRwLockReadGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: Read guard ensures no writers, data is valid.
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T> Drop for IrqRwLockReadGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Lock is currently held by this guard.
        unsafe {
            lock_tracking::pop_lock(self.lock as *const _ as *const ());
        }
        self.lock.state.fetch_sub(1, Ordering::Release);
        cpu::restore_flags(self.saved_flags);
    }
}

impl<'a, T> Deref for IrqRwLockWriteGuard<'a, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: Write guard ensures exclusive access.
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T> DerefMut for IrqRwLockWriteGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: Write guard ensures exclusive access.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<'a, T> Drop for IrqRwLockWriteGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Lock is currently held by this guard.
        unsafe {
            lock_tracking::pop_lock(self.lock as *const _ as *const ());
        }
        self.lock.state.store(0, Ordering::Release);
        cpu::restore_flags(self.saved_flags);
    }
}

/// Poison-unlock callback for IrqRwLock lock tracking.
///
/// # Safety
/// `addr` must point to a live `IrqRwLock<T>`.
unsafe fn irq_rwlock_poison_fn<T>(addr: *const ()) {
    // SAFETY: caller certifies addr is a valid IrqRwLock<T>.
    let lock = unsafe { &*(addr as *const IrqRwLock<T>) };
    lock.state.store(0, Ordering::Release);
}
