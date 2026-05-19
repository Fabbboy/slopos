//! Global registration hook for the kernel's [`KernelThreadSpawner`].
//!
//! `slopos-ostd` ships the trait + a free [`spawn`] helper. The
//! concrete implementation lives outside the trusted core (`sched/`
//! crate). Boot wires the production spawner through this hook before
//! any driver init step runs.
//!
//! One-shot init pattern matches
//! [`crate::task::scheduler_registry::register_scheduler`] and
//! [`crate::mm::frame_alloc::register_frame_allocator`]: an `AtomicPtr`
//! AcqRel-swapped against null, with a panic on double-init.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::sync::BspToken;

/// Bare kernel-thread entry point. The spawn API accepts a plain `fn`
/// because every current call site uses a `'static fn` directly; a
/// closure-based variant (`spawn_boxed<F: FnOnce() + Send + 'static>`)
/// can be layered on top later if a caller actually needs captures.
pub type KernelThreadEntry = fn();

/// Reason a spawn request failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnError {
    /// No spawner has been registered (boot wiring missing or test
    /// fixture not initialised).
    NotInitialised,
    /// The task-slot table is exhausted.
    OutOfTaskIds,
    /// Backing stacks could not be allocated.
    OutOfMemory,
    /// The newly-created task could not be installed on a runqueue.
    ScheduleFailed,
}

/// Out-of-OSTD spawner trait. The concrete impl lives in `sched/`.
///
/// Implementors are required to be `Sync` so the static handle slot
/// can be read from any CPU.
pub trait KernelThreadSpawner: Sync {
    /// Create a new kernel-mode task and place it on the run queue.
    ///
    /// On success returns an opaque [`SpawnedTaskId`]; on failure
    /// returns a typed [`SpawnError`] (so callers can log a useful
    /// reason rather than discriminating on a sentinel integer).
    fn spawn(
        &self,
        name: &'static str,
        entry: KernelThreadEntry,
        priority: u8,
    ) -> Result<SpawnedTaskId, SpawnError>;
}

/// Opaque task identifier returned by [`spawn`].
///
/// Carries the underlying `u32` from the scheduler's task table so the
/// callers that already need a numeric ID for logging or wait/exit
/// tracking can extract it via [`Self::as_u32`]. The newtype wrapper
/// exists so future shape changes (e.g. a 64-bit ID with embedded
/// generation counter) don't ripple through every `spawn` caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpawnedTaskId(u32);

impl SpawnedTaskId {
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

struct SpawnerSlot {
    /// `*const &'static dyn KernelThreadSpawner` reinterpreted as
    /// `*mut ()`.
    inner: AtomicPtr<()>,
}

static SPAWNER: SpawnerSlot = SpawnerSlot {
    inner: AtomicPtr::new(core::ptr::null_mut()),
};

/// One-shot wiring point. Pass a reference to a `&'static dyn
/// KernelThreadSpawner` — typically a `static` consumer-side wrapping
/// the production spawner singleton, then a reference to *that*. The
/// `&BspToken<'brand>` witnesses BSP-only init via the HRTB closure
/// minted by [`crate::sync::run_bsp_init`]; the underlying `dyn` impl
/// is `Sync` by trait bound so concurrent reads from any CPU after
/// publication are sound.
pub fn register_kernel_thread_spawner<'brand>(
    _token: &BspToken<'brand>,
    slot: &'static &'static dyn KernelThreadSpawner,
) {
    let raw = slot as *const &'static dyn KernelThreadSpawner as *mut ();
    let prev = SPAWNER.inner.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::task::spawner::register_kernel_thread_spawner called twice"
    );
}

/// Look up the registered spawner. Returns `None` until
/// [`register_kernel_thread_spawner`] has been called.
pub fn current_kernel_thread_spawner() -> Option<&'static dyn KernelThreadSpawner> {
    let raw = SPAWNER.inner.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` was produced by `register_kernel_thread_spawner`
    // from a `&'static &'static dyn KernelThreadSpawner`; that storage
    // is `'static` by contract, so the dereference is sound.
    let slot = unsafe { &*(raw as *const &'static dyn KernelThreadSpawner) };
    Some(*slot)
}

/// Spawn a new kernel-mode task. Free-function facade over the
/// registered [`KernelThreadSpawner`] so call sites don't have to
/// know about the registry indirection.
///
/// `name` is a `'static` string baked into the binary — the scheduler
/// keeps a fixed-length copy on the task struct. `entry` is a plain
/// `fn()` (no closure captures, no `*mut c_void` arg); see
/// [`KernelThreadEntry`] for rationale.
///
/// `priority` is the same 0–255 priority the scheduler accepts on its
/// internal API.
pub fn spawn(
    name: &'static str,
    entry: KernelThreadEntry,
    priority: u8,
) -> Result<SpawnedTaskId, SpawnError> {
    let Some(spawner) = current_kernel_thread_spawner() else {
        return Err(SpawnError::NotInitialised);
    };
    spawner.spawn(name, entry, priority)
}

/// Test-only reset hook. Allows host integration-test binaries to
/// re-install a fresh spawner between test binary invocations.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    SPAWNER
        .inner
        .store(core::ptr::null_mut(), Ordering::Release);
}
