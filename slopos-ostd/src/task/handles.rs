//! Typestate-encoded task lifecycle handles.
//!
//! `OwnedTaskHandle<T, S>` and `SharedTaskHandle<T, S>` wrap a raw
//! `*mut T` with a phantom state parameter `S` that encodes the
//! task's current lifecycle position. Wrong-state operations (e.g.
//! dispatching a `Blocked` task as if it were `Runnable`) become
//! compile-time errors.
//!
//! These types are OSTD-side wrappers around a kernel-defined task
//! type `T` (same pattern as `KArc<T>`): OSTD owns the wrapper, the
//! kernel owns `T`. The kernel impls [`TaskOps`] for its `Task` type
//! to plug in the underlying atomic state and refcount primitives the
//! handles need; the handle's safe API is then generic over `T:
//! TaskOps` and bound to the relevant typestate.
//!
//! Sister-trait [`LinkProvider`] absorbs the kernel-side
//! `unsafe impl Linked<R> for Task` markers via a single blanket
//! `unsafe impl<T: LinkProvider<R>, R> Linked<R> for T` here in OSTD —
//! the kernel impls the safe `LinkProvider` trait and the unsafe
//! `Linked` keyword stays interior to OSTD.

use core::marker::PhantomData;

use crate::sync::intrusive::{Link, Linked};

// =============================================================================
// State markers — zero-sized, exist only at the type level.
// =============================================================================

pub mod task_state {
    /// Just allocated, fields not yet initialised. Cannot be dispatched.
    pub struct Created;
    /// Initialised, on a ready queue, awaiting dispatch.
    pub struct Runnable;
    /// Currently executing on a CPU.
    pub struct Running;
    /// Blocked on a wait condition (sleep, futex, child exit).
    pub struct Blocked;
    /// Exited; awaiting reaping.
    pub struct Zombie;
    /// Reaped; pool slot released. Handle is no longer valid.
    pub struct Reaped;
}

// =============================================================================
// TaskOps — kernel-side hook for the primitives the handles need.
// =============================================================================

/// Kernel-implemented hook providing the atomic state and refcount
/// primitives that [`OwnedTaskHandle`] / [`SharedTaskHandle`] need.
///
/// The handles are generic over `T: TaskOps` so OSTD owns the
/// typestate machinery without knowing the inner-type details
/// (matches the `KArc<T>` pattern: OSTD owns the wrapper, kernel
/// owns `T`).
pub trait TaskOps {
    /// Mark this task ready for dispatch.
    fn handle_mark_ready(&self);
    /// Mark this task terminated (transitioning to zombie).
    fn handle_mark_terminated(&self);
    /// Mark this task blocked on a wait condition.
    fn handle_mark_blocked(&self);
    /// Increment the refcount.
    fn handle_inc_ref(&self);
    /// Decrement the refcount. Returns true if it hit zero.
    fn handle_dec_ref(&self) -> bool;
    /// Read the refcount.
    fn handle_ref_count(&self) -> u32;
    /// True if this task is currently in the Ready state.
    fn handle_status_is_ready(&self) -> bool;
    /// Attempt a CAS Ready→Running. Returns true on success.
    fn handle_try_cas_running(&self) -> bool;
}

// =============================================================================
// OwnedTaskHandle — affine, exclusively-owned handle.
// =============================================================================

/// Affine, exclusively-owned handle to a task. Used during construction,
/// slot allocation, and termination — anywhere a `*mut T` was previously
/// held by a single owner with no aliasing.
///
/// Layout-compatible with `*mut T` via `repr(transparent)` so call sites
/// can adopt incrementally.
#[repr(transparent)]
pub struct OwnedTaskHandle<T, S> {
    raw: *mut T,
    _state: PhantomData<S>,
}

// SAFETY: `OwnedTaskHandle` is a raw pointer + ZST; sending the handle
// moves the raw pointer's logical ownership across CPUs but the
// underlying `T` is expected to be `Sync` (interior mutability via
// atomics — the very contract `TaskOps` codifies).
unsafe impl<T, S> Send for OwnedTaskHandle<T, S> {}

impl<T, S> OwnedTaskHandle<T, S> {
    /// Construct from a raw pointer.
    ///
    /// # Safety
    ///
    /// Caller asserts the task's actual state matches `S`, that the
    /// pointer is valid for the lifetime of the handle, and that no
    /// aliasing `OwnedTaskHandle` over the same slot exists.
    #[inline]
    pub unsafe fn from_raw(raw: *mut T) -> Self {
        Self {
            raw,
            _state: PhantomData,
        }
    }

