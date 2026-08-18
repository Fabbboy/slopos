//! Ticket-lock spinning mutex / rwlock primitives.
//!
//! [`SpinLock<T>`] masks interrupts and preemption while held;
//! [`PreemptMutex<T>`] is the same shape but masks only preemption (no
//! save/restore of RFLAGS), for locks never taken from an IRQ handler;
//! [`IrqRwLock<T>`] is the reader-writer form.

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
use crate::sync::lock_tracking::LockClassKey;

// A `SpinLock` waiter spins with interrupts disabled, so it cannot service a
// TLB-shootdown IPI. If the holder is itself waiting for this CPU's shootdown
// ack, the two deadlock. The registered relax hook drains this CPU's pending
// shootdown queue between PAUSE rounds, so a waiter can always ack whatever
// lock it is spinning on.

/// Registered relax callback, encoded as a `fn()` address. 0 = none.
static SPIN_RELAX_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Per-CPU re-entrancy latch: the hook takes a (TLB queue) SpinLock itself, so
/// a contended acquire inside it must spin plainly rather than recurse.
static SPIN_RELAX_IN_HOOK: [AtomicBool; crate::cpu::x86_64::pcr::MAX_CPUS] = {
    const FALSE: AtomicBool = AtomicBool::new(false);
    [FALSE; crate::cpu::x86_64::pcr::MAX_CPUS]
};

/// Register the contended-spin relax callback. Overwriting is harmless: the
/// slot is only ever set to the same function.
pub fn register_spin_relax_hook(hook: fn()) {
    SPIN_RELAX_HOOK.store(hook as usize, Ordering::Release);
}

/// Service pending cross-CPU work once from a hand-rolled interrupts-off spin.
///
/// The lock primitives call this for you. Any other spin waiting on a peer — a
/// handover flag, a state machine's ready bit — has to call it explicitly, or
/// it becomes a CPU that waits for a peer while refusing to ack that peer's
/// shootdown.
#[inline]
pub fn spin_relax() {
    spin_relax_fire();
}

/// Fire the relax hook once, with per-CPU re-entrancy suppression. Runs from
/// IRQs-off contended spin loops, so the hook must be safe in that context.
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
        return;
    }
    if let Some(hook) = crate::util::fn_ptr::fn_ptr_decode_opt::<fn()>(raw as *mut ()) {
        hook();
    }
    SPIN_RELAX_IN_HOOK[cpu_id].store(false, Ordering::Relaxed);
}

/// Ticket-lock mutex that disables interrupts AND preemption while held, for
/// kernel data accessed from both normal and interrupt contexts. Acquisition is
/// FIFO: each acquirer takes a monotonically-increasing ticket and spins until
/// `now_serving` matches.
///
/// After a panic-time force-unlock via `poison_unlock()` the mutex is marked
/// poisoned; `is_poisoned()` tells a caller whether the protected data may be
/// inconsistent and need reinitialization.
#[repr(C)]
pub struct SpinLock<T> {
    core: LockCore,
    data: UnsafeCell<T>,
}

/// The ticket machinery, split out from the payload so it has one layout for
/// every `T`. `#[repr(C)]` at offset zero makes `&SpinLock<T>` and `&LockCore`
/// the same address, which [`lock_tracking::push_lock`] and the watchdog's
/// wait-for graph both depend on, and lets the poison callback be one function
/// rather than one monomorphisation per `T`.
#[repr(C)]
pub struct LockCore {
    /// Ticket counter. Wraps at `u16::MAX`; the equality checks are wrap-safe.
    next_ticket: AtomicU16,
    /// The ticket currently being served; a waiter spins until it matches.
    now_serving: AtomicU16,
    /// `(cpu << 16) | ticket` of the acquirer, or [`NO_HOLDER`]. Written on
    /// acquisition and never cleared: either order against the `now_serving`
    /// bump leaves a window in which the field lies, so readers validate it
    /// against `now_serving` instead — see [`LockCore::holder_cpu`].
    holder: AtomicU32,
    poisoned: AtomicBool,
    /// Declaration-site class. Carries the advisory level too, so two
    /// instances of one site cannot disagree about their own rank.
    class: &'static LockClassKey,
}

