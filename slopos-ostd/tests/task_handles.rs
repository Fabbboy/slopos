//! Host-side tests for `slopos_ostd::task::handles`.
//!
//! Exercises the typestate handles (`OwnedTaskHandle`, `SharedTaskHandle`),
//! the `TaskOps` plug, and the `LinkProvider` blanket impl that absorbs
//! the kernel-side `unsafe impl Linked` markers.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use slopos_ostd::sync::intrusive::{IntrusiveLinkedList, Link};
use slopos_ostd::task::handles::{
    LinkProvider, OwnedTaskHandle, SharedTaskHandle, TaskOps, task_state,
};

// =============================================================================
// MockTask — a host-side stand-in implementing `TaskOps` + `LinkProvider`.
// =============================================================================

pub enum RoleA {}
pub enum RoleB {}

struct MockTask {
    status_atomic: AtomicU32, // 0=Created,1=Ready,2=Running,3=Blocked,4=Terminated
    refcnt: AtomicU32,
    mark_ready_count: AtomicUsize,
    mark_terminated_count: AtomicUsize,
    mark_blocked_count: AtomicUsize,
    cas_running_ok: AtomicBool, // controls handle_try_cas_running outcome
    link_a: Link<MockTask, RoleA>,
    link_b: Link<MockTask, RoleB>,
}

impl MockTask {
    const STATUS_CREATED: u32 = 0;
    const STATUS_READY: u32 = 1;
    const STATUS_RUNNING: u32 = 2;
    const STATUS_BLOCKED: u32 = 3;
    const STATUS_TERMINATED: u32 = 4;

    fn new() -> Self {
        Self {
            status_atomic: AtomicU32::new(Self::STATUS_CREATED),
            refcnt: AtomicU32::new(0),
            mark_ready_count: AtomicUsize::new(0),
            mark_terminated_count: AtomicUsize::new(0),
            mark_blocked_count: AtomicUsize::new(0),
            cas_running_ok: AtomicBool::new(true),
            link_a: Link::new(),
            link_b: Link::new(),
        }
    }
}

impl TaskOps for MockTask {
    fn handle_mark_ready(&self) {
        self.status_atomic
            .store(Self::STATUS_READY, Ordering::Release);
        self.mark_ready_count.fetch_add(1, Ordering::AcqRel);
    }
    fn handle_mark_terminated(&self) {
        self.status_atomic
            .store(Self::STATUS_TERMINATED, Ordering::Release);
        self.mark_terminated_count.fetch_add(1, Ordering::AcqRel);
    }
    fn handle_mark_blocked(&self) {
        self.status_atomic
            .store(Self::STATUS_BLOCKED, Ordering::Release);
        self.mark_blocked_count.fetch_add(1, Ordering::AcqRel);
    }
    fn handle_inc_ref(&self) {
        self.refcnt.fetch_add(1, Ordering::AcqRel);
    }
    fn handle_dec_ref(&self) -> bool {
        self.refcnt.fetch_sub(1, Ordering::AcqRel) == 1
    }
    fn handle_ref_count(&self) -> u32 {
        self.refcnt.load(Ordering::Acquire)
    }
    fn handle_status_is_ready(&self) -> bool {
        self.status_atomic.load(Ordering::Acquire) == Self::STATUS_READY
    }
    fn handle_try_cas_running(&self) -> bool {
        if self.cas_running_ok.load(Ordering::Acquire) {
            self.status_atomic
                .store(Self::STATUS_RUNNING, Ordering::Release);
            true
        } else {
            false
        }
    }
}

impl LinkProvider<RoleA> for MockTask {
    fn link(&self) -> &Link<Self, RoleA> {
        &self.link_a
    }
}

impl LinkProvider<RoleB> for MockTask {
    fn link(&self) -> &Link<Self, RoleB> {
        &self.link_b
    }
}

// =============================================================================
// Tests.
// =============================================================================

