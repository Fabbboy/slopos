//! Kill-safe parking for a half-built task whose builder may die mid-build.
//!
//! A [`PendingTask`] is the sole reference to a task that already owns its
//! kernel stack, its data stack and its process VM, and it is deliberately
//! *unregistered*: no lookup, no active-task walk, no census and no shutdown
//! sweep can see it. That is what makes construction sound — nothing can
//! observe the task half-built — and it is also what makes a lost token
//! unrecoverable. SlopOS tears a blocked or descheduled task down from another
//! CPU without unwinding its kernel stack, so `Drop` is not a release path a
//! builder that can block may rely on.
//!
//! This spine is that release path. It owns the token for exactly the window in
//! which the builder can be asynchronously killed, and teardown drains it by
//! the builder's id. Same shape as `scheduler`'s wait-reference map and
//! `futex_remove_task`: the entry *is* the owning reference, and the atomic
//! take under the lock elects the single releaser, which then acts off the
//! lock.
//!
//! # When a frame does *not* need this
//!
//! `sched/src/scheduler.rs`'s `assert_switch_preempt_safe` panics if a context
//! switch is attempted with a preempt guard held, and it sits at the universal
//! switch chokepoint. So:
//!
//! > A [`PendingTask`] may be held on a stack frame across a `PreemptGuard`-
//! > covered, **non-blocking** region: a frame that cannot deschedule cannot be
//! > asynchronously torn down, because a peer's `task_terminate` observes
//! > `on_cpu` and takes the deferred branch, and that branch cannot run until
//! > the frame switches out.
//!
//! `task_build`, `task_fork` and `task_clone` are all of that shape and hold
//! their tokens under a guard rather than parking them. Only `spawn`'s middle —
//! an ELF read and a set of fd actions — genuinely blocks.
//!
//! # Locking
//!
//! The spine lock is a cli-lock, so nothing under it may allocate, block, or
//! take another lock. In particular `task_abandon` — which destroys the child's
//! process VM and fd table — always runs **off** it. The lock graph will not
//! catch a violation: `LOCK_LEVEL_RESOURCE` → `LOCK_LEVEL_REGISTRY` is
//! *ascending* and therefore legal to the ordering model, so the rule is
//! asserted here directly. For the same reason this spine's lock must never be
//! taken while the registry lock is held.
//!
//! Fixed capacity, and no allocation on any path — deliberately unlike the
//! wait-reference map, whose `KBTreeMap::insert` allocates inside its own
//! cli-lock.

use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_abi::task::INVALID_TASK_ID;
use slopos_ostd::cpu::preempt::PreemptGuard;
use slopos_ostd::sync::{KernelSync, LOCK_LEVEL_RESOURCE, SpinLock, held_lock_count};

use super::Task;
use super::task_lifecycle::task_abandon;
use super::task_table::{PendingTask, TaskRef};

/// One slot per task simultaneously inside a blocking build. `MAX_TASKS` is
/// 8192, but 64 concurrent in-flight spawns is already an order of magnitude
/// past anything the system does; the 65th fails the spawn rather than
/// silently dropping the guarantee, and [`parked_spawn_high_water`] makes the
/// choice observable instead of assumed.
const MAX_PARKED_SPAWNS: usize = 64;

struct ParkedSpawn {
    /// Building task's id; `INVALID_TASK_ID` marks a free slot.
    owner_id: u32,
    /// `None` while the owner has the token checked out in
    /// [`SpawnGuard::with_child`]. Only reachable while that owner is
    /// preempt-disabled and on-CPU, so no drain can observe this state for a
    /// task it is allowed to tear down.
    token: Option<KernelSync<PendingTask>>,
}

impl ParkedSpawn {
    const EMPTY: Self = Self {
        owner_id: INVALID_TASK_ID,
        token: None,
    };
}

/// `KernelSync` for the same reason the wait map needs it: `SpinLock<T>: Sync`
/// requires `T: Send`, and a `PendingTask` holds a `KArc<Task>`, which is
/// neither.
static PARKED_SPAWNS: SpinLock<[ParkedSpawn; MAX_PARKED_SPAWNS]> = SpinLock::new(
    [const { ParkedSpawn::EMPTY }; MAX_PARKED_SPAWNS],
    LOCK_LEVEL_RESOURCE,
);

/// High-water mark of simultaneously parked spawns. Diagnostics only.
static PARKED_HIGH_WATER: AtomicUsize = AtomicUsize::new(0);

