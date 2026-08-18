//! Global registration hook for the kernel's [`KernelThreadSpawner`].
//!
//! `slopos-ostd` ships the trait + a free [`spawn`] helper; the concrete
//! implementation lives outside the trusted core (`sched/`). Boot wires the
//! production spawner through this hook before any driver init step runs.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::sync::BspToken;

/// Bare kernel-thread entry point: a plain `fn`, because every call site uses
/// a `'static fn` directly and none needs captures.
pub type KernelThreadEntry = fn();

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

/// Implementors are required to be `Sync` so the static handle slot can be
/// read from any CPU.
pub trait KernelThreadSpawner: Sync {
    /// Create a new kernel-mode task and place it on the run queue.
    fn spawn(
        &self,
        name: &'static str,
        entry: KernelThreadEntry,
        priority: u8,
    ) -> Result<SpawnedTaskId, SpawnError>;
}

/// Opaque task identifier returned by [`spawn`], wrapping the scheduler task
/// table's `u32` so a later shape change does not ripple through callers.
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
    /// A `*const &'static dyn KernelThreadSpawner` reinterpreted as `*mut ()`.
    inner: AtomicPtr<()>,
}

static SPAWNER: SpawnerSlot = SpawnerSlot {
    inner: AtomicPtr::new(core::ptr::null_mut()),
};

/// One-shot wiring point. `slot` is a reference to a `&'static dyn
/// KernelThreadSpawner` — typically a consumer-side `static` wrapping the
/// production spawner singleton. The `&BspToken<'brand>` witnesses BSP-only
/// init; the `Sync` trait bound makes concurrent reads from any CPU after
/// publication sound.
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

/// `None` until [`register_kernel_thread_spawner`] has been called.
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

/// Spawn a new kernel-mode task through the registered
/// [`KernelThreadSpawner`].
///
/// The scheduler keeps a fixed-length copy of `name` on the task struct.
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

/// Test-only hook letting a host test binary re-install a fresh spawner.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    SPAWNER
        .inner
        .store(core::ptr::null_mut(), Ordering::Release);
}