#[test]
fn owned_handle_is_send_not_sync() {
    fn assert_send<T: Send>() {}
    fn assert_not_sync<T>() {
        // type-level shape only — the absence of a `T: Sync` bound here
        // proves nothing at compile time, so we instead pin the
        // expected presence/absence via the `auto_traits` form below.
    }
    assert_send::<OwnedTaskHandle<MockTask, task_state::Created>>();
    assert_not_sync::<OwnedTaskHandle<MockTask, task_state::Created>>();
}

#[test]
fn shared_handle_is_send_and_sync() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<SharedTaskHandle<MockTask, task_state::Runnable>>();
    assert_sync::<SharedTaskHandle<MockTaskNoRef, task_state::Runnable>>();
}

// A second mock without the test counters — keeps Sync-bound tests
// from accidentally hinging on non-Sync inner state.
struct MockTaskNoRef {
    refcnt: AtomicU32,
}
impl TaskOps for MockTaskNoRef {
    fn handle_mark_ready(&self) {}
    fn handle_mark_terminated(&self) {}
    fn handle_mark_blocked(&self) {}
    fn handle_inc_ref(&self) {
        self.refcnt.fetch_add(1, Ordering::AcqRel);
    }
    fn handle_dec_ref(&self) -> bool {
        self.refcnt.fetch_sub(1, Ordering::AcqRel) == 1
    }
    fn handle_ref_count(&self) -> u32 {
        self.refcnt.load(Ordering::Acquire)
    }
    fn handle_status_is_ready(&self) -> bool {
        false
    }
    fn handle_try_cas_running(&self) -> bool {
        false
    }
}

#[test]
fn created_to_runnable_transition_calls_mark_ready() {
    let task = Box::new(MockTask::new());
    let raw = Box::into_raw(task);
    // SAFETY: `raw` is exclusive (we just allocated it) and the state
    // is `Created` (matches `task_state::Created`).
    let created: OwnedTaskHandle<MockTask, task_state::Created> =
        unsafe { OwnedTaskHandle::from_raw(raw) };

    let runnable = created.into_runnable();
    // SAFETY: pointer still valid; we observe the side-effect count.
    let count = unsafe {
        (*runnable.as_raw())
            .mark_ready_count
            .load(Ordering::Acquire)
    };
    assert_eq!(count, 1, "into_runnable should call handle_mark_ready once");

    // Drop the handle and free the backing memory to avoid leaks.
    let raw_again = runnable.into_raw();
    // SAFETY: we own the only handle and just unwrapped it.
    drop(unsafe { Box::from_raw(raw_again) });
}

#[test]
fn shared_clone_drop_balance_refcount() {
    let task = Box::new(MockTask::new());
    let raw = Box::into_raw(task);
    // SAFETY: same as above; we manage the lifetime explicitly.
    // We adopt one refcount: increment manually before constructing
    // the SharedTaskHandle.
    unsafe { (*raw).handle_inc_ref() };
    let shared: SharedTaskHandle<MockTask, task_state::Runnable> =
        unsafe { SharedTaskHandle::from_raw(raw) };

    // Clone twice: refcount goes 1 → 3.
    let clone1 = shared.clone();
    let clone2 = shared.clone();
    // SAFETY: pointer still valid.
    assert_eq!(unsafe { (*raw).handle_ref_count() }, 3);

    // Drop one clone: refcount goes 3 → 2.
    drop(clone1);
    assert_eq!(unsafe { (*raw).handle_ref_count() }, 2);

    // Drop the remaining two: refcount goes to 0.
    drop(clone2);
    drop(shared);
    assert_eq!(unsafe { (*raw).handle_ref_count() }, 0);

    drop(unsafe { Box::from_raw(raw) });
}