    /// Extract the raw pointer without consuming the handle.
    #[inline]
    pub fn as_raw(&self) -> *mut T {
        self.raw
    }

    /// Consume the handle and return the raw pointer.
    #[inline]
    pub fn into_raw(self) -> *mut T {
        self.raw
    }

    /// Phantom-only state cast.
    ///
    /// # Safety
    ///
    /// Caller asserts the task's underlying atomic state now matches
    /// `S2`. Used internally by the typed state-transition methods;
    /// rarely useful directly.
    #[inline]
    pub unsafe fn into_state<S2>(self) -> OwnedTaskHandle<T, S2> {
        OwnedTaskHandle {
            raw: self.raw,
            _state: PhantomData,
        }
    }
}

impl<T: TaskOps> OwnedTaskHandle<T, task_state::Created> {
    /// Mark the task ready for dispatch.
    pub fn into_runnable(self) -> OwnedTaskHandle<T, task_state::Runnable> {
        // SAFETY: affine ownership via the handle guarantees no other
        // CPU is racing on the state field; pointer validity is the
        // caller's `from_raw` obligation.
        unsafe { (*self.raw).handle_mark_ready() };
        unsafe { self.into_state() }
    }

    /// Skip directly to Zombie (init failed before becoming runnable).
    pub fn into_zombie(self) -> OwnedTaskHandle<T, task_state::Zombie> {
        // SAFETY: as above.
        unsafe { (*self.raw).handle_mark_terminated() };
        unsafe { self.into_state() }
    }
}

impl<T: TaskOps> OwnedTaskHandle<T, task_state::Runnable> {
    /// Convert to a shared handle for queueing. The shared handle holds
    /// a refcount; the original owned handle is consumed.
    pub fn share(self) -> SharedTaskHandle<T, task_state::Runnable> {
        // SAFETY: as above.
        unsafe { (*self.raw).handle_inc_ref() };
        SharedTaskHandle {
            raw: self.raw,
            _state: PhantomData,
        }
    }
}

impl<T: TaskOps> OwnedTaskHandle<T, task_state::Running> {
    /// Voluntarily block. Running → Blocked.
    pub fn into_blocked(self) -> OwnedTaskHandle<T, task_state::Blocked> {
        // SAFETY: as above.
        unsafe { (*self.raw).handle_mark_blocked() };
        unsafe { self.into_state() }
    }

    /// Exit. Running → Terminated; reaper recycles the slot.
    pub fn into_zombie(self) -> OwnedTaskHandle<T, task_state::Zombie> {
        // SAFETY: as above.
        unsafe { (*self.raw).handle_mark_terminated() };
        unsafe { self.into_state() }
    }
}

impl<T: TaskOps> OwnedTaskHandle<T, task_state::Blocked> {
    /// Wake. Blocked → Ready.
    pub fn into_runnable(self) -> OwnedTaskHandle<T, task_state::Runnable> {
        // SAFETY: as above.
        unsafe { (*self.raw).handle_mark_ready() };
        unsafe { self.into_state() }
    }
}

impl<T> OwnedTaskHandle<T, task_state::Zombie> {
    /// Reaper consumes the zombie handle. The returned `Reaped` handle
    /// is a terminal marker; the pool-slot recycling is driven by the
    /// underlying refcount hitting zero.
    pub fn into_reaped(self) -> OwnedTaskHandle<T, task_state::Reaped> {
        // SAFETY: phantom-only cast; no state side effect.
        unsafe { self.into_state() }
    }
}

// =============================================================================
// SharedTaskHandle — refcounted, shared handle.
// =============================================================================

/// Shared, refcounted handle. Used by scheduler queues and any code
/// observing tasks across CPUs. Cloning increments the underlying
/// refcount; dropping decrements.
///
/// `T: TaskOps` is required on the struct definition rather than only
/// on the impl blocks so that `Clone` and `Drop` (which inherently must
/// match the struct's bounds) can call through to `TaskOps`.
#[repr(transparent)]
pub struct SharedTaskHandle<T: TaskOps, S> {
    raw: *mut T,
    _state: PhantomData<S>,
}