/// `holder` for a lock nobody has taken. Not zero: zero decodes as "CPU 0 holds
/// it with ticket 0", which every freshly-constructed lock would claim.
const NO_HOLDER: u32 = u32::MAX;

/// Spin iterations with `now_serving` unchanged before the spinner reports
/// itself. The bound is on *progress*, not time: a draining queue is
/// contention however slow, and only a frozen `now_serving` means the holder
/// itself is stuck.
const SPIN_STALL_ROUNDS: u32 = 1_000_000;

impl LockCore {
    #[inline]
    const fn new(class: &'static LockClassKey) -> Self {
        Self {
            next_ticket: AtomicU16::new(0),
            now_serving: AtomicU16::new(0),
            holder: AtomicU32::new(NO_HOLDER),
            poisoned: AtomicBool::new(false),
            class,
        }
    }

    /// Complete an acquisition that has already taken `my_ticket`: wait for
    /// it to be served, publish the holder, and register with the
    /// lock-order validator.
    ///
    /// One out-of-line non-generic call, deliberately covering the uncontended
    /// path too: `SpinLock::lock` is inlined at ~850 call sites and everything
    /// it keeps inline lands in each caller's frame, which
    /// `check_stack_sizes.sh --variant release` has no allowlist to absorb.
    /// Splitting the wait from the publish would leave `my_ticket` live across
    /// two calls; folded, nothing but `&self` is live across the call.
    #[inline(never)]
    fn acquire(&self, my_ticket: u16) {
        self.acquire_nested(my_ticket, 0)
    }

    /// [`LockCore::acquire`] registering under `subclass`, which gives this
    /// acquisition a class distinct from the same declaration's subclass 0.
    #[inline(never)]
    fn acquire_nested(&self, my_ticket: u16, subclass: u8) {
        if self.now_serving.load(Ordering::Acquire) != my_ticket {
            self.await_ticket(my_ticket);
        }
        let cpu = crate::cpu::x86_64::pcr::get_current_cpu() as u32;
        self.holder
            .store((cpu << 16) | my_ticket as u32, Ordering::Relaxed);
        // SAFETY: this CPU holds the lock, and `self` outlives the guard
        // the caller is about to build.
        unsafe {
            lock_tracking::push_lock_ex(
                self as *const _ as *const (),
                lock_core_poison_fn,
                self.class,
                subclass,
                lock_tracking::ACQ_NONE,
            );
        }
    }

    /// The CPU holding this lock, if the recorded holder can be believed.
    ///
    /// Both conjuncts are load-bearing. `next_ticket != now_serving` says the
    /// lock is held at all — a released lock still carries its last holder.
    /// `holder`'s ticket matching `now_serving` rejects the window between a
    /// releaser's `fetch_add` and the next winner's store, in which a reader
    /// would otherwise see the *previous* holder.
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