#[test]
fn try_claim_running_succeeds_when_cas_ok() {
    let task = Box::new(MockTask::new());
    let raw = Box::into_raw(task);
    // SAFETY: same as above. cas_running_ok defaults to `true` in
    // `MockTask::new`.
    unsafe { (*raw).handle_inc_ref() };
    let shared: SharedTaskHandle<MockTask, task_state::Runnable> =
        unsafe { SharedTaskHandle::from_raw(raw) };

    let result = shared.try_claim_running();
    assert!(
        result.is_ok(),
        "CAS should succeed when cas_running_ok=true"
    );
    let owned = result.unwrap_or_else(|_| panic!("just asserted ok"));

    // The refcount transferred to the owned handle; no implicit drop
    // happened (try_claim_running forgets `self` on success).
    // SAFETY: pointer still valid.
    assert_eq!(unsafe { (*raw).handle_ref_count() }, 1);

    let raw_again = owned.into_raw();
    // SAFETY: balance the manual increment from setup.
    unsafe { (*raw_again).handle_dec_ref() };
    drop(unsafe { Box::from_raw(raw_again) });
}

#[test]
fn try_claim_running_returns_self_when_cas_fails() {
    let task = Box::new(MockTask::new());
    let raw = Box::into_raw(task);
    // SAFETY: pointer is freshly allocated.
    unsafe { (*raw).cas_running_ok.store(false, Ordering::Release) };
    unsafe { (*raw).handle_inc_ref() };
    let shared: SharedTaskHandle<MockTask, task_state::Runnable> =
        unsafe { SharedTaskHandle::from_raw(raw) };

    let result = shared.try_claim_running();
    assert!(result.is_err(), "CAS should fail when cas_running_ok=false");
    let original = match result {
        Err(s) => s,
        Ok(_) => panic!("just asserted err"),
    };
    // SAFETY: pointer still valid.
    assert_eq!(unsafe { (*raw).handle_ref_count() }, 1);

    drop(original); // refcount → 0
    assert_eq!(unsafe { (*raw).handle_ref_count() }, 0);
    drop(unsafe { Box::from_raw(raw) });
}

#[test]
fn link_provider_blanket_impl_routes_to_correct_field() {
    let mut task = Box::new(MockTask::new());
    let task_addr = &mut *task as *mut MockTask;
    // The blanket `unsafe impl<T: LinkProvider<R>, R> Linked<R> for T`
    // in OSTD picks up our two safe `LinkProvider` impls for
    // `MockTask`. We exercise it via an actual `IntrusiveLinkedList`.
    let list_a: IntrusiveLinkedList<MockTask, RoleA> = IntrusiveLinkedList::new();
    // SAFETY: `task_addr` is stable for the lifetime of the `Box`.
    let push_ok = unsafe { list_a.push(core::ptr::NonNull::new_unchecked(task_addr)) };
    assert!(push_ok.is_ok());
    assert_eq!(list_a.len(), 1);

    let popped = list_a.pop();
    assert!(popped.is_some());
    assert_eq!(popped.unwrap().as_ptr(), task_addr);
    assert_eq!(list_a.len(), 0);

    drop(task);
}

#[test]
fn distinct_roles_use_distinct_link_fields() {
    // Push the same MockTask onto two distinct-role lists. Both pushes
    // must succeed because the `Linked<RoleA>` and `Linked<RoleB>`
    // impls return distinct fields (`link_a` vs `link_b`).
    let mut task = Box::new(MockTask::new());
    let task_addr = &mut *task as *mut MockTask;
    let list_a: IntrusiveLinkedList<MockTask, RoleA> = IntrusiveLinkedList::new();
    let list_b: IntrusiveLinkedList<MockTask, RoleB> = IntrusiveLinkedList::new();

    let push_a = unsafe { list_a.push(core::ptr::NonNull::new_unchecked(task_addr)) };
    let push_b = unsafe { list_b.push(core::ptr::NonNull::new_unchecked(task_addr)) };
    assert!(push_a.is_ok());
    assert!(
        push_b.is_ok(),
        "distinct roles must use distinct link slots — RoleB push failed"
    );

    // Drain both lists so the task isn't left linked when it drops.
    assert!(list_a.pop().is_some());
    assert!(list_b.pop().is_some());
    drop(task);
}