/// A half-built task, owned somewhere that survives its builder's stack.
///
/// Held by the builder for the whole construction window. Every exit — an early
/// `return`, a `?`, or an asynchronous kill that never returns at all —
/// releases the child: the first two through [`Drop`], the last through the
/// teardown hooks.
pub struct SpawnGuard {
    /// The task whose kernel stack the builder is running on, or
    /// `INVALID_TASK_ID` for a pre-scheduler boot frame. Such a frame has no id
    /// to key on and cannot be asynchronously killed, so its token rides in
    /// `inline` and consumes no slot.
    owner_id: u32,
    child_id: u32,
    child_process_id: u32,
    inline: Option<PendingTask>,
}

impl SpawnGuard {
    /// Park `pending` against the currently-running task.
    ///
    /// `Err(pending)` when the spine is full; the token comes back so the
    /// caller chooses the policy.
    pub fn park(mut pending: PendingTask) -> Result<Self, PendingTask> {
        let child_id = pending.id();
        let child_process_id = pending.as_mut().process_id;
        let owner_id = slopos_ostd::cpu::x86_64::pcr::current_task_id();

        if owner_id == INVALID_TASK_ID {
            return Ok(Self {
                owner_id,
                child_id,
                child_process_id,
                inline: Some(pending),
            });
        }

        let mut spine = PARKED_SPAWNS.lock();
        let Some(slot) = spine
            .iter_mut()
            .find(|slot| slot.owner_id == INVALID_TASK_ID)
        else {
            drop(spine);
            return Err(pending);
        };
        slot.owner_id = owner_id;
        slot.token = Some(KernelSync::new(pending));
        let occupied = spine
            .iter()
            .filter(|slot| slot.owner_id != INVALID_TASK_ID)
            .count();
        drop(spine);
        PARKED_HIGH_WATER.fetch_max(occupied, Ordering::Relaxed);

        Ok(Self {
            owner_id,
            child_id,
            child_process_id,
            inline: None,
        })
    }

    /// Park `pending` against an explicit owner. Test-only: production always
    /// keys on the running task, and a test needs to park against a task it is
    /// not.
    #[cfg(feature = "test-hooks")]
    pub fn park_for_owner(owner_id: u32, pending: PendingTask) -> Result<Self, PendingTask> {
        let mut pending = pending;
        let child_id = pending.id();
        let child_process_id = pending.as_mut().process_id;
        let mut spine = PARKED_SPAWNS.lock();
        let Some(slot) = spine
            .iter_mut()
            .find(|slot| slot.owner_id == INVALID_TASK_ID)
        else {
            drop(spine);
            return Err(pending);
        };
        slot.owner_id = owner_id;
        slot.token = Some(KernelSync::new(pending));
        drop(spine);
        Ok(Self {
            owner_id,
            child_id,
            child_process_id,
            inline: None,
        })
    }

    #[inline]
    pub fn child_id(&self) -> u32 {
        self.child_id
    }

    #[inline]
    pub fn child_process_id(&self) -> u32 {
        self.child_process_id
    }

    /// Exclusive access to the child under construction.
    ///
    /// The token is checked *out* of the spine for the duration and preemption
    /// is disabled, so this frame cannot deschedule and therefore cannot be
    /// asynchronously torn down: a peer's `task_terminate` observes `on_cpu`
    /// and takes the deferred branch, and the deferred branch cannot run until
    /// this frame switches out.
    ///
    /// `f` may allocate and may take locks — it runs off the spine lock. It
    /// must not **block or yield**; `assert_switch_preempt_safe` turns a
    /// violation into a panic naming this frame rather than into a leak.
    /// Anything whose release can deallocate — a displaced `KArc` — must be
    /// returned out of `f` and dropped by the caller: the buddy allocator's
    /// reuse path performs synchronous cross-CPU TLB drains, which is exactly
    /// what a preempt guard forbids.
    ///
    /// Returns `None` only if the token is already gone, which for a live frame
    /// means teardown claimed it — the builder is dying and should abandon.
    pub fn with_child<R>(&mut self, f: impl FnOnce(&mut Task) -> R) -> Option<R> {
        let _preempt = PreemptGuard::new();

        if let Some(pending) = self.inline.as_mut() {
            return Some(f(pending.as_mut()));
        }

        let mut token = self.take_from_spine()?;
        let result = f(token.as_mut());
        self.return_to_spine(token);
        Some(result)
    }

    /// Make the child reachable.
    ///
    /// `None` means the registry was full — `task_commit` abandoned the token
    /// itself — or that teardown already claimed it. Either way nothing is left
    /// to release and the guard is spent.
    pub fn commit(mut self) -> Option<TaskRef> {
        let token = self.take_token()?;
        let _preempt = PreemptGuard::new();
        super::task_lifecycle::task_commit(token)
    }