    /// Spin until `my_ticket` is served. Out of line and non-generic so the
    /// contended path stays out of `SpinLock::lock`'s inlined frame, and so a
    /// wedge is reported by the spinner — which knows which lock it wants —
    /// rather than by the peer watchdog seconds later naming only the victim.
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
            spin_relax_fire();
            let distance = my_ticket.wrapping_sub(serving) as u32;
            for _ in 0..distance.min(64) {
                spin_loop();
            }
        }
        if published {
            crate::watchdog::end_wait();
        }
    }

    /// Release one holder, as a guard's `Drop` would. Storing `next_ticket`
    /// instead would jump `now_serving` past every queued waiter, so none of
    /// their tickets would ever be served and the lock would wedge for good.
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
    class: &'static LockClassKey,
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
    pub const fn new(data: T, class: &'static LockClassKey) -> Self {
        Self {
            core: LockCore::new(class),
            data: UnsafeCell::new(data),
        }
    }

    /// In-place [`Init`] recipe: builds the lock fields directly into the heap
    /// slot so a large `T` (e.g. a 256-slot timer wheel) never materialises on
    /// the caller's stack. Used via
    /// `KBox::try_init(SpinLock::init_with(class, T::init_default()))`.
    pub fn init_with<E>(
        class: &'static LockClassKey,
        data_init: impl Init<T, E>,
    ) -> impl Init<Self, E>
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
                addr_of_mut!((*slot).core).write(LockCore::new(class));
                let data_ptr = addr_of_mut!((*slot).data) as *mut T;
                data_init.__init(data_ptr)?;
                Ok(())
            })
        }
    }

    #[inline]
    pub const fn level(&self) -> u8 {
        self.core.class.level()
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

    /// Take a ticket without building a guard, leaving the lock held by nobody
    /// — the state a panic leaves behind. Leaking a real guard would also leak
    /// the interrupt flags and preempt count it restores, silently stopping
    /// this CPU from ever ticking again.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn abandon_for_test(&self) {
        self.core.next_ticket.fetch_add(1, Ordering::Relaxed);
    }

    /// Release one holder of a lock whose guard was lost, as the panic-recovery
    /// path does. Safe because [`LockCore::release_one`] is a no-op on a lock
    /// that is already free.
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

    /// Read a naturally-aligned field of the protected data without taking the
    /// lock. Every field `f` touches must be a naturally-aligned scalar
    /// (u32 / pointer / atomic) written only under the lock, so a plain load is
    /// tear-free on x86-64; composite (multi-word) fields MUST re-acquire the
    /// lock via [`Self::lock`] instead.
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

        let my_ticket = self.core.next_ticket.fetch_add(1, Ordering::Relaxed);
        self.core.acquire(my_ticket);

        SpinLockGuard {
            mutex: self,
            saved_flags,
            _preempt: preempt,
        }
    }

    /// Acquire under `subclass`, splitting this declaration site into distinct
    /// lockdep classes. For a site that legitimately holds two of its own
    /// instances at once in a fixed order, this keeps that order *checked*
    /// where `LO_DUPOK` would discard the check for the whole class.
    #[inline]
    pub fn lock_nested(&self, subclass: u8) -> SpinLockGuard<'_, T> {
        let preempt = PreemptGuard::new();
        let saved_flags = cpu::save_flags_cli();

        let my_ticket = self.core.next_ticket.fetch_add(1, Ordering::Relaxed);
        self.core.acquire_nested(my_ticket, subclass);

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
            // `current` was `now_serving`, so the wait inside `acquire`
            // completes immediately. The `try_lock`-only locks are the ones
            // the reporter itself takes; skipping the holder here would
            // leave a hole in the graph exactly where the diagnostics run.
            self.core.acquire(current);
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
    pub const fn new(data: T, class: &'static LockClassKey) -> Self {
        Self {
            next_ticket: AtomicU16::new(0),
            now_serving: AtomicU16::new(0),
            class,
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
                self.class,
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
                    self.class,
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
    class: &'static LockClassKey,
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
    pub const fn new(data: T, class: &'static LockClassKey) -> Self {
        Self {
            state: core::sync::atomic::AtomicI32::new(0),
            writer_waiting: AtomicU32::new(0),
            class,
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
                    // Readers are recursive: nesting two read acquisitions
                    // of one instance is legal here and is not a deadlock.
                    unsafe {
                        lock_tracking::push_lock_ex(
                            self as *const _ as *const (),
                            irq_rwlock_poison_fn::<T>,
                            self.class,
                            0,
                            lock_tracking::ACQ_RECURSIVE,
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
                // Recursive for the same reason as `read`.
                unsafe {
                    lock_tracking::push_lock_ex(
                        self as *const _ as *const (),
                        irq_rwlock_poison_fn::<T>,
                        self.class,
                        0,
                        lock_tracking::ACQ_RECURSIVE,
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
                        self.class,
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
                    self.class,
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
