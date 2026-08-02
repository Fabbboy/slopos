use slopos_ostd::klog_info;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

// =============================================================================
// Task Resource Cleanup Hooks
// =============================================================================
//
// General-purpose hook registry for cleaning up task-bound resources.
// Subsystems (video, input, etc.) register cleanup functions at init time.
// Hooks are invoked during both task termination and exec() so that the old
// process image's resources (compositor surfaces, input queues, ...) are released
// before the new image starts or the task slot is reclaimed.
//
// The registry lives in `core` (low in the crate DAG).  Higher crates register
// callbacks via `register_task_resource_cleanup_hook` during their own init.

const MAX_TASK_RESOURCE_HOOKS: usize = 8;

struct TaskResourceCleanupHooks {
    hooks: [Option<fn(u32)>; MAX_TASK_RESOURCE_HOOKS],
    count: usize,
}

impl TaskResourceCleanupHooks {
    const fn new() -> Self {
        Self {
            hooks: [None; MAX_TASK_RESOURCE_HOOKS],
            count: 0,
        }
    }
}

static TASK_RESOURCE_HOOKS: SpinLock<TaskResourceCleanupHooks> = SpinLock::new(
    TaskResourceCleanupHooks::new(),
    lock_class!("TASK_RESOURCE_HOOKS", LOCK_LEVEL_REGISTRY),
);

/// Register a cleanup hook called whenever task-bound resources must be released.
///
/// This fires during:
/// - **exec()**: before the new process image starts (old window, input queue, etc. are torn down)
/// - **task termination**: as part of normal task teardown
///
/// Hooks receive the `task_id` of the task whose resources should be cleaned up.
/// Registration is expected during subsystem init and is append-only.
pub fn register_task_resource_cleanup_hook(hook: fn(u32)) {
    let mut reg = TASK_RESOURCE_HOOKS.lock();
    if reg.count >= MAX_TASK_RESOURCE_HOOKS {
        klog_info!("task_resource_hooks: registry full, hook not registered");
        return;
    }
    let idx = reg.count;
    reg.hooks[idx] = Some(hook);
    reg.count += 1;
}

/// Run all registered task resource cleanup hooks for the given task.
pub(super) fn run_task_resource_cleanup_hooks(task_id: u32) {
    let hooks = TASK_RESOURCE_HOOKS.lock();
    for hook in hooks.hooks.iter().take(hooks.count) {
        if let Some(f) = hook {
            f(task_id);
        }
    }
}

/// Clean up task-bound resources before exec() replaces the process image.
///
/// Analogous to `fileio_close_on_exec` for file descriptors: tears down
/// compositor surfaces, shared-memory buffers, input queues, and any other
/// resources registered via [`register_task_resource_cleanup_hook`].
///
/// Called from `syscall_exec` after `do_exec` succeeds (point of no return).
pub fn task_cleanup_for_exec(task_id: u32) {
    run_task_resource_cleanup_hooks(task_id);
    // memfd cleanup happens automatically via fd close on process exit
}