    /// Take this guard's token back out of wherever it lives. `None` once it is
    /// spent.
    fn take_token(&mut self) -> Option<PendingTask> {
        if let Some(pending) = self.inline.take() {
            self.owner_id = INVALID_TASK_ID;
            return Some(pending);
        }
        let token = self.take_from_spine();
        self.release_slot();
        token
    }

    /// Check the token out, leaving the slot reserved so nothing else claims it.
    fn take_from_spine(&self) -> Option<PendingTask> {
        let mut spine = PARKED_SPAWNS.lock();
        let slot = spine
            .iter_mut()
            .find(|slot| slot.owner_id == self.owner_id)?;
        slot.token.take().map(KernelSync::into_inner)
    }

    fn return_to_spine(&self, token: PendingTask) {
        let mut spine = PARKED_SPAWNS.lock();
        if let Some(slot) = spine.iter_mut().find(|slot| slot.owner_id == self.owner_id) {
            slot.token = Some(KernelSync::new(token));
            return;
        }
        // The slot vanished while checked out, which cannot happen for a frame
        // that is running: only teardown clears a slot, and teardown does not
        // run against a task that is on-CPU. Drop the token here rather than
        // leak it if that reasoning is ever wrong.
        drop(spine);
        task_abandon(token);
    }

    fn release_slot(&mut self) {
        if self.owner_id == INVALID_TASK_ID {
            return;
        }
        let mut spine = PARKED_SPAWNS.lock();
        if let Some(slot) = spine.iter_mut().find(|slot| slot.owner_id == self.owner_id) {
            slot.owner_id = INVALID_TASK_ID;
            slot.token = None;
        }
        self.owner_id = INVALID_TASK_ID;
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        let Some(token) = self.take_token() else {
            return;
        };
        // Off the spine lock by construction: `take_token` releases it before
        // returning. No assertion here — a `Drop` that can panic is its own
        // hazard, and `scripts/check_drop_panic_free.sh` says so; the
        // equivalent check lives on `release_parked_spawn`, which is not one.
        task_abandon(token);
    }
}

/// Release the spawn token (if any) held by a task being torn down.
///
/// Call only where the victim is already established **not** to be executing:
/// while it runs, its own frame owns the token, and abandoning it there would
/// destroy a process VM the builder is concurrently inside.
pub(crate) fn release_parked_spawn(owner_id: u32) {
    if owner_id == INVALID_TASK_ID {
        return;
    }
    let taken = {
        let mut spine = PARKED_SPAWNS.lock();
        spine
            .iter_mut()
            .find(|slot| slot.owner_id == owner_id)
            .and_then(|slot| {
                slot.owner_id = INVALID_TASK_ID;
                slot.token.take()
            })
    };
    let Some(token) = taken else {
        return;
    };
    // `task_abandon` destroys the child's process VM and fd table. Neither is
    // safe under a cli-lock, and the ascending RESOURCE -> REGISTRY ordering
    // means the lock graph would not have objected.
    debug_assert_eq!(
        held_lock_count(),
        0,
        "a parked spawn was abandoned under a lock"
    );
    task_abandon(token.into_inner());
}

/// Drain every parked spawn regardless of owner, returning how many were
/// released.
///
/// Shutdown backstop only: the shutdown sweep walks the registry and cannot see
/// a parked token, and an owner reaped by some other path never reaches the
/// hooks. One entry per acquisition, for the same off-lock reason as above.
pub fn drain_parked_spawns() -> usize {
    let mut drained = 0;
    loop {
        let taken = {
            let mut spine = PARKED_SPAWNS.lock();
            spine
                .iter_mut()
                .find(|slot| slot.owner_id != INVALID_TASK_ID)
                .and_then(|slot| {
                    slot.owner_id = INVALID_TASK_ID;
                    slot.token.take()
                })
        };
        let Some(token) = taken else {
            return drained;
        };
        task_abandon(token.into_inner());
        drained += 1;
    }
}

/// How many spawns are parked right now. Diagnostics and leak assertions.
pub fn parked_spawn_count() -> usize {
    let spine = PARKED_SPAWNS.lock();
    spine
        .iter()
        .filter(|slot| slot.owner_id != INVALID_TASK_ID)
        .count()
}

/// The most spawns ever parked at once. Surfaced so `MAX_PARKED_SPAWNS` is a
/// measured choice rather than an assumed one.
pub fn parked_spawn_high_water() -> usize {
    PARKED_HIGH_WATER.load(Ordering::Relaxed)
}