// SAFETY: same reasoning as `OwnedTaskHandle::Send`. `Sync` is sound
// because all state mutation goes through `TaskOps` (atomic primitives).
unsafe impl<T: TaskOps, S> Send for SharedTaskHandle<T, S> {}
unsafe impl<T: TaskOps, S> Sync for SharedTaskHandle<T, S> {}

impl<T: TaskOps, S> SharedTaskHandle<T, S> {
    /// Extract the raw pointer without consuming the handle.
    #[inline]
    pub fn as_raw(&self) -> *mut T {
        self.raw
    }

    /// Construct from a raw pointer.
    ///
    /// # Safety
    ///
    /// Caller asserts the task's actual state matches `S`, that the
    /// pointer is valid for the lifetime of the handle, and that the
    /// caller holds one refcount on the underlying task (the handle
    /// adopts that refcount; dropping the handle releases it).
    #[inline]
    pub unsafe fn from_raw(raw: *mut T) -> Self {
        Self {
            raw,
            _state: PhantomData,
        }
    }
}

impl<T: TaskOps, S> Clone for SharedTaskHandle<T, S> {
    fn clone(&self) -> Self {
        // SAFETY: pointer validity is the `from_raw` caller's obligation;
        // refcount increment is the standard shared-ownership pattern.
        unsafe { (*self.raw).handle_inc_ref() };
        Self {
            raw: self.raw,
            _state: PhantomData,
        }
    }
}

impl<T: TaskOps, S> Drop for SharedTaskHandle<T, S> {
    fn drop(&mut self) {
        // SAFETY: balances the increment from `Clone` or the initial
        // `share()` / `from_raw` adoption.
        unsafe { (*self.raw).handle_dec_ref() };
    }
}

impl<T: TaskOps> SharedTaskHandle<T, task_state::Runnable> {
    /// Scheduler dispatch: atomic CAS Runnable → Running. On success
    /// returns an `OwnedTaskHandle<T, Running>` (exclusive — only one
    /// CPU may run a task at a time). On failure (another CPU won
    /// the race or status changed), returns the original handle.
    pub fn try_claim_running(self) -> Result<OwnedTaskHandle<T, task_state::Running>, Self> {
        // SAFETY: pointer validity is the `from_raw` caller's obligation;
        // the CAS itself is atomic and returns success exactly once
        // across all racing CPUs.
        let claimed = unsafe { (*self.raw).handle_try_cas_running() };
        if claimed {
            let raw = self.raw;
            // We've taken exclusive ownership. The Shared handle's
            // implicit refcount transfers to the running CPU; suppress
            // the Drop that would otherwise decrement it.
            core::mem::forget(self);
            Ok(OwnedTaskHandle {
                raw,
                _state: PhantomData,
            })
        } else {
            Err(self)
        }
    }
}

// =============================================================================
// LinkProvider — safe trait absorbing the kernel-side `unsafe impl Linked`.
// =============================================================================

/// Kernel-implemented safe trait pointing at a task's `Link<Self, Role>`
/// field. A blanket `unsafe impl<T: LinkProvider<R>, R> Linked<R> for T`
/// below absorbs the unsafe contract into OSTD, so the kernel only
/// writes safe `impl LinkProvider for Task` blocks.
///
/// # Why a separate trait
///
/// The underlying [`Linked`] trait is `unsafe trait` because consumers
/// rely on stable in-struct field addresses and distinct fields per
/// role. Those invariants are properties of where the trait is impl'd,
/// not what the impl body says — exactly the kind of guarantee Rust
/// `unsafe trait`s exist to declare. The blanket impl below moves the
/// `unsafe trait` site interior to OSTD; the kernel's per-role impls
/// of `LinkProvider` are safe code.
pub trait LinkProvider<Role>: Sized {
    fn link(&self) -> &Link<Self, Role>;
}

// SAFETY: the `LinkProvider` impl is provided by the inner-type owner
// (kernel-side `Task`), who is responsible for the same stable-address
// and distinct-field-per-role properties the `Linked` trait demands.
// Trust is delegated one level: the unsafe contract is satisfied by
// `LinkProvider` being a kernel-defined impl on a stable kernel type.
unsafe impl<T, R> Linked<R> for T
where
    T: LinkProvider<R>,
{
    #[inline]
    fn link(&self) -> &Link<Self, R> {
        LinkProvider::link(self)
    }
}
